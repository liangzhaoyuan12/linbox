//! 垃圾清理扫描引擎（GOAL Phase 9 / 任务 9.2 + 9.3）。
//!
//! 纯逻辑：catalog 查询、目录遍历、模式匹配、大小聚合、挂载点解析。
//! 架构约束（GOAL Phase 9）：**本文件禁止 `use gtk` / `use adw` / `use glib`**。
//!
//! 安全规则（GOAL 硬规则 / D9.1 / D9.5 / 9.7）：
//! - **扫描阶段绝不删除任何文件**——扫描路径只有读操作（`read_dir`/`metadata`），
//!   `remove_file`/`fs::remove` 在全文件零命中；`Command::new` 只出现在**执行段**
//!   （9.5 的 `run_cmd`/`execute_cleanup*`，由页面在用户点「开始清理」并过二次确认后
//!   的后台线程调用），扫描函数 `quick_scan`/`deep_scan*`/`scan_*` 一律不触碰；
//! - 不跟随符号链接（防环、防把链接目标体积算两遍）；
//! - 不跨挂载点边界（子目录命中挂载点集合即跳过，深度扫描时该挂载点有自己的根）；
//! - 跳过伪文件系统 / tmpfs（9.3 `should_skip_mount`），无权限目录只计数不 panic；
//! - D9.5 红线（`~/Documents` 等六目录与含 `.git` 的源码仓库子树）整体跳过。

use crate::model::cleaner::{Category, CleanTarget, RED_LINE_DIRS, Risk, ScanEntry, by_id};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

/// D9.7：`/tmp` `/var/tmp` 默认只清「超过 7 天未修改」的文件。
pub const DEFAULT_TMP_AGE_DAYS: u32 = 7;

/// 进度回调签名：`(当前目录, 累计文件数)`。
pub type ProgressFn = dyn Fn(&str, u64) + Send + Sync;

/// 目标级进度回调签名：`(已完成条目, catalog 总条目)`，由 [`quick_scan`] 逐条触发
/// —— 9.6 页面据此画百分比（目录级 [`ProgressFn`] 没有总量，算不出百分比）。
pub type TargetProgressFn = dyn Fn(usize, usize) + Send + Sync;

/// 扫描参数（UI 层与测试共用；字段全 pub，测试直接构造注入）。
#[derive(Clone)]
pub struct ScanOptions {
    /// 主目录（`~` 展开与 D9.5 红线判定；测试注入假主目录）。
    pub home: PathBuf,
    /// 当前用户 uid（`$UID` 展开）。
    pub uid: u32,
    /// 目录深度上限（超过即停止下钻并计数，防异常深树）。
    pub max_depth: u32,
    /// 只统计 mtime 早于 `now - min_age` 的文件（D9.7：`/tmp` `/var/tmp` 默认 7 天）。
    pub min_age: Option<Duration>,
    /// 取消标记：每扫描 32 个目录检查一次，置位后返回**已扫部分**并标 `cancelled`。
    pub cancel: Arc<AtomicBool>,
    /// 进度回调（当前目录, 累计文件数），由页面层经 `idle_add` 回 UI。
    pub progress: Option<Arc<ProgressFn>>,
    /// 目标级进度（已完成, 总数），`quick_scan` 每处理一个 catalog 条目前触发一次。
    pub target_progress: Option<Arc<TargetProgressFn>>,
    /// 挂载点集合（`None` = 现场读 `/proc/self/mountinfo`；测试注入合成集合）。
    pub mount_points: Option<Arc<HashSet<PathBuf>>>,
    /// D9.7：临时目录（`/tmp` `/var/tmp`）只计超过 N 天未修改的文件（默认 7，页面可调 1-90）。
    pub tmp_min_age_days: u32,
}

impl ScanOptions {
    /// 真实运行时的默认参数（HOME 来自环境；uid 读 `/proc/self/status`）。
    pub fn new() -> Self {
        ScanOptions {
            home: PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string())),
            uid: current_uid(),
            max_depth: 64,
            min_age: None,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: None,
            target_progress: None,
            mount_points: None,
            tmp_min_age_days: DEFAULT_TMP_AGE_DAYS,
        }
    }

    /// 展开 `~` 与 `$UID`（catalog 静态路径 → 真实路径）。
    pub fn expand(&self, raw: &str) -> String {
        expand_path(raw, &self.home, self.uid)
    }
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// 当前用户 uid：读 `/proc/self/status` 的 `Uid:` 行（真实 uid 列），失败回退环境变量 `UID`。
pub fn current_uid() -> u32 {
    if let Ok(text) = std::fs::read_to_string("/proc/self/status") {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("Uid:")
                && let Some(real) = rest.split_whitespace().next()
                && let Ok(uid) = real.parse::<u32>()
            {
                return uid;
            }
        }
    }
    std::env::var("UID")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// 展开路径里的 `~` / `~/` 与 `$UID`（纯函数，测试可注入 home 与 uid）。
///
/// - `~` 单独或 `~/…` 开头 → 替换为 `home`；
/// - `$UID` → 替换为 uid 的十进制串。
pub fn expand_path(raw: &str, home: &Path, uid: u32) -> String {
    let mut out = if raw == "~" {
        home.to_string_lossy().into_owned()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        let h = home.to_string_lossy();
        if h.ends_with('/') {
            format!("{}{}", h, rest)
        } else {
            format!("{}/{}", h, rest)
        }
    } else if let Some(rest) = raw.strip_prefix('/') {
        format!("/{}", rest)
    } else {
        raw.to_string()
    };
    if out.contains("$UID") {
        out = out.replace("$UID", &uid.to_string());
    }
    out
}

/// 单目标扫描结果（GOAL 9.2：字节数 + 文件数 + mtime 说明 + 跳过计数）。
#[derive(Debug, Default, Clone)]
pub struct EntryStat {
    /// 合计字节（只计通过 mtime 阈值的文件）。
    pub bytes: u64,
    /// 文件数。
    pub files: u64,
    /// 目录数（含根）。
    pub dirs: u64,
    /// 无权限跳过的目录数（不 panic，D9.1）。
    pub inaccessible: u64,
    /// 跨挂载点跳过数。
    pub skipped_mounts: u64,
    /// 符号链接跳过数（不跟随）。
    pub skipped_symlinks: u64,
    /// 深度超限跳过的子树数。
    pub skipped_depth: u64,
    /// 命中文件里最旧的 mtime（「mtime 说明」，UI 可显示「最旧 32 天前」）。
    pub oldest_mtime: Option<SystemTime>,
    /// 是否被取消标记中断（已返回扫到的部分）。
    pub cancelled: bool,
}

/// 字节数 → 人类可读串（1024 进制：B / KB / MB / GB / TB，一位小数）。
///
/// 9.4 的边界单测（1023B-1KB-1MB-1GB）背书；整数值不带小数（`1023 B`、`512 B`）。
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut idx = 0;
    while v >= 1024.0 && idx + 1 < UNITS.len() {
        v /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{} B", bytes)
    } else {
        format!("{:.1} {}", v, UNITS[idx])
    }
}

/// 一条模式命中的文件（9.4 聚合、9.6 展示、9.5 删除的最小单位）。
#[derive(Debug, Clone)]
pub struct PatternHit {
    /// 命中文件绝对路径。
    pub path: String,
    /// 命中它的模式串（如 `*.tmp`）。
    pub pattern: String,
    /// 文件字节数。
    pub bytes: u64,
    /// 文件 mtime。
    pub mtime: Option<SystemTime>,
}

/// 整轮扫描报告（GOAL 9.2：无法访问 N 项 + 跳过挂载点）。
#[derive(Debug, Default)]
pub struct ScanReport {
    /// catalog 条目结果（9.3 组装）。
    pub entries: Vec<crate::model::cleaner::ScanEntry>,
    /// 存在性探测未命中的 catalog 条目数（缺失即跳过不报错，D9.1）。
    pub missing: u64,
    /// 全轮无法访问的目录总数。
    pub inaccessible: u64,
    /// 全轮跳过的挂载点/子树总数。
    pub skipped_mounts: u64,
    /// 被取消。
    pub cancelled: bool,
}

/// D9.5 红线判定：`path` 落在主目录下的六个个人数据目录内（任一层）。
pub fn is_red_line(path: &Path, home: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(home) else {
        return false;
    };
    let Some(first) = rel.components().next() else {
        return false;
    };
    let first = first.as_os_str().to_string_lossy();
    RED_LINE_DIRS.iter().any(|red| {
        // red 形如 `~/Documents` → 取第一段目录名比较。
        red.trim_start_matches("~/")
            .split('/')
            .next()
            .is_some_and(|name| name == first)
    })
}

/// 简单通配匹配（只支持 `*` 与 `?`，按字节回溯算法，无正则依赖）。
///
/// 用于：模式扫描的文件名匹配（`*.tmp` / `*~` / `core.*`）与 catalog 通配路径展开。
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p = pattern.as_bytes();
    let n = name.as_bytes();
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star_pi, mut star_ni) = (usize::MAX, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_pi = pi;
            star_ni = ni;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ni += 1;
            ni = star_ni;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// 展开含通配符的**路径**（如 `/boot/vmlinuz-*`、`~/.mozilla/firefox/*.default*/cache2/`）。
///
/// 逐段下钻：字面段直接拼接，含 `*`/`?` 的段用 `read_dir` + `glob_match` 展开。
/// 只返回真实存在的路径；展开失败返回空。
pub fn expand_glob(expanded: &str) -> Vec<PathBuf> {
    let segs: Vec<&str> = expanded
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let mut cur: Vec<PathBuf> = if expanded.starts_with('/') {
        vec![PathBuf::from("/")]
    } else if let Ok(cwd) = std::env::current_dir() {
        // 相对路径按当前工作目录起点处理（catalog 全是绝对或 ~ 展开后的路径）。
        vec![cwd]
    } else {
        Vec::new()
    };
    for seg in segs {
        if seg.contains('*') || seg.contains('?') {
            let mut next = Vec::new();
            for base in &cur {
                let Ok(rd) = std::fs::read_dir(base) else {
                    continue;
                };
                for entry in rd.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if glob_match(seg, &name) {
                        next.push(entry.path());
                    }
                }
            }
            cur = next;
        } else {
            for base in cur.iter_mut() {
                base.push(seg);
            }
        }
        if cur.is_empty() {
            return Vec::new();
        }
    }
    cur.retain(|p| p.symlink_metadata().is_ok() || p.metadata().is_ok());
    cur
}

/// 读取挂载点集合（`/proc/self/mountinfo` 解析，9.3 提供解析器）。
fn live_mount_points() -> HashSet<PathBuf> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    parse_mountinfo(&text)
        .into_iter()
        .map(|m| m.mount_point)
        .collect()
}

/// 一条 mountinfo 记录（9.3 解析产物）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountRecord {
    /// 挂载点（已解转义）。
    pub mount_point: PathBuf,
    /// 挂载根（btrfs 子卷 / bind 源，字段 4 前的 root 字段）。
    pub root: String,
    /// 文件系统类型（`-` 后第一列）。
    pub fs_type: String,
    /// 来源设备（`/dev/nvme…`、`//server/share`、`host:path`）。
    pub source: String,
}

/// 解析 `/proc/self/mountinfo` 全文（9.3 fixture 测试目标）。
///
/// 行格式：`id parent dev root mount_point opts [opt…] - fs_type source super_opts`。
/// 字段内空白以 `\040` 形式转义，按空白切词是安全的；坏行跳过不 panic。
pub fn parse_mountinfo(text: &str) -> Vec<MountRecord> {
    text.lines().filter_map(parse_mountinfo_line).collect()
}

/// 解析单行 mountinfo；格式不合法返回 `None`。
pub fn parse_mountinfo_line(line: &str) -> Option<MountRecord> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    if toks.len() < 10 {
        return None;
    }
    let sep = toks.iter().position(|t| *t == "-")?;
    if sep < 6 || sep + 2 >= toks.len() {
        return None;
    }
    Some(MountRecord {
        mount_point: PathBuf::from(unescape_mount_field(toks[4])),
        root: unescape_mount_field(toks[3]),
        fs_type: toks[sep + 1].to_string(),
        source: unescape_mount_field(toks[sep + 2]),
    })
}

