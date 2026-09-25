//! 音视频 / 图片转换页面（展示层 · 仅 UI）。
//!
//! 功能覆盖规划书 §2 / §3：
//! - 模块 A：音视频转换（拖拽批量导入 GdkDropTarget、ffprobe 信息、视频/音频参数、
//!   片段截取、拼接、拆条）
//! - 模块 B：图片转换（格式互转、分辨率/批量、视频↔图片序列、视频→GIF）
//! - 模块 C：高级选项（图形化控件 + 滤镜链编辑器 + 自定义参数注入 + 命令预览）
//! - 模块 3：硬件加速自动探测与手动选择
//!
//! 本页面不实现任何 ffmpeg 调用细节，统一调用 `crate::utils::media` 的纯函数，
//! 仅在「开始转换」时于后台线程执行命令。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;

use crate::model::media::*;
use crate::utils::media::command::{
    CommandPlan, ConversionSpec, FilterEntry, JobMode, RotateMode, WatermarkPos, build_commands,
    to_shell_script,
};
use crate::utils::media::hwaccel::{self, HwCapabilities};
use crate::utils::media::probe::{self, MediaInfo};

pub struct MediaConverterPage {
    root: adw::ToastOverlay,
}

/// 输入文件列表中的一项。
#[derive(Clone)]
struct InputItem {
    /// 稳定自增 ID：菜单回调按 ID 定位当前索引，避免删除/重排后旧索引错位。
    id: u64,
    path: String,
    info: RefCell<Option<MediaInfo>>,
    info_label: gtk::Label,
    /// 该文件最终输出名展示（如「→ out.mp4」），在 update_preview 中刷新。
    out_label: gtk::Label,
    /// 单项覆盖的输出文件名（None = 跟随全局默认「输出文件名」）。
    name_override: RefCell<Option<String>>,
    row: gtk::ListBoxRow,
}

/// ① 输入区：拖拽/按钮导入、文件列表与单项命名提示。
struct InputSection {
    file_list: gtk::ListBox,
    inputs: RefCell<Vec<InputItem>>,
    /// 输入项的稳定自增 ID 分配器。
    next_id: Cell<u64>,
    /// 选中列表项时才展开的「该项覆盖命名」框（留空 = 跟随全局）。
    /// 与作业模式卡里的全局框彻底分离：全局框恒显全局值，本框恒显覆盖值。
    name_override_row: adw::EntryRow,
    /// 提示条：恒一行文案（选中说明 / 未选中引导 / 非 Single 统一说明）。
    selection_hint: gtk::Label,
    /// 「跟随全局」按钮：清空选中项的覆盖名。
    override_clear: gtk::Button,
}

/// ② 输出区：作业模式、分类、格式、输出目录与命名。
struct OutputSection {
    mode_row: adw::ComboRow,
    category_row: adw::ComboRow,
    format_row: adw::ComboRow,
    out_dir_row: adw::EntryRow,
    out_name_row: adw::EntryRow,
    /// 全局默认输出文件名。与 out_name_row 解耦：选中单项时输入框编辑的是该
    /// 项的覆盖名，未选中时编辑的是这里的全局默认值。
    default_name: RefCell<String>,
    /// 程序回填命名框（全局 out_name_row / 输入区 name_override_row）时的
    /// 防回写标志：set_text 触发 notify → text_changed，靠它短路避免回写。
    loading_name: Cell<bool>,
    /// 进入图片/GIF 类模式前用户手动选择的分类，退出时恢复。
    last_manual_category: Cell<Option<u32>>,
}

/// ③ 视频参数卡片。
struct VideoSection {
    video_card: adw::PreferencesGroup,
    v_codec: adw::ComboRow,
    v_bitrate_mode: adw::ComboRow,
    v_crf: adw::SpinRow,
    v_bitrate: adw::SpinRow,
    v_res: adw::ComboRow,
    v_w: adw::SpinRow,
    v_h: adw::SpinRow,
    v_keep_aspect: adw::SwitchRow,
    v_fps: adw::ComboRow,
    v_fps_custom: adw::SpinRow,
    v_scale_algo: adw::ComboRow,
    v_colorspace: adw::ComboRow,
    v_color_range: adw::ComboRow,
    v_hdr: adw::SwitchRow,
}

/// ④ 音频参数卡片。
struct AudioSection {
    audio_card: adw::PreferencesGroup,
    a_codec: adw::ComboRow,
    a_channels: adw::ComboRow,
    a_sr: adw::ComboRow,
    a_bitrate: adw::SpinRow,
    a_gain: adw::SpinRow,
    a_fade_in: adw::SpinRow,
    a_fade_out: adw::SpinRow,
}

/// ⑤ 图片参数卡片。
struct ImageSection {
    image_card: adw::PreferencesGroup,
    i_quality: adw::SpinRow,
    i_compression: adw::SpinRow,
    i_strip: adw::SwitchRow,
    i_longest: adw::SpinRow,
    i_percent: adw::SpinRow,
    i_extract_fps: adw::SpinRow,
    i_gif_fps: adw::SpinRow,
    i_gif_w: adw::SpinRow,
}

/// ⑥ 片段截取卡片。
struct ClipSection {
    clip_card: adw::PreferencesGroup,
    clip_enabled: adw::SwitchRow,
    clip_start: adw::EntryRow,
    clip_end: adw::EntryRow,
}

/// ⑦ 高级选项卡片：画面处理、编码器细调、滤镜链。
struct AdvancedSection {
    adv_card: adw::PreferencesGroup,
    adv_crop_en: adw::SwitchRow,
    adv_crop_w: adw::SpinRow,
    adv_crop_h: adw::SpinRow,
    adv_crop_x: adw::SpinRow,
    adv_crop_y: adw::SpinRow,
    adv_pad_en: adw::SwitchRow,
    adv_pad_w: adw::SpinRow,
    adv_pad_h: adw::SpinRow,
    adv_pad_color: adw::EntryRow,
    adv_rotate: adw::ComboRow,
    adv_deinterlace: adw::SwitchRow,
    adv_denoise: adw::SwitchRow,
    adv_sharpen: adw::SwitchRow,
    adv_wm_en: adw::SwitchRow,
    adv_wm_path: adw::EntryRow,
    adv_wm_pos: adw::ComboRow,
    adv_wm_op: adw::SpinRow,
    adv_audio_denoise: adw::SwitchRow,
    adv_preset: adw::EntryRow,
    adv_tune: adw::EntryRow,
    adv_profile: adw::EntryRow,
    adv_level: adw::EntryRow,
    adv_pix_fmt: adw::EntryRow,
    adv_faststart: adw::SwitchRow,
    adv_two_pass: adw::SwitchRow,
    adv_threads: adw::SpinRow,
    adv_tonemap: adw::SwitchRow,
    vf_list: gtk::ListBox,
    af_list: gtk::ListBox,
}

/// ⑧ 硬件加速：独立卡片（下拉 + 探测状态行 + 手动刷新）。
struct HwSection {
    hw_card: adw::PreferencesGroup,
    /// 硬件加速下拉：选项**永久固定**为 ffmpeg 全量后端（见 `ALL_HW`），
    /// 不随探测结果增删，探测只影响「自动选择」指向的默认后端。
    hw_row: adw::ComboRow,
    /// 展示探测结果与检测时间的状态行（副标题）。
    hw_status_row: adw::ActionRow,
    /// 手动重新探测的按钮。
    hw_refresh: gtk::Button,
    /// 上次探测时刻（UNIX 秒，0 = 尚未探测）。
    hw_detected_at: Cell<u64>,
    /// 探测结果（启动时读缓存，点「刷新」时重探并回写缓存）。
    hw_caps: RefCell<HwCapabilities>,
}

/// ⑨ 自定义参数注入卡片。
struct CustomSection {
    custom_card: adw::PreferencesGroup,
    custom_global: gtk::TextView,
    custom_input: gtk::TextView,
    custom_output: gtk::TextView,
}

/// ⑩ 预览与执行：命令预览卡 + 固定操作条（状态/进度/开始转换）。
struct PreviewSection {
    preview_text: gtk::TextView,
    status_label: gtk::Label,
    progress: gtk::ProgressBar,
    run_button: gtk::Button,
}

/// 页面内部持有的控件句柄（用 Rc 便于在回调中廉价克隆）。
/// 按页面分区收敛为 10 个子结构 + 页面级 Toast 句柄。
struct Inner {
    input: InputSection,
    output: OutputSection,
    video: VideoSection,
    audio: AudioSection,
    image: ImageSection,
    clip: ClipSection,
    advanced: AdvancedSection,
    hw: HwSection,
    custom: CustomSection,
    preview: PreviewSection,
    toast_overlay: adw::ToastOverlay,
}

/// 下拉选项的标签集合（顺序与枚举一致）。
const MODES: &[&str] = &[
    "单文件转换",
    "拼接（按顺序）",
    "拆条分段",
    "视频 → 图片序列",
    "图片 → 视频",
    "视频 → GIF",
];
const CATEGORIES: &[&str] = &["视频", "音频", "图片"];
const V_CODECS: &[&str] = &[
    "H.264 (libx264)",
    "H.265 (libx265)",
    "VP9 (libvpx-vp9)",
    "AV1 (libaom-av1)",
    "复制（不重编码）",
];
const BITRATE_MODES: &[&str] = &[
    "CRF（恒定质量）",
    "CBR（恒定码率）",
    "VBR（动态码率）",
    "固定码率",
];
const RES_PRESETS: &[&str] = &[
    "保持源",
    "4K (3840×2160)",
    "2K (2560×1440)",
    "1080p (1920×1080)",
    "720p (1280×720)",
    "480p (854×480)",
    "自定义",
];
const FPS_PRESETS: &[&str] = &["保持源 (same)", "24", "25", "30", "50", "60", "自定义"];
const SCALE_ALGOS: &[&str] = &["bilinear", "lanczos", "bicubic", "spline"];
const COLORSPACES: &[&str] = &["bt709", "bt601", "bt2020"];
const COLOR_RANGES: &[&str] = &["tv（受限）", "pc（全范围）"];
const A_CODECS: &[&str] = &[
    "AAC",
    "MP3",
    "Opus",
    "Vorbis",
    "FLAC",
    "PCM (WAV)",
    "复制（不重编码）",
];
const CHANNELS: &[&str] = &["保持源", "单声道", "立体声", "5.1 环绕"];
const SAMPLE_RATES: &[&str] = &["保持源", "44.1 kHz", "48 kHz", "96 kHz"];
const ROTATES: &[&str] = &[
    "无",
    "顺时针 90°",
    "180°",
    "逆时针 90°",
    "水平翻转",
    "垂直翻转",
];
const WM_POSITIONS: &[&str] = &["左上", "右上", "左下", "右下", "居中"];
/// 硬件加速下拉的「完整」选项列表（覆盖 ffmpeg 全部硬件加速后端）。
/// 顺序即下拉展示顺序，索引与 `HwAccelPreference` 一一对应。
const ALL_HW: &[HwAccelPreference] = &[
    HwAccelPreference::Auto,
    HwAccelPreference::Software,
    HwAccelPreference::Nvenc,
    HwAccelPreference::Vaapi,
    HwAccelPreference::Qsv,
    HwAccelPreference::Amf,
    HwAccelPreference::Videotoolbox,
    HwAccelPreference::CudaDecode,
    HwAccelPreference::Dxva2,
    HwAccelPreference::D3d11va,
    HwAccelPreference::Vulkan,
    HwAccelPreference::Opencl,
];

impl MediaConverterPage {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }
}

/// 构造一个带标题的卡片容器，返回 (外层组, 内容组)，二者为同一对象。
///
/// 内容容器用 `adw::PreferencesGroup`（内部是 `GtkListBox`）。`ComboRow` 等行控件
/// 只有在 `ListBox` 里被点击时才会触发「激活」，进而弹出下拉选择框；放进普通
/// `Box` 时点击行不会激活，弹层就打不开。
fn card(title: &str) -> (adw::PreferencesGroup, adw::PreferencesGroup) {
    let group = adw::PreferencesGroup::new();
    group.set_title(title);
    group.add_css_class("card");
    group.set_margin_top(8);
    group.set_margin_bottom(8);
    group.set_margin_start(12);
    group.set_margin_end(12);
    (group.clone(), group.clone())
}

/// 组合行下拉。
fn combo_row(title: &str, labels: &[&str], init: u32) -> adw::ComboRow {
    let model = gtk::StringList::new(labels);

    adw::ComboRow::builder()
        .model(&model)
        .selected(init)
        .title(title)
        .build()
}

/// 数值行（adw::SpinRow）。
fn spin_row(title: &str, min: f64, max: f64, step: f64, digits: u32, value: f64) -> adw::SpinRow {
    let adj = gtk::Adjustment::new(value, min, max, step, step * 10.0, 0.0);
    let row = adw::SpinRow::builder()
        .adjustment(&adj)
        .climb_rate(0.5)
        .digits(digits)
        .build();
    row.set_title(title);
    row
}

/// 开关行。
fn switch_row(title: &str, subtitle: &str) -> adw::SwitchRow {
    let row = adw::SwitchRow::new();
    row.set_title(title);
    if !subtitle.is_empty() {
        row.set_subtitle(subtitle);
    }
    row
}

/// 文本输入行。
fn entry_row(title: &str, _placeholder: &str) -> adw::EntryRow {
    adw::EntryRow::builder().title(title).build()
}

/// 把单个控件的属性变化绑定到 `update_preview`（图形 + 自定义参数实时联动）。
/// 闭包不捕获 `Inner`，而是经由全局 `thread_local` 句柄读取，
/// 从而满足信号回调对 `Send` 的要求。
fn watch(w: &impl glib::object::IsA<glib::Object>) {
    w.connect_notify(None, |_, _| {
        g_update_preview();
    });
}

/// 批量绑定（6.7）：调用点以数组一次列全——`watch_all!(&[&v_codec, &v_crf, …]);`
/// 展开为逐控件 `watch`。之所以是宏而非 `&[&dyn …]` 数组：
/// 异构控件（ComboRow/SpinRow/EntryRow/Switch…）要进同一个数组得靠 trait object，
/// 而 `ObjectExt` 自带泛型方法（`connect_notify<F>` 等）不是 dyn-compatible；
/// glib 类型层级也没有从子类到父引用的隐式 upcast，`&[&gtk::Widget]` 同样写不了。
macro_rules! watch_all {
    (&[$($w:expr),+ $(,)?]) => {
        $(watch($w);)+
    };
}

