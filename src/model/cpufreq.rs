//! CPU 调频（cpufreq governor / EPP）的数据模型与中文文案（纯数据，无 UI 依赖）。
//!
//! 参考 `docs/项目结构规划书.md` §3.8：本文件只定义 `struct`/`enum`、静态文案字典
//! 与纯数据变换；sysfs 读取与命令构造在 [`crate::utils::cpufreq`]（GOAL Phase 8 分层）。
//!
//! 关键设计（对应 GOAL D8.1 / D8.2）：
//! - **模式清单不写死**：页面上可选的 governor / EPP 一律来自本机 sysfs 动态读取
//!   （见 [`CpufreqSnapshot::governors`] / [`CpufreqSnapshot::epp`]），本文件的字典
//!   **只负责给中文说明文案**；字典没收录的内核/厂商调速器走
//!   [`describe_governor`] 的兜底文案，既不吞掉也不编造（D8.1）。
//! - governor（何时升降频的策略）与 EPP（同等负载下偏性能还是偏省电）是两条独立的轴：
//!   intel_pstate / amd_pstate(active) 两者都有，acpi-cpufreq / cpufreq-dt 没有 EPP
//!   文件 → 快照里 `epp` 为 `None`，页面整块隐藏，不报错（D8.2）。

/// 一个模式（governor 或 EPP 偏好）的中文说明条目。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeInfo {
    /// sysfs 里的原始名（如 `performance`、`balance_power`）。
    pub name: &'static str,
    /// 中文短名（如「性能优先」）。
    pub label: &'static str,
    /// 中文说明（一句话，含适用场景，对应 GOAL 8.2 附录原文）。
    pub desc: &'static str,
}

/// governor 中文说明字典（GOAL 8.2 附录，7 项）。
///
/// 只做文案，不做「本机有哪些」的判断——本机清单一律读 sysfs。
pub static GOVERNOR_INFO: &[ModeInfo] = &[
    ModeInfo {
        name: "performance",
        label: "性能优先",
        desc: "始终锁定最高频率，性能最强、响应最快；功耗与发热最高，笔记本续航最短。适合游戏、编译、跑分、台式机高性能场景。",
    },
    ModeInfo {
        name: "powersave",
        label: "省电优先",
        desc: "始终压在最低频率，最省电、发热与噪音最低；性能最差。适合挂机下载、NAS、风扇敏感的静音机。",
    },
    ModeInfo {
        name: "ondemand",
        label: "按需调频",
        desc: "负载一上来立刻拉满、空闲立刻降频。经典默认策略，响应快、日常最均衡。",
    },
    ModeInfo {
        name: "conservative",
        label: "保守调频",
        desc: "负载升高时逐级升频、空闲逐级降频；比 ondemand 更平滑省电，但要更久才到最高频。",
    },
    ModeInfo {
        name: "schedutil",
        label: "调度器联动",
        desc: "由内核调度器按真实 CPU 负载直接定频；现代内核推荐，能效与响应兼顾（CFS util 驱动）。",
    },
    ModeInfo {
        name: "interactive",
        label: "交互优化",
        desc: "面向触控/桌面交互快速升频；常见于厂商与手机内核，通用发行版少见。",
    },
    ModeInfo {
        name: "userspace",
        label: "用户态指定",
        desc: "频率交给用户态程序手动设定（配合 `cpupower frequency-set -f` 或监控程序）。",
    },
];

/// EPP（energy_performance_preference）中文说明字典（GOAL 8.2 附录，5 项）。
pub static EPP_INFO: &[ModeInfo] = &[
    ModeInfo {
        name: "default",
        label: "默认",
        desc: "跟随内核/发行版默认偏好。",
    },
    ModeInfo {
        name: "performance",
        label: "性能最大化",
        desc: "同等负载下最激进地保持高频，性能与功耗最高。",
    },
    ModeInfo {
        name: "balance_performance",
        label: "偏性能的平衡",
        desc: "多数发行版默认，优先响应、兼顾功耗。",
    },
    ModeInfo {
        name: "balance_power",
        label: "偏省电的平衡",
        desc: "优先省电与降温，性能稍降。",
    },
    ModeInfo {
        name: "power",
        label: "省电最大化",
        desc: "最省电，适合电池供电、静音与降温。",
    },
];

