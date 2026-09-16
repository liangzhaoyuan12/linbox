//! 备忘录本地存储 + ZIP 导入/导出。
//!
//! 存储格式：每个条目是一个独立的 ZIP 文件。
//!   ~/.local/share/linbox/notepad/entries/<uuid>.zip
//!     ├── meta.json
//!     ├── content.txt          正文（纯文本，U+FFFC = 图片占位）
//!     ├── inline/0.png         正文里夹着的图片
//!     ├── inline/1.png
//!     └── files/report.docx    附件（任意文件）
//!
//! 导出时将所有 `<uuid>.zip` 打包进一个大 ZIP：
//!   <export>.zip
//!     ├── <uuid1>.zip
//!     ├── <uuid2>.zip
//!     └── ...
//!
//! 导入时从大 ZIP 里逐个提取 `<uuid>.zip` 文件，已有 ID 跳过。
//! 兼容旧版目录格式（自动迁移为 ZIP）。

use std::collections::HashMap;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MemoMeta {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub modified_at: String,
}

// ── 本地路径 ──────────────────────────────────────────────────────────────

fn base_dir() -> PathBuf {
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("linbox")
        .join("notepad");
    let _ = fs::create_dir_all(&base);
    base
}

fn entries_dir() -> PathBuf {
    let d = base_dir().join("entries");
    let _ = fs::create_dir_all(&d);
    d
}

/// 单个条目的 ZIP 文件路径。
fn entry_zip(id: &str) -> PathBuf {
    entries_dir().join(format!("{id}.zip"))
}

// ── ZIP 内部读写 ──────────────────────────────────────────────────────────

/// 把整个 ZIP 读成 HashMap<路径, 字节>（内存里操作）。
fn read_zip_to_map(path: &Path) -> Result<HashMap<String, Vec<u8>>, String> {
    let file = fs::File::open(path).map_err(|e| format!("打开 ZIP 失败：{e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("解析 ZIP 失败：{e}"))?;
    let mut map = HashMap::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .map_err(|e| format!("读取 {name} 失败：{e}"))?;
        map.insert(name, data);
    }
    Ok(map)
}

/// 把 HashMap<路径, 字节> 原子写成 ZIP 文件。
///
/// 先写临时文件 + fsync，再 rename，防止崩溃/被杀导致 ZIP 损坏。
fn write_map_to_zip(path: &Path, map: &HashMap<String, Vec<u8>>) -> Result<(), String> {
    let tmp_path = path.with_extension("zip.tmp");
    let file = fs::File::create(&tmp_path).map_err(|e| format!("创建 ZIP 临时文件失败：{e}"))?;
    let mut zip = zip::write::ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    // 按路径排序保证 ZIP 内容稳定
    let mut entries: Vec<(&String, &Vec<u8>)> = map.iter().collect();
    entries.sort_by_key(|(k, _)| k.clone());
    for (name, data) in entries {
        zip.start_file(name, opts)
            .map_err(|e| e.to_string())?;
        zip.write_all(data).map_err(|e| e.to_string())?;
    }
    let mut f = zip.finish().map_err(|e| format!("写入 ZIP 失败：{e}"))?;
    f.sync_all().map_err(|e| format!("fsync 失败：{e}"))?;
    drop(f);
    fs::rename(&tmp_path, path).map_err(|e| format!("原子重命名失败：{e}"))
}

/// 把内容写进 ZIP map 的指定路径。
fn map_insert(map: &mut HashMap<String, Vec<u8>>, path: &str, data: Vec<u8>) {
    map.insert(path.to_string(), data);
}

/// 从 ZIP map 里读取指定路径的内容。
fn map_get<'a>(map: &'a HashMap<String, Vec<u8>>, path: &str) -> Option<&'a Vec<u8>> {
    map.get(path)
}

/// 从 ZIP map 里删除指定路径（及该路径下的所有子文件）。
fn map_remove_prefixed(map: &mut HashMap<String, Vec<u8>>, prefix: &str) {
    map.retain(|k, _| !k.starts_with(prefix));
}

/// 列出 ZIP map 里某个目录下的直接文件名（不含子目录）。
fn map_list_dir(map: &HashMap<String, Vec<u8>>, dir_prefix: &str) -> Vec<String> {
    let prefix = if dir_prefix.ends_with('/') {
        dir_prefix.to_string()
    } else {
        format!("{dir_prefix}/")
    };
    let mut names = Vec::new();
    for key in map.keys() {
        if let Some(rest) = key.strip_prefix(&prefix) {
            // 只取直接子文件（rest 里不含 /）
            if !rest.is_empty() && !rest.contains('/') {
                names.push(rest.to_string());
            }
        }
    }
    names.sort();
    names
}

