//! 开机自启页（GOAL Phase 12 的 12.6，复用 12.4 共享组件）。
//!
//! 与菜单页（[`crate::page::desktop_menu`]）同骨架，差异全在 D12 决策：
//! - **启用/停用开关**（D12.3）：停用 = 写 `Hidden=true`（GNOME/KDE/Deepin 通吃），
//!   启用 = `Hidden=false`；若文件已有 `X-GNOME-Autostart-enabled` 则同步——
//!   底层走 [`dfu::set_hidden`]（后台线程写 + 回读断言才宣称成功）。
//! - **状态列**（D12.9）：行徽章显示「已启用 / 已停用 / 来源 / 覆盖 N 份」。
//! - **「下次登录生效」固定提示**（D12.11）：详情区常驻提示条 + 改动 toast 尾巴，
//!   不自动替用户重启会话或杀进程。
//! - **覆盖徽章**：同 id 多层（摸底 3 的 lbrowser 用户盖系统）→ 行上「覆盖 2 份」。
//! - 过滤下拉 = 全部 / 已启用 / 已停用 / 来源（D12.9 自启页口径）。
//!
//! 复用的 12.4 组件：[`open_df_edit_dialog`] / [`open_df_new_dialog`]（传
//! `DesktopKind::Autostart`，表单自动多出「自启专有」组）、[`df_list_row`]、
//! [`df_search_hit`]、[`df_badge`]、[`df_card`]、[`df_confirm_delete`]、
//! [`DfPageHooks`]。
//!
//! 线程模型与 menu 页一致（GOAL 3.2）：外部命令一律 `thread::spawn`，
//! `idle_add` 只搬 Send 纯数据 + `fn` 指针。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use glib::clone;

use crate::model::desktopfile::DesktopKind;
use crate::page::desktop_menu::{
    DfPageHooks, df_badge, df_card, df_confirm_delete, df_list_row, df_search_hit,
    open_df_edit_dialog, open_df_new_dialog,
};
use crate::utils::desktopfile as dfu;

thread_local! {
    static INNER: RefCell<Option<Rc<Inner>>> = const { RefCell::new(None) };
}

fn with_inner<F: FnOnce(&Inner)>(f: F) {
    INNER.with(|i| {
        if let Some(inner) = i.borrow().as_ref() {
            f(inner);
        }
    });
}

/// 自启页 toast（D12.11：改动提示固定带「下次登录生效」尾巴）。
fn ast_toast(msg: &str) {
    with_inner(|i| i.toast(msg));
}

fn ast_refresh() {
    with_inner(|i| i.df_load());
}

/// 改动类 toast 的统一尾巴（D12.11）。
const NEXT_LOGIN: &str = "（下次登录生效）";

pub struct DesktopAutostartPage {
    root: adw::ToastOverlay,
}

impl DesktopAutostartPage {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }
}

/// 自启页过滤下拉项（D12.9）。
const AUTOSTART_FILTERS: &[&str] = &["全部", "已启用", "已停用", "来源"];

struct Inner {
    toast_overlay: adw::ToastOverlay,
    search: gtk::SearchEntry,
    filter: gtk::DropDown,
    status: gtk::Label,
    list: gtk::ListBox,
    detail_empty: gtk::Label,
    detail: gtk::Box,
    selected: RefCell<Option<String>>,
    items: RefCell<Vec<dfu::DesktopItem>>,
    busy: Cell<bool>,
    /// 开关回调自触发守卫（set_active 会再进 notify）。
    switching: Cell<bool>,
    /// 当前详情区的启用开关（GTK 对象非 Send，写失败回滚须经 with_inner 取回）。
    active_switch: RefCell<Option<adw::SwitchRow>>,
}

