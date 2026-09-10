//! 下载页面：HTTP(S) 下载管理。
//!
//! - 添加任务：URL（支持多行批量）、下载目录、文件名（可留空自动）、线程数；
//! - 任务列表：状态、进度条、大小、速度、剩余时间，操作：开始/暂停/继续/取消/删除/打开目录；
//! - 设置：默认目录、UA、超时、同时任务数、重试次数、默认线程数、最小分块大小；
//! - 断点续传：暂停/失败保留 `.part.N` 分片与 `.lbm.json` 现场，恢复时续传；
//! - 下载进度：每 500ms 轮询 `DownloadManager::snapshot()` 增量渲染（只改文本/
//!   进度条，按钮仅在状态切换时重建）。
//!
//! 线程模型：页面持有全局 `DownloadManager`（下载任务生命周期独立于页面），
//! 所有耗时操作在 tokio runtime 中执行；GTK 回调只做同步状态修改。
//!
//! ## 任务行的增删必须用 ListBox 自己的 API（踩过的坑）
//!
//! 每个任务行是自己持有的 `gtk::ListBoxRow`，即 `ListBox` 的**直接**子控件。
//! 不能把普通容器直接 `append` 给 `ListBox` 再靠 `Widget::unparent()` 删除：
//! GTK 会为普通容器隐式包一层 `ListBoxRow`，那层包装不在我们手里，对它
//! `unparent()` 会绕过 `ListBox::remove()`，导致 ListBox 内部行索引不同步——
//! 已删的行仍被 `row_at_index` 计数且指针已失效，随后追加行/测量布局会触发
//! `instance_of::<ListBoxRow>` 断言失败（闪退），或让新行渲染不出来（任务不显示）。
//! 因此：行一律用 `ListBox::append/remove` 增删，且 `rows` 表与 ListBox 一一对应。

use crate::model::download::{TaskSnapshot, TaskStatus};
use crate::utils::download::{downloader::DownloadManager, settings as dl_settings};
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::sync::Arc;

/// 页面外边距与卡片间距（`scroll_list_into_view` 据此推算列表卡片的位置）。
const PAGE_MARGIN_TOP: i32 = 4;
const CARD_SPACING: i32 = 4;

// ---------------------------------------------------------------------------
// 任务行控件
// ---------------------------------------------------------------------------

struct TaskRow {
    /// 行本体：ListBox 的直接子控件（见文件头说明）。
    row: gtk::ListBoxRow,
    name_lbl: gtk::Label,
    status_lbl: gtk::Label,
    progress: gtk::ProgressBar,
    size_lbl: gtk::Label,
    speed_lbl: gtk::Label,
    btnbox: gtk::Box,
    /// 当前渲染的按钮组（1=下载/排队, 2=暂停/失败, 3=完成, 4=已取消, 0=未渲染）。
    /// 按钮只在状态组变化时重建一次——每 tick 重建会导致按钮闪烁且点不中。
    btn_group: RefCell<u8>,
}

impl TaskRow {
    fn new() -> Self {
        let name_lbl = gtk::Label::new(None);
        name_lbl.set_xalign(0.0);
        name_lbl.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name_lbl.set_max_width_chars(70);

        let status_lbl = gtk::Label::new(None);
        status_lbl.set_xalign(0.0);

        let progress = gtk::ProgressBar::new();
        progress.set_show_text(true);

        let size_lbl = gtk::Label::new(None);
        size_lbl.set_xalign(0.0);

        let speed_lbl = gtk::Label::new(None);
        speed_lbl.set_xalign(0.0);

        let btnbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);

        let inner = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        top.append(&name_lbl);
        top.append(&status_lbl);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        top.append(&spacer);
        top.append(&btnbox);
        inner.append(&top);
        inner.append(&progress);
        inner.append(&size_lbl);
        inner.append(&speed_lbl);

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        content.set_margin_top(4);
        content.set_margin_bottom(4);
        content.set_margin_start(8);
        content.set_margin_end(8);
        content.append(&inner);

        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&content));

        TaskRow {
            row,
            name_lbl,
            status_lbl,
            progress,
            size_lbl,
            speed_lbl,
            btnbox,
            btn_group: RefCell::new(0),
        }
    }
}

