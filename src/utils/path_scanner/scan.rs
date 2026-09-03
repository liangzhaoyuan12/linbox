//! 路径扫描并发引擎（纯逻辑，无 GTK）。
//!
//! 与 `utils::sniffer::scan` 结构一致：N 个 tokio 工作任务从共享字典中
//! 抢占式消费路径，对目标 URL 发起 HTTP 请求，按状态码 / 响应体大小 / 重定向
//! 判定是否「命中」，通过 mpsc 把事件送回 UI。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::model::path_scanner::{ExcludeSizeRange, PathProbeResult, PathScanEvent};

/// 每完成多少条写一次断点（目前暂未实现断点，保留扩展位）。
const _CHECKPOINT_EVERY: usize = 50;
/// 暂停轮询间隔。
const PAUSE_POLL: Duration = Duration::from_millis(120);

/// 扫描控制句柄。
pub struct Control {
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
}

impl Control {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn set_paused(&self, paused: bool) {
        self.pause.store(paused, Ordering::Relaxed);
    }
}

/// 一次扫描的全部入参。
#[derive(Debug, Clone)]
pub struct PathScanParams {
    /// 目标基础 URL（不含路径，如 `http://example.com`）。
    pub base_url: String,
    /// 字典路径列表。
    pub paths: Arc<Vec<String>>,
    /// HTTP 方法（GET / HEAD）。
    pub method: String,
    /// 附加请求头。
    pub headers: Vec<(String, String)>,
    /// 并发任务数。
    pub concurrency: usize,
    /// 每秒请求数；<= 0 不限速。
    pub rate_per_sec: f64,
    /// 单次请求超时。
    pub timeout: Duration,
    /// 网络错误 / 5xx 的重试次数。
    pub retries: usize,
    /// 排除的状态码集合。
    pub exclude_status: Vec<u16>,
    /// 排除的响应体大小范围。
    pub exclude_size: ExcludeSizeRange,
}

/// 工作任务共享状态。
struct Shared {
    cursor: AtomicUsize,
    completed: AtomicUsize,
    found: AtomicUsize,
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    limiter: Mutex<Instant>,
}

/// 异步任务上下文。
struct Ctx {
    params: PathScanParams,
    shared: Arc<Shared>,
    interval: Duration,
    client: reqwest::Client,
}

