//! 备忘录本地存储 + ZIP 导入/导出。
//!
//! 目录结构：
//!   ~/.local/share/linbox/notepad/entries/<uuid>/meta.json
//!                                      /content.txt   正文（纯文本）
//!                                      /inline/*.png  正文里夹着的图片
//!                                      /files/*       附件（ppt/docx/任意文件）
//!
//! 正文内嵌图片的编码方式：`content.txt` 里每张图占一个 U+FFFC（对象替换
//! 字符），按出现顺序对应 `inline/0.png`、`inline/1.png` …。
//! 这样正文始终是纯文本（外部工具也能读），图片另存、导出 ZIP 时一并带走。
//!
//! ZIP 格式（导出 / 导入共用）：
//!   entry-<uuid>/meta.json
//!   entry-<uuid>/content.txt
//!   entry-<uuid>/inline/<n>.png
//!   entry-<uuid>/files/<filename>
//!   （导入时仍兼容旧版的 entry-<uuid>/images/<filename>）

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

fn entry_dir(id: &str) -> PathBuf {
    entries_dir().join(id)
}

// ── CRUD ──────────────────────────────────────────────────────────────────

/// 生成 16 位随机十六进制 ID。
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
    let mut list = Vec::new();
    if let Ok(dir) = fs::read_dir(entries_dir()) {
        for e in dir.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let meta_path = e.path().join("meta.json");
            if let Ok(text) = fs::read_to_string(&meta_path) {
                if let Ok(m) = serde_json::from_str::<MemoMeta>(&text) {
                    list.push(m);
                }
            }
        }
    }
    list.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));
    list
}

/// 读取条目正文。
pub fn read_content(id: &str) -> String {
    fs::read_to_string(entry_dir(id).join("content.txt")).unwrap_or_default()
}

/// 保存条目（正文 + 元信息），目录不存在则创建。
pub fn save(id: &str, meta: &MemoMeta, content: &str) {
    let dir = entry_dir(id);
    let _ = fs::create_dir_all(&dir);
    let _ = fs::write(
        dir.join("meta.json"),
        serde_json::to_string_pretty(meta).unwrap(),
    );
    let _ = fs::write(dir.join("content.txt"), content);
}

/// 读取单个条目的 meta.json（避免 load_all 全量扫描）。
pub fn load_meta(id: &str) -> Option<MemoMeta> {
    let meta_path = entry_dir(id).join("meta.json");
    let text = fs::read_to_string(&meta_path).ok()?;
    serde_json::from_str(&text).ok()
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

/// 删除条目（整个目录）。
pub fn delete(id: &str) {
    let _ = fs::remove_dir_all(entry_dir(id));
}

// ── 附件（任意文件：ppt / docx / pdf / 图片 …） ───────────────────────────

fn files_dir(id: &str) -> PathBuf {
    let d = entry_dir(id).join("files");
    let _ = fs::create_dir_all(&d);
    d
}

/// 旧版把图片放在 `images/` 下，这里一次性搬进 `files/`（幂等，搬完删空目录）。
fn migrate_images(id: &str) {
    let old = entry_dir(id).join("images");
    if !old.is_dir() {
        return;
    }
    let new = files_dir(id);
    if let Ok(dir) = fs::read_dir(&old) {
        for e in dir.flatten() {
            let src = e.path();
            if !src.is_file() {
                continue;
            }
            if let Some(name) = src.file_name().and_then(|n| n.to_str()) {
                let dst = unique_name(&new, name);
                let _ = fs::rename(&src, dst);
            }
        }
    }
    let _ = fs::remove_dir_all(&old);
}

/// 读取条目附件文件名（按名字排序）。
pub fn list_files(id: &str) -> Vec<String> {
    migrate_images(id);
    let mut out = Vec::new();
    if let Ok(dir) = fs::read_dir(files_dir(id)) {
        for e in dir.flatten() {
            if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                if let Some(name) = e.file_name().to_str() {
                    out.push(name.to_string());
                }
            }
        }
    }
    out.sort();
    out
}