// ---------------------------------------------------------------------------
// 页面状态
// ---------------------------------------------------------------------------

struct Inner {
    mgr: Arc<DownloadManager>,
    url_input: gtk::TextView,
    add_note_lbl: gtk::Label,
    dir_entry: gtk::Entry,
    filename_entry: gtk::Entry,
    threads_spin: gtk::SpinButton,
    /// 添加卡片：用于把任务列表滚进视野时定位。
    add_card: gtk::Frame,
    list: gtk::ListBox,
    count_lbl: gtk::Label,
    total_speed_lbl: gtk::Label,
    set_dir_entry: gtk::Entry,
    set_ua_entry: gtk::Entry,
    set_timeout: gtk::SpinButton,
    set_conc: gtk::SpinButton,
    set_retries: gtk::SpinButton,
    set_threads: gtk::SpinButton,
    set_minchunk: gtk::SpinButton,
    scroll: gtk::ScrolledWindow,
    rows: RefCell<HashMap<u64, Rc<TaskRow>>>,
    /// 「暂无下载任务」提示：放在 ListBox 外面，用显隐控制。
    /// 放进 ListBox 会多占一个隐藏不掉的行（隐藏子控件，行仍在）。
    empty_lbl: gtk::Label,
    /// 定时刷新 SourceId，shutdown 时取消，避免 timer 空转。
    tick_source: RefCell<Option<glib::SourceId>>,
}

thread_local! {
    static INNER: RefCell<Option<Rc<Inner>>> = RefCell::new(None);
}

fn with_inner<F: FnOnce(&Inner)>(f: F) {
    INNER.with(|c| {
        if let Some(rc) = c.borrow().as_ref() {
            f(rc);
        }
    });
}

/// 取页面弱引用；页面未构建/已销毁时返回 `None`（回调静默跳过，不 panic）。
fn weak_handle() -> Option<Weak<Inner>> {
    INNER.with(|c| c.borrow().as_ref().map(Rc::downgrade))
}

