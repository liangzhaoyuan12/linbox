//! `/proc` 采集与解析（纯逻辑，不依赖 GTK）。
//!
//! 解析函数全部是纯函数，用真实 `/proc` 内容做单测；采集函数负责读文件并
//! 用上一次采样算差分（CPU%、网络/磁盘速率、进程 CPU%）。

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use crate::model::monitor::{CpuStat, DiskStat, MemStat, NetIface, ProcColumn, Process, TreeMode};

/// 逻辑 CPU 数。进程 CPU% 按「全部逻辑核心 = 100%」归一化时要用它
/// （与系统监视器一致；top/htop 是「1 个核心 = 100%」，同一进程会差 N 倍）。
pub fn n_cpus() -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get() as u64)
            .unwrap_or(1)
    })
}

/// 每秒节拍数（CLK_TCK）。用 sysconf 取，失败退回 Linux 常见的 100。
pub fn clock_ticks() -> u64 {
    unsafe extern "C" {
        fn sysconf(name: i32) -> i64;
    }
    const SC_CLK_TCK: i32 = 2;
    let v = unsafe { sysconf(SC_CLK_TCK) };
    if v > 0 { v as u64 } else { 100 }
}

// ---------------------------------------------------------------------------
// /proc/stat
// ---------------------------------------------------------------------------

/// 每个 CPU 的一行 jiffies。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuJiffies {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl CpuJiffies {
    pub fn total(&self) -> u64 {
        self.user
            + self.nice
            + self.system
            + self.idle
            + self.iowait
            + self.irq
            + self.softirq
            + self.steal
    }

    /// 空闲（含 iowait）。
    pub fn idle_total(&self) -> u64 {
        self.idle + self.iowait
    }

    /// 两次采样之间的占用率 0..100。
    pub fn usage_since(&self, prev: &CpuJiffies) -> f32 {
        let dt = self.total().saturating_sub(prev.total());
        if dt == 0 {
            return 0.0;
        }
        let di = self.idle_total().saturating_sub(prev.idle_total());
        (100.0 * (dt.saturating_sub(di)) as f32 / dt as f32).clamp(0.0, 100.0)
    }
}

/// `/proc/stat` 里除 cpu 行以外的汇总。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatTotals {
    pub ctxt: u64,
    pub intr: u64,
    pub btime: u64,
    pub processes: u64,
    pub procs_running: u64,
    pub procs_blocked: u64,
}

/// 解析 `/proc/stat`：返回 `(整体, 每核, 汇总)`。
pub fn parse_stat(text: &str) -> (CpuJiffies, Vec<CpuJiffies>, StatTotals) {
    let mut total = CpuJiffies::default();
    let mut cores: Vec<CpuJiffies> = Vec::new();
    let mut totals = StatTotals::default();

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("cpu ") {
            total = parse_jiffies(rest);
        } else if let Some(rest) = line.strip_prefix("cpu") {
            // cpu0 cpu1 …（按编号顺序追加）
            let mut it = rest.split_whitespace();
            let idx: usize = it
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(cores.len());
            let j = parse_jiffies(&it.collect::<Vec<_>>().join(" "));
            if idx == cores.len() {
                cores.push(j);
            } else if idx < cores.len() {
                cores[idx] = j;
            }
        } else if let Some(v) = line.strip_prefix("ctxt ") {
            totals.ctxt = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("intr ") {
            totals.intr = v
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("btime ") {
            totals.btime = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("processes ") {
            totals.processes = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("procs_running ") {
            totals.procs_running = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("procs_blocked ") {
            totals.procs_blocked = v.trim().parse().unwrap_or(0);
        }
    }
    (total, cores, totals)
}

fn parse_jiffies(rest: &str) -> CpuJiffies {
    let v: Vec<u64> = rest
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    let g = |i: usize| v.get(i).copied().unwrap_or(0);
    CpuJiffies {
        user: g(0),
        nice: g(1),
        system: g(2),
        idle: g(3),
        iowait: g(4),
        irq: g(5),
        softirq: g(6),
        steal: g(7),
    }
}

/// 用两次 `/proc/stat` 采样填出完整的 [`CpuStat`] 展示值。
pub fn cpu_stat_delta(
    cur: &CpuJiffies,
    prev: &CpuJiffies,
    cur_cores: &[CpuJiffies],
    prev_cores: &[CpuJiffies],
) -> CpuStat {
    let per_core = cur_cores
        .iter()
        .enumerate()
        .map(|(i, c)| prev_cores.get(i).map(|p| c.usage_since(p)).unwrap_or(0.0))
        .collect();
    let dt = cur.total().saturating_sub(prev.total()).max(1) as f32;
    let pct = |a: u64, b: u64| 100.0 * a.saturating_sub(b) as f32 / dt;
    CpuStat {
        total: cur.usage_since(prev),
        per_core,
        user: pct(cur.user, prev.user),
        nice: pct(cur.nice + cur.user, prev.nice + prev.user) - pct(cur.user, prev.user),
        sys: pct(cur.system, prev.system),
        iowait: pct(cur.iowait, prev.iowait),
        steal: pct(cur.steal, prev.steal),
        idle: pct(cur.idle, prev.idle),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// /proc/meminfo
// ---------------------------------------------------------------------------

/// 解析 `/proc/meminfo`（值单位 kB，输出换算成字节）。
pub fn parse_meminfo(text: &str) -> MemStat {
    let mut m = MemStat::default();
    let get = |key: &str| -> u64 {
        text.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map(|kb| kb * 1024)
            .unwrap_or(0)
    };
    m.total = get("MemTotal:");
    m.free = get("MemFree:");
    m.available = get("MemAvailable:");
    m.buffers = get("Buffers:");
    m.cached = get("Cached:");
    m.shared = get("Shmem:");
    m.dirty = get("Dirty:");
    m.slab = get("Slab:");
    m.swap_total = get("SwapTotal:");
    m.swap_used = get("SwapTotal:").saturating_sub(get("SwapFree:"));
    // 已用 = 总量 - 可回收的空闲/缓存（与 free(1) 的 used 一致）
    m.used = m
        .total
        .saturating_sub(m.free + m.buffers + m.cached)
        .min(m.total);
    m
}

/// 解析 `/proc/loadavg`：返回 (1/5/15 分钟负载, 运行中进程, 总进程数)。
pub fn parse_loadavg(text: &str) -> ([f32; 3], u64, u64) {
    let f: Vec<&str> = text.split_whitespace().collect();
    let g = |i: usize| f.get(i).and_then(|s| s.parse::<f32>().ok()).unwrap_or(0.0);
    let (running, procs) = f
        .get(3)
        .and_then(|s| s.split_once('/'))
        .map(|(a, b)| (a.parse::<u64>().unwrap_or(0), b.parse::<u64>().unwrap_or(0)))
        .unwrap_or((0, 0));
    ([g(0), g(1), g(2)], running, procs)
}

/// 解析 `/proc/uptime` 的第一段（秒）。
pub fn parse_uptime(text: &str) -> u64 {
    text.split_whitespace()
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0) as u64
}

// ---------------------------------------------------------------------------
// /proc/net/dev
// ---------------------------------------------------------------------------

/// 网卡一行原始计数。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetRaw {
    pub name: String,
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errs: u64,
    pub rx_drop: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errs: u64,
    pub tx_drop: u64,
}

/// 解析 `/proc/net/dev`。
pub fn parse_net_dev(text: &str) -> Vec<NetRaw> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || name.starts_with("Inter-") || name.starts_with("face") {
            continue;
        }
        let v: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        let g = |i: usize| v.get(i).copied().unwrap_or(0);
        out.push(NetRaw {
            name: name.to_string(),
            rx_bytes: g(0),
            rx_packets: g(1),
            rx_errs: g(2),
            rx_drop: g(3),
            tx_bytes: g(8),
            tx_packets: g(9),
            tx_errs: g(10),
            tx_drop: g(11),
        });
    }
    out
}

/// 用两次采样算速率并填出展示用的网卡列表。
pub fn net_delta(cur: &[NetRaw], prev: &[NetRaw], dt: f64) -> Vec<NetIface> {
    let dt = if dt <= 0.0 { 1.0 } else { dt };
    let mut out: Vec<NetIface> = cur
        .iter()
        .map(|c| {
            let p = prev.iter().find(|p| p.name == c.name);
            let rate =
                |a: u64, b: Option<u64>| b.map(|b| a.saturating_sub(b) as f64 / dt).unwrap_or(0.0);
            NetIface {
                name: c.name.clone(),
                rx_bytes: c.rx_bytes,
                tx_bytes: c.tx_bytes,
                rx_rate: rate(c.rx_bytes, p.map(|p| p.rx_bytes)),
                tx_rate: rate(c.tx_bytes, p.map(|p| p.tx_bytes)),
                rx_pps: rate(c.rx_packets, p.map(|p| p.rx_packets)),
                tx_pps: rate(c.tx_packets, p.map(|p| p.tx_packets)),
                rx_packets: c.rx_packets,
                tx_packets: c.tx_packets,
                rx_errs: c.rx_errs,
                tx_errs: c.tx_errs,
                rx_drop: c.rx_drop,
                tx_drop: c.tx_drop,
                is_loopback: c.name == "lo",
                ..Default::default()
            }
        })
        .collect();
    // 活跃在前，回环最后
    out.sort_by(|a, b| {
        let act = |n: &NetIface| (n.rx_rate + n.tx_rate) as u64;
        // 先按「是否回环」升序（false 在前 = 真实网卡在前），
        // 再按活跃度降序（comparator 返回 Less 的排前面）
        a.is_loopback
            .cmp(&b.is_loopback)
            .then(act(b).cmp(&act(a)))
            .then(a.name.cmp(&b.name))
    });
    out
}

// ---------------------------------------------------------------------------
// /proc/diskstats
// ---------------------------------------------------------------------------

/// 磁盘一行原始计数（只取用得到的字段）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskRaw {
    pub name: String,
    pub reads: u64,
    pub read_sectors: u64,
    pub read_ms: u64,
    pub writes: u64,
    pub write_sectors: u64,
    pub write_ms: u64,
    /// 正在进行中的 IO 数（快照值，不做差分）。
    pub in_flight: u64,
    pub io_ms: u64,
}

/// 解析 `/proc/diskstats`（跳过分区行由调用方按 `name` 判断）。
pub fn parse_diskstats(text: &str) -> Vec<DiskRaw> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 14 {
            continue;
        }
        let g = |i: usize| f.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        out.push(DiskRaw {
            name: f[2].to_string(),
            reads: g(3),
            read_sectors: g(5),
            read_ms: g(6),
            writes: g(7),
            write_sectors: g(9),
            write_ms: g(10),
            in_flight: g(11),
            io_ms: g(12),
        });
    }
    out
}

