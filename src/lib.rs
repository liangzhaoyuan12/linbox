// GTK4.10 起 GtkTreeView/TreeStore/FileChooserDialog/MessageDialog 等被标记
// deprecated（继任者 ColumnView/FileDialog 是 API 级重写）。本项目大量使用旧
// 树表控件，迁移属于独立 UI 重构任务，不在本性能 GOAL 范围内，此处整体豁免
// 并如实记录（GOAL.md 4.4 进度），不是逐处掩盖。
#![allow(deprecated)]

use adw::prelude::*;
use glib::clone;

pub mod model;
pub mod page;
pub mod utils;
pub mod widgets;

/// 单测专用：XDG 环境变量互斥（见该模块文档）。
#[cfg(test)]
mod test_env;

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
const PAGE_DOWNLOAD: &str = "download";
const PAGE_NOTEPAD: &str = "notepad";
const PAGE_SYSTEMD: &str = "systemd";
const PAGE_DESKTOP_MENU: &str = "desktopmenu";
const PAGE_DESKTOP_AUTOSTART: &str = "desktopautostart";
const PAGE_INOTIFY: &str = "inotify";
const PAGE_FSTAB: &str = "fstab";
const PAGE_CPUFREQ: &str = "cpufreq";
const PAGE_CLEANER: &str = "cleaner";
const PAGE_SMART: &str = "smart";
const PAGE_MONITOR: &str = "monitor";
const PAGE_SETTINGS: &str = "settings";

/// 嵌入自定义图标 GResource
static ICONS_GRESOURCE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/icons.gresource"));

/// 应用入口：由 `src/main.rs` 的薄包装调用。
///
/// 拆出 lib 目标是为了让 `benches/` 能链接纯逻辑层（bench 不能依赖 bin 目标）。
pub fn run() -> glib::ExitCode {
    // 注册自定义图标资源
    let resource = gtk::gio::Resource::from_data(&glib::Bytes::from(ICONS_GRESOURCE))
        .expect("加载自定义图标资源失败");
    gtk::gio::resources_register(&resource);

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
        page::download::shutdown();
        page::notepad::shutdown();
        page::inotify_tune::shutdown();
        page::fstab::shutdown();
        page::cpufreq::shutdown();
        page::cleaner::shutdown();
        page::smart::shutdown();
        page::systemd::shutdown();
        page::monitor::shutdown();
    });
    app.run()
}