fn call_inner<F: FnOnce(&Inner)>(weak: &Option<Weak<Inner>>, f: F) {
    if let Some(rc) = weak.as_ref().and_then(|w| w.upgrade()) {
        f(&rc);
    }
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

fn format_bytes(v: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let f = v as f64;
    if f >= GB {
        format!("{:.2} GB", f / GB)
    } else if f >= MB {
        format!("{:.2} MB", f / MB)
    } else if f >= KB {
        format!("{:.1} KB", f / KB)
    } else {
        format!("{v} B")
    }
}

fn format_speed(bps: u64) -> String {
    format!("{}/s", format_bytes(bps))
}

fn format_eta(remaining: u64, speed: u64) -> String {
    if speed == 0 {
        return String::new();
    }
    let secs = remaining / speed;
    if secs >= 3600 {
        format!("剩 {}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("剩 {}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("剩 {secs}s")
    }
}

// ---------------------------------------------------------------------------
// 构建
// ---------------------------------------------------------------------------

pub fn build() -> gtk::Widget {
    let mgr = DownloadManager::global(Some(dl_settings::load()));
    let cfg = mgr.config();

    // ---- 添加任务卡片 ----
    let url_input = gtk::TextView::new();
    url_input.set_wrap_mode(gtk::WrapMode::WordChar);
    url_input.set_size_request(-1, 80);
    let url_scroll = gtk::ScrolledWindow::new();
    url_scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    url_scroll.set_child(Some(&url_input));

    let add_note_lbl = gtk::Label::new(None);
    add_note_lbl.set_xalign(0.0);
    add_note_lbl.set_wrap(true);

    let dir_entry = gtk::Entry::new();
    dir_entry.set_text(&cfg.dir);
    let dir_pick_btn = gtk::Button::with_label("选择目录");
    dir_pick_btn.set_icon_name("folder-open-symbolic");

    let filename_entry = gtk::Entry::new();
    filename_entry.set_placeholder_text(Some("留空自动（Content-Disposition / URL）"));

    let threads_spin = gtk::SpinButton::with_range(1.0, 128.0, 1.0);
    threads_spin.set_value(cfg.threads_per_task as f64);

    let add_btn = gtk::Button::with_label("添加下载");
    add_btn.add_css_class("suggested-action");

    let opt_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    opt_row.append(&gtk::Label::new(Some("下载目录：")));
    dir_entry.set_hexpand(true);
    opt_row.append(&dir_entry);
    opt_row.append(&dir_pick_btn);

    let opt_row2 = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    opt_row2.append(&gtk::Label::new(Some("文件名：")));
    filename_entry.set_hexpand(true);
    opt_row2.append(&filename_entry);
    opt_row2.append(&gtk::Label::new(Some("线程数：")));
    opt_row2.append(&threads_spin);

    let add_vb = gtk::Box::new(gtk::Orientation::Vertical, 8);
    add_vb.set_margin_top(8);
    add_vb.set_margin_bottom(8);
    add_vb.set_margin_start(12);
    add_vb.set_margin_end(12);
    add_vb.append(&url_scroll);
    add_vb.append(&add_note_lbl);
    add_vb.append(&opt_row);
    add_vb.append(&opt_row2);
    let add_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    add_row.set_halign(gtk::Align::End);
    add_row.append(&add_btn);
    add_vb.append(&add_row);
    let add_card = gtk::Frame::new(Some("添加下载"));
    add_card.set_child(Some(&add_vb));

    // ---- 任务列表情报行 ----
    let count_lbl = gtk::Label::new(Some("任务数：0"));
    count_lbl.set_xalign(0.0);
    let total_speed_lbl = gtk::Label::new(Some("合计：0 B/s"));
    total_speed_lbl.set_xalign(1.0);
    let info_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    info_row.append(&count_lbl);
    info_row.append(&total_speed_lbl);
    total_speed_lbl.set_hexpand(true);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");

    // 空列表提示：ListBox 之外，显隐切换（放 ListBox 里会留下隐藏不掉的空行）
    let empty_lbl = gtk::Label::new(Some("暂无下载任务"));
    empty_lbl.add_css_class("dim-label");
    empty_lbl.set_margin_top(12);
    empty_lbl.set_margin_bottom(12);

    let list_vb = gtk::Box::new(gtk::Orientation::Vertical, 6);
    list_vb.set_margin_top(8);
    list_vb.set_margin_bottom(8);
    list_vb.set_margin_start(12);
    list_vb.set_margin_end(12);
    list_vb.append(&info_row);
    list_vb.append(&empty_lbl);
    list_vb.append(&list);
    let list_card = gtk::Frame::new(Some("下载任务"));
    list_card.set_child(Some(&list_vb));

    // ---- 设置卡片 ----
    let set_dir_entry = gtk::Entry::new();
    set_dir_entry.set_text(&cfg.dir);
    let set_ua_entry = gtk::Entry::new();
    set_ua_entry.set_text(&cfg.user_agent);
    let set_timeout = gtk::SpinButton::with_range(5.0, 600.0, 5.0);
    set_timeout.set_value(cfg.timeout_secs as f64);
    let set_conc = gtk::SpinButton::with_range(1.0, 20.0, 1.0);
    set_conc.set_value(cfg.max_concurrent_tasks as f64);
    let set_retries = gtk::SpinButton::with_range(0.0, 20.0, 1.0);
    set_retries.set_value(cfg.retries as f64);
    let set_threads = gtk::SpinButton::with_range(1.0, 128.0, 1.0);
    set_threads.set_value(cfg.threads_per_task as f64);
    let set_minchunk = gtk::SpinButton::with_range(0.0, 1024.0, 1.0);
    set_minchunk.set_value((cfg.min_chunk_bytes / (1024 * 1024)) as f64);
    let set_save_btn = gtk::Button::with_label("保存设置（对后续任务生效）");
    set_save_btn.add_css_class("suggested-action");

    let set_grid = gtk::Grid::new();
    set_grid.set_row_spacing(8);
    set_grid.set_column_spacing(8);
    let set_rows: Vec<(String, gtk::Widget, Option<&str>)> = vec![
        ("下载目录".into(), set_dir_entry.clone().upcast(), None),
        ("User-Agent".into(), set_ua_entry.clone().upcast(), None),
        (
            "超时（秒，读取空闲）".into(),
            set_timeout.clone().upcast(),
            Some("连接 10s；读取空闲超时按此值"),
        ),
        ("同时任务数".into(), set_conc.clone().upcast(), None),
        (
            "重试次数".into(),
            set_retries.clone().upcast(),
            Some("每个分片，指数退避"),
        ),
        (
            "默认线程数".into(),
            set_threads.clone().upcast(),
            Some("每个任务的分块数"),
        ),
        (
            "最小分块（MB）".into(),
            set_minchunk.clone().upcast(),
            Some("小于该大小的文件不分块；0 = 不设限（按线程数分块）"),
        ),
    ];
    for (i, (label, w, hint)) in set_rows.iter().enumerate() {
        let lbl = gtk::Label::new(Some(label));
        lbl.set_xalign(1.0);
        set_grid.attach(&lbl, 0, i as i32, 1, 1);
        set_grid.attach(w, 1, i as i32, 1, 1);
        if let Some(h) = hint {
            let h_lbl = gtk::Label::new(Some(h));
            h_lbl.set_xalign(0.0);
            h_lbl.add_css_class("dim-label");
            set_grid.attach(&h_lbl, 2, i as i32, 1, 1);
        }
    }
    let set_vb = gtk::Box::new(gtk::Orientation::Vertical, 8);
    set_vb.set_margin_top(8);
    set_vb.set_margin_bottom(8);
    set_vb.set_margin_start(12);
    set_vb.set_margin_end(12);
    set_vb.append(&set_grid);
    let save_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    save_row.set_halign(gtk::Align::End);
    save_row.append(&set_save_btn);
    set_vb.append(&save_row);
    let set_card = gtk::Frame::new(Some("设置"));
    set_card.set_child(Some(&set_vb));

    // ---- 总布局 ----
    let root = gtk::Box::new(gtk::Orientation::Vertical, CARD_SPACING);
    root.append(&add_card);
    root.append(&list_card);
    root.append(&set_card);

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scroll.set_vexpand(true);
    let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root_box.set_margin_top(PAGE_MARGIN_TOP);
    root_box.set_margin_bottom(8);
    root_box.set_margin_start(8);
    root_box.set_margin_end(8);
    root_box.append(&root);
    scroll.set_child(Some(&root_box));

    // 预克隆：信号连接在 Inner 构造之后注册（需要 INNER thread_local）
    let add_btn_sig = add_btn.clone();
    let set_save_btn_sig = set_save_btn.clone();
    let dir_pick_btn_sig = dir_pick_btn.clone();

    let inner = Rc::new(Inner {
        mgr,
        url_input,
        add_note_lbl,
        dir_entry,
        filename_entry,
        threads_spin,
        add_card: add_card.clone(),
        list,
        count_lbl,
        total_speed_lbl,
        set_dir_entry,
        set_ua_entry,
        set_timeout,
        set_conc,
        set_retries,
        set_threads,
        set_minchunk,
        scroll: scroll.clone(),
        rows: RefCell::new(HashMap::new()),
        empty_lbl,
        tick_source: RefCell::new(None),
    });

    // 页面可能被重建：先停掉上一轮定时器，避免多个 tick 并发刷新
    INNER.with(|c| {
        let mut slot = c.borrow_mut();
        if let Some(old) = slot.as_ref() {
            if let Some(src) = old.tick_source.borrow_mut().take() {
                src.remove();
            }
        }
        *slot = Some(inner.clone());
    });

    // 信号连接（使用预克隆的按钮实例）
    let weak = weak_handle();
    add_btn_sig.connect_clicked(move |_| call_inner(&weak, |i| i.add_tasks()));

    {
        let weak = weak_handle();
        dir_pick_btn_sig.connect_clicked(move |_| call_inner(&weak, |i| i.pick_dir()));
    }
    {
        let weak = weak_handle();
        set_save_btn_sig.connect_clicked(move |_| call_inner(&weak, |i| i.save_config()));
    }

    // tick 渲染
    let source_id = glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
        with_inner(|i| i.g_tick());
        glib::ControlFlow::Continue
    });
    *inner.tick_source.borrow_mut() = Some(source_id);

    scroll.upcast()
}

