//! 备忘录页面（展示层 · 仅 UI）。
//!
//! 功能：
//! - 左栏：条目列表，支持新建 / 删除
//! - 右栏：标题 + 正文（正文中可夹图片，像 Word 那样）+ 附件（任意文件：ppt / docx / …）
//! - 顶部：导入 ZIP / 导出 ZIP（增量导入，已有条目跳过）
//!
//! 正文里的图片在 `content.txt` 中是一个 U+FFFC 占位字符，按顺序对应
//! `entries/<id>/inline/0.png`、`1.png` …；`files/` 下的才是附件。
//! （详见 `storage` 模块头注释）

pub mod storage;
pub mod sync;
pub mod git_store;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::gdk;
use gtk::gdk::gdk_pixbuf::{Colorspace, InterpType, Pixbuf};
use gtk::gio;
use gtk::pango;

use storage::MemoMeta;

/// 正文中「这里有一张图」的占位字符（U+FFFC 对象替换字符）。
const OBJ: char = '\u{FFFC}';
/// 内嵌图片的最大宽度：超过就等比缩小，免得一张 4K 截图把正文撑爆。
const MAX_INLINE_W: i32 = 640;

pub struct NotepadPage {
    root: adw::ToastOverlay,
}

struct Inner {
    list: gtk::ListBox,
    /// 左栏「还没有条目」提示（与列表互斥显示）。
    left_empty: gtk::Label,
    left_scroll: gtk::ScrolledWindow,
    /// 右栏：空态提示 ↔ 编辑器。
    editor_stack: gtk::Stack,
    /// 列表行缓存：(条目 id, 行控件)。改标题时直接改这一行的文字，
    /// 不必整表重建（重建会丢选中态、把光标从正文里踢出去）。
    rows: RefCell<Vec<(String, adw::ActionRow)>>,
    title_entry: adw::EntryRow,
    buffer: gtk::TextBuffer,
    /// 附件列表（任意文件）。
    att_box: gtk::FlowBox,
    /// `att_box` 当前显示的文件名（顺序与行一致，供「打开/删除」定位）。
    att_names: RefCell<Vec<String>>,
    att_open: gtk::Button,
    /// 在文件管理器中打开该附件所在目录。
    att_show: gtk::Button,
    att_del: gtk::Button,
    current_id: RefCell<Option<String>>,
    /// 上次写盘的正文内嵌图片指纹：一样就跳过，
    /// 否则每敲一个字都要把几 MB 的图重写一遍。
    inline_fp: RefCell<Vec<u64>>,
    /// 程序化加载期间置位：挡住 set_text 触发的 changed，避免把内容写回错的条目。
    loading: Cell<bool>,
    /// 内容是否有未同步的改动。
    dirty: Cell<bool>,
    toast: adw::ToastOverlay,
    /// 正文里 URL 的超链接样式（下划线 + 链接色）。
    link_tag: gtk::TextTag,
    /// 正文右键菜单里「打开图片 / 在文件夹中显示」两个 action
    /// （选中内联图片时才启用 → 靠 hidden-when 才显示）。
    inline_actions: gio::SimpleActionGroup,
    /// 主窗口引用（用于给子对话框设置 transient_for）。
    window: RefCell<Option<gtk::Window>>,
}

impl Inner {
    fn toast(&self, msg: &str) {
        self.toast.add_toast(adw::Toast::new(msg));
    }

    /// 所在窗口（给文件对话框当 transient parent）。
    fn window(&self) -> Option<gtk::Window> {
        self.toast
            .root()
            .and_then(|r| r.downcast::<gtk::Window>().ok())
    }
}

impl NotepadPage {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }
}

thread_local! {
    /// 当前页面的**强**引用。
    ///
    /// 必须是 `Rc` 而不是 `Weak`：`build()` 返回后局部 `inner` 就析构了，
    /// 只存弱引用的话所有回调 upgrade 全部失败 —— 症状就是「点了完全没反应」
    ///（新建 / 保存 / 粘贴图片 / 选中条目一起失效）。
    static INNER: RefCell<Option<Rc<Inner>>> = const { RefCell::new(None) };

    /// 上次 `save_current` 实际写盘的时刻（毫秒级 Unix 时间戳）。
    /// 用于节流：正文改动 < 200 ms 时跳过写盘，避免逐键重写整个 ZIP。
    static LAST_SAVE_MS: Cell<u64> = const { Cell::new(0) };
}

/// 返回当前时刻的毫秒级 Unix 时间戳。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn with_inner<F: FnOnce(&Inner)>(f: F) {
    let Some(inner) = INNER.with(|i| i.try_borrow().ok().and_then(|b| b.clone())) else {
        return;
    };
    f(&*inner);
}

/// 同 `with_inner`，但把闭包的返回值带出来（没有 Inner 时返回 `None`）。
fn with_inner_r<T, F: FnOnce(&Inner) -> T>(f: F) -> Option<T> {
    let inner = INNER.with(|i| i.try_borrow().ok().and_then(|b| b.clone()))?;
    Some(f(&*inner))
}

/// 应用退出时释放全局句柄，避免 TLS 析构阶段再碰 GTK 控件。
pub fn shutdown() {
    INNER.with(|i| {
        if let Ok(mut b) = i.try_borrow_mut() {
            *b = None;
        }
    });
}

/// 设置主窗口引用（供子对话框设置 transient_for）。
pub fn set_window(w: &impl IsA<gtk::Window>) {
    with_inner(|i| {
        *i.window.borrow_mut() = Some(w.clone().upcast());
    });
}

/// 备忘录内容是否有未同步的改动。
pub fn is_dirty() -> bool {
    INNER.with(|i| i.try_borrow().ok().and_then(|b| b.as_ref().map(|inner| inner.dirty.get())).unwrap_or(false))
}

/// 把编辑区里还没落盘的内容强制写盘（绕过 200 ms 节流）。
///
/// **关窗前必须调一次。** `save_current(inner, false)` 在节流窗口内（距上次写盘
/// < 200 ms）是**直接 return、不补写**的（没有延迟写定时器），所以快速敲进去的
/// 最后几个字可能根本不在磁盘上 —— 关窗时只 pull+push 不 flush，这几个字既不会
/// 保存也不会被推上远端，用户却以为「保存并同步」成功了。
pub fn flush() {
    with_inner(|i| save_current(i, true));
}

/// 标记内容已同步（清除脏标记）。
pub fn mark_clean() {
    with_inner(|i| i.dirty.set(false));
}

pub fn build() -> NotepadPage {
    let toast = adw::ToastOverlay::new();
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    toast.set_child(Some(&outer));

    // ── 顶部工具栏 ────────────────────────────────────────────────────────
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    toolbar.set_margin_top(6);
    toolbar.set_margin_bottom(6);
    toolbar.set_margin_start(8);
    toolbar.set_margin_end(8);

    let add_btn = gtk::Button::from_icon_name("list-add-symbolic");
    add_btn.set_tooltip_text(Some("新建条目"));
    add_btn.add_css_class("flat");
    let del_btn = gtk::Button::from_icon_name("user-trash-symbolic");
    del_btn.set_tooltip_text(Some("删除当前条目"));
    del_btn.add_css_class("flat");
    let sep = gtk::Separator::new(gtk::Orientation::Vertical);
    sep.set_margin_start(4);
    sep.set_margin_end(4);
    let import_btn = gtk::Button::from_icon_name("document-import-symbolic");
    import_btn.set_tooltip_text(Some("从 ZIP 导入条目"));
    import_btn.add_css_class("flat");
    let export_btn = gtk::Button::from_icon_name("document-export-symbolic");
    export_btn.set_tooltip_text(Some("全部条目导出到 ZIP"));
    export_btn.add_css_class("flat");

    let sep2 = gtk::Separator::new(gtk::Orientation::Vertical);
    sep2.set_margin_start(4);
    sep2.set_margin_end(4);
    let cloud_cfg_btn = gtk::Button::from_icon_name("preferences-system-symbolic");
    cloud_cfg_btn.set_tooltip_text(Some("云同步设置"));
    cloud_cfg_btn.add_css_class("flat");
    let cloud_sync_btn = gtk::Button::from_icon_name("view-refresh-symbolic");
    cloud_sync_btn.set_tooltip_text(Some("同步到云端"));
    cloud_sync_btn.add_css_class("flat");
    // 未配置时灰掉同步按钮
    cloud_sync_btn.set_sensitive(sync::is_configured());

    let pull_btn = gtk::Button::from_icon_name("go-down-symbolic");
    pull_btn.set_tooltip_text(Some("刷新（从云端拉取）"));
    pull_btn.add_css_class("flat");
    pull_btn.set_sensitive(sync::is_configured());
    let push_btn = gtk::Button::from_icon_name("go-up-symbolic");
    push_btn.set_tooltip_text(Some("推送到云端"));
    push_btn.add_css_class("flat");
    push_btn.set_sensitive(sync::is_configured());

    toolbar.append(&add_btn);
    toolbar.append(&del_btn);
    toolbar.append(&sep);
    toolbar.append(&import_btn);
    toolbar.append(&export_btn);
    toolbar.append(&sep2);
    toolbar.append(&cloud_cfg_btn);
    toolbar.append(&cloud_sync_btn);
    toolbar.append(&pull_btn);
    toolbar.append(&push_btn);
    outer.append(&toolbar);

    // ── 主体：左列表 + 右编辑器 ───────────────────────────────────────────
    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_vexpand(true);
    paned.set_position(260);
    // 别让左栏被拖没：关掉收缩后至少保住列表的最小宽度
    paned.set_shrink_start_child(false);
    outer.append(&paned);

    // 左：条目列表
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.add_css_class("boxed-list");
    let left_scroll = gtk::ScrolledWindow::new();
    left_scroll.set_child(Some(&list));
    left_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    left_scroll.set_min_content_width(180);
    left_scroll.set_vexpand(true);

    // 空列表提示：必须放在 ListBox **外面**（塞进 ListBox 会留下一个空行），
    // 用 set_visible 切换。
    let left_empty = gtk::Label::new(Some("还没有条目\n点左上角「+」新建一条"));
    left_empty.add_css_class("dim-label");
    left_empty.set_justify(gtk::Justification::Center);
    left_empty.set_wrap(true);
    left_empty.set_margin_top(12);
    left_empty.set_margin_bottom(12);
    left_empty.set_margin_start(12);
    left_empty.set_margin_end(12);
    left_empty.set_halign(gtk::Align::Center);
    left_empty.set_valign(gtk::Align::Center);
    left_empty.set_vexpand(true);

    let left = gtk::Box::new(gtk::Orientation::Vertical, 0);
    left.append(&left_scroll);
    left.append(&left_empty);
    paned.set_start_child(Some(&left));

    // 右：编辑器
    let right = gtk::Box::new(gtk::Orientation::Vertical, 4);
    right.set_margin_top(4);
    right.set_margin_bottom(4);
    right.set_margin_start(8);
    right.set_margin_end(8);

    let title_entry = adw::EntryRow::new();
    title_entry.set_title("标题");
    right.append(&title_entry);

    let text_view = gtk::TextView::new();
    text_view.set_wrap_mode(gtk::WrapMode::WordChar);
    text_view.set_left_margin(4);
    text_view.set_right_margin(4);
    text_view.set_top_margin(4);
    text_view.set_bottom_margin(4);
    text_view.set_vexpand(true);
    let text_scroll = gtk::ScrolledWindow::new();
    text_scroll.set_child(Some(&text_view));
    text_scroll.set_vexpand(true);
    right.append(&text_scroll);

    // 正文工具条：往光标处插图片
    let text_toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let insert_img_btn = gtk::Button::from_icon_name("image-x-generic-symbolic");
    insert_img_btn.set_label("插入图片");
    insert_img_btn.set_tooltip_text(Some("在光标处插入图片（也可以直接 Ctrl+V 粘贴）"));
    text_toolbar.append(&insert_img_btn);
    right.append(&text_toolbar);

    // 附件区（任意文件：ppt / docx / pdf / …）
    let att_label = gtk::Label::new(Some("附件"));
    att_label.add_css_class("caption");
    att_label.add_css_class("dim-label");
    att_label.set_halign(gtk::Align::Start);
    att_label.set_margin_top(6);
    right.append(&att_label);

    let att_box = gtk::FlowBox::new();
    att_box.set_selection_mode(gtk::SelectionMode::Single);
    att_box.set_column_spacing(6);
    att_box.set_row_spacing(6);
    att_box.set_halign(gtk::Align::Start);
    att_box.set_min_children_per_line(1);
    let att_scroll = gtk::ScrolledWindow::new();
    att_scroll.set_child(Some(&att_box));
    // 纵向滚动 + 限高：否则附件换行后被裁掉，看着像「只加了一个」。
    att_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    att_scroll.set_min_content_height(88);
    att_scroll.set_max_content_height(200);
    att_scroll.set_vexpand(false);
    right.append(&att_scroll);

    let att_toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let add_att_btn = gtk::Button::from_icon_name("list-add-symbolic");
    add_att_btn.set_label("添加附件");
    add_att_btn.set_tooltip_text(Some("添加任意文件（ppt / docx / pdf / …）"));
    let att_open = gtk::Button::with_label("打开");
    att_open.set_tooltip_text(Some("用系统默认应用打开选中的附件"));
    let att_show = gtk::Button::with_label("在文件夹中显示");
    att_show.set_tooltip_text(Some("打开该附件所在的文件夹"));
    let att_del = gtk::Button::with_label("删除");
    att_del.add_css_class("destructive-action");
    att_open.set_sensitive(false);
    att_show.set_sensitive(false);
    att_del.set_sensitive(false);
    att_toolbar.append(&add_att_btn);
    att_toolbar.append(&att_open);
    att_toolbar.append(&att_show);
    att_toolbar.append(&att_del);
    right.append(&att_toolbar);

    // 右：没选中条目时显示空态页（提醒先建条目），选中后切到编辑器。
    let empty_new_btn = gtk::Button::with_label("新建条目");
    empty_new_btn.add_css_class("suggested-action");
    empty_new_btn.set_halign(gtk::Align::Center);
    empty_new_btn.connect_clicked(move |_| with_inner(|i| create_entry(i)));

    let empty_page = adw::StatusPage::new();
    empty_page.set_icon_name(Some("document-edit-symbolic"));
    empty_page.set_title("还没有选中条目");
    empty_page.set_description(Some(
        "备忘录以「条目」为单位：先在左上角点「+」新建一个条目，然后才能写正文、插图片、加附件。",
    ));
    empty_page.set_child(Some(&empty_new_btn));

    let editor_stack = gtk::Stack::new();
    editor_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    // 关掉 homogeneous：否则最小宽度按「所有页面里最宽的」算，窗口会被撑开。
    editor_stack.set_hhomogeneous(false);
    editor_stack.set_vhomogeneous(false);
    editor_stack.add_named(&empty_page, Some("empty"));
    editor_stack.add_named(&right, Some("editor"));
    paned.set_end_child(Some(&editor_stack));

    // URL 的超链接样式：注册进 buffer 的 tag table（整个 buffer 共用一个 tag）
    let link_tag = gtk::TextTag::builder()
        .underline(pango::Underline::Single)
        .foreground(link_color())
        .build();
    text_view.buffer().tag_table().add(&link_tag);

    // ── 内部状态 ───────────────────────────────────────────────────────────
    let inner = Rc::new(Inner {
        list: list.clone(),
        left_empty: left_empty.clone(),
        left_scroll: left_scroll.clone(),
        editor_stack: editor_stack.clone(),
        rows: RefCell::new(Vec::new()),
        title_entry: title_entry.clone(),
        buffer: text_view.buffer(),
        att_box: att_box.clone(),
        att_names: RefCell::new(Vec::new()),
        att_open: att_open.clone(),
        att_show: att_show.clone(),
        att_del: att_del.clone(),
        current_id: RefCell::new(None),
        inline_fp: RefCell::new(Vec::new()),
        loading: Cell::new(false),
        dirty: Cell::new(false),
        toast: toast.clone(),
        link_tag: link_tag.clone(),
        inline_actions: build_inline_actions(),
        window: RefCell::new(None),
    });

    // 强引用必须在接信号之前落进 TLS：否则 build() 一返回 `inner` 就没了，
    // 所有闭包里的弱引用全部 upgrade 失败（表现：点了什么都没反应）。
    INNER.with(|i| *i.borrow_mut() = Some(Rc::clone(&inner)));

    // 深浅色主题切换 → 链接色跟着换，并重刷一遍样式
    adw::StyleManager::default().connect_notify(Some("dark"), |_, _| {
        with_inner(|i| {
            i.link_tag.set_property("foreground", link_color());
            restyle_links(i);
        });
    });

    // 初始加载列表
    refresh_list(&inner);

    // ── 信号 ───────────────────────────────────────────────────────────────
    // 所有闭包都不捕获 Rc（避免循环引用），统一从全局句柄取。
    wire();

    // 新建 / 删除条目
    add_btn.connect_clicked(move |_| with_inner(|i| create_entry(i)));
    del_btn.connect_clicked(move |_| with_inner(|i| delete_current(i)));

    // 正文插图片 / 附件管理
    insert_img_btn.connect_clicked(move |_| with_inner(|i| insert_image_dialog(i)));
    add_att_btn.connect_clicked(move |_| with_inner(|i| pick_attachments(i)));
    att_open.connect_clicked(move |_| with_inner(|i| open_attachment(i)));
    att_show.connect_clicked(move |_| with_inner(|i| show_attachment_in_folder(i)));
    att_del.connect_clicked(move |_| with_inner(|i| confirm_delete_attachment(i)));

    // 导出 / 导入 ZIP
    export_btn.connect_clicked(move |_| with_inner(|i| export_dialog(i)));
    import_btn.connect_clicked(move |_| with_inner(|i| import_dialog(i)));

    // 云同步
    cloud_cfg_btn.connect_clicked(move |_| with_inner(|i| cloud_config_dialog(i)));
    cloud_sync_btn.connect_clicked(move |_| with_inner(|i| start_cloud_sync(i)));
    pull_btn.connect_clicked(move |_| with_inner(|i| start_pull(i)));
    push_btn.connect_clicked(move |_| with_inner(|i| start_push(i)));

    // 剪贴板粘贴图片：正文和标题两处输入框都挂上，谁有焦点谁生效
    attach_paste_controller(&text_view);
    attach_paste_controller(&title_entry);

    // 附件右键菜单（双击打开已按需求去掉，用工具栏的「打开」按钮）
    attach_attachment_gestures(&att_box);
    // 正文 URL Ctrl+点击 + 内联图片右键菜单
    attach_text_view_gestures(&text_view);
    // 正文链接悬停：手型光标 + Ctrl 提示
    attach_link_hover(&text_view);
    // 正文右键菜单追加「打开图片 / 在文件夹中显示」
    attach_inline_menu(&text_view);

    // ── 启动时自动刷新（pull） ──────────────────────────────────────────────
    if sync::is_configured() {
        std::thread::spawn(|| {
            let configs = sync::load_configs();
            if !configs.is_empty() {
                match sync::pull_only(&configs) {
                    Ok(summary) => {
                        if !summary.errors.is_empty() {
                            eprintln!("启动时自动拉取部分失败：{}", summary.errors.join("；"));
                        }
                    }
                    Err(e) => {
                        eprintln!("启动时自动拉取失败：{e}");
                    }
                }
                glib::idle_add_once(|| {
                    with_inner(|i| refresh_list(i));
                });
            }
        });
    }

    NotepadPage { root: toast }
}

