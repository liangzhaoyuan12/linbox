//! ffmpeg 命令构建器（逻辑层 · 无 GTK 依赖）。
//!
//! 输入是一份纯数据的 [`ConversionSpec`]，输出是可直接 `std::process::Command`
//! 执行的 `argv`（`Vec<String>`），以及用于预览的命令行字符串。
//!
//! 设计目标（呼应规划书 §2.3 C3 覆盖度定义）：任何合法的 ffmpeg 命令都能通过
//! 「图形化选项 + 自定义参数」等价构造。自定义参数通过 `CustomSpec` 注入，且
//! 图形化选项在合并时拥有更高优先级（自定义参数写在前、图形参数写在后，
//! ffmpeg 对多数重复参数取最后一个生效值，因此图形优先）。

use crate::model::media::*;

/// 旋转 / 翻转模式（图形化高级控件 C1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RotateMode {
    #[default]
    None,
    Rotate90,
    Rotate180,
    Rotate270,
    FlipH,
    FlipV,
}

impl RotateMode {
    pub fn label(&self) -> &'static str {
        match self {
            RotateMode::None => "无",
            RotateMode::Rotate90 => "顺时针 90°",
            RotateMode::Rotate180 => "180°",
            RotateMode::Rotate270 => "逆时针 90°",
            RotateMode::FlipH => "水平翻转",
            RotateMode::FlipV => "垂直翻转",
        }
    }
}

/// 水印位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WatermarkPos {
    #[default]
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Center,
}

impl WatermarkPos {
    pub fn label(&self) -> &'static str {
        match self {
            WatermarkPos::TopLeft => "左上",
            WatermarkPos::TopRight => "右上",
            WatermarkPos::BottomLeft => "左下",
            WatermarkPos::BottomRight => "右下",
            WatermarkPos::Center => "居中",
        }
    }

    /// 直接拼出 overlay 坐标表达式（基于主画面 W/H 与水印 ow/oh）。
    pub fn overlay(&self) -> String {
        match self {
            WatermarkPos::TopLeft => "10:10",
            WatermarkPos::TopRight => "W-ow-10:10",
            WatermarkPos::BottomLeft => "10:H-oh-10",
            WatermarkPos::BottomRight => "W-ow-10:H-oh-10",
            WatermarkPos::Center => "(W-ow)/2:(H-oh)/2",
        }
        .to_string()
    }
}

/// 滤镜链编辑器（C2）中的单个滤镜卡。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterEntry {
    pub name: String,
    pub params: String,
    pub enabled: bool,
}

impl FilterEntry {
    /// 渲染为滤镜图中的一个节点，如 `name=params`。
    pub fn to_string(&self) -> String {
        if self.params.trim().is_empty() {
            self.name.clone()
        } else {
            format!("{}={}", self.name, self.params)
        }
    }
}

/// 作业模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JobMode {
    /// 单文件转换。
    #[default]
    Single,
    /// 按顺序拼接列表内文件。
    Concat,
    /// 按时间轴分段输出多个文件。
    Split,
    /// 视频抽取为图片序列帧。
    ImageExtract,
    /// 图片序列帧合成为视频。
    ImageToVideo,
    /// 视频转 GIF。
    VideoToGif,
}

impl JobMode {
    pub fn label(&self) -> &'static str {
        match self {
            JobMode::Single => "单文件转换",
            JobMode::Concat => "拼接",
            JobMode::Split => "拆条分段",
            JobMode::ImageExtract => "视频→图片序列",
            JobMode::ImageToVideo => "图片→视频",
            JobMode::VideoToGif => "视频→GIF",
        }
    }
}

/// 单个输入文件（路径，统一 utf-8）。
#[derive(Debug, Clone, Default)]
pub struct InputSpec {
    pub path: String,
}

/// 视频参数（模块 A3）。
#[derive(Debug, Clone)]
pub struct VideoSpec {
    pub codec: VideoCodec,
    pub bitrate_mode: BitrateMode,
    pub crf: u32,
    pub bitrate_kbps: u32,
    pub resolution: ResolutionPreset,
    pub custom_w: u32,
    pub custom_h: u32,
    pub keep_aspect: bool,
    pub fps: FpsPreset,
    pub custom_fps: f64,
    pub scale_algo: ScaleAlgorithm,
    pub colorspace: ColorSpace,
    pub color_range: ColorRange,
    pub hdr_passthrough: bool,
}

impl Default for VideoSpec {
    fn default() -> Self {
        VideoSpec {
            codec: VideoCodec::default(),
            bitrate_mode: BitrateMode::default(),
            crf: VideoCodec::default().crf_default(),
            bitrate_kbps: 4000,
            resolution: ResolutionPreset::default(),
            custom_w: 0,
            custom_h: 0,
            keep_aspect: true,
            fps: FpsPreset::default(),
            custom_fps: 30.0,
            scale_algo: ScaleAlgorithm::default(),
            colorspace: ColorSpace::default(),
            color_range: ColorRange::default(),
            hdr_passthrough: false,
        }
    }
}

/// 音频参数（模块 A4）。
#[derive(Debug, Clone)]
pub struct AudioSpec {
    pub codec: AudioCodec,
    pub channels: Channels,
    pub sample_rate: SampleRate,
    pub bitrate_kbps: u32,
    pub volume_gain_db: f64,
    pub fade_in_sec: f64,
    pub fade_out_sec: f64,
}

impl Default for AudioSpec {
    fn default() -> Self {
        AudioSpec {
            codec: AudioCodec::default(),
            channels: Channels::default(),
            sample_rate: SampleRate::default(),
            bitrate_kbps: 192,
            volume_gain_db: 0.0,
            fade_in_sec: 0.0,
            fade_out_sec: 0.0,
        }
    }
}

/// 图片参数（模块 B）。
#[derive(Debug, Clone)]
pub struct ImageSpec {
    pub quality: u8,
    pub compression_level: u8,
    pub strip_metadata: bool,
    pub longest_side: u32,
    pub scale_percent: u32,
    pub extract_fps: f64,
    pub gif_fps: f64,
    pub gif_width: u32,
}

impl Default for ImageSpec {
    fn default() -> Self {
        ImageSpec {
            quality: 90,
            compression_level: 6,
            strip_metadata: true,
            longest_side: 0,
            scale_percent: 100,
            extract_fps: 1.0,
            gif_fps: 15.0,
            gif_width: 480,
        }
    }
}

/// 片段截取（模块 A5）。
#[derive(Debug, Clone, Default)]
pub struct ClipSpec {
    pub enabled: bool,
    /// 起止时间，支持秒（如 `12.5`）或 `HH:MM:SS[.mmm]`。
    pub start: String,
    pub end: String,
}

/// 高级选项（模块 C1）。
#[derive(Debug, Clone)]
pub struct AdvancedSpec {
    pub crop_enabled: bool,
    pub crop_w: u32,
    pub crop_h: u32,
    pub crop_x: u32,
    pub crop_y: u32,
    pub pad_enabled: bool,
    pub pad_w: u32,
    pub pad_h: u32,
    pub pad_color: String,
    pub rotate: RotateMode,
    pub deinterlace: bool,
    pub denoise: bool,
    pub sharpen: bool,
    pub watermark_enabled: bool,
    pub watermark_path: String,
    pub watermark_pos: WatermarkPos,
    pub watermark_opacity: f64,
    pub audio_denoise: bool,
    pub preset: String,
    pub tune: String,
    pub profile: String,
    pub level: String,
    pub pix_fmt: String,
    pub faststart: bool,
    pub two_pass: bool,
    pub threads: u32,
    pub tonemap: bool,
    /// 滤镜链编辑器（C2）：视频滤镜图。
    pub vf_filters: Vec<FilterEntry>,
    /// 滤镜链编辑器（C2）：音频滤镜图。
    pub af_filters: Vec<FilterEntry>,
}

