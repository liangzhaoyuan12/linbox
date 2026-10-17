//! 首页：功能总览（Phase 13.4，展示层 · 仅 UI）。
//!
//! 显示 navigation list 中**全部功能模块**（不含「首页」与「设置」自身，
//! D13.4），顺序 = 用户在设置页定的顺序（D13.3），支持**卡片 / 列表**两种
//! 显示模式切换（D13.5）——开关在本页工具条，不进设置页。
//!
//! - 条目数据来自 `model::nav_catalog`（单一真源，D13.1）；
//! - 顺序与视图模式持久化走 `utils::nav_order`（`~/.config/linbox/nav.json`）；
//! - 点击条目 → 由 `build()` 注入的 `on_navigate(page_id)` 回调切页，
//!   侧边栏高亮同步由 `lib.rs` 的 `stack::visible-child-notify` 负责（摸底 9）。
//!
//! 高度口径：本页卡片多，必须裹 `ScrolledWindow` 且
//! `propagate_natural_height(false)`——GTK4 Stack 取所有子页自然高度的最大值，
//! 否则整个窗口会被顶得比屏幕还高（GOAL 8.4 用户报障的同款教训）。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use glib::clone;

use crate::model::nav_catalog::NavEntry;
use crate::utils::nav_order::{self, HomeView, NavConfig};

/// 首页句柄。`Clone` 是为了让 `lib.rs` 把它挂进与窗口同寿的闭包里
/// （`build_ui` 结束后页面仍存活，设置页改序时才能刷新首页）。
#[derive(Clone)]
pub struct HomePage {
    inner: Rc<Inner>,
}

impl HomePage {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.inner.root
    }

    /// 由 `lib.rs` 在排序变化时调用（首页当时被隐藏也要更新，避免下次进入是旧序）。
    pub fn set_order(&self, order: &[String]) {
        self.inner.set_order(order);
    }

    /// 当前视图模式（设置页落盘时会用到，虽然 v1 只有首页自己写它）。
    pub fn view(&self) -> HomeView {
        self.inner.view.get()
    }
}

/// 构建首页。`on_navigate(page_id)` 负责切换到对应功能页。
pub fn build(on_navigate: impl Fn(&str) + 'static) -> HomePage {
    let nav: Rc<dyn Fn(&str)> = Rc::new(on_navigate);

    // ---------- 根容器 ----------
    let root = adw::ToastOverlay::new();
    root.set_vexpand(true);

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_propagate_natural_height(false); // 不向窗口贡献高度（8.4 教训）
    scroller.set_propagate_natural_width(true);
    scroller.set_vexpand(true);
    root.set_child(Some(&scroller));

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
    vbox.set_margin_top(12);
    vbox.set_margin_bottom(12);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);
    scroller.set_child(Some(&vbox));

    // ---------- 标题 ----------
    let title = gtk::Label::new(Some("功能总览"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    vbox.append(&title);

    // ---------- 工具条：模块计数 + 卡片/列表切换 ----------
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let count_label = gtk::Label::new(Some(""));
    count_label.add_css_class("dim-label");
    count_label.set_halign(gtk::Align::Start);
    count_label.set_hexpand(true);
    bar.append(&count_label);

    // 两个成组 ToggleButton（`lib.rs:124-131` 主题浅/深/自动同款先例；
    // adw::ToggleGroup 的成组方法要 v1_7，本项目只开 v1_4 —— 见 GOAL 摸底 8）
    let btn_card = gtk::ToggleButton::with_label("卡片");
    btn_card.set_tooltip_text(Some("以卡片网格显示全部功能模块"));
    let btn_list = gtk::ToggleButton::with_label("列表");
    btn_list.set_tooltip_text(Some("以列表显示全部功能模块"));
    btn_list.set_group(Some(&btn_card));
    bar.append(&btn_card);
    bar.append(&btn_list);
    vbox.append(&bar);

    // ---------- 两种视图容器 ----------
    let flow = gtk::FlowBox::new();
    flow.set_selection_mode(gtk::SelectionMode::None); // 点击交给卡片按钮自己处理
    flow.set_max_children_per_line(4);
    flow.set_min_children_per_line(1);
    flow.set_column_spacing(12);
    flow.set_row_spacing(12);
    flow.set_valign(gtk::Align::Start);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None); // 行点击即跳转，不需要选中态
    list.add_css_class("navigation-sidebar");
    list.set_valign(gtk::Align::Start);

    let views = gtk::Stack::new();
    views.set_transition_type(gtk::StackTransitionType::Crossfade);
    views.set_transition_duration(150);
    views.set_hhomogeneous(false);
    views.add_named(&flow, Some("card"));
    views.add_named(&list, Some("list"));
    vbox.append(&views);

    let inner = Rc::new(Inner {
        root,
        flow,
        list,
        views,
        btn_card,
        btn_list,
        count_label,
        order: RefCell::new(Vec::new()),
        view: Cell::new(HomeView::Card),
        suppress: Cell::new(false),
        nav,
    });

    // 列表模式：行激活 → 跳转（行的 widget_name 就是 page id）
    inner.list.connect_row_activated(clone!(
        #[strong]
        inner,
        move |_, row| {
            let id = row.widget_name();
            if !id.is_empty() {
                (inner.nav)(id.as_str());
            }
        }
    ));

    // 视图切换（suppress 防止程序化 set_active 触发回环）
    inner.btn_card.connect_toggled(clone!(
        #[strong]
        inner,
        move |btn| {
            if inner.suppress.get() || btn.is_active() {
                inner.set_view(HomeView::Card);
            }
        }
    ));
    inner.btn_list.connect_toggled(clone!(
        #[strong]
        inner,
        move |btn| {
            if inner.suppress.get() || btn.is_active() {
                inner.set_view(HomeView::List);
            }
        }
    ));

    // 初始状态：读持久化配置（坏文件 → 默认序 + 卡片视图，D13.6）
    let cfg = nav_order::load();
    inner.set_order(&cfg.order);
    inner.set_view(cfg.home_view);

    HomePage { inner }
}

