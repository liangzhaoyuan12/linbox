//! 硬盘健康页面（GOAL Phase 10 · D10.1-D10.7）。
//!
//! 布局（D10.7，自上而下）：
//! - **工具条**：状态文案 + spinner + 「刷新」；
//! - **每盘一卡**：卡头（型号 + 设备路径 + 健康徽章）→ 健康总览
//!   （序列号 / 固件 / 容量 / 接口 / 通电时间+档位 / 通电次数 / 温度+档位 /
//!   写入量 / 未收录属性数）→ **预警区**（D10.3 ② 命中项，红=严重、橙=链路）
//!   → **自检区**（D10.5：短/长自检按钮 + 说明确认 + 自检记录）→ **属性表**
//!   （ID / 中文名 / 当前值 / 阈值 / 最差 / 原始值 / 判定，排序 =
//!   FAIL > 预警 > 关键 > 普通，D10.7）；
//! - **读取失败的盘**：卡片只显示 `SmartFailure` 的中文标题 + 处理建议
//!   （未安装 / 无权限 / 超时 / 非 JSON 四类，D10.1）；
//! - 无磁盘 → `adw::StatusPage` 空态；lsblk 失败 → 整页错误态 + 重试。
//!
//! 线程模型与 `page::cpufreq` 相同：**全部外部命令（lsblk / smartctl /
//! pkexec）都在 `std::thread::spawn` 内**（GOAL 3.2），结果经
//! `glib::source::idle_add` + 全局 `with_inner` 回主线程；读取带超时
//! （D10.1 30s）。自检启动是**写设备动作**，必须过 `adw::MessageDialog`
//! 二次确认（D10.5），启动后按 30s 间隔轮询自检日志直至出结果。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;

use crate::model::smart::{self as ms, HealthReport, Overall, SmartAttr};
use crate::utils::cleaner::format_bytes;
use crate::utils::smart as su;
use crate::utils::smart::{DiskDevice, FailKind, SelfTestKind, SmartFailure, TestState};

pub struct SmartPage {
    root: adw::ToastOverlay,
}

impl SmartPage {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }
}

thread_local! {
    static INNER: RefCell<Option<Rc<Inner>>> = const { RefCell::new(None) };
}

/// 应用退出前调用：清空全局句柄 + 作废在跑的自检轮询。
pub fn shutdown() {
    INNER.with(|i| {
        if let Ok(mut b) = i.try_borrow_mut() {
            *b = None;
        }
    });
}

fn with_inner<F: FnOnce(&Inner)>(f: F) {
    let Some(inner) = INNER.with(|i| i.try_borrow().ok().and_then(|b| b.clone())) else {
        return;
    };
    f(&inner);
}

/// 一块盘的一次读取结果（重绘时整体克隆缓存）。
#[derive(Clone)]
struct LoadItem {
    dev: DiskDevice,
    res: Result<HealthReport, SmartFailure>,
}

