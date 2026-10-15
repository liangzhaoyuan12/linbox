//! `/etc/fstab` 解析 / 序列化（纯逻辑，不依赖 GTK / libadwaita / glib）。
//!
//! 负责：把 fstab 文本按行交错模型（[`FstabDocument`]）解析、未修改行**逐字节保真**
//! 地写回、字段空白转义，以及给写前校验闸门（`validate`，GOAL 7.4）用的纯检查。
//! 提权读写走 [`crate::utils::env_editor`] 的既有模型（D7.1/D7.2）。
//!
//! ⚠️ 本模块对**坏行**的拦截是 `findmnt --verify` 的防崩前置（2026-10-02 实测）：
//! - 字段数 <3 的行 → findmnt 输出 parse error 后**段错误退出（RC=139）**；
//! - dump/pass 含非数字 → 同样 parse error + 段错误；
//! - 根因疑似「文件里 0 条有效行时 --verify 崩溃」，无论根因为何，结论一致：
//!   **未通过本解析器的文本绝不能进 `findmnt --verify`**（GOAL 7.4 流程据此设计）。
//! - 反例：字段数 7~8（行内多余词）findmnt 反而 0 parse errors（宽容忽略）。
//!
//! 约束（见 `docs/项目结构规划书.md` §3.7）：本文件禁止 `use gtk` / `use adw` /
//! `use glib`，输入输出均为数据。

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::model::fstab::{FstabDocument, FstabEntry, FstabLine};
use crate::utils::env_editor;

/// fstab 文件路径（读写目标；测试与 dry-run 通过参数传入别的路径）。
pub const FSTAB_PATH: &str = "/etc/fstab";

// ---------------------------------------------------------------------------
// 解析
// ---------------------------------------------------------------------------

/// 解析 fstab 文本为行交错文档。
///
/// 规则（对应 fstab(5) 与 GOAL 7.2）：
/// - 空行 → [`FstabLine::Blank`]（原样保留，含仅空白字符的行）；
/// - `#` 开头 → 先尝试按「被注释停用的条目」识别（D7.5 的停用形态），
///   识别不通过则为 [`FstabLine::Comment`]（原样保留）；
/// - 其余行 → 条目，**字段数 <4 或 dump/pass 非数字即 Err（带行号）**——
///   这是 `findmnt --verify` 段错误的防线；
/// - 字段数 >6 → 取前 6 个（libmount 实测宽容忽略多余字段；原行进 raw，
///   未修改仍逐字节保真，只有用户编辑该行时尾部才会丢）。
///
/// `dump` / `pass` 缺省按 `0` 补（4/5 字段行）。
pub fn parse(text: &str) -> Result<FstabDocument, String> {
    let mut doc = FstabDocument::new();
    if text.is_empty() {
        return Ok(doc);
    }
    doc.trailing_newline = text.ends_with('\n');
    let body = text.strip_suffix('\n').unwrap_or(text);

    for (idx, line) in body.split('\n').enumerate() {
        let line_no = idx + 1;
        let t = line.trim_start();
        if t.is_empty() {
            doc.push_line(FstabLine::Blank(line.to_string()));
        } else if let Some(hash) = t.strip_prefix('#') {
            // 注释行：先看是不是「停用的条目」（识别要求比活动行更严，
            // 否则普通英文注释会被误认成条目）。
            let candidate = hash.trim_start();
            match try_disabled_entry(candidate) {
                Some(mut e) => {
                    e.line_no = line_no;
                    e.disabled = true;
                    e.raw = Some(line.to_string());
                    doc.push_line(FstabLine::Entry(e));
                }
                None => doc.push_line(FstabLine::Comment(line.to_string())),
            }
        } else {
            let mut e = parse_active_line(line, line_no)?;
            e.line_no = line_no; // 活动条目同样记录行号（set_entry_disabled 按行号定位）
            e.raw = Some(line.to_string());
            doc.push_line(FstabLine::Entry(e));
        }
    }
    Ok(doc)
}

/// 解析一行活动条目：≥4 字段、dump/pass 为纯数字，否则返回带行号的错误。
fn parse_active_line(line: &str, line_no: usize) -> Result<FstabEntry, String> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 4 {
        return Err(format!(
            "第 {line_no} 行：字段数 {} < 4（fstab(5) 要求至少 source mountpoint vfstype options）：{}",
            fields.len(),
            line.trim()
        ));
    }
    build_entry(&fields, line_no)
        .map_err(|reason| format!("第 {line_no} 行：{reason}：{}", line.trim()))
}

/// 由字段构造条目并做字段级校验（dump/pass 纯数字；缺省补 0）。
fn build_entry(fields: &[&str], _line_no: usize) -> Result<FstabEntry, String> {
    let dump = fields.get(4).copied().unwrap_or("0");
    let pass = fields.get(5).copied().unwrap_or("0");
    if !dump.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "dump 字段非数字「{dump}」（会导致 findmnt --verify 崩溃）"
        ));
    }
    if !pass.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "pass 字段非数字「{pass}」（会导致 findmnt --verify 崩溃）"
        ));
    }
    let mut e = FstabEntry::new(fields[0], fields[1], fields[2], fields[3]);
    e.dump = dump.to_string();
    e.pass = pass.to_string();
    Ok(e)
}

/// 识别「被 `#` 注释掉的条目」：必须同时满足
/// ① ≥4 字段且 dump/pass 数字；② source 形态（`UUID=`/`LABEL=`/`/dev/`/`none`/含 `:`）；
/// ③ 挂载点形态（`/` 开头或 `none`）；④ fstype 形态（无空白、无 `/`）。
///
/// 四条缺一即判定为普通注释——本机 anaconda 注释头（`# Created by anaconda…`、
/// `# See man pages fstab(5)…` 等）经此规则全部留在 Comment，不误认。
fn try_disabled_entry(body: &str) -> Option<FstabEntry> {
    let fields: Vec<&str> = body.split_whitespace().collect();
    if fields.len() < 4 || fields.len() > 8 {
        return None;
    }
    let src = fields[0];
    let src_ok = src.starts_with("UUID=")
        || src.starts_with("LABEL=")
        || src.starts_with("PARTUUID=")
        || src.starts_with("PARTLABEL=")
        || src.starts_with("/dev/")
        || src == "none"
        || src.contains(':');
    if !src_ok {
        return None;
    }
    let mp = fields[1];
    if !(mp.starts_with('/') || mp == "none") {
        return None;
    }
    let fstype = fields[2];
    if fstype.is_empty() || fstype.contains('/') || fstype.contains('\\') {
        return None;
    }
    build_entry(&fields, 0).ok()
}

// ---------------------------------------------------------------------------
// 序列化
// ---------------------------------------------------------------------------

/// 序列化回文本：逐行输出（条目未修改用 raw 原样），按 `trailing_newline` 补结尾换行。
///
/// **未修改文档的 parse → serialize 必须逐字节相等**（GOAL D7.4，golden 测试背书）。
pub fn serialize(doc: &FstabDocument) -> String {
    let mut out = String::new();
    for line in &doc.lines {
        out.push_str(&line.to_line_string());
        out.push('\n');
    }
    if !doc.trailing_newline {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------------
// 字段转义（展示层用；写回一律用 model 的 escape_field）
// ---------------------------------------------------------------------------

/// 反转义 fstab/proc 八进制转义（`\040` 空格、`\011` 制表符），仅用于展示。
/// 与 `utils::monitor::sensors::unescape_mount` 同规则；**写回时不要用它**——
/// 模型里存的永远是原文（含转义），反转义后再序列化会破坏保真。
pub fn unescape_field(s: &str) -> String {
    s.replace("\\040", " ").replace("\\011", "\t")
}

/// 挂载点规范化（比较用）：去掉尾部 `/`（根 `/` 除外），让 `/home/` 与 `/home` 相等。
pub fn normalize_mountpoint(mp: &str) -> &str {
    let t = mp.trim_end_matches('/');
    if t.is_empty() { "/" } else { t }
}

// ---------------------------------------------------------------------------
// 纯检查（GOAL 7.2 边界 / 7.4 校验的共享件）
// ---------------------------------------------------------------------------

/// 重复挂载点（启用中的条目）→ 出现次数表（告警用，**不是**解析错误）。
pub fn duplicate_mountpoints(doc: &FstabDocument) -> BTreeMap<String, usize> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for e in doc.entries().filter(|e| !e.disabled) {
        let key = normalize_mountpoint(&e.mountpoint).to_string();
        *seen.entry(key).or_insert(0) += 1;
    }
    seen.retain(|_, n| *n > 1);
    seen
}

// ---------------------------------------------------------------------------
// 读取（D7.1：直读优先，权限不足走 env_editor 的 pkexec 兜底）
// ---------------------------------------------------------------------------

/// 读取 fstab 内容与「当前用户是否可直写」。
/// 返回 `(内容, 可直写)`；文件不存在时内容为 `None`。
pub fn read_fstab() -> Result<(Option<String>, bool), String> {
    env_editor::load(FSTAB_PATH)
}

// ---------------------------------------------------------------------------
// 挂载状态对照（GOAL 7.3）
// ---------------------------------------------------------------------------

/// `findmnt --json` 输出里的一条挂载记录（树已展平）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountedFs {
    /// 挂载点（findmnt 的 target）。
    pub target: String,
    /// 来源设备（source）。
    pub source: String,
    /// 文件系统类型。
    pub fstype: String,
}

/// 展平 `findmnt --json` 的嵌套 `filesystems` 树 → 挂载点列表。
pub fn parse_findmnt_json(text: &str) -> Result<Vec<MountedFs>, String> {
    let v: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("findmnt JSON 解析失败：{e}"))?;
    let arr = v
        .get("filesystems")
        .and_then(|x| x.as_array())
        .ok_or_else(|| "findmnt JSON 缺少 filesystems 数组".to_string())?;
    let mut out = Vec::new();
    flatten_findmnt(arr, &mut out);
    Ok(out)
}

fn flatten_findmnt(nodes: &[serde_json::Value], out: &mut Vec<MountedFs>) {
    for n in nodes {
        let target = n.get("target").and_then(|x| x.as_str()).unwrap_or("");
        if !target.is_empty() {
            out.push(MountedFs {
                target: target.to_string(),
                source: n
                    .get("source")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                fstype: n
                    .get("fstype")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
        if let Some(children) = n.get("children").and_then(|x| x.as_array()) {
            flatten_findmnt(children, out);
        }
    }
}

/// 把 fstab 的 source 解析为可存在性检查的设备路径。
///
/// 返回 `None` = 无法本地判定（NFS/CIFS 远程源等）→ 存在性按「未知=不阻断」处理。
pub fn resolve_source(source: &str) -> Option<PathBuf> {
    if let Some(id) = source.strip_prefix("UUID=") {
        return Some(PathBuf::from("/dev/disk/by-uuid").join(id));
    }
    if let Some(id) = source.strip_prefix("LABEL=") {
        return Some(PathBuf::from("/dev/disk/by-label").join(id));
    }
    if let Some(id) = source.strip_prefix("PARTUUID=") {
        return Some(PathBuf::from("/dev/disk/by-partuuid").join(id));
    }
    if let Some(id) = source.strip_prefix("PARTLABEL=") {
        return Some(PathBuf::from("/dev/disk/by-partlabel").join(id));
    }
    if source.starts_with("/dev/") {
        return Some(PathBuf::from(source));
    }
    None
}

/// 计算一条条目的挂载状态（纯函数：IO 结果作为参数传入）。
///
/// 优先级：**已挂载（现实优先） > 已停用 > 挂载点不存在 > 源不存在 > 未挂载**。
/// `mountpoint == "none"`（swap）不做挂载点目录检查。
pub fn mount_status(
    entry: &FstabEntry,
    mounted: &HashSet<String>,
    dir_exists: bool,
    source_exists: bool,
) -> crate::model::fstab::MountStatus {
    use crate::model::fstab::MountStatus as S;
    let norm = normalize_mountpoint(&entry.mountpoint).to_string();
    if mounted.contains(&norm) {
        return S::Mounted;
    }
    if entry.disabled {
        return S::Disabled;
    }
    if entry.mountpoint != "none" && !dir_exists {
        return S::DirMissing;
    }
    if !source_exists {
        return S::SourceMissing;
    }
    S::NotMounted
}

/// 一次调用取全量状态（IO：挂载表 + 目录/设备存在性）。
/// 挂载表来自 `findmnt --json`（调用方负责取文本），目录/设备用 `Path::exists` 实测。
pub fn status_table(
    findmnt_json: &str,
    doc: &FstabDocument,
) -> Result<Vec<crate::model::fstab::MountStatus>, String> {
    let mounted_fs = parse_findmnt_json(findmnt_json)?;
    let mounted: HashSet<String> = mounted_fs
        .iter()
        .map(|m| normalize_mountpoint(&m.target).to_string())
        .collect();
    Ok(doc
        .entries()
        .map(|e| {
            let dir_exists = e.mountpoint == "none" || Path::new(&e.mountpoint).exists();
            let source_exists = match resolve_source(&e.source) {
                Some(p) => p.exists(),
                None => true, // 远程源/无法本地判定：不误报「源不存在」
            };
            mount_status(e, &mounted, dir_exists, source_exists)
        })
        .collect())
}

/// 执行 `findmnt --json`（后台线程调用；页面层负责线程，见 GOAL 3.2）。
pub fn run_findmnt_json() -> Result<String, String> {
    let out = std::process::Command::new("findmnt")
        .arg("--json")
        .output()
        .map_err(|e| format!("无法启动 findmnt：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "findmnt 退出码 {}：{}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("findmnt 输出非 UTF-8：{e}"))
}

// ---------------------------------------------------------------------------
// 分区枚举（GOAL 7.3：新增条目时选分区）
// ---------------------------------------------------------------------------

/// `lsblk` 里的一块可挂载分区（磁盘的叶子分区，含 LVM/加密层下的 part）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockPartition {
    /// 设备路径（`/dev/nvme1n1p2`）。
    pub path: String,
    /// 设备名（`nvme1n1p2`）。
    pub name: String,
    /// 文件系统类型（`None` = 未格式化）。
    pub fstype: Option<String>,
    /// 容量文本（lsblk 原样，如 `476.9G`）。
    pub size: String,
    /// 卷标。
    pub label: Option<String>,
    /// UUID（新增条目时写 `UUID=` 的依据）。
    pub uuid: Option<String>,
    /// 是否已挂载（`mountpoints` 非空）。
    pub mounted: bool,
}

/// 解析 `lsblk -J -o NAME,PATH,FSTYPE,SIZE,LABEL,UUID,MOUNTPOINTS` 输出，
/// 递归收集 **type == "part"** 的节点（整盘无分区表时不收——那是格式化流程的活）。
pub fn parse_lsblk_json(text: &str) -> Result<Vec<BlockPartition>, String> {
    let v: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("lsblk JSON 解析失败：{e}"))?;
    let arr = v
        .get("blockdevices")
        .and_then(|x| x.as_array())
        .ok_or_else(|| "lsblk JSON 缺少 blockdevices 数组".to_string())?;
    let mut out = Vec::new();
    flatten_lsblk(arr, &mut out);
    Ok(out)
}

fn flatten_lsblk(nodes: &[serde_json::Value], out: &mut Vec<BlockPartition>) {
    for n in nodes {
        let is_part = n.get("type").and_then(|x| x.as_str()) == Some("part");
        if is_part {
            let mounted = n
                .get("mountpoints")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().any(|m| m.is_string()))
                .unwrap_or(false);
            out.push(BlockPartition {
                path: n
                    .get("path")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                name: n
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                fstype: n.get("fstype").and_then(|x| x.as_str()).map(String::from),
                size: n
                    .get("size")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                label: n.get("label").and_then(|x| x.as_str()).map(String::from),
                uuid: n.get("uuid").and_then(|x| x.as_str()).map(String::from),
                mounted,
            });
        }
        if let Some(children) = n.get("children").and_then(|x| x.as_array()) {
            flatten_lsblk(children, out);
        }
    }
}

/// 设备树中的一个分区（`lsblk -b` 字节口径，7.7 图形分区条用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LsblkPart {
    pub path: String,
    pub name: String,
    pub fstype: Option<String>,
    /// 字节数（`lsblk -b`，JSON number）。
    pub size_bytes: u64,
    pub label: Option<String>,
    pub uuid: Option<String>,
    pub mountpoints: Vec<String>,
}

/// 一块磁盘（含其分区；无分区表的整盘 `children` 为空，`parts` 收整盘节点）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LsblkDisk {
    pub path: String,
    pub name: String,
    pub size_bytes: u64,
    pub parts: Vec<LsblkPart>,
}

