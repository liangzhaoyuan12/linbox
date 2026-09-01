//! 媒体转换逻辑层（纯逻辑，不依赖 GTK / libadwaita / glib）。
//!
//! 子模块：
//! - [`command`]：ffmpeg 命令构建器（核心）。
//! - [`probe`]：ffprobe 媒体信息解析与探测。
//! - [`hwaccel`]：硬件加速能力探测。

pub mod command;
pub mod hwaccel;
pub mod probe;

pub use command::*;
pub use hwaccel::*;
pub use probe::*;
