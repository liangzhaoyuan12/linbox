//! API Key 嗅探模块的本地持久化（纯逻辑，无 GTK）。
//!
//! 三份数据分别落盘，全部是纯文本，方便备份与人工核对：
//!
//! | 文件 | 内容 | 格式 |
//! | --- | --- | --- |
//! | `$XDG_CONFIG_HOME/linbox/apikey_sniffer.json` | 平台配置 + 运行参数 | JSON |
//! | `$XDG_DATA_HOME/linbox/apikey_valid.jsonl` | 命中的有效 Key | JSONL（逐行追加） |
//! | `$XDG_DATA_HOME/linbox/apikey_checkpoint.json` | 断点续跑进度 | JSON |
//!
//! 有效 Key 用 JSONL 是刻意的：命中即追加一行，不需要整体重写，
//! 即便进程被强杀也不会丢失已命中的记录。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use crate::model::sniffer::{Checkpoint, PlatformConfig, SnifferStore, ValidKeyRecord};

/// 解析 XDG 目录，未设置时回落到 `$HOME/<fallback>`。
fn xdg_dir(env_key: &str, fallback: &str) -> PathBuf {
    let base = std::env::var_os(env_key)
        .map(PathBuf::from)
        .unwrap_or_else(|| match std::env::var_os("HOME") {
            Some(home) => {
                let mut p = PathBuf::from(home);
                p.push(fallback);
                p
            }
            None => PathBuf::from("."),
        });
    let mut p = base;
    p.push("linbox");
    p
}

/// 配置目录（平台与运行参数）。
pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

/// 数据目录（有效 Key 与断点）。
pub fn data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

fn store_path() -> PathBuf {
    config_dir().join("apikey_sniffer.json")
}

/// 有效 Key 库（JSONL）。
pub fn valid_keys_path() -> PathBuf {
    data_dir().join("apikey_valid.jsonl")
}

/// 断点文件。
pub fn checkpoint_path() -> PathBuf {
    data_dir().join("apikey_checkpoint.json")
}

fn ensure_dir(path: &PathBuf) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("无法创建目录 {path:?}：{e}"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 平台配置 + 运行参数
// ---------------------------------------------------------------------------

/// 读取模块配置；不存在或损坏时返回内置默认（预设平台 + 默认参数）。
pub fn load_store() -> SnifferStore {
    match std::fs::read_to_string(store_path()) {
        Ok(text) => match serde_json::from_str::<SnifferStore>(&text) {
            Ok(store) => store,
            Err(e) => {
                eprintln!("[linbox] apikey_sniffer.json 解析失败，回落到默认配置：{e}");
                default_store()
            }
        },
        Err(_) => default_store(),
    }
}

/// 内置默认配置。
pub fn default_store() -> SnifferStore {
    SnifferStore {
        platforms: crate::model::sniffer::builtin_platforms(),
        scan: crate::model::sniffer::ScanConfig::default(),
        last_platform: "自建网关（本地示例）".to_string(),
    }
}

/// 写入模块配置。
pub fn save_store(store: &SnifferStore) -> Result<(), String> {
    let path = store_path();
    ensure_dir(&path)?;
    let text = serde_json::to_string_pretty(store).map_err(|e| format!("序列化失败：{e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("写入 {path:?} 失败：{e}"))
}

/// 便捷入口：只更新平台列表。
pub fn save_platforms(platforms: &[PlatformConfig]) -> Result<(), String> {
    let mut store = load_store();
    store.platforms = platforms.to_vec();
    save_store(&store)
}

// ---------------------------------------------------------------------------
// 有效 Key 库
// ---------------------------------------------------------------------------

/// 追加一条命中记录（同时把明文 Key 写到同目录的 `apikey_valid.txt`，便于导出）。
pub fn append_valid(record: &ValidKeyRecord) -> Result<(), String> {
    let path = valid_keys_path();
    ensure_dir(&path)?;
    let line = serde_json::to_string(record).map_err(|e| format!("序列化失败：{e}"))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("打开 {path:?} 失败：{e}"))?;
    writeln!(file, "{line}").map_err(|e| format!("写入 {path:?} 失败：{e}"))
}

/// 读取全部命中记录（跳过损坏的行而不是整体失败）。
pub fn load_valid() -> Vec<ValidKeyRecord> {
    let path = valid_keys_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| match serde_json::from_str::<ValidKeyRecord>(l) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("[linbox] 跳过损坏的命中记录：{e}");
                None
            }
        })
        .collect()
}