// ---------------------------------------------------------------------------
// Inner 实现
// ---------------------------------------------------------------------------

impl Inner {
    fn add_tasks(&self) {
        let buf = self.url_input.buffer();
        let text = buf.text(&buf.start_iter(), &buf.end_iter(), false);
        let lines: Vec<&str> = text
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect();
        if lines.is_empty() {
            self.add_note_lbl.set_text("请输入下载地址");
            return;
        }
        let dir = self.dir_entry.text().to_string();
        let filename = self.filename_entry.text().to_string();
        let threads = self.threads_spin.value() as u32;
        let mgr = self.mgr.clone();
        let mut ok = 0;
        let mut errs = Vec::new();
        for line in lines {
            match mgr.add_download(
                line,
                if dir.trim().is_empty() {
                    None
                } else {
                    Some(dir.clone())
                },
                if filename.trim().is_empty() {
                    None
                } else {
                    Some(filename.clone())
                },
                Some(threads),
            ) {
                Ok(_) => ok += 1,
                Err(e) => errs.push(e),
            }
        }
        // 不自动清空输入框：删除任务后再次添加/失败重试是常态，
        // 静默清空会让用户误以为“没添加成功”，实际是输入已丢。
        if errs.is_empty() {
            self.add_note_lbl.set_text(&format!(
                "已添加 {ok} 个任务（URL 已保留，可继续添加或手动清空）"
            ));
        } else {
            self.add_note_lbl.set_text(&format!(
                "已添加 {ok} 个，失败 {} 个：{}\nURL 已保留，请修正后重试",
                errs.len(),
                errs.join("；")
            ));
        }
        // 立即刷新一次列表，新任务即刻可见（无需等 500ms tick）
        self.g_tick();
        // 任务列表在页面中段，窗口矮时可能落在视口外，把它滚进视野
        if ok > 0 {
            self.scroll_list_into_view();
        }
    }