impl Default for AdvancedSpec {
    fn default() -> Self {
        AdvancedSpec {
            crop_enabled: false,
            crop_w: 1280,
            crop_h: 720,
            crop_x: 0,
            crop_y: 0,
            pad_enabled: false,
            pad_w: 1920,
            pad_h: 1080,
            pad_color: "black".to_string(),
            rotate: RotateMode::default(),
            deinterlace: false,
            denoise: false,
            sharpen: false,
            watermark_enabled: false,
            watermark_path: String::new(),
            watermark_pos: WatermarkPos::default(),
            watermark_opacity: 1.0,
            audio_denoise: false,
            preset: String::new(),
            tune: String::new(),
            profile: String::new(),
            level: String::new(),
            pix_fmt: String::new(),
            faststart: false,
            two_pass: false,
            threads: 0,
            tonemap: false,
            vf_filters: Vec::new(),
            af_filters: Vec::new(),
        }
    }
}

/// 自定义参数注入（模块 C3）。
#[derive(Debug, Clone, Default)]
pub struct CustomSpec {
    /// ffmpeg 之后、输入之前的全局参数。
    pub global: String,
    /// 每个 `-i` 之前的输入参数。
    pub input: String,
    /// 输出文件之前的输出参数。
    pub output: String,
}

/// 一份完整的转换作业描述（纯数据，由页面从控件状态组装）。
#[derive(Debug, Clone, Default)]
pub struct ConversionSpec {
    pub mode: JobMode,
    pub inputs: Vec<InputSpec>,
    pub output_category: OutputCategory,
    pub output_format: ContainerFormat,
    pub output_dir: String,
    pub output_filename: String,
    pub video: VideoSpec,
    pub audio: AudioSpec,
    pub image: ImageSpec,
    pub clip: ClipSpec,
    pub advanced: AdvancedSpec,
    pub custom: CustomSpec,
    pub hw: HwAccelPreference,
    pub quality: QualityPreset,
    /// 首个输入的总时长（秒，来自 ffprobe 探测或片段截取的结束时间），
    /// 用于需要绝对时间点的滤镜（如 afade 淡出）。未知时为 None。
    pub duration_sec: Option<f64>,
}

/// 单条可执行命令。
#[derive(Debug, Clone)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    /// 人类可读的整行命令行（用于预览 / 复制）。
    pub display: String,
    /// 该命令产出的输出文件路径（用于进度 / 校验；多输出时为空）。
    pub output: String,
}

/// 一组命令（如 2-Pass 会产生两条；拆条会产生多条）+ 额外的临时文件内容。
#[derive(Debug, Clone, Default)]
pub struct CommandPlan {
    pub commands: Vec<Command>,
    /// concat 列表文件内容（如需要，由页面写出到磁盘）。
    pub concat_list: Option<String>,
    /// concat 列表文件的实际路径（命令、运行与导出的 Shell 脚本使用同一路径）。
    pub concat_path: String,
    /// 生成过程中的用户提示（硬件回退 / 被跳过的无效选项等），展示在命令预览中。
    pub warnings: Vec<String>,
}

impl Command {
    /// 渲染为可复制的单行命令（参数用双引号包裹以处理空格）。
    pub fn to_display(&self) -> String {
        let mut s = self.program.clone();
        for a in &self.args {
            s.push(' ');
            if a.contains(' ') || a.contains('"') {
                s.push_str(&format!("\"{}\"", a.replace('"', "\\\"")));
            } else {
                s.push_str(a);
            }
        }
        s
    }
}

