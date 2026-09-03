//! API Key 嗅探模块的逻辑层（纯逻辑，不依赖 GTK / libadwaita / glib）。
//!
//! 子模块：
//! - [`generate`]：正则 → 候选 Key 字典（自实现的正则子集枚举器；可乱序去重）。
//! - [`probe`]：OpenAI 兼容端点异步探测与状态码判定（reqwest）。
//! - [`store`]：平台配置 / 有效 Key（SQLite）/ 断点的本地持久化。
//! - [`scan`]：tokio 异步并发 + 限速 + 暂停停止 + 断点续跑的扫描引擎。
//!
//! 页面层（`page::api_key_sniffer`）只负责把这些能力拼成界面，输入输出全是数据。
//!
//! ## 异步运行时
//! 整个模块共享一个全局多线程 tokio 运行时（GTK 主线程之外驱动）：
//! 字典生成（`spawn_blocking`）、网络探测（reqwest）、SQLite 读写都在它上面跑。
//! 主线程通过 `runtime().block_on(...)` 或事件排队与异步侧交互，不阻塞 UI。

use std::sync::OnceLock;

pub mod generate;
pub mod probe;
pub mod scan;
pub mod store;

/// 全局多线程 tokio 运行时（首次调用时惰性创建，进程内共享）。
///
/// - 字典生成：`runtime().spawn_blocking(...)`，CPU 密集任务不占 UI 线程；
/// - 网络嗅探：tokio 任务里 `reqwest` 异步 IO；
/// - SQLite：`sqlx` 异步连接池。
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("tokio 运行时创建失败")
    })
}

/// 把进程内已释放的空闲内存归还给操作系统（Linux glibc `malloc_trim`）。
///
/// Rust 默认用 glibc malloc：数百万条 String 的小块分配在 `free` 之后仍留在
/// arena 里供复用，进程的 RSS 不会下降（`htop`/`free` 看着"内存没释放"）。
/// 一轮扫描结束（字典已 drop）后调用一次，可把空闲 arena 块还给内核。
#[cfg(target_os = "linux")]
pub fn release_unused_memory() {
    // malloc_trim 只在 glibc 可用；musl / 非 Linux 直接跳过。
    unsafe extern "C" {
        fn malloc_trim(pad: usize) -> i32;
    }
    unsafe {
        malloc_trim(0);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn release_unused_memory() {}

// 统一在本模块重导出，页面层只需 `use crate::utils::sniffer::{...}`。
// 作为本模块对外的完整 API 面，部分符号暂未被当前页面用到，故关掉未使用告警。
#[allow(unused_imports)]
pub use generate::{
    available_memory_bytes, estimate_space, format_count, generate, recommended_max_keys,
    sample_seed_for, Dictionary, GenerateOptions,
};
#[allow(unused_imports)]
pub use probe::{chat_body, join_url, now_unix, parse_header_lines, ProbeMethod, ProbeTarget};
#[allow(unused_imports)]
pub use scan::{start as start_scan, Control, ScanEvent, ScanParams, StopReason};
#[allow(unused_imports)]
pub use store::{
    append_valid, checkpoint_path, clear_checkpoint, clear_valid, config_dir, data_dir,
    db_path, delete_valid, export_valid, init_db, load_checkpoint, load_store, load_valid,
    rename_checkpoint, rename_platform, save_checkpoint, save_platforms, save_store,
    default_store,
};