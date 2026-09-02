//! OpenAI 格式 API Key 嗅探页面（展示层 · 仅 UI）。
//!
//! 功能：
//! - 自定义平台（平台名 / Base URL / 端点 / 模型 / Key 正则 / 附加请求头），持久化到本地。
//! - 由正则批量枚举出候选 Key 字典（逻辑在 `utils::sniffer::generate`）。
//! - 以 OpenAI 格式逐个探测，按 HTTP 状态码判定：2xx 有效 / 429 限流 / 401·403 鉴权失败 / …
//! - 有效 Key 写入本地 SQLite 库永久保存（sqlx），可复制、删除、导出 JSON / CSV。
//! - tokio 异步引擎：并发 + 限速 + 暂停 / 继续 / 停止 + 断点续跑；
//!   「开始扫描」时自动按平台配置生成字典（乱序、去重）。
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
    fingerprint, PlatformConfig, ScanConfig, ValidKeyRecord, Verdict, DEFAULT_ENDPOINT,
    PATTERN_TEMPLATES, CUSTOM_TEMPLATE_INDEX,
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

impl Counters {
    fn add(&mut self, o: &Counters) {
        self.tested += o.tested;
        self.valid += o.valid;
        self.unauthorized += o.unauthorized;
        self.limited += o.limited;
        self.server += o.server;
        self.client += o.client;
        self.notfound += o.notfound;
        self.network += o.network;
    }
}

/// 平台任务列表中的一行（勾选 = 参与本次嗅探）。
#[derive(Clone)]
struct TaskRow {
    name: String,
    check: gtk::CheckButton,
    status: gtk::Label,
}

/// 一个平台的运行状态（每平台一个独立扫描实例）。
struct RunState {
    name: String,
    base_url: String,
    endpoint: String,
    model: String,
    receiver: mpsc::Receiver<ScanEvent>,
    control: scan_util::Control,
    total: usize,
    start_index: usize,
    counters: Counters,
    finished: bool,
}

// ---------------------------------------------------------------------------
// Inner
// ---------------------------------------------------------------------------

struct Inner {
    toast_overlay: adw::ToastOverlay,

    // 平台配置
    platforms: RefCell<Vec<PlatformConfig>>,
    /// 当前已载入编辑表单的平台名（空 = 尚未保存的新平台）。
    loaded_name: RefCell<String>,
    /// 平台任务列表（每行 = 勾选 + 平台 + 状态）。
    task_list: gtk::ListBox,
    task_rows: RefCell<Vec<TaskRow>>,
    /// 平台列表为空时的提示 label。
    task_empty: gtk::Label,
    /// 平台配置卡（默认折叠；新建/编辑时展开）。
    config_expander: adw::ExpanderRow,
    /// 开始扫描时等待生成字典的平台队列（逐个生成）。
    pending_dict: RefCell<Vec<String>>,
    /// 字典生成被取消（停止按钮在生成期间被点击）；在途任务完成后据此不再继续生成/开扫。
    gen_cancelled: Cell<bool>,
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
    max_row: adw::EntryRow,
    max_hint: adw::ActionRow,
    unbounded_row: adw::SpinRow,
    dict_info: gtk::Label,
    dict_preview: gtk::TextView,
    /// 各平台已生成的候选字典：(pattern 指纹, keys)。键 = 平台名。
    dicts: RefCell<std::collections::HashMap<String, (String, Arc<Vec<String>>)>>,

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
    runs: RefCell<Vec<RunState>>,
    running: Cell<bool>,
    paused: Cell<bool>,
    /// 「新平台」表单回落的全局默认扫描参数（启动时从 store.scan 快照）。
    default_scan: RefCell<ScanConfig>,
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

    // ---------- 嗅探任务（平台列表）----------
    let (task_card, tc) = card(
        "嗅探任务",
        "勾选要同时嗅探的平台；「新建」或点击「编辑」展开平台配置，点「开始扫描」对所有勾选平台并行扫描",
    );
    root_box.append(&task_card);

    let task_list = gtk::ListBox::new();
    task_list.add_css_class("boxed-list");
    let task_scroll = gtk::ScrolledWindow::new();
    task_scroll.set_child(Some(&task_list));
    task_scroll.set_min_content_height(120);
    task_scroll.set_max_content_height(240);
    tc.add(&task_scroll);

    let task_empty = gtk::Label::new(Some("尚未添加平台 —— 点右上角「新建」添加"));
    task_empty.add_css_class("dim-label");
    task_empty.set_halign(gtk::Align::Start);
    task_empty.set_margin_top(4);
    tc.add(&task_empty);

    let select_all_btn = gtk::Button::with_label("全选");
    let select_none_btn = gtk::Button::with_label("全不选");
    let new_btn = gtk::Button::with_label("新建");

    // 操作栏：全选 / 全不选 靠左，新建 靠右对齐
    let list_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    list_actions.set_margin_top(6);
    list_actions.set_margin_bottom(6);
    list_actions.append(&select_all_btn);
    list_actions.append(&select_none_btn);
    let actions_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    actions_spacer.set_hexpand(true);
    list_actions.append(&actions_spacer);
    list_actions.append(&new_btn);
    tc.add(&list_actions);

    // ---------- 平台配置（默认折叠；新建 / 编辑时展开）----------
    let (platform_card, pc) = card("", "");
    root_box.append(&platform_card);

    let config_expander = adw::ExpanderRow::new();
    config_expander.set_title("平台配置");
    config_expander.set_subtitle(
        "Key 正则与字典生成参数也在这里；「开始扫描」时自动按配置生成字典（乱序、去重）",
    );
    config_expander.set_expanded(false);
    pc.add(&config_expander);

    let name_row = entry_row("平台名");
    name_row.set_text("自建网关（本地示例）");
    config_expander.add_row(&name_row);

    let base_row = entry_row("Base URL");
    base_row.set_text("http://127.0.0.1:8000/v1");
    config_expander.add_row(&base_row);

    let endpoint_combo = combo_row("探测端点", ENDPOINTS, 0);
    config_expander.add_row(&endpoint_combo);

    let endpoint_row = entry_row("自定义端点");
    endpoint_row.set_text(DEFAULT_ENDPOINT);
    endpoint_row.set_visible(false);
    config_expander.add_row(&endpoint_row);

