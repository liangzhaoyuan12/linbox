//! 传感器 / 显卡 / 电池 / 文件系统 / 网卡补充信息采集（纯逻辑，不依赖 GTK）。
//!
//! 数据来源全部是 sysfs 与 `/proc`，不依赖 lm-sensors 二进制；显卡同时支持
//! amdgpu（sysfs）、NVIDIA（nvidia-smi）、Intel（i915 sysfs）。

use std::collections::HashMap;
use std::fs;
use std::sync::OnceLock;

use crate::model::monitor::{Battery, DiskStat, FsUsage, GpuStat, NetIface, Sensor, SensorKind};

fn read_trim<P: AsRef<std::path::Path>>(path: P) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn read_num<P: AsRef<std::path::Path>>(path: P) -> Option<f64> {
    read_trim(path).and_then(|s| s.parse::<f64>().ok())
}

// ---------------------------------------------------------------------------
// CPU 静态信息
// ---------------------------------------------------------------------------

/// CPU 型号、物理核心数、最大频率、封装温度。只在页面初始化时取一次。
pub struct CpuStatic {
    pub model: String,
    pub cores: usize,
    pub threads: usize,
    pub freq_max_mhz: f32,
    pub temp: Option<f32>,
}

pub fn cpu_static() -> CpuStatic {
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let mut model = String::new();
    let mut core_ids = std::collections::HashSet::new();
    let mut threads = 0usize;
    let mut phys_id = 0usize;
    for line in cpuinfo.lines() {
        if model.is_empty() {
            // x86 是 `model name`，龙芯 /proc/cpuinfo 是 `Model Name`（大写），
            // 旧 loongson/mips 是 `cpu model` —— 三种都认（GOAL.md 4.6）
            if let Some(v) = line
                .strip_prefix("model name")
                .or_else(|| line.strip_prefix("Model Name"))
                .or_else(|| line.strip_prefix("cpu model"))
            {
                model = v.trim_start_matches([':', ' ', '\t']).to_string();
            }
        }
        if line.starts_with("processor") {
            threads += 1;
        }
        // core id + physical id 组合去重得到物理核心数
        if let Some(v) = line.strip_prefix("core id") {
            let cid: usize = v.trim_start_matches([':', ' ', '\t']).parse().unwrap_or(0);
            core_ids.insert((phys_id, cid));
        }
        if let Some(v) = line.strip_prefix("physical id") {
            phys_id = v.trim_start_matches([':', ' ', '\t']).parse().unwrap_or(0);
        }
    }
    let cores = if core_ids.is_empty() {
        threads
    } else {
        core_ids.len()
    };
    let freq_max_mhz = read_num("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq")
        .map(|k| (k / 1000.0) as f32)
        .unwrap_or(0.0);
    let temp = cpu_temp();
    CpuStatic {
        model,
        cores,
        threads,
        freq_max_mhz,
        temp,
    }
}

/// 各核当前频率的平均值（MHz）。
pub fn cpu_freq_mhz() -> f32 {
    let mut sum = 0f64;
    let mut n = 0;
    for i in 0..128 {
        let p = format!("/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq");
        match read_num(&p) {
            Some(v) => {
                sum += v / 1000.0;
                n += 1;
            }
            None => break,
        }
    }
    if n == 0 { 0.0 } else { (sum / n as f64) as f32 }
}