/// 字典没收录 governor 时的兜底文案（D8.1：不吞掉、不编造）。
pub const UNKNOWN_GOVERNOR_DESC: &str =
    "暂无说明（内核/厂商自定义调速器），可先用 `cpupower frequency-info` 查阅该调速器的行为。";

/// 字典没收录 EPP 偏好时的兜底文案。
pub const UNKNOWN_EPP_DESC: &str = "暂无说明（内核自定义能效偏好）。";

/// 按名字查 governor 文案条目；未收录返回 `None`。
pub fn governor_info(name: &str) -> Option<&'static ModeInfo> {
    GOVERNOR_INFO.iter().find(|m| m.name == name)
}

/// 按名字查 EPP 文案条目；未收录返回 `None`。
pub fn epp_info(name: &str) -> Option<&'static ModeInfo> {
    EPP_INFO.iter().find(|m| m.name == name)
}

/// governor 是否在字典内（页面可用来决定要不要给「暂无说明」提示）。
pub fn is_known_governor(name: &str) -> bool {
    governor_info(name).is_some()
}

/// 取 governor 中文说明：字典命中给原文案，未命中给兜底文案（D8.1）。
pub fn describe_governor(name: &str) -> String {
    match governor_info(name) {
        Some(m) => m.desc.to_string(),
        None => UNKNOWN_GOVERNOR_DESC.to_string(),
    }
}

/// 取 EPP 中文说明：字典命中给原文案，未命中给兜底文案（D8.1）。
pub fn describe_epp(name: &str) -> String {
    match epp_info(name) {
        Some(m) => m.desc.to_string(),
        None => UNKNOWN_EPP_DESC.to_string(),
    }
}

/// 取 governor 中文短名；未收录时**原样返回 sysfs 名**（不编造中文名）。
pub fn governor_label(name: &str) -> &str {
    match governor_info(name) {
        Some(m) => m.label,
        None => name,
    }
}

/// 取 EPP 中文短名；未收录时原样返回 sysfs 名。
pub fn epp_label(name: &str) -> &str {
    match epp_info(name) {
        Some(m) => m.label,
        None => name,
    }
}

/// 一个 cpufreq policy（同组 CPU 共享一套 governor 设置）。
///
/// 分组依据是 sysfs 的 `related_cpus` / `affected_cpus`（见
/// [`crate::utils::cpufreq::scan_root`]）；混合架构（big.LITTLE）上不同 policy
/// 的可选清单可能不同，所以清单按 policy 保留，不做全局并集覆盖。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyGroup {
    /// 该 policy 覆盖的 CPU 序号（升序）。
    pub cpus: Vec<usize>,
    /// `scaling_driver`（intel_pstate / acpi-cpufreq / cpufreq-dt ...）。
    pub driver: Option<String>,
    /// 当前调速器 `scaling_governor`。
    pub governor: Option<String>,
    /// 该 policy 可选调速器 `scaling_available_governors`。
    pub governors: Vec<String>,
    /// 当前能效偏好 `energy_performance_preference`（无 EPP 文件的 policy 为 `None`）。
    pub epp: Option<String>,
    /// 可选能效偏好 `energy_performance_available_preferences`。
    pub epp_available: Vec<String>,
    /// 硬件下限 `cpuinfo_min_freq`（kHz）。
    pub min_khz: Option<u64>,
    /// 硬件上限 `cpuinfo_max_freq`（kHz）。
    pub max_khz: Option<u64>,
    /// `scaling_setspeed`（userspace 调速器手工设定值，通常为 `-1` 表示未设定）。
    pub setspeed: Option<String>,
}

