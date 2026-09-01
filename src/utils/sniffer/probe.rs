//! OpenAI 兼容端点的探测（纯逻辑，无 GTK，可 `cargo test`）。
//!
//! 只做三件事：拼 URL、发一次 OpenAI 格式请求、按 HTTP 状态码给出判定。
//! 网络 IO 使用阻塞式 `ureq`，因此**必须在后台线程调用**。

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::model::sniffer::{ProbeOutcome, Verdict, DEFAULT_ENDPOINT, SNIPPET_MAX};

/// 探测端点（不含 Key）。
#[derive(Debug, Clone)]
pub struct ProbeTarget {
    /// Base URL，如 `https://api.openai.com/v1`。
    pub base_url: String,
    /// 端点路径，如 `/chat/completions` 或 `/models`。
    pub endpoint: String,
    /// 请求体中的模型名。
    pub model: String,
    /// 附加请求头 (键, 值)。
    pub headers: Vec<(String, String)>,
    /// 单次请求超时。
    pub timeout: Duration,
}

impl Default for ProbeTarget {
    fn default() -> Self {
        ProbeTarget {
            base_url: String::new(),
            endpoint: DEFAULT_ENDPOINT.to_string(),
            model: String::new(),
            headers: Vec::new(),
            timeout: Duration::from_secs(15),
        }
    }
}

/// 探测方式：`/models` 用 GET，其余（如 `/chat/completions`）用 POST。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeMethod {
    Get,
    Post,
}

impl ProbeTarget {
    /// 根据端点推断请求方法。
    pub fn method(&self) -> ProbeMethod {
        let ep = self.endpoint.trim_end_matches('/').to_ascii_lowercase();
        if ep.ends_with("/models") {
            ProbeMethod::Get
        } else {
            ProbeMethod::Post
        }
    }
}

/// 当前 UNIX 时间戳（秒）；时钟异常时回落到 0。
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 拼接 base URL 与端点，避免重复或缺失 `/`。
///
/// - `https://a.com/v1` + `/chat/completions` → `https://a.com/v1/chat/completions`
/// - `https://a.com/v1/` + `/chat/completions` → `https://a.com/v1/chat/completions`
/// - 若 base URL 已经以端点结尾，则原样返回（用户把完整地址填进了 Base URL）。
/// - 端点为空时回落到 [`DEFAULT_ENDPOINT`]。
pub fn join_url(base_url: &str, endpoint: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    let ep = endpoint.trim();
    let ep = if ep.is_empty() { DEFAULT_ENDPOINT } else { ep };
    let ep = if ep.starts_with('/') { ep.to_string() } else { format!("/{ep}") };
    let normalized = ep.trim_end_matches('/').to_ascii_lowercase();
    if base.to_ascii_lowercase().ends_with(&normalized) {
        return base.to_string();
    }
    format!("{base}{ep}")
}

/// OpenAI 聊天补全请求体（最小开销：`max_tokens = 1`）。
pub fn chat_body(model: &str) -> String {
    serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1,
        "stream": false,
    })
    .to_string()
}

/// 把「每行一条 `Key: Value`」的文本解析成请求头列表。
///
/// 忽略空行与 `#` 开头的注释行；缺少冒号的行视为无效并跳过。
pub fn parse_header_lines(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            let k = k.trim();
            if k.is_empty() {
                return None;
            }
            Some((k.to_string(), v.trim().to_string()))
        })
        .collect()
}

/// 响应体摘要：截断到 `SNIPPET_MAX` 并压缩空白，方便单行展示。
fn snippet_of(body: &str) -> String {
    let one_line: String = body
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if one_line.chars().count() <= SNIPPET_MAX {
        one_line
    } else {
        let cut: String = one_line.chars().take(SNIPPET_MAX).collect();
        format!("{cut} …（已截断）")
    }
}

