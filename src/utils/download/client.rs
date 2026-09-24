//! reqwest(rustls) 客户端构造。
//!
//! - TLS 使用 rustls（Cargo.toml 的 `rustls` feature，无 native-tls）；
//! - 连接超时 10s、读取闲置超时 = 配置的 timeout_secs（下载大文件不受
//!   总请求超时影响，卡死的连接会被 read_timeout 兜底断开）；
//! - 跟随重定向（上限 10 次，跨协议 http↔https 允许）。

use crate::model::download::DownloadConfig;
use std::time::Duration;

pub fn build_client(cfg: &DownloadConfig) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(cfg.user_agent.trim())
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(cfg.timeout_secs.max(1)))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| format!("初始化 HTTP 客户端失败：{e}"))
}

/// 探询/小请求统一超时（总超时 = 配置 timeout，防止探询挂死）。
pub fn probe_timeout(cfg: &DownloadConfig) -> Duration {
    Duration::from_secs(cfg.timeout_secs.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_timeout_uses_config_with_floor() {
        let mut cfg = DownloadConfig {
            timeout_secs: 45,
            ..Default::default()
        };
        assert_eq!(probe_timeout(&cfg), Duration::from_secs(45));
        cfg.timeout_secs = 0;
        assert_eq!(
            probe_timeout(&cfg),
            Duration::from_secs(1),
            "0 必须抬到 1 秒防挂死"
        );
    }

    #[test]
    fn build_client_ok() {
        // 构造成功即说明 rustls + UA + connect/read timeout + 重定向策略合法；
        // 故意不设总请求超时（大文件靠 read_timeout 兜底，见模块头注释）。
        let cfg = DownloadConfig::default();
        build_client(&cfg).expect("reqwest 客户端应能构造");
        // floor 到 1 秒也不得 panic
        let cfg2 = DownloadConfig {
            timeout_secs: 0,
            ..Default::default()
        };
        build_client(&cfg2).expect("timeout=0 时 read_timeout 应被抬到 1s");
    }
}