/// 用两次采样算磁盘速率（扇区按 512 字节）。
pub fn disk_delta(cur: &[DiskRaw], prev: &[DiskRaw], dt: f64) -> Vec<DiskStat> {
    let dt = if dt <= 0.0 { 1.0 } else { dt };
    let mut out: Vec<DiskStat> = cur
        .iter()
        .filter(|d| !is_partition_name(&d.name))
        .map(|c| {
            let p = prev.iter().find(|p| p.name == c.name);
            let d = |a: u64, b: Option<u64>| b.map(|b| a.saturating_sub(b)).unwrap_or(0);
            let (dr, dw, drm, dwm, dio) = match p {
                Some(p) => (
                    d(c.read_sectors, Some(p.read_sectors)),
                    d(c.write_sectors, Some(p.write_sectors)),
                    d(c.read_ms, Some(p.read_ms)),
                    d(c.write_ms, Some(p.write_ms)),
                    d(c.io_ms, Some(p.io_ms)),
                ),
                None => (0, 0, 0, 0, 0),
            };
            let ios = d(c.reads, p.map(|p| p.reads)) + d(c.writes, p.map(|p| p.writes));
            DiskStat {
                name: c.name.clone(),
                read_bytes_s: dr as f64 * 512.0 / dt,
                write_bytes_s: dw as f64 * 512.0 / dt,
                read_iops: d(c.reads, p.map(|p| p.reads)) as f64 / dt,
                write_iops: d(c.writes, p.map(|p| p.writes)) as f64 / dt,
                util: (100.0 * dio as f64 / (dt * 1000.0)).clamp(0.0, 100.0) as f32,
                await_ms: if ios > 0 {
                    (drm + dwm) as f64 / ios as f64
                } else {
                    0.0
                },
                ..Default::default()
            }
        })
        .collect();
    out.sort_by(|a, b| {
        let act = |d: &DiskStat| (d.read_bytes_s + d.write_bytes_s) as u64;
        act(b).cmp(&act(a)).then(a.name.cmp(&b.name))
    });
    out
}