impl LsblkDisk {
    /// 条形图分段：`parse_lsblk_tree` 保证恒非空（无分区表的整盘已把
    /// 盘节点自身降级为唯一分段）。
    pub fn segments(&self) -> &[LsblkPart] {
        &self.parts
    }
}

/// 解析 `lsblk -J -b -o NAME,PATH,TYPE,SIZE,FSTYPE,LABEL,UUID,MOUNTPOINTS`：
/// 顶层 `type == "disk"` 节点为盘，`children` 里的 `type == "part"` 为分区；
/// 无 children 的整盘（zram、未分区盘）把盘节点自身收为唯一分段（条形图不空）。
pub fn parse_lsblk_tree(text: &str) -> Result<Vec<LsblkDisk>, String> {
    let v: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("lsblk JSON 解析失败：{e}"))?;
    let arr = v
        .get("blockdevices")
        .and_then(|x| x.as_array())
        .ok_or_else(|| "lsblk JSON 缺少 blockdevices 数组".to_string())?;
    let mut disks = Vec::new();
    for n in arr {
        let ty = n.get("type").and_then(|x| x.as_str()).unwrap_or("");
        if ty != "disk" {
            continue;
        }
        let node = lsblk_part(n);
        let mut parts: Vec<LsblkPart> = Vec::new();
        if let Some(children) = n.get("children").and_then(|x| x.as_array()) {
            for c in children {
                if c.get("type").and_then(|x| x.as_str()) == Some("part") {
                    parts.push(lsblk_part(c));
                }
            }
        }
        if parts.is_empty() {
            // 整盘一段：盘节点自身降级为分段（名字/大小/挂载信息都在盘节点上）。
            parts.push(node.clone());
        }
        disks.push(LsblkDisk {
            path: node.path,
            name: node.name,
            size_bytes: node.size_bytes,
            parts,
        });
    }
    Ok(disks)
}

/// 单节点抽取为分段结构（盘节点降级分段时复用同一构造）。
fn lsblk_part(n: &serde_json::Value) -> LsblkPart {
    let name = n
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let path = n
        .get("path")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let fstype = n.get("fstype").and_then(|x| x.as_str()).map(String::from);
    // `-b` 下 size 是 number；容错字符串（个别版本/合成 fixture）。
    let size_bytes = match n.get("size") {
        Some(x) if x.is_u64() => x.as_u64().unwrap_or(0),
        Some(x) if x.is_string() => x.as_str().unwrap_or("0").parse().unwrap_or(0),
        _ => 0,
    };
    let label = n.get("label").and_then(|x| x.as_str()).map(String::from);
    let uuid = n.get("uuid").and_then(|x| x.as_str()).map(String::from);
    let mountpoints = n
        .get("mountpoints")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| m.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    LsblkPart {
        path,
        name,
        fstype,
        size_bytes,
        label,
        uuid,
        mountpoints,
    }
}

/// 字节数 → 人类可读（分区条 tooltip / 信息面板用）。
pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// 解析 `df -B1 --output=target,size,used`：表头 locale 相关（中文机器是
/// 「挂载点 1B的块 已用」），按**位置**解析——最后两词是 (used, size) 数字、
/// 其前整段是挂载点（容忍多空格填充、挂载点含空格、中文表头行）。
pub fn parse_df_usage(text: &str) -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        // 记录每个词的字节区间 (start, end)。
        let mut words: Vec<(usize, usize)> = Vec::new();
        let mut start: Option<usize> = None;
        for (i, c) in line.char_indices() {
            if c.is_whitespace() {
                if let Some(s) = start.take() {
                    words.push((s, i));
                }
            } else if start.is_none() {
                start = Some(i);
            }
        }
        if let Some(s) = start {
            words.push((s, line.len()));
        }
        if words.len() < 3 {
            continue;
        }
        let used_w = words[words.len() - 1];
        let size_w = words[words.len() - 2];
        let Ok(used) = line[used_w.0..used_w.1].parse::<u64>() else {
            continue; // 表头行
        };
        let Ok(size) = line[size_w.0..size_w.1].parse::<u64>() else {
            continue;
        };
        let target = line[..size_w.0].trim().to_string();
        if !target.is_empty() {
            out.push((target, size, used));
        }
    }
    out
}

/// 执行 `df -B1 --output=target,size,used`（后台线程调用）。
pub fn run_df_usage() -> Result<String, String> {
    let out = std::process::Command::new("df")
        .args(["-B1", "--output=target,size,used"])
        .output()
        .map_err(|e| format!("无法启动 df：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "df 退出码 {}：{}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("df 输出非 UTF-8：{e}"))
}

/// 执行 `lsblk -J`（后台线程调用）。
pub fn run_lsblk_json() -> Result<String, String> {
    let out = std::process::Command::new("lsblk")
        .args(["-J", "-o", "NAME,PATH,FSTYPE,SIZE,LABEL,UUID,MOUNTPOINTS"])
        .output()
        .map_err(|e| format!("无法启动 lsblk：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "lsblk 退出码 {}：{}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("lsblk 输出非 UTF-8：{e}"))
}

/// 执行 `lsblk -J -b -o NAME,PATH,TYPE,SIZE,FSTYPE,LABEL,UUID,MOUNTPOINTS`
/// （7.7 设备树/分区条数据口径；后台线程调用）。
pub fn run_lsblk_tree_json() -> Result<String, String> {
    let out = std::process::Command::new("lsblk")
        .args([
            "-J",
            "-b",
            "-o",
            "NAME,PATH,TYPE,SIZE,FSTYPE,LABEL,UUID,MOUNTPOINTS",
        ])
        .output()
        .map_err(|e| format!("无法启动 lsblk：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "lsblk 退出码 {}：{}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("lsblk 输出非 UTF-8：{e}"))
}

// ---------------------------------------------------------------------------
// 写前校验闸门（GOAL 7.4）
// ---------------------------------------------------------------------------

/// 写前校验闸门：自家解析拦坏行 → 字段必查 → 序列化回读自检 → `findmnt --verify`。
///
/// 返回 `Ok(warnings)` = **0 errors 放行**（warnings 透传给页面展示）；
/// `Err(errors)` = 拒绝写入（页面 toast 列出，绝不落盘）。
///
/// 分层依据（2026-10-02 探针实测）：
/// - 字段 <4 / dump/pass 非数字的行会让 `findmnt --verify` **段错误 RC=139**，
///   所以这些坏行在 `parse` 与本函数 ①②④ 层就拦掉，**不进 findmnt**；
/// - 目标目录不存在且无 `noauto` → findmnt `2 errors` RC=1（拒绝）；
///   带 `noauto` → `0 errors, 2 warnings` RC=0（放行 + 透传警告）；
/// - 非 root 读不到块设备 → `[W] cannot detect on-disk filesystem type (权限不够)`
///   属纯权限 warning，**不拦**；
/// - 空文档 / 全停用文档 findmnt 会给 `Success` RC=0，但写空 fstab 会毁掉
///   开机挂载 → 本函数 ③ 层自己拦。
pub fn validate_for_write(doc: &FstabDocument) -> Result<Vec<String>, Vec<String>> {
    let mut errors: Vec<String> = Vec::new();

    // ① 必填字段与 dump/pass 数字（构造/编辑出来的条目可能绕过 parse）。
    for e in doc.entries() {
        let id = format!("条目 {}/{}", e.source, e.mountpoint);
        if e.source.trim().is_empty() {
            errors.push(format!("{id}：source 为空"));
        }
        if e.mountpoint.trim().is_empty() {
            errors.push(format!("{id}：挂载点为空"));
        }
        if e.vfstype.trim().is_empty() {
            errors.push(format!("{id}：文件系统类型为空"));
        }
        if e.options.trim().is_empty() {
            errors.push(format!("{id}：挂载选项为空（无选项请填 defaults）"));
        }
        if !e.dump.chars().all(|c| c.is_ascii_digit()) {
            errors.push(format!(
                "{id}：dump「{}」非数字（会导致 findmnt --verify 段错误）",
                e.dump
            ));
        }
        if !e.pass.chars().all(|c| c.is_ascii_digit()) {
            errors.push(format!(
                "{id}：pass「{}」非数字（会导致 findmnt --verify 段错误）",
                e.pass
            ));
        }
    }

    // ② 重复挂载点（启用中的条目）。
    for (mp, n) in duplicate_mountpoints(doc) {
        errors.push(format!("挂载点 {mp} 被 {n} 个启用条目重复占用"));
    }

    // ③ 零启用条目（空文档/全停用）：findmnt 不报错，但写入即毁掉开机挂载。
    if !doc.entries().any(|e| !e.disabled) {
        errors.push("文档没有任何启用条目（空 fstab 会导致开机无法挂载）".to_string());
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    // ④ 序列化回读自检：生成行必须还能被自家解析器读回（字段不错位）。
    let text = serialize(doc);
    if let Err(e) = parse(&text) {
        return Err(vec![format!(
            "序列化结果未通过自家解析器（写入前自检）：{e}"
        )]);
    }

    // ⑤ findmnt --verify 外部校验。
    run_findmnt_verify(&text)
}

/// 校验闸门的文本入口：先自家解析（<4 字段等坏行在此拦截），再走 [`validate_for_write`]。
pub fn validate_text(text: &str) -> Result<Vec<String>, Vec<String>> {
    let doc = parse(text).map_err(|e| vec![e])?;
    validate_for_write(&doc)
}

/// 把内容落到临时文件跑 `findmnt --verify --tab-file`，解析汇总行
/// `N parse errors, M errors, K warnings`：
/// `parse_errors + errors > 0` → Err（带详情行），否则 Ok（透传 `[W]` 警告）。
/// findmnt 启动失败 / 汇总行缺失且退出码非 0 → 按拒绝处理（闸门 fail-closed）。
fn run_findmnt_verify(content: &str) -> Result<Vec<String>, Vec<String>> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!(
        "linbox-fstab-verify-{}-{nanos}.tmp",
        std::process::id()
    ));
    if let Err(e) = std::fs::write(&tmp, content) {
        return Err(vec![format!("校验临时文件写入失败：{e}")]);
    }

    let out = std::process::Command::new("findmnt")
        .args(["--verify", "--tab-file"])
        .arg(&tmp)
        .output();
    let _ = std::fs::remove_file(&tmp);
    let out = match out {
        Ok(o) => o,
        Err(e) => return Err(vec![format!("无法启动 findmnt（拒绝写入）：{e}")]),
    };

    // 详情行（挂载点头 + `[E]`/`[W]`）在 stdout，汇总行在 stderr（探针实测）。
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let detail: Vec<String> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    let all = format!("{stdout}\n{stderr}");

    if let Some(summary) = all.lines().find(|l| l.contains("parse errors")) {
        // `0 parse errors, 2 errors, 0 warnings` → 取前两个计数。
        let nums: Vec<usize> = summary
            .split(',')
            .filter_map(|part| part.split_whitespace().next().and_then(|t| t.parse().ok()))
            .collect();
        let bad = match nums[..] {
            [p, e, ..] => p + e > 0,
            _ => !out.status.success(), // 汇总行形态异常：退回退出码判定
        };
        if bad {
            let mut msgs = vec![summary.to_string()];
            msgs.extend(detail);
            return Err(msgs);
        }
        let warnings = detail.into_iter().filter(|l| l.starts_with('[')).collect();
        return Ok(warnings);
    }
    if all.contains("Success") {
        return Ok(Vec::new()); // 空文档/纯注释：Success, no errors or warnings
    }
    // 汇总行缺失（异常退出）：退回退出码判定，非 0 即拒绝（fail-closed）。
    if out.status.success() {
        Ok(Vec::new())
    } else {
        Err(vec![format!(
            "findmnt --verify 异常退出（RC={}）：",
            out.status.code().unwrap_or(-1)
        )])
    }
}

// ---------------------------------------------------------------------------
// 写入链路（GOAL 7.5）
// ---------------------------------------------------------------------------

/// 写入链路的结果凭证（页面展示与测试证据用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteOutcome {
    /// 备份文件路径（目标原先不存在时为 `None`）。
    pub backup: Option<PathBuf>,
    /// 是否走了 pkexec 提权写入。
    pub elevated: bool,
    /// `systemctl daemon-reload` 的退出码。
    pub reload_rc: i32,
}

/// 备份文件路径：`<target>.linbox.bak`（GOAL D7.3）。
pub fn backup_path(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".linbox.bak");
    PathBuf::from(s)
}

/// 写入链路：**备份 → 落盘（可直写则直写，否则 pkexec，root:root 0644）→
/// 回读逐字节比对 → `systemctl daemon-reload` → 任一步失败从 `.linbox.bak` 回滚**。
///
/// 前置：调用方应先过 [`validate_for_write`]（页面流程保证）；这里再过一次
/// 自家 `parse` 作为防线——明显坏的内容连备份都不做、目标文件不动。
///
/// 提权写入需要 polkit 授权（2026-10-02 实测：桌面弹框等待输入，无法无人值守
/// 自动完成）；真机 `/etc/fstab` 提权链路的手测见 GOAL 7.6/7.7。
pub fn write_fstab(target: &Path, content: &str) -> Result<WriteOutcome, String> {
    write_fstab_with_reload(target, content, run_daemon_reload)
}

/// 链路实现，`reload` 可注入：单测用 `|| 0` / `|| 1` 确定性地验证成功与
/// 「reload 失败 → 回滚」两条路，真实 reload 由 [`write_fstab`] 或真机
/// 证据测试（`dryrun_real_evidence`，`--ignored`）执行——
/// 非 root 的 `systemctl daemon-reload` 走 polkit `auth_admin_keep`，
/// 无人值守会弹框超时（2026-10-02 实测 RC=1，Method call timed out），
/// 不能让单测依赖桌面授权状态。
fn write_fstab_with_reload<F: FnOnce() -> i32>(
    target: &Path,
    content: &str,
    reload: F,
) -> Result<WriteOutcome, String> {
    // 防线：内容必须能过自家解析（坏行绝不落盘，也绝不进 findmnt/pkexec）。
    if let Err(e) = parse(content) {
        return Err(format!("内容未通过解析，拒绝写入：{e}"));
    }

    let existed = target.exists();
    let bak = backup_path(target);
    let target_s = target.to_string_lossy().into_owned();
    let elevated = !env_editor::can_write_direct(&target_s);

    // ① 备份 + ② 落盘（失败回滚到写入前状态）。
    if elevated {
        elevated_backup_and_write(target, content)?;
    } else if let Err(e) = direct_backup_and_write(target, &bak, existed, content) {
        if existed {
            let _ = restore_from_backup(target, true, false);
        }
        return Err(e);
    }

    // ③ 回读逐字节比对（不一致 → 回滚）。
    match std::fs::read(target) {
        Ok(bytes) if bytes == content.as_bytes() => {}
        Ok(bytes) => {
            let _ = restore_from_backup(target, existed, elevated);
            return Err(format!(
                "回读逐字节比对失败（写入 {} 字节，回读 {} 字节），已从 .linbox.bak 回滚",
                content.len(),
                bytes.len()
            ));
        }
        Err(e) => {
            let _ = restore_from_backup(target, existed, elevated);
            return Err(format!("回读失败：{e}，已尝试从 .linbox.bak 回滚"));
        }
    }

    // ④ systemd 按新 fstab 重新生成挂载单元。
    let reload_rc = reload();
    if reload_rc != 0 {
        let _ = restore_from_backup(target, existed, elevated);
        return Err(format!(
            "systemctl daemon-reload 失败（RC={reload_rc}），已从 .linbox.bak 回滚"
        ));
    }

    Ok(WriteOutcome {
        backup: existed.then_some(bak),
        elevated,
        reload_rc,
    })
}

/// 直写路径的「备份 + 落盘」。
fn direct_backup_and_write(
    target: &Path,
    bak: &Path,
    existed: bool,
    content: &str,
) -> Result<(), String> {
    if existed {
        std::fs::copy(target, bak).map_err(|e| format!("备份 {} 失败：{e}", target.display()))?;
    }
    std::fs::write(target, content).map_err(|e| format!("写入 {} 失败：{e}", target.display()))
}

/// 恢复备份：目标原先存在 → 从 `.linbox.bak` 拷回；原先不存在 → 删除误写文件。
/// `elevated=true` 时经 pkexec 执行（回滚 /etc/fstab 需要 root）。
fn restore_from_backup(target: &Path, existed: bool, elevated: bool) -> Result<(), String> {
    let dst = target.display().to_string();
    let bak = backup_path(target).display().to_string();
    if elevated {
        let script = if existed {
            format!(
                "if [ -e {b} ]; then cp -a {b} {d} && chmod 0644 {d}; fi",
                b = sh_q(&bak),
                d = sh_q(&dst)
            )
        } else {
            format!("rm -f -- {d}", d = sh_q(&dst))
        };
        return run_pkexec_sh(&script);
    }
    if existed {
        std::fs::copy(&bak, target)
            .map(|_| ())
            .map_err(|e| format!("从 {bak} 回滚失败：{e}"))
    } else {
        let _ = std::fs::remove_file(target);
        Ok(())
    }
}