// ── 旧版目录迁移 ──────────────────────────────────────────────────────────

/// 把旧版目录格式 `<id>/` 迁移为 ZIP 文件 `<id>.zip`（幂等）。
fn migrate_dir_to_zip(id: &str) {
    let dir = entries_dir().join(id);
    if !dir.is_dir() {
        return;
    }
    let zip_path = entry_zip(id);
    if zip_path.exists() {
        // ZIP 已存在，直接删掉旧目录
        let _ = fs::remove_dir_all(&dir);
        return;
    }
    // 把目录内容读进 map 然后写成 ZIP
    let mut map = HashMap::new();
    collect_dir_recursive(&dir, "", &mut map);
    let _ = write_map_to_zip(&zip_path, &map);
    let _ = fs::remove_dir_all(&dir);
}

fn collect_dir_recursive(base: &Path, prefix: &str, map: &mut HashMap<String, Vec<u8>>) {
    let dir = if prefix.is_empty() {
        base.to_path_buf()
    } else {
        base.join(prefix)
    };
    if let Ok(entries) = fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name_str = name.to_string_lossy();
            let rel = if prefix.is_empty() {
                name_str.to_string()
            } else {
                format!("{prefix}/{name_str}")
            };
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                collect_dir_recursive(base, &rel, map);
            } else if let Ok(data) = fs::read(e.path()) {
                map.insert(rel, data);
            }
        }
    }
}

/// 扫描所有旧版目录并迁移。
fn migrate_all_dirs() {
    if let Ok(dir) = fs::read_dir(entries_dir()) {
        for e in dir.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(name) = e.file_name().to_str() {
                    // 只处理看起来像 hex ID 的目录
                    if name.len() == 32 && name.chars().all(|c| c.is_ascii_hexdigit()) {
                        migrate_dir_to_zip(name);
                    }
                }
            }
        }
    }
}

// ── CRUD ──────────────────────────────────────────────────────────────────

/// 生成 32 字符的随机 ID（0-9a-f）。
pub fn random_id() -> String {
    use rand::Rng;
    let mut s = String::with_capacity(32);
    for b in rand::thread_rng().r#gen::<[u8; 16]>() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// 加载所有条目元信息（按修改时间倒序）。
pub fn load_all() -> Vec<MemoMeta> {
    migrate_all_dirs();
    let mut list = Vec::new();
    if let Ok(dir) = fs::read_dir(entries_dir()) {
        for e in dir.flatten() {
            let path = e.path();
            // 只处理 .zip 文件
            if !path.extension().map(|ext| ext == "zip").unwrap_or(false) {
                continue;
            }
            if let Some(id) = path.file_stem().and_then(|s| s.to_str()) {
                if let Some(meta) = load_meta(id) {
                    list.push(meta);
                }
            }
        }
    }
    list.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));
    list
}

/// 读取条目正文。
pub fn read_content(id: &str) -> String {
    let zip_path = entry_zip(id);
    if !zip_path.exists() {
        return String::new();
    }
    let map = match read_zip_to_map(&zip_path) {
        Ok(m) => m,
        Err(_) => return String::new(),
    };
    map_get(&map, "content.txt")
        .and_then(|d| String::from_utf8(d.clone()).ok())
        .unwrap_or_default()
}

/// 保存条目（正文 + 元信息），ZIP 不存在则创建。
pub fn save(id: &str, meta: &MemoMeta, content: &str) {
    let zip_path = entry_zip(id);
    let mut map = if zip_path.exists() {
        read_zip_to_map(&zip_path).unwrap_or_default()
    } else {
        HashMap::new()
    };
    let json = serde_json::to_string_pretty(meta).unwrap();
    map_insert(&mut map, "meta.json", json.into_bytes());
    map_insert(&mut map, "content.txt", content.as_bytes().to_vec());
    let _ = write_map_to_zip(&zip_path, &map);
}

/// 读取单个条目的 meta.json。
pub fn load_meta(id: &str) -> Option<MemoMeta> {
    let zip_path = entry_zip(id);
    if !zip_path.exists() {
        return None;
    }
    let map = read_zip_to_map(&zip_path).ok()?;
    let data = map_get(&map, "meta.json")?;
    let text = std::str::from_utf8(data).ok()?;
    serde_json::from_str(text).ok()
}

