//! API Key 嗅探模块的数据模型（纯数据，无 UI 依赖）。
//!
//! 参考 `docs/项目结构规划书.md` §3.8：本文件只定义 `struct` / `enum` 与少量
//! 纯函数，被 `utils::sniffer`（逻辑层）与 `page::api_key_sniffer`（展示层）
//! 共同引用，是二者之间的契约层。
//!
//! ## 用途边界
//! 本模块是「OpenAI 兼容端点 + Key 有效性批量校验器」，面向**你自己拥有或已
//! 获得明确授权的平台**：自建网关的密钥强度审计、疑似泄漏 Key 的复核、授权
//! 范围内的红队评估。请勿用于未经授权的系统。

use serde::{Deserialize, Serialize};

/// 默认探测端点（OpenAI 聊天补全格式）。
pub const DEFAULT_ENDPOINT: &str = "/chat/completions";
/// 默认请求模型。
pub const DEFAULT_MODEL: &str = "gpt-3.5-turbo";
/// 单条命中记录中保存的响应体摘要长度上限。
pub const SNIPPET_MAX: usize = 512;

// ---------------------------------------------------------------------------
// 平台配置
// ---------------------------------------------------------------------------

/// 一个用户自定义平台（或多个 OpenAI 兼容网关）的配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformConfig {
    /// 平台名，作为唯一键（OpenAI / DeepSeek / Moonshot / 自建网关 …）。
    pub name: String,
    /// Base URL，OpenAI 格式根地址，如 `https://api.openai.com/v1`。
    pub base_url: String,
    /// 探测端点，如 `/chat/completions`（以 `/` 开头）。
    pub endpoint: String,
    /// 请求体中使用的模型名。
    pub model: String,
    /// API Key 正则规则，用于批量生成候选 Key 字典。
    pub pattern: String,
    /// 附加请求头 (键, 值)，逐条随请求发送。
    pub headers: Vec<(String, String)>,
    /// 备注。
    pub note: String,
    /// 该平台独立的扫描参数（并发/限速/超时/重试/字典规模/断点/入库）。
    /// 老配置文件没有此字段时回落到默认值。
    #[serde(default)]
    pub scan: ScanConfig,
}

impl PlatformConfig {
    pub fn new(name: &str, base_url: &str, pattern: &str, model: &str, note: &str) -> Self {
        PlatformConfig {
            name: name.to_string(),
            base_url: base_url.to_string(),
            endpoint: DEFAULT_ENDPOINT.to_string(),
            model: model.to_string(),
            pattern: pattern.to_string(),
            headers: Vec::new(),
            note: note.to_string(),
            scan: ScanConfig::default(),
        }
    }

    /// 校验配置是否合法，返回第一条错误。
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("平台名不能为空".into());
        }
        let url = self.base_url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("Base URL 必须以 http:// 或 https:// 开头".into());
        }
        if self.pattern.trim().is_empty() {
            return Err("API Key 正则规则不能为空".into());
        }
        let ep = self.endpoint.trim();
        if !ep.is_empty() && !ep.starts_with('/') {
            return Err("探测端点必须以 / 开头（如 /chat/completions）".into());
        }
        if self.model.trim().is_empty() {
            return Err("模型名不能为空".into());
        }
        Ok(())
    }
}

/// 内置预设平台（各厂商公开文档中的标准 OpenAI 兼容地址）。
///
/// 正则只描述**公开可见的 Key 形态示例**，不代表任何真实凭据；
/// 生成量由页面的「最大生成条数」封顶。
pub fn builtin_platforms() -> Vec<PlatformConfig> {
    vec![
        PlatformConfig::new(
            "自建网关（本地示例）",
            "http://127.0.0.1:8000/v1",
            r"^sk-local-[0-9]{6}$",
            "gpt-3.5-turbo",
            "本地 one-api / new-api 等自建网关，6 位数字 Key，便于验证流程",
        ),
        PlatformConfig::new(
            "OpenAI",
            "https://api.openai.com/v1",
            r"^sk-[A-Za-z0-9]{32}$",
            DEFAULT_MODEL,
            "官方地址；真实 Key 熵极高，仅用于验证请求格式",
        ),
        PlatformConfig::new(
            "DeepSeek",
            "https://api.deepseek.com/v1",
            r"^sk-[a-f0-9]{32}$",
            "deepseek-chat",
            "官方地址；32 位十六进制",
        ),
        PlatformConfig::new(
            "Moonshot (Kimi)",
            "https://api.moonshot.cn/v1",
            r"^sk-[A-Za-z0-9]{32}$",
            "moonshot-v1-8k",
            "官方地址",
        ),
        PlatformConfig::new(
            "智谱 GLM",
            "https://open.bigmodel.cn/api/paas/v4",
            r"^[0-9a-f]{32}\.[A-Za-z0-9]{16}$",
            "glm-4-flash",
            "官方地址；Key 形如 {32位hex}.{16位}",
        ),
    ]
}

