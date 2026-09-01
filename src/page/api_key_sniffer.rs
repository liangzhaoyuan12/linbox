//! OpenAI 格式 API Key 嗅探页面（展示层 · 仅 UI）。
//!
//! 功能：
//! - 自定义平台（平台名 / Base URL / 端点 / 模型 / Key 正则 / 附加请求头），持久化到本地。
//! - 由正则批量枚举出候选 Key 字典（逻辑在 `utils::sniffer::generate`）。
//! - 以 OpenAI 格式逐个探测，按 HTTP 状态码判定：2xx 有效 / 429 限流 / 401·403 鉴权失败 / …
//! - 有效 Key 写入本地 JSONL 库永久保存，可复制、删除、导出 JSON / CSV。
//! - 并发 + 限速 + 暂停 / 继续 / 停止 + 断点续跑。
//! - 「单次测试」区域：手填 Base URL 与 API Key，直接看返回的状态码与响应体。
//!
//! 页面不做任何计算与 IO，全部委托给 `crate::utils::sniffer`。
//!
//! > **用途提示**：请仅对你自己拥有或已获得明确授权的平台使用本模块。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// `adw::prelude::*` 已经包含了 `gtk::prelude::*`
use adw::prelude::*;

use crate::model::sniffer::{
    fingerprint, builtin_platforms, PlatformConfig, ScanConfig, ValidKeyRecord, Verdict,
    DEFAULT_ENDPOINT, PATTERN_TEMPLATES, CUSTOM_TEMPLATE_INDEX,
};
use crate::utils::sniffer::{
    format_count, generate, load_checkpoint, parse_header_lines, GenerateOptions, ProbeTarget,
    ScanEvent, ScanParams, StopReason,
};
use crate::utils::sniffer::{probe as probe_util, scan as scan_util, store};

pub struct ApiKeySnifferPage {
    root: adw::ToastOverlay,
}

impl ApiKeySnifferPage {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }
}

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 端点下拉选项（最后一项表示使用下面的自定义输入框）。
const ENDPOINTS: &[&str] = &["/chat/completions", "/models", "自定义…"];
/// 「自定义端点」在下拉中的位置。
const ENDPOINT_CUSTOM: u32 = 2;
/// 字典预览展示的条数上限。
const PREVIEW_LIMIT: usize = 200;
/// 关闭详细日志时，每多少条汇总一行。
const SUMMARY_EVERY: usize = 100;

// ---------------------------------------------------------------------------
// 运行期计数
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
struct Counters {
    tested: usize,
    valid: usize,
    unauthorized: usize,
    limited: usize,
    server: usize,
    client: usize,
    notfound: usize,
    network: usize,
}

// ---------------------------------------------------------------------------
// Inner
// ---------------------------------------------------------------------------

struct Inner {
    toast_overlay: adw::ToastOverlay,

    // 平台配置
    platforms: RefCell<Vec<PlatformConfig>>,
    /// 当前已载入的平台名（空 = 尚未保存的新平台）。
    loaded_name: RefCell<String>,
    platform_combo: adw::ComboRow,
    name_row: adw::EntryRow,
    base_row: adw::EntryRow,
    endpoint_combo: adw::ComboRow,
    endpoint_row: adw::EntryRow,
    model_row: adw::EntryRow,
    headers_view: gtk::TextView,
    note_row: adw::EntryRow,

    // 字典
    pattern_row: adw::EntryRow,
    template_combo: adw::ComboRow,
    max_row: adw::SpinRow,
    unbounded_row: adw::SpinRow,
    dict_info: gtk::Label,
    dict_preview: gtk::TextView,
    dict: RefCell<Option<Arc<Vec<String>>>>,

    // 扫描参数
    concurrency_row: adw::SpinRow,
    rate_row: adw::SpinRow,
    timeout_row: adw::SpinRow,
    retry_row: adw::SpinRow,
    resume_switch: adw::SwitchRow,
    persist_switch: adw::SwitchRow,
    verbose_switch: adw::SwitchRow,

    // 执行
    start_btn: gtk::Button,
    pause_btn: gtk::Button,
    stop_btn: gtk::Button,
    reset_cp_btn: gtk::Button,
    progress: gtk::ProgressBar,
    stat_label: gtk::Label,
    resume_hint: gtk::Label,

    // 结果
    valid_list: gtk::ListBox,
    valid_empty: gtk::Label,
    valid_records: RefCell<Vec<ValidKeyRecord>>,
    reveal_switch: adw::SwitchRow,
    valid_count: gtk::Label,

    // 日志
    log_view: gtk::TextView,
    log_lines: Cell<i32>,

    // 单次测试
    t_base_row: adw::EntryRow,
    t_key_row: adw::EntryRow,
    t_model_row: adw::EntryRow,
    t_result_label: gtk::Label,
    t_body_view: gtk::TextView,
    t_send_btn: gtk::Button,

    // 运行期状态
    receiver: RefCell<Option<mpsc::Receiver<ScanEvent>>>,
    control: RefCell<Option<scan_util::Control>>,
    running: Cell<bool>,
    paused: Cell<bool>,
    counters: Cell<Counters>,
    total: Cell<usize>,
    start_index: Cell<usize>,
    started_at: RefCell<Option<Instant>>,
}

// ---------------------------------------------------------------------------
// 通用小工具
// ---------------------------------------------------------------------------

/// 带标题的卡片容器，返回 (外层组, 内容组)，二者是同一对象。
///
/// 内容容器必须是 `adw::PreferencesGroup`（内部是 ListBox）：`ComboRow` 这类
/// 行控件只有在 ListBox 里被点击才会「激活」并弹出下拉；放进普通 Box 不生效。
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

/// 等宽、只读的文本视图（用于预览 / 日志 / 响应体）。
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

fn set_buffer_text(buffer: &gtk::TextBuffer, text: &str) {
    buffer.set_text(text);
}

/// 把若干按钮排成一行。
fn button_row(buttons: &[&gtk::Button]) -> gtk::Box {
    let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    box_.set_margin_top(6);
    box_.set_margin_bottom(6);
    for b in buttons {
        box_.append(*b);
    }
    box_
}

/// 把 Key 打码展示：前 8 后 4，中间省略。
fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 14 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..8].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// 本地时间文本；时钟异常时回落到原始时间戳。
fn time_text(unix: u64) -> String {
    match glib::DateTime::from_unix_local(unix as i64) {
        Ok(dt) => dt.format("%Y-%m-%d %H:%M:%S").map(|s| s.to_string()).unwrap_or_default(),
        Err(_) => unix.to_string(),
    }
}

fn now_text() -> String {
    glib::DateTime::now_local()
        .map(|dt| dt.format("%H:%M:%S").map(|s| s.to_string()).unwrap_or_default())
        .unwrap_or_default()
}

