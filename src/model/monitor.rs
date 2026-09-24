//! 系统监视器的数据模型（纯数据，无 UI 依赖）。
//!
//! 一次采样得到一份 [`Snapshot`]，界面按需取用其中的各个部分。

/// CPU 总体与每核占用。
#[derive(Debug, Clone, Default)]
pub struct CpuStat {
    /// 型号名（/proc/cpuinfo）。
    pub model: String,
    /// 物理核心数（去重后的 core id）。
    pub cores: usize,
    /// 逻辑线程数。
    pub threads: usize,
    /// 总体占用 0..100。
    pub total: f32,
    /// 每核（每逻辑线程）占用 0..100。
    pub per_core: Vec<f32>,
    /// 当前主频 MHz（各核平均，读不到为 0）。
    pub freq_mhz: f32,
    /// 最大主频 MHz。
    pub freq_max_mhz: f32,
    /// CPU 温度（coretemp/k10temp 的 Tctl/Package，读不到为 None）。
    pub temp: Option<f32>,
    /// 平均负载（1/5/15 分钟）。
    pub load: [f32; 3],
    /// 运行队列中的进程数 / 总进程数（/proc/loadavg 后两段）。
    pub running: u64,
    pub procs: u64,
    /// 自启动以来上下文切换 / 中断次数。
    pub ctxt: u64,
    pub intr: u64,
    /// 中断里被服务过的软中断等（/proc/stat 其余字段原样保留）。
    pub steal: f32,
    pub nice: f32,
    pub iowait: f32,
    pub sys: f32,
    pub user: f32,
    pub idle: f32,
}

/// 内存与交换分区。
#[derive(Debug, Clone, Default)]
pub struct MemStat {
    pub total: u64,
    pub used: u64,
    pub free: u64,
    pub available: u64,
    pub buffers: u64,
    pub cached: u64,
    pub shared: u64,
    pub dirty: u64,
    pub slab: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

impl NetIface {
    /// 错误 / 丢包的简述（没有问题时返回空串）。
    pub fn errors(&self) -> String {
        let mut v = Vec::new();
        if self.rx_errs + self.tx_errs > 0 {
            v.push(format!("错误 {} / {}", self.rx_errs, self.tx_errs));
        }
        if self.rx_drop + self.tx_drop > 0 {
            v.push(format!("丢包 {} / {}", self.rx_drop, self.tx_drop));
        }
        v.join(" · ")
    }
}

impl MemStat {
    /// 已用比例 0..1。
    pub fn used_ratio(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.used as f32 / self.total as f32
        }
    }

    pub fn swap_ratio(&self) -> f32 {
        if self.swap_total == 0 {
            0.0
        } else {
            self.swap_used as f32 / self.swap_total as f32
        }
    }
}

/// 一张网卡。
#[derive(Debug, Clone, Default)]
pub struct NetIface {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    /// 每秒收发字节（两次采样差分）。
    pub rx_rate: f64,
    pub tx_rate: f64,
    /// 每秒包数。
    pub rx_pps: f64,
    pub tx_pps: f64,
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_errs: u64,
    pub tx_errs: u64,
    pub rx_drop: u64,
    pub tx_drop: u64,
    /// up / down / unknown 等。
    pub operstate: String,
    /// 链路速率（如 `1000 Mb/s`，读不到为空）。
    pub speed: String,
    pub mac: String,
    pub mtu: String,
    pub ipv4: Vec<String>,
    pub ipv6: Vec<String>,
    /// 是否是回环（界面可折叠）。
    pub is_loopback: bool,
    /// 无线信号强度（dBm，来自 /proc/net/wireless）。
    pub signal_dbm: Option<f32>,
    /// 无线链路质量（原始值，通常 0..70，驱动相关）。
    pub link_quality: Option<f32>,
}

impl NetIface {
    /// 是否是无线网卡（有链路信息）。
    pub fn is_wireless(&self) -> bool {
        self.signal_dbm.is_some() || self.link_quality.is_some()
    }

    /// 信号强度文本（非无线返回空）。
    pub fn signal_text(&self) -> String {
        match (self.signal_dbm, self.link_quality) {
            (Some(d), Some(q)) => format!("信号 {d:.0} dBm · 链路质量 {q:.0}"),
            (Some(d), None) => format!("信号 {d:.0} dBm"),
            (None, Some(q)) => format!("链路质量 {q:.0}"),
            (None, None) => String::new(),
        }
    }
}

