//! 全盘垃圾文件清理页面（GOAL Phase 9 · D9.1-D9.9，360 安全卫士式）。
//!
//! 布局（自上而下）：
//! - **扫描控制卡**：「快速扫描（推荐）/ 深度扫描」两档按钮（D9.7）+ 临时文件阈值
//!   输入（默认 7 天，1-90）+ 「包含网络挂载」勾选（D9.7 灰显可勾，仅深度档生效）；
//! - **进度行**（扫描中可见）：进度条 + 当前路径 + 百分比 + 「取消」（D9.7 可中断）；
//! - **结果区**：空态 `adw::StatusPage`（未扫描）→ 结果树按**十大类分组**，
//!   每条 = 勾选框 / 路径 / 中文说明 / 文件数 / 体积 / 风险徽章（安全·按需·谨慎），
//!   父子去重标「已含子项」（D9.9），只报项灰显不进勾选；深度档追加「模式命中
//!   TOP20（只报）」段；顶部汇总行（存在 / 缺失 / 无法访问 / 用时）；
//! - **合计与操作卡**：底部合计「可回收 X.X GB」随勾选实时更新（9.4 口径），
//!   「一键全选安全类」（不越权勾谨慎）→「开始清理」——**含谨慎项时先弹
//!   `adw::MessageDialog` 列出将执行的命令**（D9.2 二次确认，参照 fstab 7.6 写法）。
//!
//! 线程模型与 `page::cpufreq` 相同：扫描/清理全在 `std::thread::spawn`（清理会跑
//! `rm`/`pkexec`，**绝不能在 UI 线程**），子线程只搬 `Send` 纯数据，结果经
//! `glib::source::idle_add` + 全局 `with_inner` 回主线程；GTK 控件只在 UI 线程碰。

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use adw::prelude::*;

use crate::model::cleaner::{ScanEntry, by_id};
use crate::utils::cleaner::{self as cl, PatternHit, ScanOptions, ScanReport, format_bytes};

pub struct CleanerPage {
    root: adw::ToastOverlay,
}

impl CleanerPage {
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

/// 风险徽章：`[安全]` / `[按需]` / `[谨慎]` + 配色。
fn risk_badge(risk: crate::model::cleaner::Risk) -> gtk::Label {
    let b = gtk::Label::new(Some(&format!("[{}]", risk.label())));
    b.add_css_class("caption");
    b.add_css_class(match risk {
        crate::model::cleaner::Risk::Safe => "success",
        crate::model::cleaner::Risk::OnDemand => "warning",
        crate::model::cleaner::Risk::Caution => "error",
    });
    b.set_valign(gtk::Align::Center);
    b
}

/// 一次扫描的完整结果（全部 `Send` 纯数据）。
struct ScanData {
    report: ScanReport,
    deep: bool,
    hits: Vec<PatternHit>,
    elapsed_ms: u128,
}

/// 一次清理批次的结果。
struct CleanData {
    /// 实际回收字节（逐条 before-after 差的累加，不看命令 RC）。
    reclaimed: u64,
    /// 成功清理条数。
    done: usize,
    /// 单条失败/异常的逐条回报（9.7-⑤：不中断其余）。
    failures: Vec<String>,
    /// 清理后的复扫报告（复扫对比）。
    report: ScanReport,
}

// ---------------------------------------------------------------------------
// 页面主体
// ---------------------------------------------------------------------------

struct Inner {
    toast_overlay: adw::ToastOverlay,

    // 扫描控制
    quick_btn: gtk::Button,
    deep_btn: gtk::Button,
    day_spin: gtk::SpinButton,
    net_check: gtk::CheckButton,
    spinner: gtk::Spinner,
    status_lbl: gtk::Label,

    // 进度行
    progress_row: gtk::Box,
    progress_bar: gtk::ProgressBar,
    progress_path: gtk::Label,
    progress_pct: gtk::Label,
    cancel_btn: gtk::Button,

    // 结果区
    results_stack: gtk::Stack,
    empty_page: adw::StatusPage,
    summary_lbl: gtk::Label,
    results_box: gtk::Box,