/// 能效偏好（EPP）聚合状态（D8.2 双轨中的第二条轴）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EppState {
    /// 当前值（取第一个读到的 policy）。
    pub current: Option<String>,
    /// 本机可选值（各 policy 并集，首次出现顺序）。
    pub available: Vec<String>,
}

/// 一次 cpufreq 枚举的完整结果（GOAL 8.1 的输出）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpufreqSnapshot {
    /// 本机内核是否提供 cpufreq 接口（**没有任何 `cpuN/cpufreq` 目录 = false**）。
    /// false 时其余字段全空，页面显示 StatusPage 空态，绝不 panic（部分龙芯内核如此）。
    pub supported: bool,
    /// 第一个 policy 的 `scaling_driver`（全机展示用）。
    pub driver: Option<String>,
    /// **所有 policy 可选调速器的并集**（首次出现顺序）——页面模式清单的唯一来源。
    pub governors: Vec<String>,
    /// 按 policy 分组的详细数据。
    pub policies: Vec<PolicyGroup>,
    /// EPP 聚合；本机无 EPP 文件为 `None`（页面整块隐藏，D8.2）。
    pub epp: Option<EppState>,
    /// 是否支持 boost：`Some(true)=开启` / `Some(false)=关闭` / `None=无该接口`。
    pub boost: Option<bool>,
    /// 全机硬件最低频率（各 policy `cpuinfo_min_freq` 的最小值，kHz）。
    pub freq_min_khz: Option<u64>,
    /// 全机硬件最高频率（各 policy `cpuinfo_max_freq` 的最大值，kHz）。
    pub freq_max_khz: Option<u64>,
    /// 检测到的 CPU 总数（`cpuN` 目录个数，含无 cpufreq 的核）。
    pub cpu_count: usize,
    /// 逐核当前频率 `(cpu, scaling_cur_freq)`，仅收录读到值的核（GOAL 8.1 逐核读）。
    pub cur_khz: Vec<(usize, u64)>,
}

impl CpufreqSnapshot {
    /// 本机是否有 EPP 可选项（页面用它决定显隐整块 EPP 区，D8.2）。
    pub fn has_epp(&self) -> bool {
        self.epp
            .as_ref()
            .map(|e| !e.available.is_empty())
            .unwrap_or(false)
    }