pub fn build() -> MediaConverterPage {
    // ---------- 页面骨架：ToastOverlay > (滚动内容 + 固定操作条) ----------
    let toast_overlay = adw::ToastOverlay::new();
    let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
    layout.set_vexpand(true);
    toast_overlay.set_child(Some(&layout));

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_propagate_natural_height(true);
    scroller.set_propagate_natural_width(true);
    scroller.set_vexpand(true);
    layout.append(&scroller);

    // 内容列限宽留白只在根容器这一层（卡片自身不再叠加 inset）。
    let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root_box.set_vexpand(true);
    root_box.set_margin_top(12);
    root_box.set_margin_bottom(12);
    root_box.set_margin_start(12);
    root_box.set_margin_end(12);
    scroller.set_child(Some(&root_box));

    // ---------- 标题 ----------
    let title = gtk::Label::new(Some("音视频 / 图片转换"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    root_box.append(&title);

    let subtitle = gtk::Label::new(Some(
        "基于 ffmpeg：拖入媒体文件，配置参数，预览并生成命令后转换。",
    ));
    subtitle.add_css_class("dim-label");
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_wrap(true);
    root_box.append(&subtitle);

    // ---------- 任务流三区：输入 → 输出与参数 → 预览与执行 ----------
    zone_header(&root_box, "① 输入");
    let input = build_input(&root_box);
    zone_header(&root_box, "② 输出与参数");
    let output = build_mode(&root_box);
    let video = build_video(&root_box);
    let audio = build_audio(&root_box);
    let image = build_image(&root_box);
    let clip = build_clip(&root_box);
    let hw = build_hw(&root_box);
    let advanced = build_advanced(&root_box);
    let custom = build_custom(&root_box);
    zone_header(&root_box, "③ 预览与执行");
    let preview = build_preview(&root_box);

    // ---------- 固定操作条（滚动区之外，始终可见） ----------
    append_action_bar(&layout, &preview);

    // ---------- 组装 Inner ----------
    let inner = Rc::new(Inner {
        input,
        output,
        video,
        audio,
        image,
        clip,
        advanced,
        hw,
        custom,
        preview,
        toast_overlay: toast_overlay.clone(),
    });

    // 初始化封装格式选项与布局
    {
        let prefs = load_prefs();
        apply_prefs(&inner, &prefs);
        // 分类可能改变格式下拉内容，重建后再恢复格式选择
        refresh_format_options(&inner);
        if let Some(n) = prefs.get("format").and_then(|x| x.as_u64()) {
            inner.output.format_row.set_selected(n as u32);
        }
        inner.update_visibility();
        inner.update_preview();
    }

    // 硬件加速：读磁盘缓存（永久固定），只有从未检测过时才在后台探测一次。
    // 放在恢复偏好之后：用户显式选过的后端不被覆盖，
    // 仅停留在「自动选择」时按结果定位默认后端。
    if let Some(cache) = hwaccel::load_cached() {
        inner.apply_hw_cache(&cache);
    } else {
        // 从未检测过：后台探测一次并落盘，之后永久复用
        inner.hw.hw_status_row.set_subtitle("首次检测中…");
        spawn_hw_detect(false);
    }

    // 注册全局强引用，供 signal 回调（要求 Send）经 thread_local 句柄访问本页面。
    INNER.with(|i| *i.borrow_mut() = Some(Rc::clone(&inner)));

    MediaConverterPage {
        root: toast_overlay,
    }
}

/// 任务流分区标题：把长列表切成「输入 → 输出与参数 → 预览与执行」三段。
fn zone_header(root_box: &gtk::Box, text: &str) {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("title-4");
    l.set_halign(gtk::Align::Start);
    l.set_margin_top(16);
    root_box.append(&l);
}

/// 固定操作条：状态标签 + 进度条 + 开始转换，挂在滚动区之外始终可见。
fn append_action_bar(layout: &gtk::Box, preview: &PreviewSection) {
    layout.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let action_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    action_bar.set_margin_top(8);
    action_bar.set_margin_bottom(12);
    action_bar.set_margin_start(12);
    action_bar.set_margin_end(12);
    preview.status_label.set_hexpand(true);
    action_bar.append(&preview.status_label);
    preview.progress.set_width_request(160);
    action_bar.append(&preview.progress);
    action_bar.append(&preview.run_button);
    layout.append(&action_bar);
}

/// 分区①：输入文件（拖拽 / 按钮导入、列表、单项命名提示）。
fn build_input(root_box: &gtk::Box) -> InputSection {
    let (input_card, input_content) = card("输入文件");
    root_box.append(&input_card);

    let drop_area = gtk::Box::new(gtk::Orientation::Vertical, 6);
    drop_area.set_margin_top(12);
    drop_area.set_margin_bottom(12);
    drop_area.set_vexpand(false);
    drop_area.add_css_class("card");
    drop_area.set_halign(gtk::Align::Fill);
    let drop_icon = gtk::Image::from_icon_name("folder-download-symbolic");
    drop_icon.set_pixel_size(48);
    drop_icon.add_css_class("dim-label");
    drop_area.append(&drop_icon);
    let drop_hint = gtk::Label::new(Some("将文件拖拽到此处，或点击下方按钮添加"));
    drop_hint.add_css_class("dim-label");
    drop_area.append(&drop_hint);

    // GdkDropTarget：接收拖入的文件（含文件夹递归）。
    // 文件管理器拖入文件时提供的是 `GdkFileList`（GFile 列表），目标类型必须与之匹配，
    // 否则拖放会被直接拒绝、connect_drop 永不触发（之前用 STRING 类型就是这个原因）。
    let drop_target = gtk::DropTarget::new(
        gtk::gdk::FileList::static_type(),
        gtk::gdk::DragAction::COPY,
    );
    drop_target.connect_drop(move |_, value, _x, _y| {
        if let Ok(file_list) = value.get::<gtk::gdk::FileList>() {
            let paths: Vec<String> = file_list
                .files()
                .iter()
                .filter_map(|f| f.path())
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            if !paths.is_empty() {
                g_drop_paths(&paths);
                return true;
            }
        }
        false
    });
    drop_area.add_controller(drop_target);
    input_content.add(&drop_area);

    let btn_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let add_files = gtk::Button::with_label("添加文件");
    add_files.set_halign(gtk::Align::Start);
    let add_folder = gtk::Button::with_label("添加文件夹");
    add_folder.set_halign(gtk::Align::Start);
    let clear_btn = gtk::Button::with_label("清空列表");
    clear_btn.add_css_class("destructive-action");
    clear_btn.set_halign(gtk::Align::Start);
    btn_box.append(&add_files);
    btn_box.append(&add_folder);
    btn_box.append(&clear_btn);
    input_content.add(&btn_box);

    let file_list = gtk::ListBox::new();
    file_list.add_css_class("boxed-list");
    // 单击选中（高亮）后，下方配置针对选中项；再次单击/移除后回到全局默认
    file_list.set_selection_mode(gtk::SelectionMode::Single);
    file_list.set_margin_top(8);
    input_content.add(&file_list);

    // 选中列表项时才出现的「该项覆盖命名」框（默认隐藏，语义与全局框分离）。
    // 注意：PreferencesGroup 对 PreferencesRow 类子控件与普通 widget 分段排布，
    // EntryRow 直接 add 会被提到所有普通控件（拖拽区/按钮/列表）之前，
    // 故用 Box 包裹让它按 add 顺序落在列表之后。
    let name_override_row = entry_row("该项输出名（留空 = 跟随全局）", "");
    name_override_row.set_visible(false);
    let override_wrap = gtk::Box::new(gtk::Orientation::Vertical, 0);
    override_wrap.append(&name_override_row);
    input_content.add(&override_wrap);

    // 列表下方提示条：恒一行文案
    let selection_hint = gtk::Label::new(None);
    selection_hint.set_halign(gtk::Align::Start);
    selection_hint.set_xalign(0.0);
    selection_hint.set_wrap(true);
    selection_hint.set_visible(false);
    selection_hint.add_css_class("dim-label");
    let override_clear = gtk::Button::with_label("跟随全局");
    override_clear.set_tooltip_text(Some("清除选中项的单独命名，输出名恢复为下方默认"));
    override_clear.add_css_class("flat");
    override_clear.set_visible(false);
    let hint_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    hint_box.set_margin_top(4);
    hint_box.append(&selection_hint);
    let hint_spacer = gtk::Label::new(None);
    hint_spacer.set_hexpand(true);
    hint_box.append(&hint_spacer);
    hint_box.append(&override_clear);
    input_content.add(&hint_box);

    // 单击列表行 → 高亮选中，展开该项覆盖命名区（6.6）
    file_list.connect_row_selected(|_, row| g_on_row_selected(row.cloned()));
    override_clear.connect_clicked(|_| g_override_clear());
    // 覆盖框编辑 → 写入选中项的 name_override（留空 = 删除覆盖）
    name_override_row.connect_notify(Some("text"), move |row, _| {
        with_inner(|i| i.on_override_text_changed(row.text().as_str()));
    });

    // ---------- 信号接线 ----------
    // 添加文件 / 文件夹
    add_files.connect_clicked(|_| g_pick_files(false));
    add_folder.connect_clicked(|_| g_pick_files(true));
    clear_btn.connect_clicked(|_| g_clear_inputs());
    InputSection {
        file_list,
        inputs: RefCell::new(Vec::new()),
        next_id: Cell::new(0),
        name_override_row,
        selection_hint,
        override_clear,
    }
}

/// 分区②：作业模式 / 输出（模式、分类、格式、目录、命名）。
fn build_mode(root_box: &gtk::Box) -> OutputSection {
    let (mode_card, mode_content) = card("作业模式 / 输出");
    root_box.append(&mode_card);

    let mode_row = combo_row("作业模式", MODES, 0);
    mode_content.add(&mode_row);
    let category_row = combo_row("输出大类", CATEGORIES, 0);
    mode_content.add(&category_row);
    let format_row = combo_row("封装格式", &video_format_labels(), 0);
    mode_content.add(&format_row);

    let dir_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let out_dir_row = entry_row("输出目录", "/path/to/output（留空为当前目录）");
    out_dir_row.set_hexpand(true);
    let choose_dir = gtk::Button::from_icon_name("folder-open-symbolic");
    choose_dir.set_tooltip_text(Some("选择输出目录"));
    dir_box.append(&out_dir_row);
    dir_box.append(&choose_dir);
    mode_content.add(&dir_box);

    let out_name_row = entry_row("全局输出文件名（留空 = 按各文件名推导）", "output");
    mode_content.add(&out_name_row);

    // ---------- 信号接线 ----------
    // 绑定属性变化 → 刷新预览
    watch_all!(&[&mode_row, &category_row, &format_row, &out_dir_row]);
    // 全局输出文件名（6.6 起恒为全局语义，单项覆盖在输入区的独立框编辑）
    out_name_row.connect_notify(Some("text"), move |row, _| {
        with_inner(|i| i.on_name_text_changed(row.text().as_str()));
    });
    // 分类/模式联动改变卡片可见性
    mode_row.connect_selected_notify(|row| g_on_mode_change(row.selected()));
    category_row.connect_selected_notify(|_| g_on_category_change());
    choose_dir.connect_clicked(|_| g_pick_output_dir());
    OutputSection {
        mode_row,
        category_row,
        format_row,
        out_dir_row,
        out_name_row,
        default_name: RefCell::new(String::new()),
        loading_name: Cell::new(false),
        last_manual_category: Cell::new(None),
    }
}

/// 分区③：视频参数。
fn build_video(root_box: &gtk::Box) -> VideoSection {
    let (video_card, v_content) = card("视频参数");
    root_box.append(&video_card);
    let v_codec = combo_row("编码器", V_CODECS, 0);
    v_content.add(&v_codec);
    let v_bitrate_mode = combo_row("码率控制", BITRATE_MODES, 0);
    v_content.add(&v_bitrate_mode);
    let v_crf = spin_row("CRF（质量，越小越好）", 0.0, 51.0, 1.0, 0, 23.0);
    v_content.add(&v_crf);
    let v_bitrate = spin_row("码率 (kbps)", 100.0, 100000.0, 100.0, 0, 4000.0);
    v_content.add(&v_bitrate);

    let res_row = combo_row("分辨率", RES_PRESETS, 0);
    v_content.add(&res_row);
    let custom_res = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let v_w = spin_row("宽", 0.0, 7680.0, 2.0, 0, 0.0);
    let v_h = spin_row("高", 0.0, 4320.0, 2.0, 0, 0.0);
    custom_res.append(&v_w);
    custom_res.append(&v_h);
    v_content.add(&custom_res);
    let v_keep_aspect = switch_row("保持宽高比", "自定义分辨率时另一维自动按比例");
    v_content.add(&v_keep_aspect);

    let v_fps = combo_row("帧率", FPS_PRESETS, 0);
    v_content.add(&v_fps);
    let v_fps_custom = spin_row("自定义帧率", 1.0, 240.0, 1.0, 0, 30.0);
    v_content.add(&v_fps_custom);
    let v_scale_algo = combo_row("缩放算法", SCALE_ALGOS, 1);
    v_content.add(&v_scale_algo);
    let v_colorspace = combo_row("色彩空间", COLORSPACES, 0);
    v_content.add(&v_colorspace);
    let v_color_range = combo_row("色彩范围", COLOR_RANGES, 0);
    v_content.add(&v_color_range);
    let v_hdr = switch_row("HDR 透传", "关闭时若开启色调映射则做 HDR→SDR");
    v_content.add(&v_hdr);

    // ---------- 信号接线 ----------
    watch_all!(&[
        &v_codec,
        &v_bitrate_mode,
        &v_crf,
        &v_bitrate,
        &res_row,
        &v_w,
        &v_h,
        &v_keep_aspect,
        &v_fps,
        &v_fps_custom,
        &v_scale_algo,
        &v_colorspace,
        &v_color_range,
        &v_hdr,
    ]);
    VideoSection {
        video_card,
        v_codec,
        v_bitrate_mode,
        v_crf,
        v_bitrate,
        v_res: res_row,
        v_w,
        v_h,
        v_keep_aspect,
        v_fps,
        v_fps_custom,
        v_scale_algo,
        v_colorspace,
        v_color_range,
        v_hdr,
    }
}

/// 分区④：音频参数。
fn build_audio(root_box: &gtk::Box) -> AudioSection {
    let (audio_card, a_content) = card("音频参数");
    root_box.append(&audio_card);
    let a_codec = combo_row("编码器", A_CODECS, 0);
    a_content.add(&a_codec);
    let a_channels = combo_row("声道", CHANNELS, 0);
    a_content.add(&a_channels);
    let a_sr = combo_row("采样率", SAMPLE_RATES, 0);
    a_content.add(&a_sr);
    let a_bitrate = spin_row("码率 (kbps)", 8.0, 640.0, 8.0, 0, 192.0);
    a_content.add(&a_bitrate);
    let a_gain = spin_row("音量增益 (dB)", -60.0, 60.0, 0.5, 1, 0.0);
    a_content.add(&a_gain);
    let a_fade_in = spin_row("淡入 (秒)", 0.0, 60.0, 0.5, 1, 0.0);
    a_content.add(&a_fade_in);
    let a_fade_out = spin_row("淡出 (秒)", 0.0, 60.0, 0.5, 1, 0.0);
    a_content.add(&a_fade_out);

    // ---------- 信号接线 ----------
    watch_all!(&[
        &a_codec,
        &a_channels,
        &a_sr,
        &a_bitrate,
        &a_gain,
        &a_fade_in,
        &a_fade_out,
    ]);
    AudioSection {
        audio_card,
        a_codec,
        a_channels,
        a_sr,
        a_bitrate,
        a_gain,
        a_fade_in,
        a_fade_out,
    }
}

/// 分区⑤：图片参数。
fn build_image(root_box: &gtk::Box) -> ImageSection {
    let (image_card, i_content) = card("图片参数");
    root_box.append(&image_card);
    let i_quality = spin_row("质量 (有损 JPG/WebP/AVIF, 1–100)", 1.0, 100.0, 1.0, 0, 90.0);
    i_content.add(&i_quality);
    let i_compression = spin_row("PNG 压缩级别 (1–9)", 1.0, 9.0, 1.0, 0, 6.0);
    i_content.add(&i_compression);
    let i_strip = switch_row("剥离元数据", "去除 EXIF / ICC Profile");
    i_content.add(&i_strip);
    let i_longest = spin_row("最长边约束 (0=不限制)", 0.0, 10000.0, 10.0, 0, 0.0);
    i_content.add(&i_longest);
    let i_percent = spin_row("百分比缩放 (%)", 1.0, 400.0, 1.0, 0, 100.0);
    i_content.add(&i_percent);
    let i_extract_fps = spin_row("抽取帧率 (视频→图片)", 0.1, 120.0, 0.1, 1, 1.0);
    i_content.add(&i_extract_fps);
    let i_gif_fps = spin_row("GIF 帧率", 1.0, 50.0, 1.0, 0, 15.0);
    i_content.add(&i_gif_fps);
    let i_gif_w = spin_row("GIF 宽度 (0=源)", 0.0, 2000.0, 10.0, 0, 480.0);
    i_content.add(&i_gif_w);

    // ---------- 信号接线 ----------
    watch_all!(&[
        &i_quality,
        &i_compression,
        &i_strip,
        &i_longest,
        &i_percent,
        &i_extract_fps,
        &i_gif_fps,
        &i_gif_w,
    ]);
    ImageSection {
        image_card,
        i_quality,
        i_compression,
        i_strip,
        i_longest,
        i_percent,
        i_extract_fps,
        i_gif_fps,
        i_gif_w,
    }
}

/// 分区⑥：片段截取。
fn build_clip(root_box: &gtk::Box) -> ClipSection {
    let (clip_card, clip_content) = card("片段截取");
    root_box.append(&clip_card);
    let clip_enabled = switch_row("启用 -ss / -to 截取", "");
    clip_content.add(&clip_enabled);
    let clip_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let clip_start = entry_row("起始时间", "12.5 或 00:00:12");
    let clip_end = entry_row("结束时间", "00:01:00");
    clip_box.append(&clip_start);
    clip_box.append(&clip_end);
    clip_content.add(&clip_box);

    // ---------- 信号接线 ----------
    watch_all!(&[&clip_enabled, &clip_start, &clip_end]);
    ClipSection {
        clip_card,
        clip_enabled,
        clip_start,
        clip_end,
    }
}

/// 分区⑦：高级选项（画面处理 / 编码器高级 / 滤镜链三个折叠组）。
fn build_advanced(root_box: &gtk::Box) -> AdvancedSection {
    let (adv_card, adv_content) = card("高级选项");
    root_box.append(&adv_card);
    // 三个分组折叠面板（6.3：替代 CheckButton + Revealer 手搓折叠）
    let picture_group = adw::ExpanderRow::new();
    picture_group.set_title("画面处理");
    picture_group.set_subtitle("裁剪、填充、旋转、去隔行、去噪、锐化、水印、色调映射");
    adv_content.add(&picture_group);

    let crop_row = switch_row("裁剪 (crop)", "");
    picture_group.add_row(&crop_row);
    // 宽/高 与 X/Y 分成两行：4 个 SpinRow 横排会把最小宽度撑到 678px
    let crop_dims = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let crop_dims2 = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let adv_crop_w = spin_row("宽", 0.0, 7680.0, 2.0, 0, 1280.0);
    let adv_crop_h = spin_row("高", 0.0, 4320.0, 2.0, 0, 720.0);
    let adv_crop_x = spin_row("X", 0.0, 7680.0, 2.0, 0, 0.0);
    let adv_crop_y = spin_row("Y", 0.0, 4320.0, 2.0, 0, 0.0);
    crop_dims.append(&adv_crop_w);
    crop_dims.append(&adv_crop_h);
    crop_dims2.append(&adv_crop_x);
    crop_dims2.append(&adv_crop_y);
    picture_group.add_row(&crop_dims);
    picture_group.add_row(&crop_dims2);

    let pad_row = switch_row("填充 (pad)", "");
    picture_group.add_row(&pad_row);
    let pad_dims = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let adv_pad_w = spin_row("宽", 0.0, 7680.0, 2.0, 0, 1920.0);
    let adv_pad_h = spin_row("高", 0.0, 4320.0, 2.0, 0, 1080.0);
    pad_dims.append(&adv_pad_w);
    pad_dims.append(&adv_pad_h);
    picture_group.add_row(&pad_dims);
    let adv_pad_color = entry_row("填充颜色", "black");
    picture_group.add_row(&adv_pad_color);

    let adv_rotate = combo_row("旋转 / 翻转", ROTATES, 0);
    picture_group.add_row(&adv_rotate);
    let adv_deinterlace = switch_row("去隔行 (yadif)", "");
    picture_group.add_row(&adv_deinterlace);
    let adv_denoise = switch_row("去噪 (hqdn3d)", "");
    picture_group.add_row(&adv_denoise);
    let adv_sharpen = switch_row("锐化 (unsharp)", "");
    picture_group.add_row(&adv_sharpen);

    // 水印
    let wm_row = switch_row("水印 (overlay)", "");
    picture_group.add_row(&wm_row);
    let wm_path = entry_row("水印图片路径", "/path/to/watermark.png");
    picture_group.add_row(&wm_path);
    let wm_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let adv_wm_pos = combo_row("位置", WM_POSITIONS, 3);
    let adv_wm_op = spin_row("不透明度", 0.0, 1.0, 0.05, 2, 1.0);
    wm_box.append(&adv_wm_pos);
    wm_box.append(&adv_wm_op);
    picture_group.add_row(&wm_box);

    let adv_tonemap = switch_row("HDR→SDR 色调映射", "");
    picture_group.add_row(&adv_tonemap);

    // ---------- 分组二：编码器高级 ----------
    let encoder_group = adw::ExpanderRow::new();
    encoder_group.set_title("编码器高级");
    encoder_group
        .set_subtitle("preset / tune / profile / level / pix_fmt、faststart、2-Pass、线程");
    adv_content.add(&encoder_group);

    let adv_preset = entry_row("preset", "ultrafast / medium / veryslow …");
    encoder_group.add_row(&adv_preset);
    let adv_tune = entry_row("tune", "film / animation …");
    encoder_group.add_row(&adv_tune);
    let adv_profile = entry_row("profile", "high / main …");
    encoder_group.add_row(&adv_profile);
    let adv_level = entry_row("level", "4.0 …");
    encoder_group.add_row(&adv_level);
    let adv_pix_fmt = entry_row("pix_fmt", "yuv420p …");
    encoder_group.add_row(&adv_pix_fmt);

    let adv_faststart = switch_row("MP4 faststart", "Web 流式播放优化");
    encoder_group.add_row(&adv_faststart);
    let adv_two_pass = switch_row("2-Pass 编码", "勾选后自动跑两遍（硬件编码会自动回退软件）");
    encoder_group.add_row(&adv_two_pass);
    let adv_threads = spin_row("线程数 (0=自动)", 0.0, 64.0, 1.0, 0, 0.0);
    encoder_group.add_row(&adv_threads);

    // ---------- 分组三：滤镜链 ----------
    let filter_group = adw::ExpanderRow::new();
    filter_group.set_title("滤镜链");
    filter_group.set_subtitle("音频降噪开关 + 视频 vf / 音频 af 滤镜卡片");
    adv_content.add(&filter_group);

    let adv_audio_denoise = switch_row("音频降噪 (afftdn)", "");
    filter_group.add_row(&adv_audio_denoise);

    let vf_label = gtk::Label::new(Some("视频滤镜链 (vf) — 拖拽排序的滤镜卡片"));
    vf_label.add_css_class("title-4");
    vf_label.set_halign(gtk::Align::Start);
    filter_group.add_row(&vf_label);
    let vf_list = gtk::ListBox::new();
    vf_list.add_css_class("boxed-list");
    vf_list.set_selection_mode(gtk::SelectionMode::None);
    filter_group.add_row(&vf_list);
    let add_vf = gtk::Button::with_label("添加视频滤镜");
    add_vf.set_halign(gtk::Align::Start);
    filter_group.add_row(&add_vf);

    let af_label = gtk::Label::new(Some("音频滤镜链 (af)"));
    af_label.add_css_class("title-4");
    af_label.set_halign(gtk::Align::Start);
    filter_group.add_row(&af_label);
    let af_list = gtk::ListBox::new();
    af_list.add_css_class("boxed-list");
    af_list.set_selection_mode(gtk::SelectionMode::None);
    filter_group.add_row(&af_list);
    let add_af = gtk::Button::with_label("添加音频滤镜");
    add_af.set_halign(gtk::Align::Start);
    filter_group.add_row(&add_af);

    // ---------- 信号接线 ----------
    watch_all!(&[
        &crop_row,
        &adv_crop_w,
        &adv_crop_h,
        &adv_crop_x,
        &adv_crop_y,
        &pad_row,
        &adv_pad_w,
        &adv_pad_h,
        &adv_pad_color,
        &adv_rotate,
        &adv_deinterlace,
        &adv_denoise,
        &adv_sharpen,
        &wm_row,
        &wm_path,
        &adv_wm_pos,
        &adv_wm_op,
        &adv_audio_denoise,
        &adv_preset,
        &adv_tune,
        &adv_profile,
        &adv_level,
        &adv_pix_fmt,
        &adv_faststart,
        &adv_two_pass,
        &adv_threads,
        &adv_tonemap,
    ]);
    // 滤镜链编辑器
    add_vf.connect_clicked(|_| g_add_filter(true));
    add_af.connect_clicked(|_| g_add_filter(false));
    AdvancedSection {
        adv_card,
        adv_crop_en: crop_row,
        adv_crop_w,
        adv_crop_h,
        adv_crop_x,
        adv_crop_y,
        adv_pad_en: pad_row,
        adv_pad_w,
        adv_pad_h,
        adv_pad_color,
        adv_rotate,
        adv_deinterlace,
        adv_denoise,
        adv_sharpen,
        adv_wm_en: wm_row,
        adv_wm_path: wm_path,
        adv_wm_pos,
        adv_wm_op,
        adv_audio_denoise,
        adv_preset,
        adv_tune,
        adv_profile,
        adv_level,
        adv_pix_fmt,
        adv_faststart,
        adv_two_pass,
        adv_threads,
        adv_tonemap,
        vf_list,
        af_list,
    }
}

/// 分区⑦b：硬件加速（6.5 从高级选项提级为独立卡片）。
///
/// 下拉选项**永久固定**为 ffmpeg 全量后端列表（见 `ALL_HW`），不随探测结果增删；
/// 探测结果持久化到磁盘缓存，仅由「刷新」按钮手动触发重探并覆盖。
fn build_hw(root_box: &gtk::Box) -> HwSection {
    let (hw_card, hw_content) = card("硬件加速");
    root_box.append(&hw_card);

    let hw_labels: Vec<&str> = ALL_HW.iter().map(|p| p.label()).collect();
    let hw_row = combo_row("硬件加速", &hw_labels, 0);
    hw_content.add(&hw_row);

    let hw_status_row = adw::ActionRow::builder()
        .title("硬件加速能力")
        .subtitle("尚未检测")
        .activatable(false)
        .build();
    hw_status_row.set_subtitle_selectable(true);
    let hw_refresh = gtk::Button::with_label("刷新");
    hw_refresh.set_valign(gtk::Align::Center);
    hw_refresh.set_tooltip_text(Some("重新检测本机的 ffmpeg 硬件加速能力并覆盖缓存"));
    hw_status_row.add_suffix(&hw_refresh);
    hw_content.add(&hw_status_row);

    // ---------- 信号接线 ----------
    watch_all!(&[&hw_row]);
    // 手动重新探测硬件加速（结果会覆盖磁盘缓存，长期沿用）
    hw_refresh.connect_clicked(|_| g_hw_refresh());

    HwSection {
        hw_card,
        hw_row,
        hw_status_row,
        hw_refresh,
        hw_detected_at: Cell::new(0),
        hw_caps: RefCell::new(HwCapabilities::default()),
    }
}

/// 分区⑧：自定义参数注入（全局 / 输入 / 输出三段）。
fn build_custom(root_box: &gtk::Box) -> CustomSection {
    // ---------- 自定义参数 (C3) ----------
    let (custom_card, custom_content) = card("自定义参数注入");
    root_box.append(&custom_card);
    let custom_global = text_view("ffmpeg 之后的全局参数，如 -hide_banner -stats");
    let custom_input = text_view("每个 -i 之前的输入参数");
    let custom_output = text_view("输出文件之前的输出参数（图形选项会覆盖重复项）");
    custom_content.add(&labeled("全局参数", &custom_global));
    custom_content.add(&labeled("输入参数", &custom_input));
    custom_content.add(&labeled("输出参数", &custom_output));

    // ---------- 信号接线 ----------
    custom_global
        .buffer()
        .connect_changed(|_| g_update_preview());
    custom_input
        .buffer()
        .connect_changed(|_| g_update_preview());
    custom_output
        .buffer()
        .connect_changed(|_| g_update_preview());
    CustomSection {
        custom_card,
        custom_global,
        custom_input,
        custom_output,
    }
}

/// 分区⑨：命令预览。状态标签 / 进度条 / 开始转换按钮只创建不入卡，
/// 由 `build()` 放进滚动区外的固定操作条（6.2）。
fn build_preview(root_box: &gtk::Box) -> PreviewSection {
    // ---------- 命令预览 ----------
    let (preview_card, preview_content) = card("命令预览");
    root_box.append(&preview_card);
    let preview_text = gtk::TextView::new();
    preview_text.set_monospace(true);
    preview_text.set_editable(false);
    preview_text.set_wrap_mode(gtk::WrapMode::WordChar);
    let preview_scroll = gtk::ScrolledWindow::new();
    preview_scroll.set_child(Some(&preview_text));
    preview_scroll.set_min_content_height(120);
    preview_scroll.set_vexpand(true);
    preview_content.add(&preview_scroll);

    let preview_btn_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let copy_cmd = gtk::Button::with_label("复制命令");
    let save_script = gtk::Button::with_label("保存为 Shell 脚本");
    let refresh_cmd = gtk::Button::with_label("刷新预览");
    preview_btn_box.append(&refresh_cmd);
    preview_btn_box.append(&copy_cmd);
    preview_btn_box.append(&save_script);
    preview_content.add(&preview_btn_box);

    let status_label = gtk::Label::new(Some("尚未生成命令"));
    status_label.add_css_class("dim-label");
    status_label.set_halign(gtk::Align::Start);

    let progress = gtk::ProgressBar::new();
    progress.set_show_text(true);
    progress.set_visible(false);

    // 开始转换按钮本体：由固定操作条承载（6.2），此处只创建与接线。
    let run_button = gtk::Button::with_label("开始转换");
    run_button.add_css_class("suggested-action");

    // ---------- 信号接线 ----------
    // 预览 / 复制 / 保存脚本
    refresh_cmd.connect_clicked(|_| g_update_preview());
    copy_cmd.connect_clicked(|_| g_copy());
    save_script.connect_clicked(|_| g_save());
    // 开始转换
    run_button.connect_clicked(|_| g_run());
    PreviewSection {
        preview_text,
        status_label,
        progress,
        run_button,
    }
}

/// 视频格式标签（与 `OutputCategory::Video.formats()` 顺序一致）。
fn video_format_labels() -> Vec<&'static str> {
    OutputCategory::Video
        .formats()
        .iter()
        .map(|f| f.label())
        .collect()
}
fn image_format_labels() -> Vec<&'static str> {
    OutputCategory::Image
        .formats()
        .iter()
        .map(|f| f.label())
        .collect()
}
/// 根据分类生成对应封装格式标签。
fn format_labels_for(cat: OutputCategory) -> Vec<&'static str> {
    cat.formats().iter().map(|f| f.label()).collect()
}

