//! 下载核心：任务调度、分块下载、断点续传、重试、合并。
//!
//! 设计要点：
//! - 每个任务按 `threads` 切成若干分片（Range: bytes=start-end），分片并发
//!   下载到独立的 `.filename.part.N` 文件，全部完成后按序合并为最终文件；
//! - 断点续传：分片已下载量 = 分片文件当前大小（追加写入，Range 从
//!   start+size 续下）；任务元数据（url/分片区间）存 `.filename.lbm.json`，
//!   暂停/重启后从 meta 恢复分片现场，已完成的块直接跳过；
//! - 失败重试：每个分片在配置的重试次数内指数退避重试；
//! - 并发任务数：`Semaphore` 限制同时执行的下载任务；
//! - 进度：`downloaded` 原子计数（所有分片合计），UI 轮询
//!   `snapshot()` 获得进度与速度。

use crate::model::download::{DownloadConfig, DownloadTask, Segment, TaskSnapshot, TaskStatus};
use crate::utils::download::client::build_client;
use futures_util::StreamExt as _;
use reqwest::StatusCode;
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Semaphore;

/// 下载模块独立的 tokio 运行时。
///
/// 不共享 sniffer 的运行时：嗅探任务（字典生成/端口扫描）量大时会占满
/// 共享 worker，拖慢甚至饿死下载任务；独立运行时让下载互不干扰。
pub(crate) fn dl_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("tokio 运行时创建失败")
    })
}

const META_VERSION: u32 = 1;

/// 断点续传现场（保存在 `.<filename>.lbm.json`）。
#[derive(serde::Serialize, serde::Deserialize)]
struct MetaFile {
    version: u32,
    url: String,
    dir: String,
    filename: String,
    total_size: Option<u64>,
    segments: Vec<Segment>,
    threads: u32,
}

pub struct DownloadManager {
    client: reqwest::Client,
    cfg: RwLock<DownloadConfig>,
    tasks: Mutex<HashMap<u64, Arc<Mutex<DownloadTask>>>>,
    semaphore: RwLock<Arc<Semaphore>>,
    next_id: AtomicU64,
    /// id -> (时间点, 已下载字节)：用于计算瞬时速度。
    speed_cache: Mutex<HashMap<u64, (std::time::Instant, u64)>>,
}

/// 全局下载管理器（页面重建复用；下载任务生命周期独立于页面）。
static GLOBAL: std::sync::OnceLock<Arc<DownloadManager>> = std::sync::OnceLock::new();

impl DownloadManager {
    /// 获取全局 manager；首次调用时用给定配置初始化。
    pub fn global(cfg: Option<DownloadConfig>) -> Arc<Self> {
        GLOBAL
            .get_or_init(|| {
                let cfg = cfg.unwrap_or_default();
                let client = build_client(&cfg).expect("初始化 HTTP 客户端失败（配置错误）");
                let sem = Arc::new(Semaphore::new(cfg.max_concurrent_tasks as usize));
                Arc::new(DownloadManager {
                    client,
                    cfg: RwLock::new(cfg),
                    tasks: Mutex::new(HashMap::new()),
                    semaphore: RwLock::new(sem),
                    next_id: AtomicU64::new(1),
                    speed_cache: Mutex::new(HashMap::new()),
                })
            })
            .clone()
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn config(&self) -> DownloadConfig {
        self.cfg.read().unwrap().clone()
    }

    pub fn set_config(&self, cfg: DownloadConfig) {
        *self.semaphore.write().unwrap() =
            Arc::new(Semaphore::new(cfg.max_concurrent_tasks as usize));
        *self.cfg.write().unwrap() = cfg;
    }

    // -----------------------------------------------------------------------
    // 任务操作（UI 主线程同步调用；内部 spawn 到 tokio 执行）
    // -----------------------------------------------------------------------

    /// 添加任务；返回任务 id。目录/文件名/线程数可单独指定（覆盖全局配置）。
    pub fn add_download(
        self: &Arc<Self>,
        url: &str,
        dir: Option<String>,
        filename: Option<String>,
        threads: Option<u32>,
    ) -> Result<u64, String> {
        let url = url.trim().to_string();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("仅支持 http/https 下载地址".into());
        }
        let cfg = self.config();
        let dir = dir.unwrap_or(cfg.dir.clone());
        // 目录不存在则自动创建；失败（无权限等）立即反馈，避免任务全部
        // 因“没有那个文件或目录”失败
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Err(format!("下载目录不可用（{dir}）：{e}"));
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let task = Arc::new(Mutex::new(DownloadTask {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            url: url.clone(),
            dir: dir.clone(),
            filename: filename.unwrap_or_default(),
            status: TaskStatus::Pending,
            total_size: None,
            downloaded: Arc::new(AtomicU64::new(0)),
            segments: Vec::new(),
            threads: threads.unwrap_or(cfg.threads_per_task).max(1),
            error: None,
            pause_flag: Arc::new(AtomicBool::new(false)),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            range_supported: false,
            created_at: now,
        }));
        let id = task.lock().unwrap().id;
        self.tasks.lock().unwrap().insert(id, task);
        exec_task(self.clone(), id);
        Ok(id)
    }

    /// 恢复暂停的任务（重新走探询 → meta 恢复分片 → 续下）。
    /// 仅对 Paused/Failed 状态的任务生效，其他状态静默忽略。
    pub fn resume(self: &Arc<Self>, id: u64) {
        let should_exec = if let Some(task) = self.tasks.lock().unwrap().get(&id).cloned() {
            let mut t = task.lock().unwrap();
            match t.status {
                TaskStatus::Paused => {
                    t.pause_flag.store(false, Ordering::SeqCst);
                    t.status = TaskStatus::Pending;
                    true
                }
                TaskStatus::Failed => {
                    // 失败重试：重置状态，从头开始
                    t.status = TaskStatus::Pending;
                    t.error = None;
                    true
                }
                _ => false,
            }
        } else {
            false
        };
        if should_exec {
            exec_task(self.clone(), id);
        }
    }

    /// 暂停：置位暂停标记，块下载中断，保留分片与 meta。
    pub fn pause(&self, id: u64) {
        if let Some(task) = self.tasks.lock().unwrap().get(&id).cloned() {
            let t = task.lock().unwrap();
            t.pause_flag.store(true, Ordering::SeqCst);
        }
    }

