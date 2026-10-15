//! systemd 管理页面（展示层 + 控制）。
//!
//! 真正操控 systemd 的工具：
//! - 按类型 / 状态过滤并搜索系统单元（service / timer / socket / mount …）；
//! - 对选中单元执行 启动 / 停止 / 重启 / 重载 / 启用 / 禁用；
//! - 查看 `systemctl status` 详情与 `journalctl` 日志；
//! - 系统电源动作（关机 / 重启 / 挂起 / 休眠，带确认）。
//!
//! 需要 root 的操作（启停、启用、电源）在非 root 运行时通过 `pkexec` 提权；
//! 所有命令在后台线程执行，结果回到主线程刷新界面（参考 `env_editor`）。

use std::cell::{Cell, RefCell};
use std::io::BufRead;
use std::process::Child;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use glib::clone;

use crate::model::systemd::{JournalOutput, PowerAction, Scope, Timer, Unit, UnitDetail};
use crate::model::unitfile as muf;
use crate::utils::systemd as sd;
use crate::utils::unitfile as uuf;

/// 实时跟踪时最多保留的日志行数（超出后从头丢弃，避免长时间跟随后内存无限增长）。
const MAX_FOLLOW_LINES: i32 = 5000;

/// journalctl 的 stderr 提示（普通用户看不到系统日志时的 Hint）——
/// 状态行只显示一个短标记，完整提示进 tooltip，**绝不混进日志正文**。
fn journal_note(hint: &str) -> &'static str {
    if hint.trim().is_empty() {
        ""
    } else {
        "（仅当前用户日志，悬停查看原因）"
    }
}

pub struct SystemdPage {
    root: adw::ToastOverlay,
}

impl SystemdPage {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }
}

thread_local! {
    /// 当前页面的强引用（见 `page::fcitx_fix` 注释：必须是 Rc 而不是 Weak）。
    static INNER: RefCell<Option<Rc<Inner>>> = const { RefCell::new(None) };
}

fn with_inner<F: FnOnce(&Inner)>(f: F) {
    let Some(inner) = INNER.with(|i| i.try_borrow().ok().and_then(|b| b.clone())) else {
        return;
    };
    f(&inner);
}

pub fn shutdown() {
    INNER.with(|i| {
        if let Ok(mut b) = i.try_borrow_mut()
            && let Some(inner) = b.take()
        {
            // 杀掉可能仍在运行的 journalctl -f 子进程，避免孤儿进程
            if let Some(mut c) = inner.j_follow_child.take() {
                let _ = c.kill();
            }
        }
    });
}

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

const TYPE_OPTIONS: &[&str] = &[
    "全部",
    "service",
    "timer",
    "socket",
    "mount",
    "swap",
    "target",
    "path",
    "device",
    "automount",
    "scope",
    "slice",
];

/// 作用域：系统级 / 当前用户级。索引即 [`Scope`] 的取值顺序。
const SCOPE_OPTIONS: &[&str] = &["系统 (system)", "当前用户 (--user)"];

const STATE_OPTIONS: &[&str] = &[
    "全部",
    "活动 (active)",
    "非活动 (inactive)",
    "失败 (failed)",
    "已启用 (enabled)",
    "已禁用 (disabled)",
    "已屏蔽 (masked)",
    "未找到 (not-found)",
];

/// journalctl 优先级（`-p`）。索引 0 = 全部；其余对应数值优先级（0..=7）。
/// `-p N` 表示显示优先级 ≤ N 的日志。
const PRIORITY_OPTIONS: &[&str] = &[
    "全部级别",
    "emerg (0)",
    "alert (1)",
    "crit (2)",
    "err (3)",
    "warning (4)",
    "notice (5)",
    "info (6)",
    "debug (7)",
];

#[derive(Clone, Copy)]
enum UnitAction {
    Start,
    Stop,
    Restart,
    Reload,
    Enable,
    Disable,
    Mask,
    Unmask,
    ResetFailed,
}

impl UnitAction {
    fn verb(&self) -> &'static str {
        match self {
            UnitAction::Start => "start",
            UnitAction::Stop => "stop",
            UnitAction::Restart => "restart",
            UnitAction::Reload => "reload",
            UnitAction::Enable => "enable",
            UnitAction::Disable => "disable",
            UnitAction::Mask => "mask",
            UnitAction::Unmask => "unmask",
            UnitAction::ResetFailed => "reset-failed",
        }
    }
    fn label(&self) -> &'static str {
        match self {
            UnitAction::Start => "启动(start)",
            UnitAction::Stop => "停止(stop)",
            UnitAction::Restart => "重启(restart)",
            UnitAction::Reload => "重载(reload)",
            UnitAction::Enable => "启用(enable)",
            UnitAction::Disable => "禁用(disable)",
            UnitAction::Mask => "屏蔽(mask)",
            UnitAction::Unmask => "取消屏蔽(unmask)",
            UnitAction::ResetFailed => "重置失败(reset-failed)",
        }
    }

    /// 会中断正在运行的服务、或让单元彻底无法启动的操作，先弹确认框。
    fn needs_confirm(&self, unit: &Unit) -> bool {
        match self {
            UnitAction::Stop => unit.active == "active",
            // 屏蔽后连手动启动都不行，必须确认
            UnitAction::Mask => true,
            _ => false,
        }
    }

    /// 确认框的正文。
    fn confirm_body(&self) -> &'static str {
        match self {
            UnitAction::Mask => "屏蔽后该单元彻底无法启动（连手动也起不来），取消屏蔽才能恢复。",
            _ => "停止后依赖它的服务、会话或挂载点可能一并中断。",
        }
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 构造一个带 padding 的卡片容器，返回 (外层卡片, 内层内容盒)。
fn card() -> (gtk::Box, gtk::Box) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.add_css_class("card");
    outer.set_margin_top(10);
    outer.set_margin_bottom(10);
    outer.set_margin_start(12);
    outer.set_margin_end(12);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(14);
    content.set_margin_bottom(14);
    content.set_margin_start(14);
    content.set_margin_end(14);
    outer.append(&content);
    (outer, content)
}

/// 生成一个带样式类的小标签「药丸」。
fn pill(text: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("caption");
    l.add_css_class(class);
    l
}

/// 活动状态 → (展示文案, 颜色类)。
fn active_meta(active: &str) -> (&str, &str) {
    match active {
        "active" => ("运行中", "success"),
        "failed" => ("失败", "error"),
        "activating" => ("启动中", "warning"),
        "deactivating" => ("停止中", "warning"),
        "reloading" => ("重载中", "warning"),
        "inactive" => ("停用", "dim-label"),
        other => (other, "dim-label"),
    }
}

/// LoadState → 药丸文案与颜色；正常加载（loaded / 未知）返回 None，不显示药丸。
///
/// `not-found` 是「单元文件不存在」（fstab / 已卸载软件包残留的引用），
/// 以前它会被解析成一行单元名为 `●` 的垃圾记录。
fn load_meta(load: &str) -> Option<(&str, &str)> {
    match load {
        "" | "loaded" => None,
        "not-found" => Some(("未找到", "error")),
        "masked" => Some(("已屏蔽", "error")),
        "error" => Some(("加载失败", "error")),
        "bad-setting" => Some(("配置错误", "error")),
        other => Some((other, "warning")),
    }
}

/// 设置药丸的文字与颜色类（先清掉旧的颜色类，避免叠加）。
fn set_pill(label: &gtk::Label, text: &str, class: &str) {
    label.set_text(text);
    for c in ["success", "error", "warning", "dim-label"] {
        label.remove_css_class(c);
    }
    label.add_css_class(class);
}

/// 启用状态 → (展示文案, 颜色类)。
fn enabled_meta(enabled: &str) -> (&str, &str) {
    if enabled.starts_with("enabled") {
        ("开机自启", "success")
    } else if enabled == "disabled" {
        ("手动", "dim-label")
    } else if enabled == "masked" {
        ("已屏蔽", "error")
    } else if enabled == "static" {
        ("静态", "dim-label")
    } else if enabled.is_empty() {
        ("—", "dim-label")
    } else {
        (enabled, "dim-label")
    }
}

// ---------------------------------------------------------------------------
// Inner
// ---------------------------------------------------------------------------

struct Inner {
    toast_overlay: adw::ToastOverlay,

    type_row: adw::ComboRow,
    scope_row: adw::ComboRow,
    state_row: adw::ComboRow,
    search: gtk::SearchEntry,
    /// 列表统计（共 N 个 / 显示 M 个）
    list_status: gtk::Label,

    unit_list: gtk::ListBox,

    /// 整个空态容器（label + 空态里的「新建单元」按钮），编辑器打开时必须整体隐藏。
    detail_empty: gtk::Box,
    detail_content: gtk::Box,
    /// 编辑 / 新建单元文件的表单插槽（11.4）。
    editor_slot: gtk::Box,
    /// 当前打开的编辑器卡片（关闭时从插槽移除）。
    editor_card: RefCell<Option<gtk::Box>>,
    d_name: gtk::Label,
    d_desc: gtk::Label,
    d_active_pill: gtk::Label,
    d_load_pill: gtk::Label,
    d_enabled_pill: gtk::Label,
    status_buffer: gtk::TextBuffer,
    action_buttons: Vec<(UnitAction, gtk::Button)>,
    /// 单元文件（`systemctl cat`）内容与状态
    d_file_buffer: gtk::TextBuffer,
    d_file_status: gtk::Label,
    /// 电源动作按钮与标签：用户级作用域下隐藏（电源是机器级动作）
    power_buttons: Vec<gtk::Button>,
    power_label: gtk::Label,
    log_boot: gtk::CheckButton,
    /// 以 root 读取单元日志（pkexec）
    log_elevated: gtk::CheckButton,
    log_buffer: gtk::TextBuffer,
    log_status: gtk::Label,
    spinner: gtk::Spinner,

    // 系统日志模块
    j_priority: adw::ComboRow,
    j_boot: gtk::CheckButton,
    j_elevated: gtk::CheckButton,
    j_unit: gtk::Entry,
    j_lines: gtk::SpinButton,
    j_buffer: gtk::TextBuffer,
    j_view: gtk::TextView,
    j_status: gtk::Label,
    j_follow: gtk::ToggleButton,
    j_follow_child: RefCell<Option<Child>>,

    // 定时器模块
    t_list: gtk::ListBox,
    t_status: gtk::Label,
    t_spinner: gtk::Spinner,
    t_count: RefCell<usize>,

    // 状态
    units: RefCell<Vec<Unit>>,
    selected: RefCell<Option<Unit>>,
    /// 单元操作（启停/启用禁用…）进行中
    busy: Cell<bool>,
    /// 当前单元是否无 [Install]（UnitFileState=static）→ 启用/禁用置灰（D11.11）
    static_unit: Cell<bool>,
    /// 单元列表正在后台加载中
    loading: Cell<bool>,
}