    // 合计与操作
    total_lbl: gtk::Label,
    total_sub_lbl: gtk::Label,
    safe_btn: gtk::Button,
    clean_btn: gtk::Button,

    // 状态
    entries: RefCell<Vec<ScanEntry>>,
    checked: RefCell<HashSet<String>>,
    rows: RefCell<Vec<(String, gtk::CheckButton)>>,
    hits: RefCell<Vec<PatternHit>>,
    scan_cancel: RefCell<Option<Arc<AtomicBool>>>,
    busy: Cell<bool>,
    /// 程序化改勾选时置位，避免 `connect_toggled` 递归。
    updating: Cell<bool>,
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
        self.busy.set(true);
        true
    }

    /// 扫描/清理中：控制区全部禁用（「扫描中禁止重复点击」）。
    fn set_controls_enabled(&self, on: bool) {
        self.quick_btn.set_sensitive(on);
        self.deep_btn.set_sensitive(on);
        self.day_spin.set_sensitive(on);
        self.net_check.set_sensitive(on);
        self.safe_btn
            .set_sensitive(on && !self.entries.borrow().is_empty());
        self.clean_btn
            .set_sensitive(on && !self.checked.borrow().is_empty());
    }

    fn set_scanning_ui(&self, scanning: bool) {
        self.progress_row.set_visible(scanning);
        self.progress_path.set_visible(scanning);
        if scanning {
            self.spinner.set_visible(true);
            self.spinner.start();
            self.progress_bar.set_fraction(0.0);
        } else {
            self.spinner.stop();
            self.spinner.set_visible(false);
        }
        self.set_controls_enabled(!scanning);
    }

    // ---------------- 扫描 ----------------

    fn start_scan(&self, deep: bool) {
        if !self.enter_busy() {
            return;
        }
        let mut opts = ScanOptions::new();
        opts.tmp_min_age_days = self.day_spin.value() as u32;
        let cancel = Arc::clone(&opts.cancel);
        *self.scan_cancel.borrow_mut() = Some(cancel.clone());

        // 目录级进度：当前路径（百分比由目标级进度给出）。
        opts.progress = Some(Arc::new(move |path: &str, _files: u64| {
            let p = path.to_string();
            glib::source::idle_add(move || {
                with_inner(|i| i.progress_path.set_text(&p));
                glib::ControlFlow::Break
            });
        }));
        let deep_flag = deep;
        let include_net = self.net_check.is_active();
        opts.target_progress = Some(Arc::new(move |done: usize, total: usize| {
            let (d, t) = (done, total);
            glib::source::idle_add(move || {
                with_inner(|i| i.on_catalog_progress(d, t, deep_flag));
                glib::ControlFlow::Break
            });
        }));

        self.set_scanning_ui(true);
        self.status_lbl.set_text(if deep {
            "深度扫描中（逐挂载点模式匹配）…"
        } else {
            "快速扫描中…"
        });

        let t0 = Instant::now();
        std::thread::spawn(move || {
            let data = if deep_flag {
                let (report, hits, _mounts) = cl::deep_scan_with(&opts, include_net);
                ScanData {
                    report,
                    deep: true,
                    hits,
                    elapsed_ms: t0.elapsed().as_millis(),
                }
            } else {
                ScanData {
                    report: cl::quick_scan(&opts),
                    deep: false,
                    hits: Vec::new(),
                    elapsed_ms: t0.elapsed().as_millis(),
                }
            };
            let slot = RefCell::new(Some(data));
            glib::source::idle_add(move || {
                if let Some(d) = slot.take() {
                    with_inner(|i| i.apply_scan(d));
                }
                glib::ControlFlow::Break
            });
        });
    }