// ── 文件对话框（进程内 GtkFileChooserDialog，不依赖 xdg-desktop-portal） ──

/// 选图片 → 插到正文光标处。
#[allow(deprecated)] // 进程内 GtkFileChooserDialog 4.10 起标记废弃，但不依赖 portal
fn insert_image_dialog(inner: &Inner) {
    let dialog = gtk::FileChooserDialog::builder()
        .title("插入图片到正文")
        .action(gtk::FileChooserAction::Open)
        .modal(true)
        .build();
    if let Some(w) = inner.window() {
        dialog.set_transient_for(Some(&w));
    }
    dialog.add_button("取消", gtk::ResponseType::Cancel);
    dialog.add_button("插入", gtk::ResponseType::Accept);
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("图片"));
    filter.add_mime_type("image/*");
    dialog.add_filter(&filter);
    dialog.connect_response(move |d, resp| {
        if resp == gtk::ResponseType::Accept {
            let model = d.files();
            for i in 0..model.n_items() {
                let Some(file) = model.item(i).and_then(|f| f.downcast::<gio::File>().ok()) else {
                    continue;
                };
                let Some(path) = file.path() else { continue };
                with_inner(|inner| match Pixbuf::from_file(&path) {
                    Ok(p) => insert_pixbuf_at_cursor(inner, &p),
                    Err(e) => inner.toast(&format!("读不了这张图片：{e}")),
                });
            }
        }
        d.destroy();
    });
    dialog.show();
}

/// 选任意文件 → 存成当前条目的附件（同名不覆盖，自动加 `-1` 后缀）。
#[allow(deprecated)]
fn pick_attachments(inner: &Inner) {
    let Some(id) = inner.current_id.borrow().clone() else {
        inner.toast("请先选中一个条目");
        return;
    };
    let dialog = gtk::FileChooserDialog::builder()
        .title("添加附件")
        .action(gtk::FileChooserAction::Open)
        .modal(true)
        .build();
    if let Some(w) = inner.window() {
        dialog.set_transient_for(Some(&w));
    }
    // 多选：一次可以往同一个条目里加多个附件
    dialog.set_select_multiple(true);
    dialog.add_button("取消", gtk::ResponseType::Cancel);
    dialog.add_button("添加", gtk::ResponseType::Accept);
    dialog.connect_response(move |d, resp| {
        if resp == gtk::ResponseType::Accept {
            let model = d.files();
            let mut n = 0usize;
            for i in 0..model.n_items() {
                let Some(file) = model.item(i).and_then(|f| f.downcast::<gio::File>().ok()) else {
                    continue;
                };
                let Some(path) = file.path() else { continue };
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Ok(data) = std::fs::read(&path) else { continue };
                if storage::write_file(&id, name, &data).is_some() {
                    n += 1;
                }
            }
            with_inner(|i| {
                if n > 0 {
                    let cur = i.current_id.borrow().clone();
                    if let Some(cur) = cur {
                        refresh_attachments(i, &cur);
                    }
                    i.toast(&format!("已添加 {n} 个附件"));
                }
            });
        }
        d.destroy();
    });
    dialog.show();
}

/// 导出全部条目到 ZIP。
#[allow(deprecated)]
fn export_dialog(inner: &Inner) {
    let dialog = gtk::FileChooserDialog::builder()
        .title("导出备忘录到 ZIP")
        .action(gtk::FileChooserAction::Save)
        .modal(true)
        .build();
    if let Some(w) = inner.window() {
        dialog.set_transient_for(Some(&w));
    }
    dialog.add_button("取消", gtk::ResponseType::Cancel);
    dialog.add_button("导出", gtk::ResponseType::Accept);
    dialog.set_current_name("notepad-export.zip");
    dialog.connect_response(move |d, resp| {
        if resp == gtk::ResponseType::Accept {
            let Some(path) = d.file().and_then(|f| f.path()) else {
                d.destroy();
                return;
            };
            match storage::export_zip(&path) {
                Ok(()) => with_inner(|i| i.toast("已导出")),
                Err(e) => with_inner(|i| i.toast(&format!("导出失败：{e}"))),
            }
        }
        d.destroy();
    });
    dialog.show();
}

/// 从 ZIP 增量导入条目（已有 ID 跳过）。
#[allow(deprecated)]
fn import_dialog(inner: &Inner) {
    let dialog = gtk::FileChooserDialog::builder()
        .title("从 ZIP 导入备忘录")
        .action(gtk::FileChooserAction::Open)
        .modal(true)
        .build();
    if let Some(w) = inner.window() {
        dialog.set_transient_for(Some(&w));
    }
    dialog.add_button("取消", gtk::ResponseType::Cancel);
    dialog.add_button("导入", gtk::ResponseType::Accept);
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("ZIP 归档"));
    filter.add_mime_type("application/zip");
    dialog.add_filter(&filter);
    dialog.connect_response(move |d, resp| {
        if resp == gtk::ResponseType::Accept {
            let Some(path) = d.file().and_then(|f| f.path()) else {
                d.destroy();
                return;
            };
            match storage::import_zip(&path) {
                Ok((imported, skipped)) => with_inner(|i| {
                    refresh_list(i);
                    let msg = if skipped == 0 {
                        format!("导入 {imported} 条")
                    } else {
                        format!("导入 {imported} 条，跳过 {skipped} 条（已存在）")
                    };
                    i.toast(&msg);
                }),
                Err(e) => with_inner(|i| i.toast(&format!("导入失败：{e}"))),
            }
        }
        d.destroy();
    });
    dialog.show();
}

// ── 云同步 ────────────────────────────────────────────────────────────────