    /// 删除任务：置取消标记 + 清理现场 + 从任务列表移除，一步到位。
    /// 下载中（Downloading）的任务由执行线程观察到 cancel_flag 后自行清理
    /// 分片与 meta（本方法已把任务移出列表，线程持 Arc 副本继续收尾）；
    /// 其余状态（Pending/Paused/Failed 等，无活跃下载线程）当场清理现场。
    pub fn cancel_and_remove(self: &Arc<Self>, id: u64) {
        let mut direct: Option<(String, String, usize, u64)> = None;
        if let Some(task) = self.tasks.lock().unwrap().get(&id).cloned() {
            let mut t = task.lock().unwrap();
            t.cancel_flag.store(true, Ordering::SeqCst);
            if t.status != TaskStatus::Downloading {
                t.status = TaskStatus::Cancelled;
                if !t.segments.is_empty() {
                    direct = Some((t.dir.clone(), t.filename.clone(), t.segments.len(), t.id));
                }
            }
        }
        self.tasks.lock().unwrap().remove(&id);
        self.speed_cache.lock().unwrap().remove(&id);
        if let Some((dir, filename, seg_count, task_id)) = direct {
            std::thread::spawn(move || {
                for i in 0..seg_count {
                    let _ = std::fs::remove_file(seg_path(&dir, &filename, task_id, i));
                }
                let _ = std::fs::remove_file(meta_path(&dir, &filename, task_id));
            });
        }
    }

    /// 从任务表移除（已完成/失败/取消的记录清理）。
    pub fn remove(&self, id: u64) {
        self.tasks.lock().unwrap().remove(&id);
        self.speed_cache.lock().unwrap().remove(&id);
    }

    /// 清理所有已完成/失败/取消的任务（保留进行中/暂停）。
    pub fn purge_finished(&self) {
        let mut tasks = self.tasks.lock().unwrap();
        let ids: Vec<u64> = tasks
            .iter()
            .filter(|(_, t)| {
                let t = t.lock().unwrap();
                matches!(
                    t.status,
                    TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
                )
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            tasks.remove(&id);
            self.speed_cache.lock().unwrap().remove(&id);
        }
    }

    /// 任务快照（UI 轮询）：进度 + 瞬时速度。
    pub fn snapshot(&self) -> Vec<TaskSnapshot> {
        let now = std::time::Instant::now();
        // 锁顺序与其它方法一致：tasks → speed_cache，避免死锁
        let tasks = self.tasks.lock().unwrap();
        let mut cache = self.speed_cache.lock().unwrap();
        let mut snaps: Vec<TaskSnapshot> = Vec::with_capacity(tasks.len());
        for (id, task) in tasks.iter() {
            let t = task.lock().unwrap();
            let downloaded = t.downloaded.load(Ordering::Relaxed);
            let speed = match t.status {
                TaskStatus::Downloading | TaskStatus::Pending => match cache.get(id) {
                    Some((last, ld)) => {
                        let dt = now.duration_since(*last).as_secs_f64();
                        if dt >= 0.3 && downloaded >= *ld {
                            ((downloaded - *ld) as f64 / dt) as u64
                        } else {
                            0
                        }
                    }
                    None => 0,
                },
                _ => {
                    // 非下载态清掉速度缓存，避免恢复后首帧速度虚高
                    cache.remove(id);
                    0
                }
            };
            if matches!(t.status, TaskStatus::Downloading | TaskStatus::Pending) {
                cache.insert(*id, (now, downloaded));
            }
            snaps.push(TaskSnapshot {
                id: *id,
                url: t.url.clone(),
                dir: t.dir.clone(),
                filename: t.filename.clone(),
                status: t.status,
                total_size: t.total_size,
                downloaded,
                speed,
                threads: t.threads,
                error: t.error.clone(),
                created_at: t.created_at,
            });
        }
        snaps
    }

    pub fn task_count(&self) -> usize {
        self.tasks.lock().unwrap().len()
    }
}

// ---------------------------------------------------------------------------
// 任务执行
// ---------------------------------------------------------------------------

fn exec_task(mgr: Arc<DownloadManager>, id: u64) {
    let sem = mgr.semaphore.read().unwrap().clone();
    dl_runtime().spawn(async move {
        let _perm = sem.acquire_owned().await;
        run_download(mgr, id).await;
    });
}

/// 执行一个下载任务（探询 → 造段 → 并发分块 → 合并/收尾）。
async fn run_download(mgr: Arc<DownloadManager>, id: u64) {
    let task_arc = match mgr.tasks.lock().unwrap().get(&id).cloned() {
        Some(t) => t,
        None => return,
    };
    let (cfg, client) = {
        let cfg = mgr.config();
        (cfg, mgr.client().clone())
    };
    // 兜底：目录被删/不存在时自动重建（正常情况下 add/resume 时已创建）
    {
        let dir = task_arc.lock().unwrap().dir.clone();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            let mut t = task_arc.lock().unwrap();
            t.status = TaskStatus::Failed;
            t.error = Some(format!("下载目录不可用（{dir}）：{e}"));
            return;
        }
    }
    // 标记复位：pause_flag 允许复位（新启动/恢复都从无暂停开始）；
    // cancel_flag 不复位——一旦取消必须消费到底，否则排队/探询中的任务会“删不动”。
    {
        let mut t = task_arc.lock().unwrap();
        if t.cancel_flag.load(Ordering::SeqCst) {
            // 启动前已被取消：直接退出（现场清理由 cancel_and_remove 负责）
            t.status = TaskStatus::Cancelled;
            return;
        }
        t.pause_flag.store(false, Ordering::SeqCst);
        if t.status != TaskStatus::Paused {
            t.status = TaskStatus::Downloading;
        }
    }

    // 1) 探询：Range: bytes=0-0 → 判定是否支持分块 + 总大小
    let probe_result = probe(&client, &cfg, &task_arc).await;
    let (url, dir, filename, total, segments, threads, range_ok) = match probe_result {
        Ok(r) => r,
        Err(e) => {
            {
                let mut t = task_arc.lock().unwrap();
                t.status = TaskStatus::Failed;
                t.error = Some(e.clone());
            }
            return;
        }
    };
    {
        let mut t = task_arc.lock().unwrap();
        t.url = url;
        t.dir = dir;
        t.filename = filename.clone();
        t.total_size = total;
        t.segments = segments.clone();
        t.threads = threads;
        // meta 恢复路径无法重新探询：分片数 > 1 视为支持 Range
        t.range_supported = range_ok || segments.len() > 1;
    }

    // 2) 合并执行分片下载
    let mut handles = Vec::new();
    let mut started: Vec<u64> = Vec::new();
    let mut next = 0usize;
    let total_segs = segments.len();
    let active = (threads as usize).min(total_segs);
    for _ in 0..active {
        let idx = next;
        next += 1;
        handles.push((
            idx,
            dl_runtime().spawn(download_segment(
                client.clone(),
                task_arc.clone(),
                idx,
                cfg.retries,
                cfg.timeout_secs,
            )),
        ));
        started.push(idx as u64);
    }
    let _ = started;
    let mut results = Vec::with_capacity(total_segs);
    for (idx, h) in handles {
        match h.await {
            Ok(Ok(())) => results.push((idx, Ok(()))),
            Ok(Err(e)) => results.push((idx, Err(e))),
            Err(e) => results.push((idx, Err(format!("分片任务异常：{e}")))),
        }
    }
    let _ = next;

