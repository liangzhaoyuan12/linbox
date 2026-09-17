//! inotify 调优页面（展示层 · 仅 UI）。
//!
//! 负责：展示 `/proc/sys/fs/inotify/*` 实时值与 `/etc/sysctl.d/` 持久化配置的
//! 对比，提供「一键提升」「自定义值」两种方式，把"检测 / 应用 / 持久化"三件事
//! 委托给 [`crate::utils::inotify_tune`]。本页面不做任何文件 IO / 进程调用，
//! 这些都在 `utils::inotify_tune`（无 GTK）。
//!
//! 应用流程（顺序很重要）：
//! 1. 先 `pkexec sysctl -w` 立即生效；
//! 2. **回读 /proc 校验实时值真的变了**（sysctl 对越界值可能静默拒绝 → 防假成功）；
//! 3. 校验通过才 `pkexec tee` 写持久化文件（防"重启后回退"这个半闭环）；
//! 4. 刷新界面展示持久化来源。
//!
//! 线程模型：子线程只搬运 `Send` 纯数据，不捕获 `Rc<Inner>`；
//! UI 刷新统一通过全局 `with_inner` 在主线程执行（与 `fcitx_fix` 相同）。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use glib::clone;

use crate::model::inotify::{InotifyStatus, RECOMMENDED_INSTANCES, RECOMMENDED_WATCHES};
use crate::utils::inotify_tune::{self as tune, Param};

pub struct InotifyTunePage {
    root: adw::ToastOverlay,
}

impl InotifyTunePage {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }
}

// ---------------------------------------------------------------------------
// 通用小工具
// ---------------------------------------------------------------------------

/// 构造一个带 padding 的卡片容器，返回 (外层卡片, 内层内容盒)。
fn card() -> (gtk::Box, gtk::Box) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.add_css_class("card");
    outer.set_margin_top(12);
    outer.set_margin_bottom(12);
    outer.set_margin_start(12);
    outer.set_margin_end(12);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    outer.append(&content);
    (outer, content)
}

// ---------------------------------------------------------------------------
// 构建页面
// ---------------------------------------------------------------------------

