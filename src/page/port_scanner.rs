//! 端口扫描与服务识别页面（展示层 · 仅 UI）。
//!
//! TCP connect scan + 应用层协议指纹探测。
//! 支持 30+ 种常见服务的自动识别。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use adw::prelude::*;

use crate::model::port_scanner::{PortResult, ScanConfig, ScanEvent};
use crate::utils::port_scanner;

pub struct PortScannerPage {
    root: adw::ToastOverlay,
}

impl PortScannerPage {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }
}

thread_local! {
    static INNER: RefCell<Option<std::rc::Weak<Inner>>> = const { RefCell::new(None) };
}

fn self_rc() -> Option<Rc<Inner>> {
    INNER.with(|i| i.borrow().as_ref().and_then(|w| w.upgrade()))
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn card(title: &str, subtitle: &str) -> (adw::PreferencesGroup, adw::PreferencesGroup) {
    let g = adw::PreferencesGroup::new();
    g.set_title(title);
    if !subtitle.is_empty() {
        g.set_description(Some(subtitle));
    }
    g.add_css_class("card");
    g.set_margin_top(8);
    g.set_margin_bottom(8);
    g.set_margin_start(12);
    g.set_margin_end(12);
    (g.clone(), g)
}

fn entry_row(title: &str) -> adw::EntryRow {
    adw::EntryRow::builder().title(title).build()
}

fn spin_row(title: &str, min: f64, max: f64, step: f64, digits: u32, value: f64) -> adw::SpinRow {
    let adj = gtk::Adjustment::new(value, min, max, step, step * 10.0, 0.0);
    let r = adw::SpinRow::builder()
        .adjustment(&adj)
        .climb_rate(0.5)
        .digits(digits)
        .build();
    r.set_title(title);
    r
}

fn switch_row(title: &str, subtitle: &str, active: bool) -> adw::SwitchRow {
    let r = adw::SwitchRow::new();
    r.set_title(title);
    if !subtitle.is_empty() {
        r.set_subtitle(subtitle);
    }
    r.set_active(active);
    r
}

fn button_row(buttons: &[&gtk::Button]) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    b.set_margin_top(6);
    b.set_margin_bottom(6);
    for btn in buttons {
        b.append(*btn);
    }
    b
}

fn now_text() -> String {
    glib::DateTime::now_local()
        .map(|dt| dt.format("%H:%M:%S").map(|s| s.to_string()).unwrap_or_default())
        .unwrap_or_default()
}