    /// 把任务列表滚进视野。
    ///
    /// 不再用「滚到内容 60%」的估算：页面下方还有设置卡片，比例命中不了列表。
    /// 列表卡片的位置由本文件构造的布局决定（root_box 上边距 + 添加卡片高度
    /// + 卡片间距），可直接算出，稳定可靠。
    fn scroll_list_into_view(&self) {
        let v = self.scroll.vadjustment();
        let upper = v.upper();
        let page = v.page_size();
        if upper <= page || page <= 0.0 {
            return;
        }
        let list_top = (PAGE_MARGIN_TOP + self.add_card.height() + CARD_SPACING) as f64;
        let value = v.value();
        // 列表已在视野上半部就不动，避免无意义跳动
        if list_top < value || list_top > value + page * 0.7 {
            v.set_value(list_top.clamp(0.0, upper - page));
        }
    }

    fn pick_dir(&self) {
        let dialog = gtk::FileDialog::builder().title("选择下载目录").build();
        let weak = weak_handle();
        dialog.select_folder(
            None::<&gtk::Window>,
            None::<&gio::Cancellable>,
            move |res| {
                if let Ok(folder) = res {
                    if let Some(path) = folder.path() {
                        call_inner(&weak, move |i| {
                            i.dir_entry.set_text(&path.to_string_lossy());
                        });
                    }
                }
            },
        );
    }

