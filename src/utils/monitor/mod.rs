//! 系统监视器逻辑层（纯逻辑，不依赖 GTK / libadwaita / glib）。
//!
//! - [`proc`]：`/proc` 解析与进程采集
//! - [`sensors`]：hwmon / 显卡 / 电池 / 文件系统 / 网卡补充信息
//! - [`signal`]：发信号、优先级、亲和性、IO 优先级
//!
//! 采集在后台线程里按固定周期跑，采样结果通过 channel 送到界面；界面用
//! `glib::timeout_add_local` 排空 channel，只保留最新的一份快照，避免界面
//! 被采样拖慢。

pub mod proc;
pub mod sensors;
pub mod signal;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::model::monitor::{DiskStat, FsUsage, Snapshot, SysInfo};

/// 采样线程句柄：改周期 / 停止。
#[derive(Clone)]
pub struct MonitorControl {
    stop: Arc<AtomicBool>,
    interval_ms: Arc<AtomicU64>,
}

impl MonitorControl {
    /// 停止采样线程（页面销毁 / 程序退出时调用）。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// 修改采样周期（毫秒）。
    pub fn set_interval_ms(&self, ms: u64) {
        self.interval_ms.store(ms.max(200), Ordering::Relaxed);
    }

    pub fn interval_ms(&self) -> u64 {
        self.interval_ms.load(Ordering::Relaxed)
    }

    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// 分层采样开关（GOAL.md 2.1）：速率列可见 / 详情选中时才读 /proc/<pid>/io
// ---------------------------------------------------------------------------

static IO_COLUMNS: AtomicBool = AtomicBool::new(false);
static IO_DETAIL: AtomicBool = AtomicBool::new(false);

/// 读/写速率列是否可见（由列显隐开关调用）。
pub fn set_io_columns(wanted: bool) {
    IO_COLUMNS.store(wanted, Ordering::Relaxed);
}

/// 详情面板是否有选中进程（由选择变化调用；详情文本要用 io 累计值与速率）。
pub fn set_io_detail(wanted: bool) {
    IO_DETAIL.store(wanted, Ordering::Relaxed);
}

fn want_io() -> bool {
    IO_COLUMNS.load(Ordering::Relaxed) || IO_DETAIL.load(Ordering::Relaxed)
}

/// 采样线程内部的跨周期状态。
struct SamplerState {
    prev_cpu: proc::CpuJiffies,
    prev_cores: Vec<proc::CpuJiffies>,
    prev_procs: HashMap<i32, (u64, u64, u64)>,
    prev_net: Vec<proc::NetRaw>,
    prev_disks: Vec<proc::DiskRaw>,
    users: HashMap<u32, String>,
    ticks: u64,
    last: Instant,
    /// 变化慢的数据，按较长周期刷新后缓存复用
    fs: Vec<FsUsage>,
    ips: HashMap<String, (Vec<String>, Vec<String>)>,
    cpu_static: sensors::CpuStatic,
    /// pid 级缓存：cmdline/cgroup/io_class（按 starttime 失效，进程消失即清理）。
    pid_cache: HashMap<i32, proc::PidCache>,
    /// 上一轮是否读过 io（关→开的第一轮差分基准无效）。
    io_valid: bool,
    /// 差分基准的复用槽（GOAL.md 2.2：clear + swap，不每轮新建 HashMap）。
    prev_scratch: HashMap<i32, (u64, u64, u64)>,
}

impl SamplerState {
    fn new() -> Self {
        let users = proc::uid_names();
        let cpu_static = sensors::cpu_static();
        SamplerState {
            prev_cpu: proc::CpuJiffies::default(),
            prev_cores: Vec::new(),
            prev_procs: HashMap::new(),
            prev_net: Vec::new(),
            prev_disks: Vec::new(),
            users,
            ticks: 0,
            last: Instant::now(),
            fs: Vec::new(),
            ips: HashMap::new(),
            cpu_static,
            pid_cache: HashMap::new(),
            io_valid: false,
            prev_scratch: HashMap::new(),
        }
    }
}

/// 基准测试用采样器：包住内部 [`SamplerState`],让 `benches/` 能对真实
/// `/proc` 采集路径计时。隐藏 API，仅供基准测试使用。
#[doc(hidden)]
pub struct BenchSampler(SamplerState);

#[doc(hidden)]
impl Default for BenchSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl BenchSampler {
    pub fn new() -> Self {
        BenchSampler(SamplerState::new())
    }