    // 3) 根据结果收尾
    let (paused, cancelled) = {
        let t = task_arc.lock().unwrap();
        (
            t.pause_flag.load(Ordering::SeqCst),
            t.cancel_flag.load(Ordering::SeqCst),
        )
    };

    // 取消优先于暂停：两标记同置（先暂停后删除）时按取消收尾，分片/现场全部清理
    if cancelled {
        {
            let mut t = task_arc.lock().unwrap();
            t.status = TaskStatus::Cancelled;
        }
        cleanup_segments(&task_arc).await;
        return;
    }
    if paused {
        let mut t = task_arc.lock().unwrap();
        t.status = TaskStatus::Paused;
        return;
    }
    // 有失败分片
    if let Some((_, Err(e))) = results.iter().find(|(_, r)| r.is_err()) {
        {
            let mut t = task_arc.lock().unwrap();
            t.status = TaskStatus::Failed;
            t.error = Some(e.clone());
        }
        return;
    }
    // 全部成功 → 合并
    match merge_segments(&task_arc).await {
        Ok(_path) => {
            let mut t = task_arc.lock().unwrap();
            t.status = TaskStatus::Completed;
            let total = t.total_size;
            if let Some(total) = total {
                t.downloaded.store(total, Ordering::Relaxed);
            }
        }
        Err(e) => {
            let mut t = task_arc.lock().unwrap();
            t.status = TaskStatus::Failed;
            t.error = Some(e);
        }
    }
}

/// 把 [0, total) 均匀切成 seg_count 段（无缝无重叠覆盖全部字节）。
///
/// 不能先算 chunk 再 while 循环生成：chunk 上取整会丢失段数（例如
/// 61MB 设 32 线程时 ceil(61/32)=2MB/段 → 只剩 31 段），必须按段号
/// 用边界公式精确定位，保证段数与请求线程数一致。
fn build_segments(total: u64, seg_count: usize) -> Vec<Segment> {
    // 段数不能超过字节数（total=1、请求 8 段时只能 1 段）
    let seg_count = if total == 0 {
        1
    } else {
        (seg_count as u64).min(total).max(1) as usize
    };
    let mut segs = Vec::with_capacity(seg_count);
    for i in 0..seg_count {
        let start = (total * i as u64) / seg_count as u64;
        let end = if i + 1 == seg_count {
            total.saturating_sub(1)
        } else {
            ((total * (i + 1) as u64) / seg_count as u64).saturating_sub(1)
        };
        if start <= end {
            segs.push(Segment { start, end });
        }
    }
    if segs.is_empty() {
        segs.push(Segment {
            start: 0,
            end: total.saturating_sub(1),
        });
    }
    segs
}

/// 探询：请求 Range: bytes=0-0，解析总大小/Range 支持/文件名。
async fn probe(
    client: &reqwest::Client,
    cfg: &DownloadConfig,
    task: &Arc<Mutex<DownloadTask>>,
) -> Result<(String, String, String, Option<u64>, Vec<Segment>, u32, bool), String> {
    let (url, dir, given_filename, threads, task_id) = {
        let t = task.lock().unwrap();
        (
            t.url.clone(),
            t.dir.clone(),
            t.filename.clone(),
            t.threads,
            t.id,
        )
    };

    let resp = client
        .get(&url)
        .header(reqwest::header::RANGE, "bytes=0-0")
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .timeout(crate::utils::download::client::probe_timeout(cfg))
        .send()
        .await
        .map_err(|e| format!("连接服务器失败：{e}"))?;
    let status = resp.status();

    // 文件名：Content-Disposition > 用户指定 > URL basename
    let mut filename = given_filename;
    if filename.is_empty() {
        filename = content_disposition_filename(resp.headers())
            .or_else(|| url_basename(&url))
            .unwrap_or_else(|| "download".into());
    }
    let dir_p = PathBuf::from(&dir);
    ensure_unique(&dir_p, &mut filename, task_id);

    // 检查是否存在断点续传现场（本任务的 meta，文件名含任务 id）
    let meta_file_path = meta_path(&dir, &filename, task_id);
    if let Some(meta) = load_meta(&meta_file_path) {
        if meta.url == url && meta.version == META_VERSION {
            let range_ok = meta.segments.len() > 1;
            return Ok((
                url,
                dir,
                filename,
                meta.total_size,
                meta.segments,
                meta.threads.max(1),
                range_ok,
            ));
        }
    }

    let range_ok = status == StatusCode::PARTIAL_CONTENT;
    let content_range_total = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.rsplit('/').next())
        .and_then(|s| s.parse::<u64>().ok());
    let content_length = resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let total = if range_ok {
        content_range_total
    } else {
        content_length
    };
    // 探询响应体很小，丢弃
    drop(resp);

    let (segments, used_threads, seg_range_ok) = match total {
        Some(total) if range_ok && total > 0 => {
            // 段数 = 文件按 min_chunk 可切块数，上限为请求线程数。
            // min_chunk_bytes == 0 表示「不设限」：不论文件多小都按线程数分块
            // （build_segments 会再按总字节数封顶，不会出现超长段）。
            let seg_count = if cfg.min_chunk_bytes == 0 {
                threads.max(1) as u64
            } else {
                (total.div_ceil(cfg.min_chunk_bytes))
                    .min(threads.max(1) as u64)
                    .max(1)
            }
            .max(1) as usize;
            let segs = build_segments(total, seg_count);
            let n = segs.len() as u32;
            (segs, n, true)
        }
        Some(total) if total > 0 => (
            vec![Segment {
                start: 0,
                end: total - 1,
            }],
            1,
            range_ok,
        ),
        _ => (
            vec![Segment {
                start: 0,
                end: u64::MAX,
            }],
            1,
            false,
        ),
    };

    // 保存现场（断点续传 meta）
    if segments.len() > 0 {
        let meta = MetaFile {
            version: META_VERSION,
            url: url.clone(),
            dir: dir.clone(),
            filename: filename.clone(),
            total_size: total,
            segments: segments.clone(),
            threads: used_threads.max(1),
        };
        let _ = save_meta(&meta_file_path, &meta);
    }

    Ok((
        url,
        dir,
        filename,
        total,
        segments,
        used_threads.max(1),
        seg_range_ok,
    ))
}