/// 一块磁盘。
#[derive(Debug, Clone, Default)]
pub struct DiskStat {
    pub name: String,
    pub model: String,
    /// 容量（字节，读不到为 0）。
    pub size: u64,
    /// 是否机械盘。
    pub rotational: bool,
    /// 每秒读写字节 / 次数。
    pub read_bytes_s: f64,
    pub write_bytes_s: f64,
    pub read_iops: f64,
    pub write_iops: f64,
    /// 设备忙碌时间占比 0..100。
    pub util: f32,
    /// 平均每次 IO 耗时（毫秒）。
    pub await_ms: f64,
    /// 该磁盘的分区名。
    pub partitions: Vec<String>,
}

/// 一个挂载点的使用率。
#[derive(Debug, Clone, Default)]
pub struct FsUsage {
    pub mount: String,
    pub device: String,
    pub fstype: String,
    pub total: u64,
    pub used: u64,
    pub avail: u64,
    pub use_pct: f32,
}

/// 传感器类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorKind {
    Temp,
    Fan,
    Voltage,
    Power,
    Current,
    Freq,
}

impl SensorKind {
    pub fn label(&self) -> &'static str {
        match self {
            SensorKind::Temp => "温度",
            SensorKind::Fan => "风扇",
            SensorKind::Voltage => "电压",
            SensorKind::Power => "功耗",
            SensorKind::Current => "电流",
            SensorKind::Freq => "频率",
        }
    }

    /// 单位后缀。
    pub fn unit(&self) -> &'static str {
        match self {
            SensorKind::Temp => "°C",
            SensorKind::Fan => "RPM",
            SensorKind::Voltage => "V",
            SensorKind::Power => "W",
            SensorKind::Current => "A",
            SensorKind::Freq => "MHz",
        }
    }
}

/// 一个传感器读数（hwmon / thermal）。
#[derive(Debug, Clone)]
pub struct Sensor {
    /// 芯片名，如 `k10temp` / `amdgpu` / `gigabyte_wmi`。
    pub chip: String,
    /// 通道名（label），如 `Tctl` / `edge` / `fan1`。
    pub label: String,
    pub kind: SensorKind,
    /// 已换算成单位的数值。
    pub value: f32,
    /// 上限/临界值（用于进度条与告警着色）。
    pub max: Option<f32>,
    pub crit: Option<f32>,
    /// 温度是否支持（thermal zone 用）。
    pub raw_input: String,
}

/// 一张显卡的状态。
#[derive(Debug, Clone, Default)]
pub struct GpuStat {
    pub card: String,
    pub name: String,
    pub vendor: String,
    pub driver: String,
    /// 3D 引擎占用 0..100。
    pub busy: Option<f32>,
    /// 显存占用 0..100（显存控制器忙碌度）。
    pub mem_busy: Option<f32>,
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
    /// GTT（系统内存映射）用量。
    pub gtt_used: Option<u64>,
    pub temp: Option<f32>,
    pub temp_junction: Option<f32>,
    pub temp_mem: Option<f32>,
    pub power: Option<f32>,
    pub power_cap: Option<f32>,
    pub fan: Option<f32>,
    pub fan_max: Option<f32>,
    pub sclk_mhz: Option<f32>,
    pub mclk_mhz: Option<f32>,
}

/// 电池。
#[derive(Debug, Clone, Default)]
pub struct Battery {
    pub name: String,
    /// 剩余百分比 0..100。
    pub capacity: f32,
    /// Charging / Discharging / Full …
    pub status: String,
    pub energy_now: u64,
    pub energy_full: u64,
    /// 当前功率（瓦）。
    pub power: f64,
    /// 预计可用时间（秒，0 表示未知）。
    pub time_left: u64,
}

/// 系统概况。
#[derive(Debug, Clone, Default)]
pub struct SysInfo {
    pub hostname: String,
    pub kernel: String,
    pub distro: String,
    /// 开机至今秒数。
    pub uptime: u64,
    /// 总进程数 / 线程数。
    pub procs: u64,
    pub threads: u64,
}

