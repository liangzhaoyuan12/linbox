//! cpufreq sysfs 枚举与调频命令构造（纯逻辑，不依赖 GTK / libadwaita / glib）。
//!
//! 负责（GOAL Phase 8）：
//! - **8.1 读取与枚举**：遍历 `/sys/devices/system/cpu/cpu*/cpufreq`，逐核读调速器清单、
//!   当前值、驱动、频率上下限与 EPP，按 `related_cpus`/`affected_cpus` 分组成 policy，
//!   再取**并集**作为页面模式清单的唯一来源（D8.1：绝不写死清单）。
//! - **8.3 切换执行**：构造 `pkexec tee <sysfs>` 写操作（argv + stdin），名字过白名单
//!   校验防注入；写后回读由 [`read_governor`] / [`read_epp`] 负比对，**比不上就不许宣称成功**。
//!
//! 实测事实（2026-10-03，写进注释防踩坑）：
//! - `scaling_governor` 是 `root:root 0644`，普通用户直写 `权限不够 RC=1`，
//!   `pkexec tee` 写入成功 → **一切写操作必须走 pkexec**。
//! - 本机 intel_pstate(active) 20 核清单完全一致（`performance powersave`），
//!   acpi-cpufreq 机器会给另一套 → 清单只能动态读。
//!
//! 约束（`docs/项目结构规划书.md` §3.7）：本文件禁止 `use gtk` / `use adw` / `use glib`。

use std::fs;
use std::path::{Path, PathBuf};

use crate::model::cpufreq::{CpufreqSnapshot, EppState, PolicyGroup};

/// 真实 sysfs 根（测试通过 [`scan_root`] 传别的目录）。
pub const SYS_CPU_ROOT: &str = "/sys/devices/system/cpu";

/// 一次 sysfs 写操作：`argv` 交给子进程，`stdin` 喂给它的标准输入。
///
/// 用 `pkexec tee <path>` 而非 `sh -c 'echo x > path'`：前者**不经过 shell**，
/// 没有重定向/分号注入面；`stdin` 是我们自己构造的值，仍会先过白名单校验。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SysfsWrite {
    /// 例：`["pkexec", "tee", "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"]`
    pub argv: Vec<String>,
    /// 要写入的内容（不带换行，`tee` 原样落盘）。
    pub stdin: String,
}

// ---------------------------------------------------------------------------
// 枚举（GOAL 8.1）
// ---------------------------------------------------------------------------

/// 扫描真实 sysfs（页面入口用这个）。
pub fn scan() -> CpufreqSnapshot {
    scan_root(Path::new(SYS_CPU_ROOT))
}

/// 从任意根目录枚举 cpufreq（测试喂合成 fixture）。
///
/// - 无任何 `cpuN/cpufreq` 目录 → `supported = false`，其余字段全空（部分龙芯内核
///   没有 cpufreq sysfs，页面据此显示「本机内核未提供 cpufreq 接口」空态，**不 panic**）。
/// - 分组：优先 `related_cpus`（同 policy 的核共享一套设置）；缺该文件退回
///   `affected_cpus`；再缺就按 `(driver, 调速器清单, EPP 清单)` 签名分组。
/// - `governors` 是**各 policy 并集**（首次出现顺序），`missing_cpus_for()` 标出缺席核。
pub fn scan_root(root: &Path) -> CpufreqSnapshot {
    let mut snap = CpufreqSnapshot::default();
    let cpus = cpu_indices(root);
    snap.cpu_count = cpus.len();

    let mut cores: Vec<Core> = Vec::new();
    for cpu in cpus {
        let dir = root.join(format!("cpu{cpu}")).join("cpufreq");
        if !dir.is_dir() {
            continue; // 该核没有 cpufreq 接口（在线热插拔核/无驱动）
        }
        let core = Core {
            cpu,
            related: read_list(&dir.join("related_cpus")),
            affected: read_list(&dir.join("affected_cpus")),
            driver: read_str(&dir.join("scaling_driver")),
            governor: read_str(&dir.join("scaling_governor")),
            governors: read_words(&dir.join("scaling_available_governors")),
            epp: read_str(&dir.join("energy_performance_preference")),
            epp_available: read_words(&dir.join("energy_performance_available_preferences")),
            min_khz: read_u64(&dir.join("cpuinfo_min_freq")),
            max_khz: read_u64(&dir.join("cpuinfo_max_freq")),
            setspeed: read_str(&dir.join("scaling_setspeed")),
            cur_khz: read_u64(&dir.join("scaling_cur_freq")),
        };
        snap.cur_khz.push((core.cpu, core.cur_khz.unwrap_or(0)));
        cores.push(core);
    }
    // 没读到 cur_freq 的核不进 cur_khz（避免 0 值冒充真实频率）
    snap.cur_khz.retain(|(_, v)| *v > 0);

    if cores.is_empty() {
        return snap; // supported = false
    }
    snap.supported = true;

    // ---- 按 policy 分组 ----
    let mut groups: Vec<(String, PolicyGroup)> = Vec::new();
    for core in &cores {
        let key = group_key(core);
        let entry = groups.iter_mut().find(|(k, _)| *k == key);
        match entry {
            Some((_, g)) => {
                push_cpu(g, core.cpu);
                for c in cpu_list(core) {
                    push_cpu(g, c);
                }
                merge_policy(g, core);
            }
            None => {
                let mut g = PolicyGroup::default();
                push_cpu(&mut g, core.cpu);
                for c in cpu_list(core) {
                    push_cpu(&mut g, c);
                }
                merge_policy(&mut g, core);
                groups.push((key, g));
            }
        }
    }
    let mut policies: Vec<PolicyGroup> = groups.into_iter().map(|(_, g)| g).collect();
    for g in &mut policies {
        g.cpus.sort_unstable();
        g.cpus.dedup();
    }
    snap.policies = policies;

    // ---- 并集 / EPP 聚合 / 全机边界 ----
    let mut union: Vec<String> = Vec::new();
    let mut epp_union: Vec<String> = Vec::new();
    let mut epp_current: Option<String> = None;
    let mut min_khz: Option<u64> = None;
    let mut max_khz: Option<u64> = None;

    for p in &snap.policies {
        for g in &p.governors {
            if !union.iter().any(|x| x == g) {
                union.push(g.clone());
            }
        }
        for e in &p.epp_available {
            if !epp_union.iter().any(|x| x == e) {
                epp_union.push(e.clone());
            }
        }
        if epp_current.is_none() {
            epp_current = p.epp.clone();
        }
        min_khz = min_khz
            .map(|m| p.min_khz.map_or(m, |v| v.min(m)))
            .or(p.min_khz);
        max_khz = max_khz
            .map(|m| p.max_khz.map_or(m, |v| v.max(m)))
            .or(p.max_khz);
    }
    snap.governors = union;
    snap.freq_min_khz = min_khz;
    snap.freq_max_khz = max_khz;
    snap.driver = snap.policies.first().and_then(|p| p.driver.clone());
    snap.boost = read_boost(root);
    if !epp_union.is_empty() || epp_current.is_some() {
        snap.epp = Some(EppState {
            current: epp_current,
            available: epp_union,
        });
    }
    snap
}

/// 单核读到的字段（分组用的中间结构）。
struct Core {
    cpu: usize,
    related: Vec<usize>,
    affected: Vec<usize>,
    driver: Option<String>,
    governor: Option<String>,
    governors: Vec<String>,
    epp: Option<String>,
    epp_available: Vec<String>,
    min_khz: Option<u64>,
    max_khz: Option<u64>,
    setspeed: Option<String>,
    cur_khz: Option<u64>,
}