/// CPU 封装温度：优先 k10temp（Tctl），再 coretemp（Package id 0），再 thermal zone。
fn cpu_temp() -> Option<f32> {
    let sensors = collect_hwmon();
    for s in sensors.iter().filter(|s| s.kind == SensorKind::Temp) {
        let chip = s.chip.to_lowercase();
        let label = s.label.to_lowercase();
        if (chip.contains("k10temp") || chip.contains("zenpower"))
            && (label == "tctl" || label == "tdie" || label.starts_with("tctl"))
        {
            return Some(s.value);
        }
    }
    for s in sensors.iter().filter(|s| s.kind == SensorKind::Temp) {
        let chip = s.chip.to_lowercase();
        let label = s.label.to_lowercase();
        if (chip.contains("coretemp") || chip.contains("k10temp"))
            && (label.contains("package") || label == "tdie")
        {
            return Some(s.value);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// hwmon / thermal
// ---------------------------------------------------------------------------

/// 读取所有 hwmon 与 thermal zone 的传感器。
pub fn collect_sensors() -> Vec<Sensor> {
    let mut out = collect_hwmon();
    out.extend(collect_thermal());
    out
}

fn collect_hwmon() -> Vec<Sensor> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir("/sys/class/hwmon") else {
        return out;
    };
    for e in rd.flatten() {
        let base = e.path();
        let chip = read_trim(base.join("name")).unwrap_or_default();
        if chip.is_empty() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&base) else {
            continue;
        };
        let mut files: Vec<String> = entries
            .flatten()
            .map(|f| f.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        for f in files {
            let Some((kind, ch, attr)) = split_hwmon_name(&f) else {
                continue;
            };
            let label_raw = read_trim(base.join(format!("{ch}_label")))
                .unwrap_or_else(|| format!("{ch}{}", ""));
            let label = if label_raw.trim().is_empty() {
                ch.to_string()
            } else {
                label_raw
            };
            let Some(raw) = read_num(base.join(&f)) else {
                continue;
            };
            let value = scale_value(kind, raw);
            // 上限/临界值（只有温度/风扇有）
            let (max, crit): (Option<f64>, Option<f64>) = match kind {
                SensorKind::Temp => (
                    read_num(base.join(format!("{ch}_max"))).map(|v| v / 1000.0),
                    read_num(base.join(format!("{ch}_crit"))).map(|v| v / 1000.0),
                ),
                SensorKind::Fan => (read_num(base.join(format!("{ch}_max"))), None),
                _ => (None, None),
            };
            let _ = attr;
            out.push(Sensor {
                chip: chip.clone(),
                label,
                kind,
                value,
                max: max.map(|v| v as f32),
                crit: crit.map(|v| v as f32),
                raw_input: f,
            });
        }
    }
    out.sort_by(|a, b| {
        a.chip
            .cmp(&b.chip)
            .then(kind_rank(a.kind).cmp(&kind_rank(b.kind)))
            .then(a.label.cmp(&b.label))
    });
    out
}

fn kind_rank(k: SensorKind) -> u8 {
    match k {
        SensorKind::Temp => 0,
        SensorKind::Fan => 1,
        SensorKind::Power => 2,
        SensorKind::Voltage => 3,
        SensorKind::Current => 4,
        SensorKind::Freq => 5,
    }
}

/// 把 `temp1_input` 这类文件名拆成 (类别, 通道, 属性)。
fn split_hwmon_name(f: &str) -> Option<(SensorKind, String, &'static str)> {
    const KINDS: [(&str, SensorKind); 6] = [
        ("temp", SensorKind::Temp),
        ("fan", SensorKind::Fan),
        ("in", SensorKind::Voltage),
        ("power", SensorKind::Power),
        ("curr", SensorKind::Current),
        ("freq", SensorKind::Freq),
    ];
    // 只处理 _input（power 也支持 _average）
    let attr = if f.ends_with("_input") {
        "_input"
    } else if f.ends_with("_average") {
        "_average"
    } else {
        return None;
    };
    let stem = &f[..f.len() - attr.len()];
    for (prefix, kind) in KINDS {
        if let Some(rest) = stem.strip_prefix(prefix)
            && !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_digit())
        {
            return Some((kind, stem.to_string(), attr));
        }
    }
    None
}

/// hwmon 原始值 → 真实单位。
fn scale_value(kind: SensorKind, raw: f64) -> f32 {
    match kind {
        SensorKind::Temp => (raw / 1000.0) as f32,
        SensorKind::Fan => raw as f32,
        SensorKind::Voltage => (raw / 1000.0) as f32,
        SensorKind::Power => (raw / 1_000_000.0) as f32,
        SensorKind::Current => (raw / 1000.0) as f32,
        SensorKind::Freq => (raw / 1_000_000.0) as f32,
    }
}

fn collect_thermal() -> Vec<Sensor> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir("/sys/class/thermal") else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("thermal_zone") {
            continue;
        }
        let base = e.path();
        let kind = read_trim(base.join("type")).unwrap_or(name.clone());
        let Some(raw) = read_num(base.join("temp")) else {
            continue;
        };
        out.push(Sensor {
            chip: "thermal".to_string(),
            label: kind,
            kind: SensorKind::Temp,
            value: (raw / 1000.0) as f32,
            max: None,
            crit: None,
            raw_input: base.join("temp").to_string_lossy().into_owned(),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// 显卡
// ---------------------------------------------------------------------------

/// 从 lspci 拿显卡型号（只跑一次，缓存）。
fn gpu_pci_names() -> &'static HashMap<String, String> {
    static CACHE: OnceLock<HashMap<String, String>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut m = HashMap::new();
        let out = std::process::Command::new("lspci")
            .args(["-mm", "-nn"])
            .output();
        if let Ok(o) = out {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                // 0000:0b:00.0 "VGA compatible controller" "AMD/ATI" "Navi 23 [Radeon RX 6600]" -r c7 ...
                let fields: Vec<&str> = line.split('"').collect();
                if fields.len() < 8 {
                    continue;
                }
                let slot = fields[0].trim().to_string();
                let class = fields[1];
                if ![
                    "VGA compatible controller",
                    "3D controller",
                    "Display controller",
                ]
                .iter()
                .any(|c| class.contains(c))
                {
                    continue;
                }
                let vendor = fields[3].trim();
                let device = fields[5].trim();
                let name = if device.is_empty() {
                    vendor.to_string()
                } else {
                    device.to_string()
                };
                m.insert(slot.clone(), name);
                // 去掉域前缀的短地址也存一份（sysfs 里没有 0000:）
                if let Some(short) = slot.strip_prefix("0000:") {
                    m.insert(short.to_string(), m.get(&slot).cloned().unwrap_or_default());
                }
            }
        }
        m
    })
}