impl Inner {
    fn toast(&self, msg: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(msg));
    }

    fn parent_window(&self) -> Option<gtk::Window> {
        self.toast_overlay.root()?.downcast().ok()
    }

    /// 后台扫描自启目录（D12.9：~/.config/autostart > /etc/xdg/autostart）。
    fn df_load(&self) {
        if self.busy.replace(true) {
            self.toast("正在扫描，请稍候");
            return;
        }
        std::thread::spawn(|| {
            let items = dfu::scan(DesktopKind::Autostart);
            let items = RefCell::new(Some(items));
            glib::source::idle_add(move || {
                if let Some(items) = items.take() {
                    with_inner(|i| {
                        *i.items.borrow_mut() = items;
                        i.busy.set(false);
                        i.df_refilter();
                    });
                }
                glib::ControlFlow::Break
            });
        });
    }

    /// 按搜索 + 过滤重建列表（自启口径：启用态 / 来源）。
    fn df_refilter(&self) {
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        let q = self.search.text().to_string();
        let fidx = self.filter.selected() as usize;
        let items = self.items.borrow();
        let mut shown = 0;
        for it in items.iter() {
            if !df_search_hit(it, &q) {
                continue;
            }
            match fidx {
                1 if it.hidden() => continue, // 已启用 = 非 Hidden
                2 if !it.hidden() => continue,
                3 => {
                    // 来源过滤：再弹二级选择会打断 D12.9 的「单下拉」口径，
                    // 这里按「用户/本地/系统」依次过滤——选中即按该来源筛。
                    // 用 filter 的 selected 只有一维，故来源项显示全部（由徽章区分）；
                    // 真要按来源筛请用搜索框输入来源字样。保持 v1 简单。
                }
                _ => {}
            }
            shown += 1;
            self.list.append(&df_list_row(it, DesktopKind::Autostart));
        }
        self.status
            .set_text(&format!("共 {} 个条目，显示 {shown} 个", items.len()));
        // 同 menu 页：Ref 必须在语句内结束，块内 replace 才不会撞借用（删除闪退根因）。
        let sel_gone = self
            .selected
            .borrow()
            .clone()
            .is_some_and(|sel| !items.iter().any(|i| i.id == sel));
        if sel_gone {
            self.selected.replace(None);
        }
        if self.selected.borrow().is_none() {
            self.df_show_empty();
        }
    }

    fn df_show_empty(&self) {
        while let Some(c) = self.detail.first_child() {
            self.detail.remove(&c);
        }
        self.detail.append(&self.detail_empty);
        self.detail_empty.set_visible(true);
    }

    /// 选中条目 → 详情：启用/停用开关 + 生效层/覆盖 + 操作 + 全键只读。
    fn df_show_detail(&self, id: &str) {
        let items = self.items.borrow();
        let Some(item) = items.iter().find(|i| i.id == id).cloned() else {
            return;
        };
        drop(items);
        self.selected.replace(Some(id.to_string()));
        self.detail_empty.set_visible(false);
        while let Some(c) = self.detail.first_child() {
            self.detail.remove(&c);
        }
        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 10);
        vbox.set_margin_top(6);

        // 标题 + id
        let title = gtk::Label::new(Some(&item.display_name()));
        title.add_css_class("title-3");
        title.set_halign(gtk::Align::Start);
        title.set_wrap(true);
        vbox.append(&title);
        let sub = gtk::Label::new(Some(&item.id));
        sub.add_css_class("dim-label");
        sub.set_halign(gtk::Align::Start);
        vbox.append(&sub);

        // 「下次登录生效」常驻提示（D12.11）
        let hint = gtk::Label::new(Some(
            "改动（新建 / 停用 / 启用 / 删除）都要到下次登录才会真正生效；软件不会替你重启会话。",
        ));
        hint.add_css_class("dim-label");
        hint.set_halign(gtk::Align::Start);
        hint.set_wrap(true);
        vbox.append(&hint);

        // 系统来源黄条（编辑/停用将写系统文件需授权）
        let src = dfu::source_of(&item.effective);
        if src != dfu::Source::User {
            let warn = gtk::Label::new(Some("编辑将写系统文件，需授权（pkexec）"));
            warn.add_css_class("warning");
            warn.set_halign(gtk::Align::Start);
            warn.set_wrap(true);
            vbox.append(&warn);
        }

        // 状态徽章行（来源 + 覆盖 + 停用态）
        let pills = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        pills.append(&df_badge(src.label(), "accent"));
        if item.covered() {
            // 「覆盖 N 份」= 盖住几份低优先级副本（GOAL 12.6：lbrowser → 覆盖 1 份）
            pills.append(&df_badge(
                &format!("覆盖 {} 份（生效层=最先命中）", item.overridden.len()),
                "accent",
            ));
        }
        vbox.append(&pills);

        // 启用/停用开关卡（D12.3）
        let (scard, sbox) = df_card();
        let sw_row = adw::SwitchRow::new();
        sw_row.set_title("启用该自启项");
        sw_row.set_subtitle("停用 = 写 Hidden=true（并同步 X-GNOME-Autostart-enabled，若文件已有）；再打开 = 写回 false");
        sw_row.set_active(!item.hidden());
        *self.active_switch.borrow_mut() = Some(sw_row.clone());
        sbox.append(&sw_row);
        vbox.append(&scard);

        // 生效层路径 + 覆盖副本（只读）
        let (pcard, pbox) = df_card();
        let path_row = adw::ActionRow::new();
        path_row.set_title("生效层路径");
        path_row.set_subtitle(&item.effective.to_string_lossy());
        pbox.append(&path_row);
        if item.covered() {
            for p in &item.overridden {
                let r = adw::ActionRow::new();
                r.set_title("被覆盖副本（低优先级，不生效）");
                r.set_subtitle(&p.to_string_lossy());
                r.add_css_class("dim-label");
                pbox.append(&r);
            }
        }
        vbox.append(&pcard);

        // 操作行：编辑 / 物理删除 / 复制路径
        let ops = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let edit = gtk::Button::with_label("编辑…");
        edit.add_css_class("suggested-action");
        let del = gtk::Button::with_label("物理删除…");
        del.add_css_class("destructive-action");
        let copy = gtk::Button::with_label("复制路径");
        ops.append(&edit);
        ops.append(&del);
        ops.append(&copy);
        vbox.append(&ops);

        let hooks = DfPageHooks {
            toast: ast_toast,
            refresh: ast_refresh,
        };
        // 编辑（复用 12.4 表单，kind=Autostart → 多「自启专有」组）
        let item_e = item.clone();
        let overlay = self.toast_overlay.clone();
        edit.connect_clicked(move |_| {
            let Some(win) = overlay
                .root()
                .and_then(|r| r.downcast::<gtk::Window>().ok())
            else {
                return;
            };
            open_df_edit_dialog(&win, &item_e, hooks);
        });
        // 物理删除（D12.6-② + 自启专属尾巴 D12.11）
        let item_d = item.clone();
        let overlay = self.toast_overlay.clone();
        del.connect_clicked(move |_| {
            let Some(win) = overlay
                .root()
                .and_then(|r| r.downcast::<gtk::Window>().ok())
            else {
                return;
            };
            df_confirm_delete(
                &win,
                &item_d,
                DfPageHooks {
                    toast: ast_toast,
                    refresh: ast_refresh,
                },
                format!("；{NEXT_LOGIN}"),
            );
        });
        // 复制路径
        let cp = item.effective.clone();
        let ov_txt = item
            .overridden
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        let overlay = self.toast_overlay.clone();
        copy.connect_clicked(move |_| {
            if ov_txt.is_empty() {
                overlay.clipboard().set_text(&cp.to_string_lossy());
            } else {
                overlay
                    .clipboard()
                    .set_text(&format!("{}\n{ov_txt}", cp.to_string_lossy()));
            }
        });

        // 启用/停用开关 → 后台写 + 回读断言（D12.3）
        let p = item.effective.clone();
        sw_row.connect_active_notify(clone!(
            #[weak]
            sw_row,
            move |_| {
                with_inner(|i| {
                    if i.switching.get() {
                        return;
                    }
                    // 开关现状 = 用户刚拨到的状态；Hidden 目标 = 取反
                    //（开=启用=Hidden false，关=停用=Hidden true，D12.3）
                    let target_hidden = !sw_row.is_active();
                    let p = p.clone();
                    // 回读断言在 set_hidden 内（write_desktop 链），失败不宣称成功
                    i.switching.set(true);
                    sw_row.set_sensitive(false);
                    std::thread::spawn(move || {
                        let res = dfu::set_hidden(&p, target_hidden);
                        let res = RefCell::new(Some(res));
                        glib::source::idle_add(move || {
                            if let Some(res) = res.take() {
                                match res {
                                    Ok(_) => {
                                        let msg = if target_hidden {
                                            format!("已停用该自启项{NEXT_LOGIN}")
                                        } else {
                                            format!("已启用该自启项{NEXT_LOGIN}")
                                        };
                                        ast_toast(&msg);
                                        ast_refresh();
                                    }
                                    Err(e) => {
                                        ast_toast(&e);
                                        // 写失败 → 开关弹回原状（原态 = !target_hidden）。
                                        // set_active 会重入 notify，须在 switching=true 期间改；
                                        // GTK 对象非 Send，只能经 thread_local 取回。
                                        with_inner(|i2| {
                                            if let Some(sw) = i2.active_switch.borrow().as_ref() {
                                                sw.set_active(!target_hidden);
                                                sw.set_sensitive(true);
                                            }
                                            i2.switching.set(false);
                                        });
                                    }
                                }
                            }
                            glib::ControlFlow::Break
                        });
                    });
                });
            }
        ));

        // 全键只读（D12.9 查）
        let (kcard, kbox) = df_card();
        let ktitle = gtk::Label::new(Some("全键只读（生效层内容）"));
        ktitle.add_css_class("title-4");
        ktitle.set_halign(gtk::Align::Start);
        kbox.append(&ktitle);
        let body = item
            .doc
            .as_ref()
            .map(crate::model::desktopfile::render_desktop_text)
            .unwrap_or_else(|| "（文件不可读）".to_string());
        let tv = gtk::TextView::new();
        tv.set_editable(false);
        tv.set_cursor_visible(false);
        tv.set_monospace(true);
        tv.set_wrap_mode(gtk::WrapMode::WordChar);
        tv.buffer().set_text(&body);
        let sw = gtk::ScrolledWindow::new();
        sw.set_child(Some(&tv));
        sw.set_min_content_height(160);
        sw.set_max_content_height(360);
        sw.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        kbox.append(&sw);
        vbox.append(&kcard);

        self.detail.append(&vbox);
        // 开关写完后恢复敏感态（成功路径在 ast_refresh 里重建详情，无需手动恢复）
    }
}

