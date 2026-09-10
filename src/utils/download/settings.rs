//! 下载配置持久化：`~/.config/linbox/download.json`。

use crate::model::download::DownloadConfig;
use std::path::PathBuf;

pub fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
                .unwrap_or_else(|_| PathBuf::from("."))
        });
    base.join("linbox/download.json")
}

pub fn load() -> DownloadConfig {
    let text = std::fs::read_to_string(config_path()).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save(cfg: &DownloadConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败：{e}"))?;
    }
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("写入配置失败：{e}"))
}