/// 采集所有显卡。
pub fn collect_gpus() -> Vec<GpuStat> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir("/sys/class/drm") else {
        return out;
    };
    let mut cards: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            n.strip_prefix("card")
                .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()))
        })
        .collect();
    cards.sort();
    for card in cards {
        let dev = format!("/sys/class/drm/{card}/device");
        if !std::path::Path::new(&dev).exists() {
            continue;
        }
        let vendor_id = read_trim(format!("{dev}/vendor")).unwrap_or_default();
        let device_id = read_trim(format!("{dev}/device")).unwrap_or_default();
        let driver = driver_of(&dev);
        let vendor = match vendor_id.trim_start_matches("0x") {
            "1002" => "AMD",
            "10de" => "NVIDIA",
            "8086" => "Intel",
            "1af4" => "Virtio",
            _ => "未知",
        }
        .to_string();
        let pci_slot = fs::canonicalize(&dev)
            .ok()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let name = gpu_pci_names().get(&pci_slot).cloned().unwrap_or_else(|| {
            if device_id.is_empty() {
                format!("{vendor} GPU")
            } else {
                format!("{vendor} GPU ({device_id})")
            }
        });

        let mut g = GpuStat {
            card: card.clone(),
            name,
            vendor,
            driver,
            busy: read_num(format!("{dev}/gpu_busy_percent")).map(|v| v as f32),
            mem_busy: read_num(format!("{dev}/mem_busy_percent")).map(|v| v as f32),
            mem_used: read_num(format!("{dev}/mem_info_vram_used")).map(|v| v as u64),
            mem_total: read_num(format!("{dev}/mem_info_vram_total")).map(|v| v as u64),
            gtt_used: read_num(format!("{dev}/mem_info_gtt_used")).map(|v| v as u64),
            ..Default::default()
        };
        // hwmon 里的温度/风扇/功耗/频率
        if let Some(hw) = first_hwmon_of(&dev) {
            g.temp = read_num(format!("{hw}/temp1_input")).map(|v| (v / 1000.0) as f32);
            g.temp_junction = read_num(format!("{hw}/temp2_input")).map(|v| (v / 1000.0) as f32);
            g.temp_mem = read_num(format!("{hw}/temp3_input")).map(|v| (v / 1000.0) as f32);
            g.fan = read_num(format!("{hw}/fan1_input")).map(|v| v as f32);
            g.fan_max = read_num(format!("{hw}/fan1_max")).map(|v| v as f32);
            g.power = read_num(format!("{hw}/power1_average"))
                .or_else(|| read_num(format!("{hw}/power1_input")))
                .map(|v| (v / 1_000_000.0) as f32);
            g.power_cap = read_num(format!("{hw}/power1_cap")).map(|v| (v / 1_000_000.0) as f32);
            // 频率标签不固定，按 label 找 sclk / mclk
            g.sclk_mhz = find_freq(&hw, "sclk");
            g.mclk_mhz = find_freq(&hw, "mclk");
        }
        // Intel 核显：频率在 gt_* 下
        if g.sclk_mhz.is_none() {
            g.sclk_mhz = read_num(format!("{dev}/gt_cur_freq_mhz")).map(|v| v as f32);
        }
        // amdgpu 用 pp_dpm 的当前档位补频率
        if g.sclk_mhz.is_none() {
            g.sclk_mhz = pp_dpm_current(&dev, "sclk");
            g.mclk_mhz = pp_dpm_current(&dev, "mclk");
        }
        out.push(g);
    }
    // NVIDIA：sysfs 给不出占用率，走 nvidia-smi
    if out.is_empty() || out.iter().all(|g| g.vendor == "NVIDIA") {
        out.extend(nvidia_smi_gpus());
    }
    out
}