/// mountinfo 字段转义还原：`\040` 空格、`\011` 制表、`\012` 换行、`\134` 反斜杠。
fn unescape_mount_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let esc: String = chars.by_ref().take(3).collect();
            match esc.as_str() {
                "040" => out.push(' '),
                "011" => out.push('\t'),
                "012" => out.push('\n'),
                "134" => out.push('\\'),
                _ => {
                    out.push('\\');
                    out.push_str(&esc);
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 伪文件系统集合（D9.1：跳过，不进扫描也不进可勾选列表）。
pub const PSEUDO_FS: &[&str] = &[
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "tmpfs",
    "cgroup",
    "cgroup2",
    "securityfs",
    "debugfs",
    "tracefs",
    "pstore",
    "bpf",
    "mqueue",
    "hugetlbfs",
    "configfs",
    "fusectl",
    "ramfs",
    "autofs",
    "binfmt_misc",
    "efivarfs",
    "fuse.gvfsd-fuse",
];

/// 网络文件系统集合（D9.7：默认不扫、灰显可勾）。
pub const NETWORK_FS: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smbfs",
    "smb3",
    "sshfs",
    "fuse.sshfs",
    "9p",
    "afp",
    "davfs",
    "ceph",
];

/// 判定一个挂载点是否应默认跳过；返回 `Some(中文原因)` 表示跳过（D9.1 / D9.7）。
///
/// 调用方若拿到 `Some("网络挂载…")` 且用户手动勾选（D9.7 灰显可勾），可以覆盖决定；
/// 伪文件系统与 `/boot/efi` 的判定与 9.3 的挂载点分组共用。
pub fn should_skip_mount(m: &MountRecord) -> Option<&'static str> {
    let mp = m.mount_point.to_string_lossy();
    if PSEUDO_FS.contains(&m.fs_type.as_str()) {
        return Some("伪文件系统（D9.1 跳过）");
    }
    if mp == "/boot/efi" || mp == "/efi" {
        return Some("EFI 系统分区（D9.7 默认不扫）");
    }
    if NETWORK_FS.contains(&m.fs_type.as_str()) {
        return Some("网络挂载（D9.7 默认不扫，可手动勾选）");
    }
    // UNC 形态 `//server/share` 或 `host:path`（NFS/CIFS 变体）。
    if m.source.starts_with("//") || (m.source.contains(':') && !m.source.starts_with('/')) {
        return Some("网络挂载（D9.7 默认不扫，可手动勾选）");
    }
    // 伪路径兜底（个别内核把 tmpfs 挂在 /proc 下却报别的 fs_type）。
    for prefix in ["/proc", "/sys", "/dev/pts", "/run"] {
        if (mp == prefix || mp.starts_with(&format!("{}/", prefix)))
            && (prefix != "/dev/pts" || m.fs_type == "devpts")
        {
            return Some("伪文件系统（D9.1 跳过）");
        }
    }
    None
}

/// 枚举真实挂载点并按默认规则分成「可扫」与「默认跳过」两组（9.3）。
///
/// `enable_nondefault` 为 `true` 时，原本因网络挂载/EFI 跳过的条目也进可扫组
/// （D9.7 灰显可勾）；伪文件系统永不进可扫组。
pub fn classify_mounts(
    text: &str,
    enable_nondefault: bool,
) -> (Vec<MountRecord>, Vec<(MountRecord, &'static str)>) {
    let mut scannable = Vec::new();
    let mut skipped = Vec::new();
    for m in parse_mountinfo(text) {
        match should_skip_mount(&m) {
            None => scannable.push(m),
            Some(reason) if enable_nondefault && reason.contains("可手动勾选") => {
                scannable.push(m)
            }
            Some(reason) => skipped.push((m, reason)),
        }
    }
    (scannable, skipped)
}

/// 遍历模式：求和（catalog 扫描）或收集模式命中（深度扫描）。
enum Visitor<'a> {
    Sum,
    Collect {
        patterns: &'a [&'a str],
        hits: &'a mut Vec<PatternHit>,
    },
}

/// 年龄是否**严格超过**阈值（D9.7 / GOAL 9.4：**恰好等于阈值不计入**）。
pub fn exceeds_min_age(age: Duration, threshold: Duration) -> bool {
    age > threshold
}

/// 单文件是否计入：通过 `min_age` 阈值（`None` 恒通过；判定走
/// [`exceeds_min_age`]，边界口径与 9.4 单测同源）。
fn passes_age(mtime: Option<SystemTime>, min_age: Option<Duration>) -> bool {
    match (mtime, min_age) {
        (Some(mt), Some(age)) => SystemTime::now()
            .duration_since(mt)
            .map(|d| exceeds_min_age(d, age))
            .unwrap_or(false),
        _ => true,
    }
}

fn file_mtime(meta: &std::fs::Metadata) -> Option<SystemTime> {
    meta.modified().ok()
}

/// 递归遍历目录（GOAL 9.2 跳过规则的唯一实现点）。
fn walk_dir(
    dir: &Path,
    depth: u32,
    opts: &ScanOptions,
    mounts: &HashSet<PathBuf>,
    st: &mut EntryStat,
    visitor: &mut Visitor<'_>,
) {
    st.dirs += 1;
    // 取消检查：每 32 个目录一次（GOAL 9.2「每 N 个目录检查一次」）。
    if st.dirs % 32 == 1 && opts.cancel.load(Ordering::Relaxed) {
        st.cancelled = true;
        return;
    }
    if depth > opts.max_depth {
        st.skipped_depth += 1;
        return;
    }
    if let Some(cb) = &opts.progress {
        cb(&dir.to_string_lossy(), st.files);
    }

    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                st.inaccessible += 1;
            }
            // 其余错误（ENOENT 竞态等）静默跳过，不 panic（D9.1）。
            return;
        }
    };
    let entries: Vec<std::fs::DirEntry> = rd.flatten().collect();

    // D9.5：含 `.git` 的目录 = 源码仓库，整棵子树不进扫描结果。
    if entries.iter().any(|e| e.file_name() == ".git") {
        return;
    }

    for entry in entries {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            // 不跟随符号链接（防环、防重复计量）。
            st.skipped_symlinks += 1;
            continue;
        }
        if ft.is_dir() {
            // D9.5 红线目录整体跳过。
            if is_red_line(&path, &opts.home) {
                continue;
            }
            // 不跨挂载点边界（该挂载点在深度扫描里有自己的根）。
            if mounts.contains(&path) {
                st.skipped_mounts += 1;
                continue;
            }
            walk_dir(&path, depth + 1, opts, mounts, st, visitor);
            if st.cancelled {
                return;
            }
            continue;
        }
        // 普通文件。
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let mtime = file_mtime(&meta);
        match visitor {
            Visitor::Sum => {
                if passes_age(mtime, opts.min_age) {
                    st.bytes += meta.len();
                    st.files += 1;
                    if st.oldest_mtime.is_none()
                        || mtime.is_some_and(|m| st.oldest_mtime.is_some_and(|o| m < o))
                    {
                        st.oldest_mtime = mtime;
                    }
                }
            }
            Visitor::Collect { patterns, hits } => {
                if !passes_age(mtime, opts.min_age) {
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Some(pat) = patterns.iter().find(|p| glob_match(p, &name)) {
                    hits.push(PatternHit {
                        path: path.to_string_lossy().into_owned(),
                        pattern: pat.to_string(),
                        bytes: meta.len(),
                        mtime,
                    });
                }
            }
        }
    }
}

/// 对一个已展开的真实路径做求和扫描（存在性由调用方保证）。
fn scan_dir_at_inner(path: &Path, opts: &ScanOptions, mounts: &HashSet<PathBuf>) -> EntryStat {
    let mut st = EntryStat::default();
    let mut visitor = Visitor::Sum;
    walk_dir(path, 0, opts, mounts, &mut st, &mut visitor);
    st
}

/// 按真实路径扫描（目录或单文件）；不存在返回 `None`（GOAL 9.2「不存在路径返回 None」）。
///
/// `path` 应已展开 `~`/`$UID`；通配路径用 [`scan_target`]。
pub fn scan_dir_at(path: &Path, opts: &ScanOptions) -> Option<EntryStat> {
    let meta = path.metadata().ok()?;
    let mounts = match &opts.mount_points {
        Some(set) => (**set).clone(),
        None => live_mount_points(),
    };
    if meta.is_file() {
        let mut st = EntryStat::default();
        if passes_age(file_mtime(&meta), opts.min_age) {
            st.bytes = meta.len();
            st.files = 1;
            st.oldest_mtime = file_mtime(&meta);
        }
        return Some(st);
    }
    Some(scan_dir_at_inner(path, opts, &mounts))
}

/// GOAL 9.2 主入口：存在性探测（含 `~`/`$UID`/通配展开）→ 递归求和。
///
/// - 路径不存在 / 通配无命中 → `None`（缺失即跳过不报错）；
/// - 通配路径命中多个 → 各命中求和（文件直接计、目录递归）。
pub fn scan_target(target: &CleanTarget, opts: &ScanOptions) -> Option<EntryStat> {
    let expanded = opts.expand(target.path);
    let mounts = match &opts.mount_points {
        Some(set) => (**set).clone(),
        None => live_mount_points(),
    };
    if expanded.contains('*') || expanded.contains('?') {
        let matches = expand_glob(&expanded);
        if matches.is_empty() {
            return None;
        }
        let mut total = EntryStat::default();
        for m in &matches {
            let mut st = if m.is_dir() {
                scan_dir_at_inner(m, opts, &mounts)
            } else {
                let mut s = EntryStat::default();
                if let Ok(meta) = m.metadata()
                    && passes_age(file_mtime(&meta), opts.min_age)
                {
                    s.bytes = meta.len();
                    s.files = 1;
                    s.oldest_mtime = file_mtime(&meta);
                }
                s
            };
            total.bytes += st.bytes;
            total.files += st.files;
            total.dirs += st.dirs;
            total.inaccessible += st.inaccessible;
            total.skipped_mounts += st.skipped_mounts;
            total.skipped_symlinks += st.skipped_symlinks;
            total.skipped_depth += st.skipped_depth;
            total.cancelled |= st.cancelled;
            if let Some(m) = st.oldest_mtime.take()
                && total.oldest_mtime.is_none_or(|o| m < o)
            {
                total.oldest_mtime = Some(m);
            }
        }
        return Some(total);
    }
    scan_dir_at(Path::new(&expanded), opts)
}

/// GOAL 9.2：对一组根目录跑通用文件模式扫描（深度扫描用，D9.7）。
///
/// 跳过规则与 `scan_target` 完全一致（符号链接 / 挂载边界 / 红线 / `.git` 仓库 /
/// 伪 fs 由根集合保证 / 无权限只计数）；命中结果按文件逐条返回。
/// `patterns` 传 [`crate::model::cleaner::scan_patterns`]（`*.rpmnew` 等「不删」模式不在内）。
pub fn scan_pattern(
    roots: &[PathBuf],
    patterns: &[&str],
    opts: &ScanOptions,
) -> (Vec<PatternHit>, EntryStat) {
    let mounts = match &opts.mount_points {
        Some(set) => (**set).clone(),
        None => live_mount_points(),
    };
    let mut hits = Vec::new();
    let mut st = EntryStat::default();
    for root in roots {
        // 根自身若是伪 fs 路径直接跳过（D9.1 兜底）。
        let root_str = root.to_string_lossy();
        if root_str == "/proc"
            || root_str == "/sys"
            || root_str.starts_with("/proc/")
            || root_str.starts_with("/sys/")
        {
            continue;
        }
        if !root.exists() {
            continue;
        }
        let mut visitor = Visitor::Collect {
            patterns,
            hits: &mut hits,
        };
        walk_dir(root, 0, opts, &mounts, &mut st, &mut visitor);
        if st.cancelled {
            break;
        }
    }
    (hits, st)
}

/// 一个挂载点上的模式命中汇总（9.3「按挂载点分组的可回收汇总」）。
#[derive(Debug, Clone)]
pub struct MountGroup {
    /// 挂载点。
    pub mount: PathBuf,
    /// 该挂载点上命中的可回收字节。
    pub bytes: u64,
    /// 命中文件数。
    pub files: u64,
}

/// 为单个 catalog 条目派生扫描参数（`/tmp` `/var/tmp` 施加 D9.7 阈值）。
fn scan_opts_for(target: &CleanTarget, opts: &ScanOptions) -> ScanOptions {
    let mut o = opts.clone();
    if matches!(target.id, "tmp_dir" | "var_tmp") {
        o.min_age = Some(Duration::from_secs(
            u64::from(opts.tmp_min_age_days) * 86_400,
        ));
    }
    o
}

/// D9.7 快速扫描（默认档，秒级）：只跑 catalog 逐条存在性探测 + 体积统计。
///
/// - `/tmp` `/var/tmp` 只计 `tmp_min_age_days` 天前的文件（D9.7）；
/// - 缺失条目跳过并计入 `report.missing`（不报错，D9.1）；
/// - 随 `opts.cancel` 可中断，已扫部分保留。
pub fn quick_scan(opts: &ScanOptions) -> ScanReport {
    let mut report = ScanReport::default();
    let total = crate::model::cleaner::CATALOG.len();
    for (idx, target) in crate::model::cleaner::CATALOG.iter().enumerate() {
        if opts.cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        if let Some(cb) = &opts.target_progress {
            cb(idx + 1, total);
        }
        let o = scan_opts_for(target, opts);
        match scan_target(target, &o) {
            None => report.missing += 1,
            Some(st) => {
                report.inaccessible += st.inaccessible;
                report.skipped_mounts += st.skipped_mounts;
                report.cancelled |= st.cancelled;
                let path = o.expand(target.path);
                let mut entry = crate::model::cleaner::ScanEntry::from_target(target, path);
                entry.bytes = st.bytes;
                entry.files = st.files;
                report.entries.push(entry);
            }
        }
    }
    report
}