/// 云同步设置对话框：支持 HTTP 服务器 + Git 仓库。
#[allow(deprecated)]
fn cloud_config_dialog(inner: &Inner) {
    let configs = sync::load_configs();

    // 直接用 Inner 存储的主窗口引用
    let parent_win = inner.window.borrow().clone();
    let parent_app = parent_win.as_ref().and_then(|w| w.application());
    let mut builder = gtk::Window::builder()
        .title("云同步设置")
        .modal(true)
        .default_width(520)
        .default_height(520);
    if let Some(ref app) = parent_app {
        builder = builder.application(app);
    }
    let dialog = builder.build();
    if let Some(ref w) = parent_win {
        dialog.set_transient_for(Some(w));
    }

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
    vbox.set_margin_top(12);
    vbox.set_margin_bottom(12);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);
    dialog.set_child(Some(&vbox));

    // ── 已配置的后端列表 ──
    let title_label = gtk::Label::new(Some("已配置的同步后端"));
    title_label.set_halign(gtk::Align::Start);
    title_label.add_css_class("heading");
    vbox.append(&title_label);

    let server_list = gtk::ListBox::new();
    server_list.add_css_class("boxed-list");
    let server_scroll = gtk::ScrolledWindow::new();
    server_scroll.set_child(Some(&server_list));
    server_scroll.set_vexpand(true);
    server_scroll.set_min_content_height(100);
    vbox.append(&server_scroll);

    rebuild_backend_list(&server_list);

    // ── 添加 HTTP 服务器 ──
    let http_label = gtk::Label::new(Some("添加 HTTP 服务器"));
    http_label.set_halign(gtk::Align::Start);
    http_label.add_css_class("heading");
    http_label.set_margin_top(4);
    vbox.append(&http_label);

    let url_row = adw::EntryRow::new();
    url_row.set_title("服务器地址");
    url_row.set_show_apply_button(false);
    vbox.append(&url_row);

    let user_row = adw::EntryRow::new();
    user_row.set_title("用户名");
    user_row.set_show_apply_button(false);
    vbox.append(&user_row);

    let pass_row = adw::PasswordEntryRow::new();
    pass_row.set_title("密码");
    pass_row.set_show_apply_button(false);
    vbox.append(&pass_row);

    let btn_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    btn_box.set_halign(gtk::Align::End);
    btn_box.set_margin_top(4);

    let status_label = gtk::Label::new(None);
    status_label.add_css_class("dim-label");
    status_label.set_halign(gtk::Align::Start);
    status_label.set_vexpand(true);
    btn_box.append(&status_label);

    let test_btn = gtk::Button::with_label("测试连接");
    let status_ref = status_label.clone();
    let url_ref = url_row.clone();
    let user_ref = user_row.clone();
    let pass_ref = pass_row.clone();
    test_btn.connect_clicked(move |_| {
        let cfg = sync::CloudConfig {
            url: url_ref.text().to_string(),
            username: user_ref.text().to_string(),
            password: pass_ref.text().to_string(),
        };
        status_ref.set_text("连接中…");
        match sync::test_connection(&cfg) {
            Ok(()) => status_ref.set_text("✓ 连接成功"),
            Err(e) => status_ref.set_text(&format!("✗ {e}")),
        }
    });
    btn_box.append(&test_btn);

    let add_http_btn = gtk::Button::with_label("添加服务器");
    add_http_btn.add_css_class("suggested-action");
    let url_ref2 = url_row.clone();
    let user_ref2 = user_row.clone();
    let pass_ref2 = pass_row.clone();
    let list_ref = server_list.clone();
    let status_ref2 = status_label.clone();
    add_http_btn.connect_clicked(move |_| {
        let cfg = sync::CloudConfig {
            url: url_ref2.text().to_string(),
            username: user_ref2.text().to_string(),
            password: pass_ref2.text().to_string(),
        };
        if cfg.url.is_empty() || cfg.username.is_empty() {
            status_ref2.set_text("请填写地址和用户名");
            return;
        }
        match sync::add_http_config(cfg) {
            Ok(()) => {
                rebuild_backend_list(&list_ref);
                url_ref2.set_text("");
                user_ref2.set_text("");
                pass_ref2.set_text("");
                status_ref2.set_text("✓ 已添加");
            }
            Err(e) => status_ref2.set_text(&format!("✗ {e}")),
        }
    });
    btn_box.append(&add_http_btn);
    vbox.append(&btn_box);

    // ── 添加 Git 仓库 ──
    let git_label = gtk::Label::new(Some("添加 Git 仓库"));
    git_label.set_halign(gtk::Align::Start);
    git_label.add_css_class("heading");
    git_label.set_margin_top(8);
    vbox.append(&git_label);

    let git_url_row = adw::EntryRow::new();
    git_url_row.set_title("仓库地址");
    git_url_row.set_show_apply_button(false);
    vbox.append(&git_url_row);

    let git_user_row = adw::EntryRow::new();
    git_user_row.set_title("用户名（可选，SSH 可不填）");
    git_user_row.set_show_apply_button(false);
    vbox.append(&git_user_row);

    let git_pass_row = adw::PasswordEntryRow::new();
    git_pass_row.set_title("密码 / Token（可选，SSH 可不填）");
    git_pass_row.set_show_apply_button(false);
    vbox.append(&git_pass_row);

    let git_name_row = adw::EntryRow::new();
    git_name_row.set_title("Git 用户名（可选，默认 linbox）");
    git_name_row.set_show_apply_button(false);
    vbox.append(&git_name_row);

    let git_email_row = adw::EntryRow::new();
    git_email_row.set_title("Git 邮箱（可选，默认 linbox@notepad.local）");
    git_email_row.set_show_apply_button(false);
    vbox.append(&git_email_row);

    let git_hint = gtk::Label::new(Some("支持 SSH（git@…）和 HTTPS（https://…），优先尝试 SSH 密钥"));
    git_hint.add_css_class("dim-label");
    git_hint.set_halign(gtk::Align::Start);
    git_hint.set_margin_bottom(4);
    vbox.append(&git_hint);

    let git_btn_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    git_btn_box.set_halign(gtk::Align::End);

    let git_status = gtk::Label::new(None);
    git_status.add_css_class("dim-label");
    git_status.set_halign(gtk::Align::Start);
    git_status.set_vexpand(true);
    git_btn_box.append(&git_status);

    let git_test_btn = gtk::Button::with_label("验证仓库");
    let git_status_ref = git_status.clone();
    let git_url_ref = git_url_row.clone();
    let git_user_ref = git_user_row.clone();
    let git_pass_ref = git_pass_row.clone();
    let git_name_ref = git_name_row.clone();
    let git_email_ref = git_email_row.clone();
    git_test_btn.connect_clicked(move |_| {
        let cfg = git_store::GitConfig {
            url: git_url_ref.text().to_string(),
            branch: "main".to_string(),
            user_name: git_name_ref.text().to_string(),
            user_email: git_email_ref.text().to_string(),
            username: git_user_ref.text().to_string(),
            password: git_pass_ref.text().to_string(),
        };
        if cfg.url.is_empty() {
            git_status_ref.set_text("请填写仓库地址");
            return;
        }
        git_status_ref.set_text("验证中…");
        match sync::test_git_connection(&cfg) {
            Ok(()) => git_status_ref.set_text("✓ 仓库可访问"),
            Err(e) => git_status_ref.set_text(&format!("✗ {e}")),
        }
    });
    git_btn_box.append(&git_test_btn);

    let add_git_btn = gtk::Button::with_label("添加仓库");
    add_git_btn.add_css_class("suggested-action");
    let git_url_ref2 = git_url_row.clone();
    let git_user_ref2 = git_user_row.clone();
    let git_pass_ref2 = git_pass_row.clone();
    let git_name_ref2 = git_name_row.clone();
    let git_email_ref2 = git_email_row.clone();
    let list_ref2 = server_list.clone();
    let git_status_ref2 = git_status.clone();
    add_git_btn.connect_clicked(move |_| {
        let cfg = git_store::GitConfig {
            url: git_url_ref2.text().to_string(),
            branch: "main".to_string(),
            user_name: git_name_ref2.text().to_string(),
            user_email: git_email_ref2.text().to_string(),
            username: git_user_ref2.text().to_string(),
            password: git_pass_ref2.text().to_string(),
        };
        if cfg.url.is_empty() {
            git_status_ref2.set_text("请填写仓库地址");
            return;
        }
        // 先验证可访问性
        git_status_ref2.set_text("验证仓库可访问性…");
        match sync::test_git_connection(&cfg) {
            Ok(()) => {
                match sync::add_git_config(cfg) {
                    Ok(()) => {
                        rebuild_backend_list(&list_ref2);
                        git_url_ref2.set_text("");
                        git_user_ref2.set_text("");
                        git_pass_ref2.set_text("");
                        git_name_ref2.set_text("");
                        git_email_ref2.set_text("");
                        git_status_ref2.set_text("✓ 仓库已添加");
                    }
                    Err(e) => git_status_ref2.set_text(&format!("✗ {e}")),
                }
            }
            Err(e) => git_status_ref2.set_text(&format!("✗ 无法访问：{e}")),
        }
    });
    git_btn_box.append(&add_git_btn);
    vbox.append(&git_btn_box);

    dialog.present();
}

/// 重建后端列表 UI（支持 HTTP 和 Git）。
fn rebuild_backend_list(list: &gtk::ListBox) {
    list.remove_all();
    let configs = sync::load_configs();
    for cfg in &configs {
        let row = adw::ActionRow::new();
        match cfg {
            sync::BackendConfig::Http(c) => {
                row.set_title(&c.url);
                row.set_subtitle(&format!("HTTP · 用户：{}", c.username));
            }
            sync::BackendConfig::Git(c) => {
                let repo = c.url.rsplit('/').next().unwrap_or(&c.url);
                row.set_title(repo);
                let proto = if c.url.starts_with("git@") {
                    "Git/SSH"
                } else {
                    "Git/HTTPS"
                };
                let auth = if c.username.is_empty() {
                    "密钥认证".to_string()
                } else {
                    format!("用户：{}", c.username)
                };
                row.set_subtitle(&format!("{proto} · {auth}"));
            }
        }
        // 编辑按钮
        let edit_btn = gtk::Button::from_icon_name("document-edit-symbolic");
        edit_btn.set_tooltip_text(Some("编辑"));
        let cfg_for_edit = cfg.clone();
        let list_ref = list.clone();
        edit_btn.connect_clicked(move |_| {
            edit_backend_dialog(&cfg_for_edit, &list_ref);
        });
        row.add_suffix(&edit_btn);
        // 删除按钮
        let del_btn = gtk::Button::from_icon_name("user-trash-symbolic");
        del_btn.add_css_class("destructive-action");
        del_btn.set_tooltip_text(Some("删除"));
        let key_for_delete = cfg.unique_key();
        let list_ref2 = list.clone();
        del_btn.connect_clicked(move |_| {
            let _ = sync::remove_backend(&key_for_delete);
            rebuild_backend_list(&list_ref2);
        });
        row.add_suffix(&del_btn);
        row.set_activatable(false);
        list.append(&row);
    }
    if configs.is_empty() {
        let empty_label = gtk::Label::new(Some("还没有配置同步后端"));
        empty_label.add_css_class("dim-label");
        empty_label.set_margin_top(12);
        empty_label.set_margin_bottom(12);
        list.append(&empty_label);
    }
}

/// 编辑已有后端的对话框。
#[allow(deprecated)]
fn edit_backend_dialog(cfg: &sync::BackendConfig, list_ref: &gtk::ListBox) {
    let mut parent_win: Option<gtk::Window> = None;
    with_inner(|i| {
        parent_win = i.window.borrow().clone();
    });
    let parent_app = parent_win.as_ref().and_then(|w| w.application());
    let mut builder = gtk::Window::builder()
        .title("编辑同步后端")
        .modal(true)
        .default_width(480)
        .default_height(350);
    if let Some(ref app) = parent_app {
        builder = builder.application(app);
    }
    let dialog = builder.build();
    if let Some(ref w) = parent_win {
        dialog.set_transient_for(Some(w));
    }

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
    vbox.set_margin_top(12);
    vbox.set_margin_bottom(12);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);
    dialog.set_child(Some(&vbox));

    match cfg {
        sync::BackendConfig::Http(c) => {
            let url_row = adw::EntryRow::new();
            url_row.set_title("服务器地址");
            url_row.set_text(&c.url);
            url_row.set_show_apply_button(false);
            vbox.append(&url_row);

            let user_row = adw::EntryRow::new();
            user_row.set_title("用户名");
            user_row.set_text(&c.username);
            user_row.set_show_apply_button(false);
            vbox.append(&user_row);

            let pass_row = adw::PasswordEntryRow::new();
            pass_row.set_title("密码");
            pass_row.set_text(&c.password);
            pass_row.set_show_apply_button(false);
            vbox.append(&pass_row);

            let btn_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            btn_box.set_halign(gtk::Align::End);
            btn_box.set_margin_top(8);

            let save_btn = gtk::Button::with_label("保存");
            save_btn.add_css_class("suggested-action");
            let dialog_ref = dialog.clone();
            let list_ref2 = list_ref.clone();
            let url_ref = url_row.clone();
            let user_ref = user_row.clone();
            let pass_ref = pass_row.clone();
            let old_key = cfg.unique_key();
            save_btn.connect_clicked(move |_| {
                let new_cfg = sync::BackendConfig::Http(sync::CloudConfig {
                    url: url_ref.text().to_string(),
                    username: user_ref.text().to_string(),
                    password: pass_ref.text().to_string(),
                });
                let _ = sync::remove_backend(&old_key);
                let _ = save_configs_append(&new_cfg);
                rebuild_backend_list(&list_ref2);
                dialog_ref.close();
            });
            btn_box.append(&save_btn);
            vbox.append(&btn_box);
        }
        sync::BackendConfig::Git(c) => {
            let url_row = adw::EntryRow::new();
            url_row.set_title("仓库地址");
            url_row.set_text(&c.url);
            url_row.set_show_apply_button(false);
            vbox.append(&url_row);

            let user_row = adw::EntryRow::new();
            user_row.set_title("HTTPS 用户名（可选）");
            user_row.set_text(&c.username);
            user_row.set_show_apply_button(false);
            vbox.append(&user_row);

            let pass_row = adw::PasswordEntryRow::new();
            pass_row.set_title("密码 / Token（可选）");
            pass_row.set_text(&c.password);
            pass_row.set_show_apply_button(false);
            vbox.append(&pass_row);

            let name_row = adw::EntryRow::new();
            name_row.set_title("Git 用户名（可选）");
            name_row.set_text(&c.user_name);
            name_row.set_show_apply_button(false);
            vbox.append(&name_row);

            let email_row = adw::EntryRow::new();
            email_row.set_title("Git 邮箱（可选）");
            email_row.set_text(&c.user_email);
            email_row.set_show_apply_button(false);
            vbox.append(&email_row);

            let btn_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            btn_box.set_halign(gtk::Align::End);
            btn_box.set_margin_top(8);

            let save_btn = gtk::Button::with_label("保存");
            save_btn.add_css_class("suggested-action");
            let dialog_ref = dialog.clone();
            let list_ref2 = list_ref.clone();
            let url_ref = url_row.clone();
            let user_ref = user_row.clone();
            let pass_ref = pass_row.clone();
            let name_ref = name_row.clone();
            let email_ref = email_row.clone();
            let old_key = cfg.unique_key();
            let branch = c.branch.clone();
            save_btn.connect_clicked(move |_| {
                let new_cfg = sync::BackendConfig::Git(git_store::GitConfig {
                    url: url_ref.text().to_string(),
                    branch: branch.clone(),
                    user_name: name_ref.text().to_string(),
                    user_email: email_ref.text().to_string(),
                    username: user_ref.text().to_string(),
                    password: pass_ref.text().to_string(),
                });
                let _ = sync::remove_backend(&old_key);
                let _ = save_configs_append(&new_cfg);
                rebuild_backend_list(&list_ref2);
                dialog_ref.close();
            });
            btn_box.append(&save_btn);
            vbox.append(&btn_box);
        }
    }

    dialog.present();
}

/// 在已有配置列表末尾追加一个后端。
fn save_configs_append(cfg: &sync::BackendConfig) -> Result<(), String> {
    let mut configs = sync::load_configs();
    configs.push(cfg.clone());
    sync::save_configs(&configs)
}

/// 在后台线程执行云同步，完成后在主线程显示结果。
fn start_cloud_sync(inner: &Inner) {
    let configs = sync::load_configs();
    if configs.is_empty() {
        inner.toast("请先配置同步后端");
        return;
    }
    // 先把当前编辑保存到磁盘
    save_current(inner, true);
    inner.toast(&format!("正在同步（{} 个后端）…", configs.len()));
    // 后台线程执行同步
    std::thread::spawn(move || {
        let result = sync::do_sync(&configs);
        glib::idle_add_once(move || {
            with_inner(|i| match result {
                Ok(summary) => {
                    let mut parts = Vec::new();
                    if summary.uploaded > 0 {
                        parts.push(format!("上传 {} 条", summary.uploaded));
                    }
                    if summary.downloaded > 0 {
                        parts.push(format!("下载 {} 条", summary.downloaded));
                    }
                    if summary.deleted_local > 0 {
                        parts.push(format!("删除本地 {} 条", summary.deleted_local));
                    }
                    if summary.deleted_remote > 0 {
                        parts.push(format!("通知云端删除 {} 条", summary.deleted_remote));
                    }
                    // 每台服务器的状态
                    for (name, result) in &summary.per_server {
                        match result {
                            Ok(()) => parts.push(format!("✓ {name}")),
                            Err(e) => parts.push(format!("✗ {name}: {e}")),
                        }
                    }
                    if parts.is_empty() {
                        i.toast("同步完成，无需更新");
                        mark_clean();
                    } else {
                        i.toast(&format!("同步完成：{}", parts.join("，")));
                        mark_clean();
                    }
                    if !summary.errors.is_empty() {
                        i.toast(&format!("部分错误：{}", summary.errors.join("；")));
                    }
                    // 同步完成后刷新列表并重新加载当前条目（P0 #2 + P1 #10）
                    // 延迟执行：避免 row_selected → with_inner 重入导致 RefCell panic
                    glib::idle_add_once(|| {
                        with_inner(|i| {
                            refresh_list(i);
                            // 必须先 bind 再 if let：`if let Some(x) = cell.borrow().clone()`
                            // 里的临时 Ref 会活到整个 if let 块结束，
                            // 块内再 borrow_mut() 必 panic。
                            let cur = i.current_id.borrow().clone();
                            if let Some(id) = cur {
                                i.inline_fp.borrow_mut().clear();
                                load_entry(i, &id);
                            }
                        });
                    });
                }
                Err(e) => {
                    i.toast(&format!("同步失败：{e}"));
                }
            });
        });
    });
}