impl Inner {
    fn toast(&self, msg: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(msg));
    }

    // ------------------------------------------------------------------
    // 过滤 / 列表
    // ------------------------------------------------------------------

    fn current_type(&self) -> &'static str {
        let idx = self.type_row.selected() as usize;
        if idx == 0 {
            ""
        } else {
            TYPE_OPTIONS.get(idx).copied().unwrap_or("")
        }
    }

    /// 当前作用域：索引 0 = 系统，1 = 当前用户。
    fn current_scope(&self) -> Scope {
        match self.scope_row.selected() {
            1 => Scope::User,
            _ => Scope::System,
        }
    }

    fn current_state_filter(&self) -> usize {
        self.state_row.selected() as usize
    }

    fn matches_state(&self, u: &Unit) -> bool {
        match self.current_state_filter() {
            1 => u.active == "active",
            2 => u.active == "inactive",
            3 => u.active == "failed",
            4 => u.enabled.starts_with("enabled"),
            5 => u.enabled == "disabled",
            6 => u.enabled == "masked",
            7 => u.load == "not-found",
            _ => true,
        }
    }

    /// 刷新忙碌指示（操作与列表加载共用一个 spinner）。
    fn update_busy_ui(&self) {
        if self.busy.get() || self.loading.get() {
            self.spinner.set_visible(true);
            self.spinner.start();
        } else {
            self.spinner.stop();
            self.spinner.set_visible(false);
        }
        let busy = self.busy.get();
        let static_unit = self.static_unit.get();
        for (action, b) in &self.action_buttons {
            let install_pair = matches!(action, UnitAction::Enable | UnitAction::Disable);
            b.set_sensitive(!busy && !(install_pair && static_unit));
            if install_pair {
                b.set_tooltip_text(static_unit.then_some(
                    "该单元没有 [Install] 节（UnitFileState=static），systemctl 不允许 enable/disable（GOAL 摸底 15 实测报错原文）",
                ));
            }
        }
    }

    /// 切换作用域：列表、详情、日志、定时器全部要重新取，
    /// 电源按钮只在系统作用域下有意义。
    fn on_scope_changed(&self) {
        let user = self.current_scope() == Scope::User;
        self.power_label.set_visible(!user);
        for b in &self.power_buttons {
            b.set_visible(!user);
        }
        self.selected.replace(None);
        self.close_editor(); // 编辑器绑定了旧作用域，切换时必须关
        self.units.borrow_mut().clear();
        self.rebuild_list();
        self.detail_content.set_visible(false);
        self.detail_empty.set_visible(true);
        self.refresh();
        self.load_timers();
    }

    /// 重新拉取列表（后台线程）。
    fn refresh(&self) {
        let ty = self.current_type().to_string();
        let scope = self.current_scope();
        self.loading.set(true);
        self.update_busy_ui();

        std::thread::spawn(move || {
            let res = sd::list_units(scope, &ty);
            let res = RefCell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(res) = res.take() {
                    with_inner(|i| i.apply_units(res));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_units(&self, res: Result<Vec<Unit>, String>) {
        self.loading.set(false);
        self.update_busy_ui();

        match res {
            Ok(units) => {
                *self.units.borrow_mut() = units;
                self.rebuild_list();
                // 列表是权威数据：操作后 active/enabled 会变，这里同步一次选中单元的
                // 标题与药丸，否则「点了启动，药丸还写着停用」。
                let cur = self.selected.borrow().clone();
                if let Some(u) = cur {
                    let fresh = {
                        let units = self.units.borrow();
                        units.iter().find(|x| x.name == u.name).cloned()
                    };
                    if let Some(f) = fresh {
                        *self.selected.borrow_mut() = Some(f.clone());
                        self.show_unit_meta(&f);
                    }
                }
            }
            Err(e) => self.toast(&e),
        }
    }

    // ------------------------------------------------------------------
    // 定时器（list-timers）
    // ------------------------------------------------------------------

    /// 拉取定时器列表（后台线程）。
    fn load_timers(&self) {
        let scope = self.current_scope();
        self.t_spinner.set_visible(true);
        self.t_spinner.start();
        self.t_status.set_text("正在加载定时器…");
        std::thread::spawn(move || {
            let res = sd::list_timers(scope);
            let res = RefCell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(res) = res.take() {
                    with_inner(|i| i.apply_timers(res));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_timers(&self, res: Result<Vec<Timer>, String>) {
        self.t_spinner.stop();
        self.t_spinner.set_visible(false);
        while let Some(child) = self.t_list.first_child() {
            self.t_list.remove(&child);
        }
        match res {
            Ok(timers) => {
                *self.t_count.borrow_mut() = timers.len();
                if timers.is_empty() {
                    self.t_status.set_text("没有定时器");
                    return;
                }
                for t in timers.iter() {
                    self.t_list.append(&self.build_timer_row(t));
                }
                let next_soon = timers.iter().filter(|t| !t.next_in.is_empty()).count();
                self.t_status.set_text(&format!(
                    "共 {} 个定时器，其中 {} 个已排定",
                    timers.len(),
                    next_soon
                ));
            }
            Err(e) => {
                self.t_status.set_text("加载失败");
                self.toast(&e);
            }
        }
    }

    fn build_timer_row(&self, t: &Timer) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        let h = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        h.set_margin_top(8);
        h.set_margin_bottom(8);
        h.set_margin_start(12);
        h.set_margin_end(12);

        let texts = gtk::Box::new(gtk::Orientation::Vertical, 2);
        texts.set_hexpand(true);
        let name = gtk::Label::new(Some(&t.unit));
        name.add_css_class("title-4");
        name.set_xalign(0.0);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        texts.append(&name);
        let sub = gtk::Label::new(Some(if t.activates.is_empty() {
            "（未指定激活单元）"
        } else {
            &t.activates
        }));
        sub.add_css_class("dim-label");
        sub.add_css_class("caption");
        sub.set_xalign(0.0);
        sub.set_ellipsize(gtk::pango::EllipsizeMode::End);
        texts.append(&sub);
        h.append(&texts);

        let times = gtk::Box::new(gtk::Orientation::Vertical, 2);
        times.set_halign(gtk::Align::End);
        let next = gtk::Label::new(Some(&format!(
            "下次：{}",
            if t.next_in.is_empty() {
                "未排定".to_string()
            } else {
                format!("{} 后", t.next_in)
            }
        )));
        next.add_css_class("caption");
        next.set_xalign(1.0);
        times.append(&next);
        let last = gtk::Label::new(Some(&format!(
            "上次：{}",
            if t.last_ago.is_empty() {
                "从未触发".to_string()
            } else {
                format!("{}前", t.last_ago)
            }
        )));
        last.add_css_class("caption");
        last.add_css_class("dim-label");
        last.set_xalign(1.0);
        times.append(&last);
        h.append(&times);

        row.set_child(Some(&h));
        row
    }

    /// 依据当前过滤 + 搜索重建列表。
    fn rebuild_list(&self) {
        while let Some(child) = self.unit_list.first_child() {
            self.unit_list.remove(&child);
        }
        let filter = self.search.text().to_lowercase();
        let total = self.units.borrow().len();
        let cur = self.selected.borrow().as_ref().map(|u| u.name.clone());
        let mut shown = 0usize;
        let mut keep: Option<gtk::ListBoxRow> = None;
        {
            let units = self.units.borrow();
            for u in units.iter() {
                if !self.matches_state(u) {
                    continue;
                }
                if !filter.is_empty()
                    && !u.name.to_lowercase().contains(&filter)
                    && !u.description.to_lowercase().contains(&filter)
                {
                    continue;
                }
                let row = self.build_unit_row(u);
                self.unit_list.append(&row);
                shown += 1;
                if cur.as_deref() == Some(u.name.as_str()) {
                    keep = Some(row);
                }
            }
        }
        if shown == 0 {
            let empty = gtk::Label::new(Some("没有匹配的单元"));
            empty.add_css_class("dim-label");
            empty.set_margin_top(16);
            empty.set_margin_bottom(16);
            let r = gtk::ListBoxRow::new();
            r.set_child(Some(&empty));
            r.set_activatable(false);
            self.unit_list.append(&r);
        }
        self.list_status
            .set_text(&format!("共 {total} 个单元，显示 {shown} 个"));
        // 重建会清掉选择：选中项若仍在列表里就恢复它，避免改一个搜索词就丢掉详情
        if let Some(row) = keep {
            self.unit_list.select_row(Some(&row));
        }
    }

    fn build_unit_row(&self, u: &Unit) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(&u.name);

        let h = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        h.set_margin_top(8);
        h.set_margin_bottom(8);
        h.set_margin_start(12);
        h.set_margin_end(12);

        let texts = gtk::Box::new(gtk::Orientation::Vertical, 2);
        texts.set_hexpand(true);
        let name = gtk::Label::new(Some(&u.name));
        name.add_css_class("title-4");
        name.set_xalign(0.0);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        texts.append(&name);
        let desc = gtk::Label::new(if u.description.is_empty() {
            Some("（无描述）")
        } else {
            Some(&u.description)
        });
        desc.add_css_class("dim-label");
        desc.add_css_class("caption");
        desc.set_xalign(0.0);
        desc.set_ellipsize(gtk::pango::EllipsizeMode::End);
        texts.append(&desc);
        h.append(&texts);

        // 状态药丸：加载异常（未找到/已屏蔽…）+ 运行状态 + 启用状态
        let pills = gtk::Box::new(gtk::Orientation::Vertical, 2);
        if let Some((l_text, l_class)) = load_meta(&u.load) {
            pills.append(&pill(l_text, l_class));
        }
        let (a_text, a_class) = active_meta(&u.active);
        pills.append(&pill(a_text, a_class));
        let (e_text, e_class) = enabled_meta(&u.enabled);
        pills.append(&pill(e_text, e_class));
        h.append(&pills);

        row.set_child(Some(&h));
        row
    }

    // ------------------------------------------------------------------
    // 详情 / 操作
    // ------------------------------------------------------------------

    /// 当前选中的单元是否就是 `name`（用于丢弃过期的后台结果）。
    fn is_selected(&self, name: &str) -> bool {
        let cur = self.selected.borrow();
        cur.as_ref().is_some_and(|u| u.name == name)
    }

    /// 刷新详情面板的标题 / 描述 / 药丸（纯展示，不发请求）。
    fn show_unit_meta(&self, unit: &Unit) {
        self.d_name.set_text(&unit.name);
        self.d_desc.set_text(if unit.description.is_empty() {
            "（无描述）"
        } else {
            &unit.description
        });
        let (a_text, a_class) = active_meta(&unit.active);
        set_pill(&self.d_active_pill, a_text, a_class);
        match load_meta(&unit.load) {
            Some((t, c)) => {
                set_pill(&self.d_load_pill, t, c);
                self.d_load_pill.set_visible(true);
            }
            None => self.d_load_pill.set_visible(false),
        }
        let (e_text, e_class) = enabled_meta(&unit.enabled);
        set_pill(&self.d_enabled_pill, e_text, e_class);
    }

    fn select_unit(&self, name: &str) {
        let unit = {
            let units = self.units.borrow();
            units.iter().find(|u| u.name == name).cloned()
        };
        let Some(unit) = unit else {
            self.toast("未找到该单元");
            return;
        };
        *self.selected.borrow_mut() = Some(unit.clone());
        self.static_unit.set(false);
        self.close_editor();

        self.detail_empty.set_visible(false);
        self.detail_content.set_visible(true);
        self.show_unit_meta(&unit);

        self.status_buffer.set_text("正在加载状态…");
        self.log_buffer.set_text("");
        self.log_status.set_text("尚未加载日志");
        self.log_status.set_tooltip_text(None);
        self.d_file_buffer.set_text("");
        self.d_file_status.set_text("尚未加载单元文件");

        self.fetch_status(&unit.name);
    }

    fn fetch_status(&self, name: &str) {
        let scope = self.current_scope();
        let name = name.to_string();
        std::thread::spawn(move || {
            let res = sd::unit_status(scope, &name);
            let res = RefCell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(res) = res.take() {
                    with_inner(|i| i.apply_status(&name, res));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_status(&self, name: &str, res: Result<UnitDetail, String>) {
        // 结果回来时用户可能已经切到别的单元：丢弃过期结果，
        // 否则先发出的请求会覆盖后选中单元的详情。
        if !self.is_selected(name) {
            return;
        }
        match res {
            Ok(d) => {
                let mut text = String::new();
                if !d.load.is_empty() {
                    text.push_str(&format!("LoadState:   {}\n", d.load));
                }
                if !d.active.is_empty() {
                    text.push_str(&format!("ActiveState: {}\n", d.active));
                }
                if !d.sub.is_empty() {
                    text.push_str(&format!("SubState:    {}\n", d.sub));
                }
                if !d.main_pid.is_empty() && d.main_pid != "0" {
                    text.push_str(&format!("MainPID:     {}\n", d.main_pid));
                }
                if !d.memory.is_empty() {
                    text.push_str(&format!("Memory:      {}\n", d.memory));
                }
                if !d.unit_file_state.is_empty() {
                    text.push_str(&format!("UnitFileState: {}\n", d.unit_file_state));
                }
                if !d.fragment_path.is_empty() {
                    text.push_str(&format!("FragmentPath: {}\n", d.fragment_path));
                }
                if !d.active_enter.is_empty() && d.active_enter != "n/a" {
                    text.push_str(&format!("ActiveEnterTimestamp: {}\n", d.active_enter));
                }
                if !d.restarts.is_empty() {
                    text.push_str(&format!("NRestarts:   {}\n", d.restarts));
                }
                text.push('\n');
                text.push_str(&d.status_text);
                self.status_buffer.set_text(&text);
                self.set_static_flag(&d.unit_file_state);
            }
            Err(e) => {
                self.status_buffer.set_text(&e);
                self.toast(&e);
            }
        }
    }

    /// 记录 UnitFileState=static（无 [Install]）并联动启用/禁用按钮（D11.11）。
    fn set_static_flag(&self, unit_file_state: &str) {
        self.static_unit.set(unit_file_state == "static");
        self.update_busy_ui();
    }

    /// 通用确认对话框（危险操作前二次确认）。
    fn confirm<F: Fn(&Inner) + 'static>(&self, heading: &str, body: &str, on_ok: F) {
        let dialog = gtk::MessageDialog::new(
            None::<&gtk::Window>,
            gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
            gtk::MessageType::Warning,
            gtk::ButtonsType::OkCancel,
            heading,
        );
        dialog.set_secondary_text(Some(body));
        let on_ok = Rc::new(on_ok);
        dialog.connect_response(move |d, resp| {
            if resp == gtk::ResponseType::Ok {
                with_inner(|i| on_ok(i));
            }
            d.close();
        });
        dialog.present();
    }

    fn do_action(&self, action: UnitAction) {
        let Some(unit) = self.selected.borrow().clone() else {
            return;
        };
        if self.busy.get() {
            return;
        }
        // 会中断运行中的服务、或让单元彻底起不来的操作先确认
        if action.needs_confirm(&unit) {
            let name = unit.name.clone();
            self.confirm(
                &format!("确认{} {name}？", action.label()),
                action.confirm_body(),
                move |i| i.run_action(action, &name),
            );
            return;
        }
        self.run_action(action, &unit.name);
    }

    fn run_action(&self, action: UnitAction, name: &str) {
        self.busy.set(true);
        self.update_busy_ui();

        let scope = self.current_scope();
        let name = name.to_string();
        let verb = action.verb().to_string();
        std::thread::spawn(move || {
            let res = sd::unit_action(scope, &name, &verb);
            let res = RefCell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(res) = res.take() {
                    with_inner(|i| i.apply_action(action, &name, res));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_action(&self, action: UnitAction, name: &str, res: Result<(bool, String), String>) {
        self.busy.set(false);
        self.update_busy_ui();

        match res {
            Ok((success, out)) => {
                if success {
                    self.toast(&format!("已执行：{} {}", action.label(), name));
                } else {
                    let msg = if out.is_empty() {
                        format!("{} 失败", action.label())
                    } else {
                        out
                    };
                    self.toast(&msg);
                }
            }
            Err(e) => self.toast(&e),
        }
        // 操作后刷新：详情（若还选中它）+ 列表状态（active/enabled 可能变化）
        if self.is_selected(name) {
            self.fetch_status(name);
        }
        self.refresh();
    }

    // ------------------------------------------------------------------
    // 单元文件（systemctl cat）
    // ------------------------------------------------------------------

    fn load_unit_file(&self) {
        let Some(unit) = self.selected.borrow().clone() else {
            return;
        };
        let scope = self.current_scope();
        self.d_file_status.set_text("正在加载单元文件…");
        let name = unit.name.clone();
        std::thread::spawn(move || {
            let res = sd::unit_file(scope, &name);
            let res = RefCell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(res) = res.take() {
                    with_inner(|i| i.apply_unit_file(&name, res));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_unit_file(&self, name: &str, res: Result<String, String>) {
        if !self.is_selected(name) {
            return;
        }
        match res {
            Ok(text) => {
                let lines = text.lines().count();
                self.d_file_buffer.set_text(&text);
                self.d_file_status.set_text(&format!("共 {lines} 行"));
            }
            Err(e) => {
                self.d_file_buffer.set_text(&e);
                self.d_file_status.set_text("加载失败");
                self.toast(&e);
            }
        }
    }

    // ------------------------------------------------------------------
    // 守护进程重载（daemon-reload）
    // ------------------------------------------------------------------

    /// 让 systemd 重新读取全部单元文件（改过 unit file 后必须执行）。
    fn do_daemon_reload(&self) {
        let scope = self.current_scope();
        self.toast("正在重载 systemd 守护进程…");
        std::thread::spawn(move || {
            let res = sd::daemon_reload(scope);
            let res = RefCell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(res) = res.take() {
                    with_inner(|i| i.apply_daemon_reload(res));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_daemon_reload(&self, res: Result<(bool, String), String>) {
        match res {
            Ok((true, _)) => {
                self.toast("已重载 systemd 守护进程");
                self.refresh();
            }
            Ok((false, out)) => {
                self.toast(&if out.is_empty() {
                    "重载失败".to_string()
                } else {
                    out
                });
            }
            Err(e) => self.toast(&e),
        }
    }

    // ------------------------------------------------------------------
    // 日志
    // ------------------------------------------------------------------

    fn load_logs(&self) {
        let Some(unit) = self.selected.borrow().clone() else {
            return;
        };
        let boot_only = self.log_boot.is_active();
        let scope = self.current_scope();
        let elevated = self.log_elevated.is_active();
        self.log_status.set_text("正在加载日志…");
        let name = unit.name.clone();
        std::thread::spawn(move || {
            let res = sd::unit_logs(scope, &name, boot_only, elevated);
            let res = RefCell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(res) = res.take() {
                    with_inner(|i| i.apply_logs(&name, res));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_logs(&self, name: &str, res: Result<JournalOutput, String>) {
        // 切到别的单元后回来的日志直接丢弃，否则会把 A 的日志显示在 B 名下
        if !self.is_selected(name) {
            return;
        }
        match res {
            Ok(out) => {
                self.log_buffer.set_text(&out.text);
                let lines = out.text.lines().count();
                self.log_status.set_text(&format!(
                    "共 {} 行（最近 500 行）{}",
                    lines,
                    journal_note(&out.hint)
                ));
                self.log_status
                    .set_tooltip_text(Some(out.hint.trim()).filter(|h| !h.is_empty()));
            }
            Err(e) => {
                self.log_buffer.set_text(&e);
                self.log_status.set_text("加载失败");
                self.toast(&e);
            }
        }
    }

    // ------------------------------------------------------------------
    // 系统日志（journalctl）
    // ------------------------------------------------------------------

    /// 当前日志优先级（`-p` 数值）；索引 0 = 全部。
    fn journal_priority(&self) -> Option<u8> {
        let idx = self.j_priority.selected();
        // 没有选中项时 selected() 返回 GTK_INVALID_LIST_POSITION（u32::MAX），
        // 减 1 会溢出成 254 → journalctl 收到非法的 `-p 254`。
        if idx == 0 || idx > 7 {
            None
        } else {
            // 索引 1..=8 对应数值 0..=7
            Some((idx - 1) as u8)
        }
    }

    /// 构造当前日志查询选项。
    fn journal_opts(&self) -> sd::JournalOpts {
        sd::JournalOpts {
            scope: self.current_scope(),
            priority: self.journal_priority(),
            boot_only: self.j_boot.is_active(),
            unit: Some(self.j_unit.text().to_string()),
            lines: self.j_lines.value().max(1.0) as u32,
            elevated: self.j_elevated.is_active(),
        }
    }

    /// 加载系统日志快照（后台线程）。
    fn load_journal(&self) {
        let opts = self.journal_opts();
        self.j_status.set_text("正在加载日志…");
        std::thread::spawn(move || {
            let res = sd::journal_snapshot(&opts);
            let res = RefCell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(res) = res.take() {
                    with_inner(|i| i.apply_journal(res));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_journal(&self, res: Result<JournalOutput, String>) {
        match res {
            Ok(out) => {
                self.j_buffer.set_text(&out.text);
                let lines = out.text.lines().count();
                self.j_status
                    .set_text(&format!("快照共 {} 行{}", lines, journal_note(&out.hint)));
                self.j_status
                    .set_tooltip_text(Some(out.hint.trim()).filter(|h| !h.is_empty()));
                self.scroll_journal_to_bottom();
            }
            Err(e) => {
                self.j_buffer.set_text(&e);
                self.j_status.set_text("加载失败");
                self.toast(&e);
            }
        }
    }

    /// 开始实时跟踪（`journalctl -f`）：启动子进程，逐行追加到日志区。
    fn start_follow(&self) {
        let opts = self.journal_opts();
        let args = sd::journal_base_args(&opts);
        let mut cmd = if opts.elevated && !sd::is_root() {
            // 提权跟踪：polkit 会弹一次密码框，之后子进程一直跟到底
            let mut c = std::process::Command::new("pkexec");
            c.arg("journalctl");
            c
        } else {
            std::process::Command::new("journalctl")
        };
        cmd.args(&args)
            .arg("-f")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.toast(&format!("无法启动实时日志：{e}"));
                self.j_follow.set_active(false);
                return;
            }
        };
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                self.j_follow.set_active(false);
                return;
            }
        };
        self.j_follow_child.replace(Some(child));
        self.j_status.set_text("实时跟踪中（-f）…");
        let reader = std::io::BufReader::new(stdout);
        std::thread::spawn(move || {
            // 逐行 idle_add 会把主循环刷爆（-f 全量日志每秒上千行），
            // 这里按 100ms / 64KB 攒批，一次回调整块追加。
            let mut buf = String::new();
            let mut last = Instant::now();
            let flush = |buf: &mut String, last: &mut Instant| {
                if buf.is_empty() {
                    return;
                }
                let chunk = RefCell::new(Some(std::mem::take(buf)));
                glib::source::idle_add(move || {
                    if let Some(c) = chunk.take() {
                        with_inner(|i| i.append_journal_chunk(&c));
                    }
                    glib::ControlFlow::Break
                });
                *last = Instant::now();
            };
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        buf.push_str(&l);
                        buf.push('\n');
                        if last.elapsed() >= Duration::from_millis(100) || buf.len() >= 64 * 1024 {
                            flush(&mut buf, &mut last);
                        }
                    }
                    Err(_) => break,
                }
            }
            // 收尾：把不足一批的残余也刷出去
            flush(&mut buf, &mut last);
        });
    }

    /// 停止实时跟踪（杀掉子进程，管道关闭后读取线程自然退出）。
    fn stop_follow(&self) {
        if let Some(mut c) = self.j_follow_child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.j_status.set_text("已停止实时跟踪");
    }

    /// 切换实时跟踪开关。
    fn toggle_follow(&self) {
        if self.j_follow.is_active() {
            self.start_follow();
        } else {
            self.stop_follow();
        }
    }

    /// 追加一段实时日志，并把缓冲区限制在 MAX_FOLLOW_LINES 行内。
    fn append_journal_chunk(&self, chunk: &str) {
        let mut end = self.j_buffer.end_iter();
        self.j_buffer.insert(&mut end, chunk);
        // 超长后从头丢：长时间跟随后内存不增长
        let over = self.j_buffer.line_count() - MAX_FOLLOW_LINES;
        if over > 0
            && let Some(mut cut) = self.j_buffer.iter_at_line(over)
        {
            let mut start = self.j_buffer.start_iter();
            self.j_buffer.delete(&mut start, &mut cut);
        }
        self.scroll_journal_to_bottom();
    }

    fn scroll_journal_to_bottom(&self) {
        let mut end = self.j_buffer.end_iter();
        self.j_view.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
    }

    // ------------------------------------------------------------------
    // 电源
    // ------------------------------------------------------------------

    fn confirm_power(&self, action: PowerAction) {
        self.confirm(
            &format!("确认{}？", action.label()),
            "此操作会立即影响整台机器，且不可撤销。请确认当前没有重要任务在运行。",
            move |i| i.do_power(action),
        );
    }

    fn do_power(&self, action: PowerAction) {
        self.toast(&format!("正在执行：{}…", action.label()));
        std::thread::spawn(move || {
            let res = sd::power(action);
            let res = RefCell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(res) = res.take() {
                    with_inner(|i| i.apply_power(action, res));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_power(&self, action: PowerAction, res: Result<(bool, String), String>) {
        match res {
            Ok((success, out)) => {
                let msg = if success {
                    format!("已执行：{}", action.label())
                } else if out.is_empty() {
                    format!("{} 失败", action.label())
                } else {
                    out
                };
                self.toast(&msg);
            }
            Err(e) => self.toast(&e),
        }
    }
}

// ===========================================================================
// 构建页面
// ===========================================================================

// ---------------------------------------------------------------------------
// 编辑 / 新建单元文件（GOAL 11.4 / 11.5）：属性化表单，不提供文本编辑器
// ---------------------------------------------------------------------------

/// 表单里一个受控字段（保存时按控件类型读值）。
enum FieldCtl {
    Text(adw::EntryRow),
    Select(adw::ComboRow),
    Toggle(adw::SwitchRow),
    /// 多值列表：行内输入框集合（整组替换，D11.4；容器只在构造期用，不存）。
    List(Rc<RefCell<Vec<gtk::Entry>>>),
}

/// 编辑器卡片（编辑与新建共用同一套表单，D11.1 / D11.9）。
struct UnitEditor {
    /// 目标作用域（写入目录与提权判断都看它）。
    scope: Scope,
    /// 编辑模式：目标文件路径（新建为 None）。
    target: Option<std::path::PathBuf>,
    /// 编辑模式：单元名（新建为 None，名字取自 name_row）。
    name: Option<String>,
    /// 新建模式：类型下拉（编辑为 None）。
    type_row: Option<adw::ComboRow>,
    /// 新建模式：单元名输入（编辑为 None）。
    name_row: Option<adw::EntryRow>,
    /// 表单字段：(节, 键, 控件)。
    fields: RefCell<Vec<(String, String, FieldCtl)>>,
    /// 分组容器（切换类型时清空重建）。
    form_box: gtk::Box,
    /// 对比基线：编辑 = 文件解析结果；新建 = 当前类型的默认模板。
    original: RefCell<muf::UnitDoc>,
    /// 「其它键（原样保留）」只读区（D11.4-④）。
    others_label: gtk::Label,
    /// 无 [Install] 提示（D11.11）。
    install_hint: gtk::Label,
}

/// 编辑器卡片骨架（编辑 / 新建共用）。
struct EditorShell {
    card: gtk::Box,
    content: gtk::Box,
    /// 顶部插槽：drop-in / vendor 黄条（编辑）或类型与名字（新建）。
    head_slot: gtk::Box,
    form_box: gtk::Box,
    others_label: gtk::Label,
    install_hint: gtk::Label,
}

/// 拼卡片骨架：标题 + [Install] 提示 + 表单滚动区 + 其它键只读区。
fn editor_card_shell(heading: &str) -> EditorShell {
    let (card, content) = card();
    let title = gtk::Label::new(Some(heading));
    title.add_css_class("title-4");
    title.set_halign(gtk::Align::Start);
    title.set_wrap(true);
    content.append(&title);

    let head_slot = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.append(&head_slot);

    let install_hint = gtk::Label::new(Some(
        "该单元没有 [Install] 节（UnitFileState=static）：systemctl 不允许 enable/disable。",
    ));
    install_hint.add_css_class("warning");
    install_hint.set_halign(gtk::Align::Start);
    install_hint.set_wrap(true);
    install_hint.set_visible(false);
    content.append(&install_hint);

    let form_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let form_scroll = gtk::ScrolledWindow::new();
    form_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    form_scroll.set_propagate_natural_height(true);
    form_scroll.set_child(Some(&form_box));
    content.append(&form_scroll);

    let others_title = gtk::Label::new(Some("其它键（未收录，保存时原样保留）"));
    others_title.add_css_class("heading");
    others_title.set_halign(gtk::Align::Start);
    content.append(&others_title);
    let others_label = gtk::Label::new(Some("（无）"));
    others_label.add_css_class("dim-label");
    others_label.add_css_class("monospace");
    others_label.set_halign(gtk::Align::Start);
    others_label.set_wrap(true);
    others_label.set_selectable(true);
    content.append(&others_label);

    EditorShell {
        card,
        content,
        head_slot,
        form_box,
        others_label,
        install_hint,
    }
}

/// 列表键加一行输入（读取时忽略空行）。
fn add_list_row(wrap: &gtk::Box, rows: &Rc<RefCell<Vec<gtk::Entry>>>, value: &str) {
    let h = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let e = gtk::Entry::new();
    e.set_hexpand(true);
    e.set_placeholder_text(Some("一行一项，留空行会被忽略"));
    e.set_text(value);
    let del = gtk::Button::with_label("删除");
    del.add_css_class("destructive-action");
    del.add_css_class("flat");
    let h_del = h.clone();
    let e_del = e.clone();
    let rows_d = rows.clone();
    del.connect_clicked(move |_| {
        rows_d.borrow_mut().retain(|x| x.as_ptr() != e_del.as_ptr());
        if let Some(parent) = h_del.parent()
            && let Ok(b) = parent.downcast::<gtk::Box>()
        {
            b.remove(&h_del);
        }
    });
    h.append(&e);
    h.append(&del);
    wrap.append(&h);
    rows.borrow_mut().push(e);
}

/// 按类型构建（或重建）表单：分节 → 逐键出控件（D11.1 按类型出节 / D11.4 五组）。
fn editor_build_form(ed: &Rc<UnitEditor>, unit_type: &str, baseline: muf::UnitDoc) {
    ed.original.replace(baseline);
    while let Some(c) = ed.form_box.first_child() {
        ed.form_box.remove(&c);
    }
    ed.fields.borrow_mut().clear();
    let original = ed.original.borrow().clone();

    for section in muf::sections_for_type(unit_type) {
        let group = adw::PreferencesGroup::new();
        group.set_title(&format!("[{section}]"));
        ed.form_box.append(&group);
        for spec in muf::specs_for_type(unit_type) {
            if spec.section != section {
                continue;
            }
            let cur = original.get_values(spec.section, spec.key);
            match spec.control {
                muf::Control::Text | muf::Control::Number => {
                    let row = adw::EntryRow::new();
                    let title = if spec.control == muf::Control::Number {
                        format!("{}（数字或时长，如 5 / 5s）", spec.label)
                    } else {
                        spec.label.to_string()
                    };
                    row.set_title(&title);
                    row.set_tooltip_text(Some(spec.hint));
                    if let Some(v) = cur.first() {
                        row.set_text(v);
                    }
                    group.add(&row);
                    ed.fields.borrow_mut().push((
                        spec.section.to_string(),
                        spec.key.to_string(),
                        FieldCtl::Text(row),
                    ));
                }
                muf::Control::Select => {
                    let mut opts: Vec<String> = vec!["（未设置）".to_string()];
                    opts.extend(spec.options.iter().map(|s| s.to_string()));
                    let strs: Vec<&str> = opts.iter().map(|s| s.as_str()).collect();
                    let row = adw::ComboRow::builder()
                        .title(spec.label)
                        .subtitle(spec.hint)
                        .model(&gtk::StringList::new(&strs))
                        .build();
                    if let Some(v) = cur.first()
                        && let Some(i) = spec.options.iter().position(|o| o == v)
                    {
                        row.set_selected((i + 1) as u32);
                    }
                    group.add(&row);
                    ed.fields.borrow_mut().push((
                        spec.section.to_string(),
                        spec.key.to_string(),
                        FieldCtl::Select(row),
                    ));
                }
                muf::Control::Toggle => {
                    let row = adw::SwitchRow::builder()
                        .title(spec.label)
                        .subtitle(spec.hint)
                        .build();
                    if cur.first().is_some_and(|v| v == "true") {
                        row.set_active(true);
                    }
                    group.add(&row);
                    ed.fields.borrow_mut().push((
                        spec.section.to_string(),
                        spec.key.to_string(),
                        FieldCtl::Toggle(row),
                    ));
                }
                muf::Control::List => {
                    let head = gtk::Box::new(gtk::Orientation::Vertical, 2);
                    let lbl = gtk::Label::new(Some(spec.label));
                    lbl.add_css_class("heading");
                    lbl.set_halign(gtk::Align::Start);
                    let hint = gtk::Label::new(Some(spec.hint));
                    hint.add_css_class("dim-label");
                    hint.add_css_class("caption");
                    hint.set_halign(gtk::Align::Start);
                    hint.set_wrap(true);
                    head.append(&lbl);
                    head.append(&hint);
                    group.add(&head);

                    let wrap = gtk::Box::new(gtk::Orientation::Vertical, 4);
                    let rows = Rc::new(RefCell::new(Vec::new()));
                    for v in &cur {
                        add_list_row(&wrap, &rows, v);
                    }
                    let add = gtk::Button::with_label("＋ 添加一项");
                    add.add_css_class("flat");
                    let rows_a = rows.clone();
                    let wrap_a = wrap.clone();
                    add.connect_clicked(move |_| add_list_row(&wrap_a, &rows_a, ""));
                    wrap.append(&add);
                    group.add(&wrap);
                    ed.fields.borrow_mut().push((
                        spec.section.to_string(),
                        spec.key.to_string(),
                        FieldCtl::List(rows),
                    ));
                }
            }
        }
    }

    // 其它键（未收录长尾键 / 续行键）只读区——让用户知道它们没丢（D11.4-④）
    let mut others: Vec<String> = Vec::new();
    for e in original.entries() {
        let unlisted = muf::spec_for(&e.section, &e.key).is_none();
        let continued = original.key_is_continued(&e.section, &e.key);
        if unlisted || continued {
            others.push(format!("[{}] {}={}", e.section, e.key, e.value));
        }
    }
    let text = if others.is_empty() {
        "（没有未收录的键，全部键都在上方表单里）".to_string()
    } else {
        others.join("\n")
    };
    ed.others_label.set_text(&text);

    // D11.11：无 [Install] 键 → 提示
    let has_install = original.entries().any(|e| e.section == "Install");
    ed.install_hint.set_visible(!has_install);
}

/// 从表单收集「用户改过的键」→ [`muf::FormValues`]（未改 = Unset，D11.4 只写回改动）。
fn collect_form_values(ed: &UnitEditor) -> muf::FormValues {
    let original = ed.original.borrow().clone();
    let mut form = muf::FormValues::new();
    let fields = ed.fields.borrow();
    for (section, key, ctl) in fields.iter() {
        match ctl {
            FieldCtl::Text(row) => {
                let v = row.text().to_string();
                let old = original.get_value(section, key).unwrap_or_default();
                if v == old {
                    continue;
                }
                form.fields
                    .push((section.clone(), key.clone(), muf::FormValue::Text(v)));
            }
            FieldCtl::Select(row) => {
                let Some(spec) = muf::spec_for(section, key) else {
                    continue;
                };
                let idx = row.selected() as usize;
                let old = original.get_value(section, key).unwrap_or_default();
                let new = if idx == 0 {
                    String::new()
                } else {
                    spec.options
                        .get(idx - 1)
                        .copied()
                        .unwrap_or_default()
                        .to_string()
                };
                if new == old {
                    continue;
                }
                form.fields
                    .push((section.clone(), key.clone(), muf::FormValue::Text(new)));
            }
            FieldCtl::Toggle(row) => {
                let new = row.is_active();
                let old = original.get_value(section, key).as_deref() == Some("true");
                if new == old {
                    continue;
                }
                form.fields
                    .push((section.clone(), key.clone(), muf::FormValue::Bool(new)));
            }
            FieldCtl::List(rows) => {
                let new: Vec<String> = rows
                    .borrow()
                    .iter()
                    .map(|e| e.text().to_string())
                    .filter(|t| !t.trim().is_empty())
                    .collect();
                let old = original.get_values(section, key);
                if new == old {
                    continue;
                }
                form.fields
                    .push((section.clone(), key.clone(), muf::FormValue::List(new)));
            }
        }
    }
    form
}

/// 读表单里某个字段的第一个有效值（新建 mount/swap 自动推名字用）。
fn field_first_text(ed: &UnitEditor, section: &str, key: &str) -> Option<String> {
    let fields = ed.fields.borrow();
    for (s, k, ctl) in fields.iter() {
        if s != section || k != key {
            continue;
        }
        return match ctl {
            FieldCtl::Text(row) => Some(row.text().to_string()),
            FieldCtl::Select(row) => {
                let idx = row.selected() as usize;
                if idx == 0 {
                    None
                } else {
                    muf::spec_for(section, key)
                        .and_then(|spec| spec.options.get(idx - 1))
                        .map(|s| s.to_string())
                }
            }
            FieldCtl::Toggle(row) => {
                Some(if row.is_active() { "true" } else { "false" }.to_string())
            }
            FieldCtl::List(rows) => rows
                .borrow()
                .iter()
                .map(|e| e.text().to_string())
                .find(|t| !t.trim().is_empty()),
        };
    }
    None
}

/// 变更摘要：逐键 `旧 → 新`（D11.4-⑥ 确认框内容）。
fn diff_summary(old: &muf::UnitDoc, new: &muf::UnitDoc) -> String {
    let fmt = |v: &[String]| {
        if v.is_empty() {
            "（无）".to_string()
        } else {
            v.join(" ⏎ ")
        }
    };
    let mut out: Vec<String> = Vec::new();
    for spec in muf::KEY_SPECS {
        let o = old.get_values(spec.section, spec.key);
        let n = new.get_values(spec.section, spec.key);
        if o == n {
            continue;
        }
        out.push(format!(
            "[{}] {}：{} → {}",
            spec.section,
            spec.key,
            fmt(&o),
            fmt(&n)
        ));
    }
    if out.is_empty() {
        out.push("（没有键被修改）".to_string());
    }
    out.join("\n")
}

/// 经 INNER 取当前窗口（确认框父窗口）。
fn inner_window() -> Option<gtk::Window> {
    let mut w = None;
    with_inner(|i| {
        w = i
            .toast_overlay
            .root()
            .and_then(|r| r.downcast::<gtk::Window>().ok());
    });
    w
}

/// 后台 toast（供自由函数用）。
fn toast_msg(msg: &str) {
    with_inner(|i| i.toast(msg));
}

impl Inner {
    fn close_editor(&self) {
        if let Some(card) = self.editor_card.borrow_mut().take() {
            self.editor_slot.remove(&card);
        }
        if self.selected.borrow().is_none() {
            self.detail_empty.set_visible(true);
        } else {
            // 编辑器关了，回详情视图（打开编辑器时 detail_content 被藏了）
            self.detail_content.set_visible(true);
        }
    }

    /// 编辑器/详情/空态三选一，避免卡片被前面的兄弟推出滚动视口。
    fn hide_panel_siblings(&self) {
        self.detail_empty.set_visible(false);
        self.detail_content.set_visible(false);
    }

    /// 取顶层窗口（弹确认框用）。
    fn window(&self) -> Option<gtk::Window> {
        self.toast_overlay.root()?.downcast().ok()
    }

    /// 11.6 删除单元：后台取计划（读状态，免提权）→ 确认框（D11.10）→ 后台执行 → 刷新。
    fn confirm_delete_unit(&self) {
        let Some(unit) = self.selected.borrow().clone() else {
            self.toast("先在左侧选择一个单元");
            return;
        };
        let scope = self.current_scope();
        let name = unit.name.clone();
        std::thread::spawn(move || {
            let res = uuf::delete_plan(scope, &name);
            let res = RefCell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(res) = res.take() {
                    with_inner(|i| i.show_delete_dialog(&name, res));
                }
                glib::ControlFlow::Break
            });
        });
    }

    /// 确认框：单元名 / 类型 / active / enabled / 文件路径 / drop-in 目录 + 步骤清单；
    /// 取消为默认、确认为 Destructive（D7.5 先例）。
    fn show_delete_dialog(&self, name: &str, res: Result<uuf::DeletePlan, String>) {
        let plan = match res {
            Ok(p) => p,
            Err(e) => {
                // 瞬态/无文件（含 device/scope）→ 引导用「停止」；masked → 引导先 unmask
                self.toast(&e);
                return;
            }
        };
        let Some(win) = self.window() else {
            return;
        };
        let mut body = format!(
            "单元：{}\n类型：.{}\n状态：{} / {}\n文件：{}\n",
            plan.name,
            muf::type_of(&plan.name).unwrap_or("?"),
            if plan.active {
                "运行中"
            } else {
                "未运行"
            },
            if plan.enabled {
                "已启用"
            } else {
                "未启用"
            },
            plan.fragment.display()
        );
        if !plan.drop_in_dirs.is_empty() {
            let dirs: Vec<String> = plan
                .drop_in_dirs
                .iter()
                .map(|d| d.display().to_string())
                .collect();
            body.push_str(&format!("drop-in 目录：{}\n", dirs.join(" ")));
        }
        body.push_str("\n将执行：\n");
        for (i, s) in plan.steps.iter().enumerate() {
            body.push_str(&format!("{}) {s}\n", i + 1));
        }
        if let Some(w) = &plan.vendor_warning {
            body.push_str(&format!("\n⚠ {w}\n"));
        }
        body.push_str("\n单元文件与 drop-in 目录会移动成 .linbox.bak（可恢复），不直接删除。");

        let dialog =
            adw::MessageDialog::new(Some(&win), Some(&format!("确认删除 {name}？")), Some(&body));
        dialog.add_response("cancel", "取消");
        dialog.add_response("del", "删除");
        dialog.set_response_appearance("del", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let plan_c = plan.clone();
        dialog.connect_response(None, move |d: &adw::MessageDialog, resp| {
            d.close();
            if resp == "del" {
                with_inner(|i| i.run_delete_plan(plan_c.clone()));
            }
        });
        dialog.present();
    }

    fn run_delete_plan(&self, plan: uuf::DeletePlan) {
        self.busy.set(true);
        self.update_busy_ui();
        self.toast(&format!("正在删除 {}…", plan.name));
        std::thread::spawn(move || {
            let name = plan.name.clone();
            let res = uuf::run_delete(&plan);
            let res = RefCell::new(Some((name, res)));
            glib::source::idle_add(move || {
                if let Some((name, res)) = res.take() {
                    with_inner(|i| i.apply_delete(&name, res));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_delete(&self, name: &str, res: Result<uuf::DeleteOutcome, String>) {
        self.busy.set(false);
        self.update_busy_ui();
        match res {
            Ok(o) => {
                self.selected.replace(None);
                self.detail_content.set_visible(false);
                self.detail_empty.set_visible(true);
                self.toast(&format!(
                    "已删除 {name}（备份在 {}）",
                    o.backups[0].display()
                ));
                self.refresh();
            }
            Err(e) => {
                self.toast(&e);
                self.refresh();
            }
        }
    }

    /// 打开「编辑单元文件」：后台取 FragmentPath → 可编辑闸门 → 读文件 → 回 UI 建表单。
    fn start_edit_unit(&self) {
        let Some(unit) = self.selected.borrow().clone() else {
            self.toast("先在左侧选择一个单元");
            return;
        };
        let scope = self.current_scope();
        let name = unit.name.clone();
        self.close_editor();
        self.toast("正在读取单元文件…");
        std::thread::spawn(move || {
            let res = (|| -> Result<(std::path::PathBuf, String, Vec<String>), String> {
                let paths = uuf::unit_paths(scope, &name)?;
                // vendor 警告在 UI 黄条里给（D11.2-A），这里只做可编辑闸门
                let (_kind, _warn) = uuf::check_editable(&name, &paths.fragment, scope)?;
                let path = std::path::PathBuf::from(&paths.fragment);
                let text = uuf::read_unit_text(&path)?;
                if muf::looks_like_systemctl_cat(&text) {
                    return Err(
                        "读到的是 systemctl cat 的合并输出，不是单元文件本体，拒绝编辑".to_string(),
                    );
                }
                Ok((path, text, paths.drop_ins))
            })();
            let res = RefCell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(res) = res.take() {
                    with_inner(|i| i.show_edit_editor(&name, scope, res));
                }
                glib::ControlFlow::Break
            });
        });
    }

    /// 回 UI 线程：渲染编辑器卡片。
    fn show_edit_editor(
        &self,
        name: &str,
        scope: Scope,
        res: Result<(std::path::PathBuf, String, Vec<String>), String>,
    ) {
        let (path, text, drop_ins) = match res {
            Ok(t) => t,
            Err(e) => {
                self.toast(&e);
                return;
            }
        };
        let Some(unit_type) = muf::type_of(name) else {
            self.toast("单元名不合法");
            return;
        };
        let original = muf::parse_unit_text(&text);
        let shell = editor_card_shell(&format!("编辑单元文件：{name}"));

        // 黄条：drop-in 覆盖（D11.4-③）与 vendor 升级覆盖（D11.2-A）
        if !drop_ins.is_empty() {
            let n = gtk::Label::new(Some(&format!(
                "该单元有 {} 个 drop-in 覆盖（本表单改的是主文件，drop-in 里的值优先生效）：\n{}",
                drop_ins.len(),
                drop_ins.join("\n")
            )));
            n.add_css_class("warning");
            n.set_halign(gtk::Align::Start);
            n.set_wrap(true);
            shell.head_slot.append(&n);
        }
        if uuf::target_kind(&path.to_string_lossy(), scope).is_vendor() {
            let n = gtk::Label::new(Some(
                "该文件由软件包管理，升级会覆盖本次修改（D11.2-A 已知风险，仍允许直改）",
            ));
            n.add_css_class("warning");
            n.set_halign(gtk::Align::Start);
            n.set_wrap(true);
            shell.head_slot.append(&n);
        }

        let ed = Rc::new(UnitEditor {
            scope,
            target: Some(path),
            name: Some(name.to_string()),
            type_row: None,
            name_row: None,
            fields: RefCell::new(Vec::new()),
            form_box: shell.form_box.clone(),
            original: RefCell::new(original.clone()),
            others_label: shell.others_label.clone(),
            install_hint: shell.install_hint.clone(),
        });
        editor_build_form(&ed, unit_type, original);

        let cancel = gtk::Button::with_label("取消");
        cancel.add_css_class("flat");
        cancel.connect_clicked(move |_| with_inner(|i| i.close_editor()));
        let save = gtk::Button::with_label("保存");
        save.add_css_class("suggested-action");
        let ed_s = ed.clone();
        save.connect_clicked(move |_| editor_save(&ed_s));
        let btns = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        btns.set_halign(gtk::Align::End);
        btns.append(&cancel);
        btns.append(&save);
        shell.content.append(&btns);

        self.hide_panel_siblings();
        self.editor_slot.append(&shell.card);
        *self.editor_card.borrow_mut() = Some(shell.card.clone());
    }

    /// 打开「新建单元」：先选类型（9 类）→ 同一套表单（D11.9）。
    fn open_new_unit_editor(&self) {
        let scope = self.current_scope();
        self.close_editor();
        self.hide_panel_siblings();
        let shell = editor_card_shell("新建单元文件");

        let head_group = adw::PreferencesGroup::new();
        head_group.set_title("类型与名字");
        let labels: Vec<String> = muf::NEWABLE_TYPES.iter().map(|t| format!(".{t}")).collect();
        let strs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
        let type_row = adw::ComboRow::builder()
            .title("单元类型")
            .subtitle("后缀由类型决定，不用手打；.device / .scope 由系统动态生成，不提供新建")
            .model(&gtk::StringList::new(&strs))
            .selected(0)
            .build();
        head_group.add(&type_row);
        let name_row = adw::EntryRow::new();
        name_row.set_title(
            "单元名（如 linbox-job.service；mount / swap 可留空，由挂载点 / 设备自动推导）",
        );
        head_group.add(&name_row);
        shell.head_slot.append(&head_group);

        let first_type = muf::NEWABLE_TYPES[0];
        let baseline = uuf::default_new_doc(scope, first_type)
            .map(|s| muf::parse_unit_text(&s))
            .unwrap_or_default();
        let ed = Rc::new(UnitEditor {
            scope,
            target: None,
            name: None,
            type_row: Some(type_row.clone()),
            name_row: Some(name_row.clone()),
            fields: RefCell::new(Vec::new()),
            form_box: shell.form_box.clone(),
            original: RefCell::new(baseline.clone()),
            others_label: shell.others_label.clone(),
            install_hint: shell.install_hint.clone(),
        });
        editor_build_form(&ed, first_type, baseline);

        // 切类型 → 按该类型的默认模板重建表单
        let ed_t = ed.clone();
        type_row.connect_selected_notify(move |row| {
            let idx = row.selected() as usize;
            let Some(&t) = muf::NEWABLE_TYPES.get(idx) else {
                return;
            };
            let baseline = uuf::default_new_doc(ed_t.scope, t)
                .map(|s| muf::parse_unit_text(&s))
                .unwrap_or_default();
            editor_build_form(&ed_t, t, baseline);
        });

        let cancel = gtk::Button::with_label("取消");
        cancel.add_css_class("flat");
        cancel.connect_clicked(move |_| with_inner(|i| i.close_editor()));
        let save = gtk::Button::with_label("创建");
        save.add_css_class("suggested-action");
        let ed_s = ed.clone();
        save.connect_clicked(move |_| editor_save(&ed_s));
        let btns = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        btns.set_halign(gtk::Align::End);
        btns.append(&cancel);
        btns.append(&save);
        shell.content.append(&btns);

        self.editor_slot.append(&shell.card);
        *self.editor_card.borrow_mut() = Some(shell.card.clone());
    }

    /// 写入成功后的收尾：关编辑器 + 刷新 + 重启提示。
    fn apply_editor_write(&self, name: &str, is_new: bool, res: Result<(), String>) {
        match res {
            Ok(()) => {
                self.close_editor();
                let msg = if is_new {
                    format!("已创建 {name}（已 daemon-reload；需要时可启用/启动）")
                } else {
                    format!("已保存 {name}（已 daemon-reload）")
                };
                self.toast(&msg);
                if !is_new {
                    let toast = adw::Toast::new(&format!("{name} 已保存"));
                    toast.set_button_label(Some("重启该服务"));
                    let name2 = name.to_string();
                    toast.connect_button_clicked(move |_| {
                        with_inner(|i| i.run_action(UnitAction::Restart, &name2));
                    });
                    self.toast_overlay.add_toast(toast);
                }
                self.refresh();
                if let Some(u) = self.selected.borrow().clone()
                    && u.name == name
                {
                    self.fetch_status(name);
                    self.load_unit_file();
                }
            }
            Err(e) => self.toast(&e),
        }
    }
}

/// 保存链路：收集 → apply → validate → 摘要确认 → 后台 verify+write（11.4-⑥ / 11.5）。
fn editor_save(ed: &Rc<UnitEditor>) {
    let scope = ed.scope;

    // 1) 类型与名字
    let (unit_type, name, is_new) = if let (Some(trow), Some(nrow)) = (&ed.type_row, &ed.name_row) {
        let idx = trow.selected() as usize;
        let Some(&unit_type) = muf::NEWABLE_TYPES.get(idx) else {
            toast_msg("请选择单元类型");
            return;
        };
        let mut name = nrow.text().to_string();
        if name.trim().is_empty() {
            // mount / automount 由 Where、swap 由 What 自动推导（D11.9-②）
            let (sec, key) = match unit_type {
                "mount" => ("Mount", "Where"),
                "automount" => ("Automount", "Where"),
                "swap" => ("Swap", "What"),
                _ => {
                    toast_msg("请填写单元名");
                    return;
                }
            };
            let Some(v) = field_first_text(ed, sec, key) else {
                toast_msg("请填写单元名，或先填挂载点 / 设备让名字自动生成");
                return;
            };
            name = if unit_type == "swap" {
                uuf::unit_name_from_what(&v)
            } else {
                uuf::unit_name_from_where(&v, unit_type)
            };
        }
        (unit_type, name, true)
    } else {
        let Some(name) = ed.name.clone() else {
            return;
        };
        let Some(unit_type) = muf::type_of(&name) else {
            return;
        };
        (unit_type, name, false)
    };

    if let Err(e) = uuf::validate_unit_name(&name, unit_type) {
        toast_msg(&e);
        return;
    }

    // 2) 基线 → 收集改动 → 应用 → 校验
    let original = if is_new {
        match uuf::default_new_doc(scope, unit_type) {
            Ok(s) => muf::parse_unit_text(&s),
            Err(e) => {
                toast_msg(&e);
                return;
            }
        }
    } else {
        ed.original.borrow().clone()
    };
    let form = collect_form_values(ed);
    let new_doc = match muf::apply_form(&original, &form, unit_type) {
        Ok(d) => d,
        Err(e) => {
            toast_msg(&e);
            return;
        }
    };
    if let Err(e) = muf::validate_form(&new_doc, unit_type, Some(&name)) {
        toast_msg(&format!("校验未通过：{e}"));
        return;
    }
    let rendered = muf::render_unit_text(&new_doc);
    let summary = diff_summary(&original, &new_doc);

    // 3) 目标路径
    let target = if is_new {
        match uuf::create_path(scope, &name) {
            Ok(p) => p,
            Err(e) => {
                toast_msg(&e);
                return;
            }
        }
    } else {
        match ed.target.clone() {
            Some(p) => p,
            None => {
                toast_msg("缺少目标路径，无法保存");
                return;
            }
        }
    };

    // 4) 变更摘要确认框（D11.4-⑥）
    let Some(win) = inner_window() else {
        return;
    };
    let heading = if is_new {
        format!("创建 {name}？")
    } else {
        format!("保存对 {name} 的修改？")
    };
    let body = format!(
        "{summary}

落盘路径：{}

保存后自动 daemon-reload；系统文件会弹一次授权框。",
        target.display()
    );
    let dialog = adw::MessageDialog::new(Some(&win), Some(&heading), Some(&body));
    dialog.add_response("cancel", "取消");
    dialog.add_response("ok", if is_new { "创建" } else { "保存" });
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let ed2 = ed.clone();
    dialog.connect_response(None, move |d, resp| {
        d.close();
        if resp != "ok" {
            return;
        }
        run_editor_write(
            ed2.clone(),
            unit_type,
            name.clone(),
            rendered.clone(),
            target.clone(),
            is_new,
        );
    });
    dialog.present();
}

/// 后台执行：（新建再查一次同名）→ `systemd-analyze verify` → 备份落盘（11.3 链路）。
fn run_editor_write(
    ed: Rc<UnitEditor>,
    unit_type: &'static str,
    name: String,
    rendered: String,
    target: std::path::PathBuf,
    is_new: bool,
) {
    let scope = ed.scope;
    toast_msg(if is_new {
        "正在校验并创建…"
    } else {
        "正在校验并保存…"
    });
    std::thread::spawn(move || {
        let res = (|| -> Result<(), String> {
            if is_new && let Some(c) = uuf::name_conflict(scope, &name)? {
                return Err(c);
            }
            uuf::verify_unit(&rendered, &name, unit_type)
                .map_err(|e| format!("systemd-analyze verify 未通过：\n{e}"))?;
            uuf::write_unit(&target, &rendered, scope)?;
            Ok(())
        })();
        let res = RefCell::new(Some(res));
        glib::source::idle_add(move || {
            if let Some(res) = res.take() {
                with_inner(|i| i.apply_editor_write(&name, is_new, res));
            }
            glib::ControlFlow::Break
        });
    });
}

pub fn build() -> SystemdPage {
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_vexpand(true);
    let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root_box.set_margin_top(12);
    root_box.set_margin_bottom(12);
    root_box.set_margin_start(12);
    root_box.set_margin_end(12);
    root_box.set_vexpand(true);
    toast_overlay.set_child(Some(&root_box));

    // ---------- 标题 ----------
    let title = gtk::Label::new(Some("systemd 服务管理"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    root_box.append(&title);

    let subtitle = gtk::Label::new(Some(
        "真正操控 systemd：系统级 / 当前用户级（--user）两种作用域，浏览 / 过滤 / 启停 / 启用禁用 \
         / 屏蔽 / 重置失败，查看状态、单元文件、依赖日志与定时器。\
         系统级单元需要 root，会通过 pkexec 弹窗提权；用户级单元不提权。",
    ));
    subtitle.add_css_class("dim-label");
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_wrap(true);
    subtitle.set_margin_top(2);
    root_box.append(&subtitle);

    // ---------- 标签页：单元管理 / 定时器 / 系统日志 ----------
    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    let switcher = gtk::StackSwitcher::new();
    switcher.set_stack(Some(&stack));
    switcher.set_halign(gtk::Align::Start);
    switcher.set_margin_top(10);
    root_box.append(&switcher);
    root_box.append(&stack);

    let tab_manage = gtk::Box::new(gtk::Orientation::Vertical, 0);
    tab_manage.set_vexpand(true);
    stack.add_titled(&tab_manage, Some("manage"), "单元管理");

    // ---------- 过滤器卡片 ----------
    let (filter_card, filter_content) = card();
    tab_manage.append(&filter_card);

    // ComboRow 是 GtkListBoxRow 子类，必须置于 ListBox/PreferencesGroup 内才会激活下拉，
    // 放进普通 Box 点不开（见项目 media_converter 同类坑）。
    let filter_group = adw::PreferencesGroup::new();
    filter_content.append(&filter_group);

    let type_row = adw::ComboRow::builder()
        .title("单元类型")
        .model(&gtk::StringList::new(TYPE_OPTIONS))
        .selected(1) // 默认 service
        .build();
    filter_group.add(&type_row);

    let scope_row = adw::ComboRow::builder()
        .title("作用域")
        .subtitle("系统级单元需要提权；用户级单元由你自己的 systemd 管理")
        .model(&gtk::StringList::new(SCOPE_OPTIONS))
        .selected(0)
        .build();
    filter_group.add(&scope_row);

    let state_row = adw::ComboRow::builder()
        .title("状态过滤")
        .model(&gtk::StringList::new(STATE_OPTIONS))
        .selected(0)
        .build();
    filter_group.add(&state_row);

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("搜索单元名或描述…"));
    search.set_hexpand(true);
    search.set_margin_top(4);
    filter_content.append(&search);

    let list_status = gtk::Label::new(Some("正在加载…"));
    list_status.add_css_class("dim-label");
    list_status.add_css_class("caption");
    list_status.set_halign(gtk::Align::Start);
    list_status.set_margin_top(2);
    filter_content.append(&list_status);

    // 刷新 + 守护进程重载 + 电源动作行
    let action_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    action_row.set_margin_top(8);

    let refresh_btn = gtk::Button::with_label("刷新列表");
    refresh_btn.set_icon_name("view-refresh-symbolic");
    action_row.append(&refresh_btn);

    let reload_btn = gtk::Button::with_label("重载守护进程");
    reload_btn.set_tooltip_text(Some(
        "systemctl daemon-reload：改过单元文件后让 systemd 重新读取（系统级会提权）",
    ));
    action_row.append(&reload_btn);

    let new_unit_btn = gtk::Button::with_label("新建单元");
    new_unit_btn.set_tooltip_text(Some(
        "新建 .service / .timer / .mount 等单元文件：先选类型，再按属性表单填写（落当前作用域目录）",
    ));
    action_row.append(&new_unit_btn);

    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);
    spinner.set_margin_start(4);
    action_row.append(&spinner);

    let spacer = gtk::Label::new(None);
    spacer.set_hexpand(true);
    action_row.append(&spacer);

    let power_label = gtk::Label::new(Some("电源："));
    power_label.add_css_class("dim-label");
    action_row.append(&power_label);

    let mut power_buttons = Vec::new();
    for (label, action) in [
        ("关机", PowerAction::Shutdown),
        ("重启", PowerAction::Reboot),
        ("挂起", PowerAction::Suspend),
        ("休眠", PowerAction::Hibernate),
    ] {
        let b = gtk::Button::with_label(label);
        b.add_css_class("flat");
        b.connect_clicked(clone!(move |_| with_inner(|i| i.confirm_power(action))));
        action_row.append(&b);
        power_buttons.push(b);
    }
    filter_content.append(&action_row);

    // ---------- 主区域：列表 + 详情（Paned 可拖动分栏） ----------
    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_vexpand(true);
    paned.set_position(360);
    tab_manage.append(&paned);

    // =====================================================================
    // 定时器（list-timers）标签页
    // =====================================================================
    let tab_timers = gtk::Box::new(gtk::Orientation::Vertical, 0);
    tab_timers.set_vexpand(true);
    tab_timers.set_margin_top(10);
    stack.add_titled(&tab_timers, Some("timers"), "定时器");

    let (t_card, t_content) = card();
    tab_timers.append(&t_card);
    let t_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let t_title = gtk::Label::new(Some("systemctl list-timers：谁在什么时候自动跑"));
    t_title.add_css_class("title-4");
    t_title.set_hexpand(true);
    t_title.set_halign(gtk::Align::Start);
    t_bar.append(&t_title);
    let t_spinner = gtk::Spinner::new();
    t_spinner.set_visible(false);
    t_bar.append(&t_spinner);
    let t_reload = gtk::Button::with_label("刷新");
    t_reload.set_icon_name("view-refresh-symbolic");
    t_reload.add_css_class("suggested-action");
    t_bar.append(&t_reload);
    t_content.append(&t_bar);

    let t_status = gtk::Label::new(Some("尚未加载"));
    t_status.add_css_class("dim-label");
    t_status.add_css_class("caption");
    t_status.set_halign(gtk::Align::Start);
    t_content.append(&t_status);

    let t_list = gtk::ListBox::new();
    t_list.add_css_class("boxed-list");
    t_list.set_selection_mode(gtk::SelectionMode::None);
    let t_scroll = gtk::ScrolledWindow::new();
    t_scroll.set_child(Some(&t_list));
    t_scroll.set_vexpand(true);
    tab_timers.append(&t_scroll);

    // =====================================================================
    // 系统日志（journalctl）标签页
    // =====================================================================
    let tab_journal = gtk::Box::new(gtk::Orientation::Vertical, 0);
    tab_journal.set_vexpand(true);
    tab_journal.set_margin_top(10);
    stack.add_titled(&tab_journal, Some("journal"), "系统日志");

    // 控件卡片
    let (j_card, j_content) = card();
    tab_journal.append(&j_card);

    // 优先级 ComboRow（需置于 PreferencesGroup 才会激活）
    let j_pri_group = adw::PreferencesGroup::new();
    j_content.append(&j_pri_group);
    let j_priority = adw::ComboRow::builder()
        .title("优先级过滤 (-p)")
        .model(&gtk::StringList::new(PRIORITY_OPTIONS))
        .selected(0)
        .build();
    j_pri_group.add(&j_priority);

    // 单元名 / 行数 / 仅本次启动
    let j_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    j_bar.set_margin_top(4);
    let j_unit = gtk::Entry::new();
    j_unit.set_placeholder_text(Some("按单元名过滤（可选），如 nginx.service"));
    j_unit.set_hexpand(true);
    j_bar.append(&j_unit);

    let j_lines_lbl = gtk::Label::new(Some("行数"));
    j_lines_lbl.add_css_class("dim-label");
    j_bar.append(&j_lines_lbl);
    let j_lines = gtk::SpinButton::with_range(1.0, 100000.0, 100.0);
    j_lines.set_value(500.0);
    j_lines.set_numeric(true);
    j_bar.append(&j_lines);

    let j_boot = gtk::CheckButton::with_label("仅本次启动 (-b)");
    j_bar.append(&j_boot);
    let j_elevated = gtk::CheckButton::with_label("以 root 读取");
    j_elevated.set_tooltip_text(Some(
        "普通用户看不到系统日志（不在 adm / systemd-journal 组时只有自己的日志）。\
         勾选后通过 pkexec 提权读取，会弹一次密码框。",
    ));
    j_bar.append(&j_elevated);
    j_content.append(&j_bar);

    // 按钮行：加载快照 / 实时跟踪 / 清除
    let j_btn_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    j_btn_bar.set_margin_top(8);
    let j_load = gtk::Button::with_label("加载日志");
    j_load.set_icon_name("view-refresh-symbolic");
    j_load.add_css_class("suggested-action");
    j_btn_bar.append(&j_load);

    let j_follow = gtk::ToggleButton::with_label("实时跟踪 (-f)");
    j_follow.set_icon_name("media-record-symbolic");
    j_btn_bar.append(&j_follow);

    let j_clear = gtk::Button::with_label("清除");
    j_clear.add_css_class("flat");
    j_btn_bar.append(&j_clear);

    let j_status = gtk::Label::new(Some("尚未加载日志"));
    j_status.add_css_class("dim-label");
    j_status.add_css_class("caption");
    j_status.set_hexpand(true);
    j_status.set_halign(gtk::Align::End);
    j_btn_bar.append(&j_status);
    j_content.append(&j_btn_bar);

    // 日志文本区
    let j_buffer = gtk::TextBuffer::new(None);
    let j_view = gtk::TextView::with_buffer(&j_buffer);
    j_view.set_monospace(true);
    j_view.set_editable(false);
    j_view.set_cursor_visible(false);
    j_view.set_wrap_mode(gtk::WrapMode::None);
    let j_scroll = gtk::ScrolledWindow::new();
    j_scroll.set_child(Some(&j_view));
    j_scroll.set_min_content_height(200);
    j_scroll.set_vexpand(true);
    tab_journal.append(&j_scroll);

    // 左：单元列表
    let unit_list = gtk::ListBox::new();
    unit_list.add_css_class("boxed-list");
    unit_list.set_selection_mode(gtk::SelectionMode::Single);
    let list_scroll = gtk::ScrolledWindow::new();
    list_scroll.set_child(Some(&unit_list));
    list_scroll.set_min_content_width(180);
    list_scroll.set_size_request(220, -1);
    list_scroll.set_vexpand(true);
    paned.set_start_child(Some(&list_scroll));

    // 右：详情
    let detail_scroll = gtk::ScrolledWindow::new();
    detail_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    detail_scroll.set_vexpand(true);
    let detail_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    detail_scroll.set_child(Some(&detail_box));
    paned.set_end_child(Some(&detail_scroll));

    // 空态（带「新建单元」入口，GOAL 11.5）
    let detail_empty_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
    detail_empty_box.set_halign(gtk::Align::Center);
    detail_empty_box.set_margin_top(40);
    detail_empty_box.set_margin_bottom(40);
    let detail_empty = gtk::Label::new(Some("从左侧选择一个单元以查看详情、执行操作或查看日志。"));
    detail_empty.add_css_class("dim-label");
    detail_empty.set_wrap(true);
    detail_empty_box.append(&detail_empty);
    let empty_new_btn = gtk::Button::with_label("新建单元");
    empty_new_btn.add_css_class("suggested-action");
    detail_empty_box.append(&empty_new_btn);
    detail_box.append(&detail_empty_box);

    // 详情内容
    let detail_content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    detail_content.set_visible(false);
    detail_box.append(&detail_content);

    // 编辑 / 新建单元文件的表单插槽（11.4）
    let editor_slot = gtk::Box::new(gtk::Orientation::Vertical, 10);
    detail_box.append(&editor_slot);

    let d_name = gtk::Label::new(Some(""));
    d_name.add_css_class("title-3");
    d_name.set_halign(gtk::Align::Start);
    d_name.set_wrap(true);
    detail_content.append(&d_name);

    let d_desc = gtk::Label::new(Some(""));
    d_desc.add_css_class("dim-label");
    d_desc.set_halign(gtk::Align::Start);
    d_desc.set_wrap(true);
    detail_content.append(&d_desc);

    // 状态药丸行
    let pills_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    pills_row.set_margin_top(6);
    let d_load_pill = pill("—", "error");
    d_load_pill.set_visible(false);
    let d_active_pill = pill("—", "dim-label");
    let d_enabled_pill = pill("—", "dim-label");
    pills_row.append(&d_load_pill);
    pills_row.append(&d_active_pill);
    pills_row.append(&d_enabled_pill);
    detail_content.append(&pills_row);

    // 状态卡片
    let (status_card, status_content) = card();
    detail_content.append(&status_card);
    let status_title = gtk::Label::new(Some("systemctl status"));
    status_title.add_css_class("title-4");
    status_title.set_halign(gtk::Align::Start);
    status_content.append(&status_title);
    let status_buffer = gtk::TextBuffer::new(None);
    let status_view = gtk::TextView::with_buffer(&status_buffer);
    status_view.set_monospace(true);
    status_view.set_editable(false);
    status_view.set_cursor_visible(false);
    status_view.set_wrap_mode(gtk::WrapMode::WordChar);
    let status_scroll = gtk::ScrolledWindow::new();
    status_scroll.set_child(Some(&status_view));
    status_scroll.set_min_content_height(160);
    status_scroll.set_max_content_height(320);
    status_content.append(&status_scroll);

    // 操作卡片
    let (op_card, op_content) = card();
    detail_content.append(&op_card);
    let op_title = gtk::Label::new(Some("操作（需要权限时会弹窗提权）"));
    op_title.add_css_class("title-4");
    op_title.set_halign(gtk::Align::Start);
    op_content.append(&op_title);

    let btn_grid = gtk::Box::new(gtk::Orientation::Vertical, 6);
    btn_grid.set_margin_top(4);
    let mut action_buttons = Vec::new();
    for row in [
        vec![
            UnitAction::Start,
            UnitAction::Stop,
            UnitAction::Restart,
            UnitAction::Reload,
        ],
        vec![
            UnitAction::Enable,
            UnitAction::Disable,
            UnitAction::Mask,
            UnitAction::Unmask,
            UnitAction::ResetFailed,
        ],
    ] {
        let h = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        for a in row {
            let b = gtk::Button::with_label(a.label());
            b.connect_clicked(clone!(move |_| with_inner(|i| i.do_action(a))));
            h.append(&b);
            action_buttons.push((a, b));
        }
        btn_grid.append(&h);
    }
    op_content.append(&btn_grid);

    let edit_unit_btn = gtk::Button::with_label("编辑单元文件…");
    edit_unit_btn.set_tooltip_text(Some(
        "按属性表单化编辑该单元的单元文件（不是文本编辑器）；发行版文件会提示升级覆盖风险并提权",
    ));
    edit_unit_btn.set_margin_top(6);
    op_content.append(&edit_unit_btn);

    // 单元文件卡片（systemctl cat）
    let (file_card, file_content) = card();
    detail_content.append(&file_card);
    let file_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let file_title = gtk::Label::new(Some("单元文件（systemctl cat）"));
    file_title.add_css_class("title-4");
    file_title.set_hexpand(true);
    file_title.set_halign(gtk::Align::Start);
    file_bar.append(&file_title);
    let file_load_btn = gtk::Button::with_label("查看");
    file_bar.append(&file_load_btn);
    let d_file_status = gtk::Label::new(Some("尚未加载单元文件"));
    d_file_status.add_css_class("dim-label");
    d_file_status.add_css_class("caption");
    d_file_status.set_halign(gtk::Align::End);
    file_bar.append(&d_file_status);
    file_content.append(&file_bar);

    let d_file_buffer = gtk::TextBuffer::new(None);
    let file_view = gtk::TextView::with_buffer(&d_file_buffer);
    file_view.set_monospace(true);
    file_view.set_editable(false);
    file_view.set_cursor_visible(false);
    file_view.set_wrap_mode(gtk::WrapMode::None);
    let file_scroll = gtk::ScrolledWindow::new();
    file_scroll.set_child(Some(&file_view));
    file_scroll.set_min_content_height(120);
    file_scroll.set_max_content_height(320);
    file_content.append(&file_scroll);

    // 日志卡片
    let (log_card, log_content) = card();
    detail_content.append(&log_card);
    let log_title = gtk::Label::new(Some("journalctl 日志"));
    log_title.add_css_class("title-4");
    log_title.set_halign(gtk::Align::Start);
    log_content.append(&log_title);

    let log_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let log_boot = gtk::CheckButton::with_label("仅本次启动 (-b)");
    log_bar.append(&log_boot);
    let log_elevated = gtk::CheckButton::with_label("以 root 读取");
    log_elevated.set_tooltip_text(Some(
        "系统级单元的日志普通用户看不到（只有 \"No entries\"），\
         勾选后通过 pkexec 提权读取。",
    ));
    log_bar.append(&log_elevated);
    let log_load_btn = gtk::Button::with_label("加载日志");
    log_load_btn.add_css_class("suggested-action");
    log_bar.append(&log_load_btn);
    let log_status = gtk::Label::new(Some("尚未加载日志"));
    log_status.add_css_class("dim-label");
    log_status.add_css_class("caption");
    log_status.set_hexpand(true);
    log_status.set_halign(gtk::Align::End);
    log_bar.append(&log_status);
    log_content.append(&log_bar);

    let log_buffer = gtk::TextBuffer::new(None);
    let log_view = gtk::TextView::with_buffer(&log_buffer);
    log_view.set_monospace(true);
    log_view.set_editable(false);
    log_view.set_cursor_visible(false);
    log_view.set_wrap_mode(gtk::WrapMode::None);
    let log_scroll = gtk::ScrolledWindow::new();
    log_scroll.set_child(Some(&log_view));
    log_scroll.set_min_content_height(160);
    log_scroll.set_max_content_height(360);
    log_content.append(&log_scroll);

    // ---------- 组装 Inner ----------
    let inner = Rc::new(Inner {
        toast_overlay: toast_overlay.clone(),
        type_row,
        scope_row,
        state_row,
        search: search.clone(),
        list_status: list_status.clone(),
        unit_list: unit_list.clone(),
        detail_empty: detail_empty_box.clone(),
        detail_content: detail_content.clone(),
        d_name: d_name.clone(),
        d_desc: d_desc.clone(),
        d_active_pill: d_active_pill.clone(),
        d_load_pill: d_load_pill.clone(),
        d_enabled_pill: d_enabled_pill.clone(),
        status_buffer: status_buffer.clone(),
        action_buttons,
        d_file_buffer: d_file_buffer.clone(),
        d_file_status: d_file_status.clone(),
        power_buttons,
        power_label: power_label.clone(),
        log_boot: log_boot.clone(),
        log_elevated: log_elevated.clone(),
        log_buffer: log_buffer.clone(),
        log_status: log_status.clone(),
        spinner: spinner.clone(),

        // 系统日志模块
        j_priority: j_priority.clone(),
        j_boot: j_boot.clone(),
        j_elevated: j_elevated.clone(),
        j_unit: j_unit.clone(),
        j_lines: j_lines.clone(),
        j_buffer: j_buffer.clone(),
        j_view: j_view.clone(),
        j_status: j_status.clone(),
        j_follow: j_follow.clone(),
        j_follow_child: RefCell::new(None),

        // 定时器模块
        t_list: t_list.clone(),
        t_status: t_status.clone(),
        t_spinner: t_spinner.clone(),
        t_count: RefCell::new(0),

        editor_slot,
        editor_card: RefCell::new(None),
        units: RefCell::new(Vec::new()),
        selected: RefCell::new(None),
        busy: Cell::new(false),
        static_unit: Cell::new(false),
        loading: Cell::new(false),
    });

    // ---------- 信号 ----------
    // 过滤变化
    inner.type_row.connect_selected_notify(clone!(
        #[weak]
        inner,
        move |_| inner.refresh()
    ));
    inner.scope_row.connect_selected_notify(clone!(
        #[weak]
        inner,
        move |_| inner.on_scope_changed()
    ));
    inner.state_row.connect_selected_notify(clone!(
        #[weak]
        inner,
        move |_| inner.rebuild_list()
    ));
    search.connect_search_changed(clone!(
        #[weak]
        inner,
        move |_| inner.rebuild_list()
    ));
    refresh_btn.connect_clicked(clone!(
        #[weak]
        inner,
        move |_| inner.refresh()
    ));
    reload_btn.connect_clicked(clone!(
        #[weak]
        inner,
        move |_| inner.do_daemon_reload()
    ));
    new_unit_btn.connect_clicked(clone!(
        #[weak]
        inner,
        move |_| inner.open_new_unit_editor()
    ));
    empty_new_btn.connect_clicked(clone!(
        #[weak]
        inner,
        move |_| inner.open_new_unit_editor()
    ));
    edit_unit_btn.connect_clicked(clone!(
        #[weak]
        inner,
        move |_| inner.start_edit_unit()
    ));

    let delete_unit_btn = gtk::Button::with_label("删除单元…");
    delete_unit_btn.add_css_class("destructive-action");
    delete_unit_btn.set_tooltip_text(Some(
        "停止（若在运行）→ 禁用（若已启用）→ 单元文件与 drop-in 目录 mv 成 .linbox.bak → daemon-reload；发行版文件删除后升级会重装",
    ));
    delete_unit_btn.set_margin_top(6);
    op_content.append(&delete_unit_btn);
    delete_unit_btn.connect_clicked(clone!(
        #[weak]
        inner,
        move |_| inner.confirm_delete_unit()
    ));
    log_load_btn.connect_clicked(clone!(
        #[weak]
        inner,
        move |_| inner.load_logs()
    ));
    file_load_btn.connect_clicked(clone!(
        #[weak]
        inner,
        move |_| inner.load_unit_file()
    ));
    t_reload.connect_clicked(clone!(
        #[weak]
        inner,
        move |_| inner.load_timers()
    ));

    // 系统日志
    j_load.connect_clicked(clone!(
        #[weak]
        inner,
        move |_| {
            // 切换实时跟踪时先停止，避免快照与实时混叠
            if inner.j_follow.is_active() {
                inner.j_follow.set_active(false);
                inner.stop_follow();
            }
            inner.load_journal();
        }
    ));
    j_follow.connect_toggled(clone!(
        #[weak]
        inner,
        move |_| inner.toggle_follow()
    ));
    j_clear.connect_clicked(clone!(
        #[weak]
        inner,
        move |_| {
            inner.j_buffer.set_text("");
            inner.j_status.set_text("已清除");
        }
    ));

    // 列表选择 → 详情（用 row-selected：单击和键盘上下键都会立刻加载详情；
    // 重建列表导致的空选择（None）忽略，否则搜索时详情会被清空）
    unit_list.connect_row_selected(clone!(
        #[weak]
        inner,
        move |_, row| {
            let Some(row) = row else {
                return;
            };
            let name = row.widget_name().to_string();
            if name.is_empty() || inner.is_selected(&name) {
                return;
            }
            inner.select_unit(&name);
        }
    ));

    INNER.with(|i| *i.borrow_mut() = Some(Rc::clone(&inner)));

    // 初次加载 service 列表 + 定时器
    inner.refresh();
    inner.load_timers();

    SystemdPage {
        root: toast_overlay,
    }
}
