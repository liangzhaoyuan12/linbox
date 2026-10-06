//! CPU 调频模式页面（GOAL Phase 8 · D8.1-D8.6）。
//!
//! 布局（自上而下，全部**动态取自本机 sysfs**，绝不写死清单，D8.1）：
//! - **状态卡**：驱动 / 当前调速器 / 当前能效偏好（EPP）/ 频率区间 / boost /
//!   持久化状态（D8.3「已持久化到开机（linbox-cpufreq.service）」或「未持久化，重启后失效」）；
//! - **调速器单选卡列表**：每张卡 = 原始 sysfs 名 + 中文短名 + 中文说明，
//!   当前项高亮；只列 [`CpufreqSnapshot::governors`] 并集，混合架构上部分核
//!   不支持的项标「部分核心不支持」（`missing_cpus_for`）；
//! - **EPP 分组**：同款卡片；本机无 EPP 文件时整组隐藏（D8.2）；
//! - **应用按钮**走 8.3 单次授权批量写入 + 写后回读比对；成功后重新枚举；
//! - **「持久化到开机」开关**（D8.3「跟随我的选择」）：开 → 先过 D8.6 冲突闸门
//!   （`tuned` / `auto-cpufreq` / `cpupower.service` 任一启用即阻止并提示，不
//!   静默覆盖、不代用户 disable），放行后写单元 + `daemon-reload` + `enable`；
//!   关 → `disable` + 只删自家单元；**开关开着时每次切换都会同步重建单元**；
//! - 无 cpufreq 接口的机器显示 `adw::StatusPage` 空态（不 panic）。
//!
//! 线程模型与 `page::fstab` 相同：子线程只搬 `Send` 纯数据，不捕获 `Rc<Inner>`；
//! `scan` / `exec_write` / `detect_conflicts` / `read_persist_state` 全在
//! `std::thread::spawn` 内（`pkexec` 会弹授权框，**绝不能在 UI 线程调**），
//! 结果经 `glib::source::idle_add` + 全局 `with_inner` 回主线程。

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use adw::prelude::*;

use crate::model::cpufreq::{
    CpufreqSnapshot, describe_epp, describe_governor, epp_label, governor_label,
};
use crate::utils::cpufreq::{self as cf, PersistState};

pub struct CpufreqPage {
    root: adw::ToastOverlay,
}

impl CpufreqPage {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }
}

thread_local! {
    static INNER: RefCell<Option<Rc<Inner>>> = const { RefCell::new(None) };
}

/// 应用退出前调用：清空全局句柄（避免窗口销毁期回调访问正在析构的 TLS）。
pub fn shutdown() {
    INNER.with(|i| {
        if let Ok(mut b) = i.try_borrow_mut() {
            *b = None;
        }
    });
}

fn with_inner<F: FnOnce(&Inner)>(f: F) {
    // try_borrow：销毁期可能重入，静默跳过而不是 panic。
    let Some(inner) = INNER.with(|i| i.try_borrow().ok().and_then(|b| b.clone())) else {
        return;
    };
    f(&inner);
}

// ---------------------------------------------------------------------------
// 纯展示小工具
// ---------------------------------------------------------------------------

/// 卡片容器（Phase 6 margin 收敛约定：页面根一层 12px，卡片只留内边距）。
fn card() -> (gtk::Box, gtk::Box) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.add_css_class("card");
    outer.set_margin_top(8);
    outer.set_margin_bottom(8);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(14);
    content.set_margin_bottom(14);
    content.set_margin_start(14);
    content.set_margin_end(14);
    outer.append(&content);
    (outer, content)
}

/// 「键 — 值」状态行，返回 `(整行, 值标签)`。
fn kv_row(key: &str) -> (gtk::Box, gtk::Label) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let k = gtk::Label::new(Some(key));
    k.add_css_class("dim-label");
    k.set_halign(gtk::Align::Start);
    row.append(&k);
    let v = gtk::Label::new(Some("…"));
    v.set_halign(gtk::Align::Start);
    v.set_hexpand(true);
    v.set_xalign(0.0);
    v.set_wrap(true);
    row.append(&v);
    (row, v)
}