/// 手动 Pull（刷新）：后台从所有后端拉取，不推送。
fn start_pull(inner: &Inner) {
    let configs = sync::load_configs();
    if configs.is_empty() {
        inner.toast("请先配置同步后端");
        return;
    }
    save_current(inner, true);
    inner.toast(&format!("正在刷新（{} 个后端）…", configs.len()));
    std::thread::spawn(move || {
        let result = sync::pull_only(&configs);
        glib::idle_add_once(move || {
            with_inner(|i| match result {
                Ok(summary) => {
                    let mut parts = Vec::new();
                    if summary.downloaded > 0 {
                        parts.push(format!("下载 {} 条", summary.downloaded));
                    }
                    for (name, result) in &summary.per_server {
                        match result {
                            Ok(()) => parts.push(format!("✓ {name}")),
                            Err(e) => parts.push(format!("✗ {name}: {e}")),
                        }
                    }
                    if parts.is_empty() {
                        i.toast("刷新完成，已是最新"); mark_clean();
                    } else {
                        i.toast(&format!("刷新完成：{}", parts.join("，"))); mark_clean();
                    }
                    if !summary.errors.is_empty() {
                        i.toast(&format!("部分错误：{}", summary.errors.join("；")));
                    }
                    // 刷新后重新加载列表并重新加载当前条目（P0 #2）
                    glib::idle_add_once(|| {
                        with_inner(|i| {
                            refresh_list(i);
                            // 先 bind 再 if let，避免 Ref 临时值活到块尾
                            let cur = i.current_id.borrow().clone();
                            if let Some(id) = cur {
                                i.inline_fp.borrow_mut().clear();
                                load_entry(i, &id);
                            }
                        });
                    });
                }
                Err(e) => {
                    i.toast(&format!("刷新失败：{e}"));
                }
            });
        });
    });
}

/// 手动 Push：后台推送到所有后端，不拉取。
fn start_push(inner: &Inner) {
    let configs = sync::load_configs();
    if configs.is_empty() {
        inner.toast("请先配置同步后端");
        return;
    }
    save_current(inner, true);
    inner.toast(&format!("正在推送（{} 个后端）…", configs.len()));
    std::thread::spawn(move || {
        let result = sync::push_only(&configs);
        glib::idle_add_once(move || {
            with_inner(|i| match result {
                Ok(summary) => {
                    let mut parts = Vec::new();
                    if summary.uploaded > 0 {
                        parts.push(format!("上传 {} 条", summary.uploaded));
                    }
                    for (name, result) in &summary.per_server {
                        match result {
                            Ok(()) => parts.push(format!("✓ {name}")),
                            Err(e) => parts.push(format!("✗ {name}: {e}")),
                        }
                    }
                    if parts.is_empty() {
                        i.toast("推送完成，无需更新"); mark_clean();
                    } else {
                        i.toast(&format!("推送完成：{}", parts.join("，"))); mark_clean();
                    }
                    if !summary.errors.is_empty() {
                        i.toast(&format!("部分错误：{}", summary.errors.join("；")));
                    }
                }
                Err(e) => {
                    i.toast(&format!("推送失败：{e}"));
                }
            });
        });
    });
}

// ── 正文内嵌图片 ──────────────────────────────────────────────────────────

/// 把图片（已解码）插到正文光标处。
fn insert_pixbuf_at_cursor(inner: &Inner, pixbuf: &Pixbuf) {
    let scaled = scale_down(pixbuf, MAX_INLINE_W);
    let texture = gdk::Texture::for_pixbuf(&scaled);
    let mut iter = inner.buffer.iter_at_mark(&inner.buffer.get_insert());
    inner.buffer.insert_paintable(&mut iter, &texture);
}

/// 等比缩到 `max_w` 以内（不放大）。
fn scale_down(pixbuf: &Pixbuf, max_w: i32) -> Pixbuf {
    let w = pixbuf.width();
    let h = pixbuf.height();
    if w <= max_w || w <= 0 || h <= 0 {
        return pixbuf.clone();
    }
    let nh = ((h as f64) * (max_w as f64) / (w as f64)).round().max(1.0) as i32;
    pixbuf
        .scale_simple(max_w, nh, InterpType::Bilinear)
        .unwrap_or_else(|| pixbuf.clone())
}

/// 把 PNG 字节解成 Pixbuf（剪贴板里读出来的是 PNG）。
fn pixbuf_from_png(bytes: &glib::Bytes) -> Option<Pixbuf> {
    let stream = gio::MemoryInputStream::new();
    stream.add_bytes(bytes);
    Pixbuf::from_stream(&stream, None::<&gio::Cancellable>).ok()
}

/// 给可编辑控件挂上「Ctrl+V 粘贴图片到正文」的按键控制器。
///
/// 必须走**捕获阶段**：TextView / Entry 自己会处理 Ctrl+V 并把事件吃掉，
/// 冒泡阶段（默认）根本收不到这个按键。
fn attach_paste_controller(w: &impl IsA<gtk::Widget>) {
    let ctl = gtk::EventControllerKey::new();
    ctl.set_propagation_phase(gtk::PropagationPhase::Capture);
    ctl.connect_key_pressed(|_, key, _, modif| {
        if !modif.contains(gdk::ModifierType::CONTROL_MASK) {
            return glib::Propagation::Proceed;
        }
        if key != gdk::Key::v && key != gdk::Key::V {
            return glib::Propagation::Proceed;
        }
        let Some(inner) = INNER.with(|i| i.try_borrow().ok().and_then(|b| b.clone())) else {
            return glib::Propagation::Proceed;
        };
        paste_image(&inner)
    });
    w.add_controller(ctl);
}

/// 剪贴板里是纯图片时插进正文；否则放行给控件自己粘贴文字。
fn paste_image(inner: &Inner) -> glib::Propagation {
    // 没选中条目就别拦这个按键（让输入框正常粘贴文字）
    if inner.current_id.borrow().is_none() {
        return glib::Propagation::Proceed;
    }
    let clipboard = inner.toast.display().clipboard();
    let formats = clipboard.formats();
    let has_image = formats.contain_mime_type("image/png")
        || formats.contain_mime_type("image/jpeg")
        || formats.contain_mime_type("image/gif")
        || formats.contain_mime_type("image/webp")
        || formats.contain_mime_type("image/bmp");
    // 同时带纯文本时（网页复制等）优先让输入框粘贴文字，
    // 只有「纯图片」剪贴板（截图工具）才往正文里插。
    let has_text =
        formats.contain_mime_type("text/plain") || formats.contain_mime_type("text/uri-list");
    if !(has_image && !has_text) {
        return glib::Propagation::Proceed;
    }
    clipboard.read_texture_async(None::<&gio::Cancellable>, move |result| {
        let Ok(Some(texture)) = result else {
            return;
        };
        with_inner(|i| {
            // 异步回调回来时用户可能已经切走条目，重新取一次当前 id。
            if i.current_id.borrow().is_none() {
                return;
            }
            let Some(pixbuf) = pixbuf_from_png(&texture.save_to_png_bytes()) else {
                i.toast("剪贴板里的图片读不出来");
                return;
            };
            insert_pixbuf_at_cursor(i, &pixbuf);
        });
    });
    glib::Propagation::Stop
}

/// 把正文拆成「纯文本（图片位置是 U+FFFC）」+「图片 PNG 字节列表」。
fn extract_buffer(buffer: &gtk::TextBuffer) -> (String, Vec<Vec<u8>>) {
    let mut text = String::new();
    let mut images: Vec<Vec<u8>> = Vec::new();
    let mut iter = buffer.start_iter();
    // 注意：`iter.char()` 在末尾返回 U+0000，所以必须用 is_end() 收尾，
    // 否则每保存一次正文尾巴上就多一个 NUL。
    while !iter.is_end() {
        if let Some(paintable) = iter.paintable() {
            text.push(OBJ);
            let png = paintable
                .downcast::<gdk::Texture>()
                .map(|t| t.save_to_png_bytes().to_vec())
                .unwrap_or_default();
            images.push(png);
        } else {
            text.push(iter.char());
        }
        iter.forward_char();
    }
    (text, images)
}

/// 把「纯文本 + 图片文件」还原进正文。
fn fill_buffer(buffer: &gtk::TextBuffer, id: &str, text: &str) {
    let mut iter = buffer.start_iter();
    let mut idx = 0usize;
    let mut seg = String::new();
    for c in text.chars() {
        if c != OBJ {
            seg.push(c);
            continue;
        }
        if !seg.is_empty() {
            buffer.insert(&mut iter, &seg);
            seg.clear();
        }
        let restored = match storage::read_inline(id, idx) {
            Some(data) => match gdk::Texture::from_bytes(&glib::Bytes::from(&data)) {
                Ok(texture) => {
                    buffer.insert_paintable(&mut iter, &texture);
                    true
                }
                Err(_) => false,
            },
            None => false,
        };
        // 图片丢了也要留住这个占位字符，否则下次保存会把图片位置整个抹掉
        if !restored {
            buffer.insert(&mut iter, &OBJ.to_string());
        }
        idx += 1;
    }
    if !seg.is_empty() {
        buffer.insert(&mut iter, &seg);
    }
}

/// 图片字节的抽样指纹（避免每次按键都对几 MB 的图做全量哈希）。
fn fingerprint(data: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.len().hash(&mut h);
    let step = (data.len() / 64).max(1);
    let mut i = 0;
    while i < data.len() {
        data[i].hash(&mut h);
        i += step;
    }
    h.finish()
}

/// 写盘内嵌图片：内容与上次一致就跳过（每敲一个字都复写会卡）。
fn sync_inline_images(inner: &Inner, id: &str, images: &[Vec<u8>]) {
    let fp: Vec<u64> = images.iter().map(|d| fingerprint(d)).collect();
    if fp == *inner.inline_fp.borrow() {
        return;
    }
    storage::save_inline(id, images);
    *inner.inline_fp.borrow_mut() = fp;
}

// ── 附件 ──────────────────────────────────────────────────────────────────

fn refresh_attachments(inner: &Inner, id: &str) {
    inner.att_box.remove_all();
    inner.att_names.borrow_mut().clear();
    for name in storage::list_files(id) {
        let child = gtk::FlowBoxChild::new();
        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 2);
        vbox.set_margin_top(4);
        vbox.set_margin_bottom(4);
        vbox.set_margin_start(4);
        vbox.set_margin_end(4);

        let icon = gtk::Image::from_icon_name(icon_for_file(&name));
        icon.set_pixel_size(32);
        let label = gtk::Label::new(Some(&name));
        label.add_css_class("caption");
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        label.set_max_width_chars(12);

        vbox.append(&icon);
        vbox.append(&label);
        child.set_child(Some(&vbox));
        child.set_tooltip_text(Some(&name));
        inner.att_box.append(&child);
        inner.att_names.borrow_mut().push(name);
    }
    update_attach_buttons(inner);
}

/// 当前选中的附件名（没有选中就 None）。
fn selected_attachment(inner: &Inner) -> Option<String> {
    let child = inner.att_box.selected_children().into_iter().next()?;
    let idx = child.index();
    if idx < 0 {
        return None;
    }
    inner.att_names.borrow().get(idx as usize).cloned()
}

fn update_attach_buttons(inner: &Inner) {
    let has = selected_attachment(inner).is_some();
    inner.att_open.set_sensitive(has);
    inner.att_show.set_sensitive(has);
    inner.att_del.set_sensitive(has);
}

/// 用系统默认应用打开选中的附件。
fn open_attachment(inner: &Inner) {
    let Some(id) = inner.current_id.borrow().clone() else {
        return;
    };
    let Some(name) = selected_attachment(inner) else {
        inner.toast("先选中一个附件");
        return;
    };
    open_file_by_path(&storage::file_path(&id, &name));
}

/// 在文件管理器中打开选中附件所在的文件夹。
fn show_attachment_in_folder(inner: &Inner) {
    let Some(id) = inner.current_id.borrow().clone() else {
        return;
    };
    let Some(name) = selected_attachment(inner) else {
        inner.toast("先选中一个附件");
        return;
    };
    show_in_folder(&storage::file_path(&id, &name));
}

/// 用系统默认应用打开指定路径的文件。
fn open_file_by_path(path: &std::path::Path) {
    let uri = gio::File::for_path(path).uri();
    let path_str = path.display().to_string();
    gio::AppInfo::launch_default_for_uri_async(
        &uri,
        None::<&gio::AppLaunchContext>,
        None::<&gio::Cancellable>,
        move |res| {
            if let Err(e) = res {
                eprintln!("打不开「{path_str}」：{e}");
            }
        },
    );
}

/// 在文件管理器中显示指定文件（选中状态）。
fn show_in_folder(path: &std::path::Path) {
    let Some(parent) = path.parent() else { return };
    let file = gio::File::for_path(parent);
    let uri = file.uri();
    let path_str = path.display().to_string();
    gio::AppInfo::launch_default_for_uri_async(
        &uri,
        None::<&gio::AppLaunchContext>,
        None::<&gio::Cancellable>,
        move |res| {
            if let Err(e) = res {
                eprintln!("打不开文件夹：{e}（{path_str}）");
            }
        },
    );
}

/// 鼠标是不是正停在正文里的某个 URL 上。
fn pointer_over_url(inner: &Inner, text_view: &gtk::TextView, x: f64, y: f64) -> bool {
    let (bx, by) =
        text_view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
    match text_view.iter_at_location(bx, by) {
        Some(iter) => scan_url_at_iter(&inner.buffer, &iter).is_some(),
        None => false,
    }
}

