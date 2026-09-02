//! 并发扫描引擎（纯逻辑，无 GTK）。
//!
//! 职责：把候选 Key 字典分给 N 个 tokio 工作任务，按限速逐个异步探测，
//! 把每条结果与生命周期事件通过 `std::sync::mpsc` 送回调用方（页面层在
//! 主循环里排空队列）。
//!
//! ## 并发模型
//! - `cursor` 是无锁原子下标，谁抢到谁处理，天然负载均衡。
//! - 限速用一把 `Mutex<Instant>` 做「令牌桶」：拿到锁后睡到下一个可发送时刻，
//!   因此速率上限是**全局**的，而不是每任务各自一份。
//! - 网络 IO 是异步 `reqwest`，所有任务共享一个连接池（`reqwest::Client`）。
//!
//! ## 运行时
//! 任务通过 [`super::runtime()`] 提供的全局多线程 tokio 运行时调度，
//! 由页面（GTK 主线程）发起 `start()`，事件经 mpsc 异步回流。
//!
//! ## 断点续跑
//! 并发会让完成顺序乱序，所以游标本身不能直接当断点。这里额外维护
//! `inflight`（在途下标集合），断点位置取 `min(inflight)`；集合为空时取
//! `cursor`。这保证「断点位置之前的所有候选都已经被处理过」，不会漏测。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::model::sniffer::{Checkpoint, ProbeOutcome, ValidKeyRecord, Verdict};

use super::probe::{self, ProbeTarget};
use super::store;

/// 每完成多少条写一次断点。
const CHECKPOINT_EVERY: usize = 25;
/// 暂停轮询间隔。
const PAUSE_POLL: Duration = Duration::from_millis(120);

/// 扫描结束的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// 字典跑完。
    Completed,
    /// 用户主动停止。
    Stopped,
}

impl StopReason {
    pub fn label(&self) -> &'static str {
        match self {
            StopReason::Completed => "已完成",
            StopReason::Stopped => "已停止",
        }
    }
}

/// 扫描过程中回传给 UI 的事件。
#[derive(Debug, Clone)]
pub enum ScanEvent {
    /// 引擎已启动。
    Started {
        /// 字典总条数。
        total: usize,
        /// 本次实际起始下标（断点续跑时 > 0）。
        start_index: usize,
    },
    /// 某一条候选探测完毕。
    Result {
        /// 该候选在字典中的下标。
        index: usize,
        key: String,
        outcome: ProbeOutcome,
        /// 含重试在内的总耗时（毫秒）。
        elapsed_ms: u64,
        /// 重试次数。
        attempts: usize,
    },
    /// 提示信息（重试、断点写入、落库失败等）。
    Log(String),
    /// 所有工作线程已退出。
    Finished {
        reason: StopReason,
        /// 本次（自 start_index 起）已测数量。
        tested: usize,
        valid: usize,
    },
}

/// 一次扫描的全部入参。
#[derive(Debug, Clone)]
pub struct ScanParams {
    /// 平台名（写入命中记录与断点）。
    pub platform: String,
    /// 字典指纹，用于校验断点是否仍适用于当前配置。
    pub fingerprint: String,
    /// 探测目标（不含 Key）。
    pub target: ProbeTarget,
    /// 候选 Key 字典（生成一次后由所有任务共享）。
    pub keys: Arc<Vec<String>>,
    /// 起始下标（断点续跑时 > 0）。
    pub start_index: usize,
    /// 并发任务数。
    pub concurrency: usize,
    /// 每秒请求数；<= 0 表示不限速。
    pub rate_per_sec: f64,
    /// 网络错误 / 5xx / 429 的自动重试次数。
    pub retries: usize,
    /// 命中即写入本地有效 Key 库（SQLite）。
    pub persist_valid: bool,
    /// 是否写断点（关闭「断点续跑」时为 false）。
    pub write_checkpoint: bool,
}

/// 扫描控制句柄（发给工作任务的信号）。
pub struct Control {
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
}

impl Control {
    /// 请求停止：在途请求会跑完，未开始的候选不再分发。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// 暂停 / 继续。
    pub fn set_paused(&self, paused: bool) {
        self.pause.store(paused, Ordering::Relaxed);
    }
}

/// 工作任务共享的状态。
struct Shared {
    /// 下一个待分发下标。
    cursor: AtomicUsize,
    /// 已分发但尚未完成的在途下标。
    inflight: Mutex<Vec<usize>>,
    /// 已完成计数。
    completed: AtomicUsize,
    /// 命中计数。
    valid: AtomicUsize,
    /// 上次写断点时的 completed 值。
    last_checkpoint: AtomicUsize,
    /// 存活任务数，最后一个退出的任务负责收尾。
    alive: AtomicUsize,
    /// 限速器：下一次允许发送请求的时刻。
    limiter: Mutex<Instant>,
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
}