    /// 不提供某个调速器的 CPU 列表（升序）。
    ///
    /// 调速器不在并集内 → 返回全部核；在并集内但某些 policy 没有 → 返回那些核。
    /// 页面用它标注「混合架构上哪些核不支持该模式」（GOAL 8.1）。
    pub fn missing_cpus_for(&self, governor: &str) -> Vec<usize> {
        let mut out = Vec::new();
        for p in &self.policies {
            if !p.governors.iter().any(|g| g == governor) {
                out.extend(p.cpus.iter().copied());
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// 某个调速器是否**全部核**都支持。
    pub fn supports_everywhere(&self, governor: &str) -> bool {
        self.governors.iter().any(|g| g == governor) && self.missing_cpus_for(governor).is_empty()
    }

    /// 以 MHz 为单位的当前频率（kHz → MHz，展示层用）。
    pub fn mhz(khz: u64) -> f64 {
        f64::from(u32::try_from(khz / 1000).unwrap_or(u32::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn governor_dict_covers_seven_known_with_chinese_text() {
        assert_eq!(GOVERNOR_INFO.len(), 7, "GOAL 8.2 附录要求 7 项");
        let mut names: Vec<&str> = GOVERNOR_INFO.iter().map(|m| m.name).collect();
        let mut dedup = names.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(names.len(), dedup.len(), "名字不许重复");
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "conservative",
                "interactive",
                "ondemand",
                "performance",
                "powersave",
                "schedutil",
                "userspace"
            ]
        );
        for m in GOVERNOR_INFO {
            assert!(
                !m.label.is_empty() && !m.desc.is_empty(),
                "{} 文案为空",
                m.name
            );
            assert!(
                m.desc
                    .chars()
                    .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "{} 说明必须含中文",
                m.name
            );
        }
    }

    #[test]
    fn epp_dict_has_five_unique_entries() {
        assert_eq!(EPP_INFO.len(), 5, "GOAL 8.2 附录要求 5 项");
        let mut names: Vec<&str> = EPP_INFO.iter().map(|m| m.name).collect();
        let mut dedup = names.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(names.len(), dedup.len());
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "balance_performance",
                "balance_power",
                "default",
                "performance",
                "power"
            ]
        );
        for m in EPP_INFO {
            assert!(!m.desc.is_empty());
        }
    }

    #[test]
    fn describe_governor_hit_returns_dictionary_text() {
        assert_eq!(
            describe_governor("schedutil"),
            governor_info("schedutil").unwrap().desc
        );
        assert!(is_known_governor("performance"));
        assert_eq!(governor_label("ondemand"), "按需调频");
    }

    #[test]
    fn describe_unknown_governor_falls_back_without_inventing() {
        let d = describe_governor("some_vendor_thing");
        assert!(d.contains("暂无说明"), "兜底文案：{d}");
        assert!(!is_known_governor("some_vendor_thing"));
        // 未收录的短名必须原样返回 sysfs 名，不能编造中文名
        assert_eq!(governor_label("some_vendor_thing"), "some_vendor_thing");
    }

    #[test]
    fn describe_epp_hit_and_unknown_fallback() {
        assert!(describe_epp("balance_power").contains("省电"));
        let d = describe_epp("weird_epp");
        assert!(d.contains("暂无说明"), "兜底文案：{d}");
        assert_eq!(epp_label("power"), "省电最大化");
        assert_eq!(epp_label("weird_epp"), "weird_epp");
    }

    #[test]
    fn default_snapshot_is_unsupported_and_empty() {
        let s = CpufreqSnapshot::default();
        assert!(!s.supported);
        assert!(s.governors.is_empty());
        assert!(s.policies.is_empty());
        assert!(!s.has_epp());
        assert!(s.boost.is_none());
        assert!(s.missing_cpus_for("performance").is_empty());
        assert!(!s.supports_everywhere("performance"));
    }

    #[test]
    fn missing_cpus_for_reports_policies_without_that_mode() {
        let s = CpufreqSnapshot {
            supported: true,
            governors: vec!["performance".into(), "powersave".into(), "schedutil".into()],
            policies: vec![
                PolicyGroup {
                    cpus: vec![0, 1],
                    governors: vec!["performance".into(), "powersave".into()],
                    ..PolicyGroup::default()
                },
                PolicyGroup {
                    cpus: vec![2],
                    governors: vec!["performance".into(), "powersave".into(), "schedutil".into()],
                    ..PolicyGroup::default()
                },
            ],
            ..CpufreqSnapshot::default()
        };
        // schedutil 只有 cpu2 有 → 不支持的是 0、1
        assert_eq!(s.missing_cpus_for("schedutil"), vec![0, 1]);
        assert!(!s.supports_everywhere("schedutil"));
        // performance 全核都有
        assert!(s.missing_cpus_for("performance").is_empty());
        assert!(s.supports_everywhere("performance"));
        // 完全不在并集内的名字 → 全部核都不支持
        assert_eq!(s.missing_cpus_for("nope"), vec![0, 1, 2]);
    }

    #[test]
    fn has_epp_reflects_available_list_only() {
        let mut s = CpufreqSnapshot::default();
        assert!(!s.has_epp());
        s.epp = Some(EppState::default()); // 有 EPP 状态但可选清单为空 → 仍算没有
        assert!(!s.has_epp());
        s.epp = Some(EppState {
            current: Some("default".into()),
            available: vec!["default".into()],
        });
        assert!(s.has_epp());
    }

    #[test]
    fn mhz_conversion_handles_boundaries() {
        assert_eq!(CpufreqSnapshot::mhz(800_000), 800.0);
        assert_eq!(CpufreqSnapshot::mhz(3_600_000), 3600.0);
        assert_eq!(CpufreqSnapshot::mhz(999), 0.0); // 1kHz 以下截断为 0MHz
    }
}