/// 下载一个分片：Range 请求 + 流式追加写分片文件；断点续传按已有大小续下。
async fn download_segment(
    client: reqwest::Client,
    task: Arc<Mutex<DownloadTask>>,
    idx: usize,
    retries: u32,
    timeout_secs: u64,
) -> Result<(), String> {
    let (url, dir, filename, seg, range_ok, task_id) = {
        let t = task.lock().unwrap();
        (
            t.url.clone(),
            t.dir.clone(),
            t.filename.clone(),
            t.segments[idx],
            t.range_supported,
            t.id,
        )
    };
    // 已有分片大小（分片文件名含任务 id，互不干扰）
    let seg_path = seg_path(&dir, &filename, task_id, idx);

    // 已有分片大小
    let mut existing = match tokio::fs::metadata(&seg_path).await {
        Ok(m) => m.len(),
        Err(_) => 0,
    };
    if seg.end != u64::MAX {
        let need = seg.end - seg.start + 1;
        if existing >= need {
            // 已完成
            if let Ok(t) = task.lock() {
                t.downloaded.fetch_add(need, Ordering::Relaxed);
            }
            return Ok(());
        }
    }
    // 服务器不支持 Range：无法续传，清空旧分片从头下载
    if !range_ok && existing > 0 {
        let _ = tokio::fs::remove_file(&seg_path).await;
        existing = 0;
    }

    let mut attempt = 0u32;
    loop {
        // !range_ok 不支持断点续传：每次重试前清空分片文件，防止追加导致数据重复
        if !range_ok && attempt > 0 {
            let _ = tokio::fs::remove_file(&seg_path).await;
            existing = 0;
        }
        // 每次重试时重新计算 from：文件可能在上次尝试中已追加了数据，
        // 必须从当前文件大小续传，否则 Range 重叠会导致数据重复写入。
        let from = if seg.end == u64::MAX || !range_ok {
            None
        } else {
            existing = match tokio::fs::metadata(&seg_path).await {
                Ok(m) => m.len(),
                Err(_) => 0,
            };
            let need = seg.end - seg.start + 1;
            if existing >= need {
                // 已完成（重试期间其他线程/上一次尝试已写完）
                if let Ok(t) = task.lock() {
                    t.downloaded.fetch_add(need, Ordering::Relaxed);
                }
                return Ok(());
            }
            Some(seg.start + existing)
        };
        // 暂停/取消检查
        {
            let t = task.lock().unwrap();
            if t.pause_flag.load(Ordering::SeqCst) {
                return Err("paused".into());
            }
            if t.cancel_flag.load(Ordering::SeqCst) {
                return Err("cancelled".into());
            }
        }
        let mut req = client.get(&url);
        // 下载必须禁用内容压缩：Range 语义基于原始字节，解压会破坏分块拼接
        req = req.header(reqwest::header::ACCEPT_ENCODING, "identity");
        if let Some(f) = from {
            if seg.end == u64::MAX {
                req = req.header(reqwest::header::RANGE, format!("bytes={f}-"));
            } else {
                req = req.header(reqwest::header::RANGE, format!("bytes={f}-{}", seg.end));
            }
        }
        // 不设请求总超时（大文件下载可能持续很久），只依赖客户端的
        // read_timeout（读闲置超时）兜底断开卡死的连接。
        match req.send().await {
            Ok(resp) => {
                if range_ok && seg.end != u64::MAX && resp.status() != StatusCode::PARTIAL_CONTENT {
                    return Err(format!(
                        "服务器未返回 206（实际 {}），无法分块续传",
                        resp.status()
                    ));
                }
                if resp.status().is_server_error() {
                    attempt += 1;
                    if attempt > retries {
                        return Err(format!("服务器错误（{}）", resp.status()));
                    }
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
                if !resp.status().is_success() && resp.status() != StatusCode::PARTIAL_CONTENT {
                    return Err(format!("请求失败（{}）", resp.status()));
                }
                // 追加写入（no-range 单块：已有分片已在上面清空，从头写）
                let mut file = match tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&seg_path)
                    .await
                {
                    Ok(f) => f,
                    Err(e) => return Err(format!("打开分片文件失败：{e}")),
                };
                let mut stream = resp.bytes_stream();
                let mut written: u64 = 0;
                loop {
                    let cancelled = {
                        let t = task.lock().unwrap();
                        t.pause_flag.load(Ordering::SeqCst) || t.cancel_flag.load(Ordering::SeqCst)
                    };
                    if cancelled {
                        file.flush().await.ok();
                        return Err("paused".into());
                    }
                    match stream.next().await {
                        Some(Ok(chunk)) => {
                            if let Err(e) = file.write_all(&chunk).await {
                                return Err(format!("写入分片失败：{e}"));
                            }
                            written += chunk.len() as u64;
                            // 计入任务全局已下载字节
                            if let Ok(t) = task.lock() {
                                t.downloaded
                                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                            }
                        }
                        Some(Err(e)) => {
                            file.flush().await.ok();
                            attempt += 1;
                            if attempt > retries {
                                return Err(format!("分片下载错误：{e}"));
                            }
                            tokio::time::sleep(backoff(attempt)).await;
                            break; // 重新进入外层 loop，从断点续下
                        }
                        None => {
                            file.flush().await.ok();
                            return Ok(());
                        }
                    }
                }
                let _ = written;
                // 有错误则外层 loop 继续
            }
            Err(e) => {
                attempt += 1;
                if attempt > retries {
                    return Err(format!("分片请求失败（已重试 {attempt} 次）：{e}"));
                }
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    }
}

/// 指数退避：0.5s, 1s, 2s, …（上限 8s）。
fn backoff(attempt: u32) -> std::time::Duration {
    let secs = 0.5 * 2f64.powi((attempt as i32).min(4));
    std::time::Duration::from_secs_f64(secs)
}

/// 合并分片 → 最终文件，删除分片与 meta。
async fn merge_segments(task: &Arc<Mutex<DownloadTask>>) -> Result<PathBuf, String> {
    let (dir, filename, seg_count, task_id) = {
        let t = task.lock().unwrap();
        (t.dir.clone(), t.filename.clone(), t.segments.len(), t.id)
    };
    let final_path = PathBuf::from(&dir).join(&filename);

    if seg_count <= 1 {
        // 单块：分片文件直接改名（不先 truncate 最终文件，避免 rename 失败时数据丢失）
        let part = seg_path(&dir, &filename, task_id, 0);
        if part.exists() {
            tokio::fs::rename(&part, &final_path)
                .await
                .map_err(|e| format!("重命名失败：{e}"))?;
        }
    } else {
        let mut out = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&final_path)
            .await
            .map_err(|e| format!("创建最终文件失败：{e}"))?;
        for i in 0..seg_count {
            let part = seg_path(&dir, &filename, task_id, i);
            let mut f = tokio::fs::File::open(&part)
                .await
                .map_err(|e| format!("打开分片 {i} 失败：{e}"))?;
            let mut buf = Vec::with_capacity(1 << 20);
            loop {
                use tokio::io::AsyncReadExt as _;
                buf.clear();
                let n = f.read_buf(&mut buf).await.map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf).await.map_err(|e| e.to_string())?;
            }
            let _ = tokio::fs::remove_file(&part).await;
        }
        out.flush().await.map_err(|e| e.to_string())?;
    }

    // 清理 meta
    let meta_file = meta_path(&dir, &filename, task_id);
    let _ = tokio::fs::remove_file(&meta_file).await;

    // 若最终文件不存在（0 字节任务）也确保存在
    if !final_path.exists() {
        tokio::fs::write(&final_path, b"")
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(final_path)
}

/// 取消时清理分片与 meta。
async fn cleanup_segments(task: &Arc<Mutex<DownloadTask>>) {
    let (dir, filename, seg_count, task_id) = {
        let t = task.lock().unwrap();
        (t.dir.clone(), t.filename.clone(), t.segments.len(), t.id)
    };
    for i in 0..seg_count {
        let part = seg_path(&dir, &filename, task_id, i);
        let _ = tokio::fs::remove_file(&part).await;
    }
    let meta = meta_path(&dir, &filename, task_id);
    let _ = tokio::fs::remove_file(&meta).await;
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

/// 从 Content-Disposition 提取文件名（支持 filename= 与 filename*=UTF-8''）。
fn content_disposition_filename(headers: &reqwest::header::HeaderMap) -> Option<String> {
    // 注意：to_str() 对非 ASCII（中文文件名）会失败，必须用 as_bytes
    let cd = String::from_utf8_lossy(
        headers
            .get(reqwest::header::CONTENT_DISPOSITION)?
            .as_bytes(),
    )
    .into_owned();
    let cd = cd.trim();
    // filename*=UTF-8''xxx
    if let Some(start) = cd.find("filename*=UTF-8''") {
        let rest = &cd[start + "filename*=UTF-8''".len()..];
        let name = rest.split(';').next().unwrap_or(rest);
        return Some(percent_decode(name));
    }
    // filename="xxx"
    for part in cd.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("filename=") {
            let name = rest.trim_matches('"');
            if !name.is_empty() && name != ".." {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// 简单百分号解码。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 从 URL path 提取文件名。
fn url_basename(url: &str) -> Option<String> {
    let path = url.split('?').next().unwrap_or(url);
    let path = path.split('#').next().unwrap_or(path);
    let base = path.rsplit('/').next().unwrap_or(path);
    if base.is_empty() || base == "/" {
        None
    } else {
        Some(base.to_string())
    }
}

/// 分片文件路径（含任务 id：不同任务同名 URL 的现场互不干扰，
/// 否则删除旧任务与新建同 URL 任务会并发践踏同一批分片文件）。
fn seg_path(dir: &str, filename: &str, task_id: u64, idx: usize) -> PathBuf {
    PathBuf::from(dir).join(format!(".{filename}.{task_id}.part.{idx}"))
}

/// 断点续传现场 meta 路径（同上，按任务 id 隔离）。
fn meta_path(dir: &str, filename: &str, task_id: u64) -> PathBuf {
    PathBuf::from(dir).join(format!(".{filename}.{task_id}.lbm.json"))
}

/// 目标文件名冲突时自动加序号（file (1).bin）。
/// `task_id` 用于判断是否存在本任务自己的续传现场（现场存在视为同一任务继续，
/// 不改名；无现场且目标文件已存在则加序号防覆盖）。
fn ensure_unique(dir: &Path, filename: &mut String, task_id: u64) {
    let target = dir.join(filename.as_str());
    let meta = meta_path(dir.to_string_lossy().as_ref(), filename.as_str(), task_id);
    if !target.exists() || meta.exists() {
        return; // 无冲突，或有断点续传现场（视为同一任务继续）
    }
    // 有最终文件但没有 meta → 加序号
    let Some(ext_pos) = filename.rfind('.') else {
        for i in 1..100u32 {
            let cand = format!("{filename} ({i})");
            if !dir.join(&cand).exists() {
                *filename = cand;
                return;
            }
        }
        return;
    };
    let (stem, ext) = filename.split_at(ext_pos);
    for i in 1..100u32 {
        let cand = format!("{stem} ({i}){ext}");
        if !dir.join(&cand).exists() {
            *filename = cand;
            return;
        }
    }
}

fn save_meta(path: &Path, meta: &MetaFile) -> Result<(), String> {
    let text = serde_json::to_string(meta).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())
}

fn load_meta(path: &Path) -> Option<MetaFile> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 未使用辅助：避免 dead_code 警告的占位（保留后续扩展用）。
#[allow(dead_code)]
fn _stdout_log(msg: &str) {
    let mut out = std::io::stderr();
    let _ = writeln!(out, "[download] {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_basename_basic() {
        assert_eq!(
            url_basename("https://a.com/b/c/file.zip"),
            Some("file.zip".into())
        );
        assert_eq!(url_basename("https://a.com/x?y=1"), Some("x".into()));
        assert_eq!(url_basename("https://a.com/"), None);
        // fragment 应被去除
        assert_eq!(
            url_basename("https://a.com/file.zip#section"),
            Some("file.zip".into())
        );
        assert_eq!(
            url_basename("https://a.com/file.zip?y=1#frag"),
            Some("file.zip".into())
        );
    }

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(percent_decode("中文"), "中文");
    }

    #[test]
    fn content_disposition_parse() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            reqwest::header::CONTENT_DISPOSITION,
            reqwest::header::HeaderValue::from_str("attachment; filename=\"报告.pdf\"").unwrap(),
        );
        assert_eq!(content_disposition_filename(&h), Some("报告.pdf".into()));

        let mut h2 = reqwest::header::HeaderMap::new();
        h2.insert(
            reqwest::header::CONTENT_DISPOSITION,
            reqwest::header::HeaderValue::from_str(
                "attachment; filename*=UTF-8''%E6%8A%A5%E5%91%8A.pdf",
            )
            .unwrap(),
        );
        assert_eq!(content_disposition_filename(&h2), Some("报告.pdf".into()));
    }

    #[test]
    fn ensure_unique_adds_suffix() {
        let dir = std::env::temp_dir().join(format!("linbox-unique-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("a.bin"), b"x").unwrap();
        let mut name = "a.bin".to_string();
        ensure_unique(&dir, &mut name, 99);
        assert_eq!(name, "a (1).bin");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backoff_capped() {
        assert!(backoff(1) <= std::time::Duration::from_secs(1));
        assert!(backoff(10) <= std::time::Duration::from_secs(8));
    }

    #[test]
    fn segments_exact_count_and_contiguous() {
        let mb = 1024 * 1024u64;
        // 61MB / 32 线程：旧算法 chunk=ceil(61/32)=2MB → 只剩 31 段；
        // 必须精确 32 段
        let segs = build_segments(61 * mb, 32);
        assert_eq!(segs.len(), 32, "61MB 32 线程应得 32 段");
        assert_eq!(segs[0].start, 0);
        assert_eq!(segs.last().unwrap().end, 61 * mb - 1);
        // 无缝无重叠覆盖 [0, total)
        for w in segs.windows(2) {
            assert_eq!(w[0].end + 1, w[1].start, "段间不得有缝隙/重叠");
        }
        // 100MB / 32 线程（旧算法只有 25 段）
        let segs = build_segments(100 * mb, 32);
        assert_eq!(segs.len(), 32);
        // 文件小：probe 层 seg_count=min(ceil(10MB/1MB),32)=10，忠实切 10 段
        let segs = build_segments(10 * mb, 10);
        assert_eq!(segs.len(), 10);
        assert_eq!(segs.last().unwrap().end, 10 * mb - 1);
        // 极端：极小文件
        let segs = build_segments(1, 8);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].start, 0);
        assert_eq!(segs[0].end, 0);
        let segs = build_segments(0, 4);
        assert_eq!(segs.len(), 1);
        // 不整除的字节数也要覆盖全文件
        let segs = build_segments(1000, 7);
        assert_eq!(segs.len(), 7);
        let mut covered = 0u64;
        for s in &segs {
            assert_eq!(s.start, covered);
            covered = s.end + 1;
        }
        assert_eq!(covered, 1000);
    }

    mod integration {
        use super::*;
        use std::sync::OnceLock;
        use std::time::{Duration, Instant};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        const TEST_DIR: &str = "/tmp/linbox-http-dl";

        /// 已启动的测试服务器句柄（进程结束自动释放，无需 Drop）。
        static SERVERS: OnceLock<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
            OnceLock::new();

        /// 集成测试共享锁：全局 manager / 端口互不干扰，必须串行。
        static TEST_LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        /// 取锁（容忍 poison：前一个测试断言失败后锁仍可复用）
        fn test_guard() -> std::sync::MutexGuard<'static, ()> {
            TEST_LOCK
                .get_or_init(|| std::sync::Mutex::new(()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        /// 支持 Range 的最小 HTTP 服务器：内存文件 + 206 分块响应 + 可选限速。
        /// `throttle_ms` > 0 时每写 256KB 睡该时长（用于慢速下载测试）。
        fn start_range_server(bytes: Arc<Vec<u8>>, throttle_ms: u64) -> u16 {
            dl_runtime().block_on(async {
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
                let port = listener.local_addr().unwrap().port();
                let handle = dl_runtime().spawn(async move {
                    loop {
                        let (mut sock, _) = match listener.accept().await {
                            Ok(x) => x,
                            Err(_) => break,
                        };
                        let data = bytes.clone();
                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 8192];
                            let mut used = 0usize;
                            loop {
                                let n = match sock.read(&mut buf[used..]).await {
                                    Ok(n) => n,
                                    Err(_) => return,
                                };
                                if n == 0 {
                                    return;
                                }
                                used += n;
                                if used >= 4 && buf[..used].windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                                if used >= buf.len() {
                                    break;
                                }
                            }
                            let head = String::from_utf8_lossy(&buf[..used]);
                            let mut range: Option<String> = None;
                            for line in head.lines() {
                                let l = line.trim_end_matches('\r');
                                if let Some(v) = l.strip_prefix("Range:") {
                                    range = Some(v.trim().to_string());
                                }
                            }
                            let total = data.len() as u64;
                            let (status, start, end, body): (&str, u64, u64, &[u8]) = match &range {
                                Some(r) if !data.is_empty() => {
                                    if let Some(rest) = r.strip_prefix("bytes=") {
                                        let mut it = rest.split('-');
                                        let a: u64 = it.next().unwrap_or("0").parse().unwrap_or(0);
                                        let b: u64 = it
                                            .next()
                                            .unwrap_or("")
                                            .parse()
                                            .unwrap_or(total.saturating_sub(1));
                                        let b = b.min(total.saturating_sub(1));
                                        let s = a as usize;
                                        let e = (b + 1) as usize;
                                        if s >= data.len() {
                                            ("416", 0, 0, &data[0..0])
                                        } else {
                                            ("206", a, b, &data[s..e.min(data.len())])
                                        }
                                    } else {
                                        ("200", 0, total.saturating_sub(1), &data[..])
                                    }
                                }
                                _ => ("200", 0, total.saturating_sub(1), &data[..]),
                            };
                            let resp_head = format!(
                                "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = sock.write_all(resp_head.as_bytes()).await;
                            // 限速写：每 256KB 休眠
                            for chunk in body.chunks(256 * 1024) {
                                let _ = sock.write_all(chunk).await;
                                if throttle_ms > 0 {
                                    tokio::time::sleep(Duration::from_millis(throttle_ms)).await;
                                }
                            }
                            let _ = sock.flush().await;
                        });
                    }
                });
                SERVERS
                    .get_or_init(|| std::sync::Mutex::new(Vec::new()))
                    .lock()
                    .unwrap()
                    .push(handle);
                port
            })
        }

        /// 生成随机测试数据并返回 (bytes, sha256_hex)。
        fn make_blob(mb: u64) -> (Arc<Vec<u8>>, String) {
            let mut data = Vec::with_capacity((mb * 1024 * 1024) as usize);
            let mut chunk = vec![0u8; 1024 * 1024];
            {
                use rand::RngCore as _;
                rand::thread_rng().fill_bytes(&mut chunk);
            }
            for _ in 0..mb {
                data.extend_from_slice(&chunk);
            }
            use sha2::Digest as _;
            let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
            hasher.update(&data);
            let sha = hasher
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            (Arc::new(data), sha)
        }

        fn sha256_of(data: &[u8]) -> String {
            use sha2::Digest as _;
            let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
            hasher.update(data);
            hasher
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        }

        /// 多线程分块下载：验证分片合并后 sha256 与源一致、无残留分片/meta。
        #[test]
        fn multi_thread_download_merges_correctly() {
            let _guard = test_guard();
            let _ = std::fs::remove_dir_all(TEST_DIR);
            let out = format!("{TEST_DIR}/out");
            std::fs::create_dir_all(&out).unwrap();
            let (blob, expect_sha) = make_blob(24);
            let port = start_range_server(blob, 0);

            let cfg = DownloadConfig {
                dir: out.clone(),
                user_agent: "linbox-test/1.0".into(),
                timeout_secs: 30,
                max_concurrent_tasks: 2,
                retries: 2,
                threads_per_task: 8,
                min_chunk_bytes: 1024 * 1024,
            };
            let mgr = DownloadManager::global(Some(cfg));
            let url = format!("http://127.0.0.1:{port}/big.bin");
            let id = mgr
                .add_download(&url, Some(out.clone()), None, Some(8))
                .unwrap();

            let deadline = Instant::now() + Duration::from_secs(90);
            loop {
                assert!(Instant::now() < deadline, "下载超时");
                std::thread::sleep(Duration::from_millis(300));
                let s = mgr
                    .snapshot()
                    .into_iter()
                    .find(|s| s.id == id)
                    .expect("任务丢失");
                if s.status == TaskStatus::Completed {
                    break;
                }
                if matches!(s.status, TaskStatus::Failed) {
                    panic!("任务失败：{}", s.error.clone().unwrap_or_default());
                }
            }

            let final_path = format!("{out}/big.bin");
            let data = std::fs::read(&final_path).expect("最终文件不存在");
            assert_eq!(data.len() as u64, 24 * 1024 * 1024, "文件大小不符");
            assert_eq!(sha256_of(&data), expect_sha, "分片合并结果与源不一致");
            // 分片/meta 现场文件（含任务 id）应已清理
            assert!(!std::path::Path::new(&format!("{out}/.big.bin.{id}.part.0")).exists());
            assert!(!std::path::Path::new(&format!("{out}/.big.bin.{id}.lbm.json")).exists());

            mgr.purge_finished();
            let _ = std::fs::remove_dir_all(TEST_DIR);
        }

        /// 并发任务数受 Semaphore 限制（同时下载数配置）。
        #[test]
        fn concurrent_tasks_limited_by_semaphore() {
            let _guard = test_guard();
            let _ = std::fs::remove_dir_all(TEST_DIR);
            let out = format!("{TEST_DIR}/out2");
            std::fs::create_dir_all(&out).unwrap();
            let port_a = start_range_server(make_blob(1).0, 0);
            // 三个文件用同一端口不同路径
            let cfg = DownloadConfig {
                dir: out.clone(),
                user_agent: "linbox-test/1.0".into(),
                timeout_secs: 30,
                max_concurrent_tasks: 2,
                retries: 2,
                threads_per_task: 2,
                min_chunk_bytes: 1024 * 1024,
            };
            let mgr = DownloadManager::global(Some(cfg));
            let mut ids = Vec::new();
            for name in ["a.bin", "b.bin", "c.bin"] {
                let url = format!("http://127.0.0.1:{port_a}/{name}");
                let id = mgr
                    .add_download(&url, Some(out.clone()), None, Some(2))
                    .unwrap();
                ids.push(id);
            }
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                assert!(Instant::now() < deadline, "并发任务超时");
                std::thread::sleep(Duration::from_millis(300));
                let snaps = mgr.snapshot();
                // 只统计本次添加的任务（manager 可能残留先前测试的已完成任务，
                // 全局 Completed 计数会恒大于 3 导致误等超时）
                let mine: Vec<_> = snaps.iter().filter(|s| ids.contains(&s.id)).collect();
                if mine.len() == 3 && mine.iter().all(|s| s.status == TaskStatus::Completed) {
                    break;
                }
                if mine
                    .iter()
                    .any(|s| matches!(s.status, TaskStatus::Failed | TaskStatus::Cancelled))
                {
                    let s = mine
                        .iter()
                        .find(|s| s.status != TaskStatus::Completed)
                        .unwrap();
                    panic!("任务失败：{}", s.error.clone().unwrap_or_default());
                }
            }
            for name in ["a.bin", "b.bin", "c.bin"] {
                assert!(std::path::Path::new(&format!("{out}/{name}")).exists());
            }
            mgr.purge_finished();
            let _ = std::fs::remove_dir_all(TEST_DIR);
        }

        /// 断点续传：暂停后恢复，最终文件完整。
        #[test]
        fn pause_and_resume_continues() {
            let _guard = test_guard();
            let _ = std::fs::remove_dir_all(TEST_DIR);
            let out = format!("{TEST_DIR}/out3");
            std::fs::create_dir_all(&out).unwrap();
            let (blob, expect_sha) = make_blob(24);
            // 限速 20ms/256KB：约 2s 完成，便于中途暂停
            let port = start_range_server(blob, 20);

            let cfg = DownloadConfig {
                dir: out.clone(),
                user_agent: "linbox-test/1.0".into(),
                timeout_secs: 30,
                max_concurrent_tasks: 2,
                retries: 2,
                threads_per_task: 4,
                min_chunk_bytes: 1024 * 1024,
            };
            let mgr = DownloadManager::global(Some(cfg));
            let url = format!("http://127.0.0.1:{port}/p.bin");
            let id = mgr
                .add_download(&url, Some(out.clone()), None, Some(4))
                .unwrap();

            // 等下载开始后暂停
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                assert!(Instant::now() < deadline, "未进入下载态");
                std::thread::sleep(Duration::from_millis(200));
                let s = mgr.snapshot().into_iter().find(|s| s.id == id).unwrap();
                if s.status == TaskStatus::Downloading && s.downloaded > 0 {
                    mgr.pause(id);
                    break;
                }
            }
            // 等待暂停生效
            std::thread::sleep(Duration::from_millis(800));
            let s = mgr.snapshot().into_iter().find(|s| s.id == id).unwrap();
            assert_eq!(s.status, TaskStatus::Paused, "暂停未生效");
            let paused_at = s.downloaded;
            assert!(paused_at > 0, "暂停时无已下载数据");

            // 恢复 → 续传完成
            mgr.resume(id);
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                assert!(Instant::now() < deadline, "续传超时");
                std::thread::sleep(Duration::from_millis(300));
                let s = mgr.snapshot().into_iter().find(|s| s.id == id).unwrap();
                if s.status == TaskStatus::Completed {
                    break;
                }
                if matches!(s.status, TaskStatus::Failed) {
                    panic!("续传失败：{}", s.error.clone().unwrap_or_default());
                }
            }
            let data = std::fs::read(format!("{out}/p.bin")).unwrap();
            assert_eq!(sha256_of(&data), expect_sha, "断点续传后文件损坏");
            mgr.purge_finished();
            let _ = std::fs::remove_dir_all(TEST_DIR);
        }

        /// 删除下载中的任务：立即从列表消失，现场（分片/meta）被清理。
        #[test]
        fn delete_downloading_task_removes_and_cleans() {
            let _guard = test_guard();
            let _ = std::fs::remove_dir_all(TEST_DIR);
            let out = format!("{TEST_DIR}/out4");
            std::fs::create_dir_all(&out).unwrap();
            let (blob, _) = make_blob(16);
            // 限速 50ms/256KB：16MB ≈ 3.2s，保证删除发生在下载中途
            let port = start_range_server(blob, 50);

            let cfg = DownloadConfig {
                dir: out.clone(),
                user_agent: "linbox-test/1.0".into(),
                timeout_secs: 30,
                max_concurrent_tasks: 2,
                retries: 2,
                threads_per_task: 4,
                min_chunk_bytes: 1024 * 1024,
            };
            let mgr = DownloadManager::global(Some(cfg));
            let url = format!("http://127.0.0.1:{port}/del.bin");
            let id = mgr
                .add_download(&url, Some(out.clone()), None, Some(4))
                .unwrap();

            // 等进入下载态（有数据落盘）后删除
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                assert!(Instant::now() < deadline, "未进入下载态");
                std::thread::sleep(Duration::from_millis(150));
                let s = mgr.snapshot().into_iter().find(|s| s.id == id).unwrap();
                if s.status == TaskStatus::Downloading && s.downloaded > 0 {
                    break;
                }
            }
            mgr.cancel_and_remove(id);
            // 立即从快照消失（一步删除）
            assert!(
                mgr.snapshot().iter().all(|s| s.id != id),
                "删除后任务仍在列表中"
            );
            // 等待线程收尾清理分片与 meta
            std::thread::sleep(Duration::from_millis(1500));
            let leftovers: Vec<String> = std::fs::read_dir(&out)
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
                .filter(|n| {
                    n.starts_with(".del.bin") || n.contains(".part.") || n.contains(".lbm.")
                })
                .collect();
            assert!(leftovers.is_empty(), "删除后残留现场文件：{leftovers:?}");
            let _ = std::fs::remove_dir_all(TEST_DIR);
        }

        /// 添加 → 删除 → 再次添加：第二次添加的任务必须出现在列表中并可完成。
        #[test]
        fn add_delete_then_add_again_works() {
            let _guard = test_guard();
            let _ = std::fs::remove_dir_all(TEST_DIR);
            let out = format!("{TEST_DIR}/out5");
            std::fs::create_dir_all(&out).unwrap();
            let (blob, expect_sha) = make_blob(2);
            let port = start_range_server(blob, 0);

            let cfg = DownloadConfig {
                dir: out.clone(),
                user_agent: "linbox-test/1.0".into(),
                timeout_secs: 30,
                max_concurrent_tasks: 2,
                retries: 2,
                threads_per_task: 4,
                min_chunk_bytes: 1024 * 1024,
            };
            let mgr = DownloadManager::global(Some(cfg));
            let wait_completed = |mgr: &Arc<DownloadManager>, url: &str| {
                let deadline = Instant::now() + Duration::from_secs(60);
                loop {
                    assert!(Instant::now() < deadline, "任务未完成：{url}");
                    std::thread::sleep(Duration::from_millis(200));
                    let snaps = mgr.snapshot();
                    let mine: Vec<_> = snaps.iter().filter(|s| s.url == url).collect();
                    assert!(!mine.is_empty(), "任务不在列表中：{url}");
                    if let Some(s) = mine.first() {
                        if s.status == TaskStatus::Completed {
                            break;
                        }
                        if s.status == TaskStatus::Failed {
                            panic!("任务失败：{}", s.error.clone().unwrap_or_default());
                        }
                    }
                }
            };
            // 第一次添加并等待完成
            let url1 = format!("http://127.0.0.1:{port}/x1.bin");
            let id1 = mgr
                .add_download(&url1, Some(out.clone()), None, None)
                .unwrap();
            wait_completed(&mgr, &url1);
            // 删除
            mgr.cancel_and_remove(id1);
            std::thread::sleep(Duration::from_millis(100));
            // 再次添加
            let url2 = format!("http://127.0.0.1:{port}/x2.bin");
            let id2 = mgr
                .add_download(&url2, Some(out.clone()), None, None)
                .unwrap();
            assert_ne!(id1, id2, "删除后重新添加拿到了相同 id");
            // 新任务必须出现在列表中（snapshot 非空且含新任务）
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                assert!(Instant::now() < deadline, "再次添加后列表为空");
                std::thread::sleep(Duration::from_millis(200));
                let snaps = mgr.snapshot();
                if snaps.iter().any(|s| s.id == id2) {
                    break;
                }
            }
            wait_completed(&mgr, &url2);
            let data = std::fs::read(format!("{out}/x2.bin")).unwrap();
            assert_eq!(sha256_of(&data), expect_sha);
            mgr.purge_finished();
            let _ = std::fs::remove_dir_all(TEST_DIR);
        }

        /// 下载中删除 → 立即用相同 URL 重新添加：新任务必须出现在列表并可完成
        /// （旧任务收尾清理与新任务启动并发，不得互相干扰/吞掉新任务）。
        #[test]
        fn delete_downloading_then_redownload_same_url() {
            let _guard = test_guard();
            let _ = std::fs::remove_dir_all(TEST_DIR);
            let out = format!("{TEST_DIR}/out6");
            std::fs::create_dir_all(&out).unwrap();
            let (blob, expect_sha) = make_blob(16);
            // 限速 50ms/256KB ≈ 3.2s，保证删除发生在下载中途
            let port = start_range_server(blob, 50);

            let cfg = DownloadConfig {
                dir: out.clone(),
                user_agent: "linbox-test/1.0".into(),
                timeout_secs: 30,
                max_concurrent_tasks: 4,
                retries: 2,
                threads_per_task: 4,
                min_chunk_bytes: 1024 * 1024,
            };
            let mgr = DownloadManager::global(Some(cfg));
            let url = format!("http://127.0.0.1:{port}/same.bin");

            // 1) 添加任务 A 并等其进入下载态
            let id_a = mgr
                .add_download(&url, Some(out.clone()), None, Some(4))
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                assert!(Instant::now() < deadline, "A 未进入下载态");
                std::thread::sleep(Duration::from_millis(150));
                let s = mgr.snapshot().into_iter().find(|s| s.id == id_a).unwrap();
                if s.status == TaskStatus::Downloading && s.downloaded > 0 {
                    break;
                }
            }
            // 2) 下载中删除 A
            mgr.cancel_and_remove(id_a);
            assert!(
                mgr.snapshot().iter().all(|s| s.id != id_a),
                "A 删除后仍在列表中"
            );
            // 3) 立刻用相同 URL 重新添加 B
            let id_b = mgr
                .add_download(&url, Some(out.clone()), None, Some(4))
                .unwrap();
            assert_ne!(id_a, id_b);
            // B 必须出现在列表中（不给旧任务收尾留空窗，立即检查后续每次都要在）
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut seen_b = false;
            let mut last_status = String::new();
            loop {
                assert!(Instant::now() < deadline, "B 未出现在列表中");
                std::thread::sleep(Duration::from_millis(300));
                let snaps = mgr.snapshot();
                let b: Vec<_> = snaps.iter().filter(|s| s.id == id_b).collect();
                assert!(!b.is_empty(), "B 在列表中消失了（面板会空）");
                seen_b = true;
                let s = b[0];
                last_status = format!("{:?}", s.status);
                if s.status == TaskStatus::Completed {
                    break;
                }
                if matches!(s.status, TaskStatus::Failed) {
                    panic!("B 失败：{}", s.error.clone().unwrap_or_default());
                }
            }
            assert!(seen_b);
            // 最终文件完整
            let data = std::fs::read(format!("{out}/same.bin")).unwrap();
            assert_eq!(sha256_of(&data), expect_sha, "重下文件损坏");
            let _ = std::fs::remove_dir_all(TEST_DIR);
        }
    }
}