    fn save_config(&self) {
        let mut cfg = self.mgr.config();
        cfg.dir = self.set_dir_entry.text().trim().to_string();
        cfg.user_agent = self.set_ua_entry.text().trim().to_string();
        cfg.timeout_secs = self.set_timeout.value() as u64;
        cfg.max_concurrent_tasks = self.set_conc.value() as u32;
        cfg.retries = self.set_retries.value() as u32;
        cfg.threads_per_task = self.set_threads.value() as u32;
        cfg.min_chunk_bytes = (self.set_minchunk.value() as u64) * 1024 * 1024;
        self.mgr.set_config(cfg.clone());
        match dl_settings::save(&cfg) {
            Ok(()) => self
                .add_note_lbl
                .set_text("设置已保存（作用于后续添加的任务）"),
            Err(e) => self.add_note_lbl.set_text(&format!("设置保存失败：{e}")),
        }
    }

    fn g_tick(&self) {
        let snaps = self.mgr.snapshot();
        self.sync_rows(&snaps);

        let mut total_speed = 0u64;
        let mut active = 0usize;
        for s in &snaps {
            match s.status {
                TaskStatus::Downloading | TaskStatus::Pending => {
                    total_speed += s.speed;
                    active += 1;
                }
                _ => {}
            }
        }
        self.count_lbl
            .set_text(&format!("任务数：{}（下载中 {}）", snaps.len(), active));
        self.total_speed_lbl
            .set_text(&format!("合计：{}", format_speed(total_speed)));
    }

    /// 按快照增量同步行：多则建、少则删、已有的只更新内容。
    fn sync_rows(&self, snaps: &[TaskSnapshot]) {
        // 快照来自 HashMap，迭代顺序随机；按 id（= 创建顺序）排序，
        // 保证行按固定顺序创建/显示，不会每次刷新跳位。
        let mut ordered: Vec<&TaskSnapshot> = snaps.iter().collect();
        ordered.sort_by_key(|s| s.id);
        let seen: HashSet<u64> = ordered.iter().map(|s| s.id).collect();

        // 删除消失的行：必须走 ListBox::remove（见文件头说明）
        let dead: Vec<u64> = self
            .rows
            .borrow()
            .keys()
            .copied()
            .filter(|id| !seen.contains(id))
            .collect();
        for id in dead {
            if let Some(row) = self.rows.borrow_mut().remove(&id) {
                if row.row.parent().is_some() {
                    self.list.remove(&row.row);
                }
            }
        }

        for s in ordered {
            // 先取值再分支：避免 if-let 的临时借用跨越整个匹配块
            let existing = self.rows.borrow().get(&s.id).cloned();
            match existing {
                Some(row) => Self::update_row(&row, s),
                None => {
                    let row = Rc::new(TaskRow::new());
                    self.rows.borrow_mut().insert(s.id, row.clone());
                    self.list.append(&row.row);
                    Self::update_row(&row, s);
                }
            }
        }

        let has_any = !self.rows.borrow().is_empty();
        self.empty_lbl.set_visible(!has_any);
    }