/// 执行 `systemctl daemon-reload`，返回退出码（启动失败按 -1 处理）。
fn run_daemon_reload() -> i32 {
    std::process::Command::new("systemctl")
        .arg("daemon-reload")
        .output()
        .map(|o| o.status.code().unwrap_or(-1))
        .unwrap_or(-1)
}

/// pkexec 提权执行一段 `sh` 脚本（路径转义语义与 env_editor 一致，D7.1）。
fn run_pkexec_sh(script: &str) -> Result<(), String> {
    let out = std::process::Command::new("pkexec")
        .arg("sh")
        .arg("-c")
        .arg(script)
        .output()
        .map_err(|e| format!("无法启动 pkexec：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "pkexec 脚本失败（RC={}）：{}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// pkexec 提权的「备份 + 落盘」：内容先落 0600 临时文件，再以 root 执行脚本。
///
/// 与 env_editor 的 rc 写入不同（D7.2）：**不 chown 回用户**，
/// 目标保持 root:root 0644（`cat tmp > dst` 保留既有属主，`chmod 0644` 兜底）。
fn elevated_backup_and_write(target: &Path, content: &str) -> Result<(), String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("linbox-fstab-{}-{nanos}.tmp", std::process::id()));
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("创建临时文件失败：{e}"))?;
        f.write_all(content.as_bytes())
            .map_err(|e| format!("写入临时文件失败：{e}"))?;
    }
    let script = elevated_write_script(target, &tmp);
    let res = run_pkexec_sh(&script);
    let _ = std::fs::remove_file(&tmp);
    res
}

/// 拼提权写入脚本：`mkdir -p 父目录 && 旧文件先备份 && cat 临时文件 > 目标 && chmod 0644`。
/// 所有路径经 [`sh_q`] 单引号转义。
fn elevated_write_script(target: &Path, tmp: &Path) -> String {
    let dst = target.display().to_string();
    let bak = backup_path(target).display().to_string();
    let parent = target
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let tmp_s = tmp.display().to_string();
    format!(
        "mkdir -p -- {p} && if [ -e {d} ]; then cp -a {d} {b}; fi && cat {t} > {d} && chmod 0644 {d}",
        p = sh_q(&parent),
        d = sh_q(&dst),
        b = sh_q(&bak),
        t = sh_q(&tmp_s),
    )
}

/// shell 单引号转义（把路径安全嵌进 `pkexec sh -c`；与 env_editor `sh_q` 同语义）。
fn sh_q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ── 测试 ──────────────────────────────────────────────────────────────────
//
// golden 用本机 /etc/fstab 的**真实字节**（2026-10-02 cp，726B，含 anaconda 注释头
// 与对齐空格）；坏行用例全部对应「findmnt --verify 会段错误」的实测防线。

// ---------------------------------------------------------------------------
// 管理操作（GOAL 7.6：新增 / 注释停用 / 恢复 / 立即挂载卸载 —— 纯逻辑与命令构造）
// ---------------------------------------------------------------------------

/// 字段级校验（[`add_entry`] / [`edit_entry`] 共用）：
/// ① 四必填字段非空；② 挂载点绝对路径（swap 可为 `none`）；
/// ③ vfstype 无空白无 `/`；④ dump/pass 纯数字（防 findmnt 段错误，7.4 同因）。
/// 重复挂载点检查不在这里——编辑时要把自身排除，由调用方做。
fn check_entry_fields(entry: &FstabEntry) -> Result<(), String> {
    if entry.source.trim().is_empty() {
        return Err("来源（source）不能为空".to_string());
    }
    if entry.mountpoint.trim().is_empty() {
        return Err("挂载点不能为空".to_string());
    }
    if !(entry.mountpoint.starts_with('/') || entry.mountpoint == "none") {
        return Err(format!(
            "挂载点必须以 / 开头（swap 等无挂载点条目用 none）：{}",
            entry.mountpoint
        ));
    }
    if entry.vfstype.trim().is_empty() {
        return Err("文件系统类型不能为空".to_string());
    }
    if entry.vfstype.contains(char::is_whitespace) || entry.vfstype.contains('/') {
        return Err(format!("文件系统类型含非法字符：{}", entry.vfstype));
    }
    if entry.options.trim().is_empty() {
        return Err("挂载选项不能为空（通常为 defaults）".to_string());
    }
    if !entry.dump.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("dump 必须为数字：{}", entry.dump));
    }
    if !entry.pass.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("pass 必须为数字：{}", entry.pass));
    }
    Ok(())
}

/// 新增条目到文档末尾（D7.6 表单提交的后端逻辑）。
///
/// 校验（全为必过，错误信息给页面 toast）：
/// ① 四必填字段非空；② 挂载点绝对路径（swap 可为 `none`）；
/// ③ vfstype 无空白无 `/`；④ dump/pass 纯数字（防 findmnt 段错误，7.4 同因）；
/// ⑤ 挂载点不与**活动**条目重复（停用条目不占位——可「停用旧的 + 新增」完成迁移）。
pub fn add_entry(doc: &mut FstabDocument, entry: FstabEntry) -> Result<(), String> {
    check_entry_fields(&entry)?;
    let key = normalize_mountpoint(&entry.mountpoint);
    for used in doc.entries() {
        if !used.disabled && normalize_mountpoint(&used.mountpoint) == key {
            return Err(format!(
                "挂载点 {} 已被第 {} 行占用（旧条目可先注释停用再新增）",
                entry.mountpoint, used.line_no
            ));
        }
    }
    let mut entry = entry;
    // 行号 = max(现有行数 + 1, 既有条目最大行号 + 1)：行号是会话内稳定标识
    // （set_entry_disabled/edit_entry 按它定位），物理删除会使 lines.len() 变小，
    // 只用 len+1 会与幸存条目的旧行号碰撞。落盘重读后 parse 按真实行号刷新。
    let next = doc.lines.len() + 1;
    let max_id = doc.entries().map(|e| e.line_no).max().unwrap_or(0);
    entry.line_no = next.max(max_id + 1);
    entry.raw = None;
    doc.push_line(FstabLine::Entry(entry));
    Ok(())
}

/// 按行号找到条目并设置「注释停用/恢复」（D7.5 的唯一移除方式）。
///
/// 行号来自解析，编辑/新增不改变既有条目行号（raw/canonical 只改行内容），
/// 因此在「读取 → 编辑 → 写入」一次会话内可作稳定标识。
pub fn set_entry_disabled(
    doc: &mut FstabDocument,
    line_no: usize,
    disabled: bool,
) -> Result<(), String> {
    let found = doc
        .entries_mut()
        .find(|e| e.line_no == line_no)
        .ok_or_else(|| format!("未找到第 {line_no} 行的条目"))?;
    if found.disabled == disabled {
        return Ok(()); // 幂等：重复点按不改变内容、不脏 raw
    }
    found.set_disabled(disabled);
    Ok(())
}

/// 编辑既有条目（按行号定位，7.6 编辑操作的后端逻辑）。
///
/// 校验与 [`add_entry`] 共用 [`check_entry_fields`]（四必填 + 挂载点形态 +
/// vfstype 合法 + dump/pass 纯数字），重复挂载点检查**排除自身**（挂载点没改
/// 不能被自己挡住）。写入走 setter——`raw` 置空 → 序列化用 canonical 行，
/// 与 D7.4「被编辑行单空格规范格式」一致；行号不变（set_entry_disabled 的
/// 定位不受影响）。
pub fn edit_entry(
    doc: &mut FstabDocument,
    line_no: usize,
    entry: FstabEntry,
) -> Result<(), String> {
    check_entry_fields(&entry)?;
    let key = normalize_mountpoint(&entry.mountpoint);
    for used in doc.entries() {
        if used.line_no != line_no
            && !used.disabled
            && normalize_mountpoint(&used.mountpoint) == key
        {
            return Err(format!(
                "挂载点 {} 已被第 {} 行占用（旧条目可先注释停用再改）",
                entry.mountpoint, used.line_no
            ));
        }
    }
    let found = doc
        .entries_mut()
        .find(|e| e.line_no == line_no)
        .ok_or_else(|| format!("未找到第 {line_no} 行的条目"))?;
    found.set_source(entry.source);
    found.set_mountpoint(entry.mountpoint);
    found.set_vfstype(entry.vfstype);
    found.set_options(entry.options);
    found.set_dump(entry.dump);
    found.set_pass(entry.pass);
    Ok(())
}

/// 物理删除条目所在行（D7.5 修订：与注释停用并存的第二种移除方式）。
///
/// 按行号定位 `FstabLine::Entry` 并从 `lines` 中移除**该行**（行首 `#` 的
/// 停用条目同样可删）。其余条目的 `line_no` **不重排**——它是会话内稳定标识，
/// 重排会让队列里已有的 Edit/SetDisabled/Remove 指向错误目标。
/// 评论/空行原样保留（只删条目行本身）。
pub fn remove_entry(doc: &mut FstabDocument, line_no: usize) -> Result<(), String> {
    let idx = doc
        .lines
        .iter()
        .position(|l| matches!(l, FstabLine::Entry(e) if e.line_no == line_no))
        .ok_or_else(|| format!("未找到第 {line_no} 行的条目"))?;
    doc.lines.remove(idx);
    Ok(())
}

/// 立即挂载命令（D7.6 按钮）：普通条目 `pkexec mount <挂载点>`；
/// swap 条目语义是 `swapon <源>`（model `is_swap` 注释同源）。
pub fn mount_command(entry: &FstabEntry) -> Vec<String> {
    if entry.is_swap() {
        vec![
            "pkexec".to_string(),
            "swapon".to_string(),
            entry.source.clone(),
        ]
    } else {
        vec![
            "pkexec".to_string(),
            "mount".to_string(),
            entry.mountpoint.clone(),
        ]
    }
}

/// 立即卸载命令（D7.6 按钮）：`pkexec umount <挂载点>`；swap → `swapoff <源>`。
pub fn umount_command(entry: &FstabEntry) -> Vec<String> {
    if entry.is_swap() {
        vec![
            "pkexec".to_string(),
            "swapoff".to_string(),
            entry.source.clone(),
        ]
    } else {
        vec![
            "pkexec".to_string(),
            "umount".to_string(),
            entry.mountpoint.clone(),
        ]
    }
}

/// 挂载点目录不存在时的「可选建目录」命令（7.6：提示后经 pkexec 创建）。
pub fn ensure_mountpoint_dir_command(dir: &str) -> Vec<String> {
    vec![
        "pkexec".to_string(),
        "mkdir".to_string(),
        "-p".to_string(),
        dir.to_string(),
    ]
}

// ---------------------------------------------------------------------------
// 待定操作队列（GOAL 7.8，KDE PM Pending Operations 语义）
// ---------------------------------------------------------------------------

/// 一条待定操作（只入队不落盘；Apply 前对磁盘零影响，撤销零成本）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingOp {
    /// 新增条目（行号由 preview 应用到文档时分配，与真实 Apply 同结果）。
    Add(FstabEntry),
    /// 编辑既有条目（按行号定位）。
    Edit { line_no: usize, entry: FstabEntry },
    /// 注释停用 / 恢复（D7.5 的注释移除方式）。
    SetDisabled { line_no: usize, disabled: bool },
    /// 物理删除该行（D7.5 2026-10-03 修订：与注释停用并存，确认弹窗后入队）。
    /// `mountpoint` 仅用于队列面板展示文案，preview 只用 `line_no` 定位。
    Remove { line_no: usize, mountpoint: String },
}

impl PendingOp {
    /// 队列面板展示文案。
    pub fn describe(&self) -> String {
        match self {
            PendingOp::Add(e) => {
                format!("新增 {}（{}）", unescape_field(&e.mountpoint), e.vfstype)
            }
            PendingOp::Edit { line_no, entry } => {
                format!(
                    "编辑第 {line_no} 行 → {}",
                    unescape_field(&entry.mountpoint)
                )
            }
            PendingOp::SetDisabled {
                line_no,
                disabled: true,
            } => {
                format!("注释停用第 {line_no} 行")
            }
            PendingOp::SetDisabled {
                line_no,
                disabled: false,
            } => format!("恢复第 {line_no} 行"),
            PendingOp::Remove {
                line_no,
                mountpoint,
            } => {
                format!("物理删除第 {line_no} 行（{mountpoint}）")
            }
        }
    }
}

/// 待定操作队列：入队 → [`preview`]（应用后文档，主视图预览与 Apply 共用）→
/// 真实写入成功后由调用方 [`clear`]（Apply 后队列清空）。
///
/// 合并规则：**连续同目标同类型只留最新**（同一条目连点两次编辑 → 队列 1 条，
/// 取最终值）；不同类型或不同目标不合并（顺序有意义，如「停用第 1 行 + 新增」）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpQueue {
    ops: Vec<PendingOp>,
}

impl OpQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn ops(&self) -> &[PendingOp] {
        &self.ops
    }

    /// 入队（应用连续合并规则）。
    pub fn push(&mut self, op: PendingOp) {
        if let Some(last) = self.ops.last_mut() {
            let merge = match (&*last, &op) {
                (PendingOp::Edit { line_no: a, .. }, PendingOp::Edit { line_no: b, .. }) => a == b,
                (
                    PendingOp::SetDisabled { line_no: a, .. },
                    PendingOp::SetDisabled { line_no: b, .. },
                ) => a == b,
                _ => false,
            };
            if merge {
                *last = op;
                return;
            }
        }
        self.ops.push(op);
    }

    /// 撤销最后一条（页面 Ctrl+Z / 撤销按钮），返回被撤销的操作。
    pub fn undo(&mut self) -> Option<PendingOp> {
        self.ops.pop()
    }

    /// 原地替换第 `idx` 条操作（编辑「待定新增」条目时：Add 就地改，
    /// 而不是再追加一条依赖它的 Edit——避免撤销 Add 后留下孤儿 Edit）。
    pub fn replace_at(&mut self, idx: usize, op: PendingOp) -> Result<(), String> {
        match self.ops.get_mut(idx) {
            Some(slot) => {
                *slot = op;
                Ok(())
            }
            None => Err(format!("队列中没有第 {idx} 条操作")),
        }
    }

    /// 单条撤销（队列面板每行的撤销按钮），返回被撤销的操作。
    pub fn remove_at(&mut self, idx: usize) -> Option<PendingOp> {
        (idx < self.ops.len()).then(|| self.ops.remove(idx))
    }

    /// 清空队列（KDE PM 的 Edit → Clear）。
    pub fn clear(&mut self) {
        self.ops.clear();
    }

    /// 把队列全部应用到 `base` 的**克隆**上，返回「应用后」文档。
    ///
    /// 主视图预览与 Apply 前的校验都走这里；任一操作失败 → `Err`，
    /// 且 `base` 与队列本身均不动（**校验失败队列保留**）。
    /// 同一 `base` 上 preview 与真实 Apply 结果一致（`add_entry` 的行号分配
    /// 只依赖 `base` 的行数，见其 doc 注释）。
    pub fn preview(&self, base: &FstabDocument) -> Result<FstabDocument, String> {
        let mut doc = base.clone();
        for op in &self.ops {
            match op {
                PendingOp::Add(e) => add_entry(&mut doc, e.clone())?,
                PendingOp::Edit { line_no, entry } => {
                    edit_entry(&mut doc, *line_no, entry.clone())?
                }
                PendingOp::SetDisabled { line_no, disabled } => {
                    set_entry_disabled(&mut doc, *line_no, *disabled)?
                }
                PendingOp::Remove { line_no, .. } => remove_entry(&mut doc, *line_no)?,
            }
        }
        Ok(doc)
    }

    /// 产出预览中行号 `line_no` 的那条 `Add` 在队列里的下标。
    ///
    /// 页面编辑「待定新增」行时用（替代旧的 `line_no - base行数 - 1` 算术反推：
    /// `Remove` 入队后行数关系不再成立，且 `add_entry` 的 max 行号规则会产生
    /// 行号复用，只有逐条前缀模拟在所有队列形态下都正确）。
    pub fn add_index_producing(&self, base: &FstabDocument, line_no: usize) -> Option<usize> {
        let mut doc = base.clone();
        for (i, op) in self.ops.iter().enumerate() {
            let mut trial = doc.clone();
            let ok = match op {
                PendingOp::Add(e) => add_entry(&mut trial, e.clone()).is_ok(),
                PendingOp::Edit { line_no: ln, entry } => {
                    edit_entry(&mut trial, *ln, entry.clone()).is_ok()
                }
                PendingOp::SetDisabled {
                    line_no: ln,
                    disabled,
                } => set_entry_disabled(&mut trial, *ln, *disabled).is_ok(),
                PendingOp::Remove { line_no: ln, .. } => remove_entry(&mut trial, *ln).is_ok(),
            };
            if !ok {
                return None;
            }
            let produced = !doc.entries().any(|e| e.line_no == line_no)
                && trial.entries().any(|e| e.line_no == line_no);
            if produced && matches!(op, PendingOp::Add(_)) {
                return Some(i);
            }
            doc = trial;
        }
        None
    }
}