    /// 采集一次完整快照，返回纯采集耗时（不含 criterion 计时开销的自计时）。
    pub fn collect_timed(&mut self) -> Duration {
        let t = Instant::now();
        collect(&mut self.0);
        t.elapsed()
    }
}

/// 启动采样线程，返回 (快照接收端, 控制句柄)。
///
/// 接收端每次只会拿到最新的一份快照（内部有界，满了直接丢旧数据，不会阻塞
/// 采样线程）。
pub fn start(interval_ms: u64) -> (Receiver<Snapshot>, MonitorControl) {
    let (tx, rx) = sync_channel::<Snapshot>(2);
    let control = MonitorControl {
        stop: Arc::new(AtomicBool::new(false)),
        interval_ms: Arc::new(AtomicU64::new(interval_ms.max(200))),
    };
    let ctl = control.clone();
    std::thread::Builder::new()
        .name("linbox-monitor".to_string())
        .spawn(move || run_sampler(tx, ctl))
        .expect("无法创建采样线程");
    (rx, control)
}

fn run_sampler(tx: SyncSender<Snapshot>, ctl: MonitorControl) {
    let mut state = SamplerState::new();
    while !ctl.is_stopped() {
        let started = Instant::now();
        let snap = collect(&mut state);
        match tx.try_send(snap) {
            Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
        }
        if ctl.is_stopped() {
            break;
        }
        let budget = Duration::from_millis(ctl.interval_ms());
        // 分片睡眠，保证 stop() 后能立刻退出
        while started.elapsed() < budget {
            if ctl.is_stopped() {
                return;
            }
            let left = budget.saturating_sub(started.elapsed());
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
    }
}

/// 采集一份完整快照。
fn collect(state: &mut SamplerState) -> Snapshot {
    let t0 = Instant::now();
    let dt = t0.duration_since(state.last).as_secs_f64().max(0.001);
    state.last = t0;
    state.ticks += 1;

    // ---- CPU ----
    // GOAL.md 2.2：/proc 顺序读共享一个可复用缓冲（parse_* 都返回自有数据）
    let mut text = String::with_capacity(16 * 1024);
    read_into("/proc/stat", &mut text);
    let (cur_cpu, cur_cores, totals) = proc::parse_stat(&text);
    let mut cpu = proc::cpu_stat_delta(&cur_cpu, &state.prev_cpu, &cur_cores, &state.prev_cores);
    state.prev_cpu = cur_cpu;
    state.prev_cores = cur_cores;

    // ---- 内存 / 负载 / 运行时间 ----
    read_into("/proc/meminfo", &mut text);
    let mem = proc::parse_meminfo(&text);
    read_into("/proc/loadavg", &mut text);
    let (load, running, nprocs) = proc::parse_loadavg(&text);
    read_into("/proc/uptime", &mut text);
    let uptime = proc::parse_uptime(&text);

    cpu.model = state.cpu_static.model.clone();
    cpu.cores = state.cpu_static.cores;
    cpu.threads = state.cpu_static.threads;
    cpu.freq_mhz = sensors::cpu_freq_mhz();
    cpu.freq_max_mhz = state.cpu_static.freq_max_mhz;
    cpu.temp = state.cpu_static.temp;
    cpu.load = load;
    cpu.running = running;
    cpu.procs = nprocs;
    cpu.ctxt = totals.ctxt;
    cpu.intr = totals.intr;

    // ---- 网络 ----
    read_into("/proc/net/dev", &mut text);
    let net_raw = proc::parse_net_dev(&text);
    let mut net = proc::net_delta(&net_raw, &state.prev_net, dt);
    state.prev_net = net_raw;
    sensors::enrich_net(&mut net);

    // ---- 磁盘 ----
    read_into("/proc/diskstats", &mut text);
    let disk_raw = proc::parse_diskstats(&text);
    let mut disks = proc::disk_delta(&disk_raw, &state.prev_disks, dt);
    state.prev_disks = disk_raw;
    sensors::enrich_disks(&mut disks);

    // ---- 传感器 / 显卡 / 电池 ----
    let sensors_list = sensors::collect_sensors();
    let gpus = sensors::collect_gpus();
    let batteries = sensors::collect_batteries();

    // ---- 变化慢的：文件系统（每 5 次）/ IP（每 10 次）----
    if state.fs.is_empty() || state.ticks.is_multiple_of(5) {
        state.fs = sensors::collect_fs();
    }
    if state.ips.is_empty() || state.ticks.is_multiple_of(10) {
        state.ips = sensors::collect_ips();
    }
    for i in net.iter_mut() {
        if let Some((v4, v6)) = state.ips.get(&i.name) {
            i.ipv4 = v4.clone();
            i.ipv6 = v6.clone();
        }
    }
    // 无线信号（/proc/net/wireless 很小，每轮都读）
    let wireless = sensors::collect_wireless();
    for i in net.iter_mut() {
        if let Some((dbm, link)) = wireless.get(&i.name) {
            i.signal_dbm = *dbm;
            i.link_quality = *link;
        }
    }

    // ---- 进程 ----
    let ticks_per_sec = proc::clock_ticks();
    let want_io_now = want_io();
    let mut processes = proc::collect_processes(
        &state.prev_procs,
        ticks_per_sec,
        dt,
        mem.total,
        totals.btime,
        uptime,
        &state.users,
        &mut state.pid_cache,
        want_io_now,
        state.io_valid,
    );
    state.io_valid = want_io_now;
    // 更新差分基准（缓冲复用：clear + swap，不每轮新建 HashMap）
    state.prev_scratch.clear();
    for p in processes.iter_mut() {
        state
            .prev_scratch
            .insert(p.pid, (p.cpu_ticks(), p.read_bytes, p.write_bytes));
    }
    std::mem::swap(&mut state.prev_procs, &mut state.prev_scratch);
    // 清掉已退出进程的缓存
    state
        .pid_cache
        .retain(|pid, _| state.prev_procs.contains_key(pid));
    let threads: u64 = processes.iter().map(|p| p.threads.max(0) as u64).sum();

    let sys = SysInfo {
        hostname: hostname_cached().to_string(),
        kernel: kernel_cached().to_string(),
        distro: distro_cached().to_string(),
        uptime,
        procs: processes.len() as u64,
        threads,
    };

    Snapshot {
        cpu,
        mem,
        net,
        disks,
        fs: state.fs.clone(),
        sensors: sensors_list,
        gpus,
        batteries,
        sys,
        processes,
        cost_ms: t0.elapsed().as_millis() as u64,
        at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    }
}

fn read_trim(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// 把文件读进可复用缓冲（GOAL.md 2.2）：失败时 buf 为空。
/// 调用方必须在下一次读之前完成解析（parse 返回的都是自有数据）。
fn read_into(path: &str, buf: &mut String) {
    use std::io::Read as _;
    buf.clear();
    if let Ok(mut f) = std::fs::File::open(path) {
        let _ = f.read_to_string(buf);
    }
}

/// 主机名（不变，缓存避免每轮读盘）。
fn hostname_cached() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| read_trim("/proc/sys/kernel/hostname"))
}

/// 内核版本（不变，缓存）。
fn kernel_cached() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| read_trim("/proc/sys/kernel/osrelease"))
}

