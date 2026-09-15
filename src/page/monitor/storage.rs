//! 存储标签页：每块磁盘的读写与利用率 + 每个挂载点的使用率。

use std::cell::RefCell;
use std::collections::HashMap;

use gtk::prelude::*;

use super::{card, card_title, set_level, value_label};
use crate::model::monitor::Snapshot;
use crate::utils::monitor as mon;

pub struct StorageView {
    root: gtk::Box,
    fs_summary: gtk::Label,
    fs_box: gtk::Box,
    fs_rows: RefCell<HashMap<String, FsRow>>,
    disk_summary: gtk::Label,
    disk_box: gtk::Box,
    disk_rows: RefCell<HashMap<String, DiskRow>>,
}

struct FsRow {
    device: gtk::Label,
    fstype: gtk::Label,
    total: gtk::Label,
    used: gtk::Label,
    avail: gtk::Label,
    pct: gtk::Label,
    bar: gtk::ProgressBar,
}

struct DiskRow {
    model: gtk::Label,
    size: gtk::Label,
    kind: gtk::Label,
    rate: gtk::Label,
    iops: gtk::Label,
    util: gtk::ProgressBar,
    util_text: gtk::Label,
    await_ms: gtk::Label,
    parts: gtk::Label,
}

impl StorageView {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }

    pub fn new() -> StorageView {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);

        // ---------- 文件系统 ----------
        let (c, box_) = card();
        root.append(&c);
        box_.append(&card_title("文件系统使用率"));
        let fs_summary = gtk::Label::new(Some("正在读取挂载点…"));
        fs_summary.set_xalign(0.0);
        fs_summary.add_css_class("caption");
        fs_summary.add_css_class("dim-label");
        box_.append(&fs_summary);
        let fs_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        box_.append(&fs_box);

        // ---------- 磁盘 IO ----------
        let (c, box_) = card();
        root.append(&c);
        box_.append(&card_title("磁盘读写"));
        let disk_summary = gtk::Label::new(Some("正在读取 /proc/diskstats…"));
        disk_summary.set_xalign(0.0);
        disk_summary.add_css_class("caption");
        disk_summary.add_css_class("dim-label");
        box_.append(&disk_summary);
        let disk_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        box_.append(&disk_box);

        StorageView {
            root,
            fs_summary,
            fs_box,
            fs_rows: RefCell::new(HashMap::new()),
            disk_summary,
            disk_box,
            disk_rows: RefCell::new(HashMap::new()),
        }
    }

    pub fn update(&self, s: &Snapshot) {
        self.update_fs(s);
        self.update_disks(s);
    }

    fn update_fs(&self, s: &Snapshot) {
        let mut rows = self.fs_rows.borrow_mut();
        let need_rebuild =
            rows.len() != s.fs.len() || s.fs.iter().any(|f| !rows.contains_key(&f.mount));
        if need_rebuild {
            rows.clear();
            while let Some(child) = self.fs_box.first_child() {
                self.fs_box.remove(&child);
            }
            for f in s.fs.iter() {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                let mount = gtk::Label::new(Some(&f.mount));
                mount.set_size_request(150, -1);
                mount.set_xalign(0.0);
                mount.add_css_class("caption");
                mount.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                let device = value_label();
                device.set_size_request(150, -1);
                let fstype = value_label();
                fstype.set_size_request(70, -1);
                let total = value_label();
                total.set_size_request(90, -1);
                let used = value_label();
                used.set_size_request(90, -1);
                let avail = value_label();
                avail.set_size_request(90, -1);
                let bar = gtk::ProgressBar::new();
                bar.set_size_request(120, -1);
                bar.set_valign(gtk::Align::Center);
                let pct = value_label();
                pct.set_size_request(60, -1);
                for w in [&mount, &device, &fstype, &total, &used, &avail] {
                    row.append(w);
                }
                row.append(&bar);
                row.append(&pct);
                self.fs_box.append(&row);
                rows.insert(
                    f.mount.clone(),
                    FsRow {
                        device,
                        fstype,
                        total,
                        used,
                        avail,
                        pct,
                        bar,
                    },
                );
            }
        }
        for f in s.fs.iter() {
            let Some(r) = rows.get(&f.mount) else {
                continue;
            };
            r.device.set_text(&f.device);
            r.device.set_tooltip_text(Some(&f.device));
            r.fstype.set_text(&f.fstype);
            r.total.set_text(&mon::human_bytes(f.total));
            r.used.set_text(&mon::human_bytes(f.used));
            r.avail.set_text(&mon::human_bytes(f.avail));
            r.pct.set_text(&format!("{:.1}%", f.use_pct));
            r.bar.set_fraction((f.use_pct / 100.0) as f64);
            set_level(&r.bar, f.use_pct);
        }
        let total: u64 = s.fs.iter().map(|f| f.total).sum();
        let used: u64 = s.fs.iter().map(|f| f.used).sum();
        self.fs_summary.set_text(&format!(
            "{} 个挂载点 · 合计 {} / {}（{:.1}%）",
            s.fs.len(),
            mon::human_bytes(used),
            mon::human_bytes(total),
            if total > 0 {
                100.0 * used as f64 / total as f64
            } else {
                0.0
            }
        ));
    }

    fn update_disks(&self, s: &Snapshot) {
        let mut rows = self.disk_rows.borrow_mut();
        let need_rebuild =
            rows.len() != s.disks.len() || s.disks.iter().any(|d| !rows.contains_key(&d.name));
        if need_rebuild {
            rows.clear();
            while let Some(child) = self.disk_box.first_child() {
                self.disk_box.remove(&child);
            }
            for d in s.disks.iter() {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                let name = gtk::Label::new(Some(&d.name));
                name.set_size_request(70, -1);
                name.set_xalign(0.0);
                name.add_css_class("caption");
                let model = gtk::Label::new(None);
                model.set_size_request(170, -1);
                model.set_xalign(0.0);
                model.add_css_class("caption");
                model.add_css_class("dim-label");
                model.set_ellipsize(gtk::pango::EllipsizeMode::End);
                let size = value_label();
                size.set_size_request(80, -1);
                let kind = value_label();
                kind.set_size_request(60, -1);
                let rate = value_label();
                rate.set_size_request(200, -1);
                let iops = value_label();
                iops.set_size_request(120, -1);
                let bar = gtk::ProgressBar::new();
                bar.set_size_request(100, -1);
                bar.set_valign(gtk::Align::Center);
                let util_text = value_label();
                util_text.set_size_request(50, -1);
                let await_ms = value_label();
                await_ms.set_size_request(90, -1);
                let parts = gtk::Label::new(None);
                parts.set_xalign(0.0);
                parts.add_css_class("caption");
                parts.add_css_class("dim-label");
                parts.set_ellipsize(gtk::pango::EllipsizeMode::End);
                for w in [&name, &model, &size, &kind, &rate, &iops] {
                    row.append(w);
                }
                row.append(&bar);
                row.append(&util_text);
                row.append(&await_ms);
                let (c, box_) = card();
                box_.set_spacing(2);
                box_.append(&row);
                box_.append(&parts);
                self.disk_box.append(&c);
                rows.insert(
                    d.name.clone(),
                    DiskRow {
                        model,
                        size,
                        kind,
                        rate,
                        iops,
                        util: bar,
                        util_text,
                        await_ms,
                        parts,
                    },
                );
            }
        }
        for d in s.disks.iter() {
            let Some(r) = rows.get(&d.name) else { continue };
            r.model
                .set_text(if d.model.is_empty() { "—" } else { &d.model });
            r.size.set_text(&mon::human_bytes(d.size));
            r.kind.set_text(if d.rotational {
                "机械盘"
            } else {
                "固态盘"
            });
            r.rate.set_text(&format!(
                "读 {}  写 {}",
                mon::human_rate(d.read_bytes_s),
                mon::human_rate(d.write_bytes_s)
            ));
            r.iops
                .set_text(&format!("{:.0} / {:.0} IOPS", d.read_iops, d.write_iops));
            r.util.set_fraction((d.util / 100.0) as f64);
            set_level(&r.util, d.util);
            r.util_text.set_text(&format!("{:.0}%", d.util));
            r.await_ms.set_text(&format!("{:.1} ms", d.await_ms));
            r.parts.set_text(&if d.partitions.is_empty() {
                "无分区".to_string()
            } else {
                format!("分区：{}", d.partitions.join("  "))
            });
        }
        let (r, w) = mon::disk_total_io(&s.disks);
        self.disk_summary.set_text(&format!(
            "{} 块磁盘 · 合计读 {} / 写 {}",
            s.disks.len(),
            mon::human_rate(r),
            mon::human_rate(w)
        ));
    }
}