// ---------------------------------------------------------------------------
// 字典生成参数
// ---------------------------------------------------------------------------

/// 字典生成与扫描的运行参数（跨会话持久化）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanConfig {
    /// 并发线程数。
    pub concurrency: usize,
    /// 限速：每秒请求数，0 表示不限。
    pub rate_per_sec: f64,
    /// 单次请求超时（秒）。
    pub timeout_secs: u64,
    /// 网络错误 / 5xx / 429 的自动重试次数。
    pub retries: usize,
    /// 字典最大生成条数（密钥空间更大时截断）。
    /// 最大生成条数（u128 无硬性上限，实际受运存约束）。
    pub max_candidates: u128,
    /// `*`、`+`、`{n,}` 等无界量词的展开上限。
    pub unbounded_repeat: usize,
    /// 是否启用断点续跑。
    pub resume: bool,
    /// 命中即写入本地库。
    pub persist_valid: bool,
    /// 日志区最多保留的行数。
    pub log_limit: usize,
}

impl Default for ScanConfig {
    fn default() -> Self {
        ScanConfig {
            // 默认保守：低并发 + 有限速，避免对目标平台造成压力
            concurrency: 4,
            rate_per_sec: 5.0,
            timeout_secs: 15,
            retries: 1,
            max_candidates: 100_000,
            unbounded_repeat: 3,
            resume: true,
            persist_valid: true,
            log_limit: 400,
        }
    }
}

/// 页面上可一键插入的正则模板。
pub const PATTERN_TEMPLATES: &[(&str, &str)] = &[
    ("sk- + 32 位字母数字", r"^sk-[A-Za-z0-9]{32}$"),
    ("sk- + 32 位十六进制", r"^sk-[a-f0-9]{32}$"),
    ("sk-proj- + 48 位", r"^sk-proj-[A-Za-z0-9_-]{48}$"),
    ("sk- + 6 位数字", r"^sk-[0-9]{6}$"),
    ("带环境前缀（分支）", r"^sk-(dev|test|prod)-[0-9]{4}$"),
    ("纯 16 位字母数字", r"^[A-Za-z0-9]{16}$"),
    ("hex.hex 双段式", r"^[0-9a-f]{32}\.[A-Za-z0-9]{16}$"),
    ("自定义", ""),
];

/// "自定义"模板在 `PATTERN_TEMPLATES` 中的索引。
pub const CUSTOM_TEMPLATE_INDEX: usize = 7;

// ---------------------------------------------------------------------------
// 探测判定
// ---------------------------------------------------------------------------

/// 单个候选 Key 的探测判定结果。
///
/// 判定依据主要是 HTTP 状态码：
/// - `2xx` → [`Verdict::Valid`]（有效 Key）
/// - `429` → [`Verdict::RateLimited`]（限流）
/// - `401` / `403` → [`Verdict::Unauthorized`]（鉴权失败）
/// - 其余 4xx / 5xx / 网络错误 → 对应的分类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// 2xx：Key 被接受。
    Valid,
    /// 401 / 403：鉴权失败（Key 无效或无权访问）。
    Unauthorized,
    /// 429：被限流（Key 本身可能有效，需降速重试）。
    RateLimited,
    /// 404：端点不存在（平台配置可能有误）。
    NotFound,
    /// 5xx：服务端错误。
    ServerError,
    /// 其它 4xx：请求被拒（参数 / 配额 / 付费等问题）。
    ClientError,
    /// 网络层失败（连接超时、DNS、TLS 等）。
    NetworkError,
}

impl Verdict {
    /// 由 HTTP 状态码判定归类。
    pub fn from_status(code: u16) -> Verdict {
        match code {
            200..=299 => Verdict::Valid,
            401 | 403 => Verdict::Unauthorized,
            429 => Verdict::RateLimited,
            404 => Verdict::NotFound,
            500..=599 => Verdict::ServerError,
            _ => Verdict::ClientError,
        }
    }