/// 新建条目，返回 ID。
pub fn create(title: &str) -> String {
    let id = random_id();
    let now = chrono_now();
    let meta = MemoMeta {
        id: id.clone(),
        title: title.to_string(),
        created_at: now.clone(),
        modified_at: now,
    };
    save(&id, &meta, "");
    id
}

/// 删除条目（ZIP 文件）。
pub fn delete(id: &str) {
    let _ = fs::remove_file(entry_zip(id));
    // 也删掉旧版目录（以防万一）
    let _ = fs::remove_dir_all(entries_dir().join(id));
}

// ── 附件（任意文件） ──────────────────────────────────────────────────────

/// 读取条目附件文件名（按名字排序）。
pub fn list_files(id: &str) -> Vec<String> {
    let zip_path = entry_zip(id);
    if !zip_path.exists() {
        return Vec::new();
    }
    let map = match read_zip_to_map(&zip_path) {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    map_list_dir(&map, "files")
}

/// 写入附件，返回实际落盘的文件名。
///
/// 同名文件**不会覆盖**（截图工具导出的名字高度雷同），
/// 重名时自动追加 `-1` / `-2` … 后缀。
pub fn write_file(id: &str, filename: &str, data: &[u8]) -> Option<String> {
    let name = safe_filename(filename)?;
    let zip_path = entry_zip(id);
    let mut map = if zip_path.exists() {
        read_zip_to_map(&zip_path).unwrap_or_default()
    } else {
        HashMap::new()
    };
    // 找一个不重名的文件名
    let final_name = unique_name_in_map(&map, "files", name);
    let path = format!("files/{final_name}");
    map_insert(&mut map, &path, data.to_vec());
    let _ = write_map_to_zip(&zip_path, &map);
    Some(final_name)
}

/// 在 ZIP map 的某个目录里找不重名的文件名。
fn unique_name_in_map(map: &HashMap<String, Vec<u8>>, dir: &str, name: &str) -> String {
    let path = format!("{dir}/{name}");
    if !map.contains_key(&path) {
        return name.to_string();
    }
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = Path::new(name).extension().and_then(|s| s.to_str());
    for i in 1..1000 {
        let candidate = match ext {
            Some(e) => format!("{stem}-{i}.{e}"),
            None => format!("{stem}-{i}"),
        };
        let p = format!("{dir}/{candidate}");
        if !map.contains_key(&p) {
            return candidate;
        }
    }
    name.to_string()
}

/// 附件的完整路径（给「用默认应用打开」用）。
/// 先解压到临时目录，返回临时文件路径。
pub fn file_path(id: &str, filename: &str) -> PathBuf {
    let zip_path = entry_zip(id);
    if !zip_path.exists() {
        return PathBuf::new();
    }
    let map = read_zip_to_map(&zip_path).unwrap_or_default();
    let name = safe_filename(filename).unwrap_or(filename);
    let key = format!("files/{name}");
    if let Some(data) = map_get(&map, &key) {
        // 解压到临时目录
        let tmp_dir = base_dir().join("tmp");
        let _ = fs::create_dir_all(&tmp_dir);
        let tmp_file = tmp_dir.join(format!("{id}_{name}"));
        let _ = fs::write(&tmp_file, data);
        tmp_file
    } else {
        PathBuf::new()
    }
}

/// 删除单个附件。
pub fn delete_file(id: &str, filename: &str) {
    let Some(name) = safe_filename(filename) else {
        return;
    };
    let zip_path = entry_zip(id);
    if !zip_path.exists() {
        return;
    }
    let mut map = match read_zip_to_map(&zip_path) {
        Ok(m) => m,
        Err(_) => return,
    };
    let key = format!("files/{name}");
    map.remove(&key);
    let _ = write_map_to_zip(&zip_path, &map);
}

/// 读取附件内容。
pub fn read_file(id: &str, filename: &str) -> Option<Vec<u8>> {
    let zip_path = entry_zip(id);
    if !zip_path.exists() {
        return None;
    }
    let map = read_zip_to_map(&zip_path).ok()?;
    let name = safe_filename(filename)?;
    let key = format!("files/{name}");
    map_get(&map, &key).cloned()
}

// ── 正文内嵌图片 ──────────────────────────────────────────────────────────

/// 整体重写正文内嵌图片（`inline/0.png`、`inline/1.png` …）。
pub fn save_inline(id: &str, images: &[Vec<u8>]) {
    let zip_path = entry_zip(id);
    let mut map = if zip_path.exists() {
        read_zip_to_map(&zip_path).unwrap_or_default()
    } else {
        HashMap::new()
    };
    // 先清除旧的 inline 文件
    map_remove_prefixed(&mut map, "inline/");
    // 写入新的
    for (i, data) in images.iter().enumerate() {
        let key = format!("inline/{i}.png");
        map_insert(&mut map, &key, data.clone());
    }
    let _ = write_map_to_zip(&zip_path, &map);
}

/// 读第 `index` 张内嵌图片。
pub fn read_inline(id: &str, index: usize) -> Option<Vec<u8>> {
    let zip_path = entry_zip(id);
    if !zip_path.exists() {
        return None;
    }
    let map = read_zip_to_map(&zip_path).ok()?;
    let key = format!("inline/{index}.png");
    map_get(&map, &key).cloned()
}

/// 第 `index` 张内嵌图片落成磁盘上的真实文件，返回它的路径。
///
/// 内嵌图片在磁盘上只存在于条目的 ZIP 里（`inline/N.png`），没有「所在文件夹」可言，
/// 所以和附件（`file_path`）一样解压到 `<base_dir>/tmp/` 下再返回
/// —— 这样「打开」和「在文件夹中显示」都有真文件可用，而且两者落在同一个目录。
pub fn inline_image_path(id: &str, index: usize) -> Option<PathBuf> {
    let data = read_inline(id, index)?;
    let tmp_dir = base_dir().join("tmp");
    fs::create_dir_all(&tmp_dir).ok()?;
    let tmp_file = tmp_dir.join(format!("{id}_inline{index}.png"));
    fs::write(&tmp_file, &data).ok()?;
    Some(tmp_file)
}

/// 按文件名写入内嵌图片（ZIP 导入用）。
pub fn write_inline_named(id: &str, filename: &str, data: &[u8]) {
    let Some(name) = safe_filename(filename) else {
        return;
    };
    let zip_path = entry_zip(id);
    let mut map = if zip_path.exists() {
        read_zip_to_map(&zip_path).unwrap_or_default()
    } else {
        HashMap::new()
    };
    let key = format!("inline/{name}");
    map_insert(&mut map, &key, data.to_vec());
    let _ = write_map_to_zip(&zip_path, &map);
}

/// 列出内嵌图片文件名（按名字排序，ZIP 导出用）。
pub fn list_inline(id: &str) -> Vec<String> {
    let zip_path = entry_zip(id);
    if !zip_path.exists() {
        return Vec::new();
    }
    let map = match read_zip_to_map(&zip_path) {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    map_list_dir(&map, "inline")
}

/// 清空内嵌图片。
pub fn clear_inline(id: &str) {
    let zip_path = entry_zip(id);
    if !zip_path.exists() {
        return;
    }
    if let Ok(mut map) = read_zip_to_map(&zip_path) {
        map_remove_prefixed(&mut map, "inline/");
        let _ = write_map_to_zip(&zip_path, &map);
    }
}

// ── 工具 ──────────────────────────────────────────────────────────────────

/// 只接受纯文件名：剥掉任何目录成分，挡住 `../` 之类的路径穿越。
fn safe_filename(filename: &str) -> Option<&str> {
    let name = Path::new(filename).file_name()?.to_str()?;
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    Some(name)
}

/// 简易时间戳（UTC，秒级精度）。
fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{secs}")
}

// ── ZIP 导出 ──────────────────────────────────────────────────────────────

/// 将本地所有条目导出到一个大 ZIP。
///
/// 大 ZIP 内部结构：每个条目是一个独立的 `<id>.zip` 文件。
pub fn export_zip(path: &Path) -> Result<(), String> {
    migrate_all_dirs();
    let file = fs::File::create(path).map_err(|e| format!("创建文件失败：{e}"))?;
    let mut outer_zip = zip::write::ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    if let Ok(dir) = fs::read_dir(entries_dir()) {
        for e in dir.flatten() {
            let p = e.path();
            // 只导出 .zip 文件
            if !p.extension().map(|ext| ext == "zip").unwrap_or(false) {
                continue;
            }
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // 读取整个条目 ZIP 文件的字节
            let data = fs::read(&p).map_err(|e| format!("读取 {name} 失败：{e}"))?;
            outer_zip
                .start_file(name, opts)
                .map_err(|e| e.to_string())?;
            outer_zip.write_all(&data).map_err(|e| e.to_string())?;
        }
    }

    outer_zip
        .finish()
        .map_err(|e| format!("写入 zip 失败：{e}"))?;
    Ok(())
}

// ── ZIP 导入（增量：已有 ID 跳过） ───────────────────────────────────────

/// 从 ZIP 导入条目，返回 (新增数, 跳过数)。
///
/// 大 ZIP 里的每个 `<id>.zip` 文件代表一个条目。
/// 兼容旧版格式（大 ZIP 里直接是 `entry-<id>/meta.json` 等文件）。
pub fn import_zip(path: &Path) -> Result<(usize, usize), String> {
    let file = fs::File::open(path).map_err(|e| format!("打开文件失败：{e}"))?;
    let mut outer = zip::ZipArchive::new(file).map_err(|e| format!("解析 zip 失败：{e}"))?;

    let mut imported = 0usize;
    let mut skipped = 0usize;

    // ── 新版格式：大 ZIP 里的 `<id>.zip` 文件 ──
    for i in 0..outer.len() {
        let name = match outer.by_index(i) {
            Ok(e) => {
                let n = e.name().to_string();
                drop(e);
                n
            }
            Err(_) => continue,
        };
        // 只处理根目录下的 .zip 文件（不含 / 的 .zip 文件名）
        if name.contains('/') || !name.ends_with(".zip") {
            continue;
        }
        let id = name.strip_suffix(".zip").unwrap_or(&name);
        // ID 合法性检查：32 位十六进制
        if id.len() != 32 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        // 已存在则跳过
        if entry_zip(id).exists() {
            skipped += 1;
            continue;
        }
        // 读取条目 ZIP 的字节并写到本地
        let Some(mut entry) = outer.by_index(i).ok() else { continue };
        let mut data = Vec::new();
        entry.read_to_end(&mut data).map_err(|e| e.to_string())?;
        fs::write(entry_zip(id), data).map_err(|e| format!("写入条目失败：{e}"))?;
        imported += 1;
    }

    // ── 兼容旧版格式：大 ZIP 里直接是 `entry-<id>/meta.json` 等 ──
    let mut prefixes: Vec<(String, String)> = Vec::new();
    for i in 0..outer.len() {
        let name = match outer.by_index(i) {
            Ok(e) => {
                let n = e.name().to_string();
                drop(e);
                n
            }
            Err(_) => continue,
        };
        if let Some(rest) = name.strip_prefix("entry-") {
            if let Some(id) = rest.split('/').next() {
                let id = id.to_string();
                let prefix = format!("entry-{id}");
                if !prefixes.iter().any(|(p, _)| p == &prefix)
                    && id.len() == 32
                    && id.chars().all(|c| c.is_ascii_hexdigit())
                {
                    prefixes.push((prefix, id));
                }
            }
        }
    }

    for (prefix, id) in &prefixes {
        // 已存在则跳过（新版 ZIP 或旧版目录）
        if entry_zip(id).exists() || entries_dir().join(id).is_dir() {
            skipped += 1;
            continue;
        }

        // 从旧版格式构建 ZIP map
        let mut map = HashMap::new();

        // meta.json
        let meta_path = format!("{prefix}/meta.json");
        if let Some(data) = read_zip_entry_bytes(&mut outer, &meta_path) {
            map_insert(&mut map, "meta.json", data);
        }

        // content.txt
        let content_path = format!("{prefix}/content.txt");
        if let Some(data) = read_zip_entry_bytes(&mut outer, &content_path) {
            map_insert(&mut map, "content.txt", data);
        }

        // inline/*, files/*, images/*（兼容旧版）
        for dir in ["inline", "files", "images"] {
            let head = format!("{prefix}/{dir}/");
            for i in 0..outer.len() {
                let Some(entry) = outer.by_index(i).ok() else { continue };
                let name = entry.name().to_string();
                let Some(rest) = name.strip_prefix(&head) else {
                    continue;
                };
                if rest.is_empty() || rest.contains('/') || rest == "." || rest == ".." {
                    continue;
                }
                let mut data = Vec::new();
                let mut reader = entry;
                reader.read_to_end(&mut data).ok();
                let target_dir = if dir == "images" { "files" } else { dir };
                let key = format!("{target_dir}/{rest}");
                map_insert(&mut map, &key, data);
            }
        }

        // 写成 ZIP 文件
        if !map.is_empty() {
            let _ = write_map_to_zip(&entry_zip(id), &map);
            imported += 1;
        }
    }

    Ok((imported, skipped))
}

/// 从外层 ZIP 读取指定条目的原始字节。
fn read_zip_entry_bytes(
    archive: &mut zip::ZipArchive<fs::File>,
    name: &str,
) -> Option<Vec<u8>> {
    let mut entry = archive.by_name(name).ok()?;
    let mut data = Vec::new();
    entry.read_to_end(&mut data).ok();
    Some(data)
}
