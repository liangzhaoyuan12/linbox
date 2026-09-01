//! API Key 嗅探模块的逻辑层（纯逻辑，不依赖 GTK / libadwaita / glib）。
//!
//! 子模块：
//! - [`generate`]：正则 → 候选 Key 字典（自实现的正则子集枚举器）。
//! - [`probe`]：OpenAI 兼容端点探测与状态码判定。
//! - [`store`]：平台配置 / 有效 Key / 断点的本地持久化。
//! - [`scan`]：并发 + 限速 + 暂停停止 + 断点续跑的扫描引擎。
//!
//! 页面层（`page::api_key_sniffer`）只负责把这些能力拼成界面，输入输出全是数据。

pub mod generate;
pub mod probe;
pub mod scan;
pub mod store;

// 统一在本模块重导出，页面层只需 `use crate::utils::sniffer::{...}`。
// 作为本模块对外的完整 API 面，部分符号暂未被当前页面用到，故关掉未使用告警。
#[allow(unused_imports)]
pub use generate::{estimate_space, format_count, generate, Dictionary, GenerateOptions};
#[allow(unused_imports)]
pub use probe::{chat_body, join_url, now_unix, parse_header_lines, ProbeMethod, ProbeTarget};
#[allow(unused_imports)]
pub use scan::{start as start_scan, Control, ScanEvent, ScanParams, StopReason};
#[allow(unused_imports)]
pub use store::{
    append_valid, clear_checkpoint, clear_valid, config_dir, data_dir, delete_valid, export_valid,
    load_checkpoint, load_store, load_valid, save_checkpoint, save_platforms, save_store,
    checkpoint_path, default_store, valid_keys_path,
};
