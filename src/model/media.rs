//! 媒体转换相关的纯数据模型。
//!
//! 本模块不依赖任何 GTK / libadwaita / glib，仅定义 `struct`/`enum` 与少量
//! 与 ffmpeg 参数对应的辅助方法。它被 `utils::media`（逻辑层）与 `page`
//! （展示层）共同引用，是「展示与逻辑分离」之间的契约层。

use std::fmt;

/// 输出大类：决定使用哪一组参数与封装格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputCategory {
    /// 视频（含封装盒与可选音轨）。
    #[default]
    Video,
    /// 纯音频。
    Audio,
    /// 静态图 / 动图 / 序列帧。
    Image,
}

impl OutputCategory {
    /// 侧边栏 / 下拉展示用的中文标签。
    pub fn label(&self) -> &'static str {
        match self {
            OutputCategory::Video => "视频",
            OutputCategory::Audio => "音频",
            OutputCategory::Image => "图片",
        }
    }

    /// 该大类下所有可选封装格式。
    pub fn formats(&self) -> &'static [ContainerFormat] {
        match self {
            OutputCategory::Video => &[
                ContainerFormat::Mp4,
                ContainerFormat::Mkv,
                ContainerFormat::Webm,
                ContainerFormat::Mov,
                ContainerFormat::Avi,
                ContainerFormat::Ts,
                ContainerFormat::Flv,
                ContainerFormat::OggV,
            ],
            OutputCategory::Audio => &[
                ContainerFormat::Mp3,
                ContainerFormat::Aac,
                ContainerFormat::OggA,
                ContainerFormat::Flac,
                ContainerFormat::Wav,
                ContainerFormat::Opus,
            ],
            OutputCategory::Image => &[
                ContainerFormat::Jpg,
                ContainerFormat::Png,
                ContainerFormat::Webp,
                ContainerFormat::Avif,
                ContainerFormat::Bmp,
                ContainerFormat::Gif,
            ],
        }
    }
}

/// 封装格式（视频盒 / 音频盒 / 图片编码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerFormat {
    // 视频
    Mp4,
    Mkv,
    Webm,
    Mov,
    Avi,
    Ts,
    Flv,
    OggV,
    // 音频
    Mp3,
    Aac,
    OggA,
    Flac,
    Wav,
    Opus,
    // 图片
    Jpg,
    Png,
    Webp,
    Avif,
    Bmp,
    Gif,
}

impl ContainerFormat {
    /// 文件扩展名（小写）。
    pub fn extension(&self) -> &'static str {
        match self {
            ContainerFormat::Mp4 => "mp4",
            ContainerFormat::Mkv => "mkv",
            ContainerFormat::Webm => "webm",
            ContainerFormat::Mov => "mov",
            ContainerFormat::Avi => "avi",
            ContainerFormat::Ts => "ts",
            ContainerFormat::Flv => "flv",
            ContainerFormat::OggV => "ogv",
            ContainerFormat::Mp3 => "mp3",
            ContainerFormat::Aac => "m4a",
            ContainerFormat::OggA => "ogg",
            ContainerFormat::Flac => "flac",
            ContainerFormat::Wav => "wav",
            ContainerFormat::Opus => "opus",
            ContainerFormat::Jpg => "jpg",
            ContainerFormat::Png => "png",
            ContainerFormat::Webp => "webp",
            ContainerFormat::Avif => "avif",
            ContainerFormat::Bmp => "bmp",
            ContainerFormat::Gif => "gif",
        }
    }

    /// ffmpeg `-f` 强制格式标识（部分格式需要显式指定）。
    pub fn force_format(&self) -> Option<&'static str> {
        match self {
            ContainerFormat::Mp4 => Some("mp4"),
            ContainerFormat::Mkv => Some("matroska"),
            ContainerFormat::Webm => Some("webm"),
            ContainerFormat::Mov => Some("mov"),
            ContainerFormat::Avi => Some("avi"),
            ContainerFormat::Ts => Some("mpegts"),
            ContainerFormat::Flv => Some("flv"),
            ContainerFormat::OggV => Some("ogg"),
            ContainerFormat::Mp3 => Some("mp3"),
            ContainerFormat::Aac => Some("ipod"),
            ContainerFormat::OggA => Some("ogg"),
            ContainerFormat::Flac => Some("flac"),
            ContainerFormat::Wav => Some("wav"),
            ContainerFormat::Opus => Some("opus"),
            ContainerFormat::Jpg => Some("image2"),
            ContainerFormat::Png => Some("image2"),
            ContainerFormat::Webp => Some("image2"),
            ContainerFormat::Avif => Some("image2"),
            ContainerFormat::Bmp => Some("image2"),
            ContainerFormat::Gif => Some("gif"),
        }
    }

    /// 展示标签。
    pub fn label(&self) -> &'static str {
        match self {
            ContainerFormat::Mp4 => "MP4",
            ContainerFormat::Mkv => "MKV",
            ContainerFormat::Webm => "WebM",
            ContainerFormat::Mov => "MOV",
            ContainerFormat::Avi => "AVI",
            ContainerFormat::Ts => "TS",
            ContainerFormat::Flv => "FLV",
            ContainerFormat::OggV => "OGG",
            ContainerFormat::Mp3 => "MP3",
            ContainerFormat::Aac => "AAC",
            ContainerFormat::OggA => "OGG",
            ContainerFormat::Flac => "FLAC",
            ContainerFormat::Wav => "WAV",
            ContainerFormat::Opus => "Opus",
            ContainerFormat::Jpg => "JPG",
            ContainerFormat::Png => "PNG",
            ContainerFormat::Webp => "WebP",
            ContainerFormat::Avif => "AVIF",
            ContainerFormat::Bmp => "BMP",
            ContainerFormat::Gif => "GIF",
        }
    }

    /// 是否图片格式。
    pub fn is_image(&self) -> bool {
        matches!(
            self,
            ContainerFormat::Jpg
                | ContainerFormat::Png
                | ContainerFormat::Webp
                | ContainerFormat::Avif
                | ContainerFormat::Bmp
                | ContainerFormat::Gif
        )
    }

    /// 是否动图（GIF）。
    pub fn is_animated(&self) -> bool {
        matches!(self, ContainerFormat::Gif)
    }
}

