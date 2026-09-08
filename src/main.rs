use glib::clone;
use gtk::prelude::*;
use adw::prelude::*;

mod model;
mod page;
mod utils;
mod widgets;

const APP_ID: &str = "org.linbox.App";

/// 侧边栏条目与内容页共用的页面名。
const PAGE_HOME: &str = "home";
const PAGE_JSON: &str = "json";
const PAGE_MEDIA: &str = "media";
const PAGE_APIKEY: &str = "apikey";
const PAGE_FCITX: &str = "fcitx";
const PAGE_PATHSCANNER: &str = "pathscanner";
const PAGE_ARCHCRACKER: &str = "archivecracker";
const PAGE_PORTSCANNER: &str = "portscanner";
const PAGE_ENVEDITOR: &str = "enveditor";
const PAGE_SETTINGS: &str = "settings";

fn main() -> glib::ExitCode {
    // adw::Application 会自动初始化 libadwaita；主题默认「跟随系统」
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    // 退出前清空各页全局 TLS 句柄：窗口销毁期的 GTK 回调会反查这些句柄，
    // 拖到线程 TLS 析构阶段再碰会 panic（AccessError），并可能让退出不干净。
    app.connect_shutdown(|_| {
        page::api_key_sniffer::shutdown();
        page::media_converter::shutdown();
        page::fcitx_fix::shutdown();
        page::path_scanner::shutdown();
        page::archive_cracker::shutdown();
        page::port_scanner::shutdown();
        page::env_editor::shutdown();
    });
    app.run()
}

