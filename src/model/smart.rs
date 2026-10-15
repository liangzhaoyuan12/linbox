//! GOAL 10.2 —— SMART 数据结构与中文健康分析（纯逻辑，**禁止 `use gtk`/`use adw`/`use glib`**）。
//!
//! 输入 = `smartctl -j -a <dev>` 的 JSON 文本（由 `utils::smart` 读取），输出 = `HealthReport`。
//!
//! 分析口径（GOAL D10.3 三层，硬约束）：
//! 1. **整体**：`smart_status.passed` → 健康 / 不健康；缺失 → `Unknown`（不猜）。
//! 2. **关键属性预警**：扇区类（Reallocated/Pending/Uncorrectable/Event）与链路类
//!    （UDMA_CRC）非 0 即告警；`value ≤ thresh` 或 `when_failed` 非空判 FAIL。
//! 3. **参考指标**：通电时间 / 通电次数 / 温度三档 / TBW（有 `Total_LBAs_Written`
//!    才算，缺字段不显示）。
//!
//! **绝不编造**：字典没有的属性显示原始 ID + 名称 + 值 +「暂无说明」兜底
//! （同 Phase 8 D8.1）；缺字段就缺，不用默认值冒充真实读数。

use serde_json::Value;

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

/// 整体健康结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overall {
    /// `smart_status.passed = true`。
    Passed,
    /// `smart_status.passed = false`。
    Failed,
    /// JSON 里没有 `smart_status`（设备不支持 / 未读到）——显示「未知」，不猜。
    Unknown,
}

/// 单条 SMART 属性（ATA 属性表）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmartAttr {
    /// 属性 ID（ATA ID#）。
    pub id: u16,
    /// 属性英文名（smartctl 原样，未知属性为 `Unknown_Attribute`）。
    pub name: String,
    /// 归一化当前值（0-255，越大越健康；非归一化属性可能为 0/无意义）。
    pub value: u8,
    /// 历史最差值。
    pub worst: u8,
    /// 失效阈值：`value <= thresh` 即 FAIL。
    pub thresh: u8,
    /// 判定结果（非空 = 已 FAIL，smartctl 原样 `FAILING_NOW`/`FAILED_IN_THE_PAST`）。
    pub when_failed: String,
    /// 原始值（数值）。
    pub raw_value: u64,
    /// 原始值（smartctl 拼好的字符串，可能带单位）。
    pub raw_string: String,
    /// 是否预失败属性（Pre-fail：`flags.prefailure`）。
    pub prefailure: bool,
    /// 字典命中后的中文信息（None = 兜底，UI 显示「暂无说明」）。
    pub info: Option<&'static AttrInfo>,
}

impl SmartAttr {
    /// 是否 FAIL（D10.3 ②：`value ≤ thresh` 或 `when_failed` 非空）。
    pub fn is_failing(&self) -> bool {
        !self.when_failed.is_empty() || (self.thresh > 0 && self.value <= self.thresh)
    }

    /// 中文显示名：字典命中给中文名，否则原样英文名。
    pub fn display_name(&self) -> &str {
        match self.info {
            Some(i) => i.cn,
            None => &self.name,
        }
    }
}

/// 预警条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    /// 触发预警的属性（NVMe 类预警该字段为 None，用 `source` 说明）。
    pub attr: Option<Box<SmartAttr>>,
    /// 预警级别：`Severe` = 硬盘可能已损坏，`Warning` = 需要关注。
    pub severe: bool,
    /// 中文预警文案。
    pub message: String,
}

/// 设备信息（全部来自 JSON，缺字段 = None）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    pub model: Option<String>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub capacity_bytes: Option<u64>,
    pub logical_block_size: Option<u64>,
    /// `rotation_rate == 0`（SSD）或 `trim.supported`。
    pub is_ssd: bool,
    /// 接口/协议（`sata_version.string` 或 `device.protocol`）。
    pub interface: Option<String>,
    pub power_on_hours: Option<u64>,
    pub power_cycle_count: Option<u64>,
    pub temperature_c: Option<i64>,
    pub smart_available: bool,
    pub smart_enabled: bool,
}

/// NVMe 健康日志（`nvme_smart_health_information_log`，与 ATA 属性表结构不同）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NvmeHealth {
    pub temperature_c: Option<i64>,
    pub power_on_hours: Option<u64>,
    pub power_cycles: Option<u64>,
    /// 已用寿命百分比（`percentage_used`，100 = 寿命用尽）。
    pub percentage_used: Option<u64>,
    pub available_spare: Option<u64>,
    pub media_errors: Option<u64>,
}

/// 写入量（TBW 换算素材）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tbw {
    pub written_bytes: u64,
    pub read_bytes: u64,
}

/// 分析结论。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthReport {
    pub device: DeviceInfo,
    pub overall: Overall,
    /// 属性表（按 ID 升序）。
    pub attrs: Vec<SmartAttr>,
    /// 关键属性预警（D10.3 ②）。
    pub alerts: Vec<Alert>,
    /// 已 FAIL 的属性（`value ≤ thresh` / `when_failed`）。
    pub failures: Vec<SmartAttr>,
    /// 字典未收录的属性条数（页面显示「N 条未知属性」）。
    pub unknown_count: usize,
    /// 通电时间档位文案（D10.3 ③）。
    pub hours_tier: Option<&'static str>,
    /// 温度档位文案（D10.3 ③）。
    pub temp_tier: Option<&'static str>,
    /// TBW（有 `Total_LBAs_Written` 才有值）。
    pub tbw: Option<Tbw>,
    pub nvme: Option<NvmeHealth>,
    /// 备注（如「不支持 SMART」「属性表缺失」）。
    pub notes: Vec<String>,
}

impl HealthReport {
    pub fn is_healthy(&self) -> bool {
        self.overall == Overall::Passed
    }

    /// 整体结论中文标签。
    pub fn overall_label(&self) -> &'static str {
        match self.overall {
            Overall::Passed => "健康",
            Overall::Failed => "不健康",
            Overall::Unknown => "未知",
        }
    }
}

// ---------------------------------------------------------------------------
// 属性字典（D10.3 / D8.1 同款约束：未收录必须兜底，绝不编造）
// ---------------------------------------------------------------------------

/// 属性条目。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttrInfo {
    pub id: u16,
    /// 中文名。
    pub cn: &'static str,
    /// 中文说明（含义 + 怎么看）。
    pub desc: &'static str,
}

