//! 概览标签页：CPU / 内存 / 显卡 / 网络 / 磁盘 / 温度速览 / 系统信息。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::prelude::*;

use super::{big_value, card, card_title, kv_row, set_level, usage_row, value_label};
use crate::model::monitor::{SensorKind, Snapshot};
use crate::utils::monitor as mon;
use crate::widgets::graph::Graph;

pub struct Overview {
    root: gtk::Box,
    // ---- CPU ----
    cpu_big: gtk::Label,
    cpu_graph: Graph,
    cpu_model: gtk::Label,
    cpu_cores: gtk::Label,
    cpu_freq: gtk::Label,
    cpu_temp: gtk::Label,
    cpu_split: gtk::Label,
    cpu_ctx: gtk::Label,
    core_box: gtk::Grid,
    core_rows: RefCell<Vec<(gtk::ProgressBar, gtk::Label)>>,
    // ---- 内存 ----
    mem_big: gtk::Label,
    mem_graph: Graph,
    mem_bar: gtk::ProgressBar,
    swap_bar: gtk::ProgressBar,
    mem_total: gtk::Label,
    mem_used: gtk::Label,
    mem_avail: gtk::Label,
    mem_cached: gtk::Label,
    mem_buffers: gtk::Label,
    mem_shared: gtk::Label,
    mem_slab: gtk::Label,
    mem_dirty: gtk::Label,
    swap_text: gtk::Label,
    // ---- 显卡（按卡动态建）----
    gpu_box: gtk::Box,
    gpu_cards: RefCell<HashMap<String, GpuCardUi>>,
    // ---- 网络 / 磁盘（按名字动态建）----
    net_graph: Graph,
    net_rx: gtk::Label,
    net_tx: gtk::Label,
    net_box: gtk::Box,
    net_rows: RefCell<HashMap<String, NetRowUi>>,
    disk_graph: Graph,
    disk_r: gtk::Label,
    disk_w: gtk::Label,
    disk_box: gtk::Box,
    disk_rows: RefCell<HashMap<String, DiskRowUi>>,
    // ---- 温度速览 / 系统信息 ----
    temp_box: gtk::Box,
    temp_rows: RefCell<Vec<(gtk::ProgressBar, gtk::Label, gtk::Label)>>,
    sys_host: gtk::Label,
    sys_distro: gtk::Label,
    sys_kernel: gtk::Label,
    sys_uptime: gtk::Label,
    sys_procs: gtk::Label,
    sys_threads: gtk::Label,
    sys_battery: gtk::Label,
}

/// 一张显卡在概览里的控件。
struct GpuCardUi {
    busy: gtk::Label,
    graph: Graph,
    vram: gtk::ProgressBar,
    vram_text: gtk::Label,
    detail: gtk::Label,
}

struct NetRowUi {
    rate: gtk::Label,
    total: gtk::Label,
    addr: gtk::Label,
}

struct DiskRowUi {
    rate: gtk::Label,
    util: gtk::ProgressBar,
    util_text: gtk::Label,
    await_ms: gtk::Label,
    model: gtk::Label,
}

