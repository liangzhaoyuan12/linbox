//! GOAL 10.1 —— 磁盘枚举与 SMART 读取（纯逻辑，**禁止 `use gtk`/`use adw`/`use glib`**）。
//!
//! 分层（Phase 10 架构约束）：本文件只做「枚举 + 命令构造 + 执行 + 错误分类」，
//! JSON 解析与健康分析在 `crate::model::smart`（10.2），UI 在 `crate::page::smart`（10.3）。
//!
//! 权限模型（2026-10-09 摸底实测）：
//! - 普通用户（组不含 `disk`）直读 `/dev/sda` → `Permission denied` 且**零字段输出**；
//! - 所以读取走「**先直读，权限失败再 `pkexec` 提权兜底**」（D10.1）——用户若已加入
//!   `disk` 组就完全不弹 polkit 框；
//! - 本机 polkit 不缓存授权（GOAL 8.3 在案事实），**每次调用都带超时**（D10.1，
//!   默认 30s），超时杀进程；pkexec 未获授权时脚本不执行 → 超时态系统状态不变。
//!
//! 安全约束：`smart_argv` 只接受**枚举结果里的 `/dev/...` 路径**（白名单字符 +
//! 拒绝 `..`/`//`/尾斜杠），防 shell 注入——smartctl 虽是 argv 直传不经 shell，
//! 仍不放任任意字符串进设备参数。

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 默认读取超时（D10.1：30s，超时杀进程）。
pub const SMART_TIMEOUT: Duration = Duration::from_secs(30);

/// 提权（pkexec）那一跳的超时：**必须留出人工授权时间**。
///
/// 2026-10-09 实测教训：页面用 30s（D10.1 的生产口径）跑
/// `pkexec smartctl -j -l selftest /dev/sda` 时，授权框等人点密码的时间常超过
/// 30s → 命令被 `timeout` 杀掉、页面报「读取超时」，用户随后再点框也已无客户端，
/// 还会遗留死框把后续 pkexec 全变成 `Not authorized`。
/// 直读那一跳仍用调用方给的超时（本机 0.03s 内返回）。
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(120);

/// 伪设备名前缀：即使 `type == "disk"` 也不发 SMART 查询（loop/ram/zram/光驱等）。
const PSEUDO_PREFIXES: &[&str] = &["loop", "ram", "zram", "rom", "fd", "sr", "dm-", "md"];

/// 一块物理磁盘（`lsblk` 的 `type == "disk"` 节点；分区不收）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskDevice {
    /// 绝对设备路径，如 `/dev/sda`（发给 smartctl 的唯一参数）。
    pub path: String,
    /// 设备名，如 `sda`。
    pub name: String,
    /// 容量文本（lsblk 原样，如 `223.6G`；可能缺失）。
    pub size: String,
    /// 型号（NVMe/虚拟盘常为 null）。
    pub model: Option<String>,
    /// 传输协议（`sata`/`usb`/`nvme`/null）。
    pub tran: Option<String>,
}

/// 读取失败的分类（10.1 要求区分四类，页面按此分流错误态文案）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailKind {
    /// `smartctl` 可执行文件不存在（未安装 smartmontools）。
    NotInstalled,
    /// 打开设备权限不足（提权后仍失败/未授权）。
    Permission,
    /// 超时被杀（含 pkexec 等待授权超时）。
    Timeout,
    /// 进程有输出但不是 JSON（或 JSON 结构不符）。
    BadJson,
    /// 其他错误（启动失败、退出码异常且无有效输出等）。
    Other,
}

/// 读取失败：`kind` 供页面分流，`message` 为可直接展示的中文原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmartFailure {
    pub kind: FailKind,
    pub message: String,
}

impl SmartFailure {
    pub fn new(kind: FailKind, message: impl Into<String>) -> Self {
        SmartFailure {
            kind,
            message: message.into(),
        }
    }