/// 根据当前分类刷新 `format_row` 的可选项（仅在分类变化时调用，避免循环触发）。
fn refresh_format_options(inner: &Inner) {
    let cat = category_from_index(inner.output.category_row.selected());
    let labels = format_labels_for(cat);
    let model = gtk::StringList::new(&labels);
    inner.output.format_row.set_model(Some(&model));
    inner.output.format_row.set_selected(0);
}

/// 文本视图 + 标题的小封装。
fn text_view(_placeholder: &str) -> gtk::TextView {
    let tv = gtk::TextView::new();
    tv.set_monospace(true);
    tv.set_wrap_mode(gtk::WrapMode::WordChar);
    tv
}

fn labeled(title: &str, tv: &gtk::TextView) -> gtk::Box {
    let box_ = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let l = gtk::Label::new(Some(title));
    l.add_css_class("dim-label");
    l.set_halign(gtk::Align::Start);
    box_.append(&l);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_child(Some(tv));
    scroll.set_min_content_height(48);
    scroll.set_max_content_height(120);
    box_.append(&scroll);
    box_
}

// ---------- 全局句柄 + 自由函数（signal 回调要求 Send，不捕获 Rc<Inner>） ----------

thread_local! {
    /// 当前页面的强引用，供 signal 回调（drop / 点击等）经此访问页面实例。
    /// 用 `Option<Rc<Inner>>` 而非 `Weak`：否则 `build()` 返回后 `inner` 被销毁，
    /// `Weak` 升级永远失败，导致所有交互回调静默失效（文件选择、下拉联动等都不工作）。
    static INNER: std::cell::RefCell<Option<Rc<Inner>>> = const { std::cell::RefCell::new(None) };
}