/// 启动扫描，立即返回控制句柄。
pub fn start(params: PathScanParams, tx: Sender<PathScanEvent>) -> Control {
    let concurrency = params.concurrency.clamp(1, 512);
    let interval = if params.rate_per_sec > 0.0 {
        Duration::from_secs_f64((1.0 / params.rate_per_sec).max(0.0))
    } else {
        Duration::ZERO
    };
    let total = params.paths.len();

    let shared = Arc::new(Shared {
        cursor: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
        found: AtomicUsize::new(0),
        stop: Arc::new(AtomicBool::new(false)),
        pause: Arc::new(AtomicBool::new(false)),
        limiter: Mutex::new(Instant::now()),
    });

    let control = Control {
        stop: Arc::clone(&shared.stop),
        pause: Arc::clone(&shared.pause),
    };

    let _ = tx.send(PathScanEvent::Started { total });

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none()) // 不自动跟随重定向，手动记录
        .build()
        .expect("构建异步 HTTP 客户端失败");

    let ctx = Arc::new(Ctx {
        params,
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

async fn worker(ctx: Arc<Ctx>, tx: Sender<PathScanEvent>) {
    let paths = &ctx.params.paths;

    loop {
        if ctx.shared.stop.load(Ordering::Relaxed) {
            break;
        }
        // 暂停
        while ctx.shared.pause.load(Ordering::Relaxed) && !ctx.shared.stop.load(Ordering::Relaxed) {
            tokio::time::sleep(PAUSE_POLL).await;
        }
        if ctx.shared.stop.load(Ordering::Relaxed) {
            break;
        }

        let index = ctx.shared.cursor.fetch_add(1, Ordering::Relaxed);
        if index >= paths.len() {
            break;
        }

        throttle(&ctx.shared.limiter, ctx.interval).await;

        let path = &paths[index];
        let url = join_url(&ctx.params.base_url, path);
        let started = Instant::now();

        let mut result = probe_once(&ctx.client, &ctx.params, &url, path).await;

        // 重试
        let mut attempts = 0usize;
        while attempts < ctx.params.retries && result.status == 0 {
            attempts += 1;
            if ctx.shared.stop.load(Ordering::Relaxed) {
                break;
            }
            let backoff = Duration::from_millis(200 * (1u64 << attempts.min(4)));
            tokio::time::sleep(backoff).await;
            result = probe_once(&ctx.client, &ctx.params, &url, path).await;
        }

        let latency_ms = started.elapsed().as_millis() as u64;
        result.latency_ms = latency_ms;

        // 过滤判定
        let excluded = ctx.params.exclude_status.contains(&result.status)
            || (result.status != 0 && ctx.params.exclude_size.is_excluded(result.size));

        ctx.shared.completed.fetch_add(1, Ordering::Relaxed);

        if !excluded {
            ctx.shared.found.fetch_add(1, Ordering::Relaxed);
            let _ = tx.send(PathScanEvent::Result {
                outcome: result,
                index,
            });
        } else if result.status != 0 {
            // 非 0 状态码但被排除，发日志
            let _ = tx.send(PathScanEvent::Log(format!(
                "[排除] {} -> {} ({} B)",
                path, result.status, result.size
            )));
        }
    }

    // 所有路径扫完，发最终事件
    if ctx.shared.completed.load(Ordering::Relaxed) >= paths.len() || ctx.shared.stop.load(Ordering::Relaxed) {
        let _ = tx.send(PathScanEvent::Finished {
            tested: ctx.shared.completed.load(Ordering::Relaxed),
            found: ctx.shared.found.load(Ordering::Relaxed),
        });
    }
}

/// 单次探测。
async fn probe_once(client: &reqwest::Client, params: &PathScanParams, url: &str, path: &str) -> PathProbeResult {
    let method = match params.method.to_ascii_uppercase().as_str() {
        "HEAD" => reqwest::Method::HEAD,
        _ => reqwest::Method::GET,
    };

    let mut request = client
        .request(method, url)
        .timeout(params.timeout);

    for (k, v) in &params.headers {
        if !k.trim().is_empty() {
            request = request.header(k.trim(), v.trim());
        }
    }

    let result = request.send().await;

    match result {
        Ok(response) => {
            let status = response.status();
            let status_text = status
                .canonical_reason()
                .unwrap_or_default()
                .to_string();

            let redirect_url = if status.is_redirection() {
                response
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string()
            } else {
                String::new()
            };

            let body_bytes = response.bytes().await.unwrap_or_default();
            let size = body_bytes.len();

            PathProbeResult {
                path: path.to_string(),
                status: status.as_u16(),
                status_text,
                size,
                redirect_url,
                latency_ms: 0,
            }
        }
        Err(e) => PathProbeResult {
            path: path.to_string(),
            status: 0,
            status_text: format!("网络错误: {e}"),
            size: 0,
            redirect_url: String::new(),
            latency_ms: 0,
        },
    }
}

/// 拼接 base URL 与路径。
fn join_url(base: &str, path: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    let path = path.trim();
    if path.is_empty() {
        return base.to_string();
    }
    let clean_path = path.trim_start_matches('/');
    format!("{base}/{clean_path}")
}

/// 全局限速。
async fn throttle(limiter: &Mutex<Instant>, interval: Duration) {
    if interval.is_zero() {
        return;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn joins_url_correctly() {
        assert_eq!(join_url("http://example.com", "/admin"), "http://example.com/admin");
        assert_eq!(join_url("http://example.com/", "/admin"), "http://example.com/admin");
        assert_eq!(join_url("http://example.com/api", "/admin"), "http://example.com/api/admin");
        assert_eq!(join_url("http://example.com", ""), "http://example.com");
    }

    #[tokio::test]
    async fn throttle_spaces_requests() {
        let limiter = Mutex::new(Instant::now());
        let interval = Duration::from_millis(30);
        let start = Instant::now();
        for _ in 0..4 {
            throttle(&limiter, interval).await;
        }
        assert!(start.elapsed() >= Duration::from_millis(90));
    }

    #[test]
    fn end_to_end_scan() {
        // 创建一个简单的 HTTP 服务器
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
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

                let (status_line, body) = if request.contains("GET /admin HTTP") {
                    ("HTTP/1.1 200 OK", "<html>Admin Panel</html>")
                } else if request.contains("GET /secret HTTP") {
                    ("HTTP/1.1 301 Moved Permanently", "")
                } else {
                    ("HTTP/1.1 404 Not Found", "Not Found")
                };

                let response = format!(
                    "{status_line}\r\nContent-Type: text/html\r\nContent-Length: {}{}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                    if status_line.contains("301") { "\r\nLocation: /admin/login" } else { "" }
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });

        let paths: Vec<String> = vec!["/admin".into(), "/secret".into(), "/notexist".into()];
        let (tx, rx) = std::sync::mpsc::channel();
        let _control = start(
            PathScanParams {
                base_url: format!("http://127.0.0.1:{port}"),
                paths: Arc::new(paths),
                method: "GET".into(),
                headers: Vec::new(),
                concurrency: 2,
                rate_per_sec: 0.0,
                timeout: Duration::from_secs(5),
                retries: 0,
                exclude_status: vec![404],
                exclude_size: ExcludeSizeRange::default(),
            },
            tx,
        );

        let mut found_paths: Vec<String> = Vec::new();
        let mut finished = false;
        while let Ok(ev) = rx.recv_timeout(Duration::from_secs(10)) {
            match ev {
                PathScanEvent::Result { outcome, .. } => {
                    found_paths.push(outcome.path);
                }
                PathScanEvent::Finished { .. } => {
                    finished = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(finished, "扫描未正常结束");
        assert!(found_paths.contains(&"/admin".to_string()), "应找到 /admin");
        assert!(found_paths.contains(&"/secret".to_string()), "应找到 /secret");
        assert!(!found_paths.contains(&"/notexist".to_string()), "不应包含 404");
    }
}