// --------------------------------------------------------------------------- 挂载选项清单与勾选状态（D7.9 / 7.13）

/// 核心选项（D7.9 附录：弹窗首屏直显 9 项）——（参数, 中文名, 说明）。
pub const CORE_OPTIONS: [(&str, &str, &str); 9] = [
    ("ro", "只读", "以只读方式挂载，禁止写入"),
    (
        "user",
        "允许用户挂载和卸载",
        "普通用户（非 root）可挂载，通常还允许该用户卸载",
    ),
    (
        "noexec",
        "不可执行二进制程序",
        "禁止在该分区上运行可执行文件，常用于提高安全性或挂载 Windows 分区",
    ),
    (
        "noauto",
        "不自动挂载",
        "系统启动或执行 mount -a 时不会自动挂载，需手动挂载",
    ),
    (
        "nofail",
        "无法挂载时不要阻止系统启动",
        "设备没插或找不到时忽略错误继续启动，不卡开机画面",
    ),
    (
        "noatime",
        "不更新文件访问时间",
        "访问文件时不更新「最后访问时间」，大幅减少磁盘 I/O（尤其 SSD）",
    ),
    (
        "nodiratime",
        "不更新目录访问时间",
        "同上，但不更新目录的访问时间",
    ),
    (
        "sync",
        "同步访问",
        "所有 I/O 立即写入物理设备，数据安全但降低性能",
    ),
    (
        "relatime",
        "更新与修改相关的访问时间",
        "仅当 atime 早于 mtime/ctime 才更新（现代内核常用平衡方案）",
    ),
];

/// 全量选项清单（D7.9 附录：7 组 VFS 选项）——（组名, [(参数, 含义)]）。
pub const OPTION_GROUPS: [(&str, &[(&str, &str)]); 7] = [
    (
        "读写与 I/O 模式",
        &[
            ("ro", "只读挂载"),
            ("rw", "读写挂载（默认）"),
            ("sync", "所有 I/O 同步写入（安全但慢）"),
            ("async", "所有 I/O 异步写入（默认）"),
            ("dirsync", "目录更新（mkdir/unlink/rename 等）同步写入"),
        ],
    ),
    (
        "权限与执行",
        &[
            ("exec", "允许执行二进制程序（默认）"),
            ("noexec", "禁止执行任何二进制程序"),
            ("suid", "允许 set-user-ID / set-group-ID 位生效（默认）"),
            ("nosuid", "禁止 suid/sgid 位生效"),
            ("dev", "解释字符/块设备文件（默认）"),
            ("nodev", "不解释设备文件"),
        ],
    ),
    (
        "访问时间更新",
        &[
            ("atime", "按内核默认更新 atime（即 relatime）"),
            ("noatime", "完全不更新文件访问时间（隐含 nodiratime）"),
            ("diratime", "更新目录 inode 访问时间（默认）"),
            ("nodiratime", "不更新目录访问时间"),
            (
                "relatime",
                "仅当 atime 早于 mtime/ctime 或超过 24 小时才更新（内核默认）",
            ),
            ("norelatime", "不使用 relatime"),
            ("strictatime", "每次访问都更新 atime（传统语义）"),
            ("nostrictatime", "使用内核默认 atime 行为"),
            (
                "lazytime",
                "只在内存中更新 atime/mtime/ctime，符合条件才落盘",
            ),
            ("nolazytime", "不使用 lazytime"),
        ],
    ),
    (
        "挂载时机与启动容错",
        &[
            ("auto", "被 mount -a 自动挂载（默认）"),
            ("noauto", "不自动挂载，需手动挂载"),
            ("nofail", "设备不存在时不报错、不阻塞启动"),
            ("_netdev", "该设备需网络，等网络就绪后再挂载"),
            ("defaults", "等价 rw,suid,dev,exec,auto,nouser,async 的组合"),
        ],
    ),
    (
        "用户挂载权限",
        &[
            ("user", "允许任意普通用户挂载/卸载"),
            ("nouser", "禁止普通用户挂载（默认）"),
            ("users", "允许所有用户挂载和卸载"),
            ("owner", "设备所有者可挂载（隐含 nosuid,nodev）"),
            ("group", "用户所属组匹配设备组时可挂载（隐含 nosuid,nodev）"),
        ],
    ),
    (
        "安全与锁",
        &[
            ("mand", "允许强制锁（mandatory locks，5.15 起已弃用）"),
            ("nomand", "禁止强制锁"),
            ("iversion", "每次 inode 修改递增 i_version 字段"),
            ("noiversion", "不递增 i_version"),
        ],
    ),
    (
        "行为开关",
        &[
            ("silent", "静默模式"),
            ("loud", "关闭静默模式"),
            (
                "remount",
                "重新挂载已挂载的文件系统（运行期动作，不写入 fstab）",
            ),
        ],
    ),
];

/// `defaults` 组合项的 7 个组成项（D7.9 联动①：全勾 = `defaults`，取消任一即转散项）。
pub const DEFAULTS_PARTS: [&str; 7] = ["rw", "suid", "dev", "exec", "auto", "nouser", "async"];

/// 互斥组（D7.9 联动②：组内单选，勾一项自动取消同组其它项）。
pub const MUTEX_GROUPS: [&[&str]; 15] = [
    &["ro", "rw"],
    &["sync", "async"],
    &["exec", "noexec"],
    &["suid", "nosuid"],
    &["dev", "nodev"],
    &["auto", "noauto"],
    &["atime", "noatime"],
    &["diratime", "nodiratime"],
    &["relatime", "norelatime"],
    &["strictatime", "nostrictatime"],
    &["lazytime", "nolazytime"],
    &["mand", "nomand"],
    &["iversion", "noiversion"],
    &["silent", "loud"],
    &["user", "nouser", "users", "owner", "group"],
];

/// 挂载选项勾选状态（D7.9）：清单内布尔项（勾选序稳定）+ 清单外原样保留项（key=value 等，保序）。
///
/// 语法层设计（GOAL 7.13 边界）：
/// - `defaults` 永不入 `checked`——由 7 个组成项**派生**（全勾 = 已勾 defaults）；
/// - `remount` 可勾选（UI 提示），但序列化时**不写入 fstab**；
/// - 清单外项（`subvol=@`、`compress=zstd:1` 等）只读保留在 `others`，勾选操作绝不丢弃。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionsState {
    /// 已勾选的清单内布尔选项（保留勾选/解析顺序）。
    pub checked: Vec<String>,
    /// 清单外原样保留的选项（key=value 或未知项），保序。
    pub others: Vec<String>,
}

impl Default for OptionsState {
    /// 新条目默认态（D7.9）：`defaults` + `relatime`。
    fn default() -> Self {
        let mut st = OptionsState {
            checked: Vec::new(),
            others: Vec::new(),
        };
        for p in DEFAULTS_PARTS {
            st.checked.push((*p).to_string());
        }
        st.checked.push("relatime".to_string());
        st
    }
}

impl OptionsState {
    /// 是否已勾选（`defaults` = 7 组成项全勾的派生态）。
    pub fn is_checked(&self, opt: &str) -> bool {
        if opt == "defaults" {
            return DEFAULTS_PARTS.iter().all(|p| self.is_checked(p));
        }
        self.checked.iter().any(|c| c == opt)
    }

    fn push(&mut self, opt: &str) {
        if !self.checked.iter().any(|c| c == opt) {
            self.checked.push(opt.to_string());
        }
    }

    fn remove(&mut self, opt: &str) {
        self.checked.retain(|c| c != opt);
    }
}

/// 该参数是否在 D7.9 清单内（含 `defaults`，含 `remount`）。
pub fn is_catalog_option(opt: &str) -> bool {
    OPTION_GROUPS
        .iter()
        .any(|(_, items)| items.iter().any(|(p, _)| *p == opt))
}

/// 解析挂载选项串为勾选状态。
///
/// - `defaults` → 展开 7 组成项（写出时若全勾再还原为 `defaults` 一词）；
/// - 清单内布尔项 → 进 `checked`；**解析不做互斥归一**（`defaults, noauto` 这类
///   真实惯用法要原样往返）；
/// - 清单外项 → `others` 原样保序；
/// - 空串 → 默认态（`defaults,relatime`，D7.9 新条目默认）。
pub fn parse_options(text: &str) -> OptionsState {
    let mut st = OptionsState {
        checked: Vec::new(),
        others: Vec::new(),
    };
    for raw in text.split(',') {
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        if t == "defaults" {
            for p in DEFAULTS_PARTS {
                st.push(p);
            }
            continue;
        }
        if is_catalog_option(t) {
            st.push(t);
        } else {
            st.others.push(t.to_string());
        }
    }
    if st.checked.is_empty() && st.others.is_empty() {
        return OptionsState::default();
    }
    st
}

/// 序列化勾选状态为 fstab 选项串。
///
/// - 7 组成项全勾 → 写 `defaults` 一词（D7.9 ①；散项凑满同样还原）；
/// - `remount` 不写入（D7.9 边界）；
/// - `others` 保序追加在后；
/// - 全空兜底 `defaults`（fstab 选项字段不可为空）。
pub fn serialize_options(st: &OptionsState) -> String {
    let mut out: Vec<String> = Vec::new();
    let all_defaults = DEFAULTS_PARTS.iter().all(|p| st.is_checked(p));
    let mut defaults_done = false;
    for c in &st.checked {
        if c == "remount" {
            continue; // D7.9 边界：运行期动作不入 fstab
        }
        if DEFAULTS_PARTS.contains(&c.as_str()) {
            if all_defaults {
                if !defaults_done {
                    out.push("defaults".to_string());
                    defaults_done = true;
                }
                continue;
            }
            out.push(c.clone());
            continue;
        }
        out.push(c.clone());
    }
    out.extend(st.others.iter().cloned());
    if out.is_empty() {
        return "defaults".to_string();
    }
    out.join(",")
}

/// 勾选/取消一个选项（D7.9 联动规则②③在此生效；仅用户操作走本函数，解析不走）。
///
/// - `defaults` → on：补勾 7 组成项（逐项走互斥）；off：移除 7 组成项（转散项/空态）；
/// - 互斥组（`MUTEX_GROUPS`）单选；
/// - 勾 `noatime` → 自动取消 `relatime` 并联动勾 `nodiratime`（隐含 nodiratime）；
///   取消 `noatime` 不动 `nodiratime`（之后可独立处理）；
/// - 勾 `relatime` → 若已勾 `noatime` 自动取消（对称防呆）；
/// - 清单外项不可勾选（只读保留）。
pub fn set_option(st: &mut OptionsState, opt: &str, on: bool) {
    if opt == "defaults" {
        if on {
            for p in DEFAULTS_PARTS {
                set_on(st, p);
            }
        } else {
            for p in DEFAULTS_PARTS {
                st.remove(p);
            }
        }
        return;
    }
    if !is_catalog_option(opt) {
        return;
    }
    if on {
        set_on(st, opt);
    } else {
        st.remove(opt);
    }
}

fn set_on(st: &mut OptionsState, opt: &str) {
    st.push(opt);
    // 互斥组单选
    for g in MUTEX_GROUPS {
        if g.contains(&opt) {
            for other in g.iter() {
                if *other != opt {
                    st.remove(other);
                }
            }
        }
    }
    match opt {
        "noatime" => {
            st.remove("relatime");
            st.push("nodiratime");
            for g in MUTEX_GROUPS {
                if g.contains(&"nodiratime") {
                    for other in g.iter() {
                        if *other != "nodiratime" {
                            st.remove(other);
                        }
                    }
                }
            }
        }
        "relatime" => st.remove("noatime"),
        _ => {}
    }
}

/// 新条目的选项串（默认态序列化 = `defaults,relatime`，D7.9）。
pub fn default_options_string() -> String {
    serialize_options(&OptionsState::default())
}