/// sda1 / nvme0n1p2 / mmcblk0p1 这种分区名（而不是整盘）。
fn is_partition_name(name: &str) -> bool {
    // 这些前缀后的数字属于设备名本身（dm-0 / loop0 / zram0 / md0 / sr0 / ram0）
    const WHOLE_PREFIX: [&str; 6] = ["dm-", "loop", "zram", "md", "sr", "ram"];
    if WHOLE_PREFIX.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    // nvme0n1 = 整盘，nvme0n1p1 = 分区
    if let Some(rest) = name.strip_prefix("nvme") {
        return rest.contains('p');
    }
    if let Some(rest) = name.strip_prefix("mmcblk") {
        return rest.contains('p');
    }
    // 其余（sd*/vd*/hd*）：以数字结尾即分区
    name.chars().last().is_some_and(|c| c.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// /proc/<pid>/*
// ---------------------------------------------------------------------------

/// `/proc/<pid>/stat` 里用得到的字段。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PidRaw {
    pub pid: i32,
    pub comm: String,
    pub state: char,
    pub ppid: i32,
    pub pgrp: i32,
    pub session: i32,
    pub minflt: u64,
    pub majflt: u64,
    pub utime: u64,
    pub stime: u64,
    pub priority: i64,
    pub nice: i64,
    pub num_threads: i64,
    pub starttime: u64,
    pub vsize: u64,
    /// rss（页数，0 表示不可用）。
    pub rss_pages: i64,
    pub rt_priority: i64,
    /// 调度策略编号：0 OTHER / 1 FIFO / 2 RR / 3 BATCH / 5 IDLE / 6 DEADLINE。
    pub policy: i64,
}

impl PidRaw {
    /// 用户态 + 内核态 CPU 时间（jiffies）。
    pub fn cpu_ticks(&self) -> u64 {
        self.utime + self.stime
    }
}

/// 解析 `/proc/<pid>/stat`。
///
/// comm 在括号里且**可以包含空格和括号**，所以必须按第一个 `(` 和最后一个
/// `)` 切分，不能用 `split_whitespace`。
pub fn parse_pid_stat(text: &str) -> Option<PidRaw> {
    let mut fields = Vec::new();
    parse_pid_stat_into(text, &mut fields)
}

/// 同 [`parse_pid_stat`]，但字段索引向量由调用方提供（GOAL.md 2.2：热路径
/// 跨进程复用同一个 `Vec`，省掉每进程一次堆分配）。
///
/// 索引存的是**字节区间**而不是 `&str`：区间不借用 `text`，`text` 缓冲可以
/// 随意清空复用（存 `&str` 会和跨循环的 `text.clear()` 打架，见 rust 闭包/
/// 循环借用推断）。
pub fn parse_pid_stat_into(text: &str, rest: &mut Vec<(usize, usize)>) -> Option<PidRaw> {
    rest.clear();
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let pid: i32 = text[..open].trim().parse().ok()?;
    let comm = text[open + 1..close].to_string();
    let base = close + 1;
    let base_addr = text.as_ptr() as usize;
    for w in text[base..].split_whitespace() {
        let start = w.as_ptr() as usize - base_addr;
        rest.push((start, start + w.len()));
    }
    let s = |i: usize| rest.get(i).map(|&(a, b)| &text[a..b]).unwrap_or("");
    let n = |i: usize| s(i).parse::<u64>().unwrap_or(0);
    let i = |i: usize| s(i).parse::<i64>().unwrap_or(0);
    Some(PidRaw {
        pid,
        comm,
        state: s(0).chars().next().unwrap_or('?'),
        ppid: i(1) as i32,
        pgrp: i(2) as i32,
        session: i(3) as i32,
        minflt: n(7),
        majflt: n(9),
        utime: n(11),
        stime: n(12),
        priority: i(15),
        nice: i(16),
        num_threads: i(17),
        starttime: n(19),
        vsize: n(20),
        rss_pages: i(21),
        // 字段号 40 / 41（0 基下标 = 字段号 - 3）
        rt_priority: i(37),
        policy: i(38),
    })
}

/// 解析 `/proc/<pid>/io`：返回 (read_bytes, write_bytes, rchar, wchar)。
pub fn parse_pid_io(text: &str) -> (u64, u64, u64, u64) {
    let g = |key: &str| {
        text.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    };
    (
        g("read_bytes:"),
        g("write_bytes:"),
        g("rchar:"),
        g("wchar:"),
    )
}

/// 解析 `/proc/<pid>/cmdline`（NUL 分隔）成一行。
pub fn parse_cmdline(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let joined = s
        .split('\0')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    joined.trim().to_string()
}

/// 解析 `/proc/<pid>/cgroup`，取第一条的路径部分（`0::/user.slice/...`）。
pub fn parse_cgroup(text: &str) -> String {
    text.lines()
        .next()
        .and_then(|l| l.split_once("::"))
        .map(|(_, path)| path.trim().to_string())
        .unwrap_or_default()
}

/// 调度策略编号 → 名称。
pub fn policy_name(policy: i64) -> &'static str {
    match policy {
        0 => "OTHER",
        1 => "FIFO",
        2 => "RR",
        3 => "BATCH",
        5 => "IDLE",
        6 => "DEADLINE",
        _ => "?",
    }
}

/// 进程状态字符 → 中文说明。
pub fn state_label(state: char) -> &'static str {
    match state {
        'R' => "运行",
        'S' => "睡眠",
        'D' => "不可中断",
        'T' => "已停止",
        't' => "跟踪停止",
        'Z' => "僵死",
        'X' | 'x' => "已死",
        'I' => "空闲内核",
        'K' => "内核可唤醒",
        'W' => "换页中",
        'P' => "停放",
        _ => "未知",
    }
}

/// 从 `/etc/passwd` 读 uid → 用户名。
pub fn uid_names() -> HashMap<u32, String> {
    let mut m = HashMap::new();
    if let Ok(text) = fs::read_to_string("/etc/passwd") {
        for line in text.lines() {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() >= 3
                && let Ok(uid) = f[2].parse::<u32>()
            {
                m.insert(uid, f[0].to_string());
            }
        }
    }
    m
}

/// 进程 CPU%：两次采样之间的 ticks 差换算成百分比。
///
/// 口径是「全部逻辑核心 = 100%」（与系统监视器一致）。top/htop 用的是
/// 「1 个核心 = 100%」，同一进程在 32 线程机器上会显示成这里的 32 倍——
/// 两者都对，只是归一化基准不同，这里选了和系统监视器一致的那种。
pub fn proc_cpu_pct(delta_ticks: u64, ticks_per_sec: u64, dt: f64) -> f32 {
    let dt = if dt <= 0.0 { 1.0 } else { dt };
    (100.0 * delta_ticks as f64 / ticks_per_sec.max(1) as f64 / dt / n_cpus() as f64) as f32
}

/// IO 调度类文本（进程列表「IO」列），读不到返回空串。
fn io_class_text(pid: i32) -> String {
    crate::utils::monitor::signal::get_io_priority(pid)
        .map(|(c, l)| {
            if c.has_level() {
                format!("{}/{}", c.short(), l)
            } else {
                c.short().to_string()
            }
        })
        .unwrap_or_default()
}

/// 收集所有进程的概要（差分用上次的 pid → (cpu_ticks, read_bytes, write_bytes)）。
///
/// `ticks_per_sec` 是 CLK_TCK；`dt` 是两次采样的间隔秒数。
/// `caches` 是跨周期的 pid 级缓存（cmdline/cgroup/io_class，调用方负责按存活
/// pid 清理）；`want_io` 控制是否读 `/proc/<pid>/io`（界面需要速率时才开）；
/// `io_valid` 表示上一轮是否读过 io（关→开的第一轮差分基准无效，速率置 0）。
///
/// # 分层采样（GOAL.md 2.1 / 2.3）
/// 每轮每个进程只读 `stat` +（缓存未命中时的）`cmdline/cgroup`；`io` 按需读；
/// 亲和性不再每轮计算（详情/对话框现场调 [`affinity_hex`]）。
#[doc(hidden)]
#[derive(Default)]
pub struct PidCache {
    starttime: u64,
    comm: String,
    cmdline: String,
    cgroup: String,
    io_class: String,
}