/// 常见 ATA SMART 属性中文词典（含本机 Pado SSD 的全部已知属性 + 常见盘型属性）。
/// **词典外的属性一律走「暂无说明」兜底**，本表只增不猜。
pub static ATTR_TABLE: &[AttrInfo] = &[
    AttrInfo {
        id: 1,
        cn: "原始读取错误率",
        desc: "盘片/闪存读取时的底层错误统计，原始值长期增长才需关注；个别盘出厂即有较大初值，属正常。",
    },
    AttrInfo {
        id: 3,
        cn: "马达起停时间",
        desc: "主轴马达加速/停止累计时间（机械盘）。",
    },
    AttrInfo {
        id: 4,
        cn: "启停次数",
        desc: "马达启停循环次数（机械盘）。",
    },
    AttrInfo {
        id: 5,
        cn: "重映射扇区数",
        desc: "盘片出现坏道后被备用扇区替换的数量。非 0 且持续增长 = 坏道在扩散，尽快备份换盘（机械盘偶发 1 个且不增长可观察）。",
    },
    AttrInfo {
        id: 7,
        cn: "寻道错误率",
        desc: "磁头寻道时的错误统计（机械盘），原厂常给出极大基数，看趋势不看绝对值。",
    },
    AttrInfo {
        id: 9,
        cn: "通电时间",
        desc: "累计通电小时数。参考档位：<1 万小时新盘，1-3 万小时正常使用，>3 万小时老旧（SSD 还需结合写入量）。",
    },
    AttrInfo {
        id: 10,
        cn: "启停计数",
        desc: "马达启动次数（机械盘）。",
    },
    AttrInfo {
        id: 12,
        cn: "通电次数",
        desc: "整机通电循环次数，反映使用强度。",
    },
    AttrInfo {
        id: 175,
        cn: "编程失败计数（芯片级）",
        desc: "NAND 块编程（写入）失败累计，正常为 0；增长说明闪存老化。",
    },
    AttrInfo {
        id: 176,
        cn: "擦除失败计数（芯片级）",
        desc: "NAND 块擦除失败累计，正常为 0；增长说明闪存老化。",
    },
    AttrInfo {
        id: 177,
        cn: "磨损均衡计数",
        desc: "SSD 磨损均衡轮转计数，配合 232 可用保留块一起看寿命。",
    },
    AttrInfo {
        id: 178,
        cn: "已用保留块计数（芯片级）",
        desc: "已消耗的备用块数量，持续增长属正常磨损，接近 232 阈值则需换盘。",
    },
    AttrInfo {
        id: 181,
        cn: "编程失败总计",
        desc: "全部编程（写入）失败累计，正常为 0。",
    },
    AttrInfo {
        id: 182,
        cn: "擦除失败总计",
        desc: "全部擦除失败累计，正常为 0。",
    },
    AttrInfo {
        id: 183,
        cn: "盘片降级",
        desc: "出厂检测出的降级盘片数（NAS 盘），非 0 表示盘片质量有瑕疵。",
    },
    AttrInfo {
        id: 184,
        cn: "端到端错误",
        desc: "数据从缓存到盘片全程校验错误数，非 0 需关注。",
    },
    AttrInfo {
        id: 187,
        cn: "无法校正的错误",
        desc: "ECC 无法纠正的读取错误，非 0 = 数据已出错，立即备份。",
    },
    AttrInfo {
        id: 188,
        cn: "命令超时",
        desc: "接口命令超时次数，增长快多与线缆/接口有关。",
    },
    AttrInfo {
        id: 189,
        cn: "高优先级清除",
        desc: "高优先级离线收集/清除次数（多见于 SSD）。",
    },
    AttrInfo {
        id: 190,
        cn: "温度（气流温度）",
        desc: "机械盘气流温度。建议长期 <45℃。",
    },
    AttrInfo {
        id: 191,
        cn: "温度冲击",
        desc: "温度越限造成的冲击计数，非 0 需改善散热。",
    },
    AttrInfo {
        id: 192,
        cn: "意外断电次数",
        desc: "非正常断电计数。频繁断电对盘有害，建议接 UPS。",
    },
    AttrInfo {
        id: 193,
        cn: "寻道次数",
        desc: "磁头寻道累计次数（机械盘）。",
    },
    AttrInfo {
        id: 194,
        cn: "温度",
        desc: "当前温度（℃）。SSD 建议 ≤50℃，41-50℃ 偏高，>50℃ 过热会触发降速。",
    },
    AttrInfo {
        id: 195,
        cn: "硬件 ECC 恢复",
        desc: "被 ECC 成功纠正的错误数，偶发正常，激增需关注。",
    },
    AttrInfo {
        id: 196,
        cn: "重映射事件数",
        desc: "发生重映射的事件次数，非 0 = 盘片已出现坏道事件。",
    },
    AttrInfo {
        id: 197,
        cn: "当前待映射扇区",
        desc: "等待重映射的扇区，非 0 = 盘片正在出坏道，立即备份（可能随后归零，也可能转为 5 增长）。",
    },
    AttrInfo {
        id: 198,
        cn: "离线无法修正扇区",
        desc: "离线扫描发现的无法修正扇区，非 0 = 已有坏道。",
    },
    AttrInfo {
        id: 199,
        cn: "UDMA CRC 错误",
        desc: "接口传输 CRC 校验错误，非 0 多为 SATA 线缆/接口接触不良，换线后通常不再增长。",
    },
    AttrInfo {
        id: 200,
        cn: "写入错误率",
        desc: "写入时的底层错误统计（部分厂商定义不同）。",
    },
    AttrInfo {
        id: 201,
        cn: "读取错误数",
        desc: "读取时的实时错误计数。",
    },
    AttrInfo {
        id: 202,
        cn: "寿命剩余百分比",
        desc: "SSD 寿命已用百分比（部分盘以 1-100 计，100 = 用尽）。",
    },
    AttrInfo {
        id: 203,
        cn: "寻道错误",
        desc: "运行时寻道错误计数（机械盘）。",
    },
    AttrInfo {
        id: 204,
        cn: "后台寻道错误",
        desc: "后台寻道错误计数（机械盘）。",
    },
    AttrInfo {
        id: 205,
        cn: "磁头加载次数",
        desc: "磁头加载/卸载循环（笔记本盘/SSD 厂商自定义）。",
    },
    AttrInfo {
        id: 206,
        cn: "磁头加载",
        desc: "磁头加载动作计数。",
    },
    AttrInfo {
        id: 209,
        cn: "离线搜寻成功",
        desc: "离线自检中寻道成功次数。",
    },
    AttrInfo {
        id: 210,
        cn: "离线搜寻计数",
        desc: "离线自检中寻道总次数。",
    },
    AttrInfo {
        id: 211,
        cn: "离线搜寻中止",
        desc: "离线自检被中止的次数。",
    },
    AttrInfo {
        id: 218,
        cn: "意外断电",
        desc: "意外断电计数（部分厂商 218 与 192 同义）。",
    },
    AttrInfo {
        id: 220,
        cn: "盘片位移",
        desc: "机械盘盘片位移检测结果，非 0 说明曾受冲击。",
    },
    AttrInfo {
        id: 225,
        cn: "磁头飞行高度",
        desc: "磁头悬浮高度已用比例，接近满值表示盘老化。",
    },
    AttrInfo {
        id: 226,
        cn: "磁头飞行高度",
        desc: "磁头飞行高度累计值（厂商自定义）。",
    },
    AttrInfo {
        id: 231,
        cn: "寿命剩余",
        desc: "SSD 寿命指标（与 202 同类，厂商自定义）。",
    },
    AttrInfo {
        id: 232,
        cn: "可用保留块",
        desc: "SSD 备用块剩余百分比，100 = 充足；接近阈值（多为 10）= 盘即将写满。",
    },
    AttrInfo {
        id: 233,
        cn: "SSD 剩余寿命",
        desc: "SSD 剩余寿命百分比（厂商自定义，100 = 全新）。",
    },
    AttrInfo {
        id: 234,
        cn: "平均擦除次数",
        desc: "NAND 平均擦除次数（厂商自定义）。",
    },
    AttrInfo {
        id: 235,
        cn: "厂家保留块数",
        desc: "厂商预留块数（厂商自定义）。",
    },
    AttrInfo {
        id: 240,
        cn: "磁头加载次数",
        desc: "磁头加载总次数（与 205 类似，厂商自定义）。",
    },
    AttrInfo {
        id: 241,
        cn: "累计写入 LBAs",
        desc: "SSD 累计写入扇区数，换算 TBW 与容量写入倍数，衡量写入强度。",
    },
    AttrInfo {
        id: 242,
        cn: "累计读取 LBAs",
        desc: "SSD 累计读取扇区数。",
    },
    AttrInfo {
        id: 243,
        cn: "累计写入量",
        desc: "SSD 累计写入量（部分厂商直接给出容量值）。",
    },
    AttrInfo {
        id: 244,
        cn: "累计读取量",
        desc: "SSD 累计读取量（部分厂商直接给出容量值）。",
    },
    AttrInfo {
        id: 246,
        cn: "主机写入量",
        desc: "主机累计写入量（厂商自定义）。",
    },
    AttrInfo {
        id: 247,
        cn: "主机读取量",
        desc: "主机累计读取量（厂商自定义）。",
    },
    AttrInfo {
        id: 248,
        cn: "媒体写入量",
        desc: "介质实际写入量（厂商自定义）。",
    },
    AttrInfo {
        id: 249,
        cn: "介质读取量",
        desc: "介质实际读取量（厂商自定义）。",
    },
    AttrInfo {
        id: 250,
        cn: "读取错误率",
        desc: "读取错误率（厂商自定义）。",
    },
    AttrInfo {
        id: 251,
        cn: "校正重试",
        desc: "读取校正重试次数。",
    },
    AttrInfo {
        id: 252,
        cn: "数据保留错误",
        desc: "数据保持（data retention）检测错误，非 0 需关注。",
    },
    AttrInfo {
        id: 253,
        cn: "擦除错误",
        desc: "擦除失败计数（NAND）。",
    },
    AttrInfo {
        id: 254,
        cn: "程序失败",
        desc: "编程（写入）失败计数（NAND）。",
    },
    AttrInfo {
        id: 255,
        cn: "厂商自定义",
        desc: "厂商自定义属性，含义未公开。",
    },
];

