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
    let sidebar = gtk::ListBox::new();
    sidebar.set_selection_mode(gtk::SelectionMode::Single);
    sidebar.set_activate_on_single_click(true); // 单击即触发导航
    sidebar.set_margin_top(8);
    sidebar.set_margin_bottom(8);
    sidebar.add_css_class("navigation-sidebar");

    // ---------- 右侧内容区 ----------
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade); // 规范 §6.2：250ms 淡入淡出
    stack.set_transition_duration(250);

    // 注册页面 + 侧边栏条目
    let home_page = adw::StatusPage::new();
    home_page.set_title("首页");
    home_page.set_icon_name(Some("user-home-symbolic"));
    home_page.set_description(Some("欢迎使用 linbox"));
    add_nav_item(&sidebar, &stack, PAGE_HOME, "user-home-symbolic", "首页", &home_page);

    // 分组分隔线：在「首页」之后画一条细线。
    //
    // 之前是在 ListBox 里追加一个装着 Separator 的 ListBoxRow，但 navigation-sidebar
    // 样式的行自带最小高度与背景，那条 Separator 只占 1px，剩下的行高就露出一块
    // 灰色占位。改用 ListBox 的 header 机制：header 不是行，没有背景、悬停和选中态，
    // 只画一条真正的分隔线。
    sidebar.set_header_func(|row, before| {
        let needs_separator = before.map(|b| b.widget_name() == PAGE_HOME).unwrap_or(false);
        if !needs_separator {
            row.set_header(None::<&gtk::Widget>);
            return;
        }
        // header_func 会被反复调用，已设置过就不要再建新的
        if row.header().is_none() {
            let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
            separator.set_margin_top(6);
            separator.set_margin_bottom(6);
            separator.set_margin_start(12);
            separator.set_margin_end(12);
            row.set_header(Some(&separator));
        }
    });

    // JSON 解析页面（真实功能页）
    add_nav_item(
        &sidebar,
        &stack,
        PAGE_JSON,
        "code-symbolic",
        "JSON 解析",
        page::json_parser::build().widget(),
    );

    // 音视频 / 图片转换页面（真实功能页）
    add_nav_item(
        &sidebar,
        &stack,
        PAGE_MEDIA,
        "video-x-generic-symbolic",
        "音视频 / 图片转换",
        page::media_converter::build().widget(),
    );

    // API Key 嗅探页面（真实功能页）
    add_nav_item(
        &sidebar,
        &stack,
        PAGE_APIKEY,
        "dialog-password-symbolic",
        "API Key 嗅探",
        page::api_key_sniffer::build().widget(),
    );

    // 输入法修复页面（fcitx5 / Wayland）（真实功能页）
    add_nav_item(
        &sidebar,
        &stack,
        PAGE_FCITX,
        "input-keyboard-symbolic",
        "输入法修复",
        page::fcitx_fix::build().widget(),
    );

    let settings_page = adw::StatusPage::new();
    settings_page.set_title("设置");
    settings_page.set_icon_name(Some("preferences-system-symbolic"));
    settings_page.set_description(Some("此页面尚未实现"));
    add_nav_item(&sidebar, &stack, PAGE_SETTINGS, "preferences-system-symbolic", "设置", &settings_page);

    // 条目被激活（鼠标单击 或 键盘 Enter/Space）→ 切换到对应内容页。
    //
    // 注意：这里必须连接 GtkListBox 的 `row-activated`，而不是 GtkListBoxRow 的
    // `activate`。后者是键盘绑定信号（keybinding signal），只有按 Enter/Space 时
    // 才会发射；鼠标单击只会触发 `row-activated`，因此原来点击菜单没有任何反应。
    sidebar.connect_row_activated(clone!(#[weak] stack, move |_, row| {
        let name = row.widget_name();
        if !name.is_empty() {
            stack.set_visible_child_name(&name);
        }
    }));

    // 默认选中首页
    if let Some(first) = sidebar.row_at_index(0) {
        sidebar.select_row(Some(&first));
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
