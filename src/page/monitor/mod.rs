//! 系统监视器页面（展示层）。
//!
//! 对标 KDE plasma-systemmonitor：
//! - **概览**：CPU（总量 + 每核 + 曲线）、内存/交换、显卡、网络、磁盘、温度速览、系统信息；
//! - **进程**：可排序表格（搜索 / 过滤 / 平铺或树形）/ 完整信号 / 优先级 / 亲和性 /
//!   IO 优先级 / 结束进程树 / 进程详情；
//! - **传感器**：hwmon 与 thermal 的全部温度 / 风扇 / 电压 / 功耗 / 电流 / 频率；
//! - **存储**：每块磁盘的读写速率与利用率、每个挂载点的使用率；
//! - **网络**：每张网卡的状态、速率、累计流量、地址、错误与丢包。
//!
//! 采集全部在 `utils::monitor` 的后台线程里做（`/proc` + `sysfs`，不依赖 GTK），
//! 本文件只负责把快照画出来 + 把用户操作转成 `utils::monitor::signal` 的调用。

mod net;
mod overview;
mod procs;
mod sensors_view;
mod storage;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use adw::prelude::*;
use glib::clone;
use gtk::glib;

use crate::model::monitor::{INTERVAL_OPTIONS, Snapshot};
use crate::utils::monitor as mon;
use crate::utils::monitor::MonitorControl;

/// 向用户提示一句话（错误走 `err` 样式）。
pub(crate) type ToastFn = Rc<dyn Fn(&str, bool)>;

pub struct MonitorPage {
    root: adw::ToastOverlay,
}

impl MonitorPage {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }
}

thread_local! {
    /// 当前页面实例（采样线程要能被主窗口退出时停掉）。
    static INNER: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
}

pub(crate) fn with_ui<F: FnOnce(&Ui)>(f: F) {
    let Some(ui) = INNER.with(|i| i.try_borrow().ok().and_then(|b| b.clone())) else {
        return;
    };
    f(&ui);
}

/// 程序退出时停止采样线程（由 `main.rs` 调用）。
pub fn shutdown() {
    INNER.with(|i| {
        if let Ok(mut b) = i.try_borrow_mut()
            && let Some(ui) = b.take()
        {
            ui.control.stop();
            // 页销毁后没有 UI 消费速率/详情，把分层采样开关复位
            mon::set_io_columns(false);
            mon::set_io_detail(false);
        }
    });
}

// ---------------------------------------------------------------------------
// UI 状态
// ---------------------------------------------------------------------------

pub(crate) struct Ui {
    control: MonitorControl,
    rx: RefCell<Receiver<Snapshot>>,
    /// 暂停刷新（采样继续，界面不更新，方便盯着某个进程看）
    paused: Cell<bool>,
    /// 用户选择的采样周期（页面重新可见时恢复；不可见期间被降到 HIDDEN_INTERVAL_MS）
    user_interval: Cell<u64>,
    toast_overlay: adw::ToastOverlay,
    status: gtk::Label,
    overview: overview::Overview,
    procs: Rc<procs::ProcsTab>,
    sensors: sensors_view::SensorsView,
    storage: storage::StorageView,
    net: net::NetView,
    /// 最近一份快照（供「复制系统概况」等使用）
    last: RefCell<Option<Snapshot>>,
    /// 已经刷新过几轮（第一轮数据没有差分基准，需要提示）
    ticks: Cell<u64>,
}

impl Ui {
    pub(crate) fn toast(&self, msg: &str, err: bool) {
        let t = adw::Toast::new(msg);
        t.set_timeout(if err { 6 } else { 3 });
        if err {
            // 错误提示久一点，方便看清
            t.set_priority(adw::ToastPriority::High);
        }
        self.toast_overlay.add_toast(t);
    }