/// kHz → 「800 – 4900 MHz」（缺字段给占位）。
fn freq_text(min: Option<u64>, max: Option<u64>) -> String {
    match (min, max) {
        (Some(a), Some(b)) => format!("{} – {} MHz", a / 1000, b / 1000),
        _ => "—".to_string(),
    }
}

/// 当前调速器汇总：全部 policy 一致 → 「powersave（20 个分区）」；
/// 不一致 → 「混合：powersave×18 / performance×2」。
fn governor_summary(snap: &CpufreqSnapshot) -> String {
    if snap.policies.is_empty() {
        return "—".to_string();
    }
    let mut tally: Vec<(String, usize)> = Vec::new();
    for p in &snap.policies {
        let g = p.governor.clone().unwrap_or_else(|| "未知".to_string());
        match tally.iter_mut().find(|(n, _)| *n == g) {
            Some((_, c)) => *c += 1,
            None => tally.push((g, 1)),
        }
    }
    if tally.len() == 1 {
        let (g, n) = &tally[0];
        format!("{g}（{n} 个分区一致）")
    } else {
        let parts: Vec<String> = tally.iter().map(|(g, n)| format!("{g}×{n}")).collect();
        format!("混合：{}", parts.join(" / "))
    }
}

/// boost 文案：`intel_pstate` 的 `no_turbo` 语义是**反的**（1=关，0=开），
/// 已由 `scan` 翻译成「是否支持 boost」，这里只展示。
fn boost_text(boost: Option<bool>) -> String {
    match boost {
        Some(true) => "开启（允许睿频）".to_string(),
        Some(false) => "关闭（已禁止睿频）".to_string(),
        None => "本机无该接口".to_string(),
    }
}

/// 持久化状态 → (文案, 样式类)（D8.3）。
fn persist_meta(st: &PersistState) -> (String, &'static str) {
    match st {
        PersistState::Enabled => (
            "已持久化到开机（linbox-cpufreq.service）".to_string(),
            "success",
        ),
        PersistState::Disabled => (
            "单元存在但未启用（linbox-cpufreq.service 未 enable，重启后失效）".to_string(),
            "warning",
        ),
        PersistState::Missing => ("未持久化，重启后失效".to_string(), "dim-label"),
        PersistState::Other(s) => (format!("状态：{s}"), "dim-label"),
    }
}

/// 一次后台加载的结果（全部 `Send` 纯数据）。
struct LoadData {
    snap: CpufreqSnapshot,
    persist: PersistState,
    conflicts: Vec<String>,
    raws: Vec<(String, String)>,
}

/// 一次「应用 + 回读」的结果。
struct ApplyOutcome {
    persist: PersistState,
    note: Option<String>,
}

// ---------------------------------------------------------------------------
// 页面主体
// ---------------------------------------------------------------------------

struct Inner {
    toast_overlay: adw::ToastOverlay,
    stack: gtk::Stack,

    // 状态卡
    v_driver: gtk::Label,
    v_governor: gtk::Label,
    v_epp: gtk::Label,
    v_freq: gtk::Label,
    v_boost: gtk::Label,
    v_persist: gtk::Label,

    // 选项区
    epp_outer: gtk::Box,
    gov_group: gtk::FlowBox,
    epp_group: gtk::FlowBox,

    // 操作区
    apply_btn: gtk::Button,
    persist_switch: gtk::Switch,
    conflict_lbl: gtk::Label,
    op_status: gtk::Label,
    spinner: gtk::Spinner,

