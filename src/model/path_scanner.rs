//! 路径扫描模块的数据模型（纯数据，无 UI 依赖）。
//!
//! 对域名或 IP 的路径进行枚举探测，发现隐藏的目录、文件、接口。
//!
//! ## 用途边界
//! 请仅对你自己拥有或已获得明确授权的目标使用本模块。

use serde::{Deserialize, Serialize};

/// 内置公共路径字典（top 100 常见 Web 路径）。
pub const BUILTIN_PATHS: &[&str] = &[
    "/",
    "/admin",
    "/admin/",
    "/login",
    "/wp-admin",
    "/wp-login.php",
    "/api",
    "/api/",
    "/api/v1",
    "/api/v2",
    "/api/docs",
    "/api/swagger",
    "/swagger-ui",
    "/swagger-ui.html",
    "/swagger/index.html",
    "/v1",
    "/v2",
    "/v3",
    "/graphql",
    "/graphiql",
    "/console",
    "/dashboard",
    "/panel",
    "/portal",
    "/manage",
    "/management",
    "/config",
    "/configuration",
    "/settings",
    "/backup",
    "/backups",
    "/db",
    "/database",
    "/phpmyadmin",
    "/adminer",
    "/mysql",
    "/postgres",
    "/mongo",
    "/redis",
    "/.env",
    "/.env.example",
    "/.git",
    "/.git/config",
    "/.git/HEAD",
    "/.gitignore",
    "/.htaccess",
    "/robots.txt",
    "/sitemap.xml",
    "/crossdomain.xml",
    "/favicon.ico",
    "/.well-known/",
    "/.well-known/security.txt",
    "/server-status",
    "/server-info",
    "/.DS_Store",
    "/web.config",
    "/info.php",
    "/phpinfo.php",
    "/test",
    "/test/",
    "/debug",
    "/debug/",
    "/trace",
    "/actuator",
    "/actuator/health",
    "/actuator/env",
    "/actuator/beans",
    "/metrics",
    "/health",
    "/healthz",
    "/ready",
    "/status",
    "/version",
    "/.svn",
    "/.svn/entries",
    "/cgi-bin/",
    "/cgi-bin/test",
    "/elmah.axd",
    "/trace.axd",
    "/WEB-INF/",
    "/WEB-INF/web.xml",
    "/assets",
    "/static",
    "/public",
    "/uploads",
    "/upload",
    "/files",
    "/file",
    "/download",
    "/downloads",
    "/media",
    "/images",
    "/img",
    "/css",
    "/js",
    "/scripts",
    "/includes",
    "/lib",
    "/vendor",
    "/node_modules",
    "/composer.json",
    "/package.json",
    "/Gemfile",
    "/Dockerfile",
    "/docker-compose.yml",
    "/README.md",
    "/README.txt",
    "/CHANGELOG",
    "/LICENSE",
    "/todo.txt",
    "/xmlrpc.php",
    "/wp-content",
    "/wp-includes",
    "/wp-json/wp/v2/users",
    "/wp-cron.php",
    "/feed",
    "/rss",
    "/atom.xml",
];

/// 扫描配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathScanConfig {
    /// 目标 URL（如 `http://example.com` 或 `http://192.168.1.1:8080`）。
    pub target_url: String,
    /// 使用内置字典。
    pub use_builtin: bool,
    /// 自定义字典路径（每行一条路径，如 `/admin`）。
    pub custom_wordlist_path: String,
    /// 附加请求头 (键, 值)。
    pub headers: Vec<(String, String)>,
    /// HTTP 方法。
    pub method: String,
    /// 并发线程数。
    pub concurrency: usize,
    /// 限速：每秒请求数，0 表示不限。
    pub rate_per_sec: f64,
    /// 单次请求超时（秒）。
    pub timeout_secs: u64,
    /// 失败重试次数。
    pub retries: usize,
    /// 排除的状态码（逗号分隔，如 "404,403,301"）。
    pub exclude_status: String,
    /// 排除的响应体大小范围（如 "0-500" 表示排除 0~500 字节）。
    pub exclude_size: String,
    /// 日志区最多保留的行数。
    pub log_limit: usize,
}

impl Default for PathScanConfig {
    fn default() -> Self {
        PathScanConfig {
            target_url: String::new(),
            use_builtin: true,
            custom_wordlist_path: String::new(),
            headers: Vec::new(),
            method: "GET".to_string(),
            concurrency: 10,
            rate_per_sec: 50.0,
            timeout_secs: 10,
            retries: 1,
            exclude_status: "404".to_string(),
            exclude_size: String::new(),
            log_limit: 400,
        }
    }
}

impl PathScanConfig {
    /// 校验配置是否合法。
    pub fn validate(&self) -> Result<(), String> {
        let url = self.target_url.trim();
        if url.is_empty() {
            return Err("目标 URL 不能为空".into());
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("目标 URL 必须以 http:// 或 https:// 开头".into());
        }
        if !self.use_builtin && self.custom_wordlist_path.trim().is_empty() {
            return Err("请启用内置字典或指定自定义字典路径".into());
        }
        if self.concurrency < 1 || self.concurrency > 512 {
            return Err("并发数需在 1 ~ 512 之间".into());
        }
        Ok(())
    }