/// 链接悬停反馈的状态（跨事件保留，挂在控制器闭包上）。
struct LinkHover {
    /// 最后一次鼠标位置（控件坐标；`NaN` = 还没收到过 motion 事件）。
    last: Cell<(f64, f64)>,
    /// 当前是不是已经换成了手型光标（避免每次移动都重建 Cursor）。
    pointer: Cell<bool>,
}

impl LinkHover {
    fn new() -> Self {
        Self {
            last: Cell::new((f64::NAN, f64::NAN)),
            pointer: Cell::new(false),
        }
    }
}

/// 重算悬停反馈：按着 Ctrl 停在链接上 → 手型光标；否则还原编辑用的 I 型。
fn refresh_link_hover(
    hover: &LinkHover,
    text_view: &gtk::TextView,
    original: Option<&gdk::Cursor>,
    ctrl: bool,
) {
    let (x, y) = hover.last.get();
    let want = !x.is_nan()
        && ctrl
        && with_inner_r(|inner| pointer_over_url(inner, text_view, x, y)).unwrap_or(false);
    if want == hover.pointer.get() {
        return;
    }
    hover.pointer.set(want);
    if want {
        text_view.set_cursor(gdk::Cursor::from_name("pointer", None).as_ref());
    } else {
        // 还原成 TextView 自己那个 "text" 光标（build 时先存下来的）
        text_view.set_cursor(original);
    }
}

/// 链接悬停反馈：手型光标 + 「Ctrl+左键 打开链接」提示。
///
/// ⚠️ TextView 自己的 motion 处理器每次移动都会把光标重置成 `text`
/// （`gtk_text_view_motion` → `unobscure_mouse_cursor`），所以**必须挂在它后面**
/// —— 我们的控制器是后 add 的，同一阶段下后跑，后写的光标才生效。
fn attach_link_hover(text_view: &gtk::TextView) {
    let tv = text_view.clone();
    let original = text_view.cursor();
    let hover = Rc::new(LinkHover::new());

    // 1) 鼠标移动：位置和「是否按着 Ctrl」一起更新
    {
        let tv = tv.clone();
        let original = original.clone();
        let hover = hover.clone();
        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(move |ctl, x, y| {
            hover.last.set((x, y));
            let ctrl = ctl
                .current_event()
                .map(|e| e.modifier_state().contains(gdk::ModifierType::CONTROL_MASK))
                .unwrap_or(false);
            refresh_link_hover(&hover, &tv, original.as_ref(), ctrl);
        });
        text_view.add_controller(motion);
    }

    // 2) 鼠标不动、只按/松 Ctrl 时不会有 motion 事件，得靠 modifiers 信号补
    {
        let tv = tv.clone();
        let original = original.clone();
        let hover = hover.clone();
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_modifiers(move |_, state| {
            refresh_link_hover(
                &hover,
                &tv,
                original.as_ref(),
                state.contains(gdk::ModifierType::CONTROL_MASK),
            );
            glib::Propagation::Proceed
        });
        text_view.add_controller(keys);
    }

    // 3) 悬停提示：告诉用户链接要点 Ctrl
    text_view.set_has_tooltip(true);
    {
        let tv = tv.clone();
        text_view.connect_query_tooltip(move |_, x, y, _kb, tooltip| {
            let hit = with_inner_r(|inner| pointer_over_url(inner, &tv, x as f64, y as f64))
                .unwrap_or(false);
            if hit {
                tooltip.set_text(Some("Ctrl+左键 打开链接"));
            }
            hit
        });
    }
}

/// 在 `anchor` 上弹出一个「打开 / 在文件夹中显示」菜单。
///
/// `at` 是 `anchor` 控件坐标系里的点击位置：不给的话 popover 会落在整个控件的
/// 中央（正文 TextView 那么大，菜单会飘到离鼠标很远的地方），所以要传。
fn show_file_menu(anchor: &impl IsA<gtk::Widget>, path: &std::path::Path, at: Option<(f64, f64)>) {
    let popover = gtk::Popover::new();
    popover.set_parent(anchor);
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    popover.set_child(Some(&vbox));

    let open_btn = gtk::Button::with_label("打开");
    open_btn.set_has_frame(false);
    open_btn.set_halign(gtk::Align::Fill);
    {
        let p = path.to_path_buf();
        let pop = popover.clone();
        open_btn.connect_clicked(move |_| {
            open_file_by_path(&p);
            pop.popdown();
        });
    }
    vbox.append(&open_btn);

    let folder_btn = gtk::Button::with_label("在文件夹中显示");
    folder_btn.set_has_frame(false);
    folder_btn.set_halign(gtk::Align::Fill);
    {
        let p = path.to_path_buf();
        let pop = popover.clone();
        folder_btn.connect_clicked(move |_| {
            show_in_folder(&p);
            pop.popdown();
        });
    }
    vbox.append(&folder_btn);

    if let Some((x, y)) = at {
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    }
    popover.popup();
}

/// 数出 `iter` 位置之前有几个图片占位符 —— 即这是第几张内嵌图片。
///
/// 图片在 TextBuffer 里是 paintable 对象，`iter.char()` 对图片返回 U+FFFC，
/// 与 `content.txt` 里的占位符一一对应，所以直接数字符即可。
fn inline_index_at(buffer: &gtk::TextBuffer, iter: &gtk::TextIter) -> usize {
    let mut idx = 0usize;
    let mut cur = buffer.start_iter();
    while cur.offset() < iter.offset() && !cur.is_end() {
        if cur.char() == OBJ {
            idx += 1;
        }
        cur.forward_char();
    }
    idx
}

/// 点中位置上的内联图片：返回图片所在的 `iter` 与它是第几张。
///
/// 点击落点必须**正好**在图片上（TextBuffer 里图片占一个字符位）。
fn inline_image_at(
    inner: &Inner,
    text_view: &gtk::TextView,
    x: f64,
    y: f64,
) -> Option<(gtk::TextIter, usize)> {
    let (bx, by) =
        text_view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
    let iter = text_view.iter_at_location(bx, by)?;
    if iter.paintable().is_none() {
        return None;
    }
    let idx = inline_index_at(&inner.buffer, &iter);
    Some((iter, idx))
}

/// 把某张内联图片从条目 ZIP 导出成磁盘文件，返回路径。
///
/// 落盘位置、命名都由 `storage::inline_image_path` 决定（和附件同一个 tmp 目录），
/// 这样右键菜单里的「在文件夹中显示」打开的是一个稳定、和其他附件一致的地方，
/// 而不是系统临时目录（那个可能被清理，位置也和附件对不上）。
fn inline_image_to_temp(inner: &Inner, iter: &gtk::TextIter) -> Option<std::path::PathBuf> {
    let idx = inline_index_at(&inner.buffer, iter);
    let cur_id = inner.current_id.borrow().clone();
    let id = cur_id?;
    storage::inline_image_path(&id, idx)
}

/// 点中内联图片 → 导出成临时文件（右键菜单 / 双击打开共用）。
fn inline_image_path_at(
    inner: &Inner,
    text_view: &gtk::TextView,
    x: f64,
    y: f64,
) -> Option<std::path::PathBuf> {
    let (iter, _idx) = inline_image_at(inner, text_view, x, y)?;
    inline_image_to_temp(inner, &iter)
}

/// 附件列表：右键菜单（打开 / 在文件夹中显示）。
///
/// 双击打开**故意不做**：工具栏上已经有「打开」「删除」按钮，再挂一个双击是冗余。
/// 点附件只负责选中（交给 FlowBox 自己处理）。
fn attach_attachment_gestures(att_box: &gtk::FlowBox) {
    // ── 右键菜单 ──
    // FlowBox 内部只认左键，button-3 会原样冒泡到这里。
    let att_box_rt = att_box.clone();
    let right = gtk::GestureClick::new();
    right.set_button(3);
    right.connect_released(move |_, _, x, y| {
        with_inner(|inner| {
            let Some(child) = att_box_rt.child_at_pos(x as i32, y as i32) else {
                return;
            };
            let idx = child.index();
            if idx < 0 {
                return;
            }
            att_box_rt.select_child(&child);
            let cur = inner.current_id.borrow().clone();
            let Some(id) = cur else { return };
            let name = inner.att_names.borrow().get(idx as usize).cloned();
            let Some(name) = name else { return };
            let path = storage::file_path(&id, &name);
            show_file_menu(&att_box_rt, &path, Some((x, y)));
        });
    });
    att_box.add_controller(right);
}

/// 当前选中的是不是**正好一张**内联图片：是就返回它的 iter 和序号。
///
/// 单击图片会把它选成「1 个字符位」的选区，所以「选中 → 右键」能精确对上这张图。
fn selected_inline_image(inner: &Inner) -> Option<(gtk::TextIter, usize)> {
    let (a, b) = inner.buffer.selection_bounds()?;
    let (lo, hi) = if a.offset() <= b.offset() { (a, b) } else { (b, a) };
    if hi.offset() - lo.offset() != 1 {
        return None; // 不是正好一格（拖选了文字 / 选了多张）
    }
    if lo.paintable().is_none() {
        return None; // 选中的是普通字符
    }
    Some((lo, inline_index_at(&inner.buffer, &lo)))
}

/// 把选中的那张内联图片导出成磁盘文件（右键菜单的两个动作共用）。
fn selected_inline_image_temp(inner: &Inner) -> Option<std::path::PathBuf> {
    let (iter, _idx) = selected_inline_image(inner)?;
    inline_image_to_temp(inner, &iter)
}

/// 建「打开图片 / 在文件夹中显示」两个 action。
///
/// 单独拆出来是为了单测也能拿到同一套 action（测试环境没有 text_view，
/// 但状态同步逻辑只依赖这个 group）。
fn build_inline_actions() -> gio::SimpleActionGroup {
    let group = gio::SimpleActionGroup::new();

    let open_act = gio::SimpleAction::new("open-inline", None);
    open_act.connect_activate(|_, _| {
        with_inner(|inner| {
            if let Some(path) = selected_inline_image_temp(inner) {
                open_file_by_path(&path);
            }
        });
    });
    group.add_action(&open_act);

    let folder_act = gio::SimpleAction::new("show-inline", None);
    folder_act.connect_activate(|_, _| {
        with_inner(|inner| {
            if let Some(path) = selected_inline_image_temp(inner) {
                show_in_folder(&path);
            }
        });
    });
    group.add_action(&folder_act);

    group
}

/// 按当前选区同步两个菜单项的启用状态。
///
/// 菜单项带 `hidden-when=action-disabled`：禁用即隐藏，所以
/// **没选中内联图片时这两项根本不出现**，不会污染普通正文的右键菜单。
fn update_inline_menu_state(inner: &Inner) {
    let has = selected_inline_image(inner).is_some();
    for name in ["open-inline", "show-inline"] {
        if let Some(act) = inner
            .inline_actions
            .lookup_action(name)
            .and_downcast::<gio::SimpleAction>()
        {
            act.set_enabled(has);
        }
    }
}

/// 正文右键菜单：把「打开图片 / 在文件夹中显示」追加到 GTK 自己的菜单里。
///
/// 为什么不能自己挂右键手势抢占：`gtk_gesture_handle_event` 对 `GDK_BUTTON_PRESS`
/// **不返回 TRUE**（gtkgesture.c 只有 release 分支 `return was_claimed && was_recognized`），
/// 所以即使在 Capture 阶段抢先 claim，那条 press 依然会继续传给同一个 widget 上的
/// 其他手势 —— TextView 内建那个「任意键」手势照样收到，claim 后
/// `gtk_text_view_do_popup()` 弹出它自己那份剪切/复制菜单（表现就是「右键只有
/// 复制剪切删除」）。
///
/// 官方给的正路是 `set_extra_menu()`：gtktextview.c 的
/// `gtk_text_view_get_menu_model()` 里会 `gtk_joined_menu_append_menu (joined, priv->extra_menu)`，
/// 也就是把我们的项追加到它那份菜单后面。配合 `hidden-when=action-disabled`
/// 就能只在选中内联图片时才显示。
fn attach_inline_menu(text_view: &gtk::TextView) {
    text_view.insert_action_group("notepad", Some(&build_inline_actions()));

    let menu = gio::Menu::new();
    for (label, action) in [
        ("打开图片", "notepad.open-inline"),
        ("在文件夹中显示", "notepad.show-inline"),
    ] {
        let item = gio::MenuItem::new(Some(label), Some(action));
        item.set_attribute_value("hidden-when", Some(&"action-disabled".to_variant()));
        menu.append_item(&item);
    }
    text_view.set_extra_menu(Some(&menu));
}

/// 单击内联图片：把它选中（正好 1 个字符位，图片在 TextBuffer 里占一格）。
///
/// 返回 `false` 表示当前有拖选 / Shift 扩展出来的选区，不该覆盖掉。
fn select_inline_image(inner: &Inner, iter: &gtk::TextIter) -> bool {
    if inner.buffer.selection_bounds().is_some() {
        return false;
    }
    let mut end = *iter;
    end.forward_char();
    inner.buffer.select_range(iter, &end);
    true
}

/// 正文：内联图片单击选中 / 双击打开 / 右键菜单，URL Ctrl+点击。
fn attach_text_view_gestures(text_view: &gtk::TextView) {
    let text_view = text_view.clone();

    // ── 内联图片：单击选中、双击打开 ──
    let tv_img = text_view.clone();
    let img_clk = gtk::GestureClick::new();
    img_clk.set_button(1);
    img_clk.connect_released(move |_, n_press, x, y| {
        // released 的第二个参数就是点击次数（gtk4-rs 签名为 Fn(&Self, i32, f64, f64)）
        with_inner(|inner| {
            let Some((iter, _idx)) = inline_image_at(inner, &tv_img, x, y) else {
                return;
            };
            match n_press {
                // 单击：把这一张选中，让用户看清操作的是哪张图
                1 => {
                    select_inline_image(inner, &iter);
                }
                // 双击：用系统默认程序打开
                2 => {
                    if let Some(path) = inline_image_to_temp(inner, &iter) {
                        open_file_by_path(&path);
                    }
                }
                _ => {}
            }
        });
    });
    text_view.add_controller(img_clk);

    // 内联图片的右键菜单**不在这里**做 —— 见 attach_inline_menu()：
    // 自己挂右键手势是拦不住 TextView 内置菜单的（原因写在那个函数上）。

    // ── URL Ctrl+点击打开浏览器 ──
    let tv_url = text_view.clone();
    let url_ctl = gtk::GestureClick::new();
    url_ctl.set_button(1);
    url_ctl.connect_released(move |ctl, _, x, y| {
        if !ctl
            .current_event()
            .map(|e| e.modifier_state().contains(gdk::ModifierType::CONTROL_MASK))
            .unwrap_or(false)
        {
            return;
        }
        with_inner(|inner| {
            let (bx, by) =
                tv_url.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
            let Some(iter) = tv_url.iter_at_location(bx, by) else {
                return;
            };
            if let Some(url) = scan_url_at_iter(&inner.buffer, &iter) {
                open_url(&url);
            }
        });
    });
    text_view.add_controller(url_ctl);
}
/// URL 里「词」的字符（外观和点击必须用同一套定义，否则高亮的和打开的会对不上）。
fn is_url_word_char(c: char) -> bool {
    !matches!(
        c,
        ' ' | '\t' | '\n' | '\r' | '<' | '>' | '"' | '\'' | '(' | ')' | '{' | '}'
    )
}