/// 应用退出前调用：清空全局句柄，避免窗口销毁期的 GTK 回调访问正在析构的 TLS。
pub fn shutdown() {
    INNER.with(|i| {
        if let Ok(mut b) = i.try_borrow_mut() {
            *b = None;
        }
    });
}

/// 在闭包无法安全捕获 `Rc<Inner>`（signal 回调要求 `Send`）时，从全局句柄读取页面实例。
fn with_inner<F: FnOnce(&Inner)>(f: F) {
    // try_borrow：窗口销毁期控件 notify 会重入本函数（INNER 正被 shutdown
    // 的可变借用持有），此时静默跳过而不是 RefCell 重入 panic。
    let Some(inner) = INNER.with(|i| i.try_borrow().ok().and_then(|b| b.clone())) else {
        return;
    };
    f(&inner);
}

/// 把 gio::File 列表（ListModel）解析为路径字符串列表。
fn files_to_paths(list: &gtk::gio::ListModel) -> Vec<String> {
    let n = list.n_items();
    (0..n)
        .filter_map(|i| {
            list.item(i)
                .and_then(|o| o.downcast::<gtk::gio::File>().ok())
                .and_then(|f| f.path())
        })
        .map(|p| p.to_string_lossy().to_string())
        .collect()
}

// ---------- 全局回调（由 signal 闭包（无捕获）转发） ----------

fn g_update_preview() {
    with_inner(|i| i.update_preview());
}

fn g_on_mode_change(_idx: u32) {
    with_inner(|i| {
        i.update_visibility();
        i.update_preview();
    });
}

/// 列表行选中变化：进入「为选中项单独命名」模式（未选中则回到全局默认）。
fn g_on_row_selected(_row: Option<gtk::ListBoxRow>) {
    with_inner(|i| i.on_row_selected(_row));
}

/// 「跟随全局」按钮：清除选中项的覆盖名。
fn g_override_clear() {
    with_inner(|i| i.on_override_clear());
}

fn g_on_category_change() {
    with_inner(|i| {
        refresh_format_options(i);
        i.update_visibility();
        i.update_preview();
    });
}

fn g_pick_files(folders: bool) {
    if let Some(inner) = INNER.with(|i| i.borrow().clone()) {
        pick_files(inner, folders);
    }
}

fn g_pick_output_dir() {
    if let Some(inner) = INNER.with(|i| i.borrow().clone()) {
        pick_output_dir(inner);
    }
}

fn g_clear_inputs() {
    with_inner(|i| i.clear_inputs());
}

fn g_add_filter(is_video: bool) {
    with_inner(|i| i.add_filter_card(is_video));
}

fn g_drop_paths(paths: &[String]) {
    with_inner(|i| i.add_paths(paths));
}

fn g_reorder(id: u64, delta: i32) {
    with_inner(|i| i.reorder(id, delta));
}

fn g_remove(id: u64) {
    with_inner(|i| i.remove_input(id));
}

fn g_copy() {
    with_inner(|i| i.copy_command());
}

fn g_save() {
    with_inner(|i| i.save_script());
}

fn g_run() {
    with_inner(|i| i.run());
}

/// 手动重新探测硬件加速能力（覆盖永久缓存）。
fn g_hw_refresh() {
    with_inner(|i| {
        i.hw.hw_refresh.set_sensitive(false);
        i.hw.hw_status_row.set_subtitle("正在检测…");
        spawn_hw_detect(true);
    });
}

// ---------- 文件选择（GIO 异步回调，可捕获 Rc<Inner>） ----------

fn pick_files(inner: Rc<Inner>, folders: bool) {
    let win = inner
        .input
        .file_list
        .root()
        .and_then(|r| r.downcast::<gtk::Window>().ok());
    let action = if folders {
        gtk::FileChooserAction::SelectFolder
    } else {
        gtk::FileChooserAction::Open
    };
    let dialog = gtk::FileChooserDialog::builder()
        .title(if folders {
            "选择文件夹"
        } else {
            "选择文件"
        })
        .action(action)
        .modal(true)
        .build();
    if let Some(w) = &win {
        dialog.set_transient_for(Some(w));
    }
    dialog.add_button("取消", gtk::ResponseType::Cancel);
    dialog.add_button(
        if folders { "选择" } else { "打开" },
        gtk::ResponseType::Accept,
    );
    dialog.set_select_multiple(true);
    let inner = Rc::clone(&inner);
    dialog.connect_response(move |d, resp| {
        if resp == gtk::ResponseType::Accept {
            let paths = files_to_paths(&d.files());
            if !paths.is_empty() {
                inner.add_paths(&paths);
            }
        }
        d.destroy();
    });
    dialog.show();
}

fn pick_output_dir(inner: Rc<Inner>) {
    let win = inner
        .input
        .file_list
        .root()
        .and_then(|r| r.downcast::<gtk::Window>().ok());
    let dialog = gtk::FileChooserDialog::builder()
        .title("选择输出目录")
        .action(gtk::FileChooserAction::SelectFolder)
        .modal(true)
        .build();
    if let Some(w) = &win {
        dialog.set_transient_for(Some(w));
    }
    dialog.add_button("取消", gtk::ResponseType::Cancel);
    dialog.add_button("选择", gtk::ResponseType::Accept);
    let inner = Rc::clone(&inner);
    dialog.connect_response(move |d, resp| {
        if resp == gtk::ResponseType::Accept
            && let Some(file) = d.file()
            && let Some(p) = file.path()
        {
            inner.output.out_dir_row.set_text(&p.to_string_lossy());
            inner.update_preview();
        }
        d.destroy();
    });
    dialog.show();
}

/// 后台探测硬件加速能力。
///
/// `announce` 为 true 时（用户点「刷新」）探测完成后弹 toast 汇报结果。
/// 探测结果会**永久写入磁盘缓存**（`hwaccel::save_cache`），之后启动直接复用，
/// 不再重复探测 —— 机器硬件不会频繁变动。
fn spawn_hw_detect(announce: bool) {
    std::thread::spawn(move || {
        // 探测 + 落盘都放在工作线程，避免阻塞主循环
        let cache = hwaccel::save_cache(&hwaccel::detect());
        // 用 Cell 包裹，避免把非 Copy 的缓存值在 FnMut 闭包里 move 出去
        let slot = std::cell::Cell::new(Some(cache));
        glib::source::idle_add(move || {
            if let Some(c) = slot.take()
                && let Some(inner) = INNER.with(|i| i.borrow().clone())
            {
                let summary = c.caps.summary();
                inner.apply_hw_cache(&c);
                if announce {
                    inner.toast(&format!("已重新检测：{summary}"));
                }
            }
            glib::ControlFlow::Break
        });
    });
}

// ---------- 枚举 ↔ 索引 映射 ----------

fn mode_from_index(i: u32) -> JobMode {
    match i {
        1 => JobMode::Concat,
        2 => JobMode::Split,
        3 => JobMode::ImageExtract,
        4 => JobMode::ImageToVideo,
        5 => JobMode::VideoToGif,
        _ => JobMode::Single,
    }
}
fn category_from_index(i: u32) -> OutputCategory {
    match i {
        1 => OutputCategory::Audio,
        2 => OutputCategory::Image,
        _ => OutputCategory::Video,
    }
}
fn format_from_index(cat: OutputCategory, i: u32) -> ContainerFormat {
    let formats = cat.formats();
    formats.get(i as usize).copied().unwrap_or(formats[0])
}
fn vcodec_from_index(i: u32) -> VideoCodec {
    match i {
        1 => VideoCodec::Libx265,
        2 => VideoCodec::LibvpxVp9,
        3 => VideoCodec::LibaomAv1,
        4 => VideoCodec::Copy,
        _ => VideoCodec::Libx264,
    }
}
fn bitrate_mode_from_index(i: u32) -> BitrateMode {
    match i {
        1 => BitrateMode::Cbr,
        2 => BitrateMode::Vbr,
        3 => BitrateMode::Fixed,
        _ => BitrateMode::Crf,
    }
}
fn res_from_index(i: u32) -> ResolutionPreset {
    match i {
        1 => ResolutionPreset::R4k,
        2 => ResolutionPreset::R2k,
        3 => ResolutionPreset::R1080,
        4 => ResolutionPreset::R720,
        5 => ResolutionPreset::R480,
        6 => ResolutionPreset::Custom,
        _ => ResolutionPreset::Source,
    }
}
fn fps_from_index(i: u32) -> FpsPreset {
    match i {
        1 => FpsPreset::F24,
        2 => FpsPreset::F25,
        3 => FpsPreset::F30,
        4 => FpsPreset::F50,
        5 => FpsPreset::F60,
        6 => FpsPreset::Custom,
        _ => FpsPreset::Source,
    }
}
fn scale_algo_from_index(i: u32) -> ScaleAlgorithm {
    match i {
        0 => ScaleAlgorithm::Bilinear,
        2 => ScaleAlgorithm::Bicubic,
        3 => ScaleAlgorithm::Spline,
        _ => ScaleAlgorithm::Lanczos,
    }
}
fn colorspace_from_index(i: u32) -> ColorSpace {
    match i {
        1 => ColorSpace::Bt601,
        2 => ColorSpace::Bt2020,
        _ => ColorSpace::Bt709,
    }
}
fn color_range_from_index(i: u32) -> ColorRange {
    match i {
        1 => ColorRange::Pc,
        _ => ColorRange::Tv,
    }
}
fn acodec_from_index(i: u32) -> AudioCodec {
    match i {
        1 => AudioCodec::Mp3,
        2 => AudioCodec::Opus,
        3 => AudioCodec::Vorbis,
        4 => AudioCodec::Flac,
        5 => AudioCodec::Pcm,
        6 => AudioCodec::Copy,
        _ => AudioCodec::Aac,
    }
}
fn channels_from_index(i: u32) -> Channels {
    match i {
        1 => Channels::Mono,
        2 => Channels::Stereo,
        3 => Channels::Surround51,
        _ => Channels::Source,
    }
}
fn samplerate_from_index(i: u32) -> SampleRate {
    match i {
        1 => SampleRate::Rate44100,
        2 => SampleRate::Rate48000,
        3 => SampleRate::Rate96000,
        _ => SampleRate::Source,
    }
}
fn rotate_from_index(i: u32) -> RotateMode {
    match i {
        1 => RotateMode::Rotate90,
        2 => RotateMode::Rotate180,
        3 => RotateMode::Rotate270,
        4 => RotateMode::FlipH,
        5 => RotateMode::FlipV,
        _ => RotateMode::None,
    }
}
fn wm_pos_from_index(i: u32) -> WatermarkPos {
    match i {
        0 => WatermarkPos::TopLeft,
        1 => WatermarkPos::TopRight,
        2 => WatermarkPos::BottomLeft,
        3 => WatermarkPos::BottomRight,
        _ => WatermarkPos::Center,
    }
}
/// 硬件加速偏好的下拉索引（与 `ALL_HW` 顺序一致）。
fn hw_index_of(p: HwAccelPreference) -> u32 {
    ALL_HW.iter().position(|x| *x == p).unwrap_or(0) as u32
}

// ---------- 偏好持久化（跨会话记忆） ----------
// 偏好存于 `$XDG_CONFIG_HOME/linbox/prefs.json`（glib::user_config_dir），
// 每次预览刷新时写入，启动时恢复，使各选项在关闭重开后仍保留上次选择。

/// 偏好文件路径。
/// 显隐状态机的输入：当前控件值的只读快照（不接触 GTK，可离线测试）。
#[derive(Debug, Clone)]
pub(crate) struct LayoutInput {
    pub mode: JobMode,
    /// 输出大类下拉当前 index（图片类模式下可能被强制改写）。
    pub category_idx: u32,
    /// `last_manual_category` 的当前值：进入图片类模式前手动选的分类。
    pub saved_manual: Option<u32>,
    pub format_idx: u32,
    pub bitrate_mode: BitrateMode,
    pub codec: VideoCodec,
    pub res: ResolutionPreset,
    pub fps: FpsPreset,
    pub clip_enabled: bool,
    pub crop_enabled: bool,
    pub pad_enabled: bool,
    pub wm_enabled: bool,
}

/// 显隐状态机的输出：`update_visibility` 只负责逐项 apply，不再内联规则。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayoutState {
    /// 分类下拉是否被图片类模式锁定（不可编辑）。
    pub category_locked: bool,
    /// 分类下拉需要 set_selected 的目标（None = 不动）。
    pub category_set_to: Option<u32>,
    /// `last_manual_category` 的新值（进入图片模式时保存，退出时清空）。
    pub saved_manual_next: Option<u32>,
    pub format_locked: bool,
    pub format_set_to: Option<u32>,
    /// 卡片可见性。
    pub video_card: bool,
    pub audio_card: bool,
    pub image_card: bool,
    pub clip_card: bool,
    pub advanced_card: bool,
    pub custom_card: bool,
    /// 硬件加速卡（与 advanced_card 同消费条件：ImageExtract/VideoToGif 不读 spec.hw）。
    pub hw_card: bool,
    /// 行级可见性。
    pub crf_visible: bool,
    pub bitrate_visible: bool,
    pub custom_res: bool,
    pub fps_custom_visible: bool,
    /// 子控件敏感度（开关关闭时置灰）。
    pub clip_children: bool,
    pub crop_children: bool,
    pub pad_children: bool,
    pub wm_children: bool,
}

/// 分类的模式级覆盖（图片类模式由模式决定输出大类）。
pub(crate) fn effective_category_of(mode: JobMode, category_idx: u32) -> OutputCategory {
    match mode {
        JobMode::ImageExtract | JobMode::VideoToGif => OutputCategory::Image,
        JobMode::ImageToVideo => OutputCategory::Video,
        _ => category_from_index(category_idx),
    }
}

/// 显隐状态机（纯函数）：模式/分类/开关 → 卡片可见性 + 行显隐 + 分类/格式锁定。
///
/// 卡级显隐按 `utils::media::command` 的**实际消费矩阵**收口（逐函数体核实）：
/// - 片段截取：Single/Split/ImageExtract/VideoToGif 读 `spec.clip`；Concat/ImageToVideo 不读 →
///   后两者隐藏（修正「图片类模式全隐藏」的粗前提——ImageExtract/GIF 的 `-ss` 实际生效）。
/// - 高级选项：ImageExtract/VideoToGif 完全不读 `spec.advanced` → 隐藏；
///   ImageToVideo 经 `push_container_options` 读 faststart/threads → 保留。
/// - 自定义参数：VideoToGif 的双遍调色板命令不注入 custom → 隐藏；
///   ImageExtract/ImageToVideo 读 custom.global/output → 保留（同样修正粗前提）。
pub(crate) fn layout_state(i: &LayoutInput) -> LayoutState {
    let is_image_mode = matches!(
        i.mode,
        JobMode::ImageExtract | JobMode::ImageToVideo | JobMode::VideoToGif
    );

    // 分类/格式锁：图片类模式接管分类并记住原选择，退出时恢复。
    let (category_locked, category_set_to, saved_manual_next) = if is_image_mode {
        let want = match i.mode {
            JobMode::ImageToVideo => 0, // 视频
            _ => 2,                     // 图片
        };
        (
            true,
            (i.category_idx != want).then_some(want),
            Some(i.saved_manual.unwrap_or(i.category_idx)),
        )
    } else {
        (
            false,
            i.saved_manual.filter(|&saved| i.category_idx != saved),
            None,
        )
    };
    let gif_idx = image_format_labels()
        .iter()
        .position(|l| *l == "GIF")
        .unwrap_or(5) as u32;
    let format_locked = i.mode == JobMode::VideoToGif;
    let format_set_to = format_locked
        .then_some(gif_idx)
        .filter(|&g| i.format_idx != g);

    let final_category_idx = category_set_to.unwrap_or(i.category_idx);
    let cat = effective_category_of(i.mode, final_category_idx);

    let crf_mode = i.bitrate_mode == BitrateMode::Crf;
    let copy = i.codec == VideoCodec::Copy;

    LayoutState {
        category_locked,
        category_set_to,
        saved_manual_next,
        format_locked,
        format_set_to,
        video_card: cat == OutputCategory::Video,
        audio_card: cat == OutputCategory::Video || cat == OutputCategory::Audio,
        image_card: cat == OutputCategory::Image || is_image_mode,
        clip_card: matches!(
            i.mode,
            JobMode::Single | JobMode::Split | JobMode::ImageExtract | JobMode::VideoToGif
        ),
        advanced_card: !matches!(i.mode, JobMode::ImageExtract | JobMode::VideoToGif),
        custom_card: i.mode != JobMode::VideoToGif,
        hw_card: !matches!(i.mode, JobMode::ImageExtract | JobMode::VideoToGif),
        crf_visible: crf_mode && !copy,
        bitrate_visible: !crf_mode && !copy,
        custom_res: i.res == ResolutionPreset::Custom,
        fps_custom_visible: i.fps == FpsPreset::Custom,
        clip_children: i.clip_enabled,
        crop_children: i.crop_enabled,
        pad_children: i.pad_enabled,
        wm_children: i.wm_enabled,
    }
}