/// 读一块盘：`elevated=false` 只直读（零弹窗）；`true` 走 pkexec 兜底（D10.1）。
fn read_path(path: &str, elevated: bool) -> Result<HealthReport, SmartFailure> {
    let res = if elevated {
        su::run_smart(path, su::SMART_TIMEOUT)
    } else {
        su::run_smart_direct(path, su::SMART_TIMEOUT)
    };
    match res {
        Ok(json) => ms::analyze(&json).map_err(|e| SmartFailure::new(FailKind::BadJson, e)),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// 展示小工具
// ---------------------------------------------------------------------------

/// 卡片容器（页面根一层 12px，卡片只留内边距）。
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

/// 「键 — 值」行，返回 `(整行, 值标签)`。
fn kv_row(key: &str) -> (gtk::Box, gtk::Label) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let k = gtk::Label::new(Some(key));
    k.add_css_class("dim-label");
    k.set_halign(gtk::Align::Start);
    row.append(&k);
    let v = gtk::Label::new(Some("—"));
    v.set_halign(gtk::Align::End);
    v.set_hexpand(true);
    v.set_xalign(1.0);
    v.set_wrap(true);
    row.append(&v);
    (row, v)
}

/// 小徽章（健康结论 / 属性判定）。
fn badge(text: &str, css: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("heading");
    if !css.is_empty() {
        l.add_css_class(css);
    }
    l.set_halign(gtk::Align::End);
    l
}

/// 健康结论 → (文案, css 类)。
fn overall_meta(o: Overall) -> (&'static str, &'static str) {
    match o {
        Overall::Passed => ("健康", "success"),
        Overall::Failed => ("不健康", "error"),
        Overall::Unknown => ("未知", "warning"),
    }
}

/// 分区分隔标题。
fn section_title(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("heading");
    l.set_halign(gtk::Align::Start);
    l
}

/// 容量/写入量展示。
fn bytes_text(b: Option<u64>) -> String {
    match b {
        Some(b) => format_bytes(b),
        None => "—".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Inner
// ---------------------------------------------------------------------------

struct Inner {
    toast_overlay: adw::ToastOverlay,
    content: gtk::Box,
    status: gtk::Label,
    spinner: gtk::Spinner,
    refresh_btn: gtk::Button,
    busy: Cell<bool>,
    /// 最近一次枚举的整页错误（lsblk 失败）。
    global_err: RefCell<Option<String>>,
    /// 最近一次枚举的结果缓存（单盘重读后就地更新并重绘）。
    items: RefCell<Vec<LoadItem>>,
    /// 设备路径 → 自检状态标签（自检轮询回填用）。
    st_labels: RefCell<HashMap<String, gtk::Label>>,
    /// 自检轮询代数：刷新时自增，作废在跑的轮询。
    poll_gen: Cell<u64>,
}

impl Inner {
    fn toast(&self, msg: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(msg));
    }

    fn window(&self) -> Option<gtk::Window> {
        self.toast_overlay.root()?.downcast().ok()
    }

    fn enter_busy(&self) -> bool {
        if self.busy.get() {
            self.toast("上一个操作还在进行中，请稍候");
            return false;
        }
        self.set_busy(true);
        true
    }

    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        if busy {
            self.spinner.start();
        } else {
            self.spinner.stop();
        }
        self.refresh_btn.set_sensitive(!busy);
    }

    // ----- 读取 ---------------------------------------------------------

    /// 后台枚举磁盘并逐盘**直读** SMART（外部命令全在线程内，GOAL 3.2）。
    ///
    /// 只直读不提权（`run_smart_direct`）：直读失败零弹窗，权限不足的盘在卡片上
    /// 给「授权读取」按钮由用户显式触发——否则应用一启动就弹 polkit 框
    /// （本机 polkit 不缓存授权，GOAL 8.3 在案事实；D10.1 的提权兜底仍在
    /// [`Inner::retry_one`] / `run_smart` 里，只是触发权交给用户）。
    fn refresh(&self) {
        if !self.enter_busy() {
            return;
        }
        // 作废旧自检轮询
        self.poll_gen.set(self.poll_gen.get().wrapping_add(1));
        self.st_labels.borrow_mut().clear();
        self.status.set_text("正在枚举磁盘…");

        std::thread::spawn(|| {
            let disks = su::list_disks();
            let (err, items) = match disks {
                Err(e) => (Some(e), Vec::new()),
                Ok(list) => (
                    None,
                    list.iter()
                        .map(|dev| LoadItem {
                            dev: dev.clone(),
                            res: read_path(&dev.path, false),
                        })
                        .collect::<Vec<_>>(),
                ),
            };
            let slot = RefCell::new(Some((err, items)));
            glib::source::idle_add(move || {
                if let Some(t) = slot.take() {
                    with_inner(|i| i.apply_load(t.0, t.1));
                }
                glib::ControlFlow::Break
            });
        });
    }

    /// 回主线程：存结果 + 重建结果区。
    fn apply_load(&self, global_err: Option<String>, items: Vec<LoadItem>) {
        self.busy.set(false);
        self.refresh_btn.set_sensitive(true);
        self.spinner.stop();
        self.st_labels.borrow_mut().clear();
        *self.global_err.borrow_mut() = global_err;
        *self.items.borrow_mut() = items;
        self.render();
    }

    /// 单盘重读：`elevated = true` 走 pkexec 提权（**会弹授权框**，用户显式点击才触发）。
    fn retry_one(&self, path: String, elevated: bool) {
        if !self.enter_busy() {
            return;
        }
        self.status.set_text(if elevated {
            "等待授权并读取…"
        } else {
            "正在读取…"
        });
        std::thread::spawn(move || {
            let res = read_path(&path, elevated);
            let slot = RefCell::new(Some((path, res)));
            glib::source::idle_add(move || {
                if let Some((p, res)) = slot.take() {
                    with_inner(|i| i.apply_one(p, res));
                }
                glib::ControlFlow::Break
            });
        });
    }

    /// 单盘结果回填：就地替换后整体重绘。
    fn apply_one(&self, path: String, res: Result<HealthReport, SmartFailure>) {
        self.busy.set(false);
        self.refresh_btn.set_sensitive(true);
        self.spinner.stop();
        for it in self.items.borrow_mut().iter_mut() {
            if it.dev.path == path {
                it.res = res;
                break;
            }
        }
        self.render();
    }

    /// 按当前缓存重建整个结果区。
    fn render(&self) {
        while let Some(c) = self.content.first_child() {
            self.content.remove(&c);
        }

        if let Some(err) = self.global_err.borrow().clone() {
            self.status.set_text("读取磁盘列表失败");
            let sp = adw::StatusPage::new();
            sp.set_title("读取磁盘列表失败");
            sp.set_icon_name(Some("dialog-error-symbolic"));
            sp.set_description(Some(&err));
            let retry = gtk::Button::with_label("重试");
            retry.add_css_class("suggested-action");
            retry.set_halign(gtk::Align::Center);
            retry.connect_clicked(move |_| with_inner(|i| i.refresh()));
            sp.set_child(Some(&retry));
            self.content.append(&sp);
            return;
        }

        let items = self.items.borrow().clone();
        if items.is_empty() {
            self.status.set_text("未发现磁盘");
            let sp = adw::StatusPage::new();
            sp.set_title("没有发现物理磁盘");
            sp.set_icon_name(Some("drive-harddisk-solidstate-symbolic"));
            sp.set_description(Some(
                "lsblk 没有返回任何 type=disk 的设备（虚拟机/容器里常见）。\
                 本页不做任何猜测，接入磁盘后点「刷新」。",
            ));
            self.content.append(&sp);
            return;
        }

        let ok = items.iter().filter(|i| i.res.is_ok()).count();
        self.status
            .set_text(&format!("共 {} 块盘 · 读取成功 {} 块", items.len(), ok));

        for item in items {
            self.content.append(&build_card(self, item));
        }
    }

    // ----- 自检（D10.5）-------------------------------------------------

    /// 轮询一次自检日志（后台线程）。
    fn spawn_selftest_read(&self, dev: String, ticks_left: u32) {
        std::thread::spawn(move || {
            let res = match su::run_selftest_log(&dev, su::SMART_TIMEOUT) {
                Ok(json) => su::parse_selftest_log(&json)
                    .map_err(|e| SmartFailure::new(FailKind::BadJson, e)),
                Err(e) => Err(e),
            };
            let slot = RefCell::new(Some((dev, ticks_left, res)));
            glib::source::idle_add(move || {
                if let Some(t) = slot.take() {
                    with_inner(|i| i.apply_selftest(t));
                }
                glib::ControlFlow::Break
            });
        });
    }

    /// 自检日志回填：状态标签 + 仍在执行则 30s 后再来一次。
    fn apply_selftest(
        &self,
        (dev, ticks_left, res): (String, u32, Result<Vec<su::SelfTestEntry>, SmartFailure>),
    ) {
        let lbl = self.st_labels.borrow().get(&dev).cloned();
        let Some(lbl) = lbl else { return };
        match res {
            Err(e) => {
                lbl.set_text(&format!("{}：{}", e.kind_label(), e.message));
                lbl.remove_css_class("success");
                lbl.remove_css_class("error");
                lbl.add_css_class("warning");
            }
            Ok(entries) => {
                let Some(top) = entries.first() else {
                    lbl.set_text("尚无自检记录（本页不显示不存在的历史）");
                    return;
                };
                let pct = top
                    .remaining_percent
                    .map(|p| format!("，剩余 {p}%"))
                    .unwrap_or_default();
                // 真机 ATA 日志条目**没有 `num` 字段**（2026-10-09 实测），
                // num=0 时不显示「第 0 次」；`lifetime_hours` 有就带上。
                let seq = if top.num > 0 {
                    format!("第 {} 次 ", top.num)
                } else {
                    String::new()
                };
                let life = top
                    .lifetime_hours
                    .map(|h| format!(" · 自检时已通电 {h} 小时"))
                    .unwrap_or_default();
                lbl.set_text(&format!(
                    "最新：{}{} — {}（{}{}{}）",
                    seq,
                    top.kind,
                    top.state.cn(),
                    top.status,
                    pct,
                    life
                ));
                lbl.remove_css_class("success");
                lbl.remove_css_class("error");
                lbl.remove_css_class("warning");
                match top.state {
                    TestState::Passed => lbl.add_css_class("success"),
                    TestState::Failed => lbl.add_css_class("error"),
                    _ => lbl.add_css_class("warning"),
                }
                if top.state == TestState::Running && ticks_left > 0 {
                    self.schedule_poll(dev, ticks_left - 1);
                } else if top.state == TestState::Running {
                    lbl.set_text(&format!(
                        "仍在执行（轮询已达上限，可点「自检记录」继续查看）—— {}",
                        top.status
                    ));
                }
            }
        }
    }

    /// 30s 后再读一次自检日志（代数不符 = 已刷新/已退出 → 不再续）。
    fn schedule_poll(&self, dev: String, ticks_left: u32) {
        let poll_id = self.poll_gen.get();
        glib::timeout_add_seconds(30, move || {
            let mut alive = false;
            with_inner(|i| {
                if i.poll_gen.get() == poll_id {
                    alive = true;
                }
            });
            if !alive {
                return glib::ControlFlow::Break;
            }
            with_inner(|i| i.spawn_selftest_read(dev.clone(), ticks_left));
            glib::ControlFlow::Break
        });
    }

    /// 启动自检（**已在确认弹窗的 OK 分支内**，后台线程执行）。
    fn start_test(&self, dev: String, kind: SelfTestKind) {
        if !self.enter_busy() {
            return;
        }
        self.status
            .set_text(&format!("正在启动{}（可能弹出授权框）…", kind.cn()));
        let minutes = match kind {
            SelfTestKind::Short => 2,
            SelfTestKind::Long => 10,
        };
        // 短自检 2 分钟 → 8 次 ×30s；长自检 10 分钟 → 24 次 ×30s（留余量）
        let ticks = match kind {
            SelfTestKind::Short => 8,
            SelfTestKind::Long => 24,
        };
        std::thread::spawn(move || {
            let res = su::run_selftest_start(&dev, kind, su::SMART_TIMEOUT);
            let slot = RefCell::new(Some((dev, kind, minutes, ticks, res)));
            glib::source::idle_add(move || {
                if let Some(t) = slot.take() {
                    with_inner(|i| i.apply_started(t));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_started(
        &self,
        (dev, kind, minutes, ticks, res): (
            String,
            SelfTestKind,
            u32,
            u32,
            Result<String, SmartFailure>,
        ),
    ) {
        self.busy.set(false);
        self.refresh_btn.set_sensitive(true);
        self.spinner.stop();
        match res {
            Err(e) => {
                self.status
                    .set_text(&format!("{}：{}", e.kind_label(), e.kind_hint()));
                self.toast(&format!("{}启动失败：{}", kind.cn(), e.message));
            }
            Ok(_) => {
                self.status
                    .set_text(&format!("{}已受理，预计 {minutes} 分钟内完成", kind.cn()));
                self.toast(&format!("{}已启动（约 {minutes} 分钟）", kind.cn()));
                if let Some(lbl) = self.st_labels.borrow().get(&dev).cloned() {
                    lbl.set_text("自检执行中…（请勿断电或拔盘）");
                    lbl.remove_css_class("success");
                    lbl.remove_css_class("error");
                    lbl.remove_css_class("warning");
                    lbl.add_css_class("warning");
                }
                self.schedule_poll(dev, ticks);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 确认弹窗（D10.5：写设备动作必须用户显式确认）
// ---------------------------------------------------------------------------

fn confirm_start(dev: String, kind: SelfTestKind) {
    let mut parent: Option<gtk::Window> = None;
    with_inner(|i| parent = i.window());
    let minutes = match kind {
        SelfTestKind::Short => 2,
        SelfTestKind::Long => 10,
    };
    let body = format!(
        "将对 {} 发起一次{}（本机预计 {} 分钟）。自检期间磁盘仍可正常使用，\
         但请勿断电或拔盘。这是写入设备的操作，linbox 不提供中止按钮\
         （smartctl 无中止语义）。完成后本页会自动刷新结果。",
        dev,
        kind.cn(),
        minutes
    );
    let dialog = adw::MessageDialog::new(
        parent.as_ref(),
        Some(&format!("运行{}？", kind.cn())),
        Some(&body),
    );
    dialog.add_response("cancel", "取消");
    dialog.add_response("ok", "开始自检");
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |d, resp| {
        d.close();
        if resp == "ok" {
            with_inner(|i| i.start_test(dev.clone(), kind));
        }
    });
    dialog.present();
}

// ---------------------------------------------------------------------------
// 卡片构建
// ---------------------------------------------------------------------------

/// 一块盘的完整卡片（含读取失败形态）。
fn build_card(inner: &Inner, item: LoadItem) -> gtk::Box {
    let (outer, content) = card();
    let dev = item.dev;

    // 卡头：型号 + 路径 + 徽章
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let title_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let title_text = dev.model.clone().unwrap_or_else(|| dev.name.clone());
    let title = gtk::Label::new(Some(&title_text));
    title.add_css_class("title-4");
    title.set_halign(gtk::Align::Start);
    title_box.append(&title);
    let sub = gtk::Label::new(Some(&format!(
        "{} · {}{}",
        dev.path,
        dev.size,
        dev.tran
            .as_deref()
            .map(|t| format!(" · {t}"))
            .unwrap_or_default()
    )));
    sub.add_css_class("dim-label");
    sub.set_halign(gtk::Align::Start);
    title_box.append(&sub);
    head.append(&title_box);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    head.append(&spacer);
    content.append(&head);

    match item.res {
        Err(e) => {
            // 错误态：四类分流（D10.1），给可操作建议
            head.append(&badge(e.kind_label(), "error"));
            let desc = gtk::Label::new(Some(e.kind_hint()));
            desc.set_wrap(true);
            desc.set_halign(gtk::Align::Start);
            desc.add_css_class("dim-label");
            content.append(&desc);
            let detail = gtk::Label::new(Some(&e.message));
            detail.set_wrap(true);
            detail.set_halign(gtk::Align::Start);
            detail.add_css_class("monospace");
            content.append(&detail);

            // 操作：重试（静默直读）+ 权限类再给「授权读取」（显式触发提权）
            let acts = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let retry = gtk::Button::with_label("重试");
            let p = dev.path.clone();
            retry.connect_clicked(move |_| with_inner(|i| i.retry_one(p.clone(), false)));
            acts.append(&retry);
            if e.kind == FailKind::Permission {
                let auth = gtk::Button::with_label("授权读取该盘（会弹系统授权框）");
                auth.add_css_class("suggested-action");
                let p2 = dev.path.clone();
                auth.connect_clicked(move |_| with_inner(|i| i.retry_one(p2.clone(), true)));
                acts.append(&auth);
            }
            content.append(&acts);
            outer
        }
        Ok(report) => {
            let (text, css) = overall_meta(report.overall);
            head.append(&badge(text, css));
            content.append(&build_overview(&dev, &report));
            if let Some(a) = build_alerts(&report) {
                content.append(&a);
            }
            content.append(&build_selftest(inner, &dev));
            content.append(&build_attrs(&report));
            outer
        }
    }
}

/// 健康总览：两列键值。
fn build_overview(dev: &DiskDevice, r: &HealthReport) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 6);

    let mut rows: Vec<(String, String)> = Vec::new();
    rows.push((
        "序列号".to_string(),
        r.device.serial.clone().unwrap_or_else(|| "—".to_string()),
    ));
    rows.push((
        "固件版本".to_string(),
        r.device.firmware.clone().unwrap_or_else(|| "—".to_string()),
    ));
    rows.push(("容量".to_string(), bytes_text(r.device.capacity_bytes)));
    rows.push((
        "接口".to_string(),
        r.device
            .interface
            .clone()
            .unwrap_or_else(|| "—".to_string()),
    ));
    rows.push((
        "介质".to_string(),
        if r.device.is_ssd {
            "固态（SSD）".to_string()
        } else {
            "机械（HDD）".to_string()
        },
    ));
    let hours = match (r.device.power_on_hours, r.hours_tier) {
        (Some(h), Some(t)) => format!("{h} 小时 · {t}"),
        (Some(h), None) => format!("{h} 小时"),
        (None, _) => "—".to_string(),
    };
    rows.push(("通电时间".to_string(), hours));
    rows.push((
        "通电次数".to_string(),
        r.device
            .power_cycle_count
            .map(|c| c.to_string())
            .unwrap_or_else(|| "—".to_string()),
    ));
    let temp = match (r.device.temperature_c, r.temp_tier) {
        (Some(c), Some(t)) => format!("{c} ℃ · {t}"),
        (Some(c), None) => format!("{c} ℃"),
        (None, _) => "—".to_string(),
    };
    rows.push(("温度".to_string(), temp));
    if let Some(t) = &r.tbw {
        rows.push((
            "累计写入 / 读取".to_string(),
            format!(
                "{} / {}",
                format_bytes(t.written_bytes),
                format_bytes(t.read_bytes)
            ),
        ));
    }
    if r.unknown_count > 0 {
        rows.push((
            "未收录属性".to_string(),
            format!("{} 条（显示原始值与「暂无说明」）", r.unknown_count),
        ));
    }
    if let Some(nv) = &r.nvme {
        if let Some(p) = nv.percentage_used {
            rows.push(("已用寿命".to_string(), format!("{p}%")));
        }
        if let Some(s) = nv.available_spare {
            rows.push(("备用块剩余".to_string(), format!("{s}%")));
        }
        if let Some(me) = nv.media_errors {
            rows.push(("介质错误".to_string(), format!("{me}")));
        }
    }

    for (k, v) in rows {
        let (row, lab) = kv_row(&k);
        lab.set_text(&v);
        b.append(&row);
    }

    // 备注（缺字段说明等）
    for n in &r.notes {
        let l = gtk::Label::new(Some(n));
        l.set_wrap(true);
        l.set_halign(gtk::Align::Start);
        l.add_css_class("dim-label");
        b.append(&l);
    }
    // 设备路径兜底显示（多盘时区分）
    let path = gtk::Label::new(Some(dev.path.as_str()));
    path.add_css_class("dim-label");
    path.set_halign(gtk::Align::Start);
    b.append(&path);
    b
}

/// 预警区：无预警不渲染。
fn build_alerts(r: &HealthReport) -> Option<gtk::Box> {
    if r.alerts.is_empty() {
        return None;
    }
    let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
    b.append(&section_title("预警"));
    for a in &r.alerts {
        let l = gtk::Label::new(Some(&a.message));
        l.set_wrap(true);
        l.set_halign(gtk::Align::Start);
        l.add_css_class(if a.severe { "error" } else { "warning" });
        b.append(&l);
    }
    Some(b)
}

/// 自检区：短/长自检按钮 + 记录查看 + 状态标签。
fn build_selftest(inner: &Inner, dev: &DiskDevice) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
    b.append(&section_title("自检"));

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);

    let short = gtk::Button::with_label("运行短自检（约 2 分钟）");
    let d1 = dev.path.clone();
    short.connect_clicked(move |_| confirm_start(d1.clone(), SelfTestKind::Short));
    row.append(&short);

    let long = gtk::Button::with_label("运行长自检（约 10 分钟）");
    let d2 = dev.path.clone();
    long.connect_clicked(move |_| confirm_start(d2.clone(), SelfTestKind::Long));
    row.append(&long);

    let log_btn = gtk::Button::with_label("自检记录");
    log_btn.add_css_class("flat");
    let d3 = dev.path.clone();
    log_btn.connect_clicked(move |_| {
        let d = d3.clone();
        with_inner(|i| {
            if let Some(lbl) = i.st_labels.borrow().get(&d).cloned() {
                lbl.set_text("正在读取自检日志…");
            }
            i.spawn_selftest_read(d, 0);
        });
    });
    row.append(&log_btn);
    b.append(&row);

    let status = gtk::Label::new(Some(
        "点「自检记录」查看历史；运行自检后本页每 30 秒自动刷新进度",
    ));
    status.set_wrap(true);
    status.set_halign(gtk::Align::Start);
    status.add_css_class("dim-label");
    b.append(&status);

    inner
        .st_labels
        .borrow_mut()
        .insert(dev.path.clone(), status.clone());
    b
}

/// 属性表：排序 = FAIL > 预警 > 关键 > 普通（D10.7），行内标色。
fn build_attrs(r: &HealthReport) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
    b.append(&section_title(&format!(
        "SMART 属性（{} 条）",
        r.attrs.len()
    )));

    let alert_ids: Vec<u16> = r
        .alerts
        .iter()
        .filter_map(|a| a.attr.as_deref().map(|x| x.id))
        .collect();
    let fail_ids: Vec<u16> = r.failures.iter().map(|a| a.id).collect();

    // 表头
    let head = attr_row_labels("ID", "属性", "值", "阈", "最差", "原始值", "判定");
    head.0.add_css_class("heading");
    b.append(&head.0);

    let mut attrs: Vec<&SmartAttr> = r.attrs.iter().collect();
    let rank = |a: &SmartAttr| -> u8 {
        if a.is_failing() {
            0
        } else if alert_ids.contains(&a.id) {
            1
        } else if ms::is_key_attr(a.id) {
            2
        } else {
            3
        }
    };
    attrs.sort_by_key(|a| (rank(a), a.id));

    for a in attrs {
        let verdict = if a.is_failing() {
            if a.when_failed.is_empty() {
                "FAIL".to_string()
            } else {
                a.when_failed.clone()
            }
        } else {
            "正常".to_string()
        };
        let raw_text = if a.raw_string.is_empty() {
            a.raw_value.to_string()
        } else {
            a.raw_string.clone()
        };
        let (row, _) = attr_row_labels(
            &a.id.to_string(),
            a.display_name(),
            &a.value.to_string(),
            &a.thresh.to_string(),
            &a.worst.to_string(),
            &raw_text,
            &verdict,
        );
        if fail_ids.contains(&a.id) {
            let mut child = row.first_child();
            while let Some(c) = child {
                if let Some(l) = c.downcast_ref::<gtk::Label>() {
                    l.add_css_class("error");
                }
                child = c.next_sibling();
            }
            row.add_css_class("card");
        } else if alert_ids.contains(&a.id) {
            let mut child = row.first_child();
            while let Some(c) = child {
                if let Some(l) = c.downcast_ref::<gtk::Label>() {
                    l.add_css_class("warning");
                }
                child = c.next_sibling();
            }
        }
        b.append(&row);

        // 中文说明（字典命中才显示；未命中给「暂无说明」兜底）
        let desc = match a.info {
            Some(i) => i.desc.to_string(),
            None => format!(
                "暂无说明：属性 ID {}（{}）未收录于本页词典，仅展示原始值——smartctl 数据库同样未识别该属性，含义未公开。",
                a.id, a.name
            ),
        };
        let d = gtk::Label::new(Some(&desc));
        d.set_wrap(true);
        d.set_halign(gtk::Align::Start);
        d.add_css_class("dim-label");
        d.set_margin_start(8);
        b.append(&d);
    }
    b
}

/// 七列表格行：`(整行, 各标签)`。
fn attr_row_labels(
    id: &str,
    name: &str,
    value: &str,
    thresh: &str,
    worst: &str,
    raw: &str,
    verdict: &str,
) -> (gtk::Box, Vec<gtk::Label>) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let mut labels = Vec::new();
    let specs: [(&str, usize, bool); 7] = [
        (id, 4, false),
        (name, 0, true),
        (value, 3, false),
        (thresh, 3, false),
        (worst, 4, false),
        (raw, 12, false),
        (verdict, 8, false),
    ];
    for (text, chars, expand) in specs {
        let l = gtk::Label::new(Some(text));
        l.set_xalign(0.0);
        if chars > 0 {
            l.set_width_chars(chars as i32);
        }
        if expand {
            l.set_hexpand(true);
        }
        row.append(&l);
        labels.push(l);
    }
    (row, labels)
}

// ---------------------------------------------------------------------------
// 页面装配
// ---------------------------------------------------------------------------

pub fn build() -> SmartPage {
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_vexpand(true);

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_propagate_natural_height(false);
    scroller.set_propagate_natural_width(true);
    scroller.set_vexpand(true);
    toast_overlay.set_child(Some(&scroller));

    let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root_box.set_margin_top(12);
    root_box.set_margin_bottom(12);
    root_box.set_margin_start(12);
    root_box.set_margin_end(12);
    scroller.set_child(Some(&root_box));

    // 工具条
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let status = gtk::Label::new(Some("尚未读取"));
    status.add_css_class("dim-label");
    status.set_halign(gtk::Align::Start);
    status.set_hexpand(true);
    status.set_wrap(true);
    bar.append(&status);

    let spinner = gtk::Spinner::new();
    bar.append(&spinner);

    let refresh_btn = gtk::Button::with_label("刷新");
    bar.append(&refresh_btn);
    root_box.append(&bar);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root_box.append(&content);

    let inner = Rc::new(Inner {
        toast_overlay,
        content,
        status,
        spinner,
        refresh_btn,
        busy: Cell::new(false),
        global_err: RefCell::new(None),
        items: RefCell::new(Vec::new()),
        st_labels: RefCell::new(HashMap::new()),
        poll_gen: Cell::new(0),
    });
    INNER.with(|i| *i.borrow_mut() = Some(inner.clone()));

    {
        let inner2 = inner.clone();
        inner
            .refresh_btn
            .connect_clicked(move |_| with_inner(|i| i.refresh()));
        drop(inner2);
    }

    inner.refresh();
    SmartPage {
        root: inner.toast_overlay.clone(),
    }
}