/// 一个进程（概要，用于列表）。
#[derive(Debug, Clone, Default)]
pub struct Process {
    pub pid: i32,
    pub ppid: i32,
    /// comm（/proc/pid/stat 的括号内名字）。
    pub name: String,
    /// 完整命令行（可空，内核线程为空）。
    pub cmdline: String,
    /// 可执行文件路径（/proc/pid/exe，权限不足为空）。
    pub exe: String,
    pub uid: u32,
    pub user: String,
    /// R/D/S/T/Z/I 等。
    pub state: String,
    /// CPU 占用 0..100，按「全部逻辑核心 = 100%」归一化（与系统监视器一致；
    /// 例如 32 线程机器上把 1 个核心跑满 ≈ 3.1%）。
    pub cpu: f32,
    /// 内存占比 0..100。
    pub mem_pct: f32,
    /// 常驻内存字节。
    pub rss: u64,
    pub virt: u64,
    pub swap: u64,
    pub threads: i64,
    pub nice: i64,
    pub priority: i64,
    /// 调度策略：OTHER / FIFO / RR / BATCH / IDLE。
    pub policy: String,
    /// IO 调度类：none / realtime / best-effort / idle。
    pub io_class: String,
    /// 累计 CPU 时间（秒）。
    pub cpu_time: f64,
    /// 已运行秒数。
    pub uptime: u64,
    /// 启动时刻（unix 秒，btime + starttime/ticks）。
    pub started_at: u64,
    /// 累计读写字节。
    pub read_bytes: u64,
    pub write_bytes: u64,
    /// 每秒读写字节。
    pub read_rate: f64,
    pub write_rate: f64,
    pub minflt: u64,
    pub majflt: u64,
    /// 线程状态（S 等）与是否占用 CPU。
    pub on_cpu: i32,
    /// 进程组 / 会话（用于整组发信号）。
    pub pgid: i32,
    pub sid: i32,
    /// 允许运行的 CPU 掩码（十六进制字符串，空表示读不到）。
    pub affinity: String,
    /// 树形展示用：缩进层级、子进程数；折叠状态在页面里维护。
    pub depth: usize,
    pub child_count: usize,
    /// 所属 cgroup 路径（用于「应用程序」分组，取自 /proc/pid/cgroup 的第一条）。
    pub cgroup: String,
    /// 进程中所有线程的 tid（详情面板用，按需取）。
    pub state_char: char,
}

impl Process {
    /// 显示用名字：优先 cmdline 的第一个词，否则 comm。
    pub fn display_name(&self) -> String {
        if self.name.is_empty() {
            self.cmdline
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string()
        } else {
            self.name.clone()
        }
    }

    /// 是否已僵死。
    pub fn is_zombie(&self) -> bool {
        self.state_char == 'Z'
    }
}

/// 进程详情的附加信息（按需读取，不参与每秒采样）。
#[derive(Debug, Clone, Default)]
pub struct ProcessDetail {
    pub pid: i32,
    pub exe: String,
    pub cwd: String,
    pub root: String,
    /// 环境变量（KEY=VALUE）。
    pub env: Vec<String>,
    /// 打开的文件描述符（目标是哪个文件/套接字）。
    pub fds: Vec<String>,
    /// 线程列表 (tid, 状态, 名称)。
    pub thread_list: Vec<(i32, char, String)>,
    /// 所属用户组。
    pub groups: Vec<u32>,
    /// CPU 亲和性掩码（十六进制）。
    pub affinity: String,
    /// 内存明细（KB）。
    pub vm_peak: u64,
    pub vm_size: u64,
    pub vm_rss: u64,
    pub vm_data: u64,
    pub vm_stk: u64,
    pub vm_exe: u64,
    pub vm_lib: u64,
    pub vm_swap: u64,
    /// 命名空间链接。
    pub namespaces: Vec<String>,
    /// /proc/pid/status 的原始文本（供复制）。
    pub status_text: String,
    /// cgroup 路径。
    pub cgroup: String,
    /// 累计读写的原始计数。
    pub read_bytes: u64,
    pub write_bytes: u64,
}

/// 一个进程信号的规格。
#[derive(Debug, Clone, Copy)]
pub struct SigSpec {
    pub num: i32,
    pub name: &'static str,
    pub desc: &'static str,
}