    /// 错误态标题（页面 StatusPage 用）。
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            FailKind::NotInstalled => "未安装 smartmontools",
            FailKind::Permission => "无权限读取设备",
            FailKind::Timeout => "读取超时",
            FailKind::BadJson => "输出解析失败",
            FailKind::Other => "读取失败",
        }
    }

    /// 错误态描述（页面 StatusPage 用；带可操作指引）。
    pub fn kind_hint(&self) -> &'static str {
        match self.kind {
            FailKind::NotInstalled => {
                "请先安装 smartmontools（如 `dnf install smartmontools`）后重试。"
            }
            FailKind::Permission => {
                "读取 SMART 需要特权。点下方「授权读取该盘」由系统弹出授权框；或把当前用户加入 `disk` 组（需重新登录）后即可免授权直读。"
            }
            FailKind::Timeout => {
                "命令在超时时间内未返回（polkit 授权框可能未点击）。系统状态未改变，可重试。"
            }
            FailKind::BadJson => {
                "smartctl 返回了非 JSON 内容，请检查 smartctl 版本（需 ≥7.0 支持 `-j`）。"
            }
            FailKind::Other => {
                "请重试；若持续失败，可在终端手动执行 `smartctl -a <设备>` 查看原始报错。"
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 枚举（10.1）
// ---------------------------------------------------------------------------

/// 读取字符字段：lsblk 对缺失字段给 `null`，对数值列给 number（`-b` 时），统一成 String。
fn lsblk_str(n: &serde_json::Value, key: &str) -> Option<String> {
    match n.get(key) {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(serde_json::Value::Number(x)) => Some(x.to_string()),
        _ => None,
    }
}

/// 伪设备判定（按名前缀）。真实盘名（`sda`/`nvme0n1`/`mmcblk0`/`vda`/`xvda`）不命中。
pub fn is_pseudo_disk(name: &str) -> bool {
    PSEUDO_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// 解析 `lsblk -J -o NAME,PATH,TYPE,SIZE,MODEL,TRAN` 输出 → 物理磁盘清单。
///
/// 收集规则（GOAL D10.2）：**`type == "disk"` 且非伪设备名**；`children` 里的
/// `type == "part"` 分区不收（递归遍历保证嵌套形态也能收到盘节点）。
pub fn parse_lsblk_disks(text: &str) -> Result<Vec<DiskDevice>, String> {
    let v: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("lsblk JSON 解析失败：{e}"))?;
    let arr = v
        .get("blockdevices")
        .and_then(|x| x.as_array())
        .ok_or_else(|| "lsblk JSON 缺少 blockdevices 数组".to_string())?;
    let mut out = Vec::new();
    collect_disks(arr, &mut out);
    // 路径去重（同一盘理论上只出现一次，防御 lsblk 嵌套重复）。
    let mut seen = std::collections::HashSet::new();
    out.retain(|d| seen.insert(d.path.clone()));
    Ok(out)
}

fn collect_disks(nodes: &[serde_json::Value], out: &mut Vec<DiskDevice>) {
    for n in nodes {
        let ty = n.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let name = n.get("name").and_then(|x| x.as_str()).unwrap_or("");
        if ty == "disk" && !name.is_empty() && !is_pseudo_disk(name) {
            out.push(DiskDevice {
                path: lsblk_str(n, "path").unwrap_or_else(|| format!("/dev/{name}")),
                name: name.to_string(),
                size: lsblk_str(n, "size").unwrap_or_default(),
                model: lsblk_str(n, "model"),
                tran: lsblk_str(n, "tran"),
            });
        }
        if let Some(children) = n.get("children").and_then(|x| x.as_array()) {
            collect_disks(children, out);
        }
    }
}

/// 跑 `lsblk -J -o NAME,PATH,TYPE,SIZE,MODEL,TRAN`（后台线程调用，禁止 UI 线程）。
pub fn list_disks() -> Result<Vec<DiskDevice>, String> {
    let out = Command::new("lsblk")
        .args(["-J", "-o", "NAME,PATH,TYPE,SIZE,MODEL,TRAN"])
        .output()
        .map_err(|e| format!("无法启动 lsblk：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "lsblk 退出码 {}：{}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8(out.stdout).map_err(|e| format!("lsblk 输出非 UTF-8：{e}"))?;
    parse_lsblk_disks(&text)
}

// ---------------------------------------------------------------------------
// 命令构造（10.1）
// ---------------------------------------------------------------------------

/// 设备路径白名单校验：必须是 `/dev/` 下的绝对路径，只允许
/// `[A-Za-z0-9/._+-]`，禁 `..`、`//`、尾斜杠、空段（防注入）。
pub fn validate_dev_path(dev: &str) -> Result<(), String> {
    if !dev.starts_with("/dev/") {
        return Err(format!("设备路径必须在 /dev/ 下：{dev}"));
    }
    if dev.ends_with('/') || dev.contains("//") || dev.contains("..") {
        return Err(format!("设备路径形态非法：{dev}"));
    }
    if dev.len() <= "/dev/".len() {
        return Err(format!("设备路径为空段：{dev}"));
    }
    if !dev
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._+-".contains(c))
    {
        return Err(format!("设备路径含非法字符：{dev}"));
    }
    Ok(())
}

/// 构造直读命令：`smartctl -j -a <dev>`（用户已加入 `disk` 组时免提权）。
pub fn smart_argv(dev: &str) -> Result<Vec<String>, String> {
    validate_dev_path(dev)?;
    Ok(vec![
        "smartctl".to_string(),
        "-j".to_string(),
        "-a".to_string(),
        dev.to_string(),
    ])
}

/// 构造提权读取命令：`pkexec smartctl -j -a <dev>`（直读 `Permission denied` 后的兜底）。
pub fn smart_argv_pkexec(dev: &str) -> Result<Vec<String>, String> {
    let mut a = smart_argv(dev)?;
    a.insert(0, "pkexec".to_string());
    Ok(a)
}

/// `smartctl --version` 探测（未安装 → false；页面据此显示安装指引）。
pub fn smartctl_available() -> bool {
    run_capture(
        &["smartctl".to_string(), "--version".to_string()],
        Duration::from_secs(5),
    )
    .is_ok()
}

// ---------------------------------------------------------------------------
// 执行（10.1）
// ---------------------------------------------------------------------------

/// 带超时的命令执行：捕获 stdout/stderr，超时杀进程。
///
/// stdout/stderr 由独立线程读取，避免管道写满导致子进程死锁
/// （`wait_with_output` 前若不读，>64KB 输出会把子进程卡住）。
fn run_capture(
    argv: &[String],
    timeout: Duration,
) -> Result<(String, String, Option<i32>), SmartFailure> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let kind = if e.kind() == std::io::ErrorKind::NotFound {
                FailKind::NotInstalled
            } else {
                FailKind::Other
            };
            return Err(SmartFailure::new(
                kind,
                format!("启动 {} 失败：{}", argv[0], e),
            ));
        }
    };

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stdout_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stderr_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(SmartFailure::new(
                        FailKind::Timeout,
                        format!(
                            "命令超时（{}s）已终止：{}",
                            timeout.as_secs(),
                            argv.join(" ")
                        ),
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return Err(SmartFailure::new(
                    FailKind::Other,
                    format!("等待命令失败：{e}"),
                ));
            }
        }
    };

    let stdout = String::from_utf8_lossy(&out_handle.join().unwrap_or_default()).to_string();
    let stderr = String::from_utf8_lossy(&err_handle.join().unwrap_or_default()).to_string();
    Ok((stdout, stderr, status.code()))
}

/// 从 `smartctl -j` 输出里识别错误：`smartctl.messages` 中 severity=error 的条目
/// 或 `exit_status` 的「设备打开失败」位（0x2）。
///
/// 返回 `None` = 读取成功（有可用数据，其余 exit_status 位交 10.2 的健康分析解读）。
fn json_failure(json: &str) -> Option<SmartFailure> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let sc = v.get("smartctl")?;
    let mut errs: Vec<String> = Vec::new();
    if let Some(arr) = sc.get("messages").and_then(|x| x.as_array()) {
        for m in arr {
            let sev = m.get("severity").and_then(|x| x.as_str()).unwrap_or("");
            let text = m.get("string").and_then(|x| x.as_str()).unwrap_or("");
            if sev == "error" && !text.is_empty() {
                errs.push(text.to_string());
            }
        }
    }
    if !errs.is_empty() {
        let text = errs.join("；");
        let kind = if text.contains("Permission denied")
            || text.contains("requires root")
            || text.contains("euid")
        {
            FailKind::Permission
        } else {
            FailKind::Other
        };
        return Some(SmartFailure::new(kind, snip(&text, 400)));
    }
    let exit = sc.get("exit_status").and_then(|x| x.as_u64()).unwrap_or(0);
    if exit & 0x2 != 0 {
        return Some(SmartFailure::new(
            FailKind::Other,
            format!("smartctl 打开设备失败（exit_status={exit}）"),
        ));
    }
    None
}

/// 取最后一行非空文本（smartctl 的结论行通常在末尾）。
fn last_nonempty(s: &str) -> String {
    s.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 截断展示片段：单行超长（如整块非 JSON 输出）只留前 `max` 字符 + 省略号。
fn snip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…（共 {} 字符）", s.chars().count())
    }
}

