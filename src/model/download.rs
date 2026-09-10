//! 下载模块数据模型（纯数据，无 UI 依赖，遵循 docs/项目结构规划书.md 分层）。
//!
//! HTTP(S) 下载：多线程分块（Range）、断点续传（分片 + meta）、重试、
//! 并发任务控制、进度上报。类型定义与 `utils/download/` 对应。

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

/// 任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    /// 排队等待（受并发任务数限制）。
    Pending,
    /// 下载中。
    Downloading,
    /// 已暂停（保留现场，可恢复）。
    Paused,
    /// 已完成。
    Completed,
    /// 失败（error 里有原因）。
    Failed,
    /// 已取消（清理现场）。
    Cancelled,
}

impl TaskStatus {
    pub fn label(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "排队中",
            TaskStatus::Downloading => "下载中",
            TaskStatus::Paused => "已暂停",
            TaskStatus::Completed => "已完成",
            TaskStatus::Failed => "失败",
            TaskStatus::Cancelled => "已取消",
        }
    }
}

/// 一个分片：字节区间 [start, end]（闭区间）。已下载量通过分片文件大小推断，
/// 断点续传时按文件大小续下（Range 从 start+size 开始，文件追加写入）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
}

/// 下载任务（运行期状态，manager 持有）。
pub struct DownloadTask {
    pub id: u64,
    pub url: String,
    /// 下载目录。
    pub dir: String,
    /// 最终文件名。
    pub filename: String,
    pub status: TaskStatus,
    /// 总大小；`None` = 服务器未给出（chunked，只能单块流式）。
    pub total_size: Option<u64>,
    /// 已下载字节（所有分片合计，原子计数）。
    pub downloaded: Arc<AtomicU64>,
    /// 分片区间。
    pub segments: Vec<Segment>,
    /// 当前有效线程数（实际分块数 = min(threads, segments.len())）。
    pub threads: u32,
    /// 错误信息。
    pub error: Option<String>,
    /// 暂停请求标记。
    pub pause_flag: Arc<std::sync::atomic::AtomicBool>,
    /// 取消请求标记（删除任务时置位，同时清理现场）。
    pub cancel_flag: Arc<std::sync::atomic::AtomicBool>,
    /// 服务器是否支持 Range（探询结果；false 时只能单块全量下载）。
    pub range_supported: bool,
    /// 创建时间（unix 秒）。
    pub created_at: u64,
}

impl DownloadTask {
    /// 分片文件路径。
    pub fn seg_path(&self, idx: usize) -> std::path::PathBuf {
        std::path::PathBuf::from(&self.dir).join(format!(".{}.part.{}", self.filename, idx))
    }

    /// 最终文件路径。
    pub fn final_path(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(&self.dir).join(&self.filename)
    }

    /// meta 文件路径（断点续传现场）。
    pub fn meta_path(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(&self.dir).join(format!(".{}.lbm.json", self.filename))
    }
}

/// 下载全局配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadConfig {
    /// 默认下载目录。
    pub dir: String,
    /// User-Agent。
    pub user_agent: String,
    /// 单次请求超时/连接超时（秒）。
    pub timeout_secs: u64,
    /// 同时下载的任务数。
    pub max_concurrent_tasks: u32,
    /// 每个分片的重试次数。
    pub retries: u32,
    /// 每个任务默认线程数（分块数）。
    pub threads_per_task: u32,
    /// 小于该字节数的文件不分块（单块下载）；0 = 不设限（按线程数分块）。
    pub min_chunk_bytes: u64,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        DownloadConfig {
            // XDG 用户目录动态读取（dirs 遵循 user-dirs.dirs，支持本地化
            // 如「下载」）；获取失败才退回 $HOME/Downloads
            dir: dirs::download_dir()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_else(|| {
                    std::env::var("HOME")
                        .map(|h| format!("{h}/Downloads"))
                        .unwrap_or_else(|_| ".".into())
                }),
            user_agent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36".to_string(),
            timeout_secs: 30,
            max_concurrent_tasks: 3,
            retries: 3,
            threads_per_task: 8,
            min_chunk_bytes: 8 * 1024 * 1024,
        }
    }
}

/// 下载事件：本实现采用轮询快照（`DownloadManager::snapshot`），事件机制暂缺省。

/// 任务快照（UI 渲染用）。
#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    pub id: u64,
    pub url: String,
    pub dir: String,
    pub filename: String,
    pub status: TaskStatus,
    pub total_size: Option<u64>,
    pub downloaded: u64,
    /// 瞬时速度（字节/秒，由 manager 计算）。
    pub speed: u64,
    pub threads: u32,
    pub error: Option<String>,
    pub created_at: u64,
}