/// 将整个计划渲染为可保存的 Shell 脚本。
pub fn to_shell_script(plan: &CommandPlan, with_log: bool) -> String {
    let mut out = String::from("#!/usr/bin/env bash\n# 由 linbox 生成的 ffmpeg 批处理脚本\nset -euo pipefail\n\n");
    for w in &plan.warnings {
        out.push_str(&format!("# ⚠ {w}\n"));
    }
    if let Some(list) = &plan.concat_list {
        out.push_str(&format!("# ---- concat 列表文件 (ffconcat)：{} ----\n", plan.concat_path));
        out.push_str(&format!("cat > {} <<'EOF'\n", plan.concat_path));
        out.push_str(list);
        out.push_str("EOF\n\n");
    }
    for (i, cmd) in plan.commands.iter().enumerate() {
        if plan.commands.len() > 1 {
            out.push_str(&format!("# ---- 步骤 {} ----\n", i + 1));
        }
        let line = cmd.to_display();
        if with_log {
            out.push_str(&format!("{line} 2> \"ffmpeg_{}.log\"\n", i + 1));
        } else {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// 主入口：根据 `spec` 组装一条或多条命令。
pub fn build_commands(spec: &ConversionSpec) -> Result<CommandPlan, String> {
    if spec.inputs.is_empty() {
        return Err("未添加任何输入文件".into());
    }

    match spec.mode {
        JobMode::Concat => build_concat(spec),
        JobMode::Split => build_split(spec),
        JobMode::ImageExtract => build_image_extract(spec),
        JobMode::ImageToVideo => build_image_to_video(spec),
        JobMode::VideoToGif => build_video_to_gif(spec),
        JobMode::Single => build_single(spec, false),
    }
}

// ---------------------------------------------------------------------------
// 内部辅助：参数收集
// ---------------------------------------------------------------------------

/// 把命令行片段按空白切成参数（与 shell 行为类似，但不处理引号嵌套，足够覆盖
/// 用户在「自定义参数」框里输入的简单场景）。
fn split_args(s: &str) -> Vec<String> {
    s.split_whitespace().map(|x| x.to_string()).collect()
}

/// 视频滤镜图（vf）收集。返回完整的 `-vf` 参数值字符串（不含 `-vf` 键）。
///
/// `Copy` 编码流不能施加滤镜，直接返回 `None`。几何类滤镜（scale/crop/pad/rotate）
/// 之后才补 `setsar=1:1`；色彩空间/范围修正仅在非默认时追加，避免对每次转换
/// 都强加一道无意义的缩放。
fn build_video_filters(spec: &ConversionSpec) -> Option<String> {
    // 复制流不能施加滤镜；但图片输出不经过 `-c:v`，裁剪/缩放等滤镜仍然有效。
    if spec.video.codec == VideoCodec::Copy && spec.output_category == OutputCategory::Video {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();
    let mut geometric = false;

    // 裁剪
    if spec.advanced.crop_enabled {
        parts.push(format!(
            "crop={}:{}:{}:{}",
            spec.advanced.crop_w, spec.advanced.crop_h, spec.advanced.crop_x, spec.advanced.crop_y
        ));
        geometric = true;
    }

    // 缩放（分辨率 / 最长边 / 百分比）
    if let Some((w, h)) = target_dimensions(spec) {
        let algo = spec.video.scale_algo.ffmpeg_name();
        // w 或 h 为 0 表示「该维自动按比例」（-2）；两者都非 0 且开启
        // 「保持宽高比」时用 force_original_aspect_ratio 等比适配，否则精确拉伸。
        let scale = match (w, h) {
            (0, 0) => None,
            (0, hh) => Some(format!("scale=-2:{}:flags={}", hh, algo)),
            (ww, 0) => Some(format!("scale={}:-2:flags={}", ww, algo)),
            (ww, hh) if spec.video.keep_aspect => Some(format!(
                "scale={}:{}:force_original_aspect_ratio=decrease:force_divisible_by=2:flags={}",
                ww, hh, algo
            )),
            (ww, hh) => Some(format!("scale={}:{}:flags={}", ww, hh, algo)),
        };
        if let Some(s) = scale {
            parts.push(s);
            geometric = true;
        }
    } else if spec.output_category == OutputCategory::Image
        && spec.image.scale_percent != 100
        && spec.image.scale_percent > 0
    {
        // 百分比缩放：用 iw/ih 表达式（此时 target_dimensions 返回 None）
        let f = spec.image.scale_percent as f64 / 100.0;
        parts.push(format!("scale=iw*{:.3}:ih*{:.3}:flags={}", f, f, spec.video.scale_algo.ffmpeg_name()));
        geometric = true;
    }

    // 填充
    if spec.advanced.pad_enabled {
        parts.push(format!(
            "pad={}:{}:(ow-iw)/2:(oh-ih)/2:color={}",
            spec.advanced.pad_w, spec.advanced.pad_h, spec.advanced.pad_color
        ));
        geometric = true;
    }

    // 旋转 / 翻转
    let rot = match spec.advanced.rotate {
        RotateMode::None => None,
        RotateMode::Rotate90 => Some("transpose=1"),
        RotateMode::Rotate180 => Some("transpose=1,transpose=1"),
        RotateMode::Rotate270 => Some("transpose=2"),
        RotateMode::FlipH => Some("hflip"),
        RotateMode::FlipV => Some("vflip"),
    };
    if let Some(r) = rot {
        parts.push(r.to_string());
        geometric = true;
    }

    if geometric {
        parts.push("setsar=1:1".to_string());
    }

    // 去隔行
    if spec.advanced.deinterlace {
        parts.push("yadif".to_string());
    }
    // 去噪
    if spec.advanced.denoise {
        parts.push("hqdn3d".to_string());
    }
    // 锐化
    if spec.advanced.sharpen {
        parts.push("unsharp=5:5:1.5:5:5:0.0".to_string());
    }
    // HDR → SDR 色调映射
    if spec.advanced.tonemap && !spec.video.hdr_passthrough {
        parts.push("zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=bt2390,format=yuv420p".to_string());
    }
    // 色彩空间/范围修正（仅非默认时）
    if spec.video.colorspace != ColorSpace::Bt709 || spec.video.color_range != ColorRange::Tv {
        parts.push(format!(
            "scale=in_color_matrix={sm}:out_color_matrix={sm},format={rng}",
            sm = spec.video.colorspace.label(),
            rng = if spec.video.color_range == ColorRange::Pc { "yuvj420p" } else { "yuv420p" }
        ));
    }

    // 水印
    if spec.advanced.watermark_enabled && !spec.advanced.watermark_path.is_empty() {
        let opacity = if spec.advanced.watermark_opacity < 1.0 {
            format!(",format=rgba,colorchannelmixer=aa={}", spec.advanced.watermark_opacity)
        } else {
            String::new()
        };
        parts.push(format!(
            "movie='{}'{opacity}[wm];[in][wm]overlay={}",
            spec.advanced.watermark_path.replace('\\', "\\\\").replace(':', "\\:"),
            spec.advanced.watermark_pos.overlay()
        ));
    }

    // 滤镜链编辑器（C2）注入
    for f in &spec.advanced.vf_filters {
        if f.enabled && !f.name.trim().is_empty() {
            parts.push(f.to_string());
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(","))
    }
}

/// 解析用户输入的时间规格：支持秒（`12.5`）或 `HH:MM:SS[.mmm]` / `MM:SS`。
/// 用于片段截取结束时间 → 淡出起始点。
fn parse_time_spec(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        3 => {
            let h: f64 = parts[0].parse().ok()?;
            let m: f64 = parts[1].parse().ok()?;
            let sec: f64 = parts[2].parse().ok()?;
            if h < 0.0 || m < 0.0 || sec < 0.0 {
                return None;
            }
            Some(h * 3600.0 + m * 60.0 + sec)
        }
        2 => {
            let m: f64 = parts[0].parse().ok()?;
            let sec: f64 = parts[1].parse().ok()?;
            if m < 0.0 || sec < 0.0 {
                return None;
            }
            Some(m * 60.0 + sec)
        }
        1 => parts[0].parse().ok().filter(|&v| v >= 0.0),
        _ => None,
    }
}

/// 音频滤镜图（af）收集。返回 (滤镜串, 需要向用户展示的提示)。
fn build_audio_filters(spec: &ConversionSpec) -> (Option<String>, Vec<String>) {
    let mut parts: Vec<String> = Vec::new();
    let mut warns: Vec<String> = Vec::new();

    if spec.audio.volume_gain_db != 0.0 {
        parts.push(format!("volume={:.1}dB", spec.audio.volume_gain_db));
    }
    if spec.audio.fade_in_sec > 0.0 {
        parts.push(format!("afade=t=in:st=0:d={:.2}", spec.audio.fade_in_sec));
    }
    if spec.audio.fade_out_sec > 0.0 {
        // afade 只接受绝对起始时间：st = 总时长 - 淡出时长。
        // 总时长取 ffprobe 探测值，或片段截取的结束时间；两者都未知时
        // 无法生成正确命令（旧的 st=0 会让整段音频从一开始就淡出）。
        let d = spec.audio.fade_out_sec;
        let total = spec.duration_sec.or_else(|| parse_time_spec(&spec.clip.end));
        match total {
            Some(t) if t > d => {
                parts.push(format!("afade=t=out:st={:.2}:d={:.2}", t - d, d));
            }
            Some(_) => {}
            None => warns.push(
                "音频淡出需要知道总时长：请填写片段截取的结束时间，或等待文件信息探测完成后重试（本次已跳过淡出）"
                    .to_string(),
            ),
        }
    }
    if spec.advanced.audio_denoise {
        parts.push("afftdn=n=10".to_string());
    }
    for f in &spec.advanced.af_filters {
        if f.enabled && !f.name.trim().is_empty() {
            parts.push(f.to_string());
        }
    }

    let filter = if parts.is_empty() {
        None
    } else {
        Some(parts.join(","))
    };
    (filter, warns)
}

/// 计算目标宽高（0,0 表示不缩放）。
fn target_dimensions(spec: &ConversionSpec) -> Option<(u32, u32)> {
    if spec.output_category == OutputCategory::Image {
        // 图片最长边约束
        if spec.image.longest_side > 0 {
            // 仅给出最长边，另一维 -2 由 scale 自动按比例
            return Some((spec.image.longest_side, 0));
        }
        // 百分比缩放：返回 None，由 build_video_filters 用 iw/ih 表达式处理
        if spec.image.scale_percent != 100 && spec.image.scale_percent > 0 {
            return None;
        }
    }

    match spec.video.resolution {
        ResolutionPreset::Source => {
            if spec.video.custom_w > 0 || spec.video.custom_h > 0 {
                Some((spec.video.custom_w, spec.video.custom_h))
            } else {
                None
            }
        }
        ResolutionPreset::Custom => {
            if spec.video.custom_w > 0 || spec.video.custom_h > 0 {
                Some((spec.video.custom_w, spec.video.custom_h))
            } else {
                None
            }
        }
        other => other.dimensions(),
    }
}

/// 计算软件编码器的实际码率相关参数。
fn push_video_rate(args: &mut Vec<String>, spec: &ConversionSpec) {
    let v = &spec.video;
    match v.bitrate_mode {
        BitrateMode::Crf => {
            if v.codec != VideoCodec::Copy {
                args.push("-crf".into());
                args.push(v.crf.to_string());
                // VP9 / AV1 需要 -b:v 0 配合 CRF
                if matches!(v.codec, VideoCodec::LibvpxVp9 | VideoCodec::LibaomAv1) {
                    args.push("-b:v".into());
                    args.push("0".into());
                }
            }
        }
        BitrateMode::Fixed => {
            args.push("-b:v".into());
            args.push(format!("{}k", v.bitrate_kbps));
        }
        BitrateMode::Cbr => {
            let b = v.bitrate_kbps;
            args.push("-b:v".into());
            args.push(format!("{}k", b));
            args.push("-minrate".into());
            args.push(format!("{}k", b));
            args.push("-maxrate".into());
            args.push(format!("{}k", b));
            args.push("-bufsize".into());
            args.push(format!("{}k", b * 2));
        }
        BitrateMode::Vbr => {
            let b = v.bitrate_kbps;
            args.push("-b:v".into());
            args.push(format!("{}k", b));
            args.push("-maxrate".into());
            args.push(format!("{}k", b * 2));
            args.push("-bufsize".into());
            args.push(format!("{}k", b * 2));
        }
    }
}

/// 选用硬件编码器时的映射（软件编码器原样返回）。
fn resolve_video_encoder(codec: VideoCodec, hw: HwAccelPreference) -> (String, bool) {
    // 返回 (编码器名, 是否需要 hw 上传滤镜)
    match hw {
        // 软件编码（自动 / 强制软件 / 仅解码加速后端）
        HwAccelPreference::Software
        | HwAccelPreference::Auto
        | HwAccelPreference::CudaDecode
        | HwAccelPreference::Dxva2
        | HwAccelPreference::D3d11va
        | HwAccelPreference::Vulkan
        | HwAccelPreference::Opencl => (codec.ffmpeg_name().to_string(), false),
        HwAccelPreference::Nvenc => {
            let name = match codec {
                VideoCodec::Libx265 => "hevc_nvenc",
                VideoCodec::LibaomAv1 => "av1_nvenc",
                _ => "h264_nvenc",
            };
            (name.to_string(), false)
        }
        HwAccelPreference::Vaapi => {
            let name = match codec {
                VideoCodec::Libx265 => "hevc_vaapi",
                VideoCodec::LibvpxVp9 => "vp9_vaapi",
                VideoCodec::LibaomAv1 => "av1_vaapi",
                _ => "h264_vaapi",
            };
            (name.to_string(), true)
        }
        HwAccelPreference::Qsv => {
            // QSV 编码器接受系统内存帧（内部自动上传），无需 hwupload 链
            let name = match codec {
                VideoCodec::Libx265 => "hevc_qsv",
                VideoCodec::LibaomAv1 => "av1_qsv",
                _ => "h264_qsv",
            };
            (name.to_string(), false)
        }
        HwAccelPreference::Amf => {
            let name = match codec {
                VideoCodec::Libx265 => "hevc_amf",
                VideoCodec::LibaomAv1 => "av1_amf",
                _ => "h264_amf",
            };
            (name.to_string(), false)
        }
        HwAccelPreference::Videotoolbox => {
            let name = match codec {
                VideoCodec::Libx265 => "hevc_videotoolbox",
                _ => "h264_videotoolbox",
            };
            (name.to_string(), false)
        }
    }
}

/// 写入解码端硬件加速参数。
fn push_hwaccel_decode(args: &mut Vec<String>, hw: HwAccelPreference) {
    match hw {
        HwAccelPreference::Vaapi => {
            // 解码帧下载到系统内存（不设 -hwaccel_output_format），
            // 滤镜在软件帧上运行，再由 hwupload 上传给编码器。
            args.push("-hwaccel".into());
            args.push("vaapi".into());
            args.push("-hwaccel_device".into());
            args.push("/dev/dri/renderD128".into());
        }
        HwAccelPreference::Nvenc | HwAccelPreference::CudaDecode => {
            args.push("-hwaccel".into());
            args.push("cuda".into());
        }
        HwAccelPreference::Qsv => {
            args.push("-hwaccel".into());
            args.push("qsv".into());
        }
        HwAccelPreference::Videotoolbox => {
            args.push("-hwaccel".into());
            args.push("videotoolbox".into());
        }
        HwAccelPreference::Dxva2 => {
            args.push("-hwaccel".into());
            args.push("dxva2".into());
        }
        HwAccelPreference::D3d11va => {
            args.push("-hwaccel".into());
            args.push("d3d11va".into());
        }
        HwAccelPreference::Vulkan => {
            args.push("-hwaccel".into());
            args.push("vulkan".into());
        }
        HwAccelPreference::Opencl => {
            args.push("-hwaccel".into());
            args.push("opencl".into());
        }
        _ => {}
    }
}

/// 写入输出容器的公共参数（元数据 / faststart / 线程）。
fn push_container_options(args: &mut Vec<String>, spec: &ConversionSpec) {
    let a = &spec.advanced;
    if a.faststart && spec.output_format == ContainerFormat::Mp4 {
        args.push("-movflags".into());
        args.push("+faststart".into());
    }
    if a.threads > 0 {
        args.push("-threads".into());
        args.push(a.threads.to_string());
    }
}

/// 需要硬件上传（VAAPI）时，把用户滤镜链末尾补上 `format=nv12,hwupload`；
/// 没有任何用户滤镜时也强制补一条，否则硬件编码器收到系统帧会直接报错。
fn push_hw_upload_vf(args: &mut Vec<String>, needs_upload: bool, user_vf: Option<String>) {
    let vf = match user_vf {
        Some(mut v) => {
            if needs_upload {
                v.push_str(",format=nv12,hwupload");
            }
            Some(v)
        }
        None => {
            if needs_upload {
                Some("format=nv12,hwupload".to_string())
            } else {
                None
            }
        }
    };
    if let Some(v) = vf {
        args.push("-vf".into());
        args.push(v);
    }
}

// ---------------------------------------------------------------------------
// 各模式实现
// ---------------------------------------------------------------------------

fn output_path(spec: &ConversionSpec) -> String {
    let dir = spec.output_dir.trim();
    let name = spec.output_filename.trim();
    let ext = spec.output_format.extension();
    let filename = if name.is_empty() {
        format!("output.{}", ext)
    } else if name.ends_with(&format!(".{}", ext)) {
        name.to_string()
    } else {
        format!("{}.{}", name, ext)
    };
    if dir.is_empty() {
        filename
    } else {
        format!("{}/{}", dir.trim_end_matches('/'), filename)
    }
}

/// 单文件 / 视频转视频 / 音频 / 图片单张。
fn build_single(spec: &ConversionSpec, _is_second_pass: bool) -> Result<CommandPlan, String> {
    let mut plan = CommandPlan::default();
    let out = output_path(spec);
    let program = "ffmpeg".to_string();

    // 2-Pass 仅对「视频输出 + 非 Copy」有意义；硬件编码器（NVENC/VAAPI/QSV/AMF/
    // VideoToolbox）不支持 -pass 双遍流程，按 UI 承诺自动回退到软件编码器。
    // 注意：副本默认勾选了两遍编码时，需要让「自动选择」回退后的偏好与预览一致。
    let two_pass = spec.advanced.two_pass
        && spec.output_category == OutputCategory::Video
        && spec.video.codec != VideoCodec::Copy;
    let hw_enc_backend = matches!(
        spec.hw,
        HwAccelPreference::Nvenc
            | HwAccelPreference::Vaapi
            | HwAccelPreference::Qsv
            | HwAccelPreference::Amf
            | HwAccelPreference::Videotoolbox
    );
    let eff_hw = if two_pass && hw_enc_backend {
        plan.warnings.push(
            "2-Pass 编码不支持硬件编码器，已自动回退到软件编码（如 libx264）".to_string(),
        );
        HwAccelPreference::Software
    } else {
        spec.hw
    };
    // 解析视频编码器与是否需要硬件上传滤镜（供下方视频分支与滤镜块共用）
    let (enc, needs_upload) = resolve_video_encoder(spec.video.codec, eff_hw);
    // 音频滤镜在两条 pass 中相同，提前计算避免重复告警
    let (audio_filter, audio_warnings) = build_audio_filters(spec);

    // 2-Pass：第一条仅做分析
    let passes: Vec<Option<u32>> = if two_pass {
        vec![Some(1), Some(2)]
    } else {
        vec![None]
    };

    for pass in passes {
        let mut args = vec![program.clone()];
        // 全局自定义参数（ffmpeg 之后）
        if !spec.custom.global.trim().is_empty() {
            args.extend(split_args(&spec.custom.global));
        }
        // 覆盖写
        args.push("-y".into());
        // 解码硬件加速（2-Pass 回退后为 Software，不写任何 -hwaccel）
        push_hwaccel_decode(&mut args, eff_hw);
        // 输入自定义参数（-i 之前）
        if !spec.custom.input.trim().is_empty() {
            args.extend(split_args(&spec.custom.input));
        }
        // 片段截取
        if spec.clip.enabled {
            if !spec.clip.start.trim().is_empty() {
                args.push("-ss".into());
                args.push(spec.clip.start.trim().to_string());
            }
            if !spec.clip.end.trim().is_empty() {
                args.push("-to".into());
                args.push(spec.clip.end.trim().to_string());
            }
        }
        // 输入文件
        args.push("-i".into());
        args.push(spec.inputs[0].path.clone());

        // 输出自定义参数（置于图形参数之前 → 图形优先）
        if !spec.custom.output.trim().is_empty() {
            args.extend(split_args(&spec.custom.output));
        }

        // 视频流
        if spec.output_category == OutputCategory::Audio {
            args.push("-vn".into());
        } else if spec.output_category == OutputCategory::Image {
            // 图片由下方单独处理
        } else {
            args.push("-c:v".into());
            args.push(enc.clone());
            if spec.video.codec != VideoCodec::Copy {
                push_video_rate(&mut args, spec);
                // 预设 / tune / profile / level
                let a = &spec.advanced;
                if !a.preset.is_empty() {
                    args.push("-preset".into());
                    args.push(a.preset.clone());
                }
                if !a.tune.is_empty() {
                    args.push("-tune".into());
                    args.push(a.tune.clone());
                }
                if !a.profile.is_empty() {
                    args.push("-profile:v".into());
                    args.push(a.profile.clone());
                }
                if !a.level.is_empty() {
                    args.push("-level:v".into());
                    args.push(a.level.clone());
                }
                // 像素格式
                if !a.pix_fmt.is_empty() {
                    args.push("-pix_fmt".into());
                    args.push(a.pix_fmt.clone());
                } else if spec.video.codec == VideoCodec::Libx264
                    || spec.video.codec == VideoCodec::Libx265
                {
                    args.push("-pix_fmt".into());
                    args.push("yuv420p".into());
                }
                // 帧率
                if spec.video.fps != FpsPreset::Source {
                    let r = match spec.video.fps {
                        FpsPreset::Custom => spec.video.custom_fps,
                        FpsPreset::F24 => 24.0,
                        FpsPreset::F25 => 25.0,
                        FpsPreset::F30 => 30.0,
                        FpsPreset::F50 => 50.0,
                        FpsPreset::F60 => 60.0,
                        _ => 0.0,
                    };
                    if r > 0.0 {
                        args.push("-r".into());
                        args.push(r.to_string());
                    }
                }
                // 2-Pass 标记
                if let Some(p) = pass {
                    args.push("-pass".into());
                    args.push(p.to_string());
                }
            }
        }

        // 音频流
        if spec.output_category == OutputCategory::Image {
            args.push("-an".into());
        } else {
            args.push("-c:a".into());
            args.push(spec.audio.codec.ffmpeg_name().into());
            if spec.audio.codec != AudioCodec::Copy && !spec.audio.codec.is_lossless() {
                args.push("-b:a".into());
                args.push(format!("{}k", spec.audio.bitrate_kbps));
            }
            if let Some(ch) = spec.audio.channels.value() {
                args.push("-ac".into());
                args.push(ch.to_string());
            }
            if let Some(sr) = spec.audio.sample_rate.value() {
                args.push("-ar".into());
                args.push(sr.to_string());
            }
        }

        // 视频滤镜（VAAPI 编码必须 hwupload 上传；无用户滤镜时也强制补一条）
        if spec.output_category == OutputCategory::Video {
            if let Some(mut vf) = build_video_filters(spec) {
                if needs_upload {
                    vf.push_str(",format=nv12,hwupload");
                }
                args.push("-vf".into());
                args.push(vf);
            } else if needs_upload {
                args.push("-vf".into());
                args.push("format=nv12,hwupload".to_string());
            }
        }

        // 音频滤镜（视频 / 纯音频输出都适用；图片输出无音轨）
        if spec.output_category != OutputCategory::Image {
            if let Some(af) = &audio_filter {
                args.push("-af".into());
                args.push(af.clone());
            }
        }

        // 图片专属
        if spec.output_category == OutputCategory::Image {
            if spec.output_format.is_animated() {
                // GIF 不在单张逻辑里处理
            } else {
                // 单张：取第一帧
                args.push("-frames:v".into());
                args.push("1".into());
                // 质量 / 压缩
                if matches!(spec.output_format, ContainerFormat::Jpg | ContainerFormat::Webp | ContainerFormat::Avif) {
                    args.push("-q:v".into());
                    args.push(spec.image.quality.to_string());
                }
                if spec.output_format == ContainerFormat::Png {
                    args.push("-compression_level".into());
                    args.push(spec.image.compression_level.to_string());
                }
                if spec.image.strip_metadata {
                    args.push("-map_metadata".into());
                    args.push("-1".into());
                }
                if let Some(vf) = build_video_filters(spec) {
                    args.push("-vf".into());
                    args.push(vf);
                }
            }
        }

        // 容器格式 / 公共选项
        push_container_options(&mut args, spec);
        if let Some(f) = spec.output_format.force_format() {
            args.push("-f".into());
            args.push(f.into());
        }

        // 2-Pass 第一遍不需要输出文件
        if pass == Some(1) {
            args.push("-f".into());
            args.push("null".into());
            args.push("-".into());
        } else {
            args.push(out.clone());
        }

        let display = {
            let mut s = program.clone();
            for a in &args[1..] {
                s.push(' ');
                if a.contains(' ') {
                    s.push_str(&format!("\"{}\"", a));
                } else {
                    s.push_str(a);
                }
            }
            s
        };
        plan.commands.push(Command {
            program: program.clone(),
            args: args[1..].to_vec(),
            display,
            output: if pass == Some(1) { String::new() } else { out.clone() },
        });
    }

    plan.warnings.extend(audio_warnings);

    Ok(plan)
}

/// 拼接：利用 concat demuxer。
fn build_concat(spec: &ConversionSpec) -> Result<CommandPlan, String> {
    let mut plan = CommandPlan::default();
    let out = output_path(spec);

    let mut list = String::from("ffconcat version 1.0\n");
    for inp in &spec.inputs {
        list.push_str(&format!("file '{}'\n", inp.path.replace('\\', "\\\\").replace('\'', "\\'")));
    }

    // 列表文件放在输出目录（未指定输出目录时放当前目录），
    // 命令、运行时写出与导出的 Shell 脚本都使用同一路径。
    let concat_path = if spec.output_dir.trim().is_empty() {
        "_linbox_concat.txt".to_string()
    } else {
        format!("{}/_linbox_concat.txt", spec.output_dir.trim().trim_end_matches('/'))
    };

    let mut args = vec!["ffmpeg".to_string(), "-y".to_string()];
    if !spec.custom.global.trim().is_empty() {
        args.extend(split_args(&spec.custom.global));
    }
    push_hwaccel_decode(&mut args, spec.hw);
    args.push("-f".into());
    args.push("concat".into());
    args.push("-safe".into());
    args.push("0".into());
    args.push("-i".into());
    args.push(concat_path.clone());

    if !spec.custom.output.trim().is_empty() {
        args.extend(split_args(&spec.custom.output));
    }

    if spec.output_category != OutputCategory::Audio {
        let (enc, needs_upload) = resolve_video_encoder(spec.video.codec, spec.hw);
        args.push("-c:v".into());
        args.push(enc);
        if spec.video.codec != VideoCodec::Copy {
            push_video_rate(&mut args, spec);
        }
        if spec.output_category == OutputCategory::Video {
            push_hw_upload_vf(&mut args, needs_upload, build_video_filters(spec));
        }
    } else {
        args.push("-vn".into());
    }
    args.push("-c:a".into());
    args.push(spec.audio.codec.ffmpeg_name().into());

    push_container_options(&mut args, spec);
    if let Some(f) = spec.output_format.force_format() {
        args.push("-f".into());
        args.push(f.into());
    }
    args.push(out.clone());

    let display = join_display(&args);
    plan.commands.push(Command {
        program: "ffmpeg".to_string(),
        args: args[1..].to_vec(),
        display,
        output: out,
    });
    plan.concat_list = Some(list);
    plan.concat_path = concat_path;
    Ok(plan)
}

/// 拆条：按固定时长分段输出。
fn build_split(spec: &ConversionSpec) -> Result<CommandPlan, String> {
    let mut plan = CommandPlan::default();
    let dir = spec.output_dir.trim();
    let name = spec.output_filename.trim();
    let ext = spec.output_format.extension();
    let base = if name.is_empty() { "segment" } else { name };
    let template = if dir.is_empty() {
        format!("{}_%03d.{}", base, ext)
    } else {
        format!("{}/{}_%03d.{}", dir.trim_end_matches('/'), base, ext)
    };

    let mut args = vec!["ffmpeg".to_string(), "-y".to_string()];
    if !spec.custom.global.trim().is_empty() {
        args.extend(split_args(&spec.custom.global));
    }
    push_hwaccel_decode(&mut args, spec.hw);
    if !spec.custom.input.trim().is_empty() {
        args.extend(split_args(&spec.custom.input));
    }
    if spec.clip.enabled && !spec.clip.start.trim().is_empty() {
        args.push("-ss".into());
        args.push(spec.clip.start.trim().to_string());
    }
    args.push("-i".into());
    args.push(spec.inputs[0].path.clone());

    if !spec.custom.output.trim().is_empty() {
        args.extend(split_args(&spec.custom.output));
    }

    if spec.output_category != OutputCategory::Audio {
        let (enc, needs_upload) = resolve_video_encoder(spec.video.codec, spec.hw);
        args.push("-c:v".into());
        args.push(enc);
        if spec.video.codec != VideoCodec::Copy {
            push_video_rate(&mut args, spec);
        }
        if spec.output_category == OutputCategory::Video {
            push_hw_upload_vf(&mut args, needs_upload, build_video_filters(spec));
        }
    } else {
        args.push("-vn".into());
    }
    args.push("-c:a".into());
    args.push(spec.audio.codec.ffmpeg_name().into());

    // 分段时长（默认 60s；可后续 expose 为 UI 控件）
    args.push("-f".into());
    args.push("segment".into());
    args.push("-segment_time".into());
    args.push("60".into());
    args.push("-reset_timestamps".into());
    args.push("1".into());
    push_container_options(&mut args, spec);
    args.push(template.clone());

    let display = join_display(&args);
    plan.commands.push(Command {
        program: "ffmpeg".to_string(),
        args: args[1..].to_vec(),
        display,
        output: template,
    });
    Ok(plan)
}

/// 视频 → 图片序列帧。
fn build_image_extract(spec: &ConversionSpec) -> Result<CommandPlan, String> {
    let dir = spec.output_dir.trim();
    let name = spec.output_filename.trim();
    let ext = spec.output_format.extension();
    let base = if name.is_empty() { "frame" } else { name };
    let template = if dir.is_empty() {
        format!("{}_%04d.{}", base, ext)
    } else {
        format!("{}/{}_%04d.{}", dir.trim_end_matches('/'), base, ext)
    };

    let mut args = vec!["ffmpeg".to_string(), "-y".to_string()];
    if !spec.custom.global.trim().is_empty() {
        args.extend(split_args(&spec.custom.global));
    }
    if spec.clip.enabled && !spec.clip.start.trim().is_empty() {
        args.push("-ss".into());
        args.push(spec.clip.start.trim().to_string());
    }
    args.push("-i".into());
    args.push(spec.inputs[0].path.clone());

    if !spec.custom.output.trim().is_empty() {
        args.extend(split_args(&spec.custom.output));
    }

    args.push("-an".into());
    // fps 抽取滤镜
    let mut vf = format!("fps={}", spec.image.extract_fps);
    if spec.image.longest_side > 0 {
        vf.push_str(&format!(",scale={}:-1", spec.image.longest_side));
    }
    if spec.image.strip_metadata {
        vf.push_str(",format=rgb24");
    }
    args.push("-vf".into());
    args.push(vf);
    args.push(template.clone());

    let display = join_display(&args);
    let mut plan = CommandPlan::default();
    plan.commands.push(Command {
        program: "ffmpeg".to_string(),
        args: args[1..].to_vec(),
        display,
        output: template,
    });
    Ok(plan)
}

/// 图片 → 视频：把用户给出的首张图片推导为 ffmpeg 序列模式。
/// 文件名含数字编号时替换为 %0Nd（如 frame_0001.png → frame_%04d.png）；
/// 否则原样使用（只有一帧，并给出提示）。返回 (模式, 是否推导成功)。
fn sequence_pattern(input: &str) -> (String, bool) {
    let p = std::path::Path::new(input);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default();
    // 找主名末尾的数字段（按字符边界计算字节偏移）
    let digits_start = stem
        .char_indices()
        .filter(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i + 1)
        .last()
        .unwrap_or(0);
    let tail = &stem[digits_start..];
    if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
        let prefix = &stem[..digits_start];
        let name = format!("{}%0{}d.{}", prefix, tail.len(), ext);
        let dir = p
            .parent()
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_default();
        let pattern = if dir.is_empty() {
            name
        } else {
            format!("{}/{}", dir.trim_end_matches('/'), name)
        };
        (pattern, true)
    } else {
        (input.to_string(), false)
    }
}

/// 图片序列帧 → 视频。
fn build_image_to_video(spec: &ConversionSpec) -> Result<CommandPlan, String> {
    let out = output_path(spec);
    let mut plan = CommandPlan::default();
    let mut args = vec!["ffmpeg".to_string(), "-y".to_string()];
    if !spec.custom.global.trim().is_empty() {
        args.extend(split_args(&spec.custom.global));
    }
    args.push("-framerate".into());
    args.push(spec.image.extract_fps.to_string());
    args.push("-i".into());
    let first = spec.inputs[0].path.clone();
    let (pattern, derived) = sequence_pattern(&first);
    if !derived {
        plan.warnings.push(format!(
            "输入文件「{}」不含数字编号，无法推导序列模式（如 frame_%04d.png）；将只编码这一张图片",
            std::path::Path::new(&first)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| first.clone())
        ));
    }
    args.push(pattern);

    if !spec.custom.output.trim().is_empty() {
        args.extend(split_args(&spec.custom.output));
    }

    let (enc, _) = resolve_video_encoder(spec.video.codec, spec.hw);
    args.push("-c:v".into());
    args.push(enc);
    if spec.video.codec != VideoCodec::Copy {
        push_video_rate(&mut args, spec);
    }
    args.push("-pix_fmt".into());
    args.push("yuv420p".into());
    push_container_options(&mut args, spec);
    args.push(out.clone());

    let display = join_display(&args);
    plan.commands.push(Command {
        program: "ffmpeg".to_string(),
        args: args[1..].to_vec(),
        display,
        output: out,
    });
    Ok(plan)
}

/// 视频 → GIF（降分辨率 + palettegen/paletteuse 双滤镜）。
fn build_video_to_gif(spec: &ConversionSpec) -> Result<CommandPlan, String> {
    let out = output_path(spec);
    let dir = spec.output_dir.trim();
    let name = spec.output_filename.trim();
    let base = if name.is_empty() { "palette" } else { name };
    let palette = if dir.is_empty() {
        format!("{}_palette.png", base)
    } else {
        format!("{}/{}_palette.png", dir.trim_end_matches('/'), base)
    };

    // 第一遍：生成调色板
    let mut p1 = vec!["ffmpeg".to_string(), "-y".to_string()];
    if spec.clip.enabled && !spec.clip.start.trim().is_empty() {
        p1.push("-ss".into());
        p1.push(spec.clip.start.trim().to_string());
    }
    p1.push("-i".into());
    p1.push(spec.inputs[0].path.clone());
    let w = if spec.image.gif_width > 0 { spec.image.gif_width } else { 480 };
    let fps = if spec.image.gif_fps > 0.0 { spec.image.gif_fps } else { 15.0 };
    p1.push("-vf".into());
    p1.push(format!("fps={},scale={}:-1:flags=lanczos,palettegen", fps, w));
    p1.push(palette.clone());

    // 第二遍：合成 GIF（-ss 放在 -i 之前，与第一遍同样是输入定位，保证两遍取同一段画面）
    let mut p2 = vec!["ffmpeg".to_string(), "-y".to_string()];
    if spec.clip.enabled && !spec.clip.start.trim().is_empty() {
        p2.push("-ss".into());
        p2.push(spec.clip.start.trim().to_string());
    }
    p2.push("-i".into());
    p2.push(spec.inputs[0].path.clone());
    p2.push("-i".into());
    p2.push(palette.clone());
    p2.push("-lavfi".into());
    p2.push(format!(
        "fps={},scale={}:-1:flags=lanczos[x];[x][1:v]paletteuse",
        fps, w
    ));
    p2.push(out.clone());

    let mut plan = CommandPlan::default();
    plan.commands.push(Command {
        program: "ffmpeg".to_string(),
        args: p1[1..].to_vec(),
        display: join_display(&p1),
        output: palette,
    });
    plan.commands.push(Command {
        program: "ffmpeg".to_string(),
        args: p2[1..].to_vec(),
        display: join_display(&p2),
        output: out,
    });
    Ok(plan)
}

fn join_display(args: &[String]) -> String {
    let mut s = args[0].clone();
    for a in &args[1..] {
        s.push(' ');
        if a.contains(' ') {
            s.push_str(&format!("\"{}\"", a));
        } else {
            s.push_str(a);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_spec() -> ConversionSpec {
        ConversionSpec {
            inputs: vec![InputSpec {
                path: "input.mp4".into(),
            }],
            output_format: ContainerFormat::Mp4,
            output_dir: "/tmp".into(),
            output_filename: "out".into(),
            ..Default::default()
        }
    }

    #[test]
    fn builds_basic_mp4_h264_crf() {
        let spec = base_spec();
        let plan = build_commands(&spec).unwrap();
        assert_eq!(plan.commands.len(), 1);
        let args = &plan.commands[0].args;
        assert!(args.contains(&"libx264".to_string()));
        assert!(args.contains(&"-crf".to_string()));
        assert!(args.contains(&"23".to_string()));
        assert!(args.contains(&"yuv420p".to_string()));
        assert!(plan.commands[0].output.ends_with("out.mp4"));
    }

    #[test]
    fn cbr_sets_min_max_bufsize() {
        let mut spec = base_spec();
        spec.video.bitrate_mode = BitrateMode::Cbr;
        spec.video.bitrate_kbps = 2000;
        let plan = build_commands(&spec).unwrap();
        let args = &plan.commands[0].args;
        assert!(args.contains(&"-minrate".to_string()));
        assert!(args.contains(&"2000k".to_string()));
    }

    #[test]
    fn two_pass_produces_two_commands() {
        let mut spec = base_spec();
        spec.advanced.two_pass = true;
        let plan = build_commands(&spec).unwrap();
        assert_eq!(plan.commands.len(), 2);
        assert!(plan.commands[0].args.contains(&"1".to_string()));
        assert!(plan.commands[1].args.contains(&"2".to_string()));
    }

    #[test]
    fn vaapi_switches_encoder_and_hwflags() {
        let mut spec = base_spec();
        spec.hw = HwAccelPreference::Vaapi;
        let plan = build_commands(&spec).unwrap();
        let args = &plan.commands[0].args;
        assert!(args.contains(&"h264_vaapi".to_string()));
        assert!(args.contains(&"vaapi".to_string()));
        assert!(args.contains(&"-hwaccel".to_string()));
    }

    #[test]
    fn concat_builds_list_and_command() {
        let mut spec = base_spec();
        spec.mode = JobMode::Concat;
        spec.inputs = vec![
            InputSpec { path: "a.mp4".into() },
            InputSpec { path: "b.mp4".into() },
        ];
        let plan = build_commands(&spec).unwrap();
        assert!(plan.concat_list.is_some());
        assert!(plan.concat_list.unwrap().contains("file 'a.mp4'"));
        // concat 列表路径与命令保持一致（放在输出目录下，而非 CWD）
        assert_eq!(plan.concat_path, "/tmp/_linbox_concat.txt");
        assert!(plan.commands[0].args.contains(&plan.concat_path.clone()));
    }

    #[test]
    fn video_to_gif_two_pass_palette() {
        let mut spec = base_spec();
        spec.mode = JobMode::VideoToGif;
        spec.output_format = ContainerFormat::Gif;
        let plan = build_commands(&spec).unwrap();
        assert_eq!(plan.commands.len(), 2);
        assert!(plan.commands[0].args.iter().any(|a| a.contains("palettegen")));
        assert!(plan.commands[1].args.iter().any(|a| a.contains("paletteuse")));
    }

    /// 取 args 中 key 的下一项（-vf / -af / -c:v 后面的值）。
    fn arg_after<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
        args.iter()
            .position(|a| a == key)
            .and_then(|i| args.get(i + 1))
            .map(|s| s.as_str())
    }

    #[test]
    fn image_percent_scale_uses_iw_expression() {
        let mut spec = base_spec();
        spec.output_category = OutputCategory::Image;
        spec.output_format = ContainerFormat::Png;
        spec.image.scale_percent = 50;
        let plan = build_commands(&spec).unwrap();
        let vf = arg_after(&plan.commands[0].args, "-vf").expect("图片缩放应有 -vf");
        assert!(vf.contains("scale=iw*0.500"), "vf={vf}");
        assert!(!vf.contains("scale=-2:0"), "不得再生成 scale=-2:0");
    }

    #[test]
    fn keep_aspect_uses_force_original_aspect_ratio() {
        let mut spec = base_spec();
        spec.video.resolution = ResolutionPreset::R1080;
        spec.video.keep_aspect = true;
        let plan = build_commands(&spec).unwrap();
        let vf = arg_after(&plan.commands[0].args, "-vf").expect("应有 -vf");
        assert!(vf.contains("force_original_aspect_ratio=decrease"), "vf={vf}");
        // 保持宽高比时不再硬写 (w,-2) 丢弃目标高度
        assert!(!vf.contains("scale=1920:-2"), "vf={vf}");
    }

    #[test]
    fn two_pass_with_hardware_falls_back_to_software() {
        let mut spec = base_spec();
        spec.advanced.two_pass = true;
        spec.hw = HwAccelPreference::Nvenc;
        let plan = build_commands(&spec).unwrap();
        assert_eq!(plan.commands.len(), 2);
        for cmd in &plan.commands {
            assert!(cmd.args.contains(&"libx264".to_string()));
            assert!(!cmd.args.contains(&"h264_nvenc".to_string()));
            assert!(!cmd.args.contains(&"-hwaccel".to_string()));
        }
        assert!(
            plan.warnings.iter().any(|w| w.contains("回退")),
            "应有硬件回退提示：{:?}",
            plan.warnings
        );
    }

    #[test]
    fn vaapi_always_has_hwupload_chain() {
        let mut spec = base_spec();
        spec.hw = HwAccelPreference::Vaapi;
        let plan = build_commands(&spec).unwrap();
        let args = &plan.commands[0].args;
        let vf = arg_after(args, "-vf").expect("VAAPI 无用户滤镜也应生成上传链");
        assert!(vf.contains("hwupload"), "vf={vf}");
        assert!(!vf.contains("hwaccel_output_format"), "不得与 -hwaccel 冲突");
    }

    #[test]
    fn gif_second_pass_seeks_before_input() {
        let mut spec = base_spec();
        spec.mode = JobMode::VideoToGif;
        spec.output_format = ContainerFormat::Gif;
        spec.clip.enabled = true;
        spec.clip.start = "5".into();
        let plan = build_commands(&spec).unwrap();
        let args = &plan.commands[1].args;
        let ss = args.iter().position(|a| a == "-ss").expect("应有 -ss");
        let input = args.iter().position(|a| a == "-i").expect("应有 -i");
        assert!(ss < input, "-ss 应在 -i 之前（输入定位）：{args:?}");
        assert_eq!(args[ss + 1], "5");
    }

    #[test]
    fn image_to_video_derives_sequence_pattern() {
        let mut spec = base_spec();
        spec.mode = JobMode::ImageToVideo;
        spec.inputs[0].path = "frames/frame_0001.png".into();
        let plan = build_commands(&spec).unwrap();
        assert!(
            plan.commands[0].args.contains(&"frames/frame_%04d.png".to_string()),
            "应把首帧推导为序列模式：{:?}",
            plan.commands[0].args
        );
    }

    #[test]
    fn fade_out_anchored_to_total_duration() {
        let mut spec = base_spec();
        spec.duration_sec = Some(100.0);
        spec.audio.fade_out_sec = 5.0;
        let plan = build_commands(&spec).unwrap();
        let af = arg_after(&plan.commands[0].args, "-af").expect("应有 -af");
        assert!(af.contains("afade=t=out:st=95.00:d=5.00"), "af={af}");
        assert!(!af.contains("st=0"), "淡出不得从 0s 开始：af={af}");
    }

    #[test]
    fn fade_out_without_duration_skipped_with_warning() {
        let mut spec = base_spec();
        spec.audio.fade_out_sec = 5.0;
        let plan = build_commands(&spec).unwrap();
        assert!(
            arg_after(&plan.commands[0].args, "-af").is_none(),
            "时长未知时不应生成错误的淡出命令"
        );
        assert!(plan.warnings.iter().any(|w| w.contains("淡出")));
    }

    #[test]
    fn audio_filters_apply_for_audio_output() {
        let mut spec = base_spec();
        spec.output_category = OutputCategory::Audio;
        spec.output_format = ContainerFormat::Mp3;
        spec.audio.volume_gain_db = 6.0;
        let plan = build_commands(&spec).unwrap();
        let af = arg_after(&plan.commands[0].args, "-af").expect("音频输出必须应用 -af");
        assert!(af.contains("volume=6.0dB"), "af={af}");
    }

    #[test]
    fn shell_script_escapes_spaces() {
        let mut spec = base_spec();
        spec.inputs[0].path = "my video.mp4".into();
        let plan = build_commands(&spec).unwrap();
        let sh = to_shell_script(&plan, false);
        assert!(sh.contains("\"my video.mp4\""));
    }
}
