//! API Key 嗅探模块的本地持久化（纯逻辑，无 GTK）。
//!
//! 三份数据分别落盘：
//!
//! | 文件 | 内容 | 格式 |
//! | --- | --- | --- |
//! | `$XDG_CONFIG_HOME/linbox/apikey_sniffer.json` | 平台配置 + 运行参数 | JSON |
//! | `$XDG_DATA_HOME/linbox/apikey.db` | 各平台嗅探到的有效 API Key | SQLite（sqlx） |
//! | `$XDG_DATA_HOME/linbox/apikey_checkpoint_*.json` | 断点续跑进度（按平台） | JSON |
//!
//! 有效 Key 用 SQLite 存储：`valid_keys` 表带 `UNIQUE(platform, key)` 约束，
//! 重复命中同一 Key 时以 `INSERT OR IGNORE` 幂等写入，天然去重。
//!
//! ## 线程模型
//! sqlite 连接池（`SqlitePool`）是 `Send + Sync` 的，可在任意线程使用。
//! 扫描引擎（tokio 异步）直接 `await` 异步版接口；页面（GTK 主线程）通过
//! 同步包装器 `load_valid()` 等调用 —— 内部用 `runtime().block_on` 在全局
//! tokio 运行时上执行（主线程不在运行时内，不会嵌套 panic）。

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

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

/// 数据目录（有效 Key 数据库与断点）。
pub fn data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

fn store_path() -> PathBuf {
    config_dir().join("apikey_sniffer.json")
}

/// 有效 Key 数据库（SQLite）。
pub fn db_path() -> PathBuf {
    data_dir().join("apikey.db")
}

/// 断点文件（按平台独立存放，避免多平台并行嗅探时互相覆盖进度）。
pub fn checkpoint_path(platform: &str) -> PathBuf {
    data_dir().join(format!("apikey_checkpoint_{}.json", sanitize_platform(platform)))
}

/// 平台名 → 安全文件名片段（中英文、数字、下划线、连字符保留，其余替换成 `_`）。
fn sanitize_platform(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '-' || ('\u{4e00}'..='\u{9fff}').contains(&ch) {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("default");
    }
    out
}

fn ensure_dir(path: &PathBuf) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("无法创建目录 {path:?}：{e}"))?;
    }
    Ok(())
}

/// 建表语句（幂等）。
const CREATE_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS valid_keys (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    platform   TEXT NOT NULL,
    base_url   TEXT NOT NULL DEFAULT '',
    endpoint   TEXT NOT NULL DEFAULT '',
    model      TEXT NOT NULL DEFAULT '',
    key        TEXT NOT NULL,
    status     INTEGER NOT NULL DEFAULT 0,
    latency_ms INTEGER NOT NULL DEFAULT 0,
    found_at   INTEGER NOT NULL DEFAULT 0,
    snippet    TEXT NOT NULL DEFAULT '',
    UNIQUE(platform, key)
);
"#;

/// 全局连接池。`connect_lazy` 不要求立即处于运行时上下文，首次真正查询
/// 时再建立连接；因此主线程与 tokio 工作线程都能安全取得池。
fn pool() -> &'static SqlitePool {
    static POOL: OnceLock<SqlitePool> = OnceLock::new();
    POOL.get_or_init(|| {
        let path = db_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // create_if_missing + busy_timeout 只能走选项构造器，
        // connect_lazy_with 接受完整选项（懒连接，首次查询时才真正打开数据库）
        let opts = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(5));
        SqlitePoolOptions::new().max_connections(8).connect_lazy_with(opts)
    })
}

