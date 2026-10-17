//! 设置页（Phase 13.3，展示层 · 仅 UI）。
//!
//! **两级结构（2026-10-17 用户修订）**：一级页只列设置项入口（v1 唯一一项
//! 「导航顺序」），**点击该条目才进入二级页**调整顺序——不是一进设置就直接
//! 展示排序列表。
//!
//! 二级页内容 = 每行一个功能模块（图标 + 中文名 + 页 id 灰字 + 上移/下移）+
//! 组尾「恢复默认排序」+ 状态 + 配置路径，顶部有「← 返回设置」回一级。
//! 改动**即时生效并落盘**，不设「应用」按钮、不弹确认框；**不做拖拽**
//! （GTK4 ListBox 无内置 DnD 排序，成本高收益低）。
//!
//! 排序只作用于中部功能区（首页固定顶部、设置固定底部，D13.3）。
//! 排序变化通过 `build()` 注入的 `on_order_changed` 回调交给 `lib.rs`
//! 重排侧边栏并刷新首页。
//!
//! 高度口径同首页：两个层级各自裹 `ScrolledWindow` 且
//! `propagate_natural_height(false)`（GOAL 8.4：Stack 取所有子页自然高度
//! 最大值，不裹滚动窗会把窗口顶爆）。

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use glib::clone;

use crate::model::nav_catalog::{self, NavEntry};
use crate::utils::nav_order::{self, NavConfig};

/// 二级页（导航顺序）在 `views` Stack 里的名字。
const VIEW_ORDER: &str = "order";
/// 一级页（设置项入口）。
const VIEW_MAIN: &str = "main";

/// 设置页句柄。`Clone` 为了让 `lib.rs` 把它挂进与窗口同寿的闭包里。
#[derive(Clone)]
pub struct SettingsPage {
    inner: Rc<Inner>,
}

impl SettingsPage {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.inner.root
    }

    /// 当前排序（只读，供核对）。
    pub fn order(&self) -> Vec<String> {
        self.inner.order.borrow().clone()
    }
}

