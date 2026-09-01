//! 输入法修复页面（展示层 · 仅 UI）。
//!
//! 负责：搭建控件、把"检测 / 写入"两件事委托给 [`crate::utils::imfix`]、
//! 把 [`crate::model::imfix::ImfixReport`] 渲染回界面。本页面不做任何文件 IO /
//! 进程调用 / 计算，这些都在 `utils::imfix`（无 GTK）。

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use glib::clone;

use crate::model::imfix::ImfixReport;
use crate::utils::imfix;

pub struct FcitxFixPage {
    root: adw::ToastOverlay,
}

impl FcitxFixPage {
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

pub fn build() -> FcitxFixPage {
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
    let title = gtk::Label::new(Some("输入法修复（fcitx5 / Wayland）"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    root_box.append(&title);

    let subtitle = gtk::Label::new(Some(
        "修复 fcitx5 在 Wayland 下因 /etc/environment 缺少输入法环境变量，导致部分窗口（GTK / Qt / SDL / GLFW 等）无法使用输入法的问题。",
    ));
    subtitle.add_css_class("dim-label");
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_wrap(true);
    root_box.append(&subtitle);

    // ---------- 检测状态卡片 ----------
    let (status_card, status_content) = card();
    root_box.append(&status_card);

    let fcitx_label = gtk::Label::new(Some("fcitx5 是否已安装：检测中…"));
    fcitx_label.set_halign(gtk::Align::Start);
    fcitx_label.set_selectable(true);
    fcitx_label.set_wrap(true);
    status_content.append(&fcitx_label);

    let env_label = gtk::Label::new(Some("/etc/environment 配置：检测中…"));
    env_label.set_halign(gtk::Align::Start);
    env_label.set_selectable(true);
    env_label.set_wrap(true);
    env_label.set_margin_top(6);
    status_content.append(&env_label);

    let detail_label = gtk::Label::new(Some(""));
    detail_label.add_css_class("dim-label");
    detail_label.set_halign(gtk::Align::Start);
    detail_label.set_wrap(true);
    detail_label.set_margin_top(6);
    status_content.append(&detail_label);

    let detect_button = gtk::Button::with_label("重新检测");
    detect_button.set_halign(gtk::Align::Start);
    detect_button.set_margin_top(6);
    status_content.append(&detect_button);

    // ---------- 修复卡片 ----------
    let (fix_card, fix_content) = card();
    root_box.append(&fix_card);

    let fix_title = gtk::Label::new(Some("修复"));
    fix_title.add_css_class("title-4");
    fix_title.set_halign(gtk::Align::Start);
    fix_content.append(&fix_title);

    let fix_hint = gtk::Label::new(Some(
        "需要 root 权限，通过 pkexec 提权写入 /etc/environment（不会删除文件原有内容）。",
    ));
    fix_hint.add_css_class("dim-label");
    fix_hint.set_halign(gtk::Align::Start);
    fix_hint.set_wrap(true);
    fix_hint.set_margin_bottom(6);
    fix_content.append(&fix_hint);

    let status_label = gtk::Label::new(Some("点击「重新检测」后查看需要补齐的变量。"));
    status_label.set_halign(gtk::Align::Start);
    status_label.set_wrap(true);
    status_label.set_selectable(true);
    fix_content.append(&status_label);

    let fix_button = gtk::Button::with_label("应用修复（需要 root 权限）");
    fix_button.add_css_class("suggested-action");
    fix_button.set_halign(gtk::Align::Start);
    fix_button.set_margin_top(6);
    fix_content.append(&fix_button);

    // ---------- 组装内部状态 ----------
    let inner = Rc::new(Inner {
        toast_overlay: toast_overlay.clone(),
        fcitx_label,
        env_label,
        detail_label,
        status_label,
        fix_button,
        detect_button,
        report: RefCell::new(ImfixReport::default()),
    });

    // 检测按钮
    {
        let inner = Rc::clone(&inner);
        inner
            .detect_button
            .connect_clicked(clone!(#[strong] inner, move |_| run_detect(&inner)));
    }
    // 修复按钮
    {
        let inner = Rc::clone(&inner);
        inner
            .fix_button
            .connect_clicked(clone!(#[strong] inner, move |_| apply_fix(&inner)));
    }

    // 注册全局强引用：子线程的 idle 回调通过它回主线程刷新 UI。
    INNER.with(|i| *i.borrow_mut() = Some(Rc::clone(&inner)));

    // 初次检测
    run_detect(&inner);

    FcitxFixPage { root: toast_overlay }
}

// ---------------------------------------------------------------------------
// 页面内部持有的控件句柄
// ---------------------------------------------------------------------------

struct Inner {
    toast_overlay: adw::ToastOverlay,
    fcitx_label: gtk::Label,
    env_label: gtk::Label,
    detail_label: gtk::Label,
    status_label: gtk::Label,
    fix_button: gtk::Button,
    detect_button: gtk::Button,
    /// 最近一次检测结论（数据来自 `model::imfix`）。
    report: RefCell<ImfixReport>,
}

impl Inner {
    fn toast(&self, msg: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(msg));
    }

    /// 把检测结论写回界面标签，并据此启用/禁用修复按钮。
    fn update_labels(&self) {
        let det = self.report.borrow();

        self.fcitx_label.set_text(&format!(
            "fcitx5 是否已安装：{}",
            if det.fcitx_installed {
                "是"
            } else {
                "否（建议先安装 fcitx5）"
            }
        ));

        self.env_label.set_text(&format!(
            "/etc/environment 配置：{}/{} 项已配置",
            det.configured, det.total
        ));

        if det.missing.is_empty() {
            self.detail_label
                .set_text("所有输入法环境变量均已配置，无需修复。");
            self.status_label
                .set_text("无需修复：输入法环境变量已完整配置。");
            self.fix_button.set_sensitive(false);
        } else {
            let names: Vec<&str> = det.missing.iter().map(|(n, _)| n.as_str()).collect();
            self.detail_label.set_text(&format!(
                "缺失变量（将追加写入，不改动已有内容）：{}",
                names.join(", ")
            ));
            self.status_label.set_text(&format!(
                "需要补齐 {} 条变量，点击「应用修复」通过 pkexec 提权写入。",
                det.missing.len()
            ));
            self.fix_button.set_sensitive(true);
        }
    }
}

// ---------------------------------------------------------------------------
// 全局句柄 + 无捕获的转发函数
// ---------------------------------------------------------------------------

thread_local! {
    /// 当前页面的强引用。
    ///
    /// 必须是 `Option<Rc<Inner>>` 而不是 `Weak<Inner>`：否则 `build()` 返回后
    /// `inner` 被销毁，所有回调都会静默失效。子线程通过它回主线程刷新 UI。
    static INNER: RefCell<Option<Rc<Inner>>> = RefCell::new(None);
}

fn with_inner<F: FnOnce(&Inner)>(f: F) {
    if let Some(inner) = INNER.with(|i| i.borrow().clone()) {
        f(&*inner);
    }
}

// ---------------------------------------------------------------------------
// 后台任务：委托 utils 计算，回主线程渲染
// ---------------------------------------------------------------------------

/// 在后台线程调用 `utils::imfix::detect()`，再回主线程刷新标签。
///
/// 子线程只搬运 `Send` 纯数据（`ImfixReport`），不捕获 `Rc<Inner>`；
/// UI 刷新统一通过全局 `with_inner` 在主线程执行。
fn run_detect(inner: &Rc<Inner>) {
    inner.fcitx_label.set_text("fcitx5 是否已安装：检测中…");
    inner.env_label.set_text("/etc/environment 配置：检测中…");
    inner.detail_label.set_text("");
    inner.fix_button.set_sensitive(false);

    std::thread::spawn(|| {
        let report = imfix::detect();
        let report = std::cell::Cell::new(Some(report));
        glib::source::idle_add(move || {
            if let Some(report) = report.take() {
                with_inner(|i| i.apply_report(report));
            }
            glib::ControlFlow::Break
        });
    });
}

/// 在后台线程调用 `utils::imfix::apply()` 提权写入，再回主线程刷新。
fn apply_fix(inner: &Rc<Inner>) {
    // 取出当前检测结论；基于它决定写哪些变量。
    let report = inner.report.borrow().clone();
    if report.missing.is_empty() {
        inner.toast("已配置完整，无需修复");
        return;
    }

    inner.fix_button.set_sensitive(false);
    inner
        .fix_button
        .set_label("正在提权写入…（请在弹窗中授权）");

    std::thread::spawn(move || {
        let result = imfix::apply(&report);
        let result = std::cell::Cell::new(Some(result));
        glib::source::idle_add(move || {
            if let Some(result) = result.take() {
                with_inner(|i| i.finish_fix(result));
            }
            glib::ControlFlow::Break
        });
    });
}

impl Inner {
    /// 写入检测结论并刷新界面。
    fn apply_report(&self, report: ImfixReport) {
        *self.report.borrow_mut() = report;
        self.update_labels();
    }

    /// 主线程中处理写入结果：提示、刷新状态。
    fn finish_fix(&self, result: Result<usize, String>) {
        self.fix_button.set_sensitive(true);
        self.fix_button
            .set_label("应用修复（需要 root 权限）");

        match result {
            Ok(added) => {
                let fcitx_installed = self.report.borrow().fcitx_installed;

                // 写入成功：把缺失项标记为已补齐（我们刚写入的正是这些项）。
                {
                    let mut r = self.report.borrow_mut();
                    r.configured = r.total;
                    r.missing.clear();
                }
                self.update_labels();

                if !fcitx_installed {
                    self.toast("警告：未检测到 fcitx5，变量已写入但输入法可能不生效，请先安装 fcitx5");
                    self.status_label.set_text(&format!(
                        "已写入 {} 条变量，但警告：未检测到 fcitx5，建议先安装。",
                        added
                    ));
                } else {
                    self.toast(&format!("修复成功，已补齐 {} 条输入法变量", added));
                }
            }
            Err(e) => {
                self.toast(&e);
                self.status_label.set_text(&format!("修复失败：{e}"));
            }
        }
    }
}