/// 分组键：`related_cpus` → `affected_cpus` → 签名兜底。
fn group_key(c: &Core) -> String {
    if !c.related.is_empty() {
        let mut r = c.related.clone();
        r.sort_unstable();
        return format!("related:{}", csv(&r));
    }
    if !c.affected.is_empty() {
        let mut a = c.affected.clone();
        a.sort_unstable();
        return format!("affected:{}", csv(&a));
    }
    format!(
        "sig:{}|{}|{}",
        c.driver.clone().unwrap_or_default(),
        c.governors.join(","),
        c.epp_available.join(",")
    )
}

fn csv(v: &[usize]) -> String {
    v.iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// 该核声明的同 policy 核列表（含自己）。
fn cpu_list(c: &Core) -> Vec<usize> {
    let mut v = c.related.clone();
    if v.is_empty() {
        v = c.affected.clone();
    }
    if v.is_empty() {
        v.push(c.cpu);
    }
    v
}

fn push_cpu(g: &mut PolicyGroup, cpu: usize) {
    if !g.cpus.contains(&cpu) {
        g.cpus.push(cpu);
    }
}

/// 把单核字段并进 policy（首个非空值生效，min/max 取更宽的硬件边界）。
fn merge_policy(g: &mut PolicyGroup, c: &Core) {
    if g.driver.is_none() {
        g.driver = c.driver.clone();
    }
    if g.governor.is_none() {
        g.governor = c.governor.clone();
    }
    if g.governors.is_empty() {
        g.governors = c.governors.clone();
    }
    if g.epp.is_none() {
        g.epp = c.epp.clone();
    }
    if g.epp_available.is_empty() {
        g.epp_available = c.epp_available.clone();
    }
    g.min_khz = match (g.min_khz, c.min_khz) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    g.max_khz = match (g.max_khz, c.max_khz) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    if g.setspeed.is_none() {
        g.setspeed = c.setspeed.clone();
    }
}

/// boost 状态：先看 `cpufreq/boost`，再看 `intel_pstate/no_turbo`（语义相反）。
fn read_boost(root: &Path) -> Option<bool> {
    if let Some(v) = read_str(&root.join("cpufreq").join("boost")) {
        return Some(v == "1");
    }
    if let Some(v) = read_str(&root.join("intel_pstate").join("no_turbo")) {
        return Some(v != "1");
    }
    None
}

/// 根目录下的 `cpuN` 序号（升序）。
fn cpu_indices(root: &Path) -> Vec<usize> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(root) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(num) = name.strip_prefix("cpu") else {
            continue;
        };
        if let Ok(n) = num.parse::<usize>() {
            out.push(n);
        }
    }
    out.sort_unstable();
    out
}

