//! 磁盘挂载页面（`/etc/fstab` 管理 · KDE Partition Manager 标准三分区布局，GOAL Phase 7 · D7.8）。
//!
//! 布局（对照 KDE PM 主窗口六件套，本页实现其中四件；含右键菜单、双击属性、
//! 编辑挂载点对话框与一键生成条目，均为 7.9）：
//! - **左侧设备面板**：`lsblk -J -b` 盘 → 分区两级树，只读；
//! - **中部图形分区条**：选中盘的各分区按字节占比分段，fstype 着色、
//!   底部深色叠加已用量（`df`），点击分段选中；下方**树视图**列
//!   设备/大小/FSTYPE/LABEL/UUID/挂载点/状态/fstab 行号，与分区条双向联动；
//! - **右侧信息面板**：选中分区全部字段自动读取（UUID/类型/标签**零手输**，D7.8）；
//! - **待定操作队列**：新增/编辑/停用/删除只入队（[`OpQueue`]），主视图与 fstab 表
//!   按 `preview()` 显示「应用后」徽章预览，点 **Apply** 才走 7.4→7.5
//!   校验写入链路，失败整队保留，撤销零成本。
//!
//! 解析、校验、写入、命令构造、队列 reducer 全部在 `utils::fstab`，
//! 本文件只做 UI 与线程编排。
//!
//! 线程模型（与 `inotify_tune` 相同）：子线程只搬运 `Send` 纯数据，
//! 不捕获 `Rc<Inner>`；所有外部命令调用点（`read_fstab` / `findmnt` /
//! `lsblk` / `df` / `validate_text` / `write_fstab` / `mount` / `umount` /
//! `mkdir`）都在 `std::thread::spawn` 内，结果经 `glib::source::idle_add` +
//! 全局 `with_inner` 回主线程刷新 UI。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use glib::clone;
use gtk::cairo::{FontSlant, FontWeight};
use gtk::gdk;

use crate::model::fstab::{FstabDocument, FstabEntry, MountStatus, escape_field};
use crate::utils::fstab::{
    self as ftab, BlockPartition, LsblkDisk, LsblkPart, OpQueue, PendingOp, WriteOutcome,
};

pub struct FstabPage {
    root: adw::ToastOverlay,
}

impl FstabPage {
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
// 选中态与小工具
// ---------------------------------------------------------------------------

/// 全局选中目标：选磁盘（分区条高亮整盘）或选分区（面板/分区条/树三方联动）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sel {
    Disk(usize),
    Part(usize, usize),
}

/// 树视图列（固定宽 + ellipsize；超宽时树内横向滚动，见 build）。
const TREE_COLS: [(&str, i32); 8] = [
    ("设备", 110),
    ("大小", 58),
    ("FSTYPE", 48),
    ("LABEL", 60),
    ("UUID", 110),
    ("挂载点", 84),
    ("状态", 70),
    ("fstab", 44),
];

/// 构造卡片容器（Phase 6 margin 收敛约定：页面根一层 12px，卡片只留内边距，
/// 水平方向不再叠加 inset）。返回 (外层卡片, 内容盒)。
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

/// 生成一个带样式类的小标签「药丸」。
fn pill(text: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("caption");
    l.add_css_class(class);
    l
}

/// 挂载状态 → (文案, 颜色类)。未读到 findmnt（None）由调用方处理。
fn status_meta(st: MountStatus) -> (&'static str, &'static str) {
    match st {
        MountStatus::Mounted => ("已挂载", "success"),
        MountStatus::DirMissing => ("挂载点不存在", "error"),
        MountStatus::SourceMissing => ("源不存在", "error"),
        MountStatus::NotMounted => ("未挂载", "dim-label"),
        MountStatus::Disabled => ("已停用", "dim-label"),
    }
}

/// 勾选窗底部实时预览文案（D7.9：所见即所得）。
fn options_preview(st: &ftab::OptionsState) -> String {
    let mut s = format!("将写入：{}", ftab::serialize_options(st));
    if st.is_checked("remount") {
        s.push_str("（remount 为运行期动作，不写入 fstab）");
    }
    s
}

/// 属性对话框的「键—值」行：键定宽、值可换行可选中。
fn kv_row(k: &str, v: &str) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.set_margin_top(3);
    row.set_margin_bottom(3);
    let key = gtk::Label::new(Some(k));
    key.add_css_class("dim-label");
    key.set_xalign(0.0);
    key.set_size_request(88, -1);
    let val = gtk::Label::new(Some(v));
    val.set_xalign(0.0);
    val.set_hexpand(true);
    val.set_wrap(true);
    val.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    val.set_selectable(true);
    row.append(&key);
    row.append(&val);
    row
}

/// 右键菜单项（GOAL 7.9）；label 为 "-" 时画分隔线。
struct MenuEntry {
    label: String,
    destructive: bool,
    action: Box<dyn Fn()>,
}

impl MenuEntry {
    fn new(label: &str, action: impl Fn() + 'static) -> Self {
        Self {
            label: label.to_string(),
            destructive: false,
            action: Box::new(action),
        }
    }

    fn destructive(label: &str, action: impl Fn() + 'static) -> Self {
        Self {
            label: label.to_string(),
            destructive: true,
            action: Box::new(action),
        }
    }

    fn sep() -> Self {
        Self {
            label: "-".to_string(),
            destructive: false,
            action: Box::new(|| {}),
        }
    }
}

/// 一键生成的挂载点名：把 label/分区名压成合法路径段（空白与 `/` → `-`）。
fn sanitize_mountpoint_name(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| {
            if c == '/' || c.is_whitespace() {
                '-'
            } else {
                c
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "data".to_string() } else { s }
}

/// 行交互（GOAL 7.9）：双击 → 属性对话框；右键 → 上下文菜单。
/// 手势只观察不 claim：不影响列表选中，也不抢子按钮的点击。
fn attach_row_actions(
    row: &gtk::ListBoxRow,
    on_double: impl Fn() + 'static,
    on_right: impl Fn(gtk::Widget, f64, f64) + 'static,
) {
    let dbl = gtk::GestureClick::new();
    dbl.set_button(1);
    dbl.connect_pressed(move |_g, n, _x, _y| {
        if n == 2 {
            on_double();
        }
    });
    row.add_controller(dbl);

    let right = gtk::GestureClick::new();
    right.set_button(3);
    right.connect_pressed(move |g, _n, x, y| {
        if let Some(w) = g.widget() {
            on_right(w, x, y);
        }
    });
    row.add_controller(right);
}

/// 文本输入 → fstab 文件形态（字段内空白转义），与展示层 `unescape_field` 互逆。
fn field_of(input: &str) -> String {
    escape_field(input.trim())
}

/// 表单读出的一个待写入条目（按当前编辑态构造）。
fn form_entry(
    source: &str,
    mountpoint: &str,
    vfstype: &str,
    options: &str,
    dump: f64,
    pass: f64,
) -> FstabEntry {
    let mut e = FstabEntry::new(
        field_of(source),
        field_of(mountpoint),
        vfstype.trim().to_string(),
        field_of(options),
    );
    e.set_dump(format!("{dump:.0}"));
    e.set_pass(format!("{pass:.0}"));
    e
}

/// fstype → 分区条底色 (r, g, b)。未识别/未格式化走中性灰蓝。
fn fs_color(fs: Option<&str>) -> (f64, f64, f64) {
    match fs.unwrap_or("") {
        "ext4" | "ext3" | "ext2" => (0.30, 0.55, 0.85),
        "btrfs" => (0.55, 0.42, 0.85),
        "xfs" => (0.85, 0.55, 0.25),
        "vfat" | "exfat" | "fat32" => (0.25, 0.70, 0.60),
        "ntfs" | "ntfs3" => (0.30, 0.62, 0.78),
        "swap" => (0.62, 0.42, 0.42),
        "iso9660" | "squashfs" => (0.55, 0.55, 0.55),
        "" => (0.45, 0.50, 0.58),
        _ => (0.40, 0.62, 0.55),
    }
}

/// 树视图单元格（定宽 + 左对齐 + ellipsize）。
fn tree_cell(text: &str, w: i32) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_size_request(w, -1);
    l.set_ellipsize(gtk::pango::EllipsizeMode::End);
    l.set_xalign(0.0);
    l
}

/// 在预览文档里找与分区关联的 fstab 条目：UUID= 精确匹配 → 设备路径 →
/// 挂载点（规范化后）匹配。返回 (条目, 该行状态)。
fn link_entry<'a>(
    doc: &'a FstabDocument,
    p: &LsblkPart,
    statuses: &[(usize, MountStatus)],
) -> Option<(&'a FstabEntry, MountStatus)> {
    let entry = doc.entries().find(|e| {
        let src = ftab::unescape_field(&e.source);
        if p.uuid
            .as_deref()
            .is_some_and(|u| src == format!("UUID={u}"))
        {
            return true;
        }
        if !p.path.is_empty() && src == p.path {
            return true;
        }
        p.mountpoints.iter().any(|mp| {
            ftab::normalize_mountpoint(&ftab::unescape_field(&e.mountpoint))
                == ftab::normalize_mountpoint(mp)
        })
    })?;
    let st = statuses
        .iter()
        .find(|(ln, _)| *ln == entry.line_no)
        .map(|(_, s)| *s)
        // findmnt 数据缺失/未收录该行时按条目与 lsblk 挂载点回退，
        // 保证关联行号与状态列不因 json 缺席而消失。
        .unwrap_or(if entry.disabled {
            MountStatus::Disabled
        } else if p.mountpoints.is_empty() {
            MountStatus::NotMounted
        } else {
            MountStatus::Mounted
        });
    Some((entry, st))
}

// ---------------------------------------------------------------------------
// 后台任务结果（Send 纯数据，子线程只搬这些）
// ---------------------------------------------------------------------------

/// 一次性加载 fstab + findmnt + lsblk 树 + df 的结果。
struct LoadResult {
    doc: FstabDocument,
    /// 磁盘上的原文（写入前内容，回滚凭据）。
    text: String,
    /// `findmnt --json` 输出；读取失败为 None（状态列显示「未知」）。
    json: Option<String>,
    /// 当前进程能否直写 /etc/fstab。
    writable: bool,
    /// lsblk 盘→分区树；失败为 None（沿用旧数据）。
    disks: Option<Vec<LsblkDisk>>,
    /// df 用量行 `(挂载点, 总字节, 已用字节)`；失败为 None。
    usage: Option<Vec<(String, u64, u64)>>,
    /// lsblk 读取失败的错误信息（7.10 错误态）；成功为 None。
    lsblk_err: Option<String>,
}

/// 保存链路（校验 → 写入）的结果。
enum SaveStep {
    /// 自家解析 / findmnt --verify 拒绝：细节行首条 + 计数进 toast。
    Invalid(Vec<String>),
    /// 备份 / 落盘 / 回读 / reload 任一步失败（已回滚）。
    WriteFailed(String),
    /// 写入成功。
    Done {
        warnings: Vec<String>,
        outcome: WriteOutcome,
    },
}

/// 外部命令执行结果：Ok = 输出摘要（可能为空），Err = 失败原因。
type CmdResult = Result<String, String>;

// ---------------------------------------------------------------------------
// Inner
// ---------------------------------------------------------------------------

struct Inner {
    toast_overlay: adw::ToastOverlay,

    // 操作条
    refresh_btn: gtk::Button,
    add_btn: gtk::Button,
    rollback_btn: gtk::Button,
    save_btn: gtk::Button,
    op_status: gtk::Label,
    spinner: gtk::Spinner,

    // 左：设备面板 + 队列面板
    dev_list: gtk::ListBox,
    /// 设备面板行号 → 选择目标（磁盘行/分区行交错）。
    dev_map: RefCell<Vec<Sel>>,
    queue_list: gtk::ListBox,
    queue_status: gtk::Label,
    undo_btn: gtk::Button,
    clear_btn: gtk::Button,

    // 中：分区条 + 树视图 + fstab 列表 + 表单
    bar_area: gtk::DrawingArea,
    bar_caption: gtk::Label,
    tree_list: gtk::ListBox,
    /// 树视图行号 → (盘, 分区)；第 0 行是表头，为 None。
    tree_map: RefCell<Vec<Option<(usize, usize)>>>,
    list: gtk::ListBox,
    list_status: gtk::Label,
    /// fstab 条目列表卡片（空态时隐藏）。
    list_card: gtk::Box,
    /// 空态滚动壳（Overlay 里整块显隐；内含 StatusPage + 主操作）。
    empty_sw: gtk::ScrolledWindow,
    form_revealer: gtk::Revealer,
    form_title: gtk::Label,
    part_combo: adw::ComboRow,
    f_source: adw::EntryRow,
    f_mount: adw::EntryRow,
    f_type: adw::ComboRow,
    f_opts: adw::ActionRow,
    f_dump: adw::SpinRow,
    f_pass: adw::SpinRow,
    submit_btn: gtk::Button,

    // 右：信息面板
    info_box: gtk::Box,

    // 错误态与状态栏（7.10）
    /// 三分栏主区（fstab 致命错误时整块隐藏）。
    main_box: gtk::Box,
    /// fstab 读失败/不存在 的整页错误态（UI 规范 §8）。
    fatal_sp: adw::StatusPage,
    /// 设备列表滚动区（lsblk 失败时隐藏）。
    dev_scroll: gtk::ScrolledWindow,
    /// 设备卡片（lsblk 失败时抬最小高，给错误态 StatusPage 腾空间）。
    dev_card: gtk::Box,
    /// lsblk 错误态滚动壳（整块显隐）。
    dev_err_sw: gtk::ScrolledWindow,
    /// lsblk 读取失败错误态。
    dev_err_sp: adw::StatusPage,
    /// 状态栏：待定操作计数 / 读写状态（直写/提权）。
    sb_count: gtk::Label,
    sb_mode: gtk::Label,

    // 状态
    doc: RefCell<FstabDocument>,
    /// 上次 `findmnt --json`；状态列与 status_table 复算的数据源。
    json: RefCell<Option<String>>,
    /// 磁盘当前原文（loaded）；回滚目标 = 上一次成功写入前的这份。
    loaded_text: RefCell<String>,
    /// 写入前内容（成功写入一次后非空，回滚按钮凭据）。
    prev_content: RefCell<Option<String>>,
    disks: RefCell<Vec<LsblkDisk>>,
    usage: RefCell<Vec<(String, u64, u64)>>,
    /// 表单分区下拉的数据源（on_part_selected 按下标回填来源/类型）。
    parts: RefCell<Vec<BlockPartition>>,
    /// 待定操作队列（GOAL 7.8）：一切 fstab 变更先入队，Apply 统一落盘。
    queue: RefCell<OpQueue>,
    /// 当前选中（面板/分区条/树三方联动的唯一真源）。
    sel: Cell<Option<Sel>>,
    /// 程序化选中标志：抑制 row_selected 回环。
    syncing: Cell<bool>,
    /// 正在编辑的既有条目行号；None = 新增模式。
    editing: Cell<Option<usize>>,
    busy: Cell<bool>,
    writable: Cell<bool>,
}