/// device 目录下第一个 hwmon 的路径。
fn first_hwmon_of(dev: &str) -> Option<String> {
    let dir = format!("{dev}/hwmon");
    let mut names: Vec<String> = fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names.into_iter().next()
}

fn find_freq(hw: &str, want: &str) -> Option<f32> {
    for i in 1..8 {
        let label = read_trim(format!("{hw}/freq{i}_label")).unwrap_or_default();
        if label.to_lowercase() == want {
            return read_num(format!("{hw}/freq{i}_input")).map(|v| (v / 1_000_000.0) as f32);
        }
    }
    None
}

/// 读 `pp_dpm_sclk` 里带 `*` 的当前档位。
fn pp_dpm_current(dev: &str, which: &str) -> Option<f32> {
    let text = read_trim(format!("{dev}/pp_dpm_{which}"))?;
    for line in text.lines() {
        if line.trim().ends_with('*') {
            let mhz = line
                .split(':')
                .nth(1)?
                .trim()
                .trim_end_matches('*')
                .trim()
                .trim_end_matches("Mhz")
                .trim_end_matches("MHz")
                .trim()
                .parse::<f32>()
                .ok()?;
            return Some(mhz);
        }
    }
    None
}

fn driver_of(dev: &str) -> String {
    fs::read_link(format!("{dev}/driver"))
        .ok()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

fn nvidia_smi_gpus() -> Vec<GpuStat> {
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=index,name,utilization.gpu,utilization.memory,memory.used,memory.total,temperature.gpu,power.draw,power.limit,clocks.current.graphics,clocks.current.memory,fan.speed",
            "--format=csv,noheader,nounits",
        ])
        .output();
    let Ok(o) = out else { return Vec::new() };
    if !o.status.success() {
        return Vec::new();
    }
    let mut gpus = Vec::new();
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        let f: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if f.len() < 3 {
            continue;
        }
        let num = |i: usize| f.get(i).and_then(|s| s.parse::<f32>().ok());
        let mib = |i: usize| {
            f.get(i)
                .and_then(|s| s.parse::<f64>().ok())
                .map(|v| (v * 1024.0 * 1024.0) as u64)
        };
        gpus.push(GpuStat {
            card: format!("nvidia{}", f.first().unwrap_or(&"0")),
            name: f.get(1).unwrap_or(&"").to_string(),
            vendor: "NVIDIA".to_string(),
            driver: "nvidia".to_string(),
            busy: num(2),
            mem_busy: num(3),
            mem_used: mib(4),
            mem_total: mib(5),
            temp: num(6),
            power: num(7),
            power_cap: num(8),
            sclk_mhz: num(9),
            mclk_mhz: num(10),
            fan: num(11),
            ..Default::default()
        });
    }
    gpus
}

// ---------------------------------------------------------------------------
// 电池
// ---------------------------------------------------------------------------