    /// 目标级进度（catalog 阶段）：快速档 = 直接百分比；深度档 = 前 50%，
    /// 到满后切「模式匹配阶段」流动条。
    fn on_catalog_progress(&self, done: usize, total: usize, deep: bool) {
        if !self.busy.get() {
            return;
        }
        let base = done as f64 / total.max(1) as f64;
        let (frac, pct_text) = if deep {
            (base * 0.5, (base * 50.0) as usize)
        } else {
            (base, (base * 100.0) as usize)
        };
        self.progress_bar.set_fraction(frac);
        self.progress_pct
            .set_text(&format!("{pct_text}%（目录阶段 {done}/{total}）"));
        if deep && done >= total {
            // catalog 阶段完成 → 深度档进入模式匹配（进度未知 → 流动条）。
            self.progress_bar.pulse();
            self.progress_pct
                .set_text("模式匹配阶段（逐挂载点，可取消）…");
        }
    }

    fn cancel_scan(&self) {
        if let Some(c) = self.scan_cancel.borrow().as_ref() {
            c.store(true, Ordering::Relaxed);
            self.status_lbl.set_text("正在取消…（保留已扫部分）");
            self.cancel_btn.set_sensitive(false);
        }
    }

    /// 扫描结果落盘：去重 → 默认勾选 → 渲染结果树 → 更新合计。
    fn apply_scan(&self, mut data: ScanData) {
        self.busy.set(false);
        self.set_scanning_ui(false);
        self.cancel_btn.set_sensitive(true);
        *self.scan_cancel.borrow_mut() = None;

        cl::dedup_parent_child(&mut data.report.entries);
        let checked = cl::default_selection(&data.report.entries);
        let summary = format!(
            "扫描完成（{} · {:.1}s）：存在 {} 项 · 缺失 {} · 无法访问 {}{}",
            if data.deep { "深度" } else { "快速" },
            data.elapsed_ms as f64 / 1000.0,
            data.report.entries.len(),
            data.report.missing,
            data.report.inaccessible,
            if data.report.cancelled {
                " · 已取消（结果为已扫部分）"
            } else {
                ""
            },
        );
        if data.deep {
            self.status_lbl.set_text(&format!(
                "深度扫描完成：模式命中 {} 文件（仅展示）",
                data.hits.len()
            ));
        } else {
            self.status_lbl.set_text("扫描完成");
        }

        if data.report.entries.is_empty() {
            self.empty_page.set_description(Some(&format!(
                "{summary}。\n未找到任何可清理条目——可以试试「深度扫描」做逐挂载点模式匹配。"
            )));
            self.results_stack.set_visible_child_name("empty");
            self.entries.borrow_mut().clear();
            self.checked.borrow_mut().clear();
            self.rows.borrow_mut().clear();
            self.update_total();
            return;
        }

        self.entries.replace(data.report.entries);
        self.checked.replace(checked);
        self.hits.replace(data.hits);
        self.render_results(&summary);
        self.results_stack.set_visible_child_name("results");
        self.update_total();
        self.set_controls_enabled(true);
    }