impl Overview {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }

    pub fn new() -> Overview {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);

        // ================= CPU =================
        let (c, box_) = card();
        root.append(&c);
        box_.append(&card_title("处理器"));

        let head = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let cpu_big = big_value();
        head.append(&cpu_big);
        let cpu_graph = Graph::new(180, 72, Graph::BLUE, false);
        // CPU 平时只有几个百分点，自适应到 25%~100% 才看得出波动
        cpu_graph.set_auto(25.0, Some(100.0));
        head.append(cpu_graph.widget());
        box_.append(&head);

        let cpu_model = value_label();
        let cpu_cores = value_label();
        let cpu_freq = value_label();
        let cpu_temp = value_label();
        let cpu_split = value_label();
        let cpu_ctx = value_label();
        let info = gtk::Grid::new();
        info.set_column_spacing(18);
        info.set_row_spacing(2);
        info.attach(&kv_row("型号", &cpu_model), 0, 0, 1, 1);
        info.attach(&kv_row("核心 / 线程", &cpu_cores), 1, 0, 1, 1);
        info.attach(&kv_row("主频", &cpu_freq), 0, 1, 1, 1);
        info.attach(&kv_row("温度", &cpu_temp), 1, 1, 1, 1);
        info.attach(&kv_row("用户 / 系统 / IO等待", &cpu_split), 0, 2, 1, 1);
        info.attach(&kv_row("上下文切换", &cpu_ctx), 1, 2, 1, 1);
        box_.append(&info);

        let cores_title = gtk::Label::new(Some("每个逻辑核心"));
        cores_title.add_css_class("dim-label");
        cores_title.add_css_class("caption");
        cores_title.set_halign(gtk::Align::Start);
        cores_title.set_margin_top(4);
        box_.append(&cores_title);
        let core_box = gtk::Grid::new();
        core_box.set_column_spacing(14);
        core_box.set_row_spacing(2);
        box_.append(&core_box);

        // ================= 内存 =================
        let (c, box_) = card();
        root.append(&c);
        box_.append(&card_title("内存与交换"));

        let head = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let mem_big = big_value();
        head.append(&mem_big);
        let mem_graph = Graph::new(180, 72, Graph::GREEN, true);
        head.append(mem_graph.widget());
        box_.append(&head);

        let mem_bar = gtk::ProgressBar::new();
        box_.append(&mem_bar);
        let swap_bar = gtk::ProgressBar::new();
        box_.append(&swap_bar);

        let mem_total = value_label();
        let mem_used = value_label();
        let mem_avail = value_label();
        let mem_cached = value_label();
        let mem_buffers = value_label();
        let mem_shared = value_label();
        let mem_slab = value_label();
        let mem_dirty = value_label();
        let swap_text = value_label();
        let info = gtk::Grid::new();
        info.set_column_spacing(18);
        info.set_row_spacing(2);
        info.attach(&kv_row("总量", &mem_total), 0, 0, 1, 1);
        info.attach(&kv_row("已用", &mem_used), 1, 0, 1, 1);
        info.attach(&kv_row("可用", &mem_avail), 0, 1, 1, 1);
        info.attach(&kv_row("缓存 / 缓冲", &mem_cached), 1, 1, 1, 1);
        info.attach(&kv_row("共享内存", &mem_shared), 0, 2, 1, 1);
        info.attach(&kv_row("内核 Slab", &mem_slab), 1, 2, 1, 1);
        info.attach(&kv_row("待写回 (dirty)", &mem_dirty), 0, 3, 1, 1);
        info.attach(&kv_row("交换分区", &swap_text), 1, 3, 1, 1);
        box_.append(&info);

        // ================= 显卡 =================
        let (c, gpu_box) = card();
        root.append(&c);
        gpu_box.append(&card_title("显卡"));

        // ================= 网络 =================
        let (c, box_) = card();
        root.append(&c);
        box_.append(&card_title("网络"));
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let net_rx = big_value();
        let net_tx = big_value();
        let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let l1 = gtk::Label::new(Some("接收"));
        l1.add_css_class("caption");
        l1.add_css_class("dim-label");
        l1.set_xalign(0.0);
        col.append(&l1);
        col.append(&net_rx);
        let col2 = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let l2 = gtk::Label::new(Some("发送"));
        l2.add_css_class("caption");
        l2.add_css_class("dim-label");
        l2.set_xalign(0.0);
        col2.append(&l2);
        col2.append(&net_tx);
        head.append(&col);
        head.append(&col2);
        let net_graph = Graph::new_two(180, 72, Graph::TEAL, Graph::PURPLE, "/s");
        head.append(net_graph.widget());
        box_.append(&head);
        let net_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        box_.append(&net_box);

        // ================= 磁盘 =================
        let (c, box_) = card();
        root.append(&c);
        box_.append(&card_title("磁盘"));
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let disk_r = big_value();
        let disk_w = big_value();
        let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let l1 = gtk::Label::new(Some("读取"));
        l1.add_css_class("caption");
        l1.add_css_class("dim-label");
        l1.set_xalign(0.0);
        col.append(&l1);
        col.append(&disk_r);
        let col2 = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let l2 = gtk::Label::new(Some("写入"));
        l2.add_css_class("caption");
        l2.add_css_class("dim-label");
        l2.set_xalign(0.0);
        col2.append(&l2);
        col2.append(&disk_w);
        head.append(&col);
        head.append(&col2);
        let disk_graph = Graph::new_two(180, 72, Graph::BLUE, Graph::YELLOW, "/s");
        head.append(disk_graph.widget());
        box_.append(&head);
        let disk_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        box_.append(&disk_box);

        // ================= 温度速览 =================
        let (c, box_) = card();
        root.append(&c);
        box_.append(&card_title("温度速览"));
        let temp_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        box_.append(&temp_box);

        // ================= 系统信息 =================
        let (c, box_) = card();
        root.append(&c);
        box_.append(&card_title("系统信息"));
        let sys_host = value_label();
        let sys_distro = value_label();
        let sys_kernel = value_label();
        let sys_uptime = value_label();
        let sys_procs = value_label();
        let sys_threads = value_label();
        let sys_battery = value_label();
        let info = gtk::Grid::new();
        info.set_column_spacing(18);
        info.set_row_spacing(2);
        info.attach(&kv_row("主机名", &sys_host), 0, 0, 1, 1);
        info.attach(&kv_row("发行版", &sys_distro), 1, 0, 1, 1);
        info.attach(&kv_row("内核", &sys_kernel), 0, 1, 1, 1);
        info.attach(&kv_row("运行时间", &sys_uptime), 1, 1, 1, 1);
        info.attach(&kv_row("进程数", &sys_procs), 0, 2, 1, 1);
        info.attach(&kv_row("线程数", &sys_threads), 1, 2, 1, 1);
        info.attach(&kv_row("电池", &sys_battery), 0, 3, 2, 1);
        box_.append(&info);

        Overview {
            root,
            cpu_big,
            cpu_graph,
            cpu_model,
            cpu_cores,
            cpu_freq,
            cpu_temp,
            cpu_split,
            cpu_ctx,
            core_box,
            core_rows: RefCell::new(Vec::new()),
            mem_big,
            mem_graph,
            mem_bar,
            swap_bar,
            mem_total,
            mem_used,
            mem_avail,
            mem_cached,
            mem_buffers,
            mem_shared,
            mem_slab,
            mem_dirty,
            swap_text,
            gpu_box,
            gpu_cards: RefCell::new(HashMap::new()),
            net_graph,
            net_rx,
            net_tx,
            net_box,
            net_rows: RefCell::new(HashMap::new()),
            disk_graph,
            disk_r,
            disk_w,
            disk_box,
            disk_rows: RefCell::new(HashMap::new()),
            temp_box,
            temp_rows: RefCell::new(Vec::new()),
            sys_host,
            sys_distro,
            sys_kernel,
            sys_uptime,
            sys_procs,
            sys_threads,
            sys_battery,
        }
    }

    /// 每轮采样刷新一次。
    pub fn update(&self, s: &Snapshot) {
        self.update_cpu(s);
        self.update_mem(s);
        self.update_gpu(s);
        self.update_net(s);
        self.update_disk(s);
        self.update_temp(s);
        self.update_sys(s);
    }

    fn update_cpu(&self, s: &Snapshot) {
        let c = &s.cpu;
        self.cpu_big.set_text(&format!("{:.1}%", c.total));
        self.cpu_graph.push(c.total as f64);
        self.cpu_model.set_text(if c.model.is_empty() {
            "未知"
        } else {
            &c.model
        });
        self.cpu_model.set_tooltip_text(Some(&c.model));
        self.cpu_cores
            .set_text(&format!("{} 核 / {} 线程", c.cores, c.threads));
        self.cpu_freq.set_text(&format!(
            "{:.0} MHz / 最高 {:.0} MHz",
            c.freq_mhz, c.freq_max_mhz
        ));
        self.cpu_temp.set_text(&format!(
            "{}  ·  负载 {:.2} {:.2} {:.2}",
            mon::temp_text(c.temp),
            c.load[0],
            c.load[1],
            c.load[2]
        ));
        self.cpu_split.set_text(&format!(
            "{:.1}% / {:.1}% / {:.1}%",
            c.user + c.nice,
            c.sys,
            c.iowait
        ));
        self.cpu_ctx.set_text(&format!(
            "{} 次  ·  运行中 {}",
            mon::fmt_thousands(c.ctxt),
            c.running
        ));

        // 每核行只建一次（核数不会变）
        {
            let mut rows = self.core_rows.borrow_mut();
            if rows.is_empty() {
                for i in 0..c.per_core.len() {
                    let (row, _label, value, bar) = usage_row(&format!("CPU{i}"), 56);
                    self.core_box
                        .attach(&row, (i % 4) as i32, (i / 4) as i32, 1, 1);
                    rows.push((bar, value));
                }
            }
            for (i, (bar, value)) in rows.iter().enumerate() {
                let v = c.per_core.get(i).copied().unwrap_or(0.0);
                bar.set_fraction((v / 100.0) as f64);
                set_level(bar, v);
                value.set_text(&mon::pct0(v));
            }
        }
    }

    fn update_mem(&self, s: &Snapshot) {
        let m = &s.mem;
        self.mem_big
            .set_text(&format!("{:.1}%", m.used_ratio() * 100.0));
        self.mem_graph.push(m.used_ratio() as f64 * 100.0);
        self.mem_bar.set_fraction(m.used_ratio() as f64);
        self.mem_bar.set_text(Some(&format!(
            "{} / {}",
            mon::human_bytes(m.used),
            mon::human_bytes(m.total)
        )));
        self.mem_bar.set_show_text(true);
        set_level(&self.mem_bar, m.used_ratio() * 100.0);
        self.swap_bar.set_fraction(m.swap_ratio() as f64);
        self.swap_bar.set_text(Some(&format!(
            "交换 {} / {}",
            mon::human_bytes(m.swap_used),
            mon::human_bytes(m.swap_total)
        )));
        self.swap_bar.set_show_text(true);
        set_level(&self.swap_bar, m.swap_ratio() * 100.0);

        self.mem_total.set_text(&mon::human_bytes(m.total));
        self.mem_used.set_text(&format!(
            "{} ({:.1}%)",
            mon::human_bytes(m.used),
            m.used_ratio() * 100.0
        ));
        self.mem_avail.set_text(&mon::human_bytes(m.available));
        self.mem_cached.set_text(&format!(
            "{} / {}",
            mon::human_bytes(m.cached),
            mon::human_bytes(m.buffers)
        ));
        let _ = &self.mem_buffers;
        self.mem_shared.set_text(&mon::human_bytes(m.shared));
        self.mem_slab.set_text(&mon::human_bytes(m.slab));
        self.mem_dirty.set_text(&mon::human_bytes(m.dirty));
        self.swap_text.set_text(&format!(
            "{} / {}（{:.1}%）",
            mon::human_bytes(m.swap_used),
            mon::human_bytes(m.swap_total),
            m.swap_ratio() * 100.0
        ));
    }

    fn update_gpu(&self, s: &Snapshot) {
        let mut cards = self.gpu_cards.borrow_mut();
        // 显卡数量变化时重建（很少发生）
        let need_rebuild =
            s.gpus.len() != cards.len() || s.gpus.iter().any(|g| !cards.contains_key(&g.card));
        if need_rebuild {
            cards.clear();
            while let Some(child) = self.gpu_box.first_child() {
                if child.downcast_ref::<gtk::Label>().is_some() {
                    break; // 保留标题
                }
                self.gpu_box.remove(&child);
            }
            for g in s.gpus.iter() {
                let (c, content) = card();
                c.set_margin_start(0);
                c.set_margin_end(0);
                let name =
                    gtk::Label::new(Some(&format!("{}  ·  {}  ({})", g.name, g.vendor, g.card)));
                name.add_css_class("heading");
                name.set_halign(gtk::Align::Start);
                content.append(&name);

                let head = gtk::Box::new(gtk::Orientation::Horizontal, 12);
                let busy = big_value();
                head.append(&busy);
                let graph = Graph::new(180, 64, Graph::RED, false);
                graph.set_auto(25.0, Some(100.0));
                head.append(graph.widget());
                content.append(&head);

                let vram = gtk::ProgressBar::new();
                vram.set_show_text(true);
                content.append(&vram);
                let vram_text = value_label();
                let detail = value_label();
                let info = gtk::Grid::new();
                info.set_column_spacing(18);
                info.attach(&kv_row("显存", &vram_text), 0, 0, 3, 1);
                content.append(&info);
                content.append(&kv_row("温度 / 功耗 / 频率", &detail));
                self.gpu_box.append(&c);

                cards.insert(
                    g.card.clone(),
                    GpuCardUi {
                        busy,
                        graph,
                        vram,
                        vram_text,
                        detail,
                    },
                );
            }
        }
        for g in s.gpus.iter() {
            let Some(ui) = cards.get(&g.card) else {
                continue;
            };
            match g.busy {
                Some(b) => {
                    ui.busy.set_text(&format!("{b:.0}%"));
                    ui.graph.push(b as f64);
                }
                None => {
                    ui.busy.set_text("—");
                    ui.graph.push(0.0);
                }
            }
            let (used, total) = (g.mem_used.unwrap_or(0), g.mem_total.unwrap_or(0));
            if total > 0 {
                ui.vram.set_fraction(used as f64 / total as f64);
                ui.vram.set_text(Some(&format!(
                    "显存 {} / {}",
                    mon::human_bytes(used),
                    mon::human_bytes(total)
                )));
                set_level(&ui.vram, 100.0 * used as f32 / total as f32);
                ui.vram_text.set_text(&format!(
                    "{} / {} ({:.0}%){}",
                    mon::human_bytes(used),
                    mon::human_bytes(total),
                    100.0 * used as f64 / total as f64,
                    match g.gtt_used {
                        Some(gtt) => format!(" · GTT {}", mon::human_bytes(gtt)),
                        None => String::new(),
                    }
                ));
            } else {
                ui.vram.set_text(Some("显存 不可用"));
                ui.vram_text.set_text("—");
            }
            let mut parts = Vec::new();
            if let Some(t) = g.temp {
                parts.push(format!("{t:.0}°C"));
            }
            if let Some(t) = g.temp_junction {
                parts.push(format!("结温 {t:.0}°C"));
            }
            if let Some(t) = g.temp_mem {
                parts.push(format!("显存 {t:.0}°C"));
            }
            if let Some(p) = g.power {
                parts.push(match g.power_cap {
                    Some(cap) if cap > 0.0 => format!("{p:.0}W / {cap:.0}W"),
                    _ => format!("{p:.0}W"),
                });
            }
            match (g.sclk_mhz, g.mclk_mhz) {
                (Some(a), Some(b)) => parts.push(format!("核心 {a:.0} / 显存 {b:.0} MHz")),
                (Some(a), None) => parts.push(format!("核心 {a:.0} MHz")),
                _ => {}
            }
            if let Some(f) = g.fan {
                parts.push(format!("风扇 {f:.0} RPM"));
            }
            if let Some(u) = g.mem_busy {
                parts.push(format!("显存控制器 {u:.0}%"));
            }
            ui.detail.set_text(&parts.join(" · "));
            ui.detail.set_tooltip_text(Some(&parts.join("\n")));
        }
    }

    fn update_net(&self, s: &Snapshot) {
        let (rx, tx): (f64, f64) = s
            .net
            .iter()
            .filter(|i| !i.is_loopback)
            .fold((0.0, 0.0), |(a, b), i| (a + i.rx_rate, b + i.tx_rate));
        self.net_rx.set_text(&mon::human_rate(rx));
        self.net_tx.set_text(&mon::human_rate(tx));
        self.net_graph.push_pair(rx, tx);
        self.refresh_net_rows(s);
    }

    fn refresh_net_rows(&self, s: &Snapshot) {
        let mut rows = self.net_rows.borrow_mut();
        let need_rebuild =
            rows.len() != s.net.len() || s.net.iter().any(|i| !rows.contains_key(&i.name));
        if need_rebuild {
            rows.clear();
            while let Some(child) = self.net_box.first_child() {
                self.net_box.remove(&child);
            }
            for i in s.net.iter() {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                let name = gtk::Label::new(Some(&i.name));
                name.set_size_request(80, -1);
                name.set_xalign(0.0);
                name.add_css_class("caption");
                let rate = value_label();
                let total = value_label();
                let addr = value_label();
                addr.set_size_request(200, -1);
                addr.add_css_class("dim-label");
                row.append(&name);
                row.append(&rate);
                row.append(&total);
                row.append(&addr);
                self.net_box.append(&row);
                rows.insert(i.name.clone(), NetRowUi { rate, total, addr });
            }
        }
        for i in s.net.iter() {
            let Some(ui) = rows.get(&i.name) else {
                continue;
            };
            ui.rate.set_text(&format!(
                "↓{} ↑{}",
                mon::human_rate(i.rx_rate),
                mon::human_rate(i.tx_rate)
            ));
            ui.total.set_text(&format!(
                "累计 ↓{} ↑{}",
                mon::human_bytes(i.rx_bytes),
                mon::human_bytes(i.tx_bytes)
            ));
            let mut addr = Vec::new();
            if let Some(ip) = i.ipv4.first() {
                addr.push(ip.clone());
            }
            if !i.mac.is_empty() && i.name != "lo" {
                addr.push(i.mac.clone());
            }
            if !i.speed.is_empty() {
                addr.push(i.speed.clone());
            }
            let sig = i.signal_text();
            if !sig.is_empty() {
                addr.push(sig);
            }
            if !i.errors().is_empty() {
                addr.push(i.errors());
            }
            ui.addr.set_text(&addr.join(" · "));
        }
    }

    fn update_disk(&self, s: &Snapshot) {
        let (r, w) = mon::disk_total_io(&s.disks);
        self.disk_r.set_text(&mon::human_rate(r));
        self.disk_w.set_text(&mon::human_rate(w));
        self.disk_graph.push_pair(r, w);

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
                model.set_size_request(180, -1);
                model.set_xalign(0.0);
                model.add_css_class("caption");
                model.add_css_class("dim-label");
                model.set_ellipsize(gtk::pango::EllipsizeMode::End);
                let rate = value_label();
                rate.set_size_request(190, -1);
                let util = gtk::ProgressBar::new();
                util.set_size_request(110, -1);
                util.set_valign(gtk::Align::Center);
                let util_text = value_label();
                let await_ms = value_label();
                row.append(&name);
                row.append(&model);
                row.append(&rate);
                row.append(&util);
                row.append(&util_text);
                row.append(&await_ms);
                self.disk_box.append(&row);
                rows.insert(
                    d.name.clone(),
                    DiskRowUi {
                        rate,
                        util,
                        util_text,
                        await_ms,
                        model,
                    },
                );
            }
        }
        for d in s.disks.iter() {
            let Some(ui) = rows.get(&d.name) else {
                continue;
            };
            ui.model
                .set_text(if d.model.is_empty() { "—" } else { &d.model });
            ui.model.set_tooltip_text(Some(&format!(
                "{} · {} · {}{}",
                d.model,
                mon::human_bytes(d.size),
                if d.rotational {
                    "机械盘"
                } else {
                    "固态盘"
                },
                if d.partitions.is_empty() {
                    String::new()
                } else {
                    format!(" · 分区 {}", d.partitions.join(" "))
                }
            )));
            ui.rate.set_text(&format!(
                "读 {} 写 {}",
                mon::human_rate(d.read_bytes_s),
                mon::human_rate(d.write_bytes_s)
            ));
            ui.util.set_fraction((d.util / 100.0) as f64);
            set_level(&ui.util, d.util);
            ui.util_text.set_text(&format!("{:.0}%", d.util));
            ui.await_ms.set_text(&format!("延迟 {:.1} ms", d.await_ms));
        }
    }

    fn update_temp(&self, s: &Snapshot) {
        // 温度里挑最热的若干个（GPU/CPU 也在里面），只建一次行
        let mut temps: Vec<_> = s
            .sensors
            .iter()
            .filter(|x| x.kind == SensorKind::Temp)
            .collect();
        temps.sort_by(|a, b| {
            b.value
                .partial_cmp(&a.value)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let take = temps.len().min(10);

        let mut rows = self.temp_rows.borrow_mut();
        if rows.is_empty() {
            for _ in 0..take {
                let (row, name, value, bar) = usage_row("", 220);
                self.temp_box.append(&row);
                rows.push((bar, value, name));
            }
        }
        for (i, (bar, value, name)) in rows.iter().enumerate() {
            match temps.get(i) {
                Some(t) => {
                    bar.set_visible(true);
                    value.set_visible(true);
                    name.set_visible(true);
                    name.set_text(&format!("{} · {}", t.chip, t.label));
                    value.set_text(&format!("{:.0}°C", t.value));
                    // 有临界值就按临界值着色，否则 100°C 当满分
                    let frac = (t.crit.or(t.max).unwrap_or(100.0).max(1.0)) as f64;
                    bar.set_fraction((t.value as f64 / frac).clamp(0.0, 1.0));
                    let pct = (t.value / frac as f32) * 100.0;
                    set_level(bar, pct);
                }
                None => {
                    bar.set_visible(false);
                    value.set_visible(false);
                    name.set_visible(false);
                }
            }
        }
    }

    fn update_sys(&self, s: &Snapshot) {
        self.sys_host.set_text(&s.sys.hostname);
        self.sys_distro.set_text(&s.sys.distro);
        self.sys_kernel.set_text(&s.sys.kernel);
        self.sys_uptime.set_text(&mon::human_duration(s.sys.uptime));
        self.sys_procs.set_text(&mon::fmt_thousands(s.sys.procs));
        self.sys_threads
            .set_text(&mon::fmt_thousands(s.sys.threads));
        self.sys_battery.set_text(&if s.batteries.is_empty() {
            "无（台式机 / 未检测到）".to_string()
        } else {
            s.batteries
                .iter()
                .map(|b| {
                    format!(
                        "{} {:.0}%{} {}",
                        b.name,
                        b.capacity,
                        b.status,
                        if b.power > 0.0 {
                            format!(" · {:.0}W", b.power)
                        } else {
                            String::new()
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join("；")
        });
    }
}