// 采样热路径（GOAL 2.x 优化核心）：参数再打包成 struct 会改动调用方与
// bench，收益仅是 lint 数字，此处如实豁免。
#[allow(clippy::too_many_arguments)]
pub fn collect_processes(
    prev: &HashMap<i32, (u64, u64, u64)>,
    ticks_per_sec: u64,
    dt: f64,
    total_mem: u64,
    btime: u64,
    uptime: u64,
    users: &HashMap<u32, String>,
    caches: &mut HashMap<i32, PidCache>,
    want_io: bool,
    io_valid: bool,
) -> Vec<Process> {
    let page_size = 4096u64;
    let dt = if dt <= 0.0 { 1.0 } else { dt };
    let mut out = Vec::with_capacity(prev.len().max(256));
    // —— 缓冲复用（GOAL.md 2.2）：路径 / 文本 / 字段索引 / cmdline 字节跨进程复用 ——
    use std::io::Read as _;
    let mut path_buf = String::with_capacity(64);
    let mut text = String::with_capacity(1024);
    let mut cmd_bytes: Vec<u8> = Vec::with_capacity(512);
    let mut stat_fields: Vec<(usize, usize)> = Vec::with_capacity(48);

    let Ok(rd) = fs::read_dir("/proc") else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        // uid 直接看 /proc/<pid> 的属主，省一次文件读
        let uid = e
            .metadata()
            .map(|m| std::os::unix::fs::MetadataExt::uid(&m))
            .unwrap_or(0);
        // 路径拼成字符串复用缓冲（省掉每进程2 次 PathBuf 分配）
        stat_fields.clear();
        path_buf.clear();
        path_buf.push_str("/proc/");
        path_buf.push_str(&name);
        let base_len = path_buf.len();
        path_buf.push_str("/stat");
        text.clear();
        if fs::File::open(&path_buf)
            .and_then(|mut f| f.read_to_string(&mut text))
            .is_err()
        {
            continue; // 进程刚退出
        }
        let Some(raw) = parse_pid_stat_into(&text, &mut stat_fields) else {
            continue;
        };

        // —— 分层采样（GOAL.md 2.1/2.3）——
        // cmdline / cgroup / io_class 按 pid 缓存：cmdline 只在 comm 变化
        // （execve 会改 comm）时重读；cgroup 按 starttime 失效；io_class
        // 没有高频变化源，直接按 pid 缓存（进程消失时由调用方清理）。
        let cmdline;
        let cgroup;
        let io_class;
        let stale = caches
            .get(&pid)
            .is_none_or(|c| c.starttime != raw.starttime);
        if stale {
            path_buf.truncate(base_len);
            path_buf.push_str("/cmdline");
            cmd_bytes.clear();
            let _ = fs::File::open(&path_buf).and_then(|mut f| f.read_to_end(&mut cmd_bytes));
            cmdline = parse_cmdline(&cmd_bytes);
            path_buf.truncate(base_len);
            path_buf.push_str("/cgroup");
            text.clear();
            let _ = fs::File::open(&path_buf).and_then(|mut f| f.read_to_string(&mut text));
            cgroup = parse_cgroup(&text);
            io_class = io_class_text(pid);
            caches.insert(
                pid,
                PidCache {
                    starttime: raw.starttime,
                    comm: raw.comm.clone(),
                    cmdline: cmdline.clone(),
                    cgroup: cgroup.clone(),
                    io_class: io_class.clone(),
                },
            );
        } else {
            // 内部不变量（非 IO 错误路径）：上一步刚 insert 必存在
            let c = caches.get_mut(&pid).expect("上一步刚查过必存在");
            if c.comm != raw.comm {
                // execve 保留 starttime，但 comm / cmdline 都会变
                c.comm = raw.comm.clone();
                path_buf.truncate(base_len);
                path_buf.push_str("/cmdline");
                cmd_bytes.clear();
                let _ = fs::File::open(&path_buf).and_then(|mut f| f.read_to_end(&mut cmd_bytes));
                c.cmdline = parse_cmdline(&cmd_bytes);
            }
            cmdline = c.cmdline.clone();
            cgroup = c.cgroup.clone();
            io_class = c.io_class.clone();
        }
        // /proc/<pid>/io 只在速率列可见 / 详情选中时才读（GOAL.md 2.1）
        let (read_bytes, write_bytes, rchar, wchar) = if want_io {
            path_buf.truncate(base_len);
            path_buf.push_str("/io");
            text.clear();
            let ok = fs::File::open(&path_buf)
                .and_then(|mut f| f.read_to_string(&mut text))
                .is_ok();
            if ok {
                parse_pid_io(&text)
            } else {
                (0, 0, 0, 0)
            }
        } else {
            (0, 0, 0, 0)
        };

        let ticks = raw.cpu_ticks();
        // 速率只有「上一轮也读了 io」时才可信，否则差分基准是空值会算出天文数字
        let rates_ok = want_io && io_valid;
        let (cpu, read_rate, write_rate) = match prev.get(&pid) {
            Some((pt, pr, pw)) => {
                let dc = ticks.saturating_sub(*pt);
                (
                    proc_cpu_pct(dc, ticks_per_sec, dt),
                    if rates_ok {
                        (read_bytes.saturating_sub(*pr)) as f64 / dt
                    } else {
                        0.0
                    },
                    if rates_ok {
                        (write_bytes.saturating_sub(*pw)) as f64 / dt
                    } else {
                        0.0
                    },
                )
            }
            _ => (0.0, 0.0, 0.0),
        };
        // 用不到的字段也读一下，保证「打开的文件描述符数」一类信息完整
        let _ = (rchar, wchar);

        let rss = (raw.rss_pages.max(0) as u64) * page_size;
        let started = raw.starttime / ticks_per_sec;
        let uptime_secs = uptime.saturating_sub(started);

        out.push(Process {
            pid,
            ppid: raw.ppid,
            name: raw.comm,
            cmdline,
            exe: String::new(),
            uid,
            user: users.get(&uid).cloned().unwrap_or_else(|| uid.to_string()),
            state: state_label(raw.state).to_string(),
            state_char: raw.state,
            cpu,
            mem_pct: if total_mem > 0 {
                100.0 * rss as f32 / total_mem as f32
            } else {
                0.0
            },
            rss,
            virt: raw.vsize,
            swap: 0,
            threads: raw.num_threads,
            nice: raw.nice,
            priority: raw.priority,
            policy: policy_name(raw.policy).to_string(),
            io_class,
            cpu_time: ticks as f64 / ticks_per_sec as f64,
            uptime: uptime_secs,
            read_bytes,
            write_bytes,
            read_rate,
            write_rate,
            minflt: raw.minflt,
            majflt: raw.majflt,
            on_cpu: if raw.state == 'R' { 1 } else { 0 },
            pgid: raw.pgrp,
            sid: raw.session,
            affinity: String::new(), // 快照不算亲和性：详情/对话框现场调 affinity_hex
            depth: 0,
            child_count: 0,
            cgroup,
            started_at: btime + started,
        });
    }
    out
}

/// 把进程列表按父进程关系摊平：树形模式按 DFS 输出（同级按 `cmp` 排序）；
/// `collapsed` 里的 pid 其子树会被跳过。
pub fn flatten_tree(
    procs: Vec<Process>,
    mode: TreeMode,
    collapsed: &std::collections::HashSet<i32>,
    cmp: &dyn Fn(&Process, &Process) -> std::cmp::Ordering,
) -> Vec<Process> {
    if mode == TreeMode::Flat {
        let mut v = procs;
        v.sort_by(|a, b| cmp(a, b));
        return v;
    }

    let pids: std::collections::HashSet<i32> = procs.iter().map(|p| p.pid).collect();
    let mut children: HashMap<i32, Vec<Process>> = HashMap::new();
    let mut roots: Vec<Process> = Vec::new();
    for p in procs {
        // 父进程不存在（或自己是 0/1）的当根
        if p.ppid == p.pid || !pids.contains(&p.ppid) {
            roots.push(p);
        } else {
            children.entry(p.ppid).or_default().push(p);
        }
    }
    // 子进程数（界面上用来决定要不要画折叠箭头）
    let child_counts: HashMap<i32, usize> = children.iter().map(|(k, v)| (*k, v.len())).collect();

    let mut out: Vec<Process> = Vec::new();
    let mut stack: Vec<(Process, usize)> = roots.drain(..).map(|p| (p, 0usize)).collect::<Vec<_>>();
    stack.sort_by(|a, b| cmp(&a.0, &b.0));
    // 显式栈做 DFS，保证顺序稳定
    let mut queue: std::collections::VecDeque<(Process, usize)> =
        stack.into_iter().collect::<std::collections::VecDeque<_>>();
    while let Some((mut p, depth)) = queue.pop_front() {
        p.depth = depth;
        p.child_count = child_counts.get(&p.pid).copied().unwrap_or(0);
        let pid = p.pid;
        let hidden = collapsed.contains(&pid);
        out.push(p);
        if hidden {
            continue;
        }
        if let Some(mut kids) = children.remove(&pid) {
            kids.sort_by(|a, b| cmp(a, b));
            // 插入到队首，保证兄弟连续、且深度优先
            for (i, k) in kids.into_iter().enumerate() {
                queue.insert(i, (k, depth + 1));
            }
        }
    }
    out
}