fn build_ui(app: &adw::Application) {
    // ---------- 顶部工具栏 ----------
    let header = adw::HeaderBar::new();
    let title = gtk::Label::new(Some("linbox"));
    title.add_css_class("title");
    header.set_title_widget(Some(&title));

    // 页签栏（GtkStackSwitcher）的按钮被 Adwaita 样式强制了较大的 min-width，
    // 5 个页签光是按钮就撑到 666px，是窗口缩不窄的主因之一；这里只针对
    // 页签栏解除该限制（按钮按文字自适应，仍保留正常内边距）。
    {
        let css = gtk::CssProvider::new();
        css.load_from_string("stackswitcher > button { min-width: 0; }");
        if let Some(d) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &d,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    }

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
    let radio_auto = gtk::CheckButton::builder()
        .label("自动")
        .active(true)
        .build();
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
    // 关闭横向 homogeneous：GtkStack 默认按「所有页面」里最宽的那个算最小宽度，
    // 哪怕该页面当时并没显示；关掉后只按当前显示的页面算，窗口才能真正缩窄。
    stack.set_hhomogeneous(false);

    // 注册页面 + 侧边栏条目
    // 首页（Phase 13.4）：功能总览，点击条目切到对应功能页；侧边栏高亮同步
    // 由下方 `visible-child-notify` 统一处理（原先只有「点侧边栏 → 切 stack」，
    // 从首页跳页不会高亮，见 GOAL 摸底 9）。
    let stack_for_home = stack.clone();
    let home_page = page::home::build(move |page_id| {
        stack_for_home.set_visible_child_name(page_id);
    });
    add_nav_item(
        &home_list,
        &stack,
        PAGE_HOME,
        "user-home-symbolic",
        "首页",
        home_page.widget(),
    );

    // 功能页：按 model::nav_catalog::NAV_CATALOG 逐条注册。
    // Phase 13.1 起，page id / 图标 / 中文名的唯一真源是该目录
    // （D13.1），这里只按 `page` 分派对应的 build 函数。
    // 顺便收集每个模块的侧边栏行：设置页改序时按新序 remove+append 复用
    // 同一个 row（GTK4 ListBox 没有重排 API，GOAL 摸底 7）。
    let mut nav_row_vec: Vec<(String, gtk::ListBoxRow)> = Vec::new();
    for entry in model::nav_catalog::NAV_CATALOG {
        let (icon, label) = (entry.icon, entry.label);
        let row = match entry.page {
            PAGE_JSON => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::json_parser::build().widget(),
            ),
            PAGE_MEDIA => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::media_converter::build().widget(),
            ),
            PAGE_APIKEY => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::api_key_sniffer::build().widget(),
            ),
            PAGE_FCITX => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::fcitx_fix::build().widget(),
            ),
            PAGE_PATHSCANNER => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::path_scanner::build().widget(),
            ),
            PAGE_ARCHCRACKER => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::archive_cracker::build().widget(),
            ),
            PAGE_PORTSCANNER => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::port_scanner::build().widget(),
            ),
            // 下载页的 build() 直接返回 gtk::Widget（其余页返回各自的 Page 结构）
            PAGE_DOWNLOAD => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                &page::download::build(),
            ),
            PAGE_NOTEPAD => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::notepad::build().widget(),
            ),
            PAGE_SYSTEMD => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::systemd::build().widget(),
            ),
            PAGE_DESKTOP_MENU => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::desktop_menu::build().widget(),
            ),
            PAGE_DESKTOP_AUTOSTART => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::desktop_autostart::build().widget(),
            ),
            PAGE_MONITOR => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::monitor::build().widget(),
            ),
            PAGE_ENVEDITOR => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::env_editor::build().widget(),
            ),
            PAGE_INOTIFY => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::inotify_tune::build().widget(),
            ),
            PAGE_FSTAB => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::fstab::build().widget(),
            ),
            PAGE_CPUFREQ => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::cpufreq::build().widget(),
            ),
            PAGE_CLEANER => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::cleaner::build().widget(),
            ),
            PAGE_SMART => add_nav_item(
                &nav_list,
                &stack,
                entry.page,
                icon,
                label,
                page::smart::build().widget(),
            ),
            // 目录里出现未知 page id 时跳过（正常情况下不会发生，
            // catalog 与 PAGE_* 常量的一致性由 model::nav_catalog 测试锁定）
            _ => continue,
        };
        nav_row_vec.push((entry.page.to_string(), row));
    }

    // 功能区行 + 设置页（Phase 13.3）：设置页是 v1 唯一的设置项 = 导航排序。
    // 排序变化经回调重排侧边栏、刷新首页（首页当时被隐藏也要更新，避免
    // 下次进入是旧序）。
    let nav_rows: std::rc::Rc<std::cell::RefCell<Vec<(String, gtk::ListBoxRow)>>> =
        std::rc::Rc::new(std::cell::RefCell::new(nav_row_vec));

    let nav_list_for_order = nav_list.clone();
    let nav_rows_for_order = nav_rows.clone();
    let home_page_cb = home_page.clone();
    let stack_for_cb = stack.clone();
    let home_list_for_cb = home_list.clone();
    let nav_list_for_cb = nav_list.clone();
    let settings_list_for_cb = settings_list.clone();
    let settings_page = page::settings::build(move |new_order| {
        {
            let mut rows = nav_rows_for_order.borrow_mut();
            reorder_nav_rows(&nav_list_for_order, &mut rows, &new_order);
        }
        // 重排会清掉选中态：把当前显示页对应的行重新选上
        if let Some(name) = stack_for_cb.visible_child_name() {
            let rows = nav_rows_for_order.borrow();
            select_nav_row(
                &home_list_for_cb,
                &nav_list_for_cb,
                &settings_list_for_cb,
                &rows,
                name.as_str(),
            );
        }
        home_page_cb.set_order(&new_order);
    });
    add_nav_item(
        &settings_list,
        &stack,
        PAGE_SETTINGS,
        "preferences-system-symbolic",
        "设置",
        settings_page.widget(),
    );

    // 应用已保存的排序（Phase 13.3）：坏配置由 utils::nav_order 归一化为默认序，
    // 首页与侧边栏同源同序（用户要求的硬口径）。
    let saved_cfg = utils::nav_order::load();
    let default_order = model::nav_catalog::default_order();
    let effective_order = utils::nav_order::normalize_order(&saved_cfg.order, &default_order);
    {
        let mut rows = nav_rows.borrow_mut();
        reorder_nav_rows(&nav_list, &mut rows, &effective_order);
    }
    home_page.set_order(&effective_order);

    // 三个 ListBox 互斥选中：选中一行时取消另外两个 ListBox 的选中
    let _home_list_c = home_list.clone();
    let nav_list_c = nav_list.clone();
    let settings_list_c = settings_list.clone();
    home_list.connect_row_selected(move |_, row| {
        if row.is_some() {
            nav_list_c.unselect_all();
            settings_list_c.unselect_all();
        }
    });
    let home_list_c2 = home_list.clone();
    let _nav_list_c2 = nav_list.clone();
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

    // 反向导航同步（Phase 13.4 摸底 9）：stack 当前页变了 → 三个 ListBox 的
    // 选中态跟着变（首页点卡片跳页后侧边栏也会高亮）。
    // `select_row` 只触发 row-selected，不会触发 row-activated，无递归风险。
    // 顺带：本闭包与 stack（= 窗口）同寿，把首页/设置两个页面句柄也捕获进来，
    // 避免 build_ui 结束后页面被析构（它们的回调还要继续工作）。
    let home_list_sync = home_list.clone();
    let nav_list_sync = nav_list.clone();
    let settings_list_sync = settings_list.clone();
    let nav_rows_sync = nav_rows.clone();
    let _keep_pages_alive = (home_page.clone(), settings_page.clone());
    stack.connect_visible_child_notify(move |stack| {
        let _ = &_keep_pages_alive;
        let Some(name) = stack.visible_child_name() else {
            return;
        };
        select_nav_row(
            &home_list_sync,
            &nav_list_sync,
            &settings_list_sync,
            &nav_rows_sync.borrow(),
            name.as_str(),
        );
    });

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
    sidebar_toggle.connect_toggled(clone!(
        #[weak]
        split,
        move |btn| {
            split.set_show_sidebar(btn.is_active());
        }
    ));

    toolbar.set_content(Some(&split));

    // ---------- 窗口 ----------
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(800)
        .default_height(640)
        .title("linbox")
        .content(&toolbar)
        .build();
    // 让备忘录页的子对话框能引用主窗口
    page::notepad::set_window(&window);

    // 关闭窗体：询问是否保存并同步
    window.connect_close_request(|win| {
        if !page::notepad::sync::is_configured() || !page::notepad::is_dirty() {
            // 没配云端，直接关
            return gtk::glib::Propagation::Proceed;
        }
        let dialog = adw::MessageDialog::builder()
            .heading("关闭前同步？")
            .body("是否先拉取再推送到云端，然后关闭？")
            .build();
        dialog.add_response("cancel", "取消");
        dialog.add_response("no", "直接关闭");
        dialog.add_response("yes", "保存并同步");
        dialog.set_response_appearance("yes", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("yes"));
        let win_weak = win.downgrade();
        dialog.connect_response(None, move |_, response| {
            let Some(w) = win_weak.upgrade() else {
                return;
            };
            match response {
                "yes" => {
                    // 先把编辑区里可能还在节流窗口内的内容强行写盘，否则
                    // 最后几个字根本不在磁盘上，也就不会被推上远端。
                    page::notepad::flush();
                    // 先 pull 再 push，完成后关窗
                    let configs = page::notepad::sync::load_configs();
                    if !configs.is_empty() {
                        // 关窗过程没法弹 toast，但至少别把失败吞掉（stderr 留痕）
                        if let Err(e) = page::notepad::sync::pull_only(&configs) {
                            eprintln!("关窗前拉取失败：{e}");
                        }
                        if let Err(e) = page::notepad::sync::push_only(&configs) {
                            eprintln!("关窗前推送失败：{e}");
                        }
                    }
                    page::notepad::mark_clean();
                    w.close();
                }
                "no" => {
                    // 直接关
                    w.close();
                }
                _ => {} // cancel: 不关
            }
        });
        dialog.present();
        // 阻止默认关闭，等对话框处理完
        gtk::glib::Propagation::Stop
    });

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
fn apply_scheme_animated(
    window: &adw::ApplicationWindow,
    sm: &adw::StyleManager,
    scheme: adw::ColorScheme,
) {
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
    anim_out.connect_done(clone!(
        #[weak]
        sm,
        move |_| {
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
        }
    ));

    anim_out.play();
}