    /// 渲染结果树：汇总行 + 十大类分组 +（深度档）模式命中段。
    fn render_results(&self, summary: &str) {
        while let Some(c) = self.results_box.first_child() {
            self.results_box.remove(&c);
        }
        self.rows.borrow_mut().clear();

        self.summary_lbl.set_text(summary);

        let entries = self.entries.borrow().clone();
        for group in cl::group_by_category(&entries) {
            // 组头：类别 + 组合计（进合计口径）。
            let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            head.set_margin_top(10);
            let hl = gtk::Label::new(Some(group.category.label()));
            hl.add_css_class("heading");
            hl.set_halign(gtk::Align::Start);
            head.append(&hl);
            let hb = gtk::Label::new(Some(&format_bytes(group.bytes)));
            hb.add_css_class("dim-label");
            hb.set_halign(gtk::Align::Start);
            head.append(&hb);
            self.results_box.append(&head);

            for &idx in &group.indices {
                let e = &entries[idx];
                let row = self.build_row(e);
                self.results_box.append(&row);
            }
        }

        // 深度档：模式命中 TOP20（只报不清——命中路径不在 catalog 白名单，不提供执行）。
        let hits = self.hits.borrow().clone();
        if !hits.is_empty() {
            let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            head.set_margin_top(14);
            let hl = gtk::Label::new(Some("深度扫描 · 模式命中 TOP20（只报不清）"));
            hl.add_css_class("heading");
            hl.set_halign(gtk::Align::Start);
            head.append(&hl);
            let total_hits: u64 = hits.iter().map(|h| h.bytes).sum();
            let hb = gtk::Label::new(Some(&format!(
                "共 {} 文件 · {}",
                hits.len(),
                format_bytes(total_hits)
            )));
            hb.add_css_class("dim-label");
            hb.set_halign(gtk::Align::Start);
            head.append(&hb);
            self.results_box.append(&head);

            for h in cl::top_files(&hits, 20) {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                row.set_margin_top(4);
                row.set_margin_bottom(4);
                let mid = gtk::Box::new(gtk::Orientation::Vertical, 2);
                mid.set_hexpand(true);
                let p = gtk::Label::new(Some(&h.path));
                p.set_halign(gtk::Align::Start);
                p.set_xalign(0.0);
                p.set_ellipsize(gtk::pango::EllipsizeMode::End);
                p.add_css_class("dim-label");
                mid.append(&p);
                let pat = gtk::Label::new(Some(&format!("匹配模式 {}", h.pattern)));
                pat.add_css_class("dim-label");
                pat.add_css_class("caption");
                pat.set_halign(gtk::Align::Start);
                pat.set_xalign(0.0);
                mid.append(&pat);
                row.append(&mid);
                let b = gtk::Label::new(Some(&format_bytes(h.bytes)));
                b.add_css_class("dim-label");
                row.append(&b);
                self.results_box.append(&row);
            }
        }
    }

    /// 单条结果行：勾选框 + 路径/说明/文件数 + 体积 + 风险徽章。
    fn build_row(&self, e: &ScanEntry) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.set_margin_top(4);
        row.set_margin_bottom(4);

        let check = gtk::CheckButton::new();
        if e.report_only {
            // 只报项（D9.9）：灰显、不进勾选。
            check.set_visible(false);
        } else {
            check.set_active(self.checked.borrow().contains(&e.id));
            let id = e.id.clone();
            check.connect_toggled(move |b| {
                with_inner(|i| i.on_toggle(&id, b.is_active()));
            });
            self.rows.borrow_mut().push((e.id.clone(), check.clone()));
        }
        row.append(&check);

        let mid = gtk::Box::new(gtk::Orientation::Vertical, 2);
        mid.set_hexpand(true);
        let p = gtk::Label::new(Some(&e.path));
        p.set_halign(gtk::Align::Start);
        p.set_xalign(0.0);
        p.set_ellipsize(gtk::pango::EllipsizeMode::End);
        if e.report_only {
            p.add_css_class("dim-label");
        }
        mid.append(&p);

        let mut desc = e.desc.clone();
        if e.contains_children {
            desc.push_str("（已含子项）");
        }
        if e.files > 0 {
            desc.push_str(&format!(" · {} 个文件", e.files));
        }
        if e.report_only {
            desc.push_str(" · 只报告不清理");
        }
        let d = gtk::Label::new(Some(&desc));
        d.add_css_class("dim-label");
        d.add_css_class("caption");
        d.set_halign(gtk::Align::Start);
        d.set_xalign(0.0);
        d.set_ellipsize(gtk::pango::EllipsizeMode::End);
        mid.append(&d);

        // 只报项给出建议命令（D9.9：显示体积 + 建议命令）。
        if e.report_only
            && let Some(target) = by_id(&e.id)
            && let Some(hint) = cl::hint_command_for(target, cl::detect_pkg_manager())
        {
            let h = gtk::Label::new(Some(&format!("建议命令：{hint}")));
            h.add_css_class("dim-label");
            h.add_css_class("caption");
            h.set_halign(gtk::Align::Start);
            h.set_xalign(0.0);
            h.set_ellipsize(gtk::pango::EllipsizeMode::End);
            h.set_selectable(true);
            mid.append(&h);
        }
        row.append(&mid);