/// 写入附件，返回实际落盘的文件名。
///
/// 同名文件**不会覆盖**（截图工具导出的名字高度雷同），
/// 重名时自动追加 `-1` / `-2` … 后缀。
pub fn write_file(id: &str, filename: &str, data: &[u8]) -> Option<String> {
    let name = safe_filename(filename)?;
    let dir = files_dir(id);
    let target = unique_name(&dir, name);
    let file_name = target.file_name().and_then(|n| n.to_str())?.to_string();
    fs::write(&target, data).ok()?;
    Some(file_name)
}

/// 附件的完整路径（给「用默认应用打开」用）。
pub fn file_path(id: &str, filename: &str) -> PathBuf {
    files_dir(id).join(safe_filename(filename).unwrap_or(filename))
}

/// 删除单个附件。
pub fn delete_file(id: &str, filename: &str) {
    if let Some(name) = safe_filename(filename) {
        let _ = fs::remove_file(files_dir(id).join(name));
    }
}

// ── 正文内嵌图片 ──────────────────────────────────────────────────────────

fn inline_dir(id: &str) -> PathBuf {
    entry_dir(id).join("inline")
}

/// 整体重写正文内嵌图片（`0.png`、`1.png` …，与正文里的 U+FFFC 一一对应）。
pub fn save_inline(id: &str, images: &[Vec<u8>]) {
    clear_inline(id);
    if images.is_empty() {
        return;
    }
    let dir = inline_dir(id);
    let _ = fs::create_dir_all(&dir);
    for (i, data) in images.iter().enumerate() {
        let _ = fs::write(dir.join(format!("{i}.png")), data);
    }
}

/// 读第 `index` 张内嵌图片。
pub fn read_inline(id: &str, index: usize) -> Option<Vec<u8>> {
    fs::read(inline_dir(id).join(format!("{index}.png"))).ok()
}

/// 按文件名读内嵌图片（ZIP 导出用）。
fn read_inline_file(id: &str, filename: &str) -> Option<Vec<u8>> {
    fs::read(inline_dir(id).join(safe_filename(filename)?)).ok()
}

/// 按原文件名写入内嵌图片（ZIP 导入用）。
pub fn write_inline_named(id: &str, filename: &str, data: &[u8]) {
    if let Some(name) = safe_filename(filename) {
        let dir = inline_dir(id);
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(dir.join(name), data);
    }
}

/// 列出内嵌图片文件名（按名字排序，ZIP 导出用）。
pub fn list_inline(id: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(dir) = fs::read_dir(inline_dir(id)) {
        for e in dir.flatten() {
            if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                if let Some(name) = e.file_name().to_str() {
                    out.push(name.to_string());
                }
            }
        }
    }
    out.sort();
    out
}

/// 清空内嵌图片。
pub fn clear_inline(id: &str) {
    let _ = fs::remove_dir_all(inline_dir(id));
}

/// 在 `dir` 里找一个没被占用的文件名：`a.png` → `a-1.png` → `a-2.png` …
fn unique_name(dir: &Path, name: &str) -> PathBuf {
    let direct = dir.join(name);
    if !direct.exists() {
        return direct;
    }
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let ext = Path::new(name).extension().and_then(|s| s.to_str());
    for i in 1..1000 {
        let candidate = match ext {
            Some(e) => format!("{stem}-{i}.{e}"),
            None => format!("{stem}-{i}"),
        };
        let p = dir.join(&candidate);
        if !p.exists() {
            return p;
        }
    }
    direct
}

/// 只接受纯文件名：剥掉任何目录成分，挡住 `../` 之类的路径穿越。
fn safe_filename(filename: &str) -> Option<&str> {
    let name = Path::new(filename).file_name()?.to_str()?;
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    Some(name)
}

/// 读取附件内容。
pub fn read_file(id: &str, filename: &str) -> Option<Vec<u8>> {
    fs::read(file_path(id, filename)).ok()
}

// ── ZIP 导出 ──────────────────────────────────────────────────────────────

