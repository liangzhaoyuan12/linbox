//! 备忘录云端同步客户端（多服务器去中心化）。
//!
//! 支持同时配置多个云端服务器，同步时：
//! - 向所有服务器上传本地变更
//! - 从所有服务器下载远端变更
//! - 冲突解决：按 `modified_at` 时间戳，全局最晚者胜出
//!
//! 配置存储在 `~/.config/linbox/notepad/cloud.json`（JSON 数组）。
//! API 规范见项目根目录 `docs/notepad-cloud-api.md`。

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};

use super::storage;
use super::git_store::GitConfig;

// ── 配置 ──────────────────────────────────────────────────────────────────

/// 同步后端类型。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type")]
pub enum BackendConfig {
    #[serde(rename = "http")]
    Http(CloudConfig),
    #[serde(rename = "git")]
    Git(GitConfig),
}

impl BackendConfig {
    /// 后端唯一标识键（用于删除和匹配，不依赖 display_name）。
    /// 对 HTTP 后端返回 URL，对 Git 后端返回 URL。
    pub fn unique_key(&self) -> String {
        match self {
            BackendConfig::Http(c) => c.url.clone(),
            BackendConfig::Git(c) => c.url.clone(),
        }
    }

    /// 后端显示名。
    pub fn display_name(&self) -> String {
        match self {
            BackendConfig::Http(c) => {
                let host = c
                    .url
                    .trim_start_matches("http://")
                    .trim_start_matches("https://")
                    .split('/')
                    .next()
                    .unwrap_or(&c.url);
                format!("HTTP: {host}")
            }
            BackendConfig::Git(c) => {
                let repo = c
                    .url
                    .rsplit('/')
                    .next()
                    .unwrap_or(&c.url)
                    .strip_suffix(".git")
                    .unwrap_or_else(|| c.url.rsplit('/').next().unwrap_or(&c.url));
                format!("Git: {repo}")
            }
        }
    }
}

/// 单个云端服务器连接配置（HTTP 后端用）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CloudConfig {
    /// 服务器地址，如 `http://192.168.1.100:8080`
    pub url: String,
    /// 用户名
    pub username: String,
    /// 密码
    pub password: String,
}

/// 同步状态（本地持久化）。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SyncState {
    /// 本端设备 ID（区分不同设备的上传）。
    pub device_id: String,
    /// 每台服务器各自的上次同步时间戳。
    /// key = 服务器 URL，value = last_sync（unix 秒）。
    #[serde(default)]
    pub server_sync: std::collections::HashMap<String, u64>,
}

/// 同步结果摘要。
#[derive(Clone, Debug, Default)]
pub struct SyncSummary {
    pub uploaded: usize,
    pub downloaded: usize,
    pub deleted_local: usize,
    pub deleted_remote: usize,
    pub errors: Vec<String>,
    /// 每台服务器的同步结果：(服务器名, 结果)。
    pub per_server: Vec<(String, Result<(), String>)>,
}

// ── 持久化路径 ────────────────────────────────────────────────────────────

fn config_dir() -> PathBuf {
    let base = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("linbox")
        .join("notepad");
    let _ = fs::create_dir_all(&base);
    base
}

fn config_path() -> PathBuf {
    config_dir().join("cloud.json")
}

fn state_path() -> PathBuf {
    config_dir().join("sync-state.json")
}

// ── 配置管理（多后端：HTTP + Git） ────────────────────────────────────────

/// 加载所有后端配置。
pub fn load_configs() -> Vec<BackendConfig> {
    let text = match fs::read_to_string(config_path()) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    // 优先：新版带 type 标签的格式
    if let Ok(list) = serde_json::from_str::<Vec<BackendConfig>>(&text) {
        return list;
    }
    // 兼容旧版单 HTTP 对象格式
    if let Ok(single) = serde_json::from_str::<CloudConfig>(&text) {
        return vec![BackendConfig::Http(single)];
    }
    // 兼容旧版 HTTP 数组格式
    if let Ok(http_list) = serde_json::from_str::<Vec<CloudConfig>>(&text) {
        return http_list.into_iter().map(BackendConfig::Http).collect();
    }
    serde_json::from_str(&text).unwrap_or_default()
}