/// 句末标点：中文句子里 URL 后面常直接跟标点（`https://x.com。`）。
fn is_url_trailing_punct(c: char) -> bool {
    matches!(
        c,
        '.' | ',' | ';' | ':' | '!' | '?' | '。' | '，' | '；' | '：' | '！' | '？' | '、' | '）' | '】' | '》'
    )
}

/// 在 `text` 里找 `char_offset` 所在的 URL。
///
/// 返回 `(起始字符下标, 结束字符下标[不含], 可直接打开的 URL)`。
///
/// ⚠️ `TextIter::offset()` 给的是**字符**下标，不是字节下标 —— 中英文混排时
/// 拿它去切 `as_bytes()` 会切错位置，进而「找得到高亮、点开却是别的东西」。
fn find_url_at(text: &str, char_offset: usize) -> Option<(usize, usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    if char_offset >= chars.len() {
        return None;
    }
    // 向前、向后扩到词边界
    let mut start = char_offset;
    while start > 0 && is_url_word_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = char_offset;
    while end < chars.len() && is_url_word_char(chars[end]) {
        end += 1;
    }
    while end > start && is_url_trailing_punct(chars[end - 1]) {
        end -= 1;
    }
    if end == start {
        return None;
    }

    let word: String = chars[start..end].iter().collect();
    let lower = word.to_ascii_lowercase();
    let open = if lower.starts_with("www.") {
        format!("https://{word}")
    } else if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("ftp://")
    {
        word
    } else {
        return None;
    };
    Some((start, end, open))
}

/// 扫出 `text` 里所有 URL 的字符区间 `[start, end)`。
fn scan_url_ranges(text: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if !is_url_word_char(chars[i]) {
            i += 1;
            continue;
        }
        match find_url_at(text, i) {
            Some((s, e, _)) => {
                out.push((s, e));
                // 跳过**整个词**：e 之后可能还留着被切掉的句末标点，
                // 停在 e 上会在下一轮把同一个 URL 再识别一次。
                i = e;
                while i < chars.len() && is_url_word_char(chars[i]) {
                    i += 1;
                }
            }
            // 不是 URL：整段词跳过，别在词里逐字符重试
            None => {
                while i < chars.len() && is_url_word_char(chars[i]) {
                    i += 1;
                }
            }
        }
    }
    out
}

/// 取 `iter` 位置上的 URL（Ctrl+点击用）。
fn scan_url_at_iter(buffer: &gtk::TextBuffer, iter: &gtk::TextIter) -> Option<String> {
    let text = buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string();
    find_url_at(&text, iter.offset() as usize).map(|(_, _, url)| url)
}

/// 给正文里所有 URL 套上超链接样式（下划线 + 链接色）。
///
/// 直接从 `changed` 回调里调用是安全的：GTK 的 `apply_tag` / `remove_tag`
/// 走 `gtk_text_buffer_emit_tag`，只发 `apply-tag` / `remove-tag`，
/// **不会**再发 `changed`（已核 gtktextbuffer.c），所以不存在递归。
/// 也因此重刷样式既不会置脏标记，也不会白写一次盘。
fn restyle_links(inner: &Inner) {
    inner.link_tag.set_property("foreground", link_color());

    let buffer = &inner.buffer;
    let text = buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string();
    let (start, end) = (buffer.start_iter(), buffer.end_iter());
    buffer.remove_tag(&inner.link_tag, &start, &end);
    for (s, e) in scan_url_ranges(&text) {
        let a = buffer.iter_at_offset(s as i32);
        let b = buffer.iter_at_offset(e as i32);
        buffer.apply_tag(&inner.link_tag, &a, &b);
    }
}

/// 超链接颜色：跟随深浅色主题（Adwaita 蓝 3 / 深色下的蓝 2）。
///
/// 没用 `StyleManager::accent_color_rgba()` —— 那个要 libadwaita ≥ 1.6，
/// 而本项目 deb/rpm/pacman 声明的是 ≥ 1.4。
fn link_color() -> &'static str {
    if adw::StyleManager::default().is_dark() {
        "#78aeed"
    } else {
        "#1c71d8"
    }
}

/// 用系统默认浏览器打开一个 URL。
///
/// 必须走 `launch_default_for_uri` **原样**传 URL：不能用 `open_file_by_path`
/// —— 那样会 `File::for_path("https://…")` 把双斜杠规范化成 `https:/…`，
/// 然后当成文件名去 `~/.local/share/.../https:/…` 里找，必然报「没有那个文件」。
fn open_url(url: &str) {
    let u = url.to_string();
    gio::AppInfo::launch_default_for_uri_async(
        url,
        None::<&gio::AppLaunchContext>,
        None::<&gio::Cancellable>,
        move |res| {
            if let Err(e) = res {
                eprintln!("打不开「{u}」：{e}");
            }
        },
    );
}

/// 删除附件前先确认。
fn confirm_delete_attachment(inner: &Inner) {
    if inner.current_id.borrow().is_none() {
        return;
    }
    let Some(name) = selected_attachment(inner) else {
        inner.toast("先选中一个附件");
        return;
    };
    let dialog = adw::MessageDialog::builder()
        .heading("删除附件")
        .body(&format!("确定删除「{name}」？此操作不可撤销。"))
        .build();
    if let Some(w) = inner.window() {
        dialog.set_transient_for(Some(&w));
    }
    dialog.add_response("cancel", "取消");
    dialog.add_response("delete", "删除");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let file_name = name;
    dialog.connect_response(Some("delete"), move |d, _| {
        with_inner(|i| {
            let Some(id) = i.current_id.borrow().clone() else {
                return;
            };
            storage::delete_file(&id, &file_name);
            refresh_attachments(i, &id);
            i.toast("已删除附件");
        });
        d.destroy();
    });
    dialog.present();
}

/// 按扩展名给个图标（不查 mime 库，够用且离线）。
fn icon_for_file(name: &str) -> &'static str {
    let ext = name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "doc" | "docx" | "odt" | "rtf" | "txt" | "md" => "x-office-document-symbolic",
        "ppt" | "pptx" | "odp" | "key" => "x-office-presentation-symbolic",
        "xls" | "xlsx" | "ods" | "csv" => "x-office-spreadsheet-symbolic",
        "pdf" => "application-pdf-symbolic",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" => "image-x-generic-symbolic",
        "mp4" | "mkv" | "avi" | "mov" | "webm" => "video-x-generic-symbolic",
        "mp3" | "flac" | "wav" | "ogg" | "m4a" => "audio-x-generic-symbolic",
        "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" => "package-x-generic-symbolic",
        _ => "text-x-generic-symbolic",
    }
}

// ── 辅助函数 ──────────────────────────────────────────────────────────────

/// 接上「选中即加载、正文/标题改动即保存」的信号。
///
/// 闭包一律走 `with_inner`（不捕获 `Rc`），调用前必须先把 `Inner` 存进 `INNER`。
fn wire() {
    with_inner(|inner| {
        // 选中条目 → 加载内容
        inner.list.connect_row_selected(|_, row| {
            let Some(row) = row else { return };
            let id = row.widget_name().to_string();
            with_inner(|i| load_entry(i, &id));
        });

        // 内容改变 → 自动保存（节流：正文逐键 < 200 ms 跳过写盘）+ 重刷链接样式
        inner.buffer.connect_changed(|_| {
            with_inner(|i| {
                save_current(i, false);
                restyle_links(i);
                update_inline_menu_state(i);
            })
        });

        // 选区变化 → 同步「打开图片 / 在文件夹中显示」的启用/隐藏状态
        inner.buffer.connect_notify(Some("has-selection"), |_, _| {
            with_inner(|i| update_inline_menu_state(i))
        });

        // 标题改变 → 自动保存（立即写盘）
        inner.title_entry.connect_changed(|_| with_inner(|i| save_current(i, true)));

        // 附件选中变化 → 打开 / 在文件夹中显示 / 删除 三个按钮跟着启用禁用。
        // 放这里（而不是 build 里）是为了让单测也覆盖到：测试环境只调 wire()，
        // 漏了这条线的话按钮会永远是灰的，而且编译、单测都不会报错。
        inner
            .att_box
            .connect_selected_children_changed(|_| with_inner(|i| update_attach_buttons(i)));
    });
}

/// 新建条目 → 刷新列表 → 立刻选中它。
fn create_entry(inner: &Inner) {
    let id = storage::create("新条目");
    refresh_list(inner);
    select_row_by_id(inner, &id);
    inner.title_entry.grab_focus();
}

/// 删除当前条目。
fn delete_current(inner: &Inner) {
    let Some(id) = inner.current_id.borrow().clone() else {
        inner.toast("没有选中条目");
        return;
    };
    storage::delete(&id);
    sync::record_deletion(&id);
    clear_editor(inner);
    refresh_list(inner);
}

/// 按 id 选中列表里的那一行（找不到就算了）。
fn select_row_by_id(inner: &Inner, id: &str) {
    let mut i = 0;
    while let Some(row) = inner.list.row_at_index(i) {
        if row.widget_name() == id {
            inner.list.select_row(Some(&row));
            return;
        }
        i += 1;
    }
}

/// 把条目的正文/标题/附件加载进编辑区。
///
/// 顺序很关键：先上锁并切 `current_id`，再写 buffer。
/// 写 buffer 会**同步**触发 `changed` → `save_current`，此时若 `current_id`
/// 还指向上一条，这一条的内容/标题就会被写进上一条（切换条目 = 互相覆盖，
/// 新建条目会把上一条清空）。`loading` 就是为这个同步回调准备的。
fn load_entry(inner: &Inner, id: &str) {
    let meta = storage::load_meta(id);
    let text = storage::read_content(id);
    inner.loading.set(true);
    *inner.current_id.borrow_mut() = Some(id.to_string());
    // 换条目 = 换一套内嵌图片，指纹作废（否则新条目的图会被判定为「没变」而漏写）
    inner.inline_fp.borrow_mut().clear();
    inner.buffer.set_text("");
    fill_buffer(&inner.buffer, id, &text);
    // 载入后立刻套上链接样式（此刻 loading 还是 true，不会触发写盘）
    restyle_links(inner);
    inner
        .title_entry
        .set_text(meta.as_ref().map(|m| m.title.as_str()).unwrap_or(""));
    inner.loading.set(false);
    update_inline_menu_state(inner);
    refresh_attachments(inner, id);
    update_state(inner);
}

/// 按「列表是不是空的 / 有没有选中条目」切换两处空态提示。
fn update_state(inner: &Inner) {
    let has_current = inner.current_id.borrow().is_some();
    inner
        .editor_stack
        .set_visible_child_name(if has_current { "editor" } else { "empty" });
    let no_entry = inner.rows.borrow().is_empty();
    inner.left_empty.set_visible(no_entry);
    inner.left_scroll.set_visible(!no_entry);
}

fn refresh_list(inner: &Inner) {
    let keep = inner.current_id.borrow().clone();
    inner.list.remove_all();
    inner.rows.borrow_mut().clear();
    let mut found = false;
    for meta in storage::load_all() {
        let row = adw::ActionRow::new();
        row.set_title(&meta.title);
        row.set_subtitle(&format_time(&meta.modified_at));
        row.set_widget_name(&meta.id);
        inner.list.append(&row);
        inner.rows.borrow_mut().push((meta.id.clone(), row));
        if keep.as_deref() == Some(meta.id.as_str()) {
            found = true;
        }
    }
    // 正在编辑的条目已被删掉/不在列表里 → 清空右栏，别让后续输入写回幽灵 ID。
    if keep.is_some() && !found {
        clear_editor(inner);
    }
    update_state(inner);
}

/// 清空右栏编辑区（不会把空内容写回磁盘）。
fn clear_editor(inner: &Inner) {
    inner.loading.set(true);
    *inner.current_id.borrow_mut() = None;
    inner.inline_fp.borrow_mut().clear();
    inner.buffer.set_text("");
    inner.title_entry.set_text("");
    inner.loading.set(false);
    inner.att_box.remove_all();
    inner.att_names.borrow_mut().clear();
    update_attach_buttons(inner);
}