fn build_ui(app: &adw::Application) {
    // ---------- 顶部工具栏 ----------
    let header = adw::HeaderBar::new();
    let title = gtk::Label::new(Some("linbox"));
    title.add_css_class("title");
    header.set_title_widget(Some(&title));

    // 侧边栏切换按钮：显示 libadwaita/GTK4 自带的「显示侧边栏」图标
    let sidebar_toggle = gtk::ToggleButton::new();
    sidebar_toggle.set_icon_name("sidebar-show-symbolic");
    sidebar_toggle.set_active(true);
    sidebar_toggle.set_tooltip_text(Some("切换主菜单"));
    header.pack_start(&sidebar_toggle);

    // 右上角：浅色 / 深色 / 自动 主题模式选择
    let theme_button = gtk::MenuButton::new();
    theme_button.set_icon_name("display-brightness-symbolic");
    theme_button.set_tooltip_text(Some("主题模式"));

    let popover = gtk::Popover::new();
    let theme_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    theme_box.set_margin_top(8);
    theme_box.set_margin_bottom(8);
    theme_box.set_margin_start(12);
    theme_box.set_margin_end(12);

    let style_manager = app.style_manager();
    let radio_light = gtk::CheckButton::builder().label("浅色").build();
    let radio_dark = gtk::CheckButton::builder().label("深色").build();
    let radio_auto = gtk::CheckButton::builder().label("自动").active(true).build();
    radio_dark.set_group(Some(&radio_light));
    radio_auto.set_group(Some(&radio_light));

    theme_box.append(&radio_light);
    theme_box.append(&radio_dark);
    theme_box.append(&radio_auto);
    popover.set_child(Some(&theme_box));
    theme_button.set_popover(Some(&popover));
    header.pack_end(&theme_button);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);

    // ---------- 左侧主菜单（侧边栏） ----------
    // 结构：Box = [固定首页] + [分隔线] + [可滚动功能页] + [分隔线] + [固定设置]
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar.set_margin_top(8);
    sidebar.set_margin_bottom(8);
    sidebar.add_css_class("navigation-sidebar");

    // 顶部：固定首页
    let home_list = gtk::ListBox::new();
    home_list.set_selection_mode(gtk::SelectionMode::Single);
    home_list.set_activate_on_single_click(true);
    home_list.add_css_class("navigation-sidebar");
    sidebar.append(&home_list);

    // 分隔线
    let sep_top = gtk::Separator::new(gtk::Orientation::Horizontal);
    sep_top.set_margin_start(12);
    sep_top.set_margin_end(12);
    sidebar.append(&sep_top);

    // 中部：可滚动功能页
    let nav_list = gtk::ListBox::new();
    nav_list.set_selection_mode(gtk::SelectionMode::Single);
    nav_list.set_activate_on_single_click(true);
    nav_list.add_css_class("navigation-sidebar");

    let nav_scroll = gtk::ScrolledWindow::new();
    nav_scroll.set_child(Some(&nav_list));
    nav_scroll.set_vexpand(true);
    nav_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    sidebar.append(&nav_scroll);

    // 分隔线
    let sep_bottom = gtk::Separator::new(gtk::Orientation::Horizontal);
    sep_bottom.set_margin_start(12);
    sep_bottom.set_margin_end(12);
    sidebar.append(&sep_bottom);

    // 底部：固定设置
    let settings_list = gtk::ListBox::new();
    settings_list.set_selection_mode(gtk::SelectionMode::Single);
    settings_list.set_activate_on_single_click(true);
    settings_list.add_css_class("navigation-sidebar");
    sidebar.append(&settings_list);

    // ---------- 右侧内容区 ----------
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(250);

    // 注册页面 + 侧边栏条目
    let home_page = adw::StatusPage::new();
    home_page.set_title("首页");
    home_page.set_icon_name(Some("user-home-symbolic"));
    home_page.set_description(Some("欢迎使用 linbox"));
    add_nav_item(&home_list, &stack, PAGE_HOME, "user-home-symbolic", "首页", &home_page);

    // JSON 解析页面（真实功能页）
    add_nav_item(
        &nav_list,
        &stack,
        PAGE_JSON,
        "accessories-text-editor-symbolic",
        "JSON 解析",
        page::json_parser::build().widget(),
    );

    // 音视频 / 图片转换页面（真实功能页）
    add_nav_item(
        &nav_list,
        &stack,
        PAGE_MEDIA,
        "video-x-generic-symbolic",
        "音视频 / 图片转换",
        page::media_converter::build().widget(),
    );

    // API Key 嗅探页面（真实功能页）
    add_nav_item(
        &nav_list,
        &stack,
        PAGE_APIKEY,
        "dialog-password-symbolic",
        "API Key 嗅探",
        page::api_key_sniffer::build().widget(),
    );

    // 输入法修复页面（fcitx5 / Wayland）（真实功能页）
    add_nav_item(
        &nav_list,
        &stack,
        PAGE_FCITX,
        "input-keyboard-symbolic",
        "输入法修复",
        page::fcitx_fix::build().widget(),
    );

    // 路径扫描页面（真实功能页）
    add_nav_item(
        &nav_list,
        &stack,
        PAGE_PATHSCANNER,
        "system-search-symbolic",
        "域名/IP 路径扫描",
        page::path_scanner::build().widget(),
    );

    // 压缩包密码爆破页面（真实功能页）
    add_nav_item(
        &nav_list,
        &stack,
        PAGE_ARCHCRACKER,
        "changes-prevent-symbolic",
        "压缩包爆破",
        page::archive_cracker::build().widget(),
    );

    // 端口扫描与服务识别页面（真实功能页）
    add_nav_item(
        &nav_list,
        &stack,
        PAGE_PORTSCANNER,
        "network-transmit-receive-symbolic",
        "端口扫描",
        page::port_scanner::build().widget(),
    );

    // 环境变量编辑器页面（真实功能页）
    add_nav_item(
        &nav_list,
        &stack,
        PAGE_ENVEDITOR,
        "system-run-symbolic",
        "环境变量编辑",
        page::env_editor::build().widget(),
    );

    // 设置项：固定在底部
    let settings_page = adw::StatusPage::new();
    settings_page.set_title("设置");
    settings_page.set_icon_name(Some("preferences-system-symbolic"));
    settings_page.set_description(Some("此页面尚未实现"));
    add_nav_item(
        &settings_list,
        &stack,
        PAGE_SETTINGS,
        "preferences-system-symbolic",
        "设置",
        &settings_page,
    );

    // 三个 ListBox 互斥选中：选中一行时取消另外两个 ListBox 的选中
    let home_list_c = home_list.clone();
    let nav_list_c = nav_list.clone();
    let settings_list_c = settings_list.clone();
    home_list.connect_row_selected(move |_, row| {
        if row.is_some() {
            nav_list_c.unselect_all();
            settings_list_c.unselect_all();
        }
    });
    let home_list_c2 = home_list.clone();
    let nav_list_c2 = nav_list.clone();
    let settings_list_c2 = settings_list.clone();
    nav_list.connect_row_selected(move |_, row| {
        if row.is_some() {
            home_list_c2.unselect_all();
            settings_list_c2.unselect_all();
        }
    });
    let home_list_c3 = home_list.clone();
    let nav_list_c3 = nav_list.clone();
    settings_list.connect_row_selected(move |_, row| {
        if row.is_some() {
            home_list_c3.unselect_all();
            nav_list_c3.unselect_all();
        }
    });

    // 条目被激活 → 切换到对应内容页
    let stack_clone = stack.clone();
    let handler = move |_: &gtk::ListBox, row: &gtk::ListBoxRow| {
        let name = row.widget_name();
        if !name.is_empty() {
            stack_clone.set_visible_child_name(&name);
        }
    };
    home_list.connect_row_activated(handler.clone());
    nav_list.connect_row_activated(handler.clone());
    settings_list.connect_row_activated(handler);

    // 默认选中首页
    if let Some(first) = home_list.row_at_index(0) {
        home_list.select_row(Some(&first));
        stack.set_visible_child_name(PAGE_HOME);
    }

    // ---------- 左右组合成导航视图 ----------
    // 用 OverlaySplitView：抽屉式侧边栏，set_show_sidebar 切换自带滑入/滑出弹簧动画，
    // 且接受普通 Widget（无需 NavigationPage 包裹），支持边缘滑动手势。
    let split = adw::OverlaySplitView::new();
    split.set_sidebar(Some(&sidebar));
    split.set_content(Some(&stack));
    split.set_show_sidebar(true);
    split.set_min_sidebar_width(200.0);
    split.set_max_sidebar_width(320.0);
    split.set_sidebar_width_fraction(0.25); // 约 260px @1080 宽
    split.set_enable_show_gesture(true); // 从屏幕边缘滑动可呼出侧边栏
    split.set_enable_hide_gesture(true); // 从边缘滑动可收起侧边栏

    // 汉堡按钮联动侧边栏显隐（展开/收起动画由 libadwaita 内置弹簧动画完成）
    sidebar_toggle.connect_toggled(clone!(#[weak] split, move |btn| {
        split.set_show_sidebar(btn.is_active());
    }));

    toolbar.set_content(Some(&split));

    // ---------- 窗口 ----------
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(1080)
        .default_height(720)
        .title("linbox")
        .content(&toolbar)
        .build();
    window.present();

    // 主题切换：用 TimedAnimation 做淡出→切换→淡入，保证有可见过渡
    radio_light.connect_toggled({
        let sm = style_manager.clone();
        let win = window.downgrade();
        move |btn| {
            if btn.is_active() {
                match win.upgrade() {
                    Some(w) => apply_scheme_animated(&w, &sm, adw::ColorScheme::ForceLight),
                    None => sm.set_color_scheme(adw::ColorScheme::ForceLight),
                }
            }
        }
    });
    radio_dark.connect_toggled({
        let sm = style_manager.clone();
        let win = window.downgrade();
        move |btn| {
            if btn.is_active() {
                match win.upgrade() {
                    Some(w) => apply_scheme_animated(&w, &sm, adw::ColorScheme::ForceDark),
                    None => sm.set_color_scheme(adw::ColorScheme::ForceDark),
                }
            }
        }
    });
    radio_auto.connect_toggled({
        let sm = style_manager.clone();
        let win = window.downgrade();
        move |btn| {
            if btn.is_active() {
                match win.upgrade() {
                    Some(w) => apply_scheme_animated(&w, &sm, adw::ColorScheme::Default),
                    None => sm.set_color_scheme(adw::ColorScheme::Default),
                }
            }
        }
    });
}