/// 把 stderr/退出码翻译成失败分类（`Permission denied` 单独识别，
/// smartctl 对不存在设备也报 open failed → 归 `Other`）。
fn classify(stderr: &str, code: Option<i32>, argv: &[String]) -> SmartFailure {
    let tail = last_nonempty(stderr);
    let msg = if tail.is_empty() {
        format!(
            "{} 退出码 {}",
            argv[0],
            code.map(|c| c.to_string())
                .unwrap_or_else(|| "-".to_string())
        )
    } else {
        snip(&tail, 400)
    };
    if stderr.contains("Permission denied")
        || stderr.contains("requires root")
        || stderr.contains("euid")
        || msg.contains("权限")
    {
        SmartFailure::new(FailKind::Permission, msg)
    } else {
        SmartFailure::new(FailKind::Other, msg)
    }
}

/// 执行一条已构造好的读取命令，返回 stdout（JSON 文本）。
///
/// 非 0 退出码不直接判失败——smartctl 的 `exit_status` 是**位掩码**
/// （D10.1：有 JSON 就按 JSON 内容判定），只要 stdout 是 JSON 就算读取成功。
pub fn run_smart_argv(argv: &[String], timeout: Duration) -> Result<String, SmartFailure> {
    let (stdout, stderr, code) = run_capture(argv, timeout)?;
    let trimmed = stdout.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        // 关键实测（2026-10-09 修正摸底）：`smartctl -j` 模式下**权限失败也返回
        // JSON**（stdout、`exit_status=2` + `messages[].severity=error`、stderr 为空），
        // 文本模式才是 stderr 报错。所以 JSON 到手不能直接当成功——先读 smartctl
        // 自己的错误消息，权限类错误必须抛出以触发 pkexec 兜底（D10.1）。
        return match json_failure(trimmed) {
            Some(f) => Err(f),
            None => Ok(stdout),
        };
    }
    // 没拿到 JSON：先看 stderr（未安装在 spawn 阶段已拦，这里多为权限/设备问题）。
    if !last_nonempty(&stderr).is_empty() {
        return Err(classify(&stderr, code, argv));
    }
    // stderr 为空 → 输出形态不符（如 smartctl 过老不支持 `-j`，或设备无输出）。
    if trimmed.is_empty() {
        return Err(SmartFailure::new(
            FailKind::Other,
            format!(
                "{} 退出码 {}，无任何输出",
                argv[0],
                code.map(|c| c.to_string())
                    .unwrap_or_else(|| "-".to_string())
            ),
        ));
    }
    Err(SmartFailure::new(
        FailKind::BadJson,
        format!("输出不是 JSON：{}", snip(&last_nonempty(&stdout), 400)),
    ))
}

/// 只直读、**不自动提权**（页面启动/静默刷新用：失败零弹窗）。
///
/// D10.1 的提权兜底由 [`run_smart`] 提供；页面在用户显式点「授权读取」时才走它，
/// 否则应用一启动就会弹 polkit 框（本机 polkit 不缓存授权，GOAL 8.3 在案事实）。
pub fn run_smart_direct(dev: &str, timeout: Duration) -> Result<String, SmartFailure> {
    let argv = smart_argv(dev).map_err(|e| SmartFailure::new(FailKind::Other, e))?;
    run_smart_argv(&argv, timeout)
}

/// 读取一块盘的 SMART JSON（D10.1：**先直读，`Permission` 失败再 pkexec 兜底**）。
///
/// 返回 `smartctl -j -a` 的完整 JSON 文本；失败按 `FailKind` 分类。
/// 后台线程调用，禁止 UI 线程（GOAL 3.2）。
pub fn run_smart(dev: &str, timeout: Duration) -> Result<String, SmartFailure> {
    let to_failure = |e: String| SmartFailure::new(FailKind::Other, e);
    // 直读优先、`Permission` 时 pkexec 兜底（D10.1），与自检共用同一机制。
    run_with_priv_fallback(
        smart_argv(dev).map_err(to_failure)?,
        smart_argv_pkexec(dev).map_err(to_failure)?,
        timeout,
    )
}

// ---------------------------------------------------------------------------
// 自检（GOAL 10.4 · D10.5 定稿：纳入 v1）
// ---------------------------------------------------------------------------

/// 自检类型（smartctl `-t` 的 TEST，帮助文本实测：`offline, short, long,
/// conveyance, force, …`；v1 只开短/长两种）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelfTestKind {
    /// 短自检（本机推荐轮询时长 2 分钟）。
    Short,
    /// 长自检（本机推荐轮询时长 10 分钟）。
    Long,
}

impl SelfTestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SelfTestKind::Short => "short",
            SelfTestKind::Long => "long",
        }
    }

    /// 中文名（弹窗与按钮文案）。
    pub fn cn(self) -> &'static str {
        match self {
            SelfTestKind::Short => "短自检",
            SelfTestKind::Long => "长自检",
        }
    }
}

/// 自检状态四态（**未知状态绝不编造**）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestState {
    /// `Self-test routine in progress`（执行中）。
    Running,
    /// `Completed without error`。
    Passed,
    /// 读失败 / 中断 / 未完成等失败类。
    Failed,
    /// 字典外状态原文 → 显示原文，不猜。
    Unknown,
}

impl TestState {
    pub fn cn(self) -> &'static str {
        match self {
            TestState::Running => "执行中",
            TestState::Passed => "通过",
            TestState::Failed => "未通过",
            TestState::Unknown => "状态未知",
        }
    }
}

/// 单次自检结果条目（`smartctl -j -l selftest` 的
/// `ata_smart_self_test_log.table[]`；键名取自本机 `/sbin/smartctl`
/// 二进制 `strings` 实测，非猜测）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfTestEntry {
    /// 序号（最新一次 = 最大 num）。
    pub num: u32,
    /// 测试类型（`type.string`，如 `Short offline`）。
    pub kind: String,
    /// 状态原文（`status.string`）。
    pub status: String,
    /// 归一化状态。
    pub state: TestState,
    /// 执行中时的剩余百分比（`remaining_percent`，非执行中为 None）。
    pub remaining_percent: Option<u8>,
    /// 该次自检完成时的通电小时数（`lifetime_hours`；本机真机日志实测有此键，
    /// 部分固件不报 → None）。
    pub lifetime_hours: Option<u64>,
}