    /// 展示用中文标签。
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Valid => "有效",
            Verdict::Unauthorized => "鉴权失败",
            Verdict::RateLimited => "限流",
            Verdict::NotFound => "端点不存在",
            Verdict::ServerError => "服务端错误",
            Verdict::ClientError => "请求被拒",
            Verdict::NetworkError => "网络错误",
        }
    }

    /// libadwaita 样式类，用于给标签上色。
    pub fn css_class(&self) -> &'static str {
        match self {
            Verdict::Valid => "success",
            Verdict::RateLimited | Verdict::ServerError | Verdict::NetworkError => "warning",
            _ => "error",
        }
    }

    /// 是否算作有效 Key（会被写入本地库）。
    pub fn is_valid(&self) -> bool {
        matches!(self, Verdict::Valid)
    }

    /// 是否建议自动重试。
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Verdict::NetworkError | Verdict::ServerError | Verdict::RateLimited
        )
    }
}

/// 一次探测的完整结果（纯数据）。
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub verdict: Verdict,
    /// HTTP 状态码，0 表示未拿到响应（网络层失败）。
    pub status: u16,
    /// 状态文本（如 `Too Many Requests`）。
    pub status_text: String,
    /// 往返耗时（毫秒）。
    pub latency_ms: u64,
    /// 响应体（已截断到 [`SNIPPET_MAX`]）。
    pub body: String,
    /// 提炼出的一句话说明（优先取 JSON 中的 `error.message`）。
    pub detail: String,
}

// ---------------------------------------------------------------------------
// 命中记录与断点
// ---------------------------------------------------------------------------

/// 一条被判定为有效的 Key 记录（写入本地库，永久保存）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidKeyRecord {
    pub platform: String,
    pub base_url: String,
    pub endpoint: String,
    pub model: String,
    pub key: String,
    pub status: u16,
    pub latency_ms: u64,
    /// 命中时刻（UNIX 秒）。
    pub found_at: u64,
    /// 响应体摘要。
    pub snippet: String,
}

/// 断点续跑的进度快照。
///
/// `cursor` 表示「下一条待测候选在字典中的下标」。字典由
/// `pattern + max_candidates + unbounded_repeat` 确定性生成，因此只要这些
/// 参数不变（用 [`fingerprint`] 校验），重开时重新生成即可从 cursor 继续。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub platform: String,
    /// 字典指纹：`base_url|endpoint|model|pattern|max|unbounded` 的哈希。
    pub fingerprint: String,
    pub total: usize,
    /// 下一条待测下标。
    pub cursor: usize,
    /// 本次已测数量。
    pub tested: usize,
    /// 本次命中数量。
    pub valid: usize,
    pub updated_at: u64,
}

/// 磁盘上的模块配置快照（平台列表 + 运行参数 + 上次选中平台）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SnifferStore {
    pub platforms: Vec<PlatformConfig>,
    pub scan: ScanConfig,
    pub last_platform: String,
}

/// 计算字典指纹：FNV-1a 64 位哈希，用于判断断点是否仍然可用。
pub fn fingerprint(parts: &[&str]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in part.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        // 段分隔，避免 "a"+"bc" 与 "ab"+"c" 碰撞
        hash ^= 0x1f;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_from_status() {
        assert_eq!(Verdict::from_status(200), Verdict::Valid);
        assert_eq!(Verdict::from_status(201), Verdict::Valid);
        assert_eq!(Verdict::from_status(401), Verdict::Unauthorized);
        assert_eq!(Verdict::from_status(403), Verdict::Unauthorized);
        assert_eq!(Verdict::from_status(429), Verdict::RateLimited);
        assert_eq!(Verdict::from_status(404), Verdict::NotFound);
        assert_eq!(Verdict::from_status(500), Verdict::ServerError);
        assert_eq!(Verdict::from_status(400), Verdict::ClientError);
    }

    #[test]
    fn platform_validation() {
        let mut p = PlatformConfig::new("x", "https://a.com/v1", "^sk-[0-9]{4}$", "m", "");
        assert!(p.validate().is_ok());
        p.base_url = "a.com/v1".into();
        assert!(p.validate().is_err());
        p.base_url = "https://a.com/v1".into();
        p.endpoint = "/models".into();
        assert!(p.validate().is_ok());
        p.endpoint = "models".into();
        assert!(p.validate().is_err());
    }

    #[test]
    fn fingerprint_is_stable_and_ordered() {
        assert_eq!(fingerprint(&["a", "bc"]), fingerprint(&["a", "bc"]));
        assert_ne!(fingerprint(&["a", "bc"]), fingerprint(&["ab", "c"]));
    }
}