/// 异步任务间传递的只读上下文。
struct Ctx {
    params: ScanParams,
    shared: Arc<Shared>,
    /// 两次请求之间至少间隔多久（0 表示不限速）。
    interval: Duration,
    /// 共享的 HTTP 连接池（所有任务复用）。
    client: reqwest::Client,
}

/// 启动扫描，立即返回控制句柄。
///
/// `tx` 收到的事件可在任意线程产生，UI 侧需要在主循环里排空。
pub fn start(params: ScanParams, tx: Sender<ScanEvent>) -> Control {
    let concurrency = params.concurrency.clamp(1, 512);
    let interval = if params.rate_per_sec > 0.0 {
        Duration::from_secs_f64((1.0 / params.rate_per_sec).max(0.0))
    } else {
        Duration::ZERO
    };
    let total = params.keys.len();
    let start_index = params.start_index.min(total);

    let shared = Arc::new(Shared {
        cursor: AtomicUsize::new(start_index),
        inflight: Mutex::new(Vec::new()),
        completed: AtomicUsize::new(0),
        valid: AtomicUsize::new(0),
        last_checkpoint: AtomicUsize::new(0),
        alive: AtomicUsize::new(concurrency),
        limiter: Mutex::new(Instant::now()),
        stop: Arc::new(AtomicBool::new(false)),
        pause: Arc::new(AtomicBool::new(false)),
    });

    let control = Control {
        stop: Arc::clone(&shared.stop),
        pause: Arc::clone(&shared.pause),
    };

    let _ = tx.send(ScanEvent::Started { total, start_index });

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("构建异步 HTTP 客户端失败");
    let ctx = Arc::new(Ctx {
        params: ScanParams { start_index, ..params },
        shared: Arc::clone(&shared),
        interval,
        client,
    });

    for _ in 0..concurrency {
        let ctx = Arc::clone(&ctx);
        let tx = tx.clone();
        super::runtime().spawn(async move {
            worker(ctx, tx).await;
        });
    }

    control
}

async fn worker(ctx: Arc<Ctx>, tx: Sender<ScanEvent>) {
    let keys = &ctx.params.keys;

    loop {
        if ctx.shared.stop.load(Ordering::Relaxed) {
            break;
        }
        // 暂停：轮询等待，期间仍可被 stop 打断
        while ctx.shared.pause.load(Ordering::Relaxed) && !ctx.shared.stop.load(Ordering::Relaxed)
        {
            tokio::time::sleep(PAUSE_POLL).await;
        }
        if ctx.shared.stop.load(Ordering::Relaxed) {
            break;
        }

        let index = ctx.shared.cursor.fetch_add(1, Ordering::Relaxed);
        if index >= keys.len() {
            break;
        }
        {
            let mut inflight = ctx.shared.inflight.lock().unwrap();
            inflight.push(index);
        }

        throttle(&ctx.shared.limiter, ctx.interval).await;

        let key = keys[index].clone();
        let started = Instant::now();
        let mut outcome = probe::probe(&ctx.client, &ctx.params.target, &key).await;
        let mut attempts = 0usize;
        while attempts < ctx.params.retries && outcome.verdict.retryable() {
            attempts += 1;
            if ctx.shared.stop.load(Ordering::Relaxed) {
                break;
            }
            // 指数退避：200ms → 400 → 800 → 1600 → 1600 …
            let backoff = Duration::from_millis(200 * (1u64 << attempts.min(4)));
            let _ = tx.send(ScanEvent::Log(format!(
                "#{} {}（{}），{} ms 后重试（第 {} 次）",
                index,
                outcome.verdict.label(),
                outcome.detail,
                backoff.as_millis(),
                attempts
            )));
            tokio::time::sleep(backoff).await;
            outcome = probe::probe(&ctx.client, &ctx.params.target, &key).await;
        }
        let elapsed_ms = started.elapsed().as_millis() as u64;

        if outcome.verdict.is_valid() {
            ctx.shared.valid.fetch_add(1, Ordering::Relaxed);
            if ctx.params.persist_valid {
                let record = ValidKeyRecord {
                    platform: ctx.params.platform.clone(),
                    base_url: ctx.params.target.base_url.clone(),
                    endpoint: ctx.params.target.endpoint.clone(),
                    model: ctx.params.target.model.clone(),
                    key: key.clone(),
                    status: outcome.status,
                    latency_ms: outcome.latency_ms,
                    found_at: probe::now_unix(),
                    snippet: outcome.body.clone(),
                };
                // 异步写 SQLite（UNIQUE 约束天然去重）
                if let Err(e) = store::append_valid(&record).await {
                    let _ = tx.send(ScanEvent::Log(format!("命中记录落库失败：{e}")));
                }
            }
        }

        let completed = ctx.shared.completed.fetch_add(1, Ordering::Relaxed) + 1;
        {
            let mut inflight = ctx.shared.inflight.lock().unwrap();
            inflight.retain(|&i| i != index);
        }

        let _ = tx.send(ScanEvent::Result {
            index,
            key,
            outcome,
            elapsed_ms,
            attempts,
        });

        if ctx.params.write_checkpoint
            && completed - ctx.shared.last_checkpoint.load(Ordering::Relaxed) >= CHECKPOINT_EVERY
        {
            write_checkpoint(&ctx).await;
        }
    }

    // 最后一个退出的任务负责收尾
    if ctx.shared.alive.fetch_sub(1, Ordering::AcqRel) == 1 {
        // 停止 = 主动放弃本轮进度：不再写断点，避免「页面刚清掉、这里又写回」。
        // 仅自然跑完才写最终断点（应用中途退出时断点仍可续跑）。
        if !ctx.shared.stop.load(Ordering::Relaxed) {
            write_checkpoint(&ctx).await;
        }
        let reason = if ctx.shared.stop.load(Ordering::Relaxed) {
            StopReason::Stopped
        } else {
            StopReason::Completed
        };
        let _ = tx.send(ScanEvent::Finished {
            reason,
            tested: ctx.shared.completed.load(Ordering::Relaxed),
            valid: ctx.shared.valid.load(Ordering::Relaxed),
        });
    }
}