/// 将本地所有条目导出到 ZIP。
pub fn export_zip(path: &Path) -> Result<(), String> {
    let file = fs::File::create(path).map_err(|e| format!("创建文件失败：{e}"))?;
    let mut zip = zip::write::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    for meta in load_all() {
        let prefix = format!("entry-{}", meta.id);

        // meta.json
        let json = serde_json::to_string_pretty(&meta).unwrap();
        zip.start_file(format!("{prefix}/meta.json"), options)
            .map_err(|e| e.to_string())?;
        zip.write_all(json.as_bytes()).map_err(|e| e.to_string())?;

        // content.txt
        let content = read_content(&meta.id);
        zip.start_file(format!("{prefix}/content.txt"), options)
            .map_err(|e| e.to_string())?;
        zip.write_all(content.as_bytes())
            .map_err(|e| e.to_string())?;

        // inline/*.png（正文里夹着的图片）
        for img in list_inline(&meta.id) {
            let data = read_inline_file(&meta.id, &img).unwrap_or_default();
            zip.start_file(format!("{prefix}/inline/{img}"), options)
                .map_err(|e| e.to_string())?;
            zip.write_all(&data).map_err(|e| e.to_string())?;
        }

        // files/*（附件：ppt / docx / 任意文件）
        for f in list_files(&meta.id) {
            let data = read_file(&meta.id, &f).unwrap_or_default();
            zip.start_file(format!("{prefix}/files/{f}"), options)
                .map_err(|e| e.to_string())?;
            zip.write_all(&data).map_err(|e| e.to_string())?;
        }
    }

    zip.finish().map_err(|e| format!("写入 zip 失败：{e}"))?;
    Ok(())
}

// ── ZIP 导入（增量：已有 ID 跳过） ───────────────────────────────────────

/// 从 ZIP 导入条目，返回 (新增数, 跳过数)。
pub fn import_zip(path: &Path) -> Result<(usize, usize), String> {
    let file = fs::File::open(path).map_err(|e| format!("打开文件失败：{e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("解析 zip 失败：{e}"))?;

    // 收集所有 entry-<id> 前缀
    let mut prefixes: Vec<(String, String)> = Vec::new(); // (prefix, id)
    for i in 0..archive.len() {
        let name = archive.by_index(i).unwrap().name().to_string();
        if let Some(rest) = name.strip_prefix("entry-") {
            if let Some(id) = rest.split('/').next() {
                let id = id.to_string();
                let prefix = format!("entry-{id}");
                if !prefixes.iter().any(|(p, _)| p == &prefix) {
                    prefixes.push((prefix, id));
                }
            }
        }
    }

    let mut imported = 0usize;
    let mut skipped = 0usize;

    for (prefix, id) in &prefixes {
        // 已存在则跳过
        if entry_dir(id).join("meta.json").exists() {
            skipped += 1;
            continue;
        }

        // 读 meta.json
        let meta_path = format!("{prefix}/meta.json");
        let meta_text = read_zip_entry(&mut archive, &meta_path)?;
        let meta: MemoMeta =
            serde_json::from_str(&meta_text).map_err(|e| format!("meta.json 解析失败：{e}"))?;

        // 读 content.txt
        let content_path = format!("{prefix}/content.txt");
        let content = read_zip_entry(&mut archive, &content_path).unwrap_or_default();

        save(id, &meta, &content);

        // 读 inline/*（正文内嵌图片）与 files/*（附件，兼容旧版 images/*）
        for dir in ["inline", "files", "images"] {
            let head = format!("{prefix}/{dir}/");
            for i in 0..archive.len() {
                let name = {
                    let entry = archive.by_index(i).unwrap();
                    entry.name().to_string()
                };
                let Some(rest) = name.strip_prefix(&head) else {
                    continue;
                };
                // 只收「该目录下直接的文件」，跳过子目录条目和空名。
                if rest.is_empty() || rest.contains('/') || rest == "." || rest == ".." {
                    continue;
                }
                let mut data = Vec::new();
                let mut reader = archive.by_index(i).unwrap();
                reader.read_to_end(&mut data).ok();
                match dir {
                    "inline" => write_inline_named(id, rest, &data),
                    // 旧版 images/ 里的图按附件导入
                    _ => {
                        write_file(id, rest, &data);
                    }
                }
            }
        }

        imported += 1;
    }

    Ok((imported, skipped))
}

fn read_zip_entry(archive: &mut zip::ZipArchive<fs::File>, name: &str) -> Result<String, String> {
    let mut file = archive.by_name(name).map_err(|e| format!("{name}：{e}"))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .map_err(|e| format!("{name} 读取失败：{e}"))?;
    Ok(buf)
}

// ── 工具 ──────────────────────────────────────────────────────────────────

/// 简易时间戳（UTC，秒级精度，ISO-8601）。
fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{secs}")
}