/// 发行版（不变，缓存）。
fn distro_cached() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(distro_name)
}

/// `/etc/os-release` 的 PRETTY_NAME。
fn distro_name() -> String {
    let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("PRETTY_NAME=") {
            return v.trim().trim_matches('"').to_string();
        }
    }
    "Linux".to_string()
}

// ---------------------------------------------------------------------------
// 格式化工具（界面与自检共用）
// ---------------------------------------------------------------------------

/// 字节数 → 人类可读（1024 进制）。
pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// 速率（字节/秒）→ 人类可读。
pub fn human_rate(bps: f64) -> String {
    if bps < 0.05 {
        return "0 B/s".to_string();
    }
    format!("{}/s", human_bytes(bps as u64))
}

/// 秒数 → 人类可读时长。
pub fn human_duration(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs} 秒");
    }
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if d > 0 {
        if h > 0 {
            format!("{d} 天 {h} 小时")
        } else {
            format!("{d} 天")
        }
    } else if h > 0 {
        if m > 0 {
            format!("{h} 小时 {m} 分")
        } else {
            format!("{h} 小时")
        }
    } else if m > 0 {
        if s > 0 && m < 10 {
            format!("{m} 分 {s} 秒")
        } else {
            format!("{m} 分")
        }
    } else {
        format!("{s} 秒")
    }
}