/// 按列比较两个进程（升序）；`desc` 时取反。
pub fn compare_by(a: &Process, b: &Process, col: ProcColumn, desc: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ord = match col {
        ProcColumn::Name => a
            .display_name()
            .to_lowercase()
            .cmp(&b.display_name().to_lowercase()),
        ProcColumn::Pid => a.pid.cmp(&b.pid),
        ProcColumn::User => a.user.cmp(&b.user),
        ProcColumn::State => a.state_char.cmp(&b.state_char),
        ProcColumn::Cpu => a.cpu.partial_cmp(&b.cpu).unwrap_or(Ordering::Equal),
        ProcColumn::Mem => a.rss.cmp(&b.rss),
        ProcColumn::MemPct => a.mem_pct.partial_cmp(&b.mem_pct).unwrap_or(Ordering::Equal),
        ProcColumn::Threads => a.threads.cmp(&b.threads),
        ProcColumn::Priority => a.priority.cmp(&b.priority),
        ProcColumn::Nice => a.nice.cmp(&b.nice),
        ProcColumn::Policy => a.policy.cmp(&b.policy),
        ProcColumn::Io => a.io_class.cmp(&b.io_class),
        ProcColumn::ReadRate => a
            .read_rate
            .partial_cmp(&b.read_rate)
            .unwrap_or(Ordering::Equal),
        ProcColumn::WriteRate => a
            .write_rate
            .partial_cmp(&b.write_rate)
            .unwrap_or(Ordering::Equal),
        ProcColumn::CpuTime => a
            .cpu_time
            .partial_cmp(&b.cpu_time)
            .unwrap_or(Ordering::Equal),
        ProcColumn::Started => a.uptime.cmp(&b.uptime),
        ProcColumn::Command => a.cmdline.cmp(&b.cmdline),
        ProcColumn::Cgroup => a.cgroup.cmp(&b.cgroup),
    };
    let ord = if desc { ord.reverse() } else { ord };
    // 平局用 pid 兜底，保证顺序稳定（否则每秒都在跳）；
    // 兜底方向也随 desc 翻转，这样「降序」严格等于「升序的反向」
    let tie = if desc {
        b.pid.cmp(&a.pid)
    } else {
        a.pid.cmp(&b.pid)
    };
    ord.then(tie)
}

/// 过滤：搜索关键字（名称/命令行/用户/pid）、是否只看当前用户、是否隐藏内核线程。
pub fn matches_filter(p: &Process, query: &str, only_uid: Option<u32>, hide_kernel: bool) -> bool {
    if hide_kernel && p.cmdline.is_empty() && p.pid != 1 {
        return false;
    }
    if let Some(uid) = only_uid
        && p.uid != uid
    {
        return false;
    }
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    // 纯数字按 pid 精确/前缀匹配
    if q.chars().all(|c| c.is_ascii_digit()) && p.pid.to_string().starts_with(&q) {
        return true;
    }
    p.display_name().to_lowercase().contains(&q)
        || p.cmdline.to_lowercase().contains(&q)
        || p.user.to_lowercase().contains(&q)
}

/// 一个进程及其所有后代（用于「结束整个进程树」）。
pub fn descendants_of(procs: &[Process], pid: i32) -> Vec<i32> {
    let mut out = vec![pid];
    let mut i = 0;
    while i < out.len() {
        let cur = out[i];
        for p in procs.iter() {
            if p.ppid == cur && p.pid != cur && !out.contains(&p.pid) {
                out.push(p.pid);
            }
        }
        i += 1;
    }
    out
}

/// 该 pid 是否是内核对线程（无命令行且不是 init）。
pub fn is_kernel_thread(p: &Process) -> bool {
    p.cmdline.is_empty() && p.pid != 1
}

/// 进程详情（按需读取，不进每秒采样）。
pub fn process_detail(pid: i32) -> crate::model::monitor::ProcessDetail {
    use crate::model::monitor::ProcessDetail;
    let mut d = ProcessDetail {
        pid,
        ..Default::default()
    };
    let base = Path::new("/proc").join(pid.to_string());
    let link = |n: &str| -> String {
        fs::read_link(base.join(n))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    d.exe = link("exe");
    d.cwd = link("cwd");
    d.root = link("root");

    if let Ok(text) = fs::read_to_string(base.join("status")) {
        d.status_text = text.clone();
        for line in text.lines() {
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            let v = v.trim();
            let kb = || -> u64 {
                v.split_whitespace()
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(|k| k * 1024)
                    .unwrap_or(0)
            };
            match k {
                "Groups" => {
                    d.groups = v
                        .split_whitespace()
                        .filter_map(|s| s.parse::<u32>().ok())
                        .collect()
                }
                "VmPeak" => d.vm_peak = kb(),
                "VmSize" => d.vm_size = kb(),
                "VmRSS" => d.vm_rss = kb(),
                "VmData" => d.vm_data = kb(),
                "VmStk" => d.vm_stk = kb(),
                "VmExe" => d.vm_exe = kb(),
                "VmLib" => d.vm_lib = kb(),
                "VmSwap" => d.vm_swap = kb(),
                _ => {}
            }
        }
    }
    // 环境变量
    if let Ok(bytes) = fs::read(base.join("environ")) {
        d.env = parse_cmdline(&bytes)
            .split_whitespace()
            .filter(|s| s.contains('='))
            .map(|s| s.to_string())
            .collect();
    }
    // 打开的文件描述符
    if let Ok(rd) = fs::read_dir(base.join("fd")) {
        let mut fds: Vec<String> = rd
            .flatten()
            .map(|e| {
                let target = fs::read_link(e.path())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "?".to_string());
                format!("{} → {}", e.file_name().to_string_lossy(), target)
            })
            .collect();
        fds.sort();
        d.fds = fds;
    }
    // 线程
    if let Ok(rd) = fs::read_dir(base.join("task")) {
        let mut v: Vec<(i32, char, String)> = rd
            .flatten()
            .filter_map(|e| {
                let tid = e.file_name().to_string_lossy().parse::<i32>().ok()?;
                let stat = fs::read_to_string(e.path().join("stat")).ok()?;
                let raw = parse_pid_stat(&stat)?;
                Some((tid, raw.state, raw.comm))
            })
            .collect();
        v.sort();
        d.thread_list = v;
    }
    // 命名空间
    if let Ok(rd) = fs::read_dir(base.join("ns")) {
        let mut v: Vec<String> = rd
            .flatten()
            .map(|e| {
                let t = fs::read_link(e.path())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                format!("{}: {}", e.file_name().to_string_lossy(), t)
            })
            .collect();
        v.sort();
        d.namespaces = v;
    }
    if let Ok(text) = fs::read_to_string(base.join("io")) {
        let (r, w, _, _) = parse_pid_io(&text);
        d.read_bytes = r;
        d.write_bytes = w;
    }
    d.cgroup = fs::read_to_string(base.join("cgroup"))
        .map(|t| parse_cgroup(&t))
        .unwrap_or_default();
    d.affinity = affinity_hex(pid);
    d
}

