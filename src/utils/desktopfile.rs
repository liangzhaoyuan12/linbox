//! desktop 文件的**扫描与写入执行层**（GOAL Phase 12 的 12.3）。
//!
//! 职责：
//! - [`scan`]：多目录枚举 + **同 id 优先级合并**（生效层 + 被覆盖层，D12.2）；
//! - [`read_effective`]：按 id 读最高优先级那份；
//! - [`write_desktop`]：备份 → 写（用户层直写 / 系统层 `pkexec sh -c` 单条）→
//!   回读逐字节比对 → 失败回滚（`WriteOutcome` 凭证，D12.7-④）；
//! - [`create_desktop`]：id 校验 + 目标已存在拒绝（D12.5-③）；
//! - [`hide_desktop`]：写 `Hidden=true`（D12.3）；
//! - [`delete_desktop`]：`mv` 成 `.linbox.bak` + 回读断言（D12.6-②）；
//! - [`run_desktop_file_validate`]：临时副本跑 `desktop-file-validate`、
//!   **逐行解析报错、工具缺失降级**（D12.7-③，不假装跑过）；
//! - [`update_desktop_database`]：存在才跑、失败仅记录（D12.11）。
//!
//! 约束（Phase 7/10/11 同款）：本文件禁止 `use gtk` / `use adw` / `use glib`。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::model::desktopfile::{
    DesktopDoc, DesktopKind, ENTRY_GROUP, backup_path_of, parse_desktop_text, render_desktop_text,
};
use crate::model::systemd::Scope;
use crate::utils::env_editor::can_write_direct;
use crate::utils::unitfile::{elevated_write_script, unit_write_argv};

// ---------------------------------------------------------------------------
// 扫描与优先级合并（D12.2）
// ---------------------------------------------------------------------------

/// 来源徽章（D12.9：按目录判 用户 / 本地 / 系统）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// 用户目录（`~/.local/share/applications` / `~/.config/autostart`）。
    User,
    /// `/usr/local/share/applications`（管理员本地）。
    Local,
    /// 系统目录（`/usr/share/applications` / `/etc/xdg/autostart`）。
    System,
}

impl Source {
    /// 中文标签。
    pub fn label(self) -> &'static str {
        match self {
            Source::User => "用户",
            Source::Local => "本地",
            Source::System => "系统",
        }
    }
}

/// 按路径判来源（`/usr/local/…` 优先判 Local；其余按 kind 目录归属）。
pub fn source_of(path: &Path) -> Source {
    let s = path.to_string_lossy();
    if s.starts_with("/usr/local/share/applications") {
        return Source::Local;
    }
    if s.starts_with("/usr/share/applications") || s.starts_with("/etc/xdg/autostart") {
        return Source::System;
    }
    Source::User
}

/// 一个枚举到的文件：`(目录优先级序号, 路径, id)`。
type Hit = (usize, PathBuf, String);

/// 纯函数：同 id 的多层命中 → `(生效层, 被覆盖层)`。
///
/// 规则：目录序号升序（XDG 优先级），序号最小的为生效层；同序号重复按出现序
/// 稳定排列（后者进被覆盖层）；完全相同的路径去重。
pub fn merge_priority(mut hits: Vec<Hit>) -> (PathBuf, Vec<PathBuf>) {
    debug_assert!(!hits.is_empty(), "空命中集不进 merge（调用方保证）");
    hits.sort_by_key(|h| h.0);
    let mut effective: Option<PathBuf> = None;
    let mut overridden: Vec<PathBuf> = Vec::new();
    for (_, p, _) in hits {
        match &effective {
            None => effective = Some(p),
            Some(e) if *e == p => {}
            Some(_) => overridden.push(p),
        }
    }
    (effective.expect("非空命中集必有生效层"), overridden)
}

/// 一条合并后的条目（页面列表 / 详情用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopItem {
    /// 文件名 id（`cn.loongnix.lbrowser.desktop`）。
    pub id: String,
    pub kind: DesktopKind,
    /// 生效层文件路径。
    pub effective: PathBuf,
    /// 生效层来源。
    pub source: Source,
    /// 被更低优先级覆盖的同 id 路径（只读展示，D12.2）。
    pub overridden: Vec<PathBuf>,
    /// 生效层内容；`None` = 文件不可读（坏文件仍可查可删，D12.9）。
    pub doc: Option<DesktopDoc>,
}

impl DesktopItem {
    /// 显示名：`Name=` → 否则去后缀的 id。
    pub fn display_name(&self) -> String {
        self.doc
            .as_ref()
            .and_then(|d| d.get("Name"))
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.id.trim_end_matches(".desktop").to_string())
    }

    /// `Comment=`（列表副标题）。
    pub fn comment(&self) -> Option<String> {
        self.doc.as_ref().and_then(|d| d.get("Comment"))
    }

    /// `Exec=`。
    pub fn exec(&self) -> Option<String> {
        self.doc.as_ref().and_then(|d| d.get("Exec"))
    }

    /// `NoDisplay=true`（D12.3 徽章「不在菜单显示」）。
    pub fn no_display(&self) -> bool {
        self.doc
            .as_ref()
            .and_then(|d| d.get("NoDisplay"))
            .is_some_and(|v| v == "true")
    }

    /// `Hidden=true`（D12.3 徽章「已禁用」/ 自启停用）。
    pub fn hidden(&self) -> bool {
        self.doc
            .as_ref()
            .and_then(|d| d.get("Hidden"))
            .is_some_and(|v| v == "true")
    }

    /// 有低优先级同名副本（徽章「覆盖 N 份」，D12.2/摸底 2）。
    pub fn covered(&self) -> bool {
        !self.overridden.is_empty()
    }
}

/// 枚举单个目录：只收 `*.desktop` 文件（目录缺失/不可读 → 空，容错）。
fn hits_in_dir(dir: &Path, idx: usize) -> Vec<Hit> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Some(name) = p.file_name().map(|n| n.to_os_string()) else {
            continue;
        };
        let name = name.to_string_lossy().into_owned();
        if !name.ends_with(".desktop") || !p.is_file() {
            continue;
        }
        out.push((idx, p, name));
    }
    out
}