        let right = gtk::Box::new(gtk::Orientation::Vertical, 2);
        right.set_valign(gtk::Align::Center);
        let b = gtk::Label::new(Some(&format_bytes(e.bytes)));
        b.add_css_class("heading");
        b.set_halign(gtk::Align::End);
        right.append(&b);
        right.append(&risk_badge(e.risk));
        row.append(&right);
        row
    }

    // ---------------- 勾选与合计 ----------------

    fn on_toggle(&self, id: &str, active: bool) {
        if self.updating.get() {
            return;
        }
        let mut checked = self.checked.borrow_mut();
        if active {
            checked.insert(id.to_string());
        } else {
            checked.remove(id);
        }
        drop(checked);
        self.update_total();
    }

    /// 底部合计：已勾选可回收量（9.4 口径：只报项不进、父子去重后不虚增）。
    fn update_total(&self) {
        let entries = self.entries.borrow();
        let checked = self.checked.borrow();
        let total = cl::total_checked(&entries, &checked);
        let all = cl::total_all(&entries);
        let n_checked = entries
            .iter()
            .filter(|e| !e.report_only && checked.contains(&e.id))
            .count();
        let n_all = entries.iter().filter(|e| !e.report_only).count();
        self.total_lbl
            .set_text(&format!("可回收 {}", format_bytes(total)));
        self.total_sub_lbl.set_text(&format!(
            "已勾选 {n_checked}/{n_all} 项 · 全部清完 {all}",
            all = format_bytes(all)
        ));
        self.clean_btn
            .set_sensitive(!self.busy.get() && !checked.is_empty());
    }

    /// 一键全选安全类（D9.2：仅【安全】，绝不越权勾按需/谨慎）。
    fn select_all_safe(&self) {
        if self.busy.get() {
            return;
        }
        let entries = self.entries.borrow().clone();
        let sel = cl::select_all_safe(&entries);
        self.updating.set(true);
        for (id, cb) in self.rows.borrow().iter() {
            cb.set_active(sel.contains(id));
        }
        self.updating.set(false);
        *self.checked.borrow_mut() = sel;
        self.update_total();
        self.toast(&format!(
            "已全选安全类（{} 项）；按需/谨慎需手动勾选",
            self.checked.borrow().len()
        ));
    }

    // ---------------- 清理 ----------------

    fn start_clean(&self) {
        if self.busy.get() {
            self.toast("上一个操作还在进行中，请稍候");
            return;
        }
        let ids: Vec<String> = {
            let entries = self.entries.borrow();
            let checked = self.checked.borrow();
            entries
                .iter()
                .filter(|e| !e.report_only && checked.contains(&e.id))
                .map(|e| e.id.clone())
                .collect()
        };
        if ids.is_empty() {
            self.toast("未勾选任何条目");
            return;
        }
        // 谨慎项 → 二次确认弹窗列出将执行的命令（D9.2）。
        let caution: Vec<ScanEntry> = {
            let entries = self.entries.borrow();
            entries
                .iter()
                .filter(|e| ids.contains(&e.id) && e.risk == crate::model::cleaner::Risk::Caution)
                .cloned()
                .collect()
        };
        if caution.is_empty() {
            self.do_clean(ids);
            return;
        }
        let Some(win) = self.window() else {
            return;
        };
        let opts = ScanOptions::new();
        let mut body = format!(
            "已勾选 {} 项，其中【谨慎】{} 项。清理不可恢复，将执行以下谨慎命令：\n",
            ids.len(),
            caution.len()
        );
        for e in &caution {
            let cmd = by_id(&e.id)
                .and_then(|t| cl::cleanup_action(t, &opts).ok().map(|a| a.display))
                .unwrap_or_else(|| "（无执行动作）".to_string());
            body.push_str(&format!("\n• {} \n  {cmd}\n", e.path));
        }
        body.push_str("\n其余为【安全/按需】项。确认后逐条执行，实际回收量以清理前后扫描差为准。");
        let dialog = adw::MessageDialog::new(Some(&win), Some("确认清理所选条目？"), Some(&body));
        dialog.add_response("cancel", "取消");
        dialog.add_response("ok", "确认清理");
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, move |d: &adw::MessageDialog, resp| {
            d.close();
            if resp == "ok" {
                with_inner(|i| i.do_clean(ids.clone()));
            }
        });
        dialog.present();
    }

    /// 逐条执行清理（后台线程）：单条失败不中断其余（9.7-⑤），全部完成后复扫。
    fn do_clean(&self, ids: Vec<String>) {
        if !self.enter_busy() {
            return;
        }
        self.set_scanning_ui(true);
        self.progress_pct.set_text(&format!("0/{}", ids.len()));
        self.status_lbl.set_text("正在清理…");

        std::thread::spawn(move || {
            let opts = ScanOptions::new();
            // 9.7-⑤：单条失败不中断其余、逐条回报——循环语义收口在
            // utils::execute_cleanup_batch（有单测），本闭包只负责每条的进度回调。
            let outcome = cl::execute_cleanup_batch(&ids, &opts, |idx, tot, p_note| {
                glib::source::idle_add(move || {
                    with_inner(|i| {
                        i.progress_path.set_text(&p_note);
                        i.progress_bar.set_fraction(idx as f64 / tot as f64);
                        i.progress_pct.set_text(&format!("{idx}/{tot}"));
                        i.status_lbl.set_text(&format!("正在清理 {idx}/{tot}"));
                    });
                    glib::ControlFlow::Break
                });
            });
            // 复扫（同一阈值口径）：给「清理后对比」与 UI 刷新。
            let mut rescan_opts = ScanOptions::new();
            rescan_opts.tmp_min_age_days = opts.tmp_min_age_days;
            let report = cl::quick_scan(&rescan_opts);
            let data = CleanData {
                reclaimed: outcome.reclaimed,
                done: outcome.done,
                failures: outcome.failures,
                report,
            };
            let slot = RefCell::new(Some(data));
            glib::source::idle_add(move || {
                if let Some(d) = slot.take() {
                    with_inner(|i| i.apply_clean(d));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_clean(&self, mut data: CleanData) {
        self.busy.set(false);
        self.set_scanning_ui(false);
        // 复扫结果落盘（同样去重 + 默认勾选，让用户接着操作）。
        let summary = format!(
            "已清理 {} 项 · 实际回收 {}（前后扫描差）{}",
            data.done,
            format_bytes(data.reclaimed),
            if data.failures.is_empty() {
                String::new()
            } else {
                format!(" · {} 条失败见提示", data.failures.len())
            }
        );
        cl::dedup_parent_child(&mut data.report.entries);
        if data.report.entries.is_empty() {
            self.empty_page
                .set_description(Some("复扫未找到任何可清理条目。"));
            self.results_stack.set_visible_child_name("empty");
            self.entries.borrow_mut().clear();
            self.checked.borrow_mut().clear();
            self.rows.borrow_mut().clear();
        } else {
            let checked = cl::default_selection(&data.report.entries);
            self.entries.replace(data.report.entries);
            self.checked.replace(checked);
            self.hits.borrow_mut().clear();
            self.render_results(&summary);
            self.results_stack.set_visible_child_name("results");
        }
        self.update_total();
        self.set_controls_enabled(true);
        self.status_lbl.set_text(&format!(
            "已清理 {} 项，实际回收 {}",
            data.done,
            format_bytes(data.reclaimed)
        ));
        self.toast(&format!(
            "清理完成：{} 项，实际回收 {}",
            data.done,
            format_bytes(data.reclaimed)
        ));
        if !data.failures.is_empty() {
            let first = &data.failures[0];
            self.toast(&format!(
                "有 {} 条未完全成功（首条：{first}）",
                data.failures.len()
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// 组装
// ---------------------------------------------------------------------------

pub fn build() -> CleanerPage {
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_vexpand(true);

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    // 关键：本页卡片多，不向窗口贡献自然高度（8.4 报障教训），内容自己滚。
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

    let title = gtk::Label::new(Some("垃圾清理"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    root_box.append(&title);

    let subtitle = gtk::Label::new(Some(
        "先全盘扫描、后汇总、再清理（D9.1）；扫描阶段绝不删除。个人数据红线\
         （主目录/文档/下载等六类）永不进清理范围，旧内核只提示不自删（D9.4）。",
    ));
    subtitle.add_css_class("dim-label");
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_wrap(true);
    subtitle.set_margin_top(2);
    root_box.append(&subtitle);

    // ---- 扫描控制卡 ----
    let (ctrl_card, ctrl) = card();

    let btn_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let quick_btn = gtk::Button::with_label("快速扫描（推荐）");
    quick_btn.add_css_class("suggested-action");
    quick_btn.set_tooltip_text(Some(
        "只查 catalog 百余条已知可清理路径，秒级出结果（D9.7 默认档）",
    ));
    btn_row.append(&quick_btn);

    let deep_btn = gtk::Button::with_label("深度扫描");
    deep_btn.set_tooltip_text(Some(
        "catalog + 逐可扫挂载点做通用模式匹配（*.tmp、*~、core.* 等），更全但更慢",
    ));
    btn_row.append(&deep_btn);

    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);
    btn_row.append(&spinner);

    let status_lbl = gtk::Label::new(Some("尚未扫描"));
    status_lbl.add_css_class("dim-label");
    status_lbl.set_halign(gtk::Align::Start);
    status_lbl.set_hexpand(true);
    btn_row.append(&status_lbl);
    ctrl.append(&btn_row);

    // 阈值 + 网络挂载
    let opt_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    opt_row.set_valign(gtk::Align::Center);
    let day_lbl = gtk::Label::new(Some("临时文件阈值（天）："));
    day_lbl.add_css_class("dim-label");
    opt_row.append(&day_lbl);
    let day_spin = gtk::SpinButton::with_range(1.0, 90.0, 1.0);
    day_spin.set_value(cl::DEFAULT_TMP_AGE_DAYS as f64);
    day_spin.set_tooltip_text(Some(
        "/tmp 与 /var/tmp 只统计超过该天数未修改的文件（D9.7，1-90）",
    ));
    opt_row.append(&day_spin);
    let day_hint = gtk::Label::new(Some("（/tmp /var/tmp 只计超过阈值的文件）"));
    day_hint.add_css_class("dim-label");
    day_hint.add_css_class("caption");
    opt_row.append(&day_hint);

    let net_check = gtk::CheckButton::with_label("深度扫描包含网络挂载");
    net_check.add_css_class("dim-label");
    net_check.set_tooltip_text(Some("NFS/SMB/SSHFS 挂载默认跳过，勾上后纳入深度扫描（D9.7 灰显可勾）；伪文件系统与 /boot/efi 永不扫描"));
    opt_row.append(&net_check);
    ctrl.append(&opt_row);

    // 进度行（扫描中可见）
    let progress_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    progress_row.set_visible(false);
    let progress_bar = gtk::ProgressBar::new();
    progress_bar.set_hexpand(true);
    progress_bar.set_valign(gtk::Align::Center);
    progress_row.append(&progress_bar);
    let progress_pct = gtk::Label::new(Some(""));
    progress_pct.add_css_class("caption");
    progress_pct.set_width_chars(22);
    progress_row.append(&progress_pct);
    let cancel_btn = gtk::Button::with_label("取消");
    cancel_btn.set_tooltip_text(Some("取消本次扫描（保留已扫部分）"));
    progress_row.append(&cancel_btn);
    ctrl.append(&progress_row);

    let progress_path = gtk::Label::new(Some(""));
    progress_path.add_css_class("dim-label");
    progress_path.add_css_class("caption");
    progress_path.set_halign(gtk::Align::Start);
    progress_path.set_xalign(0.0);
    progress_path.set_ellipsize(gtk::pango::EllipsizeMode::Start);
    progress_path.set_visible(false);
    ctrl.append(&progress_path);

    root_box.append(&ctrl_card);

    // ---- 结果区：空态 StatusPage / 结果树 ----
    let results_stack = gtk::Stack::new();
    results_stack.set_vexpand(false);
    let empty_page = adw::StatusPage::new();
    empty_page.set_title("尚未扫描");
    empty_page.set_icon_name(Some("user-trash-symbolic"));
    empty_page.set_description(Some(
        "点上方「快速扫描（推荐）」开始。\
         扫描只读不删；结果按十大类分组展示，默认只勾选【安全】项。",
    ));
    results_stack.add_named(&empty_page, Some("empty"));

    // 结果树直接入栈：整页由 build 的外层 ScrolledWindow 滚动，
    // 内层再套滚动会把结果区压成几行高（实测教训）。
    let results_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    results_box.set_margin_top(6);
    results_box.set_margin_bottom(6);
    results_stack.add_named(&results_box, Some("results"));

    let (sum_card, sum_content) = card();
    let summary_lbl = gtk::Label::new(Some(""));
    summary_lbl.add_css_class("dim-label");
    summary_lbl.set_halign(gtk::Align::Start);
    summary_lbl.set_wrap(true);
    sum_content.append(&summary_lbl);
    sum_content.append(&results_stack);
    root_box.append(&sum_card);

    // ---- 合计与操作卡 ----
    let (act_card, act) = card();
    let total_lbl = gtk::Label::new(Some("可回收 0 B"));
    total_lbl.add_css_class("title-3");
    total_lbl.set_halign(gtk::Align::Start);
    act.append(&total_lbl);
    let total_sub_lbl = gtk::Label::new(Some("尚未扫描"));
    total_sub_lbl.add_css_class("dim-label");
    total_sub_lbl.set_halign(gtk::Align::Start);
    act.append(&total_sub_lbl);

    let btn_row2 = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    btn_row2.set_margin_top(6);
    let safe_btn = gtk::Button::with_label("一键全选安全类");
    safe_btn.set_tooltip_text(Some("只勾选【安全】档；按需/谨慎不会被越权勾选（D9.2）"));
    safe_btn.set_sensitive(false);
    btn_row2.append(&safe_btn);
    let clean_btn = gtk::Button::with_label("开始清理");
    clean_btn.add_css_class("destructive-action");
    clean_btn.set_tooltip_text(Some(
        "逐条执行所选清理；含谨慎项时先二次确认列出将执行的命令；实际回收量以清理前后扫描差为准",
    ));
    clean_btn.set_sensitive(false);
    btn_row2.append(&clean_btn);
    act.append(&btn_row2);
    root_box.append(&act_card);

    // ---------- 全局状态 ----------
    let inner = Rc::new(Inner {
        toast_overlay,
        quick_btn,
        deep_btn,
        day_spin,
        net_check,
        spinner,
        status_lbl,
        progress_row,
        progress_bar,
        progress_path,
        progress_pct,
        cancel_btn,
        results_stack,
        empty_page,
        summary_lbl,
        results_box,
        total_lbl,
        total_sub_lbl,
        safe_btn,
        clean_btn,
        entries: RefCell::new(Vec::new()),
        checked: RefCell::new(HashSet::new()),
        rows: RefCell::new(Vec::new()),
        hits: RefCell::new(Vec::new()),
        scan_cancel: RefCell::new(None),
        busy: Cell::new(false),
        updating: Cell::new(false),
    });
    INNER.with(|i| *i.borrow_mut() = Some(inner.clone()));

    {
        let b = inner.quick_btn.clone();
        b.connect_clicked(move |_| with_inner(|i| i.start_scan(false)));
    }
    {
        let b = inner.deep_btn.clone();
        b.connect_clicked(move |_| with_inner(|i| i.start_scan(true)));
    }
    {
        let b = inner.cancel_btn.clone();
        b.connect_clicked(move |_| with_inner(|i| i.cancel_scan()));
    }
    {
        let b = inner.safe_btn.clone();
        b.connect_clicked(move |_| with_inner(|i| i.select_all_safe()));
    }
    {
        let b = inner.clean_btn.clone();
        b.connect_clicked(move |_| with_inner(|i| i.start_clean()));
    }

    inner.results_stack.set_visible_child_name("empty");
    CleanerPage {
        root: inner.toast_overlay.clone(),
    }
}
