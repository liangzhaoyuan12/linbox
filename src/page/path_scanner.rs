//! 路径扫描页面（展示层 · 仅 UI）。
//!
//! 功能：
//! - 对目标域名 / IP 的路径进行枚举探测，发现隐藏目录、文件、接口。
//! - 支持内置 100+ 常见路径字典，或自定义字典文件（每行一条路径）。
//! - 并发扫描 + 限速 + 暂停/继续/停止。
//! - 按状态码、响应体大小过滤结果。
//! - 显示命中路径的状态码、大小、重定向、延迟。
//!
//! > **用途提示**：请仅对你自己拥有或已获得明确授权的目标使用本模块。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use adw::prelude::*;

use crate::model::path_scanner::{
    ExcludeSizeRange, PathProbeResult, PathScanConfig, PathScanEvent, BUILTIN_PATHS,
};
use crate::utils::path_scanner::scan::{self, PathScanParams};

pub struct PathScannerPage {
    root: adw::ToastOverlay,
}

impl PathScannerPage {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }
}

// ---------------------------------------------------------------------------
// 全局 TLS
// ---------------------------------------------------------------------------

thread_local! {
    static INNER: std::cell::RefCell<Option<std::rc::Weak<Inner>>> = const { std::cell::RefCell::new(None) };
}

fn self_rc() -> Option<Rc<Inner>> {
    INNER.with(|i| i.borrow().as_ref().and_then(|w| w.upgrade()))
}

// ---------------------------------------------------------------------------
// 通用小工具
// ---------------------------------------------------------------------------

fn card(title: &str, subtitle: &str) -> (adw::PreferencesGroup, adw::PreferencesGroup) {
    let group = adw::PreferencesGroup::new();
    group.set_title(title);
    if !subtitle.is_empty() {
        group.set_description(Some(subtitle));
    }
    group.add_css_class("card");
    group.set_margin_top(8);
    group.set_margin_bottom(8);
    group.set_margin_start(12);
    group.set_margin_end(12);
    (group.clone(), group.clone())
}

fn entry_row(title: &str) -> adw::EntryRow {
    adw::EntryRow::builder().title(title).build()
}

fn spin_row(title: &str, min: f64, max: f64, step: f64, digits: u32, value: f64) -> adw::SpinRow {
    let adj = gtk::Adjustment::new(value, min, max, step, step * 10.0, 0.0);
    let row = adw::SpinRow::builder()
        .adjustment(&adj)
        .climb_rate(0.5)
        .digits(digits)
        .build();
    row.set_title(title);
    row
}

fn combo_row(title: &str, labels: &[&str], init: u32) -> adw::ComboRow {
    let model = gtk::StringList::new(labels);
    adw::ComboRow::builder()
        .model(&model)
        .selected(init)
        .title(title)
        .build()
}

fn switch_row(title: &str, subtitle: &str, active: bool) -> adw::SwitchRow {
    let row = adw::SwitchRow::new();
    row.set_title(title);
    if !subtitle.is_empty() {
        row.set_subtitle(subtitle);
    }
    row.set_active(active);
    row
}

fn mono_view(min_height: i32, editable: bool) -> gtk::TextView {
    let tv = gtk::TextView::new();
    tv.set_monospace(true);
    tv.set_editable(editable);
    tv.set_wrap_mode(gtk::WrapMode::WordChar);
    tv.set_left_margin(8);
    tv.set_right_margin(8);
    tv.set_top_margin(6);
    tv.set_bottom_margin(6);
    if min_height > 0 {
        tv.set_size_request(-1, min_height);
    }
    tv
}

fn buffer_text(buffer: &gtk::TextBuffer) -> String {
    buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string()
}