/// D9.7 深度扫描：catalog（快速档内容）+ 对每个可扫挂载点跑通用模式匹配。
///
/// 返回 `(catalog 报告, 模式命中, 可扫挂载点)`；网络挂载与 `/boot/efi` 默认不进
/// 可扫组（`should_skip_mount` 判定，D9.7 灰显可勾的覆盖在 9.6 页面层传入）。
pub fn deep_scan(opts: &ScanOptions) -> (ScanReport, Vec<PatternHit>, Vec<MountRecord>) {
    deep_scan_with(opts, false)
}

/// [`deep_scan`] 的可选覆盖：`enable_nondefault = true` 时把网络挂载（NFS/SMB/SSHFS）
/// 也放进可扫组（**D9.7「灰显可手动勾上」**；伪 fs 与 `/boot/efi` 永不放行）。
pub fn deep_scan_with(
    opts: &ScanOptions,
    enable_nondefault: bool,
) -> (ScanReport, Vec<PatternHit>, Vec<MountRecord>) {
    let mut report = quick_scan(opts);
    let text = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let (scannable, skipped) = classify_mounts(&text, enable_nondefault);
    // 遍历时的挂载边界集合 = **全部**挂载点（含伪 fs），保证 walk 不下钻进它们。
    let all: HashSet<PathBuf> = parse_mountinfo(&text)
        .into_iter()
        .map(|m| m.mount_point)
        .collect();
    let mut o = opts.clone();
    o.mount_points = Some(Arc::new(all));
    let roots: Vec<PathBuf> = scannable.iter().map(|m| m.mount_point.clone()).collect();
    let patterns = crate::model::cleaner::scan_patterns();
    let (hits, st) = scan_pattern(&roots, &patterns, &o);
    report.inaccessible += st.inaccessible;
    report.skipped_mounts += st.skipped_mounts;
    report.cancelled |= st.cancelled;
    // skipped（伪 fs / EFI / 网络挂载）不进扫描根，仅在 9.6 页面层灰显展示。
    let _ = &skipped;
    (report, hits, scannable)
}

/// 按可回收字节降序取前 n 条 catalog 结果（GOAL 9.3「top-N」）。
pub fn top_entries(report: &ScanReport, n: usize) -> Vec<&crate::model::cleaner::ScanEntry> {
    let mut v: Vec<&crate::model::cleaner::ScanEntry> = report.entries.iter().collect();
    v.sort_by_key(|e| std::cmp::Reverse(e.bytes));
    v.truncate(n);
    v
}

/// 按字节数降序取前 n 个模式命中文件（GOAL 9.3「top-N 大文件」）。
pub fn top_files(hits: &[PatternHit], n: usize) -> Vec<&PatternHit> {
    let mut v: Vec<&PatternHit> = hits.iter().collect();
    v.sort_by_key(|h| std::cmp::Reverse(h.bytes));
    v.truncate(n);
    v
}

/// 模式命中按**最长前缀匹配**归属到挂载点，输出按字节降序的分组汇总（GOAL 9.3）。
pub fn group_by_mount(hits: &[PatternHit], mounts: &[MountRecord]) -> Vec<MountGroup> {
    let mut groups: Vec<MountGroup> = mounts
        .iter()
        .map(|m| MountGroup {
            mount: m.mount_point.clone(),
            bytes: 0,
            files: 0,
        })
        .collect();
    for hit in hits {
        // 最长挂载点前缀 = 命中的归属挂载点。
        let mut best: Option<usize> = None;
        for (i, g) in groups.iter().enumerate() {
            let mp = g.mount.to_string_lossy();
            let at_root = mp == "/";
            let matched = if at_root {
                hit.path.starts_with('/')
            } else {
                hit.path.starts_with(mp.as_ref())
                    && (hit.path.len() == mp.len()
                        || hit.path.as_bytes().get(mp.len()) == Some(&b'/')
                        || mp.ends_with('/'))
            };
            if matched && best.is_none_or(|b| mp.len() > groups[b].mount.as_os_str().len()) {
                best = Some(i);
            }
        }
        if let Some(i) = best {
            groups[i].bytes += hit.bytes;
            groups[i].files += 1;
        }
    }
    groups.retain(|g| g.files > 0);
    groups.sort_by_key(|g| std::cmp::Reverse(g.bytes));
    groups
}

// ── 9.4 聚合与勾选（纯函数：分组 / 合计 / 默认勾选 / 父子去重，禁 UI） ──

/// 目录前缀父子判定（D9.9 去重前提）：`parent` 是 `child` 的严格祖先目录。
fn is_parent_of(parent: &str, child: &str) -> bool {
    if parent == child {
        return false;
    }
    let p = if parent.ends_with('/') {
        parent.to_string()
    } else {
        format!("{}/", parent)
    };
    child.starts_with(&p)
}

/// D9.9 父子去重（纯函数，就地修改）：父条目扣除**直接子条目**的体积并标注
/// `contains_children`（UI 显示「已含子项」）。
///
/// 只扣直接子条目（存在链条 A⊃B⊃C 时 A 只扣 B 的原始体积——B 已含 C，
/// 再扣 C 会重复扣减），合计口径因此每个字节只计一次。
pub fn dedup_parent_child(entries: &mut [ScanEntry]) {
    let mut subs: Vec<(usize, u64)> = Vec::new();
    for i in 0..entries.len() {
        let children: Vec<usize> = (0..entries.len())
            .filter(|&j| j != i && is_parent_of(&entries[i].path, &entries[j].path))
            .collect();
        for &j in &children {
            // j 是直接子条目：不存在 k 使 i ⊃ k ⊃ j。
            let direct = !children
                .iter()
                .any(|&k| k != j && is_parent_of(&entries[k].path, &entries[j].path));
            if direct {
                subs.push((i, entries[j].bytes));
            }
        }
    }
    for (i, child_bytes) in subs {
        let e = &mut entries[i];
        e.bytes = e.bytes.saturating_sub(child_bytes);
        e.contains_children = true;
    }
}

/// 默认勾选（D9.2 三档）：**仅【安全】档且非只报项**。
pub fn default_checked(e: &ScanEntry) -> bool {
    e.risk == Risk::Safe && !e.report_only
}

/// 默认勾选集合（页面初始状态）。
pub fn default_selection(entries: &[ScanEntry]) -> HashSet<String> {
    entries
        .iter()
        .filter(|e| default_checked(e))
        .map(|e| e.id.clone())
        .collect()
}

/// 一键「全选安全类」（9.4/9.6）：与默认勾选同一口径——**绝不越权勾按需/谨慎**。
pub fn select_all_safe(entries: &[ScanEntry]) -> HashSet<String> {
    default_selection(entries)
}

/// 勾选后合计（勾选实时更新的口径）：只报项不进合计；调用前应先
/// [`dedup_parent_child`]，否则父子会重复计。
pub fn total_checked(entries: &[ScanEntry], checked: &HashSet<String>) -> u64 {
    entries
        .iter()
        .filter(|e| !e.report_only && checked.contains(&e.id))
        .map(|e| e.bytes)
        .sum()
}

/// 全部可回收合计（去重后、非只报项）。
pub fn total_all(entries: &[ScanEntry]) -> u64 {
    entries
        .iter()
        .filter(|e| !e.report_only)
        .map(|e| e.bytes)
        .sum()
}

/// 按类别分出的一组（9.6 结果树的「类别 → 条目」层）。
#[derive(Debug, Clone)]
pub struct EntryGroup {
    /// 组类别。
    pub category: Category,
    /// 组内条目下标（按体积降序）。
    pub indices: Vec<usize>,
    /// 组合计（**进合计口径**：只报项列在 `indices` 但不计入）。
    pub bytes: u64,
}

/// 按类别分组：组序 = 十大类固定顺序，组内按体积降序。
pub fn group_by_category(entries: &[ScanEntry]) -> Vec<EntryGroup> {
    let mut groups: Vec<EntryGroup> = Category::ALL
        .iter()
        .map(|&category| EntryGroup {
            category,
            indices: Vec::new(),
            bytes: 0,
        })
        .collect();
    for (idx, e) in entries.iter().enumerate() {
        if let Some(g) = groups.iter_mut().find(|g| g.category == e.category) {
            g.indices.push(idx);
            if !e.report_only {
                g.bytes += e.bytes;
            }
        }
    }
    for g in &mut groups {
        g.indices
            .sort_by_key(|&i| std::cmp::Reverse(entries[i].bytes));
    }
    groups.retain(|g| !g.indices.is_empty());
    groups
}

/// 按风险三档分出的一组。
#[derive(Debug, Clone)]
pub struct RiskGroup {
    /// 组风险档。
    pub risk: Risk,
    /// 组内条目下标（按体积降序）。
    pub indices: Vec<usize>,
    /// 组合计（同 [`total_all`] 口径）。
    pub bytes: u64,
}

/// 按风险分组：组序 = 安全 → 按需 → 谨慎。
pub fn group_by_risk(entries: &[ScanEntry]) -> Vec<RiskGroup> {
    let mut groups: Vec<RiskGroup> = Risk::ALL
        .iter()
        .map(|&risk| RiskGroup {
            risk,
            indices: Vec::new(),
            bytes: 0,
        })
        .collect();
    for (idx, e) in entries.iter().enumerate() {
        if let Some(g) = groups.iter_mut().find(|g| g.risk == e.risk) {
            g.indices.push(idx);
            if !e.report_only {
                g.bytes += e.bytes;
            }
        }
    }
    for g in &mut groups {
        g.indices
            .sort_by_key(|&i| std::cmp::Reverse(entries[i].bytes));
    }
    groups.retain(|g| !g.indices.is_empty());
    groups
}

// ── 9.5 清理执行：命令构造（白名单） + 前后扫描差回填实际回收量 ──

/// 本机包管理器（D9.4 旧内核建议命令按它分流；纯探测，不跑外部命令）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkgManager {
    Dnf,
    Apt,
    Pacman,
    Zypper,
}

impl PkgManager {
    pub fn label(&self) -> &'static str {
        match self {
            PkgManager::Dnf => "dnf/yum",
            PkgManager::Apt => "apt",
            PkgManager::Pacman => "pacman",
            PkgManager::Zypper => "zypper",
        }
    }
}

/// 按可执行文件存在性探测包管理器（pacman > apt-get > zypper > dnf/yum，兜底 dnf）。
pub fn detect_pkg_manager() -> PkgManager {
    for (path, pm) in [
        ("/usr/bin/pacman", PkgManager::Pacman),
        ("/usr/bin/apt-get", PkgManager::Apt),
        ("/usr/bin/zypper", PkgManager::Zypper),
        ("/usr/bin/dnf", PkgManager::Dnf),
        ("/usr/bin/yum", PkgManager::Dnf),
    ] {
        if Path::new(path).exists() {
            return pm;
        }
    }
    PkgManager::Dnf
}

/// 只提示项（`CleanupKind::Hint`）的建议命令（D9.4 / D9.9：点选=复制，不代执行）。
///
/// 旧内核按本机包管理器给 D9.4 指定的四选一；容器/HF 按条目给固定建议；
/// 无公认安全命令的返回 `None`（UI 只显示体积）。
pub fn hint_command_for(target: &CleanTarget, pm: PkgManager) -> Option<String> {
    if target.category == crate::model::cleaner::Category::OldKernel {
        return Some(
            match pm {
                PkgManager::Dnf => "dnf remove --oldinstallonly",
                PkgManager::Apt => "apt autoremove",
                PkgManager::Pacman => "pacman -R <旧内核包名>",
                PkgManager::Zypper => "zypper purge-kernels",
            }
            .to_string(),
        );
    }
    match target.id {
        "docker_storage" => Some("docker system prune".to_string()),
        "podman_system" | "podman_user" => Some("podman system prune".to_string()),
        "huggingface_cache" => Some("huggingface-cli delete-cache".to_string()),
        _ => None,
    }
}

/// 展开后路径是否需要 root（D9.3：系统级统一 pkexec）。
///
/// 主目录下与本用户 `/run/user/<uid>/` 会话目录免提权，其余一律需要。
pub fn needs_root(path: &str, home: &Path, uid: u32) -> bool {
    let home_s = home.to_string_lossy();
    let home_s = home_s.trim_end_matches('/');
    if path == home_s || path.starts_with(&format!("{}/", home_s)) {
        return false;
    }
    if path.starts_with(&format!("/run/user/{}/", uid)) {
        return false;
    }
    true
}

/// 一条可执行的清理动作（构造结果；执行见 [`execute_cleanup`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupAction {
    /// 来源 catalog 条目 id。
    pub target_id: String,
    /// 完整 argv（系统级已包 `pkexec`；直接喂 `Command::new(argv[0])`）。
    pub argv: Vec<String>,
    /// 展示给二次确认弹窗的命令串（D9.2：谨慎类列出将执行的命令）。
    pub display: String,
    /// 是否走提权（D9.3）。
    pub needs_root: bool,
}