pub fn build() -> InotifyTunePage {
    // ---------- 根容器 ----------
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
    let title = gtk::Label::new(Some("inotify 监视数提升"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    root_box.append(&title);

    // ---------- 说明卡片 ----------
    let (intro_card, intro_content) = card();
    root_box.append(&intro_card);

    let intro = gtk::Label::new(Some(
        "Linux 用 inotify 机制让程序感知文件变化（VS Code / WebStorm / webpack / nodemon / \
         Docker 挂载等都依赖它）。上限太低时这些工具会报 \
         「ENOSPC: System limit for number of file watchers reached」。\n\
         本页面把 fs.inotify.max_user_watches / max_user_instances 立即调大（sysctl -w），\
         并写入 /etc/sysctl.d/99-linbox-inotify.conf 持久化（重启不丢）。需要 root，走 pkexec。",
    ));
    intro.add_css_class("dim-label");
    intro.set_halign(gtk::Align::Start);
    intro.set_wrap(true);
    intro.set_xalign(0.0);
    intro_content.append(&intro);

    // ---------- 当前状态卡片 ----------
    let (status_card, status_content) = card();
    root_box.append(&status_card);

    let status_title = gtk::Label::new(Some("当前状态"));
    status_title.add_css_class("title-4");
    status_title.set_halign(gtk::Align::Start);
    status_content.append(&status_title);

    let watches_label = gtk::Label::new(Some("max_user_watches：检测中…"));
    watches_label.set_halign(gtk::Align::Start);
    watches_label.set_selectable(true);
    watches_label.set_wrap(true);
    status_content.append(&watches_label);

    let instances_label = gtk::Label::new(Some("max_user_instances：检测中…"));
    instances_label.set_halign(gtk::Align::Start);
    instances_label.set_selectable(true);
    instances_label.set_wrap(true);
    status_content.append(&instances_label);

    let source_label = gtk::Label::new(Some(""));
    source_label.add_css_class("dim-label");
    source_label.set_halign(gtk::Align::Start);
    source_label.set_selectable(true);
    source_label.set_wrap(true);
    source_label.set_xalign(0.0);
    source_label.set_margin_top(6);
    status_content.append(&source_label);

    let detect_button = gtk::Button::with_label("重新检测");
    detect_button.set_halign(gtk::Align::Start);
    detect_button.set_margin_top(6);
    status_content.append(&detect_button);

    // ---------- 快捷预设卡片 ----------
    let (preset_card, preset_content) = card();
    root_box.append(&preset_card);

    let preset_title = gtk::Label::new(Some("一键提升"));
    preset_title.add_css_class("title-4");
    preset_title.set_halign(gtk::Align::Start);
    preset_content.append(&preset_title);

    let preset_hint = gtk::Label::new(Some(&format!(
        "把两项都提到推荐值：max_user_watches → {RECOMMENDED_WATCHES}，\
         max_user_instances → {RECOMMENDED_INSTANCES}。当前已达标的项保持不变、不会被调小。",
    )));
    preset_hint.add_css_class("dim-label");
    preset_hint.set_halign(gtk::Align::Start);
    preset_hint.set_wrap(true);
    preset_hint.set_xalign(0.0);
    preset_content.append(&preset_hint);

    let preset_button = gtk::Button::with_label("应用推荐值（需要 root 权限）");
    preset_button.add_css_class("suggested-action");
    preset_button.set_halign(gtk::Align::Start);
    preset_button.set_margin_top(6);
    preset_content.append(&preset_button);

    // ---------- 自定义值卡片 ----------
    let (custom_card, custom_content) = card();
    root_box.append(&custom_card);

    let custom_title = gtk::Label::new(Some("自定义数值"));
    custom_title.add_css_class("title-4");
    custom_title.set_halign(gtk::Align::Start);
    custom_content.append(&custom_title);

    let custom_hint = gtk::Label::new(Some(
        "直接指定两项上限（单位：个），留空的那项保持当前值不动。\
         watches 建议不超过 2097152；过大数值会侵蚀内核 slab 内存，超出合理范围会被拒绝。",
    ));
    custom_hint.add_css_class("dim-label");
    custom_hint.set_halign(gtk::Align::Start);
    custom_hint.set_wrap(true);
    custom_hint.set_xalign(0.0);
    custom_content.append(&custom_hint);

    let watches_row = adw::EntryRow::new();
    watches_row.set_title("新的 max_user_watches 数值（留空 = 不修改）");
    watches_row.set_input_purpose(gtk::InputPurpose::Digits);
    custom_content.append(&watches_row);

    let instances_row = adw::EntryRow::new();
    instances_row.set_title("新的 max_user_instances 数值（留空 = 不修改）");
    instances_row.set_input_purpose(gtk::InputPurpose::Digits);
    custom_content.append(&instances_row);

    let custom_button = gtk::Button::with_label("应用并持久化（需要 root 权限）");
    custom_button.set_halign(gtk::Align::Start);
    custom_button.set_margin_top(6);
    custom_content.append(&custom_button);

    let custom_status = gtk::Label::new(Some(""));
    custom_status.add_css_class("dim-label");
    custom_status.set_halign(gtk::Align::Start);
    custom_status.set_wrap(true);
    custom_status.set_selectable(true);
    custom_status.set_xalign(0.0);
    custom_content.append(&custom_status);

    // ---------- 组装内部状态 ----------
    let inner = Rc::new(Inner {
        toast_overlay: toast_overlay.clone(),
        watches_label,
        instances_label,
        source_label,
        detect_button,
        preset_button,
        watches_row,
        instances_row,
        custom_button,
        custom_status,
        status: RefCell::new(InotifyStatus {
            watches: 0,
            instances: 0,
            persisted_watches: None,
            persisted_instances: None,
            persisted_source: None,
        }),
        busy: Cell::new(false),
    });

    // 注册全局强引用：子线程的 idle 回调通过它回主线程刷新 UI。
    INNER.with(|i| *i.borrow_mut() = Some(Rc::clone(&inner)));

    // 非 Linux / 无 /proc 支持时：禁用操作按钮并说明。
    if !tune::platform_supported() {
        inner.toast("当前系统不支持 inotify 调优（找不到 /proc/sys/fs/inotify）");
        inner.preset_button.set_sensitive(false);
        inner.custom_button.set_sensitive(false);
    }

    // 检测按钮
    inner
        .detect_button
        .connect_clicked(clone!(#[strong] inner, move |_| run_detect(&inner)));
    // 一键提升
    inner
        .preset_button
        .connect_clicked(clone!(#[strong] inner, move |_| apply_preset(&inner)));
    // 自定义
    inner
        .custom_button
        .connect_clicked(clone!(#[strong] inner, move |_| apply_custom(&inner)));

    // 初次检测
    run_detect(&inner);

    InotifyTunePage { root: toast_overlay }
}

// ---------------------------------------------------------------------------
// 页面内部持有的控件句柄
// ---------------------------------------------------------------------------

struct Inner {
    toast_overlay: adw::ToastOverlay,
    watches_label: gtk::Label,
    instances_label: gtk::Label,
    source_label: gtk::Label,
    detect_button: gtk::Button,
    preset_button: gtk::Button,
    watches_row: adw::EntryRow,
    instances_row: adw::EntryRow,
    custom_button: gtk::Button,
    custom_status: gtk::Label,
    status: RefCell<InotifyStatus>,
    /// 后台任务进行中（防连点重复提权）。
    busy: Cell<bool>,
}

impl Inner {
    fn toast(&self, msg: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(msg));
    }

    /// 把当前状态写回界面标签。
    fn update_labels(&self) {
        let st = self.status.borrow().clone();

        self.watches_label.set_text(&format!(
            "max_user_watches：当前生效 {}{}",
            st.watches,
            if st.watches_low() {
                format!("（偏低，推荐 ≥ {RECOMMENDED_WATCHES}）")
            } else {
                "（足够）".to_string()
            }
        ));

        self.instances_label.set_text(&format!(
            "max_user_instances：当前生效 {}{}",
            st.instances,
            if st.instances < RECOMMENDED_INSTANCES {
                format!("（偏低，建议 ≥ {RECOMMENDED_INSTANCES}）")
            } else {
                "（足够）".to_string()
            }
        ));

        // 持久化情况：来源文件 / 无持久化 / 与生效值不一致（重启会回退）
        let mut src = match &st.persisted_source {
            Some(f) => format!("持久化配置：{f}"),
            None => "持久化配置：无（重启后会回落默认值）".to_string(),
        };
        if st.watches != 0 && !st.watches_persisted_match() {
            src.push_str(&format!(
                "\n⚠ watches 生效值与持久化不一致（生效 {}，持久化 {}）——重启后可能回退。\
                 若持久化来自更晚加载的文件属正常；否则请重新应用一次。",
                st.watches,
                st.persisted_watches
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "无".into())
            ));
        }
        if st.instances != 0 && !st.instances_persisted_match() {
            src.push_str(&format!(
                "\n⚠ instances 生效值与持久化不一致（生效 {}，持久化 {}）。\
                 若是第三方配置（如 KDE）覆盖了本页面写入的值，属正常现象。",
                st.instances,
                st.persisted_instances
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "无".into())
            ));
        }
        self.source_label.set_text(&src);
    }

    /// 应用中：禁用按钮（防连点重复提权）。
    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        self.detect_button.set_sensitive(!busy);
        self.preset_button.set_sensitive(!busy);
        self.custom_button.set_sensitive(!busy);
    }
}

thread_local! {
    /// 当前页面的强引用。
    ///
    /// 必须是 `Option<Rc<Inner>>` 而不是 `Weak<Inner>`：否则 `build()` 返回后
    /// `inner` 被销毁，所有回调都会静默失效。子线程通过它回主线程刷新 UI。
    static INNER: RefCell<Option<Rc<Inner>>> = RefCell::new(None);
}

/// 应用退出前调用：清空全局句柄，避免窗口销毁期的回调访问正在析构的 TLS。
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
    f(&*inner);
}

// ---------------------------------------------------------------------------
// 动作：检测
// ---------------------------------------------------------------------------

/// 后台线程读取状态，回到主线程刷新。子线程只搬运 `InotifyStatus`（纯数据）。
fn run_detect(inner: &Rc<Inner>) {
    inner.set_busy(true);
    inner.watches_label.set_text("max_user_watches：检测中…");
    inner.instances_label.set_text("max_user_instances：检测中…");

    std::thread::spawn(|| {
        let st = tune::status();
        let st = std::cell::Cell::new(Some(st));
        glib::source::idle_add(move || {
            if let Some(st) = st.take() {
                with_inner(|i| {
                    *i.status.borrow_mut() = st;
                    i.update_labels();
                    i.set_busy(false);
                });
            }
            glib::ControlFlow::Break
        });
    });
}

// ---------------------------------------------------------------------------
// 动作：应用推荐值 / 自定义值
// ---------------------------------------------------------------------------

fn apply_preset(inner: &Rc<Inner>) {
    let st = inner.status.borrow().clone();
    let params = tune::preset_params(st.watches, st.instances);
    if params.is_empty() {
        inner.toast("当前值已达标，无需调整");
        return;
    }
    let desc: Vec<String> = params
        .iter()
        .map(|(p, v)| format!("{} → {}", p.label(), v))
        .collect();
    run_apply(inner, params, desc.join("；"));
}

fn apply_custom(inner: &Rc<Inner>) {
    // 两行各自独立：留空 = 该项不修改。至少要填一项。
    let mut params: Vec<(Param, u64)> = Vec::new();
    let mut desc: Vec<String> = Vec::new();

    for (param, row) in [
        (Param::Watches, &inner.watches_row),
        (Param::Instances, &inner.instances_row),
    ] {
        let raw = row.text().trim().to_string();
        if raw.is_empty() {
            continue;
        }
        let Ok(v) = raw.parse::<u64>() else {
            inner.custom_status.set_text(&format!(
                "「{raw}」不是合法数字（{}，必须是纯数字，如 2097152）。",
                param.label()
            ));
            return;
        };
        if let Err(e) = tune::validate(param, v) {
            inner.custom_status.set_text(&format!(
                "{} 数值不合理：{e}",
                param.label()
            ));
            return;
        }
        params.push((param, v));
        desc.push(format!("{} → {v}", param.label()));
    }

    if params.is_empty() {
        inner.custom_status.set_text(
            "两项都留空了 —— 至少填写一项数值，或直接用上方「一键提升」。",
        );
        return;
    }
    inner.custom_status.set_text("");
    run_apply(inner, params, desc.join("；"));
}

/// 后台执行结果（全 `Send`），idle 回主线程一次性交付。
struct Outcome {
    apply: Result<(), String>,
    /// sysctl -w 报成功但回读不一致的参数：(参数, 期望, 实际)。
    mismatched: Vec<(Param, u64, u64)>,
    persist: Result<(), String>,
    desc: String,
    status: InotifyStatus,
}

/// 应用主流程：sysctl -w → 回读校验 → 持久化 → 刷新。
fn run_apply(inner: &Rc<Inner>, params: Vec<(Param, u64)>, desc: String) {
    inner.set_busy(true);
    inner.toast(&format!("正在应用（{desc}），请在弹窗中授权…"));

    let job = std::cell::Cell::new(Some((params, desc)));
    std::thread::spawn(move || {
        let (params, desc) = job.take().expect("job 只被取一次");

        // 1. 立即生效
        let apply = tune::apply_live(&params);
        // 2. 回读校验（sysctl 退出码 0 不代表值真的变了）
        let mismatched = if apply.is_ok() {
            tune::verify_live(&params)
        } else {
            Vec::new()
        };
        // 3. 持久化：只有即时生效且校验通过才写文件
        let persist = if apply.is_ok() && mismatched.is_empty() {
            tune::persist(&params)
        } else {
            Ok(())
        };
        // 4. 重读最新状态
        let status = tune::status();

        let out = std::cell::Cell::new(Some(Outcome {
            apply,
            mismatched,
            persist,
            desc,
            status,
        }));
        glib::source::idle_add(move || {
            if let Some(o) = out.take() {
                with_inner(|i| finish_apply(i, o));
            }
            glib::ControlFlow::Break
        });
    });
}

/// 主线程收尾：写回状态、提示结果。
fn finish_apply(inner: &Inner, o: Outcome) {
    *inner.status.borrow_mut() = o.status.clone();
    inner.update_labels();
    inner.set_busy(false);

    match &o.apply {
        Err(e) => {
            inner.toast("应用失败");
            inner.custom_status.set_text(&format!("✗ {e}"));
        }
        Ok(()) => {
            if !o.mismatched.is_empty() {
                let detail: Vec<String> = o
                    .mismatched
                    .iter()
                    .map(|(p, want, got)| format!("{} 期望 {want}，实际 {got}", p.label()))
                    .collect();
                inner.toast("应用失败：值未生效");
                inner.custom_status.set_text(&format!(
                    "✗ sysctl 未报错但回读不一致：{}",
                    detail.join("；")
                ));
            } else {
                match &o.persist {
                    Ok(()) => {
                        let msg = format!("✓ {}（已持久化，重启不丢）", o.desc);
                        inner.toast(&msg);
                        inner.custom_status.set_text(&msg);
                    }
                    Err(e) => {
                        // 半闭环要明说：即时生效了但没持久化，重启会回退
                        let msg = format!(
                            "△ {} 已即时生效，但持久化写入失败：{e}。重启后会回退。",
                            o.desc
                        );
                        inner.toast("已生效，但持久化失败");
                        inner.custom_status.set_text(&msg);
                    }
                }
            }
        }
    }
}