fn button_row(buttons: &[&gtk::Button]) -> gtk::Box {
    let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    box_.set_margin_top(6);
    box_.set_margin_bottom(6);
    for b in buttons {
        box_.append(*b);
    }
    box_
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

/// 格式化字节大小。
fn human_size(bytes: usize) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// 状态码对应的颜色 CSS 类。
fn status_css(status: u16) -> &'static str {
    match status {
        200..=299 => "success",
        300..=399 => "",
        400..=499 => "error",
        500..=599 => "warning",
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// Inner
// ---------------------------------------------------------------------------

struct Inner {
    toast_overlay: adw::ToastOverlay,

    // 目标配置
    target_url_row: adw::EntryRow,
    method_combo: adw::ComboRow,
    headers_view: gtk::TextView,
    builtin_switch: adw::SwitchRow,
    custom_path_row: adw::EntryRow,
    custom_path_chooser: gtk::Button,

    // 扫描参数
    concurrency_row: adw::SpinRow,
    rate_row: adw::SpinRow,
    timeout_row: adw::SpinRow,
    retry_row: adw::SpinRow,
    exclude_status_row: adw::EntryRow,
    exclude_size_row: adw::EntryRow,

    // 执行
    start_btn: gtk::Button,
    pause_btn: gtk::Button,
    stop_btn: gtk::Button,
    progress: gtk::ProgressBar,
    stat_label: gtk::Label,

    // 结果
    result_list: gtk::ListBox,
    result_empty: gtk::Label,
    result_count: gtk::Label,
    results: RefCell<Vec<PathProbeResult>>,

    // 日志
    log_view: gtk::TextView,
    log_lines: Cell<i32>,

    // 运行期
    receiver: RefCell<Option<mpsc::Receiver<PathScanEvent>>>,
    control: RefCell<Option<scan::Control>>,
    running: Cell<bool>,
    paused: Cell<bool>,
    started_at: RefCell<Option<Instant>>,
}

impl Inner {
    /// 读取 UI 表单 → 生成扫描配置。
    fn read_config(&self) -> PathScanConfig {
        let headers = parse_header_lines(&buffer_text(&self.headers_view.buffer()));
        PathScanConfig {
            target_url: self.target_url_row.text().to_string(),
            use_builtin: self.builtin_switch.is_active(),
            custom_wordlist_path: self.custom_path_row.text().to_string(),
            headers,
            method: match self.method_combo.selected() {
                0 => "GET",
                1 => "HEAD",
                _ => "GET",
            }
            .to_string(),
            concurrency: self.concurrency_row.value() as usize,
            rate_per_sec: self.rate_row.value(),
            timeout_secs: self.timeout_row.value() as u64,
            retries: self.retry_row.value() as usize,
            exclude_status: self.exclude_status_row.text().to_string(),
            exclude_size: self.exclude_size_row.text().to_string(),
            log_limit: 400,
        }
    }

    /// 向日志追加一行。
    fn append_log(&self, text: &str) {
        let buf = self.log_view.buffer();
        let mut end = buf.end_iter();
        buf.insert(&mut end, &format!("[{}] {text}\n", now_text()));
        let line_count = buf.line_count();
        // 截断超过上限的行
        let limit = 400;
        if line_count > limit + 50 {
            let start_iter = buf.start_iter();
            if let Some(mut end_trim) = buf.iter_at_line(limit) {
                end_trim.backward_char(); // 删到换行符前
                buf.delete(&mut start_iter.clone(), &mut end_trim);
            }
        }
        // 滚动到底部
        let end = buf.end_iter();
        self.log_view.scroll_mark_onscreen(&buf.create_mark(None, &end, false));
        self.log_lines.set(buf.line_count());
    }

    fn set_running_state(&self, running: bool) {
        self.running.set(running);
        self.paused.set(false);
        self.start_btn.set_sensitive(!running);
        self.pause_btn.set_sensitive(running);
        self.stop_btn.set_sensitive(running);
        if !running {
            self.pause_btn.set_label("暂停");
        }
    }

    /// 构建一条结果行。
    fn build_result_row(r: &PathProbeResult) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        box_.set_margin_start(8);
        box_.set_margin_end(8);
        box_.set_margin_top(4);
        box_.set_margin_bottom(4);

        // 状态码
        let status_label = gtk::Label::new(Some(&format!("{}", r.status)));
        status_label.set_width_chars(4);
        status_label.set_xalign(1.0);
        let css = status_css(r.status);
        if !css.is_empty() {
            status_label.add_css_class(css);
        }
        status_label.add_css_class("monospace");
        box_.append(&status_label);

        // 路径
        let path_label = gtk::Label::new(Some(&r.path));
        path_label.set_hexpand(true);
        path_label.set_xalign(0.0);
        path_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        path_label.set_selectable(true);
        box_.append(&path_label);

        // 大小
        let size_label = gtk::Label::new(Some(&human_size(r.size)));
        size_label.set_width_chars(10);
        size_label.set_xalign(1.0);
        size_label.add_css_class("dim-label");
        box_.append(&size_label);

        // 延迟
        let latency_label = gtk::Label::new(Some(&format!("{} ms", r.latency_ms)));
        latency_label.set_width_chars(8);
        latency_label.set_xalign(1.0);
        latency_label.add_css_class("dim-label");
        box_.append(&latency_label);

        // 重定向
        if !r.redirect_url.is_empty() {
            let redirect_label = gtk::Label::new(Some(&format!("-> {}", r.redirect_url)));
            redirect_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            redirect_label.add_css_class("accent");
            box_.append(&redirect_label);
        }

        row.set_child(Some(&box_));
        row
    }

    fn rebuild_result_list(&self) {
        while let Some(child) = self.result_list.first_child() {
            self.result_list.remove(&child);
        }
        let results = self.results.borrow();
        if results.is_empty() {
            self.result_empty.set_visible(true);
            self.result_count.set_text("共 0 条");
        } else {
            self.result_empty.set_visible(false);
            for r in results.iter() {
                self.result_list.append(&Self::build_result_row(r));
            }
            self.result_count.set_text(&format!("共 {} 条", results.len()));
        }
    }
}

fn parse_header_lines(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            let k = k.trim();
            if k.is_empty() {
                return None;
            }
            Some((k.to_string(), v.trim().to_string()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 全局操作函数（满足 signal 回调的 Send 要求）
// ---------------------------------------------------------------------------

fn g_start() {
    let Some(s) = self_rc() else { return };
    if s.running.get() {
        return;
    }

    let config = s.read_config();
    if let Err(e) = config.validate() {
        let toast = adw::Toast::new(&e);
        s.toast_overlay.add_toast(toast);
        return;
    }

    // 构建字典
    let mut paths: Vec<String> = Vec::new();
    if config.use_builtin {
        paths.extend(BUILTIN_PATHS.iter().map(|p| p.to_string()));
    }
    if !config.custom_wordlist_path.trim().is_empty() {
        let p = config.custom_wordlist_path.trim();
        match std::fs::read_to_string(p) {
            Ok(content) => {
                for line in content.lines() {
                    let line = line.trim();
                    if !line.is_empty() && !line.starts_with('#') {
                        let line = if line.starts_with('/') {
                            line.to_string()
                        } else {
                            format!("/{line}")
                        };
                        paths.push(line);
                    }
                }
            }
            Err(e) => {
                let toast = adw::Toast::new(&format!("读取字典文件失败：{e}"));
                s.toast_overlay.add_toast(toast);
                return;
            }
        }
    }

    if paths.is_empty() {
        let toast = adw::Toast::new("字典为空");
        s.toast_overlay.add_toast(toast);
        return;
    }

    // 去重
    paths.sort();
    paths.dedup();

    // 清空旧结果
    s.results.borrow_mut().clear();
    s.rebuild_result_list();

    let exclude_status = config.parse_exclude_status();
    let exclude_size = ExcludeSizeRange::parse(&config.exclude_size);

    let target_url = config.target_url.trim().trim_end_matches('/').to_string();
    let timeout = Duration::from_secs(config.timeout_secs);

    let (tx, rx) = mpsc::channel();
    *s.receiver.borrow_mut() = Some(rx);

    let control = scan::start(
        PathScanParams {
            base_url: target_url,
            paths: std::sync::Arc::new(paths.clone()),
            method: config.method,
            headers: config.headers,
            concurrency: config.concurrency,
            rate_per_sec: config.rate_per_sec,
            timeout,
            retries: config.retries,
            exclude_status,
            exclude_size,
        },
        tx,
    );

    *s.control.borrow_mut() = Some(control);
    s.set_running_state(true);
    *s.started_at.borrow_mut() = Some(Instant::now());

    s.append_log(&format!("开始扫描，字典 {} 条路径", paths.len()));
}

fn g_toggle_pause() {
    let Some(s) = self_rc() else { return };
    if !s.running.get() {
        return;
    }
    let new_paused = !s.paused.get();
    s.paused.set(new_paused);
    if let Some(ref ctrl) = *s.control.borrow() {
        ctrl.set_paused(new_paused);
    }
    s.pause_btn.set_label(if new_paused { "继续" } else { "暂停" });
    s.append_log(if new_paused { "已暂停" } else { "继续扫描" });
}

fn g_stop() {
    let Some(s) = self_rc() else { return };
    if let Some(ref ctrl) = *s.control.borrow() {
        ctrl.stop();
    }
    s.set_running_state(false);
    s.append_log("已停止");
}

fn g_clear_log() {
    let Some(s) = self_rc() else { return };
    s.log_view.buffer().set_text("");
    s.log_lines.set(0);
}

// ---------------------------------------------------------------------------
// 事件循环
// ---------------------------------------------------------------------------

fn tick() -> glib::ControlFlow {
    let Some(s) = self_rc() else {
        return glib::ControlFlow::Continue;
    };

    let mut finished = None;
    if let Some(rx) = s.receiver.borrow().as_ref() {
        // 每 tick 排空最多 200 条事件，避免 UI 卡顿
        for _ in 0..200 {
            match rx.try_recv() {
                Ok(PathScanEvent::Started { total }) => {
                    s.append_log(&format!("引擎已启动，共 {total} 条路径"));
                    s.progress.set_fraction(0.0);
                }
                Ok(PathScanEvent::Result { outcome, index }) => {
                    s.results.borrow_mut().push(outcome.clone());
                    s.rebuild_result_list();
                }
                Ok(PathScanEvent::Log(msg)) => {
                    s.append_log(&msg);
                }
                Ok(PathScanEvent::Finished { tested, found }) => {
                    finished = Some((tested, found));
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    finished = Some((0, 0));
                    break;
                }
            }
        }
    }

    // 更新进度条和统计
    if let Some((tested, found)) = finished {
        s.set_running_state(false);
        *s.receiver.borrow_mut() = None;
        *s.control.borrow_mut() = None;

        let elapsed = s
            .started_at
            .borrow()
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        s.progress.set_fraction(1.0);
        s.stat_label.set_text(&format!(
            "扫描完成 — 已探测 {tested} 条，命中 {found} 条，耗时 {}",
            duration_text(elapsed)
        ));
        s.append_log(&format!(
            "扫描完成：已探测 {tested} 条，命中 {found} 条，耗时 {}",
            duration_text(elapsed)
        ));
    } else if s.running.get() {
        // 更新进行中的进度
        let results = s.results.borrow();
        let tested = results.len();
        // 简单估算：用已收到的结果数 / 日志行数来近似
        // 实际进度由 finished 事件精确设置
        if let Some(ref started_at) = *s.started_at.borrow() {
            let elapsed = started_at.elapsed().as_secs();
            s.stat_label.set_text(&format!(
                "扫描中... 已命中 {tested} 条，已运行 {}",
                duration_text(elapsed)
            ));
        }
    }

    glib::ControlFlow::Continue
}

// ---------------------------------------------------------------------------
// 构建页面
// ---------------------------------------------------------------------------

pub fn build() -> PathScannerPage {
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

    // ---------- 标题 ----------
    let title = gtk::Label::new(Some("路径扫描"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    root_box.append(&title);

    let subtitle = gtk::Label::new(Some(
        "对目标域名或 IP 的路径进行枚举探测，发现隐藏的目录、文件、接口。",
    ));
    subtitle.add_css_class("dim-label");
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_wrap(true);
    root_box.append(&subtitle);

    let notice = gtk::Label::new(Some(
        "请仅对你自己拥有或已获得明确授权的目标使用本模块。内置字典包含 100+ 常见路径。",
    ));
    notice.add_css_class("dim-label");
    notice.add_css_class("caption");
    notice.set_halign(gtk::Align::Start);
    notice.set_wrap(true);
    notice.set_margin_top(4);
    root_box.append(&notice);

    // ---------- 目标配置 ----------
    let (target_card, tc) = card("目标配置", "填写目标地址与字典来源");
    root_box.append(&target_card);

    let target_url_row = entry_row("目标 URL（如 http://example.com）");
    target_url_row.set_text("http://");
    tc.add(&target_url_row);

    let method_combo = combo_row("HTTP 方法", &["GET", "HEAD"], 0);
    tc.add(&method_combo);

    let headers_view = mono_view(64, true);
    let headers_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let headers_label = gtk::Label::new(Some("附加请求头（可选，每行 Key: Value）"));
    headers_label.add_css_class("dim-label");
    headers_label.set_halign(gtk::Align::Start);
    headers_box.append(&headers_label);
    let headers_scroll = gtk::ScrolledWindow::new();
    headers_scroll.set_child(Some(&headers_view));
    headers_scroll.set_min_content_height(64);
    headers_scroll.set_max_content_height(140);
    headers_box.append(&headers_scroll);
    tc.add(&headers_box);

    // 字典来源
    let builtin_switch = switch_row("使用内置字典", "包含 /admin、/api、/.git 等 100+ 常见路径", true);
    tc.add(&builtin_switch);

    let custom_path_row = entry_row("自定义字典文件路径（每行一条路径）");
    tc.add(&custom_path_row);

    let custom_path_chooser = gtk::Button::with_label("选择文件…");
    tc.add(&custom_path_chooser);

    // ---------- 扫描参数 ----------
    let (scan_card, sc) = card("扫描参数", "并发、限速与过滤");
    root_box.append(&scan_card);

    let concurrency_row = spin_row("并发数（线程）", 1.0, 512.0, 1.0, 0, 10.0);
    sc.add(&concurrency_row);

    let rate_row = spin_row("限速（请求/秒，0 = 不限）", 0.0, 10000.0, 1.0, 0, 50.0);
    sc.add(&rate_row);

    let timeout_row = spin_row("单次请求超时（秒）", 1.0, 120.0, 1.0, 0, 10.0);
    sc.add(&timeout_row);

    let retry_row = spin_row("失败重试次数", 0.0, 5.0, 1.0, 0, 1.0);
    sc.add(&retry_row);

    let exclude_status_row = entry_row("排除的状态码（逗号分隔）");
    exclude_status_row.set_text("404");
    sc.add(&exclude_status_row);

    let exclude_size_row = entry_row("排除的响应大小（如 0-500，空 = 不限）");
    sc.add(&exclude_size_row);

    // ---------- 执行 ----------
    let (run_card, rc_) = card("执行", "");
    root_box.append(&run_card);

    let start_btn = gtk::Button::with_label("开始扫描");
    start_btn.add_css_class("suggested-action");
    let pause_btn = gtk::Button::with_label("暂停");
    pause_btn.set_sensitive(false);
    let stop_btn = gtk::Button::with_label("停止");
    stop_btn.add_css_class("destructive-action");
    stop_btn.set_sensitive(false);
    rc_.add(&button_row(&[&start_btn, &pause_btn, &stop_btn]));

    let progress = gtk::ProgressBar::new();
    progress.set_show_text(true);
    progress.set_margin_top(6);
    rc_.add(&progress);

    let stat_label = gtk::Label::new(Some("尚未开始"));
    stat_label.set_halign(gtk::Align::Start);
    stat_label.set_wrap(true);
    stat_label.set_selectable(true);
    stat_label.set_margin_top(6);
    rc_.add(&stat_label);

    // ---------- 结果 ----------
    let (result_card, rc) = card("扫描结果", "命中的路径（非排除状态码 + 非排除大小）");
    root_box.append(&result_card);

    let result_count = gtk::Label::new(Some("共 0 条"));
    result_count.add_css_class("dim-label");
    result_count.set_halign(gtk::Align::Start);
    rc.add(&result_count);

    let result_list = gtk::ListBox::new();
    result_list.add_css_class("boxed-list");
    result_list.set_selection_mode(gtk::SelectionMode::None);
    let result_scroll = gtk::ScrolledWindow::new();
    result_scroll.set_child(Some(&result_list));
    result_scroll.set_min_content_height(120);
    result_scroll.set_max_content_height(400);
    rc.add(&result_scroll);

    let result_empty = gtk::Label::new(Some("暂无结果 — 点「开始扫描」"));
    result_empty.add_css_class("dim-label");
    result_empty.set_margin_top(8);
    result_empty.set_margin_bottom(8);
    rc.add(&result_empty);

    // ---------- 日志 ----------
    let (log_card, lc) = card("运行日志", "");
    root_box.append(&log_card);
    let log_view = mono_view(150, false);
    let log_scroll = gtk::ScrolledWindow::new();
    log_scroll.set_child(Some(&log_view));
    log_scroll.set_min_content_height(120);
    log_scroll.set_max_content_height(280);
    lc.add(&log_scroll);
    let clear_log_btn = gtk::Button::with_label("清空日志");
    lc.add(&button_row(&[&clear_log_btn]));

    // ---------- 组装 Inner ----------
    let inner = Rc::new(Inner {
        toast_overlay: toast_overlay.clone(),
        target_url_row: target_url_row.clone(),
        method_combo: method_combo.clone(),
        headers_view: headers_view.clone(),
        builtin_switch: builtin_switch.clone(),
        custom_path_row: custom_path_row.clone(),
        custom_path_chooser: custom_path_chooser.clone(),
        concurrency_row: concurrency_row.clone(),
        rate_row: rate_row.clone(),
        timeout_row: timeout_row.clone(),
        retry_row: retry_row.clone(),
        exclude_status_row: exclude_status_row.clone(),
        exclude_size_row: exclude_size_row.clone(),
        start_btn: start_btn.clone(),
        pause_btn: pause_btn.clone(),
        stop_btn: stop_btn.clone(),
        progress: progress.clone(),
        stat_label: stat_label.clone(),
        result_list: result_list.clone(),
        result_empty: result_empty.clone(),
        result_count: result_count.clone(),
        results: RefCell::new(Vec::new()),
        log_view: log_view.clone(),
        log_lines: Cell::new(0),
        receiver: RefCell::new(None),
        control: RefCell::new(None),
        running: Cell::new(false),
        paused: Cell::new(false),
        started_at: RefCell::new(None),
    });

    // ---------- 信号连接 ----------
    start_btn.connect_clicked(|_| g_start());
    pause_btn.connect_clicked(|_| g_toggle_pause());
    stop_btn.connect_clicked(|_| g_stop());
    clear_log_btn.connect_clicked(|_| g_clear_log());

    // 文件选择器
    custom_path_chooser.connect_clicked(|_| {
        let dialog = gtk::FileDialog::builder().title("选择字典文件").build();
        let s_weak = self_rc().map(|s| Rc::downgrade(&s));
        dialog.open(
            None::<&gtk::Window>,
            gtk::gio::Cancellable::NONE,
            move |result| {
                if let Ok(file) = result {
                    if let Some(path) = file.path() {
                        if let Some(s) = s_weak.as_ref().and_then(|w| w.upgrade()) {
                            s.custom_path_row.set_text(&path.to_string_lossy());
                        }
                    }
                }
            },
        );
    });

    INNER.with(|i| *i.borrow_mut() = Some(Rc::downgrade(&inner)));

    // 主循环排空事件
    glib::source::timeout_add(Duration::from_millis(100), tick);

    PathScannerPage { root: toast_overlay }
}

/// 关闭时清理（供 app.connect_shutdown 调用）。
pub fn shutdown() {
    INNER.with(|i| {
        if let Some(weak) = i.borrow().as_ref() {
            if let Some(s) = weak.upgrade() {
                if let Some(ctrl) = s.control.borrow().as_ref() {
                    ctrl.stop();
                }
            }
        }
        *i.borrow_mut() = None;
    });
}