    fn update_row(row: &Rc<TaskRow>, s: &TaskSnapshot) {
        // 文件名未探明（探询失败/进行中）时用 URL 兜底显示
        let name = if s.filename.is_empty() {
            s.url.clone()
        } else {
            s.filename.clone()
        };
        row.name_lbl.set_text(&name);
        row.name_lbl.set_tooltip_text(Some(&s.url));

        let status_text = s.status.label();
        let css = match s.status {
            TaskStatus::Completed => "success",
            TaskStatus::Failed => "error",
            TaskStatus::Downloading | TaskStatus::Pending => "accent",
            _ => "",
        };
        // 先移除所有可能的状态 CSS class，避免状态切换后多个 class 叠加冲突
        row.status_lbl.remove_css_class("success");
        row.status_lbl.remove_css_class("error");
        row.status_lbl.remove_css_class("accent");
        if !css.is_empty() {
            row.status_lbl.add_css_class(css);
        }
        row.status_lbl.set_text(&status_text);

        let total = s.total_size;
        let pct = match total {
            Some(t) if t > 0 => (s.downloaded as f64 / t as f64 * 100.0).min(100.0),
            _ => 0.0,
        };
        row.progress.set_fraction(pct / 100.0);
        row.progress.set_text(Some(&format!("{pct:.1}%")));

        match total {
            Some(t) => row.size_lbl.set_text(&format!(
                "{} / {}",
                format_bytes(s.downloaded),
                format_bytes(t)
            )),
            None => row
                .size_lbl
                .set_text(&format!("已下载 {}", format_bytes(s.downloaded))),
        }

        if s.status == TaskStatus::Downloading || s.status == TaskStatus::Pending {
            let mut txt = format!("{} · {} 线程", format_speed(s.speed), s.threads);
            if let Some(t) = total {
                if t > s.downloaded {
                    let eta = format_eta(t - s.downloaded, s.speed);
                    if !eta.is_empty() {
                        txt.push_str(&format!(" · {eta}"));
                    }
                }
            }
            row.speed_lbl.set_text(&txt);
        } else if s.status == TaskStatus::Failed {
            row.speed_lbl.set_text(s.error.as_deref().unwrap_or(""));
        } else {
            row.speed_lbl.set_text("");
        }

        // 操作按钮：仅当状态组变化时重建一次（进度刷新不重建，否则按钮
        // 每 500ms 被移除重建会闪烁且点不中）
        let group: u8 = match s.status {
            TaskStatus::Downloading | TaskStatus::Pending => 1,
            TaskStatus::Paused | TaskStatus::Failed => 2,
            TaskStatus::Completed => 3,
            TaskStatus::Cancelled => 4,
        };
        if *row.btn_group.borrow() != group {
            while let Some(child) = row.btnbox.first_child() {
                row.btnbox.remove(&child);
            }
            let id = s.id;
            match group {
                1 => {
                    Self::row_btn(&row.btnbox, "media-playback-pause-symbolic", "暂停", move |i| {
                        i.mgr.pause(id)
                    });
                    Self::row_btn(&row.btnbox, "edit-delete-symbolic", "删除", move |i| {
                        i.mgr.cancel_and_remove(id)
                    });
                }
                2 => {
                    Self::row_btn(
                        &row.btnbox,
                        "media-playback-start-symbolic",
                        "继续（断点续传）",
                        move |i| i.mgr.resume(id),
                    );
                    Self::row_btn(&row.btnbox, "edit-delete-symbolic", "删除", move |i| {
                        i.mgr.cancel_and_remove(id)
                    });
                }
                3 => {
                    let dir = s.dir.clone();
                    let file = s.filename.clone();
                    Self::row_btn(
                        &row.btnbox,
                        "folder-open-symbolic",
                        "打开所在目录",
                        move |_i| Self::open_dir(&dir, &file),
                    );
                    Self::row_btn(&row.btnbox, "edit-delete-symbolic", "移除记录", move |i| {
                        i.mgr.remove(id)
                    });
                }
                4 => {
                    Self::row_btn(&row.btnbox, "edit-delete-symbolic", "移除记录", move |i| {
                        i.mgr.remove(id)
                    });
                }
                _ => {}
            }
            *row.btn_group.borrow_mut() = group;
        }
    }

    fn row_btn<F>(box_: &gtk::Box, icon: &str, tooltip: &str, handler: F)
    where
        F: Fn(&Inner) + 'static,
    {
        let btn = gtk::Button::new();
        btn.set_icon_name(icon);
        btn.set_tooltip_text(Some(tooltip));
        btn.set_has_frame(false);
        let weak = weak_handle();
        btn.connect_clicked(move |_| call_inner(&weak, |i| handler(i)));
        box_.append(&btn);
    }

    /// 打开「文件所在文件夹」（按钮文案即「打开所在目录」）。
    ///
    /// 用 `gio::File::for_path().uri()` 取得**正确百分号编码**的 URI：
    /// 若手工 `format!("file://{}", path)` 拼接，路径含中文/空格/`#` 时会产出
    /// 非法 URI，`launch_default_for_uri` 解析失败并直接返回 `Err`，而原来的
    /// `let _ =` 把错误吞掉，导致点击「毫无反应」。下载文件名常带中文/空格，
    /// 这正是最初点击无反应的根因。
    ///
    /// 始终打开目录本身（而非文件），与按钮语义一致。
    fn open_dir(dir: &str, filename: &str) {
        let dir_path = std::path::PathBuf::from(dir);
        // 优先定位到文件所在目录；文件不存在则退回任务目录本身。
        let target = if !filename.is_empty() {
            let file = dir_path.join(filename);
            file.parent().map(|p| p.to_path_buf()).unwrap_or(dir_path)
        } else {
            dir_path
        };
        let gfile = gio::File::for_path(&target);
        let uri = gfile.uri();
        if let Err(e) = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
            eprintln!("打开下载目录失败（{}）：{e}", target.display());
        }
    }
}