/// 白名单校验（D9.3：只有 catalog 内条目能构造删除）。
///
/// 拒绝：空 id / 空路径、id 不在 catalog、路径与 catalog 记录不一致（防拼装伪造）。
fn validate_whitelist(target: &CleanTarget) -> Result<&'static CleanTarget, String> {
    if target.id.is_empty() || target.path.is_empty() {
        return Err("空目标不发命令".to_string());
    }
    let canon = crate::model::cleaner::by_id(target.id)
        .ok_or_else(|| format!("路径 {} 不在 catalog 白名单内，拒绝构造删除", target.path))?;
    if canon.path != target.path {
        return Err(format!(
            "条目 {} 的路径与 catalog 不一致（{} ≠ {}），拒绝构造",
            target.id, target.path, canon.path
        ));
    }
    Ok(canon)
}

/// 专用清理命令表（`CleanupKind::Command` → argv，D9.3 优先于裸 rm）。
fn command_argv(id: &str, path: &str) -> Option<Vec<String>> {
    let v = match id {
        // D9.8：journal 统一保留 200M。
        "journal_log" => vec!["journalctl", "--vacuum-size=200M"],
        "dnf_cache" => vec!["dnf", "clean", "all"],
        "dnf5_cache" => vec!["dnf5", "clean", "all"],
        "yum_cache" => vec!["yum", "clean", "all"],
        "apt_archives" | "apt_partial" => vec!["apt-get", "clean"],
        "pacman_pkg" => vec!["pacman", "-Sc", "--noconfirm"],
        "npm_cache" => vec!["npm", "cache", "clean", "--force"],
        "flatpak_unused" => vec!["flatpak", "uninstall", "--unused"],
        "coredump_dir" => vec!["coredumpctl", "vacuum-size", "100M"],
        // 登录统计：truncate 清零（谨慎类，二次确认后执行）。
        "log_wtmp" | "log_btmp" | "log_lastlog" => vec!["truncate", "-s", "0", path],
        // /var/log 只清轮转文件（*.gz/*.old/数字后缀），不动活动日志与 journal。
        "var_log" => vec![
            "find", "/var/log", "-xdev", "-type", "f", "(", "-name", "*.gz", "-o", "-name",
            "*.old", "-o", "-name", "*.[0-9]", ")", "-delete",
        ],
        _ => return None,
    };
    Some(v.into_iter().map(str::to_string).collect())
}