/// 全局限速：保证任意两次请求之间至少间隔 `interval`。
async fn throttle(limiter: &Mutex<Instant>, interval: Duration) {
    if interval.is_zero() {
        return;
    }
    // 锁的作用域在 await 前结束，避免 MutexGuard 越过 .await（非 Send）
    let sleep_for;
    {
        let mut next = limiter.lock().unwrap();
        let now = Instant::now();
        sleep_for = if *next > now { Some(*next - now) } else { None };
        *next = std::cmp::max(now, *next) + interval;
    }
    if let Some(d) = sleep_for {
        tokio::time::sleep(d).await;
    }
}

/// 写入断点：位置取「在途下标的最小值」，保证之前的候选都已处理。
async fn write_checkpoint(ctx: &Ctx) {
    let cursor = {
        let inflight = ctx.shared.inflight.lock().unwrap();
        inflight
            .iter()
            .copied()
            .min()
            .unwrap_or_else(|| ctx.shared.cursor.load(Ordering::Relaxed))
    };
    let tested = ctx.shared.completed.load(Ordering::Relaxed);
    ctx.shared.last_checkpoint.store(tested, Ordering::Relaxed);

    let cp = Checkpoint {
        platform: ctx.params.platform.clone(),
        fingerprint: ctx.params.fingerprint.clone(),
        total: ctx.params.keys.len(),
        cursor,
        tested,
        valid: ctx.shared.valid.load(Ordering::Relaxed),
        updated_at: probe::now_unix(),
    };
    if ctx.params.write_checkpoint {
        if let Err(e) = store::save_checkpoint(&cp) {
            eprintln!("[linbox] 断点写入失败：{e}");
        }
    }
}