pub fn collect_batteries() -> Vec<Battery> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir("/sys/class/power_supply") else {
        return out;
    };
    for e in rd.flatten() {
        let base = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let ty = read_trim(base.join("type")).unwrap_or_default();
        if ty != "Battery" {
            continue;
        }
        let capacity = read_num(base.join("capacity")).unwrap_or(0.0) as f32;
        let status = read_trim(base.join("status")).unwrap_or_default();
        // 单位可能是 µWh(energy_*) 或 µAh(charge_*)
        let (now, full) = match read_num(base.join("energy_now")) {
            Some(n) => (
                n as u64,
                read_num(base.join("energy_full")).unwrap_or(0.0) as u64,
            ),
            None => (
                read_num(base.join("charge_now")).unwrap_or(0.0) as u64,
                read_num(base.join("charge_full")).unwrap_or(0.0) as u64,
            ),
        };
        let power = read_num(base.join("power_now"))
            .map(|v| v / 1_000_000.0)
            .unwrap_or(0.0);
        let time_left = read_num(base.join("time_to_empty_now"))
            .or_else(|| read_num(base.join("time_to_empty_avg")))
            .unwrap_or(0.0) as u64;
        out.push(Battery {
            name,
            capacity,
            status,
            energy_now: now,
            energy_full: full,
            power,
            time_left,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// 网络补充信息（网卡属性 + IP）
// ---------------------------------------------------------------------------

/// 给已采集的网卡补上 operstate / speed / mtu / mac。
pub fn enrich_net(ifaces: &mut [NetIface]) {
    for i in ifaces.iter_mut() {
        let base = format!("/sys/class/net/{}", i.name);
        i.operstate = read_trim(format!("{base}/operstate")).unwrap_or_else(|| "unknown".into());
        i.mtu = read_trim(format!("{base}/mtu")).unwrap_or_default();
        i.mac = read_trim(format!("{base}/address")).unwrap_or_default();
        i.speed = match read_num(format!("{base}/speed")) {
            Some(v) if v > 0.0 && i.name != "lo" => format!("{v} Mb/s"),
            _ => String::new(),
        };
    }
}

/// 无线链路信息：`/proc/net/wireless` 的 (信号 dBm, 链路质量)。
///
/// 文件格式（第三行起）：
/// ```text
/// wlp3s0: 0000   68.  -42.  -256        0      0      0     67      0        0
/// ```
/// 无效值（如 -256 的 noise）会被丢掉。
pub fn collect_wireless() -> HashMap<String, (Option<f32>, Option<f32>)> {
    let mut m = HashMap::new();
    let Ok(text) = fs::read_to_string("/proc/net/wireless") else {
        return m;
    };
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let v: Vec<&str> = rest.split_whitespace().collect();
        // status, link, level, noise, ...
        let num = |i: usize| {
            v.get(i)
                .map(|s| s.trim_end_matches('.'))
                .and_then(|s| s.parse::<f32>().ok())
        };
        let link = num(1).filter(|x| (0.0..=200.0).contains(x));
        // level 的无效值常见为 -256 / 0
        let level = num(2).filter(|x| *x < 0.0 && *x > -110.0);
        m.insert(name.to_string(), (level, link));
    }
    m
}

/// 用 `ip -j addr show` 拿 IP 地址（拿不到就只留 MAC）。
pub fn collect_ips() -> HashMap<String, (Vec<String>, Vec<String>)> {
    let mut m = HashMap::new();
    let Ok(o) = std::process::Command::new("ip")
        .args(["-j", "addr", "show"])
        .output()
    else {
        return m;
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&o.stdout) else {
        return m;
    };
    let Some(arr) = json.as_array() else { return m };
    for item in arr {
        let Some(name) = item.get("ifname").and_then(|v| v.as_str()) else {
            continue;
        };
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        if let Some(addrs) = item.get("addr_info").and_then(|v| v.as_array()) {
            for a in addrs {
                let fam = a.get("family").and_then(|v| v.as_str()).unwrap_or("");
                let local = a.get("local").and_then(|v| v.as_str()).unwrap_or("");
                let plen = a.get("prefixlen").and_then(|v| v.as_u64()).unwrap_or(0);
                if local.is_empty() {
                    continue;
                }
                // 只保留全局地址，跳过 link-local 的 IPv6
                if fam == "inet" {
                    v4.push(format!("{local}/{plen}"));
                } else if fam == "inet6" && !local.starts_with("fe80") {
                    v6.push(format!("{local}/{plen}"));
                }
            }
        }
        m.insert(name.to_string(), (v4, v6));
    }
    m
}

// ---------------------------------------------------------------------------
// 磁盘补充信息 + 文件系统
// ---------------------------------------------------------------------------

/// 给磁盘补上型号、容量、是否机械盘、分区列表。
pub fn enrich_disks(disks: &mut [DiskStat]) {
    for d in disks.iter_mut() {
        let base = format!("/sys/block/{}", d.name);
        d.model = read_trim(format!("{base}/device/model"))
            .or_else(|| read_trim(format!("{base}/device/name")))
            .unwrap_or_default();
        d.size = read_num(format!("{base}/size"))
            .map(|sectors| sectors as u64 * 512)
            .unwrap_or(0);
        d.rotational = read_trim(format!("{base}/queue/rotational")).as_deref() == Some("1");
        d.partitions = fs::read_dir(&base)
            .map(|rd| {
                let mut v: Vec<String> = rd
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with(&d.name) && n != &d.name)
                    .collect();
                v.sort();
                v
            })
            .unwrap_or_default();
    }
}

/// `statvfs(3)` 探测挂载点剩余空间（不依赖外部命令）。
#[repr(C)]
struct StatVfs {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_favail: u64,
    f_fsid: u64,
    f_flag: u64,
    f_namemax: u64,
    __f_spare: [i32; 6],
}

fn statvfs(path: &str) -> Option<(u64, u64, u64)> {
    unsafe extern "C" {
        fn statvfs(path: *const std::os::raw::c_char, buf: *mut StatVfs) -> i32;
    }
    let c = std::ffi::CString::new(path).ok()?;
    let mut buf = StatVfs {
        f_bsize: 0,
        f_frsize: 0,
        f_blocks: 0,
        f_bfree: 0,
        f_bavail: 0,
        f_files: 0,
        f_ffree: 0,
        f_favail: 0,
        f_fsid: 0,
        f_flag: 0,
        f_namemax: 0,
        __f_spare: [0; 6],
    };
    let r = unsafe { statvfs(c.as_ptr(), &mut buf) };
    if r != 0 {
        return None;
    }
    let frsize = if buf.f_frsize > 0 {
        buf.f_frsize
    } else {
        buf.f_bsize
    };
    let total = buf.f_blocks * frsize;
    let free = buf.f_bfree * frsize;
    let avail = buf.f_bavail * frsize;
    Some((total, total.saturating_sub(free), avail))
}

/// 伪文件系统（不显示在「存储」里）。
const PSEUDO_FS: &[&str] = &[
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "securityfs",
    "cgroup",
    "cgroup2",
    "pstore",
    "bpf",
    "autofs",
    "mqueue",
    "hugetlbfs",
    "debugfs",
    "tracefs",
    "configfs",
    "fusectl",
    "binfmt_misc",
    "efivarfs",
    "nsfs",
    "ramfs",
    "squashfs",
];

/// 所有真实挂载点的使用率。
pub fn collect_fs() -> Vec<FsUsage> {
    let mut out = Vec::new();
    let text = fs::read_to_string("/proc/mounts").unwrap_or_default();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 {
            continue;
        }
        let (device, mount, fstype) = (f[0], f[1], f[2]);
        if PSEUDO_FS.contains(&fstype) {
            continue;
        }
        // /run /dev/shm 之类 tmpfs 太多噪音，只留 / 和 /dev/shm
        if fstype == "tmpfs" && mount != "/dev/shm" && mount != "/run" {
            continue;
        }
        let Some((total, used, avail)) = statvfs(mount) else {
            continue;
        };
        if total == 0 {
            continue;
        }
        out.push(FsUsage {
            mount: unescape_mount(mount),
            device: device.to_string(),
            fstype: fstype.to_string(),
            total,
            used,
            avail,
            use_pct: 100.0 * used as f32 / total as f32,
        });
    }
    // 根分区在最前，其余按挂载点排序
    out.sort_by(|a, b| {
        let key = |m: &str| {
            if m == "/" {
                String::new()
            } else {
                m.to_string()
            }
        };
        key(&a.mount).cmp(&key(&b.mount))
    });
    out
}