fn prefs_path() -> std::path::PathBuf {
    let mut p = glib::user_config_dir();
    p.push("linbox");
    p.push("prefs.json");
    p
}

/// 读取已保存的偏好（不存在 / 解析失败则返回 Null）。
fn load_prefs() -> serde_json::Value {
    std::fs::read_to_string(prefs_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// 把当前所有选项快照为 JSON。
fn snapshot(inner: &Inner) -> serde_json::Value {
    use serde_json::{Map, Value};
    let mut m: Map<String, Value> = Map::new();
    // 用闭包逐个插入，避免 `json!` 宏在字段过多时触发递归上限
    let ins_u = |m: &mut Map<String, Value>, k: &str, v: u32| {
        m.insert(k.to_string(), Value::from(v));
    };
    let ins_f = |m: &mut Map<String, Value>, k: &str, v: f64| {
        m.insert(k.to_string(), Value::from(v));
    };
    let ins_b = |m: &mut Map<String, Value>, k: &str, v: bool| {
        m.insert(k.to_string(), Value::from(v));
    };
    let ins_s = |m: &mut Map<String, Value>, k: &str, v: &str| {
        m.insert(k.to_string(), Value::from(v));
    };

    ins_u(&mut m, "mode", inner.output.mode_row.selected());
    ins_u(&mut m, "category", inner.output.category_row.selected());
    ins_u(&mut m, "format", inner.output.format_row.selected());
    ins_s(&mut m, "out_dir", &inner.output.out_dir_row.text());
    // 注意：out_name 不复盘 —— 全局默认名不再是持久配置：
    // 留空时每项按自身文件名推导（auto_output_stem），旧版本持久化的
    // 「首个文件 - output」会被误当成统一默认名继续污染所有 item。
    ins_u(&mut m, "v_codec", inner.video.v_codec.selected());
    ins_u(
        &mut m,
        "v_bitrate_mode",
        inner.video.v_bitrate_mode.selected(),
    );
    ins_f(&mut m, "v_crf", inner.video.v_crf.adjustment().value());
    ins_f(
        &mut m,
        "v_bitrate",
        inner.video.v_bitrate.adjustment().value(),
    );
    ins_u(&mut m, "v_res", inner.video.v_res.selected());
    ins_f(&mut m, "v_w", inner.video.v_w.adjustment().value());
    ins_f(&mut m, "v_h", inner.video.v_h.adjustment().value());
    ins_b(
        &mut m,
        "v_keep_aspect",
        inner.video.v_keep_aspect.is_active(),
    );
    ins_u(&mut m, "v_fps", inner.video.v_fps.selected());
    ins_f(
        &mut m,
        "v_fps_custom",
        inner.video.v_fps_custom.adjustment().value(),
    );
    ins_u(&mut m, "v_scale_algo", inner.video.v_scale_algo.selected());
    ins_u(&mut m, "v_colorspace", inner.video.v_colorspace.selected());
    ins_u(
        &mut m,
        "v_color_range",
        inner.video.v_color_range.selected(),
    );
    ins_b(&mut m, "v_hdr", inner.video.v_hdr.is_active());
    ins_u(&mut m, "a_codec", inner.audio.a_codec.selected());
    ins_u(&mut m, "a_channels", inner.audio.a_channels.selected());
    ins_u(&mut m, "a_sr", inner.audio.a_sr.selected());
    ins_f(
        &mut m,
        "a_bitrate",
        inner.audio.a_bitrate.adjustment().value(),
    );
    ins_f(&mut m, "a_gain", inner.audio.a_gain.adjustment().value());
    ins_f(
        &mut m,
        "a_fade_in",
        inner.audio.a_fade_in.adjustment().value(),
    );
    ins_f(
        &mut m,
        "a_fade_out",
        inner.audio.a_fade_out.adjustment().value(),
    );
    ins_f(
        &mut m,
        "i_quality",
        inner.image.i_quality.adjustment().value(),
    );
    ins_f(
        &mut m,
        "i_compression",
        inner.image.i_compression.adjustment().value(),
    );
    ins_b(&mut m, "i_strip", inner.image.i_strip.is_active());
    ins_f(
        &mut m,
        "i_longest",
        inner.image.i_longest.adjustment().value(),
    );
    ins_f(
        &mut m,
        "i_percent",
        inner.image.i_percent.adjustment().value(),
    );
    ins_f(
        &mut m,
        "i_extract_fps",
        inner.image.i_extract_fps.adjustment().value(),
    );
    ins_f(
        &mut m,
        "i_gif_fps",
        inner.image.i_gif_fps.adjustment().value(),
    );
    ins_f(&mut m, "i_gif_w", inner.image.i_gif_w.adjustment().value());
    ins_b(&mut m, "clip_enabled", inner.clip.clip_enabled.is_active());
    ins_s(&mut m, "clip_start", &inner.clip.clip_start.text());
    ins_s(&mut m, "clip_end", &inner.clip.clip_end.text());
    ins_b(
        &mut m,
        "adv_crop_en",
        inner.advanced.adv_crop_en.is_active(),
    );
    ins_f(
        &mut m,
        "adv_crop_w",
        inner.advanced.adv_crop_w.adjustment().value(),
    );
    ins_f(
        &mut m,
        "adv_crop_h",
        inner.advanced.adv_crop_h.adjustment().value(),
    );
    ins_f(
        &mut m,
        "adv_crop_x",
        inner.advanced.adv_crop_x.adjustment().value(),
    );
    ins_f(
        &mut m,
        "adv_crop_y",
        inner.advanced.adv_crop_y.adjustment().value(),
    );
    ins_b(&mut m, "adv_pad_en", inner.advanced.adv_pad_en.is_active());
    ins_f(
        &mut m,
        "adv_pad_w",
        inner.advanced.adv_pad_w.adjustment().value(),
    );
    ins_f(
        &mut m,
        "adv_pad_h",
        inner.advanced.adv_pad_h.adjustment().value(),
    );
    ins_s(
        &mut m,
        "adv_pad_color",
        &inner.advanced.adv_pad_color.text(),
    );
    ins_u(&mut m, "adv_rotate", inner.advanced.adv_rotate.selected());
    ins_b(
        &mut m,
        "adv_deinterlace",
        inner.advanced.adv_deinterlace.is_active(),
    );
    ins_b(
        &mut m,
        "adv_denoise",
        inner.advanced.adv_denoise.is_active(),
    );
    ins_b(
        &mut m,
        "adv_sharpen",
        inner.advanced.adv_sharpen.is_active(),
    );
    ins_b(&mut m, "adv_wm_en", inner.advanced.adv_wm_en.is_active());
    ins_s(&mut m, "adv_wm_path", &inner.advanced.adv_wm_path.text());
    ins_u(&mut m, "adv_wm_pos", inner.advanced.adv_wm_pos.selected());
    ins_f(
        &mut m,
        "adv_wm_op",
        inner.advanced.adv_wm_op.adjustment().value(),
    );
    ins_b(
        &mut m,
        "adv_audio_denoise",
        inner.advanced.adv_audio_denoise.is_active(),
    );
    ins_s(&mut m, "adv_preset", &inner.advanced.adv_preset.text());
    ins_s(&mut m, "adv_tune", &inner.advanced.adv_tune.text());
    ins_s(&mut m, "adv_profile", &inner.advanced.adv_profile.text());
    ins_s(&mut m, "adv_level", &inner.advanced.adv_level.text());
    ins_s(&mut m, "adv_pix_fmt", &inner.advanced.adv_pix_fmt.text());
    ins_b(
        &mut m,
        "adv_faststart",
        inner.advanced.adv_faststart.is_active(),
    );
    ins_b(
        &mut m,
        "adv_two_pass",
        inner.advanced.adv_two_pass.is_active(),
    );
    ins_f(
        &mut m,
        "adv_threads",
        inner.advanced.adv_threads.adjustment().value(),
    );
    ins_b(
        &mut m,
        "adv_tonemap",
        inner.advanced.adv_tonemap.is_active(),
    );
    ins_u(&mut m, "hw", inner.hw.hw_row.selected());
    ins_s(
        &mut m,
        "custom_global",
        &buffer_text(&inner.custom.custom_global.buffer()),
    );
    ins_s(
        &mut m,
        "custom_input",
        &buffer_text(&inner.custom.custom_input.buffer()),
    );
    ins_s(
        &mut m,
        "custom_output",
        &buffer_text(&inner.custom.custom_output.buffer()),
    );

    Value::Object(m)
}

/// 将已保存偏好恢复到各控件（缺字段则保持默认）。
fn apply_prefs(inner: &Inner, v: &serde_json::Value) {
    let get_u = |k: &str| v.get(k).and_then(|x| x.as_u64()).map(|n| n as u32);
    let get_f = |k: &str| v.get(k).and_then(|x| x.as_f64());
    let get_b = |k: &str| v.get(k).and_then(|x| x.as_bool());
    let get_s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string());

    // 先恢复分类再恢复模式：图片/GIF 类模式会接管分类下拉，
    // 先设分类可让「退出模式后恢复的分类」捕获到用户真实的偏好值。
    if let Some(n) = get_u("category") {
        inner.output.category_row.set_selected(n);
    }
    if let Some(n) = get_u("mode") {
        inner.output.mode_row.set_selected(n);
    }
    if let Some(s) = get_s("out_dir") {
        inner.output.out_dir_row.set_text(&s);
    }
    // out_name 偏好键已废弃（见 persist 注释）：不再恢复全局默认名，
    // 保持为空让每项按自身文件名自动命名；旧 prefs 里残留的
    // 「首个文件 - output」也不会再被当作统一默认名。
    if let Some(n) = get_u("v_codec") {
        inner.video.v_codec.set_selected(n);
    }
    if let Some(n) = get_u("v_bitrate_mode") {
        inner.video.v_bitrate_mode.set_selected(n);
    }
    if let Some(f) = get_f("v_crf") {
        inner.video.v_crf.adjustment().set_value(f);
    }
    if let Some(f) = get_f("v_bitrate") {
        inner.video.v_bitrate.adjustment().set_value(f);
    }
    if let Some(n) = get_u("v_res") {
        inner.video.v_res.set_selected(n);
    }
    if let Some(f) = get_f("v_w") {
        inner.video.v_w.adjustment().set_value(f);
    }
    if let Some(f) = get_f("v_h") {
        inner.video.v_h.adjustment().set_value(f);
    }
    if let Some(b) = get_b("v_keep_aspect") {
        inner.video.v_keep_aspect.set_active(b);
    }
    if let Some(n) = get_u("v_fps") {
        inner.video.v_fps.set_selected(n);
    }
    if let Some(f) = get_f("v_fps_custom") {
        inner.video.v_fps_custom.adjustment().set_value(f);
    }
    if let Some(n) = get_u("v_scale_algo") {
        inner.video.v_scale_algo.set_selected(n);
    }
    if let Some(n) = get_u("v_colorspace") {
        inner.video.v_colorspace.set_selected(n);
    }
    if let Some(n) = get_u("v_color_range") {
        inner.video.v_color_range.set_selected(n);
    }
    if let Some(b) = get_b("v_hdr") {
        inner.video.v_hdr.set_active(b);
    }
    if let Some(n) = get_u("a_codec") {
        inner.audio.a_codec.set_selected(n);
    }
    if let Some(n) = get_u("a_channels") {
        inner.audio.a_channels.set_selected(n);
    }
    if let Some(n) = get_u("a_sr") {
        inner.audio.a_sr.set_selected(n);
    }
    if let Some(f) = get_f("a_bitrate") {
        inner.audio.a_bitrate.adjustment().set_value(f);
    }
    if let Some(f) = get_f("a_gain") {
        inner.audio.a_gain.adjustment().set_value(f);
    }
    if let Some(f) = get_f("a_fade_in") {
        inner.audio.a_fade_in.adjustment().set_value(f);
    }
    if let Some(f) = get_f("a_fade_out") {
        inner.audio.a_fade_out.adjustment().set_value(f);
    }
    if let Some(f) = get_f("i_quality") {
        inner.image.i_quality.adjustment().set_value(f);
    }
    if let Some(f) = get_f("i_compression") {
        inner.image.i_compression.adjustment().set_value(f);
    }
    if let Some(b) = get_b("i_strip") {
        inner.image.i_strip.set_active(b);
    }
    if let Some(f) = get_f("i_longest") {
        inner.image.i_longest.adjustment().set_value(f);
    }
    if let Some(f) = get_f("i_percent") {
        inner.image.i_percent.adjustment().set_value(f);
    }
    if let Some(f) = get_f("i_extract_fps") {
        inner.image.i_extract_fps.adjustment().set_value(f);
    }
    if let Some(f) = get_f("i_gif_fps") {
        inner.image.i_gif_fps.adjustment().set_value(f);
    }
    if let Some(f) = get_f("i_gif_w") {
        inner.image.i_gif_w.adjustment().set_value(f);
    }
    if let Some(b) = get_b("clip_enabled") {
        inner.clip.clip_enabled.set_active(b);
    }
    if let Some(s) = get_s("clip_start") {
        inner.clip.clip_start.set_text(&s);
    }
    if let Some(s) = get_s("clip_end") {
        inner.clip.clip_end.set_text(&s);
    }
    if let Some(b) = get_b("adv_crop_en") {
        inner.advanced.adv_crop_en.set_active(b);
    }
    if let Some(f) = get_f("adv_crop_w") {
        inner.advanced.adv_crop_w.adjustment().set_value(f);
    }
    if let Some(f) = get_f("adv_crop_h") {
        inner.advanced.adv_crop_h.adjustment().set_value(f);
    }
    if let Some(f) = get_f("adv_crop_x") {
        inner.advanced.adv_crop_x.adjustment().set_value(f);
    }
    if let Some(f) = get_f("adv_crop_y") {
        inner.advanced.adv_crop_y.adjustment().set_value(f);
    }
    if let Some(b) = get_b("adv_pad_en") {
        inner.advanced.adv_pad_en.set_active(b);
    }
    if let Some(f) = get_f("adv_pad_w") {
        inner.advanced.adv_pad_w.adjustment().set_value(f);
    }
    if let Some(f) = get_f("adv_pad_h") {
        inner.advanced.adv_pad_h.adjustment().set_value(f);
    }
    if let Some(s) = get_s("adv_pad_color") {
        inner.advanced.adv_pad_color.set_text(&s);
    }
    if let Some(n) = get_u("adv_rotate") {
        inner.advanced.adv_rotate.set_selected(n);
    }
    if let Some(b) = get_b("adv_deinterlace") {
        inner.advanced.adv_deinterlace.set_active(b);
    }
    if let Some(b) = get_b("adv_denoise") {
        inner.advanced.adv_denoise.set_active(b);
    }
    if let Some(b) = get_b("adv_sharpen") {
        inner.advanced.adv_sharpen.set_active(b);
    }
    if let Some(b) = get_b("adv_wm_en") {
        inner.advanced.adv_wm_en.set_active(b);
    }
    if let Some(s) = get_s("adv_wm_path") {
        inner.advanced.adv_wm_path.set_text(&s);
    }
    if let Some(n) = get_u("adv_wm_pos") {
        inner.advanced.adv_wm_pos.set_selected(n);
    }
    if let Some(f) = get_f("adv_wm_op") {
        inner.advanced.adv_wm_op.adjustment().set_value(f);
    }
    if let Some(b) = get_b("adv_audio_denoise") {
        inner.advanced.adv_audio_denoise.set_active(b);
    }
    if let Some(s) = get_s("adv_preset") {
        inner.advanced.adv_preset.set_text(&s);
    }
    if let Some(s) = get_s("adv_tune") {
        inner.advanced.adv_tune.set_text(&s);
    }
    if let Some(s) = get_s("adv_profile") {
        inner.advanced.adv_profile.set_text(&s);
    }
    if let Some(s) = get_s("adv_level") {
        inner.advanced.adv_level.set_text(&s);
    }
    if let Some(s) = get_s("adv_pix_fmt") {
        inner.advanced.adv_pix_fmt.set_text(&s);
    }
    if let Some(b) = get_b("adv_faststart") {
        inner.advanced.adv_faststart.set_active(b);
    }
    if let Some(b) = get_b("adv_two_pass") {
        inner.advanced.adv_two_pass.set_active(b);
    }
    if let Some(f) = get_f("adv_threads") {
        inner.advanced.adv_threads.adjustment().set_value(f);
    }
    if let Some(b) = get_b("adv_tonemap") {
        inner.advanced.adv_tonemap.set_active(b);
    }
    if let Some(n) = get_u("hw") {
        inner.hw.hw_row.set_selected(n);
    }
    if let Some(s) = get_s("custom_global") {
        inner.custom.custom_global.buffer().set_text(&s);
    }
    if let Some(s) = get_s("custom_input") {
        inner.custom.custom_input.buffer().set_text(&s);
    }
    if let Some(s) = get_s("custom_output") {
        inner.custom.custom_output.buffer().set_text(&s);
    }
}

/// 把当前偏好写入磁盘。
fn persist(inner: &Inner) {
    if let Ok(s) = serde_json::to_string_pretty(&snapshot(inner)) {
        if let Some(parent) = prefs_path().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(prefs_path(), s);
    }
}

// ---------- Inner 实现 ----------

impl Inner {
    fn toast(&self, msg: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(msg));
    }

    /// 当前下拉选中项对应的硬件加速偏好（下拉选项即固定的 `ALL_HW`）。
    fn hw_from_selected(&self) -> HwAccelPreference {
        ALL_HW
            .get(self.hw.hw_row.selected() as usize)
            .copied()
            .unwrap_or(HwAccelPreference::Auto)
    }

    /// 采用一份探测结果（来自磁盘缓存或本次探测），刷新状态行与默认值。
    fn apply_hw_cache(&self, cache: &hwaccel::HwCache) {
        *self.hw.hw_caps.borrow_mut() = cache.caps.clone();
        self.hw.hw_detected_at.set(cache.detected_at);
        // 下拉固定列出全部后端；仅当停留在「自动选择」时按结果定位最优后端。
        if self.hw.hw_row.selected() == 0 {
            let pref = self.hw.hw_caps.borrow().auto_preference();
            self.hw.hw_row.set_selected(hw_index_of(pref));
        }
        self.hw.hw_refresh.set_sensitive(true);
        self.refresh_hw_status();
        self.update_preview();
    }

    /// 刷新「硬件加速能力」状态行：可用后端 + 检测时间。
    fn refresh_hw_status(&self) {
        let at = self.hw.hw_detected_at.get();
        let text = if at == 0 {
            "尚未检测，点右侧「刷新」检测本机硬件加速能力".to_string()
        } else {
            let caps = self.hw.hw_caps.borrow();
            format!("{} · 检测于{}", caps.summary(), hwaccel::age_text(at))
        };
        self.hw.hw_status_row.set_subtitle(&text);
    }

    /// 模式决定的「实际输出大类」：视频→图片序列与视频→GIF 输出图片，
    /// 图片→视频输出视频；其余模式沿用用户在分类下拉里的选择。
    fn effective_category(&self) -> OutputCategory {
        effective_category_of(
            mode_from_index(self.output.mode_row.selected()),
            self.output.category_row.selected(),
        )
    }

    /// 读取当前控件值 → 纯状态机 → 统一 apply（显隐规则全部在 `layout_state`）。
    fn update_visibility(&self) {
        let st = layout_state(&LayoutInput {
            mode: mode_from_index(self.output.mode_row.selected()),
            category_idx: self.output.category_row.selected(),
            saved_manual: self.output.last_manual_category.get(),
            format_idx: self.output.format_row.selected(),
            bitrate_mode: bitrate_mode_from_index(self.video.v_bitrate_mode.selected()),
            codec: vcodec_from_index(self.video.v_codec.selected()),
            res: res_from_index(self.video.v_res.selected()),
            fps: fps_from_index(self.video.v_fps.selected()),
            clip_enabled: self.clip.clip_enabled.is_active(),
            crop_enabled: self.advanced.adv_crop_en.is_active(),
            pad_enabled: self.advanced.adv_pad_en.is_active(),
            wm_enabled: self.advanced.adv_wm_en.is_active(),
        });
        self.apply_layout(&st);
    }

    /// 把状态机结果逐项落到控件。set_selected 会重入本函数，靠「目标与现状相等
    /// 则不动」（状态机已产出 None）保证收敛，不再依赖散落的 `!=` guard。
    fn apply_layout(&self, st: &LayoutState) {
        // 先写分类槽位再改下拉：图片类模式进入时保存原选择，退出时清空。
        self.output.last_manual_category.set(st.saved_manual_next);
        self.output.category_row.set_sensitive(!st.category_locked);
        if let Some(idx) = st.category_set_to {
            self.output.category_row.set_selected(idx);
        }
        self.output.format_row.set_sensitive(!st.format_locked);
        if let Some(idx) = st.format_set_to {
            self.output.format_row.set_selected(idx);
        }

        self.video.video_card.set_visible(st.video_card);
        self.audio.audio_card.set_visible(st.audio_card);
        self.image.image_card.set_visible(st.image_card);
        self.clip.clip_card.set_visible(st.clip_card);
        self.advanced.adv_card.set_visible(st.advanced_card);
        self.custom.custom_card.set_visible(st.custom_card);
        self.hw.hw_card.set_visible(st.hw_card);

        self.video.v_crf.set_visible(st.crf_visible);
        self.video.v_bitrate.set_visible(st.bitrate_visible);
        self.video.v_w.set_visible(st.custom_res);
        self.video.v_h.set_visible(st.custom_res);
        self.video.v_fps_custom.set_visible(st.fps_custom_visible);

        self.clip.clip_start.set_sensitive(st.clip_children);
        self.clip.clip_end.set_sensitive(st.clip_children);
        self.advanced.adv_crop_w.set_sensitive(st.crop_children);
        self.advanced.adv_crop_h.set_sensitive(st.crop_children);
        self.advanced.adv_crop_x.set_sensitive(st.crop_children);
        self.advanced.adv_crop_y.set_sensitive(st.crop_children);
        self.advanced.adv_pad_w.set_sensitive(st.pad_children);
        self.advanced.adv_pad_h.set_sensitive(st.pad_children);
        self.advanced.adv_pad_color.set_sensitive(st.pad_children);
        self.advanced.adv_wm_path.set_sensitive(st.wm_children);
        self.advanced.adv_wm_pos.set_sensitive(st.wm_children);
        self.advanced.adv_wm_op.set_sensitive(st.wm_children);
    }

    fn build_plan(&self) -> Result<CommandPlan, String> {
        let spec = self.collect_spec();
        // 单文件转换（Single）支持多输入批量：每项一张独立 spec，
        // 输出名互不干扰（覆盖名优先，重名自动 -2/-3），时长也各取各的。
        if spec.mode == JobMode::Single {
            let names = self.single_item_names();
            let items = self.input.inputs.borrow();
            let mut plan = CommandPlan::default();
            for (item, name) in items.iter().zip(names.iter()) {
                let mut s = spec.clone();
                s.inputs = vec![crate::utils::media::command::InputSpec {
                    path: item.path.clone(),
                }];
                s.output_filename = name.clone();
                s.duration_sec = item.info.borrow().as_ref().map(|i| i.duration_sec);
                {
                    let p = build_commands(&s)?;
                    plan.commands.extend(p.commands);
                    plan.warnings.extend(p.warnings);
                }
            }
            return Ok(plan);
        }
        build_commands(&spec)
    }

    /// 从所有控件读取当前状态，组装成纯数据 `ConversionSpec`。
    fn collect_spec(&self) -> ConversionSpec {
        let mode = mode_from_index(self.output.mode_row.selected());
        // 图片/GIF 类模式的实际输出大类由模式决定（collect 侧与 UI 侧保持一致）
        let cat = self.effective_category();
        let mut format = format_from_index(cat, self.output.format_row.selected());
        if mode == JobMode::VideoToGif {
            format = ContainerFormat::Gif;
        }

        let inputs: Vec<_> = self
            .input
            .inputs
            .borrow()
            .iter()
            .map(|i| crate::utils::media::command::InputSpec {
                path: i.path.clone(),
            })
            .collect();

        let mut video = crate::utils::media::command::VideoSpec {
            codec: vcodec_from_index(self.video.v_codec.selected()),
            bitrate_mode: bitrate_mode_from_index(self.video.v_bitrate_mode.selected()),
            crf: self.video.v_crf.value() as u32,
            bitrate_kbps: self.video.v_bitrate.value() as u32,
            resolution: res_from_index(self.video.v_res.selected()),
            custom_w: self.video.v_w.value() as u32,
            custom_h: self.video.v_h.value() as u32,
            keep_aspect: self.video.v_keep_aspect.is_active(),
            fps: fps_from_index(self.video.v_fps.selected()),
            custom_fps: self.video.v_fps_custom.value(),
            scale_algo: scale_algo_from_index(self.video.v_scale_algo.selected()),
            colorspace: colorspace_from_index(self.video.v_colorspace.selected()),
            color_range: color_range_from_index(self.video.v_color_range.selected()),
            hdr_passthrough: self.video.v_hdr.is_active(),
        };
        // CRF 量程随编码器变化（§5.4 坑点）
        let crf_max = video.codec.crf_max();
        if video.crf > crf_max {
            video.crf = crf_max;
        }

        let audio = crate::utils::media::command::AudioSpec {
            codec: acodec_from_index(self.audio.a_codec.selected()),
            channels: channels_from_index(self.audio.a_channels.selected()),
            sample_rate: samplerate_from_index(self.audio.a_sr.selected()),
            bitrate_kbps: self.audio.a_bitrate.value() as u32,
            volume_gain_db: self.audio.a_gain.value(),
            fade_in_sec: self.audio.a_fade_in.value(),
            fade_out_sec: self.audio.a_fade_out.value(),
        };

        let image = crate::utils::media::command::ImageSpec {
            quality: self.image.i_quality.value() as u8,
            compression_level: self.image.i_compression.value() as u8,
            strip_metadata: self.image.i_strip.is_active(),
            longest_side: self.image.i_longest.value() as u32,
            scale_percent: self.image.i_percent.value() as u32,
            extract_fps: self.image.i_extract_fps.value(),
            gif_fps: self.image.i_gif_fps.value(),
            gif_width: self.image.i_gif_w.value() as u32,
        };

        let clip = crate::utils::media::command::ClipSpec {
            enabled: self.clip.clip_enabled.is_active(),
            start: self.clip.clip_start.text().to_string(),
            end: self.clip.clip_end.text().to_string(),
        };

        let vf_filters = collect_filters(&self.advanced.vf_list);
        let af_filters = collect_filters(&self.advanced.af_list);

        let advanced = crate::utils::media::command::AdvancedSpec {
            crop_enabled: self.advanced.adv_crop_en.is_active(),
            crop_w: self.advanced.adv_crop_w.value() as u32,
            crop_h: self.advanced.adv_crop_h.value() as u32,
            crop_x: self.advanced.adv_crop_x.value() as u32,
            crop_y: self.advanced.adv_crop_y.value() as u32,
            pad_enabled: self.advanced.adv_pad_en.is_active(),
            pad_w: self.advanced.adv_pad_w.value() as u32,
            pad_h: self.advanced.adv_pad_h.value() as u32,
            pad_color: self.advanced.adv_pad_color.text().to_string(),
            rotate: rotate_from_index(self.advanced.adv_rotate.selected()),
            deinterlace: self.advanced.adv_deinterlace.is_active(),
            denoise: self.advanced.adv_denoise.is_active(),
            sharpen: self.advanced.adv_sharpen.is_active(),
            watermark_enabled: self.advanced.adv_wm_en.is_active(),
            watermark_path: self.advanced.adv_wm_path.text().to_string(),
            watermark_pos: wm_pos_from_index(self.advanced.adv_wm_pos.selected()),
            watermark_opacity: self.advanced.adv_wm_op.value(),
            audio_denoise: self.advanced.adv_audio_denoise.is_active(),
            preset: self.advanced.adv_preset.text().to_string(),
            tune: self.advanced.adv_tune.text().to_string(),
            profile: self.advanced.adv_profile.text().to_string(),
            level: self.advanced.adv_level.text().to_string(),
            pix_fmt: self.advanced.adv_pix_fmt.text().to_string(),
            faststart: self.advanced.adv_faststart.is_active(),
            two_pass: self.advanced.adv_two_pass.is_active(),
            threads: self.advanced.adv_threads.value() as u32,
            tonemap: self.advanced.adv_tonemap.is_active(),
            vf_filters,
            af_filters,
        };

        let custom = crate::utils::media::command::CustomSpec {
            global: buffer_text(&self.custom.custom_global.buffer()),
            input: buffer_text(&self.custom.custom_input.buffer()),
            output: buffer_text(&self.custom.custom_output.buffer()),
        };

        let hw = self.hw_from_selected();
        let duration_sec = self
            .input
            .inputs
            .borrow()
            .first()
            .and_then(|i| i.info.borrow().as_ref().map(|info| info.duration_sec));

        ConversionSpec {
            mode,
            inputs,
            output_category: cat,
            output_format: format,
            output_dir: self.output.out_dir_row.text().to_string(),
            output_filename: self.output.default_name.borrow().clone(),
            video,
            audio,
            image,
            clip,
            advanced,
            custom,
            hw,
            quality: crate::model::media::QualityPreset::Balanced,
            duration_sec,
        }
    }

    fn update_preview(&self) {
        self.update_visibility();
        match self.build_plan() {
            Ok(plan) => {
                let mut text = plan
                    .commands
                    .iter()
                    .map(|c| c.to_display())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if !plan.warnings.is_empty() {
                    text.push_str("\n\n");
                    text.push_str(
                        &plan
                            .warnings
                            .iter()
                            .map(|w| format!("# ⚠ {w}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
                self.preview.preview_text.buffer().set_text(&text);
                let status = if plan.warnings.is_empty() {
                    "命令已生成，可复制或开始转换".to_string()
                } else {
                    format!("命令已生成（{} 条提示，见预览底部）", plan.warnings.len())
                };
                self.preview.status_label.set_text(&status);
            }
            Err(e) => {
                self.preview.preview_text.buffer().set_text("");
                self.preview.status_label.set_text(&format!("（{e}）"));
            }
        }
        // 刷新每行最终输出名与「全局/单项」提示条
        self.refresh_row_output_names();
        self.update_selection_hint();
        // 任意选项变化后持久化，实现跨会话记忆
        persist(self);
    }

    // ---------- 单项命名与批量输出 ----------

    /// 列表行选中变化：全局框不动（恒显全局值），把选中项的覆盖名回填到覆盖框。
    fn on_row_selected(&self, _row: Option<gtk::ListBoxRow>) {
        let selected = self.input.file_list.selected_row();
        let override_name = selected.and_then(|r| {
            self.input
                .inputs
                .borrow()
                .iter()
                .find(|it| it.row == r)
                .and_then(|it| it.name_override.borrow().clone())
        });
        self.output.loading_name.set(true);
        self.input
            .name_override_row
            .set_text(override_name.as_deref().unwrap_or(""));
        self.output.loading_name.set(false);
        self.update_preview();
    }

    /// 全局输出文件名输入框变化：恒写全局默认（6.6 起不再兼管单项覆盖）。
    fn on_name_text_changed(&self, text: &str) {
        if self.output.loading_name.get() {
            return;
        }
        *self.output.default_name.borrow_mut() = text.trim().to_string();
        self.update_preview();
    }

    /// 覆盖框变化：写入选中项的 name_override（留空 = 删除覆盖，恢复跟随全局）。
    fn on_override_text_changed(&self, text: &str) {
        if self.output.loading_name.get() {
            return;
        }
        let trimmed = text.trim().to_string();
        if let Some(row) = self.input.file_list.selected_row()
            && let Some(item) = self.input.inputs.borrow().iter().find(|it| it.row == row)
        {
            *item.name_override.borrow_mut() = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            };
        }
        self.update_preview();
    }

    /// 「跟随全局」：清空选中项的覆盖名（等价于清空覆盖框，按钮提供一键操作）。
    fn on_override_clear(&self) {
        if let Some(row) = self.input.file_list.selected_row() {
            if let Some(item) = self.input.inputs.borrow().iter().find(|it| it.row == row) {
                *item.name_override.borrow_mut() = None;
            }
            self.output.loading_name.set(true);
            self.input.name_override_row.set_text("");
            self.output.loading_name.set(false);
            self.update_preview();
        }
    }

    /// 设置全局默认输出文件名（同步内部状态与输入框）。
    fn set_global_name(&self, name: &str) {
        *self.output.default_name.borrow_mut() = name.to_string();
        self.output.loading_name.set(true);
        self.output.out_name_row.set_text(name);
        self.output.loading_name.set(false);
    }

    /// Single 模式下各输入的实际输出文件名（覆盖名优先；其次全局默认；
    /// 全局为空则按每项自身文件名推导；重名自动追加 -2/-3）。
    fn single_item_names(&self) -> Vec<String> {
        let spec = self.collect_spec();
        let dir = spec.output_dir.trim();
        let ext = spec.output_format.extension();
        let global = spec.output_filename;
        let global_empty = global.trim().is_empty();
        let items = self.input.inputs.borrow();
        let bases: Vec<String> = items
            .iter()
            .map(|it| {
                it.name_override.borrow().clone().unwrap_or_else(|| {
                    if global_empty {
                        auto_output_stem(&it.path)
                    } else {
                        global.clone()
                    }
                })
            })
            .collect();
        dedupe_output_names(&bases, ext, dir)
    }

    /// 刷新列表每行的最终输出名展示。
    fn refresh_row_output_names(&self) {
        let mode = mode_from_index(self.output.mode_row.selected());
        let names = if mode == JobMode::Single {
            self.single_item_names()
        } else {
            Vec::new()
        };
        let items = self.input.inputs.borrow();
        for (i, item) in items.iter().enumerate() {
            let text = names.get(i).map(|n| format!("→ {}", n)).unwrap_or_default();
            item.out_label.set_text(&text);
        }
    }

    /// 列表下方控件联动（6.6）：默认只显示全局命名；选中列表项且为 Single
    /// 模式时，展开「该项覆盖命名框 + 跟随全局按钮」。提示恒一行。
    fn update_selection_hint(&self) {
        let mode = mode_from_index(self.output.mode_row.selected());
        let count = self.input.inputs.borrow().len();
        if count == 0 {
            self.input.name_override_row.set_visible(false);
            self.input.override_clear.set_visible(false);
            self.input.selection_hint.set_visible(false);
            return;
        }
        if mode != JobMode::Single {
            // 非 Single：输出名统一走全局框，覆盖框无处生效
            self.input.name_override_row.set_visible(false);
            self.input.override_clear.set_visible(false);
            self.input
                .selection_hint
                .set_text("该作业模式以整个文件列表为单位，统一使用全局输出文件名。");
            self.input.selection_hint.set_visible(true);
            return;
        }
        let selected = self.input.file_list.selected_row();
        if let Some(row) = selected {
            // 选中：展开该行的覆盖命名区
            let inputs = self.input.inputs.borrow();
            let item = inputs.iter().find(|it| it.row == row);
            let file_name = item
                .map(|it| {
                    std::path::Path::new(&it.path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            self.input.name_override_row.set_visible(true);
            self.input.override_clear.set_visible(true);
            self.input.selection_hint.set_text(&format!(
                "正在为「{}」单独命名（留空 = 跟随全局默认；不影响其他文件）。",
                file_name
            ));
        } else {
            // 未选中：只保留一行引导
            self.input.name_override_row.set_visible(false);
            self.input.override_clear.set_visible(false);
            self.input
                .selection_hint
                .set_text("单击列表中的文件，可单独设置它的输出文件名。");
        }
        self.input.selection_hint.set_visible(true);
    }

    // ---------- 输入文件管理 ----------

    fn add_paths(&self, paths: &[String]) {
        for p in paths {
            self.add_one(p.clone());
        }
        self.update_preview();
    }

    fn add_one(&self, path: String) {
        // 文件夹递归展开
        if std::path::Path::new(&path).is_dir() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        self.add_one(p.to_string_lossy().to_string());
                    } else {
                        self.push_input(p.to_string_lossy().to_string());
                    }
                }
            }
            return;
        }
        self.push_input(path);
    }

    /// 若输出目录仍为空，填入首个输入文件所在目录。
    /// 输出文件名不再全局自动生成：留空时每项按自身文件名推导（auto_output_stem）。
    fn auto_fill_output(&self, first_input_path: &str) {
        if !self.output.out_dir_row.text().is_empty() {
            return;
        }
        let p = std::path::Path::new(first_input_path);
        if let Some(parent) = p.parent() {
            self.output.out_dir_row.set_text(&parent.to_string_lossy());
        }
        self.update_preview();
    }

    fn push_input(&self, path: String) {
        let info_label = gtk::Label::new(Some("读取信息中…"));
        info_label.add_css_class("dim-label");
        info_label.set_halign(gtk::Align::Start);
        info_label.set_wrap(true);
        info_label.set_xalign(0.0);
        // 该文件的最终输出名（如 → out.mp4），update_preview 时刷新
        let out_label = gtk::Label::new(None);
        out_label.add_css_class("dim-label");
        out_label.set_halign(gtk::Align::Start);
        out_label.set_xalign(0.0);

        let file_name = std::path::Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone());

        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let name = gtk::Label::new(Some(&file_name));
        name.set_halign(gtk::Align::Start);
        name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        box_.append(&name);
        box_.append(&info_label);
        box_.append(&out_label);

        let up = gtk::Button::from_icon_name("go-up-symbolic");
        up.set_tooltip_text(Some("上移"));
        up.add_css_class("flat");
        let down = gtk::Button::from_icon_name("go-down-symbolic");
        down.set_tooltip_text(Some("下移"));
        down.add_css_class("flat");
        let remove = gtk::Button::from_icon_name("user-trash-symbolic");
        remove.set_tooltip_text(Some("移除"));
        remove.add_css_class("flat");

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        actions.append(&up);
        actions.append(&down);
        actions.append(&remove);

        let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row_box.set_margin_top(6);
        row_box.set_margin_bottom(6);
        row_box.set_margin_start(12);
        row_box.set_margin_end(12);
        row_box.append(&box_);
        let spacer = gtk::Label::new(None);
        spacer.set_hexpand(true);
        row_box.append(&spacer);
        row_box.append(&actions);

        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&row_box));
        self.input.file_list.append(&row);

        let id = self.input.next_id.get();
        self.input.next_id.set(id + 1);
        let item = InputItem {
            id,
            path: path.clone(),
            info: RefCell::new(None),
            info_label: info_label.clone(),
            out_label: out_label.clone(),
            name_override: RefCell::new(None),
            row: row.clone(),
        };
        self.input.inputs.borrow_mut().push(item);
        let idx = self.input.inputs.borrow().len() - 1;

        // 首个文件加入时，自动填充输出目录与输出文件名
        if idx == 0 {
            self.auto_fill_output(&path);
            // 默认高亮第一项：触发 row-selected → on_row_selected，
            // 输出文件名框随即载入该项的默认输出名（如 “A - output”）
            self.input.file_list.select_row(Some(&row));
        }

        // 上移 / 下移 / 删除：按稳定 ID 定位当前索引，避免列表变动后旧索引错位
        up.connect_clicked(move |_| g_reorder(id, -1));
        down.connect_clicked(move |_| g_reorder(id, 1));
        remove.connect_clicked(move |_| g_remove(id));

        // 后台 ffprobe（仅捕获 Send 数据；UI 更新经 INNER 句柄，避免捕获 !Send 的控件）
        let path_for_thread = path.to_string();
        std::thread::spawn(move || {
            let result = probe::probe_file(&path_for_thread);
            glib::source::idle_add(move || {
                if let Some(inner) = INNER.with(|i| i.borrow().clone()) {
                    if let Some(item) = inner
                        .input
                        .inputs
                        .borrow()
                        .iter()
                        .find(|it| it.path == path_for_thread)
                    {
                        match &result {
                            Ok(info) => {
                                let mut s = String::new();
                                if let Some(v) = info.streams.iter().find(|s| s.kind == "video") {
                                    s.push_str(&format!(
                                        "视频 {} · {} · {}",
                                        v.codec,
                                        info.resolution_text(),
                                        v.frame_rate
                                    ));
                                }
                                if let Some(a) = info.streams.iter().find(|s| s.kind == "audio") {
                                    s.push_str(&format!(" · 音频 {} {}ch", a.codec, a.channels));
                                }
                                s.push_str(&format!(" · 时长 {}", info.duration_text()));
                                item.info_label.set_text(&s);
                                *item.info.borrow_mut() = Some(info.clone());
                            }
                            Err(e) => item.info_label.set_text(&format!("探测失败：{e}")),
                        }
                    }
                    inner.update_preview();
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn reorder(&self, id: u64, delta: i32) {
        let mut inputs = self.input.inputs.borrow_mut();
        let Some(idx) = inputs.iter().position(|it| it.id == id) else {
            return;
        };
        let new_idx = idx as i32 + delta;
        if new_idx < 0 || new_idx as usize >= inputs.len() {
            return;
        }
        let new_idx = new_idx as usize;
        inputs.swap(idx, new_idx);
        // ListBox 无 reorder/insert API：按新顺序整体重排行
        let rows: Vec<gtk::ListBoxRow> = inputs.iter().map(|i| i.row.clone()).collect();
        // 记住当前选中项（按稳定 id），重排后恢复高亮
        let sel_id = self
            .input
            .file_list
            .selected_row()
            .and_then(|r| inputs.iter().find(|it| it.row == r))
            .map(|it| it.id);
        for r in &rows {
            self.input.file_list.remove(r);
        }
        for r in &rows {
            self.input.file_list.append(r);
        }
        drop(inputs);
        if let Some(sid) = sel_id
            && let Some(row) = self.input.inputs.borrow().iter().find(|it| it.id == sid)
        {
            self.input.file_list.select_row(Some(&row.row));
        }
        self.update_preview();
    }

    fn remove_input(&self, id: u64) {
        let row = {
            let mut inputs = self.input.inputs.borrow_mut();
            let Some(idx) = inputs.iter().position(|it| it.id == id) else {
                return;
            };
            let row = inputs[idx].row.clone();
            inputs.remove(idx);
            row
        };
        // 先释放 inputs 借用再删行：`file_list.remove` 在删掉选中行时会同步
        // 触发 row-selected → on_row_selected 会读 inputs；此时若仍持有
        // borrow_mut 会触发 RefCell「already borrowed」panic。
        self.input.file_list.remove(&row);
        let empty = self.input.inputs.borrow().is_empty();
        // 若列表清空，重置输出字段使下次加文件时能重新自动填充
        if empty {
            self.output.out_dir_row.set_text("");
            self.set_global_name("");
        }
        self.update_preview();
    }

    fn clear_inputs(&self) {
        // 先取出行引用，再一次性 drop 掉 RefMut，避免 set_text 触发 notify 时
        // collect_spec → inputs.borrow() 与此处的 borrow_mut 冲突导致 panic。
        let rows: Vec<gtk::ListBoxRow> = {
            let mut inputs = self.input.inputs.borrow_mut();
            let rows: Vec<_> = inputs.iter().map(|i| i.row.clone()).collect();
            inputs.clear();
            rows
        };
        for r in &rows {
            self.input.file_list.remove(r);
        }
        // 清空输出字段，使下次加入首个文件时能重新自动填充
        self.output.out_dir_row.set_text("");
        self.set_global_name("");
        self.update_preview();
    }

    // ---------- 滤镜链编辑器 ----------

    fn add_filter_card(&self, is_video: bool) {
        let list = if is_video {
            &self.advanced.vf_list
        } else {
            &self.advanced.af_list
        };
        let name_entry = gtk::Entry::new();
        name_entry.set_placeholder_text(Some("滤镜名，如 scale / drawtext"));
        name_entry.set_hexpand(true);
        let params_entry = gtk::Entry::new();
        params_entry.set_placeholder_text(Some("参数，如 w=1280:h=720"));
        params_entry.set_hexpand(true);
        let remove = gtk::Button::from_icon_name("user-trash-symbolic");
        remove.add_css_class("flat");

        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        box_.set_margin_top(4);
        box_.set_margin_bottom(4);
        box_.set_margin_start(12);
        box_.set_margin_end(12);
        box_.append(&name_entry);
        box_.append(&params_entry);
        box_.append(&remove);

        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.set_selectable(false);
        row.set_child(Some(&box_));
        list.append(&row);

        let list = list.clone();
        remove.connect_clicked(move |_| {
            list.remove(&row);
        });
        self.update_preview();
    }

    // ---------- 运行 ----------

    fn run(&self) {
        let plan = match self.build_plan() {
            Ok(p) => p,
            Err(e) => {
                self.toast(&format!("无法开始：{e}"));
                return;
            }
        };
        if plan.commands.is_empty() {
            self.toast("没有可执行的命令");
            return;
        }

        // 写出 concat 列表（如有）：路径与命令 / 导出脚本保持一致
        if let Some(list) = &plan.concat_list {
            if plan.concat_path.is_empty() {
                self.toast("concat 列表路径为空");
                return;
            }
            if let Err(e) = std::fs::write(&plan.concat_path, list) {
                self.toast(&format!("无法写入 concat 列表：{e}"));
                return;
            }
        }
        // 生成提示（如 2-Pass 硬件回退、被跳过的滤镜）
        if let Some(w) = plan.warnings.first() {
            self.toast(&format!("提示：{w}"));
        }

        // 取首个输入时长用于进度估算
        let duration = self
            .input
            .inputs
            .borrow()
            .first()
            .and_then(|i| i.info.borrow().as_ref().map(|info| info.duration_sec));

        self.preview.run_button.set_sensitive(false);
        self.preview.progress.set_visible(true);
        self.preview.progress.set_fraction(0.0);
        self.preview.status_label.set_text("转换中…");

        let total = plan.commands.len();
        let commands = plan.commands.clone();

        std::thread::spawn(move || {
            let mut ok = true;
            let mut last_err = String::new();
            for (i, cmd) in commands.iter().enumerate() {
                let frac = match run_one(cmd, duration) {
                    Ok(f) => f,
                    Err(e) => {
                        ok = false;
                        last_err = e;
                        0.0
                    }
                };
                let progress = ((i as f64 + frac) / total as f64).min(1.0);
                glib::source::idle_add(move || {
                    if let Some(inner) = INNER.with(|i| i.borrow().clone()) {
                        inner.preview.progress.set_fraction(progress);
                    }
                    glib::ControlFlow::Break
                });
                if !ok {
                    break;
                }
            }
            glib::source::idle_add(move || {
                if let Some(inner) = INNER.with(|i| i.borrow().clone()) {
                    inner.preview.progress.set_visible(false);
                    inner.preview.run_button.set_sensitive(true);
                    if ok {
                        // 清理中间产物（GIF 调色板等除最后一条命令外的输出）
                        for c in &commands[..commands.len().saturating_sub(1)] {
                            if !c.output.is_empty() {
                                let _ = std::fs::remove_file(&c.output);
                            }
                        }
                        inner
                            .preview
                            .status_label
                            .set_text(&format!("完成：共 {} 条命令", total));
                        inner.toast("转换完成");
                    } else {
                        inner
                            .preview
                            .status_label
                            .set_text(&format!("失败：{last_err}"));
                        inner.toast("转换失败，详见命令预览");
                    }
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn copy_command(&self) {
        let buf = self.preview.preview_text.buffer();
        let text = buf
            .text(&buf.start_iter(), &buf.end_iter(), false)
            .to_string();
        if text.trim().is_empty() {
            self.toast("暂无可复制的命令");
            return;
        }
        self.preview.preview_text.clipboard().set_text(&text);
        self.toast("命令已复制到剪贴板");
    }

    fn save_script(&self) {
        match self.build_plan() {
            Ok(plan) => {
                let script = to_shell_script(&plan, true);
                let path = format!(
                    "{}/linbox_ffmpeg.sh",
                    if self.output.out_dir_row.text().is_empty() {
                        ".".to_string()
                    } else {
                        self.output.out_dir_row.text().to_string()
                    }
                );
                match std::fs::write(&path, script) {
                    Ok(_) => {
                        self.toast(&format!("已保存脚本：{path}"));
                    }
                    Err(e) => self.toast(&format!("保存失败：{e}")),
                }
            }
            Err(e) => {
                self.toast(&format!("无法生成：{e}"));
            }
        }
    }
}

/// 收集滤镜链列表中的滤镜卡片。
fn collect_filters(list: &gtk::ListBox) -> Vec<FilterEntry> {
    let mut out = Vec::new();
    let mut row: Option<gtk::Widget> = list.first_child();
    while let Some(w) = row {
        if let Some(child) = w.first_child()
            && let Ok(box_) = child.downcast::<gtk::Box>()
        {
            let mut entry_child = box_.first_child();
            let mut name = String::new();
            let mut params = String::new();
            let mut count = 0;
            while let Some(c) = entry_child {
                if let Ok(entry) = c.clone().downcast::<gtk::Entry>() {
                    if count == 0 {
                        name = entry.text().to_string();
                    } else if count == 1 {
                        params = entry.text().to_string();
                    }
                    count += 1;
                }
                entry_child = c.next_sibling();
            }
            if !name.trim().is_empty() {
                out.push(FilterEntry {
                    name: name.trim().to_string(),
                    params: params.trim().to_string(),
                    enabled: true,
                });
            }
        }
        row = w.next_sibling();
    }
    out
}

/// 执行单条命令，解析 stderr 中的 time= 估算进度，返回 0..=1 的完成度。
fn run_one(
    cmd: &crate::utils::media::command::Command,
    duration: Option<f64>,
) -> Result<f64, String> {
    use std::io::BufRead;
    use std::process::{Command as Proc, Stdio};

    let mut proc = Proc::new(&cmd.program);
    proc.args(&cmd.args);
    proc.stdout(Stdio::null());
    proc.stderr(Stdio::piped());

    let mut child = proc.spawn().map_err(|e| format!("无法启动 ffmpeg：{e}"))?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "无法获取 ffmpeg 输出".to_string())?;
    let mut last_time = 0.0f64;
    let reader = std::io::BufReader::new(stderr);
    for line in reader.lines().map_while(Result::ok) {
        if let Some(pos) = line.find("time=") {
            let rest = &line[pos + 5..];
            if let Some(t) = parse_time(rest.trim()) {
                last_time = t;
            }
        }
    }
    let status = child.wait().map_err(|e| format!("等待 ffmpeg 失败：{e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg 退出码 {}", status.code().unwrap_or(-1)));
    }
    let frac = match duration {
        Some(d) if d > 0.0 => (last_time / d).clamp(0.0, 1.0),
        _ => 1.0,
    };
    Ok(frac)
}

/// 解析 ffmpeg 进度行中的 `HH:MM:SS.ss` 或 `SS.ss` 为秒。
fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split(':').collect();
    let secs: f64 = match parts.len() {
        3 => {
            let h: f64 = parts[0].parse().ok()?;
            let m: f64 = parts[1].parse().ok()?;
            let s: f64 = parts[2].parse().ok()?;
            h * 3600.0 + m * 60.0 + s
        }
        2 => {
            let m: f64 = parts[0].parse().ok()?;
            let s: f64 = parts[1].parse().ok()?;
            m * 60.0 + s
        }
        1 => s.parse().ok()?,
        _ => return None,
    };
    Some(secs)
}

/// 取出 TextBuffer 的全部文本。
fn buffer_text(buffer: &gtk::TextBuffer) -> String {
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string()
}

/// 从输入路径推导该文件的默认输出名（不含扩展名）：`{stem} - output`。
/// 与自动填充的历史格式保持一致；拿不到文件名时退回 `output - output`。
fn auto_output_stem(path: &str) -> String {
    let stem = std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".to_string());
    format!("{} - output", stem)
}

/// 批量输出名去重：同名输出自动追加 -2/-3…（带扩展名比较，`dir` 非空时按
/// 目录内唯一）。返回与 `bases` 一一对应的最终文件名（不含扩展名）。
fn dedupe_output_names(bases: &[String], ext: &str, dir: &str) -> Vec<String> {
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    bases
        .iter()
        .map(|base| {
            let base = base.trim().to_string();
            let mut name = base.clone();
            let mut n = 2u32;
            loop {
                let full = if name.ends_with(&format!(".{}", ext)) {
                    name.clone()
                } else {
                    format!("{}.{}", name, ext)
                };
                let key = if dir.is_empty() {
                    full.clone()
                } else {
                    format!("{}/{}", dir.trim_end_matches('/'), full)
                };
                if used.insert(key) {
                    break;
                }
                name = format!("{}-{}", base, n);
                n += 1;
            }
            name
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{LayoutInput, LayoutState, auto_output_stem, dedupe_output_names, layout_state};
    use crate::model::media::{BitrateMode, FpsPreset, ResolutionPreset, VideoCodec};
    use crate::utils::media::command::JobMode;

    /// 构造一份「全默认」的显隐输入：单文件 / 指定分类 / CRF / libx264 / 源分辨率 / 源帧率 / 全开关关。
    fn li(mode: JobMode, category_idx: u32) -> LayoutInput {
        LayoutInput {
            mode,
            category_idx,
            saved_manual: None,
            format_idx: 0,
            bitrate_mode: BitrateMode::Crf,
            codec: VideoCodec::Libx264,
            res: ResolutionPreset::Source,
            fps: FpsPreset::Source,
            clip_enabled: false,
            crop_enabled: false,
            pad_enabled: false,
            wm_enabled: false,
        }
    }

    /// 卡片可见性六元组：(视频, 音频, 图片, 片段截取, 高级选项, 自定义参数)。
    fn cards(st: &LayoutState) -> (bool, bool, bool, bool, bool, bool) {
        (
            st.video_card,
            st.audio_card,
            st.image_card,
            st.clip_card,
            st.advanced_card,
            st.custom_card,
        )
    }

    // ---------- 6.4 真值表：作业模式 × 输出大类 ----------

    #[test]
    fn layout_single_mode_follows_manual_category() {
        // 单文件：分类自由，卡片随分类走；片段截取/高级/自定义全部消费
        let st = layout_state(&li(JobMode::Single, 0));
        assert_eq!(cards(&st), (true, true, false, true, true, true));
        assert!(st.hw_card, "单文件经 resolve_video_encoder 消费 spec.hw");
        assert_eq!(
            cards(&layout_state(&li(JobMode::Single, 1))),
            (false, true, false, true, true, true)
        );
        assert_eq!(
            cards(&layout_state(&li(JobMode::Single, 2))),
            (false, false, true, true, true, true)
        );
    }

    #[test]
    fn layout_concat_locks_out_clip() {
        // 拼接不消费 spec.clip → 片段截取卡隐藏，其余同单文件
        for cat in 0..3 {
            let st = layout_state(&li(JobMode::Concat, cat));
            assert!(!st.clip_card, "Concat 不消费 clip, cat={cat}");
            assert!(st.advanced_card && st.custom_card, "cat={cat}");
        }
        let st = layout_state(&li(JobMode::Concat, 0));
        assert_eq!(cards(&st), (true, true, false, false, true, true));
    }

    #[test]
    fn layout_split_keeps_clip() {
        // 拆条按 clip.start 切段 → 片段截取卡必须可见
        let st = layout_state(&li(JobMode::Split, 0));
        assert_eq!(cards(&st), (true, true, false, true, true, true));
    }

    #[test]
    fn layout_image_extract_locks_category_and_hides_advanced() {
        // 视频→图片序列：强制图片分类、锁下拉；不消费 advanced → 隐藏；消费 clip/custom → 保留
        let st = layout_state(&li(JobMode::ImageExtract, 0));
        assert_eq!(cards(&st), (false, false, true, true, false, true));
        assert!(!st.hw_card, "抽帧命令不读 spec.hw → 硬件加速卡隐藏");
        assert!(st.category_locked);
        assert_eq!(st.category_set_to, Some(2));
        assert_eq!(st.saved_manual_next, Some(0), "进入时保存用户原分类");
    }

    #[test]
    fn layout_image_to_video_locks_video_category() {
        // 图片→视频：强制视频分类；不消费 clip → 隐藏；经 push_container_options 消费 advanced → 保留
        let st = layout_state(&li(JobMode::ImageToVideo, 2));
        assert_eq!(cards(&st), (true, true, true, false, true, true));
        assert!(st.category_locked);
        assert_eq!(st.category_set_to, Some(0));
        assert_eq!(st.saved_manual_next, Some(2));
    }

    #[test]
    fn layout_video_to_gif_locks_format_and_hides_custom() {
        // 视频→GIF：图片分类 + GIF 格式锁；双遍调色板命令不注入 custom → 自定义卡隐藏
        let st = layout_state(&li(JobMode::VideoToGif, 0));
        assert_eq!(cards(&st), (false, false, true, true, false, false));
        assert!(!st.hw_card, "GIF 双遍调色板不读 spec.hw → 硬件加速卡隐藏");
        assert!(st.format_locked);
        assert_eq!(st.format_set_to, Some(5), "GIF 是图片格式列表第 6 项");
        assert_eq!(st.category_set_to, Some(2));
    }

    #[test]
    fn layout_gif_format_already_set_needs_no_reselect() {
        // 格式已在 GIF → 不再 set_selected，保证重入收敛
        let mut i = li(JobMode::VideoToGif, 2);
        i.format_idx = 5;
        let st = layout_state(&i);
        assert!(st.format_locked);
        assert_eq!(st.format_set_to, None);
        assert_eq!(st.category_set_to, None, "分类已是图片 → 不再改写");
    }

    #[test]
    fn layout_restore_saved_category_after_image_mode() {
        // 退出图片类模式：恢复保存的分类并清空槽位
        let mut i = li(JobMode::Single, 1);
        i.saved_manual = Some(0);
        let st = layout_state(&i);
        assert!(!st.category_locked);
        assert_eq!(st.category_set_to, Some(0), "恢复到离开前的视频分类");
        assert_eq!(st.saved_manual_next, None, "槽位清空");
        assert!(st.video_card, "按恢复后的分类算卡片");

        // 恢复目标与当前相同 → 不动，收敛
        let mut i2 = li(JobMode::Single, 0);
        i2.saved_manual = Some(0);
        assert_eq!(layout_state(&i2).category_set_to, None);
    }

    #[test]
    fn layout_crf_bitrate_and_copy_visibility() {
        // CRF 模式显示 CRF、隐藏码率；非 CRF 反之；Copy 两者都藏
        let mut i = li(JobMode::Single, 0);
        i.bitrate_mode = BitrateMode::Crf;
        let st = layout_state(&i);
        assert!(st.crf_visible && !st.bitrate_visible);

        i.bitrate_mode = BitrateMode::Cbr;
        let st = layout_state(&i);
        assert!(!st.crf_visible && st.bitrate_visible);

        i.codec = VideoCodec::Copy;
        let st = layout_state(&i);
        assert!(!st.crf_visible && !st.bitrate_visible, "Copy 不压码率");
    }

    #[test]
    fn layout_custom_res_and_fps_visibility() {
        let mut i = li(JobMode::Single, 0);
        i.res = ResolutionPreset::Custom;
        assert!(layout_state(&i).custom_res);

        i.res = ResolutionPreset::R1080;
        assert!(!layout_state(&i).custom_res);

        i.fps = FpsPreset::Custom;
        assert!(layout_state(&i).fps_custom_visible);

        i.fps = FpsPreset::F30;
        assert!(
            !layout_state(&i).fps_custom_visible,
            "非自定义帧率时隐藏输入框"
        );
    }

    #[test]
    fn layout_switch_children_greyed_when_off() {
        // 开关全关 → 子项全部置灰；逐个打开只点亮对应组
        let st = layout_state(&li(JobMode::Single, 0));
        assert!(!st.clip_children && !st.crop_children && !st.pad_children && !st.wm_children);

        let mut i = li(JobMode::Single, 0);
        i.clip_enabled = true;
        i.crop_enabled = true;
        let st = layout_state(&i);
        assert!(st.clip_children && st.crop_children);
        assert!(!st.pad_children && !st.wm_children);

        i.pad_enabled = true;
        i.wm_enabled = true;
        let st = layout_state(&i);
        assert!(st.pad_children && st.wm_children);
    }

    #[test]
    fn layout_video_to_gif_keeps_clip_but_greys_children() {
        // GIF 模式消费 clip.start；开关关 → 时间输入框置灰（而非隐藏）
        let mut i = li(JobMode::VideoToGif, 0);
        i.clip_enabled = true;
        let st = layout_state(&i);
        assert!(st.clip_card);
        assert!(st.clip_children);
    }

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn dedupe_keeps_unique_names() {
        let out = dedupe_output_names(&v(&["a", "b", "c"]), "mp4", "");
        assert_eq!(out, v(&["a", "b", "c"]));
    }

    #[test]
    fn dedupe_appends_suffix_on_collision() {
        let out = dedupe_output_names(&v(&["out", "out", "out"]), "mp4", "");
        assert_eq!(out, v(&["out", "out-2", "out-3"]));
    }

    #[test]
    fn dedupe_scopes_by_output_dir() {
        // 去重键带上输出目录：同一调用内同名文件在目录内去重
        let out = dedupe_output_names(&v(&["out", "out"]), "mp4", "");
        assert_eq!(out, v(&["out", "out-2"]));
        let out2 = dedupe_output_names(&v(&["out", "out"]), "mp4", "/tmp/a");
        assert_eq!(out2, v(&["out", "out-2"]));
        let out3 = dedupe_output_names(&v(&["a", "b"]), "mp4", "/tmp/b");
        assert_eq!(out3, v(&["a", "b"]));
    }

    #[test]
    fn dedupe_handles_existing_extension() {
        // 用户写了 .gif 后缀时不再重复追加
        let out = dedupe_output_names(&v(&["clip.gif", "clip"]), "gif", "");
        assert_eq!(out, v(&["clip.gif", "clip-2"]));
    }

    #[test]
    fn auto_stem_uses_own_filename() {
        // 每项默认名 = 自身文件名（去扩展名） - output
        assert_eq!(auto_output_stem("/data/我的视频.mp4"), "我的视频 - output");
        assert_eq!(
            auto_output_stem("/data/会议记录 2026.mkv"),
            "会议记录 2026 - output"
        );
    }

    #[test]
    fn auto_stem_fallback() {
        // 无扩展名、无文件名时兜底
        assert_eq!(auto_output_stem("/data/clip"), "clip - output");
        assert_eq!(auto_output_stem(""), "output - output");
    }
}