/// 添加一个侧边栏菜单条目，并同步注册已构建好的内容页。
///
/// 返回创建的行：功能区行需要被设置页排序时按新序重排（Phase 13.3），
/// 首页/设置两行固定位置、不参与排序，可忽略返回值。
fn add_nav_item(
    sidebar: &gtk::ListBox,
    stack: &gtk::Stack,
    page: &str,
    icon: &str,
    label: &str,
    content: &impl IsA<gtk::Widget>,
) -> gtk::ListBoxRow {
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
    row
}

/// 按页面名在三个 ListBox 里选中对应的行（Phase 13.4 反向导航同步 +
/// Phase 13.3 重排后的选中态恢复）。
///
/// 功能模块在 `rows` 里按 `widget_name` = page id 精确匹配；首页与设置固定
/// 在各自列表的第 0 行（D13.3：它们不参与排序）。
fn select_nav_row(
    home_list: &gtk::ListBox,
    nav_list: &gtk::ListBox,
    settings_list: &gtk::ListBox,
    rows: &[(String, gtk::ListBoxRow)],
    name: &str,
) {
    let hit = rows
        .iter()
        .find(|(id, _)| id == name)
        .map(|(_, row)| row.clone());
    match hit {
        Some(row) => {
            home_list.unselect_all();
            settings_list.unselect_all();
            nav_list.select_row(Some(&row));
        }
        None if name == PAGE_HOME => {
            nav_list.unselect_all();
            settings_list.unselect_all();
            if let Some(r) = home_list.row_at_index(0) {
                home_list.select_row(Some(&r));
            }
        }
        None if name == PAGE_SETTINGS => {
            nav_list.unselect_all();
            home_list.unselect_all();
            if let Some(r) = settings_list.row_at_index(0) {
                settings_list.select_row(Some(&r));
            }
        }
        None => {
            // 未知页面名：不猜，保持现状
        }
    }
}

/// 按给定顺序重排侧边栏功能区的行（Phase 13.3）。
///
/// GTK4 `ListBox` 没有重排 API（GOAL 摸底 7）：按新序 `remove` 全部行、
/// 再按新序 `append` 回去，全程复用同一个 row widget（行的 `widget_name`
/// = page id 不变，与 stack 页面的对应关系不受影响；stack 内页面不重建）。
fn reorder_nav_rows(list: &gtk::ListBox, rows: &mut [(String, gtk::ListBoxRow)], order: &[String]) {
    rows.sort_by_key(|(id, _)| {
        order.iter().position(|o| o == id).unwrap_or(usize::MAX) // 不在新序里的（理论上不会发生）排到最后
    });
    for (_, row) in rows.iter() {
        list.remove(row);
    }
    for (_, row) in rows.iter() {
        list.append(row);
    }
}
