//! 数据模型层（纯数据，无 UI 依赖）。
//!
//! 参考 `docs/项目结构规划书.md` §3.8：`model/` 仅定义 `struct`/`enum`，
//! 不依赖任何 UI 框架；`utils` 读写 `model`，`page` 展示 `model`。

pub mod archive_cracker;
pub mod download;
pub mod env_editor;
pub mod imfix;
pub mod inotify;
pub mod media;
pub mod monitor;
pub mod path_scanner;
pub mod port_scanner;
pub mod sniffer;
pub mod systemd;

// 故意不做 `pub use xxx::*` 顶层扁平导出：多个模块导出同名项
// （ScanEvent/ScanConfig/fingerprint），glob 互相冲突（ambiguous glob re-exports）。
// 全仓引用均为子模块路径（crate::model::<mod>::<Item>），顶层不聚合。