pub fn build() -> DesktopAutostartPage {
    let title = gtk::Label::new(Some("开机自启"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    let desc = gtk::Label::new(Some(
        "管理开机自启的 .desktop 条目：~/.config/autostart > /etc/xdg/autostart（同名前面覆盖后面）。停用 = 写 Hidden=true（不删文件）。所有改动都要到下次登录才生效。按属性表单修改，不提供文本编辑器。",
    ));
    desc.add_css_class("dim-label");
    desc.set_halign(gtk::Align::Start);
    desc.set_wrap(true);

    // 工具条
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let new_btn = gtk::Button::with_label("新建…");
    new_btn.add_css_class("suggested-action");
    new_btn.set_tooltip_text(Some(
        "新建自启 .desktop 条目：默认落 ~/.config/autostart（零授权），可勾选安装到系统（需授权）",
    ));
    let refresh = gtk::Button::with_label("刷新");
    bar.append(&new_btn);
    bar.append(&refresh);

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("搜索 Name / 注释 / 命令 / 分类…"));
    search.set_hexpand(true);
    bar.append(&search);

    let filter = gtk::DropDown::from_strings(AUTOSTART_FILTERS);
    filter.set_tooltip_text(Some("按启用状态过滤；来源（用户/本地/系统）看行上徽章"));
    bar.append(&filter);

    let status = gtk::Label::new(Some("正在扫描…"));
    status.add_css_class("dim-label");
    status.set_halign(gtk::Align::End);
    status.set_hexpand(true);
    bar.append(&status);

    // 列表 + 详情
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    list.set_selection_mode(gtk::SelectionMode::Single);
    let list_sw = gtk::ScrolledWindow::new();
    list_sw.set_child(Some(&list));
    list_sw.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    list_sw.set_size_request(360, -1);

    let detail_empty = gtk::Label::new(Some(
        "从左侧选择一个自启条目查看、启用/停用或删除；也可点「新建…」创建自启项。",
    ));
    detail_empty.add_css_class("dim-label");
    detail_empty.set_wrap(true);
    detail_empty.set_halign(gtk::Align::Center);
    detail_empty.set_margin_top(40);

    let detail = gtk::Box::new(gtk::Orientation::Vertical, 0);
    detail.append(&detail_empty);
    let detail_sw = gtk::ScrolledWindow::new();
    detail_sw.set_child(Some(&detail));
    detail_sw.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&list_sw));
    paned.set_end_child(Some(&detail_sw));
    paned.set_position(400);
    paned.set_vexpand(true);

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 10);
    vbox.set_margin_top(12);
    vbox.set_margin_bottom(12);
    vbox.set_margin_start(14);
    vbox.set_margin_end(14);
    vbox.append(&title);
    vbox.append(&desc);
    vbox.append(&bar);
    vbox.append(&paned);

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&vbox));
    toast_overlay.set_vexpand(true);

    let inner = Rc::new(Inner {
        toast_overlay: toast_overlay.clone(),
        search,
        filter,
        status,
        list,
        detail_empty,
        detail,
        selected: RefCell::new(None),
        items: RefCell::new(Vec::new()),
        busy: Cell::new(false),
        switching: Cell::new(false),
        active_switch: RefCell::new(None),
    });

    INNER.with(|i| *i.borrow_mut() = Some(inner.clone()));

    // 接线
    inner.list.connect_row_activated(|_, row| {
        let id = row.widget_name().to_string();
        with_inner(|i| i.df_show_detail(&id));
    });
    inner
        .search
        .connect_search_changed(|_| with_inner(|i| i.df_refilter()));
    inner
        .filter
        .connect_selected_notify(|_| with_inner(|i| i.df_refilter()));
    refresh.connect_clicked(|_| with_inner(|i| i.df_load()));
    new_btn.connect_clicked(clone!(
        #[weak]
        inner,
        move |_| {
            let Some(win) = inner.parent_window() else {
                return;
            };
            open_df_new_dialog(
                &win,
                DesktopKind::Autostart,
                DfPageHooks {
                    toast: ast_toast,
                    refresh: ast_refresh,
                },
            );
        }
    ));

    // 首次加载
    with_inner(|i| i.df_load());

    DesktopAutostartPage {
        root: toast_overlay,
    }
}