/// 时间戳（unix 秒）→ `2026-09-15 08:30:12`。
pub fn human_time(ts: u64) -> String {
    // 不引入 chrono：用本地时区的简单换算（依赖 TZ 偏移从 /etc/localtime 取不方便，
    // 这里用 `date` 之外的纯算法：civil_from_days(UTC) + 本地偏移由 libc localtime 提供）
    unsafe extern "C" {
        fn localtime_r(time: *const i64, tm: *mut Tm) -> *mut Tm;
    }
    #[repr(C)]
    struct Tm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        tm_gmtoff: i64,
        tm_zone: *const std::os::raw::c_char,
    }
    let t = ts as i64;
    let mut tm = Tm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
        tm_gmtoff: 0,
        tm_zone: std::ptr::null(),
    };
    let r = unsafe { localtime_r(&t, &mut tm) };
    if r.is_null() {
        return ts.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// 百分比文本。
pub fn pct(v: f32) -> String {
    if v >= 99.95 {
        "100%".to_string()
    } else {
        format!("{v:.1}%")
    }
}

/// 百分比文本（整数，列表里更紧凑）。
pub fn pct0(v: f32) -> String {
    format!("{:.0}%", v)
}

/// 温度文本。
pub fn temp_text(v: Option<f32>) -> String {
    match v {
        Some(t) => format!("{t:.0}°C"),
        None => "—".to_string(),
    }
}

/// 根据占用率给出 GTK 的 CSS class（用于进度条着色）。
pub fn level_class(pct: f32) -> &'static str {
    if pct >= 90.0 {
        "error"
    } else if pct >= 70.0 {
        "warning"
    } else {
        "success"
    }
}

/// 一次采样里 CPU 时间的 jiffies 还原（与 [`CpuTicks`] 同样的算法）。
impl crate::model::monitor::Process {
    pub fn cpu_ticks(&self) -> u64 {
        (self.cpu_time * proc::clock_ticks() as f64) as u64
    }
}

/// 诊断用：直接跑一次采集（自检 / 单测用，不启线程）。
pub fn sample_once() -> Snapshot {
    let mut st = SamplerState::new();
    // 第一次采样差分没有基准，先跑一次丢弃，再跑一次得到有意义的数值
    let _ = collect(&mut st);
    std::thread::sleep(Duration::from_millis(220));
    collect(&mut st)
}

/// 千分位分隔（进程数、包数这类大数字用）。
pub fn fmt_thousands(v: u64) -> String {
    let s = v.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*c as char);
    }
    out
}

/// 从 cgroup 路径里提取「应用名」：`…/app-steam@autostart.service` → `steam`。
pub fn cgroup_app(path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or("");
    if last.is_empty() {
        return String::new();
    }
    // 去掉 .service / .scope 后缀与 @后缀
    let name = last
        .split('@')
        .next()
        .unwrap_or("")
        .trim_end_matches(".service")
        .trim_end_matches(".scope");
    let name = name
        .strip_prefix("app-")
        .or_else(|| name.strip_prefix("libreoffice-"))
        .unwrap_or(name);
    name.to_string()
}

/// 显卡摘要（复制系统概况用）。
pub fn gpu_summary_text(g: &crate::model::monitor::GpuStat) -> String {
    let mut parts = vec![format!("{}（{} · {}）", g.name, g.vendor, g.card)];
    if let Some(b) = g.busy {
        parts.push(format!("占用 {b:.1}%"));
    }
    if let Some(u) = g.mem_used {
        match g.mem_total {
            Some(t) if t > 0 => parts.push(format!(
                "显存 {} / {}（{:.0}%）",
                human_bytes(u),
                human_bytes(t),
                100.0 * u as f64 / t as f64
            )),
            _ => parts.push(format!("显存 {}", human_bytes(u))),
        }
    }
    if let Some(t) = g.temp {
        parts.push(format!("温度 {t:.0}°C"));
    }
    if let Some(t) = g.temp_junction {
        parts.push(format!("结温 {t:.0}°C"));
    }
    if let Some(p) = g.power {
        parts.push(format!("功耗 {p:.0}W"));
    }
    if let Some(a) = g.sclk_mhz {
        parts.push(format!("核心 {a:.0} MHz"));
    }
    if let Some(f) = g.fan {
        parts.push(format!("风扇 {f:.0} RPM"));
    }
    parts.join(" · ")
}