/// 按 ID 查字典；未收录返回 `None` → 调用方走「暂无说明」兜底。
pub fn attr_info(id: u16) -> Option<&'static AttrInfo> {
    ATTR_TABLE.iter().find(|a| a.id == id)
}

/// 关键属性（D10.3 ② 扇区级）：任一非 0 即严重预警。
const SECTOR_CRITICAL_IDS: &[u16] = &[5, 196, 197, 198, 187];
/// 链路类：非 0 告警（一般不等于盘坏）。
const LINK_WARNING_IDS: &[u16] = &[199, 188];

/// 关键属性（D10.3 ②：扇区类 + 链路类）——页面排序与标色用。
pub fn is_key_attr(id: u16) -> bool {
    SECTOR_CRITICAL_IDS.contains(&id) || LINK_WARNING_IDS.contains(&id)
}

// ---------------------------------------------------------------------------
// 三档参考指标（D10.3 ③）
// ---------------------------------------------------------------------------

/// 通电时间三档：`<1 万 / 1-3 万 / >3 万` 小时。
pub fn hours_tier(hours: u64) -> &'static str {
    if hours < 10_000 {
        "使用较少（<1 万小时）"
    } else if hours <= 30_000 {
        "正常使用（1-3 万小时）"
    } else {
        "服役较久（>3 万小时）"
    }
}

/// 温度三档（SSD 口径）：`≤40 正常 / 41-50 偏高 / >50 过热`。
pub fn temp_tier(c: i64) -> &'static str {
    if c <= 40 {
        "正常"
    } else if c <= 50 {
        "偏高"
    } else {
        "过热"
    }
}

// ---------------------------------------------------------------------------
// 解析与分析
// ---------------------------------------------------------------------------

/// 读 JSON 字符串字段（缺失/null → None）。
fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
}