pub fn shutdown() {
    INNER.with(|c| {
        let mut inner = c.borrow_mut();
        // 取消定时刷新 timer，避免空转
        if let Some(ref inner) = *inner {
            if let Some(src) = inner.tick_source.borrow_mut().take() {
                src.remove();
            }
        }
        *inner = None;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::download::TaskStatus;

    fn snap(id: u64, status: TaskStatus) -> TaskSnapshot {
        TaskSnapshot {
            id,
            url: format!("http://example.com/f{id}.bin"),
            dir: "/tmp".into(),
            filename: format!("f{id}.bin"),
            status,
            total_size: Some(1024),
            downloaded: 512,
            speed: 0,
            threads: 4,
            error: None,
            created_at: 0,
        }
    }

    fn row_count(list: &gtk::ListBox) -> i32 {
        let mut i = 0;
        while list.row_at_index(i).is_some() {
            i += 1;
        }
        i
    }

    /// 面板行的增删必须与 ListBox 内部行索引一致。
    ///
    /// 回归防护：曾经用 `Widget::unparent()` 删除隐式生成的 ListBoxRow，
    /// 会导致 ListBox 内部状态错乱（行数不减、指针失效），后续追加行时
    /// 触发 `instance_of::<ListBoxRow>` 断言崩溃，且新行渲染不出来。
    #[test]
    fn row_add_remove_keeps_listbox_consistent() {
        if gtk::init().is_err() {
            // 无显示环境（CI/无头）时跳过，本用例只在有 GTK 时有效
            return;
        }
        let _page = build();
        with_inner(|i| {
            // 三个任务 → 三行
            i.sync_rows(&[
                snap(1, TaskStatus::Downloading),
                snap(2, TaskStatus::Pending),
                snap(3, TaskStatus::Completed),
            ]);
            assert_eq!(i.rows.borrow().len(), 3);
            assert_eq!(row_count(&i.list), 3, "ListBox 行数与任务数不符");

            // 删掉中间一个 → 两行，且剩余行仍可用
            i.sync_rows(&[snap(1, TaskStatus::Downloading), snap(3, TaskStatus::Completed)]);
            assert_eq!(i.rows.borrow().len(), 2);
            assert_eq!(row_count(&i.list), 2, "删除后 ListBox 行数未同步");
            assert!(i.list.row_at_index(0).is_some());
            assert!(i.list.row_at_index(1).is_some());

            // 再新增一个 → 三行（旧 BUG：这里会崩溃/不渲染）
            i.sync_rows(&[
                snap(1, TaskStatus::Downloading),
                snap(3, TaskStatus::Completed),
                snap(4, TaskStatus::Pending),
            ]);
            assert_eq!(row_count(&i.list), 3, "重新添加后 ListBox 行数不符");
            // 每行都还“活着”且是 ListBoxRow（旧 BUG：此处断言失败）
            for idx in 0..3 {
                let r = i.list.row_at_index(idx).expect("行丢失");
                assert!(r.is_visible() || !r.is_visible());
                assert!(r.child().is_some());
            }

            // 全部删除 → 空列表提示显现
            i.sync_rows(&[]);
            assert_eq!(row_count(&i.list), 0);
            assert!(i.empty_lbl.is_visible());

            // 清空后再添加，仍要正常显示
            i.sync_rows(&[snap(5, TaskStatus::Downloading)]);
            assert_eq!(row_count(&i.list), 1);
            assert!(!i.empty_lbl.is_visible());
        });
        shutdown();
    }
}