/// 扫描一个模块的全部目录 → 按 id 合并的条目（id 字典序，列表稳定）。
pub fn scan(kind: DesktopKind) -> Vec<DesktopItem> {
    let dirs: Vec<PathBuf> = match kind {
        DesktopKind::Menu => crate::model::desktopfile::menu_dirs(),
        DesktopKind::Autostart => crate::model::desktopfile::autostart_dirs(),
    };
    scan_in(&dirs, kind)
}

/// [`scan`] 的可注入核心（单测喂临时目录；目录缺失容错在 `hits_in_dir`）。
pub fn scan_in(dirs: &[PathBuf], kind: DesktopKind) -> Vec<DesktopItem> {
    let mut groups: Vec<(String, Vec<Hit>)> = Vec::new();
    for (idx, dir) in dirs.iter().enumerate() {
        for hit in hits_in_dir(dir, idx) {
            let id = hit.2.clone();
            match groups.iter_mut().find(|(g, _)| g == &id) {
                Some((_, v)) => v.push(hit),
                None => groups.push((id, vec![hit])),
            }
        }
    }
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    groups
        .into_iter()
        .map(|(_, hits)| {
            let (effective, overridden) = merge_priority(hits);
            let doc = std::fs::read_to_string(&effective)
                .ok()
                .map(|s| parse_desktop_text(&s));
            let source = source_of(&effective);
            let id = effective
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            DesktopItem {
                id,
                kind,
                effective,
                source,
                overridden,
                doc,
            }
        })
        .collect()
}

/// 按 id 读生效层（最高优先级那份）：`(路径, 内容)`。
pub fn read_effective(id: &str, kind: DesktopKind) -> Result<(PathBuf, String), String> {
    validate_desktop_id(id)?;
    let dirs: Vec<PathBuf> = match kind {
        DesktopKind::Menu => crate::model::desktopfile::menu_dirs(),
        DesktopKind::Autostart => crate::model::desktopfile::autostart_dirs(),
    };
    for dir in &dirs {
        let p = dir.join(id);
        if p.is_file() {
            let s = std::fs::read_to_string(&p)
                .map_err(|e| format!("读取 {} 失败：{e}", p.display()))?;
            return Ok((p, s));
        }
    }
    Err(format!("{id} 在 {} 的任何目录都不存在", kind.label()))
}

// ---------------------------------------------------------------------------
// id 校验与路径安全（D12.5-③）
// ---------------------------------------------------------------------------

/// id 校验：`^[A-Za-z0-9._@-]+\.desktop$`——禁 `/`、空格、`..`、空字节、
/// 必须 `.desktop` 后缀且基名非空。
pub fn validate_desktop_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("id 不能为空".to_string());
    }
    if id != id.trim() {
        return Err("id 首尾不能有空白".to_string());
    }
    if id.contains('/') || id.contains('\\') || id.contains('\0') || id.contains('\n') {
        return Err(format!("id 含非法字符：{id:?}"));
    }
    if id.contains("..") {
        return Err(format!("id 不允许包含 `..`：{id}"));
    }
    if id.chars().any(|c| c.is_whitespace()) {
        return Err(format!("id 不能含空白字符：{id:?}"));
    }
    for c in id.chars() {
        if !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-')) {
            return Err(format!("id 含白名单外字符 {c:?}：{id}"));
        }
    }
    let Some(base) = id.strip_suffix(".desktop") else {
        return Err(format!("id 必须以 .desktop 结尾：{id}"));
    };
    if base.is_empty() || base == ".desktop".trim_end_matches(".desktop") {
        return Err(format!("id 不完整：{id}"));
    }
    if base.starts_with('.') {
        return Err(format!("id 基名不能以 . 开头：{id}"));
    }
    Ok(())
}

