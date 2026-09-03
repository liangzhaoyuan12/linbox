//! 路径扫描模块的逻辑层（纯逻辑，不依赖 GTK / libadwaita / glib）。
//!
//! 子模块：
//! - [`scan`]：tokio 异步并发 + 限速 + 暂停停止的路径扫描引擎。
//!
//! 页面层（`page::path_scanner`）负责把这些能力拼成界面。

pub mod scan;

/// 全局多线程 tokio 运行时（复用 sniffer 模块的运行时实例）。
pub fn runtime() -> &'static tokio::runtime::Runtime {
    crate::utils::sniffer::runtime()
}

#[allow(unused_imports)]
pub use scan::{start as start_scan, Control, PathScanParams};