/// Linux 全部信号（1..=31 标准 + 34..=64 实时）。
pub const SIGNALS: &[SigSpec] = &[
    SigSpec {
        num: 1,
        name: "SIGHUP",
        desc: "挂起：多数守护进程收到后重读配置文件",
    },
    SigSpec {
        num: 2,
        name: "SIGINT",
        desc: "中断：等价于 Ctrl+C",
    },
    SigSpec {
        num: 3,
        name: "SIGQUIT",
        desc: "退出并生成 core dump（Ctrl+\\）",
    },
    SigSpec {
        num: 4,
        name: "SIGILL",
        desc: "非法指令",
    },
    SigSpec {
        num: 5,
        name: "SIGTRAP",
        desc: "断点/陷阱",
    },
    SigSpec {
        num: 6,
        name: "SIGABRT",
        desc: "异常终止（abort）",
    },
    SigSpec {
        num: 7,
        name: "SIGBUS",
        desc: "总线错误",
    },
    SigSpec {
        num: 8,
        name: "SIGFPE",
        desc: "算术错误",
    },
    SigSpec {
        num: 9,
        name: "SIGKILL",
        desc: "强杀：不可捕获，进程立即结束",
    },
    SigSpec {
        num: 10,
        name: "SIGUSR1",
        desc: "用户自定义信号 1",
    },
    SigSpec {
        num: 11,
        name: "SIGSEGV",
        desc: "段错误",
    },
    SigSpec {
        num: 12,
        name: "SIGUSR2",
        desc: "用户自定义信号 2",
    },
    SigSpec {
        num: 13,
        name: "SIGPIPE",
        desc: "写入已关闭的管道",
    },
    SigSpec {
        num: 14,
        name: "SIGALRM",
        desc: "定时器到期",
    },
    SigSpec {
        num: 15,
        name: "SIGTERM",
        desc: "终止：可捕获，进程能优雅退出",
    },
    SigSpec {
        num: 16,
        name: "SIGSTKFLT",
        desc: "协处理器栈错误",
    },
    SigSpec {
        num: 17,
        name: "SIGCHLD",
        desc: "子进程状态改变",
    },
    SigSpec {
        num: 18,
        name: "SIGCONT",
        desc: "继续：恢复被暂停的进程",
    },
    SigSpec {
        num: 19,
        name: "SIGSTOP",
        desc: "暂停：不可捕获（冻结进程）",
    },
    SigSpec {
        num: 20,
        name: "SIGTSTP",
        desc: "终端挂起（Ctrl+Z）",
    },
    SigSpec {
        num: 21,
        name: "SIGTTIN",
        desc: "后台进程读终端",
    },
    SigSpec {
        num: 22,
        name: "SIGTTOU",
        desc: "后台进程写终端",
    },
    SigSpec {
        num: 23,
        name: "SIGURG",
        desc: "套接字紧急数据",
    },
    SigSpec {
        num: 24,
        name: "SIGXCPU",
        desc: "超出 CPU 时间上限",
    },
    SigSpec {
        num: 25,
        name: "SIGXFSZ",
        desc: "超出文件大小上限",
    },
    SigSpec {
        num: 26,
        name: "SIGVTALRM",
        desc: "虚拟定时器到期",
    },
    SigSpec {
        num: 27,
        name: "SIGPROF",
        desc: "性能分析定时器到期",
    },
    SigSpec {
        num: 28,
        name: "SIGWINCH",
        desc: "终端窗口大小改变",
    },
    SigSpec {
        num: 29,
        name: "SIGIO",
        desc: "异步 IO 事件",
    },
    SigSpec {
        num: 30,
        name: "SIGPWR",
        desc: "电源失效",
    },
    SigSpec {
        num: 31,
        name: "SIGSYS",
        desc: "非法系统调用",
    },
    SigSpec {
        num: 34,
        name: "SIGRTMIN",
        desc: "实时信号 34（RTMIN）",
    },
    SigSpec {
        num: 35,
        name: "SIGRTMIN+1",
        desc: "实时信号 35",
    },
    SigSpec {
        num: 36,
        name: "SIGRTMIN+2",
        desc: "实时信号 36",
    },
    SigSpec {
        num: 37,
        name: "SIGRTMIN+3",
        desc: "实时信号 37",
    },
    SigSpec {
        num: 38,
        name: "SIGRTMIN+4",
        desc: "实时信号 38",
    },
    SigSpec {
        num: 39,
        name: "SIGRTMIN+5",
        desc: "实时信号 39",
    },
    SigSpec {
        num: 40,
        name: "SIGRTMIN+6",
        desc: "实时信号 40",
    },
    SigSpec {
        num: 41,
        name: "SIGRTMIN+7",
        desc: "实时信号 41",
    },
    SigSpec {
        num: 42,
        name: "SIGRTMIN+8",
        desc: "实时信号 42",
    },
    SigSpec {
        num: 43,
        name: "SIGRTMIN+9",
        desc: "实时信号 43",
    },
    SigSpec {
        num: 44,
        name: "SIGRTMIN+10",
        desc: "实时信号 44",
    },
    SigSpec {
        num: 45,
        name: "SIGRTMIN+11",
        desc: "实时信号 45",
    },
    SigSpec {
        num: 46,
        name: "SIGRTMIN+12",
        desc: "实时信号 46",
    },
    SigSpec {
        num: 47,
        name: "SIGRTMIN+13",
        desc: "实时信号 47",
    },
    SigSpec {
        num: 48,
        name: "SIGRTMIN+14",
        desc: "实时信号 48",
    },
    SigSpec {
        num: 49,
        name: "SIGRTMIN+15",
        desc: "实时信号 49",
    },
    SigSpec {
        num: 50,
        name: "SIGRTMAX-14",
        desc: "实时信号 50",
    },
    SigSpec {
        num: 51,
        name: "SIGRTMAX-13",
        desc: "实时信号 51",
    },
    SigSpec {
        num: 52,
        name: "SIGRTMAX-12",
        desc: "实时信号 52",
    },
    SigSpec {
        num: 53,
        name: "SIGRTMAX-11",
        desc: "实时信号 53",
    },
    SigSpec {
        num: 54,
        name: "SIGRTMAX-10",
        desc: "实时信号 54",
    },
    SigSpec {
        num: 55,
        name: "SIGRTMAX-9",
        desc: "实时信号 55",
    },
    SigSpec {
        num: 56,
        name: "SIGRTMAX-8",
        desc: "实时信号 56",
    },
    SigSpec {
        num: 57,
        name: "SIGRTMAX-7",
        desc: "实时信号 57",
    },
    SigSpec {
        num: 58,
        name: "SIGRTMAX-6",
        desc: "实时信号 58",
    },
    SigSpec {
        num: 59,
        name: "SIGRTMAX-5",
        desc: "实时信号 59",
    },
    SigSpec {
        num: 60,
        name: "SIGRTMAX-4",
        desc: "实时信号 60",
    },
    SigSpec {
        num: 61,
        name: "SIGRTMAX-3",
        desc: "实时信号 61",
    },
    SigSpec {
        num: 62,
        name: "SIGRTMAX-2",
        desc: "实时信号 62",
    },
    SigSpec {
        num: 63,
        name: "SIGRTMAX-1",
        desc: "实时信号 63",
    },
    SigSpec {
        num: 64,
        name: "SIGRTMAX",
        desc: "实时信号 64（RTMAX）",
    },
];