impl fmt::Display for ContainerFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl Default for ContainerFormat {
    fn default() -> Self {
        ContainerFormat::Mp4
    }
}

/// 视频编解码器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoCodec {
    /// H.264 软件编码（默认）。
    #[default]
    Libx264,
    /// H.265 / HEVC 软件编码。
    Libx265,
    /// VP9 软件编码。
    LibvpxVp9,
    /// AV1 软件编码（libaom）。
    LibaomAv1,
    /// 复制流（不重新编码）。
    Copy,
}

impl VideoCodec {
    pub fn label(&self) -> &'static str {
        match self {
            VideoCodec::Libx264 => "H.264 (libx264)",
            VideoCodec::Libx265 => "H.265 (libx265)",
            VideoCodec::LibvpxVp9 => "VP9 (libvpx-vp9)",
            VideoCodec::LibaomAv1 => "AV1 (libaom-av1)",
            VideoCodec::Copy => "复制（不重编码）",
        }
    }

    /// ffmpeg `-c:v` 取值。
    pub fn ffmpeg_name(&self) -> &'static str {
        match self {
            VideoCodec::Libx264 => "libx264",
            VideoCodec::Libx265 => "libx265",
            VideoCodec::LibvpxVp9 => "libvpx-vp9",
            VideoCodec::LibaomAv1 => "libaom-av1",
            VideoCodec::Copy => "copy",
        }
    }

    /// CRF 量程上限（CRF 模式下滑块最大）。
    pub fn crf_max(&self) -> u32 {
        match self {
            VideoCodec::Libx264 | VideoCodec::Libx265 => 51,
            VideoCodec::LibvpxVp9 | VideoCodec::LibaomAv1 => 63,
            VideoCodec::Copy => 0,
        }
    }

    /// CRF 默认建议值。
    pub fn crf_default(&self) -> u32 {
        match self {
            VideoCodec::Libx264 | VideoCodec::Libx265 => 23,
            VideoCodec::LibvpxVp9 | VideoCodec::LibaomAv1 => 30,
            VideoCodec::Copy => 0,
        }
    }
}

/// 码率控制模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BitrateMode {
    /// 恒定质量（CRF / QP）。
    #[default]
    Crf,
    /// 恒定码率。
    Cbr,
    /// 动态码率上限。
    Vbr,
    /// 固定目标码率（单位 kbps）。
    Fixed,
}