/// 路径安全闸门（防注入进 pkexec 脚本 / 越界）：绝对路径、无 `..`、无控制字符。
pub fn check_path_safety(path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("只接受绝对路径：{path}"));
    }
    if path.contains("..") {
        return Err(format!("路径不允许包含 `..`：{path}"));
    }
    if path.contains('\0') || path.contains('\n') || path.contains('\r') {
        return Err(format!("路径含控制字符：{path:?}"));
    }
    if path.contains('\'') || path.contains('\\') || path.contains('$') {
        return Err(format!("路径含 shell 特殊字符，拒绝：{path}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 新建（D12.5）
// ---------------------------------------------------------------------------

/// 新建目标目录（D12.5-②：默认用户目录；系统层菜单选 `/usr/local/share/applications`、
/// 自启选 `/etc/xdg/autostart`）。
pub fn create_dir_for(kind: DesktopKind, scope: Scope) -> Result<PathBuf, String> {
    match (kind, scope) {
        (DesktopKind::Menu, Scope::User) => {
            let home = dirs::home_dir().ok_or_else(|| "无法确定用户主目录".to_string())?;
            Ok(home.join(".local/share/applications"))
        }
        (DesktopKind::Menu, Scope::System) => Ok(PathBuf::from("/usr/local/share/applications")),
        (DesktopKind::Autostart, Scope::User) => {
            let home = dirs::home_dir().ok_or_else(|| "无法确定用户主目录".to_string())?;
            Ok(home.join(".config/autostart"))
        }
        (DesktopKind::Autostart, Scope::System) => Ok(PathBuf::from("/etc/xdg/autostart")),
    }
}

/// 新建前置（D12.5-③）：id 校验 + **同 id 全目录冲突拒绝**（摸底 2：同名会
/// 顶掉生效层，12.7-⑥ 的「同名拒绝」场景）→ 返回目标路径（内容写入走 [`write_desktop`]）。
pub fn create_desktop(kind: DesktopKind, id: &str, scope: Scope) -> Result<PathBuf, String> {
    validate_desktop_id(id)?;
    let dirs: Vec<PathBuf> = match kind {
        DesktopKind::Menu => crate::model::desktopfile::menu_dirs(),
        DesktopKind::Autostart => crate::model::desktopfile::autostart_dirs(),
    };
    for dir in &dirs {
        let p = dir.join(id);
        if p.exists() {
            return Err(format!(
                "{id} 已存在（{}），请改用「编辑」——同名文件会直接顶掉生效层",
                p.display()
            ));
        }
    }
    Ok(create_dir_for(kind, scope)?.join(id))
}

// ---------------------------------------------------------------------------
// 写入链（D12.7-④ 备份 → 写 → 回读 → 回滚；骨架照抄 unitfile::write_unit）
// ---------------------------------------------------------------------------

/// 写入结果凭证。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteOutcome {
    /// 备份路径（目标原先不存在 → `None`）。
    pub backup: Option<PathBuf>,
    /// 是否走了 pkexec 提权。
    pub elevated: bool,
    /// 注入 reload 的退出码（真实链路传 `||0`，desktop 文件无 reload 概念；
    /// `update-desktop-database` 是写成后单独跑、失败仅记录，见 D12.7-④）。
    pub reload_rc: i32,
}

/// 写入全链（`reload` 可注入，单测 `||1` 验证回滚）。
///
/// 自家防线（D12.7-①）：`parse → render → parse` 语义等值 + 必须以 `\n` 结尾；
/// 坏内容连备份都不做。
fn write_desktop_with_reload<F: FnOnce() -> i32>(
    target: &Path,
    content: &str,
    reload: F,
) -> Result<WriteOutcome, String> {
    let target_s = target.to_string_lossy().into_owned();
    check_path_safety(&target_s)?;
    if !content.ends_with('\n') {
        return Err("内容必须以换行结尾，拒绝写入".to_string());
    }
    let d1 = parse_desktop_text(content);
    let rendered = render_desktop_text(&d1);
    let d2 = parse_desktop_text(&rendered);
    if d1 != d2 {
        return Err("内容未通过自家往返自检，拒绝写入".to_string());
    }

    let existed = target.exists();
    let bak = backup_path_of(target);
    let elevated = !can_write_direct(&target_s);

    if elevated {
        elevated_backup_and_write(target, content)?;
    } else if let Err(e) = direct_backup_and_write(target, &bak, existed, content) {
        if existed {
            let _ = restore(target, existed, false);
        }
        return Err(e);
    }

    match std::fs::read(target) {
        Ok(bytes) if bytes == content.as_bytes() => {}
        Ok(bytes) => {
            let _ = restore(target, existed, elevated);
            return Err(format!(
                "回读逐字节比对失败（写入 {} 字节，回读 {} 字节），已从 .linbox.bak 回滚",
                content.len(),
                bytes.len()
            ));
        }
        Err(e) => {
            let _ = restore(target, existed, elevated);
            return Err(format!("回读失败：{e}，已尝试从 .linbox.bak 回滚"));
        }
    }

    let reload_rc = reload();
    if reload_rc != 0 {
        let _ = restore(target, existed, elevated);
        return Err(format!(
            "注入的 reload 失败（RC={reload_rc}），已从 .linbox.bak 回滚"
        ));
    }
    Ok(WriteOutcome {
        backup: existed.then_some(bak),
        elevated,
        reload_rc,
    })
}

/// 对外写入口：真实链路（reload 恒 0；系统层缓存刷新走 [`update_desktop_database`]）。
pub fn write_desktop(target: &Path, content: &str) -> Result<WriteOutcome, String> {
    write_desktop_with_reload(target, content, || 0)
}

/// 只测链路时用（reload 注入）。
pub fn write_desktop_with<F: FnOnce() -> i32>(
    target: &Path,
    content: &str,
    reload: F,
) -> Result<WriteOutcome, String> {
    write_desktop_with_reload(target, content, reload)
}

/// 直写：先备份旧文件再落盘。
fn direct_backup_and_write(
    target: &Path,
    bak: &Path,
    existed: bool,
    content: &str,
) -> Result<(), String> {
    if existed {
        std::fs::copy(target, bak).map_err(|e| format!("备份 {} 失败：{e}", target.display()))?;
    }
    std::fs::write(target, content).map_err(|e| format!("写入失败：{e}"))
}

/// 提权写：内容先落 0600 临时文件，`pkexec sh -c` 单条脚本拷进目标
/// （复用 unitfile 的脚本与 argv 构造，**不经 shell 展开内容**）。
fn elevated_backup_and_write(target: &Path, content: &str) -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!("linbox-desktop-write-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("建临时目录失败：{e}"))?;
    let tmp = dir.join("content");
    std::fs::write(&tmp, content).map_err(|e| format!("写临时文件失败：{e}"))?;
    let script = elevated_write_script(target, &tmp);
    let argv = unit_write_argv(&script);
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("无法执行提权写入：{e}"))?;
    let _ = std::fs::remove_dir_all(&dir);
    if !out.status.success() {
        return Err(format!(
            "提权写入失败：{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// 回滚：原先存在 → 从备份拷回；原先不存在 → 删掉误写文件（提权路径经 pkexec）。
fn restore(target: &Path, existed: bool, elevated: bool) -> Result<(), String> {
    let bak = backup_path_of(target);
    if existed {
        if elevated {
            let script = format!(
                "cat {} > {}\n",
                sh_q(&bak.display().to_string()),
                sh_q(&target.display().to_string())
            );
            let argv = unit_write_argv(&script);
            let out = Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("回滚执行失败：{e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "回滚失败：{}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Ok(())
        } else {
            std::fs::copy(&bak, target)
                .map(|_| ())
                .map_err(|e| format!("回滚拷回失败：{e}"))
        }
    } else if elevated {
        let script = format!("rm -f {}\n", sh_q(&target.display().to_string()));
        let argv = unit_write_argv(&script);
        let _ = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("回滚执行失败：{e}"))?;
        Ok(())
    } else {
        let _ = std::fs::remove_file(target);
        Ok(())
    }
}

/// shell 单引号转义。
fn sh_q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// 停用 / 启用（D12.3）
// ---------------------------------------------------------------------------

/// 写 `Hidden=true`（D12.3：不删文件的可逆停用；若已有 `X-GNOME-Autostart-enabled`
/// 则同步 `false`——本机 lbrowser 与 GNOME 惯例）。返回写入凭证。
pub fn hide_desktop(path: &Path) -> Result<WriteOutcome, String> {
    set_hidden(path, true)
}

/// `Hidden` 开关（启用 = `false`；原键不存在时**不写行**——缺省即启用，摸底 8）。
pub fn set_hidden(path: &Path, hidden: bool) -> Result<WriteOutcome, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("读取失败：{e}"))?;
    let mut doc = parse_desktop_text(&text);
    let had_hidden = doc.get("Hidden").is_some();
    if !hidden && !had_hidden {
        // 启用且本就没有停用键：只处理 GNOME 开关，不无谓写行
        return sync_gnome_enabled(&mut doc, true, &text, path);
    }
    doc.set_value(ENTRY_GROUP, "Hidden", if hidden { "true" } else { "false" });
    let gnome_synced = sync_gnome_in(&mut doc, !hidden);
    if !gnome_synced && render_desktop_text(&doc) == text {
        return Ok(WriteOutcome {
            backup: None,
            elevated: false,
            reload_rc: 0,
        }); // 内容无变化，不落盘
    }
    write_desktop(path, &render_desktop_text(&doc))
}

/// 启用分支的辅助：无 Hidden 键时仅同步 GNOME 开关（可能什么都不改）。
fn sync_gnome_enabled(
    doc: &mut DesktopDoc,
    enabled: bool,
    original: &str,
    path: &Path,
) -> Result<WriteOutcome, String> {
    let synced = sync_gnome_in(doc, enabled);
    if !synced || render_desktop_text(doc) == original {
        return Ok(WriteOutcome {
            backup: None,
            elevated: false,
            reload_rc: 0,
        });
    }
    let _ = path;
    write_desktop(path, &render_desktop_text(doc))
}

/// 文件已有 `X-GNOME-Autostart-enabled` → 同步 true/false；没有 → 不新建（返回 false）。
fn sync_gnome_in(doc: &mut DesktopDoc, enabled: bool) -> bool {
    if doc.get("X-GNOME-Autostart-enabled").is_some() {
        doc.set_value(
            ENTRY_GROUP,
            "X-GNOME-Autostart-enabled",
            if enabled { "true" } else { "false" },
        );
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// 物理删除（D12.6-②）
// ---------------------------------------------------------------------------

/// 删除脚本（纯函数，单测锁形状）：只 `mv` 点名的那一个文件，**绝不 glob、绝不 rm**。
pub fn delete_script(path: &Path) -> String {
    let p = path.display().to_string();
    let bak = format!("{p}.linbox.bak");
    format!("mv -f {} {}\n", sh_q(&p), sh_q(&bak))
}

/// 删除结果凭证。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteOutcome {
    /// 备份路径。
    pub backup: PathBuf,
    /// 是否走了 pkexec。
    pub elevated: bool,
}

/// 物理删除：`mv` 成 `.linbox.bak`（已存在备份直接覆盖）+ **回读断言**
/// （文件仍在 → Err；备份必须在 → 否则 Err）。系统层一条 `pkexec sh -c`。
pub fn delete_desktop(path: &Path) -> Result<DeleteOutcome, String> {
    let path_s = path.to_string_lossy().into_owned();
    check_path_safety(&path_s)?;
    if !path_s.ends_with(".desktop") {
        return Err(format!("只删除 .desktop 文件：{path_s}"));
    }
    if !path.exists() {
        return Err(format!("文件已不存在：{path_s}"));
    }
    let bak = backup_path_of(path);
    let elevated = !can_write_direct(&path_s);
    if elevated {
        let argv = unit_write_argv(&delete_script(path));
        let out = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("无法执行删除：{e}"))?;
        if !out.status.success() {
            return Err(format!(
                "删除失败：{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    } else if let Err(e) = std::fs::rename(path, &bak) {
        return Err(format!("mv 备份失败：{e}"));
    }
    // 回读断言
    if path.exists() {
        return Err(format!("删除未生效（{} 仍在）", path.display()));
    }
    if !bak.exists() {
        return Err(format!("备份未生成：{}", bak.display()));
    }
    Ok(DeleteOutcome {
        backup: bak,
        elevated,
    })
}

// ---------------------------------------------------------------------------
// desktop-file-validate（D12.7-③）
// ---------------------------------------------------------------------------

/// 校验消息级别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VLevel {
    Error,
    Warning,
}

/// 一条校验消息（逐行解析产出）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VMessage {
    pub level: VLevel,
    /// 去掉临时文件路径前缀后的原文。
    pub text: String,
}

/// 校验结果（`degraded` = 工具缺失走降级，D12.7-③ 不假装跑过）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidateOutcome {
    /// 无 Error 级消息（`degraded` 时恒 true——仅自家校验兜底）。
    pub ok: bool,
    pub messages: Vec<VMessage>,
    /// `desktop-file-validate` 不存在 → 降级（UI 标注）。
    pub degraded: bool,
}

/// 解析 `desktop-file-validate` 输出（纯函数）。
///
/// 行格式（摸底 6 实测）：`<path>: error: <msg>` / `<path>: warning: <msg>`；
/// 路径前缀剥掉后保留原文。不带级别前缀的行忽略。
pub fn parse_validate_output(output: &str, file_path: &str) -> Vec<VMessage> {
    let mut out = Vec::new();
    for line in output.lines() {
        let line = line.trim_end();
        let stripped = match line.strip_prefix(&format!("{file_path}: ")) {
            Some(rest) => rest.to_string(),
            None => line.to_string(),
        };
        if let Some(msg) = stripped.strip_prefix("error: ") {
            out.push(VMessage {
                level: VLevel::Error,
                text: msg.to_string(),
            });
        } else if let Some(msg) = stripped.strip_prefix("warning: ") {
            out.push(VMessage {
                level: VLevel::Warning,
                text: msg.to_string(),
            });
        }
    }
    out
}

/// 跑 `desktop-file-validate`（临时副本文件名 = `<id>`）。
///
/// - 工具不存在 → `degraded: true, ok: true`（降级为仅自家校验，UI 标注）；
/// - RC≠0 或有 Error 消息 → `ok: false`（消息逐行给用户）。
pub fn run_desktop_file_validate(id: &str, content: &str) -> Result<ValidateOutcome, String> {
    validate_desktop_id(id)?;
    // 目录必须每次调用唯一：同进程并发调用会互相 remove_dir_all 竞态（本批实测踩过）
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("linbox-dv-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("建临时目录失败：{e}"))?;
    let file = dir.join(id);
    std::fs::write(&file, content).map_err(|e| format!("写临时副本失败：{e}"))?;
    let file_s = file.to_string_lossy().into_owned();
    let out = Command::new("desktop-file-validate")
        .arg(&file_s)
        .stdin(Stdio::null())
        .output();
    let _ = std::fs::remove_dir_all(&dir);
    let out = match out {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ValidateOutcome {
                ok: true,
                messages: vec![],
                degraded: true,
            });
        }
        Err(e) => return Err(format!("无法执行 desktop-file-validate：{e}")),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let messages = parse_validate_output(&format!("{stdout}{stderr}"), &file_s);
    let ok = out.status.success() && !messages.iter().any(|m| m.level == VLevel::Error);
    Ok(ValidateOutcome {
        ok,
        messages,
        degraded: false,
    })
}

// ---------------------------------------------------------------------------
// update-desktop-database（D12.11：存在才跑、失败仅记录）
// ---------------------------------------------------------------------------

/// 刷新菜单缓存。返回 `Some(rc)`（工具跑了）、`None`（工具不存在，未跑）。
///
/// 失败**只记录不回滚**（D12.7-④ 括注）；输出原样返回给页面 toast。
pub fn update_desktop_database(dir: &Path) -> Option<(i32, String)> {
    if !dir.is_dir() {
        return None;
    }
    let out = Command::new("update-desktop-database")
        .arg(dir)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Some((out.status.code().unwrap_or(-1), text))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时沙箱目录（每测试独立子目录，结束清理）。
    fn sandbox(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "linbox-df-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, id: &str, content: &str) -> PathBuf {
        let p = dir.join(id);
        std::fs::write(&p, content).unwrap();
        p
    }

    // ── 优先级合并真值表 5 场景 ────────────────────────────────────────────

    /// 场景 1：无覆盖（只有一层有）。
    #[test]
    fn merge_single_layer() {
        let (eff, over) =
            merge_priority(vec![(2, "/usr/share/a.desktop".into(), "a.desktop".into())]);
        assert_eq!(eff, PathBuf::from("/usr/share/a.desktop"));
        assert!(over.is_empty());
    }

    /// 场景 2：用户盖系统（摸底 2 机制——序号小者生效）。
    #[test]
    fn merge_user_overrides_system() {
        let (eff, over) = merge_priority(vec![
            (2, "/usr/share/a.desktop".into(), "a.desktop".into()),
            (
                0,
                "/home/u/.local/share/applications/a.desktop".into(),
                "a.desktop".into(),
            ),
        ]);
        assert!(eff.starts_with("/home/u"), "生效层=用户：{eff:?}");
        assert_eq!(over, vec![PathBuf::from("/usr/share/a.desktop")]);
    }

    /// 场景 3：三层各一份（菜单全优先级链）。
    #[test]
    fn merge_three_layers() {
        let (eff, over) = merge_priority(vec![
            (2, "/usr/share/a.desktop".into(), "a.desktop".into()),
            (
                0,
                "/home/u/.local/share/a.desktop".into(),
                "a.desktop".into(),
            ),
            (1, "/usr/local/share/a.desktop".into(), "a.desktop".into()),
        ]);
        assert!(eff.starts_with("/home/u"));
        assert_eq!(
            over,
            vec![
                PathBuf::from("/usr/local/share/a.desktop"),
                PathBuf::from("/usr/share/a.desktop")
            ],
            "被覆盖层按优先级升序"
        );
    }

    /// 场景 4：同层重复（同序号两路径 → 后者进被覆盖层；同路径去重）。
    #[test]
    fn merge_same_layer_duplicates() {
        let (eff, over) = merge_priority(vec![
            (1, "/usr/local/share/a.desktop".into(), "a.desktop".into()),
            (1, "/usr/local/share/b-a.desktop".into(), "a.desktop".into()),
        ]);
        assert_eq!(eff, PathBuf::from("/usr/local/share/a.desktop"));
        assert_eq!(over, vec![PathBuf::from("/usr/local/share/b-a.desktop")]);
        // 完全相同路径去重
        let (eff, over) = merge_priority(vec![
            (1, "/x/a.desktop".into(), "a.desktop".into()),
            (1, "/x/a.desktop".into(), "a.desktop".into()),
        ]);
        assert_eq!(eff, PathBuf::from("/x/a.desktop"));
        assert!(over.is_empty());
    }

    /// 场景 5：目录缺失容错（hits_in_dir 对不存在目录返回空 → scan_in 不炸）。
    #[test]
    fn merge_missing_dir_tolerated() {
        let d = sandbox("missing");
        write(
            &d,
            "x.desktop",
            "[Desktop Entry]\nType=Application\nName=x\nExec=y\n",
        );
        let dirs = vec![
            PathBuf::from("/nonexistent/dir/applications"),
            d.clone(),
            PathBuf::from("/usr/share/applications"),
        ];
        let items = scan_in(&dirs, DesktopKind::Menu);
        // /usr/share/applications 真实存在 → 会有真实条目；本沙箱的 x.desktop 必须在
        let mine = items
            .iter()
            .find(|i| i.id == "x.desktop")
            .expect("x.desktop");
        assert_eq!(mine.effective, d.join("x.desktop"));
        assert_eq!(mine.source, Source::User);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 摸底 3 真实结构：lbrowser 用户副本盖系统副本（真机，两文件缺失则跳过）。
    #[test]
    fn merge_real_lbrowser_structure() {
        let user =
            dirs::home_dir().map(|h| h.join(".config/autostart/cn.loongnix.lbrowser.desktop"));
        let system = PathBuf::from("/etc/xdg/autostart/cn.loongnix.lbrowser.desktop");
        let (Some(u), true) = (user, system.exists()) else {
            return;
        };
        if !u.exists() {
            return;
        }
        let items = scan_in(&autostart_dirs_probe(), DesktopKind::Autostart);
        let item = items
            .iter()
            .find(|i| i.id == "cn.loongnix.lbrowser.desktop")
            .expect("lbrowser 条目");
        assert_eq!(item.effective, u, "生效层=用户副本（摸底 3）");
        assert_eq!(item.overridden, vec![system.clone()]);
        assert!(item.covered());
        assert!(item.hidden(), "用户副本带 Hidden=true");
        assert_eq!(item.source, Source::User);
    }

    /// 摸底 3 结构的纯函数版（不依赖本机文件）。
    #[test]
    fn merge_lbrowser_structure_pure() {
        let (eff, over) = merge_priority(vec![
            (
                1,
                "/etc/xdg/autostart/cn.loongnix.lbrowser.desktop".into(),
                "x.desktop".into(),
            ),
            (
                0,
                "/home/u/.config/autostart/cn.loongnix.lbrowser.desktop".into(),
                "x.desktop".into(),
            ),
        ]);
        assert!(eff.starts_with("/home/u/.config/autostart"));
        assert_eq!(
            over,
            vec![PathBuf::from(
                "/etc/xdg/autostart/cn.loongnix.lbrowser.desktop"
            )]
        );
    }

    fn autostart_dirs_probe() -> Vec<PathBuf> {
        crate::model::desktopfile::autostart_dirs()
    }

    // ── id 校验 10 形态 ────────────────────────────────────────────────────

    #[test]
    fn id_validation_truth_table() {
        // 合法形态
        for ok in [
            "a.desktop",
            "org.deepin.dde-shell.desktop",
            "userapp-Telegram-Desktop-YXBFU3.desktop",
            "cn.loongnix.lbrowser@2.desktop",
            "a-b_c.d.desktop",
            "A1@2-b_c.d.desktop",
        ] {
            assert!(validate_desktop_id(ok).is_ok(), "{ok}");
        }
        // 非法 10 形态
        for bad in [
            "a/b.desktop",   // 斜杠
            "a b.desktop",   // 空格
            "a..b.desktop",  // ..
            "a.desktop.txt", // 非 .desktop 后缀
            "",              // 空串
            "a\\b.desktop",  // 反斜杠（白名单外字符）
            "a\nb.desktop",  // 换行
            ".desktop",      // 空基名
            "..desktop",     // 基名以 . 开头
            "a?.desktop",    // ? 白名单外
        ] {
            assert!(validate_desktop_id(bad).is_err(), "{bad:?} 应拒");
        }
        assert!(validate_desktop_id(" a.desktop").is_err(), "前导空格应拒");
    }

    // ── validate 输出解析（摸底 6 两条真实原文）─────────────────────────────

    #[test]
    fn parse_validate_output_real_fixtures() {
        // 真实原文 1：缺 Name（2026-10-15 实测抄录）
        let out = "/tmp/x/noname.desktop: error: required key \"Name\" in group \"Desktop Entry\" is not present\n";
        let msgs = parse_validate_output(out, "/tmp/x/noname.desktop");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].level, VLevel::Error);
        assert_eq!(
            msgs[0].text,
            "required key \"Name\" in group \"Desktop Entry\" is not present"
        );
        // 真实原文 2：非法字段码（同文件多条 error）
        let out = "/tmp/x/bad.desktop: error: value \"echo %o %z %f\" for key \"Exec\" in group \"Desktop Entry\" contains an invalid field code \"%o\"\n/tmp/x/bad.desktop: error: value \"echo %o %z %f\" for key \"Exec\" in group \"Desktop Entry\" contains an invalid field code \"%z\"\n";
        let msgs = parse_validate_output(out, "/tmp/x/bad.desktop");
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].text.contains("invalid field code \"%o\""));
        assert!(msgs[1].text.contains("invalid field code \"%z\""));
        // warning 级
        let msgs = parse_validate_output(
            "/tmp/x/w.desktop: warning: file contains line \"junk\"\n",
            "/tmp/x/w.desktop",
        );
        assert_eq!(msgs[0].level, VLevel::Warning);
        // 无级别前缀的行忽略
        assert!(parse_validate_output("random noise\n", "/tmp/x").is_empty());
    }

    /// 真机跑工具：好文件 RC=0 过、坏文件拒（工具在位时）。
    #[test]
    fn run_validate_real_tool() {
        let good = "[Desktop Entry]\nType=Application\nName=x\nExec=/bin/true\n";
        let r = run_desktop_file_validate("linbox-dv-good.desktop", good).unwrap();
        if !r.degraded {
            assert!(r.ok, "{:?}", r.messages);
        }
        let missing_name = "[Desktop Entry]\nType=Application\nExec=echo\n";
        let r = run_desktop_file_validate("linbox-dv-noname.desktop", missing_name).unwrap();
        if !r.degraded {
            assert!(!r.ok);
            assert!(
                r.messages
                    .iter()
                    .any(|m| m.text.contains("required key \"Name\"")),
                "{:?}",
                r.messages
            );
        }
        let bad_code = "[Desktop Entry]\nType=Application\nName=x\nExec=echo %z\n";
        let r = run_desktop_file_validate("linbox-dv-code.desktop", bad_code).unwrap();
        if !r.degraded {
            assert!(!r.ok, "非法字段码必须拒");
        }
    }

    // ── 注入拒绝 ───────────────────────────────────────────────────────────

    #[test]
    fn injection_rejected() {
        // 路径安全
        assert!(check_path_safety("relative/x.desktop").is_err());
        assert!(check_path_safety("/opt/../etc/x.desktop").is_err());
        assert!(check_path_safety("/opt/a\nb.desktop").is_err());
        assert!(check_path_safety("/opt/a\0b.desktop").is_err());
        assert!(check_path_safety("/opt/a'b.desktop").is_err());
        assert!(check_path_safety("/opt/a$b.desktop").is_err());
        assert!(check_path_safety("/opt/ok/x.desktop").is_ok());
        // 删除链路的路径闸门
        assert!(delete_desktop(Path::new("/tmp/../etc/x.desktop")).is_err());
        assert!(
            delete_desktop(Path::new("/tmp/x.txt")).is_err(),
            "非 .desktop 拒"
        );
        assert!(delete_desktop(Path::new("/tmp/nonexistent-x.desktop")).is_err());
        // 写入链路
        assert!(write_desktop(Path::new("rel.desktop"), "[Desktop Entry]\n").is_err());
        assert!(write_desktop(Path::new("/tmp/../x.desktop"), "[Desktop Entry]\n").is_err());
    }

    // ── 回滚：注入 reload=||1 ──────────────────────────────────────────────

    #[test]
    fn write_rollback_on_reload_failure() {
        let d = sandbox("rollback");
        let target = d.join("r.desktop");
        let old = "[Desktop Entry]\nType=Application\nName=old\nExec=/bin/old\n";
        std::fs::write(&target, old).unwrap();
        let new = "[Desktop Entry]\nType=Application\nName=new\nExec=/bin/new\n";
        let e = write_desktop_with(&target, new, || 1).unwrap_err();
        assert!(e.contains("回滚"), "{e}");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            old,
            "旧内容必须恢复"
        );
        let bak = backup_path_of(&target);
        assert!(bak.exists(), "备份必须在");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 原先不存在的目标 + reload 失败 → 误写文件被清掉（回滚的另一半）。
    #[test]
    fn write_rollback_removes_new_file() {
        let d = sandbox("rollback-new");
        let target = d.join("n.desktop");
        let new = "[Desktop Entry]\nType=Application\nName=n\nExec=/bin/n\n";
        let e = write_desktop_with(&target, new, || 1).unwrap_err();
        assert!(e.contains("回滚"), "{e}");
        assert!(!target.exists(), "原先不存在 → 误写文件应删掉");
        let _ = std::fs::remove_dir_all(&d);
    }

    // ── 用户层免提权分支 + dry-run 三证 ────────────────────────────────────

    /// /tmp dry-run 三证（GOAL 12.3）：备份=旧内容、回读 diff 空、validate RC=0。
    #[test]
    fn write_dry_run_three_proofs() {
        let d = sandbox("dry");
        let target = d.join("linbox-dry.desktop");
        let old = "[Desktop Entry]\nType=Application\nName=old\nExec=/bin/true\n";
        std::fs::write(&target, old).unwrap();
        let new =
            "[Desktop Entry]\nType=Application\nName=dry\nExec=/bin/true\nCategories=Utility;\n";
        let out = write_desktop(&target, new).unwrap();
        // 证 1：备份 = 旧内容
        let bak = out.backup.expect("应有备份");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), old);
        // 证 2：回读 diff 空（逐字节一致由写入链断言，这里再独立证一次）
        assert_eq!(std::fs::read_to_string(&target).unwrap(), new);
        // 证 3：validate RC=0（或降级）
        let v = run_desktop_file_validate("linbox-dry.desktop", new).unwrap();
        assert!(v.ok || v.degraded, "{:?}", v.messages);
        assert!(!out.elevated, "/tmp 属可直写目录，零提权");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 用户层免提权分支：可写目录 → elevated=false（且真走了直写）。
    #[test]
    fn write_user_layer_no_elevation() {
        let d = sandbox("user");
        let target = d.join("u.desktop");
        let out = write_desktop(
            &target,
            "[Desktop Entry]\nType=Application\nName=u\nExec=/bin/u\n",
        )
        .unwrap();
        assert!(!out.elevated);
        assert!(out.backup.is_none(), "新建无备份");
        assert!(target.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    // ── 自家往返自检 ┽ 坏内容连备份都不做 ─────────────────────────────────

    #[test]
    fn write_rejects_bad_content_before_backup() {
        let d = sandbox("bad");
        let target = d.join("b.desktop");
        let old = "[Desktop Entry]\nName=old\n";
        std::fs::write(&target, old).unwrap();
        // 无尾换行拒
        assert!(write_desktop(&target, "[Desktop Entry]\nName=x").is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), old, "原文件未动");
        assert!(!backup_path_of(&target).exists(), "连备份都不该有");
        let _ = std::fs::remove_dir_all(&d);
    }

    // ── hide / set_hidden（D12.3 语义）────────────────────────────────────

    #[test]
    fn hide_writes_hidden_true_and_gnome_sync() {
        let d = sandbox("hide");
        let p = write(
            &d,
            "h.desktop",
            "[Desktop Entry]\nType=Application\nName=h\nExec=/bin/h\nX-GNOME-Autostart-enabled=true\n",
        );
        hide_desktop(&p).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("Hidden=true"), "{s}");
        assert!(
            s.contains("X-GNOME-Autostart-enabled=false"),
            "已有 GNOME 开关要同步：{s}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn set_hidden_false_and_gnome_exists_sync() {
        let d = sandbox("unhide");
        let p = write(
            &d,
            "h.desktop",
            "[Desktop Entry]\nType=Application\nName=h\nExec=/bin/h\nHidden=true\n",
        );
        set_hidden(&p, false).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("Hidden=false"), "{s}");
        // 无 GNOME 键 → 不无谓新建
        assert!(!s.contains("X-GNOME-Autostart-enabled"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn set_hidden_absent_key_no_write() {
        let d = sandbox("unhide-absent");
        let p = write(
            &d,
            "h.desktop",
            "[Desktop Entry]\nType=Application\nName=h\nExec=/bin/h\n",
        );
        let before = std::fs::read_to_string(&p).unwrap();
        let out = set_hidden(&p, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "缺省即启用，不该动文件"
        );
        assert!(out.backup.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    // ── delete（D12.6-②）──────────────────────────────────────────────────

    #[test]
    fn delete_script_shape_no_glob_no_rm() {
        let s = delete_script(Path::new("/opt/a.desktop"));
        assert_eq!(s, "mv -f '/opt/a.desktop' '/opt/a.desktop.linbox.bak'\n");
        assert!(!s.contains('*'), "绝不 glob：{s}");
        assert!(!s.contains("rm "), "绝不 rm：{s}");
    }

    #[test]
    fn delete_real_dry_run() {
        let d = sandbox("del");
        let p = write(
            &d,
            "d.desktop",
            "[Desktop Entry]\nType=Application\nName=d\nExec=/bin/d\n",
        );
        let out = delete_desktop(&p).unwrap();
        assert!(!p.exists(), "原文件应已移走");
        assert!(out.backup.exists(), ".linbox.bak 应在");
        assert_eq!(
            std::fs::read_to_string(&out.backup).unwrap(),
            "[Desktop Entry]\nType=Application\nName=d\nExec=/bin/d\n",
            "备份=原内容"
        );
        assert!(!out.elevated, "/tmp 直写零提权");
        // 重复删 → 文件已不存在
        assert!(delete_desktop(&p).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    // ── create_desktop（D12.5-③）──────────────────────────────────────────

    #[test]
    fn create_dir_for_scopes() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            create_dir_for(DesktopKind::Menu, Scope::User).unwrap(),
            home.join(".local/share/applications")
        );
        assert_eq!(
            create_dir_for(DesktopKind::Menu, Scope::System).unwrap(),
            PathBuf::from("/usr/local/share/applications")
        );
        assert_eq!(
            create_dir_for(DesktopKind::Autostart, Scope::User).unwrap(),
            home.join(".config/autostart")
        );
        assert_eq!(
            create_dir_for(DesktopKind::Autostart, Scope::System).unwrap(),
            PathBuf::from("/etc/xdg/autostart")
        );
    }

    /// 同名拒绝（12.7-⑥：对已存在的 id 新建 → 拒并提示改用编辑）。
    /// 用沙箱目录注入做纯逻辑验证：借用 scan_in 不便——直接验冲突文案来源。
    #[test]
    fn create_rejects_existing_id() {
        // 本机真实存在的 id（菜单侧任取一个系统文件名）
        let sys = PathBuf::from("/usr/share/applications");
        let Ok(rd) = std::fs::read_dir(&sys) else {
            return;
        };
        let Some(existing) = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|n| n.ends_with(".desktop"))
        else {
            return;
        };
        let e = create_desktop(DesktopKind::Menu, &existing, Scope::User).unwrap_err();
        assert!(e.contains("已存在") && e.contains("编辑"), "{e}");
        // 非法 id 先于冲突被拒
        assert!(create_desktop(DesktopKind::Menu, "bad id.desktop", Scope::User).is_err());
        // 干净 id（不与现有任何文件冲突的随机名）→ Ok
        let fresh = format!("linbox-unique-test-{}.desktop", std::process::id());
        let p = create_desktop(DesktopKind::Menu, &fresh, Scope::User).unwrap();
        assert!(p.ends_with(&fresh));
        assert!(p.starts_with(dirs::home_dir().unwrap()));
    }

    // ── read_effective ────────────────────────────────────────────────────

    #[test]
    fn read_effective_real_menu_file() {
        let p = PathBuf::from("/usr/share/applications");
        let Ok(rd) = std::fs::read_dir(&p) else {
            return;
        };
        let Some(id) = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|n| n.ends_with(".desktop"))
        else {
            return;
        };
        let (path, content) = read_effective(&id, DesktopKind::Menu).unwrap();
        assert_eq!(path, p.join(&id));
        assert!(content.contains("[Desktop Entry]"));
        assert!(read_effective("nope-not-here.desktop", DesktopKind::Menu).is_err());
        assert!(
            read_effective("../etc/x.desktop", DesktopKind::Menu).is_err(),
            "注入拒"
        );
    }

    // ── source 判定 ───────────────────────────────────────────────────────

    #[test]
    fn source_of_paths() {
        assert_eq!(
            source_of(Path::new("/usr/share/applications/x.desktop")),
            Source::System
        );
        assert_eq!(
            source_of(Path::new("/usr/local/share/applications/x.desktop")),
            Source::Local
        );
        assert_eq!(
            source_of(Path::new("/home/u/.config/autostart/x.desktop")),
            Source::User
        );
        assert_eq!(
            source_of(Path::new("/etc/xdg/autostart/x.desktop")),
            Source::System
        );
        assert_eq!(Source::Local.label(), "本地");
    }

    /// DesktopItem 展示辅助（D12.9 徽章语义）。
    #[test]
    fn item_display_helpers() {
        let d = sandbox("item");
        write(
            &d,
            "i.desktop",
            "[Desktop Entry]\nType=Application\nName=Item\nComment=C\nExec=/bin/i\nNoDisplay=true\n",
        );
        let items = scan_in(std::slice::from_ref(&d), DesktopKind::Menu);
        let it = items.iter().find(|i| i.id == "i.desktop").unwrap();
        assert_eq!(it.display_name(), "Item");
        assert_eq!(it.comment().as_deref(), Some("C"));
        assert_eq!(it.exec().as_deref(), Some("/bin/i"));
        assert!(it.no_display());
        assert!(!it.hidden());
        assert!(!it.covered());
        // 坏文件 → doc None，显示名回落 id 去后缀
        write(&d, "broken.desktop", "garbage without group\n");
        let items = scan_in(std::slice::from_ref(&d), DesktopKind::Menu);
        let it = items.iter().find(|i| i.id == "broken.desktop").unwrap();
        assert!(it.doc.is_none() || it.doc.as_ref().unwrap().entries().count() == 0);
        assert_eq!(it.display_name(), "broken");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// update_desktop_database：目录不存在 → None（不跑不炸）。
    #[test]
    fn update_database_missing_dir() {
        assert!(update_desktop_database(Path::new("/nonexistent/xdg/apps")).is_none());
    }

    /// 沙箱目录也过 update（工具在位才 Some；失败只记录不回滚）。
    #[test]
    fn update_database_real_sandbox() {
        let d = sandbox("db");
        // 工具对任意目录都可能返回 0（本机实测对默认目录 RC=0）——这里只要求不 panic
        let _ = update_desktop_database(&d);
        let _ = std::fs::remove_dir_all(&d);
    }
}
