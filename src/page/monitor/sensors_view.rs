//! 传感器标签页：hwmon / thermal 的全部温度、风扇、电压、功耗、电流、频率。

use std::cell::RefCell;

use gtk::prelude::*;

use super::{card, card_title, set_level, usage_row};
use crate::model::monitor::{Sensor, SensorKind, SensorKind as K, Snapshot};
use crate::utils::monitor as mon;

pub struct SensorsView {
    root: gtk::Box,
    summary: gtk::Label,
    chips: gtk::Box,
    /// 上一次渲染的「芯片/标签/类别」签名，用来判断是否需要重建行
    signature: RefCell<String>,
    rows: RefCell<Vec<(gtk::ProgressBar, gtk::Label)>>,
}

impl SensorsView {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }

    pub fn new() -> SensorsView {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let (c, box_) = card();
        root.append(&c);
        box_.append(&card_title("硬件传感器"));
        let summary = gtk::Label::new(Some("正在读取 /sys/class/hwmon 与 /sys/class/thermal…"));
        summary.set_xalign(0.0);
        summary.add_css_class("dim-label");
        summary.add_css_class("caption");
        box_.append(&summary);

        let chips = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&chips);

        SensorsView {
            root,
            summary,
            chips,
            signature: RefCell::new(String::new()),
            rows: RefCell::new(Vec::new()),
        }
    }

    pub fn update(&self, s: &Snapshot) {
        let sig = signature_of(&s.sensors);
        if sig != *self.signature.borrow() {
            self.rebuild(s);
            *self.signature.borrow_mut() = sig;
        }
        self.fill(s);
    }

    /// 传感器集合变化时重建整个列表（很少发生）。
    fn rebuild(&self, s: &Snapshot) {
        while let Some(child) = self.chips.first_child() {
            self.chips.remove(&child);
        }
        let mut rows = self.rows.borrow_mut();
        rows.clear();

        let mut chips: Vec<&str> = s.sensors.iter().map(|x| x.chip.as_str()).collect();
        chips.dedup_by(|a, b| a == b);
        let mut seen: Vec<&str> = Vec::new();
        for chip in chips {
            if seen.contains(&chip) {
                continue;
            }
            seen.push(chip);
            let (c, box_) = card();
            box_.append(&card_title(chip));
            let list = s
                .sensors
                .iter()
                .filter(|x| x.chip == chip)
                .cloned()
                .collect::<Vec<_>>();
            for item in list {
                let (row, _label, value, bar) =
                    usage_row(&format!("{}（{}）", item.label, item.kind.label()), 200);
                if item.kind != SensorKind::Temp {
                    // 非温度没有百分比概念：隐藏进度条
                    bar.set_visible(false);
                }
                box_.append(&row);
                rows.push((bar, value));
            }
            self.chips.append(&c);
        }
    }

    /// 把数值填进去（顺序与 rebuild 一致）。
    fn fill(&self, s: &Snapshot) {
        let mut temps: Vec<f32> = s
            .sensors
            .iter()
            .filter(|x| x.kind == K::Temp)
            .map(|x| x.value)
            .collect();
        temps.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        let gpus = s.gpus.iter().filter_map(|g| g.temp).fold(0.0f32, f32::max);
        self.summary.set_text(&format!(
            "共 {} 个传感器（{} 个温度）· 最高温度 {} · 显卡 {} · 数据来自 sysfs，无需 lm-sensors",
            s.sensors.len(),
            s.sensors.iter().filter(|x| x.kind == K::Temp).count(),
            mon::temp_text(temps.first().copied().filter(|_| !temps.is_empty())),
            mon::temp_text(if gpus > 0.0 { Some(gpus) } else { None }),
        ));

        let rows = self.rows.borrow();
        for (i, item) in s.sensors.iter().enumerate() {
            let Some((bar, value)) = rows.get(i) else {
                break;
            };
            let text = match item.kind {
                SensorKind::Temp => format!("{:.1} °C", item.value),
                SensorKind::Fan => format!("{:.0} RPM", item.value),
                SensorKind::Voltage => format!("{:.3} V", item.value),
                SensorKind::Power => format!("{:.1} W", item.value),
                SensorKind::Current => format!("{:.2} A", item.value),
                SensorKind::Freq => format!("{:.0} MHz", item.value),
            };
            value.set_text(&text);
            value.set_tooltip_text(Some(&format!(
                "{} / {}\n{}",
                item.chip, item.label, item.raw_input
            )));
            if item.kind == SensorKind::Temp {
                let full = item.crit.or(item.max).unwrap_or(100.0).max(1.0);
                bar.set_fraction((item.value as f64 / full as f64).clamp(0.0, 1.0));
                set_level(bar, item.value / full * 100.0);
                if let Some(c) = item.crit {
                    value.set_tooltip_text(Some(&format!(
                        "{} / {}\n临界 {c:.0}°C{}",
                        item.chip,
                        item.label,
                        match item.max {
                            Some(m) => format!(" · 上限 {m:.0}°C"),
                            None => String::new(),
                        }
                    )));
                }
            } else {
                bar.set_visible(false);
            }
        }
    }
}

/// 传感器列表的签名（芯片 + 类别 + 标签）。
fn signature_of(list: &[Sensor]) -> String {
    let mut s = String::new();
    for x in list {
        s.push_str(&x.chip);
        s.push('/');
        s.push_str(&x.label);
        s.push('/');
        s.push_str(x.kind.label());
        s.push(';');
    }
    s
}