/// 磁盘总量（用于概览页汇总）。
pub fn disk_total_io(disks: &[DiskStat]) -> (f64, f64) {
    let r = disks.iter().map(|d| d.read_bytes_s).sum();
    let w = disks.iter().map(|d| d.write_bytes_s).sum();
    (r, w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::RecvTimeoutError;

    #[test]
    fn human_bytes_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(1024 * 1024 * 3 / 2), "1.5 MB");
        assert_eq!(human_bytes(32_784_652 * 1024), "31.3 GB");
        // 大数值不带小数
        assert_eq!(human_bytes(200 * 1024 * 1024 * 1024), "200 GB");
    }

    #[test]
    fn human_rate_and_duration() {
        assert_eq!(human_rate(0.0), "0 B/s");
        assert_eq!(human_rate(2048.0), "2.0 KB/s");
        assert_eq!(human_duration(0), "0 秒");
        assert_eq!(human_duration(45), "45 秒");
        assert_eq!(human_duration(60), "1 分");
        assert_eq!(human_duration(125), "2 分 5 秒");
        assert_eq!(human_duration(3600), "1 小时");
        assert_eq!(human_duration(3700), "1 小时 1 分");
        assert_eq!(human_duration(90000), "1 天 1 小时");
        assert_eq!(human_duration(172800), "2 天");
    }

    #[test]
    fn pct_and_level() {
        assert_eq!(pct(0.0), "0.0%");
        assert_eq!(pct(99.999), "100%");
        assert_eq!(pct0(42.6), "43%");
        assert_eq!(level_class(10.0), "success");
        assert_eq!(level_class(75.0), "warning");
        assert_eq!(level_class(95.0), "error");
    }

    #[test]
    fn human_time_formats() {
        // 2026-09-15 00:00:00 UTC 附近，只校验格式与年份
        let s = human_time(1_789_432_800);
        assert_eq!(s.len(), 19, "{s}");
        assert!(s.starts_with("2026-"), "{s}");
        assert_eq!(&s[10..11], " ");
    }

    #[test]
    fn sampler_thread_works_and_stops() {
        let (rx, ctl) = start(200);
        let snap = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("3 秒内没拿到快照");
        assert!(snap.cpu.threads > 0);
        assert!(snap.mem.total > 0);
        assert!(!snap.processes.is_empty());
        assert!(!snap.gpus.is_empty());
        assert!(!snap.sensors.is_empty());
        assert!(!snap.fs.is_empty());
        ctl.stop();
        // 停止后不应再收到新快照
        let mut got = 0;
        while let Ok(_s) = rx.recv_timeout(std::time::Duration::from_millis(400)) {
            got += 1;
            if got > 2 {
                break;
            }
        }
        assert!(got <= 2, "stop() 之后采样线程没有停下");
        assert!(ctl.is_stopped());
    }

    #[test]
    fn sample_once_real_values() {
        let s = sample_once();
        // 环境自适应断言（GOAL.md 4.6）：只断言真实硬件上恒成立的不变量，
        // 依赖具体平台能力的（cpufreq sysfs 等）按「有该能力才断言」处理。
        assert!(s.cpu.cores >= 4);
        assert!(s.cpu.threads >= s.cpu.cores);
        assert_eq!(s.cpu.per_core.len(), s.cpu.threads);
        assert!(s.cpu.per_core.iter().all(|v| (0.0..=100.0).contains(v)));
        if std::path::Path::new("/sys/devices/system/cpu/cpu0/cpufreq").exists() {
            assert!(s.cpu.freq_mhz > 0.0, "有 cpufreq 却读不到当前主频");
        }
        assert!(s.mem.total > 1024 * 1024 * 1024);
        assert!(s.sys.uptime > 0);
        assert!(s.sys.procs > 50);
        assert!(s.sys.threads >= s.sys.procs);
        assert!(!s.sys.kernel.is_empty());
        assert!(!s.sys.distro.is_empty());
        assert!(s.processes.iter().any(|p| p.pid == 1));
        // 进程 CPU% 汇总（全机口径）不该超过 100% 太多；差分算错（漏除核数）会到几千
        let total: f32 = s.processes.iter().map(|p| p.cpu).sum();
        assert!(
            total <= 150.0,
            "进程 CPU 汇总异常：{total}（全机口径应约等于系统总占用，≤100 量级）"
        );
        // 至少有一个进程抓到 CPU 时间
        assert!(s.processes.iter().any(|p| p.cpu_time > 0.0));
        // 网络汇总字段
        assert!(!s.net.is_empty());
        assert!(!s.disks.is_empty());
        let (r, w) = disk_total_io(&s.disks);
        assert!(r >= 0.0 && w >= 0.0);
        let _ = RecvTimeoutError::Timeout;
    }

    #[test]
    fn human_bytes_unit_boundaries() {
        assert_eq!(human_bytes(1023), "1023 B", "未满 1KB 不缩放");
        assert_eq!(
            human_bytes(1024 * 1024 - 1),
            "1024 KB",
            "KB 上探到 1024 不进位的旧行为"
        );
        assert_eq!(human_bytes(1024 * 1024), "1.0 MB", "正好 1MB");
        assert_eq!(human_bytes(1024u64.pow(4)), "1.0 TB");
        assert_eq!(human_bytes(1024u64.pow(5)), "1.0 PB");
        assert!(
            human_bytes(u64::MAX).ends_with(" PB"),
            "封顶到 PB 不溢出：{}",
            human_bytes(u64::MAX)
        );
    }

    #[test]
    fn human_rate_small_and_negative() {
        assert_eq!(human_rate(0.04), "0 B/s", "小于阈值归零");
        assert_eq!(human_rate(-100.0), "0 B/s", "负数（采样回绕）不得出现 -xxx");
        assert_eq!(human_rate(1.0), "1 B/s");
        assert_eq!(human_rate(1536.0), "1.5 KB/s");
    }

    #[test]
    fn human_duration_minute_display_rules() {
        // 分钟段只在 <10 分且有秒时才显示「x 分 y 秒」
        assert_eq!(human_duration(59), "59 秒");
        assert_eq!(human_duration(599), "9 分 59 秒");
        assert_eq!(human_duration(600), "10 分", "满 10 分不再带秒");
        assert_eq!(human_duration(90), "1 分 30 秒");
        assert_eq!(human_duration(86400 + 3600), "1 天 1 小时");
        assert_eq!(human_duration(3600 * 5), "5 小时");
    }

    #[test]
    fn level_class_thresholds() {
        assert_eq!(level_class(69.99), "success");
        assert_eq!(level_class(70.0), "warning");
        assert_eq!(level_class(89.99), "warning");
        assert_eq!(level_class(90.0), "error");
        assert_eq!(level_class(0.0), "success");
    }

    #[test]
    fn pct_formatting_edges() {
        assert_eq!(pct(42.5), "42.5%");
        assert_eq!(pct(-3.25), "-3.2%", "负值（理论不出现）走同格式不 panic");
        assert_eq!(pct(99.95), "100%", "封顶边界");
        assert_eq!(pct(99.9), "99.9%");
        assert_eq!(pct0(99.6), "100%");
        assert_eq!(pct0(0.0), "0%");
    }

    #[test]
    fn temp_text_some_and_none() {
        assert_eq!(temp_text(Some(45.6)), "46°C");
        assert_eq!(temp_text(Some(0.0)), "0°C");
        assert_eq!(temp_text(None), "—", "读不到温度显示破折号");
    }

    #[test]
    fn monitor_control_interval_floor() {
        let (rx, ctl) = start(1);
        assert!(ctl.interval_ms() >= 200, "周期下限 200ms");
        ctl.set_interval_ms(10);
        assert!(ctl.interval_ms() >= 200, "set 同样被下限钳制");
        ctl.stop();
        drop(rx);
        assert!(ctl.is_stopped());
    }
}