/// 按编号查信号。
pub fn signal_by_num(num: i32) -> Option<&'static SigSpec> {
    SIGNALS.iter().find(|s| s.num == num)
}

/// 进程列表的列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcColumn {
    Name,
    Pid,
    User,
    State,
    Cpu,
    Mem,
    MemPct,
    Threads,
    Priority,
    Nice,
    Policy,
    Io,
    ReadRate,
    WriteRate,
    CpuTime,
    Started,
    Command,
    Cgroup,
}

impl ProcColumn {
    pub fn label(&self) -> &'static str {
        match self {
            ProcColumn::Name => "名称",
            ProcColumn::Pid => "PID",
            ProcColumn::User => "用户",
            ProcColumn::State => "状态",
            ProcColumn::Cpu => "CPU%",
            ProcColumn::Mem => "内存",
            ProcColumn::MemPct => "内存%",
            ProcColumn::Threads => "线程",
            ProcColumn::Priority => "优先级",
            ProcColumn::Nice => "Nice",
            ProcColumn::Policy => "调度",
            ProcColumn::Io => "IO 类",
            ProcColumn::ReadRate => "读速率",
            ProcColumn::WriteRate => "写速率",
            ProcColumn::CpuTime => "CPU 时间",
            ProcColumn::Started => "运行时长",
            ProcColumn::Command => "命令行",
            ProcColumn::Cgroup => "应用/cgroup",
        }
    }

    /// 所有列（界面按这个顺序建表）。
    pub const ALL: &'static [ProcColumn] = &[
        ProcColumn::Name,
        ProcColumn::Pid,
        ProcColumn::User,
        ProcColumn::State,
        ProcColumn::Cpu,
        ProcColumn::Mem,
        ProcColumn::MemPct,
        ProcColumn::Threads,
        ProcColumn::Priority,
        ProcColumn::Nice,
        ProcColumn::Policy,
        ProcColumn::Io,
        ProcColumn::ReadRate,
        ProcColumn::WriteRate,
        ProcColumn::CpuTime,
        ProcColumn::Started,
        ProcColumn::Cgroup,
        ProcColumn::Command,
    ];

    /// 默认显示的列（对齐 Plasma 的默认视图，再去掉几个冷门的）。
    pub const DEFAULT_VISIBLE: &'static [ProcColumn] = &[
        ProcColumn::Name,
        ProcColumn::Pid,
        ProcColumn::User,
        ProcColumn::State,
        ProcColumn::Cpu,
        ProcColumn::Mem,
        ProcColumn::MemPct,
        ProcColumn::Threads,
        ProcColumn::Priority,
        ProcColumn::Started,
        ProcColumn::Command,
    ];

    /// 数值列（排序时按数字比较，界面右对齐）。
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            ProcColumn::Pid
                | ProcColumn::Cpu
                | ProcColumn::Mem
                | ProcColumn::MemPct
                | ProcColumn::Threads
                | ProcColumn::Priority
                | ProcColumn::Nice
                | ProcColumn::ReadRate
                | ProcColumn::WriteRate
                | ProcColumn::CpuTime
                | ProcColumn::Started
        )
    }

    /// 默认列宽。
    pub fn width(&self) -> i32 {
        match self {
            ProcColumn::Name => 190,
            ProcColumn::Command => 320,
            ProcColumn::Cgroup => 220,
            ProcColumn::Pid | ProcColumn::User => 80,
            ProcColumn::State => 70,
            ProcColumn::Cpu => 70,
            ProcColumn::Mem => 90,
            ProcColumn::MemPct => 70,
            ProcColumn::Threads => 60,
            ProcColumn::Priority | ProcColumn::Nice | ProcColumn::Policy => 80,
            ProcColumn::Io => 90,
            ProcColumn::ReadRate | ProcColumn::WriteRate => 100,
            ProcColumn::CpuTime => 100,
            ProcColumn::Started => 100,
        }
    }
}

