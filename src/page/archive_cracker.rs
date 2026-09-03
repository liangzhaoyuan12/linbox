//! 压缩包密码爆破页面（展示层 · 仅 UI）。
//!
//! 功能：
//! - 支持 zip 和 7z 格式，纯 Rust crate 实现，无系统依赖。
//! - 字典类型：纯数字、纯小写、纯大写、特殊符号、数字+小写、数字+大写、
//!   小写+大写、数字+小写+大写、全部混合。
//! - 可设置密码长度范围（最小~最大）。
//! - 支持多压缩包同时爆破，每个压缩包可独立字典或使用全局设置。
//! - 并发扫描 + 暂停/停止。
//!
//! > **用途提示**：请仅对你自己拥有或已获得明确授权的文件使用本模块。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use adw::prelude::*;

use crate::model::archive_cracker::{
    ArchiveConfig, ArchiveFormat, CrackResult, DictConfig, DictType, ScanEvent,
};
use crate::utils::archive_cracker::{self, Control, Dictionary, ScanParams};

pub struct ArchiveCrackerPage {
    root: adw::ToastOverlay,
}

impl ArchiveCrackerPage {
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
    let g = adw::PreferencesGroup::new();
    g.set_title(title);
    if !subtitle.is_empty() {
        g.set_description(Some(subtitle));
    }
    g.add_css_class("card");
    g.set_margin_top(8);
    g.set_margin_bottom(8);
    g.set_margin_start(12);
    g.set_margin_end(12);
    (g.clone(), g)
}

fn entry_row(title: &str) -> adw::EntryRow {
    adw::EntryRow::builder().title(title).build()
}

fn spin_row(title: &str, min: f64, max: f64, step: f64, digits: u32, value: f64) -> adw::SpinRow {
    let adj = gtk::Adjustment::new(value, min, max, step, step * 10.0, 0.0);
    let r = adw::SpinRow::builder()
        .adjustment(&adj)
        .climb_rate(0.5)
        .digits(digits)
        .build();
    r.set_title(title);
    r
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
    let r = adw::SwitchRow::new();
    r.set_title(title);
    if !subtitle.is_empty() {
        r.set_subtitle(subtitle);
    }
    r.set_active(active);
    r
}

fn button_row(buttons: &[&gtk::Button]) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    b.set_margin_top(6);
    b.set_margin_bottom(6);
    for btn in buttons {
        b.append(*btn);
    }
    b
}