impl Inner {
    fn toast(&self, msg: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(msg));
    }

    fn window(&self) -> Option<gtk::Window> {
        self.toast_overlay.root()?.downcast().ok()
    }

    /// 占位忙碌锁：已有操作在跑则拒绝并提示。
    fn enter_busy(&self) -> bool {
        if self.busy.get() {
            self.toast("上一个操作还在进行中，请稍候");
            return false;
        }
        self.busy.set(true);
        self.update_busy();
        true
    }

    fn update_busy(&self) {
        let busy = self.busy.get();
        let n = self.queue.borrow().len();
        if busy {
            self.spinner.start();
        } else {
            self.spinner.stop();
        }
        self.spinner.set_visible(busy);
        self.save_btn.set_sensitive(!busy && n > 0);
        self.add_btn.set_sensitive(!busy);
        self.refresh_btn.set_sensitive(!busy);
        self.undo_btn.set_sensitive(!busy && n > 0);
        self.clear_btn.set_sensitive(!busy && n > 0);
        self.rollback_btn
            .set_sensitive(!busy && self.prev_content.borrow().is_some());
        let text = if busy {
            "正在执行…".to_string()
        } else if n > 0 {
            format!("{n} 个待定操作（未写入磁盘，点 Apply 生效）")
        } else if self.writable.get() {
            "与磁盘一致（可直接写入）".to_string()
        } else {
            "与磁盘一致（写入将 pkexec 提权）".to_string()
        };
        self.op_status.set_text(&text);
        // 状态栏（7.10）：N 个待定操作 + 读写状态（直写/需 pkexec）
        let sbq = if n == 0 {
            "无待定操作".to_string()
        } else {
            format!("{n} 个待定操作")
        };
        self.sb_count.set_text(&sbq);
        let sbm = if busy {
            "执行中…".to_string()
        } else if self.writable.get() {
            "读写状态：直写（无需提权）".to_string()
        } else {
            "读写状态：写入需 pkexec 提权".to_string()
        };
        self.sb_mode.set_text(&sbm);
    }

    /// 当前「应用后」文档（队列 preview）；队列失效时退回 base（调用方负责报错态）。
    fn preview_doc(&self) -> FstabDocument {
        let base = self.doc.borrow();
        match self.queue.borrow().preview(&base) {
            Ok(p) => p,
            Err(_) => base.clone(),
        }
    }

    /// 当前选中对应的盘下标（无选中回退 0）。
    fn sel_disk_idx(&self) -> usize {
        match self.sel.get() {
            Some(Sel::Disk(d)) | Some(Sel::Part(d, _)) => d,
            None => 0,
        }
    }

    // ------------------------------------------------------------------ 入队

    /// 校验并入队：克隆队列 → push → preview 全量校验 → 通过才提交。
    /// 任一操作（含被合并的历史操作）校验失败 → 队列保持原样（GOAL 7.8）。
    fn enqueue(&self, op: PendingOp) -> Result<(), String> {
        let mut cand = self.queue.borrow().clone();
        cand.push(op);
        let res = {
            let base = self.doc.borrow();
            cand.preview(&base)
        };
        match res {
            Ok(_) => {
                *self.queue.borrow_mut() = cand;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// 编辑「待定新增」条目：就地替换产生它的那条 Add（而不是追加依赖它的
    /// Edit，避免撤销 Add 后留下孤儿 Edit）。归属判定走 `add_index_producing`
    /// 的前缀模拟——`Remove` 入队后行数关系不再成立，算术反推会失效。
    fn replace_pending_add(&self, line_no: usize, entry: FstabEntry) -> Result<(), String> {
        let base = self.doc.borrow().clone();
        let mut cand = self.queue.borrow().clone();
        let Some(idx) = cand.add_index_producing(&base, line_no) else {
            return self.enqueue(PendingOp::Edit { line_no, entry });
        };
        cand.replace_at(idx, PendingOp::Add(entry))?;
        let res = cand.preview(&base);
        match res {
            Ok(_) => {
                *self.queue.borrow_mut() = cand;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// 队列变更成功后的统一刷新：主视图/树/列表/队列面板/信息面板 + 联动恢复。
    fn refresh_views(&self) {
        self.rebuild_list();
        self.rebuild_tree();
        self.rebuild_queue();
        self.update_busy();
        // 树重建会清掉行选中，按当前 sel 恢复三方联动高亮（内含信息面板重建）。
        self.sync_sel(self.sel.get());
    }

    /// 单条撤销（队列面板每行 ✕ / Ctrl+Z 后调用前先取 describe）。
    fn remove_op(&self, idx: usize) {
        if self.busy.get() {
            self.toast("上一个操作还在进行中，请稍候");
            return;
        }
        if let Some(op) = self.queue.borrow_mut().remove_at(idx) {
            self.toast(&format!("已撤销：{}", op.describe()));
            self.refresh_views();
        }
    }

    /// 撤销最后一条（Ctrl+Z / 队列头「撤销」）。
    fn undo_last(&self) {
        if self.busy.get() {
            self.toast("上一个操作还在进行中，请稍候");
            return;
        }
        match self.queue.borrow_mut().undo() {
            Some(op) => {
                self.toast(&format!("已撤销：{}", op.describe()));
                self.refresh_views();
            }
            None => self.toast("队列为空，没有可撤销的操作"),
        }
    }

    fn clear_queue(&self) {
        if self.busy.get() {
            self.toast("上一个操作还在进行中，请稍候");
            return;
        }
        let n = self.queue.borrow().len();
        if n == 0 {
            self.toast("队列为空");
            return;
        }
        self.queue.borrow_mut().clear();
        self.toast(&format!("已清空 {n} 个待定操作"));
        self.refresh_views();
    }

    // ------------------------------------------------------------------ 加载

    /// 刷新前的丢弃确认（手动刷新且队列非空时弹一次）。
    fn confirm_refresh(&self) {
        let n = self.queue.borrow().len();
        if n == 0 {
            self.refresh();
            return;
        }
        let Some(win) = self.window() else {
            return;
        };
        let dialog = adw::MessageDialog::new(
            Some(&win),
            Some("刷新将丢弃待定操作？"),
            Some(&format!(
                "当前有 {n} 个待定操作尚未写入。重新读取会以磁盘现状重建列表，\
                 这些操作将被丢弃（对磁盘本就零影响）。"
            )),
        );
        dialog.add_response("cancel", "取消");
        dialog.add_response("ok", "丢弃并刷新");
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, move |d: &adw::MessageDialog, resp| {
            d.close();
            if resp == "ok" {
                with_inner(|i| {
                    i.queue.borrow_mut().clear();
                    i.refresh();
                });
            }
        });
        dialog.present();
    }

    /// 后台重新读取 fstab + findmnt + lsblk 树 + df（外部命令全在线程内）。
    fn refresh(&self) {
        if !self.enter_busy() {
            return;
        }
        std::thread::spawn(move || {
            let res: Result<LoadResult, String> = (|| {
                let (text, writable) = ftab::read_fstab()?;
                let text = text.unwrap_or_default();
                let doc = ftab::parse(&text)?;
                let json = ftab::run_findmnt_json().ok();
                let (disks, lsblk_err) = match ftab::run_lsblk_tree_json() {
                    Ok(j) => match ftab::parse_lsblk_tree(&j) {
                        Ok(d) => (Some(d), None),
                        Err(e) => (None, Some(e)),
                    },
                    Err(e) => (None, Some(e)),
                };
                let usage = ftab::run_df_usage().ok().map(|t| ftab::parse_df_usage(&t));
                Ok(LoadResult {
                    doc,
                    text,
                    json,
                    writable,
                    disks,
                    usage,
                    lsblk_err,
                })
            })();
            let res = RefCell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(r) = res.take() {
                    with_inner(|i| i.apply_load(r));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_load(&self, res: Result<LoadResult, String>) {
        self.busy.set(false);
        match res {
            Err(e) => {
                self.show_fatal(&e);
                self.update_busy();
            }
            Ok(r) => {
                // env_editor 对 NotFound 返回 Ok(None)：fstab 不存在归入错误态（7.10），不误显示空态
                if r.text.is_empty() && !std::path::Path::new("/etc/fstab").exists() {
                    self.show_fatal("文件不存在");
                    self.update_busy();
                    return;
                }
                self.clear_fatal();
                self.set_lsblk_error(r.lsblk_err);
                if let Some(disks) = r.disks {
                    *self.disks.borrow_mut() = disks;
                }
                if let Some(usage) = r.usage {
                    *self.usage.borrow_mut() = usage;
                }
                let parts: Vec<BlockPartition> = self
                    .disks
                    .borrow()
                    .iter()
                    .flat_map(|d| d.parts.iter())
                    .map(|p| BlockPartition {
                        path: p.path.clone(),
                        name: p.name.clone(),
                        fstype: p.fstype.clone(),
                        size: ftab::human_bytes(p.size_bytes),
                        label: p.label.clone(),
                        uuid: p.uuid.clone(),
                        mounted: !p.mountpoints.is_empty(),
                    })
                    .collect();
                self.set_partitions(parts);
                *self.doc.borrow_mut() = r.doc;
                *self.json.borrow_mut() = r.json;
                *self.loaded_text.borrow_mut() = r.text;
                self.writable.set(r.writable);
                // 选中保底：越界回退首盘首分区（默认展示 UUID 供截图核对）。
                let disks = self.disks.borrow();
                let n = disks.len();
                let cur = self.sel.get();
                let fixed = match cur {
                    Some(Sel::Disk(d)) if d >= n => None,
                    Some(Sel::Part(d, p)) => match disks.get(d) {
                        Some(disk) if p < disk.parts.len() => Some(Sel::Part(d, p)),
                        _ => None,
                    },
                    other => other,
                };
                let fallback = if n > 0 && disks[0].parts.is_empty() {
                    Some(Sel::Disk(0))
                } else if n > 0 {
                    Some(Sel::Part(0, 0))
                } else {
                    None
                };
                self.sel.set(fixed.or(fallback));
                drop(disks);
                self.rebuild_devices();
                self.refresh_views();
            }
        }
    }

    /// 后台只重读 findmnt（挂载/卸载/建目录操作后刷新状态列）。
    fn refresh_statuses(&self) {
        if !self.enter_busy() {
            return;
        }
        std::thread::spawn(move || {
            let res = ftab::run_findmnt_json();
            let res = RefCell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(r) = res.take() {
                    with_inner(|i| i.apply_statuses(r));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_statuses(&self, res: Result<String, String>) {
        self.busy.set(false);
        match res {
            Ok(json) => *self.json.borrow_mut() = Some(json),
            Err(e) => self.toast(&format!("读取挂载状态失败：{e}")),
        }
        self.refresh_views();
    }

    fn set_partitions(&self, parts: Vec<BlockPartition>) {
        let mut labels = vec!["（手动输入来源）".to_string()];
        for p in &parts {
            let fs = p.fstype.as_deref().unwrap_or("未格式化");
            let label = p
                .label
                .as_deref()
                .map(|l| format!(" · {l}"))
                .unwrap_or_default();
            labels.push(format!("{} · {fs} · {}{label}", p.name, p.size));
        }
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        self.part_combo
            .set_model(Some(&gtk::StringList::new(&refs)));
        *self.parts.borrow_mut() = parts;
    }

    /// 分区下拉选中：把 UUID（无 UUID 用设备路径）与 fstype 填进表单。
    fn on_part_selected(&self, idx: u32) {
        if idx == 0 {
            return;
        }
        let parts = self.parts.borrow();
        let Some(p) = parts.get(idx as usize - 1) else {
            return;
        };
        let source = match &p.uuid {
            Some(u) => format!("UUID={u}"),
            None => p.path.clone(),
        };
        self.f_source.set_text(&source);
        if let Some(fs) = &p.fstype {
            self.select_fstype(fs);
        }
    }

    // ------------------------------------------------------------------ 联动

    /// 三方联动选中：设 sel → 同步面板/树行选中（syncing 抑制回环）→
    /// 重画分区条 → 重建信息面板。
    fn sync_sel(&self, new: Option<Sel>) {
        self.sel.set(new);
        self.syncing.set(true);
        let dev_idx = new.and_then(|s| self.dev_map.borrow().iter().position(|&m| m == s));
        match dev_idx {
            Some(i) => {
                if let Some(r) = self.dev_list.row_at_index(i as i32) {
                    self.dev_list.select_row(Some(&r));
                }
            }
            None => self.dev_list.select_row(None::<&gtk::ListBoxRow>),
        }
        let tree_idx = match new {
            Some(Sel::Part(d, p)) => self
                .tree_map
                .borrow()
                .iter()
                .position(|m| *m == Some((d, p))),
            _ => None,
        };
        match tree_idx {
            Some(i) => {
                if let Some(r) = self.tree_list.row_at_index(i as i32) {
                    self.tree_list.select_row(Some(&r));
                }
            }
            None => self.tree_list.select_row(None::<&gtk::ListBoxRow>),
        }
        self.syncing.set(false);
        self.bar_area.queue_draw();
        self.update_bar_caption();
        self.rebuild_info();
    }

    fn update_bar_caption(&self) {
        let disks = self.disks.borrow();
        let text = match disks.get(self.sel_disk_idx()) {
            None => "分区条 · 无块设备数据（lsblk 读取失败或无磁盘）".to_string(),
            Some(d) => format!(
                "分区条 · {} · {} · {} 个分段 · 点击分段选中分区",
                d.name,
                ftab::human_bytes(d.size_bytes),
                d.parts.len()
            ),
        };
        self.bar_caption.set_text(&text);
    }

    // ------------------------------------------------------------------ 设备面板

    fn rebuild_devices(&self) {
        while let Some(child) = self.dev_list.first_child() {
            self.dev_list.remove(&child);
        }
        let mut map = Vec::new();
        let disks = self.disks.borrow();
        if disks.is_empty() {
            let row = gtk::ListBoxRow::new();
            row.set_selectable(false);
            let l = gtk::Label::new(Some("未读到块设备数据"));
            l.add_css_class("dim-label");
            l.add_css_class("caption");
            l.set_margin_top(8);
            l.set_margin_bottom(8);
            l.set_margin_start(10);
            row.set_child(Some(&l));
            self.dev_list.append(&row);
        }
        for (di, d) in disks.iter().enumerate() {
            let row = gtk::ListBoxRow::new();
            let h = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            h.set_margin_top(6);
            h.set_margin_bottom(6);
            h.set_margin_start(10);
            h.set_margin_end(10);
            let name = gtk::Label::new(Some(&format!("■ {}", d.name)));
            name.add_css_class("title-4");
            name.set_xalign(0.0);
            name.set_hexpand(true);
            name.set_ellipsize(gtk::pango::EllipsizeMode::End);
            h.append(&name);
            let size = gtk::Label::new(Some(&ftab::human_bytes(d.size_bytes)));
            size.add_css_class("dim-label");
            size.add_css_class("caption");
            h.append(&size);
            row.set_child(Some(&h));
            self.dev_list.append(&row);
            map.push(Sel::Disk(di));
            attach_row_actions(
                &row,
                move || with_inner(|i| i.show_dev_props(Sel::Disk(di))),
                move |w, x, y| with_inner(|i| i.show_part_menu(Sel::Disk(di), &w, x, y)),
            );

            for (pi, p) in d.parts.iter().enumerate() {
                let row = gtk::ListBoxRow::new();
                let h = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                h.set_margin_top(3);
                h.set_margin_bottom(3);
                h.set_margin_start(26);
                h.set_margin_end(10);
                let mut sub = format!(
                    "{} · {} · {}",
                    p.name,
                    p.fstype.as_deref().unwrap_or("未格式化"),
                    ftab::human_bytes(p.size_bytes)
                );
                if let Some(mp) = p.mountpoints.first() {
                    sub.push_str(&format!(" · {mp}"));
                }
                let l = gtk::Label::new(Some(&sub));
                l.set_xalign(0.0);
                l.set_ellipsize(gtk::pango::EllipsizeMode::End);
                if p.mountpoints.is_empty() {
                    l.add_css_class("dim-label");
                }
                h.append(&l);
                row.set_child(Some(&h));
                self.dev_list.append(&row);
                map.push(Sel::Part(di, pi));
                attach_row_actions(
                    &row,
                    move || with_inner(|i| i.show_dev_props(Sel::Part(di, pi))),
                    move |w, x, y| with_inner(|i| i.show_part_menu(Sel::Part(di, pi), &w, x, y)),
                );
            }
        }
        drop(disks);
        *self.dev_map.borrow_mut() = map;
    }

    // ------------------------------------------------------------------ 树视图

    fn rebuild_tree(&self) {
        while let Some(child) = self.tree_list.first_child() {
            self.tree_list.remove(&child);
        }
        let mut map: Vec<Option<(usize, usize)>> = vec![None];

        // 表头（不可选）
        let head = gtk::ListBoxRow::new();
        head.set_selectable(false);
        let hb = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        hb.set_margin_top(4);
        hb.set_margin_bottom(4);
        hb.set_margin_start(12);
        hb.set_margin_end(12);
        for (text, w) in TREE_COLS {
            let l = tree_cell(text, w);
            l.add_css_class("caption");
            l.add_css_class("dim-label");
            hb.append(&l);
        }
        head.set_child(Some(&hb));
        self.tree_list.append(&head);

        let base = self.doc.borrow();
        let (doc, perr) = match self.queue.borrow().preview(&base) {
            Ok(p) => (p, None),
            Err(e) => (base.clone(), Some(e)),
        };
        let statuses = self.statuses_for(&doc);

        for (di, d) in self.disks.borrow().iter().enumerate() {
            for (pi, p) in d.parts.iter().enumerate() {
                let row = gtk::ListBoxRow::new();
                let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                b.set_margin_top(5);
                b.set_margin_bottom(5);
                b.set_margin_start(12);
                b.set_margin_end(12);
                b.append(&tree_cell(&p.path, TREE_COLS[0].1));
                b.append(&tree_cell(&ftab::human_bytes(p.size_bytes), TREE_COLS[1].1));
                b.append(&tree_cell(
                    p.fstype.as_deref().unwrap_or("—"),
                    TREE_COLS[2].1,
                ));
                b.append(&tree_cell(
                    p.label.as_deref().unwrap_or("—"),
                    TREE_COLS[3].1,
                ));
                b.append(&tree_cell(p.uuid.as_deref().unwrap_or("—"), TREE_COLS[4].1));
                let mp = p.mountpoints.first().map(String::as_str).unwrap_or("—");
                b.append(&tree_cell(mp, TREE_COLS[5].1));
                // 状态 + fstab 行号：有关联条目用 findmnt 状态，否则按 lsblk 挂载点。
                let linked = link_entry(&doc, p, &statuses);
                let (st_text, st_class, line_text) = match linked {
                    Some((e, st)) => {
                        let (t, c) = status_meta(st);
                        (t, c, e.line_no.to_string())
                    }
                    None => {
                        if p.mountpoints.is_empty() {
                            ("未挂载", "dim-label", "—".to_string())
                        } else {
                            ("已挂载", "success", "—".to_string())
                        }
                    }
                };
                let st_cell = tree_cell("", TREE_COLS[6].1);
                st_cell.set_text(st_text);
                st_cell.add_css_class("caption");
                st_cell.add_css_class(st_class);
                b.append(&st_cell);
                b.append(&tree_cell(&line_text, TREE_COLS[7].1));
                row.set_child(Some(&b));
                self.tree_list.append(&row);
                map.push(Some((di, pi)));
                attach_row_actions(
                    &row,
                    move || with_inner(|i| i.show_dev_props(Sel::Part(di, pi))),
                    move |w, x, y| with_inner(|i| i.show_part_menu(Sel::Part(di, pi), &w, x, y)),
                );
            }
        }
        drop(base);
        *self.tree_map.borrow_mut() = map;
        let _ = perr; // 预览错误已由 rebuild_list 统一写入 list_status
    }

    /// 预览文档 + findmnt 的 (行号, 状态) 表；json 缺失/预览失败为空。
    fn statuses_for(&self, doc: &FstabDocument) -> Vec<(usize, MountStatus)> {
        self.json
            .borrow()
            .as_deref()
            .and_then(|j| ftab::status_table(j, doc).ok())
            .map(|v| doc.entries().map(|e| e.line_no).zip(v).collect())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------------ 信息面板

    fn rebuild_info(&self) {
        while let Some(child) = self.info_box.first_child() {
            self.info_box.remove(&child);
        }
        let disks = self.disks.borrow();
        let head = |text: &str| {
            let l = gtk::Label::new(Some(text));
            l.add_css_class("title-4");
            l.set_xalign(0.0);
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
            self.info_box.append(&l);
        };
        let kv = |key: &str, val: &str| {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.set_margin_top(2);
            row.set_margin_bottom(2);
            let k = gtk::Label::new(Some(key));
            k.add_css_class("dim-label");
            k.set_xalign(0.0);
            k.set_size_request(84, -1);
            let v = gtk::Label::new(Some(val));
            v.set_xalign(0.0);
            v.set_hexpand(true);
            v.set_selectable(true);
            v.set_wrap(true);
            v.set_wrap_mode(gtk::pango::WrapMode::Char);
            v.set_ellipsize(gtk::pango::EllipsizeMode::End);
            row.append(&k);
            row.append(&v);
            self.info_box.append(&row);
        };
        match self.sel.get() {
            None => {
                let l = gtk::Label::new(Some("选择左侧设备或分区查看完整信息"));
                l.add_css_class("dim-label");
                l.set_xalign(0.0);
                l.set_margin_top(6);
                self.info_box.append(&l);
            }
            Some(Sel::Disk(d)) => {
                let Some(disk) = disks.get(d) else { return };
                head(&disk.name);
                kv("类型", "磁盘");
                kv("路径", &disk.path);
                kv(
                    "大小",
                    &format!(
                        "{}（{} 字节）",
                        ftab::human_bytes(disk.size_bytes),
                        disk.size_bytes
                    ),
                );
                kv("分区数", &disk.parts.len().to_string());
                let mounted = disk
                    .parts
                    .iter()
                    .filter(|p| !p.mountpoints.is_empty())
                    .count();
                kv("已挂载分区", &format!("{mounted} / {}", disk.parts.len()));
                let hint = gtk::Label::new(Some("点击左侧分区行或中部树视图查看分区字段"));
                hint.add_css_class("dim-label");
                hint.add_css_class("caption");
                hint.set_xalign(0.0);
                hint.set_margin_top(8);
                self.info_box.append(&hint);
            }
            Some(Sel::Part(d, p)) => {
                let Some(disk) = disks.get(d) else { return };
                let Some(part) = disk.parts.get(p) else {
                    return;
                };
                head(&part.name);
                kv("设备路径", &part.path);
                kv(
                    "大小",
                    &format!(
                        "{}（{} 字节）",
                        ftab::human_bytes(part.size_bytes),
                        part.size_bytes
                    ),
                );
                kv("文件系统", part.fstype.as_deref().unwrap_or("未格式化"));
                kv("标签", part.label.as_deref().unwrap_or("—"));
                kv("UUID", part.uuid.as_deref().unwrap_or("—"));
                kv(
                    "挂载点",
                    part.mountpoints
                        .first()
                        .map(String::as_str)
                        .unwrap_or("未挂载"),
                );
                // 用量（df，按挂载点匹配）
                if let Some(mp0) = part.mountpoints.first() {
                    let key = ftab::normalize_mountpoint(mp0);
                    if let Some((_, size, used)) = self
                        .usage
                        .borrow()
                        .iter()
                        .find(|(t, _, _)| ftab::normalize_mountpoint(t) == key)
                    {
                        let pct = if *size > 0 {
                            *used as f64 / *size as f64 * 100.0
                        } else {
                            0.0
                        };
                        kv(
                            "用量",
                            &format!(
                                "{} / {}（{pct:.1}%）",
                                ftab::human_bytes(*used),
                                ftab::human_bytes(*size)
                            ),
                        );
                    }
                }
                // fstab 关联（按当前队列 preview，含待定状态）
                let base = self.doc.borrow();
                let preview = match self.queue.borrow().preview(&base) {
                    Ok(pv) => pv,
                    Err(_) => base.clone(),
                };
                let statuses = self.statuses_for(&preview);
                match link_entry(&preview, part, &statuses) {
                    Some((e, st)) => {
                        kv("fstab 行", &format!("第 {} 行", e.line_no));
                        let (t, _) = status_meta(st);
                        kv("fstab 状态", t);
                        kv("fstab 来源", &ftab::unescape_field(&e.source));
                        kv("挂载选项", &ftab::unescape_field(&e.options));
                    }
                    None => {
                        kv("fstab 行", "不在 /etc/fstab");
                        let hint = gtk::Label::new(Some("可在下方表单填来源后「加入待定队列」"));
                        hint.add_css_class("dim-label");
                        hint.add_css_class("caption");
                        hint.set_xalign(0.0);
                        hint.set_margin_top(4);
                        self.info_box.append(&hint);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------ 队列面板

    fn rebuild_queue(&self) {
        while let Some(child) = self.queue_list.first_child() {
            self.queue_list.remove(&child);
        }
        let n = self.queue.borrow().len();
        self.queue_status.set_text(&format!("待定操作（{n}）"));
        if n == 0 {
            let row = gtk::ListBoxRow::new();
            row.set_selectable(false);
            let l = gtk::Label::new(Some(
                "（空 · 新增/编辑/停用/删除只入队，点 Apply 统一写入，撤销零成本）",
            ));
            l.add_css_class("dim-label");
            l.add_css_class("caption");
            l.set_xalign(0.0);
            l.set_margin_top(6);
            l.set_margin_bottom(6);
            l.set_margin_start(10);
            l.set_margin_end(10);
            l.set_wrap(true);
            row.set_child(Some(&l));
            self.queue_list.append(&row);
            return;
        }
        for (idx, op) in self.queue.borrow().ops().iter().enumerate() {
            let row = gtk::ListBoxRow::new();
            row.set_selectable(false);
            let h = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            h.set_margin_top(4);
            h.set_margin_bottom(4);
            h.set_margin_start(10);
            h.set_margin_end(6);
            let idx_no = gtk::Label::new(Some(&format!("{}.", idx + 1)));
            idx_no.add_css_class("caption");
            idx_no.add_css_class("dim-label");
            h.append(&idx_no);
            let desc = gtk::Label::new(Some(&op.describe()));
            desc.set_xalign(0.0);
            desc.set_hexpand(true);
            desc.set_ellipsize(gtk::pango::EllipsizeMode::End);
            h.append(&desc);
            let undo = gtk::Button::with_label("撤销");
            undo.add_css_class("flat");
            undo.add_css_class("pill");
            undo.set_tooltip_text(Some("撤销这一条待定操作（对磁盘零影响）"));
            undo.connect_clicked(move |_| with_inner(|inner| inner.remove_op(idx)));
            h.append(&undo);
            row.set_child(Some(&h));
            self.queue_list.append(&row);
        }
    }

    // ------------------------------------------------------------------ 分区条

    fn draw_bar(&self, ctx: &gtk::cairo::Context, w: i32, h: i32) {
        let (wf, hf) = (w as f64, h as f64);
        if wf < 8.0 || hf < 8.0 {
            return;
        }
        // 背景
        ctx.set_source_rgba(0.5, 0.5, 0.5, 0.10);
        ctx.rectangle(0.0, 0.0, wf, hf);
        let _ = ctx.fill();

        let disks = self.disks.borrow();
        let sel = self.sel.get();
        let di = self.sel_disk_idx();
        let Some(disk) = disks.get(di) else {
            ctx.set_source_rgba(0.5, 0.5, 0.5, 1.0);
            ctx.select_font_face("sans-serif", FontSlant::Normal, FontWeight::Normal);
            ctx.set_font_size(13.0);
            ctx.move_to(12.0, hf / 2.0 + 4.0);
            let _ = ctx.show_text("无块设备数据");
            return;
        };
        let segs = disk.segments();
        let total: u64 = segs.iter().map(|p| p.size_bytes.max(1)).sum::<u64>().max(1);
        let mut x = 0.0;
        for (i, p) in segs.iter().enumerate() {
            let sw = p.size_bytes.max(1) as f64 / total as f64 * wf;
            let x1 = (x + sw).min(wf);
            let wseg = x1 - x;
            if wseg < 1.0 {
                x = x1;
                continue;
            }
            let is_sel = matches!(sel, Some(Sel::Part(d, pp)) if d == di && pp == i);
            let (r, g, b) = fs_color(p.fstype.as_deref());
            ctx.set_source_rgba(r, g, b, if is_sel { 0.95 } else { 0.78 });
            ctx.rectangle(x, 1.0, wseg - 1.5, hf - 2.0);
            let _ = ctx.fill();

            // 已用量：底部深色叠加（df 按挂载点匹配）
            if let Some(mp0) = p.mountpoints.first() {
                let key = ftab::normalize_mountpoint(mp0);
                if let Some((_, size, used)) = self
                    .usage
                    .borrow()
                    .iter()
                    .find(|(t, _, _)| ftab::normalize_mountpoint(t) == key)
                    .filter(|(_, size, used)| *size > 0 && *used > 0)
                {
                    let frac = (*used as f64 / *size as f64).clamp(0.0, 1.0);
                    let bh = (hf - 2.0) * frac;
                    ctx.set_source_rgba(0.0, 0.0, 0.0, 0.32);
                    ctx.rectangle(x + 0.75, hf - 1.0 - bh, wseg - 1.5, bh);
                    let _ = ctx.fill();
                }
            }

            // 选中描边
            if is_sel {
                ctx.set_source_rgba(1.0, 1.0, 1.0, 0.95);
                ctx.set_line_width(2.0);
                ctx.rectangle(x + 1.0, 2.0, (wseg - 3.0).max(1.0), hf - 4.0);
                let _ = ctx.stroke();
            }

            // 文字（裁剪防溢出；段太窄只画挂载点/名称）
            if wseg > 52.0 {
                let _ = ctx.save();
                ctx.rectangle(x, 0.0, wseg, hf);
                ctx.clip();
                ctx.select_font_face("sans-serif", FontSlant::Normal, FontWeight::Normal);
                ctx.set_font_size(11.0);
                ctx.set_source_rgba(1.0, 1.0, 1.0, 0.98);
                let l1 = p.mountpoints.first().map(String::as_str).unwrap_or(&p.name);
                ctx.move_to(x + 5.0, 17.0);
                let _ = ctx.show_text(l1);
                if wseg > 88.0 {
                    ctx.set_font_size(10.0);
                    ctx.set_source_rgba(1.0, 1.0, 1.0, 0.85);
                    ctx.move_to(x + 5.0, 32.0);
                    let _ = ctx.show_text(&ftab::human_bytes(p.size_bytes));
                    if wseg > 130.0
                        && let Some(fs) = p.fstype.as_deref()
                    {
                        ctx.move_to(x + 5.0, 46.0);
                        let _ = ctx.show_text(fs);
                    }
                }
                let _ = ctx.restore();
            }
            x = x1;
        }
    }

    /// 分区条点击：x 坐标 → 分段 → 选中。
    fn on_bar_click(&self, x: f64, w: f64) {
        if w < 8.0 {
            return;
        }
        let disks = self.disks.borrow();
        let di = self.sel_disk_idx();
        let Some(disk) = disks.get(di) else {
            return;
        };
        let segs = disk.segments();
        let total: u64 = segs.iter().map(|p| p.size_bytes.max(1)).sum::<u64>().max(1);
        let mut acc = 0.0;
        let mut hit = None;
        for (i, p) in segs.iter().enumerate() {
            let sw = p.size_bytes.max(1) as f64 / total as f64 * w;
            if x < acc + sw {
                hit = Some(i);
                break;
            }
            acc += sw;
        }
        drop(disks);
        if let Some(i) = hit {
            self.sync_sel(Some(Sel::Part(di, i)));
        }
    }

    // ------------------------------------------------------------------ fstab 列表

    /// 用「应用后」预览文档重建列表：与 base 逐行对比打待定徽章。
    fn rebuild_list(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let base = self.doc.borrow();
        let (doc, perr) = match self.queue.borrow().preview(&base) {
            Ok(p) => (p, None),
            Err(e) => (base.clone(), Some(e)),
        };
        let statuses = self.statuses_for(&doc);

        let mut mounted = 0usize;
        let mut disabled = 0usize;
        let mut pending = 0usize;
        for e in doc.entries() {
            let st = statuses
                .iter()
                .find(|(ln, _)| *ln == e.line_no)
                .map(|(_, s)| *s);
            if st == Some(MountStatus::Mounted) {
                mounted += 1;
            }
            if e.disabled {
                disabled += 1;
            }
            // Remove 入队后 add_entry 会复用被删行号 → base 同号旧行不算数
            let line_removed =
                self.queue.borrow().ops().iter().any(
                    |op| matches!(op, PendingOp::Remove { line_no, .. } if *line_no == e.line_no),
                );
            let base_e = if line_removed {
                None
            } else {
                base.entries().find(|b| b.line_no == e.line_no)
            };
            let badge = match base_e {
                None => {
                    pending += 1;
                    Some(("待定·新增", "accent"))
                }
                Some(b) if b.disabled != e.disabled => {
                    pending += 1;
                    Some(if e.disabled {
                        ("待定·停用", "warning")
                    } else {
                        ("待定·恢复", "warning")
                    })
                }
                Some(b) if b != e => {
                    pending += 1;
                    Some(("待定·修改", "warning"))
                }
                Some(_) => None,
            };
            let on_disk = base_e.is_some();
            let row = self.build_row(e, st, badge, on_disk);
            self.list.append(&row);
        }
        let mut status = format!(
            "共 {} 条 · 已挂载 {mounted} · 已停用 {disabled} · 待定 {pending}",
            doc.entry_count()
        );
        if let Some(e) = perr {
            status.push_str(&format!(" · 队列预览失败：{e}"));
        }
        self.list_status.set_text(&status);
        // 空态（7.10）：零条目 → StatusPage，有条目 → 列表卡片
        let total = doc.entry_count();
        self.list_card.set_visible(total > 0);
        self.empty_sw.set_visible(total == 0);
    }

    fn build_row(
        &self,
        e: &FstabEntry,
        st: Option<MountStatus>,
        badge: Option<(&str, &str)>,
        on_disk: bool,
    ) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        let h = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        h.set_margin_top(8);
        h.set_margin_bottom(8);
        h.set_margin_start(12);
        h.set_margin_end(12);

        // 文本区：挂载点（标题） + 来源/类型/选项（次行）
        let texts = gtk::Box::new(gtk::Orientation::Vertical, 2);
        texts.set_hexpand(true);
        let title = gtk::Label::new(Some(&ftab::unescape_field(&e.mountpoint)));
        title.add_css_class("title-4");
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        if e.disabled {
            title.add_css_class("dim-label");
        }
        texts.append(&title);
        let sub = gtk::Label::new(Some(&format!(
            "{} · {} · {} · 第 {} 行",
            ftab::unescape_field(&e.source),
            e.vfstype,
            ftab::unescape_field(&e.options),
            e.line_no
        )));
        sub.add_css_class("dim-label");
        sub.add_css_class("caption");
        sub.set_xalign(0.0);
        sub.set_ellipsize(gtk::pango::EllipsizeMode::End);
        texts.append(&sub);
        h.append(&texts);

        // 状态列 + 待定徽章
        let (pill_text, pill_class) = match st {
            Some(s) => status_meta(s),
            None => ("状态未知", "warning"),
        };
        let pills = gtk::Box::new(gtk::Orientation::Vertical, 2);
        pills.append(&pill(pill_text, pill_class));
        if let Some((bt, bc)) = badge {
            pills.append(&pill(bt, bc));
        }
        h.append(&pills);

        // 操作按钮
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let line = e.line_no;

        let edit_btn = gtk::Button::with_label("编辑");
        edit_btn.add_css_class("flat");
        edit_btn.set_tooltip_text(Some("修改该条目（只入队，点 Apply 写入磁盘）"));
        edit_btn.connect_clicked(move |_| with_inner(|i| i.start_edit(line)));
        buttons.append(&edit_btn);

        let toggle_btn = gtk::Button::with_label(if e.disabled { "恢复" } else { "停用" });
        toggle_btn.add_css_class("flat");
        toggle_btn.set_tooltip_text(Some(
            "注释停用 = 行首加 #（可一键恢复）；只入队，点 Apply 写入",
        ));
        toggle_btn.connect_clicked(move |_| with_inner(|i| i.toggle_disabled(line)));
        buttons.append(&toggle_btn);

        // 物理删除（D7.5 2026-10-03 修订）：确认弹窗 → 入队 → Apply 才落盘
        let del_btn = gtk::Button::with_label("删除");
        del_btn.add_css_class("flat");
        del_btn.add_css_class("destructive-action");
        del_btn.set_tooltip_text(Some(
            "物理删除该行（不可恢复，需确认）；只入队，点 Apply 写入",
        ));
        del_btn.connect_clicked(move |_| with_inner(|i| i.confirm_delete(line)));
        buttons.append(&del_btn);

        // 挂载/卸载只对已在磁盘上的条目有意义（待定新增还没进 /etc/fstab）
        if on_disk {
            let mounted = st == Some(MountStatus::Mounted);
            if !e.disabled {
                let mount_btn = gtk::Button::with_label("挂载");
                mount_btn.add_css_class("flat");
                mount_btn.set_tooltip_text(Some("立即挂载（pkexec；swap 走 swapon）"));
                mount_btn.set_sensitive(!mounted);
                mount_btn.connect_clicked(move |_| with_inner(|i| i.mount_entry(line)));
                buttons.append(&mount_btn);
            }
            let umount_btn = gtk::Button::with_label("卸载");
            umount_btn.add_css_class("flat");
            umount_btn.set_tooltip_text(Some("立即卸载（pkexec；swap 走 swapoff）"));
            umount_btn.set_sensitive(match st {
                Some(MountStatus::Mounted) => true,
                Some(_) => false,
                None => true, // 状态未知时放开，让命令结果说话
            });
            umount_btn.connect_clicked(move |_| with_inner(|i| i.umount_entry(line)));
            buttons.append(&umount_btn);
        }

        h.append(&buttons);
        row.set_child(Some(&h));

        // 双击属性 / 右键菜单（GOAL 7.9）
        attach_row_actions(
            &row,
            move || with_inner(|i| i.show_entry_props(line)),
            move |w, x, y| with_inner(|i| i.show_entry_menu(line, &w, x, y)),
        );
        row
    }

    // ---------------- 行交互：双击属性 / 右键菜单 / 编辑挂载点 / 生成条目（GOAL 7.9） ----------------

    /// 双击条目行 → 属性对话框。
    fn show_entry_props(&self, line_no: usize) {
        let preview = self.preview_doc();
        let statuses = self.statuses_for(&preview);
        let Some(e) = preview.entries().find(|e| e.line_no == line_no).cloned() else {
            self.toast(&format!("未找到第 {line_no} 行的条目"));
            return;
        };
        let st = statuses
            .iter()
            .find(|(l, _)| *l == line_no)
            .map(|(_, s)| *s);
        let on_disk = self.doc.borrow().entries().any(|b| b.line_no == line_no);
        let st_text = st.map(status_meta).map(|(t, _)| t).unwrap_or("未知");
        let mut rows: Vec<(&str, String)> = vec![
            ("行号", line_no.to_string()),
            ("挂载点", ftab::unescape_field(&e.mountpoint)),
            ("来源", ftab::unescape_field(&e.source)),
            ("文件系统", e.vfstype.clone()),
            ("挂载选项", ftab::unescape_field(&e.options)),
            ("dump / pass", format!("{} / {}", e.dump, e.pass)),
            ("状态", st_text.to_string()),
            (
                "所在",
                if on_disk {
                    "已在磁盘"
                } else {
                    "待定新增"
                }
                .to_string(),
            ),
        ];
        if e.disabled {
            rows.push(("停用", "已注释停用（行首 #）".to_string()));
        }
        self.show_props_dialog(&format!("属性 · 第 {line_no} 行条目"), rows);
    }

    /// 双击设备/树行 → 磁盘或分区属性对话框。
    fn show_dev_props(&self, sel: Sel) {
        let preview = self.preview_doc();
        let statuses = self.statuses_for(&preview);
        let disks = self.disks.borrow();
        match sel {
            Sel::Disk(di) => {
                let Some(d) = disks.get(di) else { return };
                let rows: Vec<(&str, String)> = vec![
                    ("磁盘", d.name.clone()),
                    ("路径", d.path.clone()),
                    ("大小", ftab::human_bytes(d.size_bytes)),
                    ("分区数", d.parts.len().to_string()),
                    (
                        "已挂载分区",
                        d.parts
                            .iter()
                            .filter(|p| !p.mountpoints.is_empty())
                            .count()
                            .to_string(),
                    ),
                ];
                self.show_props_dialog(&format!("属性 · {}", d.name), rows);
            }
            Sel::Part(di, pi) => {
                let Some(d) = disks.get(di) else { return };
                let Some(p) = d.parts.get(pi) else { return };
                let mut rows: Vec<(&str, String)> = vec![
                    ("设备", p.path.clone()),
                    ("磁盘", d.name.clone()),
                    ("大小", ftab::human_bytes(p.size_bytes)),
                    (
                        "文件系统",
                        p.fstype.clone().unwrap_or_else(|| "未格式化".to_string()),
                    ),
                    ("标签", p.label.clone().unwrap_or_else(|| "—".to_string())),
                    ("UUID", p.uuid.clone().unwrap_or_else(|| "—".to_string())),
                    (
                        "挂载点",
                        p.mountpoints
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "—（未挂载）".to_string()),
                    ),
                ];
                if let Some((_, used, total)) = self
                    .usage
                    .borrow()
                    .iter()
                    .find(|(path, _, _)| path == &p.path)
                {
                    rows.push((
                        "用量",
                        format!(
                            "{} / {}",
                            ftab::human_bytes(*used),
                            ftab::human_bytes(*total)
                        ),
                    ));
                }
                match link_entry(&preview, p, &statuses) {
                    Some((e, st)) => {
                        rows.push(("fstab 行", e.line_no.to_string()));
                        rows.push(("fstab 状态", status_meta(st).0.to_string()));
                        rows.push(("fstab 选项", ftab::unescape_field(&e.options)));
                    }
                    None => rows.push(("fstab 行", "未关联".to_string())),
                }
                self.show_props_dialog(&format!("属性 · {}", p.name), rows);
            }
        }
    }

    /// fstab 致命错误（读失败/不存在）：隐藏主区，整页 StatusPage（7.10，UI 规范 §8）。
    fn show_fatal(&self, e: &str) {
        if std::path::Path::new("/etc/fstab").exists() {
            // StatusPage 描述按 Pango markup 解析（错误文本常含 `<`，如「字段数 3 < 4」）→ 必须转义
            let d = glib::markup_escape_text(e);
            self.fatal_sp.set_title("读取 /etc/fstab 失败");
            self.fatal_sp.set_description(Some(d.as_str()));
        } else {
            self.fatal_sp.set_title("/etc/fstab 不存在");
            self.fatal_sp
                .set_description(Some("系统未找到 /etc/fstab——挂载配置缺失，修复后点重试"));
        }
        self.main_box.set_visible(false);
        self.fatal_sp.set_visible(true);
    }

    fn clear_fatal(&self) {
        self.fatal_sp.set_visible(false);
        self.main_box.set_visible(true);
    }

    /// lsblk 读取失败：设备面板内切换为 StatusPage（7.10 错误态）。
    fn set_lsblk_error(&self, e: Option<String>) {
        match e {
            Some(err) => {
                let d = glib::markup_escape_text(&err);
                self.dev_err_sp.set_description(Some(d.as_str()));
                // 滚动壳整块替换列表；抬 dev 卡最小高给壳腾槽位（左列 queue 自然高大，不抬必被压到 58px）
                self.dev_err_sw.set_visible(true);
                self.dev_scroll.set_visible(false);
                self.dev_card.set_size_request(-1, 280);
            }
            None => {
                self.dev_err_sw.set_visible(false);
                self.dev_scroll.set_visible(true);
                self.dev_card.set_size_request(-1, -1);
            }
        }
    }

    /// 状态栏入口：/etc/fstab 原文只读查看（内存中已加载原文，零 IO）。
    fn show_fstab_raw(&self) {
        let text = self.loaded_text.borrow().clone();
        if text.trim().is_empty() {
            self.toast("fstab 原文尚未加载");
            return;
        }
        self.show_raw_dialog("/etc/fstab 原文（只读）", &text);
    }

    /// 状态栏入口：备份文件只读查看（7.10）。
    fn show_backup_raw(&self) {
        let path = ftab::backup_path(std::path::Path::new("/etc/fstab"));
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                self.toast(&format!("备份暂不可读（{}）：{e}", path.display()));
                return;
            }
        };
        self.show_raw_dialog(&format!("备份 {}（只读）", path.display()), &text);
    }

    /// 原文/备份统一展示窗：等宽只读 TextView + 滚动。
    fn show_raw_dialog(&self, title: &str, text: &str) {
        let Some(win) = self.window() else { return };
        let tv = gtk::TextView::new();
        tv.set_editable(false);
        tv.set_cursor_visible(false);
        tv.set_monospace(true);
        tv.set_wrap_mode(gtk::WrapMode::WordChar);
        tv.buffer().set_text(text);
        let sw = gtk::ScrolledWindow::new();
        sw.set_min_content_height(320);
        sw.set_max_content_height(520);
        sw.set_propagate_natural_height(true);
        sw.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        sw.set_child(Some(&tv));
        let dialog = adw::MessageDialog::new(Some(&win), Some(title), None);
        dialog.set_extra_child(Some(&sw));
        dialog.add_response("close", "关闭");
        dialog.set_default_response(Some("close"));
        dialog.set_close_response("close");
        dialog.connect_response(None, |d, _| d.close());
        dialog.present();
    }

    /// 属性对话框通用渲染（GOAL 7.9：双击弹出）。
    fn show_props_dialog(&self, title: &str, rows: Vec<(&str, String)>) {
        let Some(win) = self.window() else { return };
        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(4);
        content.set_margin_end(4);
        for (k, v) in rows {
            content.append(&kv_row(k, &v));
        }
        let dialog = adw::MessageDialog::new(Some(&win), Some(title), None);
        dialog.set_extra_child(Some(&content));
        dialog.add_response("close", "关闭");
        dialog.set_default_response(Some("close"));
        dialog.set_close_response("close");
        dialog.connect_response(None, |d, _| d.close());
        dialog.present();
    }

    /// 条目行右键菜单（GOAL 7.9）。数据在打开时现算，不缓存。
    fn show_entry_menu(&self, line_no: usize, anchor: &gtk::Widget, x: f64, y: f64) {
        let preview = self.preview_doc();
        let statuses = self.statuses_for(&preview);
        let Some(e) = preview.entries().find(|e| e.line_no == line_no).cloned() else {
            return;
        };
        let st = statuses
            .iter()
            .find(|(l, _)| *l == line_no)
            .map(|(_, s)| *s);
        let on_disk = self.doc.borrow().entries().any(|b| b.line_no == line_no);
        let mut items = vec![
            MenuEntry::new("属性（双击亦可）", move || {
                with_inner(|i| i.show_entry_props(line_no))
            }),
            MenuEntry::new("编辑（完整表单）", move || {
                with_inner(|i| i.start_edit(line_no))
            }),
            MenuEntry::new("编辑挂载点…", move || {
                with_inner(|i| i.edit_mountpoint(line_no))
            }),
            MenuEntry::new(
                if e.disabled {
                    "恢复（取消注释）"
                } else {
                    "停用（注释停用）"
                },
                move || with_inner(|i| i.toggle_disabled(line_no)),
            ),
            MenuEntry::destructive("物理删除…", move || {
                with_inner(|i| i.confirm_delete(line_no))
            }),
        ];
        let mut ops = Vec::new();
        if on_disk {
            if !e.disabled && st != Some(MountStatus::Mounted) {
                ops.push(MenuEntry::new("立即挂载", move || {
                    with_inner(|i| i.mount_entry(line_no))
                }));
            }
            if st == Some(MountStatus::Mounted) || st.is_none() {
                ops.push(MenuEntry::new("立即卸载", move || {
                    with_inner(|i| i.umount_entry(line_no))
                }));
            }
        }
        if !ops.is_empty() {
            items.push(MenuEntry::sep());
            items.extend(ops);
        }
        self.popup_menu(anchor, x, y, items);
    }

    /// 设备/树行右键菜单（GOAL 7.9）：分区未挂载且不在 fstab → 一键生成；
    /// 已关联 fstab → 编辑挂载点/停用/挂载/卸载。
    fn show_part_menu(&self, sel: Sel, anchor: &gtk::Widget, x: f64, y: f64) {
        let mut items = vec![MenuEntry::new("属性（双击亦可）", move || {
            with_inner(|i| i.show_dev_props(sel))
        })];
        if let Sel::Part(di, pi) = sel {
            let preview = self.preview_doc();
            let statuses = self.statuses_for(&preview);
            let linked = {
                let disks = self.disks.borrow();
                disks
                    .get(di)
                    .and_then(|d| d.parts.get(pi))
                    .and_then(|p| link_entry(&preview, p, &statuses))
            };
            match linked {
                Some((e, st)) => {
                    let line = e.line_no;
                    let disabled = e.disabled;
                    items.push(MenuEntry::new("编辑挂载点…", move || {
                        with_inner(|i| i.edit_mountpoint(line))
                    }));
                    items.push(MenuEntry::new(
                        if disabled {
                            "恢复（取消注释）"
                        } else {
                            "停用（注释停用）"
                        },
                        move || with_inner(|i| i.toggle_disabled(line)),
                    ));
                    let mut ops = Vec::new();
                    if !disabled && st != MountStatus::Mounted {
                        ops.push(MenuEntry::new("立即挂载", move || {
                            with_inner(|i| i.mount_entry(line))
                        }));
                    }
                    if st == MountStatus::Mounted {
                        ops.push(MenuEntry::new("立即卸载", move || {
                            with_inner(|i| i.umount_entry(line))
                        }));
                    }
                    if !ops.is_empty() {
                        items.push(MenuEntry::sep());
                        items.extend(ops);
                    }
                }
                None => {
                    let can_generate = {
                        let disks = self.disks.borrow();
                        disks
                            .get(di)
                            .and_then(|d| d.parts.get(pi))
                            .map(|p| p.mountpoints.is_empty())
                            .unwrap_or(false)
                    };
                    if can_generate {
                        items.push(MenuEntry::new(
                            "生成 fstab 条目（一键入队）",
                            move || with_inner(|i| i.generate_entry(di, pi)),
                        ));
                    }
                }
            }
        }
        self.popup_menu(anchor, x, y, items);
    }

    /// 行级右键菜单：popover 挂在该行上（GTK4 popover 是独立 surface，不占行布局），
    /// 关闭即解挂，行销毁时连带销毁。
    fn popup_menu(&self, anchor: &gtk::Widget, x: f64, y: f64, items: Vec<MenuEntry>) {
        let pop = gtk::Popover::new();
        pop.set_parent(anchor);
        pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 1);
        vbox.set_margin_top(6);
        vbox.set_margin_bottom(6);
        vbox.set_margin_start(6);
        vbox.set_margin_end(6);
        for it in items {
            if it.label == "-" {
                vbox.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                continue;
            }
            let btn = gtk::Button::with_label(&it.label);
            btn.add_css_class("flat");
            btn.set_halign(gtk::Align::Start);
            btn.set_hexpand(true);
            if it.destructive {
                btn.add_css_class("destructive-action");
            }
            let weak = pop.downgrade();
            btn.connect_clicked(move |_| {
                if let Some(p) = weak.upgrade() {
                    p.popdown();
                }
                (it.action)();
            });
            vbox.append(&btn);
        }
        pop.set_child(Some(&vbox));
        let weak = pop.downgrade();
        pop.connect_closed(move |_| {
            if let Some(p) = weak.upgrade()
                && p.parent().is_some()
            {
                p.unparent();
            }
        });
        pop.popup();
    }

    /// 编辑挂载点对话框（KDE PM 同款，GOAL 7.9）：只开挂载点 + 挂载选项两字段，
    /// 其余字段保持原值；选项字段同样走勾选窗（禁止手输）；确认只入队，Apply 才落盘。
    fn edit_mountpoint(&self, line_no: usize) {
        if self.busy.get() {
            self.toast("上一个操作还在进行中，请稍候");
            return;
        }
        let preview = self.preview_doc();
        let Some(e) = preview.entries().find(|e| e.line_no == line_no).cloned() else {
            self.toast(&format!("未找到第 {line_no} 行的条目"));
            return;
        };
        let Some(win) = self.window() else { return };

        let dialog = adw::MessageDialog::new(
            Some(&win),
            Some(&format!("编辑挂载点（第 {line_no} 行）")),
            None,
        );
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_margin_top(8);
        content.set_margin_bottom(8);
        content.set_margin_start(6);
        content.set_margin_end(6);

        let mp_head = gtk::Label::new(Some("挂载点（必填，绝对路径；swap 填 none）"));
        mp_head.add_css_class("caption");
        mp_head.add_css_class("dim-label");
        mp_head.set_xalign(0.0);
        content.append(&mp_head);
        let mp_entry = gtk::Entry::new();
        mp_entry.set_text(&ftab::unescape_field(&e.mountpoint));
        mp_entry.set_hexpand(true);
        content.append(&mp_entry);

        let opts_head = gtk::Label::new(Some("挂载选项（点击按钮勾选，禁止手输）"));
        opts_head.add_css_class("caption");
        opts_head.add_css_class("dim-label");
        opts_head.set_xalign(0.0);
        content.append(&opts_head);
        let opts_btn = gtk::Button::with_label(&ftab::unescape_field(&e.options));
        opts_btn.set_hexpand(true);
        opts_btn.set_tooltip_text(Some("打开勾选窗（D7.9：核心选项 + 全部选项两层）"));
        content.append(&opts_btn);

        let hint = gtk::Label::new(Some("修改只入队；点「入队修改」后仍需 Apply 才写入磁盘"));
        hint.add_css_class("caption");
        hint.add_css_class("dim-label");
        hint.set_xalign(0.0);
        content.append(&hint);

        dialog.set_extra_child(Some(&content));
        dialog.add_response("cancel", "取消");
        dialog.add_response("ok", "入队修改");
        dialog.set_default_response(Some("ok"));
        dialog.set_close_response("cancel");
        let opts_btn_done = opts_btn.clone();
        dialog.connect_response(None, move |d, resp| {
            d.close();
            if resp == "ok" {
                let mp = mp_entry.text().to_string();
                let opts = opts_btn_done
                    .label()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                with_inner(|i| i.apply_mountpoint_edit(line_no, &mp, &opts));
            }
        });

        // 选项按钮 → 勾选窗（父 = 本对话框，嵌套弹层）
        opts_btn.connect_clicked(clone!(
            #[strong]
            dialog,
            #[strong]
            opts_btn,
            move |_| {
                let cur = opts_btn.label().map(|s| s.to_string()).unwrap_or_default();
                with_inner(|i| {
                    i.pick_options_in(&dialog, &cur, {
                        let b = opts_btn.clone();
                        move |s| b.set_label(&s)
                    });
                });
            }
        ));
        dialog.present();
    }

    /// 编辑挂载点确认：只改挂载点 + 选项，其余字段沿用预览值；走同一条入队路径。
    fn apply_mountpoint_edit(&self, line_no: usize, mp: &str, opts: &str) {
        let preview = self.preview_doc();
        let Some(e) = preview.entries().find(|e| e.line_no == line_no).cloned() else {
            self.toast(&format!("未找到第 {line_no} 行的条目"));
            return;
        };
        let mut e2 = e.clone();
        e2.set_mountpoint(field_of(mp));
        e2.set_options(field_of(opts));
        match self.replace_pending_add(line_no, e2) {
            Err(msg) => self.toast(&format!("入队失败：{msg}")),
            Ok(()) => {
                self.refresh_views();
                self.toast("挂载点修改已入队（点 Apply 写入磁盘）");
                self.check_mountpoint_dir(mp.trim());
            }
        }
    }

    /// 一键生成条目（GOAL 7.9）：选中未挂载且不在 fstab 的分区 →
    /// 自动填 UUID + fstype + 推荐 options 入队；挂载点建议 `/mnt/<label|分区名>`。
    /// 系统盘（含 `/` 挂载点的盘）= 内盘 → `defaults,relatime`（pass 2）；
    /// 其余按外盘处理 → 追加 `noauto,nofail`（pass 0）。
    fn generate_entry(&self, di: usize, pi: usize) {
        if self.busy.get() {
            self.toast("上一个操作还在进行中，请稍候");
            return;
        }
        let preview = self.preview_doc();
        let statuses = self.statuses_for(&preview);
        let (source, fstype, name, unmounted, system, linked_line) = {
            let disks = self.disks.borrow();
            let Some(d) = disks.get(di) else { return };
            let Some(p) = d.parts.get(pi) else { return };
            let linked_line = link_entry(&preview, p, &statuses).map(|(e, _)| e.line_no);
            let system = d
                .parts
                .iter()
                .any(|q| q.mountpoints.iter().any(|m| m == "/"));
            let source = match &p.uuid {
                Some(u) if !u.is_empty() => format!("UUID={u}"),
                _ => p.path.clone(),
            };
            let name = p
                .label
                .as_deref()
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| p.name.clone());
            (
                source,
                p.fstype.clone(),
                name,
                p.mountpoints.is_empty(),
                system,
                linked_line,
            )
        };
        if let Some(line) = linked_line {
            self.toast(&format!("该分区已是 fstab 第 {line} 行"));
            return;
        }
        if !unmounted {
            self.toast("该分区已挂载；如需开机自动挂载请走「新增条目」表单");
            return;
        }
        let Some(fstype) = fstype else {
            self.toast("该分区未格式化，无法生成条目");
            return;
        };
        let mountpoint = format!("/mnt/{}", sanitize_mountpoint_name(&name));
        let options = if system {
            "defaults,relatime".to_string()
        } else {
            "defaults,relatime,noauto,nofail".to_string()
        };
        let mut e = FstabEntry::new(
            field_of(&source),
            field_of(&mountpoint),
            fstype,
            field_of(&options),
        );
        e.set_dump("0");
        e.set_pass(if system { "2" } else { "0" });
        match self.enqueue(PendingOp::Add(e)) {
            Err(msg) => self.toast(&format!("生成失败：{msg}")),
            Ok(()) => {
                self.refresh_views();
                self.toast(&format!(
                    "已生成待定条目 {mountpoint}（可「编辑/编辑挂载点」调整，Apply 写入）"
                ));
                self.check_mountpoint_dir(&mountpoint);
            }
        }
    }

    // ---------------------------------------------------------------- 表单

    fn show_form(&self) {
        self.form_revealer.set_reveal_child(true);
    }

    fn hide_form(&self) {
        self.form_revealer.set_reveal_child(false);
        self.editing.set(None);
    }

    /// 表单「挂载选项」当前值（ActionRow subtitle 只读展示，D7.9 禁手输）。
    fn opts_text(&self) -> String {
        self.f_opts
            .subtitle()
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    fn set_opts_text(&self, s: &str) {
        self.f_opts.set_subtitle(s);
    }

    /// 文件系统类型下拉当前值（D7.10：字段禁手输，读下拉不读文本）。
    fn f_type_text(&self) -> String {
        let idx = self.f_type.selected();
        if idx == gtk::INVALID_LIST_POSITION {
            return "auto".to_string();
        }
        self.f_type
            .model()
            .and_then(|m| m.downcast::<gtk::StringList>().ok())
            .and_then(|m| m.string(idx))
            .map(|s| s.to_string())
            .unwrap_or_else(|| "auto".to_string())
    }

    /// 按值选中文件系统类型；值不在清单 → 动态补入后选中（编辑既有条目所见即所得）。
    fn select_fstype(&self, value: &str) {
        let v = value.trim();
        let v = if v.is_empty() { "auto" } else { v };
        let Some(model) = self
            .f_type
            .model()
            .and_then(|m| m.downcast::<gtk::StringList>().ok())
        else {
            return;
        };
        let mut pos =
            (0..model.n_items()).find(|i| model.string(*i).is_some_and(|s| s.as_str() == v));
        if pos.is_none() {
            model.append(v);
            pos = Some(model.n_items().saturating_sub(1));
        }
        if let Some(p) = pos {
            self.f_type.set_selected(p);
        }
    }

    /// 新增模式：清空表单并展开。
    fn start_add(&self) {
        self.editing.set(None);
        self.form_title.set_text("新增条目");
        self.submit_btn.set_label("加入待定队列");
        self.f_source.set_text("");
        self.f_mount.set_text("");
        self.select_fstype("ext4");
        self.f_opts.set_subtitle(&ftab::default_options_string()); // D7.9 新条目默认
        self.f_dump.set_value(0.0);
        self.f_pass.set_value(0.0);
        self.part_combo.set_selected(0);
        self.show_form();
        self.f_source.grab_focus();
    }

    /// 编辑模式：把「应用后」条目回填表单（待定新增也可编辑，展示层做反转义）。
    fn start_edit(&self, line_no: usize) {
        let preview = self.preview_doc();
        let entry = preview.entries().find(|e| e.line_no == line_no).cloned();
        let Some(e) = entry else {
            self.toast(&format!("未找到第 {line_no} 行的条目"));
            return;
        };
        self.editing.set(Some(line_no));
        self.form_title
            .set_text(&format!("编辑第 {line_no} 行条目"));
        self.submit_btn.set_label("保存修改（入队）");
        self.f_source.set_text(&ftab::unescape_field(&e.source));
        self.f_mount.set_text(&ftab::unescape_field(&e.mountpoint));
        self.select_fstype(&e.vfstype);
        self.f_opts.set_subtitle(&ftab::unescape_field(&e.options));
        self.f_dump.set_value(e.dump.parse::<f64>().unwrap_or(0.0));
        self.f_pass.set_value(e.pass.parse::<f64>().unwrap_or(0.0));
        self.part_combo.set_selected(0);
        self.show_form();
        self.f_mount.grab_focus();
    }

    /// 表单提交：构造 PendingOp → 校验入队（GOAL 7.8：只入队不落盘）。
    fn submit_form(&self) {
        if self.busy.get() {
            self.toast("上一个操作还在进行中，请稍候");
            return;
        }
        let entry = form_entry(
            &self.f_source.text(),
            &self.f_mount.text(),
            &self.f_type_text(),
            &self.opts_text(),
            self.f_dump.value(),
            self.f_pass.value(),
        );
        let mountpoint = entry.mountpoint.clone();
        let editing = self.editing.get();
        let res = match editing {
            None => self.enqueue(PendingOp::Add(entry)),
            Some(line) => self.replace_pending_add(line, entry),
        };
        match res {
            Err(msg) => {
                // 校验失败入口（页面截图核对项：新增表单 → toast）
                self.toast(&format!("校验失败：{msg}"));
            }
            Ok(()) => {
                self.hide_form();
                self.refresh_views();
                let what = if editing.is_some() {
                    "修改"
                } else {
                    "新增"
                };
                self.toast(&format!("{what}已加入待定队列（点 Apply 写入磁盘）"));
                self.check_mountpoint_dir(&mountpoint);
            }
        }
    }

    // ---------------- 挂载选项勾选弹窗（D7.9 / 7.13） ----------------

    /// 表单入口：父窗口 = 主窗口。
    fn open_options_picker(&self, current: &str, done: impl FnOnce(String) + 'static) {
        let Some(win) = self.window() else {
            return;
        };
        self.pick_options_in(&win, current, done);
    }

    /// 挂载选项勾选弹窗（D7.9）：核心选项首屏直显 9 项，全部选项 7 组折叠于 Expander；
    /// 勾选走互斥/联动（utils::set_option），底部实时预览串；
    /// 确认只回填字符串，点 Apply 才落盘。
    fn pick_options_in(
        &self,
        parent: &impl IsA<gtk::Window>,
        current: &str,
        done: impl FnOnce(String) + 'static,
    ) {
        let state = Rc::new(RefCell::new(ftab::parse_options(current)));
        let updating = Rc::new(Cell::new(false));
        let boxes: Rc<RefCell<Vec<(String, gtk::CheckButton)>>> = Rc::new(RefCell::new(Vec::new()));

        let dialog = adw::MessageDialog::new(Some(parent), Some("挂载选项"), None);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(4);
        content.set_margin_end(4);

        let core_head = gtk::Label::new(Some("核心选项"));
        core_head.add_css_class("caption");
        core_head.add_css_class("dim-label");
        core_head.set_xalign(0.0);
        content.append(&core_head);

        let preview = gtk::Label::new(None);
        preview.set_wrap(true);
        preview.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        preview.set_xalign(0.0);
        preview.add_css_class("caption");
        preview.set_text(&options_preview(&state.borrow()));

        // 生成一个勾选框：初始值同步状态，变更走 set_option + 全量刷新（联动后所有项都可能变）
        let mk = {
            let boxes = boxes.clone();
            let state = state.clone();
            let updating = updating.clone();
            let preview = preview.clone();
            move |param: &str, label: &str, tooltip: Option<&str>| -> gtk::CheckButton {
                // 标签可换行（子控件）：全量清单长含义在窄窗内完整可见
                let cb = gtk::CheckButton::new();
                let lab = gtk::Label::new(Some(label));
                lab.set_wrap(true);
                lab.set_wrap_mode(gtk::pango::WrapMode::WordChar);
                lab.set_max_width_chars(64);
                lab.set_xalign(0.0);
                lab.set_halign(gtk::Align::Start);
                cb.set_child(Some(&lab));
                if let Some(t) = tooltip {
                    cb.set_tooltip_text(Some(t));
                }
                cb.set_active(state.borrow().is_checked(param));
                {
                    let boxes = boxes.clone();
                    let state = state.clone();
                    let updating = updating.clone();
                    let preview = preview.clone();
                    let param = param.to_string();
                    cb.connect_toggled(move |b| {
                        if updating.get() {
                            return; // 程序化 set_active 触发的回声，忽略
                        }
                        let active = b.is_active();
                        ftab::set_option(&mut state.borrow_mut(), &param, active);
                        updating.set(true);
                        for (p, c) in boxes.borrow().iter() {
                            let want = state.borrow().is_checked(p);
                            if c.is_active() != want {
                                c.set_active(want);
                            }
                        }
                        updating.set(false);
                        preview.set_text(&options_preview(&state.borrow()));
                    });
                }
                boxes.borrow_mut().push((param.to_string(), cb.clone()));
                cb
            }
        };

        // 核心选项（首屏直显）
        for (p, name, desc) in ftab::CORE_OPTIONS {
            let cb = mk(p, &format!("{name}（{p}）"), Some(desc));
            content.append(&cb);
        }

        // 全部选项：7 组折叠
        let exp = gtk::Expander::new(Some("全部选项（7 组 VFS 清单）"));
        exp.set_expanded(false);
        let groups_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        groups_box.set_margin_top(6);
        for (gname, items) in ftab::OPTION_GROUPS {
            let gh = gtk::Label::new(Some(gname));
            gh.add_css_class("caption");
            gh.add_css_class("dim-label");
            gh.set_xalign(0.0);
            groups_box.append(&gh);
            for (p, meaning) in items {
                let cb = mk(p, &format!("{p} — {meaning}"), None);
                if *p == "remount" {
                    cb.set_tooltip_text(Some(
                        "运行期动作：仅对「立即挂载」生效，按设计不写入 fstab",
                    ));
                }
                groups_box.append(&cb);
            }
        }
        exp.set_child(Some(&groups_box));
        content.append(&exp);

        // 清单外项只读区（key=value 原样保留）
        if !state.borrow().others.is_empty() {
            let others = state.borrow().others.join(", ");
            let ol = gtk::Label::new(Some(&format!("其他项（原样保留）：{others}")));
            ol.set_wrap(true);
            ol.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            ol.set_xalign(0.0);
            ol.add_css_class("caption");
            ol.add_css_class("dim-label");
            content.append(&ol);
        }
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_max_content_height(560); // 核心9项首屏直显（D7.9）
        scroll.set_propagate_natural_height(true);
        scroll.set_child(Some(&content));
        // 预览固定在滚动区之外（D7.9：实时预览恒可见）
        preview.set_margin_top(6);
        let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        outer.set_margin_start(4);
        outer.set_margin_end(4);
        outer.append(&scroll);
        outer.append(&preview);
        dialog.set_extra_child(Some(&outer));

        dialog.add_response("cancel", "取消");
        dialog.add_response("ok", "确定");
        dialog.set_default_response(Some("ok"));
        dialog.set_close_response("cancel");
        let done = Rc::new(RefCell::new(Some(done)));
        dialog.connect_response(None, {
            let state = state.clone();
            move |d, resp| {
                d.close();
                if resp == "ok"
                    && let Some(f) = done.borrow_mut().take()
                {
                    f(ftab::serialize_options(&state.borrow()));
                }
            }
        });
        dialog.present();
    }

    /// 挂载点目录不存在 → 提示 + 可选 pkexec 建目录（GOAL 7.6）。
    fn check_mountpoint_dir(&self, mp: &str) {
        if mp == "none" || mp.is_empty() || std::path::Path::new(mp).exists() {
            return;
        }
        let toast = adw::Toast::new(&format!("挂载点目录不存在：{mp}"));
        toast.set_button_label(Some("立即创建"));
        toast.set_timeout(8000);
        let mp = mp.to_string();
        toast.connect_button_clicked(move |_| with_inner(|i| i.create_dir(&mp)));
        self.toast_overlay.add_toast(toast);
    }

    fn create_dir(&self, dir: &str) {
        self.run_cmd(
            ftab::ensure_mountpoint_dir_command(dir),
            "创建挂载点目录".to_string(),
        );
    }

    // ---------------------------------------------- 停用/恢复（入队）、挂载/卸载

    fn toggle_disabled(&self, line_no: usize) {
        if self.busy.get() {
            self.toast("上一个操作还在进行中，请稍候");
            return;
        }
        let preview = self.preview_doc();
        let cur = preview
            .entries()
            .find(|e| e.line_no == line_no)
            .map(|e| e.disabled);
        let Some(cur) = cur else {
            self.toast(&format!("未找到第 {line_no} 行的条目"));
            return;
        };
        let op = PendingOp::SetDisabled {
            line_no,
            disabled: !cur,
        };
        match self.enqueue(op) {
            Err(e) => self.toast(&format!("校验失败：{e}")),
            Ok(()) => {
                self.refresh_views();
                let msg = if cur { "恢复" } else { "停用" };
                self.toast(&format!("已入队{msg}（点 Apply 写入磁盘）"));
            }
        }
    }

    // ---------------------------------------------- 物理删除（D7.5 修订）

    /// 删除按钮入口：待定新增行 → 直接撤掉那条 Add（未落盘，零成本）；
    /// 已在磁盘的行 → 确认弹窗 → 入队 `PendingOp::Remove`（Apply 才真正删）。
    fn confirm_delete(&self, line_no: usize) {
        if self.busy.get() {
            self.toast("上一个操作还在进行中，请稍候");
            return;
        }
        let base = self.doc.borrow().clone();

        // 待定新增（含 Remove 后复用行号的再新增）：删的是队列操作，不涉磁盘
        let mut cand = self.queue.borrow().clone();
        if let Some(idx) = cand.add_index_producing(&base, line_no) {
            // 连带撤掉其后针对该行的依赖操作（Edit/SetDisabled/Remove），
            // 否则 preview 会留下孤儿操作而报错。
            let mut kill = vec![idx];
            for (i, op) in cand.ops().iter().enumerate().skip(idx + 1) {
                let targets = match op {
                    PendingOp::Edit { line_no: l, .. }
                    | PendingOp::SetDisabled { line_no: l, .. }
                    | PendingOp::Remove { line_no: l, .. } => *l == line_no,
                    PendingOp::Add(_) => false,
                };
                if targets {
                    kill.push(i);
                }
            }
            for i in kill.into_iter().rev() {
                cand.remove_at(i);
            }
            match cand.preview(&base) {
                Ok(_) => {
                    *self.queue.borrow_mut() = cand;
                    self.refresh_views();
                    self.toast("已撤销待定新增（尚未写入磁盘）");
                }
                Err(e) => self.toast(&format!("撤销失败：{e}")),
            }
            return;
        }

        // 已在磁盘：确认弹窗（用户要求：物理删除必须弹窗警告）
        let preview = self.preview_doc();
        let Some(e) = preview.entries().find(|e| e.line_no == line_no).cloned() else {
            self.toast(&format!("未找到第 {line_no} 行的条目"));
            return;
        };
        let Some(win) = self.window() else {
            return;
        };
        let mp = ftab::unescape_field(&e.mountpoint);
        let dialog = adw::MessageDialog::new(
            Some(&win),
            Some(&format!("物理删除第 {line_no} 行？")),
            Some(&format!(
                "将从 {} 物理删除这一行（不可恢复）：\n\n                 {mp}\n{} · {} · {}\n\n                 与「停用」不同：停用只是行首加 #，可一键恢复；物理删除后该行不再存在，\n                 只能靠「回滚上次写入」找回（需写入前留有快照）。\n\n                 确认后此操作进入待定队列，点「Apply 应用并写入」才写入磁盘；\n                 写入前可随时在队列面板撤销。",
                ftab::FSTAB_PATH,
                ftab::unescape_field(&e.source),
                e.vfstype,
                ftab::unescape_field(&e.options),
            )),
        );
        dialog.add_response("cancel", "取消");
        dialog.add_response("del", "确认删除");
        dialog.set_response_appearance("del", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, move |d: &adw::MessageDialog, resp| {
            d.close();
            if resp == "del" {
                with_inner(|i| i.do_delete(line_no, mp.clone()));
            }
        });
        dialog.present();
    }

    fn do_delete(&self, line_no: usize, mountpoint: String) {
        match self.enqueue(PendingOp::Remove {
            line_no,
            mountpoint,
        }) {
            Err(e) => self.toast(&format!("入队失败：{e}")),
            Ok(()) => {
                self.refresh_views();
                self.toast("已入队物理删除（点 Apply 写入磁盘）");
            }
        }
    }

    fn entry_argv(
        &self,
        line_no: usize,
        which: impl Fn(&FstabEntry) -> Vec<String>,
    ) -> Option<(Vec<String>, String)> {
        // 立即挂载读的是磁盘上的 /etc/fstab → 用 base 文档（不是预览）。
        let doc = self.doc.borrow();
        let e = doc.entries().find(|e| e.line_no == line_no)?;
        let argv = which(e);
        let what = if e.is_swap() { "swap" } else { &e.mountpoint };
        Some((argv, what.to_string()))
    }

    fn mount_entry(&self, line_no: usize) {
        let Some((argv, what)) = self.entry_argv(line_no, ftab::mount_command) else {
            self.toast(&format!("未找到第 {line_no} 行的条目"));
            return;
        };
        self.run_cmd(argv, format!("挂载 {what}"));
    }

    fn umount_entry(&self, line_no: usize) {
        let Some((argv, what)) = self.entry_argv(line_no, ftab::umount_command) else {
            self.toast(&format!("未找到第 {line_no} 行的条目"));
            return;
        };
        self.run_cmd(argv, format!("卸载 {what}"));
    }

    /// 后台执行一条外部命令（pkexec mount / umount / mkdir 等）。
    fn run_cmd(&self, argv: Vec<String>, desc: String) {
        if !self.enter_busy() {
            return;
        }
        std::thread::spawn(move || {
            let res: CmdResult = match std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .output()
            {
                Ok(o) => {
                    let out = String::from_utf8_lossy(&o.stdout);
                    let err = String::from_utf8_lossy(&o.stderr);
                    if o.status.success() {
                        let mut msg = out.trim().to_string();
                        if !err.trim().is_empty() {
                            if !msg.is_empty() {
                                msg.push('\n');
                            }
                            msg.push_str(err.trim());
                        }
                        Ok(msg)
                    } else {
                        let rc = o
                            .status
                            .code()
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "信号".into());
                        let detail = if err.trim().is_empty() { out } else { err };
                        Err(format!("{desc}失败（退出码 {rc}）：{}", detail.trim()))
                    }
                }
                Err(e) => Err(format!("无法执行 {}：{e}", argv[0])),
            };
            let res = RefCell::new(Some((desc, res)));
            glib::source::idle_add(move || {
                if let Some((desc, r)) = res.take() {
                    with_inner(|i| i.apply_cmd(&desc, r));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_cmd(&self, desc: &str, res: CmdResult) {
        self.busy.set(false);
        match res {
            Ok(msg) => {
                if msg.is_empty() {
                    self.toast(&format!("{desc}成功"));
                } else {
                    self.toast(&format!("{desc}成功：{msg}"));
                }
            }
            Err(e) => self.toast(&e),
        }
        self.update_busy();
        // 操作后回读 findmnt 刷新状态列（D7.6）
        self.refresh_statuses();
    }

    // ---------------------------------------------------------------- 保存（Apply）

    /// Apply：preview 全队校验 → 序列化 → 7.4 闸门 → 7.5 写入链路。
    /// 失败（Invalid / WriteFailed）队列保留；成功清队并回读。
    fn save(&self) {
        if self.queue.borrow().is_empty() {
            self.toast("没有待定操作（先新增/编辑/停用/删除入队，再点 Apply）");
            return;
        }
        let preview = {
            let base = self.doc.borrow();
            self.queue.borrow().preview(&base)
        };
        let preview = match preview {
            Ok(p) => p,
            Err(e) => {
                self.toast(&format!(
                    "队列预览失败：{e}（队列保留，可撤销问题操作或清空）"
                ));
                return;
            }
        };
        if !self.enter_busy() {
            return;
        }
        let content = ftab::serialize(&preview);
        let prev = self.loaded_text.borrow().clone();
        std::thread::spawn(move || {
            let step = match ftab::validate_text(&content) {
                Err(errors) => SaveStep::Invalid(errors),
                Ok(warnings) => {
                    match ftab::write_fstab(std::path::Path::new(ftab::FSTAB_PATH), &content) {
                        Ok(outcome) => SaveStep::Done { warnings, outcome },
                        Err(e) => SaveStep::WriteFailed(e),
                    }
                }
            };
            let cell = RefCell::new(Some((step, content, prev, preview)));
            glib::source::idle_add(move || {
                if let Some((step, content, prev, preview)) = cell.take() {
                    with_inner(|i| i.apply_save(step, content, prev, preview));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_save(&self, step: SaveStep, content: String, prev: String, preview: FstabDocument) {
        let is_done = matches!(step, SaveStep::Done { .. });
        self.busy.set(false);
        match step {
            SaveStep::Invalid(errors) => {
                let first = errors.first().cloned().unwrap_or_default();
                self.toast(&format!(
                    "校验失败：{first}（共 {} 项，未写入；队列保留）",
                    errors.len()
                ));
            }
            SaveStep::WriteFailed(e) => {
                self.toast(&format!("写入失败（已回滚，队列保留）：{e}"));
            }
            SaveStep::Done { warnings, outcome } => {
                // 成功：预览成为新 base，队列清空（GOAL 7.8「Apply 后队列清空」）。
                *self.doc.borrow_mut() = preview;
                *self.loaded_text.borrow_mut() = content;
                *self.prev_content.borrow_mut() = Some(prev);
                self.queue.borrow_mut().clear();
                let bak = outcome
                    .backup
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "（无备份）".into());
                let mut msg = format!(
                    "已写入 {}（备份 {bak}，daemon-reload RC={}{}）",
                    ftab::FSTAB_PATH,
                    outcome.reload_rc,
                    if outcome.elevated { "，已提权" } else { "" }
                );
                if let Some(w) = warnings.first() {
                    msg.push_str(&format!("；警告：{w}"));
                }
                self.toast(&msg);
            }
        }
        self.refresh_views();
        if is_done {
            // 回读磁盘确认往返一致
            self.refresh();
        }
    }

    // ---------------------------------------------------------------- 回滚

    fn confirm_rollback(&self) {
        let Some(prev) = self.prev_content.borrow().clone() else {
            self.toast("没有可回滚的内容（尚未成功写入过）");
            return;
        };
        let Some(win) = self.window() else {
            return;
        };
        let dialog = adw::MessageDialog::new(
            Some(&win),
            Some("回滚到上一次写入前的内容？"),
            Some(&format!(
                "将 {} 恢复为写入前的原文（来自本页保留的快照）。\n\
                 当前列表中未保存的更改与待定操作会一并丢弃。",
                ftab::FSTAB_PATH
            )),
        );
        dialog.add_response("cancel", "取消");
        dialog.add_response("ok", "确认回滚");
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, move |d: &adw::MessageDialog, resp| {
            d.close();
            if resp == "ok" {
                let prev = prev.clone();
                with_inner(|i| i.do_rollback(prev));
            }
        });
        dialog.present();
    }

    fn do_rollback(&self, prev: String) {
        if !self.enter_busy() {
            return;
        }
        std::thread::spawn(move || {
            let res = ftab::write_fstab(std::path::Path::new(ftab::FSTAB_PATH), &prev);
            let res = RefCell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(r) = res.take() {
                    with_inner(|i| i.apply_rollback(r));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_rollback(&self, res: Result<WriteOutcome, String>) {
        self.busy.set(false);
        match res {
            Ok(_) => {
                *self.prev_content.borrow_mut() = None;
                self.queue.borrow_mut().clear();
                self.toast("已回滚到写入前的内容（待定操作已丢弃）");
                self.refresh_views();
                self.refresh();
            }
            Err(e) => {
                self.toast(&format!("回滚失败（已从备份二次回滚）：{e}"));
                self.update_busy();
            }
        }
    }
}
// ---------------------------------------------------------------------------
// 构建页面
// ---------------------------------------------------------------------------

pub fn build() -> FstabPage {
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
    let title = gtk::Label::new(Some("磁盘挂载"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    root_box.append(&title);

    let subtitle = gtk::Label::new(Some(
        "KDE 分区管理器式布局：左侧设备面板 · 中部分区条与树视图 · 右侧信息面板（UUID/类型/标签自动读取）· \
         一切变更先进待定操作队列，点 Apply 统一校验写入（findmnt --verify + 备份 + 回读 + daemon-reload）。",
    ));
    subtitle.add_css_class("dim-label");
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_wrap(true);
    subtitle.set_margin_top(2);
    root_box.append(&subtitle);

    // ---------- 操作条（滚动区外常驻） ----------
    let (bar_card, bar_content) = card();
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let refresh_btn = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh_btn.set_tooltip_text(Some("重新读取 /etc/fstab 与挂载状态（队列非空会先确认）"));
    bar.append(&refresh_btn);

    let add_btn = gtk::Button::with_label("新增条目");
    bar.append(&add_btn);

    let op_status = gtk::Label::new(Some("正在加载…"));
    op_status.add_css_class("dim-label");
    op_status.add_css_class("caption");
    op_status.set_halign(gtk::Align::Start);
    op_status.set_hexpand(true);
    bar.append(&op_status);

    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);
    bar.append(&spinner);

    let rollback_btn = gtk::Button::with_label("回滚上次写入");
    rollback_btn.set_tooltip_text(Some(
        "恢复为上一次成功写入前的内容（本页内存快照；D7.3 一键回滚）",
    ));
    rollback_btn.set_sensitive(false);
    bar.append(&rollback_btn);

    let save_btn = gtk::Button::with_label("Apply 应用并写入");
    save_btn.add_css_class("suggested-action");
    save_btn.set_tooltip_text(Some(
        "把待定操作队列一次性写入：自家解析闸门 + findmnt --verify → 备份 .linbox.bak → 落盘 → 回读比对 → daemon-reload",
    ));
    save_btn.set_sensitive(false);
    bar.append(&save_btn);

    bar_content.append(&bar);
    root_box.append(&bar_card);

    // ---------- 主区三分栏 ----------
    let main = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    main.set_vexpand(true);
    root_box.append(&main);

    // 错误态整页（7.10：fstab 读失败/不存在 → UI 规范 §8 StatusPage + 主操作）
    let fatal_sp = adw::StatusPage::new();
    fatal_sp.set_icon_name(Some("dialog-error-symbolic"));
    fatal_sp.set_title("读取 /etc/fstab 失败");
    fatal_sp.set_description(Some("请检查文件是否存在与权限"));
    let fatal_retry = gtk::Button::with_label("重试");
    fatal_retry.add_css_class("suggested-action");
    fatal_retry.connect_clicked(|_| with_inner(|i| i.confirm_refresh()));
    fatal_sp.set_child(Some(&fatal_retry));
    fatal_sp.set_visible(false);
    root_box.append(&fatal_sp);

    // ---------- 状态栏（7.10：N 个待定操作 · 读写状态 · fstab 原文/备份入口） ----------
    let (sb_card, sb_content) = card();
    let sb = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let sb_count = gtk::Label::new(Some("无待定操作"));
    sb_count.add_css_class("dim-label");
    sb_count.add_css_class("caption");
    sb_count.set_halign(gtk::Align::Start);
    sb_count.set_tooltip_text(Some("只入队不落盘；点 Apply 统一写入"));
    sb.append(&sb_count);
    let sb_mode = gtk::Label::new(Some("正在加载…"));
    sb_mode.add_css_class("dim-label");
    sb_mode.add_css_class("caption");
    sb_mode.set_halign(gtk::Align::Start);
    sb_mode.set_hexpand(true);
    sb.append(&sb_mode);
    let raw_btn = gtk::Button::with_label("fstab 原文");
    raw_btn.add_css_class("flat");
    raw_btn.set_tooltip_text(Some("查看 /etc/fstab 当前原文（只读）"));
    sb.append(&raw_btn);
    let bak_btn = gtk::Button::with_label("备份：/etc/fstab.linbox.bak");
    bak_btn.add_css_class("flat");
    bak_btn.set_tooltip_text(Some("查看上次成功写入前的备份（只读）；首次写入前不存在"));
    sb.append(&bak_btn);
    sb_content.append(&sb);
    root_box.append(&sb_card);

    // 左：设备面板 + 队列面板
    let left = gtk::Box::new(gtk::Orientation::Vertical, 8);
    left.set_size_request(262, -1);
    main.append(&left);

    let (dev_card, dev_content) = card();
    dev_card.set_vexpand(true);
    let dev_title = gtk::Label::new(Some("设备（lsblk · 只读）"));
    dev_title.add_css_class("title-4");
    dev_title.set_halign(gtk::Align::Start);
    dev_content.append(&dev_title);
    let dev_scroll = gtk::ScrolledWindow::new();
    dev_scroll.set_vexpand(true);
    dev_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    let dev_list = gtk::ListBox::new();
    dev_list.add_css_class("boxed-list");
    dev_list.set_selection_mode(gtk::SelectionMode::Single);
    dev_scroll.set_child(Some(&dev_list));
    dev_content.append(&dev_scroll);
    // lsblk 读失败错误态（7.10：面板内 StatusPage + 主操作）
    // 面板槽位小，StatusPage 自然高 ~390 会被压到最小 58px（内容不可见）→ 外包滚动壳兜住
    let dev_err_sp = adw::StatusPage::new();
    dev_err_sp.set_icon_name(Some("dialog-error-symbolic"));
    dev_err_sp.set_title("读取磁盘分区失败");
    dev_err_sp.set_description(Some("lsblk 执行失败，设备面板暂不可用"));
    let dev_retry = gtk::Button::with_label("重试");
    dev_retry.connect_clicked(|_| with_inner(|i| i.confirm_refresh()));
    dev_err_sp.set_child(Some(&dev_retry));
    let dev_err_sw = gtk::ScrolledWindow::new();
    dev_err_sw.set_child(Some(&dev_err_sp));
    dev_err_sw.set_propagate_natural_height(true);
    dev_err_sw.set_min_content_height(120);
    dev_err_sw.set_max_content_height(200);
    dev_err_sw.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    dev_err_sw.set_visible(false);
    dev_content.append(&dev_err_sw);
    left.append(&dev_card);

    let (queue_card, queue_content) = card();
    let queue_head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let queue_status = gtk::Label::new(Some("待定操作（0）"));
    queue_status.add_css_class("title-4");
    queue_status.set_halign(gtk::Align::Start);
    queue_status.set_hexpand(true);
    queue_head.append(&queue_status);
    let undo_btn = gtk::Button::with_label("撤销");
    undo_btn.add_css_class("flat");
    undo_btn.set_tooltip_text(Some("撤销最后一条待定操作（Ctrl+Z）"));
    undo_btn.set_sensitive(false);
    queue_head.append(&undo_btn);
    let clear_btn = gtk::Button::with_label("清空");
    clear_btn.add_css_class("flat");
    clear_btn.set_tooltip_text(Some("清空全部待定操作（对磁盘零影响）"));
    clear_btn.set_sensitive(false);
    queue_head.append(&clear_btn);
    queue_content.append(&queue_head);
    let queue_scroll = gtk::ScrolledWindow::new();
    queue_scroll.set_min_content_height(96);
    queue_scroll.set_max_content_height(240);
    queue_scroll.set_propagate_natural_height(true);
    queue_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    let queue_list = gtk::ListBox::new();
    queue_list.add_css_class("boxed-list");
    queue_list.set_selection_mode(gtk::SelectionMode::None);
    queue_scroll.set_child(Some(&queue_list));
    queue_content.append(&queue_scroll);
    left.append(&queue_card);

    // 中：分区条 + （树视图 + fstab 列表 + 表单）滚动区
    let center = gtk::Box::new(gtk::Orientation::Vertical, 8);
    center.set_hexpand(true);
    main.append(&center);

    let (bar_area_card, bar_area_content) = card();
    let bar_caption = gtk::Label::new(Some("分区条"));
    bar_caption.add_css_class("title-4");
    bar_caption.set_halign(gtk::Align::Start);
    bar_caption.set_ellipsize(gtk::pango::EllipsizeMode::End);
    bar_area_content.append(&bar_caption);
    let bar_area = gtk::DrawingArea::new();
    bar_area.set_content_height(56);
    bar_area.set_hexpand(true);
    bar_area.set_vexpand(false);
    bar_area.set_draw_func(move |_, ctx, w, h| with_inner(|i| i.draw_bar(ctx, w, h)));
    let click = gtk::GestureClick::new();
    let area_probe = bar_area.clone();
    click.connect_pressed(move |_, _n, x, _y| {
        let w = area_probe.width() as f64;
        with_inner(|i| i.on_bar_click(x, w));
    });
    bar_area.add_controller(click);
    bar_area_content.append(&bar_area);
    center.append(&bar_area_card);

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_vexpand(true);
    scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    // 空态用 Overlay 铺满中栏视口（scroll_box 只拿最小分配，直接放里面会被压到 58px）
    let mid_overlay = gtk::Overlay::new();
    mid_overlay.set_child(Some(&scroller));
    center.append(&mid_overlay);

    let scroll_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    scroller.set_child(Some(&scroll_box));

    // 树视图卡片
    let (tree_card, tree_content) = card();
    let tree_title = gtk::Label::new(Some("分区树视图（状态/fstab 行号对照 · 点击与分区条联动）"));
    tree_title.add_css_class("title-4");
    tree_title.set_halign(gtk::Align::Start);
    tree_content.append(&tree_title);
    let tree_scroll = gtk::ScrolledWindow::new();
    tree_scroll.set_min_content_height(140);
    tree_scroll.set_max_content_height(340);
    tree_scroll.set_propagate_natural_height(true);
    tree_scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    let tree_list = gtk::ListBox::new();
    tree_list.add_css_class("boxed-list");
    tree_list.set_selection_mode(gtk::SelectionMode::Single);
    tree_scroll.set_child(Some(&tree_list));
    tree_content.append(&tree_scroll);
    scroll_box.append(&tree_card);

    // fstab 列表卡片
    let (list_card, list_content) = card();
    let list_status = gtk::Label::new(Some("正在加载…"));
    list_status.add_css_class("dim-label");
    list_status.add_css_class("caption");
    list_status.set_halign(gtk::Align::Start);
    list_content.append(&list_status);

    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list_content.append(&list);
    scroll_box.append(&list_card);

    // 条目空态（7.10：UI 规范 §8 StatusPage + 主操作）→ Overlay 滚动壳，防被压扁
    let empty_sp = adw::StatusPage::new();
    empty_sp.set_icon_name(Some("view-list-symbolic"));
    empty_sp.set_title("暂无挂载条目");
    empty_sp.set_description(Some("/etc/fstab 中没有可显示的条目"));
    let empty_add = gtk::Button::with_label("新增条目");
    empty_add.add_css_class("suggested-action");
    empty_add.connect_clicked(|_| with_inner(|i| i.start_add()));
    empty_sp.set_child(Some(&empty_add));
    let empty_sw = gtk::ScrolledWindow::new();
    empty_sw.set_child(Some(&empty_sp));
    empty_sw.set_propagate_natural_height(true);
    empty_sw.set_min_content_height(180);
    empty_sw.set_max_content_height(300);
    empty_sw.set_halign(gtk::Align::Center);
    empty_sw.set_valign(gtk::Align::Center);
    empty_sw.set_size_request(420, -1);
    empty_sw.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    empty_sw.set_visible(false);
    mid_overlay.add_overlay(&empty_sw);

    // ---------- 新增/编辑表单（Revealer 展开/收起） ----------
    let (form_card, form_content) = card();
    let form_title = gtk::Label::new(Some("新增条目"));
    form_title.add_css_class("title-4");
    form_title.set_halign(gtk::Align::Start);
    form_content.append(&form_title);

    // ComboRow/EntryRow/SpinRow 是 ListBoxRow 子类，必须放进
    // PreferencesGroup（内部是 ListBox）才会正常激活（全仓既有约定）。
    let group = adw::PreferencesGroup::new();
    form_content.append(&group);

    let part_combo = adw::ComboRow::builder()
        .title("分区选择")
        .subtitle("枚举本机分区（lsblk），选中自动填来源与类型")
        .model(&gtk::StringList::new(&["（读取分区中…）"]))
        .build();
    group.add(&part_combo);

    let f_source = adw::EntryRow::builder()
        .title("来源 source（UUID=… / LABEL=… / /dev/sdX1）")
        .build();
    group.add(&f_source);

    let f_mount = adw::EntryRow::builder()
        .title("挂载点（如 /mnt/data；swap 填 none）")
        .build();
    group.add(&f_mount);

    // 文件系统类型（D7.10 禁手输）：点击下拉，模型 = FSTYPE_OPTIONS 全量清单
    let f_types = gtk::StringList::new(&[]);
    for t in ftab::FSTYPE_OPTIONS {
        f_types.append(t);
    }
    let f_type = adw::ComboRow::builder()
        .title("文件系统类型（点击选择…）")
        .subtitle("下拉全量清单，自动探测选 auto")
        .model(&f_types)
        .build();
    f_type.set_selected(
        ftab::FSTYPE_OPTIONS
            .iter()
            .position(|t| *t == "ext4")
            .unwrap_or(0) as u32,
    );
    group.add(&f_type);

    // 挂载选项字段（D7.9）：只读展示 + 点击整行弹勾选窗，禁止自由输入
    let f_opts = adw::ActionRow::builder()
        .title("挂载选项（点击选择…）")
        .subtitle("defaults,relatime")
        .activatable(true)
        .build();
    let opts_hint = gtk::Label::new(Some("点击选择…"));
    opts_hint.add_css_class("dim-label");
    opts_hint.add_css_class("caption");
    f_opts.add_suffix(&opts_hint);
    f_opts.set_tooltip_text(Some(
        "点击弹出勾选窗（D7.9）：首屏核心选项 + 「全部选项」折叠 7 组清单；禁止手输",
    ));
    group.add(&f_opts);

    let f_dump = adw::SpinRow::builder()
        .title("dump（开机备份到 dump：0 / 1）")
        .adjustment(&gtk::Adjustment::new(0.0, 0.0, 9.0, 1.0, 2.0, 0.0))
        .digits(0)
        .build();
    group.add(&f_dump);

    let f_pass = adw::SpinRow::builder()
        .title("pass（fsck 顺序：0=不检查，根分区 1，其余 2）")
        .adjustment(&gtk::Adjustment::new(0.0, 0.0, 9.0, 1.0, 2.0, 0.0))
        .digits(0)
        .build();
    group.add(&f_pass);

    let form_btns = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let submit_btn = gtk::Button::with_label("加入待定队列");
    submit_btn.add_css_class("suggested-action");
    form_btns.append(&submit_btn);
    let cancel_btn = gtk::Button::with_label("收起");
    cancel_btn.add_css_class("flat");
    form_btns.append(&cancel_btn);
    form_content.append(&form_btns);

    let form_revealer = gtk::Revealer::new();
    form_revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    form_revealer.set_transition_duration(200);
    form_revealer.set_child(Some(&form_card));
    scroll_box.append(&form_revealer);

    // 右：信息面板
    let right = gtk::Box::new(gtk::Orientation::Vertical, 8);
    right.set_size_request(302, -1);
    main.append(&right);
    let (info_card, info_content) = card();
    info_card.set_vexpand(true);
    let info_title = gtk::Label::new(Some("信息面板（自动读取 · 零手输）"));
    info_title.add_css_class("title-4");
    info_title.set_halign(gtk::Align::Start);
    info_content.append(&info_title);
    let info_scroll = gtk::ScrolledWindow::new();
    info_scroll.set_vexpand(true);
    info_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    let info_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    info_scroll.set_child(Some(&info_box));
    info_content.append(&info_scroll);
    right.append(&info_card);

    // ---------- 组装 Inner ----------
    let inner = Rc::new(Inner {
        toast_overlay: toast_overlay.clone(),
        refresh_btn,
        add_btn,
        rollback_btn,
        save_btn,
        op_status,
        spinner,
        dev_list,
        dev_map: RefCell::new(Vec::new()),
        queue_list,
        queue_status,
        undo_btn,
        clear_btn,
        bar_area,
        bar_caption,
        tree_list,
        tree_map: RefCell::new(vec![None]),
        list,
        list_status,
        list_card,
        empty_sw,
        form_revealer,
        form_title,
        part_combo,
        f_source,
        f_mount,
        f_type,
        f_opts,
        f_dump,
        f_pass,
        submit_btn,
        info_box,
        main_box: main,
        fatal_sp,
        dev_scroll,
        dev_err_sp,
        dev_card,
        dev_err_sw,
        sb_count,
        sb_mode,
        doc: RefCell::new(FstabDocument::new()),
        json: RefCell::new(None),
        loaded_text: RefCell::new(String::new()),
        prev_content: RefCell::new(None),
        disks: RefCell::new(Vec::new()),
        usage: RefCell::new(Vec::new()),
        parts: RefCell::new(Vec::new()),
        queue: RefCell::new(OpQueue::new()),
        sel: Cell::new(None),
        syncing: Cell::new(false),
        editing: Cell::new(None),
        busy: Cell::new(false),
        writable: Cell::new(false),
    });
    // 注册全局强引用：子线程的 idle 回调通过它回主线程刷新 UI。
    INNER.with(|i| *i.borrow_mut() = Some(Rc::clone(&inner)));

    inner.refresh_btn.connect_clicked(clone!(
        #[strong]
        inner,
        move |_| inner.confirm_refresh()
    ));
    inner.add_btn.connect_clicked(clone!(
        #[strong]
        inner,
        move |_| inner.start_add()
    ));
    inner.save_btn.connect_clicked(clone!(
        #[strong]
        inner,
        move |_| inner.save()
    ));
    inner.rollback_btn.connect_clicked(clone!(
        #[strong]
        inner,
        move |_| inner.confirm_rollback()
    ));
    inner.undo_btn.connect_clicked(clone!(
        #[strong]
        inner,
        move |_| inner.undo_last()
    ));
    inner.clear_btn.connect_clicked(clone!(
        #[strong]
        inner,
        move |_| inner.clear_queue()
    ));
    inner.submit_btn.connect_clicked(clone!(
        #[strong]
        inner,
        move |_| inner.submit_form()
    ));
    cancel_btn.connect_clicked(clone!(
        #[strong]
        inner,
        move |_| inner.hide_form()
    ));
    // 挂载选项字段：点击整行弹勾选窗（D7.9 禁手输）
    inner.f_opts.connect_activated(clone!(
        #[strong]
        inner,
        move |_| {
            let cur = inner.opts_text();
            inner.open_options_picker(&cur, |s| with_inner(|i| i.set_opts_text(&s)));
        }
    ));
    inner.part_combo.connect_selected_notify(clone!(
        #[strong]
        inner,
        move |row| inner.on_part_selected(row.selected())
    ));
    // 状态栏入口（7.10）：fstab 原文 / 备份文件
    raw_btn.connect_clicked(clone!(
        #[strong]
        inner,
        move |_| inner.show_fstab_raw()
    ));
    bak_btn.connect_clicked(clone!(
        #[strong]
        inner,
        move |_| inner.show_backup_raw()
    ));

    // 设备面板行选中 → 三方联动
    inner.dev_list.connect_row_selected(clone!(
        #[strong]
        inner,
        move |_, row| {
            if inner.syncing.get() {
                return;
            }
            let Some(row) = row else {
                return;
            };
            let idx = row.index() as usize;
            if let Some(&s) = inner.dev_map.borrow().get(idx) {
                inner.sync_sel(Some(s));
            }
        }
    ));

    // 树视图行选中 → 三方联动
    inner.tree_list.connect_row_selected(clone!(
        #[strong]
        inner,
        move |_, row| {
            if inner.syncing.get() {
                return;
            }
            let Some(row) = row else {
                return;
            };
            let idx = row.index() as usize;
            if let Some(Some(s)) = inner.tree_map.borrow().get(idx) {
                let (d, p) = *s;
                inner.sync_sel(Some(Sel::Part(d, p)));
            }
        }
    ));

    // Ctrl+Z：撤销最后一条待定操作（焦点在输入框时交给输入框自身撤销）
    let key = gtk::EventControllerKey::new();
    key.connect_key_pressed(move |_, k, _, state| {
        if k == gdk::Key::z && state.contains(gdk::ModifierType::CONTROL_MASK) {
            let mut handled = false;
            with_inner(|i| {
                if !i.queue.borrow().is_empty() {
                    i.undo_last();
                    handled = true;
                }
            });
            if handled {
                return glib::Propagation::Stop;
            }
        }
        glib::Propagation::Proceed
    });
    root_box.add_controller(key);

    // 初次加载
    inner.refresh();

    FstabPage {
        root: toast_overlay,
    }
}