impl BitrateMode {
    pub fn label(&self) -> &'static str {
        match self {
            BitrateMode::Crf => "CRF（恒定质量）",
            BitrateMode::Cbr => "CBR（恒定码率）",
            BitrateMode::Vbr => "VBR（动态码率）",
            BitrateMode::Fixed => "固定码率",
        }
    }
}

/// 缩放算法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScaleAlgorithm {
    Bilinear,
    #[default]
    Lanczos,
    Bicubic,
    Spline,
}

impl ScaleAlgorithm {
    pub fn label(&self) -> &'static str {
        match self {
            ScaleAlgorithm::Bilinear => "bilinear",
            ScaleAlgorithm::Lanczos => "lanczos",
            ScaleAlgorithm::Bicubic => "bicubic",
            ScaleAlgorithm::Spline => "spline",
        }
    }

    pub fn ffmpeg_name(&self) -> &'static str {
        match self {
            ScaleAlgorithm::Bilinear => "bilinear",
            ScaleAlgorithm::Lanczos => "lanczos",
            ScaleAlgorithm::Bicubic => "bicubic",
            ScaleAlgorithm::Spline => "spline",
        }
    }
}

/// 色彩空间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorSpace {
    #[default]
    Bt709,
    Bt601,
    Bt2020,
}

impl ColorSpace {
    pub fn label(&self) -> &'static str {
        match self {
            ColorSpace::Bt709 => "bt709",
            ColorSpace::Bt601 => "bt601",
            ColorSpace::Bt2020 => "bt2020",
        }
    }
}

/// 色彩范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorRange {
    #[default]
    Tv,
    Pc,
}

impl ColorRange {
    pub fn label(&self) -> &'static str {
        match self {
            ColorRange::Tv => "tv（受限）",
            ColorRange::Pc => "pc（全范围）",
        }
    }

    pub fn ffmpeg_name(&self) -> &'static str {
        match self {
            ColorRange::Tv => "tv",
            ColorRange::Pc => "pc",
        }
    }
}

/// 音频编解码器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioCodec {
    #[default]
    Aac,
    Mp3,
    Opus,
    Vorbis,
    Flac,
    Pcm,
    Copy,
}

impl AudioCodec {
    pub fn label(&self) -> &'static str {
        match self {
            AudioCodec::Aac => "AAC",
            AudioCodec::Mp3 => "MP3",
            AudioCodec::Opus => "Opus",
            AudioCodec::Vorbis => "Vorbis",
            AudioCodec::Flac => "FLAC",
            AudioCodec::Pcm => "PCM (WAV)",
            AudioCodec::Copy => "复制（不重编码）",
        }
    }

    pub fn ffmpeg_name(&self) -> &'static str {
        match self {
            AudioCodec::Aac => "aac",
            AudioCodec::Mp3 => "libmp3lame",
            AudioCodec::Opus => "libopus",
            AudioCodec::Vorbis => "libvorbis",
            AudioCodec::Flac => "flac",
            AudioCodec::Pcm => "pcm_s16le",
            AudioCodec::Copy => "copy",
        }
    }

    /// 是否无损编码（码率滑块无意义）。
    pub fn is_lossless(&self) -> bool {
        matches!(self, AudioCodec::Flac | AudioCodec::Pcm | AudioCodec::Copy)
    }
}

/// 声道布局。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Channels {
    #[default]
    Source,
    Mono,
    Stereo,
    Surround51,
}

impl Channels {
    pub fn label(&self) -> &'static str {
        match self {
            Channels::Source => "保持源",
            Channels::Mono => "单声道",
            Channels::Stereo => "立体声",
            Channels::Surround51 => "5.1 环绕",
        }
    }

    /// `-ac` 取值；保持源返回 None（不写该参数）。
    pub fn value(&self) -> Option<u32> {
        match self {
            Channels::Source => None,
            Channels::Mono => Some(1),
            Channels::Stereo => Some(2),
            Channels::Surround51 => Some(6),
        }
    }
}

/// 采样率。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SampleRate {
    #[default]
    Source,
    Rate44100,
    Rate48000,
    Rate96000,
}

impl SampleRate {
    pub fn label(&self) -> &'static str {
        match self {
            SampleRate::Source => "保持源",
            SampleRate::Rate44100 => "44.1 kHz",
            SampleRate::Rate48000 => "48 kHz",
            SampleRate::Rate96000 => "96 kHz",
        }
    }

    pub fn value(&self) -> Option<u32> {
        match self {
            SampleRate::Source => None,
            SampleRate::Rate44100 => Some(44100),
            SampleRate::Rate48000 => Some(48000),
            SampleRate::Rate96000 => Some(96000),
        }
    }
}