struct Inner {
    root: adw::ToastOverlay,
    flow: gtk::FlowBox,
    list: gtk::ListBox,
    views: gtk::Stack,
    btn_card: gtk::ToggleButton,
    btn_list: gtk::ToggleButton,
    count_label: gtk::Label,
    order: RefCell<Vec<String>>,
    view: Cell<HomeView>,
    /// 程序化改 ToggleButton 时置位，避免 toggled 处理器回环。
    suppress: Cell<bool>,
    nav: Rc<dyn Fn(&str)>,
}

impl Inner {
    fn toast(&self, msg: &str) {
        self.root.add_toast(adw::Toast::new(msg));
    }

    fn set_order(&self, order: &[String]) {
        *self.order.borrow_mut() = order.to_vec();
        self.render();
    }

    fn set_view(&self, view: HomeView) {
        let changed = self.view.get() != view;
        self.view.set(view);

        // 同步两个按钮的按下态（suppress 防回环）
        self.suppress.set(true);
        match view {
            HomeView::Card => self.btn_card.set_active(true),
            HomeView::List => self.btn_list.set_active(true),
        }
        self.suppress.set(false);

        match view {
            HomeView::Card => self.views.set_visible_child_name("card"),
            HomeView::List => self.views.set_visible_child_name("list"),
        }

        if changed {
            self.persist();
        }
    }

    /// 把当前顺序 + 视图模式落盘（D13.6：写失败只报，不 panic、不假装成功）。
    fn persist(&self) {
        let cfg = NavConfig {
            order: self.order.borrow().clone(),
            home_view: self.view.get(),
        };
        if let Err(e) = nav_order::save(&cfg) {
            self.toast(&format!("保存首页设置失败：{e}"));
        }
    }

    /// 按当前顺序重建卡片与列表（两模式条目集合与顺序完全一致，D13.5）。
    fn render(&self) {
        // ---- 清空 ----
        while let Some(c) = self.flow.first_child() {
            self.flow.remove(&c);
        }
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }

        let order = self.order.borrow().clone();
        for page in &order {
            let Some(entry) = NavEntry::by_page(page) else {
                // 正常情况不会发生：顺序在 utils::nav_order 里已归一化，
                // 只含目录中存在的 id（D13.3 / D13.6）。
                continue;
            };
            let card = build_card(entry, &self.nav);
            self.flow.insert(&card, -1);

            let row = build_row(entry);
            row.set_widget_name(entry.page);
            self.list.append(&row);
        }

        self.count_label
            .set_text(&format!("共 {} 个功能模块 · 点击进入", order.len()));
    }
}

/// 卡片模式的一张卡：图标 + 名称 + 一句话描述，整卡可点。
fn build_card(entry: &'static NavEntry, nav: &Rc<dyn Fn(&str)>) -> gtk::Button {
    let btn = gtk::Button::new();
    btn.add_css_class("card");
    btn.set_hexpand(true);
    btn.set_vexpand(true);
    btn.set_tooltip_text(Some(entry.desc));

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
    vbox.set_margin_top(16);
    vbox.set_margin_bottom(16);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);

    let image = gtk::Image::from_icon_name(entry.icon);
    image.set_icon_size(gtk::IconSize::Large);
    image.set_halign(gtk::Align::Center);
    vbox.append(&image);

    let name = gtk::Label::new(Some(entry.label));
    name.add_css_class("heading");
    name.set_halign(gtk::Align::Center);
    name.set_wrap(true);
    vbox.append(&name);

    let desc = gtk::Label::new(Some(entry.desc));
    desc.add_css_class("dim-label");
    desc.set_halign(gtk::Align::Center);
    desc.set_justify(gtk::Justification::Center);
    desc.set_wrap(true);
    desc.set_max_width_chars(20);
    vbox.append(&desc);

    btn.set_child(Some(&vbox));

    let page = entry.page.to_string();
    btn.connect_clicked(clone!(
        #[strong]
        nav,
        move |_| nav(&page)
    ));
    btn
}

/// 列表模式的一行：图标 + 名称 + 描述 + 右侧箭头。
fn build_row(entry: &'static NavEntry) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();

    let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    hbox.set_margin_top(8);
    hbox.set_margin_bottom(8);
    hbox.set_margin_start(12);
    hbox.set_margin_end(12);

    let image = gtk::Image::from_icon_name(entry.icon);
    hbox.append(&image);

    let texts = gtk::Box::new(gtk::Orientation::Vertical, 2);
    texts.set_hexpand(true);
    let name = gtk::Label::new(Some(entry.label));
    name.set_halign(gtk::Align::Start);
    texts.append(&name);
    let desc = gtk::Label::new(Some(entry.desc));
    desc.add_css_class("dim-label");
    desc.set_halign(gtk::Align::Start);
    desc.set_ellipsize(gtk::pango::EllipsizeMode::End);
    texts.append(&desc);
    hbox.append(&texts);

    let arrow = gtk::Image::from_icon_name("go-next-symbolic");
    arrow.set_valign(gtk::Align::Center);
    hbox.append(&arrow);

    row.set_child(Some(&hbox));
    row
}