/// 供页面层判断某个判定是否需要重试（与引擎内部逻辑保持一致）。
pub fn is_retryable(v: Verdict) -> bool {
    v.retryable()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn throttle_spaces_requests() {
        let limiter = Mutex::new(Instant::now());
        let interval = Duration::from_millis(30);
        let start = Instant::now();
        for _ in 0..4 {
            throttle(&limiter, interval).await;
        }
        // 4 次请求 → 至少 3 个间隔
        assert!(start.elapsed() >= Duration::from_millis(90));
    }

    #[tokio::test]
    async fn zero_interval_does_not_sleep() {
        let limiter = Mutex::new(Instant::now());
        let start = Instant::now();
        for _ in 0..200 {
            throttle(&limiter, Duration::ZERO).await;
        }
        assert!(start.elapsed() < Duration::from_millis(200));
    }

    /// 起一个极简 HTTP 服务器：只有 `sk-good` 返回 200，其余返回 401。
    ///
    /// 用来在不依赖任何公网服务的前提下，端到端验证「并发扫描 → 状态码判定」。
    fn spawn_fake_api() -> u16 {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定本地端口失败");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let (status_line, body) = if request.contains("Bearer sk-good") {
                    ("HTTP/1.1 200 OK", r#"{"choices":[{"message":{"content":"hi"}}]}"#)
                } else {
                    (
                        "HTTP/1.1 401 Unauthorized",
                        r#"{"error":{"message":"Incorrect API key provided"}}"#,
                    )
                };
                let response = format!(
                    "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        port
    }

    #[test]
    fn end_to_end_scan_classifies_status_codes() {
        let port = spawn_fake_api();
        let keys: Vec<String> = vec!["sk-bad-1", "sk-bad-2", "sk-good", "sk-bad-3"]
            .into_iter()
            .map(String::from)
            .collect();

        let (tx, rx) = std::sync::mpsc::channel();
        let control = start(
            ScanParams {
                platform: "本地假 API".into(),
                fingerprint: "test".into(),
                target: ProbeTarget {
                    base_url: format!("http://127.0.0.1:{port}/v1"),
                    endpoint: "/chat/completions".into(),
                    model: "fake-model".into(),
                    headers: Vec::new(),
                    timeout: Duration::from_secs(5),
                },
                keys: Arc::new(keys),
                start_index: 0,
                concurrency: 4,
                rate_per_sec: 0.0,
                retries: 0,
                // 测试中不要污染用户真实的本地库与断点文件
                persist_valid: false,
                write_checkpoint: false,
            },
            tx,
        );

        let mut valid_keys: Vec<String> = Vec::new();
        let mut unauthorized = 0usize;
        let mut finished = false;
        while let Ok(ev) = rx.recv_timeout(Duration::from_secs(20)) {
            match ev {
                ScanEvent::Result { key, outcome, .. } => match outcome.verdict {
                    Verdict::Valid => valid_keys.push(key),
                    Verdict::Unauthorized => unauthorized += 1,
                    other => panic!("意外的判定：{other:?}"),
                },
                ScanEvent::Finished { .. } => {
                    finished = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(finished, "扫描未正常结束");
        assert_eq!(valid_keys, vec!["sk-good".to_string()]);
        assert_eq!(unauthorized, 3);
    }

    #[test]
    fn stop_mid_scan_always_emits_finished() {
        let port = spawn_fake_api();
        let keys: Vec<String> = (0..400).map(|i| format!("sk-stop-{i:03}")).collect();
        let (tx, rx) = std::sync::mpsc::channel();
        let control = start(
            ScanParams {
                platform: "p".into(),
                fingerprint: "f".into(),
                target: ProbeTarget {
                    base_url: format!("http://127.0.0.1:{port}/v1"),
                    endpoint: "/chat/completions".into(),
                    model: "m".into(),
                    headers: Vec::new(),
                    timeout: Duration::from_secs(5),
                },
                keys: Arc::new(keys),
                start_index: 0,
                concurrency: 4,
                // 限速 50/s：400 条 ≈ 8 秒才能扫完，保证 stop 时扫描必然还在进行中
                rate_per_sec: 50.0,
                retries: 0,
                persist_valid: false,
                write_checkpoint: false,
            },
            tx,
        );
        // 让扫描跑一会儿（此时有请求在途/暂停轮询的多种状态并存）
        std::thread::sleep(Duration::from_millis(600));
        control.stop();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut saw_stopped = false;
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(ScanEvent::Finished { reason, .. }) => {
                    // 只应发一次，且 stop 后必为 Stopped
                    assert_eq!(reason, StopReason::Stopped);
                    saw_stopped = true;
                    break;
                }
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if std::time::Instant::now() > deadline {
                        panic!("停止后 10 秒内未收到 Finished（saw_stopped={saw_stopped}）");
                    }
                }
                Err(e) => panic!("通道错误 {e}"),
            }
        }
        assert!(saw_stopped);
    }

    #[test]
    fn start_finishes_immediately_on_empty_dictionary() {
        let (tx, rx) = std::sync::mpsc::channel();
        let control = start(
            ScanParams {
                platform: "p".into(),
                fingerprint: "f".into(),
                target: ProbeTarget::default(),
                keys: Arc::new(Vec::new()),
                start_index: 0,
                concurrency: 2,
                rate_per_sec: 0.0,
                retries: 0,
                persist_valid: false,
                write_checkpoint: false,
            },
            tx,
        );
        control.stop();
        let mut saw_finished = false;
        while let Ok(ev) = rx.recv_timeout(Duration::from_secs(5)) {
            if matches!(ev, ScanEvent::Finished { .. }) {
                saw_finished = true;
                break;
            }
        }
        assert!(saw_finished);
    }
}