fn now_text() -> String {
    glib::DateTime::now_local()
        .map(|dt| {
            dt.format("%H:%M:%S")
                .map(|s| s.to_string())
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

fn duration_text(secs: u64) -> String {
    if secs >= 3600 {
        format!(
            "{}h{:02}m{:02}s",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// 从当前全局 Inner 读取全局字典配置。
fn read_global_dict(s: &Inner) -> DictConfig {
    DictConfig {
        dict_type: DictType::ALL[s.global_type_combo.selected() as usize],
        min_len: s.global_min_len.value() as usize,
        max_len: s.global_max_len.value() as usize,
    }
}

// ---------------------------------------------------------------------------
// Inner
// ---------------------------------------------------------------------------

struct Inner {
    toast_overlay: adw::ToastOverlay,

    // 全局字典设置
    global_type_combo: adw::ComboRow,
    global_min_len: adw::SpinRow,
    global_max_len: adw::SpinRow,
    global_info: gtk::Label,

    // 压缩包列表
    archive_list: gtk::ListBox,
    archive_rows: RefCell<Vec<ArchiveRow>>,
    archive_empty: gtk::Label,

    // 扫描参数
    concurrency_row: adw::SpinRow,

    // 执行
    start_btn: gtk::Button,
    pause_btn: gtk::Button,
    stop_btn: gtk::Button,
    progress: gtk::ProgressBar,
    stat_label: gtk::Label,

    // 结果
    result_list: gtk::ListBox,
    result_empty: gtk::Label,
    results: RefCell<Vec<CrackResult>>,

    // 日志
    log_view: gtk::TextView,

    // 运行期
    receiver: RefCell<Option<mpsc::Receiver<ScanEvent>>>,
    control: RefCell<Option<Control>>,
    running: Cell<bool>,
    paused: Cell<bool>,
    started_at: RefCell<Option<Instant>>,
}

struct ArchiveRow {
    config: RefCell<ArchiveConfig>,
    check: gtk::CheckButton,
    label: gtk::Label,
    status: gtk::Label,
}

impl Inner {
    fn append_log(&self, text: &str) {
        let buf = self.log_view.buffer();
        let mut end = buf.end_iter();
        buf.insert(&mut end, &format!("[{}] {text}\n", now_text()));
        // 截断
        let line_count = buf.line_count();
        if line_count > 450 {
            let start_iter = buf.start_iter();
            if let Some(mut trim) = buf.iter_at_line(400) {
                trim.backward_char();
                buf.delete(&mut start_iter.clone(), &mut trim);
            }
        }
        let end = buf.end_iter();
        self.log_view
            .scroll_mark_onscreen(&buf.create_mark(None, &end, false));
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

    fn update_global_info(&self) {
        let dict = read_global_dict(self);
        let total = dict.total_candidates();
        let mem = dict.estimate_memory();
        let total_str = if total >= 1_000_000_000 {
            format!("{:.2}B", total as f64 / 1_000_000_000.0)
        } else if total >= 1_000_000 {
            format!("{:.1}M", total as f64 / 1_000_000.0)
        } else if total >= 1_000 {
            format!("{:.1}K", total as f64 / 1_000.0)
        } else {
            total.to_string()
        };
        let mem_str = if mem >= 1_073_741_824 {
            format!("{:.1} GB", mem as f64 / 1_073_741_824.0)
        } else if mem >= 1_048_576 {
            format!("{:.1} MB", mem as f64 / 1_048_576.0)
        } else {
            format!("{} KB", mem / 1024)
        };
        self.global_info
            .set_text(&format!("字典候选：{total_str} 条，约 {mem_str} 内存"));
    }

    fn build_result_row(r: &CrackResult) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        b.set_margin_start(8);
        b.set_margin_end(8);
        b.set_margin_top(4);
        b.set_margin_bottom(4);

        // 状态图标
        let icon_name = if r.password.is_some() {
            "object-select-symbolic"
        } else {
            "dialog-error-symbolic"
        };
        let icon = gtk::Image::from_icon_name(icon_name);
        if r.password.is_some() {
            icon.add_css_class("success");
        } else {
            icon.add_css_class("error");
        }
        b.append(&icon);

        // 文件名
        let name_label = gtk::Label::new(Some(&r.file_name));
        name_label.set_hexpand(true);
        name_label.set_xalign(0.0);
        name_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        b.append(&name_label);

        // 密码
        let pwd_text = match &r.password {
            Some(p) => format!("密码: {p}"),
            None => "未找到".to_string(),
        };
        let pwd_label = gtk::Label::new(Some(&pwd_text));
        pwd_label.add_css_class("monospace");
        if r.password.is_some() {
            pwd_label.add_css_class("success");
        } else {
            pwd_label.add_css_class("dim-label");
        }
        b.append(&pwd_label);

        // 已测/耗时
        let info_label = gtk::Label::new(Some(&format!(
            "{} / {}",
            r.tested,
            duration_text(r.elapsed_secs)
        )));
        info_label.add_css_class("dim-label");
        info_label.set_width_chars(16);
        b.append(&info_label);

        row.set_child(Some(&b));
        row
    }

    fn rebuild_result_list(&self) {
        while let Some(child) = self.result_list.first_child() {
            self.result_list.remove(&child);
        }
        let results = self.results.borrow();
        self.result_empty.set_visible(results.is_empty());
        for r in results.iter() {
            self.result_list.append(&Self::build_result_row(r));
        }
    }
}

// ---------------------------------------------------------------------------
// 全局操作函数
// ---------------------------------------------------------------------------

fn g_add_archive() {
    let Some(s) = self_rc() else { return };
    let dialog = gtk::FileDialog::builder()
        .title("选择压缩包（zip / 7z）")
        .build();

    let s_weak = self_rc().map(|s| Rc::downgrade(&s));
    dialog.open_multiple(
        None::<&gtk::Window>,
        gtk::gio::Cancellable::NONE,
        move |result| {
            if let Ok(files) = result {
                let n = files.n_items();
                for i in 0..n {
                    if let Some(file) = files.item(i).and_then(|f| f.downcast::<gtk::gio::File>().ok()) {
                        if let Some(path) = file.path() {
                            let path_str = path.to_string_lossy();
                            if let Some(config) = ArchiveConfig::new(&path_str) {
                                if let Some(s) = s_weak.as_ref().and_then(|w| w.upgrade()) {
                                    g_add_archive_inner(&s, config);
                                }
                            }
                        }
                    }
                }
            }
        },
    );
}

fn g_add_archive_inner(s: &Inner, config: ArchiveConfig) {
    let global_dict = read_global_dict(s);
    let dict_label = if config.use_global_dict {
        format!(
            "{} · {}~{}位",
            global_dict.dict_type.label(),
            global_dict.min_len,
            global_dict.max_len
        )
    } else {
        format!(
            "{} · {}~{}位（自定义）",
            config.dict.dict_type.label(),
            config.dict.min_len,
            config.dict.max_len
        )
    };

    let icon_name = match config.format {
        ArchiveFormat::Zip => "package-x-generic-symbolic",
        ArchiveFormat::SevenZip => "package-x-generic-symbolic",
    };

    // 构建行
    let row_widget = gtk::Box::new(gtk::Orientation::Vertical, 4);
    row_widget.set_margin_top(4);
    row_widget.set_margin_bottom(4);
    row_widget.set_margin_start(8);
    row_widget.set_margin_end(8);

    let top_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let check = gtk::CheckButton::new();
    check.set_active(true);
    top_box.append(&check);

    let icon = gtk::Image::from_icon_name(icon_name);
    top_box.append(&icon);

    let name_label = gtk::Label::new(Some(&format!(
        "{} ({})",
        config.file_name,
        human_size(config.file_size)
    )));
    name_label.set_hexpand(true);
    name_label.set_xalign(0.0);
    name_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    top_box.append(&name_label);

    let status = gtk::Label::new(Some("待爆破"));
    status.add_css_class("dim-label");
    top_box.append(&status);

    row_widget.append(&top_box);

    let dict_info_label = gtk::Label::new(Some(&dict_label));
    dict_info_label.add_css_class("caption");
    dict_info_label.add_css_class("dim-label");
    dict_info_label.set_halign(gtk::Align::Start);
    dict_info_label.set_margin_start(36);
    row_widget.append(&dict_info_label);

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&row_widget));

    s.archive_list.append(&row);
    s.archive_empty.set_visible(false);

    let arch_row = ArchiveRow {
        config: RefCell::new(config),
        check,
        label: name_label,
        status,
    };
    s.archive_rows.borrow_mut().push(arch_row);
}

fn g_remove_selected() {
    let Some(s) = self_rc() else { return };
    let rows = s.archive_rows.borrow();
    let to_remove: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.check.is_active())
        .map(|(i, _)| i)
        .rev()
        .collect();
    drop(rows);

    let mut rows = s.archive_rows.borrow_mut();
    for i in to_remove {
        if let Some(r) = rows.get(i) {
            // 从 ListBox 移除对应行
            if let Some(parent) = r.label.parent() {
                if let Some(row) = parent.parent() {
                    if let Ok(listbox) = row.downcast::<gtk::ListBoxRow>() {
                        if let Some(listbox_parent) = listbox.parent() {
                            if let Ok(lb) = listbox_parent.downcast::<gtk::ListBox>() {
                                lb.remove(&listbox);
                            }
                        }
                    }
                }
            }
        }
        rows.remove(i);
    }
    s.archive_empty.set_visible(rows.is_empty());
}

fn g_start() {
    let Some(s) = self_rc() else { return };
    if s.running.get() {
        return;
    }

    let rows = s.archive_rows.borrow();
    let selected: Vec<ArchiveConfig> = rows
        .iter()
        .filter(|r| r.check.is_active())
        .map(|r| {
            let mut c = r.config.borrow().clone();
            // 更新字典设置
            if c.use_global_dict {
                c.dict = read_global_dict(&s);
            }
            c
        })
        .collect();
    drop(rows);

    if selected.is_empty() {
        let toast = adw::Toast::new("请至少勾选一个压缩包");
        s.toast_overlay.add_toast(toast);
        return;
    }

    // 校验所有文件存在
    for a in &selected {
        if !std::path::Path::new(&a.path).exists() {
            let toast = adw::Toast::new(&format!("文件不存在：{}", a.file_name));
            s.toast_overlay.add_toast(toast);
            return;
        }
    }

    let concurrency = s.concurrency_row.value() as usize;
    let total_candidates: u128 = selected.iter().map(|a| a.dict.total_candidates()).sum();

    // 构建字典列表
    let dictionaries: Vec<Dictionary> = selected.iter().map(|a| Dictionary::new(&a.dict)).collect();

    s.results.borrow_mut().clear();
    s.rebuild_result_list();

    let (tx, rx) = mpsc::channel();
    *s.receiver.borrow_mut() = Some(rx);

    let control = archive_cracker::start(
        ScanParams {
            archives: selected,
            dictionaries,
            concurrency,
        },
        tx,
    );

    *s.control.borrow_mut() = Some(control);
    s.set_running_state(true);
    *s.started_at.borrow_mut() = Some(Instant::now());

    s.append_log(&format!(
        "开始爆破，共 {} 个压缩包，{:.0} 个候选密码",
        total_candidates.min(u128::MAX),
        total_candidates.min(u128::MAX),
    ));
}

fn g_toggle_pause() {
    let Some(s) = self_rc() else { return };
    if !s.running.get() {
        return;
    }
    // 暂停/继续：通过停止+重新启动实现（简单方案）
    // 更好的方案是让 Control 支持 pause
    s.append_log("提示：压缩包爆破暂不支持暂停，请使用停止");
}

fn g_stop() {
    let Some(s) = self_rc() else { return };
    if let Some(ref ctrl) = *s.control.borrow() {
        ctrl.stop();
    }
    s.append_log("已停止");
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
        for _ in 0..200 {
            match rx.try_recv() {
                Ok(ScanEvent::Started {
                    archives,
                    total_candidates,
                }) => {
                    s.append_log(&format!(
                        "引擎已启动，{archives} 个压缩包，{total_candidates} 个候选"
                    ));
                }
                Ok(ScanEvent::Found {
                    path,
                    password,
                    tested,
                    elapsed_secs,
                }) => {
                    let file_name = path
                        .rsplit('/')
                        .next()
                        .or_else(|| path.rsplit('\\').next())
                        .unwrap_or(&path)
                        .to_string();
                    s.results.borrow_mut().push(CrackResult {
                        path,
                        file_name: file_name.clone(),
                        password: Some(password.clone()),
                        tested,
                        elapsed_secs,
                        error: None,
                    });
                    s.rebuild_result_list();
                    s.append_log(&format!("*** 找到密码：{file_name} -> {password} ***"));

                    // 更新压缩包列表中的状态
                    for row in s.archive_rows.borrow().iter() {
                        if row.config.borrow().file_name == file_name {
                            row.status.set_text("已破解");
                            row.status.remove_css_class("dim-label");
                            row.status.add_css_class("success");
                        }
                    }
                }
                Ok(ScanEvent::Exhausted {
                    path,
                    tested,
                    elapsed_secs,
                }) => {
                    let file_name = path
                        .rsplit('/')
                        .next()
                        .or_else(|| path.rsplit('\\').next())
                        .unwrap_or(&path)
                        .to_string();
                    s.results.borrow_mut().push(CrackResult {
                        path,
                        file_name: file_name.clone(),
                        password: None,
                        tested,
                        elapsed_secs,
                        error: None,
                    });
                    s.rebuild_result_list();
                    s.append_log(&format!("{file_name}：未找到密码（已测 {tested}）"));

                    for row in s.archive_rows.borrow().iter() {
                        if row.config.borrow().file_name == file_name {
                            row.status.set_text("未找到");
                            row.status.remove_css_class("dim-label");
                            row.status.add_css_class("error");
                        }
                    }
                }
                Ok(ScanEvent::Error { path, message }) => {
                    let file_name = path
                        .rsplit('/')
                        .next()
                        .or_else(|| path.rsplit('\\').next())
                        .unwrap_or(&path)
                        .to_string();
                    s.append_log(&format!("{file_name} 出错：{message}"));
                    for row in s.archive_rows.borrow().iter() {
                        if row.config.borrow().file_name == file_name {
                            row.status.set_text("出错");
                            row.status.add_css_class("warning");
                        }
                    }
                }
                Ok(ScanEvent::Log(msg)) => {
                    s.append_log(&msg);
                }
                Ok(ScanEvent::Finished { found, total }) => {
                    finished = Some((found, total));
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

    if let Some((found, total)) = finished {
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
            "爆破完成 — 找到 {found} 个密码，已测试 {total} 次，耗时 {}",
            duration_text(elapsed)
        ));
        s.append_log(&format!(
            "爆破完成：找到 {found} 个密码，测试 {total} 次，耗时 {}",
            duration_text(elapsed)
        ));
    } else if s.running.get() {
        if let Some(ref started_at) = *s.started_at.borrow() {
            let elapsed = started_at.elapsed().as_secs();
            s.stat_label.set_text(&format!("爆破中... 已运行 {}", duration_text(elapsed)));
        }
    }

    glib::ControlFlow::Continue
}

// ---------------------------------------------------------------------------
// 构建页面
// ---------------------------------------------------------------------------

const DICT_TYPE_LABELS: &[&str] = &[
    "纯数字 (0-9)",
    "纯小写 (a-z)",
    "纯大写 (A-Z)",
    "特殊符号",
    "数字+小写",
    "数字+大写",
    "小写+大写",
    "数字+小写+大写",
    "全部混合",
];

pub fn build() -> ArchiveCrackerPage {
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

    // 标题
    let title = gtk::Label::new(Some("压缩包密码爆破"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    root_box.append(&title);

    let subtitle = gtk::Label::new(Some(
        "对 zip / 7z 压缩包进行密码爆破，纯 Rust 实现，无系统依赖。",
    ));
    subtitle.add_css_class("dim-label");
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_wrap(true);
    root_box.append(&subtitle);

    let notice = gtk::Label::new(Some(
        "请仅对你自己拥有或已获得明确授权的文件使用本模块。",
    ));
    notice.add_css_class("dim-label");
    notice.add_css_class("caption");
    notice.set_halign(gtk::Align::Start);
    notice.set_wrap(true);
    notice.set_margin_top(4);
    root_box.append(&notice);

    // ---------- 全局字典设置 ----------
    let (dict_card, dc) = card("全局字典设置", "所有使用「全局设置」的压缩包共用此配置");
    root_box.append(&dict_card);

    let type_combo = combo_row("字符集", DICT_TYPE_LABELS, 0);
    dc.add(&type_combo);

    let min_len = spin_row("最小密码长度", 1.0, 16.0, 1.0, 0, 1.0);
    dc.add(&min_len);
    let max_len = spin_row("最大密码长度", 1.0, 16.0, 1.0, 0, 6.0);
    dc.add(&max_len);

    let global_info = gtk::Label::new(Some(""));
    global_info.add_css_class("dim-label");
    global_info.add_css_class("caption");
    global_info.set_halign(gtk::Align::Start);
    dc.add(&global_info);

    // ---------- 压缩包列表 ----------
    let (archive_card, ac) = card("压缩包列表", "勾选要爆破的压缩包");
    root_box.append(&archive_card);

    let archive_list = gtk::ListBox::new();
    archive_list.add_css_class("boxed-list");
    let archive_scroll = gtk::ScrolledWindow::new();
    archive_scroll.set_child(Some(&archive_list));
    archive_scroll.set_min_content_height(100);
    archive_scroll.set_max_content_height(300);
    ac.add(&archive_scroll);

    let archive_empty = gtk::Label::new(Some("尚未添加压缩包 — 点「添加文件」"));
    archive_empty.add_css_class("dim-label");
    archive_empty.set_margin_top(4);
    ac.add(&archive_empty);

    let add_btn = gtk::Button::with_label("添加文件…");
    let remove_btn = gtk::Button::with_label("移除选中");
    let select_all_btn = gtk::Button::with_label("全选");
    let select_none_btn = gtk::Button::with_label("全不选");
    ac.add(&button_row(&[
        &add_btn,
        &remove_btn,
        &select_all_btn,
        &select_none_btn,
    ]));

    // ---------- 扫描参数 ----------
    let (scan_card, sc) = card("扫描参数", "");
    root_box.append(&scan_card);

    let concurrency_row = spin_row("并发线程数", 1.0, 64.0, 1.0, 0, 8.0);
    sc.add(&concurrency_row);

    // ---------- 执行 ----------
    let (run_card, rc) = card("执行", "");
    root_box.append(&run_card);

    let start_btn = gtk::Button::with_label("开始爆破");
    start_btn.add_css_class("suggested-action");
    let pause_btn = gtk::Button::with_label("暂停");
    pause_btn.set_sensitive(false);
    let stop_btn = gtk::Button::with_label("停止");
    stop_btn.add_css_class("destructive-action");
    stop_btn.set_sensitive(false);
    rc.add(&button_row(&[&start_btn, &pause_btn, &stop_btn]));

    let progress = gtk::ProgressBar::new();
    progress.set_show_text(true);
    progress.set_margin_top(6);
    rc.add(&progress);

    let stat_label = gtk::Label::new(Some("尚未开始"));
    stat_label.set_halign(gtk::Align::Start);
    stat_label.set_wrap(true);
    stat_label.set_selectable(true);
    stat_label.set_margin_top(6);
    rc.add(&stat_label);

    // ---------- 结果 ----------
    let (result_card, rsc) = card("爆破结果", "");
    root_box.append(&result_card);

    let result_list = gtk::ListBox::new();
    result_list.add_css_class("boxed-list");
    result_list.set_selection_mode(gtk::SelectionMode::None);
    let result_scroll = gtk::ScrolledWindow::new();
    result_scroll.set_child(Some(&result_list));
    result_scroll.set_min_content_height(80);
    result_scroll.set_max_content_height(300);
    rsc.add(&result_scroll);

    let result_empty = gtk::Label::new(Some("暂无结果"));
    result_empty.add_css_class("dim-label");
    result_empty.set_margin_top(8);
    result_empty.set_margin_bottom(8);
    rsc.add(&result_empty);

    // ---------- 日志 ----------
    let (log_card, lc) = card("运行日志", "");
    root_box.append(&log_card);
    let log_view = gtk::TextView::new();
    log_view.set_monospace(true);
    log_view.set_editable(false);
    log_view.set_wrap_mode(gtk::WrapMode::WordChar);
    log_view.set_left_margin(8);
    log_view.set_right_margin(8);
    log_view.set_top_margin(6);
    log_view.set_bottom_margin(6);
    log_view.set_size_request(-1, 150);
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
        global_type_combo: type_combo.clone(),
        global_min_len: min_len.clone(),
        global_max_len: max_len.clone(),
        global_info: global_info.clone(),
        archive_list: archive_list.clone(),
        archive_rows: RefCell::new(Vec::new()),
        archive_empty: archive_empty.clone(),
        concurrency_row: concurrency_row.clone(),
        start_btn: start_btn.clone(),
        pause_btn: pause_btn.clone(),
        stop_btn: stop_btn.clone(),
        progress: progress.clone(),
        stat_label: stat_label.clone(),
        result_list: result_list.clone(),
        result_empty: result_empty.clone(),
        results: RefCell::new(Vec::new()),
        log_view: log_view.clone(),
        receiver: RefCell::new(None),
        control: RefCell::new(None),
        running: Cell::new(false),
        paused: Cell::new(false),
        started_at: RefCell::new(None),
    });

    // ---------- 信号连接 ----------
    start_btn.connect_clicked(|_| g_start());
    stop_btn.connect_clicked(|_| g_stop());
    pause_btn.connect_clicked(|_| g_toggle_pause());
    add_btn.connect_clicked(|_| g_add_archive());
    remove_btn.connect_clicked(|_| g_remove_selected());
    clear_log_btn.connect_clicked(|_| {
        if let Some(s) = self_rc() {
            s.log_view.buffer().set_text("");
        }
    });

    select_all_btn.connect_clicked(|_| {
        if let Some(s) = self_rc() {
            for r in s.archive_rows.borrow().iter() {
                r.check.set_active(true);
            }
        }
    });
    select_none_btn.connect_clicked(|_| {
        if let Some(s) = self_rc() {
            for r in s.archive_rows.borrow().iter() {
                r.check.set_active(false);
            }
        }
    });

    // 字典参数变化时更新信息
    type_combo.connect_selected_notify(|_| {
        if let Some(s) = self_rc() {
            s.update_global_info();
        }
    });
    min_len.connect_value_notify(|_| {
        if let Some(s) = self_rc() {
            s.update_global_info();
        }
    });
    max_len.connect_value_notify(|_| {
        if let Some(s) = self_rc() {
            s.update_global_info();
        }
    });

    INNER.with(|i| *i.borrow_mut() = Some(Rc::downgrade(&inner)));

    // 初始化字典信息
    inner.update_global_info();

    // 事件循环
    glib::source::timeout_add(Duration::from_millis(100), tick);

    ArchiveCrackerPage { root: toast_overlay }
}

/// 关闭时清理。
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