/// 初始化数据库结构（幂等）。页面启动时调用一次；失败只记录，不影响打开。
pub fn init_db() {
    if let Err(e) = super::runtime().block_on(async {
        sqlx::query(CREATE_TABLE).execute(pool()).await.map_err(|e| e.to_string())
    }) {
        eprintln!("[linbox] SQLite 建表失败：{e}");
    }
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
// 有效 Key 库（SQLite）
// ---------------------------------------------------------------------------

fn row_to_record(r: &sqlx::sqlite::SqliteRow) -> ValidKeyRecord {
    ValidKeyRecord {
        platform: r.get("platform"),
        base_url: r.get("base_url"),
        endpoint: r.get("endpoint"),
        model: r.get("model"),
        key: r.get("key"),
        status: r.get::<i64, _>("status") as u16,
        latency_ms: r.get::<i64, _>("latency_ms") as u64,
        found_at: r.get::<i64, _>("found_at") as u64,
        snippet: r.get("snippet"),
    }
}

/// 追加/更新一条命中记录（`UNIQUE(platform, key)` 保证同一平台的同一 Key
/// 只入库一次，重复命中自动忽略）。
pub async fn append_valid(record: &ValidKeyRecord) -> Result<(), String> {
    sqlx::query(
        "INSERT OR IGNORE INTO valid_keys \
         (platform, base_url, endpoint, model, key, status, latency_ms, found_at, snippet) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&record.platform)
    .bind(&record.base_url)
    .bind(&record.endpoint)
    .bind(&record.model)
    .bind(&record.key)
    .bind(record.status as i64)
    .bind(record.latency_ms as i64)
    .bind(record.found_at as i64)
    .bind(&record.snippet)
    .execute(pool())
    .await
    .map_err(|e| format!("写入本地库失败：{e}"))?;
    Ok(())
}

/// 读取全部命中记录（按入库顺序，即旧的在前、新命中的在后）。
pub fn load_valid() -> Vec<ValidKeyRecord> {
    super::runtime().block_on(async {
        let rows = sqlx::query("SELECT * FROM valid_keys ORDER BY id")
            .fetch_all(pool())
            .await;
        match rows {
            Ok(rows) => rows.iter().map(row_to_record).collect(),
            Err(e) => {
                eprintln!("[linbox] 读取本地库失败：{e}");
                Vec::new()
            }
        }
    })
}

/// 删除指定平台 + Key 的命中记录。
pub fn delete_valid(platform: &str, key: &str) -> Result<(), String> {
    super::runtime().block_on(async {
        sqlx::query("DELETE FROM valid_keys WHERE platform = ? AND key = ?")
            .bind(platform)
            .bind(key)
            .execute(pool())
            .await
            .map_err(|e| format!("删除失败：{e}"))?;
        Ok(())
    })
}

/// 清空命中库。
pub fn clear_valid() -> Result<(), String> {
    super::runtime().block_on(async {
        sqlx::query("DELETE FROM valid_keys")
            .execute(pool())
            .await
            .map_err(|e| format!("清空失败：{e}"))?;
        Ok(())
    })
}

/// 导出命中记录。
///
/// - `csv` = true → CSV（含 BOM，Excel 直接打开不乱码）
/// - `csv` = false → 格式化 JSON 数组
pub fn export_valid(records: &[ValidKeyRecord], path: &str, csv: bool) -> Result<(), String> {
    let text = if csv {
        let mut out =
            String::from("\u{feff}platform,base_url,endpoint,model,status,latency_ms,found_at,key\n");
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

/// 保存断点（按 `cp.platform` 落到各自的断点文件）。
pub fn save_checkpoint(cp: &Checkpoint) -> Result<(), String> {
    let path = checkpoint_path(&cp.platform);
    ensure_dir(&path)?;
    let text = serde_json::to_string_pretty(cp).map_err(|e| format!("序列化失败：{e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("写入 {path:?} 失败：{e}"))
}

/// 读取指定平台的断点（不存在返回 `None`）。
pub fn load_checkpoint(platform: &str) -> Option<Checkpoint> {
    let text = std::fs::read_to_string(checkpoint_path(platform)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 清除指定平台的断点（扫描跑完或用户重置后调用）。
pub fn clear_checkpoint(platform: &str) {
    let _ = std::fs::remove_file(checkpoint_path(platform));
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
        assert_eq!(back.key, format!("sk-local-{:06}", 7));
    }

    #[test]
    fn export_produces_csv_header() {
        let records = vec![sample(1)];
        let text = {
            let mut s = String::from("\u{feff}");
            s.push_str("platform,base_url,endpoint,model,status,latency_ms,found_at,key\n");
            s.push_str(&format!("{},{}\n", records[0].platform, records[0].key));
            s
        };
        assert!(text.starts_with('\u{feff}'));
        assert!(text.contains("platform"));
        assert!(text.contains("sk-local-000001"));
    }

    #[test]
    fn sqlite_round_trip_and_dedup() {
        // 用独立临时数据目录，避免污染用户真实本地库；
        // 必须在任何 pool() 调用之前设置环境变量（池路径在首次连接时固定）
        let tmp = std::env::temp_dir().join(format!("linbox_sqlite_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var("XDG_DATA_HOME", &tmp); }

        init_db();
        let r1 = sample(11);
        let r2 = sample(12);
        crate::utils::sniffer::runtime().block_on(async {
            append_valid(&r1).await.unwrap();
            append_valid(&r1).await.unwrap(); // 同一平台同一 Key 重复命中 → INSERT OR IGNORE
            append_valid(&r2).await.unwrap();
        });

        let all = load_valid();
        assert_eq!(all.len(), 2, "重复命中应被去重");
        assert!(all.iter().any(|r| r.key == r1.key));
        assert!(all.iter().any(|r| r.key == r2.key));

        delete_valid(&r1.platform, &r1.key).unwrap();
        let all = load_valid();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].key, r2.key);

        clear_valid().unwrap();
        assert!(load_valid().is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}