    /// 排空 channel，只保留最新快照。
    fn drain(&self) {
        let mut newest = None;
        {
            let rx = self.rx.borrow();
            loop {
                match rx.try_recv() {
                    Ok(s) => newest = Some(s),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
        }
        let Some(snap) = newest else { return };
        self.ticks.set(self.ticks.get() + 1);
        if self.paused.get() {
            return;
        }
        self.status.set_text(&format!(
            "上次刷新 {} · 采样耗时 {} ms · 周期 {} · 进程 {} · 线程 {}",
            mon::human_time(snap.at_ms / 1000),
            snap.cost_ms,
            interval_label(self.control.interval_ms()),
            snap.processes.len(),
            snap.sys.threads,
        ));
        // 只刷新「内层标签可见」的视图（GOAL.md 2.5）：隐藏视图每秒全量
        // 重建也是白烧 CPU。进程表另有一层自带门控（还负责存数据，必须每轮调用）。
        if self.overview.widget().is_mapped() {
            self.overview.update(&snap);
        }
        self.procs.update(&snap);
        if self.sensors.widget().is_mapped() {
            self.sensors.update(&snap);
        }
        if self.storage.widget().is_mapped() {
            self.storage.update(&snap);
        }
        if self.net.widget().is_mapped() {
            self.net.update(&snap);
        }
        *self.last.borrow_mut() = Some(snap);
    }

    fn set_paused(&self, paused: bool) {
        self.paused.set(paused);
        if paused {
            self.status
                .set_text("已暂停刷新（采样仍在后台进行，点「继续刷新」恢复显示）");
        }
    }

    /// 页面不可见（被切走 / 窗口最小化）：采样降到 [`HIDDEN_INTERVAL_MS`]，
    /// 界面排空由 120ms 定时器按 `is_mapped` 跳过（GOAL.md 2.4）。
    fn on_unmapped(&self) {
        self.control.set_interval_ms(HIDDEN_INTERVAL_MS);
    }

    /// 页面重新可见：恢复用户选择的采样周期。
    fn on_mapped(&self) {
        self.control
            .set_interval_ms(self.user_interval.get().max(200));
    }
}

/// 页面不可见时的采样周期（毫秒）。
const HIDDEN_INTERVAL_MS: u64 = 5000;

fn interval_label(ms: u64) -> String {
    for (name, secs) in INTERVAL_OPTIONS {
        if (secs * 1000.0) as u64 == ms {
            return (*name).to_string();
        }
    }
    format!("{ms} ms")
}

// ---------------------------------------------------------------------------
// 通用小组件
// ---------------------------------------------------------------------------

/// 与 systemd 页面一致的卡片容器：返回 (外层卡片, 内容盒)。
pub(crate) fn card() -> (gtk::Box, gtk::Box) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.add_css_class("card");
    outer.set_margin_top(6);
    outer.set_margin_bottom(6);
    outer.set_margin_start(4);
    outer.set_margin_end(4);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(14);
    content.set_margin_end(14);
    outer.append(&content);
    (outer, content)
}

/// 卡片标题。
pub(crate) fn card_title(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("heading");
    l.set_halign(gtk::Align::Start);
    l
}

/// 数值标签：等宽字体、右对齐，方便对齐成一列。
pub(crate) fn value_label() -> gtk::Label {
    let l = gtk::Label::new(None);
    l.set_xalign(1.0);
    l.set_halign(gtk::Align::End);
    l.set_hexpand(true);
    l.add_css_class("monospace");
    l.add_css_class("caption");
    l
}

/// 一行「左标签 + 右数值」。
pub(crate) fn kv_row(name: &str, value: &gtk::Label) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let l = gtk::Label::new(Some(name));
    l.set_xalign(0.0);
    l.add_css_class("dim-label");
    l.add_css_class("caption");
    l.set_hexpand(true);
    row.append(&l);
    row.append(value);
    row
}

/// 大号数值 + 单位（概览页顶部用）。
pub(crate) fn big_value() -> gtk::Label {
    let l = gtk::Label::new(None);
    l.add_css_class("title-1");
    l.set_xalign(0.0);
    l
}