    let model_row = entry_row("模型名");
    model_row.set_text("gpt-3.5-turbo");
    config_expander.add_row(&model_row);

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
    config_expander.add_row(&headers_box);

    let note_row = entry_row("备注");
    config_expander.add_row(&note_row);

    // ---- 字典生成参数（并入平台配置）----
    let template_labels: Vec<&str> = PATTERN_TEMPLATES.iter().map(|(n, _)| *n).collect();
    let template_combo = combo_row("插入模板", &template_labels, 0);
    config_expander.add_row(&template_combo);

    let pattern_row = entry_row("API Key 正则规则");
    pattern_row.set_text(r"^sk-local-[0-9]{6}$");
    pattern_row.set_sensitive(false); // 默认选中预设模板，正则不可编辑
    config_expander.add_row(&pattern_row);

    let max_row = adw::EntryRow::new();
    max_row.set_title("最大生成条数（密钥空间更大时截断）");
    max_row.set_text("100000");
    config_expander.add_row(&max_row);
    let max_hint = adw::ActionRow::new();
    max_hint.set_title("注意");
    max_hint.set_subtitle("u128 无上限，不设固定条数上限；生成前按当前可用运存判断，数值越大占用的运存越多");
    config_expander.add_row(&max_hint);
    let unbounded_row = spin_row("* + {n,} 等无界量词展开上限", 1.0, 8.0, 1.0, 0, 3.0);
    config_expander.add_row(&unbounded_row);

    // 扫描参数并入 item 设置：每个平台各自记忆一式（并发/限速/超时/重试/断点/入库）
    let scan_sep = adw::ActionRow::new();
    scan_sep.set_title("扫描参数（每个平台独立记忆）");
    scan_sep.set_subtitle("「开始扫描」时按各平台自己的参数执行");
    scan_sep.set_selectable(false);
    scan_sep.set_activatable(false);
    config_expander.add_row(&scan_sep);

    let concurrency_row = spin_row("并发数（线程）", 1.0, 512.0, 1.0, 0, 4.0);
    config_expander.add_row(&concurrency_row);
    let rate_row = spin_row("限速（请求 / 秒，0 = 不限）", 0.0, 10000.0, 1.0, 0, 5.0);
    config_expander.add_row(&rate_row);
    let timeout_row = spin_row("单次请求超时（秒）", 1.0, 120.0, 1.0, 0, 15.0);
    config_expander.add_row(&timeout_row);
    let retry_row = spin_row("失败重试次数（网络错误 / 5xx / 429）", 0.0, 5.0, 1.0, 0, 1.0);
    config_expander.add_row(&retry_row);
    let resume_switch = switch_row("断点续跑", "中断后下次从断点继续；配置变更会自动失效", true);
    config_expander.add_row(&resume_switch);
    let persist_switch = switch_row("命中即入本地库", "有效 Key 追加写入 SQLite，自动去重，永久保存", true);
    config_expander.add_row(&persist_switch);

    let save_btn = gtk::Button::with_label("保存");
    save_btn.add_css_class("suggested-action");
    let cancel_btn = gtk::Button::with_label("取消");
    config_expander.add_row(&button_row(&[&save_btn, &cancel_btn]));

    let dict_info = gtk::Label::new(Some("尚未生成字典（点「开始扫描」时自动生成）"));
    dict_info.add_css_class("dim-label");
    dict_info.set_halign(gtk::Align::Start);
    dict_info.set_wrap(true);
    dict_info.set_selectable(true);
    config_expander.add_row(&dict_info);

    let dict_preview = mono_view(150, false);
    let dict_scroll = gtk::ScrolledWindow::new();
    dict_scroll.set_child(Some(&dict_preview));
    dict_scroll.set_min_content_height(120);
    dict_scroll.set_max_content_height(260);
    config_expander.add_row(&dict_scroll);

    // ---------- 执行 ----------
    let (run_card, rc_) = card("执行", "");
    root_box.append(&run_card);

    let verbose_switch = switch_row("记录每一条的判定", "关闭时只记录命中、汇总与异常", false);
    rc_.add(&verbose_switch);

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
    let (valid_card, vc) = card(
        "有效 Key（本地库）",
        "2xx 判定为有效，命中即写入本地 SQLite 数据库（自动去重）",
    );
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
        task_list: task_list.clone(),
        task_rows: RefCell::new(Vec::new()),
        task_empty: task_empty.clone(),
        config_expander: config_expander.clone(),
        pending_dict: RefCell::new(Vec::new()),
        gen_cancelled: Cell::new(false),
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
        max_hint: max_hint.clone(),
        unbounded_row: unbounded_row.clone(),
        dict_info: dict_info.clone(),
        dict_preview: dict_preview.clone(),
        dicts: RefCell::new(std::collections::HashMap::new()),
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
        runs: RefCell::new(Vec::new()),
        running: Cell::new(false),
        paused: Cell::new(false),
        default_scan: RefCell::new(ScanConfig::default()),
        started_at: RefCell::new(None),
    });

    // ---------- 信号连接 ----------
    select_all_btn.connect_clicked(|_| g_select_all(true));
    select_none_btn.connect_clicked(|_| g_select_all(false));
    endpoint_combo.connect_selected_notify(|_| g_on_endpoint_changed());
    template_combo.connect_selected_notify(|_| g_on_template_selected());
    pattern_row.connect_changed(|_| g_on_pattern_changed());

    new_btn.connect_clicked(|_| g_new_platform());
    save_btn.connect_clicked(|_| g_save_platform());
    cancel_btn.connect_clicked(|_| g_cancel_edit());

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

    // 注册全局强引用：signal 回调要求 Send，无法直接捕获 Rc<Inner>
    // 必须先于 rebuild_platform_list（行回调通过 self_rc() 取 Weak，INNER 未注册会 panic）
    INNER.with(|i| *i.borrow_mut() = Some(Rc::clone(&inner)));

    // ---------- 初始化 ----------
    {
        store::init_db();
        let store_data = store::load_store();
        *inner.platforms.borrow_mut() = store_data.platforms;
        inner.apply_scan_config(&store_data.scan);
        *inner.default_scan.borrow_mut() = store_data.scan; // 「新平台」回落默认值
        inner.rebuild_platform_list(Some(&store_data.last_platform));
        if let Some(p) = inner.current_platform() {
            inner.load_platform(&p);
        }
        inner.reload_valid();
        inner.refresh_resume_hint();
        inner.update_stats();
        inner.refresh_max_hint();
    }

    // 主循环里排空扫描事件队列（闭包不带捕获，满足 signal 的 Send 要求）
    glib::source::timeout_add(Duration::from_millis(100), tick);

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

