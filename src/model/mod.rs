//! 数据模型层（纯数据，无 UI 依赖）。
//!
//! 参考 `docs/项目结构规划书.md` §3.8：`model/` 仅定义 `struct`/`enum`，
//! 不依赖任何 UI 框架；`utils` 读写 `model`，`page` 展示 `model`。

pub mod archive_cracker;
pub mod imfix;
pub mod media;
pub mod path_scanner;
pub mod sniffer;

pub use archive_cracker::*;
pub use imfix::*;
pub use media::*;
pub use path_scanner::*;
pub use sniffer::*;