/// 分辨率预设。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResolutionPreset {
    #[default]
    Source,
    R4k,
    R2k,
    R1080,
    R720,
    R480,
    Custom,
}

impl ResolutionPreset {
    pub fn label(&self) -> &'static str {
        match self {
            ResolutionPreset::Source => "保持源",
            ResolutionPreset::R4k => "4K (3840×2160)",
            ResolutionPreset::R2k => "2K (2560×1440)",
            ResolutionPreset::R1080 => "1080p (1920×1080)",
            ResolutionPreset::R720 => "720p (1280×720)",
            ResolutionPreset::R480 => "480p (854×480)",
            ResolutionPreset::Custom => "自定义",
        }
    }

    /// 预设宽高；自定义/保持源返回 None。
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        match self {
            ResolutionPreset::R4k => Some((3840, 2160)),
            ResolutionPreset::R2k => Some((2560, 1440)),
            ResolutionPreset::R1080 => Some((1920, 1080)),
            ResolutionPreset::R720 => Some((1280, 720)),
            ResolutionPreset::R480 => Some((854, 480)),
            _ => None,
        }
    }
}

/// 帧率预设。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FpsPreset {
    #[default]
    Source,
    F24,
    F25,
    F30,
    F50,
    F60,
    Custom,
}

impl FpsPreset {
    pub fn label(&self) -> &'static str {
        match self {
            FpsPreset::Source => "保持源 (same)",
            FpsPreset::F24 => "24",
            FpsPreset::F25 => "25",
            FpsPreset::F30 => "30",
            FpsPreset::F50 => "50",
            FpsPreset::F60 => "60",
            FpsPreset::Custom => "自定义",
        }
    }
}

/// 硬件加速偏好（覆盖 ffmpeg 全部硬件加速后端）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HwAccelPreference {
    /// 自动探测并选择最优。
    #[default]
    Auto,
    /// 强制软件编码。
    Software,
    /// NVIDIA：CUDA 解码 + NVENC 编码。
    Nvenc,
    /// Intel/AMD（Linux）：VAAPI 解码 + VAAPI 编码。
    Vaapi,
    /// Intel：QSV 解码 + QSV 编码。
    Qsv,
    /// AMD：AMF 编码（无需 -hwaccel）。
    Amf,
    /// macOS：VideoToolbox 解码 + 编码。
    Videotoolbox,
    /// 仅 CUDA 解码加速（仍用软件编码）。
    CudaDecode,
    /// Windows：DXVA2 解码加速。
    Dxva2,
    /// Windows：D3D11VA 解码加速。
    D3d11va,
    /// Vulkan 加速（滤镜 / 解码）。
    Vulkan,
    /// OpenCL 加速（滤镜 / 解码）。
    Opencl,
}

impl HwAccelPreference {
    /// 下拉展示用的标签（覆盖 ffmpeg 全部硬件加速后端）。
    pub fn label(&self) -> &'static str {
        match self {
            HwAccelPreference::Auto => "自动选择",
            HwAccelPreference::Software => "强制软件",
            HwAccelPreference::Nvenc => "NVENC (NVIDIA)",
            HwAccelPreference::Vaapi => "VAAPI (Intel/AMD)",
            HwAccelPreference::Qsv => "QSV (Intel)",
            HwAccelPreference::Amf => "AMF (AMD)",
            HwAccelPreference::Videotoolbox => "VideoToolbox (macOS)",
            HwAccelPreference::CudaDecode => "CUDA 解码加速",
            HwAccelPreference::Dxva2 => "DXVA2 解码加速 (Windows)",
            HwAccelPreference::D3d11va => "D3D11VA 解码加速 (Windows)",
            HwAccelPreference::Vulkan => "Vulkan 加速",
            HwAccelPreference::Opencl => "OpenCL 加速",
        }
    }
}

/// 质量/速度取舍档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QualityPreset {
    #[default]
    Balanced,
    Quality,
    Speed,
}

impl QualityPreset {
    pub fn label(&self) -> &'static str {
        match self {
            QualityPreset::Quality => "质量优先",
            QualityPreset::Balanced => "平衡",
            QualityPreset::Speed => "速度优先",
        }
    }
}
