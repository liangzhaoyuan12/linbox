# linbox UI 规范书

| 项目 | 内容 |
| --- | --- |
| 版本 | v1.0 |
| 日期 | 2026-08-31 |
| 状态 | 草案（待评审） |
| 技术栈 | Rust · GTK 4.8（gtk4 crate 0.11.4）· libadwaita 1.2（libadwaita crate 0.9.2） |
| 适用范围 | linbox 全部界面（窗口、导航、内容区、控件、动画） |

---

## 1. 概述与目标

linbox 是一款基于 **GTK4 + libadwaita** 构建的桌面应用。本规范书定义其用户界面的统一标准，目标：

1. **与 GNOME 原生体验完全一致**：控件、样式、行为均来自 libadwaita（Adwaita 设计语言），不使用自定义主题或非标准控件。
2. **主菜单栏位于应用左侧**：采用 GNOME 40+ 应用通行的「左侧导航侧边栏 + 右侧内容区」布局（类似 GNOME Files、GNOME Software）。
3. **过渡动画优雅且克制**：全部使用 libadwaita / GTK4 内置动画体系，遵循 GNOME 动效节奏，快速、平滑、可中断、尊重系统「减少动态效果」设置。

---

## 2. 设计原则

| 原则 | 说明 |
| --- | --- |
| 遵循 GNOME HIG | 以 [GNOME Human Interface Guidelines](https://developer.gnome.org/hig/) 为最高准则，本规范是其对 linbox 的落地细化 |
| 内容优先 | 界面服务于内容；装饰性元素最小化 |
| 直接操作 | 主要功能通过直接点击、选择完成，避免弹窗打断流程 |
| 一致性 | 同类型操作在全应用中外观、行为、动画一致 |
| 自适应 | 窗口宽度变化时，导航侧边栏自动折叠为栈式导航（NavigationView） |
| 克制动画 | 动画仅用于「表达空间关系与状态变化」，不用于炫技 |

---

## 3. 整体布局与导航

### 3.1 窗口布局结构

```
┌────────────────────────────────────────────────────────────────┐
│  AdwApplicationWindow (1080×720, 最小 480×400)                  │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │  AdwToolbarView                                            │ │
│  │  ┌──────────────────────────────────────────────────────┐  │ │
│  │  │  AdwHeaderBar (顶部工具栏)                           │  │ │
│  │  │  [⌂ 返回]  标题「linbox」                [搜索][菜单] │  │ │
│  │  └──────────────────────────────────────────────────────┘  │ │
│  │  ┌────────────────────────────────────────────────────────┐ │ │
│  │  │  AdwNavigationSplitView                               │ │ │
│  │  │  ┌───────────────┬────────────────────────────────────┐│ │ │
│  │  │  │ 左侧主菜单栏   │  内容区 (GtkStack)                 ││ │ │
│  │  │  │ 260px         │  ┌──────────────────────────────┐  ││ │ │
│  │  │  │ ┌───────────┐ │  │ 页面 1 / 页面 2 / 页面 3     │  ││ │ │
│  │  │  │ │ ○ 首页    │ │  │                              │  ││ │ │
│  │  │  │ │ □ 项目    │ │  │                              │  ││ │ │
│  │  │  │ │ ◇ 收藏    │ │  │                              │  ││ │ │
│  │  │  │ │ ✉ 消息    │ │  │                              │  ││ │ │
│  │  │  │ ├───────────┤ │  │                              │  ││ │ │
│  │  │  │ │ ⚙ 设置    │ │  │                              │  ││ │ │
│  │  │  │ └───────────┘ │  └──────────────────────────────┘  ││ │ │
│  │  │  └───────────────┴────────────────────────────────────┘│ │ │
│  │  └────────────────────────────────────────────────────────┘ │ │
│  └────────────────────────────────────────────────────────────┘ │
└────────────────────────────────────────────────────────────────┘
```

### 3.2 组件分层

| 层级 | 组件 | 职责 |
| --- | --- | --- |
| 窗口 | `adw::ApplicationWindow` | 顶层容器，自带窗框、宽高、阴影 |
| 工具栏 | `adw::ToolbarView` | 承载顶部 `AdwHeaderBar` 与主内容 |
| 顶部栏 | `adw::HeaderBar` | 标题、返回按钮、搜索入口、全局菜单 |
| 导航 | `adw::NavigationSplitView` | 左侧主菜单 + 右侧内容，窄窗口自动折叠 |
| 侧边栏 | `adw::NavigationSidebar` / `gtk::ListBox` | 主菜单条目（图标 + 文字） |
| 内容 | `gtk::Stack` | 各页面容器，页面间切换动画 |

### 3.3 窗口规范

| 属性 | 规范值 |
| --- | --- |
| 默认尺寸 | 1080 × 720 |
| 最小尺寸 | 480 × 400 |
| 窗口标题 | 「linbox」（仅一个窗口时隐藏标题栏内容，保留控件） |
| Application ID | 遵循反向域名，如 `org.linbox.App`（示例，按实际定） |

### 3.4 左侧主菜单栏规范

| 属性 | 规范值 |
| --- | --- |
| 宽度 | 260px（含边距）；允许用户拖拽调整 200–320px |
| 条目构成 | 图标（16px）+ 文字（body 字号），条目高 36px |
| 选中态 | libadwaita 默认 accent 背景高亮 + 左侧无额外指示条 |
| 悬停态 | libadwaita 默认 hover 背景 |
| 分组 | 主要功能在上，次级（设置/关于）用分隔线分在下部 |
| 折叠行为 | 窗口宽度 < 700px 时，`NavigationSplitView` 自动折叠为栈式导航（见 §6.2 动画） |
| 显隐控制 | 顶部栏「汉堡」按钮切换侧边栏显隐（`show_sidebar`），带展开/收起动画 |

菜单条目示例（以实际功能为准）：

- 首页 / 项目 / 收藏 / 最近使用 …
- 分隔线
- 设置（`AdwPreferencesPage` 风格页）

---

## 4. 控件选型规范

**强制要求**：一切可用 libadwaita 控件覆盖的场景，必须使用 libadwaita 控件；libadwaita 未提供的底层控件使用 GTK4，且样式遵循 Adwaita 默认。**禁止**：自定义绘制控件、手写 CSS 覆盖主题变量、引入非 GNOME 控件库。

| 用途 | 指定控件 | 说明 |
| --- | --- | --- |
| 窗口 | `adw::ApplicationWindow` | — |
| 工具栏容器 | `adw::ToolbarView` | `add_top_bar(&header_bar)` |
| 顶部栏 | `adw::HeaderBar` | 居中标题、两侧装填控件 |
| 左侧导航容器 | `adw::NavigationSplitView` + `adw::NavigationSidebar` | 自动折叠 |
| 页面切换 | `gtk::Stack` | `transition_type` 见 §6.3 |
| 菜单按钮 | `gtk::MenuButton` | 下拉菜单用 `gio::Menu` |
| 开关 | `adw::SwitchRow` | 设置页 |
| 文本行 | `adw::ActionRow` / `adw::EntryRow` / `adw::PasswordEntryRow` | 表单与列表 |
| 偏好设置页 | `adw::PreferencesPage` / `adw::PreferencesGroup` | 设置界面 |
| 标签 | `adw::TabView` / `adw::TabBar`（如需多标签） | — |
| 按钮 | `gtk::Button`（样式类 `suggested-action` / `destructive-action`） | 主操作 / 危险操作 |
| 列表 | `gtk::ListBox` / `gtk::ListView` | 简单列表用 ListBox，大数据用 ListView |
| 头像 | `adw::Avatar` | 用户/项目头像 |
| 通知 | `adw::ToastOverlay` + `adw::Toast` | 轻量提示，自带滑入滑出动画 |
| 弹窗 | `adw::MessageDialog` | 确认 / 危险操作 |
| 进度 | `gtk::ProgressBar` / `adw::Spinner` | — |
| 空状态 | `adw::StatusPage` | 图标 + 标题 + 描述 + 可选动作 |
| 搜索 | `gtk::SearchBar` / `adw::SearchEntry` | — |
| 日期时间 | `adw::DatePicker` / `adw::TimePicker`（如需要） | — |
| 分割线 | `gtk::Separator` | — |

### 4.1 按钮层级（参考 GNOME HIG）

| 层级 | 样式 | 用途 |
| --- | --- | --- |
| 建议操作 | `suggested-action` | 页面内唯一主操作 |
| 普通 | 默认 | 次级操作 |
| 危险 | `destructive-action` | 删除等不可逆操作，配合 `MessageDialog` 二次确认 |

---

## 5. 视觉规范

### 5.1 主题

- 主题管理统一使用 `adw::StyleManager`，提供 **跟随系统 / 浅色 / 深色** 三种模式，默认「跟随系统」。
- 不做任何自定义配色覆盖；明暗切换时 libadwaita 自动处理背景、文字、控件的语义色切换。
- 深色模式下对比度由 libadwaita 默认值保证（≥ 4.5:1，正文）。

### 5.2 色彩（使用 libadwaita 语义色，禁止硬编码色值）

| 用途 | 语义色 / 样式类 |
| --- | --- |
| 强调/选中 | accent（默认蓝） |
| 成功 | `success`（列表行状态、Toast 图标） |
| 警告 | `warning` |
| 错误/危险 | `error` / `destructive-action` |
| 文字 | 默认前景色（浅色/深色自动） |
| 次级文字 | `dim-label` 样式类 |
| 背景层级 | `view`（内容）、`card`（卡片）、`sidebar`（侧栏）默认值 |

### 5.3 排版

- 字体：系统默认（Cantarell）；不指定自定义字体。
- 字号与字重使用 libadwaita 样式类，禁止直接设置像素字号：

| 场景 | 样式类 | 备注 |
| --- | --- | --- |
| 页面大标题 | `title-1` | 每个内容页顶部 |
| 区块标题 | `title-3` / `title-4` | 设置分组、卡片标题 |
| 正文 | `body` | 默认 |
| 次级说明 | `dim-label` | 辅助文字 |
| 标题栏 | 系统默认 | HeaderBar 自动 |

### 5.4 间距与尺寸

- **基础网格 4px**，间距取值必须为 4 的倍数。
- 常用间距：内容边距 24px（窄窗口 12px）；卡片内边距 12px；条目间距 4–8px；分组间距 24px。
- 控件最小可点击区域：24 × 24px（触控友好）。
- 圆角、阴影、边框一律使用 libadwaita 默认值（卡片 `card` 大圆角、条带分隔），不自定义。

---

## 6. 过渡动画规范（重点）

### 6.1 总原则

| 原则 | 要求 |
| --- | --- |
| 快速 | 微交互 ≤ 150ms，常规 ≤ 300ms，不拖沓 |
| 克制 | 同一时刻最多一个「页面级」动画 + 少量微动画 |
| 可中断 | 动画可被新动画打断，不排队播放 |
| 尊重系统 | 当系统禁用动画（`gtk-enable-animations=false`）时，所有动画立即完成、不播放 |
| 统一缓动 | 进入 `ease-out`，往返 `ease-in-out`，避免 `linear`（除进度条） |
| 统一来源 | 一律使用 GTK4/libadwaita 动画 API，禁止手写计时器动画 |

### 6.2 时长与缓动参数表

| 场景 | 时长 | 缓动 | 实现 |
| --- | --- | --- | --- |
| 微交互（悬停/按压高亮、图标反馈） | ≤ 100ms | ease-out | 内置（无需配置） |
| 控件展开/收起（Revealer） | 200ms | ease-out-cubic | `gtk::Revealer` 默认 |
| 页面切换（Stack） | 250ms | ease-out-cubic | `GtkStack` transition |
| 侧边栏折叠/展开（NavigationSplitView） | ~250ms 弹簧 | 弹簧（damping 0.7） | libadwaita 内置 |
| 侧边栏收起为栈式导航（窗口变窄） | ~300ms 弹簧 | 弹簧 | `NavigationSplitView` 内置 |
| 栈式导航推入/弹出（NavigationView） | ~350ms | ease-out-quint / 弹簧 | 内置 |
| Toast 滑入/滑出 | 250ms | ease-out / ease-in | `AdwToast` 内置 |
| 对话框出现 | 250ms | ease-out | `AdwMessageDialog` 内置 |
| 列表行增删 | 150–200ms | ease-out | `GtkListBox`/`ListView` 过渡 |

> 弹簧参数参考：`AdwSpringParams(damping_ratio=0.7, mass=1.0, stiffness≈600)`，接近 GNOME 实际动效。

### 6.3 各场景动画规范

| 场景 | 动画 | 说明 |
| --- | --- | --- |
| 主菜单切换页面 | `GtkStack` 默认 `Crossfade`（250ms） | 内容区淡入淡出，平滑不晕眩；若页面方向性强（向导）可用 `SlideUpDown` |
| 点击左侧菜单条目 | 条目选中高亮（≤100ms）+ 内容区 Crossfade | 高亮先于内容变化，形成「先反馈后呈现」 |
| 侧边栏显示/隐藏 | `NavigationSplitView` 内置弹簧动画 | 顶部栏汉堡按钮触发 |
| 窗口变窄折叠 | 侧边栏滑出 + 内容区变为栈式导航 | 自动触发，动画由库完成 |
| 栈式导航深入 | 页面从左推入（`NavigationView` 默认） | 返回时反向滑出 |
| 元素显示/隐藏（面板、筛选行） | `GtkRevealer` + `SlideDown`/`SlideUp`，200ms | 配合 `reveal_child` |
| 列表删除 | 行淡出/高度收缩后移除 | 数据量小时用 `ListBox` 过渡 |
| 全局通知 | `AdwToast` 从底部滑入，自动消退 | 禁止打断当前操作 |
| 对话框 | 居中放大淡入 | `MessageDialog` 默认 |
| 加载中 | `adw::Spinner` 旋转 + 页面内容占位保持稳定 | 不整页闪烁 |
| 首帧 | 无开场动画（应用直接呈现） | — |

### 6.4 推荐实现方式（Rust 代码示例）

```rust
use gtk::prelude::*;
use libadwaita::prelude::*;

// —— 1. 内容区页面切换：Crossfade 250ms ——
let stack = gtk::Stack::new();
stack.set_transition_type(gtk::StackTransitionType::Crossfade);
stack.set_transition_duration(250);

// 通过菜单选中条目时切换页面
menu_clicked.connect_activate(move |_| {
    stack.set_visible_child_name("page_home");
});

// —— 2. 左侧导航：NavigationSplitView（自动折叠 + 弹簧动画） ——
let split_view = adw::NavigationSplitView::new();
split_view.set_sidebar(Some(&sidebar));     // 左侧主菜单
split_view.set_content(Some(&stack));       // 右侧内容区
split_view.set_show_sidebar(true);
// 汉堡按钮切换：
toggle_btn.connect_clicked(move |_| {
    split_view.set_show_sidebar(!split_view.show_sidebar());
});

// —— 3. 面板展开/收起：GtkRevealer ——
let revealer = gtk::Revealer::new();
revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
revealer.set_transition_duration(200);
toggle.connect_toggled(move |b| revealer.set_reveal_child(b.is_active()));

// —— 4. 自定义属性动画（示例：强调色数字跳动）——
// 使用 GTK4 动画框架 + libadwaita 弹簧参数
use glib::clone;

let params = adw::SpringParams::new(0.7, 1.0, 600.0); // damping, mass, stiffness
let target = gtk::PropertyAnimationTarget::new(&some_widget, "opacity");
let anim = adw::SpringAnimation::new(&some_widget, 0.0, 1.0, 0.0, &params, &target);
anim.play();
```

### 6.5 禁止事项

- 禁止自定义缓动函数曲线、禁止逐帧手写动画（`timeout_add` + 手动改属性）。
- 禁止动画循环播放（无限 loop）用于纯装饰。
- 禁止页面整体平移/缩放动画（除非 NavigationView 内置）。
- 禁止不同页面使用不同时长/缓动的同类型动画。
- 禁止动画阻塞交互；动画播放期间控件必须仍然可点。

---

## 7. 图标与插画

| 项目 | 规范 |
| --- | --- |
| 图标集 | GNOME 官方 `adwaita-icon-theme`（系统自带） |
| 来源 | 优先使用命名符号图标（`"list-symbolic"` 等）与语义图标（`"edit-delete-symbolic"`） |
| 尺寸 | 侧边栏/工具栏 16px；内容区状态图标 32–64px；空状态 128px |
| 类型 | 一律 symbolic（单色，随主题切换深浅色） |
| 禁止 | 禁止嵌入位图图标、第三方图标集 |

---

## 8. 交互与反馈规范

| 场景 | 行为 |
| --- | --- |
| 悬停 | libadwaita 默认背景变化（≤100ms） |
| 按压 | 默认按压效果（按下加深） |
| 键盘焦点 | Tab 顺序 = 视觉顺序；焦点环使用默认样式 |
| 快捷键 | 显示在条目 tooltip 与菜单中；全局快捷键在应用内处理 |
| 右键 | 不依赖右键完成关键操作（提供等价入口） |
| 提示 | 使用 `gtk::Tooltip`，延迟 500ms 显示 |
| 空状态 | 每个页面提供 `adw::StatusPage`（图标 + 标题 + 描述 + 主操作） |
| 长任务 | 使用非阻塞操作 + 状态提示，禁止冻结界面 |

---

## 9. 无障碍要求

- 文本对比度遵循 Adwaita 默认（正文 ≥ 4.5:1）。
- 全部控件可键盘操作；不依赖拖拽完成功能。
- 设置辅助标签（`set_tooltip_text` / `set_accessible_name`）确保读屏器可用。
- 遵守「减少动态效果」：检测系统动画设置，关闭非必要动画。
- 触控目标 ≥ 24px。

---

## 10. 实现约束

### 10.1 依赖（当前 Cargo.toml）

```toml
[dependencies]
gtk4 = { version = "0.11.4", features = ["gnome_43"] }
libadwaita = { version = "0.9.2", features = ["gtk_v4_8"] }
```

### 10.2 窗口骨架（供实现参考）

```rust
use gtk::prelude::*;
use libadwaita::prelude::*;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("org.linbox.App")
        .build();

    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    // 顶部工具栏
    let header = adw::HeaderBar::new();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);

    // 左侧主菜单（侧边栏条目：图标 + 标题）
    let sidebar = gtk::ListBox::new();
    // …填充菜单条目，选中回调切换 stack 页面…

    // 右侧内容区
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(250);
    // …add_named 各页面…

    let split = adw::NavigationSplitView::new();
    split.set_sidebar(Some(&sidebar));
    split.set_content(Some(&stack));
    split.set_show_sidebar(true);

    toolbar.set_content(Some(&split));

    let win = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(1080)
        .default_height(720)
        .title("linbox")
        .content(&toolbar)
        .build();
    win.present();
}
```

### 10.3 代码规范

- 布局代码全部使用 `gtk::Builder` / 直接代码构建，不使用 Glade 遗留格式。
- 样式类必须在代码中声明（Rust 无 CSS 文件），使用 libadwaita 内置类名。
- 组件按模块拆分（`window` / `sidebar` / `pages` / `widgets`），便于维护。

---

## 11. 验收检查清单

- [ ] 启动后窗口为 `AdwApplicationWindow`，默认 1080×720，最小 480×400。
- [ ] 主菜单栏位于应用左侧，含图标+文字条目，选中高亮为 accent。
- [ ] 点击菜单条目，内容区以 Crossfade（250ms）切换，选中高亮先变化。
- [ ] 汉堡按钮可展开/收起侧边栏，动画为弹簧效果。
- [ ] 窗口变窄时侧边栏自动折叠为栈式导航，动画平滑。
- [ ] 所有控件均来自 libadwaita/GTK4，无自定义绘制、无自定义主题 CSS。
- [ ] 深浅色模式切换（含跟随系统）全部界面正常。
- [ ] Toast、Revealer、对话框动画时长/缓动符合 §6.2 参数表。
- [ ] 系统开启「减少动态效果」后动画全部禁用。
- [ ] 全部功能可键盘操作，触控目标 ≥ 24px。
- [ ] 无任何禁用的自定义动画实现。