/// 秒数 → `1h02m03s` 形式。
fn duration_text(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h{:02}m{:02}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

// ---------------------------------------------------------------------------
// 构建页面
// ---------------------------------------------------------------------------

pub fn build() -> ApiKeySnifferPage {
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
    let title = gtk::Label::new(Some("API Key 嗅探"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    root_box.append(&title);

    let subtitle = gtk::Label::new(Some(
        "按正则批量生成候选 Key，以 OpenAI 格式逐个探测，2xx 判定有效并永久保存。",
    ));
    subtitle.add_css_class("dim-label");
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_wrap(true);
    root_box.append(&subtitle);

    let notice = gtk::Label::new(Some(
        "请仅对你自己拥有或已获得明确授权的平台使用本模块（自建网关密钥审计、泄漏 Key 复核等）。默认 4 并发 + 5 次/秒，请按目标平台的承受能力调整。",
    ));
    notice.add_css_class("dim-label");
    notice.add_css_class("caption");
    notice.set_halign(gtk::Align::Start);
    notice.set_wrap(true);
    notice.set_margin_top(4);
    root_box.append(&notice);

    // ---------- 平台配置 ----------
    let (platform_card, pc) = card("平台配置", "平台名 / Base URL / 端点 / 模型 / Key 正则 / 附加请求头");
    root_box.append(&platform_card);

    let platform_combo = combo_row("已保存平台", &["（空）"], 0);
    pc.add(&platform_combo);

    let name_row = entry_row("平台名");
    name_row.set_text("自建网关（本地示例）");
    pc.add(&name_row);

    let base_row = entry_row("Base URL");
    base_row.set_text("http://127.0.0.1:8000/v1");
    pc.add(&base_row);

    let endpoint_combo = combo_row("探测端点", ENDPOINTS, 0);
    pc.add(&endpoint_combo);

    let endpoint_row = entry_row("自定义端点");
    endpoint_row.set_text(DEFAULT_ENDPOINT);
    endpoint_row.set_visible(false);
    pc.add(&endpoint_row);

    let model_row = entry_row("模型名");
    model_row.set_text("gpt-3.5-turbo");
    pc.add(&model_row);

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
    pc.add(&headers_box);

    let note_row = entry_row("备注");
    pc.add(&note_row);

    let new_btn = gtk::Button::with_label("新建");
    let save_btn = gtk::Button::with_label("保存");
    save_btn.add_css_class("suggested-action");
    let delete_btn = gtk::Button::with_label("删除");
    delete_btn.add_css_class("destructive-action");
    let restore_btn = gtk::Button::with_label("恢复内置预设");
    pc.add(&button_row(&[&new_btn, &save_btn, &delete_btn, &restore_btn]));

    // ---------- 字典生成 ----------
    let (dict_card, dc) = card(
        "Key 字典生成",
        "由正则枚举出候选 Key；支持 [a-z]、\\d、{n,m}、(a|b) 等语法，^ $ 锚点会被忽略",
    );
    root_box.append(&dict_card);

    let template_labels: Vec<&str> = PATTERN_TEMPLATES.iter().map(|(n, _)| *n).collect();
    let template_combo = combo_row("插入模板", &template_labels, 0);
    dc.add(&template_combo);

    let pattern_row = entry_row("API Key 正则规则");
    pattern_row.set_text(r"^sk-local-[0-9]{6}$");
    pattern_row.set_sensitive(false); // 默认选中预设模板，正则不可编辑
    dc.add(&pattern_row);

    let max_row = spin_row("最大生成条数（密钥空间更大时截断）", 1.0, 2_000_000.0, 10_000.0, 0, 100_000.0);
    dc.add(&max_row);
    let unbounded_row = spin_row("* + {n,} 等无界量词展开上限", 1.0, 8.0, 1.0, 0, 3.0);
    dc.add(&unbounded_row);

    let gen_btn = gtk::Button::with_label("生成并预览");
    gen_btn.add_css_class("suggested-action");
    let clear_dict_btn = gtk::Button::with_label("清空字典");
    dc.add(&button_row(&[&gen_btn, &clear_dict_btn]));

    let dict_info = gtk::Label::new(Some("尚未生成字典"));
    dict_info.add_css_class("dim-label");
    dict_info.set_halign(gtk::Align::Start);
    dict_info.set_wrap(true);
    dict_info.set_selectable(true);
    dc.add(&dict_info);

    let dict_preview = mono_view(150, false);
    let dict_scroll = gtk::ScrolledWindow::new();
    dict_scroll.set_child(Some(&dict_preview));
    dict_scroll.set_min_content_height(120);
    dict_scroll.set_max_content_height(260);
    dc.add(&dict_scroll);

    // ---------- 扫描参数 ----------
    let (scan_card, sc) = card("扫描参数", "并发、限速、超时、重试与断点续跑");
    root_box.append(&scan_card);

    let concurrency_row = spin_row("并发数（线程）", 1.0, 128.0, 1.0, 0, 4.0);
    sc.add(&concurrency_row);
    let rate_row = spin_row("限速（请求 / 秒，0 = 不限）", 0.0, 1000.0, 1.0, 0, 5.0);
    sc.add(&rate_row);
    let timeout_row = spin_row("单次请求超时（秒）", 1.0, 120.0, 1.0, 0, 15.0);
    sc.add(&timeout_row);
    let retry_row = spin_row("失败重试次数（网络错误 / 5xx / 429）", 0.0, 5.0, 1.0, 0, 1.0);
    sc.add(&retry_row);
    let resume_switch = switch_row("断点续跑", "中断后下次从断点继续；配置变更会自动失效", true);
    sc.add(&resume_switch);
    let persist_switch = switch_row("命中即入本地库", "有效 Key 追加写入 JSONL，永久保存", true);
    sc.add(&persist_switch);
    let verbose_switch = switch_row("记录每一条的判定", "关闭时只记录命中、汇总与异常", false);
    sc.add(&verbose_switch);

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
    let reset_cp_btn = gtk::Button::with_label("清除断点");
    rc_.add(&button_row(&[&start_btn, &pause_btn, &stop_btn, &reset_cp_btn]));

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

    let resume_hint = gtk::Label::new(Some(""));
    resume_hint.add_css_class("dim-label");
    resume_hint.add_css_class("caption");
    resume_hint.set_halign(gtk::Align::Start);
    resume_hint.set_wrap(true);
    rc_.add(&resume_hint);

    // ---------- 有效 Key ----------
    let (valid_card, vc) = card("有效 Key（本地库）", "2xx 判定为有效，命中即写入本地 JSONL 文件");
    root_box.append(&valid_card);

    let reveal_switch = switch_row("显示完整 Key（默认打码）", "", false);
    vc.add(&reveal_switch);

    let copy_all_btn = gtk::Button::with_label("复制全部");
    let export_json_btn = gtk::Button::with_label("导出 JSON");
    let export_csv_btn = gtk::Button::with_label("导出 CSV");
    let refresh_valid_btn = gtk::Button::with_label("刷新");
    let clear_valid_btn = gtk::Button::with_label("清空本地库");
    clear_valid_btn.add_css_class("destructive-action");
    vc.add(&button_row(&[
        &refresh_valid_btn,
        &copy_all_btn,
        &export_json_btn,
        &export_csv_btn,
        &clear_valid_btn,
    ]));

    let valid_count = gtk::Label::new(Some("共 0 条"));
    valid_count.add_css_class("dim-label");
    valid_count.set_halign(gtk::Align::Start);
    vc.add(&valid_count);

    let valid_list = gtk::ListBox::new();
    valid_list.add_css_class("boxed-list");
    valid_list.set_selection_mode(gtk::SelectionMode::None);
    vc.add(&valid_list);

    let valid_empty = gtk::Label::new(Some("暂无有效 Key"));
    valid_empty.add_css_class("dim-label");
    valid_empty.set_margin_top(8);
    valid_empty.set_margin_bottom(8);
    vc.add(&valid_empty);

    // ---------- 运行日志 ----------
    let (log_card, lc) = card("运行日志", "");
    root_box.append(&log_card);
    let log_view = mono_view(180, false);
    let log_scroll = gtk::ScrolledWindow::new();
    log_scroll.set_child(Some(&log_view));
    log_scroll.set_min_content_height(140);
    log_scroll.set_max_content_height(320);
    lc.add(&log_scroll);
    let clear_log_btn = gtk::Button::with_label("清空日志");
    lc.add(&button_row(&[&clear_log_btn]));

    // ---------- 单次测试 ----------
    let (test_card, tc) = card(
        "单次测试（手动验证）",
        "不依赖上面的字典，直接拿一个 Base URL + API Key 打一次 OpenAI 格式请求，查看原始返回",
    );
    root_box.append(&test_card);

    let t_base_row = entry_row("Base URL");
    t_base_row.set_text("http://127.0.0.1:8000/v1");
    tc.add(&t_base_row);
    let t_key_row = entry_row("API Key");
    t_key_row.set_text("sk-local-000000");
    tc.add(&t_key_row);
    let t_model_row = entry_row("模型名");
    t_model_row.set_text("gpt-3.5-turbo");
    tc.add(&t_model_row);

    let t_send_btn = gtk::Button::with_label("发送请求");
    t_send_btn.add_css_class("suggested-action");
    let t_fill_btn = gtk::Button::with_label("用当前平台填充");
    tc.add(&button_row(&[&t_send_btn, &t_fill_btn]));

    let t_result_label = gtk::Label::new(Some("尚未测试"));
    t_result_label.set_halign(gtk::Align::Start);
    t_result_label.set_wrap(true);
    t_result_label.set_selectable(true);
    t_result_label.set_margin_top(6);
    tc.add(&t_result_label);

    let t_body_view = mono_view(160, false);
    let t_scroll = gtk::ScrolledWindow::new();
    t_scroll.set_child(Some(&t_body_view));
    t_scroll.set_min_content_height(120);
    t_scroll.set_max_content_height(280);
    tc.add(&t_scroll);

    // ---------- 组装 Inner ----------
    let inner = Rc::new(Inner {
        toast_overlay: toast_overlay.clone(),
        platforms: RefCell::new(Vec::new()),
        loaded_name: RefCell::new(String::new()),
        platform_combo: platform_combo.clone(),
        name_row: name_row.clone(),
        base_row: base_row.clone(),
        endpoint_combo: endpoint_combo.clone(),
        endpoint_row: endpoint_row.clone(),
        model_row: model_row.clone(),
        headers_view: headers_view.clone(),
        note_row: note_row.clone(),
        pattern_row: pattern_row.clone(),
        template_combo: template_combo.clone(),
        max_row: max_row.clone(),
        unbounded_row: unbounded_row.clone(),
        dict_info: dict_info.clone(),
        dict_preview: dict_preview.clone(),
        dict: RefCell::new(None),
        concurrency_row: concurrency_row.clone(),
        rate_row: rate_row.clone(),
        timeout_row: timeout_row.clone(),
        retry_row: retry_row.clone(),
        resume_switch: resume_switch.clone(),
        persist_switch: persist_switch.clone(),
        verbose_switch: verbose_switch.clone(),
        start_btn: start_btn.clone(),
        pause_btn: pause_btn.clone(),
        stop_btn: stop_btn.clone(),
        reset_cp_btn: reset_cp_btn.clone(),
        progress: progress.clone(),
        stat_label: stat_label.clone(),
        resume_hint: resume_hint.clone(),
        valid_list: valid_list.clone(),
        valid_empty: valid_empty.clone(),
        valid_records: RefCell::new(Vec::new()),
        reveal_switch: reveal_switch.clone(),
        valid_count: valid_count.clone(),
        log_view: log_view.clone(),
        log_lines: Cell::new(0),
        t_base_row: t_base_row.clone(),
        t_key_row: t_key_row.clone(),
        t_model_row: t_model_row.clone(),
        t_result_label: t_result_label.clone(),
        t_body_view: t_body_view.clone(),
        t_send_btn: t_send_btn.clone(),
        receiver: RefCell::new(None),
        control: RefCell::new(None),
        running: Cell::new(false),
        paused: Cell::new(false),
        counters: Cell::new(Counters::default()),
        total: Cell::new(0),
        start_index: Cell::new(0),
        started_at: RefCell::new(None),
    });

    // ---------- 信号连接 ----------
    platform_combo.connect_selected_notify(|_| g_on_platform_selected());
    endpoint_combo.connect_selected_notify(|_| g_on_endpoint_changed());
    template_combo.connect_selected_notify(|_| g_on_template_selected());

    new_btn.connect_clicked(|_| g_new_platform());
    save_btn.connect_clicked(|_| g_save_platform());
    delete_btn.connect_clicked(|_| g_delete_platform());
    restore_btn.connect_clicked(|_| g_restore_builtin());

    gen_btn.connect_clicked(|_| g_generate());
    clear_dict_btn.connect_clicked(|_| g_clear_dict());

    start_btn.connect_clicked(|_| g_start());
    pause_btn.connect_clicked(|_| g_toggle_pause());
    stop_btn.connect_clicked(|_| g_stop());
    reset_cp_btn.connect_clicked(|_| g_reset_checkpoint());

    refresh_valid_btn.connect_clicked(|_| g_reload_valid());
    copy_all_btn.connect_clicked(|_| g_copy_all());
    export_json_btn.connect_clicked(|_| g_export(false));
    export_csv_btn.connect_clicked(|_| g_export(true));
    clear_valid_btn.connect_clicked(|_| g_clear_valid());
    reveal_switch.connect_active_notify(|_| g_rebuild_valid());
    clear_log_btn.connect_clicked(|_| g_clear_log());

    t_send_btn.connect_clicked(|_| g_test_one());
    t_fill_btn.connect_clicked(|_| g_fill_test_from_platform());

    // ---------- 初始化 ----------
    {
        let store_data = store::load_store();
        *inner.platforms.borrow_mut() = store_data.platforms;
        inner.apply_scan_config(&store_data.scan);
        inner.rebuild_platform_combo(Some(&store_data.last_platform));
        if let Some(p) = inner.current_platform() {
            inner.load_platform(&p);
        }
        inner.reload_valid();
        inner.refresh_resume_hint();
        inner.update_stats();
    }

    // 主循环里排空扫描事件队列（闭包不带捕获，满足 signal 的 Send 要求）
    glib::source::timeout_add(Duration::from_millis(100), tick);

    // 注册全局强引用：signal 回调要求 Send，无法直接捕获 Rc<Inner>
    INNER.with(|i| *i.borrow_mut() = Some(Rc::clone(&inner)));

    ApiKeySnifferPage { root: toast_overlay }
}

/// 定时排空扫描事件队列。
fn tick() -> glib::ControlFlow {
    g_drain();
    glib::ControlFlow::Continue
}

// ---------------------------------------------------------------------------
// 全局句柄 + 无捕获的转发函数
// ---------------------------------------------------------------------------

thread_local! {
    /// 当前页面的强引用。
    ///
    /// 必须是 `Option<Rc<Inner>>` 而不是 `Weak<Inner>`：否则 `build()` 返回后
    /// `inner` 被销毁，所有回调都会静默失效。
    static INNER: RefCell<Option<Rc<Inner>>> = RefCell::new(None);
}

fn with_inner<F: FnOnce(&Inner)>(f: F) {
    if let Some(inner) = INNER.with(|i| i.borrow().clone()) {
        f(&*inner);
    }
}

fn g_on_platform_selected() {
    with_inner(|i| {
        let idx = i.platform_combo.selected() as usize;
        if let Some(p) = i.platforms.borrow().get(idx).cloned() {
            i.load_platform(&p);
            i.persist_last_platform(&p.name);
        }
    });
}

fn g_on_endpoint_changed() {
    with_inner(|i| {
        i.endpoint_row
            .set_visible(i.endpoint_combo.selected() == ENDPOINT_CUSTOM);
    });
}

fn g_on_template_selected() {
    with_inner(|i| {
        let idx = i.template_combo.selected() as usize;
        let is_custom = idx == CUSTOM_TEMPLATE_INDEX;
        if is_custom {
            // 自定义：清空正则输入框，允许用户编辑
            i.pattern_row.set_sensitive(true);
            i.pattern_row.set_text("");
            i.pattern_row.grab_focus();
        } else if let Some((_, pattern)) = PATTERN_TEMPLATES.get(idx) {
            // 预设模板：填入模板正则，禁止编辑
            i.pattern_row.set_sensitive(false);
            i.pattern_row.set_text(pattern);
        }
    });
}

fn g_new_platform() {
    with_inner(|i| i.new_platform());
}
fn g_save_platform() {
    with_inner(|i| i.save_platform());
}
fn g_delete_platform() {
    with_inner(|i| i.delete_platform());
}
fn g_restore_builtin() {
    with_inner(|i| i.restore_builtin());
}
fn g_generate() {
    with_inner(|i| i.generate_async());
}
fn g_clear_dict() {
    with_inner(|i| i.clear_dict());
}
fn g_start() {
    with_inner(|i| i.start());
}
fn g_toggle_pause() {
    with_inner(|i| i.toggle_pause());
}
fn g_stop() {
    with_inner(|i| i.stop());
}
fn g_reset_checkpoint() {
    with_inner(|i| i.reset_checkpoint());
}
fn g_reload_valid() {
    with_inner(|i| i.reload_valid());
}
fn g_copy_all() {
    with_inner(|i| i.copy_all());
}
fn g_export(csv: bool) {
    if let Some(inner) = INNER.with(|i| i.borrow().clone()) {
        pick_export_path(inner, csv);
    }
}
fn g_clear_valid() {
    with_inner(|i| i.clear_valid());
}
fn g_rebuild_valid() {
    with_inner(|i| i.rebuild_valid_list());
}
fn g_clear_log() {
    with_inner(|i| i.clear_log());
}
fn g_test_one() {
    with_inner(|i| i.test_one());
}
fn g_fill_test_from_platform() {
    with_inner(|i| {
        i.t_base_row.set_text(&i.base_row.text());
        i.t_key_row.set_text("");
        i.t_model_row.set_text(&i.model_row.text());
        i.toast("已填充 Base URL 与模型，请填入 API Key");
    });
}
fn g_drain() {
    with_inner(|i| i.drain_events());
}
fn g_remove_valid(index: usize) {
    with_inner(|i| i.remove_valid(index));
}
fn g_copy_key(index: usize) {
    with_inner(|i| {
        if let Some(rec) = i.valid_records.borrow().get(index) {
            i.valid_list.clipboard().set_text(&rec.key);
            i.toast("Key 已复制到剪贴板");
        }
    });
}

// ---------------------------------------------------------------------------
// Inner 实现
// ---------------------------------------------------------------------------

impl Inner {
    fn toast(&self, msg: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(msg));
    }

    // ---------- 平台 ----------

    fn current_platform(&self) -> Option<PlatformConfig> {
        let idx = self.platform_combo.selected() as usize;
        self.platforms.borrow().get(idx).cloned()
    }

    /// 把一份配置填进表单。
    fn load_platform(&self, p: &PlatformConfig) {
        *self.loaded_name.borrow_mut() = p.name.clone();
        self.name_row.set_text(&p.name);
        self.base_row.set_text(&p.base_url);
        self.model_row.set_text(&p.model);
        self.note_row.set_text(&p.note);

        // 尝试将 pattern 匹配到预设模板，匹配到则锁定、否则进入自定义模式
        let matched = PATTERN_TEMPLATES
            .iter()
            .position(|(_, pat)| *pat == p.pattern);
        match matched {
            Some(idx) => {
                self.template_combo.set_selected(idx as u32);
                self.pattern_row.set_sensitive(false);
                self.pattern_row.set_text(&p.pattern);
            }
            None => {
                self.template_combo.set_selected(CUSTOM_TEMPLATE_INDEX as u32);
                self.pattern_row.set_sensitive(true);
                self.pattern_row.set_text(&p.pattern);
            }
        }
        set_buffer_text(
            &self.headers_view.buffer(),
            &p.headers
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );

        let known = ENDPOINTS.iter().position(|e| *e == p.endpoint).map(|i| i as u32);
        match known {
            Some(i) => {
                self.endpoint_combo.set_selected(i);
                self.endpoint_row.set_visible(false);
                self.endpoint_row.set_text(&p.endpoint);
            }
            None => {
                self.endpoint_combo.set_selected(ENDPOINT_CUSTOM);
                self.endpoint_row.set_text(&p.endpoint);
                self.endpoint_row.set_visible(true);
            }
        }
        // 载入新平台意味着旧断点不再适用
        self.dict.take();
        set_buffer_text(&self.dict_preview.buffer(), "");
        self.dict_info.set_text("切换平台后需重新生成字典");
        self.refresh_resume_hint();
    }

    /// 当前生效的探测端点（下拉选中项，或自定义输入框的内容）。
    fn current_endpoint(&self) -> String {
        if self.endpoint_combo.selected() == ENDPOINT_CUSTOM {
            self.endpoint_row.text().trim().to_string()
        } else {
            ENDPOINTS
                .get(self.endpoint_combo.selected() as usize)
                .copied()
                .unwrap_or(DEFAULT_ENDPOINT)
                .to_string()
        }
    }

    /// 从表单读取一份平台配置。
    fn collect_platform(&self) -> PlatformConfig {
        let endpoint = self.current_endpoint();
        PlatformConfig {
            name: self.name_row.text().trim().to_string(),
            base_url: self.base_row.text().trim().to_string(),
            endpoint,
            model: self.model_row.text().trim().to_string(),
            pattern: self.pattern_row.text().to_string(),
            headers: parse_header_lines(&buffer_text(&self.headers_view.buffer())),
            note: self.note_row.text().to_string(),
        }
    }

    fn new_platform(&self) {
        let base = "新平台";
        let mut name = base.to_string();
        let mut n = 2;
        while self.platforms.borrow().iter().any(|p| p.name == name) {
            name = format!("{base}{n}");
            n += 1;
        }
        *self.loaded_name.borrow_mut() = String::new();
        self.name_row.set_text(&name);
        self.base_row.set_text("https://example.com/v1");
        self.model_row.set_text("gpt-3.5-turbo");
        self.template_combo.set_selected(0);
        // 确保正则填入并锁定（set_selected 在已经是 0 时不会触发信号）
        if let Some((_, pattern)) = PATTERN_TEMPLATES.first() {
            self.pattern_row.set_sensitive(false);
            self.pattern_row.set_text(pattern);
        }
        self.note_row.set_text("");
        set_buffer_text(&self.headers_view.buffer(), "");
        self.endpoint_combo.set_selected(0);
        self.endpoint_row.set_visible(false);
        self.endpoint_row.set_text(DEFAULT_ENDPOINT);
        self.toast("已切换到新平台，填好后点「保存」");
    }

    fn save_platform(&self) {
        let cfg = self.collect_platform();
        if let Err(e) = cfg.validate() {
            self.toast(&format!("保存失败：{e}"));
            return;
        }
        let old_name = self.loaded_name.borrow().clone();
        {
            let mut list = self.platforms.borrow_mut();
            // 改名时要先确认新名字没有和别人撞车
            if old_name != cfg.name && list.iter().any(|p| p.name == cfg.name) {
                self.toast("同名平台已存在，请换一个平台名");
                return;
            }
            match list.iter_mut().find(|p| p.name == old_name) {
                Some(existing) => *existing = cfg.clone(),
                None => list.push(cfg.clone()),
            }
        }

        *self.loaded_name.borrow_mut() = cfg.name.clone();
        self.persist_platforms();
        self.rebuild_platform_combo(Some(&cfg.name));
        self.dict.take();
        set_buffer_text(&self.dict_preview.buffer(), "");
        self.dict_info.set_text("配置已保存，请重新生成字典");
        self.refresh_resume_hint();
        self.toast(&format!("平台「{}」已保存", cfg.name));
    }

    fn delete_platform(&self) {
        let name = self.loaded_name.borrow().clone();
        if name.is_empty() {
            self.toast("当前是未保存的新平台，无需删除");
            return;
        }
        self.platforms.borrow_mut().retain(|p| p.name != name);
        self.persist_platforms();
        *self.loaded_name.borrow_mut() = String::new();
        self.rebuild_platform_combo(None);
        if let Some(p) = self.current_platform() {
            self.load_platform(&p);
        } else {
            self.new_platform();
        }
        self.toast(&format!("已删除平台「{name}」"));
    }

    fn restore_builtin(&self) {
        *self.platforms.borrow_mut() = builtin_platforms();
        self.persist_platforms();
        self.rebuild_platform_combo(None);
        if let Some(p) = self.current_platform() {
            self.load_platform(&p);
        }
        self.toast("已恢复内置预设平台");
    }

    /// 重建平台下拉，并尽量选中 `select` 指定的平台。
    fn rebuild_platform_combo(&self, select: Option<&str>) {
        let names: Vec<String> = self.platforms.borrow().iter().map(|p| p.name.clone()).collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let model = gtk::StringList::new(if refs.is_empty() { &["（空）"] } else { &refs });
        self.platform_combo.set_model(Some(&model));
        let target = select
            .and_then(|s| names.iter().position(|n| n == s))
            .unwrap_or(0) as u32;
        self.platform_combo.set_selected(target);
        // set_selected 到同一索引不会触发 notify，这里手动同步一次
        if let Some(p) = self.platforms.borrow().get(target as usize).cloned() {
            *self.loaded_name.borrow_mut() = p.name.clone();
        }
    }

    fn persist_platforms(&self) {
        let mut data = store::load_store();
        data.platforms = self.platforms.borrow().clone();
        data.scan = self.collect_scan_config();
        if let Err(e) = store::save_store(&data) {
            self.toast(&format!("配置写入失败：{e}"));
        }
    }

    fn persist_last_platform(&self, name: &str) {
        let mut data = store::load_store();
        data.last_platform = name.to_string();
        let _ = store::save_store(&data);
    }

    // ---------- 运行参数 ----------

    fn collect_scan_config(&self) -> ScanConfig {
        ScanConfig {
            concurrency: self.concurrency_row.value() as usize,
            rate_per_sec: self.rate_row.value(),
            timeout_secs: self.timeout_row.value() as u64,
            retries: self.retry_row.value() as usize,
            max_candidates: self.max_row.value() as usize,
            unbounded_repeat: self.unbounded_row.value() as usize,
            resume: self.resume_switch.is_active(),
            persist_valid: self.persist_switch.is_active(),
            log_limit: 400,
        }
    }

    fn apply_scan_config(&self, cfg: &ScanConfig) {
        self.concurrency_row.set_value(cfg.concurrency as f64);
        self.rate_row.set_value(cfg.rate_per_sec);
        self.timeout_row.set_value(cfg.timeout_secs as f64);
        self.retry_row.set_value(cfg.retries as f64);
        self.max_row.set_value(cfg.max_candidates as f64);
        self.unbounded_row.set_value(cfg.unbounded_repeat as f64);
        self.resume_switch.set_active(cfg.resume);
        self.persist_switch.set_active(cfg.persist_valid);
    }

    // ---------- 字典 ----------

    fn generate_options(&self) -> GenerateOptions {
        GenerateOptions {
            max_results: self.max_row.value().max(1.0) as usize,
            unbounded_repeat: self.unbounded_row.value().max(1.0) as usize,
        }
    }

    /// 在后台线程生成字典（大字典也不卡界面）。
    fn generate_async(&self) {
        let pattern = self.pattern_row.text().to_string();
        let opts = self.generate_options();
        self.dict_info.set_text("正在生成字典…");
        // `Arc<Mutex<..>>` 才能跨线程搬运（`Rc` 不是 Send）
        let slot: Arc<Mutex<Option<Result<crate::utils::sniffer::Dictionary, String>>>> =
            Arc::new(Mutex::new(None));
        GENERATE_SLOT.with(|s| *s.borrow_mut() = Some(Arc::clone(&slot)));
        std::thread::spawn(move || {
            let result = generate(&pattern, &opts);
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(result);
            }
            glib::source::idle_add(tick_generate_done);
        });
    }

    /// 后台生成完成后回到主线程：把结果填进 `dict`。
    fn generate_done(&self) {
        let result = GENERATE_SLOT
            .with(|s| s.borrow_mut().take())
            .and_then(|slot| slot.lock().ok().and_then(|mut g| g.take()));
        match result {
            Some(Ok(dict)) => {
                let preview: Vec<&str> = dict.keys.iter().take(PREVIEW_LIMIT).map(|s| s.as_str()).collect();
                let mut text = preview.join("\n");
                if dict.keys.len() > PREVIEW_LIMIT {
                    text.push_str(&format!(
                        "\n… 其余 {} 条未展示",
                        format_count((dict.keys.len() - PREVIEW_LIMIT) as u128)
                    ));
                }
                set_buffer_text(&self.dict_preview.buffer(), &text);

                let mut info = format!(
                    "密钥空间 {} · 已生成 {} 条",
                    format_count(dict.total_space),
                    format_count(dict.keys.len() as u128)
                );
                if dict.truncated {
                    info.push_str("（已达上限，被截断）");
                }
                if dict.dropped > 0 {
                    info.push_str(&format!(
                        " · 剔除 {} 条含控制字符的候选",
                        format_count(dict.dropped as u128)
                    ));
                }
                self.dict_info.set_text(&info);

                let len = dict.keys.len();
                *self.dict.borrow_mut() = Some(Arc::new(dict.keys));
                self.log(&format!("字典生成完成：{len} 条候选"));
                self.refresh_resume_hint();
            }
            Some(Err(e)) => {
                self.dict_info.set_text(&format!("生成失败：{e}"));
                self.toast(&format!("正则解析失败：{e}"));
            }
            None => self.dict_info.set_text("生成失败：内部状态丢失"),
        }
    }

    fn clear_dict(&self) {
        self.dict.take();
        set_buffer_text(&self.dict_preview.buffer(), "");
        self.dict_info.set_text("尚未生成字典");
        self.refresh_resume_hint();
    }

    fn dict_len(&self) -> usize {
        self.dict.borrow().as_ref().map(|d| d.len()).unwrap_or(0)
    }

    /// 当前配置对应的字典指纹（配置一变，断点即失效）。
    fn current_fingerprint(&self) -> String {
        let cfg = self.collect_platform();
        let scan = self.collect_scan_config();
        fingerprint(&[
            &cfg.name,
            &cfg.base_url,
            &cfg.endpoint,
            &cfg.model,
            &cfg.pattern,
            &scan.max_candidates.to_string(),
            &scan.unbounded_repeat.to_string(),
        ])
    }

    // ---------- 断点 ----------

    fn refresh_resume_hint(&self) {
        if !self.resume_switch.is_active() {
            self.resume_hint.set_text("断点续跑已关闭");
            return;
        }
        match load_checkpoint() {
            None => self.resume_hint.set_text("当前无断点，将从头开始"),
            Some(cp) => {
                if cp.fingerprint == self.current_fingerprint() && self.name_row.text().trim() == cp.platform {
                    self.resume_hint.set_text(&format!(
                        "发现断点（{}）：上次测到 {}/{}，命中 {} 条；开始时将从第 {} 条继续",
                        time_text(cp.updated_at),
                        format_count(cp.cursor as u128),
                        format_count(cp.total as u128),
                        cp.valid,
                        format_count((cp.cursor + 1) as u128),
                    ));
                } else {
                    self.resume_hint.set_text("已存在的断点与当前配置不匹配，开始时会自动忽略");
                }
            }
        }
    }

    fn reset_checkpoint(&self) {
        store::clear_checkpoint();
        self.refresh_resume_hint();
        self.toast("断点已清除");
    }

    // ---------- 扫描 ----------

    fn start(&self) {
        if self.running.get() {
            self.toast("扫描正在进行中");
            return;
        }
        let cfg = self.collect_platform();
        if let Err(e) = cfg.validate() {
            self.toast(&format!("无法开始：{e}"));
            return;
        }
        if self.dict_len() == 0 {
            self.toast("请先点「生成并预览」生成 Key 字典");
            return;
        }
        let scan = self.collect_scan_config();
        let keys = match self.dict.borrow().clone() {
            Some(k) => k,
            None => {
                self.toast("字典为空");
                return;
            }
        };
        let fp = self.current_fingerprint();

        // 断点续跑
        let mut start_index = 0usize;
        if scan.resume {
            if let Some(cp) = load_checkpoint() {
                if cp.platform == cfg.name && cp.fingerprint == fp {
                    if cp.cursor >= keys.len() {
                        self.toast("该字典已跑完，请清除断点或修改生成规则");
                        return;
                    }
                    start_index = cp.cursor;
                    self.log(&format!(
                        "断点续跑：从第 {} 条继续（上次命中 {} 条）",
                        format_count((cp.cursor + 1) as u128),
                        cp.valid
                    ));
                } else {
                    self.log("断点与当前配置不匹配，已忽略并从头开始");
                    store::clear_checkpoint();
                }
            }
        } else {
            store::clear_checkpoint();
        }

        self.persist_platforms();

        let target = ProbeTarget {
            base_url: cfg.base_url.clone(),
            endpoint: cfg.endpoint.clone(),
            model: cfg.model.clone(),
            headers: cfg.headers.clone(),
            timeout: Duration::from_secs(scan.timeout_secs.max(1)),
        };

        let params = ScanParams {
            platform: cfg.name.clone(),
            fingerprint: fp,
            target,
            keys: Arc::clone(&keys),
            start_index,
            concurrency: scan.concurrency.max(1),
            rate_per_sec: scan.rate_per_sec,
            retries: scan.retries,
            persist_valid: scan.persist_valid,
            write_checkpoint: scan.resume,
        };

        let (tx, rx) = mpsc::channel();
        *self.receiver.borrow_mut() = Some(rx);
        let control = scan_util::start(params, tx);
        *self.control.borrow_mut() = Some(control);

        self.running.set(true);
        self.paused.set(false);
        self.counters.set(Counters::default());
        self.total.set(keys.len());
        self.start_index.set(start_index);
        *self.started_at.borrow_mut() = Some(Instant::now());

        self.start_btn.set_sensitive(false);
        self.pause_btn.set_sensitive(true);
        self.pause_btn.set_label("暂停");
        self.stop_btn.set_sensitive(true);
        self.progress.set_fraction(0.0);

        self.log(&format!(
            "开始扫描：平台「{}」，字典 {} 条，从第 {} 条开始，并发 {}，限速 {} 次/秒",
            cfg.name,
            format_count(keys.len() as u128),
            format_count((start_index + 1) as u128),
            scan.concurrency.max(1),
            if scan.rate_per_sec > 0.0 {
                scan.rate_per_sec.to_string()
            } else {
                "不限".to_string()
            }
        ));
        self.update_stats();
    }

    fn toggle_pause(&self) {
        if !self.running.get() {
            return;
        }
        let next = !self.paused.get();
        self.paused.set(next);
        if let Some(c) = self.control.borrow().as_ref() {
            c.set_paused(next);
        }
        self.pause_btn.set_label(if next { "继续" } else { "暂停" });
        self.log(if next { "已暂停" } else { "已继续" });
    }

    fn stop(&self) {
        if !self.running.get() {
            return;
        }
        if self.paused.get() {
            // 暂停中先恢复，否则工作线程卡在暂停轮询里看不到停止信号
            self.paused.set(false);
            if let Some(c) = self.control.borrow().as_ref() {
                c.set_paused(false);
            }
        }
        if let Some(c) = self.control.borrow().as_ref() {
            c.stop();
        }
        self.log("正在停止…（等待在途请求结束）");
    }

    /// 主循环定时调用：排空事件队列并更新界面。
    fn drain_events(&self) {
        if !self.running.get() {
            return;
        }
        let mut batch: Vec<ScanEvent> = Vec::new();
        if let Some(rx) = self.receiver.borrow().as_ref() {
            while let Ok(ev) = rx.try_recv() {
                batch.push(ev);
                if batch.len() >= 2000 {
                    break;
                }
            }
        }
        if batch.is_empty() {
            self.update_progress();
            return;
        }

        let mut counters = self.counters.get();
        let verbose = self.verbose_switch.is_active();
        let mut new_records: Vec<ValidKeyRecord> = Vec::new();

        for ev in batch {
            match ev {
                ScanEvent::Started { total, start_index } => {
                    self.log(&format!(
                        "引擎已启动：共 {} 条，起点 #{}",
                        format_count(total as u128),
                        start_index
                    ));
                }
                ScanEvent::Result {
                    index,
                    key,
                    outcome,
                    elapsed_ms,
                    attempts,
                } => {
                    counters.tested += 1;
                    match outcome.verdict {
                        Verdict::Valid => counters.valid += 1,
                        Verdict::Unauthorized => counters.unauthorized += 1,
                        Verdict::RateLimited => counters.limited += 1,
                        Verdict::NotFound => counters.notfound += 1,
                        Verdict::ServerError => counters.server += 1,
                        Verdict::ClientError => counters.client += 1,
                        Verdict::NetworkError => counters.network += 1,
                    }
                    if outcome.verdict.is_valid() {
                        let record = ValidKeyRecord {
                            platform: self.name_row.text().to_string(),
                            base_url: self.base_row.text().to_string(),
                            endpoint: self.current_endpoint(),
                            model: self.model_row.text().to_string(),
                            key: key.clone(),
                            status: outcome.status,
                            latency_ms: outcome.latency_ms,
                            found_at: probe_util::now_unix(),
                            snippet: outcome.body.clone(),
                        };
                        new_records.push(record);
                        self.log(&format!(
                            "✔ 命中 #{}：{} → HTTP {} · {} ms · {}",
                            index,
                            mask_key(&key),
                            outcome.status,
                            outcome.latency_ms,
                            outcome.detail
                        ));
                    } else if verbose {
                        self.log(&format!(
                            "#{} {} → {} {}（{} ms{}）· {}",
                            index,
                            mask_key(&key),
                            outcome.status,
                            outcome.verdict.label(),
                            elapsed_ms,
                            if attempts > 0 {
                                format!("，重试 {} 次", attempts)
                            } else {
                                String::new()
                            },
                            outcome.detail
                        ));
                    } else if outcome.verdict == Verdict::NetworkError {
                        self.log(&format!(
                            "网络错误 #{}：{}（请检查 Base URL / 网络 / 超时设置）",
                            index, outcome.detail
                        ));
                    } else if counters.tested % SUMMARY_EVERY == 0 {
                        self.log(&format!(
                            "进度：已测 {} 条 · 有效 {} · 鉴权失败 {} · 限流 {} · 网络错误 {}",
                            format_count(counters.tested as u128),
                            counters.valid,
                            counters.unauthorized,
                            counters.limited,
                            counters.network
                        ));
                    }
                }
                ScanEvent::Log(text) => self.log(&text),
                ScanEvent::Finished {
                    reason,
                    tested,
                    valid,
                } => {
                    self.log(&format!(
                        "扫描{}：本次测 {} 条，命中 {} 条",
                        reason.label(),
                        format_count(tested as u128),
                        valid
                    ));
                    if reason == StopReason::Completed {
                        store::clear_checkpoint();
                        self.toast(&format!("扫描完成，命中 {valid} 条有效 Key"));
                    } else {
                        self.toast(&format!("已停止，本次命中 {valid} 条"));
                    }
                    self.finish_run();
                    self.refresh_resume_hint();
                }
            }
        }

        self.counters.set(counters);
        for rec in new_records {
            let index = {
                let mut records = self.valid_records.borrow_mut();
                records.push(rec.clone());
                records.len() - 1
            };
            self.append_valid_row(&rec, index);
            self.update_valid_count();
        }
        self.update_progress();
        self.update_stats();
    }

    fn finish_run(&self) {
        self.running.set(false);
        self.paused.set(false);
        *self.control.borrow_mut() = None;
        *self.receiver.borrow_mut() = None;
        self.start_btn.set_sensitive(true);
        self.pause_btn.set_sensitive(false);
        self.pause_btn.set_label("暂停");
        self.stop_btn.set_sensitive(false);
    }

    fn update_progress(&self) {
        let total = self.total.get();
        if total == 0 {
            self.progress.set_fraction(0.0);
            self.progress.set_text(Some("0 / 0"));
            return;
        }
        let done = (self.start_index.get() + self.counters.get().tested).min(total);
        let frac = done as f64 / total as f64;
        self.progress.set_fraction(frac);
        self.progress
            .set_text(Some(&format!("{:.1}% · {} / {}", frac * 100.0, format_count(done as u128), format_count(total as u128))));
    }

    fn update_stats(&self) {
        if !self.running.get() && self.counters.get().tested == 0 {
            self.stat_label.set_text("尚未开始");
            return;
        }
        let c = self.counters.get();
        let mut text = format!(
            "已测 {} · 有效 {} · 鉴权失败 {} · 限流 {} · 端点 404 {} · 服务端错误 {} · 其它 4xx {} · 网络错误 {}",
            format_count(c.tested as u128),
            c.valid,
            c.unauthorized,
            c.limited,
            c.notfound,
            c.server,
            c.client,
            c.network
        );
        if let Some(started) = self.started_at.borrow().as_ref() {
            let secs = started.elapsed().as_secs();
            if secs > 0 && c.tested > 0 {
                let rate = c.tested as f64 / secs as f64;
                let remaining = (self.total.get().saturating_sub(self.start_index.get() + c.tested)) as f64;
                let eta = if rate > 0.0 {
                    duration_text((remaining / rate).ceil() as u64)
                } else {
                    "—".to_string()
                };
                text.push_str(&format!(
                    "\n用时 {} · 平均 {:.1} 次/秒 · 预计剩余 {}",
                    duration_text(secs),
                    rate,
                    eta
                ));
            }
        }
        if c.tested > 0 {
            text.push_str(&format!(
                " · 命中率 {:.4}%",
                c.valid as f64 / c.tested as f64 * 100.0
            ));
        }
        self.stat_label.set_text(&text);
    }

    // ---------- 有效 Key 列表 ----------

    fn reload_valid(&self) {
        let records = store::load_valid();
        *self.valid_records.borrow_mut() = records;
        self.rebuild_valid_list();
    }

    fn rebuild_valid_list(&self) {
        while let Some(row) = self.valid_list.first_child() {
            self.valid_list.remove(&row);
        }
        let records = self.valid_records.borrow().clone();
        for (index, rec) in records.iter().enumerate() {
            self.append_valid_row(rec, index);
        }
        self.update_valid_count();
    }

    fn append_valid_row(&self, rec: &ValidKeyRecord, index: usize) {
        let reveal = self.reveal_switch.is_active();
        let masked = mask_key(&rec.key);
        let row = adw::ActionRow::new();
        row.set_title(if reveal { &rec.key } else { &masked });
        row.set_title_selectable(true);
        row.set_subtitle(&format!(
            "{} · {} · HTTP {} · {} ms · {}",
            rec.platform,
            rec.model,
            rec.status,
            rec.latency_ms,
            time_text(rec.found_at)
        ));
        row.set_subtitle_selectable(true);
        row.set_activatable(false);

        let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
        copy.set_tooltip_text(Some("复制 Key"));
        copy.add_css_class("flat");
        copy.set_valign(gtk::Align::Center);
        let remove = gtk::Button::from_icon_name("user-trash-symbolic");
        remove.set_tooltip_text(Some("从本地库删除"));
        remove.add_css_class("flat");
        remove.set_valign(gtk::Align::Center);
        row.add_suffix(&copy);
        row.add_suffix(&remove);

        self.valid_list.append(&row);

        copy.connect_clicked(move |_| g_copy_key(index));
        remove.connect_clicked(move |_| g_remove_valid(index));
    }

    fn update_valid_count(&self) {
        let n = self.valid_records.borrow().len();
        self.valid_count
            .set_text(&format!("共 {} 条 · 本地库：{:?}", n, store::valid_keys_path()));
        self.valid_empty.set_visible(n == 0);
        self.valid_list.set_visible(n > 0);
    }

    fn remove_valid(&self, index: usize) {
        let rec = match self.valid_records.borrow().get(index).cloned() {
            Some(r) => r,
            None => return,
        };
        match store::delete_valid(&rec.platform, &rec.key) {
            Ok(()) => {
                self.valid_records.borrow_mut().remove(index);
                self.rebuild_valid_list();
                self.toast("已从本地库删除");
            }
            Err(e) => self.toast(&format!("删除失败：{e}")),
        }
    }

    fn copy_all(&self) {
        let text: String = self
            .valid_records
            .borrow()
            .iter()
            .map(|r| r.key.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            self.toast("本地库为空");
            return;
        }
        self.valid_list.clipboard().set_text(&text);
        self.toast("全部 Key 已复制到剪贴板");
    }

    fn clear_valid(&self) {
        match store::clear_valid() {
            Ok(()) => {
                self.valid_records.borrow_mut().clear();
                self.rebuild_valid_list();
                self.toast("本地库已清空");
            }
            Err(e) => self.toast(&format!("清空失败：{e}")),
        }
    }

    // ---------- 日志 ----------

    fn log(&self, line: &str) {
        let buffer = self.log_view.buffer();
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, &format!("[{}] {}\n", now_text(), line));

        let limit = 400i32;
        let lines = buffer.line_count();
        if lines > limit {
            if let Some(mut cut) = buffer.iter_at_line(lines - limit) {
                let mut start = buffer.start_iter();
                buffer.delete(&mut start, &mut cut);
            }
        }
        self.log_lines.set(buffer.line_count().min(limit));
        let mut end = buffer.end_iter();
        self.log_view
            .scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
    }

    fn clear_log(&self) {
        self.log_view.buffer().set_text("");
        self.log_lines.set(0);
    }

    // ---------- 单次测试 ----------

    fn test_one(&self) {
        let base = self.t_base_row.text().trim().to_string();
        let key = self.t_key_row.text().trim().to_string();
        let model = self.t_model_row.text().trim().to_string();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            self.toast("Base URL 必须以 http:// 或 https:// 开头");
            return;
        }
        if key.is_empty() {
            self.toast("请填写 API Key");
            return;
        }
        self.t_send_btn.set_sensitive(false);
        self.t_result_label.set_text("请求中…");
        set_buffer_text(&self.t_body_view.buffer(), "");

        let target = ProbeTarget {
            base_url: base,
            endpoint: DEFAULT_ENDPOINT.to_string(),
            model,
            headers: Vec::new(),
            timeout: Duration::from_secs(self.timeout_row.value().max(1.0) as u64),
        };

        let slot: Arc<Mutex<Option<(crate::model::sniffer::ProbeOutcome, String, &'static str)>>> =
            Arc::new(Mutex::new(None));
        TEST_SLOT.with(|s| *s.borrow_mut() = Some(Arc::clone(&slot)));

        std::thread::spawn(move || {
            let agent = ureq::AgentBuilder::new().build();
            let url = probe_util::join_url(&target.base_url, &target.endpoint);
            let method = target.method().as_str();
            let outcome = probe_util::probe(&agent, &target, &key);
            if let Ok(mut guard) = slot.lock() {
                *guard = Some((outcome, url, method));
            }
            glib::source::idle_add(tick_test_done);
        });
    }

    /// 单次测试的后台结果回到主线程。
    fn test_done(&self) {
        self.t_send_btn.set_sensitive(true);
        let slot = TEST_SLOT.with(|s| s.borrow_mut().take());
        let (outcome, url, method) = match slot.and_then(|s| s.lock().ok().and_then(|mut g| g.take())) {
            Some(v) => v,
            None => {
                self.t_result_label.set_text("测试失败：内部状态丢失");
                return;
            }
        };
        self.t_result_label.set_text(&format!(
            "{} {} → HTTP {} {}（{} ms）\n判定：{}\n说明：{}",
            method,
            url,
            outcome.status,
            outcome.status_text,
            outcome.latency_ms,
            outcome.verdict.label(),
            outcome.detail
        ));
        set_buffer_text(&self.t_body_view.buffer(), &outcome.body);
    }
}