/// 切换主题时播放淡出→切换→淡入过渡动画，保证有可见的过渡效果。
fn apply_scheme_animated(window: &adw::ApplicationWindow, sm: &adw::StyleManager, scheme: adw::ColorScheme) {
    let win_weak = window.downgrade();
    let target = adw::CallbackAnimationTarget::new(move |v| {
        if let Some(w) = win_weak.upgrade() {
            w.set_opacity(v);
        }
    });

    let anim_out = adw::TimedAnimation::builder()
        .widget(window)
        .value_from(1.0)
        .value_to(0.0)
        .duration(150)
        .target(&target)
        .build();
    // 即使系统关闭了「动画」设置，也强制播放过渡
    anim_out.set_follow_enable_animations_setting(false);

    let win_weak_in = window.downgrade();
    let target_in = target.clone();
    let sm = sm.clone();
    anim_out.connect_done(clone!(#[weak] sm, move |_| {
        sm.set_color_scheme(scheme);
        if let Some(w) = win_weak_in.upgrade() {
            let anim_in = adw::TimedAnimation::builder()
                .widget(&w)
                .value_from(0.0)
                .value_to(1.0)
                .duration(150)
                .target(&target_in)
                .build();
            anim_in.set_follow_enable_animations_setting(false);
            anim_in.play();
        }
    }));

    anim_out.play();
}

/// 添加一个侧边栏菜单条目，并同步注册已构建好的内容页。
fn add_nav_item(
    sidebar: &gtk::ListBox,
    stack: &gtk::Stack,
    page: &str,
    icon: &str,
    label: &str,
    content: &impl IsA<gtk::Widget>,
) {
    // 内容页
    stack.add_named(content, Some(page));

    // 侧边栏条目：图标 + 文字
    let row = gtk::ListBoxRow::new();
    let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    box_.set_margin_start(12);
    box_.set_margin_end(12);
    box_.set_margin_top(6);
    box_.set_margin_bottom(6);
    let image = gtk::Image::from_icon_name(icon);
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    box_.append(&image);
    box_.append(&text);
    row.set_child(Some(&box_));
    // 把页面名记在行上，供 `row-activated` 回调取出（分隔行没有名字，会被跳过）
    row.set_widget_name(page);
    sidebar.append(&row);
}