fn g_select_platform(name: String) {
    with_inner(|i| {
        if let Some(p) = i.platforms.borrow().iter().find(|p| p.name == name).cloned() {
            i.load_platform(&p);
            i.persist_last_platform(&p.name);
        }
    });
}

fn g_select_all(enable: bool) {
    with_inner(|i| {
        let rows = i.task_rows.borrow().clone();
        for row in rows {
            row.check.set_active(enable);
        }
    });
}

fn g_on_endpoint_changed() {
    with_inner(|i| {
        i.endpoint_row
            .set_visible(i.endpoint_combo.selected() == ENDPOINT_CUSTOM);
    });
}

fn g_on_pattern_changed() {
    with_inner(|i| i.refresh_max_hint());
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
fn g_cancel_edit() {
    with_inner(|i| i.cancel_edit());
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
        let name = self.loaded_name.borrow().clone();
        if name.is_empty() {
            return None;
        }
        self.platforms.borrow().iter().find(|p| p.name == name).cloned()
    }

    /// 根据当前可用运存与当前正则，实时刷新「注意」行的推荐最大条数。
    fn refresh_max_hint(&self) {
        let pattern = self.pattern_row.text().trim().to_string();
        let unbounded = self.unbounded_row.value().max(1.0) as usize;
        let avail = crate::utils::sniffer::available_memory_bytes();
        let recommended = crate::utils::sniffer::recommended_max_keys(&pattern, unbounded);
        match (avail, recommended) {
            (Some(bytes), Some(keys)) => {
                self.max_hint.set_subtitle(&format!(
                    "u128 无上限；超出当前可用运存（约 {:.1} GB）会在生成时被拦截。\
                     按当前正则推荐最大生成条数 ≤ {} 条",
                    bytes as f64 / 1_000_000_000.0,
                    crate::utils::sniffer::format_count(keys)
                ));
            }
            (Some(_), None) => {
                self.max_hint.set_subtitle(
                    "u128 无上限；超出当前可用运存会在生成时被拦截。填入合法的正则后，\
                     将按当前可用运存显示推荐最大条数",
                );
            }
            _ => {
                // 读不到可用内存信息（非 Linux）：回到静态提示
                self.max_hint.set_subtitle(
                    "u128 无上限，不设固定条数上限；生成前按当前可用运存判断，数值越大占用的运存越多",
                );
            }
        }
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
        // 载入该平台自己记忆的扫描参数（表单同步回填）
        self.apply_scan_config(&p.scan);
        // 载入平台时展示该平台已生成的字典状态（若有）
        let dict_state = self
            .dicts
            .borrow()
            .get(&p.name)
            .map(|(pat, keys)| (pat.clone(), keys.len()));
        match dict_state {
            Some((pat, len)) if pat == p.pattern => {
                self.dict_info.set_text(&format!(
                    "该平台已生成字典：共 {} 条（乱序 · 与当前正则一致，可直接扫描）",
                    format_count(len as u128)
                ));
            }
            _ => {
                set_buffer_text(&self.dict_preview.buffer(), "");
                self.dict_info
                    .set_text("尚未为当前正则生成字典（点「开始扫描」时自动生成）");
            }
        }
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
            // 平台自带的扫描参数（每个 item 独立记忆）
            scan: self.collect_scan_config(),
        }
    }

    fn new_platform(&self) {
        self.fill_new_form();
        self.config_expander.set_expanded(true);
        self.toast("已切换到新平台，填好后点「保存」");
    }

    /// 把表单重置为「新平台」默认值（不展开、不提示，供取消/删除后复用）。
    fn fill_new_form(&self) {
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
        // 新平台回落全局默认扫描参数（store.scan 快照）
        self.apply_scan_config(&self.default_scan.borrow());
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
        self.dict_info
            .set_text("尚未生成字典（点「开始扫描」时自动生成）");
    }

    /// 「取消」：放弃当前表单的新建/修改，收起平台配置卡。
    fn cancel_edit(&self) {
        let name = self.loaded_name.borrow().clone();
        if name.is_empty() {
            self.fill_new_form();
        } else if let Some(p) = self.platforms.borrow().iter().find(|p| p.name == name).cloned() {
            // 重新载入已保存的配置，丢弃未保存的编辑
            self.load_platform(&p);
        }
        self.config_expander.set_expanded(false);
        self.toast("已取消，未保存的修改被丢弃");
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
        self.rebuild_platform_list(Some(&cfg.name));
        // 字典按平台缓存：改名时迁移，否则保持
        {
            let mut dicts = self.dicts.borrow_mut();
            if old_name != cfg.name {
                if let Some(d) = dicts.remove(&old_name) {
                    dicts.insert(cfg.name.clone(), d);
                }
            }
        }
        // 级联改名：本地库（valid_keys 表）与断点文件里的平台名一起迁移，
        // 并刷新「有效 Key」列表，避免旧记录仍挂着旧平台名
        if old_name != cfg.name {
            if let Err(e) = store::rename_platform(&old_name, &cfg.name) {
                self.toast(&format!("本地库平台名迁移失败：{e}"));
            } else {
                self.log(&format!(
                    "平台「{old_name}」已改名为「{}」，本地库命中记录与断点已同步迁移",
                    cfg.name
                ));
            }
            store::rename_checkpoint(&old_name, &cfg.name);
            self.reload_valid();
        }
        self.dict_info
            .set_text("配置已保存（「开始扫描」时自动生成字典）");
        self.config_expander.set_expanded(false);
        self.refresh_resume_hint();
        self.toast(&format!("平台「{}」已保存", cfg.name));
    }

    /// 按名字删除平台（列表行删除按钮回调）；若删除的正是正在编辑的平台则收起配置卡。
    fn delete_platform_by_name(&self, name: &str) {
        {
            let mut list = self.platforms.borrow_mut();
            list.retain(|p| p.name != name);
        }
        self.dicts.borrow_mut().remove(name);
        self.persist_platforms();
        if *self.loaded_name.borrow() == name {
            *self.loaded_name.borrow_mut() = String::new();
            self.config_expander.set_expanded(false);
        }
        self.rebuild_platform_list(None);
        self.toast(&format!("已删除平台「{name}」"));
    }

    /// 按名字载入平台（列表行点击 / 编辑按钮回调），展开配置卡。
    fn load_platform_by_name(&self, name: &str) {
        if let Some(p) = self.platforms.borrow().iter().find(|p| p.name == name).cloned() {
            self.load_platform(&p);
            self.config_expander.set_expanded(true);
        }
    }

    /// 重建平台任务列表（勾选 + 平台 + 状态 + 删除），`select` 指定的平台会载入表单。
    fn rebuild_platform_list(&self, select: Option<&str>) {
        // 清空列表
        while let Some(child) = self.task_list.first_child() {
            self.task_list.remove(&child);
        }
        self.task_rows.borrow_mut().clear();

        let rows: Vec<(PlatformConfig, gtk::CheckButton, gtk::Label)> = {
            let list = self.platforms.borrow();
            list.iter()
                .map(|p| {
                    let check = gtk::CheckButton::new();
                    check.set_active(true); // 默认全部勾选参与嗅探
                    let status = gtk::Label::new(Some("待机"));
                    status.add_css_class("dim-label");
                    status.set_halign(gtk::Align::End);
                    (p.clone(), check, status)
                })
                .collect()
        };

        for (p, check, status) in rows {
            // 用 ActionRow 作为行主体：自带点击高亮与 activated 信号，点击即载入编辑
            let row = adw::ActionRow::new();
            row.set_title(&p.name);
            row.set_subtitle(&format!("{} · {}", p.base_url, p.model));
            row.set_activatable(true);
            row.set_subtitle_selectable(false);

            row.add_prefix(&check);
            row.add_suffix(&status);
            let edit_btn = gtk::Button::from_icon_name("document-edit-symbolic");
            edit_btn.set_tooltip_text(Some("编辑该平台"));
            edit_btn.add_css_class("flat");
            edit_btn.add_css_class("circular");
            edit_btn.set_valign(gtk::Align::Center);
            row.add_suffix(&edit_btn);
            let del_btn = gtk::Button::from_icon_name("user-trash-symbolic");
            del_btn.set_tooltip_text(Some("删除该平台"));
            del_btn.add_css_class("flat");
            del_btn.add_css_class("circular");
            del_btn.set_valign(gtk::Align::Center);
            row.add_suffix(&del_btn);

            self.task_list.append(&row);
            self.task_rows.borrow_mut().push(TaskRow {
                name: p.name.clone(),
                check: check.clone(),
                status: status.clone(),
            });

            let name_for_edit = p.name.clone();
            let name_for_edit_btn = p.name.clone();
            let name_for_del = p.name.clone();
            let self_ref_edit = Rc::downgrade(&self.self_rc());
            let self_ref_edit_btn = Rc::downgrade(&self.self_rc());
            let self_ref_del = Rc::downgrade(&self.self_rc());
            row.connect_activated(move |_| {
                if let Some(inner) = self_ref_edit.upgrade() {
                    inner.load_platform_by_name(&name_for_edit);
                    inner.persist_last_platform(&name_for_edit);
                }
            });
            edit_btn.connect_clicked(move |_| {
                if let Some(inner) = self_ref_edit_btn.upgrade() {
                    inner.load_platform_by_name(&name_for_edit_btn);
                    inner.persist_last_platform(&name_for_edit_btn);
                }
            });
            del_btn.connect_clicked(move |_| {
                if let Some(inner) = self_ref_del.upgrade() {
                    inner.delete_platform_by_name(&name_for_del);
                }
            });
        }

        self.task_empty.set_visible(self.platforms.borrow().is_empty());
        if let Some(name) = select {
            if let Some(p) = self.platforms.borrow().iter().find(|p| p.name == name).cloned() {
                *self.loaded_name.borrow_mut() = p.name.clone();
            }
        }
    }

    /// 内部方法：Rc 自身，供列表行回调使用。
    fn self_rc(&self) -> Rc<Inner> {
        INNER.with(|i| i.borrow().clone().unwrap_or_else(|| unreachable!("页面未注册")))
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

    /// 读取「最大生成条数」：u128 无硬性上限（受运存约束）；空/非法输入回退 100_000。
    fn max_candidates(&self) -> u128 {
        self.max_row
            .text()
            .trim()
            .parse::<u128>()
            .unwrap_or(100_000)
            .max(1)
    }

    fn collect_scan_config(&self) -> ScanConfig {
        ScanConfig {
            concurrency: self.concurrency_row.value() as usize,
            rate_per_sec: self.rate_row.value(),
            timeout_secs: self.timeout_row.value() as u64,
            retries: self.retry_row.value() as usize,
            max_candidates: self.max_candidates(),
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
        self.max_row.set_text(&cfg.max_candidates.to_string());
        self.unbounded_row.set_value(cfg.unbounded_repeat as f64);
        self.resume_switch.set_active(cfg.resume);
        self.persist_switch.set_active(cfg.persist_valid);
    }

    // ---------- 字典 ----------

    /// 依次为 `pending_dict` 队列里的平台生成字典。
    ///
    /// tokio 异步：CPU 密集部分走 `spawn_blocking` 不卡 UI；网络与回调经
    /// 全局运行时调度。生成始终乱序（洗牌种子取自平台正则 + 生成参数，
    /// 同一配置多次运行顺序一致，「断点续跑」游标跨会话依然有效）。
    fn gen_next_dict(&self) {
        let name = match self.pending_dict.borrow().first().cloned() {
            Some(n) => n,
            None => {
                self.after_dict_all_done();
                return;
            }
        };
        self.pending_dict.borrow_mut().remove(0);
        let Some(p) = self.platforms.borrow().iter().find(|p| p.name == name).cloned() else {
            self.log(&format!("「{name}」配置缺失，跳过字典生成"));
            self.set_task_status(&name, "配置缺失");
            return self.gen_next_dict();
        };
        // 用该平台自己记忆的字典规模参数生成（不是表单当前值）
        let max = p.scan.max_candidates;
        let unbounded = p.scan.unbounded_repeat.max(1);
        let opts = GenerateOptions {
            max_results: max,
            unbounded_repeat: unbounded,
            random_sample: true,
            seed: crate::utils::sniffer::sample_seed_for(&[
                &p.pattern,
                &max.to_string(),
                &unbounded.to_string(),
            ]),
        };
        self.set_task_status(&name, "生成字典中…");
        self.dict_info
            .set_text(&format!("正在为「{name}」生成字典（乱序、去重）…"));
        self.log(&format!("「{name}」正在生成字典…"));

        // `Arc<Mutex<..>>` 才能跨线程搬运（`Rc` 不是 Send）；结果附带生成时所属的平台名
        let slot: Arc<
            Mutex<Option<(String, Result<crate::utils::sniffer::Dictionary, String>)>>,
        > = Arc::new(Mutex::new(None));
        GENERATE_SLOT.with(|s| *s.borrow_mut() = Some(Arc::clone(&slot)));
        let pattern = p.pattern.clone();
        let task_name = name.clone();
        crate::utils::sniffer::runtime().spawn(async move {
            let result = match tokio::task::spawn_blocking(move || generate(&pattern, &opts)).await
            {
                Ok(r) => r,
                Err(e) => Err(format!("生成任务异常：{e}")),
            };
            if let Ok(mut guard) = slot.lock() {
                *guard = Some((task_name, result));
            }
            let _ = glib::source::idle_add(tick_generate_done);
        });
    }

    /// 一个平台的字典生成完毕：缓存结果，继续队列里下一个平台；全部完成后开始扫描。
    fn generate_done(&self) {
        let result = GENERATE_SLOT
            .with(|s| s.borrow_mut().take())
            .and_then(|slot| slot.lock().ok().and_then(|mut g| g.take()));
        match result {
            Some((name, Ok(dict))) => {
                let len = dict.keys.len();
                let keys = Arc::new(dict.keys.clone());
                let pattern = self
                    .platforms
                    .borrow()
                    .iter()
                    .find(|p| p.name == name)
                    .map(|p| p.pattern.clone())
                    .unwrap_or_default();
                {
                    let mut dicts = self.dicts.borrow_mut();
                    dicts.insert(name.clone(), (pattern, Arc::clone(&keys)));
                }
                self.set_task_status(&name, "字典就绪");
                self.log(&format!("「{name}」字典生成完成：{len} 条候选（乱序）"));
                // 只更新「当前正在编辑那个平台」的预览
                if *self.loaded_name.borrow() == name {
                    let preview: Vec<&str> = keys.iter().take(PREVIEW_LIMIT).map(|s| s.as_str()).collect();
                    let mut text = preview.join("\n");
                    if len > PREVIEW_LIMIT {
                        text.push_str(&format!(
                            "\n… 其余 {} 条未展示",
                            format_count((len - PREVIEW_LIMIT) as u128)
                        ));
                    }
                    set_buffer_text(&self.dict_preview.buffer(), &text);
                    let mut info = format!(
                        "「{name}」密钥空间 {} · 已生成 {} 条（乱序）",
                        format_count(dict.total_space),
                        format_count(len as u128)
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
                }
                self.refresh_resume_hint();
            }
            Some((name, Err(e))) => {
                self.set_task_status(&name, "生成失败");
                self.log(&format!("「{name}」字典生成失败：{e}"));
                self.toast(&format!("「{name}」字典生成失败：{e}"));
            }
            None => self.dict_info.set_text("生成失败：内部状态丢失"),
        }
        // 停止被点击过：不再继续生成、也不进入扫描（生成的字典缓存已被 stop 清除）
        if self.gen_cancelled.get() {
            self.gen_cancelled.set(false);
            self.start_btn.set_sensitive(true);
            self.log("字典生成已取消，未开始扫描");
            return;
        }
        // 队列里还有平台就继续生成，否则进入扫描
        if self.pending_dict.borrow().is_empty() {
            self.after_dict_all_done();
        } else {
            self.gen_next_dict();
        }
    }

    /// 所有待生成平台的字典都处理完：交给扫描启动流程（生成失败的平台会被跳过）。
    fn after_dict_all_done(&self) {
        self.start_btn.set_sensitive(true);
        self.begin_scan();
    }

    /// 当前配置对应的字典指纹（配置一变，断点即失效）。
    fn current_fingerprint(&self) -> String {
        let cfg = self.collect_platform();
        self.fingerprint_for(&cfg)
    }

    /// 按具体平台配置计算指纹（多平台各自独立断点）。
    fn fingerprint_for(&self, cfg: &PlatformConfig) -> String {
        fingerprint(&[
            &cfg.name,
            &cfg.base_url,
            &cfg.endpoint,
            &cfg.model,
            &cfg.pattern,
            &cfg.scan.max_candidates.to_string(),
            &cfg.scan.unbounded_repeat.to_string(),
        ])
    }

    // ---------- 断点 ----------

    /// 每个平台的断点独立存放（store::checkpoint_path 按平台分文件）。
    fn refresh_resume_hint(&self) {
        if !self.resume_switch.is_active() {
            self.resume_hint.set_text("断点续跑已关闭");
            return;
        }
        let name = self.loaded_name.borrow().clone();
        if name.is_empty() {
            self.resume_hint.set_text("当前平台未保存，断点按平台名区分");
            return;
        }
        match load_checkpoint(&name) {
            None => self.resume_hint.set_text("当前平台无断点，将从头开始"),
            Some(cp) => {
                if cp.fingerprint == self.current_fingerprint() {
                    self.resume_hint.set_text(&format!(
                        "「{name}」发现断点（{}）：上次测到 {}/{}，命中 {} 条；开始时会从第 {} 条继续",
                        time_text(cp.updated_at),
                        format_count(cp.cursor as u128),
                        format_count(cp.total as u128),
                        cp.valid,
                        format_count((cp.cursor + 1) as u128),
                    ));
                } else {
                    self.resume_hint
                        .set_text("已存在的断点与当前配置不匹配，开始时会自动忽略");
                }
            }
        }
    }

    fn reset_checkpoint(&self) {
        let name = self.loaded_name.borrow().clone();
        if name.is_empty() {
            self.toast("当前平台未保存，暂无断点可清除");
            return;
        }
        store::clear_checkpoint(&name);
        self.refresh_resume_hint();
        self.toast(&format!("「{name}」断点已清除"));
    }

    // ---------- 扫描 ----------

    /// 收集勾选的平台（参与本次嗅探的任务列表）。
    fn enabled_platforms(&self) -> Vec<PlatformConfig> {
        let rows = self.task_rows.borrow();
        self.platforms
            .borrow()
            .iter()
            .filter(|p| rows.iter().any(|r| r.name == p.name && r.check.is_active()))
            .cloned()
            .collect()
    }

    /// 对勾选的多个平台启动扫描。
    ///
    /// 缺少缓存字典的平台会先异步生成（乱序、去重），全部就绪后进入
    /// [`begin_scan`]；已有字典的平台直接开扫。
    fn start(&self) {
        if self.running.get() {
            self.toast("扫描正在进行中");
            return;
        }
        let enabled = self.enabled_platforms();
        if enabled.is_empty() {
            self.toast("请先在「嗅探任务」列表中勾选至少一个平台");
            return;
        }
        // 找出缺少「与当前正则匹配」的缓存字典的平台，先异步生成
        let need: Vec<String> = enabled
            .iter()
            .filter(|p| {
                !matches!(
                    self.dicts.borrow().get(&p.name),
                    Some((pat, _)) if pat == &p.pattern
                )
            })
            .map(|p| p.name.clone())
            .collect();
        if !need.is_empty() {
            self.start_btn.set_sensitive(false);
            self.stat_label
                .set_text(&format!("正在为 {} 个平台生成字典…", need.len()));
            self.log(&format!(
                "开始扫描前先为 {} 个平台生成字典（乱序、去重）",
                need.len()
            ));
            *self.pending_dict.borrow_mut() = need.clone();
            for name in &need {
                self.set_task_status(name, "待生成字典");
            }
            self.gen_next_dict();
            return;
        }
        self.begin_scan();
    }

    /// 字典就绪后的扫描启动主体：逐平台校验 + 断点 + 启动扫描实例。
    fn begin_scan(&self) {
        if self.running.get() {
            return;
        }
        // 注意：扫描参数按平台各自记忆（p.scan），不在此处取全局
        let enabled = self.enabled_platforms();
        if enabled.is_empty() {
            self.start_btn.set_sensitive(true);
            self.toast("请先在「嗅探任务」列表中勾选至少一个平台");
            return;
        }
        // 逐平台校验 + 准备字典
        let mut runs: Vec<RunState> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        for p in &enabled {
            if let Err(e) = p.validate() {
                skipped.push(format!("{}（{e}）", p.name));
                continue;
            }
            let keys = match self.dicts.borrow().get(&p.name).cloned() {
                Some((pat, keys)) if pat == p.pattern => keys,
                _ => {
                    skipped.push(format!("{}（未生成与该平台正则匹配的字典）", p.name));
                    continue;
                }
            };
            let fp = self.fingerprint_for(p);

            // 断点续跑（每个平台独立断点文件、独立开关）
            let mut start_index = 0usize;
            if p.scan.resume {
                if let Some(cp) = load_checkpoint(&p.name) {
                    if cp.fingerprint == fp {
                        if cp.cursor >= keys.len() {
                            skipped.push(format!(
                                "{}（该字典已跑完，请清除断点或修改生成规则）",
                                p.name
                            ));
                            continue;
                        }
                        start_index = cp.cursor;
                        self.log(&format!(
                            "「{}」断点续跑：从第 {} 条继续（上次命中 {} 条）",
                            p.name,
                            format_count((cp.cursor + 1) as u128),
                            cp.valid
                        ));
                    } else {
                        self.log(&format!("「{}」断点与当前配置不匹配，已忽略并从头开始", p.name));
                        store::clear_checkpoint(&p.name);
                    }
                }
            } else {
                store::clear_checkpoint(&p.name);
            }

            let target = ProbeTarget {
                base_url: p.base_url.clone(),
                endpoint: p.endpoint.clone(),
                model: p.model.clone(),
                headers: p.headers.clone(),
                timeout: Duration::from_secs(p.scan.timeout_secs.max(1)),
            };
            let params = ScanParams {
                platform: p.name.clone(),
                fingerprint: fp,
                target,
                keys: Arc::clone(&keys),
                start_index,
                concurrency: p.scan.concurrency.max(1),
                rate_per_sec: p.scan.rate_per_sec,
                retries: p.scan.retries,
                persist_valid: p.scan.persist_valid,
                write_checkpoint: p.scan.resume,
            };
            let (tx, rx) = mpsc::channel();
            let control = scan_util::start(params, tx);
            runs.push(RunState {
                name: p.name.clone(),
                base_url: p.base_url.clone(),
                endpoint: p.endpoint.clone(),
                model: p.model.clone(),
                receiver: rx,
                control,
                total: keys.len(),
                start_index,
                counters: Counters::default(),
                finished: false,
            });
            self.set_task_status(&p.name, &format!("启动中 · {} 条", format_count(keys.len() as u128)));
        }

        if runs.is_empty() {
            self.start_btn.set_sensitive(true);
            self.toast(&format!("没有可扫描的平台：{}", skipped.join("；")));
            return;
        }

        self.persist_platforms();
        *self.runs.borrow_mut() = runs;
        self.running.set(true);
        self.paused.set(false);
        *self.started_at.borrow_mut() = Some(Instant::now());

        self.start_btn.set_sensitive(false);
        self.pause_btn.set_sensitive(true);
        self.pause_btn.set_label("暂停");
        self.stop_btn.set_sensitive(true);
        self.progress.set_fraction(0.0);
        self.stat_label.set_text("正在启动…");

        let total_runs = self.runs.borrow().len();
        for run in self.runs.borrow().iter() {
            let p = self.platforms.borrow().iter().find(|p| p.name == run.name).cloned();
            let Some(cfg) = p else { continue };
            self.log(&format!(
                "开始扫描「{}」：字典 {} 条，从第 {} 条开始，并发 {}，限速 {} 次/秒",
                run.name,
                format_count(run.total as u128),
                format_count((run.start_index + 1) as u128),
                cfg.scan.concurrency.max(1),
                if cfg.scan.rate_per_sec > 0.0 {
                    cfg.scan.rate_per_sec.to_string()
                } else {
                    "不限".to_string()
                }
            ));
        }
        if !skipped.is_empty() {
            self.log(&format!("跳过 {} 个平台：{}", skipped.len(), skipped.join("；")));
            self.toast(&format!("已跳过 {} 个平台（详见日志）", skipped.len()));
        }
        self.log(&format!("共启动 {} 个平台的扫描，并行进行中", total_runs));
        self.update_stats();
    }

    fn toggle_pause(&self) {
        if !self.running.get() {
            return;
        }
        let next = !self.paused.get();
        self.paused.set(next);
        for run in self.runs.borrow().iter() {
            run.control.set_paused(next);
        }
        self.pause_btn.set_label(if next { "继续" } else { "暂停" });
        self.log(if next { "已暂停全部扫描" } else { "已继续全部扫描" });
    }

    fn stop(&self) {
        // 场景一：还在字典生成阶段（扫描未开始）→ 取消剩余生成
        if !self.running.get() {
            if !self.pending_dict.borrow().is_empty() {
                self.gen_cancelled.set(true);
                self.pending_dict.borrow_mut().clear();
                self.start_btn.set_sensitive(true);
                self.log("已取消字典生成，尚未开始扫描");
            }
            return;
        }
        if self.paused.get() {
            // 暂停中先恢复，否则工作线程卡在暂停轮询里看不到停止信号
            self.paused.set(false);
            for run in self.runs.borrow().iter() {
                run.control.set_paused(false);
            }
        }
        // 先置停止信号（引擎收尾因此不再写断点），再清断点 → 不会被写回
        for run in self.runs.borrow().iter() {
            run.control.stop();
        }
        let names: Vec<String> = self.runs.borrow().iter().map(|r| r.name.clone()).collect();
        for name in &names {
            store::clear_checkpoint(name);
        }
        for name in &names {
            self.set_task_status(name, "已停止");
        }
        // 立即完成界面收尾，不等待引擎的 Finished（在途请求最长可能拖满超时）
        self.reset_controls_after_scan();
        self.refresh_resume_hint();
        self.log(&format!(
            "已停止全部扫描（{} 个平台）：断点、字典缓存已清除，进度已复位",
            names.len()
        ));
    }

    /// 更新任务列表里某平台行的状态文字。
    fn set_task_status(&self, name: &str, text: &str) {
        let rows = self.task_rows.borrow();
        if let Some(row) = rows.iter().find(|r| r.name == name) {
            row.status.set_text(text);
        }
    }

    /// 主循环定时调用：排空所有平台的扫描事件队列并更新界面。
    fn drain_events(&self) {
        if !self.running.get() {
            return;
        }
        // 1) 排空每个 run 的队列（只碰 runs 内部）
        let mut batch: Vec<(usize, ScanEvent)> = Vec::new();
        {
            let runs = self.runs.borrow();
            for (i, run) in runs.iter().enumerate() {
                if run.finished {
                    continue;
                }
                while let Ok(ev) = run.receiver.try_recv() {
                    batch.push((i, ev));
                    if batch.len() >= 4000 {
                        break;
                    }
                }
            }
        }
        if batch.is_empty() {
            self.update_progress();
            return;
        }

        // 2) 处理事件（此时不持有 runs 借用）：只做日志与有效 Key 收集，计数统一在第 3 步应用
        let verbose = self.verbose_switch.is_active();
        let mut new_records: Vec<ValidKeyRecord> = Vec::new();
        let mut finished_notes: Vec<String> = Vec::new();
        // 收集：run 索引 → 本批累计的计数；以及已结束的 run 索引
        let mut counters_updates: Vec<(usize, Counters)> = Vec::new();
        let mut finished_ids: Vec<usize> = Vec::new();
        {
            let runs = self.runs.borrow();
            for (i, ev) in batch {
                let Some(run) = runs.get(i) else {
                    continue;
                };
                match ev {
                    ScanEvent::Started { total, start_index } => {
                        self.log(&format!(
                            "「{}」引擎已启动：共 {} 条，起点 #{}",
                            run.name,
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
                        // 累加该 run 的本批计数
                        let entry = match counters_updates.iter_mut().find(|(idx, _)| *idx == i) {
                            Some(entry) => entry,
                            None => {
                                counters_updates.push((i, run.counters));
                                counters_updates.last_mut().unwrap()
                            }
                        };
                        entry.1.tested += 1;
                        match outcome.verdict {
                            Verdict::Valid => entry.1.valid += 1,
                            Verdict::Unauthorized => entry.1.unauthorized += 1,
                            Verdict::RateLimited => entry.1.limited += 1,
                            Verdict::NotFound => entry.1.notfound += 1,
                            Verdict::ServerError => entry.1.server += 1,
                            Verdict::ClientError => entry.1.client += 1,
                            Verdict::NetworkError => entry.1.network += 1,
                        }
                        if outcome.verdict.is_valid() {
                            let record = ValidKeyRecord {
                                platform: run.name.clone(),
                                base_url: run.base_url.clone(),
                                endpoint: run.endpoint.clone(),
                                model: run.model.clone(),
                                key: key.clone(),
                                status: outcome.status,
                                latency_ms: outcome.latency_ms,
                                found_at: probe_util::now_unix(),
                                snippet: outcome.body.clone(),
                            };
                            new_records.push(record);
                            self.log(&format!(
                                "✔ 「{}」命中 #{}：{} → HTTP {} · {} ms · {}",
                                run.name,
                                index,
                                mask_key(&key),
                                outcome.status,
                                outcome.latency_ms,
                                outcome.detail
                            ));
                        } else if verbose {
                            self.log(&format!(
                                "「{}」#{} {} → {} {}（{} ms{}）· {}",
                                run.name,
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
                                "「{}」网络错误 #{}：{}（请检查 Base URL / 网络 / 超时设置）",
                                run.name, index, outcome.detail
                            ));
                        } else if entry.1.tested % SUMMARY_EVERY == 0 {
                            self.log(&format!(
                                "「{}」进度：已测 {} 条 · 有效 {} · 鉴权失败 {} · 限流 {} · 网络错误 {}",
                                run.name,
                                format_count(entry.1.tested as u128),
                                entry.1.valid,
                                entry.1.unauthorized,
                                entry.1.limited,
                                entry.1.network
                            ));
                        }
                    }
                    ScanEvent::Log(text) => self.log(&format!("「{}」{text}", run.name)),
                    ScanEvent::Finished {
                        reason,
                        tested,
                        valid,
                    } => {
                        // 扫描结束即清断点：无论「跑完」还是「停止」都代表放弃本轮的
                        // 中间进度（停止 = 放弃，下次从头开始）。只有「暂停」（不触发
                        // Finished）或中途退出应用时才保留断点，供「断点续跑」使用。
                        store::clear_checkpoint(&run.name);
                        self.log(&format!(
                            "「{}」扫描{}：本次测 {} 条，命中 {} 条（断点已清除）",
                            run.name,
                            reason.label(),
                            format_count(tested as u128),
                            valid
                        ));
                        finished_ids.push(i);
                        finished_notes.push(if reason == StopReason::Completed {
                            format!("{} 已完成，命中 {valid} 条", run.name)
                        } else {
                            format!("{} 已停止，命中 {valid} 条", run.name)
                        });
                    }
                }
            }
        }

        // 3) 应用本批计数与结束标记（需要可变借用 runs）
        {
            let mut runs = self.runs.borrow_mut();
            for (i, counters) in counters_updates {
                if let Some(run) = runs.get_mut(i) {
                    run.counters = counters;
                }
            }
            for i in finished_ids {
                if let Some(run) = runs.get_mut(i) {
                    run.finished = true;
                    self.set_task_status(&run.name, "已完成");
                }
            }
        }

        for note in &finished_notes {
            self.toast(note);
        }
        let all_done = {
            let runs = self.runs.borrow();
            runs.iter().all(|r| r.finished)
        };
        if all_done {
            let total_valid = {
                let runs = self.runs.borrow();
                runs.iter().map(|r| r.counters.valid).sum::<usize>()
            };
            self.log("全部平台的扫描均已结束");
            self.toast(&format!("全部扫描结束，共命中 {total_valid} 条有效 Key"));
            self.finish_run();
            self.refresh_resume_hint();
        }

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
        // 自然跑完或全部收到停止事件后的统一界面收尾
        self.reset_controls_after_scan();
        self.refresh_resume_hint();
    }

    /// stop / finish_run 共用的界面复位：任务与按钮状态、进度条、统计标签。
    /// 一轮扫描结束（无论「自然跑完」还是「主动停止」）都会清除每平台的内存
    /// 字典缓存，下次「开始扫描」时按当前配置重新生成；暂停不触发此函数，
    /// 缓存保留，供继续扫描使用。
    fn reset_controls_after_scan(&self) {
        self.running.set(false);
        self.paused.set(false);
        self.runs.borrow_mut().clear();
        // 一轮结束即清字典缓存（正在扫描的任务持有 Arc 克隆，不受影响）
        self.dicts.borrow_mut().clear();
        self.start_btn.set_sensitive(true);
        self.pause_btn.set_sensitive(false);
        self.pause_btn.set_label("暂停");
        self.stop_btn.set_sensitive(false);
        self.progress.set_fraction(0.0);
        self.progress.set_text(Some("0 / 0"));
        self.stat_label.set_text("就绪");
    }

    /// 汇总所有平台的进度。
    fn update_progress(&self) {
        let runs = self.runs.borrow();
        let total: usize = runs.iter().map(|r| r.total).sum();
        let done: usize = runs
            .iter()
            .map(|r| (r.start_index + r.counters.tested).min(r.total))
            .sum();
        if total == 0 {
            self.progress.set_fraction(0.0);
            self.progress.set_text(Some("0 / 0"));
            return;
        }
        let frac = done as f64 / total as f64;
        self.progress.set_fraction(frac);
        self.progress
            .set_text(Some(&format!("{:.1}% · {} / {}", frac * 100.0, format_count(done as u128), format_count(total as u128))));
        // 同步每行状态
        drop(runs);
        let runs = self.runs.borrow();
        for run in runs.iter() {
            if run.finished {
                continue;
            }
            let p = if run.total > 0 {
                (run.start_index + run.counters.tested) as f64 / run.total as f64 * 100.0
            } else {
                100.0
            };
            self.set_task_status(
                &run.name,
                &format!("{:.0}% · {} / {}", p, format_count(run.counters.tested as u128), format_count(run.total as u128)),
            );
        }
    }

    /// 汇总所有平台的统计。
    fn update_stats(&self) {
        let runs = self.runs.borrow();
        if runs.is_empty() {
            self.stat_label.set_text("尚未开始");
            return;
        }
        let mut c = Counters::default();
        let mut total = 0usize;
        let mut done = 0usize;
        for run in runs.iter() {
            c.add(&run.counters);
            total += run.total;
            done += (run.start_index + run.counters.tested).min(run.total);
        }
        let mut text = format!(
            "{} 个平台 · 已测 {} · 有效 {} · 鉴权失败 {} · 限流 {} · 端点 404 {} · 服务端错误 {} · 其它 4xx {} · 网络错误 {}",
            runs.len(),
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
                let remaining = (total.saturating_sub(done)) as f64;
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
            .set_text(&format!("共 {} 条 · 本地库：{:?}", n, store::db_path()));
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
        let url = probe_util::join_url(&target.base_url, &target.endpoint);
        let method = target.method().as_str();

        let slot: Arc<Mutex<Option<(crate::model::sniffer::ProbeOutcome, String, &'static str)>>> =
            Arc::new(Mutex::new(None));
        TEST_SLOT.with(|s| *s.borrow_mut() = Some(Arc::clone(&slot)));

        // tokio 异步探测：不阻塞 UI，完成后回主线程更新结果
        crate::utils::sniffer::runtime().spawn(async move {
            let client = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build();
            let outcome = match client {
                Ok(c) => probe_util::probe(&c, &target, &key).await,
                Err(e) => crate::model::sniffer::ProbeOutcome {
                    verdict: Verdict::NetworkError,
                    status: 0,
                    status_text: String::new(),
                    latency_ms: 0,
                    body: String::new(),
                    detail: format!("构建 HTTP 客户端失败：{e}"),
                },
            };
            if let Ok(mut guard) = slot.lock() {
                *guard = Some((outcome, url, method));
            }
            let _ = glib::source::idle_add(tick_test_done);
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
    /// 后台字典生成的结果中转槽（结果附带所属平台名；放在 `Arc<Mutex<..>>` 里才能跨线程搬运）。
    static GENERATE_SLOT: RefCell<
        Option<Arc<Mutex<Option<(String, Result<crate::utils::sniffer::Dictionary, String>)>>>>,
    > = RefCell::new(None);
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