/// 文件系统类型全量下拉清单（D7.10：字段禁手输，点击下拉选择，清单写全）。
///
/// 顺序即下拉顺序：`auto` 首位 → 常见磁盘 → Windows → 只读/嵌入式 → 其他磁盘 →
/// swap → 网络 → FUSE/虚拟块 → 伪文件系统。编辑既有条目时值不在清单由页面动态补入。
pub const FSTYPE_OPTIONS: &[&str] = &[
    // 首位：让内核自动探测（fstab 惯例）
    "auto",
    // 常见磁盘文件系统
    "ext4",
    "ext3",
    "ext2",
    "xfs",
    "btrfs",
    "f2fs",
    "bcachefs",
    "jfs",
    "nilfs2",
    "reiserfs",
    "zfs",
    "ocfs2",
    "gfs2",
    "minix",
    // Windows / 可移动介质
    "vfat",
    "exfat",
    "ntfs3",
    "ntfs",
    "msdos",
    "umsdos",
    // 光盘 / 只读 / 嵌入式
    "squashfs",
    "erofs",
    "iso9660",
    "udf",
    "cramfs",
    "romfs",
    "jffs2",
    "ubifs",
    // 其他磁盘文件系统（遗留/小众但合法）
    "hfs",
    "hfsplus",
    "affs",
    "adfs",
    "befs",
    "bfs",
    "efs",
    "hpfs",
    "omfs",
    "qnx4",
    "qnx6",
    "sysv",
    "ufs",
    "vxfs",
    "ocfs1",
    // 交换分区（fstab type 字段惯用）
    "swap",
    // 网络文件系统
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "9p",
    "afs",
    "coda",
    "ncpfs",
    "smbfs",
    // FUSE / 虚拟块设备 / 堆叠
    "fuse",
    "fuseblk",
    "autofs",
    "overlay",
    "overlay2",
    "aufs",
    "tmpfs",
    "ramfs",
    "hugetlbfs",
    "mqueue",
    // 伪文件系统（fstab 里同样合法）
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "securityfs",
    "debugfs",
    "tracefs",
    "configfs",
    "pstore",
    "bpf",
    "cgroup",
    "cgroup2",
    "binfmt_misc",
    "fusectl",
    "functionfs",
    "gadgetfs",
    "rpc_pipefs",
    "binderfs",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::fstab::escape_field;

    /// 本机 /etc/fstab 全文真实字节（726B，golden）。
    const REAL_FSTAB: &str = "\n#\n# /etc/fstab\n# Created by anaconda on Wed Oct  1 16:16:37 2025\n#\n# Accessible filesystems, by reference, are maintained under '/dev/disk/'.\n# See man pages fstab(5), findfs(8), mount(8) and/or blkid(8) for more info.\n#\n# After editing this file, run 'systemctl daemon-reload' to update systemd\n# units generated from this file.\n#\nUUID=cdb4f2f6-ff9b-4a25-9f5a-a390ab89aaeb /                       btrfs   subvol=root,compress=zstd:1 0 0\nUUID=17dd5d78-e9ce-4551-84b3-38d06a753f78 /boot                   ext4    defaults        1 2\nUUID=3912-7550          /boot/efi               vfat    umask=0077,shortname=winnt 0 2\nUUID=cdb4f2f6-ff9b-4a25-9f5a-a390ab89aaeb /home                   btrfs   subvol=home,compress=zstd:1 0 0\n";

    // ---------------- 7.13 挂载选项往返真值表（D7.9） ----------------

    #[test]
    fn options_empty_string_gives_default_state() {
        // 空串 → 新条目默认态 defaults + relatime（D7.9）
        let st = parse_options("");
        assert!(st.is_checked("defaults"), "7 组成项全勾派生 defaults");
        assert!(st.is_checked("relatime"));
        assert_eq!(serialize_options(&st), "defaults,relatime");
        assert_eq!(default_options_string(), "defaults,relatime");
        // 纯空白串同样
        assert_eq!(
            serialize_options(&parse_options("  ,  ")),
            "defaults,relatime"
        );
    }

    #[test]
    fn options_defaults_expand_and_roundtrip() {
        // defaults 展开与还原
        let st = parse_options("defaults");
        for p in DEFAULTS_PARTS {
            assert!(st.is_checked(p), "defaults 应展开 {p}");
        }
        assert_eq!(serialize_options(&st), "defaults");
        // 所见即所得：编辑既有 defaults,noauto 反解析（GOAL 7.13 真值表 8）
        let st = parse_options("defaults,noauto");
        assert!(st.is_checked("defaults") && st.is_checked("noauto"));
        assert_eq!(serialize_options(&st), "defaults,noauto");
        // 顺序不敏感也还原
        assert_eq!(
            serialize_options(&parse_options("noauto, defaults")),
            "noauto,defaults"
        );
    }

    #[test]
    fn options_mutex_single_select() {
        // 互斥组切换单选（D7.9 联动②）：默认态含 rw，勾 ro → rw 出局
        let mut st = OptionsState::default();
        assert!(st.is_checked("rw"));
        set_option(&mut st, "ro", true);
        assert!(st.is_checked("ro") && !st.is_checked("rw"));
        // 5 项用户权限组单选
        set_option(&mut st, "users", true);
        assert!(st.is_checked("users"));
        set_option(&mut st, "owner", true);
        assert!(st.is_checked("owner") && !st.is_checked("users"));
        // exec ↔ noexec
        set_option(&mut st, "noexec", true);
        assert!(st.is_checked("noexec") && !st.is_checked("exec"));
        let s = serialize_options(&st);
        assert!(s.contains("ro") && !s.contains(",rw,") && !s.contains("noauto,auto"));
        assert!(
            !s.contains("exec,") || s.contains("noexec"),
            "exec 已被 noexec 取代"
        );
    }

    #[test]
    fn options_noatime_linkage() {
        // D7.9 联动③：勾 noatime → 取消 relatime、联动勾 nodiratime
        let mut st = OptionsState::default();
        assert!(st.is_checked("relatime"));
        set_option(&mut st, "noatime", true);
        assert!(st.is_checked("noatime"));
        assert!(!st.is_checked("relatime"), "勾 noatime 取消 relatime");
        assert!(st.is_checked("nodiratime"), "勾 noatime 隐含勾 nodiratime");
        assert!(!st.is_checked("diratime"), "nodiratime 互斥 diratime");
        // 取消 noatime → nodiratime 保留（之后可独立处理）
        set_option(&mut st, "noatime", false);
        assert!(!st.is_checked("noatime") && st.is_checked("nodiratime"));
        // 对称防呆：勾 relatime → 取消 noatime
        set_option(&mut st, "noatime", true);
        set_option(&mut st, "relatime", true);
        assert!(st.is_checked("relatime") && !st.is_checked("noatime"));
    }

    #[test]
    fn options散项凑满还原_defaults_word() {
        // 散项凑满 7 项 → 写出 defaults 一词（D7.9 联动①）
        let mut st = OptionsState::default();
        set_option(&mut st, "defaults", false); // 取消 defaults（relatime 仍在）
        assert!(!st.is_checked("defaults"));
        assert_eq!(
            serialize_options(&st),
            "relatime",
            "defaults 取消后剩 relatime"
        );
        set_option(&mut st, "relatime", false);
        assert_eq!(
            serialize_options(&st),
            "defaults",
            "全空兜底 defaults（选项字段不可空）"
        );
        // 7 个散项逐一勾上
        for p in DEFAULTS_PARTS {
            set_option(&mut st, p, true);
        }
        assert!(st.is_checked("defaults"), "散项凑满派生 defaults");
        assert_eq!(serialize_options(&st), "defaults");
        // 取消任一组成项 → defaults 自动取消转散项
        set_option(&mut st, "exec", false);
        assert!(!st.is_checked("defaults"));
        let s = serialize_options(&st);
        assert!(!s.contains("defaults"), "exec 出局后不能写 defaults：{s}");
        assert!(!s.contains("exec") || s.contains("noexec"), "exec 已出局");
    }

    #[test]
    fn options_keyvalue_preserved() {
        // key=value 原样保留、原序（GOAL 7.13 真值表 6；btrfs 本机真实串）
        let s1 = "subvol=root,compress=zstd:1";
        assert_eq!(serialize_options(&parse_options(s1)), s1);
        let s2 = "umask=0077,shortname=winnt";
        assert_eq!(serialize_options(&parse_options(s2)), s2);
        // 混合：清单内 + 清单外，勾选操作不丢 others
        let st = parse_options("defaults,noauto,subvol=@");
        assert_eq!(st.others, vec!["subvol=@".to_string()]);
        assert_eq!(serialize_options(&st), "defaults,noauto,subvol=@");
        let mut st2 = parse_options("noatime,ssd,subvol=@");
        set_option(&mut st2, "ro", true);
        let s = serialize_options(&st2);
        assert!(s.contains("ssd") && s.contains("subvol=@") && s.contains("ro"));
    }

    #[test]
    fn options_remount_never_serialized() {
        // remount 勾了也不写入 fstab（D7.9 边界）
        let st = parse_options("defaults,remount");
        assert!(st.is_checked("remount"), "解析保留勾选态供 UI 提示");
        let out = serialize_options(&st);
        assert!(!out.contains("remount"), "remount 不入串：{out}");
        assert_eq!(out, "defaults");
        // 用户勾上 remount → 序列化仍不含
        let mut st2 = OptionsState::default();
        set_option(&mut st2, "remount", true);
        assert!(!serialize_options(&st2).contains("remount"));
    }

    #[test]
    fn options_full_catalog_parse_serialize() {
        // 全量 7 组清单逐项 parse-serialize（GOAL 7.13 真值表 9）
        let mut count = 0;
        for (_, items) in OPTION_GROUPS {
            for (p, meaning) in items {
                assert!(!meaning.is_empty());
                count += 1;
                if *p == "remount" {
                    // 边界项：不入串
                    let out = serialize_options(&parse_options(p));
                    assert!(!out.contains("remount"));
                    continue;
                }
                assert_eq!(
                    serialize_options(&parse_options(p)),
                    *p,
                    "清单项 {p} 往返失败"
                );
            }
        }
        assert!(count >= 38, "清单项数不应缩水，实际 {count}");
        // 核心 9 项都必须在清单内（D7.9 首屏与全量两层同源）
        for (p, _, _) in CORE_OPTIONS {
            assert!(is_catalog_option(p), "核心项 {p} 不在清单");
        }
        assert!(is_catalog_option("defaults"));
    }

    #[test]
    fn options_bad_string_no_panic() {
        // 坏串不 panic（GOAL 7.13 真值表 10）
        for bad in ["", ",", " , , ", "====", "=", ",,,", " rw , , noauto "] {
            let st = parse_options(bad);
            let _ = serialize_options(&st);
        }
        // 只有清单外项 → others 原样，不进默认态分支
        let st = parse_options("=");
        assert_eq!(st.others, vec!["=".to_string()]);
        assert_eq!(serialize_options(&st), "=");
        // 清单外项不可勾选
        let mut st2 = st.clone();
        set_option(&mut st2, "ssd", true);
        assert!(
            !st2.checked.contains(&"ssd".to_string()),
            "清单外项勾选被忽略"
        );
    }

    #[test]
    fn options_roundtrip_real_machine_strings() {
        // 本机 /etc/fstab 真实串逐条往返（值取自 7.2 真实读取，非凭记忆）
        for s in [
            "defaults,relatime",
            "defaults,noauto,nofail",
            "subvol=root,compress=zstd:1",
            "defaults",
            "noatime,nodiratime",
        ] {
            assert_eq!(
                serialize_options(&parse_options(s)),
                s,
                "真实串 {s} 往返失败"
            );
        }
    }

    // ---------------- 7.14 文件系统类型下拉清单（D7.10） ----------------

    #[test]
    fn fstype_options_complete_and_unique() {
        // 清单写全 + 无重复 + 形态合法
        assert_eq!(FSTYPE_OPTIONS[0], "auto", "auto 必须在首位");
        assert!(
            FSTYPE_OPTIONS.len() >= 60,
            "清单要写全，实际只有 {} 项",
            FSTYPE_OPTIONS.len()
        );
        let mut seen = std::collections::BTreeSet::new();
        for t in FSTYPE_OPTIONS {
            assert!(!t.is_empty(), "空项");
            assert!(
                t.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "非法字符：{t}"
            );
            assert!(seen.insert(*t), "重复项：{t}");
        }
        // 关键类型全覆盖（桌面/服务器/移动介质/网络/伪 fs）
        for must in [
            "auto", "ext4", "ext3", "ext2", "xfs", "btrfs", "f2fs", "bcachefs", "vfat", "exfat",
            "ntfs3", "ntfs", "iso9660", "squashfs", "erofs", "zfs", "swap", "nfs", "nfs4", "cifs",
            "smb3", "9p", "fuse", "fuseblk", "tmpfs", "overlay", "proc", "sysfs",
        ] {
            assert!(
                FSTYPE_OPTIONS.contains(&must),
                "清单缺关键类型 {must}（清单要写全）"
            );
        }
    }

    // ---------------- 7.6 管理操作 ----------------

    /// 新增合法条目：追加到末尾、默认 dump/pass=0、serialize 可回读。
    #[test]
    fn add_entry_ok_roundtrip() {
        let mut doc = parse("UUID=aaaa /a ext4 defaults 0 0\n").unwrap();
        let n = FstabEntry::new("UUID=bbbb", "/mnt/data", "ext4", "defaults,noauto");
        add_entry(&mut doc, n).expect("新挂载点应可新增");
        assert_eq!(doc.entry_count(), 2);
        let text = serialize(&doc);
        assert!(
            text.contains("UUID=bbbb /mnt/data ext4 defaults,noauto 0 0"),
            "{text}"
        );
        let reparsed = parse(&text).expect("新增行必须能被自家解析器读回");
        assert_eq!(reparsed.entry_count(), 2);
    }

    /// 新增被拒的各形态：重复活动挂载点/相对挂载点/非法 fstype/非数字 dump-pass/空字段。
    #[test]
    fn add_entry_rejects_bad_input() {
        let mut doc =
            parse("UUID=aaaa /a ext4 defaults 0 0\n# UUID=cccc /b ext4 defaults 0 0\n").unwrap();

        // 重复活动挂载点
        let dup = FstabEntry::new("UUID=bbbb", "/a", "ext4", "defaults");
        assert!(
            add_entry(&mut doc, dup)
                .unwrap_err()
                .contains("已被第 1 行占用")
        );

        // 停用条目不占位：/b 已被注释停用 → 可新增
        let reuse = FstabEntry::new("UUID=dddd", "/b", "ext4", "defaults");
        add_entry(&mut doc, reuse).expect("停用条目不应挡住新增");
        assert_eq!(doc.entry_count(), 3);

        // 相对挂载点
        let rel = FstabEntry::new("UUID=eeee", "mnt/c", "ext4", "defaults");
        assert!(add_entry(&mut doc, rel).unwrap_err().contains("/ 开头"));

        // 非法 fstype（含空白）
        let badtype = FstabEntry::new("UUID=ffff", "/d", "ext 4", "defaults");
        assert!(
            add_entry(&mut doc, badtype)
                .unwrap_err()
                .contains("非法字符")
        );

        // 非数字 dump（findmnt 段错误防线，7.4 同因）
        let mut odd = FstabEntry::new("UUID=1111", "/e", "ext4", "defaults");
        odd.set_dump("x");
        assert!(
            add_entry(&mut doc, odd)
                .unwrap_err()
                .contains("dump 必须为数字")
        );

        // 空 source
        let empty = FstabEntry::new("  ", "/f", "ext4", "defaults");
        assert!(add_entry(&mut doc, empty).unwrap_err().contains("source"));
    }

    /// 注释停用/恢复：按行号定位、序列化带 `# ` 前缀、恢复后回活动行、未知行号报错、幂等。
    #[test]
    fn toggle_disabled_lifecycle() {
        let mut doc =
            parse("UUID=aaaa /a ext4 defaults 0 0\nUUID=bbbb /b ext4 defaults 0 0\n").unwrap();

        set_entry_disabled(&mut doc, 2, true).unwrap();
        let text = serialize(&doc);
        assert!(text.contains("# UUID=bbbb /b ext4 defaults 0 0"), "{text}");
        assert!(doc.entries().nth(1).unwrap().disabled);

        // 序列化→回读仍是「停用条目」而非普通注释
        let back = parse(&text).unwrap();
        assert!(back.entries().nth(1).unwrap().disabled);

        set_entry_disabled(&mut doc, 2, false).unwrap();
        let text2 = serialize(&doc);
        assert!(
            text2.starts_with("UUID=aaaa /a ext4 defaults 0 0\nUUID=bbbb"),
            "{text2}"
        );

        // 未知行号
        assert!(
            set_entry_disabled(&mut doc, 99, true)
                .unwrap_err()
                .contains("未找到")
        );

        // 幂等：重复停用不产生 "## " 叠加
        set_entry_disabled(&mut doc, 1, true).unwrap();
        set_entry_disabled(&mut doc, 1, true).unwrap();
        let text3 = serialize(&doc);
        assert!(!text3.contains("## "), "{text3}");
    }

    /// 编辑条目：字段更新走 canonical 行（raw 清空）、行号不变、
    /// 重复挂载点排除自身、坏输入复用 add 校验、未知行号报错。
    #[test]
    fn edit_entry_updates_fields_in_place() {
        let mut doc =
            parse("# 注释保留\nUUID=aaaa /a ext4 defaults 0 0\nUUID=bbbb /b ext4 defaults 0 0\n")
                .unwrap();

        // 挂载点不变的「自编辑」不能被自己的旧值挡住（排除自身）
        let mut e = doc.entries().next().unwrap().clone();
        e.set_options("defaults,nofail");
        edit_entry(&mut doc, 2, e).unwrap();

        let edited = doc.entries().next().unwrap();
        assert_eq!(edited.options, "defaults,nofail");
        assert_eq!(edited.line_no, 2); // 行号不变
        assert!(edited.raw.is_none()); // setter 清 raw → canonical 输出

        // 改挂载点撞上另一条活动条目 → 拒绝
        let mut clash = doc.entries().next().unwrap().clone();
        clash.set_mountpoint("/b");
        assert!(
            edit_entry(&mut doc, 2, clash)
                .unwrap_err()
                .contains("已被第 3 行占用")
        );

        // 字段级坏输入（dump 非数字）复用 add 校验
        let mut odd = doc.entries().next().unwrap().clone();
        odd.set_dump("x");
        assert!(
            edit_entry(&mut doc, 2, odd)
                .unwrap_err()
                .contains("dump 必须为数字")
        );

        // 未知行号
        assert!(
            edit_entry(
                &mut doc,
                99,
                FstabEntry::new("UUID=cccc", "/c", "ext4", "defaults")
            )
            .unwrap_err()
            .contains("未找到")
        );

        // 序列化回读：注释行保真、编辑行是 canonical、另一条原样
        let text = serialize(&doc);
        let back = parse(&text).unwrap();
        assert_eq!(back.entry_count(), 2);
        assert_eq!(back.entries().next().unwrap().options, "defaults,nofail");
        assert!(text.contains("# 注释保留"), "{text}");
        assert!(text.contains("UUID=bbbb /b ext4 defaults 0 0"), "{text}");
    }

    /// 立即挂载/卸载/建目录命令构造（D7.6）：普通条目与 swap 分流。
    #[test]
    fn mount_command_shapes() {
        let ext4 = FstabEntry::new("UUID=aaaa", "/mnt/x", "ext4", "defaults");
        assert_eq!(mount_command(&ext4), ["pkexec", "mount", "/mnt/x"]);
        assert_eq!(umount_command(&ext4), ["pkexec", "umount", "/mnt/x"]);

        let swap = FstabEntry::new("UUID=bbbb", "none", "swap", "sw");
        assert!(swap.is_swap());
        assert_eq!(mount_command(&swap), ["pkexec", "swapon", "UUID=bbbb"]);
        assert_eq!(umount_command(&swap), ["pkexec", "swapoff", "UUID=bbbb"]);

        assert_eq!(
            ensure_mountpoint_dir_command("/mnt/x"),
            ["pkexec", "mkdir", "-p", "/mnt/x"]
        );
    }

    /// golden：真实 fstab 解析出 4 条目 + 10 注释 + 1 空行，结尾换行保留。
    #[test]
    fn real_fstab_parses_golden() {
        let doc = parse(REAL_FSTAB).expect("真实 fstab 必须能解析");
        assert_eq!(doc.entry_count(), 4);
        assert_eq!(doc.lines.len(), 15, "1 空行 + 10 注释 + 4 条目");
        assert!(doc.trailing_newline);
        let e: Vec<&FstabEntry> = doc.entries().collect();
        assert_eq!(e[0].mountpoint, "/");
        assert_eq!(e[0].vfstype, "btrfs");
        assert_eq!(e[0].options, "subvol=root,compress=zstd:1");
        assert_eq!(e[1].dump, "1");
        assert_eq!(e[1].pass, "2");
        assert_eq!(e[2].source, "UUID=3912-7550");
        assert_eq!(e[3].mountpoint, "/home");
        assert!(e.iter().all(|x| !x.disabled));
    }

    /// golden 往返：parse → serialize 与真实文件**逐字节相等**（D7.4 核心承诺）。
    #[test]
    fn real_fstab_roundtrip_byte_identical() {
        let doc = parse(REAL_FSTAB).unwrap();
        assert_eq!(serialize(&doc), REAL_FSTAB);
        // 二次往返（防首次往返掩盖的状态漂移）
        let doc2 = parse(&serialize(&doc)).unwrap();
        assert_eq!(serialize(&doc2), REAL_FSTAB);
    }

    /// 无结尾换行的文件往返后不得凭空补换行。
    #[test]
    fn no_trailing_newline_roundtrip() {
        let src = "UUID=aaaa /a ext4 defaults 0 0";
        let doc = parse(src).unwrap();
        assert!(!doc.trailing_newline);
        assert_eq!(serialize(&doc), src);
    }

    /// 空文本与纯注释文本都是合法文档（0 条目），往返保真。
    #[test]
    fn empty_and_comment_only_ok() {
        let doc = parse("").unwrap();
        assert_eq!(doc.entry_count(), 0);
        assert_eq!(serialize(&doc), "");

        let src = "# 只有注释\n\n# 第二段\n";
        let doc = parse(src).unwrap();
        assert_eq!(doc.entry_count(), 0);
        assert_eq!(serialize(&doc), src);
    }

    /// 错误输入：1 字段、2 字段行必须 Err（2 字段 = findmnt 段错误实测防线）。
    #[test]
    fn short_lines_rejected() {
        let err = parse("UUID=aaaa\n").unwrap_err();
        assert!(err.contains("第 1 行"), "错误须带行号：{err}");
        assert!(err.contains("字段数 1"), "错误须带字段数：{err}");

        let err = parse("# 头\nUUID=aaaa /mnt\n").unwrap_err();
        assert!(err.contains("第 2 行"), "错误须带真实行号：{err}");
    }

    /// 错误输入：dump/pass 非数字必须 Err（实测 findmnt --verify 对此段错误 RC=139）。
    #[test]
    fn non_numeric_dump_pass_rejected() {
        let err = parse("UUID=aaaa /mnt/x ext4 defaults x y\n").unwrap_err();
        assert!(err.contains("dump"), "应指出 dump 字段：{err}");

        let err = parse("UUID=aaaa /mnt/x ext4 defaults 0 0x\n").unwrap_err();
        assert!(err.contains("pass"), "应指出 pass 字段：{err}");
        assert!(err.contains("第 1 行"));
    }

    /// 边界：4 字段缺省 dump/pass=0；5 字段只缺 pass；制表符分隔。
    #[test]
    fn four_five_fields_and_tabs_default_zero() {
        let doc = parse("UUID=aaaa /mnt/x ext4 defaults\n").unwrap();
        let e = doc.entries().next().unwrap();
        assert_eq!((e.dump.as_str(), e.pass.as_str()), ("0", "0"));

        let doc = parse("UUID=aaaa /mnt/x ext4 defaults 1\n").unwrap();
        let e = doc.entries().next().unwrap();
        assert_eq!((e.dump.as_str(), e.pass.as_str()), ("1", "0"));

        let doc = parse("UUID=aaaa\t/mnt/x\text4\tdefaults\t0\t2\n").unwrap();
        let e = doc.entries().next().unwrap();
        assert_eq!((e.mountpoint.as_str(), e.pass.as_str()), ("/mnt/x", "2"));
        assert_eq!(serialize(&doc), "UUID=aaaa\t/mnt/x\text4\tdefaults\t0\t2\n");
    }

    /// swap 条目（mountpoint=none）解析与序列化。
    #[test]
    fn swap_none_entry() {
        let doc = parse("UUID=aaaa none swap sw 0 0\n").unwrap();
        let e = doc.entries().next().unwrap();
        assert!(e.is_swap());
        assert_eq!(e.mountpoint, "none");
        assert_eq!(serialize(&doc), "UUID=aaaa none swap sw 0 0\n");
    }

    /// 停用条目识别：`#UUID=…` 与 `# UUID=…` 都识别为 disabled 条目且 raw 保真。
    #[test]
    fn disabled_entry_detected() {
        let src = "#UUID=aaaa /data ext4 defaults 0 0\n# UUID=bbbb none swap sw 0 0\n";
        let doc = parse(src).unwrap();
        let e: Vec<&FstabEntry> = doc.entries().collect();
        assert_eq!(e.len(), 2);
        assert!(e.iter().all(|x| x.disabled));
        assert_eq!(e[1].mountpoint, "none");
        assert_eq!(serialize(&doc), src, "停用行原样保真");
        // 恢复启用 → 行首 # 去掉
        let mut doc2 = doc.clone();
        doc2.entries_mut().for_each(|x| x.set_disabled(false));
        assert_eq!(
            serialize(&doc2),
            "UUID=aaaa /data ext4 defaults 0 0\nUUID=bbbb none swap sw 0 0\n"
        );
    }

    /// 本机 anaconda 注释头不得被误认成停用条目（四项形态检查的回归锚）。
    #[test]
    fn anaconda_prose_stays_comment() {
        let doc = parse(REAL_FSTAB).unwrap();
        let entries = doc.entry_count();
        let comments = doc
            .lines
            .iter()
            .filter(|l| matches!(l, FstabLine::Comment(_)))
            .count();
        assert_eq!(entries, 4, "普通注释绝不计入条目");
        assert_eq!(comments, 10, "10 行注释全部保留为 Comment");
        // 形态检查四条单独验：source 不像设备 / 挂载点不以 / 开头
        assert!(try_disabled_entry("See man pages fstab(5), findfs(8), mount(8) info.").is_none());
        assert!(try_disabled_entry("/etc/fstab - static file system information").is_none());
        assert!(try_disabled_entry("UUID=aaaa notamount ext4 defaults 0 0").is_none());
        assert!(try_disabled_entry("UUID=aaaa /data ext4 defaults 0 0").is_some());
    }

    /// 编辑只影响目标行：其他行（含对齐空格）逐字节不动。
    #[test]
    fn edit_touches_only_target_line() {
        let mut doc = parse(REAL_FSTAB).unwrap();
        doc.entries_mut()
            .find(|e| e.mountpoint == "/boot")
            .unwrap()
            .set_options("defaults,noatime");
        let out = serialize(&doc);
        let orig: Vec<&str> = REAL_FSTAB.split('\n').collect();
        let new: Vec<&str> = out.split('\n').collect();
        assert_eq!(orig.len(), new.len());
        let mut changed = 0;
        for (i, (a, b)) in orig.iter().zip(new.iter()).enumerate() {
            if a != b {
                changed += 1;
                assert_eq!(i, 12, "只有 /boot 行（第 13 行）允许变");
            }
        }
        assert_eq!(changed, 1);
        // 被编辑行 raw 失效 → 重新生成为单空格规范格式（对齐空格是有意放弃的：raw 已失效）
        assert!(out.contains("/boot ext4 defaults,noatime 1 2"));
        // 未编辑的 / 行保持原始对齐空格
        assert!(
            out.contains(
                "UUID=cdb4f2f6-ff9b-4a25-9f5a-a390ab89aaeb /                       btrfs   subvol=root,compress=zstd:1 0 0"
            ),
            "未编辑行必须保持原字节"
        );
    }

    /// 转义往返：写入 escape、展示 unescape，二者互逆且生成行恒为 6 字段。
    #[test]
    fn escape_unescape_roundtrip() {
        assert_eq!(unescape_field("/mnt/my\\040disk"), "/mnt/my disk");
        assert_eq!(unescape_field("a\\011b"), "a\tb");
        assert_eq!(
            escape_field(&unescape_field("a\\040b")),
            "a\\040b",
            "不二次转义"
        );
        // 含空格挂载点：解析（原文存）→ 生成行 6 字段 → 再解析字段一致
        let src = "UUID=aaaa /mnt/my\\040disk ext4 defaults 0 0\n";
        let doc = parse(src).unwrap();
        assert_eq!(serialize(&doc), src);
        let e = doc.entries().next().unwrap();
        assert_eq!(e.mountpoint, "/mnt/my\\040disk", "模型存原文");
        assert_eq!(
            serialize(&doc).split_whitespace().count(),
            src.split_whitespace().count()
        );
    }

    /// normalize_mountpoint：尾斜杠等价、根路径不被吃成空串。
    #[test]
    fn normalize_mountpoint_rules() {
        assert_eq!(normalize_mountpoint("/home/"), "/home");
        assert_eq!(normalize_mountpoint("/home"), "/home");
        assert_eq!(normalize_mountpoint("/"), "/");
        assert_eq!(normalize_mountpoint("//"), "/");
    }

    /// 重复挂载点只告警不报错（GOAL 7.2 边界：停用条目不参与重复判定）。
    #[test]
    fn duplicate_mountpoints_warn_only() {
        let doc = parse(
            "UUID=aaaa /data ext4 defaults 0 0\nUUID=bbbb /data xfs defaults 0 0\n#UUID=cccc /data ext4 noauto 0 0\n",
        )
        .unwrap();
        let dups = duplicate_mountpoints(&doc);
        assert_eq!(dups.get("/data"), Some(&2), "停用条目不计入");

        let clean =
            parse("UUID=aaaa /a ext4 defaults 0 0\nUUID=bbbb /b ext4 defaults 0 0\n").unwrap();
        assert!(duplicate_mountpoints(&clean).is_empty());
    }

    /// findmnt JSON 展平：嵌套 children 全部收到（本机真实输出结构）。
    #[test]
    fn findmnt_json_flatten() {
        let j = r#"{"filesystems":[{"target":"/","source":"/dev/sda1","fstype":"ext4","options":"rw","children":[
            {"target":"/boot","source":"/dev/sda1","fstype":"ext4","options":"rw","children":[
                {"target":"/boot/efi","source":"/dev/sda2","fstype":"vfat","options":"rw"}]}]}]}"#;
        let v = parse_findmnt_json(j).unwrap();
        let targets: Vec<&str> = v.iter().map(|m| m.target.as_str()).collect();
        assert_eq!(targets, vec!["/", "/boot", "/boot/efi"]);
        assert_eq!(v[2].source, "/dev/sda2");
        assert!(parse_findmnt_json("{}").is_err(), "缺 filesystems 必须报错");
        assert!(parse_findmnt_json("not json").is_err());
    }

    /// resolve_source：UUID/LABEL/PARTUUID/裸设备映射到 /dev 路径；远程源返回 None。
    #[test]
    fn resolve_source_mapping() {
        assert_eq!(
            resolve_source("UUID=abc-123"),
            Some(PathBuf::from("/dev/disk/by-uuid/abc-123"))
        );
        assert_eq!(
            resolve_source("LABEL=data"),
            Some(PathBuf::from("/dev/disk/by-label/data"))
        );
        assert_eq!(
            resolve_source("PARTUUID=1234-01"),
            Some(PathBuf::from("/dev/disk/by-partuuid/1234-01"))
        );
        assert_eq!(
            resolve_source("/dev/sdb1"),
            Some(PathBuf::from("/dev/sdb1"))
        );
        assert_eq!(resolve_source("nas:/share"), None, "NFS 远程源不判定");
        assert_eq!(resolve_source("//nas/share"), None, "CIFS 远程源不判定");
        assert_eq!(resolve_source("none"), None);
    }

    /// mount_status 真值表：优先级 已挂载 > 已停用 > 挂载点不存在 > 源不存在 > 未挂载。
    #[test]
    fn mount_status_truth_table() {
        use crate::model::fstab::MountStatus as S;
        let e = FstabEntry::new("UUID=a", "/data", "ext4", "defaults");
        let mut mounted = HashSet::new();
        // 现实优先：已挂载的条目即使标记停用也报已挂载
        mounted.insert("/data".to_string());
        assert_eq!(mount_status(&e, &mounted, true, true), S::Mounted);
        mounted.clear();

        let mut d = e.clone();
        d.set_disabled(true);
        assert_eq!(mount_status(&d, &mounted, true, true), S::Disabled);

        let live = FstabEntry::new("UUID=a", "/data", "ext4", "defaults");
        assert_eq!(mount_status(&live, &mounted, false, true), S::DirMissing);
        assert_eq!(mount_status(&live, &mounted, true, false), S::SourceMissing);
        assert_eq!(mount_status(&live, &mounted, true, true), S::NotMounted);

        // swap（none）不做挂载点目录检查：目录不存在也不报 DirMissing
        let swap = FstabEntry::new("UUID=a", "none", "swap", "sw");
        assert_eq!(mount_status(&swap, &mounted, false, true), S::NotMounted);
    }

    /// lsblk JSON：递归收集 part 节点，整盘（type=disk）不收，mountpoints 判 mounted。
    #[test]
    fn lsblk_json_partitions() {
        let j = r#"{"blockdevices":[
          {"name":"nvme0n1","path":"/dev/nvme0n1","type":"disk","fstype":null,"size":"476.9G","label":null,"uuid":null,"mountpoints":null,
           "children":[
             {"name":"nvme0n1p1","path":"/dev/nvme0n1p1","type":"part","fstype":"vfat","size":"16M","label":"BOOT","uuid":"AAAA-1111","mountpoints":["/boot/efi"]},
             {"name":"nvme0n1p2","path":"/dev/nvme0n1p2","type":"part","fstype":"ext4","size":"100G","label":null,"uuid":"bbbb-2222","mountpoints":[null]},
             {"name":"nvme0n1p3","path":"/dev/nvme0n1p3","type":"part","fstype":null,"size":"100G","label":null,"uuid":null,"mountpoints":[]}
           ]}
        ]}"#;
        let v = parse_lsblk_json(j).unwrap();
        assert_eq!(v.len(), 3, "整盘不收，3 个分区全收");
        assert!(v[0].mounted, "mountpoints 非空字符串 → 已挂载");
        assert!(!v[1].mounted, "[null] 视为未挂载");
        assert!(!v[2].mounted);
        assert_eq!(v[0].uuid.as_deref(), Some("AAAA-1111"));
        assert_eq!(v[2].fstype, None, "未格式化分区保留 None");
        assert!(parse_lsblk_json("{}").is_err());
    }

    // ── 7.4 写前校验闸门（真值表 ≥8 用例）─────────────────────────────────

    /// golden 快照里的 root UUID 是否存在于本机 `/dev/disk/by-uuid/`。
    ///
    /// 2026-10-09 实测：`REAL_FSTAB` 是 2026-10-02 抄录的真实 fstab（btrfs +
    /// `UUID=cdb4f2f6…`），而本机此后**重装过系统**（现为 ext4 +
    /// `UUID=587fe140-f6cb-49fd-8704-a72574e96b5b`，`/dev/disk/by-uuid/` 已无旧 UUID）。
    fn golden_uuid_on_this_machine() -> bool {
        std::path::Path::new("/dev/disk/by-uuid/cdb4f2f6-ff9b-4a25-9f5a-a390ab89aaeb").exists()
    }

    /// 真值表①：本机真实 fstab → 放行（0 errors），权限类 warning 透传。
    ///
    /// **环境自适应（GOAL 4.6 先例，2026-10-09）**：golden 与本机设备不匹配时，
    /// `findmnt --verify` 必报 `unreachable on boot required source: UUID=…`
    /// ——这是快照过期、不是校验闸门的缺陷；此时仍走完整 `validate_text` 路径，
    /// 但只断言 findmnt 报错**全部是 unreachable 类**，自家解析闸门照常放行。
    #[test]
    fn validate_real_fstab_passes_gate() {
        match validate_text(REAL_FSTAB) {
            Ok(warnings) => eprintln!("真实 fstab 放行，warnings={warnings:?}"),
            Err(errors) => {
                if golden_uuid_on_this_machine() {
                    panic!("真实 fstab 应放行，却得到 errors={errors:?}");
                }
                // 快照 UUID 不在本机：只允许 findmnt 的 unreachable 类报错
                for e in &errors {
                    if let Some(detail) = e.strip_prefix("[E]") {
                        assert!(
                            detail.contains("unreachable on boot"),
                            "非快照过期类错误（需人工排查）：{e}"
                        );
                    }
                }
                eprintln!(
                    "本机无 golden UUID（系统重装过）→ 自家闸门放行、findmnt 仅 unreachable：{errors:?}"
                );
            }
        }
    }

    /// 真值表②：必填字段缺失（options 空）→ 自家拦截，不进 findmnt。
    #[test]
    fn validate_rejects_missing_required_field() {
        let mut doc = parse("UUID=aaaa /mnt/x ext4 defaults 0 0\n").unwrap();
        doc.entries_mut().next().unwrap().set_options("  ");
        let errors = validate_for_write(&doc).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("挂载选项为空")),
            "{errors:?}"
        );
    }

    /// 真值表③：重复挂载点（启用中）→ 自家拦截。
    #[test]
    fn validate_rejects_duplicate_mountpoints() {
        let doc =
            parse("UUID=aaaa /data ext4 defaults 0 0\nUUID=bbbb /data xfs defaults 0 0\n").unwrap();
        let errors = validate_for_write(&doc).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("重复占用")), "{errors:?}");
    }

    /// 真值表④：空文档与全停用文档 → 自家拦截（findmnt 会给 Success，不能信它）。
    #[test]
    fn validate_rejects_empty_or_all_disabled() {
        let empty = parse("").unwrap();
        let errors = validate_for_write(&empty).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("没有任何启用条目")),
            "{errors:?}"
        );

        let disabled = parse("#UUID=aaaa /data ext4 defaults 0 0\n").unwrap();
        assert!(validate_for_write(&disabled).is_err());
    }

    /// 真值表⑤：dump/pass 非数字（构造绕过 parse）→ 自家拦截（段错误防线）。
    #[test]
    fn validate_rejects_non_numeric_dump_pass() {
        let mut doc = parse("UUID=aaaa /mnt/x ext4 defaults 0 0\n").unwrap();
        doc.entries_mut().next().unwrap().set_dump("x");
        let errors = validate_for_write(&doc).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("dump")), "{errors:?}");
    }

    /// 真值表⑥：2 字段行 → 解析层拦截（绝不进 findmnt，防段错误）。
    #[test]
    fn validate_text_rejects_short_line_before_findmnt() {
        let errors = validate_text("UUID=aaaa /mnt\n").unwrap_err();
        assert!(errors[0].contains("字段数 2"), "{errors:?}");
    }

    /// 真值表⑦：目标目录不存在且无 noauto → findmnt 2 errors → 拒绝（真调 findmnt）。
    #[test]
    fn validate_missing_dir_without_noauto_rejected() {
        let text = "UUID=aaaa /mnt/linbox_no_such_dir ext4 defaults 0 0\n";
        let errors = validate_text(text).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("errors")),
            "应含 findmnt 汇总行：{errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains("[E]")),
            "应含 findmnt [E] 详情：{errors:?}"
        );
    }

    /// 真值表⑧：同目标 + noauto → findmnt 0 errors + warnings → 放行且透传警告。
    #[test]
    fn validate_missing_dir_with_noauto_passes_with_warnings() {
        let text = "UUID=aaaa /mnt/linbox_no_such_dir ext4 defaults,noauto 0 0\n";
        let warnings = validate_text(text).expect("noauto 应放行");
        assert!(!warnings.is_empty(), "warnings 应透传非空");
        assert!(
            warnings.iter().any(|w| w.starts_with("[W]")),
            "应为 findmnt [W] 行：{warnings:?}"
        );
    }

    // ── 7.5 写入链路（dry-run 全流程）──────────────────────────────────────

    /// dry-run 全流程三证：备份存在且为旧内容 + 回读逐字节一致 + daemon-reload RC=0。
    /// 目标用 TMPDIR 下的临时文件（提权路径不进单测，见 GOAL 7.6/7.7 手测）。
    #[test]
    fn write_fstab_dryrun_full_chain() {
        let target =
            std::env::temp_dir().join(format!("linbox-fstab-dryrun-{}", std::process::id()));
        let old = "UUID=oldaaaa /a ext4 defaults 0 0\n";
        let new = "UUID=newaaaa /a ext4 defaults,noauto 0 0\n";
        std::fs::write(&target, old).unwrap();

        let report = write_fstab_with_reload(&target, new, || 0).expect("dry-run 全链路应成功");
        // 证1：备份存在且是旧内容
        let bak = report.backup.expect("目标原先存在 → 应有备份");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), old, "备份=旧内容");
        // 证2：回读逐字节一致（write_fstab 内部已比对，这里再从外部复核）
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            new,
            "回读=新内容"
        );
        // 证3：daemon-reload 退出码
        assert_eq!(report.reload_rc, 0, "daemon-reload 应成功");
        assert!(!report.elevated, "TMPDIR 归当前用户 → 直写路径");

        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_file(&bak);
    }

    /// reload 失败 → 从备份回滚，目标恢复旧内容（GOAL 7.5 失败路径，注入 `|| 1`）。
    #[test]
    fn reload_failure_rolls_back() {
        let target =
            std::env::temp_dir().join(format!("linbox-fstab-reloadfail-{}", std::process::id()));
        let old = "UUID=oldaaaa /a ext4 defaults 0 0\n";
        std::fs::write(&target, old).unwrap();

        let err = write_fstab_with_reload(&target, "UUID=newaaaa /a ext4 noauto 0 0\n", || 1)
            .unwrap_err();
        assert!(err.contains("daemon-reload 失败"), "{err}");
        assert!(err.contains("回滚"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            old,
            "失败后必须恢复旧内容"
        );
        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_file(backup_path(&target));
    }

    /// 真机证据（GOAL 7.5 三证，`--ignored` 默认不跑）：对 /tmp/fstab.dryrun
    /// 走**真实** daemon-reload 全链路，--nocapture 可见三证输出。
    /// 非 root 会触发 polkit 弹框（无人值守超时）→ 用 `sudo -E cargo test
    /// dryrun_real_evidence -- --ignored --nocapture` 跑，或桌面有人时直跑。
    #[test]
    #[ignore = "真实 daemon-reload 需 polkit 授权，见模块注释"]
    fn dryrun_real_evidence() {
        let target = Path::new("/tmp/fstab.dryrun");
        let _ = std::fs::remove_file(target);
        let _ = std::fs::remove_file(backup_path(target));
        let old = "UUID=olddry /mnt/dry ext4 defaults 0 0\n";
        let new = "UUID=newdry /mnt/dry ext4 defaults,noauto 0 0\n";
        std::fs::write(target, old).unwrap();

        // 真机证据的 reload：无人值守下普通用户的 polkit 弹框无人应答会超时
        // （2026-10-02 实测 RC=1），设置了 SUDO_ASKPASS 时改走 `sudo -A`
        // 执行真实 daemon-reload；桌面有人时直接跑则走普通 systemctl（polkit 对话框）。
        let r = write_fstab_with_reload(target, new, || {
            let mut cmd = std::process::Command::new("systemctl");
            if std::env::var_os("SUDO_ASKPASS").is_some() {
                let mut s = std::process::Command::new("sudo");
                s.arg("-A").args(["systemctl", "daemon-reload"]);
                return match s.status() {
                    Ok(st) => st.code().unwrap_or(-1),
                    Err(_) => -1,
                };
            }
            match cmd.status() {
                Ok(st) => st.code().unwrap_or(-1),
                Err(_) => -1,
            }
        })
        .expect("真机 dry-run 全链路应成功");
        // 证1：备份存在且为旧内容
        let bak = r.backup.as_ref().expect("应有备份");
        let bak_bytes = std::fs::read(bak).expect("备份可读");
        assert_eq!(bak_bytes, old.as_bytes());
        // 证2：回读 diff 为空
        let diff_empty = std::fs::read(target).unwrap() == new.as_bytes();
        assert!(diff_empty);
        // 证3：daemon-reload 退出码
        assert_eq!(r.reload_rc, 0);
        eprintln!(
            "证1 备份存在: {} ({} 字节, 内容=旧文件)",
            bak.display(),
            bak_bytes.len()
        );
        eprintln!("证2 回读 diff 为空: {diff_empty}");
        eprintln!("证3 daemon-reload RC = {}", r.reload_rc);
        eprintln!(
            "证4 属主/权限: {:?}",
            std::fs::metadata(target).unwrap().permissions()
        );
    }

    /// 坏内容被链路拒绝：目标文件不被触碰、不产生备份（防线在 parse 层）。
    #[test]
    fn write_fstab_rejects_bad_content_without_touching_target() {
        let target =
            std::env::temp_dir().join(format!("linbox-fstab-guard-{}", std::process::id()));
        std::fs::write(&target, "original\n").unwrap();
        let err = write_fstab(&target, "UUID=aaaa /mnt\n").unwrap_err();
        assert!(err.contains("拒绝写入"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "original\n",
            "目标未动"
        );
        assert!(!backup_path(&target).exists(), "未走到备份步骤");
        let _ = std::fs::remove_file(&target);
    }

    /// 回滚：备份 → 篡改目标 → restore_from_backup 恢复原内容。
    #[test]
    fn restore_from_backup_restores_original() {
        let target =
            std::env::temp_dir().join(format!("linbox-fstab-rollback-{}", std::process::id()));
        let bak = backup_path(&target);
        std::fs::write(&target, "original\n").unwrap();
        std::fs::copy(&target, &bak).unwrap();
        std::fs::write(&target, "corrupted\n").unwrap();

        restore_from_backup(&target, true, false).expect("回滚应成功");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original\n");
        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_file(&bak);
    }

    /// 提权脚本形状：备份在落盘前、cat 落盘、chmod 0644、全程单引号转义（不实跑 pkexec）。
    #[test]
    fn elevated_write_script_shape() {
        let target = Path::new("/etc/fstab");
        let tmp = Path::new("/tmp/linbox-fstab-x.tmp");
        let s = elevated_write_script(target, tmp);
        let cp = s.find("cp -a").expect("必须先备份");
        let cat = s.find("cat").expect("必须有 cat 落盘");
        let chmod = s.find("chmod 0644").expect("必须 chmod 0644");
        assert!(cp < cat && cat < chmod, "顺序应为 备份→落盘→chmod：{s}");
        assert!(s.contains("'/etc/fstab.linbox.bak'"), "备份路径转义：{s}");
        assert!(
            !s.contains("chown"),
            "D7.2：不 chown 回用户（保持 root:root）"
        );
    }

    /// backup_path 命名：`<target>.linbox.bak`。
    #[test]
    fn backup_path_naming() {
        assert_eq!(
            backup_path(Path::new("/etc/fstab")),
            PathBuf::from("/etc/fstab.linbox.bak")
        );
    }

    /// Linux 真机 smoke（GOAL 7.3 数据链路）：真实 fstab × 真实 findmnt →
    /// 4 条目全出状态、且已挂载条目的挂载点与 findmnt 输出对得上。
    /// `cargo test status_table_real_machine -- --nocapture` 可看状态表。
    #[test]
    fn status_table_real_machine() {
        let text = std::fs::read_to_string(FSTAB_PATH).expect("真机 fstab 可读");
        let doc = parse(&text).expect("真机 fstab 可解析");
        let j = match run_findmnt_json() {
            Ok(j) => j,
            Err(e) => {
                eprintln!("findmnt 不可用，跳过：{e}");
                return;
            }
        };
        let statuses = status_table(&j, &doc).expect("状态表可算");
        assert_eq!(statuses.len(), doc.entry_count());
        let mounted_fs = parse_findmnt_json(&j).unwrap();
        let targets: HashSet<&str> = mounted_fs.iter().map(|m| m.target.as_str()).collect();
        for (e, s) in doc.entries().zip(statuses.iter()) {
            eprintln!(
                "{:<12} {:<14} {} -> {}",
                e.mountpoint,
                s.label(),
                e.source,
                e.vfstype
            );
            if targets.contains(e.mountpoint.as_str()) {
                assert_eq!(
                    *s,
                    crate::model::fstab::MountStatus::Mounted,
                    "在 findmnt 里必须报已挂载"
                );
            }
        }
        assert!(
            statuses.contains(&crate::model::fstab::MountStatus::Mounted),
            "本机至少根分区在挂"
        );
    }

    /// Linux 真机 smoke：/etc/fstab 存在时必须能解析且往返逐字节一致；
    /// findmnt/lsblk 可用时真实输出必须能解析（机器上必装，缺失则跳过对应段）。
    #[test]
    #[ignore = "探针：cairo toy API 中文渲染（截图人工核对后可删）"]
    fn probe_cairo_cjk_render() {
        use gtk::cairo::{Context, FontSlant, FontWeight, Format};
        let mut surface = gtk::cairo::ImageSurface::create(Format::ARgb32, 460, 60).unwrap();
        let ctx = Context::new(&surface).unwrap();
        ctx.set_source_rgb(1.0, 1.0, 1.0);
        let _ = ctx.paint();
        ctx.set_source_rgb(0.0, 0.0, 0.0);
        ctx.select_font_face("sans-serif", FontSlant::Normal, FontWeight::Normal);
        ctx.set_font_size(22.0);
        let _ = ctx.show_text("/mnt/数据 挂载点 test");
        let ext = ctx.text_extents("/mnt/数据 挂载点 test").unwrap();
        drop(ctx); // Context 借用 surface，先释放才能 data()
        // 无 png feature：手工转 PPM（cairo 行序为自上而下、BGRA 预乘）
        surface.flush();
        let stride = surface.stride() as usize;
        let w = surface.width() as usize;
        let h = surface.height() as usize;
        let data = surface.data().unwrap();
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        for y in 0..h {
            for x in 0..w {
                let i = y * stride + x * 4;
                let (b, g, rr) = (data[i], data[i + 1], data[i + 2]);
                ppm.extend_from_slice(&[rr, g, b]);
            }
        }
        std::fs::write(
            concat!(env!("HOME"), "/.hermes/cache/scratch/probe_cairo_cjk.ppm"),
            &ppm,
        )
        .unwrap();
        assert!(ext.width() > 20.0, "width = {}", ext.width());
    }

    #[test]
    fn linux_smoke_real_fstab_and_tools() {
        if Path::new(FSTAB_PATH).exists() {
            let text = std::fs::read_to_string(FSTAB_PATH).expect("真实 fstab 应可读");
            let doc = parse(&text).expect("真实 fstab 必须可解析");
            assert_eq!(serialize(&doc), text, "真实文件往返逐字节一致");
            assert!(doc.entry_count() >= 1, "至少 1 条挂载条目");
        }
        if let Ok(j) = run_findmnt_json() {
            let v = parse_findmnt_json(&j).expect("真机 findmnt JSON 必须可解析");
            assert!(v.iter().any(|m| m.target == "/"), "必含根挂载点");
        }
        if let Ok(j) = run_lsblk_json() {
            let v = parse_lsblk_json(&j).expect("真机 lsblk JSON 必须可解析");
            assert!(v.iter().all(|p| p.path.starts_with("/dev/")), "路径形态");
        }
        // 7.7 数据层：真机 lsblk 树 + df 用量必须能解析
        if let Ok(j) = run_lsblk_tree_json() {
            let disks = parse_lsblk_tree(&j).expect("真机 lsblk 树必须可解析");
            assert!(!disks.is_empty(), "至少一块盘");
            for d in &disks {
                assert!(!d.segments().is_empty(), "{} 至少一个分段", d.name);
                assert!(d.size_bytes > 0, "{} 字节数 > 0", d.name);
                assert!(
                    d.segments().iter().all(|p| p.size_bytes > 0),
                    "分段字节数 > 0"
                );
            }
        }
        if let Ok(t) = run_df_usage() {
            let rows = parse_df_usage(&t);
            assert!(rows.iter().any(|r| r.0 == "/"), "df 必含根挂载点");
            assert!(rows.iter().all(|r| r.1 > 0), "size > 0");
        }
    }

    // -----------------------------------------------------------------------
    // 待定操作队列（GOAL 7.8，≥8 用例）
    // -----------------------------------------------------------------------

    /// 两行条目的基线文档。
    fn queue_base() -> FstabDocument {
        parse("UUID=aaa / ext4 defaults 0 1\nUUID=bbb /boot ext4 defaults,noauto 0 2\n")
            .expect("基线解析")
    }

    fn entry_of(mp: &str, src: &str) -> FstabEntry {
        let mut e = FstabEntry::new(src.to_string(), mp.to_string(), "ext4", "defaults");
        e.set_dump("0");
        e.set_pass("0");
        e
    }

    #[test]
    fn queue_push_undo_clear() {
        let mut q = OpQueue::new();
        assert!(q.is_empty());
        q.push(PendingOp::Add(entry_of("/mnt/a", "UUID=x")));
        q.push(PendingOp::SetDisabled {
            line_no: 1,
            disabled: true,
        });
        assert_eq!(q.len(), 2);
        let undone = q.undo().expect("撤销返回被撤操作");
        assert_eq!(undone.describe(), "注释停用第 1 行");
        assert_eq!(q.len(), 1);
        q.clear();
        assert!(q.is_empty());
        assert!(q.undo().is_none(), "空队列撤销返回 None");
    }

    #[test]
    fn queue_preview_add_appends_entry() {
        let base = queue_base();
        let mut q = OpQueue::new();
        q.push(PendingOp::Add(entry_of("/mnt/data", "UUID=ccc")));
        let preview = q.preview(&base).expect("preview 成功");
        assert_eq!(preview.entry_count(), 3);
        let last = preview.entries().last().expect("末条");
        assert_eq!(last.mountpoint, "/mnt/data");
        assert_eq!(base.entry_count(), 2, "base 不被 preview 修改");
    }

    #[test]
    fn queue_consecutive_edits_merge_to_latest() {
        let mut q = OpQueue::new();
        let mut e1 = entry_of("/boot", "UUID=bbb");
        e1.set_options("defaults,noauto,x-systemd.automount");
        let mut e2 = entry_of("/boot", "UUID=bbb");
        e2.set_options("ro,noauto");
        q.push(PendingOp::Edit {
            line_no: 2,
            entry: e1,
        });
        q.push(PendingOp::Edit {
            line_no: 2,
            entry: e2,
        });
        assert_eq!(q.len(), 1, "连续同目标编辑合并为 1 条");
        let base = queue_base();
        let preview = q.preview(&base).expect("preview 成功");
        let boot = preview.entries().find(|e| e.line_no == 2).expect("第2行");
        assert_eq!(boot.options, "ro,noauto", "取最新值");
    }

    #[test]
    fn queue_different_targets_not_merged() {
        let mut q = OpQueue::new();
        q.push(PendingOp::Edit {
            line_no: 1,
            entry: entry_of("/", "UUID=aaa"),
        });
        q.push(PendingOp::Edit {
            line_no: 2,
            entry: entry_of("/boot", "UUID=bbb"),
        });
        assert_eq!(q.len(), 2, "不同目标不合并");
        q.push(PendingOp::SetDisabled {
            line_no: 1,
            disabled: true,
        });
        assert_eq!(q.len(), 3, "不同类型不合并（顺序有意义）");
    }

    #[test]
    fn queue_validation_failure_keeps_queue() {
        let mut q = OpQueue::new();
        // 挂载点重复（/ 已被第 1 行占用）→ preview 必失败
        q.push(PendingOp::Add(entry_of("/", "UUID=zzz")));
        let base = queue_base();
        let err = q.preview(&base).expect_err("重复挂载点应拒绝");
        assert!(err.contains("已被"), "错误信息含占用提示：{err}");
        assert_eq!(q.len(), 1, "校验失败队列保留");
        assert_eq!(base.entry_count(), 2, "base 不被改动");
    }

    #[test]
    fn queue_disable_then_add_migration() {
        // 「停用旧的 + 新增」迁移挂载点（add_entry 的停用条目不占位语义）
        let mut q = OpQueue::new();
        q.push(PendingOp::SetDisabled {
            line_no: 2,
            disabled: true,
        });
        let mut e = entry_of("/boot", "UUID=ddd");
        e.set_options("defaults,noauto");
        q.push(PendingOp::Add(e));
        let base = queue_base();
        let preview = q.preview(&base).expect("迁移组合应成功");
        assert_eq!(preview.entry_count(), 3);
        let old = preview.entries().find(|e| e.line_no == 2).expect("旧行");
        assert!(old.disabled, "旧行已停用");
        assert_eq!(preview.entries().last().expect("新行").mountpoint, "/boot");
    }

    #[test]
    fn queue_edit_unknown_line_fails_and_keeps_queue() {
        let mut q = OpQueue::new();
        q.push(PendingOp::Edit {
            line_no: 99,
            entry: entry_of("/x", "UUID=x"),
        });
        let base = queue_base();
        let err = q.preview(&base).expect_err("未知行号应失败");
        assert!(err.contains("未找到"), "错误信息：{err}");
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn queue_apply_then_clear_cycle() {
        // Apply 流程语义：preview 成功 →（真实写入，此处跳过）→ clear
        let mut q = OpQueue::new();
        q.push(PendingOp::Add(entry_of("/mnt/c", "UUID=ccc")));
        q.push(PendingOp::SetDisabled {
            line_no: 1,
            disabled: true,
        });
        let base = queue_base();
        let preview = q.preview(&base).expect("Apply 前预检成功");
        // …真实校验+写入（7.4→7.5 链路）成功后：
        q.clear();
        assert!(q.is_empty(), "Apply 后队列清空");
        // 清空后再 preview = base 本身（零残留）
        assert_eq!(q.preview(&base).expect("空 preview"), base);
        assert_eq!(preview.entry_count(), 3, "Apply 的文档含全部操作结果");
        assert!(
            preview
                .entries()
                .find(|e| e.line_no == 1)
                .expect("1行")
                .disabled
        );
    }

    // -----------------------------------------------------------------------
    // 7.7 设备树数据层（lsblk -b / df / human_bytes）
    // -----------------------------------------------------------------------

    #[test]
    fn lsblk_tree_parses_disks_and_parts() {
        let json = r#"{
           "blockdevices": [
              {"name":"zram0","path":"/dev/zram0","type":"disk","size":8589934592,
               "fstype":"swap","label":"zram0","uuid":"1682","mountpoints":["[SWAP]"]},
              {"name":"nvme2n1","path":"/dev/nvme2n1","type":"disk","size":512110190592,
               "fstype":null,"label":null,"uuid":null,"mountpoints":[],
               "children":[
                  {"name":"nvme2n1p1","path":"/dev/nvme2n1p1","type":"part","size":16759808,
                   "fstype":null,"label":null,"uuid":null,"mountpoints":[]},
                  {"name":"nvme2n1p2","path":"/dev/nvme2n1p2","type":"part","size":512092012544,
                   "fstype":"ntfs","label":"新加卷","uuid":"48E4","mountpoints":[]}
               ]},
              {"name":"loop0","path":"/dev/loop0","type":"loop","size":1048576,
               "fstype":"squashfs","label":null,"uuid":null,"mountpoints":[]}
           ]
        }"#;
        let disks = parse_lsblk_tree(json).expect("解析");
        assert_eq!(disks.len(), 2, "只收 type=disk（loop 不收）");
        // 无分区表整盘：盘节点自身降级为唯一分段
        assert_eq!(disks[0].name, "zram0");
        assert_eq!(disks[0].segments().len(), 1);
        assert_eq!(disks[0].segments()[0].size_bytes, 8589934592);
        assert_eq!(disks[0].segments()[0].mountpoints, vec!["[SWAP]"]);
        // 有分区表：按 children part 收
        assert_eq!(disks[1].segments().len(), 2);
        assert_eq!(disks[1].size_bytes, 512110190592);
        assert_eq!(disks[1].segments()[1].label.as_deref(), Some("新加卷"));
        assert_eq!(disks[1].segments()[1].size_bytes, 512092012544);
        assert_eq!(disks[1].segments()[0].fstype, None);
    }

    #[test]
    fn lsblk_tree_tolerates_string_size_and_bad_json() {
        // 合成 fixture：size 给字符串（容错分支）
        let json = r#"{"blockdevices":[{"name":"sda","path":"/dev/sda","type":"disk",
            "size":"1024","fstype":null,"label":null,"uuid":null,"mountpoints":[]}]}"#;
        let disks = parse_lsblk_tree(json).expect("容错解析");
        assert_eq!(disks[0].segments()[0].size_bytes, 1024);
        assert!(parse_lsblk_tree("not json").is_err());
        assert!(
            parse_lsblk_tree(r#"{"foo":1}"#).is_err(),
            "缺 blockdevices 报错"
        );
    }

    #[test]
    fn df_usage_parses_chinese_header_and_spaces() {
        let text = "挂载点                                           1B的块         已用
            /                                         1022505254912 484101079040
            /dev                                        16578117632            0
            /mnt/my data                                  104857600    52428800
";
        let rows = parse_df_usage(text);
        assert_eq!(rows.len(), 3, "中文表头被跳过，3 行数据");
        assert_eq!(rows[0], ("/".to_string(), 1022505254912, 484101079040));
        assert_eq!(rows[1].2, 0, "全零 used");
        assert_eq!(rows[2].0, "/mnt/my data", "挂载点含空格保真");
        assert_eq!(rows[2].1, 104857600);
    }

    #[test]
    fn human_bytes_formats() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(512110190592), "476.9 GB");
        assert_eq!(human_bytes(1099511627776), "1.0 TB");
    }

    #[test]
    fn queue_replace_and_remove_at() {
        let mut q = OpQueue::new();
        let mut e1 = entry_of("/boot", "UUID=bbb");
        e1.set_options("ro,noauto");
        q.push(PendingOp::Edit {
            line_no: 2,
            entry: e1,
        });
        q.push(PendingOp::SetDisabled {
            line_no: 1,
            disabled: true,
        });
        assert_eq!(q.len(), 2);

        // 原地替换：编辑待定条目的语义
        let mut e2 = entry_of("/boot", "UUID=ccc");
        e2.set_options("defaults");
        q.replace_at(
            0,
            PendingOp::Edit {
                line_no: 2,
                entry: e2,
            },
        )
        .expect("替换第 0 条");
        assert_eq!(q.len(), 2, "替换不增不减");
        assert!(
            q.replace_at(9, PendingOp::Add(entry_of("/x", "UUID=x")))
                .is_err()
        );

        let base = queue_base();
        let preview = q.preview(&base).expect("替换后 preview 成功");
        let boot = preview.entries().find(|e| e.line_no == 2).expect("第2行");
        assert_eq!(boot.source, "UUID=ccc", "preview 反映替换后的操作");

        // 单条撤销中间一条，剩余操作仍可用
        let removed = q.remove_at(0).expect("撤销第 0 条");
        assert_eq!(removed.describe(), "编辑第 2 行 → /boot");
        assert_eq!(q.len(), 1);
        assert!(q.remove_at(9).is_none(), "越界返回 None");
        let preview = q.preview(&base).expect("移除后 preview 成功");
        let boot = preview.entries().find(|e| e.line_no == 2).expect("第2行");
        assert_eq!(
            boot.options, "defaults,noauto",
            "Edit 已撤，SetDisabled 仍在但不动 options"
        );
    }

    // ---------------- D7.5 修订：物理删除（2026-10-03 用户批复） ----------------

    /// 删除条目行：该行消失、注释与其他条目原样保留、幸存条目行号不重排。
    #[test]
    fn remove_entry_deletes_only_target_line() {
        let mut doc = parse("# 头注释\nUUID=aaa / ext4 defaults 0 1\n# 中间注释\nUUID=bbb /boot ext4 defaults 0 2\nUUID=ccc /home ext4 defaults 0 2\n").unwrap();
        remove_entry(&mut doc, 4).expect("删第 4 行（/boot）");
        let text = serialize(&doc);
        assert!(!text.contains("/boot "), "目标行已物理删除：{text}");
        assert!(
            text.contains("# 头注释") && text.contains("# 中间注释"),
            "注释保留：{text}"
        );
        assert!(text.contains("/home"), "相邻条目保留：{text}");
        let home = doc
            .entries()
            .find(|e| e.source.contains("ccc"))
            .expect("home 条目");
        assert_eq!(home.line_no, 5, "幸存条目行号不重排（稳定标识）");
        let reparsed = parse(&text).expect("删除后仍可解析");
        assert_eq!(reparsed.entry_count(), 2);
    }

    /// 删不存在的行号 → Err（与 add/edit 的定位错误风格一致）。
    #[test]
    fn remove_entry_rejects_unknown_line() {
        let mut doc = queue_base();
        assert!(
            remove_entry(&mut doc, 99)
                .unwrap_err()
                .contains("未找到第 99 行")
        );
        assert!(remove_entry(&mut doc, 0).is_err());
    }

    /// 删末行后新增：新行号不与幸存条目碰撞（max(行数+1, 最大行号+1) 规则），
    /// 且 preview 与「真实 Apply 同序执行」结果一致。
    #[test]
    fn add_after_remove_no_line_collision() {
        let base = queue_base(); // 2 行 2 条目，行号 1、2
        let mut q = OpQueue::new();
        q.push(PendingOp::Remove {
            line_no: 2,
            mountpoint: "/boot".into(),
        });
        q.push(PendingOp::Add(entry_of("/data", "UUID=ddd")));
        let doc = q.preview(&base).expect("删后新增 preview 成功");
        let lines: Vec<usize> = doc.entries().map(|e| e.line_no).collect();
        let mut uniq = lines.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(lines.len(), uniq.len(), "行号不得重复：{lines:?}");
        assert_eq!(doc.entry_count(), 2, "删 1 增 1 = 2 条");
        // 独立复现真实 Apply：对 base 克隆按序手工执行
        let mut manual = base.clone();
        remove_entry(&mut manual, 2).unwrap();
        add_entry(&mut manual, entry_of("/data", "UUID=ddd")).unwrap();
        assert_eq!(serialize(&manual), serialize(&doc), "preview == 手工 Apply");
    }

    /// 队列含 Remove：preview 少一条、describe 文案、Remove 之后同行为孤儿 → Err。
    #[test]
    fn queue_remove_preview_and_orphan_edit() {
        let base = queue_base();
        let mut q = OpQueue::new();
        q.push(PendingOp::Remove {
            line_no: 1,
            mountpoint: "/".into(),
        });
        assert_eq!(q.len(), 1);
        assert_eq!(q.ops()[0].describe(), "物理删除第 1 行（/）");
        let doc = q.preview(&base).expect("Remove preview 成功");
        assert_eq!(doc.entry_count(), 1, "只剩 /boot");
        assert!(doc.entries().any(|e| e.mountpoint == "/boot"));

        // Remove 之后再编辑同一行（行已不存在）→ preview 必须报错而不是静默
        let mut q2 = q.clone();
        q2.push(PendingOp::Edit {
            line_no: 1,
            entry: entry_of("/new", "UUID=z"),
        });
        assert!(
            q2.preview(&base).unwrap_err().contains("未找到第 1 行"),
            "Remove 后的孤儿 Edit 应被 preview 拦下（队列保留、状态栏报错）"
        );

        // Remove 与同目标 SetDisabled 连续入队不合并（类型不同，_ => false）
        let mut q3 = OpQueue::new();
        q3.push(PendingOp::SetDisabled {
            line_no: 1,
            disabled: true,
        });
        q3.push(PendingOp::Remove {
            line_no: 1,
            mountpoint: "/".into(),
        });
        assert_eq!(q3.len(), 2, "停用+删除语义不同不合并");
        let doc = q3.preview(&base).expect("先停用后删除可执行");
        assert_eq!(doc.entry_count(), 1);
    }

    /// add_index_producing：含 Remove 的队列里定位产出某行号的 Add；
    /// base 既有行号返回 None（页面据此分流 Edit / 替换 Add）。
    #[test]
    fn add_index_producing_with_remove() {
        let base = queue_base();
        let mut q = OpQueue::new();
        q.push(PendingOp::Remove {
            line_no: 2,
            mountpoint: "/boot".into(),
        });
        q.push(PendingOp::Add(entry_of("/data", "UUID=ddd")));
        let doc = q.preview(&base).unwrap();
        let new_line = doc
            .entries()
            .find(|e| e.mountpoint == "/data")
            .expect("新增行在 preview 中")
            .line_no;
        assert_eq!(
            q.add_index_producing(&base, new_line),
            Some(1),
            "产出新行号的是队列第 1 条 Add"
        );
        assert_eq!(
            q.add_index_producing(&base, 1),
            None,
            "base 存活行号不是 Add 产物"
        );
        // 无 Add 的队列一律 None
        let mut q4 = OpQueue::new();
        q4.push(PendingOp::Remove {
            line_no: 1,
            mountpoint: "/".into(),
        });
        assert_eq!(q4.add_index_producing(&base, 99), None);
    }

    #[test]
    fn queue_describe_texts() {
        assert_eq!(
            PendingOp::Add(entry_of("/mnt/a", "UUID=x")).describe(),
            "新增 /mnt/a（ext4）"
        );
        assert_eq!(
            PendingOp::Edit {
                line_no: 3,
                entry: entry_of("/b", "UUID=y")
            }
            .describe(),
            "编辑第 3 行 → /b"
        );
        assert_eq!(
            PendingOp::SetDisabled {
                line_no: 2,
                disabled: false
            }
            .describe(),
            "恢复第 2 行"
        );
        assert_eq!(
            PendingOp::Remove {
                line_no: 5,
                mountpoint: "/mnt/x".into()
            }
            .describe(),
            "物理删除第 5 行（/mnt/x）"
        );
    }
}