fn duration_text(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h{:02}m{:02}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// 协议族对应的 CSS 类。
fn family_css(fam: &str) -> &'static str {
    match fam {
        "TCP" => "accent",
        "UDP" => "success",
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// Inner
// ---------------------------------------------------------------------------

struct Inner {
    toast_overlay: adw::ToastOverlay,

    target_row: adw::EntryRow,
    port_start_row: adw::SpinRow,
    port_end_row: adw::SpinRow,
    custom_ports_row: adw::EntryRow,
    concurrency_row: adw::SpinRow,
    timeout_row: adw::SpinRow,
    service_switch: adw::SwitchRow,

    start_btn: gtk::Button,
    stop_btn: gtk::Button,
    progress: gtk::ProgressBar,
    stat_label: gtk::Label,

    result_list: gtk::ListBox,
    result_empty: gtk::Label,
    open_count: gtk::Label,

    log_view: gtk::TextView,

    receiver: RefCell<Option<mpsc::Receiver<ScanEvent>>>,
    control: RefCell<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>>,
    running: Cell<bool>,
    started_at: RefCell<Option<Instant>>,
    open_ports: Cell<usize>,
}

impl Inner {
    fn append_log(&self, text: &str) {
        let buf = self.log_view.buffer();
        let mut end = buf.end_iter();
        buf.insert(&mut end, &format!("[{}] {text}\n", now_text()));
        let line_count = buf.line_count();
        if line_count > 450 {
            let start_iter = buf.start_iter();
            if let Some(mut trim) = buf.iter_at_line(400) {
                trim.backward_char();
                buf.delete(&mut start_iter.clone(), &mut trim);
            }
        }
        let end = buf.end_iter();
        self.log_view
            .scroll_mark_onscreen(&buf.create_mark(None, &end, false));
    }

    fn set_running(&self, running: bool) {
        self.running.set(running);
        self.start_btn.set_sensitive(!running);
        self.stop_btn.set_sensitive(running);
    }

    fn build_result_row(r: &PortResult) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        b.set_margin_start(8);
        b.set_margin_end(8);
        b.set_margin_top(3);
        b.set_margin_bottom(3);

        // 端口号
        let port_label = gtk::Label::new(Some(&format!("{:>5}", r.port)));
        port_label.set_width_chars(6);
        port_label.set_xalign(1.0);
        port_label.add_css_class("monospace");
        b.append(&port_label);

        // 协议族
        let fam_label = gtk::Label::new(Some(r.family.label()));
        fam_label.set_width_chars(4);
        fam_label.set_xalign(0.5);
        let css = family_css(r.family.label());
        if !css.is_empty() {
            fam_label.add_css_class(css);
        }
        fam_label.add_css_class("monospace");
        b.append(&fam_label);

        // 状态
        let status_icon = gtk::Image::from_icon_name("object-select-symbolic");
        status_icon.add_css_class("success");
        b.append(&status_icon);

        // 服务
        let service_label = gtk::Label::new(Some(&r.service));
        service_label.set_width_chars(16);
        service_label.set_xalign(0.0);
        b.append(&service_label);

        // 协议
        let proto_label = gtk::Label::new(Some(&r.protocol));
        proto_label.set_width_chars(14);
        proto_label.set_xalign(0.0);
        proto_label.add_css_class("dim-label");
        b.append(&proto_label);

        // 延迟
        let latency_label = gtk::Label::new(Some(&format!("{} ms", r.latency_ms)));
        latency_label.set_width_chars(8);
        latency_label.set_xalign(1.0);
        latency_label.add_css_class("dim-label");
        latency_label.add_css_class("monospace");
        b.append(&latency_label);

        // Banner（版本信息）
        if !r.banner.is_empty() {
            let banner_label = gtk::Label::new(Some(&r.banner));
            banner_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            banner_label.set_hexpand(true);
            banner_label.add_css_class("caption");
            b.append(&banner_label);
        }

        row.set_child(Some(&b));
        row
    }
}

// ---------------------------------------------------------------------------
// 全局操作
// ---------------------------------------------------------------------------

fn g_start() {
    let Some(s) = self_rc() else { return };
    if s.running.get() {
        return;
    }

    let target = s.target_row.text().to_string();
    let custom_ports_str = s.custom_ports_row.text().to_string();
    let custom_ports: Vec<u16> = if !custom_ports_str.trim().is_empty() {
        custom_ports_str
            .split(|c: char| c == ',' || c == ' ')
            .filter_map(|p| p.trim().parse().ok())
            .collect()
    } else {
        Vec::new()
    };

    let config = ScanConfig {
        target,
        port_start: s.port_start_row.value() as u16,
        port_end: s.port_end_row.value() as u16,
        custom_ports,
        concurrency: s.concurrency_row.value() as usize,
        timeout_ms: s.timeout_row.value() as u64,
        service_detection: s.service_switch.is_active(),
    };

    if let Err(e) = config.validate() {
        let toast = adw::Toast::new(&e);
        s.toast_overlay.add_toast(toast);
        return;
    }

    let port_count = config.ports().len();

    // 清空旧结果
    while let Some(child) = s.result_list.first_child() {
        s.result_list.remove(&child);
    }
    s.open_ports.set(0);
    s.open_count.set_text("开放 0 个端口");

    let (tx, rx) = mpsc::channel();
    *s.receiver.borrow_mut() = Some(rx);
    s.set_running(true);
    *s.started_at.borrow_mut() = Some(Instant::now());

    s.append_log(&format!("开始扫描 {}，共 {} 个端口", config.target, port_count));

    // 启动扫描线程
    std::thread::spawn(move || {
        port_scanner::start_scan(config, tx);
    });
}

fn g_stop() {
    let Some(s) = self_rc() else { return };
    // 断开 receiver 让扫描线程的 send 失败
    *s.receiver.borrow_mut() = None;
    s.set_running(false);
    s.append_log("已停止");
}

// ---------------------------------------------------------------------------
// 事件循环
// ---------------------------------------------------------------------------

fn tick() -> glib::ControlFlow {
    let Some(s) = self_rc() else {
        return glib::ControlFlow::Continue;
    };

    let mut scan_finished = None;
    if let Some(rx) = s.receiver.borrow().as_ref() {
        for _ in 0..500 {
            match rx.try_recv() {
                Ok(ScanEvent::Resolving { target }) => {
                    s.append_log(&format!("正在解析 {target}..."));
                }
                Ok(ScanEvent::Resolved { ip, port_count }) => {
                    s.append_log(&format!("目标 IP：{ip}，{port_count} 个端口"));
                }
                Ok(ScanEvent::PortResult(r)) => {
                    if r.open {
                        let row = Inner::build_result_row(&r);
                        s.result_list.append(&row);
                        s.result_empty.set_visible(false);
                        let count = s.open_ports.get() + 1;
                        s.open_ports.set(count);
                        s.open_count.set_text(&format!("开放 {count} 个端口"));
                    }
                }
                Ok(ScanEvent::Progress { done }) => {
                    s.progress
                        .set_text(Some(&format!("{done} / ...")));
                }
                Ok(ScanEvent::Finished { total, open: _, elapsed_secs }) => {
                    scan_finished = Some((total, elapsed_secs));
                    break;
                }
                Ok(ScanEvent::Log(msg)) => {
                    s.append_log(&msg);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    scan_finished = Some((0, 0));
                    break;
                }
            }
        }
    }

    if let Some((total, elapsed)) = scan_finished {
        s.set_running(false);
        *s.receiver.borrow_mut() = None;
        let open = s.open_ports.get();
        s.progress.set_fraction(1.0);
        s.stat_label.set_text(&format!(
            "扫描完成 — {total} 个端口，开放 {open} 个，耗时 {}",
            duration_text(elapsed)
        ));
        s.append_log(&format!(
            "扫描完成：{total} 个端口，开放 {open} 个，耗时 {}",
            duration_text(elapsed)
        ));
    } else if s.running.get() {
        if let Some(ref started_at) = *s.started_at.borrow() {
            let elapsed = started_at.elapsed().as_secs();
            let open = s.open_ports.get();
            s.stat_label
                .set_text(&format!("扫描中... 已发现 {open} 个开放端口，已运行 {}", duration_text(elapsed)));
        }
    }

    glib::ControlFlow::Continue
}

// ---------------------------------------------------------------------------
// 构建页面
// ---------------------------------------------------------------------------

pub fn build() -> PortScannerPage {
    let toast_overlay = adw::ToastOverlay::new();
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_propagate_natural_height(true);
    scroller.set_propagate_natural_width(true);

    let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root_box.set_margin_top(12);
    root_box.set_margin_bottom(12);
    root_box.set_margin_start(12);
    root_box.set_margin_end(12);
    scroller.set_child(Some(&root_box));
    toast_overlay.set_child(Some(&scroller));

    // 标题
    let title = gtk::Label::new(Some("端口扫描与服务识别"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    root_box.append(&title);

    let subtitle = gtk::Label::new(Some(
        "TCP connect scan + 应用层协议指纹探测，支持 30+ 种服务自动识别。",
    ));
    subtitle.add_css_class("dim-label");
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_wrap(true);
    root_box.append(&subtitle);

    // ---------- 目标配置 ----------
    let (target_card, tc) = card("目标配置", "");
    root_box.append(&target_card);

    let target_row = entry_row("目标地址（IP 或域名）");
    tc.add(&target_row);

    let port_start_row = spin_row("起始端口", 1.0, 65535.0, 1.0, 0, 1.0);
    tc.add(&port_start_row);
    let port_end_row = spin_row("结束端口", 1.0, 65535.0, 1.0, 0, 65535.0);
    tc.add(&port_end_row);

    let custom_ports_row = entry_row("自定义端口（逗号分隔，优先于范围）");
    tc.add(&custom_ports_row);

    // ---------- 扫描参数 ----------
    let (param_card, pc) = card("扫描参数", "");
    root_box.append(&param_card);

    let concurrency_row = spin_row("并发数", 1.0, 10000.0, 10.0, 0, 200.0);
    pc.add(&concurrency_row);
    let timeout_row = spin_row("超时（毫秒）", 100.0, 30000.0, 100.0, 0, 2000.0);
    pc.add(&timeout_row);
    let service_switch = switch_row(
        "服务/协议识别",
        "对开放端口发送探测包识别具体服务（较慢但更精确）",
        true,
    );
    pc.add(&service_switch);

    // ---------- 执行 ----------
    let (run_card, rc) = card("执行", "");
    root_box.append(&run_card);

    let start_btn = gtk::Button::with_label("开始扫描");
    start_btn.add_css_class("suggested-action");
    let stop_btn = gtk::Button::with_label("停止");
    stop_btn.add_css_class("destructive-action");
    stop_btn.set_sensitive(false);
    rc.add(&button_row(&[&start_btn, &stop_btn]));

    let progress = gtk::ProgressBar::new();
    progress.set_show_text(true);
    progress.set_margin_top(6);
    rc.add(&progress);

    let stat_label = gtk::Label::new(Some("尚未开始"));
    stat_label.set_halign(gtk::Align::Start);
    stat_label.set_wrap(true);
    stat_label.set_selectable(true);
    stat_label.set_margin_top(6);
    rc.add(&stat_label);

    let open_count = gtk::Label::new(Some(""));
    open_count.add_css_class("dim-label");
    open_count.set_halign(gtk::Align::Start);
    rc.add(&open_count);

    // ---------- 结果 ----------
    let (result_card, rsc) = card("扫描结果", "端口 | 协议 | 服务 | 协议族 | 延迟 | 版本");
    root_box.append(&result_card);

    // 表头
    let header_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    header_box.set_margin_start(8);
    header_box.set_margin_end(8);
    header_box.set_margin_bottom(4);
    let h_port = gtk::Label::new(Some("  端口"));
    h_port.set_width_chars(6);
    h_port.set_xalign(1.0);
    h_port.add_css_class("caption");
    h_port.add_css_class("dim-label");
    header_box.append(&h_port);
    let h_proto = gtk::Label::new(Some("协议"));
    h_proto.set_width_chars(4);
    h_proto.set_xalign(0.5);
    h_proto.add_css_class("caption");
    h_proto.add_css_class("dim-label");
    header_box.append(&h_proto);
    let h_status = gtk::Label::new(Some("  "));
    h_status.set_width_chars(3);
    header_box.append(&h_status);
    let h_svc = gtk::Label::new(Some("服务"));
    h_svc.set_width_chars(16);
    h_svc.set_xalign(0.0);
    h_svc.add_css_class("caption");
    h_svc.add_css_class("dim-label");
    header_box.append(&h_svc);
    let h_app = gtk::Label::new(Some("应用协议"));
    h_app.set_width_chars(14);
    h_app.set_xalign(0.0);
    h_app.add_css_class("caption");
    h_app.add_css_class("dim-label");
    header_box.append(&h_app);
    let h_lat = gtk::Label::new(Some("延迟"));
    h_lat.set_width_chars(8);
    h_lat.set_xalign(1.0);
    h_lat.add_css_class("caption");
    h_lat.add_css_class("dim-label");
    header_box.append(&h_lat);
    let h_ver = gtk::Label::new(Some("版本 / Banner"));
    h_ver.set_hexpand(true);
    h_ver.set_xalign(0.0);
    h_ver.add_css_class("caption");
    h_ver.add_css_class("dim-label");
    header_box.append(&h_ver);
    rsc.add(&header_box);

    let result_list = gtk::ListBox::new();
    result_list.add_css_class("boxed-list");
    result_list.set_selection_mode(gtk::SelectionMode::None);
    let result_scroll = gtk::ScrolledWindow::new();
    result_scroll.set_child(Some(&result_list));
    result_scroll.set_min_content_height(120);
    result_scroll.set_max_content_height(500);
    rsc.add(&result_scroll);

    let result_empty = gtk::Label::new(Some("暂无结果 — 点「开始扫描」"));
    result_empty.add_css_class("dim-label");
    result_empty.set_margin_top(8);
    result_empty.set_margin_bottom(8);
    rsc.add(&result_empty);

    // ---------- 日志 ----------
    let (log_card, lc) = card("运行日志", "");
    root_box.append(&log_card);
    let log_view = gtk::TextView::new();
    log_view.set_monospace(true);
    log_view.set_editable(false);
    log_view.set_wrap_mode(gtk::WrapMode::WordChar);
    log_view.set_left_margin(8);
    log_view.set_right_margin(8);
    log_view.set_top_margin(6);
    log_view.set_bottom_margin(6);
    log_view.set_size_request(-1, 120);
    let log_scroll = gtk::ScrolledWindow::new();
    log_scroll.set_child(Some(&log_view));
    log_scroll.set_min_content_height(100);
    log_scroll.set_max_content_height(240);
    lc.add(&log_scroll);

    // ---------- 组装 ----------
    let inner = Rc::new(Inner {
        toast_overlay: toast_overlay.clone(),
        target_row: target_row.clone(),
        port_start_row: port_start_row.clone(),
        port_end_row: port_end_row.clone(),
        custom_ports_row: custom_ports_row.clone(),
        concurrency_row: concurrency_row.clone(),
        timeout_row: timeout_row.clone(),
        service_switch: service_switch.clone(),
        start_btn: start_btn.clone(),
        stop_btn: stop_btn.clone(),
        progress: progress.clone(),
        stat_label: stat_label.clone(),
        open_count: open_count.clone(),
        result_list: result_list.clone(),
        result_empty: result_empty.clone(),
        log_view: log_view.clone(),
        receiver: RefCell::new(None),
        control: RefCell::new(None),
        running: Cell::new(false),
        started_at: RefCell::new(None),
        open_ports: Cell::new(0),
    });

    start_btn.connect_clicked(|_| g_start());
    stop_btn.connect_clicked(|_| g_stop());

    INNER.with(|i| *i.borrow_mut() = Some(Rc::downgrade(&inner)));

    glib::source::timeout_add(Duration::from_millis(80), tick);

    PortScannerPage { root: toast_overlay }
}

pub fn shutdown() {
    INNER.with(|i| {
        *i.borrow_mut() = None;
    });
}