/// 把秒级时间戳转成本地可读时间；解析不了就原样显示。
fn format_time(ts: &str) -> String {
    let Ok(secs) = ts.parse::<i64>() else {
        return ts.to_string();
    };
    glib::DateTime::from_unix_local(secs)
        .and_then(|dt| dt.format("%Y-%m-%d %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_else(|_| ts.to_string())
}

/// 保存当前编辑的内容到本地。
///
/// `force = true` 时跳过节流立即写盘（标题修改 / 手动保存前调用）；
/// `force = false` 时若距上次写盘 < 200 ms 则跳过（正文逐键输入节流）。
fn save_current(inner: &Inner, force: bool) {
    // 程序化加载内容（选中条目/清空右栏）期间不写盘。
    if inner.loading.get() {
        return;
    }
    inner.dirty.set(true);
    if !force {
        let now = now_ms();
        let last = LAST_SAVE_MS.with(|c| c.get());
        if now.saturating_sub(last) < 200 {
            return;
        }
        LAST_SAVE_MS.with(|c| c.set(now));
    } else {
        LAST_SAVE_MS.with(|c| c.set(now_ms()));
    }
    let Some(id) = inner.current_id.borrow().clone() else {
        return;
    };
    let title = inner.title_entry.text().to_string();
    let (content, images) = extract_buffer(&inner.buffer);
    let now = now_timestamp();
    let mut meta = storage::load_meta(&id).unwrap_or_else(|| MemoMeta {
        id: id.clone(),
        title: title.clone(),
        created_at: now.clone(),
        modified_at: String::new(),
    });
    meta.title = title;
    meta.modified_at = now;
    storage::save(&id, &meta, &content);
    sync_inline_images(inner, &id, &images);

    // 同步列表里的标题。只改这一行的文字，不重建列表 ——
    // 重建会把选中态和正文光标一起弄丢（用户正在打字）。
    for (rid, row) in inner.rows.borrow().iter() {
        if rid == &id {
            row.set_title(&meta.title);
            break;
        }
    }
}

fn now_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{secs}")
}

// ── 测试 ──────────────────────────────────────────────────────────────────
//
// 这几条锁的是「切换/新建条目时互相覆盖」这个回归：编辑区的 set_text 会同步
// 触发 changed → save_current，顺序写错就会把内容写进上一条。
//
// 需要能初始化 GTK（有显示时才能构造控件）；无显示环境自动跳过。
// 存储目录用临时 XDG_DATA_HOME 隔离，不碰真实 ~/.local/share/linbox。
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// GTK 只能在单一线程初始化，而 cargo test 每个用例一个线程 ——
    /// 所以几个场景必须在同一个 `#[test]` 里顺序跑。
    #[test]
    fn notepad_editor_does_not_leak_between_entries() {
        if gtk::init().is_err() {
            eprintln!("跳过：当前环境无法初始化 GTK");
            return;
        }
        let _ = adw::init();
        scenario_switching_entry();
        scenario_creating_entry();
        scenario_entry_deleted_elsewhere();
        scenario_multiple_attachments();
        scenario_inline_image_roundtrip();
        scenario_inline_index_two_images();
        scenario_inline_image_click_selects();
        scenario_url_detection();
        scenario_link_styling_applied();
        scenario_url_detection();
        scenario_link_restyle_on_edit();
        scenario_gesture_wiring();
        scenario_link_hover_wiring();
        scenario_attach_buttons_follow_selection();
        scenario_inline_image_lands_on_disk();
        scenario_inline_menu_items();
        scenario_throttled_save_is_flushed();
    }

    /// 建一个只有 notepad 存储的隔离环境（临时 XDG_DATA_HOME）。
    fn harness(tag: &str) -> Rc<Inner> {
        let dir: PathBuf = std::env::temp_dir().join(format!("linbox-notepad-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时数据目录失败");
        // SAFETY: 单测内单线程顺序执行，且其它模块不读 XDG_DATA_HOME。
        unsafe { std::env::set_var("XDG_DATA_HOME", &dir) };

        let buffer = gtk::TextBuffer::new(None);
        let link_tag = gtk::TextTag::builder()
            .underline(pango::Underline::Single)
            .build();
        buffer.tag_table().add(&link_tag);
        let inner = Rc::new(Inner {
            list: gtk::ListBox::new(),
            left_empty: gtk::Label::new(None),
            left_scroll: gtk::ScrolledWindow::new(),
            editor_stack: gtk::Stack::new(),
            rows: RefCell::new(Vec::new()),
            title_entry: adw::EntryRow::new(),
            buffer,
            att_box: gtk::FlowBox::new(),
            att_names: RefCell::new(Vec::new()),
            att_open: gtk::Button::new(),
            att_show: gtk::Button::new(),
            att_del: gtk::Button::new(),
            current_id: RefCell::new(None),
            inline_fp: RefCell::new(Vec::new()),
            loading: Cell::new(false),
            dirty: Cell::new(false),
            toast: adw::ToastOverlay::new(),
            link_tag,
            inline_actions: build_inline_actions(),
            window: RefCell::new(None),
        });
        // 单测也要走全局句柄：wire 的闭包是从 INNER 里取 Inner 的。
        INNER.with(|i| *i.borrow_mut() = Some(Rc::clone(&inner)));
        wire();
        inner
    }

    fn editor_text(inner: &Inner) -> String {
        let b = &inner.buffer;
        b.text(&b.start_iter(), &b.end_iter(), false).to_string()
    }

    /// 建一个带正文的条目。
    fn entry(title: &str, content: &str) -> String {
        let id = storage::create(title);
        let meta = storage::load_meta(&id).expect("刚建的条目应能读到 meta");
        storage::save(&id, &meta, content);
        id
    }

    /// 模拟点列表里的某一行。
    fn select_row(inner: &Inner, id: &str) {
        let mut i = 0;
        while let Some(row) = inner.list.row_at_index(i) {
            if row.widget_name() == id {
                inner.list.select_row(Some(&row));
                return;
            }
            i += 1;
        }
        panic!("列表里找不到条目 {id}");
    }

    /// 切换条目不得把上一条的正文/标题覆盖掉。
    fn scenario_switching_entry() {
        let inner = harness("switch");
        let a = entry("条目A", "AAA");
        let b = entry("条目B", "BBB");
        refresh_list(&inner);

        select_row(&inner, &a);
        assert_eq!(editor_text(&inner), "AAA");
        assert_eq!(inner.title_entry.text(), "条目A");

        // 用户编辑 A 的正文（set_text 同步触发 changed → save_current）
        inner.buffer.set_text("AAA-edited");
        // 节流场景：测试连续操作极快，强制写盘确保内容落盘
        save_current(&inner, true);
        assert_eq!(storage::read_content(&a), "AAA-edited");

        // 切到 B：修复前这里会把 B 的正文/标题写进 A
        select_row(&inner, &b);
        assert_eq!(editor_text(&inner), "BBB");
        assert_eq!(inner.title_entry.text(), "条目B");
        assert_eq!(
            storage::read_content(&a),
            "AAA-edited",
            "切到 B 把 A 的正文覆盖了"
        );
        assert_eq!(
            storage::load_meta(&a).unwrap().title,
            "条目A",
            "切到 B 把 A 的标题覆盖了"
        );
    }

    /// 新建条目时不得把正在编辑的上一条清空/改名。
    fn scenario_creating_entry() {
        let inner = harness("create");
        let a = entry("条目A", "AAA");
        refresh_list(&inner);
        select_row(&inner, &a);
        assert_eq!(editor_text(&inner), "AAA");

        // 走真实路径：新建 → 刷新列表 → 选中新条目
        let c = entry("新条目", "");
        refresh_list(&inner);
        select_row(&inner, &c);

        assert_eq!(
            storage::read_content(&a),
            "AAA",
            "新建条目把上一条的正文清空了"
        );
        assert_eq!(
            storage::load_meta(&a).unwrap().title,
            "条目A",
            "新建条目把上一条标题改了"
        );
        assert_eq!(editor_text(&inner), "");
        assert_eq!(inner.title_entry.text(), "新条目");
    }

    /// 当前条目在磁盘上消失后，刷新列表必须断开右栏，不能让后续输入写进别的条目。
    fn scenario_entry_deleted_elsewhere() {
        let inner = harness("prune");
        let a = entry("条目A", "AAA");
        let b = entry("条目B", "BBB");
        refresh_list(&inner);
        select_row(&inner, &a);
        assert_eq!(editor_text(&inner), "AAA");

        storage::delete(&a);
        refresh_list(&inner);

        assert!(
            inner.current_id.borrow().is_none(),
            "条目没了但 current_id 还在指向它"
        );
        assert_eq!(editor_text(&inner), "");
        assert_eq!(inner.title_entry.text(), "");

        // 之后乱敲不能写回任何地方（尤其不能写回仍然存在的 B）
        inner.buffer.set_text("孤儿输入");
        assert_eq!(storage::read_content(&b), "BBB", "内容被写进了别的条目");
    }

    /// 一个条目必须能挂多个附件：截图工具导出的文件名高度雷同，
    /// 写盘重名覆盖的话，条目里永远只剩最后加的那一个。
    fn scenario_multiple_attachments() {
        let inner = harness("files");
        let a = entry("条目A", "AAA");
        refresh_list(&inner);
        select_row(&inner, &a);

        let n1 = storage::write_file(&a, "报告.docx", b"one");
        let n2 = storage::write_file(&a, "报告.docx", b"two");
        let n3 = storage::write_file(&a, "slides.pptx", b"three");
        assert!(n1.is_some() && n2.is_some() && n3.is_some());
        assert_ne!(n1.unwrap(), n2.unwrap(), "同名附件互相覆盖了");
        assert_eq!(storage::list_files(&a).len(), 3, "一个条目只能存下部分附件");
    }

    /// 正文夹图片：写盘 → 重新加载，图片必须还在原位。
    fn scenario_inline_image_roundtrip() {
        let inner = harness("inline");
        let a = entry("条目A", "");
        refresh_list(&inner);
        select_row(&inner, &a);

        // 造一张 4x4 的小图插到正文里（前后各留点文字，验证位置）
        let pixbuf = Pixbuf::new(Colorspace::Rgb, false, 8, 4, 4).expect("造测试图片失败");
        inner.buffer.set_text("前");
        insert_pixbuf_at_cursor(&inner, &pixbuf);
        inner
            .buffer
            .insert(&mut inner.buffer.end_iter(), "后");
        save_current(&inner, true);

        let saved = storage::read_content(&a);
        assert_eq!(saved, format!("前{}后", OBJ), "正文里的图片位置存丢了");
        assert!(
            storage::read_inline(&a, 0).is_some(),
            "内嵌图片没有落盘"
        );

        // 重新加载：图片必须回到正文里
        // 注意 GtkTextBuffer::text() 是不含图片的（GTK 会跳过图片段），
        // 所以只能断言「文字」+「第 1 个位置上是张图」。
        load_entry(&inner, &a);
        assert_eq!(editor_text(&inner), "前后", "重新加载后正文文字不对");
        let mut iter = inner.buffer.start_iter();
        iter.forward_char();
        assert!(
            iter.paintable().is_some(),
            "重新加载后正文里的图片不见了"
        );
        // 双击/右键打开时靠这个索引去找 inline/N.png，算错就会打开别的图或打不开
        assert_eq!(
            inline_index_at(&inner.buffer, &iter),
            0,
            "点中第 1 张图时索引算错了"
        );
    }

    /// 正文里夹两张图时，索引必须按出现顺序递增。
    fn scenario_inline_index_two_images() {
        let inner = harness("inline-idx");
        let a = entry("条目A", "");
        refresh_list(&inner);
        select_row(&inner, &a);

        let pixbuf = Pixbuf::new(Colorspace::Rgb, false, 8, 4, 4).expect("造测试图片失败");
        inner.buffer.set_text("甲");
        insert_pixbuf_at_cursor(&inner, &pixbuf);
        inner.buffer.insert(&mut inner.buffer.end_iter(), "乙");
        insert_pixbuf_at_cursor(&inner, &pixbuf);
        inner.buffer.insert(&mut inner.buffer.end_iter(), "丙");
        save_current(&inner, true);

        load_entry(&inner, &a);
        // 逐个 offset 检查：第 1 张图索引 0，第 2 张图索引 1
        let mut seen = Vec::new();
        let mut it = inner.buffer.start_iter();
        while !it.is_end() {
            if it.paintable().is_some() {
                seen.push(inline_index_at(&inner.buffer, &it));
            }
            it.forward_char();
        }
        assert_eq!(seen, vec![0, 1], "两张图的索引不是 0、1");
    }

    /// 单击内联图片 = 把它选中（正好 1 个字符位）；已有拖选时不许覆盖。
    fn scenario_inline_image_click_selects() {
        let inner = harness("inline-select");
        let a = entry("条目A", "");
        refresh_list(&inner);
        select_row(&inner, &a);

        let pixbuf = Pixbuf::new(Colorspace::Rgb, false, 8, 4, 4).expect("造测试图片失败");
        inner.buffer.set_text("前");
        insert_pixbuf_at_cursor(&inner, &pixbuf);
        inner.buffer.insert(&mut inner.buffer.end_iter(), "后");
        save_current(&inner, true);
        load_entry(&inner, &a);

        // 找到正文里图片所在的 iter
        let mut img = inner.buffer.start_iter();
        while !img.is_end() && img.paintable().is_none() {
            img.forward_char();
        }
        assert!(img.paintable().is_some(), "没找到正文里的图片");

        // 没有选区时：单击 → 图片被选中，且选区正好盖住那一格
        inner.buffer.place_cursor(&img);
        assert!(
            select_inline_image(&inner, &img),
            "单击内联图片应该要选中它"
        );
        let (s, e) = inner
            .buffer
            .selection_bounds()
            .expect("单击后应该产生选区");
        let (lo, hi) = (s.offset().min(e.offset()), s.offset().max(e.offset()));
        assert_eq!(
            (lo, hi),
            (img.offset(), img.offset() + 1),
            "选区没有正好盖住图片那一格"
        );

        // 已有选区（拖选 / Shift 扩展）时不许覆盖
        assert!(
            !select_inline_image(&inner, &img),
            "已有选区时不该覆盖掉用户的选区"
        );
    }

    /// 取正文里所有带 link_tag 的连续区间（按出现顺序）。
    fn tagged_ranges(inner: &Inner) -> Vec<String> {
        let text: Vec<char> = editor_text(inner).chars().collect();
        let mut out = Vec::new();
        let mut it = inner.buffer.start_iter();
        while !it.is_end() {
            if !it.has_tag(&inner.link_tag) {
                it.forward_char();
                continue;
            }
            let start = it.offset() as usize;
            let mut end = it;
            while !end.is_end() && end.has_tag(&inner.link_tag) {
                end.forward_char();
            }
            let stop = end.offset() as usize;
            out.push(text[start..stop].iter().collect::<String>());
            it = end;
        }
        out
    }

    /// 载入带 URL 的正文后，URL 区间必须真被套上链接标签（下划线 + 链接色）。
    fn scenario_link_styling_applied() {
        let inner = harness("link-style");
        let a = entry("条目A", "见 https://a.com/x 好了");
        refresh_list(&inner);
        select_row(&inner, &a);

        assert_eq!(
            tagged_ranges(&inner),
            vec!["https://a.com/x"],
            "链接样式没套上，或套到了错误区间"
        );
        assert_eq!(
            inner.link_tag.property::<pango::Underline>("underline"),
            pango::Underline::Single,
            "链接标签没有下划线"
        );
    }

    /// URL 识别：字符下标（不是字节）、句末标点、www 补 scheme、#fragment、中文混排。
    fn scenario_url_detection() {
        let chars_of = |t: &str| -> Vec<char> { t.chars().collect() };
        let slice = |t: &str, s: usize, e: usize| -> String { chars_of(t)[s..e].iter().collect() };

        // 1) 中英混排：必须按「字符」下标定位（按字节切会切偏）
        let t = "见 https://a.com/x 好了";
        let (s, e, u) = find_url_at(t, 5).expect("应该认出 URL");
        assert_eq!(slice(t, s, e), "https://a.com/x");
        assert_eq!(u, "https://a.com/x");

        // 2) 中文句号要被切掉
        let t = "看 https://a.com。 好";
        let (_, _, u) = find_url_at(t, 5).expect("应该认出 URL");
        assert_eq!(u, "https://a.com", "句末标点被粘进 URL 了");

        // 3) www. 开头自动补 scheme
        let t = "www.example.com";
        let (_, _, u) = find_url_at(t, 1).expect("应该认出 URL");
        assert_eq!(u, "https://www.example.com");

        // 4) 带 #fragment 的 URL 要完整保留
        let t = "打开 https://m.mipay.com/deduct/recission#/RecissionList 看看";
        let (_, _, u) = find_url_at(t, 10).expect("应该认出 URL");
        assert_eq!(u, "https://m.mipay.com/deduct/recission#/RecissionList");

        // 5) 没有 scheme 的不算
        assert!(find_url_at("只是普通文字", 0).is_none());
        assert!(find_url_at("a.com/path", 0).is_none(), "无 scheme 不该当 URL");

        // 6) 全文扫描：两个 URL 各推一次（句末标点不能导致重复识别）
        let t = "第一行 https://a.com\n第二行 http://b.org/x。";
        let got: Vec<String> = scan_url_ranges(t)
            .iter()
            .map(|(s, e)| slice(t, *s, *e))
            .collect();
        assert_eq!(
            got,
            vec!["https://a.com", "http://b.org/x"],
            "扫描结果不对/有重复"
        );
    }

    /// 编辑新增的 URL 要立刻获得链接样式 —— 锁住 `changed` → `restyle_links` 那条链。
    fn scenario_link_restyle_on_edit() {
        let inner = harness("link-edit");
        let a = entry("条目A", "没有链接");
        refresh_list(&inner);
        select_row(&inner, &a);
        assert!(tagged_ranges(&inner).is_empty(), "一开始不该有链接区间");

        // 用户手打一个 URL 进正文
        inner
            .buffer
            .insert(&mut inner.buffer.end_iter(), " 看 https://b.org/y ");
        assert_eq!(
            tagged_ranges(&inner),
            vec!["https://b.org/y"],
            "编辑新增的 URL 没被套上链接样式"
        );

        // 顺手确认：打链接不该破坏原有内容
        assert_eq!(editor_text(&inner), "没有链接 看 https://b.org/y ");
    }

    /// 内联图片要能落成磁盘上的**真文件**（「打开 / 在文件夹中显示」都要用它），
    /// 而且落点必须和附件一致（同一个 tmp 目录），不能是系统临时目录。
    fn scenario_inline_image_lands_on_disk() {
        let inner = harness("inline-path");
        let a = entry("条目A", "");
        refresh_list(&inner);
        select_row(&inner, &a);

        let png = Pixbuf::new(Colorspace::Rgb, false, 8, 4, 4).expect("造测试图片失败");
        inner.buffer.set_text("前");
        insert_pixbuf_at_cursor(&inner, &png);
        save_current(&inner, true);

        let mut img = inner.buffer.start_iter();
        while !img.is_end() && img.paintable().is_none() {
            img.forward_char();
        }
        let path = inline_image_to_temp(&inner, &img).expect("内联图片应该能导出到磁盘");

        assert!(path.exists(), "导出的文件不存在：{}", path.display());
        let parent = path.parent().expect("应该有父目录").to_path_buf();
        assert_eq!(
            parent.file_name().and_then(|s| s.to_str()),
            Some("tmp"),
            "内联图片的目录不对：{}",
            path.display()
        );

        // 必须是数据目录下，不能是系统临时目录（那个会被清理，位置也飘）
        let data_root = std::env::var("XDG_DATA_HOME").expect("harness 设过 XDG_DATA_HOME");
        assert!(
            path.starts_with(&data_root),
            "内联图片落到了数据目录外：{}",
            path.display()
        );

        // 内容要和 ZIP 里的一致
        assert_eq!(
            std::fs::read(&path).ok(),
            storage::read_inline(&a, 0),
            "导出的图片内容和 ZIP 里的对不上"
        );

        // 附件用的是同一个 tmp 目录 —— 「在文件夹中显示」的落点要统一
        assert!(storage::write_file(&a, "报告.docx", b"one").is_some());
        let att = storage::file_path(&a, "报告.docx");
        assert_eq!(
            att.parent(),
            Some(parent.as_path()),
            "附件和内联图片的 tmp 目录不一致"
        );
    }

    /// 附件工具栏三个按钮（打开 / 在文件夹中显示 / 删除）必须跟着选中状态启用禁用。
    ///
    /// 新增按钮最容易漏掉的就是 `update_attach_buttons` 那条链 —— 漏了按钮就永远是灰的。
    fn scenario_attach_buttons_follow_selection() {
        let inner = harness("attach-buttons");
        let a = entry("条目A", "AAA");
        refresh_list(&inner);
        select_row(&inner, &a);

        // 一条附件都没有
        assert!(!inner.att_open.is_sensitive(), "没附件时「打开」不该可用");
        assert!(!inner.att_show.is_sensitive(), "没附件时「在文件夹中显示」不该可用");
        assert!(!inner.att_del.is_sensitive(), "没附件时「删除」不该可用");

        assert!(storage::write_file(&a, "报告.docx", b"one").is_some());
        refresh_attachments(&inner, &a);

        // 有附件但没选中：仍然全部禁用
        assert!(
            !inner.att_show.is_sensitive(),
            "没选中附件时「在文件夹中显示」不该可用"
        );

        // 选中一个附件 → 三个都启用（靠 selected-children-changed 那条信号链）
        let child = inner.att_box.child_at_index(0).expect("附件列表里应该有第一项");
        inner.att_box.select_child(&child);
        assert!(inner.att_open.is_sensitive(), "选中后「打开」该可用");
        assert!(
            inner.att_show.is_sensitive(),
            "选中后「在文件夹中显示」该可用 —— 是不是漏了 update_attach_buttons？"
        );
        assert!(inner.att_del.is_sensitive(), "选中后「删除」该可用");
    }

    /// 节流窗口内的按键不会落盘 → `flush()` 必须能把它补上。
    ///
    /// 锁的是「关窗丢字」：`save_current(force=false)` 在距上次写盘 < 200 ms 时
    /// **直接 return 且不补写**（没有延迟写定时器），关窗前不强制 flush 的话，
    /// 最后敲进去的几个字既不在磁盘上、也不会被推上远端。
    fn scenario_throttled_save_is_flushed() {
        let inner = harness("flush");
        let a = entry("条目A", "");
        refresh_list(&inner);
        select_row(&inner, &a);

        inner.buffer.set_text("A");
        save_current(&inner, true);
        assert_eq!(storage::read_content(&a), "A");

        // 紧接着再改：force=false 会被 200 ms 节流吃掉（这里是在描述现状）
        inner.buffer.insert(&mut inner.buffer.end_iter(), "B");
        save_current(&inner, false);
        assert_eq!(
            storage::read_content(&a),
            "A",
            "节流窗口内不该写盘 —— 现状描述，若这条变了说明节流被改了"
        );

        // flush() 必须把丢掉的那次补上（关窗前调的就是它）
        flush();
        assert_eq!(
            storage::read_content(&a),
            "AB",
            "flush() 没把节流丢掉的内容补写，关窗就会丢字"
        );
    }

    /// 内联图片的右键菜单项：追加进 GTK 自己的菜单，且**只选中图片时才显示**。
    ///
    /// 显示与否靠 `hidden-when=action-disabled` —— 也就是「action 禁用 = 菜单项隐藏」，
    /// 所以这里断言的就是启用状态。
    fn scenario_inline_menu_items() {
        let inner = harness("inline-menu");
        let a = entry("条目A", "");
        refresh_list(&inner);
        select_row(&inner, &a);

        let act = |name: &str| -> gio::SimpleAction {
            inner
                .inline_actions
                .lookup_action(name)
                .unwrap_or_else(|| panic!("菜单动作 {name} 不存在"))
                .downcast::<gio::SimpleAction>()
                .expect("应该是 SimpleAction")
        };

        // 什么都没选中 → 两项都禁用（配合 hidden-when 就是不出现）
        assert!(
            !act("open-inline").is_enabled(),
            "没选中图片时「打开图片」不该可用"
        );
        assert!(
            !act("show-inline").is_enabled(),
            "没选中图片时「在文件夹中显示」不该可用"
        );

        // 插一张图并选中它（单击选中走的正是 select_inline_image）
        let png = Pixbuf::new(Colorspace::Rgb, false, 8, 4, 4).expect("造测试图片失败");
        inner.buffer.set_text("前");
        insert_pixbuf_at_cursor(&inner, &png);
        save_current(&inner, true);

        let mut img = inner.buffer.start_iter();
        while !img.is_end() && img.paintable().is_none() {
            img.forward_char();
        }
        assert!(select_inline_image(&inner, &img), "应该选中了图片");
        assert!(
            selected_inline_image(&inner).is_some(),
            "选区判定没认出这是一张内联图片"
        );
        update_inline_menu_state(&inner);
        assert!(act("open-inline").is_enabled(), "选中图片后「打开图片」该可用");
        assert!(
            act("show-inline").is_enabled(),
            "选中图片后「在文件夹中显示」该可用"
        );

        // 改选中普通文字 → 又该禁用（别把文字选区当成图片）
        let mut s0 = inner.buffer.start_iter();
        let mut e1 = s0;
        e1.forward_char();
        inner.buffer.select_range(&s0, &e1);
        assert!(
            selected_inline_image(&inner).is_none(),
            "普通文字的选区不该被当成内联图片"
        );
        update_inline_menu_state(&inner);
        assert!(
            !act("show-inline").is_enabled(),
            "选中文字时「在文件夹中显示」不该可用"
        );
        let _ = &mut s0;

        // 菜单模型确实追加进了 TextView（GTK 会把它拼到自己那份后面）
        let tv = gtk::TextView::new();
        assert!(tv.extra_menu().is_none(), "新 TextView 不该自带 extra_menu");
        attach_inline_menu(&tv);
        assert!(tv.extra_menu().is_some(), "没把菜单追加进 TextView");
    }

    /// 链接悬停反馈的接线：少任何一根线，用户看到的就是「鼠标不变手型 / 没有提示」。
    ///
    /// 这几条都容易被"顺手删掉"而不报错（编译一样过），所以钉住：
    /// - motion 控制器：光标得跟着鼠标走；
    /// - key 控制器必须在 **Capture** 阶段：鼠标不动只按 Ctrl 时没有 motion 事件，
    ///   只能靠 `modifiers` 信号补；
    /// - 必须开 tooltip，否则悬停提示不会出现；
    /// - TextView 自带的光标（"text"）要能被读到，退出链接时才能还原成 I 型。
    fn scenario_link_hover_wiring() {
        let tv = gtk::TextView::new();
        let before = tv.observe_controllers().n_items();
        attach_link_hover(&tv);

        assert!(tv.has_tooltip(), "没开 tooltip，悬停提示不会出现");
        assert!(
            tv.cursor().is_some(),
            "读不到 TextView 原本的光标，退出链接时无法还原 I 型"
        );

        let ctrls = tv.observe_controllers();
        assert_eq!(
            ctrls.n_items(),
            before + 2,
            "应该新增 motion + key 两个控制器"
        );
        // 注意 observe_controllers() 是按**阶段排序**返回的（Capture 在前），
        // 不是插入顺序 —— 所以要把所有 key 控制器的阶段都收起来判断。
        // TextView 自己也带一个 key 控制器（Bubble），别把它当成了我们的。
        let phases: Vec<gtk::PropagationPhase> = (0..ctrls.n_items())
            .filter_map(|i| ctrls.item(i).and_downcast::<gtk::EventControllerKey>())
            .map(|k| k.propagation_phase())
            .collect();
        assert!(
            phases.contains(&gtk::PropagationPhase::Capture),
            "没有 Capture 阶段的 key 控制器，鼠标不动时按 Ctrl 收不到通知：{phases:?}"
        );
        assert!(
            phases.contains(&gtk::PropagationPhase::Bubble),
            "TextView 自带的 key 控制器不该被动过：{phases:?}"
        );
    }

    /// 交互接线回归：这几处接错，用户看到的就是「点了没反应」。
    ///
    /// 1. 附件只保留右键菜单，不动 `activate-on-single-click`（双击打开已删）；
    /// 2. 正文内联图片的右键手势必须在 Capture 阶段：TextView 对右键会自己
    ///    claim 按键序列（gtktextview.c），Bubble 阶段的手势永远收不到事件。
    fn scenario_gesture_wiring() {
        let inner = harness("gestures");

        // 附件只挂右键菜单；双击打开已按需求删掉，
        // 所以「单击即激活」要留成 GTK 默认值，不能为了双击去动它。
        attach_attachment_gestures(&inner.att_box);
        assert!(
            inner.att_box.activates_on_single_click(),
            "附件列表的 activate-on-single-click 不该被动过"
        );

        let tv = gtk::TextView::new();
        attach_text_view_gestures(&tv);
        let ctrls = tv.observe_controllers();
        let mut buttons = Vec::new();
        for i in 0..ctrls.n_items() {
            let Some(c) = ctrls.item(i).and_downcast::<gtk::GestureClick>() else {
                continue;
            };
            // button()==0 是 GTK 自己内建的那个「任意键」手势（gtktextview.c:
            // gtk_gesture_single_set_button(gesture, 0)），不算我们的。
            if c.button() == 0 {
                continue;
            }
            buttons.push(c.button());
        }
        buttons.sort_unstable();
        // 只留两个左键手势（单击选中 / 双击打开 + URL Ctrl+点击）。
        // 右键**故意没有手势**：GTK 对手势的 press 事件不认 claim，拦不住
        // TextView 内置菜单，改走 set_extra_menu（见 attach_inline_menu）。
        assert_eq!(buttons, vec![1, 1], "正文左键手势数量/按键接错了");
        assert!(
            !buttons.contains(&3),
            "不该再挂右键手势 —— 它拦不住 TextView 的内置菜单"
        );
    }
}