/// 进程列表展示模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeMode {
    /// 平铺列表（父进程和子进程都在同一层，按排序键排）。
    Flat,
    /// 树形：主进程 + 其所有子进程（缩进 + 可折叠）。
    Tree,
}

/// 采样周期选项（秒）。
pub const INTERVAL_OPTIONS: &[(&str, f64)] =
    &[("0.5 秒", 0.5), ("1 秒", 1.0), ("2 秒", 2.0), ("5 秒", 5.0)];

/// 一次完整采样。
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub cpu: CpuStat,
    pub mem: MemStat,
    pub net: Vec<NetIface>,
    pub disks: Vec<DiskStat>,
    pub fs: Vec<FsUsage>,
    pub sensors: Vec<Sensor>,
    pub gpus: Vec<GpuStat>,
    pub batteries: Vec<Battery>,
    pub sys: SysInfo,
    pub processes: Vec<Process>,
    /// 采样耗时（毫秒），用于界面显示。
    pub cost_ms: u64,
    /// 采样时刻（毫秒时间戳）。
    pub at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_used_ratio_math() {
        let m = MemStat {
            total: 1000,
            used: 250,
            ..Default::default()
        };
        assert_eq!(m.used_ratio(), 0.25);
        let zero = MemStat::default();
        assert_eq!(zero.used_ratio(), 0.0, "total=0 必须防御除零");
        assert_eq!(zero.swap_ratio(), 0.0, "swap_total=0 必须防御除零");
        let s = MemStat {
            swap_total: 4,
            swap_used: 3,
            ..Default::default()
        };
        assert_eq!(s.swap_ratio(), 0.75);
    }

    #[test]
    fn net_errors_empty_when_clean() {
        assert_eq!(NetIface::default().errors(), "");
        let n = NetIface {
            rx_errs: 2,
            tx_errs: 1,
            ..Default::default()
        };
        assert_eq!(n.errors(), "错误 2 / 1");
        let n = NetIface {
            rx_drop: 7,
            tx_drop: 3,
            ..Default::default()
        };
        assert_eq!(n.errors(), "丢包 7 / 3");
        let n = NetIface {
            rx_errs: 1,
            tx_drop: 5,
            ..Default::default()
        };
        assert_eq!(n.errors(), "错误 1 / 0 · 丢包 0 / 5");
    }

    #[test]
    fn wireless_detection() {
        assert!(!NetIface::default().is_wireless());
        let w = NetIface {
            signal_dbm: Some(-45.0),
            ..Default::default()
        };
        assert!(w.is_wireless());
        let w = NetIface {
            link_quality: Some(50.0),
            ..Default::default()
        };
        assert!(w.is_wireless());
    }

    #[test]
    fn signal_text_matrix() {
        let both = NetIface {
            signal_dbm: Some(-50.0),
            link_quality: Some(40.7),
            ..Default::default()
        };
        assert_eq!(both.signal_text(), "信号 -50 dBm · 链路质量 41");
        let dbm = NetIface {
            signal_dbm: Some(-60.0),
            ..Default::default()
        };
        assert_eq!(dbm.signal_text(), "信号 -60 dBm");
        let qual = NetIface {
            link_quality: Some(30.0),
            ..Default::default()
        };
        assert_eq!(qual.signal_text(), "链路质量 30");
        assert_eq!(NetIface::default().signal_text(), "");
    }

    #[test]
    fn sensor_kind_labels_and_units() {
        assert_eq!(SensorKind::Temp.label(), "温度");
        assert_eq!(SensorKind::Fan.label(), "风扇");
        assert_eq!(SensorKind::Voltage.label(), "电压");
        assert_eq!(SensorKind::Power.label(), "功耗");
        assert_eq!(SensorKind::Current.label(), "电流");
        assert_eq!(SensorKind::Freq.label(), "频率");
        assert_eq!(SensorKind::Temp.unit(), "°C");
        assert_eq!(SensorKind::Fan.unit(), "RPM");
        assert_eq!(SensorKind::Voltage.unit(), "V");
        assert_eq!(SensorKind::Power.unit(), "W");
        assert_eq!(SensorKind::Current.unit(), "A");
        assert_eq!(SensorKind::Freq.unit(), "MHz");
    }

    #[test]
    fn display_name_prefers_comm_then_cmdline() {
        let p = Process {
            name: "chrome".into(),
            cmdline: "/usr/bin/chrome --type=renderer".into(),
            ..Default::default()
        };
        assert_eq!(p.display_name(), "chrome");
        let p = Process {
            name: String::new(),
            cmdline: "/usr/bin/python3 script.py".into(),
            ..Default::default()
        };
        assert_eq!(
            p.display_name(),
            "/usr/bin/python3",
            "空 comm 取 cmdline 首词"
        );
        let p = Process::default();
        assert_eq!(p.display_name(), "");
    }

    #[test]
    fn zombie_detection() {
        let p = Process {
            state_char: 'Z',
            ..Default::default()
        };
        assert!(p.is_zombie());
        let p = Process {
            state_char: 'S',
            ..Default::default()
        };
        assert!(!p.is_zombie());
    }

    #[test]
    fn signal_lookup() {
        let s = signal_by_num(31).expect("31 = SIGSYS 在表里");
        assert_eq!(s.name, "SIGSYS");
        assert!(signal_by_num(9999).is_none());
    }

    #[test]
    fn proc_columns_covered() {
        assert_eq!(ProcColumn::Name.label(), "名称");
        assert_eq!(ProcColumn::Cgroup.label(), "应用/cgroup");
        assert_eq!(ProcColumn::ALL.len(), 18, "ALL 必须覆盖全部 18 列变体");
        // 默认可见列必须是 ALL 的子集
        for c in ProcColumn::DEFAULT_VISIBLE {
            assert!(ProcColumn::ALL.contains(c), "{:?} 不在 ALL 里", c);
        }
        assert!(ProcColumn::DEFAULT_VISIBLE.len() < ProcColumn::ALL.len());
        for c in ProcColumn::ALL {
            assert!(!c.label().is_empty());
        }
    }
}