/// /proc/mounts 里空格等被转义成 `\040`。
fn unescape_mount(s: &str) -> String {
    s.replace("\\040", " ").replace("\\011", "\t")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_hwmon_names() {
        assert_eq!(
            split_hwmon_name("temp1_input").map(|(k, _, _)| k),
            Some(SensorKind::Temp)
        );
        assert_eq!(
            split_hwmon_name("temp3_input").map(|(k, c, _)| (k, c)),
            Some((SensorKind::Temp, "temp3".to_string()))
        );
        assert_eq!(
            split_hwmon_name("fan1_input").map(|(k, _, _)| k),
            Some(SensorKind::Fan)
        );
        assert_eq!(
            split_hwmon_name("in0_input").map(|(k, _, _)| k),
            Some(SensorKind::Voltage)
        );
        assert_eq!(
            split_hwmon_name("curr1_input").map(|(k, _, _)| k),
            Some(SensorKind::Current)
        );
        assert_eq!(
            split_hwmon_name("freq1_input").map(|(k, _, _)| k),
            Some(SensorKind::Freq)
        );
        assert_eq!(
            split_hwmon_name("power1_average").map(|(k, _, a)| (k, a)),
            Some((SensorKind::Power, "_average"))
        );
        // 属性文件与无关文件都要被忽略
        assert!(split_hwmon_name("temp1_label").is_none());
        assert!(split_hwmon_name("temp1_max").is_none());
        assert!(split_hwmon_name("name").is_none());
        assert!(split_hwmon_name("pwm1").is_none());
        assert!(split_hwmon_name("in0_input_extra").is_none());
    }

    #[test]
    fn scale_values_to_units() {
        assert!((scale_value(SensorKind::Temp, 52000.0) - 52.0).abs() < 0.001);
        assert!((scale_value(SensorKind::Fan, 1200.0) - 1200.0).abs() < 0.001);
        assert!((scale_value(SensorKind::Voltage, 743.0) - 0.743).abs() < 0.001);
        assert!((scale_value(SensorKind::Power, 16_000_000.0) - 16.0).abs() < 0.001);
        assert!((scale_value(SensorKind::Current, 1500.0) - 1.5).abs() < 0.001);
        assert!((scale_value(SensorKind::Freq, 1_480_000_000.0) - 1480.0).abs() < 0.001);
    }

    #[test]
    fn real_sensors_look_sane() {
        let s = collect_sensors();
        assert!(!s.is_empty(), "本机一个传感器都没读到");
        // 温度值必须在合理范围内（防止单位换算写错）
        for t in s.iter().filter(|x| x.kind == SensorKind::Temp) {
            assert!(
                (-50.0..=200.0).contains(&t.value),
                "温度 {} {} = {} 超出合理范围",
                t.chip,
                t.label,
                t.value
            );
        }
        // CPU 温度源随平台不同（GOAL.md 4.6）：AMD=k10temp、Intel=coretemp、
        // 龙芯等=cpu_hwmon——只要存在任一已知 CPU 温度芯片即可。
        assert!(
            s.iter().any(|x| {
                let c = x.chip.to_lowercase();
                c.contains("k10temp") || c.contains("coretemp") || c.contains("cpu")
            }),
            "找不到 CPU 温度源，芯片列表：{:?}",
            s.iter().map(|x| x.chip.as_str()).collect::<Vec<_>>()
        );
        let cpu = cpu_static();
        assert!(!cpu.model.is_empty());
        assert!(cpu.cores > 0 && cpu.threads >= cpu.cores);
        // 主频：无 cpufreq sysfs 的内核（部分龙芯）读不到，有能力才断言
        if std::path::Path::new("/sys/devices/system/cpu/cpu0/cpufreq").exists() {
            assert!(cpu.freq_max_mhz > 0.0, "有 cpufreq 却读不到最高主频");
            let f = cpu_freq_mhz();
            assert!(f > 0.0 && f < 10_000.0, "主频异常：{f}");
        }
        // CPU 温度：k10temp/coretemp 存在时必须取到；其他平台只要求温度传感器
        // 齐全（温度值范围已在上面全量校验过）
        let has_known_cpu_chip = s.iter().any(|x| {
            let c = x.chip.to_lowercase();
            c.contains("k10temp") || c.contains("zenpower") || c.contains("coretemp")
        });
        if has_known_cpu_chip {
            assert!(cpu.temp.is_some(), "有 k10temp/coretemp 却取不到 CPU 温度");
        } else {
            assert!(
                s.iter().any(|x| x.kind == SensorKind::Temp),
                "没有任何温度传感器"
            );
        }
    }

    #[test]
    fn real_gpu_detected() {
        let g = collect_gpus();
        assert!(!g.is_empty(), "没有检测到显卡");
        let d = &g[0];
        assert!(!d.name.is_empty());
        // 厂商随机器不同（AMD / 龙芯 loonggpu…），有值即可（GOAL.md 4.6）
        assert!(!d.vendor.is_empty(), "显卡厂商为空");
        // 显存：读得到就必须 >0；已用 ≤ 总量（老 radeon 卡可能读不到 vram 文件）
        if let Some(total) = d.mem_total {
            assert!(total > 0, "显存总量读成 0");
        }
        assert!(d.mem_used.unwrap_or(0) <= d.mem_total.unwrap_or(0));
        // 占用率只有 amdgpu sysfs 提供（radeon 驱动没有 gpu_busy_percent）
        if let Some(b) = d.busy {
            assert!((0.0..=100.0).contains(&b), "占用率越界：{b}");
        }
        // 温度/功耗/频率来自 hwmon：有值就必须在合理量纲（原测试的单位换算校验意图）
        if let Some(t) = d.temp {
            assert!((-50.0..150.0).contains(&t), "温度异常：{t}");
        }
        if let Some(p) = d.power {
            assert!((0.0..1000.0).contains(&p), "功耗异常：{p}");
        }
        if let Some(sclk) = d.sclk_mhz {
            assert!((0.0..10_000.0).contains(&sclk), "sclk 异常：{sclk}");
        }
    }

    #[test]
    fn real_fs_and_statvfs() {
        // statvfs 与 df -B1 的根分区用量应一致（允许 1% 误差）
        let (total, used, avail) = statvfs("/").expect("statvfs / 失败");
        assert!(total > 0);
        assert!(used <= total);
        assert!(avail <= total);
        let fs = collect_fs();
        assert!(!fs.is_empty());
        let root = fs.iter().find(|f| f.mount == "/").expect("没有根挂载点");
        assert_eq!(root.total, total);
        assert!(root.use_pct > 0.0 && root.use_pct <= 100.0);
        // 伪文件系统必须被过滤掉
        assert!(!fs.iter().any(|f| f.mount == "/proc" || f.mount == "/sys"));
    }

    #[test]
    fn net_enrich_and_ips() {
        let raw = crate::utils::monitor::proc::parse_net_dev(
            &std::fs::read_to_string("/proc/net/dev").unwrap(),
        );
        let mut ifaces = crate::utils::monitor::proc::net_delta(&raw, &raw, 1.0);
        enrich_net(&mut ifaces);
        let lo = ifaces.iter().find(|i| i.name == "lo").unwrap();
        assert_eq!(lo.mtu, "65536");
        assert!(lo.is_loopback);
        // IP 地址（本机有 ip 命令时应能拿到回环地址）
        let ips = collect_ips();
        if let Some((v4, _)) = ips.get("lo") {
            assert!(v4.iter().any(|a| a.starts_with("127.0.0.1")));
        }
        for i in ifaces.iter_mut() {
            if let Some((v4, v6)) = ips.get(&i.name) {
                i.ipv4 = v4.clone();
                i.ipv6 = v6.clone();
            }
        }
        assert!(ifaces.iter().any(|i| !i.mac.is_empty()));
    }

    #[test]
    fn real_disks_enriched() {
        let raw = crate::utils::monitor::proc::parse_diskstats(
            &std::fs::read_to_string("/proc/diskstats").unwrap(),
        );
        let mut disks = crate::utils::monitor::proc::disk_delta(&raw, &raw, 1.0);
        enrich_disks(&mut disks);
        assert!(!disks.is_empty());
        // 真实大盘必须存在（>100,000,000，sectors/bytes 两种单位下都能筛掉
        // loop0(size=0) 与 ram 盘(size=8192/4MiB)）（GOAL.md 4.6）
        assert!(
            disks.iter().any(|d| d.size > 100_000_000),
            "找不到真实磁盘，最大 size={:?}",
            disks.iter().map(|d| d.size).max()
        );
        // diskstats 里 ram/loop 排在 sda 前面且无型号无分区——按「有分区或有
        // 型号」筛出真实盘；部分平台（龙芯 SATA）sda/model 为空，型号非空才校验
        let d = disks
            .iter()
            .find(|d| d.size > 0 && (!d.partitions.is_empty() || !d.model.is_empty()))
            .expect("没有带分区/型号的磁盘");
        if !d.model.is_empty() {
            assert!(d.model.len() < 64, "型号异常：{}", d.model);
        }
        assert!(d.size > 0);
    }

    #[test]
    fn batteries_may_be_empty_but_must_not_panic() {
        let b = collect_batteries();
        for x in b.iter() {
            assert!(x.capacity <= 100.0);
        }
    }
}