/// 读取 CPU 亲和性掩码（十六进制），读不到返回空串。
///
/// 从高位字节向低位拼接（等价于「全拼再 trim 前导 0」），跳过前导零字节、
/// 用 `write!` 直接写入缓冲：原来每字节一次 `format!` 分配，一次调用
/// 128 次堆分配，放不进每轮采样热路径。
pub fn affinity_hex(pid: i32) -> String {
    unsafe extern "C" {
        fn sched_getaffinity(pid: i32, cpusetsize: usize, mask: *mut u8) -> i32;
    }
    let mut mask = [0u8; 128]; // 最多 1024 个 CPU
    let r = unsafe { sched_getaffinity(pid, mask.len(), mask.as_mut_ptr()) };
    if r != 0 {
        return String::new();
    }
    let mut hex = String::with_capacity(48);
    let mut started = false;
    use std::fmt::Write as _;
    for byte in mask.iter().rev() {
        if !started {
            if *byte == 0 {
                continue;
            }
            started = true;
        }
        let _ = write!(hex, "{byte:02x}");
    }
    let trimmed = hex.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 把亲和性掩码解析成 CPU 编号列表（十六进制，右侧对齐）。
pub fn affinity_cpus(hex: &str) -> Vec<usize> {
    let h = hex.trim().trim_start_matches("0x");
    let chars: Vec<char> = h.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let mut cpus = Vec::new();
    // 每 32 个十六进制字符 = 128 位，从右往左分组
    let groups = chars.len().div_ceil(32);
    for g in 0..groups {
        let end = chars.len().saturating_sub(g * 32);
        let start = end.saturating_sub(32);
        let chunk: String = chars[start..end].iter().collect();
        if let Ok(v) = u128::from_str_radix(&chunk, 16) {
            for b in 0..128u32 {
                if v & (1u128 << b) != 0 {
                    cpus.push(g * 128 + b as usize);
                }
            }
        }
    }
    cpus.sort_unstable();
    cpus
}

/// 把 CPU 编号列表压成亲和性掩码（十六进制，供 sched_setaffinity 用）。
pub fn affinity_mask(cpus: &[usize]) -> Vec<u8> {
    let mut mask = vec![0u8; 128];
    for c in cpus {
        let byte = c / 8;
        if byte < mask.len() {
            mask[byte] |= 1 << (c % 8);
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::monitor::TreeMode;
    use std::collections::HashSet;

    // 真实 /proc/stat 片段
    const STAT: &str = "cpu  38540507 3432 24465425 882807111 7590260 0 95667 0 0 0\n\
cpu0 1935436 333 1930068 25261280 359417 0 2046 0 0 0\n\
cpu1 1225670 111 978676 27089957 428581 0 4639 0 0 0\n\
intr 8181991554 39 0 0 0\n\
ctxt 13765890836\n\
btime 1789102182\n\
processes 22387285\n\
procs_running 1\n\
procs_blocked 0\n";

    #[test]
    fn parse_stat_blocks() {
        let (total, cores, t) = parse_stat(STAT);
        assert_eq!(cores.len(), 2);
        assert_eq!(total.user, 38540507);
        assert_eq!(total.iowait, 7590260);
        assert_eq!(total.softirq, 95667);
        assert_eq!(cores[1].system, 978676);
        assert_eq!(t.ctxt, 13765890836);
        assert_eq!(t.intr, 8181991554);
        assert_eq!(t.btime, 1789102182);
        assert_eq!(t.procs_running, 1);
        assert_eq!(t.processes, 22387285);
        // 总量应等于各列之和
        assert_eq!(
            total.total(),
            ((38540507 + 3432 + 24465425 + 882807111 + 7590260) + 95667)
        );
    }

    #[test]
    fn cpu_usage_between_two_samples() {
        let prev = CpuJiffies {
            user: 100,
            idle: 900,
            ..Default::default()
        };
        let cur = CpuJiffies {
            user: 200,
            idle: 1700,
            ..Default::default()
        };
        // 忙碌 100 / 总计 900
        let u = cur.usage_since(&prev);
        assert!((u - 100.0 * 100.0 / 900.0).abs() < 0.01);
        // 没变化时不能除零
        assert_eq!(cur.usage_since(&cur), 0.0);
    }

    const MEMINFO: &str = "MemTotal:       32784652 kB\n\
MemFree:          987364 kB\n\
MemAvailable:   14965320 kB\n\
Buffers:              40 kB\n\
Cached:         13988464 kB\n\
SwapCached:       582832 kB\n\
SwapTotal:       8388604 kB\n\
SwapFree:        4194304 kB\n\
Shmem:            123456 kB\n\
Slab:             987654 kB\n";

    #[test]
    fn parse_meminfo_units_and_used() {
        let m = parse_meminfo(MEMINFO);
        assert_eq!(m.total, 32784652 * 1024);
        assert_eq!(m.available, 14965320 * 1024);
        assert_eq!(m.shared, 123456 * 1024);
        assert_eq!(m.swap_total, 8388604 * 1024);
        assert_eq!(m.swap_used, (8388604 - 4194304) * 1024);
        // 已用 = 总 - 空闲 - 缓冲 - 缓存
        assert_eq!(m.used, (32784652 - 987364 - 40 - 13988464) * 1024);
        assert!(m.used_ratio() > 0.4 && m.used_ratio() < 0.7);
        // SwapCached 不能污染 Cached
        assert_eq!(m.cached, 13988464 * 1024);
    }

    #[test]
    fn parse_loadavg_and_uptime() {
        let (load, running, procs) = parse_loadavg("1.61 1.58 1.73 1/5169 1440281");
        assert_eq!(load, [1.61, 1.58, 1.73]);
        assert_eq!(running, 1);
        assert_eq!(procs, 5169);
        assert_eq!(parse_uptime("299725.81 8828071.24"), 299725);
    }

    const NETDEV: &str = "Inter-|   Receive                                                |  Transmit\n\
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n\
    lo: 1763511367 1457464    0    0    0     0          0         0 1763511367 1457464    0    0    0     0       0          0\n\
enp4s0:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0\n\
wlp3s0: 46469330773 38555671    0    3    0     0          0         0 2224624108 14356376    0    0    0     0       0          0\n";

    #[test]
    fn parse_net_dev_fields() {
        let v = parse_net_dev(NETDEV);
        assert_eq!(v.len(), 3);
        let w = v.iter().find(|i| i.name == "wlp3s0").unwrap();
        assert_eq!(w.rx_bytes, 46469330773);
        assert_eq!(w.rx_packets, 38555671);
        assert_eq!(w.rx_drop, 3);
        assert_eq!(w.tx_bytes, 2224624108);
        assert_eq!(w.tx_packets, 14356376);
        // 速率按 1 秒间隔算
        let mut cur = v.clone();
        cur.iter_mut().for_each(|i| i.rx_bytes += 1024);
        let rate = net_delta(&cur, &v, 1.0);
        let w2 = rate.iter().find(|i| i.name == "wlp3s0").unwrap();
        assert!((w2.rx_rate - 1024.0).abs() < 0.001);
        // 回环排在最后
        assert!(rate.last().unwrap().is_loopback);
    }

    const DISKSTATS: &str = "   8       0 sda 1920009 550608 160801178 2628586 8596468 11427498 551943530 7116396 0 1875380 10546756 246782 0 1593620536 245919 753551 555853\n\
   8       1 sda1 193 3860 9970 85 2 0 2 0 0 48 89 3 0 1976368 4 0 0\n\
 259       0 nvme0n1 530541 231291 68494122 250793 2152581 3621693 46972846 810184 0 387901 4293224 3 0 0 0 0 0 0\n\
 259       1 nvme0n1p1 529977 231291 68469576 250680 2152581 3621693 46972846 810184 0 387901 4294195 1 0 0 0 0 0 0\n";

    #[test]
    fn parse_diskstats_fields_and_partition_filter() {
        let v = parse_diskstats(DISKSTATS);
        assert_eq!(v.len(), 4);
        let sda = v.iter().find(|d| d.name == "sda").unwrap();
        assert_eq!(sda.reads, 1920009);
        assert_eq!(sda.read_sectors, 160801178);
        assert_eq!(sda.writes, 8596468);
        assert_eq!(sda.write_sectors, 551943530);
        // io_ms = 下标 12（下标 13 是加权后的 weighted_ms_io）
        assert_eq!(sda.io_ms, 1875380);
        // 分区判定
        assert!(is_partition_name("sda1"));
        assert!(is_partition_name("nvme0n1p1"));
        assert!(!is_partition_name("sda"));
        assert!(!is_partition_name("nvme0n1"));
        assert!(!is_partition_name("dm-0"));
        assert!(!is_partition_name("loop0"));
        assert!(!is_partition_name("zram0"));
        // 差分成速率，且过滤掉分区
        let mut cur = v.clone();
        for d in cur.iter_mut() {
            d.read_sectors += 2048; // 1 MiB
            d.writes += 10;
        }
        let out = disk_delta(&cur, &v, 1.0);
        assert_eq!(out.len(), 2);
        let sda2 = out.iter().find(|d| d.name == "sda").unwrap();
        assert!((sda2.read_bytes_s - 1_048_576.0).abs() < 1.0);
        assert!((sda2.write_iops - 10.0).abs() < 0.001);
    }

    // 真实 /proc/4637/stat（bash，注意 comm 不含空格）
    const PID_STAT: &str = "4637 (bash) S 3583 4637 4637 0 -1 4194304 2967 57803 1 1387 0 1 50 66 20 0 1 0 15945 8179712 669 18446744073709551615 94078892355584 94078893178721 140727611303808 0 0 0 65536 4100 65538 1 0 0 17 25 0 0 0 0 0 94078893410896 94078893459300 94079826243584 140727611307695 140727611307769 140727611307769 140727611310058 0";

    #[test]
    fn parse_pid_stat_real_line() {
        let r = parse_pid_stat(PID_STAT).unwrap();
        assert_eq!(r.pid, 4637);
        assert_eq!(r.comm, "bash");
        assert_eq!(r.state, 'S');
        assert_eq!(r.ppid, 3583);
        assert_eq!(r.pgrp, 4637);
        assert_eq!(r.session, 4637);
        assert_eq!(r.minflt, 2967);
        assert_eq!(r.majflt, 1);
        assert_eq!(r.utime, 0);
        assert_eq!(r.stime, 1);
        assert_eq!(r.priority, 20);
        assert_eq!(r.nice, 0);
        assert_eq!(r.num_threads, 1);
        assert_eq!(r.starttime, 15945);
        assert_eq!(r.vsize, 8179712);
        assert_eq!(r.rss_pages, 669);
        // 字段号 40/41 → 下标 37/38
        assert_eq!(r.rt_priority, 0);
        assert_eq!(policy_name(r.policy), "OTHER");
    }

    #[test]
    fn parse_pid_stat_handles_parens_and_spaces_in_comm() {
        // comm 里带空格和括号；下标 = 字段号 - 3
        let mut f: Vec<String> = (0..39).map(|_| "0".to_string()).collect();
        f[0] = "R".into(); // state
        f[1] = "1".into(); // ppid
        f[15] = "25".into(); // priority
        f[16] = "5".into(); // nice
        f[17] = "4".into(); // num_threads
        f[19] = "100".into(); // starttime
        f[20] = "1000".into(); // vsize
        f[21] = "10".into(); // rss
        f[38] = "1".into(); // policy = SCHED_FIFO
        let line = format!("999 (my (weird) proc) {}", f.join(" "));
        let r = parse_pid_stat(&line).unwrap();
        assert_eq!(r.pid, 999);
        assert_eq!(r.comm, "my (weird) proc");
        assert_eq!(r.state, 'R');
        assert_eq!(r.ppid, 1);
        assert_eq!(r.priority, 25);
        assert_eq!(r.nice, 5);
        assert_eq!(r.num_threads, 4);
        assert_eq!(r.starttime, 100);
        assert_eq!(r.rss_pages, 10);
        assert_eq!(policy_name(r.policy), "FIFO");
    }

    #[test]
    fn parse_io_cmdline_cgroup() {
        let io = "rchar: 76650581\nwchar: 10114331\nsyscr: 16204\nsyscw: 3092\nread_bytes: 4096\nwrite_bytes: 8192\n";
        assert_eq!(parse_pid_io(io), (4096, 8192, 76650581, 10114331));
        let cmd = b"bash\0/home/u/steam.sh\0-srt-logger-opened\0";
        assert_eq!(
            parse_cmdline(cmd),
            "bash /home/u/steam.sh -srt-logger-opened"
        );
        assert_eq!(parse_cmdline(b""), "");
        let cg = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-steam@autostart.service\n";
        assert_eq!(
            parse_cgroup(cg),
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-steam@autostart.service"
        );
        assert_eq!(parse_cgroup(""), "");
    }

    fn mk(pid: i32, ppid: i32, name: &str, cpu: f32) -> Process {
        Process {
            pid,
            ppid,
            name: name.to_string(),
            cpu,
            ..Default::default()
        }
    }

    #[test]
    fn flatten_tree_dfs_order_and_depth() {
        let procs = vec![
            mk(1, 0, "init", 0.0),
            mk(2, 1, "a", 0.0),
            mk(3, 2, "a1", 0.0),
            mk(4, 1, "b", 0.0),
            mk(9, 0, "orphan", 0.0),
        ];
        let cmp = |a: &Process, b: &Process| a.pid.cmp(&b.pid);
        let flat = flatten_tree(procs.clone(), TreeMode::Flat, &HashSet::new(), &cmp);
        assert_eq!(flat.len(), 5);
        assert_eq!(
            flat.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 9]
        );

        let tree = flatten_tree(procs.clone(), TreeMode::Tree, &HashSet::new(), &cmp);
        assert_eq!(
            tree.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 9]
        );
        assert_eq!(tree[0].depth, 0);
        assert_eq!(tree[1].depth, 1);
        assert_eq!(tree[2].depth, 2);
        assert_eq!(tree[3].depth, 1);
        assert_eq!(tree[1].child_count, 1);
        assert_eq!(tree[0].child_count, 2);

        // 折叠 pid=2 后，它的子树（3）不出现
        let mut collapsed = HashSet::new();
        collapsed.insert(2);
        let t2 = flatten_tree(procs, TreeMode::Tree, &collapsed, &cmp);
        assert_eq!(
            t2.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![1, 2, 4, 9]
        );
    }

    #[test]
    fn flatten_tree_sorts_siblings_by_cpu_desc() {
        let procs = vec![
            mk(1, 0, "init", 0.0),
            mk(2, 1, "low", 1.0),
            mk(3, 1, "high", 90.0),
        ];
        let cmp = |a: &Process, b: &Process| {
            compare_by(a, b, crate::model::monitor::ProcColumn::Cpu, true)
        };
        let tree = flatten_tree(procs, TreeMode::Tree, &HashSet::new(), &cmp);
        // init 自己排根，两个子进程按 CPU 降序
        assert_eq!(tree[0].pid, 1);
        assert_eq!(tree[1].pid, 3);
        assert_eq!(tree[2].pid, 2);
    }

    #[test]
    fn filters_and_descendants() {
        let mut p = mk(100, 1, "firefox", 5.0);
        p.cmdline = "/usr/lib/firefox/firefox --new-window".to_string();
        p.user = "liang".to_string();
        assert!(matches_filter(&p, "fire", None, false));
        assert!(matches_filter(&p, "FIREFOX", None, false));
        assert!(matches_filter(&p, "100", None, false)); // pid 前缀
        assert!(matches_filter(&p, "liang", None, false)); // 用户
        assert!(matches_filter(&p, "new-window", None, false)); // 命令行
        assert!(!matches_filter(&p, "chrome", None, false));
        assert!(!matches_filter(&p, "", Some(1000), false)); // uid 不匹配
        p.uid = 1000;
        assert!(matches_filter(&p, "", Some(1000), false));

        // 内核线程
        let kt = mk(2, 0, "kthreadd", 0.0);
        assert!(is_kernel_thread(&kt));
        assert!(!matches_filter(&kt, "", None, true));

        let procs = vec![
            mk(1, 0, "init", 0.0),
            mk(2, 1, "a", 0.0),
            mk(3, 2, "b", 0.0),
            mk(4, 1, "c", 0.0),
        ];
        let mut d = descendants_of(&procs, 1);
        d.sort();
        assert_eq!(d, vec![1, 2, 3, 4]);
        assert_eq!(descendants_of(&procs, 4), vec![4]);
    }

    #[test]
    fn affinity_mask_roundtrip() {
        let cpus = vec![0usize, 1, 5, 31, 40];
        let mask = affinity_mask(&cpus);
        let hex = mask
            .iter()
            .rev()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let back = affinity_cpus(&hex);
        assert_eq!(back, cpus);
        // 单核（掩码是右侧对齐的十六进制，最高位字节在前）
        let m = affinity_mask(&[0]);
        let hex = m
            .iter()
            .rev()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        assert_eq!(affinity_cpus(&hex), vec![0]);
        assert!(affinity_cpus("").is_empty());
    }

    #[test]
    fn compare_by_all_columns_is_total() {
        // 每一列都必须能比较（不能 panic，且 desc 严格取反）
        let mut a = mk(1, 0, "a", 10.0);
        a.rss = 100;
        a.user = "u".into();
        a.cmdline = "cmd".into();
        let mut b = mk(2, 0, "b", 20.0);
        b.rss = 200;
        b.user = "v".into();
        b.cmdline = "cmd2".into();
        for col in crate::model::monitor::ProcColumn::ALL {
            let asc = compare_by(&a, &b, *col, false);
            let desc = compare_by(&a, &b, *col, true);
            assert_eq!(asc, desc.reverse(), "列 {col:?} 的 desc 不是 asc 的反向");
        }
    }

    #[test]
    fn proc_cpu_pct_uses_all_cores_as_100() {
        let hz = 100u64; // 典型 CLK_TCK
        let n = n_cpus();
        assert!(n >= 1, "逻辑核心数异常：{n}");
        // 1 秒内跑满 1 个核心 → 全机口径 = 100/n %
        let one_core = proc_cpu_pct(hz, hz, 1.0);
        let want = 100.0 / n as f32;
        assert!(
            (one_core - want).abs() < 0.01,
            "单核跑满应约 {want}%，实际 {one_core}%"
        );
        // 所有核心都跑满 → 100%
        let all = proc_cpu_pct(hz * n, hz, 1.0);
        assert!((all - 100.0).abs() < 0.01, "全核跑满应 100%，实际 {all}%");
        // dt=0 不能除出 inf
        assert!(proc_cpu_pct(hz, hz, 0.0).is_finite());
    }

    #[test]
    fn real_system_smoke() {
        // 真实读取本机 /proc（cargo test 在纯终端即可跑）
        let text = std::fs::read_to_string("/proc/stat").unwrap();
        let (t, cores, totals) = parse_stat(&text);
        assert!(t.total() > 0);
        assert!(!cores.is_empty());
        assert!(totals.btime > 0);

        let mem = parse_meminfo(&std::fs::read_to_string("/proc/meminfo").unwrap());
        assert!(mem.total > 1024 * 1024 * 1024);
        assert!(mem.used <= mem.total);

        let users = uid_names();
        assert!(users.contains_key(&0));
        let procs = collect_processes(
            &HashMap::new(),
            clock_ticks(),
            1.0,
            mem.total,
            totals.btime,
            parse_uptime(&std::fs::read_to_string("/proc/uptime").unwrap()),
            &users,
            &mut HashMap::new(),
            true,
            false,
        );
        assert!(procs.len() > 20, "进程数异常：{}", procs.len());
        let p1 = procs.iter().find(|p| p.pid == 1).unwrap();
        assert_eq!(p1.name, "systemd");
        assert_eq!(p1.user, "root");
        // pid 1 的详情
        let d = process_detail(1);
        assert!(d.status_text.contains("Name:"), "读不到 pid 1 的 status");
        // 非 root 读 /proc/1/exe、/proc/1/fd 会被拒绝（返回空），不能因此 panic
        if crate::utils::monitor::signal::is_root() {
            assert_eq!(d.exe, "/usr/lib/systemd/systemd");
            assert!(!d.fds.is_empty());
        }
        assert!(!d.affinity.is_empty(), "读不到 pid 1 的 CPU 亲和性");
        // 自己的进程一定能读到 exe
        let me = process_detail(std::process::id() as i32);
        assert!(!me.exe.is_empty(), "读不到自己的 exe");
        assert!(me.cwd.starts_with('/'));
    }

    /// GOAL 4.5：损坏的 /proc/stat 片段 → None，不 panic。
    #[test]
    fn parse_corrupt_pid_stat_returns_none() {
        assert!(parse_pid_stat("").is_none());
        assert!(parse_pid_stat("1").is_none(), "只有 pid");
        assert!(parse_pid_stat("not a stat line").is_none());
        assert!(
            parse_pid_stat("42 (no closing paren S x y").is_none(),
            "未闭合括号"
        );
        // "pid (comm)" 缺全部后续字段：宽松解析返回部分默认值的 PidRaw
        // （真实 /proc/<pid>/stat 行恒完整，消费方以完整行为前提）；
        // 关键是不 panic。
        let _ = parse_pid_stat("1 (x)").expect("缺字段行应得部分默认值而非 panic");
    }

    /// GOAL 4.5：损坏的聚合文件片段 → 默认值 / 空列表，不 panic。
    #[test]
    fn parse_corrupt_aggregates_default_without_panic() {
        let m = parse_meminfo("");
        assert_eq!(m.total, 0, "空输入给默认值");
        let m = parse_meminfo("garbage lines\nMemTotal: notanumber kB");
        assert_eq!(m.total, 0, "非法数字忽略");
        let (load, _, _) = parse_loadavg("");
        assert_eq!(load, [0.0; 3]);
        let (load, _, _) = parse_loadavg("x y z");
        assert_eq!(load, [0.0; 3], "非数字负载忽略");
        assert!(parse_net_dev("").is_empty());
        assert!(parse_net_dev("header garbage\n!!! ***").is_empty());
        assert!(parse_diskstats("").is_empty());
        assert!(parse_diskstats("!! junk !!").is_empty());
    }
}