/// 把 smartctl 的状态原文归类成四态。
///
/// 判定顺序不可换：`Completed without error` 里含 `error`，必须先命中「通过」；
/// 关键词全部来自本机 smartctl 二进制 `strings` 实测
/// （`Completed without error` / `Completed: read failure` /
/// `Completed, segment failed` / `Self-test routine in progress`）。
pub fn classify_status(status: &str) -> TestState {
    let s = status.trim();
    if s.contains("without error") {
        return TestState::Passed;
    }
    if s.contains("in progress") {
        return TestState::Running;
    }
    const FAIL_WORDS: &[&str] = &[
        "failure",
        "failed",
        "read failure",
        "segment failed",
        "did not complete",
        "not completed",
        "aborted",
        "cancelled",
        "interrupted",
        "fatal",
    ];
    if FAIL_WORDS
        .iter()
        .any(|w| s.to_ascii_lowercase().contains(w))
    {
        return TestState::Failed;
    }
    TestState::Unknown
}

/// 从状态原文里抠执行中百分比（如 `… unknown (0% complete)`）。
fn remaining_percent_of(status: &str) -> Option<u8> {
    let start = status.find('(')?;
    let tail = &status[start + 1..];
    let end = tail.find('%')?;
    tail[..end].trim().parse::<u8>().ok()
}

/// 启动自检的命令：`smartctl -j -t short|long <dev>`（`-j` 是为了让权限/
/// 设备错误也走 stdout JSON，复用 `json_failure` 分类）。
pub fn selftest_start_argv(dev: &str, kind: SelfTestKind) -> Result<Vec<String>, String> {
    validate_dev_path(dev)?;
    Ok(vec![
        "smartctl".to_string(),
        "-j".to_string(),
        "-t".to_string(),
        kind.as_str().to_string(),
        dev.to_string(),
    ])
}

/// 启动自检的提权命令：`pkexec smartctl -j -t short|long <dev>`。
pub fn selftest_start_argv_pkexec(dev: &str, kind: SelfTestKind) -> Result<Vec<String>, String> {
    let mut a = selftest_start_argv(dev, kind)?;
    a.insert(0, "pkexec".to_string());
    Ok(a)
}

/// 读自检日志的命令：`smartctl -j -l selftest <dev>`。
pub fn selftest_log_argv(dev: &str) -> Result<Vec<String>, String> {
    validate_dev_path(dev)?;
    Ok(vec![
        "smartctl".to_string(),
        "-j".to_string(),
        "-l".to_string(),
        "selftest".to_string(),
        dev.to_string(),
    ])
}

/// 读自检日志的提权命令。
pub fn selftest_log_argv_pkexec(dev: &str) -> Result<Vec<String>, String> {
    let mut a = selftest_log_argv(dev)?;
    a.insert(0, "pkexec".to_string());
    Ok(a)
}

