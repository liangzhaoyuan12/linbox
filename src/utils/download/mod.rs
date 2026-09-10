//! 下载模块（纯逻辑，不依赖 GTK / libadwaita / glib）。
//!
//! HTTP(S) 下载引擎：reqwest(rustls) + tokio 异步，多线程分块（Range）、
//! 断点续传（分片文件 + meta）、重试、并发任务控制、进度上报。
//! UI 通过 `DownloadManager` 操作任务、轮询 `TaskSnapshot` 渲染。

pub mod client;
pub mod downloader;
pub mod settings;
