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
use crate::utils::systemd as sd;

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
    f(&*inner);
}

pub fn shutdown() {
    INNER.with(|i| {
        if let Ok(mut b) = i.try_borrow_mut() {
            if let Some(inner) = b.take() {
                // 杀掉可能仍在运行的 journalctl -f 子进程，避免孤儿进程
                if let Some(mut c) = inner.j_follow_child.take() {
                    let _ = c.kill();
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

const TYPE_OPTIONS: &[&str] = &[
    "全部", "service", "timer", "socket", "mount", "swap", "target", "path", "device",
    "automount", "scope", "slice",
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

    detail_empty: gtk::Label,
    detail_content: gtk::Box,
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
        for (_, b) in &self.action_buttons {
            b.set_sensitive(!busy);
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
                let next_soon = timers
                    .iter()
                    .filter(|t| !t.next_in.is_empty())
                    .count();
                self.t_status
                    .set_text(&format!("共 {} 个定时器，其中 {} 个已排定", timers.len(), next_soon));
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
            }
            Err(e) => {
                self.status_buffer.set_text(&e);
                self.toast(&e);
            }
        }
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
        if over > 0 {
            if let Some(mut cut) = self.j_buffer.iter_at_line(over) {
                let mut start = self.j_buffer.start_iter();
                self.j_buffer.delete(&mut start, &mut cut);
            }
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

    // 空态
    let detail_empty = gtk::Label::new(Some("从左侧选择一个单元以查看详情、执行操作或查看日志。"));
    detail_empty.add_css_class("dim-label");
    detail_empty.set_wrap(true);
    detail_empty.set_margin_top(40);
    detail_empty.set_margin_bottom(40);
    detail_empty.set_margin_start(20);
    detail_empty.set_margin_end(20);
    detail_box.append(&detail_empty);

    // 详情内容
    let detail_content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    detail_content.set_visible(false);
    detail_box.append(&detail_content);

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
        detail_empty: detail_empty.clone(),
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

        units: RefCell::new(Vec::new()),
        selected: RefCell::new(None),
        busy: Cell::new(false),
        loading: Cell::new(false),
    });

    // ---------- 信号 ----------
    // 过滤变化
    inner
        .type_row
        .connect_selected_notify(clone!(#[weak] inner, move |_| inner.refresh()));
    inner
        .scope_row
        .connect_selected_notify(clone!(#[weak] inner, move |_| inner.on_scope_changed()));
    inner
        .state_row
        .connect_selected_notify(clone!(#[weak] inner, move |_| inner.rebuild_list()));
    search.connect_search_changed(clone!(#[weak] inner, move |_| inner.rebuild_list()));
    refresh_btn.connect_clicked(clone!(#[weak] inner, move |_| inner.refresh()));
    reload_btn.connect_clicked(clone!(#[weak] inner, move |_| inner.do_daemon_reload()));
    log_load_btn.connect_clicked(clone!(#[weak] inner, move |_| inner.load_logs()));
    file_load_btn.connect_clicked(clone!(#[weak] inner, move |_| inner.load_unit_file()));
    t_reload.connect_clicked(clone!(#[weak] inner, move |_| inner.load_timers()));

    // 系统日志
    j_load.connect_clicked(clone!(#[weak] inner, move |_| {
        // 切换实时跟踪时先停止，避免快照与实时混叠
        if inner.j_follow.is_active() {
            inner.j_follow.set_active(false);
            inner.stop_follow();
        }
        inner.load_journal();
    }));
    j_follow.connect_toggled(clone!(#[weak] inner, move |_| inner.toggle_follow()));
    j_clear.connect_clicked(clone!(#[weak] inner, move |_| {
        inner.j_buffer.set_text("");
        inner.j_status.set_text("已清除");
    }));

    // 列表选择 → 详情（用 row-selected：单击和键盘上下键都会立刻加载详情；
    // 重建列表导致的空选择（None）忽略，否则搜索时详情会被清空）
    unit_list.connect_row_selected(clone!(#[weak] inner, move |_, row| {
        let Some(row) = row else {
            return;
        };
        let name = row.widget_name().to_string();
        if name.is_empty() || inner.is_selected(&name) {
            return;
        }
        inner.select_unit(&name);
    }));

    INNER.with(|i| *i.borrow_mut() = Some(Rc::clone(&inner)));

    // 初次加载 service 列表 + 定时器
    inner.refresh();
    inner.load_timers();

    SystemdPage { root: toast_overlay }
}