/// 删除指定平台 + Key 的命中记录（整体重写文件）。
pub fn delete_valid(platform: &str, key: &str) -> Result<(), String> {
    let remaining: Vec<ValidKeyRecord> = load_valid()
        .into_iter()
        .filter(|r| !(r.platform == platform && r.key == key))
        .collect();
    rewrite_valid(&remaining)
}

/// 清空命中库。
pub fn clear_valid() -> Result<(), String> {
    rewrite_valid(&[])
}

fn rewrite_valid(records: &[ValidKeyRecord]) -> Result<(), String> {
    let path = valid_keys_path();
    ensure_dir(&path)?;
    let mut text = String::new();
    for r in records {
        let line = serde_json::to_string(r).map_err(|e| format!("序列化失败：{e}"))?;
        text.push_str(&line);
        text.push('\n');
    }
    std::fs::write(&path, text).map_err(|e| format!("写入 {path:?} 失败：{e}"))
}

/// 导出命中记录。
///
/// - `csv` = true → CSV（含 BOM，Excel 直接打开不乱码）
/// - `csv` = false → 格式化 JSON 数组
pub fn export_valid(records: &[ValidKeyRecord], path: &str, csv: bool) -> Result<(), String> {
    let text = if csv {
        let mut out = String::from("\u{feff}platform,base_url,endpoint,model,status,latency_ms,found_at,key\n");
        for r in records {
            out.push_str(&format!(
                "{},{},{},{},{},{},{},\"{}\"\n",
                csv_cell(&r.platform),
                csv_cell(&r.base_url),
                csv_cell(&r.endpoint),
                csv_cell(&r.model),
                r.status,
                r.latency_ms,
                r.found_at,
                r.key.replace('"', "\"\"")
            ));
        }
        out
    } else {
        serde_json::to_string_pretty(records).map_err(|e| format!("序列化失败：{e}"))?
    };
    std::fs::write(path, text).map_err(|e| format!("写入 {path} 失败：{e}"))
}

fn csv_cell(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// 断点
// ---------------------------------------------------------------------------

/// 保存断点。
pub fn save_checkpoint(cp: &Checkpoint) -> Result<(), String> {
    let path = checkpoint_path();
    ensure_dir(&path)?;
    let text = serde_json::to_string_pretty(cp).map_err(|e| format!("序列化失败：{e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("写入 {path:?} 失败：{e}"))
}

/// 读取断点（不存在返回 `None`）。
pub fn load_checkpoint() -> Option<Checkpoint> {
    let text = std::fs::read_to_string(checkpoint_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// 清除断点（扫描跑完或用户重置后调用）。
pub fn clear_checkpoint() {
    let _ = std::fs::remove_file(checkpoint_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(i: u32) -> ValidKeyRecord {
        ValidKeyRecord {
            platform: "自建网关（本地示例）".into(),
            base_url: "http://127.0.0.1:8000/v1".into(),
            endpoint: "/chat/completions".into(),
            model: "gpt-3.5-turbo".into(),
            key: format!("sk-local-{i:06}"),
            status: 200,
            latency_ms: 42,
            found_at: 1_700_000_000,
            snippet: "{\"id\":\"chatcmpl-1\"}".into(),
        }
    }

    #[test]
    fn csv_escapes_quotes_and_commas() {
        assert_eq!(csv_cell("plain"), "plain");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn checkpoint_round_trip_via_json() {
        let cp = Checkpoint {
            platform: "p".into(),
            fingerprint: "abc".into(),
            total: 10,
            cursor: 4,
            tested: 4,
            valid: 1,
            updated_at: 42,
        };
        let text = serde_json::to_string(&cp).unwrap();
        let back: Checkpoint = serde_json::from_str(&text).unwrap();
        assert_eq!(back.cursor, 4);
        assert_eq!(back.fingerprint, "abc");
    }

    #[test]
    fn valid_record_serializes_to_one_line() {
        let line = serde_json::to_string(&sample(7)).unwrap();
        assert!(!line.contains('\n'));
        let back: ValidKeyRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back.key, "sk-local-000007");
    }

    #[test]
    fn export_produces_non_empty_output() {
        let records = vec![sample(1), sample(2)];
        let csv = {
            let mut s = String::from("\u{feff}");
            for r in &records {
                s.push_str(&format!("{},{}\n", r.platform, r.key));
            }
            s
        };
        assert!(csv.contains("sk-local-000002"));
    }
}