/// 保存所有后端配置。
pub fn save_configs(configs: &[BackendConfig]) -> Result<(), String> {
    let json = serde_json::to_string_pretty(configs).map_err(|e| e.to_string())?;
    fs::write(config_path(), json).map_err(|e| format!("写入配置失败：{e}"))
}

/// 添加一个 HTTP 后端。
pub fn add_http_config(config: CloudConfig) -> Result<(), String> {
    let mut configs = load_configs();
    if configs
        .iter()
        .any(|c| matches!(c, BackendConfig::Http(h) if h.url == config.url))
    {
        return Err("该服务器已存在".to_string());
    }
    configs.push(BackendConfig::Http(config));
    save_configs(&configs)
}

/// 添加一个 Git 后端。
pub fn add_git_config(config: GitConfig) -> Result<(), String> {
    let mut configs = load_configs();
    if configs
        .iter()
        .any(|c| matches!(c, BackendConfig::Git(g) if g.url == config.url))
    {
        return Err("该 Git 仓库已存在".to_string());
    }
    configs.push(BackendConfig::Git(config));
    save_configs(&configs)
}

/// 删除一个后端（按唯一键匹配，即 URL）。
///
/// 同时清理对应的本地 Git 仓库目录。
pub fn remove_backend(key: &str) -> Result<(), String> {
    let mut configs = load_configs();
    // 删除前记录要清理的 Git URL
    let git_urls: Vec<String> = configs
        .iter()
        .filter(|c| c.unique_key() == key)
        .filter_map(|c| match c {
            BackendConfig::Git(g) => Some(g.url.clone()),
            _ => None,
        })
        .collect();
    configs.retain(|c| c.unique_key() != key);
    save_configs(&configs)?;
    // 清理本地 Git 仓库目录
    for url in &git_urls {
        let dir = super::git_store::repo_work_dir(url);
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok(())
}

/// 是否已配置至少一个后端。
pub fn is_configured() -> bool {
    !load_configs().is_empty()
}

/// 测试单台 HTTP 服务器连接。
pub fn test_connection(config: &CloudConfig) -> Result<(), String> {
    ApiClient::new(config).ping()
}

/// 测试 Git 仓库连接。
pub fn test_git_connection(config: &GitConfig) -> Result<(), String> {
    super::git_store::verify_access(config)
}

/// 删除本地同步状态（断开所有连接时用）。
pub fn clear_sync_state() {
    let _ = fs::remove_file(state_path());
    clear_deleted_ids();
}

// ── 同步状态 ──────────────────────────────────────────────────────────────

fn load_state() -> SyncState {
    fs::read_to_string(state_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_state(state: &SyncState) {
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = fs::write(state_path(), json);
    }
}

fn generate_device_id() -> String {
    use rand::Rng;
    let mut s = String::with_capacity(16);
    for b in rand::thread_rng().r#gen::<[u8; 8]>() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

// ── API 数据结构 ──────────────────────────────────────────────────────────

/// 与云端交互的条目格式。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CloudEntry {
    pub id: String,
    pub title: String,
    pub content: String,
    pub created_at: String,
    pub modified_at: String,
    /// 正文内嵌图片（base64 编码的 PNG）。
    #[serde(default)]
    pub inline_images: Vec<String>,
    /// 附件（base64 编码），每项是 `{ "name": "xx.docx", "data": "base64..." }`。
    #[serde(default)]
    pub attachments: Vec<CloudAttachment>,
    /// 逻辑删除标记（非空 = 已删除的 ISO 时间戳）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CloudAttachment {
    pub name: String,
    pub data: String,
}

/// 同步请求：本端变更 + 本端已删除的 ID 列表。
#[derive(Serialize, Deserialize, Debug)]
pub struct SyncRequest {
    pub entries: Vec<CloudEntry>,
    #[serde(default)]
    pub deleted_ids: Vec<String>,
}

/// 同步响应：远端变更 + 远端已删除的 ID 列表。
#[derive(Serialize, Deserialize, Debug)]
pub struct SyncResponse {
    pub entries: Vec<CloudEntry>,
    #[serde(default)]
    pub deleted_ids: Vec<String>,
    pub server_time: u64,
}

// ── 本地 ↔ CloudEntry 转换 ────────────────────────────────────────────────

/// 把本地条目转为云端格式。
fn local_to_cloud(meta: &storage::MemoMeta) -> CloudEntry {
    let content = storage::read_content(&meta.id);

    let inline_images: Vec<String> = (0..storage::list_inline(&meta.id).len())
        .filter_map(|idx| {
            let data = storage::read_inline(&meta.id, idx)?;
            Some(B64.encode(&data))
        })
        .collect();

    let attachments: Vec<CloudAttachment> = storage::list_files(&meta.id)
        .iter()
        .map(|name| {
            let data = storage::read_file(&meta.id, name).unwrap_or_default();
            CloudAttachment {
                name: name.clone(),
                data: B64.encode(&data),
            }
        })
        .collect();

    CloudEntry {
        id: meta.id.clone(),
        title: meta.title.clone(),
        content,
        created_at: meta.created_at.clone(),
        modified_at: meta.modified_at.clone(),
        inline_images,
        attachments,
        deleted_at: None,
    }
}

/// 把云端条目写入本地存储。
fn cloud_to_local(entry: &CloudEntry) -> Result<String, String> {
    let id = &entry.id;

    let meta = storage::MemoMeta {
        id: id.clone(),
        title: entry.title.clone(),
        created_at: entry.created_at.clone(),
        modified_at: entry.modified_at.clone(),
    };
    storage::save(id, &meta, &entry.content);

    let inline_data: Vec<Vec<u8>> = entry
        .inline_images
        .iter()
        .filter_map(|b64| B64.decode(b64).ok())
        .collect();
    storage::save_inline(id, &inline_data);

    for old_name in storage::list_files(id) {
        storage::delete_file(id, &old_name);
    }
    for att in &entry.attachments {
        if let Ok(data) = B64.decode(&att.data) {
            storage::write_file(id, &att.name, &data);
        }
    }

    Ok(id.clone())
}

fn local_entries() -> Vec<storage::MemoMeta> {
    storage::load_all()
}

fn local_meta(id: &str) -> Option<storage::MemoMeta> {
    storage::load_meta(id)
}

fn parse_ts(ts: &str) -> u64 {
    ts.parse::<u64>().unwrap_or(0)
}

// ── HTTP 客户端 ───────────────────────────────────────────────────────────

struct ApiClient {
    config: CloudConfig,
    agent: ureq::Agent,
}

impl ApiClient {
    fn new(config: &CloudConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(30))
            .build();
        Self {
            config: config.clone(),
            agent,
        }
    }

    fn base_url(&self) -> &str {
        self.config.url.trim_end_matches('/')
    }

    fn server_name(&self) -> &str {
        // 用 URL 的 host 部分作为显示名
        self.config
            .url
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .split('/')
            .next()
            .unwrap_or(&self.config.url)
    }

    fn auth_request(&self, method: &str, path: &str) -> ureq::Request {
        let url = format!("{}{}", self.base_url(), path);
        let req = self.agent.request(method, &url);
        let cred = B64.encode(format!("{}:{}", self.config.username, self.config.password));
        req.set("Authorization", &format!("Basic {cred}"))
    }

    fn ping(&self) -> Result<(), String> {
        self.auth_request("GET", "/api/ping")
            .call()
            .map_err(|e| format!("连接失败：{e}"))?;
        Ok(())
    }

    fn pull_changes(&self, since: u64) -> Result<SyncResponse, String> {
        let resp: SyncResponse = self
            .auth_request("GET", &format!("/api/sync?since={}", since))
            .call()
            .map_err(|e| format!("拉取失败：{e}"))?
            .into_json()
            .map_err(|e| format!("解析响应失败：{e}"))?;
        Ok(resp)
    }

    fn push_changes(&self, entries: &[CloudEntry], deleted: &[String]) -> Result<(), String> {
        let body = SyncRequest {
            entries: entries.to_vec(),
            deleted_ids: deleted.to_vec(),
        };
        self.auth_request("POST", "/api/sync")
            .set("Content-Type", "application/json")
            .send_json(&body)
            .map_err(|e| format!("推送失败：{e}"))?;
        Ok(())
    }
}

// ── 多服务器同步逻辑 ─────────────────────────────────────────────────────

/// 执行一次完整的多后端双向同步（HTTP + Git）。
pub fn do_sync(configs: &[BackendConfig]) -> Result<SyncSummary, String> {
    if configs.is_empty() {
        return Err("没有配置任何同步后端".to_string());
    }

    let mut state = load_state();
    if state.device_id.is_empty() {
        state.device_id = generate_device_id();
    }

    let mut summary = SyncSummary::default();
    let mut all_ok = true;

    // 1. 收集本地变更
    let mut local_metas = local_entries();
    let deleted_ids = load_deleted_ids();

    // 2. 逐个后端同步
    for backend in configs {
        let ok = match backend {
            BackendConfig::Http(config) => {
                sync_http_backend(config, &local_metas, &deleted_ids, &mut state, &mut summary);
                !summary.per_server.last().map_or(false, |(_, r)| r.is_err())
            }
            BackendConfig::Git(config) => {
                sync_git_backend(config, &local_metas, &deleted_ids, &mut summary);
                !summary.per_server.last().map_or(false, |(_, r)| r.is_err())
            }
        };
        if !ok {
            all_ok = false;
        }
        // 3. 每个成功后端之后重新读取本地条目，让后续后端可见前一个后端拉取的条目
        if ok {
            local_metas = local_entries();
        }
    }

    // 4. 保存状态
    save_state(&state);

    // 5. 仅当所有后端同步成功时，才清理已同步的删除记录
    if all_ok && !deleted_ids.is_empty() {
        clear_deleted_ids();
    }

    Ok(summary)
}

/// 只从所有后端拉取（刷新），不推送本地。
pub fn pull_only(configs: &[BackendConfig]) -> Result<SyncSummary, String> {
    if configs.is_empty() {
        return Err("没有配置任何同步后端".to_string());
    }

    let mut state = load_state();
    if state.device_id.is_empty() {
        state.device_id = generate_device_id();
    }

    let mut summary = SyncSummary::default();

    for backend in configs {
        match backend {
            BackendConfig::Http(config) => {
                pull_http_only(config, &mut state, &mut summary);
            }
            BackendConfig::Git(config) => {
                let name = format!(
                    "Git: {}",
                    config
                        .url
                        .rsplit('/')
                        .next()
                        .unwrap_or(&config.url)
                );
                match super::git_store::pull_only(config) {
                    Ok(downloaded) => {
                        summary.downloaded += downloaded;
                        summary.per_server.push((name, Ok(())));
                    }
                    Err(e) => {
                        summary.errors.push(format!("[{name}] 刷新失败：{e}"));
                        summary.per_server.push((name, Err(e)));
                    }
                }
            }
        }
    }

    save_state(&state);
    Ok(summary)
}

/// 只推送到所有后端，不拉取。
pub fn push_only(configs: &[BackendConfig]) -> Result<SyncSummary, String> {
    if configs.is_empty() {
        return Err("没有配置任何同步后端".to_string());
    }

    let mut state = load_state();
    if state.device_id.is_empty() {
        state.device_id = generate_device_id();
    }

    let mut summary = SyncSummary::default();
    let deleted_ids = load_deleted_ids();
    let mut all_ok = true;
    let mut local_metas = local_entries();

    for backend in configs {
        let ok = match backend {
            BackendConfig::Http(config) => {
                push_http_only(config, &local_metas, &deleted_ids, &mut state, &mut summary);
                !summary.per_server.last().map_or(false, |(_, r)| r.is_err())
            }
            BackendConfig::Git(config) => {
                let name = format!(
                    "Git: {}",
                    config.url.rsplit('/').next().unwrap_or(&config.url)
                );
                match super::git_store::push_only(config, &local_metas, &deleted_ids) {
                    Ok(uploaded) => {
                        summary.uploaded += uploaded;
                        summary.per_server.push((name, Ok(())));
                        true
                    }
                    Err(e) => {
                        summary.errors.push(format!("[{name}] 推送失败：{e}"));
                        summary.per_server.push((name, Err(e)));
                        false
                    }
                }
            }
        };
        if !ok {
            all_ok = false;
        }
        // 每个成功后端之后重新读取本地条目
        if ok {
            local_metas = local_entries();
        }
    }

    save_state(&state);
    // 仅当所有后端推送成功时，才清理已同步的删除记录
    if all_ok && !deleted_ids.is_empty() {
        clear_deleted_ids();
    }
    Ok(summary)
}

/// HTTP 后端只拉取。
fn pull_http_only(
    config: &CloudConfig,
    state: &mut SyncState,
    summary: &mut SyncSummary,
) {
    let client = ApiClient::new(config);
    let server_name = client.server_name().to_string();
    let last_sync = state.server_sync.get(&config.url).copied().unwrap_or(0);

    match client.pull_changes(last_sync) {
        Ok(remote) => {
            let mut max_ts = last_sync;
            for entry in &remote.entries {
                let ts = parse_ts(&entry.modified_at);
                if ts > max_ts {
                    max_ts = ts;
                }
                // #18: 跳过已标记删除的远端条目（tombstone）
                if entry.deleted_at.is_some() {
                    storage::delete(&entry.id);
                    continue;
                }
                match local_meta(&entry.id) {
                    Some(local_m) => {
                        let local_ts = parse_ts(&local_m.modified_at);
                        if ts > local_ts {
                            if let Err(e) = cloud_to_local(entry) {
                                summary.errors.push(format!("写入条目 {} 失败：{e}", entry.id));
                            } else {
                                summary.downloaded += 1;
                            }
                        }
                    }
                    None => {
                        if let Err(e) = cloud_to_local(entry) {
                            summary.errors.push(format!("写入条目 {} 失败：{e}", entry.id));
                        } else {
                            summary.downloaded += 1;
                        }
                    }
                }
            }
            let new_ts = max_ts.max(remote.server_time);
            state.server_sync.insert(config.url.clone(), new_ts);
            summary.per_server.push((server_name, Ok(())));
        }
        Err(e) => {
            summary.errors.push(format!("[{server_name}] 拉取失败：{e}"));
            summary.per_server.push((server_name, Err(e)));
        }
    }
}

/// HTTP 后端只推送。
fn push_http_only(
    config: &CloudConfig,
    local_metas: &[storage::MemoMeta],
    deleted_ids: &[String],
    _state: &mut SyncState,
    summary: &mut SyncSummary,
) {
    let client = ApiClient::new(config);
    let server_name = client.server_name().to_string();

    if local_metas.is_empty() && deleted_ids.is_empty() {
        summary.per_server.push((server_name, Ok(())));
        return;
    }

    let changed: Vec<CloudEntry> = local_metas.iter().map(|m| local_to_cloud(m)).collect();
    match client.push_changes(&changed, deleted_ids) {
        Ok(()) => {
            summary.uploaded += changed.len();
            summary.deleted_remote += deleted_ids.len();
            summary.per_server.push((server_name, Ok(())));
        }
        Err(e) => {
            summary.errors.push(format!("[{server_name}] 推送失败：{e}"));
            summary.per_server.push((server_name, Err(e)));
        }
    }
}

/// 同步单个 HTTP 后端。
fn sync_http_backend(
    config: &CloudConfig,
    local_metas: &[storage::MemoMeta],
    deleted_ids: &[String],
    state: &mut SyncState,
    summary: &mut SyncSummary,
) {
    let client = ApiClient::new(config);
    let server_name = client.server_name().to_string();
    let last_sync = state.server_sync.get(&config.url).copied().unwrap_or(0);

    // 推送本地变更（推送所有本地条目，不依赖时间戳过滤）
    if !local_metas.is_empty() || !deleted_ids.is_empty() {
        let changed: Vec<CloudEntry> = local_metas.iter().map(|m| local_to_cloud(m)).collect();
        match client.push_changes(&changed, deleted_ids) {
            Ok(()) => {
                summary.uploaded += changed.len();
                summary.deleted_remote += deleted_ids.len();
            }
            Err(e) => {
                summary.errors.push(format!("[{server_name}] 推送失败：{e}"));
                summary.per_server.push((server_name, Err(e)));
                return;
            }
        }
    }

    // 拉取远端变更
    match client.pull_changes(last_sync) {
        Ok(remote) => {
            let mut max_ts = last_sync;
            for entry in &remote.entries {
                let ts = parse_ts(&entry.modified_at);
                if ts > max_ts {
                    max_ts = ts;
                }
                // #18: 跳过已标记删除的远端条目（tombstone）
                if entry.deleted_at.is_some() {
                    // #9: 远端删除 + 本地有修改 → 保留本地
                    if let Some(local_m) = local_meta(&entry.id) {
                        let local_ts = parse_ts(&local_m.modified_at);
                        let remote_deleted_ts = parse_ts(entry.deleted_at.as_deref().unwrap_or("0"));
                        if local_ts > remote_deleted_ts {
                            continue; // 本地更新，不删
                        }
                    }
                    storage::delete(&entry.id);
                    continue;
                }
                // 冲突解决 + 写入本地
                match local_meta(&entry.id) {
                    Some(local_m) => {
                        let local_ts = parse_ts(&local_m.modified_at);
                        if ts > local_ts {
                            if let Err(e) = cloud_to_local(entry) {
                                summary.errors.push(format!("写入条目 {} 失败：{e}", entry.id));
                            } else {
                                summary.downloaded += 1;
                            }
                        }
                    }
                    None => {
                        if let Err(e) = cloud_to_local(entry) {
                            summary.errors.push(format!("写入条目 {} 失败：{e}", entry.id));
                        } else {
                            summary.downloaded += 1;
                        }
                    }
                }
            }
            // #9: 远端 deleted_ids 处理：仅当本地未修改时才删除
            for id in &remote.deleted_ids {
                if let Some(local_m) = local_meta(id) {
                    // 如果本地比远端更晚修改，保留本地
                    // 远端 deleted_ids 没有时间戳信息，但我们仍需检查本地是否
                    // 在本次同步窗口内被修改过。由于我们无法确定远端删除的精确时间，
                    // 这里保守策略：如果本地有此条目且 remote 也返回了 deleted_ids，
                    // 说明远端确实删除了它。但如果 entry 列表中有同 id 的 tombstone，
                    // 上面的 #18 已经处理过了。对于仅出现在 deleted_ids 中的 id，
                    // 我们检查是否有比 last_sync 更新的本地修改。
                    let local_ts = parse_ts(&local_m.modified_at);
                    if local_ts > last_sync {
                        // 本地有更新，跳过删除
                        continue;
                    }
                }
                storage::delete(id);
                summary.deleted_local += 1;
            }
            let new_ts = max_ts.max(remote.server_time);
            state.server_sync.insert(config.url.clone(), new_ts);
            summary.per_server.push((server_name, Ok(())));
        }
        Err(e) => {
            summary.errors.push(format!("[{server_name}] 拉取失败：{e}"));
            summary.per_server.push((server_name, Err(e)));
        }
    }
}

/// 同步单个 Git 后端。
fn sync_git_backend(
    config: &GitConfig,
    local_metas: &[storage::MemoMeta],
    deleted_ids: &[String],
    summary: &mut SyncSummary,
) {
    let name = format!(
        "Git: {}",
        config
            .url
            .rsplit('/')
            .next()
            .unwrap_or(&config.url)
    );
    match super::git_store::sync_git_backend(config, local_metas, deleted_ids) {
        Ok((uploaded, downloaded)) => {
            summary.uploaded += uploaded;
            summary.downloaded += downloaded;
            summary.per_server.push((name, Ok(())));
        }
        Err(e) => {
            summary.errors.push(format!("[{name}] 同步失败：{e}"));
            summary.per_server.push((name, Err(e)));
        }
    }
}

// ── 本地删除记录 ──────────────────────────────────────────────────────────

fn deleted_ids_path() -> PathBuf {
    config_dir().join("deleted-ids.json")
}

fn load_deleted_ids() -> Vec<String> {
    fs::read_to_string(deleted_ids_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_deleted_ids(ids: &[String]) {
    if let Ok(json) = serde_json::to_string(ids) {
        let _ = fs::write(deleted_ids_path(), json);
    }
}

fn clear_deleted_ids() {
    let _ = fs::remove_file(deleted_ids_path());
}

/// 记录一个本地删除（下次同步时通知所有云端）。
pub fn record_deletion(id: &str) {
    let mut ids = load_deleted_ids();
    if !ids.contains(&id.to_string()) {
        ids.push(id.to_string());
        save_deleted_ids(&ids);
    }
}