/// 带进度条的占用行：返回 (行容器, 名称标签, 数值标签, 进度条)。
///
/// `label_width` 是名称列宽度（像素）：核名用 56 就够，传感器名要 200 才不会被截断。
pub(crate) fn usage_row(
    name: &str,
    label_width: i32,
) -> (gtk::Box, gtk::Label, gtk::Label, gtk::ProgressBar) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let l = gtk::Label::new(Some(name));
    l.set_xalign(0.0);
    l.add_css_class("caption");
    l.set_size_request(label_width, -1);
    l.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let bar = gtk::ProgressBar::new();
    bar.set_hexpand(true);
    bar.set_valign(gtk::Align::Center);
    let v = value_label();
    v.set_size_request(64, -1);
    row.append(&l);
    row.append(&bar);
    row.append(&v);
    (row, l, v, bar)
}

/// 给进度条按占用率上色（成功 / 警告 / 危险）。
pub(crate) fn set_level(bar: &gtk::ProgressBar, pct: f32) {
    for c in ["success", "warning", "error"] {
        bar.remove_css_class(c);
    }
    bar.add_css_class(mon::level_class(pct));
}

// ---------------------------------------------------------------------------
// 页面构建
// ---------------------------------------------------------------------------

pub fn build() -> MonitorPage {
    let toast_overlay = adw::ToastOverlay::new();
    let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root_box.set_margin_top(12);
    root_box.set_margin_bottom(12);
    root_box.set_margin_start(12);
    root_box.set_margin_end(12);
    root_box.set_vexpand(true);
    toast_overlay.set_child(Some(&root_box));

    // ---------- 标题 ----------
    let title = gtk::Label::new(Some("系统监视器"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    root_box.append(&title);

    let subtitle = gtk::Label::new(Some(
        "实时采集 /proc 与 sysfs：CPU（每核）、内存与交换、显卡（占用 / 显存 / 温度 / 功耗）、\
         网络、磁盘、全部硬件传感器。进程页可搜索、可按平铺或「主进程 + 子进程」树形查看，\
         支持全部 64 个信号、优先级、CPU 亲和性、IO 优先级与结束整棵进程树。",
    ));
    subtitle.add_css_class("dim-label");
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_wrap(true);
    subtitle.set_margin_top(2);
    root_box.append(&subtitle);

    // ---------- 工具条 ----------
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bar.set_margin_top(10);

    let (rx, control) = mon::start(1000);

    let pause_btn = gtk::ToggleButton::with_label("暂停刷新");
    pause_btn.set_tooltip_text(Some("冻结界面显示（采样继续），方便查看某个进程"));
    bar.append(&pause_btn);

    let interval_label_w = gtk::Label::new(Some("采样周期"));
    interval_label_w.add_css_class("dim-label");
    interval_label_w.add_css_class("caption");
    bar.append(&interval_label_w);

    let interval_names: Vec<&str> = INTERVAL_OPTIONS.iter().map(|(n, _)| *n).collect();
    let interval_dd = gtk::DropDown::new(
        Some(gtk::StringList::new(&interval_names)),
        None::<gtk::Expression>,
    );
    interval_dd.set_selected(1); // 默认 1 秒
    interval_dd.set_tooltip_text(Some("采样周期越短越细腻，但占用也越高"));
    bar.append(&interval_dd);

    let copy_btn = gtk::Button::with_label("复制系统概况");
    copy_btn.set_tooltip_text(Some("把当前快照整理成文本复制到剪贴板"));
    bar.append(&copy_btn);

    let status = gtk::Label::new(Some("正在采集…"));
    status.add_css_class("dim-label");
    status.add_css_class("caption");
    status.set_xalign(1.0);
    status.set_hexpand(true);
    status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    bar.append(&status);
    root_box.append(&bar);

    // ---------- 标签页 ----------
    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    let switcher = gtk::StackSwitcher::new();
    switcher.set_stack(Some(&stack));
    switcher.set_halign(gtk::Align::Start);
    root_box.append(&switcher);
    root_box.append(&stack);

    let toast: ToastFn = {
        let overlay = toast_overlay.clone();
        Rc::new(move |msg: &str, err: bool| {
            let t = adw::Toast::new(msg);
            t.set_timeout(if err { 6 } else { 3 });
            if err {
                t.set_priority(adw::ToastPriority::High);
            }
            overlay.add_toast(t);
        })
    };

    let overview = overview::Overview::new();
    let procs = procs::ProcsTab::new(toast.clone());
    let sensors = sensors_view::SensorsView::new();
    let storage = storage::StorageView::new();
    let net = net::NetView::new();

    // 概览页自带滚动（内容很长）
    let overview_scroll = gtk::ScrolledWindow::new();
    overview_scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    overview_scroll.set_child(Some(overview.widget()));
    overview_scroll.set_vexpand(true);
    stack.add_titled(&overview_scroll, Some("overview"), "概览");

    let proc_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    proc_box.set_vexpand(true);
    proc_box.append(procs.widget());
    stack.add_titled(&proc_box, Some("procs"), "进程");

    let sensor_scroll = gtk::ScrolledWindow::new();
    sensor_scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    sensor_scroll.set_child(Some(sensors.widget()));
    sensor_scroll.set_vexpand(true);
    stack.add_titled(&sensor_scroll, Some("sensors"), "传感器");

    let storage_scroll = gtk::ScrolledWindow::new();
    storage_scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    storage_scroll.set_child(Some(storage.widget()));
    storage_scroll.set_vexpand(true);
    stack.add_titled(&storage_scroll, Some("storage"), "存储");

    let net_scroll = gtk::ScrolledWindow::new();
    net_scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    net_scroll.set_child(Some(net.widget()));
    net_scroll.set_vexpand(true);
    stack.add_titled(&net_scroll, Some("net"), "网络");

    let ui = Rc::new(Ui {
        control: control.clone(),
        rx: RefCell::new(rx),
        paused: Cell::new(false),
        user_interval: Cell::new(control.interval_ms()),
        toast_overlay: toast_overlay.clone(),
        status: status.clone(),
        overview,
        procs,
        sensors,
        storage,
        net,
        last: RefCell::new(None),
        ticks: Cell::new(0),
    });
    INNER.with(|i| *i.borrow_mut() = Some(ui.clone()));

    // ---------- 交互 ----------
    pause_btn.connect_toggled(clone!(
        #[weak]
        ui,
        move |b| {
            ui.set_paused(b.is_active());
            b.set_label(if b.is_active() {
                "继续刷新"
            } else {
                "暂停刷新"
            });
        }
    ));

    interval_dd.connect_selected_notify(clone!(
        #[weak]
        ui,
        move |dd| {
            let idx = dd.selected() as usize;
            if let Some((name, secs)) = INTERVAL_OPTIONS.get(idx) {
                let ms = (secs * 1000.0) as u64;
                ui.user_interval.set(ms);
                ui.control.set_interval_ms(ms);
                ui.toast(&format!("采样周期已切换为 {name}"), false);
            }
        }
    ));

    copy_btn.connect_clicked(clone!(
        #[weak]
        ui,
        #[weak]
        copy_btn,
        move |_| {
            let snap = ui.last.borrow();
            let Some(s) = snap.as_ref() else {
                ui.toast("还没有采集到数据", true);
                return;
            };
            let text = summary_text(s);
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&text);
                ui.toast("系统概况已复制到剪贴板", false);
            } else {
                ui.toast("拿不到剪贴板", true);
            }
            let _ = &copy_btn;
        }
    ));

    // 每 120ms 排空一次 channel（采样线程按自己的周期产出）。
    // 页面不可见时直接跳过：排空 + 五个视图刷新会让 GtkColumnView 每轮
    // 全量销毁重建 ~2200 个单元格 widget（实测首页挂机 73% CPU 的主因，GOAL.md 2.4）。
    let weak = Rc::downgrade(&ui);
    glib::timeout_add_local(Duration::from_millis(120), move || match weak.upgrade() {
        Some(ui) => {
            if ui.toast_overlay.is_mapped() {
                ui.drain();
            }
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });

    // GOAL.md 2.4：不可见降频 / 可见恢复（窗口最小化同样走 unmap）
    toast_overlay.connect_unmap(clone!(
        #[weak]
        ui,
        move |_| ui.on_unmapped()
    ));
    toast_overlay.connect_map(clone!(
        #[weak]
        ui,
        move |_| ui.on_mapped()
    ));
    // 启动停在首页：监视器页从未 map 过，先按不可见处理
    if !toast_overlay.is_mapped() {
        ui.on_unmapped();
    }

    MonitorPage {
        root: toast_overlay,
    }
}

/// 把快照整理成可粘贴的文本（复制到剪贴板 / 贴到 issue 里）。
pub(crate) fn summary_text(s: &Snapshot) -> String {
    let mut out = String::new();
    out.push_str("=== 系统 ===\n");
    out.push_str(&format!(
        "主机 {} · {} · 内核 {}\n",
        s.sys.hostname, s.sys.distro, s.sys.kernel
    ));
    out.push_str(&format!(
        "运行时间 {} · 进程 {} · 线程 {} · 采样耗时 {} ms\n",
        mon::human_duration(s.sys.uptime),
        s.sys.procs,
        s.sys.threads,
        s.cost_ms
    ));
    out.push_str("\n=== CPU ===\n");
    out.push_str(&format!(
        "{}（{} 核 {} 线程）\n占用 {:.1}% · 主频 {:.0} MHz / 最高 {:.0} MHz · 温度 {}\n",
        s.cpu.model,
        s.cpu.cores,
        s.cpu.threads,
        s.cpu.total,
        s.cpu.freq_mhz,
        s.cpu.freq_max_mhz,
        mon::temp_text(s.cpu.temp)
    ));
    out.push_str(&format!(
        "负载 {:.2} {:.2} {:.2} · 运行中 {} · 上下文切换 {}\n",
        s.cpu.load[0], s.cpu.load[1], s.cpu.load[2], s.cpu.running, s.cpu.ctxt
    ));
    out.push_str("\n=== 内存 ===\n");
    out.push_str(&format!(
        "已用 {} / {}（{:.1}%）· 可用 {} · 缓存 {} · 交换 {} / {}\n",
        mon::human_bytes(s.mem.used),
        mon::human_bytes(s.mem.total),
        s.mem.used_ratio() * 100.0,
        mon::human_bytes(s.mem.available),
        mon::human_bytes(s.mem.cached),
        mon::human_bytes(s.mem.swap_used),
        mon::human_bytes(s.mem.swap_total)
    ));
    for g in s.gpus.iter() {
        out.push_str(&format!(
            "\n=== 显卡 {} ===\n{}\n",
            g.card,
            mon::gpu_summary_text(g)
        ));
    }
    out.push_str("\n=== 网络 ===\n");
    for i in s.net.iter() {
        out.push_str(&format!(
            "{:<10} {} 收 {}/s 发 {}/s 累计 {}/{}\n",
            i.name,
            i.operstate,
            mon::human_bytes(i.rx_rate as u64),
            mon::human_bytes(i.tx_rate as u64),
            mon::human_bytes(i.rx_bytes),
            mon::human_bytes(i.tx_bytes)
        ));
    }
    out.push_str("\n=== 磁盘 ===\n");
    for d in s.disks.iter() {
        out.push_str(&format!(
            "{:<8} {:<20} 读 {}/s 写 {}/s 利用率 {:.0}%\n",
            d.name,
            d.model,
            mon::human_bytes(d.read_bytes_s as u64),
            mon::human_bytes(d.write_bytes_s as u64),
            d.util
        ));
    }
    out.push_str("\n=== 占用前 10 的进程 ===\n");
    let mut list: Vec<_> = s.processes.iter().collect();
    list.sort_by(|a, b| {
        b.cpu
            .partial_cmp(&a.cpu)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for p in list.iter().take(10) {
        out.push_str(&format!(
            "{:>7} {:<20} {:>6.1}% {:>10} {}\n",
            p.pid,
            p.display_name(),
            p.cpu,
            mon::human_bytes(p.rss),
            p.user
        ));
    }
    out
}