/// 解析 `smartctl -j -l selftest` 的 JSON → 自检记录（**最新在前**）。
///
/// 找不到日志键（NVMe / 从未跑过 / 结构不符）→ `Ok(vec![])`；
/// 不是 JSON / 顶层不是对象 → `Err`（不 panic）。
pub fn parse_selftest_log(json: &str) -> Result<Vec<SelfTestEntry>, String> {
    let text = json.trim();
    if text.is_empty() {
        return Err("自检日志为空".to_string());
    }
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("自检日志 JSON 解析失败：{e}"))?;
    if !v.is_object() {
        return Err("自检日志顶层不是对象".to_string());
    }
    // ATA 键名 = ata_smart_self_test_log（二进制 strings 实测）；**真机的表嵌在
    // `standard` 下**（2026-10-09 真机 golden 实测——初版按平铺取值拿到的是空表，
    // 正是真机 fixture 抓出来的解析 bug），平铺形态作为兼容兜底。
    // NVMe = nvme_self_test_log（结构不同且无样本）→ v1 只认 ATA 表，缺键按「无记录」返回。
    let log = v.get("ata_smart_self_test_log");
    let table = log
        .and_then(|x| x.get("standard"))
        .and_then(|x| x.get("table"))
        .or_else(|| log.and_then(|x| x.get("table")))
        .and_then(|x| x.as_array());
    let Some(table) = table else {
        return Ok(Vec::new());
    };
    let mut out: Vec<SelfTestEntry> = Vec::with_capacity(table.len());
    for e in table {
        let status = e
            .get("status")
            .and_then(|x| x.get("string"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let state = classify_status(&status);
        out.push(SelfTestEntry {
            num: e.get("num").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            kind: e
                .get("type")
                .and_then(|x| x.get("string"))
                .and_then(|x| x.as_str())
                .unwrap_or("未知类型")
                .to_string(),
            remaining_percent: if state == TestState::Running {
                e.get("remaining_percent")
                    .and_then(|x| x.as_u64())
                    .map(|p| p.min(100) as u8)
                    .or_else(|| remaining_percent_of(&status))
            } else {
                None
            },
            status,
            state,
            lifetime_hours: e.get("lifetime_hours").and_then(|x| x.as_u64()),
        });
    }
    // 最新在前（num 大的更新；缺 num 的保持原序靠后）
    out.sort_by_key(|e| std::cmp::Reverse(e.num));
    Ok(out)
}

/// 通用：直读失败（权限）→ 提权兜底，其余错误原样返回。
fn run_with_priv_fallback(
    direct: Vec<String>,
    elevated: Vec<String>,
    timeout: Duration,
) -> Result<String, SmartFailure> {
    match run_smart_argv(&direct, timeout) {
        Ok(s) => Ok(s),
        // 提权那一跳固定用 AUTH_TIMEOUT（120s）：给人点授权框留时间（见常量注释）。
        Err(e) if e.kind == FailKind::Permission => run_smart_argv(&elevated, AUTH_TIMEOUT),
        Err(e) => Err(e),
    }
}

/// 启动一次自检（D10.5：**写设备动作，必须由用户显式点击**；返回 smartctl 的 JSON 文本）。
///
/// 后台线程调用，禁止 UI 线程；带超时。成功仅表示「已受理启动」，
/// 真实结论要等日志（`run_selftest_log` → `parse_selftest_log`）。
pub fn run_selftest_start(
    dev: &str,
    kind: SelfTestKind,
    timeout: Duration,
) -> Result<String, SmartFailure> {
    let to_failure = |e: String| SmartFailure::new(FailKind::Other, e);
    run_with_priv_fallback(
        selftest_start_argv(dev, kind).map_err(to_failure)?,
        selftest_start_argv_pkexec(dev, kind).map_err(to_failure)?,
        timeout,
    )
}

/// 读自检日志（返回 `smartctl -j -l selftest` 的 JSON 文本）。
pub fn run_selftest_log(dev: &str, timeout: Duration) -> Result<String, SmartFailure> {
    let to_failure = |e: String| SmartFailure::new(FailKind::Other, e);
    run_with_priv_fallback(
        selftest_log_argv(dev).map_err(to_failure)?,
        selftest_log_argv_pkexec(dev).map_err(to_failure)?,
        timeout,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机真实 `lsblk -J -o NAME,PATH,TYPE,SIZE,MODEL,TRAN` 输出（2026-10-09 抄录）。
    const LSBLK_REAL: &str = r#"{
   "blockdevices": [
      {
         "name": "sda",
         "path": "/dev/sda",
         "type": "disk",
         "size": "223.6G",
         "model": "Pado 240G SSD",
         "tran": "sata",
         "children": [
            {
               "name": "sda1",
               "path": "/dev/sda1",
               "type": "part",
               "size": "300M",
               "model": null,
               "tran": null
            },{
               "name": "sda2",
               "path": "/dev/sda2",
               "type": "part",
               "size": "10.6G",
               "model": null,
               "tran": null
            },{
               "name": "sda3",
               "path": "/dev/sda3",
               "type": "part",
               "size": "212.7G",
               "model": null,
               "tran": null
            }
         ]
      }
   ]
}"#;

    /// 单盘 + 分区：只收整盘节点，分区一条不收，字段齐全。
    #[test]
    fn parse_single_disk_excludes_partitions() {
        let disks = parse_lsblk_disks(LSBLK_REAL).unwrap();
        assert_eq!(disks.len(), 1, "分区不得混入：{:?}", disks);
        let d = &disks[0];
        assert_eq!(d.path, "/dev/sda");
        assert_eq!(d.name, "sda");
        assert_eq!(d.size, "223.6G");
        assert_eq!(d.model.as_deref(), Some("Pado 240G SSD"));
        assert_eq!(d.tran.as_deref(), Some("sata"));
    }

    /// 多盘形态（SATA + NVMe + USB 外置）：本机没有，用 fixture 覆盖（GOAL 阻塞项）。
    #[test]
    fn parse_multi_disk_shapes() {
        let text = r#"{"blockdevices":[
          {"name":"sda","path":"/dev/sda","type":"disk","size":"931.5G","model":"ST1000DM010","tran":"sata",
           "children":[{"name":"sda1","path":"/dev/sda1","type":"part","size":"500G"}]},
          {"name":"nvme0n1","path":"/dev/nvme0n1","type":"disk","size":"476.9G","model":"Samsung SSD 980","tran":null,
           "children":[{"name":"nvme0n1p1","path":"/dev/nvme0n1p1","type":"part","size":"100M"}]},
          {"name":"sdb","path":"/dev/sdb","type":"disk","size":"1.8T","model":"External HDD","tran":"usb"}
        ]}"#;
        let disks = parse_lsblk_disks(text).unwrap();
        assert_eq!(disks.len(), 3);
        assert_eq!(disks[0].path, "/dev/sda");
        assert_eq!(disks[1].name, "nvme0n1");
        assert_eq!(disks[1].tran, None, "NVMe 无 tran 字段应为 None");
        assert_eq!(disks[2].tran.as_deref(), Some("usb"));
        assert!(disks.iter().all(|d| d.path.starts_with("/dev/")));
    }

    /// 伪设备排除：loop/ram/zrom/zram 即使 type=disk 也不收；嵌套在 disk 下的分区同样不收。
    #[test]
    fn pseudo_devices_excluded() {
        let text = r#"{"blockdevices":[
          {"name":"loop0","path":"/dev/loop0","type":"disk","size":"4M"},
          {"name":"ram0","path":"/dev/ram0","type":"disk","size":"4M"},
          {"name":"zram0","path":"/dev/zram0","type":"disk","size":"8G"},
          {"name":"sr0","path":"/dev/sr0","type":"disk","size":"102M"},
          {"name":"rom0","path":"/dev/rom0","type":"disk","size":"1M"},
          {"name":"fd0","path":"/dev/fd0","type":"disk","size":"1.4M"},
          {"name":"sda","path":"/dev/sda","type":"disk","size":"223.6G",
           "children":[{"name":"loop1","path":"/dev/loop1","type":"disk","size":"4M"}]}
        ]}"#;
        let disks = parse_lsblk_disks(text).unwrap();
        assert_eq!(disks.len(), 1, "只应剩 sda：{:?}", disks);
        assert_eq!(disks[0].name, "sda");
        assert!(is_pseudo_disk("zram0") && is_pseudo_disk("loop7"));
        assert!(!is_pseudo_disk("sda") && !is_pseudo_disk("nvme0n1") && !is_pseudo_disk("mmcblk0"));
    }

    /// 字段缺失与 null：size/model/tran 缺失不 panic，path 缺失回退 /dev/<name>。
    #[test]
    fn missing_fields_tolerated() {
        let text = r#"{"blockdevices":[
          {"name":"vda","path":"/dev/vda","type":"disk","size":null,"model":null,"tran":null},
          {"name":"nvme1n1","type":"disk","size":"1000204886016"}
        ]}"#;
        let disks = parse_lsblk_disks(text).unwrap();
        assert_eq!(disks.len(), 2);
        assert_eq!(disks[0].size, "");
        assert_eq!(disks[0].model, None);
        assert_eq!(disks[1].path, "/dev/nvme1n1", "path 缺失按 name 回退");
        assert_eq!(disks[1].size, "1000204886016", "数值型 size 转字符串");
    }

    /// 空/坏输入：空文本、非 JSON、缺 blockdevices、空数组。
    #[test]
    fn bad_lsblk_input_errors() {
        assert!(parse_lsblk_disks("").is_err());
        assert!(parse_lsblk_disks("not json").is_err());
        assert!(parse_lsblk_disks(r#"{"foo":1}"#).is_err());
        let empty = parse_lsblk_disks(r#"{"blockdevices":[]}"#).unwrap();
        assert!(empty.is_empty());
    }

    /// 路径去重：同一 path 出现两次只留一条。
    #[test]
    fn duplicate_path_deduped() {
        let text = r#"{"blockdevices":[
          {"name":"sda","path":"/dev/sda","type":"disk","size":"100G"},
          {"name":"sda","path":"/dev/sda","type":"disk","size":"100G"}
        ]}"#;
        assert_eq!(parse_lsblk_disks(text).unwrap().len(), 1);
    }

    /// 命令构造：直读与提权两种 argv 形态。
    #[test]
    fn argv_shapes() {
        let a = smart_argv("/dev/sda").unwrap();
        assert_eq!(a, ["smartctl", "-j", "-a", "/dev/sda"]);
        let p = smart_argv_pkexec("/dev/nvme0n1").unwrap();
        assert_eq!(p, ["pkexec", "smartctl", "-j", "-a", "/dev/nvme0n1"]);
    }

    /// 注入拒绝：6 种非法设备参数一律 Err（白名单外不进 argv）。
    #[test]
    fn argv_injection_rejected() {
        let bad = [
            "",
            "/dev/",
            "/dev/sda;rm -rf /",
            "/dev/sda$(reboot)",
            "../../etc/passwd",
            "sda",
            "/dev/sda|tee",
            "/dev/../sda",
            "/dev//sda",
            "/dev/sda/",
            "/tmp/sda",
        ];
        for b in bad {
            assert!(smart_argv(b).is_err(), "应拒绝非法设备参数：{:?}", b);
        }
        // 合法字符面：分区、nvme、mmc、带 + 的路径。
        for ok in ["/dev/sda1", "/dev/nvme0n1p2", "/dev/mmcblk0", "/dev/dm-0"] {
            assert!(smart_argv(ok).is_ok(), "应放行：{}", ok);
        }
    }

    /// 未安装：argv[0] 不存在 → `NotInstalled`。
    #[test]
    fn missing_binary_is_not_installed() {
        let argv = vec![
            "definitely-not-a-real-binary-xyz".to_string(),
            "--version".to_string(),
        ];
        let err = run_smart_argv(&argv, Duration::from_secs(5)).unwrap_err();
        assert_eq!(err.kind, FailKind::NotInstalled, "{}", err.message);
        assert_eq!(err.kind_label(), "未安装 smartmontools");
        assert!(err.kind_hint().contains("smartmontools"));
    }

    /// 超时：sleep 30 配 300ms 超时 → `Timeout` 且进程被杀。
    #[test]
    fn timeout_kills_and_classifies() {
        let argv = vec!["sleep".to_string(), "30".to_string()];
        let err = run_smart_argv(&argv, Duration::from_millis(300)).unwrap_err();
        assert_eq!(err.kind, FailKind::Timeout, "{}", err.message);
        assert!(err.message.contains("超时"), "{}", err.message);
    }

    /// 权限失败分类：stderr 含 Permission denied → `Permission`（触发 pkexec 兜底的依据）。
    #[test]
    fn permission_stderr_classified() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo 'Smartctl open device: /dev/sda failed: Permission denied' >&2; exit 2"
                .to_string(),
        ];
        let err = run_smart_argv(&argv, Duration::from_secs(5)).unwrap_err();
        assert_eq!(err.kind, FailKind::Permission, "{}", err.message);
        assert_eq!(err.kind_label(), "无权限读取设备");
    }

    /// 非 JSON 输出 → `Other`/`BadJson` 路径不 panic，错误信息带原始 stderr。
    #[test]
    fn non_json_output_is_error() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo 'some plain text'".to_string(),
        ];
        let err = run_smart_argv(&argv, Duration::from_secs(5)).unwrap_err();
        assert_eq!(err.kind, FailKind::BadJson, "{}", err.message);
        assert!(err.message.contains("plain text"), "{}", err.message);
        assert_eq!(err.kind_label(), "输出解析失败");
    }

    /// smartctl 有 JSON 就算成功：退出码非 0（位掩码）但 stdout 是 JSON → Ok。
    #[test]
    fn json_wins_over_exit_code() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo '{\"smartctl\":{\"exit_status\":8}}'; exit 8".to_string(),
        ];
        let out = run_smart_argv(&argv, Duration::from_secs(5)).unwrap();
        assert!(out.contains("exit_status"));
    }

    /// 大输出不死锁（>64KB 管道缓冲）：独立读线程方案的回归护栏。
    #[test]
    fn large_output_not_deadlock() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "yes AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | head -c 200000; printf TAIL_MARKER\\n"
                .to_string(),
        ];
        let err = run_smart_argv(&argv, Duration::from_secs(10)).unwrap_err();
        // 读满 200KB（> 64KB 管道缓冲）后仍能拿到最后一行 → 未死锁、未超时；
        // 若没被读线程抽干，子进程会卡在写管道上，本断言会以 Timeout 失败。
        assert_eq!(err.kind, FailKind::BadJson, "{}", err.message);
        assert!(err.message.contains("TAIL_MARKER"), "{}", err.message);
    }

    /// 真实权限失败形态（2026-10-09 实测抄录）：`smartctl -j` 把错误写进 **stdout JSON**
    /// （`exit_status=2` + messages severity=error、stderr 为空）→ 必须判 Permission，
    /// 触发 pkexec 兜底，绝不能当成功数据返回。
    #[test]
    fn json_permission_error_is_failure_not_success() {
        let real = r#"{
  "json_format_version": [1, 0],
  "smartctl": {
    "version": [7, 5],
    "argv": ["smartctl", "-j", "-a", "/dev/sda"],
    "messages": [
      {"string": "Smartctl open device: /dev/sda failed: Permission denied", "severity": "error"}
    ],
    "exit_status": 2
  }
}"#;
        // ① 直接喂真实形态 JSON 给错误识别函数。
        let f = json_failure(real).expect("权限错误 JSON 必须被识别为失败");
        assert_eq!(f.kind, FailKind::Permission, "{}", f.message);
        assert!(f.message.contains("Permission denied"), "{}", f.message);
        // ② 端到端：sh 复现「错误 JSON 走 stdout、stderr 为空」的形态。
        let payload = "{\"smartctl\":{\"messages\":[{\"string\":\"Smartctl open device: /dev/sda failed: Permission denied\",\"severity\":\"error\"}],\"exit_status\":2}}";
        let sh = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("printf '%s' '{}'", payload),
        ];
        let err = run_smart_argv(&sh, Duration::from_secs(5)).unwrap_err();
        assert_eq!(err.kind, FailKind::Permission, "{}", err.message);
        assert!(err.message.contains("Permission denied"), "{}", err.message);
    }

    /// 非权限类 error 消息 → Other（不误触发提权兜底）。
    #[test]
    fn json_other_error_is_other_kind() {
        let sh = vec![
            "sh".to_string(),
            "-c".to_string(),
            r#"printf '%s' '{"smartctl":{"messages":[{"string":"ATA INQUIRY failed","severity":"error"}],"exit_status":64}}'"#.to_string(),
        ];
        let err = run_smart_argv(&sh, Duration::from_secs(5)).unwrap_err();
        assert_eq!(err.kind, FailKind::Other, "{}", err.message);
        assert!(err.message.contains("INQUIRY"), "{}", err.message);
    }

    /// 只有 exit_status=2（设备打开失败位）而无 messages → Other，不 panic。
    #[test]
    fn json_exit_status_open_failed_without_messages() {
        let sh = vec![
            "sh".to_string(),
            "-c".to_string(),
            r#"printf '%s' '{"smartctl":{"exit_status":2}}'"#.to_string(),
        ];
        let err = run_smart_argv(&sh, Duration::from_secs(5)).unwrap_err();
        assert_eq!(err.kind, FailKind::Other, "{}", err.message);
        assert!(err.message.contains("打开设备失败"), "{}", err.message);
    }

    /// 成功读取：JSON 无 error 消息、exit_status 无打开失败位 → Ok 原文返回
    /// （`exit_status=8` 这类读日志失败位交 10.2 健康分析，不在本层拦截）。
    #[test]
    fn json_clean_read_returns_ok() {
        let sh = vec![
            "sh".to_string(),
            "-c".to_string(),
            r#"printf '%s' '{"smartctl":{"exit_status":8},"smart_status":{"passed":true}}'"#
                .to_string(),
        ];
        let out = run_smart_argv(&sh, Duration::from_secs(5)).unwrap();
        assert!(out.contains("smart_status"));
    }

    /// 提权腿的超时必须显著长于直读腿（2026-10-09 实测：授权框等人点密码
    /// 常 >30s，用 30s 会把命令杀掉、页面报「读取超时」）。
    #[test]
    fn auth_timeout_leaves_room_for_human_auth() {
        assert!(AUTH_TIMEOUT > SMART_TIMEOUT, "提权腿必须更长");
        assert!(
            AUTH_TIMEOUT >= Duration::from_secs(60),
            "至少给人工授权留 60s，实测点框经常超过 30s"
        );
        assert_eq!(SMART_TIMEOUT, Duration::from_secs(30), "D10.1 生产口径不变");
    }

    /// 自检命令构造：启动（短/长）与读日志两种 argv 形态 + 提权形态。
    #[test]
    fn selftest_argv_shapes() {
        let s = selftest_start_argv("/dev/sda", SelfTestKind::Short).unwrap();
        assert_eq!(s, ["smartctl", "-j", "-t", "short", "/dev/sda"]);
        let l = selftest_start_argv("/dev/sda", SelfTestKind::Long).unwrap();
        assert_eq!(l, ["smartctl", "-j", "-t", "long", "/dev/sda"]);
        let p = selftest_start_argv_pkexec("/dev/nvme0n1", SelfTestKind::Short).unwrap();
        assert_eq!(
            p,
            ["pkexec", "smartctl", "-j", "-t", "short", "/dev/nvme0n1"]
        );
        let log = selftest_log_argv("/dev/sda").unwrap();
        assert_eq!(log, ["smartctl", "-j", "-l", "selftest", "/dev/sda"]);
        let lp = selftest_log_argv_pkexec("/dev/sda").unwrap();
        assert_eq!(
            lp,
            ["pkexec", "smartctl", "-j", "-l", "selftest", "/dev/sda"]
        );
        assert_eq!(SelfTestKind::Short.cn(), "短自检");
        assert_eq!(SelfTestKind::Long.cn(), "长自检");
    }

    /// 自检命令的注入拒绝：三个构造器都必须过设备路径白名单。
    #[test]
    fn selftest_argv_rejects_injection() {
        for bad in [
            "",
            "sda",
            "/dev/sda;reboot",
            "/dev/$(id)",
            "/dev/../sda",
            "/tmp/x",
        ] {
            assert!(
                selftest_start_argv(bad, SelfTestKind::Short).is_err(),
                "启动命令应拒绝：{bad}"
            );
            assert!(
                selftest_start_argv_pkexec(bad, SelfTestKind::Long).is_err(),
                "提权启动命令应拒绝：{bad}"
            );
            assert!(selftest_log_argv(bad).is_err(), "日志命令应拒绝：{bad}");
        }
    }

    /// 状态归类真值表（关键词全部来自本机 smartctl 二进制 strings 实测）。
    #[test]
    fn classify_status_truth_table() {
        assert_eq!(
            classify_status("Completed without error"),
            TestState::Passed
        );
        // 判定顺序：without error 必须先于 error 类关键词命中
        assert_eq!(
            classify_status("Self-test routine in progress"),
            TestState::Running
        );
        assert_eq!(
            classify_status("Completed: read failure"),
            TestState::Failed
        );
        assert_eq!(
            classify_status("Completed, segment failed"),
            TestState::Failed
        );
        assert_eq!(classify_status("Was aborted by host"), TestState::Failed);
        assert_eq!(classify_status("Did not complete"), TestState::Failed);
        // 字典外原文 → 未知，不编造
        assert_eq!(
            classify_status("Some vendor specific status"),
            TestState::Unknown
        );
        assert_eq!(classify_status(""), TestState::Unknown);
        assert_eq!(classify_status("   "), TestState::Unknown);
        // 中文标签
        assert_eq!(TestState::Running.cn(), "执行中");
        assert_eq!(TestState::Unknown.cn(), "状态未知");
    }

    /// 自检日志解析：三态 fixture（执行中/通过/失败）+ 最新在前 + 百分比两来源。
    #[test]
    fn parse_selftest_log_fixture() {
        let json = r#"{
          "json_format_version": [1, 0],
          "smartctl": {"version": [7, 5], "exit_status": 0},
          "ata_smart_self_test_log": {
            "revision": 1,
            "table": [
              {"num": 3, "type": {"string": "Short offline", "value": 0},
               "status": {"string": "Self-test routine in progress, routine completion time unknown (0% complete)", "value": 1},
               "remaining_percent": 0},
              {"num": 2, "type": {"string": "Short offline", "value": 0},
               "status": {"string": "Completed without error", "value": 0}},
              {"num": 1, "type": {"string": "Extended offline", "value": 1},
               "status": {"string": "Completed: read failure", "value": 3},
               "lba_of_first_error": "0x1234"}
            ]
          }
        }"#;
        let v = parse_selftest_log(json).unwrap();
        assert_eq!(v.len(), 3);
        // 最新在前
        assert_eq!(v[0].num, 3);
        assert_eq!(v[0].state, TestState::Running);
        assert_eq!(v[0].remaining_percent, Some(0));
        assert!(v[0].status.contains("in progress"));
        assert_eq!(v[1].num, 2);
        assert_eq!(v[1].state, TestState::Passed);
        assert_eq!(v[1].remaining_percent, None, "非执行中不给百分比");
        assert_eq!(v[2].num, 1);
        assert_eq!(v[2].kind, "Extended offline");
        assert_eq!(v[2].state, TestState::Failed);

        // 百分比的第二来源：字段为 null（拿不到）时从状态原文括号里抠
        let json = json.replace(r#""remaining_percent": 0"#, r#""remaining_percent": null"#);
        assert!(
            json.contains(r#""remaining_percent": null"#),
            "字段必须真的被改成 null"
        );
        let v = parse_selftest_log(&json).unwrap();
        assert_eq!(v[0].state, TestState::Running);
        assert_eq!(v[0].remaining_percent, Some(0), "从 (0% complete) 抠出");
        let json = json.replace("(0% complete)", "(37% complete)");
        let v = parse_selftest_log(&json).unwrap();
        assert_eq!(v[0].remaining_percent, Some(37));
    }

    /// 真机自检日志（启动前，`count = 0` 且无 `table` 键）——
    /// 2026-10-09 11:56 `pkexec smartctl -j -l selftest /dev/sda` 原样输出。
    const SELFTEST_BEFORE_REAL: &str = r#"{
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
      "-l",
      "selftest",
      "/dev/sda"
    ],
    "drive_database_version": {
      "string": "7.5/5706"
    },
    "exit_status": 0
  },
  "local_time": {
    "time_t": 1791518203,
    "asctime": "Fri Oct  9 11:56:43 2026 CST"
  },
  "device": {
    "name": "/dev/sda",
    "info_name": "/dev/sda [SAT]",
    "type": "sat",
    "protocol": "ATA"
  },
  "ata_smart_self_test_log": {
    "standard": {
      "revision": 1,
      "count": 0
    }
  }
}"#;

    /// 真机自检日志（短自检跑完后）——同日 11:59 原样输出：一条
    /// `Short offline / Completed without error`，`lifetime_hours = 9315`，
    /// 条目无 `num`、无 `remaining_percent`（真机形态与合成 fixture 不同）。
    const SELFTEST_AFTER_REAL: &str = r#"{
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
      "-l",
      "selftest",
      "/dev/sda"
    ],
    "drive_database_version": {
      "string": "7.5/5706"
    },
    "exit_status": 0
  },
  "local_time": {
    "time_t": 1791518353,
    "asctime": "Fri Oct  9 11:59:13 2026 CST"
  },
  "device": {
    "name": "/dev/sda",
    "info_name": "/dev/sda [SAT]",
    "type": "sat",
    "protocol": "ATA"
  },
  "ata_smart_self_test_log": {
    "standard": {
      "revision": 1,
      "table": [
        {
          "type": {
            "value": 1,
            "string": "Short offline"
          },
          "status": {
            "value": 0,
            "string": "Completed without error",
            "passed": true
          },
          "lifetime_hours": 9315
        }
      ],
      "count": 1,
      "error_count_total": 0,
      "error_count_outdated": 0
    }
  }
}"#;

    /// 真机前后对照（GOAL 10.4 取证）：启动前空表 → 跑完 1 条「通过」。
    #[test]
    fn real_selftest_log_before_after() {
        let before = parse_selftest_log(SELFTEST_BEFORE_REAL).unwrap();
        assert!(before.is_empty(), "启动前应无记录：{before:?}");

        let after = parse_selftest_log(SELFTEST_AFTER_REAL).unwrap();
        assert_eq!(after.len(), 1, "{after:?}");
        let e = &after[0];
        assert_eq!(e.kind, "Short offline");
        assert_eq!(e.status, "Completed without error");
        assert_eq!(e.state, TestState::Passed, "必须识别为通过");
        assert_eq!(e.lifetime_hours, Some(9315));
        assert_eq!(e.num, 0, "真机条目无 num 字段 → 0（页面据此不显示第 0 次）");
        assert_eq!(e.remaining_percent, None, "已完成的自检不该有剩余百分比");
        assert_eq!(after[0].state.cn(), "通过");
    }

    /// 自检日志的空/缺键/坏输入：缺键与空表 → 空记录，坏输入 → Err，不 panic。
    #[test]
    fn parse_selftest_log_empty_and_bad() {
        // 从未跑过 / 键不存在（含 NVMe 无 ATA 表）
        assert_eq!(
            parse_selftest_log(r#"{"smartctl":{"exit_status":0}}"#).unwrap(),
            Vec::<SelfTestEntry>::new()
        );
        assert_eq!(
            parse_selftest_log(r#"{"ata_smart_self_test_log":{"revision":1,"table":[]}}"#).unwrap(),
            Vec::<SelfTestEntry>::new()
        );
        // 坏输入
        assert!(parse_selftest_log("").is_err());
        assert!(parse_selftest_log("not json").is_err());
        assert!(parse_selftest_log("[1,2]").is_err());
        assert!(parse_selftest_log("{broken").is_err());
        // 未知状态条目照收，state=Unknown 显示原文
        let json = r#"{"ata_smart_self_test_log":{"table":[{"num":1,"type":{"string":"Short offline"},"status":{"string":"Weird vendor status"}}]}}"#;
        let v = parse_selftest_log(json).unwrap();
        assert_eq!(v[0].state, TestState::Unknown);
        assert_eq!(v[0].status, "Weird vendor status");
    }

    /// 本机真机 smoke（需提权，可能弹 polkit 授权框）：
    /// `cargo test --lib utils::smart::tests::real_machine_smoke -- --ignored --nocapture`
    #[test]
    #[ignore = "真机 + 提权，手动执行"]
    fn real_machine_smoke() {
        let disks = list_disks().expect("lsblk 枚举失败");
        println!("枚举到 {} 块盘：", disks.len());
        for d in &disks {
            println!(
                "  {} size={} model={:?} tran={:?}",
                d.path, d.size, d.model, d.tran
            );
        }
        assert!(!disks.is_empty(), "本机至少有 sda");
        assert!(disks.iter().all(|d| d.name != "zram0"), "zram 不得进清单");

        let dev = disks[0].path.clone();
        // 真机取证放宽到 120s：pkexec 会弹授权框等人点（D10.1 的 30s 是生产读取
        // 口径，取证测试需要留出人工授权时间）。
        let json = run_smart(&dev, Duration::from_secs(120)).expect("读取 SMART 失败");
        assert!(json.trim_start().starts_with('{'));
        assert!(
            json.contains("smart_status"),
            "应拿到健康字段（= 真实数据，非权限错误 JSON）"
        );
        assert!(
            json.contains("ata_smart_attributes"),
            "本机 SATA SSD 应有属性表"
        );
        println!(
            "smartctl JSON 前 300 字符：{}",
            &json[..json.len().min(300)]
        );
        println!("smartctl_available={}", smartctl_available());
    }
}