/// 读 JSON 整数字段（数字或数字串都认）。
fn num(v: &Value, key: &str) -> Option<u64> {
    match v.get(key)? {
        Value::Number(n) => n.as_u64().or_else(|| n.as_i64().map(|i| i.max(0) as u64)),
        Value::String(t) => t.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// 读 JSON 有符号整数（数字或数字串都认）。
fn num_i64(v: &Value, key: &str) -> Option<i64> {
    match v.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(t) => t.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// 解析 `smartctl -j -a` JSON → `HealthReport`。
///
/// 错误只在「不是 JSON / 结构完全对不上」时返回；字段缺失一律 None + `notes`，
/// **禁止 panic**（GOAL 10.2 ⑦⑧⑨）。
pub fn analyze(json: &str) -> Result<HealthReport, String> {
    let text = json.trim();
    if text.is_empty() {
        return Err("SMART 输出为空".to_string());
    }
    let v: Value = serde_json::from_str(text).map_err(|e| format!("SMART JSON 解析失败：{e}"))?;
    if !v.is_object() {
        return Err("SMART JSON 顶层不是对象".to_string());
    }
    let mut notes: Vec<String> = Vec::new();

    // ---- 设备信息 ----
    let mut device = DeviceInfo::default();
    if let Some(cap) = v.get("user_capacity").and_then(|c| c.get("bytes")) {
        device.capacity_bytes = cap.as_u64();
    }
    device.logical_block_size = num(&v, "logical_block_size");
    let rotation = v.get("rotation_rate").and_then(|r| r.as_u64());
    device.is_ssd = rotation == Some(0)
        || v.get("trim")
            .and_then(|t| t.get("supported"))
            .and_then(|x| x.as_bool())
            == Some(true);
    device.model = s(&v, "model_name");
    device.serial = s(&v, "serial_number");
    device.firmware = s(&v, "firmware_version");
    device.interface = v
        .get("sata_version")
        .and_then(|x| s(x, "string"))
        .or_else(|| v.get("device").and_then(|d| s(d, "protocol")))
        .or_else(|| v.get("ata_version").and_then(|x| s(x, "string")));
    device.temperature_c = v.get("temperature").and_then(|t| num_i64(t, "current"));
    device.power_on_hours = v.get("power_on_time").and_then(|p| num(p, "hours"));
    device.power_cycle_count = num(&v, "power_cycle_count");
    if let Some(sup) = v.get("smart_support") {
        device.smart_available = sup
            .get("available")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        device.smart_enabled = sup
            .get("enabled")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
    }
    if !device.smart_available {
        notes.push("该设备未报告 SMART 支持状态（可能是 USB 桥接盘或虚拟盘）".to_string());
    }

    // ---- 整体结论 ----
    let overall = match v
        .get("smart_status")
        .and_then(|x| x.get("passed"))
        .and_then(|x| x.as_bool())
    {
        Some(true) => Overall::Passed,
        Some(false) => Overall::Failed,
        None => {
            notes.push("JSON 中没有 smart_status（设备不支持 SMART 或未读到健康自评）".to_string());
            Overall::Unknown
        }
    };

    // ---- ATA 属性表 ----
    let mut attrs: Vec<SmartAttr> = Vec::new();
    if let Some(table) = v
        .get("ata_smart_attributes")
        .and_then(|x| x.get("table"))
        .and_then(|x| x.as_array())
    {
        for a in table {
            let id = a.get("id").and_then(|x| x.as_u64()).unwrap_or(0) as u16;
            let name = s(a, "name").unwrap_or_else(|| format!("Unknown_ID_{id}"));
            let raw = a.get("raw");
            let (raw_value, raw_string) = match raw {
                Some(Value::Object(_)) => (
                    raw.and_then(|r| r.get("value"))
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0),
                    raw.and_then(|r| r.get("string"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
                Some(Value::Number(n)) => (n.as_u64().unwrap_or(0), n.to_string()),
                Some(Value::String(t)) => (t.trim().parse().unwrap_or(0), t.trim().to_string()),
                _ => (0, String::new()),
            };
            let flags = a.get("flags");
            let attr = SmartAttr {
                id,
                name,
                value: a.get("value").and_then(|x| x.as_u64()).unwrap_or(0) as u8,
                worst: a.get("worst").and_then(|x| x.as_u64()).unwrap_or(0) as u8,
                thresh: a.get("thresh").and_then(|x| x.as_u64()).unwrap_or(0) as u8,
                when_failed: a
                    .get("when_failed")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                raw_value,
                raw_string,
                prefailure: flags
                    .and_then(|f| f.get("prefailure"))
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false),
                info: attr_info(id),
            };
            attrs.push(attr);
        }
        attrs.sort_by_key(|a| a.id);
    } else if v.get("nvme_smart_health_information_log").is_none() {
        notes.push("JSON 中没有 ATA 属性表（设备可能不支持 SMART 属性查询）".to_string());
    }

    // 顶层缺字段时用属性表回填（smartctl 顶层与属性表都给这些读数，取已有者）。
    if device.power_on_hours.is_none() {
        device.power_on_hours = attrs.iter().find(|a| a.id == 9).map(|a| a.raw_value);
    }
    if device.power_cycle_count.is_none() {
        device.power_cycle_count = attrs.iter().find(|a| a.id == 12).map(|a| a.raw_value);
    }
    if device.temperature_c.is_none() {
        device.temperature_c = attrs
            .iter()
            .find(|a| a.id == 194)
            .map(|a| a.raw_value.min(i64::MAX as u64) as i64);
    }

    // ---- 关键属性预警 + FAIL 判定（D10.3 ②）----
    let mut alerts: Vec<Alert> = Vec::new();
    for a in &attrs {
        if SECTOR_CRITICAL_IDS.contains(&a.id) && a.raw_value > 0 {
            let cn = a.display_name();
            alerts.push(Alert {
                attr: Some(Box::new(a.clone())),
                severe: true,
                message: format!(
                    "{cn} = {}（{}）：盘片/闪存已出现坏道类事件，建议立即备份数据并考虑更换硬盘",
                    a.raw_value, a.id
                ),
            });
        } else if LINK_WARNING_IDS.contains(&a.id) && a.raw_value > 0 {
            let cn = a.display_name();
            alerts.push(Alert {
                attr: Some(Box::new(a.clone())),
                severe: false,
                message: format!("{cn} = {}（{}）：多为接口/线缆或链路问题，检查 SATA 线与供电后观察是否继续增长", a.raw_value, a.id),
            });
        }
    }
    let failures: Vec<SmartAttr> = attrs.iter().filter(|a| a.is_failing()).cloned().collect();
    if !failures.is_empty() {
        let names: Vec<String> = failures
            .iter()
            .map(|a| a.display_name().to_string())
            .collect();
        alerts.push(Alert {
            attr: None,
            severe: true,
            message: format!(
                "属性越过失效阈值（FAIL）：{} —— 硬件已判定该属性失效",
                names.join("、")
            ),
        });
    }
    let unknown_count = attrs.iter().filter(|a| a.info.is_none()).count();

    // ---- 参考指标（D10.3 ③）----
    let hours_tier_v = device.power_on_hours.map(hours_tier);
    let temp_tier_v = device.temperature_c.map(temp_tier);

    // TBW：Total_LBAs_Written × 逻辑扇区大小（缺任一字段就不显示）
    let mut tbw = None;
    if let Some(a) = attrs.iter().find(|a| a.id == 241 && a.raw_value > 0) {
        let block = device.logical_block_size.unwrap_or(512);
        let written_bytes = a.raw_value.saturating_mul(block);
        let read = attrs.iter().find(|x| x.id == 242).map(|x| x.raw_value);
        if written_bytes > 0 {
            tbw = Some(Tbw {
                written_bytes,
                read_bytes: read.map(|r| r.saturating_mul(block)).unwrap_or(0),
            });
        }
    }

    // ---- NVMe 健康日志 ----
    let nvme = v
        .get("nvme_smart_health_information_log")
        .map(|n| NvmeHealth {
            temperature_c: n.get("temperature").and_then(|x| x.as_i64()),
            power_on_hours: num(n, "power_on_hours"),
            power_cycles: num(n, "power_cycles"),
            percentage_used: num(n, "percentage_used"),
            available_spare: num(n, "available_spare"),
            media_errors: num(n, "media_errors"),
        });
    if let Some(nv) = &nvme {
        if device.temperature_c.is_none() {
            device.temperature_c = nv.temperature_c;
        }
        if device.power_on_hours.is_none() {
            device.power_on_hours = nv.power_on_hours;
        }
        if device.power_cycle_count.is_none() {
            device.power_cycle_count = nv.power_cycles;
        }
        if let Some(me) = nv.media_errors.filter(|&m| m > 0) {
            alerts.push(Alert {
                attr: None,
                severe: true,
                message: format!(
                    "NVMe 介质错误 media_errors = {me}：闪存已出现不可纠正错误，立即备份并更换"
                ),
            });
        }
        if let Some((used, spare)) = nv
            .percentage_used
            .zip(nv.available_spare)
            .filter(|(u, s)| *u >= 100 || *s <= 1)
        {
            alerts.push(Alert {
                attr: None,
                severe: true,
                message: format!("NVMe 寿命状态告警：已用 {used}%、备用块剩余 {spare}%"),
            });
        }
    }

    Ok(HealthReport {
        device,
        overall,
        attrs,
        alerts,
        failures,
        unknown_count,
        hours_tier: hours_tier_v,
        temp_tier: temp_tier_v,
        tbw,
        nvme,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机 `smartctl -a` 文本输出的 30 个属性 ID（2026-10-09 实测
    /// `python3 -c ...` 从 JSON 读出的真实列表）。
    const REAL_IDS: &[u16] = &[
        1, 5, 9, 12, 160, 161, 163, 164, 165, 166, 167, 168, 169, 175, 176, 177, 178, 181, 182,
        192, 194, 195, 196, 197, 198, 199, 232, 241, 242, 245,
    ];

    /// 以真机 golden 为底、按闭包改字段后重新序列化。
    ///
    /// **golden 常量本身保持 smartctl 原样文本**（2 空格缩进），各测试的字段突变
    /// 走本助手而不是 `str::replace`——避免依赖原始排版（换行/缩进一变替换就失效，
    /// 这类「测试悄悄没生效」比失败更危险）。
    fn mutate<F: FnOnce(&mut serde_json::Value)>(f: F) -> String {
        let mut v: serde_json::Value =
            serde_json::from_str(SMART_JSON_REAL).expect("golden 必须是合法 JSON");
        f(&mut v);
        serde_json::to_string(&v).expect("序列化不可失败")
    }

    /// 按 ID 定位属性表条目（突变用）。
    fn attr_mut(v: &mut serde_json::Value, id: u64) -> &mut serde_json::Value {
        let t = v["ata_smart_attributes"]["table"]
            .as_array_mut()
            .expect("golden 含属性表");
        t.iter_mut()
            .find(|a| a["id"] == id)
            .unwrap_or_else(|| panic!("属性 {id} 应存在于 golden"))
    }

    /// 本机真实数据的 JSON 化 fixture —— **真机 `smartctl -j -a /dev/sda` 原样输出**。
    ///
    /// **来源（反幻觉，GOAL 10.2-① 已闭合）**：2026-10-09 11:37 用户授权后
    /// `timeout 1800 pkexec smartctl -j -a /dev/sda > /tmp/smart_golden.json`
    /// 取回（`RC=0 SIZE=18505`），全文逐字嵌入本常量（2 空格缩进为 smartctl 原生排版）。
    /// 覆盖：设备信息（Pado 240G SSD / SN AA000000000000001784 / S1230A0）、
    /// `smart_status.passed=true`、通电 9314 小时、735 次通电、温度 40℃、
    /// **30 条属性含此前占位的 ID 164-168 真实 raw 值（876913 / 2963 / 153 / 300 / 5050）**、
    /// 累计写入 417946 LBA、读取 551042 LBA、空自检日志
    /// （`ata_smart_self_test_log.standard.count = 0`，无 `table` 键）。
    const SMART_JSON_REAL: &str = r#"{
  "json_format_version": [
    1,
    0
  ],
  "smartctl": {
    "version": [
      7,
      5
    ],
    "pre_release": false,
    "svn_revision": "5714",
    "platform_info": "loongarch64-linux-6.6.143-loong64-desktop-hwe",
    "build_info": "(local build)",
    "argv": [
      "smartctl",
      "-j",
      "-a",
      "/dev/sda"
    ],
    "drive_database_version": {
      "string": "7.5/5706"
    },
    "exit_status": 0
  },
  "local_time": {
    "time_t": 1791517053,
    "asctime": "Fri Oct  9 11:37:33 2026 CST"
  },
  "device": {
    "name": "/dev/sda",
    "info_name": "/dev/sda [SAT]",
    "type": "sat",
    "protocol": "ATA"
  },
  "model_name": "Pado 240G SSD",
  "serial_number": "AA000000000000001784",
  "firmware_version": "S1230A0",
  "user_capacity": {
    "blocks": 468862128,
    "bytes": 240057409536
  },
  "logical_block_size": 512,
  "physical_block_size": 512,
  "rotation_rate": 0,
  "form_factor": {
    "ata_value": 3,
    "name": "2.5 inches"
  },
  "trim": {
    "supported": true,
    "deterministic": true,
    "zeroed": true
  },
  "in_smartctl_database": false,
  "ata_version": {
    "string": "ACS-2 T13/2015-D revision 3",
    "major_value": 1008,
    "minor_value": 272
  },
  "sata_version": {
    "string": "SATA 3.2",
    "value": 255
  },
  "interface_speed": {
    "max": {
      "sata_value": 14,
      "string": "6.0 Gb/s",
      "units_per_second": 60,
      "bits_per_unit": 100000000
    },
    "current": {
      "sata_value": 2,
      "string": "3.0 Gb/s",
      "units_per_second": 30,
      "bits_per_unit": 100000000
    }
  },
  "smart_support": {
    "available": true,
    "enabled": true
  },
  "smart_status": {
    "passed": true
  },
  "ata_smart_data": {
    "offline_data_collection": {
      "status": {
        "value": 0,
        "string": "was never started"
      },
      "completion_seconds": 120
    },
    "self_test": {
      "status": {
        "value": 0,
        "string": "completed without error",
        "passed": true
      },
      "polling_minutes": {
        "short": 2,
        "extended": 10
      }
    },
    "capabilities": {
      "values": [
        17,
        2
      ],
      "exec_offline_immediate_supported": true,
      "offline_is_aborted_upon_new_cmd": false,
      "offline_surface_scan_supported": false,
      "self_tests_supported": true,
      "conveyance_self_test_supported": false,
      "selective_self_test_supported": false,
      "attribute_autosave_enabled": false,
      "error_logging_supported": true,
      "gp_logging_supported": true
    }
  },
  "ata_smart_attributes": {
    "revision": 1,
    "table": [
      {
        "id": 1,
        "name": "Raw_Read_Error_Rate",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 0,
          "string": "0"
        }
      },
      {
        "id": 5,
        "name": "Reallocated_Sector_Ct",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 0,
          "string": "0"
        }
      },
      {
        "id": 9,
        "name": "Power_On_Hours",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 9314,
          "string": "9314"
        }
      },
      {
        "id": 12,
        "name": "Power_Cycle_Count",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 735,
          "string": "735"
        }
      },
      {
        "id": 160,
        "name": "Unknown_Attribute",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 0,
          "string": "0"
        }
      },
      {
        "id": 161,
        "name": "Unknown_Attribute",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 51,
          "string": "PO--CK ",
          "prefailure": true,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 100,
          "string": "100"
        }
      },
      {
        "id": 163,
        "name": "Unknown_Attribute",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 35,
          "string": "35"
        }
      },
      {
        "id": 164,
        "name": "Unknown_Attribute",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 876913,
          "string": "876913"
        }
      },
      {
        "id": 165,
        "name": "Unknown_Attribute",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 2963,
          "string": "2963"
        }
      },
      {
        "id": 166,
        "name": "Unknown_Attribute",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 153,
          "string": "153"
        }
      },
      {
        "id": 167,
        "name": "Unknown_Attribute",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 300,
          "string": "300"
        }
      },
      {
        "id": 168,
        "name": "Unknown_Attribute",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 5050,
          "string": "5050"
        }
      },
      {
        "id": 169,
        "name": "Unknown_Attribute",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 95,
          "string": "95"
        }
      },
      {
        "id": 175,
        "name": "Program_Fail_Count_Chip",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 0,
          "string": "0"
        }
      },
      {
        "id": 176,
        "name": "Erase_Fail_Count_Chip",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 0,
          "string": "0"
        }
      },
      {
        "id": 177,
        "name": "Wear_Leveling_Count",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 0,
          "string": "0"
        }
      },
      {
        "id": 178,
        "name": "Used_Rsvd_Blk_Cnt_Chip",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 0,
          "string": "0"
        }
      },
      {
        "id": 181,
        "name": "Program_Fail_Cnt_Total",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 0,
          "string": "0"
        }
      },
      {
        "id": 182,
        "name": "Erase_Fail_Count_Total",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 0,
          "string": "0"
        }
      },
      {
        "id": 192,
        "name": "Power-Off_Retract_Count",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 350,
          "string": "350"
        }
      },
      {
        "id": 194,
        "name": "Temperature_Celsius",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 34,
          "string": "-O---K ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": false,
          "auto_keep": true
        },
        "raw": {
          "value": 40,
          "string": "40"
        }
      },
      {
        "id": 195,
        "name": "Hardware_ECC_Recovered",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 0,
          "string": "0"
        }
      },
      {
        "id": 196,
        "name": "Reallocated_Event_Count",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 0,
          "string": "0"
        }
      },
      {
        "id": 197,
        "name": "Current_Pending_Sector",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 0,
          "string": "0"
        }
      },
      {
        "id": 198,
        "name": "Offline_Uncorrectable",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 0,
          "string": "0"
        }
      },
      {
        "id": 199,
        "name": "UDMA_CRC_Error_Count",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 0,
          "string": "0"
        }
      },
      {
        "id": 232,
        "name": "Available_Reservd_Space",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 100,
          "string": "100"
        }
      },
      {
        "id": 241,
        "name": "Total_LBAs_Written",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 48,
          "string": "----CK ",
          "prefailure": false,
          "updated_online": false,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 417946,
          "string": "417946"
        }
      },
      {
        "id": 242,
        "name": "Total_LBAs_Read",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 48,
          "string": "----CK ",
          "prefailure": false,
          "updated_online": false,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 551042,
          "string": "551042"
        }
      },
      {
        "id": 245,
        "name": "Unknown_Attribute",
        "value": 100,
        "worst": 100,
        "thresh": 50,
        "when_failed": "",
        "flags": {
          "value": 50,
          "string": "-O--CK ",
          "prefailure": false,
          "updated_online": true,
          "performance": false,
          "error_rate": false,
          "event_count": true,
          "auto_keep": true
        },
        "raw": {
          "value": 675634,
          "string": "675634"
        }
      }
    ]
  },
  "spare_available": {
    "current_percent": 100
  },
  "power_on_time": {
    "hours": 9314
  },
  "power_cycle_count": 735,
  "endurance_used": {
    "current_percent": 0
  },
  "temperature": {
    "current": 40
  },
  "ata_smart_error_log": {
    "summary": {
      "revision": 1,
      "count": 0
    }
  },
  "ata_smart_self_test_log": {
    "standard": {
      "revision": 1,
      "count": 0
    }
  }
}"#;

    /// ① golden：设备信息全字段 + 属性表条数与关键读数。
    #[test]
    fn golden_parses_device_and_attrs() {
        let r = analyze(SMART_JSON_REAL).unwrap();
        assert_eq!(r.device.model.as_deref(), Some("Pado 240G SSD"));
        assert_eq!(r.device.serial.as_deref(), Some("AA000000000000001784"));
        assert_eq!(r.device.firmware.as_deref(), Some("S1230A0"));
        assert_eq!(r.device.capacity_bytes, Some(240_057_409_536));
        assert_eq!(r.device.logical_block_size, Some(512));
        assert!(r.device.is_ssd, "rotation_rate=0 应判为 SSD");
        assert_eq!(r.device.interface.as_deref(), Some("SATA 3.2"));
        assert!(r.device.smart_available && r.device.smart_enabled);
        assert_eq!(r.device.power_on_hours, Some(9314));
        assert_eq!(r.device.temperature_c, Some(40));
        assert_eq!(r.overall, Overall::Passed);
        assert!(r.is_healthy());
        assert_eq!(r.overall_label(), "健康");

        assert_eq!(r.attrs.len(), 30, "本机属性表 30 条");
        assert_eq!(
            r.attrs.iter().map(|a| a.id).collect::<Vec<_>>(),
            REAL_IDS.to_vec()
        );
        let hours = r.attrs.iter().find(|a| a.id == 9).unwrap();
        assert_eq!(hours.raw_value, 9314);
        assert_eq!(hours.display_name(), "通电时间");
        let temp = r.attrs.iter().find(|a| a.id == 194).unwrap();
        assert_eq!(temp.raw_value, 40);
        // 164-168 的**真实** raw 值（占位 0 已随 11:37 真机 golden 替换掉）
        let raw_of = |id: u16| {
            r.attrs
                .iter()
                .find(|a| a.id == id)
                .unwrap_or_else(|| panic!("属性 {id} 应存在"))
                .raw_value
        };
        assert_eq!(
            (
                raw_of(164),
                raw_of(165),
                raw_of(166),
                raw_of(167),
                raw_of(168)
            ),
            (876_913, 2_963, 153, 300, 5_050),
            "ID 164-168 必须是真机原值而非占位"
        );
        assert_eq!(raw_of(241), 417_946, "累计写入 LBA 真机原值");
        assert!(r.alerts.is_empty(), "健康盘不应有预警：{:?}", r.alerts);
        assert!(r.failures.is_empty());
    }

    /// ② 30 条属性逐条「命中字典或落兜底」，兜底必须给「暂无说明」。
    #[test]
    fn every_attr_has_chinese_or_fallback() {
        let r = analyze(SMART_JSON_REAL).unwrap();
        assert_eq!(r.attrs.len(), 30);
        let mut hit = 0;
        let mut fallback = 0;
        for a in &r.attrs {
            if a.info.is_some() {
                hit += 1;
                assert!(!a.display_name().is_ascii(), "命中字典应有中文：{}", a.id);
            } else {
                fallback += 1;
                assert_eq!(
                    a.name, "Unknown_Attribute",
                    "字典未收录的应是未知名属性：{}",
                    a.name
                );
            }
        }
        // 本机 30 条里 10 条是 smartctl 也认不出的 Unknown_Attribute（2026-10-09 实测）。
        assert_eq!(fallback, 10, "兜底条数");
        assert_eq!(hit, 20, "字典命中条数");
        assert_eq!(r.unknown_count, 10);
    }

    /// ③ `passed=false` → 不健康 + 红色置顶依据。
    #[test]
    fn failed_overall_detected() {
        let json = SMART_JSON_REAL.replace("\"passed\": true", "\"passed\": false");
        let r = analyze(&json).unwrap();
        assert_eq!(r.overall, Overall::Failed);
        assert!(!r.is_healthy());
        assert_eq!(r.overall_label(), "不健康");
    }

    /// ④ `value ≤ thresh` 与 `when_failed` 判 FAIL。
    #[test]
    fn failing_attr_detected_both_ways() {
        // 形态 A：value <= thresh（thresh=50，value=40）——用字段突变，不依赖排版
        let json = mutate(|v| {
            let a = attr_mut(v, 5);
            a["value"] = 40.into();
            a["worst"] = 40.into();
        });
        let r = analyze(&json).unwrap();
        assert_eq!(r.failures.len(), 1, "{:?}", r.failures);
        assert_eq!(r.failures[0].id, 5);
        assert!(r.failures[0].is_failing());
        assert!(
            r.alerts
                .iter()
                .any(|a| a.severe && a.message.contains("FAIL"))
        );

        // 形态 B：when_failed = "FAILING_NOW"
        let json = mutate(|v| {
            attr_mut(v, 9)["when_failed"] = "FAILING_NOW".into();
        });
        let r = analyze(&json).unwrap();
        assert!(r.failures.iter().any(|a| a.id == 9), "{:?}", r.failures);
    }

    /// ⑤ 关键属性非 0 预警真值表：扇区类 → 严重，CRC → 链路告警，全 0 → 无预警。
    #[test]
    fn key_attr_alert_matrix() {
        // 全 0（本机真实状态）→ 无预警
        let base = analyze(SMART_JSON_REAL).unwrap();
        assert!(base.alerts.is_empty());

        // 扇区类 197（Current_Pending_Sector）raw 置 3 → 严重预警
        let json = mutate(|v| {
            attr_mut(v, 197)["raw"]["value"] = 3.into();
        });
        let r = analyze(&json).unwrap();
        assert!(
            r.alerts.iter().any(|a| a.severe
                && a.message.contains("当前待映射扇区")
                && a.message.contains("3")),
            "{:?}",
            r.alerts
        );

        // CRC 199（UDMA_CRC_Error_Count）raw 置 7 → 链路告警（非 severe）
        let json = mutate(|v| {
            attr_mut(v, 199)["raw"]["value"] = 7.into();
        });
        let r = analyze(&json).unwrap();
        assert!(
            r.alerts
                .iter()
                .any(|a| !a.severe && a.message.contains("UDMA CRC") && a.message.contains("7")),
            "{:?}",
            r.alerts
        );
    }

    /// ⑥ 温度三档边界（40/41/50/51）。
    #[test]
    fn temperature_tiers_boundary() {
        assert_eq!(temp_tier(0), "正常");
        assert_eq!(temp_tier(40), "正常");
        assert_eq!(temp_tier(41), "偏高");
        assert_eq!(temp_tier(50), "偏高");
        assert_eq!(temp_tier(51), "过热");
        assert_eq!(temp_tier(80), "过热");
        // 真实 40℃ → 正常
        let r = analyze(SMART_JSON_REAL).unwrap();
        assert_eq!(r.temp_tier, Some("正常"));
    }

    /// ⑥ 通电时间三档边界（9999 / 10000 / 30000 / 30001）。
    #[test]
    fn hours_tiers_boundary() {
        assert!(hours_tier(9_999).contains("<1 万"));
        assert!(hours_tier(10_000).contains("1-3 万"));
        assert!(hours_tier(30_000).contains("1-3 万"));
        assert!(hours_tier(30_001).contains(">3 万"));
        let r = analyze(SMART_JSON_REAL).unwrap();
        assert_eq!(r.hours_tier, Some("使用较少（<1 万小时）"));
        assert_eq!(r.device.power_cycle_count, Some(735));
    }

    /// ⑦ 无 temperature / power_on_time 键 → None 且不 panic。
    #[test]
    fn missing_optional_fields_tolerated() {
        let json = mutate(|v| {
            v.as_object_mut().unwrap().remove("power_on_time");
            v.as_object_mut().unwrap().remove("temperature");
        });
        let r = analyze(&json).unwrap();
        // 顶层缺失 → 属性表回填（9/12/194 都在）
        assert_eq!(r.device.power_on_hours, Some(9314), "属性表 9 回填");
        assert_eq!(r.device.temperature_c, Some(40), "属性表 194 回填");
        assert_eq!(r.temp_tier, Some("正常"));
        assert_eq!(r.device.power_cycle_count, Some(735));

        // 顶层与属性表都没有 → None，绝不编造
        let bare = r#"{"smart_status":{"passed":true},"ata_smart_attributes":{"table":[]}}"#;
        let r2 = analyze(bare).unwrap();
        assert_eq!(r2.device.temperature_c, None);
        assert_eq!(r2.temp_tier, None, "无温度就不给档位（不猜）");
        assert_eq!(r2.device.power_on_hours, None);
    }

    /// ⑧ 无 ata_smart_attributes（NVMe / 不支持）→ 不 panic，属性表为空且有备注。
    #[test]
    fn no_ata_table_no_panic() {
        let json = r#"{"model_name":"X","smart_status":{"passed":true},"smart_support":{"available":true,"enabled":true}}"#;
        let r = analyze(json).unwrap();
        assert!(r.attrs.is_empty());
        assert_eq!(r.overall, Overall::Passed);
        assert!(
            r.notes.iter().any(|n| n.contains("属性表")),
            "{:?}",
            r.notes
        );
    }

    /// ⑨ 空 / 截断 / 非 JSON → Err，不 panic。
    #[test]
    fn bad_json_inputs_error() {
        assert!(analyze("").is_err());
        assert!(analyze("   ").is_err());
        assert!(analyze("not json at all").is_err());
        assert!(analyze("[1,2,3]").is_err(), "顶层非对象应报错");
        let cut = &SMART_JSON_REAL[..SMART_JSON_REAL.len() / 2];
        assert!(analyze(cut).is_err(), "截断 JSON 应报错");
        assert!(analyze("{broken").is_err());
        // 错误信息要有内容
        let err = analyze("").unwrap_err();
        assert!(err.contains("为空"), "{}", err);
    }

    /// ⑩ TBW 换算（241 LBA × 512B）与缺字段不显示。
    #[test]
    fn tbw_conversion_and_absence() {
        let r = analyze(SMART_JSON_REAL).unwrap();
        let tbw = r.tbw.expect("有 241 应给 TBW");
        assert_eq!(tbw.written_bytes, 417_946 * 512);
        assert_eq!(tbw.read_bytes, 551_042 * 512);

        // 241 的 raw 置 0（等价于设备不报告写入量）→ TBW 不显示（缺字段就不猜）
        let json = mutate(|v| {
            attr_mut(v, 241)["raw"]["value"] = 0.into();
        });
        let r = analyze(&json).unwrap();
        assert_eq!(r.tbw, None, "缺字段就不显示");
    }

    /// 未知属性兜底文案：字典查不到 → `info=None`、英文原名保留、`unknown_count` 正确。
    #[test]
    fn unknown_attr_fallback() {
        assert!(attr_info(160).is_none(), "160 不该在字典里（含义未公开）");
        assert!(attr_info(5).is_some());
        let r = analyze(SMART_JSON_REAL).unwrap();
        let a = r.attrs.iter().find(|x| x.id == 160).unwrap();
        assert!(a.info.is_none());
        assert_eq!(a.display_name(), "Unknown_Attribute", "兜底给原始英文名");
        assert_eq!(a.raw_value, 0);
        // 中文兜底文案由页面给出「暂无说明」——这里校验结构上已可判定
        assert_eq!(r.unknown_count, 10);
    }

    /// 整体结论缺失 → `Unknown`（不猜）。
    #[test]
    fn overall_unknown_when_status_missing() {
        let json = mutate(|v| {
            v.as_object_mut().unwrap().remove("smart_status");
        });
        let r = analyze(&json).unwrap();
        assert_eq!(r.overall, Overall::Unknown);
        assert_eq!(r.overall_label(), "未知");
        assert!(r.notes.iter().any(|n| n.contains("smart_status")));
    }

    /// NVMe 健康日志（无真机样本，键名按 smartctl 文档实现）：解析 + 介质错误预警 + 字段回填。
    #[test]
    fn nvme_health_log_parsed() {
        let json = r#"{
          "model_name": "Samsung SSD 980",
          "rotation_rate": 0,
          "smart_support": {"available": true, "enabled": true},
          "smart_status": {"passed": true},
          "nvme_smart_health_information_log": {
            "critical_warning": 0,
            "temperature": 42,
            "available_spare": 100,
            "available_spare_threshold": 10,
            "percentage_used": 3,
            "data_units_written": 123456,
            "power_on_hours": 5000,
            "power_cycles": 100,
            "media_errors": 2
          }
        }"#;
        let r = analyze(json).unwrap();
        let nv = r.nvme.as_ref().expect("应解析 NVMe 日志");
        assert_eq!(nv.temperature_c, Some(42));
        assert_eq!(nv.power_on_hours, Some(5000));
        assert_eq!(nv.percentage_used, Some(3));
        assert_eq!(nv.available_spare, Some(100));
        assert_eq!(nv.media_errors, Some(2));
        // 字段回填：设备温度/通电时间
        assert_eq!(r.device.temperature_c, Some(42));
        assert_eq!(r.device.power_on_hours, Some(5000));
        // 介质错误 > 0 → 严重预警
        assert!(
            r.alerts
                .iter()
                .any(|a| a.severe && a.message.contains("media_errors")),
            "{:?}",
            r.alerts
        );
        // 无 ATA 表 → 有备注但不 panic
        assert!(r.attrs.is_empty());
    }

    /// NVMe 备用块耗尽 / 寿命用尽 → 严重预警。
    #[test]
    fn nvme_life_exhausted_alerts() {
        let json = r#"{
          "smart_status": {"passed": false},
          "nvme_smart_health_information_log": {"percentage_used": 100, "available_spare": 5, "media_errors": 0}
        }"#;
        let r = analyze(json).unwrap();
        assert_eq!(r.overall, Overall::Failed);
        assert!(
            r.alerts
                .iter()
                .any(|a| a.severe && a.message.contains("寿命")),
            "{:?}",
            r.alerts
        );
    }

    /// 字典质量：无重复 ID、ID 升序、每条中文说明非空。
    #[test]
    fn attr_table_quality() {
        let mut prev = 0u16;
        for a in ATTR_TABLE {
            assert!(a.id > prev, "ID 必须升序且不重复：{} vs {}", a.id, prev);
            prev = a.id;
            assert!(!a.cn.is_empty(), "{} 中文名为空", a.id);
            assert!(a.desc.chars().count() > 5, "{} 说明过短", a.id);
            assert!(!a.cn.is_ascii(), "{} 中文名应含中文", a.id);
        }
        assert!(ATTR_TABLE.len() >= 40, "字典条数 {}", ATTR_TABLE.len());
        // 本机真实已知属性（非 Unknown 那 20 条）必须全部收录
        for id in [
            1u16, 5, 9, 12, 175, 176, 177, 178, 181, 182, 192, 194, 195, 196, 197, 198, 199, 232,
            241, 242,
        ] {
            assert!(attr_info(id).is_some(), "本机属性 {} 必须在字典里", id);
        }
    }

    /// 错误输入不 panic：单个字段类型错乱（字符串当数字等）。
    #[test]
    fn malformed_field_types_no_panic() {
        let json = r#"{
          "user_capacity": {"bytes": "not-a-number"},
          "logical_block_size": "512",
          "rotation_rate": null,
          "smart_support": {"available": "yes"},
          "smart_status": {"passed": 1},
          "power_on_time": {"hours": "9303"},
          "temperature": {"current": "40"},
          "ata_smart_attributes": {"table": [
            {"id": "oops", "name": null, "value": "100", "raw": "plain-string"},
            {"name": "No_Id_Attr", "raw": {"value": "x"}}
          ]}
        }"#;
        let r = analyze(json).unwrap();
        assert_eq!(r.device.capacity_bytes, None);
        assert_eq!(r.device.logical_block_size, Some(512), "数字串应被接受");
        assert_eq!(r.overall, Overall::Unknown, "passed=1 不是 bool → Unknown");
        assert_eq!(r.device.power_on_hours, Some(9303));
        assert_eq!(
            r.device.temperature_c,
            Some(40),
            "数值串同样接受（统一数字口径）"
        );
        assert_eq!(r.attrs.len(), 2);
        assert_eq!(r.attrs[0].raw_value, 0);
        assert_eq!(r.attrs[1].raw_value, 0);
    }
}