/// 读一个 sysfs 文件并 trim；空内容按「没有值」处理（脏文件不冒充有效值）。
fn read_str(p: &Path) -> Option<String> {
    fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 空白分隔的数字列表文件（`related_cpus` / `affected_cpus`）。
fn read_list(p: &Path) -> Vec<usize> {
    read_str(p)
        .map(|s| {
            s.split_whitespace()
                .filter_map(|x| x.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// 空白分隔的字符串列表（调速器/EPP 清单）。
fn read_words(p: &Path) -> Vec<String> {
    read_str(p)
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

fn read_u64(p: &Path) -> Option<u64> {
    read_str(p)?.parse().ok()
}

// ---------------------------------------------------------------------------
// 回读（写后验证，GOAL 8.3）
// ---------------------------------------------------------------------------

/// 回读某核当前调速器（写操作后必须调它比对，比不上就不许宣称成功）。
pub fn read_governor(root: &Path, cpu: usize) -> Option<String> {
    read_str(
        &root
            .join(format!("cpu{cpu}"))
            .join("cpufreq")
            .join("scaling_governor"),
    )
}

/// 回读某核当前 EPP。
pub fn read_epp(root: &Path, cpu: usize) -> Option<String> {
    read_str(
        &root
            .join(format!("cpu{cpu}"))
            .join("cpufreq")
            .join("energy_performance_preference"),
    )
}

// ---------------------------------------------------------------------------
// 切换（GOAL 8.3）
// ---------------------------------------------------------------------------

/// 模式名白名单：非空、≤64 字符、只允许 `A-Z a-z 0-9 _ -`。
///
/// 内核调速器名（含厂商自定义）都落在这个集合里；挡住空格/引号/分号/重定向，
/// 这样即使后续有人把 `SysfsWrite` 改成经 shell 执行也不会被注入。
pub fn is_valid_mode_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 构造「把某核的调速器设为 `name`」的写操作。
///
/// `name` 必须**同时**通过白名单与该 policy 的可用清单校验（D8.1：写也只能写
/// 本机真实存在的模式），否则 `Err`。
pub fn set_governor_cmd(
    root: &Path,
    policy: &PolicyGroup,
    name: &str,
) -> Result<SysfsWrite, String> {
    if !is_valid_mode_name(name) {
        return Err(format!("非法调速器名：{name:?}（只允许字母数字与 _ -）"));
    }
    if !policy.governors.iter().any(|g| g == name) {
        return Err(format!(
            "本机该频率分区不提供调速器 {name}，可选：{}",
            policy.governors.join(" / ")
        ));
    }
    let cpu = policy
        .cpus
        .first()
        .copied()
        .ok_or_else(|| "该频率分区没有 CPU".to_string())?;
    Ok(sysfs_write(root, cpu, "scaling_governor", name))
}

/// 构造「把某核的 EPP 设为 `name`」的写操作（无 EPP 的分区直接 `Err`，D8.2）。
pub fn set_epp_cmd(root: &Path, policy: &PolicyGroup, name: &str) -> Result<SysfsWrite, String> {
    if !is_valid_mode_name(name) {
        return Err(format!("非法 EPP 名：{name:?}（只允许字母数字与 _ -）"));
    }
    if policy.epp_available.is_empty() {
        return Err("该频率分区不支持能效偏好（EPP）".to_string());
    }
    if !policy.epp_available.iter().any(|e| e == name) {
        return Err(format!(
            "本机不提供能效偏好 {name}，可选：{}",
            policy.epp_available.join(" / ")
        ));
    }
    let cpu = policy
        .cpus
        .first()
        .copied()
        .ok_or_else(|| "该频率分区没有 CPU".to_string())?;
    Ok(sysfs_write(
        root,
        cpu,
        "energy_performance_preference",
        name,
    ))
}

/// 为**全部 policy** 构造同一调速器的切换命令：**一条命令、一次 polkit 授权**。
///
/// 为什么不逐 policy 调 `pkexec`（2026-10-03 实测改设计）：本机 intel_pstate 是
/// **每核一个 policy**（`related_cpus` 各自只有自己，20 核 = 20 个 policy），
/// 逐条 `pkexec tee` 就是 20 次授权对话框，而 polkit 默认**不缓存**授权
/// （journalctl 实测三次调用三次 `FAILED to authenticate`，各等 60s 超时）→ 不可用。
/// 因此合成 `pkexec sh -c 'set -e; printf %s <name> > <path0>; ...'`：一次授权写完全部。
///
/// 注入面：`name` 过 [`is_valid_mode_name`] 白名单（`[A-Za-z0-9_-]`），路径由我们按
/// CPU 序号拼出，脚本里没有别的用户输入；写法是 `printf %s` + 双引号路径，不做变量展开。
///
/// 只要有一个分区不提供该调速器就整体 `Err`——避免半台机器换了模式、另一半没换
/// 的中间态（混合架构上清单可能不同，交给用户显式处理）。
pub fn set_governor_all(
    root: &Path,
    snap: &CpufreqSnapshot,
    name: &str,
) -> Result<SysfsWrite, String> {
    if !is_valid_mode_name(name) {
        return Err(format!("非法调速器名：{name:?}（只允许字母数字与 _ -）"));
    }
    if snap.policies.is_empty() {
        return Err("本机没有可写的 cpufreq 分区".to_string());
    }
    if !snap.governors.iter().any(|g| g == name) {
        return Err(format!(
            "本机不提供调速器 {name}，可选：{}",
            snap.governors.join(" / ")
        ));
    }
    let mut paths = Vec::new();
    for p in &snap.policies {
        // 逐 policy 先做与单条命令相同的校验（清单内才许写）
        set_governor_cmd(root, p, name)?;
        paths.push(policy_path(root, p, "scaling_governor")?);
    }
    Ok(batch_write(&paths, name))
}

/// 为全部 policy 构造同一 EPP 的切换命令（一次授权）；任一分区不支持即整体 `Err`。
pub fn set_epp_all(root: &Path, snap: &CpufreqSnapshot, name: &str) -> Result<SysfsWrite, String> {
    if !is_valid_mode_name(name) {
        return Err(format!("非法 EPP 名：{name:?}（只允许字母数字与 _ -）"));
    }
    if !snap.has_epp() {
        return Err("本机能效偏好接口不可用".to_string());
    }
    let mut paths = Vec::new();
    for p in &snap.policies {
        set_epp_cmd(root, p, name)?;
        paths.push(policy_path(root, p, "energy_performance_preference")?);
    }
    Ok(batch_write(&paths, name))
}

/// policy 代表核的 sysfs 写入路径。
fn policy_path(root: &Path, policy: &PolicyGroup, file: &str) -> Result<PathBuf, String> {
    let cpu = policy
        .cpus
        .first()
        .copied()
        .ok_or_else(|| "该频率分区没有 CPU".to_string())?;
    Ok(root.join(format!("cpu{cpu}")).join("cpufreq").join(file))
}

/// 把多条写操作合成一条 `sh -c` 脚本（一次授权；`set -e` 任一步失败即中止）。
fn batch_write(paths: &[PathBuf], name: &str) -> SysfsWrite {
    let mut script = String::from("set -e");
    for p in paths {
        script.push_str(&format!(
            "; printf %s '{}' > \"{}\"",
            name,
            p.to_string_lossy()
        ));
    }
    SysfsWrite {
        argv: vec![
            "pkexec".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            script,
        ],
        stdin: String::new(),
    }
}

fn sysfs_write(root: &Path, cpu: usize, file: &str, value: &str) -> SysfsWrite {
    let path: PathBuf = root.join(format!("cpu{cpu}")).join("cpufreq").join(file);
    SysfsWrite {
        argv: vec![
            "pkexec".to_string(),
            "tee".to_string(),
            path.to_string_lossy().into_owned(),
        ],
        stdin: value.to_string(),
    }
}

// ---------------------------------------------------------------------------
// 持久化到开机（GOAL 8.6，D8.3/D8.6）
// ---------------------------------------------------------------------------

/// linbox 自建的 systemd 单元名。**只碰这一个文件**：绝不改写发行版自带的
/// `cpupower.service` / `tuned` / `auto-cpufreq`（D8.3、D8.6）。
pub const PERSIST_UNIT_NAME: &str = "linbox-cpufreq.service";
/// 单元绝对路径（`/etc/systemd/system` 0755，单元文件 0644，读取免提权、写入需 root）。
pub const PERSIST_UNIT_PATH: &str = "/etc/systemd/system/linbox-cpufreq.service";
/// 写单元用的 heredoc 分隔符；单元内容若含它则拒绝（防截断注入）。
const UNIT_EOF: &str = "LINBOX_UNIT_EOF";
/// D8.6 冲突闸门要探测的既有调频服务（任一 enabled → 阻止持久化）。
pub const CONFLICT_UNITS: [&str; 3] = ["tuned.service", "auto-cpufreq.service", "cpupower.service"];

/// 持久化状态（`systemctl is-enabled` 的解析结果）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PersistState {
    /// 已 enable（开机自动恢复）。
    Enabled,
    /// 存在但未 enable。
    Disabled,
    /// 单元不存在。
    Missing,
    /// 其它状态原文（masked / linked / 意外输出…），页面原样展示不猜。
    Other(String),
}

/// 解析 `systemctl is-enabled` 输出（纯函数，供页面与单测共用）。
pub fn parse_is_enabled(raw: &str) -> PersistState {
    match raw.trim() {
        "enabled" | "enabled-runtime" => PersistState::Enabled,
        "disabled" | "indirect" | "static" => PersistState::Disabled,
        "not-found" | "" => PersistState::Missing,
        other => PersistState::Other(other.to_string()),
    }
}

/// 该 `is-enabled` 原文是否代表「冲突服务已启用」（D8.6 闸门）。
///
/// `static` 不算冲突：它没有 `[Install]`、谈不上被谁 enable，由别的机制拉起时
/// 我们照样阻止没有意义；只认真正被 enable 的（含 runtime/linked 变体）。
pub fn is_conflict_enabled(raw: &str) -> bool {
    matches!(
        raw.trim(),
        "enabled" | "enabled-runtime" | "linked" | "linked-runtime"
    )
}

/// 从「单元名 → is-enabled 原文」列表里挑出已启用的冲突服务（D8.6）。
///
/// 页面负责后台跑 `systemctl is-enabled`，把输出原样传进来；本函数只做判定，
/// 不执行任何命令（禁 gtk 之外也避免在纯逻辑层起进程）。
pub fn conflict_units(results: &[(&str, &str)]) -> Vec<String> {
    results
        .iter()
        .filter(|(_, raw)| is_conflict_enabled(raw))
        .map(|(name, _)| name.to_string())
        .collect()
}

/// 取每个 policy 的代表核（持久化写入目标，一个 policy 一行）。
pub fn policy_cpus(snap: &CpufreqSnapshot) -> Vec<usize> {
    snap.policies
        .iter()
        .filter_map(|p| p.cpus.first().copied())
        .collect()
}

/// 生成持久化单元内容（GOAL 8.6 `render_unit`）。
///
/// 三个坑（都写进单测）：
/// 1. **systemd 规格符**：单元文件里 `%` 会被当 specifier 展开，`printf %s`
///    必须写成 `printf %%s`，否则开机时 ExecStart 拿到的命令是坏的；
/// 2. **引号嵌套**：`ExecStart=/bin/sh -c '…'` 由 systemd 解析，单引号串里
///    不能再出现单引号 → 模式名走白名单（`[A-Za-z0-9_-]`）不加引号、路径用双引号；
/// 3. **EPP 不一定存在**（D8.2：acpi-cpufreq 机器没有该文件）→ 用
///    `if [ -e … ]` 守卫，开机时缺文件不导致 oneshot 失败。
pub fn render_unit(
    root: &Path,
    governor: &str,
    epp: Option<&str>,
    cpus: &[usize],
) -> Result<String, String> {
    if !is_valid_mode_name(governor) {
        return Err(format!(
            "非法调速器名：{governor:?}（只允许字母数字与 _ -）"
        ));
    }
    if let Some(e) = epp
        && !is_valid_mode_name(e)
    {
        return Err(format!("非法 EPP 名：{e:?}（只允许字母数字与 _ -）"));
    }
    if cpus.is_empty() {
        return Err("没有可写入的频率分区".to_string());
    }

    let mut script = String::from("set -e");
    for cpu in cpus {
        let gpath = root
            .join(format!("cpu{cpu}"))
            .join("cpufreq")
            .join("scaling_governor");
        script.push_str(&format!("; printf %s {governor} > \"{}\"", gpath.display()));
        if let Some(e) = epp {
            let epath = root
                .join(format!("cpu{cpu}"))
                .join("cpufreq")
                .join("energy_performance_preference");
            script.push_str(&format!(
                "; if [ -e \"{}\" ]; then printf %s {e} > \"{}\"; fi",
                epath.display(),
                epath.display()
            ));
        }
    }
    // systemd 规格符转义：只转 ExecStart 里的脚本（%s → %%s）
    let exec = script.replace('%', "%%");
    let epp_note = if epp.is_some() {
        "与能效偏好（EPP）"
    } else {
        ""
    };
    Ok(format!(
        "[Unit]\nDescription=linbox 调频持久化：开机恢复 CPU 调速器{epp_note}\n\n[Service]\nType=oneshot\nRemainAfterExit=no\nExecStart=/bin/sh -c '{exec}'\n\n[Install]\nWantedBy=multi-user.target\n"
    ))
}

/// 生成「写入单元 → daemon-reload → enable」的**单条**命令（一次 polkit 授权）。
///
/// 单元内容经单引号 heredoc 落盘（`<<'…'` 不做任何展开，内容原样写入）；
/// 内容以 `\n` 结尾、且不含分隔符，否则拒绝。
pub fn persist_enable_cmd(unit_content: &str) -> Result<SysfsWrite, String> {
    if unit_content.contains(UNIT_EOF) {
        return Err("单元内容含保留分隔符，拒绝写入".to_string());
    }
    if !unit_content.contains("[Service]") || !unit_content.contains("ExecStart=") {
        return Err("单元内容不像 systemd unit，拒绝写入".to_string());
    }
    let body = if unit_content.ends_with('\n') {
        unit_content.to_string()
    } else {
        format!("{unit_content}\n")
    };
    let script = format!(
        "set -e; cat > \"{PERSIST_UNIT_PATH}\" <<'{UNIT_EOF}'\n{body}{UNIT_EOF}\n\
         systemctl daemon-reload; systemctl enable {PERSIST_UNIT_NAME}"
    );
    Ok(SysfsWrite {
        argv: vec![
            "pkexec".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            script,
        ],
        stdin: String::new(),
    })
}

/// 生成「disable → 删单元 → daemon-reload」的**单条**撤销命令（一次授权，幂等）。
///
/// 删的是 [`PERSIST_UNIT_PATH`] 这一个文件；`disable` 带 `|| true`，
/// 单元本来不存在时也返回成功（关开关可重复点）。
pub fn persist_disable_cmd() -> SysfsWrite {
    let script = format!(
        "systemctl disable {PERSIST_UNIT_NAME} >/dev/null 2>&1 || true; \
         rm -f \"{PERSIST_UNIT_PATH}\"; systemctl daemon-reload"
    );
    SysfsWrite {
        argv: vec![
            "pkexec".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            script,
        ],
        stdin: String::new(),
    }
}

/// 单元文件是否已存在（`/etc/systemd/system` 目录可读，读文件本身免提权）。
pub fn unit_exists() -> bool {
    Path::new(PERSIST_UNIT_PATH).exists()
}

/// 单元名合法性（`systemctl` 的 argv 参数，白名单即可，不走 shell）。
fn safe_unit_name(unit: &str) -> bool {
    !unit.is_empty()
        && unit.len() <= 128
        && unit
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@'))
}

/// 读 `systemctl is-enabled <unit>` 的状态串（只读、无需提权；**必须在后台线程调**）。
///
/// 实测：存在且启用 → `enabled`(RC 0)、存在未启用 → `disabled`(RC 1)、
/// 单元不存在 → `not-found`(RC 4)；stdout 始终是要的状态串。
pub fn is_enabled(unit: &str) -> Result<String, String> {
    if !safe_unit_name(unit) {
        return Err(format!("非法单元名：{unit:?}"));
    }
    let out = std::process::Command::new("systemctl")
        .args(["is-enabled", unit])
        .output()
        .map_err(|e| format!("启动 systemctl 失败：{e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stdout.is_empty() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "systemctl is-enabled 无输出".to_string()
        } else {
            err
        });
    }
    Ok(stdout)
}

/// 读本页持久化单元的状态（GOAL 8.6 `read_persist_state`）：
/// 单元文件存在性（免提权） + `is-enabled`。
pub fn read_persist_state() -> PersistState {
    if !unit_exists() {
        return PersistState::Missing;
    }
    match is_enabled(PERSIST_UNIT_NAME) {
        Ok(raw) => parse_is_enabled(&raw),
        Err(e) => PersistState::Other(format!("读取失败：{e}")),
    }
}

/// D8.6 冲突闸门：跑三次只读的 `systemctl is-enabled` 并判定（**后台线程调**）。
///
/// 返回 `(冲突服务名列表, 三个探测的原始输出)`——原始输出要原样进提示文案，
/// 用户得看见「谁在管这台机器的调频」。
pub fn detect_conflicts() -> (Vec<String>, Vec<(String, String)>) {
    let mut raws: Vec<(String, String)> = Vec::new();
    for name in CONFLICT_UNITS {
        let got = is_enabled(name).unwrap_or_else(|_| "读取失败".to_string());
        raws.push((name.to_string(), got));
    }
    let borrowed: Vec<(&str, &str)> = raws.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    (conflict_units(&borrowed), raws)
}

/// 执行一条构造好的写命令（`argv` + 可选 stdin）。
///
/// **只在后台线程里调**：`pkexec` 会弹授权框，在 UI 线程调会冻结窗口且授权
/// 框超时无人点时整页假死（2026-10-03 实测结论）。返回 stdout（UTF-8 宽容）。
pub fn exec_write(w: &SysfsWrite) -> Result<String, String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let Some(head) = w.argv.first() else {
        return Err("空命令".to_string());
    };
    let mut cmd = Command::new(head);
    cmd.args(&w.argv[1..]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if !w.stdin.is_empty() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().map_err(|e| format!("启动 {head} 失败：{e}"))?;
    if !w.stdin.is_empty()
        && let Some(mut si) = child.stdin.take()
    {
        si.write_all(w.stdin.as_bytes())
            .map_err(|e| format!("给 {head} 写 stdin 失败：{e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("等待 {head} 结束失败：{e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        let detail = if stderr.is_empty() { stdout } else { stderr };
        return Err(format!(
            "{head} 退出码 {:?}：{detail}",
            out.status.code().unwrap_or(-1)
        ));
    }
    Ok(stdout)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个隔离的 fixture 根目录（沿全仓约定：temp_dir + pid + 纳秒）。
    fn tmp_root(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "linbox-cpufreq-{tag}-{}-{nanos}.tmp",
            std::process::id()
        ));
        fs::create_dir_all(&p).expect("建 fixture 目录");
        p
    }

    fn put(root: &Path, rel: &str, val: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(p, val).expect("写 fixture 文件");
    }

    /// 造一个带完整 cpufreq 接口的核。
    fn add_cpu(root: &Path, cpu: usize, governor: &str, available: &str) {
        let d = format!("cpu{cpu}/cpufreq/");
        put(root, &format!("{d}scaling_governor"), governor);
        put(root, &format!("{d}scaling_available_governors"), available);
        put(root, &format!("{d}scaling_driver"), "intel_pstate");
        put(root, &format!("{d}cpuinfo_min_freq"), "800000");
        put(root, &format!("{d}cpuinfo_max_freq"), "3600000");
        put(root, &format!("{d}scaling_cur_freq"), "1200000");
    }

    #[test]
    fn scan_identical_20_cpus_yields_single_policy_and_union() {
        let root = tmp_root("identical");
        for cpu in 0..20 {
            add_cpu(&root, cpu, "powersave", "performance powersave");
        }
        put(&root, "cpufreq/boost", "1");

        let s = scan_root(&root);
        assert!(s.supported);
        assert_eq!(s.cpu_count, 20);
        assert_eq!(
            s.governors,
            vec!["performance", "powersave"],
            "并集顺序=首次出现"
        );
        assert_eq!(s.policies.len(), 1, "20 核清单一致 → 同一 policy");
        assert_eq!(s.policies[0].cpus.len(), 20);
        assert_eq!(s.driver.as_deref(), Some("intel_pstate"));
        assert_eq!(s.freq_min_khz, Some(800_000));
        assert_eq!(s.freq_max_khz, Some(3_600_000));
        assert_eq!(s.boost, Some(true));
        assert_eq!(s.cur_khz.len(), 20);
        assert!(s.missing_cpus_for("performance").is_empty());
        assert!(!s.has_epp(), "fixture 无 EPP 文件 → 整块隐藏");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_mixed_lists_unions_and_marks_missing_cpus() {
        let root = tmp_root("mixed");
        add_cpu(&root, 0, "performance", "performance powersave");
        add_cpu(&root, 1, "performance", "performance powersave");
        add_cpu(&root, 2, "performance", "performance powersave schedutil");

        let s = scan_root(&root);
        assert!(s.supported);
        assert_eq!(s.governors, vec!["performance", "powersave", "schedutil"]);
        assert_eq!(s.policies.len(), 2, "清单不同 → 两个 policy");
        assert_eq!(s.missing_cpus_for("schedutil"), vec![0, 1]);
        assert!(!s.supports_everywhere("schedutil"));
        assert!(s.supports_everywhere("powersave"));
        // 并集里没有的名字 → 全部核都算不支持
        assert_eq!(s.missing_cpus_for("nope"), vec![0, 1, 2]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_handles_missing_cpu0_and_online_gap() {
        let root = tmp_root("nocpu0");
        add_cpu(&root, 1, "powersave", "performance powersave");
        add_cpu(&root, 3, "powersave", "performance powersave");

        let s = scan_root(&root);
        assert!(s.supported, "cpu0 缺失不影响其余核");
        assert_eq!(s.cpu_count, 2);
        let cpus: Vec<usize> = s
            .policies
            .iter()
            .flat_map(|p| p.cpus.iter().copied())
            .collect();
        assert_eq!(cpus, vec![1, 3]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_missing_field_files_yields_none_not_panic() {
        let root = tmp_root("missingfield");
        // 只给清单，不给 governor 当前值 / 频率 / driver / cur_freq
        put(
            &root,
            "cpu0/cpufreq/scaling_available_governors",
            "performance powersave",
        );
        put(&root, "cpu0/cpufreq/cpuinfo_min_freq", "800000");
        // cpuinfo_max_freq 故意缺；cpu1 整个缺 scaling_governor 文件

        let s = scan_root(&root);
        assert!(s.supported);
        assert_eq!(s.governors, vec!["performance", "powersave"]);
        assert!(s.policies[0].governor.is_none(), "缺文件 → None 而非空串");
        assert!(s.driver.is_none());
        assert_eq!(s.freq_min_khz, Some(800_000));
        assert_eq!(s.freq_max_khz, None);
        assert!(s.cur_khz.is_empty());
        assert!(s.boost.is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_empty_files_treated_as_no_value() {
        let root = tmp_root("empty");
        put(&root, "cpu0/cpufreq/scaling_governor", "");
        put(&root, "cpu0/cpufreq/scaling_available_governors", "   \n");
        put(&root, "cpu0/cpufreq/scaling_driver", "  \t\n");

        let s = scan_root(&root);
        assert!(s.supported, "目录在但内容空 = 有接口");
        assert!(s.governors.is_empty(), "空白清单不产生伪模式");
        assert!(s.policies[0].governor.is_none());
        assert!(s.driver.is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_without_epp_has_none_and_no_error() {
        let root = tmp_root("noepp");
        add_cpu(&root, 0, "powersave", "performance powersave");

        let s = scan_root(&root);
        assert!(s.supported);
        assert!(!s.has_epp());
        assert!(s.epp.is_none(), "无 EPP 文件 → epp = None（D8.2 整块隐藏）");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_with_epp_aggregates_available_and_current() {
        let root = tmp_root("epp");
        add_cpu(&root, 0, "powersave", "performance powersave");
        put(
            &root,
            "cpu0/cpufreq/energy_performance_available_preferences",
            "default performance balance_performance balance_power power",
        );
        put(
            &root,
            "cpu0/cpufreq/energy_performance_preference",
            "balance_performance",
        );

        let s = scan_root(&root);
        assert!(s.has_epp());
        let epp = s.epp.expect("有 EPP");
        assert_eq!(epp.available.len(), 5);
        assert_eq!(epp.current.as_deref(), Some("balance_performance"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_without_cpufreq_dirs_is_unsupported_empty_state() {
        let root = tmp_root("nocpufreq");
        // cpu0 目录存在但没有 cpufreq 子目录；cpu1 完全没有
        let _ = fs::create_dir_all(root.join("cpu0"));
        let _ = fs::create_dir_all(root.join("cpu1"));

        let s = scan_root(&root);
        assert!(!s.supported, "无 cpufreq → 空态不 panic");
        assert!(s.policies.is_empty());
        assert!(s.governors.is_empty());
        assert_eq!(s.cpu_count, 2, "仍报告检测到的 CPU 数");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_nonexistent_root_is_unsupported() {
        let root = std::env::temp_dir().join("linbox-cpufreq-does-not-exist-xyz");
        let s = scan_root(&root);
        assert!(!s.supported);
        assert_eq!(s.cpu_count, 0);
    }

    #[test]
    fn scan_groups_by_related_cpus_file() {
        let root = tmp_root("related");
        add_cpu(&root, 0, "performance", "performance schedutil");
        put(&root, "cpu0/cpufreq/related_cpus", "0 2\n");
        // cpu2 是离线核：目录在、cpufreq 接口不在，但仍在 cpu0 的 related_cpus 里
        let _ = fs::create_dir_all(root.join("cpu2"));
        add_cpu(&root, 3, "powersave", "performance powersave");
        put(&root, "cpu3/cpufreq/related_cpus", "3\n");

        let s = scan_root(&root);
        assert_eq!(s.policies.len(), 2);
        let g0 = s
            .policies
            .iter()
            .find(|p| p.cpus.contains(&0))
            .expect("policy 含 cpu0");
        assert_eq!(
            g0.cpus,
            vec![0, 2],
            "related_cpus 里但没有 cpufreq 目录的核也归属该 policy"
        );
        let g1 = s
            .policies
            .iter()
            .find(|p| p.cpus.contains(&3))
            .expect("policy 含 cpu3");
        assert_eq!(g1.cpus, vec![3]);
        assert_eq!(s.governors, vec!["performance", "schedutil", "powersave"]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_boost_reads_intel_pstate_no_turbo_inverted() {
        let root = tmp_root("boost");
        add_cpu(&root, 0, "powersave", "performance powersave");
        put(&root, "intel_pstate/no_turbo", "1"); // no_turbo=1 → boost 关闭
        let s = scan_root(&root);
        assert_eq!(s.boost, Some(false), "no_turbo 语义与 boost 相反");
        let _ = fs::remove_dir_all(&root);

        let root2 = tmp_root("boost2");
        add_cpu(&root2, 0, "powersave", "performance powersave");
        put(&root2, "cpufreq/boost", "0");
        let s2 = scan_root(&root2);
        assert_eq!(s2.boost, Some(false), "cpufreq/boost=0 → 关闭");
        let _ = fs::remove_dir_all(&root2);
    }

    #[test]
    fn set_governor_cmd_builds_pkexec_tee_with_validated_stdin() {
        let root = Path::new("/sys/devices/system/cpu");
        let p = PolicyGroup {
            cpus: vec![0],
            governors: vec!["performance".into(), "powersave".into()],
            ..PolicyGroup::default()
        };

        let w = set_governor_cmd(root, &p, "performance").expect("合法模式");
        assert_eq!(
            w.argv,
            vec![
                "pkexec".to_string(),
                "tee".to_string(),
                "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor".to_string()
            ],
            "走 pkexec + tee，不经 shell"
        );
        assert_eq!(w.stdin, "performance");
    }

    #[test]
    fn set_governor_cmd_rejects_unknown_and_injection_names() {
        let root = Path::new("/sys/devices/system/cpu");
        let p = PolicyGroup {
            cpus: vec![0],
            governors: vec!["performance".into(), "powersave".into()],
            ..PolicyGroup::default()
        };
        // 名字合法但本机没有
        let err = set_governor_cmd(root, &p, "schedutil").unwrap_err();
        assert!(err.contains("不提供"), "错误要带可选清单语义：{err}");
        // 注入形态全部拒绝
        for bad in [
            "powersave; rm -rf /",
            "powersave > /etc/passwd",
            "a b",
            "",
            "$(whoami)",
            "../etc/passwd",
        ] {
            assert!(
                set_governor_cmd(root, &p, bad).is_err(),
                "必须拒绝注入名：{bad:?}"
            );
            assert!(!is_valid_mode_name(bad), "白名单也必须拦：{bad:?}");
        }
        assert!(is_valid_mode_name("balance_performance"));
        assert!(is_valid_mode_name("cpu-scalvar_2"));
    }

    #[test]
    fn set_epp_cmd_requires_available_list_and_membership() {
        let root = Path::new("/sys/devices/system/cpu");
        let no_epp = PolicyGroup {
            cpus: vec![0],
            governors: vec!["performance".into()],
            ..PolicyGroup::default()
        };
        let err = set_epp_cmd(root, &no_epp, "power").unwrap_err();
        assert!(err.contains("不支持能效偏好"), "{err}");

        let with_epp = PolicyGroup {
            cpus: vec![0],
            governors: vec!["performance".into()],
            epp_available: vec!["default".into(), "power".into()],
            ..PolicyGroup::default()
        };
        let w = set_epp_cmd(root, &with_epp, "power").expect("清单内");
        assert!(w.argv[2].ends_with("energy_performance_preference"));
        assert_eq!(w.stdin, "power");
        assert!(
            set_epp_cmd(root, &with_epp, "balance_power").is_err(),
            "清单外拒绝"
        );
        assert!(
            set_epp_cmd(root, &with_epp, "power; x").is_err(),
            "注入拒绝"
        );
    }

    #[test]
    fn set_governor_all_builds_one_write_per_policy_or_fails_whole() {
        let root = Path::new("/sys/devices/system/cpu");
        let mut snap = CpufreqSnapshot {
            supported: true,
            governors: vec!["performance".into(), "powersave".into()],
            policies: vec![
                PolicyGroup {
                    cpus: vec![0, 1],
                    governors: vec!["performance".into(), "powersave".into()],
                    ..PolicyGroup::default()
                },
                PolicyGroup {
                    cpus: vec![2],
                    governors: vec!["performance".into(), "powersave".into()],
                    ..PolicyGroup::default()
                },
            ],
            ..CpufreqSnapshot::default()
        };

        // 一次授权写全部 policy：合成单条 `pkexec sh -c` 脚本
        // （本机 20 个 policy 逐条发 = 20 次授权弹窗，实测不可用）
        let w = set_governor_all(root, &snap, "performance").expect("全分区都支持");
        assert_eq!(w.argv[0], "pkexec");
        assert_eq!(w.argv[1], "sh");
        assert_eq!(w.argv[2], "-c");
        let script = &w.argv[3];
        assert!(script.starts_with("set -e"), "{script}");
        assert!(
            script.contains("/cpu0/cpufreq/scaling_governor"),
            "{script}"
        );
        assert!(
            script.contains("/cpu2/cpufreq/scaling_governor"),
            "{script}"
        );
        assert!(
            script.contains("printf %s 'performance'"),
            "模式名带单引号、且已过白名单：{script}"
        );
        assert_eq!(w.stdin, "", "批量命令不经 stdin");

        // 注入名同样被拒（白名单在合成脚本之前生效）
        assert!(
            set_governor_all(root, &snap, "performance; rm -rf /").is_err(),
            "注入名必须拒绝"
        );

        // 只有部分 policy 支持（并集里有、第二个分区没有）→ 整体拒绝，
        // 避免「半台机器换模式」的中间态
        snap.policies[0].governors.push("schedutil".into());
        snap.governors.push("schedutil".into());
        let err = set_governor_all(root, &snap, "schedutil").unwrap_err();
        assert!(err.contains("不提供"), "{err}");

        // 空快照
        let empty = CpufreqSnapshot::default();
        assert!(set_governor_all(root, &empty, "performance").is_err());
    }

    #[test]
    fn set_epp_all_refuses_when_machine_has_no_epp() {
        let root = Path::new("/sys/devices/system/cpu");
        let snap = CpufreqSnapshot {
            supported: true,
            governors: vec!["performance".into()],
            policies: vec![PolicyGroup {
                cpus: vec![0],
                governors: vec!["performance".into()],
                ..PolicyGroup::default()
            }],
            ..CpufreqSnapshot::default()
        };
        assert!(set_epp_all(root, &snap, "power").is_err(), "无 EPP → 拒绝");
    }

    #[test]
    fn read_governor_roundtrip_on_fixture() {
        let root = tmp_root("readback");
        add_cpu(&root, 0, "powersave", "performance powersave");
        assert_eq!(read_governor(&root, 0).as_deref(), Some("powersave"));
        assert!(read_governor(&root, 7).is_none(), "不存在的核 → None");
        assert!(read_epp(&root, 0).is_none(), "无 EPP 文件 → None");
        let _ = fs::remove_dir_all(&root);
    }

    /// 真机 smoke（`--ignored` 手动跑，GOAL 8.1/8.3 的实测取证入口）：
    /// 打印本机真实清单与回读，与 `cat scaling_available_governors` 对照。
    #[test]
    #[ignore = "真机取证手动跑：cargo test utils::cpufreq::tests::real_machine_smoke -- --ignored --nocapture"]
    fn real_machine_smoke() {
        let root = Path::new(SYS_CPU_ROOT);
        let s = scan_root(root);
        println!("supported={} cpus={}", s.supported, s.cpu_count);
        println!("driver={:?}", s.driver);
        println!("governors(union)={:?}", s.governors);
        println!("policies={}", s.policies.len());
        println!("epp={:?}", s.epp);
        println!(
            "boost={:?} freq={}..{} kHz",
            s.boost,
            s.freq_min_khz.unwrap_or(0),
            s.freq_max_khz.unwrap_or(0)
        );
        if s.supported {
            let cpu0 = s
                .policies
                .first()
                .and_then(|p| p.cpus.first().copied())
                .unwrap_or(0);
            println!("回读 cpu{cpu0} governor={:?}", read_governor(root, cpu0));
            assert!(!s.governors.is_empty(), "有接口就必须有可选模式");
        }
    }

    /// 真机**切换往返**取证（GOAL 8.3）：切到 `performance` → 记录全机 → 切回原值。
    ///
    /// 设计要点（2026-10-03 实测）：把「切过去 + 取证 + 切回来」合成**一条** `pkexec sh -c`，
    /// 全程只弹**一次** polkit 授权对话框（本机 20 个 policy，逐 policy 逐次授权 = 20+ 次弹窗）。
    /// **无人点击授权框时 pkexec 卡在授权阶段、脚本不会执行 → 系统状态不变，超时也安全**。
    ///
    /// 手动跑法（桌面会话内，弹框点一次允许）：
    /// `cargo test --lib utils::cpufreq::tests::real_machine_switch_roundtrip -- --ignored --nocapture`
    #[test]
    #[ignore = "需交互式 polkit 授权（弹一次 KDE 授权框），手动跑见函数注释"]
    fn real_machine_switch_roundtrip() {
        use std::process::{Command, Stdio};

        let root = Path::new(SYS_CPU_ROOT);
        let snap = scan_root(root);
        assert!(snap.supported, "本机无 cpufreq 接口");
        let restore = snap
            .policies
            .iter()
            .find_map(|p| p.governor.clone())
            .unwrap_or_else(|| "powersave".to_string());
        assert!(
            snap.governors.iter().any(|g| g == "performance"),
            "本机没有 performance"
        );
        assert_ne!(restore, "performance", "原值已是 performance，无往返可做");

        let w_on = set_governor_all(root, &snap, "performance").expect("构造：切到 performance");
        let w_off = set_governor_all(root, &snap, &restore).expect("构造：切回原值");
        assert_eq!(w_on.argv[0], "pkexec");

        let report =
            std::env::temp_dir().join(format!("linbox-cpufreq-rt-{}.txt", std::process::id()));
        let glob = format!("{SYS_CPU_ROOT}/cpu*/cpufreq/scaling_governor");
        let script = format!(
            "{on}; echo '== after performance ==' > \"{rep}\"; \
             grep -h . {glob} | sort | uniq -c >> \"{rep}\"; \
             {off}; echo '== after restore({restore}) ==' >> \"{rep}\"; \
             grep -h . {glob} | sort | uniq -c >> \"{rep}\"",
            on = w_on.argv[3],
            off = w_off.argv[3],
            rep = report.display(),
            glob = glob,
            restore = restore,
        );

        let out = Command::new("pkexec")
            .args(["sh", "-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn pkexec");
        println!(
            "pkexec status={} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        assert!(
            out.status.success(),
            "授权或写入失败：{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let content = fs::read_to_string(&report).expect("读取证报告");
        println!("---- 取证报告 {report:?} ----\n{content}");

        // 自家回读逐 policy 核对（GOAL 8.3：回读比对才算成功）
        let now: Vec<String> = snap
            .policies
            .iter()
            .filter_map(|p| p.cpus.first())
            .map(|c| read_governor(root, *c).unwrap_or_default())
            .collect();
        println!("回读（应全为 {restore}）: {now:?}");
        assert_eq!(now.len(), snap.policies.len());
        assert!(
            now.iter().all(|g| g == &restore),
            "切回原值后全机回读不一致：{now:?}"
        );
        assert!(
            content.contains("== after performance =="),
            "取证报告缺切换后记录"
        );
        let _ = fs::remove_file(&report);
    }

    // -------------------------------------------------------------------------
    // 持久化（GOAL 8.6）
    // -------------------------------------------------------------------------

    #[test]
    fn render_unit_golden_covers_policies_governor_epp_and_specifier_escape() {
        let root = Path::new("/sys/devices/system/cpu");
        let unit = render_unit(root, "powersave", Some("balance_performance"), &[0, 2, 19])
            .expect("合法单元");
        for frag in [
            "[Unit]\n",
            "Description=",
            "[Service]\n",
            "Type=oneshot\n",
            "RemainAfterExit=no\n",
            "ExecStart=/bin/sh -c '",
            "[Install]\n",
            "WantedBy=multi-user.target\n",
        ] {
            assert!(unit.contains(frag), "缺 {frag:?}：\n{unit}");
        }
        // 每个 policy 一行 governor 写入 + EPP 带 -e 守卫（D8.2 缺文件不炸）
        for cpu in [0, 2, 19] {
            let g = format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_governor");
            assert!(
                unit.contains(&format!("printf %%s powersave > \"{g}\"")),
                "缺 cpu{cpu} 的 governor 行：\n{unit}"
            );
            let e =
                format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/energy_performance_preference");
            assert!(
                unit.contains(&format!("if [ -e \"{e}\" ]")),
                "缺 cpu{cpu} 的 EPP 守卫：\n{unit}"
            );
        }
        // systemd 规格符：单元里不许有未转义的 %
        assert!(unit.contains("printf %%s"), "%%s 转义缺失：\n{unit}");
        let unescaped = unit.replace("%%", "");
        assert!(!unescaped.contains('%'), "存在未转义 %：\n{unit}");
        // ExecStart 单引号串里不能再有单引号（systemd 引号解析）
        let exec_line = unit
            .lines()
            .find(|l| l.starts_with("ExecStart="))
            .expect("有 ExecStart 行");
        let inner = exec_line
            .trim_start_matches("ExecStart=/bin/sh -c '")
            .trim_end_matches('\'');
        assert!(!inner.contains('\''), "ExecStart 内出现单引号：{exec_line}");
        assert_eq!(
            inner.matches("printf %%s powersave").count(),
            3,
            "三个 policy 各一行"
        );
    }

    #[test]
    fn render_unit_without_epp_omits_epp_lines() {
        let root = Path::new("/sys/devices/system/cpu");
        let unit = render_unit(root, "performance", None, &[0, 1]).expect("合法单元");
        assert!(
            !unit.contains("energy_performance_preference"),
            "无 EPP 时不得出现 EPP 行：\n{unit}"
        );
        assert!(unit.contains("printf %%s performance"));
        assert_eq!(
            unit.matches("printf %%s performance").count(),
            2,
            "两个 policy 各一行"
        );
    }

    #[test]
    fn render_unit_rejects_injection_names_and_empty_policies() {
        let root = Path::new("/sys/devices/system/cpu");
        for bad in ["powersave; rm -rf /", "a b", "", "x'y", "$(reboot)"] {
            assert!(
                render_unit(root, bad, None, &[0]).is_err(),
                "必须拒绝非法调速器名：{bad:?}"
            );
        }
        assert!(
            render_unit(root, "powersave", Some("power; x"), &[0]).is_err(),
            "非法 EPP 名必须拒绝"
        );
        assert!(
            render_unit(root, "powersave", Some("power"), &[]).is_err(),
            "空 policy 列表必须拒绝"
        );
    }

    #[test]
    fn persist_constants_only_ever_touch_own_unit() {
        assert_eq!(PERSIST_UNIT_NAME, "linbox-cpufreq.service");
        assert_eq!(
            PERSIST_UNIT_PATH,
            "/etc/systemd/system/linbox-cpufreq.service"
        );
        assert!(PERSIST_UNIT_PATH.ends_with(PERSIST_UNIT_NAME));
        for other in CONFLICT_UNITS {
            assert_ne!(other, PERSIST_UNIT_NAME, "冲突清单不得含自己的单元");
        }
        assert_eq!(
            CONFLICT_UNITS,
            ["tuned.service", "auto-cpufreq.service", "cpupower.service"]
        );
        // 撤销命令只动自己的文件（D8.3：绝不碰发行版自带服务）
        let off = persist_disable_cmd();
        let s = &off.argv[3];
        assert!(s.contains(&format!("rm -f \"{PERSIST_UNIT_PATH}\"")), "{s}");
        assert!(
            s.contains(&format!("systemctl disable {PERSIST_UNIT_NAME}")),
            "{s}"
        );
        assert!(s.contains("systemctl daemon-reload"), "{s}");
        for other in CONFLICT_UNITS {
            assert!(!s.contains(other), "撤销命令越界提到 {other}：{s}");
        }
    }

    #[test]
    fn persist_enable_cmd_is_single_auth_batch() {
        let root = Path::new("/sys/devices/system/cpu");
        let unit =
            render_unit(root, "powersave", Some("balance_performance"), &[0, 1]).expect("单元");
        let w = persist_enable_cmd(&unit).expect("可构造");
        assert_eq!(w.argv[0], "pkexec");
        assert_eq!(w.argv[1], "sh");
        assert_eq!(w.argv[2], "-c", "一次授权的批量命令");
        let s = &w.argv[3];
        assert!(
            s.contains(&format!("cat > \"{PERSIST_UNIT_PATH}\" <<'{UNIT_EOF}'")),
            "{s}"
        );
        assert!(
            s.contains("WantedBy=multi-user.target"),
            "单元内容进了 heredoc：{s}"
        );
        assert!(s.contains("systemctl daemon-reload"), "{s}");
        assert!(
            s.contains(&format!("systemctl enable {PERSIST_UNIT_NAME}")),
            "{s}"
        );
        assert_eq!(w.stdin, "", "经 heredoc 落盘，不走 stdin");

        // 内容校验：不像单元 / 含保留分隔符 → 拒绝
        assert!(
            persist_enable_cmd("随便一段文字").is_err(),
            "非单元内容拒绝"
        );
        assert!(
            persist_enable_cmd(&format!("[Service]\nExecStart=/bin/true\n{UNIT_EOF}\n")).is_err(),
            "含 heredoc 分隔符拒绝"
        );
        // 缺尾换行自动补齐（否则分隔符会粘在最后一行上）
        let no_nl = unit.trim_end_matches('\n').to_string();
        let w2 = persist_enable_cmd(&no_nl).expect("补换行");
        assert!(
            w2.argv[3].contains(&format!("{UNIT_EOF}\nsystemctl daemon-reload")),
            "分隔符必须独占一行：{}",
            w2.argv[3]
        );
    }

    #[test]
    fn conflict_gate_matrix_matches_d86() {
        // 有冲突：只认真正被 enable 的
        let with_conflict = [
            ("tuned.service", "enabled"),
            ("auto-cpufreq.service", "disabled"),
            ("cpupower.service", "not-found"),
        ];
        assert_eq!(conflict_units(&with_conflict), vec!["tuned.service"]);
        // 全无冲突（D8.6 放行分支）
        let none = [
            ("tuned.service", "disabled"),
            ("auto-cpufreq.service", "static"),
            ("cpupower.service", "not-found"),
        ];
        assert!(conflict_units(&none).is_empty(), "三者皆未启用 → 放行");
        // 判定边界
        assert!(is_conflict_enabled("enabled"));
        assert!(is_conflict_enabled("enabled-runtime"));
        assert!(is_conflict_enabled("linked"));
        assert!(
            !is_conflict_enabled("static"),
            "static 无 [Install]，不算冲突"
        );
        assert!(!is_conflict_enabled("disabled"));
        assert!(!is_conflict_enabled(""));
        assert!(!is_conflict_enabled("not-found"));
    }

    #[test]
    fn parse_is_enabled_truth_table() {
        assert_eq!(parse_is_enabled("enabled\n"), PersistState::Enabled);
        assert_eq!(parse_is_enabled("enabled-runtime"), PersistState::Enabled);
        assert_eq!(parse_is_enabled("disabled"), PersistState::Disabled);
        assert_eq!(parse_is_enabled("static"), PersistState::Disabled);
        assert_eq!(parse_is_enabled("indirect"), PersistState::Disabled);
        assert_eq!(parse_is_enabled("not-found"), PersistState::Missing);
        assert_eq!(parse_is_enabled("  "), PersistState::Missing);
        assert_eq!(
            parse_is_enabled("masked"),
            PersistState::Other("masked".to_string())
        );
    }

    #[test]
    fn render_unit_dryrun_write_readback_identical() {
        let root = Path::new("/sys/devices/system/cpu");
        let unit = render_unit(root, "powersave", Some("balance_performance"), &[0, 1, 2])
            .expect("合法单元");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "linbox-cpufreq-render-{}-{nanos}.unit",
            std::process::id()
        ));
        fs::write(&p, &unit).expect("落盘");
        let back = fs::read_to_string(&p).expect("cat 回读");
        assert_eq!(back, unit, "回读必须逐字节一致");
        assert_eq!(
            fs::metadata(&p).expect("stat").len() as usize,
            unit.len(),
            "文件长度与内容一致"
        );
        let _ = fs::remove_file(&p);
    }

    /// 真机 dry-run 取证（`--ignored` 手动跑，GOAL 8.6 验证②）：
    /// 对 `/tmp/linbox-cpufreq.dryrun` 走「落盘 → 回读 → 逐字节一致」三证，
    /// 并打印将要执行的 enable 命令——**只构造不执行**（真写 /etc 留给 8.4 手测）。
    #[test]
    #[ignore = "真机取证手动跑：cargo test utils::cpufreq::tests::dryrun_real_evidence -- --ignored --nocapture"]
    fn dryrun_real_evidence() {
        let snap = scan();
        assert!(snap.supported, "本机无 cpufreq 接口");
        let cpus = policy_cpus(&snap);
        let gov = snap
            .policies
            .iter()
            .find_map(|p| p.governor.clone())
            .unwrap_or_else(|| "powersave".to_string());
        let epp = snap.epp.as_ref().and_then(|e| e.current.clone());
        let unit =
            render_unit(Path::new(SYS_CPU_ROOT), &gov, epp.as_deref(), &cpus).expect("渲染单元");
        let path = Path::new("/tmp/linbox-cpufreq.dryrun");
        fs::write(path, &unit).expect("落盘 /tmp");
        let back = fs::read_to_string(path).expect("回读");
        println!("---- /tmp/linbox-cpufreq.dryrun ----\n{back}");
        assert_eq!(back, unit, "证2：回读逐字节一致");
        assert_eq!(
            fs::metadata(path).expect("stat").len() as usize,
            unit.len(),
            "证1：文件存在且长度一致"
        );
        assert_eq!(cpus.len(), snap.policies.len(), "policy 数 = 写入行数");
        let cmd = persist_enable_cmd(&unit).expect("证3：enable 命令可构造");
        println!("---- enable 命令（构造，不执行）----\n{}", cmd.argv[3]);
        assert!(cmd.argv[3].contains(PERSIST_UNIT_PATH));
        // 保留 /tmp/linbox-cpufreq.dryrun 作为取证工件（系统临时目录，24h 内会被清理）
    }

    #[test]
    fn exec_write_runs_plain_command_with_stdin() {
        let w = SysfsWrite {
            argv: vec!["cat".to_string()],
            stdin: "powersave\n".to_string(),
        };
        assert_eq!(exec_write(&w).expect("cat 成功"), "powersave");
    }

    #[test]
    fn exec_write_reports_failure_and_empty_argv() {
        let bad = SysfsWrite {
            argv: vec!["false".to_string()],
            stdin: String::new(),
        };
        let e = exec_write(&bad).expect_err("false 必须报错");
        assert!(e.contains("退出码"), "{e}");
        let empty = SysfsWrite {
            argv: vec![],
            stdin: String::new(),
        };
        assert!(exec_write(&empty).is_err(), "空命令拒绝");
    }

    #[test]
    fn is_enabled_rejects_shell_metachar_unit_names() {
        for bad in ["a b", "x;rm -rf /", "$(id)", ""] {
            assert!(is_enabled(bad).is_err(), "必须拒绝单元名 {bad:?}");
        }
    }

    /// 真机持久化状态 + D8.6 冲突闸门取证（`--ignored` 手动跑，只读、不提权）。
    #[test]
    #[ignore = "真机取证：cargo test utils::cpufreq::tests::persist_state_evidence -- --ignored --nocapture"]
    fn persist_state_evidence() {
        let st = read_persist_state();
        println!("read_persist_state = {st:?}");
        assert_eq!(st, PersistState::Missing, "本机尚未持久化 → Missing");
        let (conflicts, raws) = detect_conflicts();
        for (name, raw) in &raws {
            println!("is-enabled {name} = {raw}");
        }
        println!("conflict_units = {conflicts:?}");
        // 本机实测 tuned.service 为 enabled；若哪天用户关掉 tuned，冲突清单变空也合法——
        // 所以只断言「三个探测都拿到了像样的状态串」，不硬编码冲突结论。
        assert_eq!(raws.len(), 3);
        for (_, raw) in &raws {
            assert!(!raw.is_empty(), "探测不能是空串");
        }
    }
}