    snap: RefCell<Option<CpufreqSnapshot>>,
    sel_gov: RefCell<String>,
    sel_epp: RefCell<String>,
    conflicts: RefCell<Vec<String>>,
    busy: Cell<bool>,
    /// 程序化改开关状态时置位，避免 `connect_active_notify` 递归触发。
    ignore_switch: Cell<bool>,
}

impl Inner {
    fn toast(&self, msg: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(msg));
    }

    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        if busy {
            self.spinner.set_visible(true);
            self.spinner.start();
        } else {
            self.spinner.stop();
            self.spinner.set_visible(false);
        }
        self.apply_btn.set_sensitive(!busy);
        self.persist_switch
            .set_sensitive(!busy && self.conflicts.borrow().is_empty());
    }

    fn enter_busy(&self) -> bool {
        if self.busy.get() {
            self.toast("上一个操作还在进行中，请稍候");
            return false;
        }
        self.set_busy(true);
        true
    }

    /// 状态卡：按快照重填六个值。
    fn render_status(&self, snap: &CpufreqSnapshot) {
        self.v_driver
            .set_text(snap.driver.as_deref().unwrap_or("未知"));
        self.v_governor.set_text(&governor_summary(snap));
        let epp_text = snap
            .epp
            .as_ref()
            .and_then(|e| e.current.as_deref())
            .map(|s| format!("{s}（{}）", epp_label(s)))
            .unwrap_or_else(|| "本机无 EPP 接口".to_string());
        self.v_epp.set_text(&epp_text);
        self.v_freq
            .set_text(&freq_text(snap.freq_min_khz, snap.freq_max_khz));
        self.v_boost.set_text(&boost_text(snap.boost));
    }

    /// 持久化状态 + 冲突闸门（D8.6）。
    fn render_persist(&self, st: &PersistState, conflicts: &[String], raws: &[(String, String)]) {
        let (text, class) = persist_meta(st);
        self.v_persist.set_text(&text);
        self.v_persist.remove_css_class("success");
        self.v_persist.remove_css_class("warning");
        self.v_persist.remove_css_class("dim-label");
        self.v_persist.add_css_class(class);

        self.conflicts.borrow_mut().clear();
        self.conflicts.borrow_mut().extend_from_slice(conflicts);

        self.conflict_lbl.set_visible(!conflicts.is_empty());
        // 程序化改开关前挂保护，避免 connect_active_notify 递归触发写操作
        self.ignore_switch.set(true);
        if conflicts.is_empty() {
            self.conflict_lbl.set_text("");
            self.persist_switch
                .set_active(matches!(st, PersistState::Enabled));
        } else {
            let raw: Vec<String> = raws.iter().map(|(n, r)| format!("{n}={r}")).collect();
            self.conflict_lbl.set_text(&format!(
                "已阻止持久化：冲突服务启用中（{}）——本机调频由它们说了算，要接管请先自行停用（本页不代为 disable）",
                conflicts.join("、")
            ));
            self.conflict_lbl.set_tooltip_text(Some(&format!(
                "systemctl is-enabled 探测：{}",
                raw.join("；")
            )));
            self.persist_switch.set_active(false);
        }
        self.ignore_switch.set(false);
        // 开关状态与真实状态对齐（D8.3「跟随我的选择」的起点）
        self.persist_switch
            .set_sensitive(!self.busy.get() && conflicts.is_empty());
    }

    /// 重建调速器卡组（GOAL 8.4：单选卡列表 + 当前项高亮）。
    fn render_options(&self, snap: &CpufreqSnapshot) {
        while let Some(c) = self.gov_group.first_child() {
            self.gov_group.remove(&c);
        }
        while let Some(c) = self.epp_group.first_child() {
            self.epp_group.remove(&c);
        }
        self.epp_outer.set_visible(snap.has_epp());

        // ---- 调速器 ----
        let mut gov_leader: Option<gtk::ToggleButton> = None;
        for g in &snap.governors {
            let missing = snap.missing_cpus_for(g);
            let mut desc = format!("{}：{}", governor_label(g), describe_governor(g));
            if !missing.is_empty() {
                desc.push_str(&format!("（{} 个核心不支持，混合架构）", missing.len()));
            }
            let btn = mode_card(g, &desc);
            match gov_leader.as_ref() {
                None => gov_leader = Some(btn.clone()),
                Some(l) => btn.set_group(Some(l)),
            }
            if snap
                .policies
                .iter()
                .all(|p| p.governor.as_deref() == Some(g))
            {
                btn.set_active(true);
                self.sel_gov.replace(g.clone());
            }
            let name = g.clone();
            btn.connect_toggled(move |b| {
                b.remove_css_class("suggested-action");
                if b.is_active() {
                    b.add_css_class("suggested-action");
                    with_inner(|i| {
                        i.sel_gov.replace(name.clone());
                        i.apply_btn.set_sensitive(!i.busy.get());
                    });
                }
            });
            if btn.is_active() {
                btn.add_css_class("suggested-action");
            }
            self.gov_group.append(&btn);
        }
        if self.sel_gov.borrow().is_empty()
            && let Some(first) = snap.governors.first()
        {
            self.sel_gov.replace(first.clone());
        }

        // ---- EPP（D8.2：无接口整组隐藏）----
        if snap.has_epp() {
            let current = snap
                .epp
                .as_ref()
                .and_then(|e| e.current.clone())
                .unwrap_or_default();
            let mut epp_leader: Option<gtk::ToggleButton> = None;
            for e in snap
                .epp
                .as_ref()
                .map(|s| &s.available)
                .cloned()
                .unwrap_or_default()
            {
                let btn = mode_card(&e, &format!("{}：{}", epp_label(&e), describe_epp(&e)));
                match epp_leader.as_ref() {
                    None => epp_leader = Some(btn.clone()),
                    Some(l) => btn.set_group(Some(l)),
                }
                if e == current {
                    btn.set_active(true);
                    self.sel_epp.replace(e.clone());
                }
                let name = e.clone();
                btn.connect_toggled(move |b| {
                    b.remove_css_class("suggested-action");
                    if b.is_active() {
                        b.add_css_class("suggested-action");
                        with_inner(|i| {
                            i.sel_epp.replace(name.clone());
                        });
                    }
                });
                if btn.is_active() {
                    btn.add_css_class("suggested-action");
                }
                self.epp_group.append(&btn);
            }
            if self.sel_epp.borrow().is_empty()
                && let Some(first) = snap.epp.as_ref().and_then(|s| s.available.first())
            {
                self.sel_epp.replace(first.clone());
            }
        }
    }

    /// 后台重新枚举 sysfs + 持久化状态 + D8.6 冲突探测（全是只读）。
    fn refresh(&self) {
        if !self.enter_busy() {
            return;
        }
        self.op_status.set_text("正在读取本机 cpufreq…");
        std::thread::spawn(|| {
            let snap = cf::scan();
            let persist = cf::read_persist_state();
            let (conflicts, raws) = cf::detect_conflicts();
            let data = LoadData {
                snap,
                persist,
                conflicts,
                raws,
            };
            let slot = RefCell::new(Some(data));
            glib::source::idle_add(move || {
                if let Some(d) = slot.take() {
                    with_inner(|i| i.apply_load(d));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_load(&self, data: LoadData) {
        self.busy.set(false);
        if !data.snap.supported {
            self.stack.set_visible_child_name("empty");
            return;
        }
        self.stack.set_visible_child_name("main");
        self.render_status(&data.snap);
        self.render_options(&data.snap);
        self.render_persist(&data.persist, &data.conflicts, &data.raws);
        self.snap.borrow_mut().replace(data.snap.clone());
        let epp_hint = if data.snap.has_epp() {
            " · 含 EPP 分组"
        } else {
            ""
        };
        self.op_status.set_text(&format!(
            "已读取 {} 个 cpufreq policy（{} 个核）{epp_hint}",
            data.snap.policies.len(),
            data.snap.cpu_count
        ));
        self.set_busy(false);
    }

    /// 应用当前选择：写入 → 回读比对 →（开关开着时）同步重建单元。
    fn apply(&self) {
        if !self.enter_busy() {
            return;
        }
        let Some(snap) = self.snap.borrow().clone() else {
            self.set_busy(false);
            return;
        };
        let gov = self.sel_gov.borrow().clone();
        if gov.is_empty() {
            self.set_busy(false);
            self.toast("没有可应用的调速器");
            return;
        }
        let epp = if snap.has_epp() {
            let e = self.sel_epp.borrow().clone();
            if e.is_empty() { None } else { Some(e) }
        } else {
            None
        };
        let persist_on = self.persist_switch.is_active();
        self.op_status.set_text("正在写入（可能弹出授权框）…");

        std::thread::spawn(move || {
            let res: Result<ApplyOutcome, String> = (|| {
                let root = Path::new(cf::SYS_CPU_ROOT);
                let w = cf::set_governor_all(root, &snap, &gov)?;
                cf::exec_write(&w)?;
                if let Some(e) = &epp {
                    let w2 = cf::set_epp_all(root, &snap, e)?;
                    cf::exec_write(&w2)?;
                }
                // 写后回读比对：不一致绝不宣称成功（GOAL 8.3）
                let mut bad: Vec<usize> = Vec::new();
                for p in &snap.policies {
                    let Some(cpu) = p.cpus.first() else { continue };
                    let got = cf::read_governor(root, *cpu);
                    if got.as_deref() != Some(gov.as_str()) {
                        bad.push(*cpu);
                    }
                }
                if !bad.is_empty() {
                    return Err(format!(
                        "写入后回读不一致：核 {:?} 仍不是 {gov}（可能是授权被拒或内核拒绝）",
                        &bad[..bad.len().min(6)]
                    ));
                }
                // D8.3「跟随我的选择」+ D8.6 闸门
                let mut note = None;
                if persist_on {
                    let (conflicts, _) = cf::detect_conflicts();
                    if !conflicts.is_empty() {
                        note = Some(format!(
                            "冲突服务 {} 启用中，未重建持久化单元",
                            conflicts.join("、")
                        ));
                    } else {
                        let unit =
                            cf::render_unit(root, &gov, epp.as_deref(), &cf::policy_cpus(&snap))?;
                        let cmd = cf::persist_enable_cmd(&unit)?;
                        cf::exec_write(&cmd)?;
                        note = Some("已同步重建持久化单元".to_string());
                    }
                }
                let persist = cf::read_persist_state();
                Ok(ApplyOutcome { persist, note })
            })();

            let slot = RefCell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(r) = slot.take() {
                    with_inner(|i| match r {
                        Ok(o) => {
                            let extra = o.note.clone().unwrap_or_default();
                            i.render_persist(&o.persist, &i.conflicts.borrow(), &[]);
                            i.toast(&format!(
                                "已切换到 {}{}",
                                i.sel_gov.borrow(),
                                if extra.is_empty() {
                                    String::new()
                                } else {
                                    format!("（{extra}）")
                                }
                            ));
                            i.set_busy(false);
                            i.refresh();
                        }
                        Err(e) => {
                            i.set_busy(false);
                            i.op_status.set_text("上次操作失败");
                            i.toast(&format!("操作失败：{e}"));
                        }
                    });
                }
                glib::ControlFlow::Break
            });
        });
    }

    /// 「持久化到开机」开关（D8.3 + D8.6 闸门）。
    fn toggled_persist(&self, on: bool) {
        if self.ignore_switch.get() {
            return;
        }
        if self.busy.get() {
            self.ignore_switch.set(true);
            self.persist_switch.set_active(false);
            self.ignore_switch.set(false);
            self.toast("上一个操作还在进行中，请稍候");
            return;
        }
        if on && !self.conflicts.borrow().is_empty() {
            self.ignore_switch.set(true);
            self.persist_switch.set_active(false);
            self.ignore_switch.set(false);
            self.toast("已阻止：本机存在启用中的冲突服务（见下方红字）");
            return;
        }
        self.set_busy(true);
        self.op_status.set_text(if on {
            "正在写入开机持久化（可能弹出授权框）…"
        } else {
            "正在撤销持久化…"
        });

        let snap = self.snap.borrow().clone();
        let gov = self.sel_gov.borrow().clone();
        let epp = if snap.as_ref().map(|s| s.has_epp()).unwrap_or(false) {
            let e = self.sel_epp.borrow().clone();
            if e.is_empty() { None } else { Some(e) }
        } else {
            None
        };

        std::thread::spawn(move || {
            let res: Result<(PersistState, String), String> = (|| {
                if on {
                    // 闸门再查一遍（服务状态可能在页面打开后变化）
                    let (conflicts, _) = cf::detect_conflicts();
                    if !conflicts.is_empty() {
                        return Err(format!(
                            "冲突服务启用中（{}），已阻止持久化；要接管请先自行停用它们",
                            conflicts.join("、")
                        ));
                    }
                    let snap = snap.ok_or("尚未读取本机 cpufreq 状态")?;
                    let root = Path::new(cf::SYS_CPU_ROOT);
                    let unit =
                        cf::render_unit(root, &gov, epp.as_deref(), &cf::policy_cpus(&snap))?;
                    let cmd = cf::persist_enable_cmd(&unit)?;
                    cf::exec_write(&cmd)?;
                    Ok((cf::read_persist_state(), "已持久化到开机".to_string()))
                } else {
                    let cmd = cf::persist_disable_cmd();
                    cf::exec_write(&cmd)?;
                    Ok((
                        cf::read_persist_state(),
                        "已撤销持久化，重启后回到系统原设置".to_string(),
                    ))
                }
            })();

            let slot = RefCell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(r) = slot.take() {
                    with_inner(|i| match r {
                        Ok((st, msg)) => {
                            let (text, _) = persist_meta(&st);
                            i.v_persist.set_text(&text);
                            i.render_persist(&st, &i.conflicts.borrow(), &[]);
                            i.toast(&msg);
                            i.op_status.set_text("就绪");
                            i.set_busy(false);
                        }
                        Err(e) => {
                            i.set_busy(false);
                            i.ignore_switch.set(true);
                            i.persist_switch.set_active(false);
                            i.ignore_switch.set(false);
                            i.op_status.set_text("持久化操作失败");
                            i.toast(&format!("失败：{e}"));
                        }
                    });
                }
                glib::ControlFlow::Break
            });
        });
    }
}

/// 单选卡：上行 = sysfs 原始名，下行 = 中文说明。
fn mode_card(name: &str, desc: &str) -> gtk::ToggleButton {
    let b = gtk::ToggleButton::new();
    let bx = gtk::Box::new(gtk::Orientation::Vertical, 4);
    bx.set_margin_top(10);
    bx.set_margin_bottom(10);
    bx.set_margin_start(12);
    bx.set_margin_end(12);

    let title = gtk::Label::new(Some(name));
    title.add_css_class("heading");
    title.set_halign(gtk::Align::Start);
    bx.append(&title);

    let d = gtk::Label::new(Some(desc));
    d.add_css_class("dim-label");
    d.add_css_class("caption");
    d.set_halign(gtk::Align::Start);
    d.set_wrap(true);
    d.set_xalign(0.0);
    bx.append(&d);

    b.set_child(Some(&bx));
    b.add_css_class("card");
    b
}

// ---------------------------------------------------------------------------
// 组装
// ---------------------------------------------------------------------------

pub fn build() -> CpufreqPage {
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_vexpand(true);

    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    toast_overlay.set_child(Some(&stack));

    // ---------- 空态：无 cpufreq 接口 ----------
    let empty = adw::StatusPage::new();
    empty.set_title("本机内核未提供 cpufreq 接口");
    empty.set_icon_name(Some("power-profile-balanced-symbolic"));
    empty.set_description(Some(
        "这台机器没有任何 cpuN/cpufreq 目录（部分虚拟机、容器或未编译 cpufreq 的内核如此）。\
         本页不显示任何模式，也不提供切换按钮。",
    ));
    stack.add_named(&empty, Some("empty"));

    // ---------- 主体 ----------
    // 关键修复：GTK4 的 Stack 取**所有**子页自然高度的最大值，本页卡片多、
    // 不裹滚动窗就会把整个应用窗口顶得比屏幕还高（用户实测反馈）。
    // propagate_natural_height(false) = 本页不向窗口贡献高度，内容自己滚。
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_propagate_natural_height(false);
    scroller.set_propagate_natural_width(true);
    scroller.set_vexpand(true);
    stack.add_named(&scroller, Some("main"));

    let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root_box.set_margin_top(12);
    root_box.set_margin_bottom(12);
    root_box.set_margin_start(12);
    root_box.set_margin_end(12);
    scroller.set_child(Some(&root_box));

    let title = gtk::Label::new(Some("CPU 调频"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    root_box.append(&title);

    let subtitle = gtk::Label::new(Some(
        "调速器与能效偏好全部动态读自本机 sysfs（D8.1，不同机器可选项不同）；\
         写入走一次 polkit 授权的批量命令并逐分区回读比对；可选持久化到开机（D8.3）。",
    ));
    subtitle.add_css_class("dim-label");
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_wrap(true);
    subtitle.set_margin_top(2);
    root_box.append(&subtitle);

    // ---- 状态卡 ----
    let (status_card, status_content) = card();
    let (row, v_driver) = kv_row("驱动");
    status_content.append(&row);
    let (row, v_governor) = kv_row("当前调速器");
    status_content.append(&row);
    let (row, v_epp) = kv_row("能效偏好");
    status_content.append(&row);
    let (row, v_freq) = kv_row("频率区间");
    status_content.append(&row);
    let (row, v_boost) = kv_row("睿频 boost");
    status_content.append(&row);
    let (row, v_persist) = kv_row("持久化");
    status_content.append(&row);
    root_box.append(&status_card);

    // ---- 调速器卡组 ----
    let (gov_card, gov_content) = card();
    let gov_head = gtk::Label::new(Some("调速器（governor）"));
    gov_head.add_css_class("heading");
    gov_head.set_halign(gtk::Align::Start);
    gov_content.append(&gov_head);
    let gov_hint = gtk::Label::new(Some(
        "只列出本机 scaling_available_governors 的并集，点选后按「应用」生效",
    ));
    gov_hint.add_css_class("dim-label");
    gov_hint.add_css_class("caption");
    gov_hint.set_halign(gtk::Align::Start);
    gov_content.append(&gov_hint);
    let gov_group = gtk::FlowBox::new();
    gov_group.set_max_children_per_line(4);
    gov_group.set_selection_mode(gtk::SelectionMode::None);
    gov_content.append(&gov_group);
    root_box.append(&gov_card);

    // ---- EPP 卡组（无接口时整组隐藏）----
    let epp_outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let (epp_card, epp_content) = card();
    let epp_head = gtk::Label::new(Some("能效偏好（EPP）"));
    epp_head.add_css_class("heading");
    epp_head.set_halign(gtk::Align::Start);
    epp_content.append(&epp_head);
    let epp_hint = gtk::Label::new(Some(
        "intel_pstate 才有该轴；省电更积极 ↔ 性能更积极，与调速器互不影响（D8.2）",
    ));
    epp_hint.add_css_class("dim-label");
    epp_hint.add_css_class("caption");
    epp_hint.set_halign(gtk::Align::Start);
    epp_content.append(&epp_hint);
    let epp_group = gtk::FlowBox::new();
    epp_group.set_max_children_per_line(4);
    epp_group.set_selection_mode(gtk::SelectionMode::None);
    epp_content.append(&epp_group);
    epp_outer.append(&epp_card);
    root_box.append(&epp_outer);

    // ---- 操作条 ----
    let (act_card, act_content) = card();
    let act = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let apply_btn = gtk::Button::with_label("应用");
    apply_btn.add_css_class("suggested-action");
    apply_btn.set_tooltip_text(Some(
        "批量写入全部频率分区（一次授权）→ 逐分区回读比对；开关开着时同步重建持久化单元",
    ));
    act.append(&apply_btn);

    let refresh_btn = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh_btn.set_tooltip_text(Some("重新读取 sysfs / 持久化状态 / 冲突服务"));
    act.append(&refresh_btn);

    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);
    act.append(&spinner);

    let op_status = gtk::Label::new(Some("正在读取…"));
    op_status.add_css_class("dim-label");
    op_status.add_css_class("caption");
    op_status.set_halign(gtk::Align::Start);
    op_status.set_hexpand(true);
    act.append(&op_status);
    act_content.append(&act);

    // 持久化开关行
    let pers_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let persist_switch = gtk::Switch::new();
    persist_switch.set_valign(gtk::Align::Center);
    pers_row.append(&persist_switch);
    let pers_text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let pers_title = gtk::Label::new(Some("持久化到开机（跟随我的选择）"));
    pers_title.set_halign(gtk::Align::Start);
    pers_text.append(&pers_title);
    let pers_desc = gtk::Label::new(Some(
        "开：每次切换都同步重建 linbox-cpufreq.service，重启后恢复本页选择；\
         关：disable 并删除该单元（只删自家的），重启回系统原设置。\
         开启前会先探测 tuned / auto-cpufreq / cpupower.service——任一启用即阻止（D8.6）。",
    ));
    pers_desc.add_css_class("dim-label");
    pers_desc.add_css_class("caption");
    pers_desc.set_halign(gtk::Align::Start);
    pers_desc.set_wrap(true);
    pers_desc.set_xalign(0.0);
    pers_text.append(&pers_desc);
    pers_row.append(&pers_text);
    act_content.append(&pers_row);

    let conflict_lbl = gtk::Label::new(Some(""));
    conflict_lbl.add_css_class("error");
    conflict_lbl.set_halign(gtk::Align::Start);
    conflict_lbl.set_wrap(true);
    conflict_lbl.set_xalign(0.0);
    conflict_lbl.set_visible(false);
    act_content.append(&conflict_lbl);

    root_box.append(&act_card);

    // ---------- 全局状态 ----------
    let inner = Rc::new(Inner {
        toast_overlay,
        stack,
        v_driver,
        v_governor,
        v_epp,
        v_freq,
        v_boost,
        v_persist,
        epp_outer,
        gov_group,
        epp_group,
        apply_btn: apply_btn.clone(),
        persist_switch: persist_switch.clone(),
        conflict_lbl,
        op_status,
        spinner,
        snap: RefCell::new(None),
        sel_gov: RefCell::new(String::new()),
        sel_epp: RefCell::new(String::new()),
        conflicts: RefCell::new(Vec::new()),
        busy: Cell::new(false),
        ignore_switch: Cell::new(false),
    });
    INNER.with(|i| *i.borrow_mut() = Some(inner.clone()));

    apply_btn.connect_clicked(move |_| with_inner(|i| i.apply()));
    refresh_btn.connect_clicked(move |_| with_inner(|i| i.refresh()));
    persist_switch.connect_active_notify(move |sw| {
        with_inner(|i| i.toggled_persist(sw.is_active()));
    });

    inner.refresh();
    CpufreqPage {
        root: inner.toast_overlay.clone(),
    }
}