/// 把 argv 展示成人类可读命令（含空格的参数加双引号，供弹窗列出）。
fn display_of(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.contains(' ') {
                format!("\"{}\"", a)
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// GOAL 9.5 主入口：构造某条目的清理动作。
///
/// - `Direct` → `rm -rf -- <展开路径>`（**递归删除必须在 catalog 内**，白名单三重校验）；
///   `/tmp` `/var/tmp` 两个阈值目录改走 `find -mmin +<N*1440> -delete`（与 D9.7 的
///   7 天口径同源，恰好等于阈值的文件不删——与扫描判定一致）；
/// - `Command` → 专用命令（[`command_argv`] 表）；
///   `Hint` → `Err`（只出建议命令，见 [`hint_command_for`]，不提供执行动作）；
/// - 系统级（非主目录、非本用户 `/run/user/<uid>`）自动包 `pkexec`（D9.3）。
pub fn cleanup_action(target: &CleanTarget, opts: &ScanOptions) -> Result<CleanupAction, String> {
    let canon = validate_whitelist(target)?;
    let path = opts.expand(canon.path);
    // D9.5 红线兜底（catalog 本身不含红线路径，此处为防御性二次校验，恒判定）。
    if crate::model::cleaner::RED_LINE_DIRS.iter().any(|red| {
        let red = opts.expand(red);
        path == red || path.starts_with(&format!("{}/", red.trim_end_matches('/')))
    }) {
        return Err(format!("红线目录 {} 禁止构造删除", path));
    }
    let root = needs_root(&path, &opts.home, opts.uid);
    let mut argv: Vec<String> = match canon.cleanup_kind {
        crate::model::cleaner::CleanupKind::Hint => {
            return Err(format!(
                "{} 是只提示项，不提供清理动作（建议命令见 hint_command）",
                canon.id
            ));
        }
        crate::model::cleaner::CleanupKind::Command => command_argv(canon.id, &path)
            .ok_or_else(|| format!("条目 {} 缺少专用命令定义", canon.id))?,
        crate::model::cleaner::CleanupKind::Direct => {
            if matches!(canon.id, "tmp_dir" | "var_tmp") {
                let minutes = u64::from(opts.tmp_min_age_days) * 1440;
                vec![
                    "find".to_string(),
                    path.clone(),
                    "-xdev".to_string(),
                    "-type".to_string(),
                    "f".to_string(),
                    "-mmin".to_string(),
                    format!("+{}", minutes),
                    "-delete".to_string(),
                ]
            } else {
                vec![
                    "rm".to_string(),
                    "-rf".to_string(),
                    "--".to_string(),
                    path.clone(),
                ]
            }
        }
    };
    if root {
        argv.insert(0, "pkexec".to_string());
    }
    Ok(CleanupAction {
        target_id: canon.id.to_string(),
        display: display_of(&argv),
        argv,
        needs_root: root,
    })
}

/// 执行结果：**实际回收量 = 清理前后同一目标的扫描字节差**（不凭命令 RC 宣称）。
#[derive(Debug, Clone)]
pub struct CleanupOutcome {
    /// 清理前字节。
    pub before: u64,
    /// 清理后字节（目标被删光则为 0）。
    pub after: u64,
    /// 实际回收 = before - after。
    pub reclaimed: u64,
    /// 命令退出码是否为 0（仅作提示，不影响 reclaimed 口径）。
    pub status_ok: bool,
    /// 命令 stderr 尾部（失败诊断）。
    pub stderr: String,
}

/// 真实执行的默认超时（pkexec 无人值守会永挂授权阶段——GOAL 8.3 实测；
/// 超时杀掉 pkexec 时脚本尚未执行，系统状态不变）。
pub const DEFAULT_CLEANUP_TIMEOUT_SECS: u64 = 120;

/// 跑一条 argv（前台阻塞；**调用方必须已在后台线程**，GOAL 3.2）。
/// 超时后杀进程并返回 Err。
fn run_cmd(argv: &[String], timeout: Option<Duration>) -> Result<(bool, String), String> {
    use std::process::Command;
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 {} 失败：{}", argv[0], e))?;
    let deadline = timeout.map(|t| std::time::Instant::now() + t);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = child
                    .wait_with_output()
                    .map_err(|e| format!("读取输出失败：{e}"))?;
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                return Ok((status.success(), stderr));
            }
            Ok(None) => {
                if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                    let _ = child.kill();
                    let _ = child.wait();
                    let secs = timeout.map(|t| t.as_secs()).unwrap_or(0);
                    return Err(format!(
                        "清理命令超时（{}s）已终止：{}",
                        secs,
                        display_of(argv)
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("等待清理命令失败：{e}")),
        }
    }
}

/// GOAL 9.5 执行入口：构造 → （前后各扫一遍）→ 跑命令 → **用字节差回填实际回收量**。
///
/// 目标不存在时不发命令直接报错；命令 RC 只进 `status_ok`，不参与回收量口径。
pub fn execute_cleanup(target: &CleanTarget, opts: &ScanOptions) -> Result<CleanupOutcome, String> {
    execute_cleanup_with(target, opts, |argv| {
        run_cmd(
            argv,
            Some(Duration::from_secs(DEFAULT_CLEANUP_TIMEOUT_SECS)),
        )
    })
}

/// [`execute_cleanup`] 的可注入 runner 形态（单测传假执行器，不跑真命令）。
pub fn execute_cleanup_with<F>(
    target: &CleanTarget,
    opts: &ScanOptions,
    runner: F,
) -> Result<CleanupOutcome, String>
where
    F: FnOnce(&[String]) -> Result<(bool, String), String>,
{
    let action = cleanup_action(target, opts)?;
    let scan_opts = scan_opts_for(target, opts);
    let before = scan_target(target, &scan_opts)
        .ok_or_else(|| "目标不存在，无事可做（不发命令）".to_string())?;
    let (status_ok, stderr) = runner(&action.argv)?;
    let after = scan_target(target, &scan_opts)
        .map(|s| s.bytes)
        .unwrap_or(0);
    Ok(CleanupOutcome {
        before: before.bytes,
        after,
        reclaimed: before.bytes.saturating_sub(after),
        status_ok,
        stderr,
    })
}

/// 批量清理结果（9.7-⑤：单条失败不中断其余，逐条回报）。
pub struct BatchOutcome {
    /// 实际回收字节数合计（前后扫描差，仅统计执行成功的目标）。
    pub reclaimed: u64,
    /// 执行完成（拿到 Outcome，含命令非零）的目标数。
    pub done: usize,
    /// 失败明细，逐条一条（不在白名单 / 命令非零 / 执行器报错）。
    pub failures: Vec<String>,
}

/// 逐条执行批量清理：任何一条失败只记入 [`BatchOutcome::failures`]，**绝不中断后续条目**；
/// `on_each(第几条, 总数, 路径注记)` 在每条开始前回调（页面拿来刷进度条，无 UI 时传 `|_,_,_| {}`）。
/// [`execute_cleanup_batch_with`] 的真执行器包装。
pub fn execute_cleanup_batch<F>(ids: &[String], opts: &ScanOptions, on_each: F) -> BatchOutcome
where
    F: FnMut(usize, usize, String),
{
    execute_cleanup_batch_with(ids, opts, execute_cleanup, on_each)
}

/// [`execute_cleanup_batch`] 的可注入 runner 形态（单测传假执行器，不跑真命令）。
pub fn execute_cleanup_batch_with<C, F>(
    ids: &[String],
    opts: &ScanOptions,
    runner: C,
    mut on_each: F,
) -> BatchOutcome
where
    C: Fn(&CleanTarget, &ScanOptions) -> Result<CleanupOutcome, String>,
    F: FnMut(usize, usize, String),
{
    let total = ids.len();
    let mut reclaimed: u64 = 0;
    let mut done = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        let path_note = by_id(id).map(|t| opts.expand(t.path)).unwrap_or_default();
        on_each(i + 1, total, path_note);
        let Some(target) = by_id(id) else {
            failures.push(format!("{id}: 不在 catalog 白名单，跳过"));
            continue;
        };
        match runner(target, opts) {
            Ok(out) => {
                reclaimed += out.reclaimed;
                done += 1;
                if !out.status_ok && !out.stderr.is_empty() {
                    failures.push(format!(
                        "{}: 命令返回非零（回收 {}）：{}",
                        target.id,
                        format_bytes(out.reclaimed),
                        out.stderr
                    ));
                }
            }
            Err(e) => failures.push(format!("{}: {e}", target.id)),
        }
    }
    BatchOutcome {
        reclaimed,
        done,
        failures,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;

    /// 造一棵合成树：`base/` 下按 `spec` 写文件（`a/b.txt` 形式，内容长度 = size）。
    fn make_tree(base: &Path, spec: &[(&str, usize)]) {
        for (rel, size) in spec {
            let p = base.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            let mut f = File::create(&p).unwrap();
            f.write_all(&vec![b'x'; *size]).unwrap();
        }
    }

    fn opts_in(base: &Path) -> ScanOptions {
        ScanOptions {
            home: base.to_path_buf(),
            uid: 1000,
            max_depth: 64,
            min_age: None,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: None,
            target_progress: None,
            mount_points: Some(Arc::new(HashSet::new())),
            tmp_min_age_days: DEFAULT_TMP_AGE_DAYS,
        }
    }

    /// ① 命中统计字节数与文件数（嵌套目录合计）。
    #[test]
    fn scan_counts_bytes_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(
            tmp.path(),
            &[("a.txt", 10), ("sub/b.txt", 20), ("sub/deep/c.txt", 30)],
        );
        let st = scan_dir_at(tmp.path(), &opts_in(tmp.path())).unwrap();
        assert_eq!(st.bytes, 60);
        assert_eq!(st.files, 3);
        assert_eq!(st.dirs, 3, "根 + sub + deep 共 3 个目录");
        assert_eq!(st.inaccessible, 0);
        assert!(st.oldest_mtime.is_some(), "应给出 mtime 说明");
    }

    /// ② 不存在路径返回 None（GOAL 9.2）。
    #[test]
    fn missing_path_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(scan_dir_at(&tmp.path().join("nope"), &opts_in(tmp.path())).is_none());
        let target = CleanTarget {
            id: "t",
            category: crate::model::cleaner::Category::TempFiles,
            path: "~/definitely_missing_dir_xyz/",
            desc: "不存在的目录",
            risk: crate::model::cleaner::Risk::OnDemand,
            cleanup_kind: crate::model::cleaner::CleanupKind::Direct,
            pattern: None,
        };
        assert!(scan_target(&target, &opts_in(tmp.path())).is_none());
    }

    /// ③ 空目录：0 字节 0 文件，不报错。
    #[test]
    fn empty_dir_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let st = scan_dir_at(tmp.path(), &opts_in(tmp.path())).unwrap();
        assert_eq!((st.bytes, st.files), (0, 0));
        assert_eq!(st.dirs, 1);
    }

    /// ④ 符号链接不跟随（防环 + 不把链接目标算两遍）。
    #[test]
    fn symlink_not_followed_no_loop() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path(), &[("real/big.txt", 4096)]);
        let real = tmp.path().join("real");
        // 环：real/loop -> real（若跟随会无限递归）
        std::os::unix::fs::symlink(&real, real.join("loop")).unwrap();
        // 指向外部文件的链接（若跟随会多算 4096）
        std::os::unix::fs::symlink(real.join("big.txt"), real.join("link.txt")).unwrap();
        let st = scan_dir_at(&real, &opts_in(tmp.path())).unwrap();
        assert_eq!(st.files, 1, "只应统计真实文件");
        assert_eq!(st.bytes, 4096);
        assert!(
            st.skipped_symlinks >= 2,
            "两个链接都应跳过，实为 {}",
            st.skipped_symlinks
        );
    }

    /// ⑤ 无权限目录计数且不 panic（D9.1）。
    #[test]
    fn permission_denied_counted_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path(), &[("open/a.txt", 5), ("locked/b.txt", 7)]);
        let locked = tmp.path().join("locked");
        let mut perm = fs::metadata(&locked).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perm.set_mode(0o000);
        fs::set_permissions(&locked, perm.clone()).unwrap();
        // 若当前进程仍能读（例如以 root 跑测试），本用例无意义，恢复后跳过。
        if fs::read_dir(&locked).is_ok() {
            perm.set_mode(0o755);
            fs::set_permissions(&locked, perm).unwrap();
            return;
        }
        let st = scan_dir_at(tmp.path(), &opts_in(tmp.path())).unwrap();
        assert_eq!(st.inaccessible, 1, "locked 目录应计入无法访问");
        assert_eq!(st.files, 1, "可读部分照常统计");
        perm.set_mode(0o755);
        fs::set_permissions(&locked, perm).unwrap();
    }

    /// ⑥ 深度上限：超限子树不统计并计数。
    #[test]
    fn depth_limit_stops_descent() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(
            tmp.path(),
            &[
                ("l1/a.txt", 1),
                ("l1/l2/b.txt", 1),
                ("l1/l2/l3/c.txt", 1),
                ("l1/l2/l3/l4/d.txt", 1),
            ],
        );
        let mut o = opts_in(tmp.path());
        o.max_depth = 2;
        let st = scan_dir_at(tmp.path(), &o).unwrap();
        assert_eq!(st.files, 2, "l1/l2 内文件应被统计，更深的不计");
        assert!(st.skipped_depth >= 1, "应记录超限子树");
    }

    /// ⑦ 通配模式命中：`*.tmp` / `*~` / `core.*`，普通文件不命中。
    #[test]
    fn pattern_hits_match_expected_globs() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(
            tmp.path(),
            &[
                ("x.tmp", 3),
                ("y.bak", 4),
                ("core.1234", 5),
                ("note~", 6),
                ("plain.txt", 100),
            ],
        );
        let pats = ["*.tmp", "*.bak", "core.*", "*~"];
        let (hits, st) = scan_pattern(&[tmp.path().to_path_buf()], &pats, &opts_in(tmp.path()));
        assert_eq!(hits.len(), 4, "应命中 4 个文件: {:?}", hits);
        assert!(st.files + st.dirs > 0);
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(paths.iter().any(|p| p.ends_with("x.tmp")));
        assert!(paths.iter().any(|p| p.ends_with("core.1234")));
        assert!(!paths.iter().any(|p| p.ends_with("plain.txt")));
    }

    /// ⑧ `*.rpmnew` 不计（「不删」模式不进默认扫描集合，D9.1 附录九）。
    #[test]
    fn rpmnew_not_counted_by_default_patterns() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(
            tmp.path(),
            &[("pkg.rpmnew", 9), ("pkg.rpmsave", 9), ("ok.tmp", 2)],
        );
        let defaults = crate::model::cleaner::scan_patterns();
        assert!(!defaults.contains(&"*.rpmnew"));
        assert!(!defaults.contains(&"*.rpmsave"));
        let (hits, _) = scan_pattern(&[tmp.path().to_path_buf()], &defaults, &opts_in(tmp.path()));
        assert_eq!(hits.len(), 1, "只应命中 ok.tmp，实为 {:?}", hits);
        assert!(hits[0].path.ends_with("ok.tmp"));
    }

    /// ⑨ 取消中途返回已扫部分（每 32 目录检查一次）。
    #[test]
    fn cancel_returns_partial_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let mut spec: Vec<(&str, usize)> = Vec::new();
        for i in 0..100 {
            let rel = format!("d{:03}/f.txt", i);
            spec.push((Box::leak(rel.into_boxed_str()), 1));
        }
        make_tree(tmp.path(), &spec);
        let mut o = opts_in(tmp.path());
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        o.cancel = cancel;
        o.progress = Some(Arc::new(move |_: &str, _: u64| {
            flag.store(true, Ordering::Relaxed);
        }));
        let st = scan_dir_at(tmp.path(), &o).unwrap();
        assert!(st.cancelled, "应被取消标记中断");
        assert!(
            st.files > 0 && st.files < 100,
            "应返回已扫部分，实为 {}",
            st.files
        );
    }

    /// ⑩ 7 天阈值（D9.7）：早于阈值的文件计入，新文件不计。
    #[test]
    fn min_age_filters_recent_files() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path(), &[("old.txt", 11), ("fresh.txt", 22)]);
        let old = tmp.path().join("old.txt");
        let f = File::options().write(true).open(&old).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(10 * 86_400))
            .unwrap();
        let mut o = opts_in(tmp.path());
        o.min_age = Some(Duration::from_secs(7 * 86_400));
        let st = scan_dir_at(tmp.path(), &o).unwrap();
        assert_eq!(st.files, 1, "只应计入 10 天前的 old.txt");
        assert_eq!(st.bytes, 11);
        // 边界（GOAL 9.4 口径）：恰好等于阈值**不计入**，严格超过才计入。
        let seven = Duration::from_secs(7 * 86_400);
        assert!(!exceeds_min_age(seven, seven), "恰好等于阈值不计入");
        assert!(
            exceeds_min_age(seven + Duration::from_nanos(1), seven),
            "严格超过才计入"
        );
        assert!(!exceeds_min_age(seven - Duration::from_secs(1), seven));
        // 扫描级：阈值放到 30 天（远超 10 天旧文件）→ 一个都不计。
        let mut o2 = opts_in(tmp.path());
        o2.min_age = Some(Duration::from_secs(30 * 86_400));
        let st2 = scan_dir_at(tmp.path(), &o2).unwrap();
        assert_eq!(st2.files, 0, "30 天阈值下 10 天旧文件不计入");
    }

    /// ⑪ 不跨挂载点边界：子目录命中挂载点集合即跳过并计数。
    #[test]
    fn mount_boundary_not_crossed() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path(), &[("keep/a.txt", 5), ("mnt/b.txt", 9)]);
        let mut mounts = HashSet::new();
        mounts.insert(tmp.path().join("mnt"));
        let mut o = opts_in(tmp.path());
        o.mount_points = Some(Arc::new(mounts));
        let st = scan_dir_at(tmp.path(), &o).unwrap();
        assert_eq!(st.files, 1, "挂载点子树不应统计");
        assert_eq!(st.bytes, 5);
        assert_eq!(st.skipped_mounts, 1);
    }

    /// ⑫ D9.5 红线与 `.git` 仓库整体跳过（模式扫描不产出命中）。
    #[test]
    fn red_line_and_git_repo_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(
            tmp.path(),
            &[
                ("Documents/报告.tmp", 50),
                ("Downloads/setup.tmp", 60),
                ("ok/place.tmp", 7),
                ("repo/src/main.rs", 3),
                ("repo/target/left.tmp", 8),
            ],
        );
        fs::create_dir(tmp.path().join("repo/.git")).unwrap();
        let pats = ["*.tmp"];
        let (hits, _) = scan_pattern(&[tmp.path().to_path_buf()], &pats, &opts_in(tmp.path()));
        assert_eq!(hits.len(), 1, "红线目录与仓库都不应产出命中: {:?}", hits);
        assert!(hits[0].path.ends_with("ok/place.tmp"));
        assert!(is_red_line(
            Path::new(&tmp.path().join("Documents/x")),
            tmp.path()
        ));
        assert!(!is_red_line(
            Path::new(&tmp.path().join("ok/x")),
            tmp.path()
        ));
        assert!(!is_red_line(Path::new("/etc/x"), tmp.path()));
    }

    /// ⑬ 路径展开：`~` / `~/x` / `$UID`。
    #[test]
    fn expand_path_tilde_and_uid() {
        let home = Path::new("/home/alice");
        assert_eq!(expand_path("~", home, 1000), "/home/alice");
        assert_eq!(expand_path("~/", home, 1000), "/home/alice/");
        assert_eq!(expand_path("~/.cache/", home, 1000), "/home/alice/.cache/");
        assert_eq!(
            expand_path("/run/user/$UID/", home, 1000),
            "/run/user/1000/"
        );
        assert_eq!(expand_path("/tmp/x", home, 1000), "/tmp/x");
        // 尾带斜杠的 home。
        let home2 = Path::new("/home/bob/");
        assert_eq!(expand_path("~/a", home2, 1), "/home/bob/a");
    }

    /// ⑭ 通配匹配边界：`*` 贪婪回溯、`?` 单字符、全字面量。
    #[test]
    fn glob_match_truth_table() {
        assert!(glob_match("*.tmp", "a.tmp"));
        assert!(glob_match("*.tmp", ".tmp"));
        assert!(!glob_match("*.tmp", "a.tmpx"));
        assert!(glob_match("*~", "note~"));
        assert!(!glob_match("*~", "note"));
        assert!(glob_match("core.*", "core.123"));
        assert!(!glob_match("core.*", "coredump"));
        assert!(glob_match("core.*", "core."));
        assert!(glob_match("?.txt", "a.txt"));
        assert!(!glob_match("?.txt", "ab.txt"));
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("vmlinuz-*", "vmlinuz-6.8.5-301.fc40.x86_64"));
        assert!(!glob_match("vmlinuz-*", "vmlinuz"));
        assert!(glob_match("*.default*", "abcd123.default-release"));
    }

    /// ⑮ 通配 catalog 路径展开（GOAL 9.2：通配目标先展开再探测）。
    #[test]
    fn glob_path_expansion_for_targets() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(
            tmp.path(),
            &[
                ("vmlinuz-6.8.5", 100),
                ("vmlinuz-6.6.2", 200),
                ("initramfs-6.6.2.img", 50),
                ("other.txt", 1),
            ],
        );
        let home = tmp.path();
        let mut o = opts_in(home);
        o.mount_points = Some(Arc::new(HashSet::new()));
        let expanded = expand_path("~/vmlinuz-*", home, 1000);
        let matches = expand_glob(&expanded);
        assert_eq!(matches.len(), 2, "应命中两个 vmlinuz: {:?}", matches);
        let target = CleanTarget {
            id: "boot_vmlinuz",
            category: crate::model::cleaner::Category::OldKernel,
            path: "~/vmlinuz-*",
            desc: "旧内核",
            risk: crate::model::cleaner::Risk::Caution,
            cleanup_kind: crate::model::cleaner::CleanupKind::Hint,
            pattern: None,
        };
        let st = scan_target(&target, &o).unwrap();
        assert_eq!(st.files, 2);
        assert_eq!(st.bytes, 300, "两个 vmlinuz 合计 300 字节");
    }

    /// ⑯ scan_target 常规路径（`~` 展开后求和）。
    #[test]
    fn scan_target_expands_tilde() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path(), &[(".cache/a.bin", 33)]);
        let target = CleanTarget {
            id: "user_cache",
            category: crate::model::cleaner::Category::UserCache,
            path: "~/.cache/",
            desc: "用户缓存总目录",
            risk: crate::model::cleaner::Risk::OnDemand,
            cleanup_kind: crate::model::cleaner::CleanupKind::Direct,
            pattern: None,
        };
        let st = scan_target(&target, &opts_in(tmp.path())).unwrap();
        assert_eq!((st.bytes, st.files), (33, 1));
    }

    // ── 9.3：mountinfo 解析 fixture（overlay / btrfs 子卷 / bind / NFS / 坏行） ──

    /// ⑰ 普通 ext4 根行：字段位次与 fs/source 提取。
    #[test]
    fn mountinfo_parses_ext4_root_line() {
        let line = "22 1 8:3 / /boot rw,relatime shared:28 - ext4 /dev/nvme1n1p2 rw,seclabel";
        let m = parse_mountinfo_line(line).expect("应解析成功");
        assert_eq!(m.mount_point, PathBuf::from("/boot"));
        assert_eq!(m.root, "/");
        assert_eq!(m.fs_type, "ext4");
        assert_eq!(m.source, "/dev/nvme1n1p2");
        assert_eq!(should_skip_mount(&m), None, "/boot 应可扫");
    }

    /// ⑱ btrfs 子卷：root 字段带子卷路径，挂载点仍是 `/`。
    #[test]
    fn mountinfo_parses_btrfs_subvol() {
        let line = "28 1 259:2 /root / rw,relatime shared:1 - btrfs /dev/nvme1n1p3 rw,seclabel,subvol=/root";
        let m = parse_mountinfo_line(line).unwrap();
        assert_eq!(m.mount_point, PathBuf::from("/"));
        assert_eq!(m.root, "/root", "应保留 btrfs 子卷 root 字段");
        assert_eq!(m.fs_type, "btrfs");
        assert_eq!(should_skip_mount(&m), None);
    }

    /// ⑲ bind mount：root 字段 ≠ 挂载点（源目录绑到别处）。
    #[test]
    fn mountinfo_parses_bind_mount() {
        let line =
            "90 28 259:2 /root/data /srv/data rw,relatime shared:50 - btrfs /dev/nvme1n1p3 rw";
        let m = parse_mountinfo_line(line).unwrap();
        assert_eq!(m.mount_point, PathBuf::from("/srv/data"));
        assert_eq!(m.root, "/root/data");
        assert_eq!(
            should_skip_mount(&m),
            None,
            "bind 挂载点本身可扫（不重复：walk 有边界跳过）"
        );
    }

    /// ⑳ 网络挂载 NFS/CIFS：默认跳过（D9.7）。
    #[test]
    fn mountinfo_nfs_and_cifs_skipped_as_network() {
        let nfs = parse_mountinfo_line(
            "60 28 0:55 / /mnt/nas rw,relatime shared:40 - nfs4 server:/export rw,vers=4.2",
        )
        .unwrap();
        let reason = should_skip_mount(&nfs).expect("nfs 应默认跳过");
        assert!(reason.contains("网络挂载"), "原因应为网络挂载: {}", reason);
        let cifs = parse_mountinfo_line(
            "65 28 0:56 / /srv/share rw,relatime - cifs //fileserver/share rw,uid=1000",
        )
        .unwrap();
        let reason = should_skip_mount(&cifs).expect("cifs 应默认跳过");
        assert!(reason.contains("网络挂载"), "{}", reason);
        // sshfs（FUSE 形态）
        let sshfs = parse_mountinfo_line(
            "70 28 0:57 / /mnt/remote rw,nosuid,nodev - fuse.sshfs user@host:/path rw",
        )
        .unwrap();
        assert!(should_skip_mount(&sshfs).unwrap().contains("网络挂载"));
    }

    /// ㉑ overlay（容器层）与伪 fs / EFI 的默认跳过判定。
    #[test]
    fn mountinfo_overlay_scannable_pseudo_skipped() {
        let overlay = parse_mountinfo_line(
            "100 28 0:120 / / rw,relatime - overlay overlay rw,lowerdir=/l,upperdir=/u,workdir=/w",
        )
        .unwrap();
        assert_eq!(overlay.fs_type, "overlay");
        assert_eq!(should_skip_mount(&overlay), None, "overlay 是真实可扫层");
        for (line, why) in [
            ("21 30 0:20 / /sys rw - sysfs sysfs rw", "伪文件系统"),
            ("24 1 0:23 / /proc rw - proc proc rw", "伪文件系统"),
            ("50 1 0:46 / /run rw - tmpfs tmpfs rw", "伪文件系统"),
            ("26 1 0:25 / /dev rw - devtmpfs devtmpfs rw", "伪文件系统"),
            (
                "30 1 0:27 / /sys/kernel/tracing rw - tracefs tracefs rw",
                "伪文件系统",
            ),
            (
                "55 1 8:1 / /boot/efi rw,relatime - vfat /dev/nvme1n1p1 rw",
                "EFI",
            ),
        ] {
            let m = parse_mountinfo_line(line).expect("fixture 应可解析");
            let reason = should_skip_mount(&m).unwrap_or("可扫");
            assert!(
                reason.contains(why),
                "{} 应判为{}，实为「{}」",
                m.mount_point.display(),
                why,
                reason
            );
        }
    }

    /// ㉒ 坏行跳过不 panic：字段不足 / 无 `-` 分隔符 / 空行。
    #[test]
    fn mountinfo_malformed_lines_skipped() {
        assert!(parse_mountinfo_line("").is_none());
        assert!(
            parse_mountinfo_line("21 30 0:20 / /sys rw").is_none(),
            "字段不足"
        );
        assert!(
            parse_mountinfo_line("21 30 0:20 / /sys rw,nosuid sysfs sysfs rw").is_none(),
            "无 - 分隔符"
        );
        assert!(parse_mountinfo_line("garbage line with no structure at all here").is_none());
        let text = "21 30 0:20 / /sys rw - sysfs sysfs rw\n\n坏行坏行\n22 1 8:3 / /boot rw - ext4 /dev/sda1 rw";
        let recs = parse_mountinfo(text);
        assert_eq!(recs.len(), 2, "只应解析出两条合法记录，实为 {}", recs.len());
    }

    /// ㉓ 字段转义：`\040` 还原为空格、`\134` 还原为反斜杠、未知转义原样保留。
    #[test]
    fn mountinfo_unescapes_fields() {
        // Rust 源里写 `\\040` 才是 mountinfo 文本中的 `\040` 转义序列。
        let m = parse_mountinfo_line("80 28 8:5 / /mnt/my\\040disk rw - ext4 /dev/sdb1 rw")
            .expect("应解析成功");
        assert_eq!(
            m.mount_point,
            PathBuf::from("/mnt/my disk"),
            "转义空格应还原"
        );
        let m2 = parse_mountinfo_line("81 28 8:5 / /mnt/a\\134b rw - ext4 /dev/sdb2 rw").unwrap();
        assert_eq!(
            m2.mount_point,
            PathBuf::from("/mnt/a\\b"),
            "反斜杠转义应还原"
        );
        let m3 = parse_mountinfo_line("82 28 8:5 / /mnt/x\\041y rw - ext4 /dev/sdb3 rw").unwrap();
        assert_eq!(
            m3.mount_point,
            PathBuf::from("/mnt/x\\041y"),
            "未知转义原样保留"
        );
    }

    /// ㉔ classify_mounts 分组：默认档与「启用非默认」档（D9.7 灰显可勾）。
    #[test]
    fn classify_mounts_partitions_and_nondefault_override() {
        let text = "21 30 0:20 / /sys rw - sysfs sysfs rw\n22 1 8:3 / /boot rw - ext4 /dev/sda1 rw\n55 1 8:1 / /boot/efi rw - vfat /dev/sda1 rw\n60 28 0:55 / /mnt/nas rw - nfs4 server:/export rw\n28 1 8:3 / / rw - ext4 /dev/sda3 rw\n50 1 0:46 / /run rw - tmpfs tmpfs rw";
        let (scannable, skipped) = classify_mounts(text, false);
        let sc: Vec<String> = scannable
            .iter()
            .map(|m| m.mount_point.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            sc,
            vec!["/boot".to_string(), "/".to_string()],
            "可扫 = /boot 与 /"
        );
        assert_eq!(skipped.len(), 4, "sysfs + efi + nfs + tmpfs 四条默认跳过");
        // 启用非默认（D9.7 用户勾上网络挂载）：nfs 进可扫组，伪 fs 与 EFI 仍不进。
        let (scannable2, skipped2) = classify_mounts(text, true);
        assert_eq!(scannable2.len(), 3, "nfs 被勾进可扫组");
        assert_eq!(skipped2.len(), 3, "sysfs/EFI/tmpfs 仍跳过");
    }

    /// ㉕ 挂载点分组：最长前缀归属（/home 的命中归 /home 而不是 /）。
    #[test]
    fn group_by_mount_uses_longest_prefix() {
        let mounts = parse_mountinfo(
            "22 1 8:3 / /home rw - ext4 /dev/sda1 rw\n28 1 8:3 / / rw - ext4 /dev/sda3 rw",
        );
        let hits = vec![
            PatternHit {
                path: "/home/alice/a.tmp".into(),
                pattern: "*.tmp".into(),
                bytes: 100,
                mtime: None,
            },
            PatternHit {
                path: "/home/bob/b.tmp".into(),
                pattern: "*.tmp".into(),
                bytes: 200,
                mtime: None,
            },
            PatternHit {
                path: "/var/c.tmp".into(),
                pattern: "*.tmp".into(),
                bytes: 30,
                mtime: None,
            },
        ];
        let groups = group_by_mount(&hits, &mounts);
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[0].mount,
            PathBuf::from("/home"),
            "字节降序：/home 300 在前"
        );
        assert_eq!((groups[0].bytes, groups[0].files), (300, 2));
        assert_eq!((groups[1].bytes, groups[1].files), (30, 1));
    }

    /// ㉖ top-N：catalog 结果与模式命中都按字节降序截断。
    #[test]
    fn top_n_sorts_by_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path(), &[("a.tmp", 10), ("b.tmp", 500), ("c.tmp", 50)]);
        let (hits, _) = scan_pattern(
            &[tmp.path().to_path_buf()],
            &["*.tmp"],
            &opts_in(tmp.path()),
        );
        let top = top_files(&hits, 2);
        assert_eq!(top.len(), 2);
        assert!(top[0].path.ends_with("b.tmp"), "最大在首位");
        let mut report = ScanReport::default();
        for (id, bytes) in [("x", 1u64), ("y", 100), ("z", 10)] {
            let mut e = crate::model::cleaner::ScanEntry::from_target(
                crate::model::cleaner::by_id("dnf_cache").unwrap(),
                format!("/fake/{}", id),
            );
            e.bytes = bytes;
            report.entries.push(e);
        }
        let top = top_entries(&report, 2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].bytes, 100);
        assert_eq!(top[1].bytes, 10);
    }

    /// ㉗ 快速扫描（9.3 真机 dry-run 入口，`--ignored --nocapture` 跑）：
    /// 打印本机前 10 大可回收项与 missing/inaccessible 计数。
    #[ignore = "真机取证：cargo test --lib utils::cleaner::tests::real_machine_quick_scan -- --ignored --nocapture"]
    #[test]
    fn real_machine_quick_scan() {
        let opts = ScanOptions::new();
        let started = std::time::Instant::now();
        let report = quick_scan(&opts);
        let elapsed = started.elapsed();
        println!("== 真机快速扫描（仅 catalog，D9.7 默认档） ==");
        println!(
            "条目 {} 个，缺失跳过 {}，无法访问 {}，跳过挂载 {}，耗时 {:?}",
            report.entries.len(),
            report.missing,
            report.inaccessible,
            report.skipped_mounts,
            elapsed
        );
        println!("== 前 10 大可回收项 ==");
        for (i, e) in top_entries(&report, 10).iter().enumerate() {
            println!(
                "{:>2}. {:>10}  {}  [{}] {}",
                i + 1,
                format_bytes(e.bytes),
                e.risk.label(),
                e.category.label(),
                e.path
            );
        }
        assert!(!report.entries.is_empty(), "真机至少应扫出一个存在的条目");
        assert!(
            report.entries.iter().any(|e| e.bytes > 100 * 1024 * 1024),
            "应能看到 >100MB 的可回收项（~/.cache / journal / .cargo 量级）"
        );
    }

    /// 目标级进度（9.6 页面百分比来源）：逐条触发、`(已完成, 总数)` 单调；
    /// 第 3 条后置取消标记，验证「取消即停」且已触发的进度保留。
    #[test]
    fn target_progress_reports_each_entry() {
        let mut o = opts_in(Path::new("/nonexistent-home-xyz"));
        let cancel = o.cancel.clone();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_c = seen.clone();
        o.target_progress = Some(std::sync::Arc::new(move |done: usize, total: usize| {
            seen_c.lock().unwrap().push((done, total));
            if done >= 3 {
                cancel.store(true, Ordering::Relaxed);
            }
        }));
        let report = quick_scan(&o);
        let v = seen.lock().unwrap();
        assert_eq!(v.len(), 3, "第 3 条后置取消 → 只触发 3 次");
        assert!(
            v.iter()
                .all(|&(_, t)| t == crate::model::cleaner::CATALOG.len())
        );
        let dones: Vec<usize> = v.iter().map(|&(d, _)| d).collect();
        assert_eq!(dones, vec![1, 2, 3], "从 1 开始单调不跳号");
        drop(v);
        assert!(report.cancelled, "取消标记应反映到报告");
    }

    // ── 9.4：聚合与勾选真值表 ──

    /// 由 catalog 条目造一条带体积的扫描结果（id 来自真实 catalog，路径用原样 `~` 形式，
    /// 前缀判定对两种形态同样成立）。
    fn ent(id: &str, bytes: u64) -> ScanEntry {
        let t = crate::model::cleaner::by_id(id).unwrap_or_else(|| panic!("catalog 缺 {id}"));
        let mut e = ScanEntry::from_target(t, t.path.to_string());
        e.bytes = bytes;
        e
    }

    /// 手工造任意路径的条目（测链条去重用）。
    fn ent_path(id: &str, path: &str, bytes: u64) -> ScanEntry {
        ScanEntry {
            id: id.to_string(),
            path: path.to_string(),
            desc: "测试条目".to_string(),
            category: Category::UserCache,
            risk: Risk::OnDemand,
            cleanup_kind: crate::model::cleaner::CleanupKind::Direct,
            bytes,
            files: 1,
            report_only: false,
            contains_children: false,
            missing: false,
        }
    }

    /// ㉘ 三档默认勾选（D9.2）：安全勾、按需/谨慎不勾；只报项永不勾。
    #[test]
    fn default_checked_follows_three_tiers() {
        assert!(default_checked(&ent("trash_files", 1)), "【安全】默认勾选");
        assert!(!default_checked(&ent("pip_cache", 1)), "【按需】默认不勾");
        assert!(!default_checked(&ent("journal_log", 1)), "【谨慎】默认不勾");
        // 只报项即便风险是安全档也不勾（D9.9）。
        let mut hint = ent("trash_files", 1);
        hint.report_only = true;
        assert!(!default_checked(&hint), "只报项不进勾选");
    }

    /// ㉙ 一键全选安全类：只收【安全】，绝不越权勾按需/谨慎/只报项。
    #[test]
    fn select_all_safe_never_overreaches() {
        let entries = vec![
            ent("apt_archives", 10),      // 安全
            ent("trash_files", 20),       // 安全
            ent("pip_cache", 30),         // 按需
            ent("journal_log", 40),       // 谨慎
            ent("docker_storage", 50),    // 谨慎 + 只报
            ent("huggingface_cache", 60), // 按需 + 只报
        ];
        let sel = select_all_safe(&entries);
        assert!(sel.contains("apt_archives") && sel.contains("trash_files"));
        assert!(!sel.contains("pip_cache"), "按需不得被越权勾选");
        assert!(!sel.contains("journal_log"), "谨慎不得被越权勾选");
        assert!(!sel.contains("docker_storage") && !sel.contains("huggingface_cache"));
        assert_eq!(sel.len(), 2);
        // 默认勾选与全选安全类同口径。
        assert_eq!(default_selection(&entries), sel);
    }

    /// ㉚ 勾选合计 = 各勾选项之和；未知 id 不计入；取消勾选即下降。
    #[test]
    fn total_is_sum_of_checked() {
        let entries = vec![
            ent("apt_archives", 10),
            ent("pip_cache", 20),
            ent("log_apt", 30),
        ];
        let all: HashSet<String> = entries.iter().map(|e| e.id.clone()).collect();
        assert_eq!(total_checked(&entries, &all), 60);
        let mut minus = all.clone();
        minus.remove("pip_cache");
        assert_eq!(
            total_checked(&entries, &minus),
            40,
            "取消勾选后合计实时下降"
        );
        minus.insert("ghost_id".to_string());
        assert_eq!(total_checked(&entries, &minus), 40, "未知 id 不计入");
        assert_eq!(total_checked(&entries, &HashSet::new()), 0, "空勾选 = 0");
    }

    /// ㉛ 父子去重（D9.9）：子体积从父扣除，合计不虚增。
    #[test]
    fn parent_child_dedup_no_inflation() {
        let mut entries = vec![ent("user_cache", 100), ent("thumbnails_cache", 30)];
        assert_eq!(total_all(&entries), 130, "去重前父子重复计（虚增 30）");
        dedup_parent_child(&mut entries);
        assert_eq!(entries[0].bytes, 70, "父条目扣除子体积");
        assert!(entries[0].contains_children, "父条目标注已含子项");
        assert!(!entries[1].contains_children, "子条目不变");
        assert_eq!(entries[1].bytes, 30);
        assert_eq!(
            total_all(&entries),
            100,
            "去重后合计 = 真实可回收量，不虚增"
        );
    }

    /// ㉜ 多子与链条：/var/log 两个子条目；A⊃B⊃C 只扣直接子（不重复扣）。
    #[test]
    fn dedup_handles_multiple_children_and_chains() {
        let mut entries = vec![
            ent("var_log", 100),
            ent("journal_log", 40),
            ent("log_apt", 10),
        ];
        dedup_parent_child(&mut entries);
        assert_eq!(entries[0].bytes, 50, "var_log 扣掉 journal 40 + apt 10");
        assert_eq!(total_all(&entries), 100, "三口径合计仍是原始 100");

        // 链条 A(/a/) ⊃ B(/a/b/) ⊃ C(/a/b/c/)：A 只扣直接子 B 的原始体积 60。
        let mut chain = vec![
            ent_path("a", "/a/", 100),
            ent_path("b", "/a/b/", 60),
            ent_path("c", "/a/b/c/", 20),
        ];
        dedup_parent_child(&mut chain);
        assert_eq!(chain[0].bytes, 40, "A 扣直接子 B（含 C 的 60）");
        assert_eq!(chain[1].bytes, 40, "B 扣 C");
        assert_eq!(chain[2].bytes, 20);
        assert_eq!(total_all(&chain), 100, "链条去重后合计仍是 100");
    }

    /// ㉝ 去重不做误判：/tmp/ 与 /var/tmp/ 非父子，互不扣减。
    #[test]
    fn dedup_ignores_unrelated_paths() {
        let mut entries = vec![ent("tmp_dir", 50), ent("var_tmp", 30)];
        dedup_parent_child(&mut entries);
        assert_eq!(entries[0].bytes, 50, "/var/tmp/ 不是 /tmp/ 的子目录");
        assert!(!entries[0].contains_children);
        assert_eq!(total_all(&entries), 80);
    }

    /// ㉞ 7 天边界（GOAL 9.4）：恰好等于阈值**不计入**，严格超过才计入。
    #[test]
    fn age_boundary_exact_not_counted() {
        let seven = Duration::from_secs(7 * 86_400);
        assert!(!exceeds_min_age(seven, seven), "恰好 7 天不计入");
        assert!(exceeds_min_age(seven + Duration::from_secs(1), seven));
        assert!(!exceeds_min_age(seven - Duration::from_secs(1), seven));
        assert!(!exceeds_min_age(Duration::ZERO, seven), "刚写的文件不计入");
        // quick_scan 对临时目录施加的默认阈值 = D9.7 的 7 天。
        assert_eq!(DEFAULT_TMP_AGE_DAYS, 7);
    }

    /// ㉟ format_bytes 单位换算边界：1023B-1KB-1MB-1GB-TB。
    #[test]
    fn format_bytes_boundaries() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(format_bytes(1024u64.pow(4)), "1.0 TB");
        // 超出 TB 档仍停在 TB（不出现第 6 档）。
        assert!(format_bytes(u64::MAX).ends_with(" TB"));
    }

    /// ㊱ 空结果：合计 0、无分组、空选择，不 panic。
    #[test]
    fn empty_aggregates_are_zero() {
        let entries: Vec<ScanEntry> = Vec::new();
        assert_eq!(total_all(&entries), 0);
        assert_eq!(total_checked(&entries, &HashSet::new()), 0);
        assert!(group_by_category(&entries).is_empty());
        assert!(group_by_risk(&entries).is_empty());
        assert!(default_selection(&entries).is_empty());
        let mut none: Vec<ScanEntry> = Vec::new();
        dedup_parent_child(&mut none); // 空切片不 panic
    }

    /// ㊲ 无法访问项的合计口径：`inaccessible` 只计数展示，不冲减合计。
    #[test]
    fn inaccessible_not_netted_from_total() {
        let report = ScanReport {
            entries: vec![ent("pip_cache", 50)],
            missing: 2,
            inaccessible: 3,
            skipped_mounts: 1,
            cancelled: false,
        };
        assert_eq!(total_all(&report.entries), 50, "合计只由条目字节构成");
        assert_eq!(report.inaccessible, 3, "无法访问 N 项单独展示");
        assert_eq!(report.missing, 2, "缺失跳过单独展示");
    }

    /// ㊳ 只报项（D9.9）：不进合计、不进组计数，但仍在组内列出（灰显展示）。
    #[test]
    fn report_only_excluded_from_totals() {
        let entries = vec![ent("apt_archives", 5), ent("docker_storage", 999)];
        assert_eq!(total_all(&entries), 5, "只报项 999 不进合计");
        assert_eq!(total_checked(&entries, &default_selection(&entries)), 5);
        let groups = group_by_category(&entries);
        let docker = groups
            .iter()
            .find(|g| g.category == Category::Container)
            .unwrap();
        assert_eq!(docker.indices.len(), 1, "只报项仍在组内（展示灰显）");
        assert_eq!(docker.bytes, 0, "但不进组计数");
        let pkg = groups
            .iter()
            .find(|g| g.category == Category::PkgCache)
            .unwrap();
        assert_eq!(pkg.bytes, 5);
    }

    /// ㊴ 按类别分组：十大类各归其位、组序固定、Σ组 = total_all、组内体积降序。
    #[test]
    fn group_by_category_partitions_all() {
        let entries = vec![
            ent("apt_archives", 1),   // PkgCache
            ent("var_log", 2),        // SystemLog
            ent("tmp_dir", 3),        // TempFiles
            ent("user_cache", 4),     // UserCache
            ent("boot_vmlinuz", 5),   // OldKernel
            ent("var_crash", 6),      // CrashDebug
            ent("npm_cache", 7),      // DevCache
            ent("cups_cache", 8),     // DesktopSvc
            ent("pat_tmp", 9),        // GenericPattern
            ent("docker_storage", 0), // Container（只报，bytes 不计）
            ent("log_apt", 10),       // SystemLog 第二条（测组内降序）
        ];
        let groups = group_by_category(&entries);
        assert_eq!(groups.len(), 10, "十类各有一组");
        let order: Vec<Category> = groups.iter().map(|g| g.category).collect();
        let mut expect = Category::ALL.to_vec();
        expect.retain(|c| groups.iter().any(|g| g.category == *c));
        assert_eq!(order, expect, "组序 = 十大类固定顺序");
        let sl = groups
            .iter()
            .find(|g| g.category == Category::SystemLog)
            .unwrap();
        assert_eq!(
            sl.indices,
            vec![10, 1],
            "组内按体积降序（log_apt 10 > var_log 2）"
        );
        let sum: u64 = groups.iter().map(|g| g.bytes).sum();
        assert_eq!(sum, total_all(&entries), "Σ 组计数 = total_all");
    }

    /// ㊵ 按风险分组：安全/按需/谨慎三桶，成员正确、只报项不计数。
    #[test]
    fn group_by_risk_buckets() {
        let entries = vec![
            ent("apt_archives", 100),     // 安全
            ent("pip_cache", 50),         // 按需
            ent("journal_log", 10),       // 谨慎
            ent("huggingface_cache", 90), // 按需 + 只报
        ];
        let groups = group_by_risk(&entries);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].risk, Risk::Safe);
        assert_eq!(groups[0].bytes, 100);
        assert_eq!(groups[1].risk, Risk::OnDemand);
        assert_eq!(groups[1].bytes, 50, "只报的 huggingface 90 不计");
        assert_eq!(
            groups[1].indices,
            vec![3, 1],
            "按需桶两条都在（组内降序 90>50）"
        );
        assert_eq!(groups[2].risk, Risk::Caution);
        assert_eq!(groups[2].bytes, 10);
    }

    // ── 9.5：清理命令构造与执行 ──

    fn cat(id: &str) -> &'static crate::model::cleaner::CleanTarget {
        crate::model::cleaner::by_id(id).expect("catalog id")
    }

    /// ㉟-b journal 专用命令（D9.8）：pkexec + `--vacuum-size=200M`。
    #[test]
    fn cmd_journal_vacuum_200m() {
        let o = opts_in(Path::new("/home/alice"));
        let a = cleanup_action(cat("journal_log"), &o).unwrap();
        assert_eq!(a.argv, vec!["pkexec", "journalctl", "--vacuum-size=200M"]);
        assert!(a.needs_root, "/var/log/journal 系统级");
        assert!(a.display.contains("--vacuum-size=200M"));
    }

    /// ㊶-b 包管理器与工具命令表：dnf/apt/pacman/npm/flatpak。
    #[test]
    fn cmd_package_manager_table() {
        let o = opts_in(Path::new("/home/alice"));
        // 系统级 → pkexec 包裹
        assert_eq!(
            cleanup_action(cat("dnf_cache"), &o).unwrap().argv,
            vec!["pkexec", "dnf", "clean", "all"]
        );
        assert_eq!(
            cleanup_action(cat("apt_archives"), &o).unwrap().argv,
            vec!["pkexec", "apt-get", "clean"]
        );
        assert_eq!(
            cleanup_action(cat("pacman_pkg"), &o).unwrap().argv,
            vec!["pkexec", "pacman", "-Sc", "--noconfirm"]
        );
        // 主目录下 → 用户级，不提权
        let npm = cleanup_action(cat("npm_cache"), &o).unwrap();
        assert_eq!(npm.argv, vec!["npm", "cache", "clean", "--force"]);
        assert!(!npm.needs_root);
        assert_eq!(
            npm.argv,
            vec!["npm", "cache", "clean", "--force"],
            "npm 命令逐字核对"
        );
        let fp = cleanup_action(cat("flatpak_unused"), &o).unwrap();
        assert_eq!(fp.argv, vec!["flatpak", "uninstall", "--unused"]);
        assert!(!fp.needs_root);
    }

    /// ㊷ 旧内核四家提示（D9.4）+ 容器/HF 提示 + 非 Hint 无建议。
    #[test]
    fn hint_commands_per_pm() {
        let bm = cat("boot_vmlinuz");
        assert!(
            hint_command_for(bm, PkgManager::Pacman)
                .unwrap()
                .contains("pacman -R"),
            "pacman -R 提示"
        );
        assert!(
            hint_command_for(bm, PkgManager::Dnf)
                .unwrap()
                .contains("dnf remove --oldinstallonly")
        );
        assert_eq!(
            hint_command_for(bm, PkgManager::Apt).unwrap(),
            "apt autoremove"
        );
        assert!(
            hint_command_for(bm, PkgManager::Zypper)
                .unwrap()
                .contains("zypper purge-kernels")
        );
        assert!(
            hint_command_for(cat("docker_storage"), PkgManager::Dnf)
                .unwrap()
                .contains("docker system prune")
        );
        assert!(hint_command_for(cat("huggingface_cache"), PkgManager::Dnf).is_some());
        // 普通条目没有「建议命令」语义。
        assert!(hint_command_for(cat("user_cache"), PkgManager::Dnf).is_none());
    }

    /// ㊸ 白名单三重校验：catalog 外拒绝、路径篡改拒绝、空目标拒绝。
    #[test]
    fn whitelist_rejects_foreign_tampered_empty() {
        let o = opts_in(Path::new("/home/alice"));
        // ① catalog 外条目
        let fake = crate::model::cleaner::CleanTarget {
            id: "evil_entry",
            category: Category::UserCache,
            path: "/etc",
            desc: "伪造条目",
            risk: Risk::Caution,
            cleanup_kind: crate::model::cleaner::CleanupKind::Direct,
            pattern: None,
        };
        let err = cleanup_action(&fake, &o).unwrap_err();
        assert!(err.contains("白名单"), "白名单外路径应被拒：{}", err);
        // ② 合法 id + 篡改路径
        let mut tampered = *cat("user_cache");
        tampered.path = "/etc/";
        let err = cleanup_action(&tampered, &o).unwrap_err();
        assert!(err.contains("不一致"), "路径篡改应被拒：{}", err);
        // ③ 空 id / 空路径
        let mut empty_id = *cat("user_cache");
        empty_id.id = "";
        assert!(
            cleanup_action(&empty_id, &o)
                .unwrap_err()
                .contains("空目标")
        );
        let mut empty_path = *cat("user_cache");
        empty_path.path = "";
        assert!(
            cleanup_action(&empty_path, &o)
                .unwrap_err()
                .contains("空目标")
        );
    }

    /// ㊹ Direct 的 rm 路径只来自 catalog（系统级包 pkexec、用户级不包）。
    #[test]
    fn direct_rm_paths_from_catalog_only() {
        let home = Path::new("/home/alice");
        let o = opts_in(home);
        assert_eq!(
            cleanup_action(cat("cups_cache"), &o).unwrap().argv,
            vec!["pkexec", "rm", "-rf", "--", "/var/cache/cups/"]
        );
        assert_eq!(
            cleanup_action(cat("user_cache"), &o).unwrap().argv,
            vec!["rm", "-rf", "--", "/home/alice/.cache/"]
        );
    }

    /// ㊺ Hint 项不提供执行动作（只出建议命令）。
    #[test]
    fn hint_target_has_no_action() {
        let o = opts_in(Path::new("/home/alice"));
        let err = cleanup_action(cat("docker_storage"), &o).unwrap_err();
        assert!(err.contains("只提示"), "{}", err);
        let err = cleanup_action(cat("boot_vmlinuz"), &o).unwrap_err();
        assert!(err.contains("只提示"), "旧内核只提示不自删（D9.4）");
    }

    /// ㊻ /tmp /var/tmp 走 find + 天数换算分钟（与 D9.7 扫描阈值同源）。
    #[test]
    fn tmp_targets_use_find_with_age_threshold() {
        let o = opts_in(Path::new("/home/alice"));
        let a = cleanup_action(cat("tmp_dir"), &o).unwrap();
        assert_eq!(
            a.argv,
            vec![
                "pkexec", "find", "/tmp/", "-xdev", "-type", "f", "-mmin", "+10080", "-delete"
            ],
            "默认 7 天 = 7*1440 = 10080 分钟"
        );
        let mut o1 = opts_in(Path::new("/home/alice"));
        o1.tmp_min_age_days = 1;
        assert!(
            cleanup_action(cat("var_tmp"), &o1)
                .unwrap()
                .argv
                .iter()
                .any(|s| s == "+1440"),
            "阈值可调（1 天 = 1440 分钟）"
        );
        // wtmp 类 truncate 命令
        assert_eq!(
            cleanup_action(cat("log_wtmp"), &o).unwrap().argv,
            vec!["pkexec", "truncate", "-s", "0", "/var/log/wtmp"]
        );
    }

    /// ㊼ needs_root 矩阵：主目录与本用户 run/user 免提权，其余系统级。
    #[test]
    fn needs_root_matrix() {
        let home = Path::new("/home/alice");
        assert!(!needs_root("/home/alice/.cache/", home, 1000));
        assert!(!needs_root("/home/alice", home, 1000));
        assert!(
            !needs_root("/run/user/1000/bus", home, 1000),
            "本用户会话目录"
        );
        assert!(needs_root("/run/user/2000/bus", home, 1000), "别的用户");
        assert!(needs_root("/var/cache/dnf/", home, 1000));
        assert!(needs_root("/tmp/", home, 1000));
    }

    /// ㊽ 执行（注入 runner）：构造 rm → 真删 → 回收量 = 前后字节差。
    #[test]
    fn execute_reclaimed_is_byte_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let o = opts_in(tmp.path());
        let thumbs = tmp.path().join(".cache/thumbnails");
        std::fs::create_dir_all(&thumbs).unwrap();
        std::fs::write(thumbs.join("a.bin"), vec![1u8; 100]).unwrap();
        std::fs::write(thumbs.join("b.bin"), vec![1u8; 50]).unwrap();
        let out = execute_cleanup_with(cat("thumbnails_cache"), &o, |argv| {
            let expect = format!("rm,-rf,--,{}/", thumbs.to_str().unwrap());
            assert_eq!(argv.join(","), expect, "runner 收到的是白名单内的 rm 命令");
            std::fs::remove_dir_all(&thumbs).unwrap(); // 模拟 rm 的效果
            Ok((true, String::new()))
        })
        .unwrap();
        assert_eq!(out.before, 150);
        assert_eq!(out.after, 0);
        assert_eq!(out.reclaimed, 150, "实际回收 = 清理前后扫描差");
        assert!(out.status_ok);
    }

    /// ㊾ 回收量与 RC 解耦：删了但 RC≠0 照样报回收；RC=0 没删则回收 0。
    #[test]
    fn reclaimed_independent_of_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let o = opts_in(tmp.path());
        let thumbs = tmp.path().join(".cache/thumbnails");
        std::fs::create_dir_all(&thumbs).unwrap();
        std::fs::write(thumbs.join("a.bin"), vec![1u8; 80]).unwrap();
        // 案例 A：删了，但命令报失败（RC=1）→ reclaimed 仍 = 80。
        let out = execute_cleanup_with(cat("thumbnails_cache"), &o, |_| {
            std::fs::remove_dir_all(&thumbs).unwrap();
            Ok((false, "boom".to_string()))
        })
        .unwrap();
        assert_eq!(out.reclaimed, 80, "不凭 RC 宣称回收，凭字节差");
        assert!(!out.status_ok);
        assert_eq!(out.stderr, "boom");
        // 案例 B：命令成功但什么都没删 → reclaimed = 0，不得虚报。
        std::fs::create_dir_all(&thumbs).unwrap();
        std::fs::write(thumbs.join("a.bin"), vec![1u8; 80]).unwrap();
        let out = execute_cleanup_with(cat("thumbnails_cache"), &o, |_| Ok((true, String::new())))
            .unwrap();
        assert_eq!(out.reclaimed, 0, "没删到东西就不许宣称回收");
        assert!(out.status_ok);
    }

    /// ㊿ 目标不存在：不发命令直接报错（runner 不会被调用）。
    #[test]
    fn execute_missing_target_never_runs_command() {
        let tmp = tempfile::tempdir().unwrap();
        let o = opts_in(tmp.path()); // 无 .cache/thumbnails
        let called = std::cell::Cell::new(false);
        let err = execute_cleanup_with(cat("thumbnails_cache"), &o, |_| {
            called.set(true);
            Ok((true, String::new()))
        })
        .unwrap_err();
        assert!(err.contains("无事可做"), "{}", err);
        assert!(!called.get(), "空目标不发命令");
    }

    /// 51 真实 runner 超时会杀进程（pkexec 无人值守永挂的防线，GOAL 8.3 教训）。
    #[test]
    fn run_cmd_timeout_kills_and_errors() {
        let argv: Vec<String> = ["sleep", "30"].iter().map(|s| s.to_string()).collect();
        let err = run_cmd(&argv, Some(Duration::from_millis(300))).unwrap_err();
        assert!(err.contains("超时"), "{}", err);
        // 正常命令：捕获退出码与 stderr。
        let argv: Vec<String> = ["sh", "-c", "echo boom >&2; exit 1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (ok, stderr) = run_cmd(&argv, Some(Duration::from_secs(5))).unwrap();
        assert!(!ok);
        assert!(stderr.contains("boom"), "stderr 应被捕获：{}", stderr);
    }

    /// 9.7-⑤：批量清理中单条失败（执行器报错）与白名单外条目都**不中断其余**——
    /// 失败在前、成功在后，逐条回报且进度回调对每一条都触发。
    #[test]
    fn batch_single_failure_does_not_stop_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let opts = opts_in(tmp.path());
        // 第 1 条执行器报错；第 2 条不在 catalog 白名单（跳过）；第 3 条仍须执行。
        let ids = vec![
            "fontconfig_cache".to_string(),
            "bogus_not_in_catalog".to_string(),
            "thumbnails_cache".to_string(),
        ];
        let calls = std::cell::Cell::new(0usize);
        let ticks = std::cell::Cell::new(0usize);
        let out = execute_cleanup_batch_with(
            &ids,
            &opts,
            |t, _| {
                calls.set(calls.get() + 1);
                if t.id == "fontconfig_cache" {
                    Err("权限不足（文件被占用）".to_string())
                } else {
                    Ok(CleanupOutcome {
                        before: 500,
                        after: 200,
                        reclaimed: 300,
                        status_ok: true,
                        stderr: String::new(),
                    })
                }
            },
            |_, _, _| ticks.set(ticks.get() + 1),
        );
        assert_eq!(
            calls.get(),
            2,
            "首条报错后仍须执行到最后一条真目标（bogus 不发执行）"
        );
        assert_eq!(ticks.get(), 3, "含白名单外条目，进度回调每条都触发");
        assert_eq!(out.done, 1, "只有成功拿到 Outcome 的算完成");
        assert_eq!(out.reclaimed, 300);
        assert_eq!(out.failures.len(), 2, "逐条回报：执行器错误 + 白名单外跳过");
        assert!(out.failures[0].contains("fontconfig_cache"));
        assert!(out.failures[0].contains("权限不足"));
        assert!(out.failures[1].contains("bogus_not_in_catalog"));
        assert!(out.failures[1].contains("白名单"));
    }

    /// 52 真机安全类实跑取证（`--ignored --nocapture`）：清 ~/.cache 缩略图（【安全】可再生），
    /// 打印前后扫描字节并断言确实回收；终端侧另贴 `du -sh` 前后对照。
    #[ignore = "真机取证：删除 ~/.cache 下【安全】可再生目录，需 cargo test -- --ignored 跑"]
    #[test]
    fn real_machine_safe_cleanup_evidence() {
        let opts = ScanOptions::new();
        for id in ["thumbnails_cache", "thumbnails_home"] {
            let t = cat(id);
            let Some(st) = scan_target(t, &opts) else {
                continue;
            };
            if st.bytes == 0 {
                continue;
            }
            println!("== 目标 {} ({}) ==", t.id, opts.expand(t.path));
            println!("清理前扫描: {} bytes / {} files", st.bytes, st.files);
            let out = execute_cleanup(t, &opts).expect("执行清理失败");
            println!("执行: status_ok={} stderr={:?}", out.status_ok, out.stderr);
            println!(
                "回收: before={} after={} reclaimed={}",
                out.before, out.after, out.reclaimed
            );
            match scan_target(t, &opts) {
                None => println!("复扫: 目标已不存在（= 删净）"),
                Some(s) => println!("复扫: {} bytes / {} files", s.bytes, s.files),
            }
            assert!(out.reclaimed > 0, "应有实际回收量");
            return;
        }
        panic!("本机没有非空的缩略图目录，真机取证无法完成");
    }
}