/// 从响应体中提炼一句话：优先取 JSON 的 `error.message`。
fn detail_of(body: &str, fallback: &str) -> String {
    if body.trim().is_empty() {
        return fallback.to_string();
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        let msg = value
            .pointer("/error/message")
            .or_else(|| value.pointer("/error"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| value.get("message").and_then(|v| v.as_str()).map(|s| s.to_string()));
        if let Some(m) = msg {
            let m: String = m.chars().take(200).collect();
            return m;
        }
    }
    let flat = snippet_of(body);
    flat.chars().take(200).collect()
}

/// 对单个候选 Key 发起一次探测。
///
/// `agent` 由调用方持有（每线程一个，复用连接）。
pub fn probe(agent: &ureq::Agent, target: &ProbeTarget, key: &str) -> ProbeOutcome {
    let url = join_url(&target.base_url, &target.endpoint);
    let body = chat_body(&target.model);

    let mut request = agent
        .request(target.method().as_str(), &url)
        .timeout(target.timeout)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Accept", "application/json");

    if target.method() == ProbeMethod::Post {
        request = request.set("Content-Type", "application/json");
    }
    for (k, v) in &target.headers {
        let k = k.trim();
        if k.is_empty() || k.eq_ignore_ascii_case("authorization") {
            continue; // 不允许覆盖鉴权头
        }
        request = request.set(k, v.trim());
    }

    let started = Instant::now();
    let outcome = if target.method() == ProbeMethod::Post {
        request.send_string(&body)
    } else {
        request.call()
    };
    let latency_ms = started.elapsed().as_millis() as u64;

    match outcome {
        Ok(response) => finish(response.status(), response.status_text().to_string(), response, latency_ms),
        Err(ureq::Error::Status(code, response)) => {
            finish(code, response.status_text().to_string(), response, latency_ms)
        }
        Err(other) => ProbeOutcome {
            verdict: Verdict::NetworkError,
            status: 0,
            status_text: String::new(),
            latency_ms,
            body: String::new(),
            detail: format!("网络请求失败：{other}"),
        },
    }
}

fn finish(
    status: u16,
    status_text: String,
    response: ureq::Response,
    latency_ms: u64,
) -> ProbeOutcome {
    let retry_after = response.header("retry-after").map(|s| s.to_string());
    let body = response.into_string().unwrap_or_default();
    let verdict = Verdict::from_status(status);
    let mut detail = detail_of(&body, &status_text);
    if let Some(ra) = retry_after {
        if !ra.trim().is_empty() {
            detail = format!("{detail}（Retry-After: {ra}）");
        }
    }
    ProbeOutcome {
        verdict,
        status,
        status_text,
        latency_ms,
        body: snippet_of(&body),
        detail,
    }
}

impl ProbeMethod {
    /// ureq 需要的方法名字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeMethod::Get => "GET",
            ProbeMethod::Post => "POST",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_url_without_duplicate_slash() {
        assert_eq!(
            join_url("https://a.com/v1", "/chat/completions"),
            "https://a.com/v1/chat/completions"
        );
        assert_eq!(
            join_url("https://a.com/v1/", "/chat/completions"),
            "https://a.com/v1/chat/completions"
        );
        assert_eq!(join_url("https://a.com/v1", "chat/completions"), "https://a.com/v1/chat/completions");
    }

    #[test]
    fn does_not_duplicate_when_base_already_has_endpoint() {
        assert_eq!(
            join_url("https://a.com/v1/chat/completions", "/chat/completions"),
            "https://a.com/v1/chat/completions"
        );
    }

    #[test]
    fn empty_endpoint_falls_back_to_default() {
        assert_eq!(join_url("https://a.com/v1", ""), "https://a.com/v1/chat/completions");
    }

    #[test]
    fn method_depends_on_endpoint() {
        let mut t = ProbeTarget::default();
        assert_eq!(t.method(), ProbeMethod::Post);
        t.endpoint = "/models".into();
        assert_eq!(t.method(), ProbeMethod::Get);
        t.endpoint = "/v1/models".into();
        assert_eq!(t.method(), ProbeMethod::Get);
    }

    #[test]
    fn chat_body_is_minimal_json() {
        let body = chat_body("gpt-3.5-turbo");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["model"], "gpt-3.5-turbo");
        assert_eq!(v["max_tokens"], 1);
        assert_eq!(v["stream"], false);
    }

    #[test]
    fn extracts_error_message_from_json() {
        let body = r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error"}}"#;
        assert_eq!(detail_of(body, "fallback"), "Incorrect API key provided");
    }

    #[test]
    fn detail_falls_back_to_raw_text() {
        assert_eq!(detail_of("plain text body", "x"), "plain text body");
        assert_eq!(detail_of("", "x"), "x");
    }

    #[test]
    fn parses_header_lines() {
        let headers = parse_header_lines("X-A: 1\n# comment\n\nX-B:two\nno-colon\n");
        assert_eq!(
            headers,
            vec![("X-A".to_string(), "1".to_string()), ("X-B".to_string(), "two".to_string())]
        );
    }

    #[test]
    fn snippet_is_truncated_and_single_line() {
        let long = "a ".repeat(2000);
        let s = snippet_of(&long);
        assert!(s.chars().count() <= SNIPPET_MAX + 20);
        assert!(!s.contains('\n'));
        assert_eq!(snippet_of("a\n\n  b  "), "a b");
    }
}