thread_local! {
    /// 后台字典生成的结果中转槽（放在 `Arc<Mutex<..>>` 里才能跨线程搬运）。
    static GENERATE_SLOT: RefCell<Option<Arc<Mutex<Option<Result<crate::utils::sniffer::Dictionary, String>>>>>> =
        RefCell::new(None);
    /// 单次测试的结果中转槽：(探测结果, 实际请求的 URL, 请求方法)。
    static TEST_SLOT: RefCell<Option<Arc<Mutex<Option<(crate::model::sniffer::ProbeOutcome, String, &'static str)>>>>> =
        RefCell::new(None);
}

fn tick_generate_done() -> glib::ControlFlow {
    g_generate_done();
    glib::ControlFlow::Break
}

fn tick_test_done() -> glib::ControlFlow {
    g_test_done();
    glib::ControlFlow::Break
}

fn g_generate_done() {
    with_inner(|i| i.generate_done());
}

fn g_test_done() {
    with_inner(|i| i.test_done());
}

// ---------------------------------------------------------------------------
// 文件选择（GIO 回调，可捕获 Rc<Inner>）
// ---------------------------------------------------------------------------

/// 弹出保存对话框，把命中记录导出成 JSON / CSV。
fn pick_export_path(inner: Rc<Inner>, csv: bool) {
    let win = inner
        .valid_list
        .root()
        .and_then(|r| r.downcast::<gtk::Window>().ok());
    let dialog = gtk::FileChooserDialog::builder()
        .title(if csv { "导出为 CSV" } else { "导出为 JSON" })
        .action(gtk::FileChooserAction::Save)
        .modal(true)
        .build();
    if let Some(w) = &win {
        dialog.set_transient_for(Some(w));
    }
    dialog.add_button("取消", gtk::ResponseType::Cancel);
    dialog.add_button("保存", gtk::ResponseType::Accept);
    dialog.set_current_name(if csv { "api_keys.csv" } else { "api_keys.json" });

    dialog.connect_response(move |d, resp| {
        if resp == gtk::ResponseType::Accept {
            if let Some(file) = d.file().and_then(|f| f.path()) {
                let path = file.to_string_lossy().to_string();
                let records = inner.valid_records.borrow().clone();
                match store::export_valid(&records, &path, csv) {
                    Ok(()) => inner.toast(&format!("已导出 {} 条到 {path}", records.len())),
                    Err(e) => inner.toast(&format!("导出失败：{e}")),
                }
            }
        }
        d.destroy();
    });
    dialog.show();
}