/// 构建设置页。`on_order_changed(new_order)` 在排序变化（含恢复默认）后调用，
/// 参数是**已归一化**的新顺序。
pub fn build(on_order_changed: impl Fn(Vec<String>) + 'static) -> SettingsPage {
    let changed: Rc<dyn Fn(Vec<String>)> = Rc::new(on_order_changed);

    // ---------- 根容器 ----------
    let root = adw::ToastOverlay::new();
    root.set_vexpand(true);

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
    vbox.set_margin_top(12);
    vbox.set_margin_bottom(12);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);
    root.set_child(Some(&vbox));

    // ---------- 标题（两级共用） ----------
    let title = gtk::Label::new(Some("设置"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    vbox.append(&title);

    // ---------- 两级视图栈 ----------
    let views = gtk::Stack::new();
    views.set_transition_type(gtk::StackTransitionType::SlideLeftRight);
    views.set_transition_duration(180);
    views.set_hhomogeneous(false);
    views.set_vexpand(true);
    vbox.append(&views);

    // ================= 一级：设置项入口 =================
    let main_scroller = gtk::ScrolledWindow::new();
    main_scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    main_scroller.set_propagate_natural_height(false);
    main_scroller.set_propagate_natural_width(true);
    main_scroller.set_vexpand(true);

    let main_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    main_scroller.set_child(Some(&main_box));

    let main_hint = gtk::Label::new(Some("点击设置项进入对应页面进行调整。"));
    main_hint.add_css_class("dim-label");
    main_hint.set_halign(gtk::Align::Start);
    main_hint.set_wrap(true);
    main_hint.set_xalign(0.0);
    main_box.append(&main_hint);

    // 入口列表：每行一个设置项（图标 + 名称 + 说明 + 右箭头），点击进二级页
    let entry_list = gtk::ListBox::new();
    entry_list.set_selection_mode(gtk::SelectionMode::None);
    entry_list.set_activate_on_single_click(true);
    entry_list.add_css_class("navigation-sidebar");

    let entry_card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    entry_card.add_css_class("card");
    entry_card.append(&entry_list);
    main_box.append(&entry_card);
    views.add_named(&main_scroller, Some(VIEW_MAIN));

    let entry_row = gtk::ListBoxRow::new();
    entry_row.set_widget_name(VIEW_ORDER); // 供 row_activated 取设置项名
    {
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        hbox.set_margin_top(10);
        hbox.set_margin_bottom(10);
        hbox.set_margin_start(12);
        hbox.set_margin_end(12);

        let image = gtk::Image::from_icon_name("view-grid-symbolic");
        image.set_valign(gtk::Align::Center);
        hbox.append(&image);

        let texts = gtk::Box::new(gtk::Orientation::Vertical, 2);
        texts.set_hexpand(true);
        let name = gtk::Label::new(Some("导航顺序"));
        name.set_halign(gtk::Align::Start);
        texts.append(&name);
        let desc = gtk::Label::new(Some("自定义左侧功能菜单的显示顺序（同时作用于首页排列）"));
        desc.add_css_class("dim-label");
        desc.set_halign(gtk::Align::Start);
        desc.set_ellipsize(gtk::pango::EllipsizeMode::End);
        texts.append(&desc);
        hbox.append(&texts);

        let arrow = gtk::Image::from_icon_name("go-next-symbolic");
        arrow.set_valign(gtk::Align::Center);
        hbox.append(&arrow);

        entry_row.set_child(Some(&hbox));
    }
    entry_list.append(&entry_row);

    // ================= 二级：导航顺序 =================
    let order_box = gtk::Box::new(gtk::Orientation::Vertical, 8);

    // 顶部返回条：回一级页
    let back_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let back = gtk::Button::with_label("← 返回设置");
    back.set_tooltip_text(Some("回到设置项列表"));
    back_bar.append(&back);
    let order_heading = gtk::Label::new(Some("导航顺序"));
    order_heading.add_css_class("title-4");
    order_heading.set_halign(gtk::Align::Start);
    back_bar.append(&order_heading);
    order_box.append(&back_bar);

    let order_scroller = gtk::ScrolledWindow::new();
    order_scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    order_scroller.set_propagate_natural_height(false);
    order_scroller.set_propagate_natural_width(true);
    order_scroller.set_vexpand(true);

    let order_inner = gtk::Box::new(gtk::Orientation::Vertical, 8);
    order_scroller.set_child(Some(&order_inner));
    order_box.append(&order_scroller);
    views.add_named(&order_box, Some(VIEW_ORDER));

    // 二级页内容卡片
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("card");

    let card_inner = gtk::Box::new(gtk::Orientation::Vertical, 8);
    card_inner.set_margin_top(16);
    card_inner.set_margin_bottom(16);
    card_inner.set_margin_start(16);
    card_inner.set_margin_end(16);
    card.append(&card_inner);

    let hint = gtk::Label::new(Some(
        "调整左侧功能菜单的显示顺序，改动立即生效并保存（同时作用于首页的功能排列）。\
         首页与设置固定在菜单两端，不参与排序。",
    ));
    hint.add_css_class("dim-label");
    hint.set_halign(gtk::Align::Start);
    hint.set_wrap(true);
    hint.set_xalign(0.0);
    card_inner.append(&hint);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("navigation-sidebar");
    card_inner.append(&list);

    // 组尾操作：恢复默认排序 + 状态
    let foot = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let reset = gtk::Button::with_label("恢复默认排序");
    reset.set_tooltip_text(Some("回到安装时的功能菜单顺序"));
    foot.append(&reset);
    let status = gtk::Label::new(Some(""));
    status.add_css_class("dim-label");
    status.set_halign(gtk::Align::End);
    status.set_hexpand(true);
    status.set_selectable(true);
    foot.append(&status);
    card_inner.append(&foot);

    let cfg_path_label = gtk::Label::new(Some(&format!(
        "排序保存在：{}",
        nav_order::config_path().display()
    )));
    cfg_path_label.add_css_class("dim-label");
    cfg_path_label.set_halign(gtk::Align::Start);
    cfg_path_label.set_wrap(true);
    cfg_path_label.set_xalign(0.0);
    cfg_path_label.set_selectable(true);
    card_inner.append(&cfg_path_label);

    order_inner.append(&card);

    let inner = Rc::new(Inner {
        root,
        views,
        list,
        status,
        order: RefCell::new(Vec::new()),
        changed,
    });

    // 一级页：点设置项 → 进二级页
    entry_list.connect_row_activated(clone!(
        #[strong]
        inner,
        move |_, row| {
            if row.widget_name() == VIEW_ORDER {
                inner.views.set_visible_child_name(VIEW_ORDER);
            }
        }
    ));

    // 二级页：返回 → 回一级页
    back.connect_clicked(clone!(
        #[strong]
        inner,
        move |_| inner.views.set_visible_child_name(VIEW_MAIN)
    ));

    // 恢复默认
    reset.connect_clicked(clone!(
        #[strong]
        inner,
        move |_| Inner::apply_new_order(&inner, nav_catalog::default_order(), "已恢复默认排序")
    ));

    // 初始顺序：读持久化配置（坏文件 → 默认序，D13.6）
    let cfg = nav_order::load();
    *inner.order.borrow_mut() = cfg.order;
    Inner::render(&inner);

    SettingsPage { inner }
}

struct Inner {
    root: adw::ToastOverlay,
    views: gtk::Stack,
    list: gtk::ListBox,
    status: gtk::Label,
    order: RefCell<Vec<String>>,
    changed: Rc<dyn Fn(Vec<String>)>,
}

impl Inner {
    /// 应用一份新顺序：更新内存 → 落盘 → 重画列表 → 通知 lib.rs。
    fn apply_new_order(this: &Rc<Inner>, order: Vec<String>, action: &str) {
        let default = nav_catalog::default_order();
        let norm = nav_order::normalize_order(&order, &default);
        *this.order.borrow_mut() = norm.clone();

        // 视图模式归首页管，这里读回来保留，别把用户的卡片/列表选择冲掉。
        let cfg = NavConfig {
            order: norm.clone(),
            home_view: nav_order::load().home_view,
        };
        match nav_order::save(&cfg) {
            Ok(()) => {
                this.status.set_text(&format!("{action} · 已保存"));
                Inner::render(this);
                (this.changed)(norm);
            }
            Err(e) => {
                // 保存失败：列表保持原样、明确报错，不假装成功（D13.6）
                this.status.set_text(&format!("保存失败：{e}"));
                this.root
                    .add_toast(adw::Toast::new(&format!("排序保存失败：{e}")));
            }
        }
    }

    /// 重建 19 行（图标 + 名称 + 页 id 灰字 + 上移/下移）。
    fn render(this: &Rc<Inner>) {
        while let Some(c) = this.list.first_child() {
            this.list.remove(&c);
        }

        let order = this.order.borrow().clone();
        for (idx, page) in order.iter().enumerate() {
            let Some(entry) = NavEntry::by_page(page) else {
                continue; // 已归一化，正常不会发生
            };
            let (row, up, down) = build_row(entry, idx, order.len());
            // 行的 widget_name = page id：move_row 靠它读取界面上的真实行序
            row.set_widget_name(entry.page);

            let inner_up = Rc::clone(this);
            let list_up = this.list.clone();
            up.connect_clicked(move |_| move_row(&inner_up, &list_up, idx, -1));
            let inner_down = Rc::clone(this);
            let list_down = this.list.clone();
            down.connect_clicked(move |_| move_row(&inner_down, &list_down, idx, 1));

            this.list.append(&row);
        }
    }
}

/// 一行：[图标] [名称 + 页 id] ......... [上移] [下移]
fn build_row(
    entry: &'static NavEntry,
    idx: usize,
    total: usize,
) -> (gtk::ListBoxRow, gtk::Button, gtk::Button) {
    let row = gtk::ListBoxRow::new();

    let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    hbox.set_margin_top(6);
    hbox.set_margin_bottom(6);
    hbox.set_margin_start(12);
    hbox.set_margin_end(12);

    let image = gtk::Image::from_icon_name(entry.icon);
    image.set_valign(gtk::Align::Center);
    hbox.append(&image);

    let texts = gtk::Box::new(gtk::Orientation::Vertical, 2);
    texts.set_hexpand(true);
    let name = gtk::Label::new(Some(entry.label));
    name.set_halign(gtk::Align::Start);
    texts.append(&name);
    let id = gtk::Label::new(Some(entry.page));
    id.add_css_class("dim-label");
    id.set_halign(gtk::Align::Start);
    texts.append(&id);
    hbox.append(&texts);

    let up = gtk::Button::from_icon_name("go-up-symbolic");
    up.set_tooltip_text(Some("上移"));
    up.set_sensitive(idx > 0);
    up.set_valign(gtk::Align::Center);
    hbox.append(&up);

    let down = gtk::Button::from_icon_name("go-down-symbolic");
    down.set_tooltip_text(Some("下移"));
    down.set_sensitive(idx + 1 < total);
    down.set_valign(gtk::Align::Center);
    hbox.append(&down);

    row.set_child(Some(&hbox));
    (row, up, down)
}

/// 把第 `idx` 行向上/向下挪一格（-1 = 上移，+1 = 下移）。
fn move_row(inner: &Rc<Inner>, list: &gtk::ListBox, idx: usize, delta: isize) {
    // 取「当前 ListBox 里真实显示的行序」（行的 widget_name = page id），
    // 防止闭包里的索引与界面不同步（重排/恢复默认后索引会变）。
    let mut order: Vec<String> = Vec::new();
    let mut child = list.first_child();
    while let Some(c) = child {
        let name = c.widget_name();
        if !name.is_empty() {
            order.push(name.to_string());
        }
        child = c.next_sibling();
    }
    if order.is_empty() {
        order = inner.order.borrow().clone();
    }

    if idx >= order.len() {
        return;
    }
    let to = match delta {
        -1 if idx > 0 => idx - 1,
        1 if idx + 1 < order.len() => idx + 1,
        _ => return,
    };
    order.swap(idx, to);
    Inner::apply_new_order(inner, order, "顺序已更新");
}