    /// 解析排除状态码列表。
    pub fn parse_exclude_status(&self) -> Vec<u16> {
        self.exclude_status
            .split(',')
            .filter_map(|s| s.trim().parse::<u16>().ok())
            .collect()
    }
}

/// 排除响应体大小范围。
#[derive(Debug, Clone, Default)]
pub struct ExcludeSizeRange {
    pub min: Option<usize>,
    pub max: Option<usize>,
}

impl ExcludeSizeRange {
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        if s.is_empty() {
            return Self::default();
        }
        if let Some((min_s, max_s)) = s.split_once('-') {
            let min = min_s.trim().parse().ok();
            let max = max_s.trim().parse().ok();
            return ExcludeSizeRange { min, max };
        }
        // 单个值：排除等于该大小的响应
        if let Ok(v) = s.trim().parse::<usize>() {
            return ExcludeSizeRange {
                min: Some(v),
                max: Some(v),
            };
        }
        Self::default()
    }

    pub fn is_excluded(&self, size: usize) -> bool {
        if let Some(min) = self.min {
            if size < min {
                return false;
            }
        }
        if let Some(max) = self.max {
            if size > max {
                return false;
            }
        }
        self.min.is_some() || self.max.is_some()
    }
}

/// 单次路径探测结果。
#[derive(Debug, Clone)]
pub struct PathProbeResult {
    /// 请求的路径。
    pub path: String,
    /// HTTP 状态码。
    pub status: u16,
    /// 状态文本。
    pub status_text: String,
    /// 响应体大小（字节）。
    pub size: usize,
    /// 重定向 URL（3xx 时）。
    pub redirect_url: String,
    /// 往返耗时（毫秒）。
    pub latency_ms: u64,
}

/// 扫描过程中回传给 UI 的事件。
#[derive(Debug, Clone)]
pub enum PathScanEvent {
    /// 引擎已启动。
    Started {
        /// 字典总条数。
        total: usize,
    },
    /// 某条路径探测完毕。
    Result {
        /// 探测结果。
        outcome: PathProbeResult,
        /// 该路径在字典中的下标。
        index: usize,
    },
    /// 提示信息。
    Log(String),
    /// 所有工作线程已退出。
    Finished {
        /// 已探测数量。
        tested: usize,
        /// 命中数量（通过过滤器的）。
        found: usize,
    },
}

/// 断点续跑的进度快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathScanCheckpoint {
    /// 目标 URL。
    pub target_url: String,
    /// 字典指纹。
    pub fingerprint: String,
    /// 字典总条数。
    pub total: usize,
    /// 下一条待探测下标。
    pub cursor: usize,
    /// 已探测数量。
    pub tested: usize,
    /// 命中数量。
    pub found: usize,
    /// 更新时间戳（UNIX 秒）。
    pub updated_at: u64,
}

/// 磁盘上的模块配置快照。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PathScanStore {
    /// 扫描配置。
    pub config: PathScanConfig,
    /// 上次使用的配置。
    pub last_target: String,
}

/// 计算字典指纹（FNV-1a 64 位哈希）。
pub fn fingerprint(parts: &[&str]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in part.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        hash ^= 0x1f;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validation() {
        let mut c = PathScanConfig::default();
        c.target_url = "http://example.com".into();
        assert!(c.validate().is_ok());

        c.target_url = "".into();
        assert!(c.validate().is_err());
        c.target_url = "ftp://example.com".into();
        assert!(c.validate().is_err());

        c.target_url = "http://example.com".into();
        c.use_builtin = false;
        c.custom_wordlist_path = "".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn exclude_status_parsing() {
        let mut c = PathScanConfig::default();
        c.exclude_status = "404,403, 301".into();
        let excluded = c.parse_exclude_status();
        assert_eq!(excluded, vec![404, 403, 301]);
    }

    #[test]
    fn exclude_size_range() {
        let r = ExcludeSizeRange::parse("100-500");
        assert!(!r.is_excluded(50));
        assert!(r.is_excluded(200));
        assert!(!r.is_excluded(600));

        let r = ExcludeSizeRange::parse("0-500");
        assert!(r.is_excluded(0));
        assert!(r.is_excluded(250));
        assert!(!r.is_excluded(600));

        let r = ExcludeSizeRange::parse("");
        assert!(!r.is_excluded(0));
        assert!(!r.is_excluded(999999));
    }

    #[test]
    fn fingerprint_is_stable() {
        assert_eq!(fingerprint(&["a", "bc"]), fingerprint(&["a", "bc"]));
        assert_ne!(fingerprint(&["a", "bc"]), fingerprint(&["ab", "c"]));
    }
}
