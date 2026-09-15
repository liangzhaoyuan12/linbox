//! systemd 管理逻辑层（纯逻辑，不依赖 GTK / libadwaita / glib）。
//!
//! 负责：调用 `systemctl` / `journalctl`、解析其文本输出、以及需要提权时
//! 通过 `pkexec` 提升到 root 执行（参考 `utils::env_editor` 的权限模型）。
//!
//! 作用域（[`Scope`]）：系统级单元要 root 才能启停，走 `pkexec systemctl`；
//! 用户级单元走 `systemctl --user`，由用户自己的 systemd 实例管理，
//! **永远不提权**（提权反而会操作错实例）。
//!
//! 约束（见 `docs/项目结构规划书.md` §3.7）：本文件禁止 `use gtk` / `use adw` /
//! `use glib`，输入输出均为数据。

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::model::systemd::{JournalOutput, PowerAction, Scope, Timer, Unit, UnitDetail};
use crate::utils::env_editor::current_uid;

// ---------------------------------------------------------------------------
// 命令执行
// ---------------------------------------------------------------------------

/// 一次 `systemctl show` 最多带多少个单元名（补描述用）。
/// 只为防 ARG_MAX，实际 126 个单元名约 3 KB。
const SHOW_BATCH: usize = 200;

/// 当前进程是否已是 root（提权判断用）。
pub fn is_root() -> bool {
    current_uid() == 0
}

/// 构造 `systemctl` 基础参数：先作用域，再子命令。
fn systemctl_args(scope: Scope, args: &[String]) -> Vec<String> {
    let mut v: Vec<String> = scope.args().iter().map(|s| s.to_string()).collect();
    v.extend(args.iter().cloned());
    v
}

/// 运行 `systemctl`。
///
/// `elevated` 为 true、作用域是系统级且当前非 root 时，通过 `pkexec` 提权执行
/// （用户级作用域由用户自己的 systemd 管理，提权会操作错实例，故忽略 elevated）。
///
/// 返回 `(是否成功, 合并后的 stdout+stderr)`；合并输出便于把错误信息展示给用户。
fn run_systemctl(scope: Scope, args: &[String], elevated: bool) -> Result<(bool, String), String> {
    let all = systemctl_args(scope, args);
    let mut cmd = if elevated && scope.needs_root() && current_uid() != 0 {
        let mut c = Command::new("pkexec");
        c.arg("systemctl");
        c.args(&all);
        c
    } else {
        let mut c = Command::new("systemctl");
        c.args(&all);
        c
    };
    cmd.stdin(Stdio::null());
    let out = cmd
        .output()
        .map_err(|e| format!("无法执行 systemctl（请确认已安装 systemd 与 polkit）：{e}"))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok((out.status.success(), combined.trim().to_string()))
}

/// 运行 `journalctl`（只读）。
///
/// 返回 `(stdout, stderr)`——**两者不能合并**：普通用户运行 journalctl 时
/// stderr 会带一段「你看不到系统日志」的 Hint，混进正文会显示成一行日志。
fn run_journalctl(args: &[String], elevated: bool) -> Result<(String, String), String> {
    let mut cmd = if elevated && current_uid() != 0 {
        let mut c = Command::new("pkexec");
        c.arg("journalctl");
        c.args(args);
        c
    } else {
        let mut c = Command::new("journalctl");
        c.args(args);
        c
    };
    cmd.stdin(Stdio::null());
    let out = cmd
        .output()
        .map_err(|e| format!("无法执行 journalctl：{e}"))?;
    Ok((
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}

// ---------------------------------------------------------------------------
// 输出解析（纯函数，便于单测）
// ---------------------------------------------------------------------------

/// 解析 `systemctl list-units --all --no-legend` 的输出。
///
/// 列定义（systemd 257 实测表头）：`UNIT LOAD ACTIVE SUB DESCRIPTION...`——
/// **没有 JOB 列**，所以描述从第 5 个字段开始。
///
/// 两个必须处理的坑：
/// 1. `not-found` / `failed` 的单元行首会多一个 `●` 标记列（即使输出不是终端）。
///    不剥掉的话单元名会变成 `●`，且所有这类单元会折叠成同一条记录。
/// 2. description 里含空格，所以只能按「前 4 列固定、其余全部是描述」来切。
fn parse_list_units(list_out: &str) -> Vec<Unit> {
    let mut units = Vec::new();
    for line in list_out.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        // 单元名一定含 '.'（类型后缀）；用第一个含 '.' 的字段跳过 `●` / `*` 标记列，
        // 同时天然跳过结尾统计行（"... units listed." 前面没有含 '.' 的字段）
        let Some(off) = f.iter().position(|t| t.contains('.')) else {
            continue;
        };
        let f = &f[off..];
        // UNIT LOAD ACTIVE SUB（描述可能为空，允许只有 4 列）
        if f.len() < 4 {
            continue;
        }
        units.push(Unit {
            name: f[0].to_string(),
            load: f[1].to_string(),
            active: f[2].to_string(),
            enabled: String::new(),
            description: f[4..].join(" "),
        });
    }
    units
}

/// 解析 `systemctl list-unit-files --no-legend` 的输出（列：`UNIT FILE STATE PRESET`）。
fn parse_unit_files(file_out: &str) -> Vec<(String, String)> {
    let mut v = Vec::new();
    for line in file_out.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 2 {
            continue;
        }
        v.push((f[0].to_string(), f[1].to_string()));
    }
    v
}

/// 解析 `systemctl show -p Id,Names,Description`（可含多个单元，块之间以空行分隔）。
///
/// 返回 `(该单元的全部名字, Description)`。别名单元（如 `dbus-org.freedesktop.login1.service`
/// → `systemd-logind.service`）返回的 `Id` 是**真实单元名**，所以必须按 `Names`
/// 给所有别名都建索引，否则这些单元的描述永远补不上。
fn parse_show_names_desc(out: &str) -> Vec<(Vec<String>, String)> {
    let mut v: Vec<(Vec<String>, String)> = Vec::new();
    let (mut names, mut desc) = (Vec::new(), String::new());
    for line in out.lines() {
        if let Some(x) = line.strip_prefix("Id=") {
            if !names.is_empty() {
                v.push((std::mem::take(&mut names), std::mem::take(&mut desc)));
            }
            names.push(x.to_string());
        } else if let Some(x) = line.strip_prefix("Names=") {
            for n in x.split_whitespace() {
                if !names.iter().any(|e| e == n) {
                    names.push(n.to_string());
                }
            }
        } else if let Some(x) = line.strip_prefix("Description=") {
            desc = x.to_string();
        }
    }
    if !names.is_empty() {
        v.push((names, desc));
    }
    v
}

/// 是否是「真模板」单元名（`getty@.service` 这种 @ 后面直接跟类型后缀的）。
///
/// `systemctl show` 遇到真模板会整条命令报错退出
/// （`Unit name getty@.service is neither a valid invocation ID nor unit name`），
/// 因此补描述时必须先排除它们；带实例的（`app-x@autostart.service`）正常保留。
fn is_template(name: &str) -> bool {
    match name.split_once('@') {
        Some((_, rest)) => rest.starts_with('.'),
        None => false,
    }
}

/// 微秒时长 → 中文可读（`3 小时 41 分`）。
fn human_duration(us: u64) -> String {
    let s = us / 1_000_000;
    match s {
        0..=59 => format!("{s} 秒"),
        60..=3599 => {
            let (m, sec) = (s / 60, s % 60);
            if sec == 0 {
                format!("{m} 分")
            } else {
                format!("{m} 分 {sec} 秒")
            }
        }
        3600..=86399 => {
            let (h, m) = (s / 3600, (s % 3600) / 60);
            if m == 0 {
                format!("{h} 小时")
            } else {
                format!("{h} 小时 {m} 分")
            }
        }
        _ => {
            let (d, h) = (s / 86400, (s % 86400) / 3600);
            if h == 0 {
                format!("{d} 天")
            } else {
                format!("{d} 天 {h} 小时")
            }
        }
    }
}

/// 解析 `systemctl list-timers --output=json` 的输出。
///
/// `now_us` 传当前时间（微秒），便于单测固定时间。JSON 里：
/// - `next` / `last` 是**真实时间**（µs，可缺省 = null）
/// - `left` / `passed` 是**单调时钟**值（µs since boot），不能当"还剩多久"用，
///   所以剩余/已过去都由本函数自己用 `now_us - next/last` 算。
fn parse_timers_json(text: &str, now_us: u64) -> Result<Vec<Timer>, String> {
    let v: Value = serde_json::from_str(text.trim())
        .map_err(|e| format!("list-timers 输出不是合法 JSON（需要 systemd 253+）：{e}"))?;
    let arr = v.as_array().ok_or("list-timers 输出不是数组")?;
    let mut out = Vec::new();
    for item in arr {
        let s = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let num = |k: &str| item.get(k).and_then(Value::as_u64).unwrap_or(0);
        let next = num("next");
        let last = num("last");
        out.push(Timer {
            unit: s("unit"),
            activates: s("activates"),
            next_in: if next > now_us {
                human_duration(next - now_us)
            } else {
                String::new()
            },
            last_ago: if last > 0 && now_us > last {
                human_duration(now_us - last)
            } else {
                String::new()
            },
        });
    }
    Ok(out)
}

/// 当前时间（微秒 since epoch）。
fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 列出单元
// ---------------------------------------------------------------------------

/// 列出某作用域、某类型的全部单元（含未运行的），并把「启用状态」从
/// `list-unit-files` 合并进来。`unit_type` 为空字符串时列出所有类型。
pub fn list_units(scope: Scope, unit_type: &str) -> Result<Vec<Unit>, String> {
    // 1) `list-units --all`：拿到 load / active / 描述（只覆盖「已加载」的单元）
    let mut list_args = vec![
        "list-units".to_string(),
        "--all".to_string(),
        "--no-legend".to_string(),
        "--no-pager".to_string(),
    ];
    if !unit_type.is_empty() {
        list_args.push(format!("--type={unit_type}"));
    }
    let (_, list_out) = run_systemctl(scope, &list_args, false)?;

    let mut map: HashMap<String, Unit> = parse_list_units(&list_out)
        .into_iter()
        .map(|u| (u.name.clone(), u))
        .collect();

    // 2) `list-unit-files`：拿到启用状态；未出现在 list-units 的（未加载）补进来
    let mut file_args = vec![
        "list-unit-files".to_string(),
        "--no-legend".to_string(),
        "--no-pager".to_string(),
    ];
    if !unit_type.is_empty() {
        file_args.push(format!("--type={unit_type}"));
    }
    let (_, file_out) = run_systemctl(scope, &file_args, false)?;
    for (name, state) in parse_unit_files(&file_out) {
        match map.get_mut(&name) {
            Some(u) => u.enabled = state,
            None => {
                map.insert(
                    name.clone(),
                    Unit {
                        name,
                        // 未加载：LoadState 未知（不代表 not-found）
                        load: String::new(),
                        active: "inactive".to_string(),
                        enabled: state,
                        description: String::new(),
                    },
                );
            }
        }
    }

    let mut units: Vec<Unit> = map.into_values().collect();

    // 3) 补描述：`list-units` 只覆盖已加载单元，而 `list-unit-files` 不带描述，
    //    所以「未加载」的单元（本机 126/259 个 service）会全是空描述。
    fill_descriptions(scope, &mut units);

    units.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(units)
}

/// 为没有描述的单元批量补 `Description`。
///
/// 一次 `systemctl show` 可带多个单元名（实测 84 个 0.39 s）。
/// 分批 + 排除真模板，避免个别不合法的名字让整条命令失败、全军覆没。
fn fill_descriptions(scope: Scope, units: &mut [Unit]) {
    let targets: Vec<String> = units
        .iter()
        .filter(|u| u.description.is_empty() && !is_template(&u.name))
        .map(|u| u.name.clone())
        .collect();
    if targets.is_empty() {
        return;
    }

    let mut got: HashMap<String, String> = HashMap::new();
    for chunk in targets.chunks(SHOW_BATCH) {
        let mut args = vec!["show".to_string()];
        args.extend(chunk.iter().cloned());
        args.push("--no-pager".to_string());
        args.push("--property=Id,Names,Description".to_string());
        let Ok((_, out)) = run_systemctl(scope, &args, false) else {
            continue;
        };
        for (names, desc) in parse_show_names_desc(&out) {
            // Description 恰好等于某个单元名 ⇒ 该单元文件其实没有 Description
            if desc.is_empty() || names.iter().any(|n| *n == desc) {
                continue;
            }
            // 别名单元：Description 要挂到它的每一个名字上
            for n in names {
                got.insert(n, desc.clone());
            }
        }
    }

    for u in units.iter_mut() {
        if let Some(d) = got.get(&u.name) {
            u.description = d.clone();
        }
    }
}

/// 列出定时器（`systemctl list-timers`，含未排定的）。
pub fn list_timers(scope: Scope) -> Result<Vec<Timer>, String> {
    let args = vec![
        "list-timers".to_string(),
        "--all".to_string(),
        "--no-legend".to_string(),
        "--no-pager".to_string(),
        "--output=json".to_string(),
    ];
    let (ok, out) = run_systemctl(scope, &args, false)?;
    if !ok && out.is_empty() {
        return Err("无法列出定时器".to_string());
    }
    if out.trim().is_empty() {
        // 没有任何定时器时 systemctl 输出空数组或空文本
        return Ok(Vec::new());
    }
    parse_timers_json(&out, now_us())
}

// ---------------------------------------------------------------------------
// 单元详情 / 单元文件
// ---------------------------------------------------------------------------

/// `systemctl show` 要取的属性。
const SHOW_PROPERTIES: &str = "Id,Description,LoadState,ActiveState,SubState,MainPID,\
     MemoryCurrent,FragmentPath,UnitFileState,ActiveEnterTimestamp,NRestarts";

/// 取单元的详情：从 `systemctl show` 解析关键字段，并附上 `systemctl status` 原文。
pub fn unit_status(scope: Scope, name: &str) -> Result<UnitDetail, String> {
    let show_args = vec![
        "show".to_string(),
        name.to_string(),
        "--no-pager".to_string(),
        format!("--property={SHOW_PROPERTIES}"),
    ];
    let (ok, show_out) = run_systemctl(scope, &show_args, false)?;

    let mut detail = UnitDetail::default();
    if ok {
        for line in show_out.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k {
                // unit 不存在时 systemd 会把 Description 回显成单元名，视为「无描述」
                "Description" => {
                    if v != name {
                        detail.description = v.to_string();
                    }
                }
                "LoadState" => detail.load = v.to_string(),
                "ActiveState" => detail.active = v.to_string(),
                "SubState" => detail.sub = v.to_string(),
                "MainPID" => detail.main_pid = v.to_string(),
                "MemoryCurrent" => detail.memory = human_bytes(v),
                "FragmentPath" => detail.fragment_path = v.to_string(),
                "UnitFileState" => detail.unit_file_state = v.to_string(),
                "ActiveEnterTimestamp" => detail.active_enter = v.to_string(),
                "NRestarts" => detail.restarts = v.to_string(),
                _ => {}
            }
        }
    }

    // 状态原文（去掉分页控制序列）
    let status_args = vec![
        "status".to_string(),
        name.to_string(),
        "--no-pager".to_string(),
        "-n".to_string(),
        "0".to_string(),
    ];
    let (_, status_text) = run_systemctl(scope, &status_args, false)?;
    detail.status_text = strip_ansi(&status_text);
    Ok(detail)
}

/// 查看单元文件内容（`systemctl cat`，只读、无需提权）。
pub fn unit_file(scope: Scope, name: &str) -> Result<String, String> {
    let args = vec![
        "cat".to_string(),
        name.to_string(),
        "--no-pager".to_string(),
    ];
    let (_, out) = run_systemctl(scope, &args, false)?;
    Ok(strip_ansi(&out))
}

/// 字节数转人类可读（如 `12 MB`）；非数字 / 未设置 / 无限制返回空串。
fn human_bytes(v: &str) -> String {
    if v.is_empty() || v == "[not set]" || v == "0" || v.eq_ignore_ascii_case("infinity") {
        return String::new();
    }
    match v.parse::<u64>() {
        // u64::MAX 是 systemd 表示「无限制」的哨兵值，不能当成 16 EB 显示
        Ok(n) if n > 0 && n != u64::MAX => {
            const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
            let mut n = n;
            let mut i = 0;
            while n >= 1024 && i + 1 < UNITS.len() {
                n /= 1024;
                i += 1;
            }
            format!("{n} {}", UNITS[i])
        }
        Ok(_) => String::new(),
        _ => v.to_string(),
    }
}

/// 去除 `systemctl status` 输出里的 ANSI 转义（颜色/光标序列）。
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // 跳过直到 'm' 或合法 CSI 终止符
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------------
// 日志
// ---------------------------------------------------------------------------

/// 取单元最近日志（`journalctl -u <name> -n 500`，`this_boot_only` 时追加 `-b`）。
///
/// 用户级单元用 `--user --user-unit=<name>` 读用户日志，否则普通用户看不到自己的
/// 用户单元日志（用户级单元的日志在用户 journal 里，不在系统 journal 里）。
pub fn unit_logs(
    scope: Scope,
    name: &str,
    this_boot_only: bool,
    elevated: bool,
) -> Result<JournalOutput, String> {
    let mut args: Vec<String> = Vec::new();
    match scope {
        Scope::System => {
            args.push("-u".to_string());
            args.push(name.to_string());
        }
        Scope::User => {
            args.push("--user".to_string());
            args.push(format!("--user-unit={name}"));
        }
    }
    args.push("-n".to_string());
    args.push("500".to_string());
    args.push("--no-pager".to_string());
    if this_boot_only {
        args.push("-b".to_string());
    }
    let (text, hint) = run_journalctl(&args, elevated)?;
    Ok(JournalOutput { text, hint })
}

/// 日志快照查询选项。
pub struct JournalOpts {
    /// 作用域：系统日志 / 当前用户日志。
    pub scope: Scope,
    /// 优先级（0..=7，`-p`）。`None` 表示不过滤（显示全部级别）。
    pub priority: Option<u8>,
    /// 仅本次启动（`-b`）。
    pub boot_only: bool,
    /// 按单元名过滤（`-u` / `--user-unit`，为空则不过滤）。
    pub unit: Option<String>,
    /// 取最近多少行（`-n`）。
    pub lines: u32,
    /// 通过 pkexec 提权到 root 读取（普通用户看不到系统日志时用）。
    pub elevated: bool,
}

/// 取日志快照（`journalctl`），支持作用域 / 优先级 / 仅本启动 / 单元名 / 行数过滤。
pub fn journal_snapshot(opts: &JournalOpts) -> Result<JournalOutput, String> {
    let mut args = journal_base_args(opts);
    args.push("-n".to_string());
    args.push(opts.lines.max(1).to_string());
    let (text, hint) = run_journalctl(&args, opts.elevated)?;
    Ok(JournalOutput { text, hint })
}

/// 构造 `journalctl` 的基础参数（实时跟踪时再追加 `-f`，快照查询再加 `-n`）。
pub fn journal_base_args(opts: &JournalOpts) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    match opts.scope {
        Scope::System => {}
        Scope::User => args.push("--user".to_string()),
    }
    if let Some(p) = opts.priority {
        args.push("-p".to_string());
        args.push(p.to_string());
    }
    if opts.boot_only {
        args.push("-b".to_string());
    }
    if let Some(u) = &opts.unit {
        let u = u.trim();
        if !u.is_empty() {
            match opts.scope {
                Scope::System => {
                    args.push("-u".to_string());
                    args.push(u.to_string());
                }
                Scope::User => args.push(format!("--user-unit={u}")),
            }
        }
    }
    args.push("--no-pager".to_string());
    args
}

// ---------------------------------------------------------------------------
// 单元操作 / 电源
// ---------------------------------------------------------------------------

/// 对单元执行操作（`action` ∈ start/stop/restart/reload/enable/disable/
/// mask/unmask/reset-failed）。
///
/// 系统级单元需要 root，非 root 时通过 `pkexec` 提权；用户级单元不提权。
pub fn unit_action(scope: Scope, name: &str, action: &str) -> Result<(bool, String), String> {
    let args = vec![action.to_string(), name.to_string()];
    run_systemctl(scope, &args, true)
}

/// 让 systemd 重新读取单元文件（`daemon-reload`）。系统级需要 root。
pub fn daemon_reload(scope: Scope) -> Result<(bool, String), String> {
    let args = vec!["daemon-reload".to_string()];
    run_systemctl(scope, &args, true)
}

/// 系统电源动作（关机/重启/挂起/休眠）。需要 root，非 root 时通过 `pkexec` 提权。
pub fn power(action: PowerAction) -> Result<(bool, String), String> {
    let args = vec![action.systemctl_verb().to_string()];
    run_systemctl(Scope::System, &args, true)
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_formatting() {
        assert_eq!(human_bytes("[not set]"), "");
        assert_eq!(human_bytes("0"), "");
        assert_eq!(human_bytes("infinity"), "");
        assert_eq!(human_bytes(&u64::MAX.to_string()), "");
        assert_eq!(human_bytes("1048576"), "1 MB");
        assert_eq!(human_bytes("2048"), "2 KB");
        assert_eq!(human_bytes("512"), "512 B");
        assert_eq!(human_bytes("3221225472"), "3 GB");
        assert_eq!(human_bytes("not-a-number"), "not-a-number");
    }

    #[test]
    fn human_duration_formatting() {
        assert_eq!(human_duration(0), "0 秒");
        assert_eq!(human_duration(45 * 1_000_000), "45 秒");
        assert_eq!(human_duration(60 * 1_000_000), "1 分");
        assert_eq!(human_duration(299 * 1_000_000), "4 分 59 秒");
        assert_eq!(human_duration(3600 * 1_000_000), "1 小时");
        assert_eq!(human_duration(13_300 * 1_000_000), "3 小时 41 分");
        assert_eq!(human_duration(86_400 * 1_000_000), "1 天");
        assert_eq!(human_duration(187_200 * 1_000_000), "2 天 4 小时");
    }

    #[test]
    fn strip_ansi_removes_sequences() {
        assert_eq!(strip_ansi("hello\u{1b}[32mworld\u{1b}[0m"), "helloworld");
        assert_eq!(strip_ansi("plain text"), "plain text");
    }

    #[test]
    fn is_template_detection() {
        assert!(is_template("getty@.service"));
        assert!(is_template("systemd-cryptsetup@.service"));
        assert!(!is_template("app-x@autostart.service"));
        assert!(!is_template("sshd.service"));
    }

    #[test]
    fn scope_args_and_privilege() {
        assert_eq!(Scope::System.args(), &[] as &[&str]);
        assert_eq!(Scope::User.args(), &["--user"]);
        assert!(Scope::System.needs_root());
        assert!(!Scope::User.needs_root());
    }

    /// 描述必须完整保留（曾经从第 6 个字段开始取，导致第一个词被吞掉）。
    #[test]
    fn parse_list_units_keeps_full_description() {
        let out = "\
  accounts-daemon.service                               loaded    active   running Accounts Service\n\
  NetworkManager-dispatcher.service                     loaded    inactive dead    NetworkManager-dispatcher\n\
  bolt.service                                          loaded    active   running Thunderbolt system service";
        let units = parse_list_units(out);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].name, "accounts-daemon.service");
        assert_eq!(units[0].load, "loaded");
        assert_eq!(units[0].active, "active");
        assert_eq!(units[0].description, "Accounts Service");
        assert_eq!(units[1].description, "NetworkManager-dispatcher");
        assert_eq!(units[2].description, "Thunderbolt system service");
    }

    /// `●` 标记列：曾经 `f[0]` 变成 `●`，所有 not-found 单元折叠成一条记录。
    #[test]
    fn parse_list_units_strips_marker_column() {
        let out = "\
\u{25cf} home.mount                                            not-found inactive dead      home.mount\n\
\u{25cf} auto-cpufreq.service                                   not-found inactive dead      auto-cpufreq.service\n\
  sshd.service                                          loaded    active   running OpenBSD Secure Shell server";
        let units = parse_list_units(out);
        assert_eq!(units.len(), 3);
        assert!(units.iter().all(|u| u.name != "\u{25cf}"));
        assert_eq!(units[0].name, "home.mount");
        assert_eq!(units[0].load, "not-found");
        assert_eq!(units[0].active, "inactive");
        assert_eq!(units[1].name, "auto-cpufreq.service");
        assert_eq!(units[2].name, "sshd.service");
    }

    /// 结尾统计行 / 空行必须被跳过，不能被当成单元。
    #[test]
    fn parse_list_units_skips_footer() {
        let out = "\n163 loaded units listed.\n0 loaded units listed.\n";
        assert!(parse_list_units(out).is_empty());
    }

    #[test]
    fn parse_unit_files_pairs() {
        let out = "accounts-daemon.service                      enabled         enabled\n\
                   alsa-restore.service                         static          -";
        let v = parse_unit_files(out);
        assert_eq!(
            v,
            vec![
                ("accounts-daemon.service".to_string(), "enabled".to_string()),
                ("alsa-restore.service".to_string(), "static".to_string()),
            ]
        );
    }

    #[test]
    fn parse_show_names_desc_blocks() {
        let out = "Id=alsa-utils.service\nNames=alsa-utils.service\nDescription=alsa-utils.service\n\n\
                   Id=systemd-logind.service\nNames=systemd-logind.service dbus-org.freedesktop.login1.service\nDescription=User Login Management\n\n";
        let v = parse_show_names_desc(out);
        assert_eq!(v.len(), 2);
        assert!(v[0].0.iter().any(|n| n == "alsa-utils.service"));
        assert_eq!(v[0].1, "alsa-utils.service");
        // 别名：Id 是真实单元名，Names 里既有真实名也有别名
        assert_eq!(v[1].0[0], "systemd-logind.service");
        assert!(v[1]
            .0
            .iter()
            .any(|n| n == "dbus-org.freedesktop.login1.service"));
        assert_eq!(v[1].1, "User Login Management");
    }

    /// `list-timers --output=json` 的真实结构（取自本机 systemd 257）。
    /// 注意 `left`/`passed` 是单调时钟值，不能当"还剩多久"，剩余/已过去必须用
    /// `now - next/last` 现算。
    #[test]
    fn parse_timers_json_real_sample() {
        let json = r#"[
          {"next":1789401000000000,"left":1789401000000000,"last":1789400403146377,"passed":298220354082,"unit":"sysstat-collect.timer","activates":"sysstat-collect.service"},
          {"next":null,"left":null,"last":0,"passed":0,"unit":"drkonqi-sentry-postman.timer","activates":"drkonqi-sentry-postman.service"}
        ]"#;
        let now = 1_789_400_740 * 1_000_000; // 与 next 相差 260 秒
        let timers = parse_timers_json(json, now).unwrap();
        assert_eq!(timers.len(), 2);
        assert_eq!(timers[0].unit, "sysstat-collect.timer");
        assert_eq!(timers[0].activates, "sysstat-collect.service");
        assert_eq!(timers[0].next_in, "4 分 20 秒");
        assert_eq!(timers[0].last_ago, "5 分 36 秒");
        // 未排定的定时器：没有下次/上次
        assert_eq!(timers[1].unit, "drkonqi-sentry-postman.timer");
        assert_eq!(timers[1].next_in, "");
        assert_eq!(timers[1].last_ago, "");
    }

    #[test]
    fn parse_timers_json_rejects_garbage() {
        assert!(parse_timers_json("", 0).is_err());
        assert!(parse_timers_json("Failed to list timers", 0).is_err());
    }

    /// 作用域参数必须出现在子命令之前（`systemctl --user list-units`），
    /// 顺序错了 systemctl 会报 unknown option。
    #[test]
    fn scope_args_come_first() {
        let args = vec!["list-units".to_string(), "--all".to_string()];
        assert_eq!(systemctl_args(Scope::User, &args), vec![
            "--user".to_string(),
            "list-units".to_string(),
            "--all".to_string()
        ]);
        assert_eq!(systemctl_args(Scope::System, &args), args);
    }

    /// 真实系统冒烟（`cargo test -- --ignored --nocapture real_system`）：
    /// 系统作用域与用户作用域都要能列出单元、解析正确、描述覆盖足够。
    #[test]
    #[ignore = "需要真实 systemd 环境，手动运行"]
    fn real_system_list_units_smoke() {
        for scope in [Scope::System, Scope::User] {
            let units = list_units(scope, "service").expect("list_units 失败");
            assert!(!units.is_empty(), "{scope:?} 没拿到任何单元");
            let bad: Vec<&str> = units
                .iter()
                .map(|u| u.name.as_str())
                .filter(|n| !n.contains('.') || n.contains(' '))
                .collect();
            assert!(bad.is_empty(), "{scope:?} 解析出了非单元名：{bad:?}");
            let with_desc = units.iter().filter(|u| !u.description.is_empty()).count();
            let total = units.len();
            println!("{scope:?}: service 单元 {total} 个，其中有描述 {with_desc} 个");
            for u in units.iter().take(3) {
                println!("   {} [{} / {}] {}", u.name, u.load, u.active, u.description);
            }
            // 系统作用域的单元文件大多自带 Description；用户作用域几乎全部是
            // transient/generated（没有 unit file），所以只对系统作用域要求覆盖率。
            if scope == Scope::System {
                assert!(
                    with_desc * 10 >= total * 8,
                    "描述覆盖过低：{with_desc}/{total}"
                );
            }
        }

        let timers = list_timers(Scope::System).expect("list_timers 失败");
        println!("系统定时器 {} 个：", timers.len());
        for t in timers.iter().take(5) {
            println!(
                "   {} -> {} | 下次 {} | 上次 {}",
                t.unit,
                t.activates,
                if t.next_in.is_empty() {
                    "未排定".to_string()
                } else {
                    t.next_in.clone()
                },
                if t.last_ago.is_empty() {
                    "从未".to_string()
                } else {
                    t.last_ago.clone()
                }
            );
        }
        assert!(!timers.is_empty(), "没拿到任何定时器");

        let cat = unit_file(Scope::System, "dbus.service").expect("cat 失败");
        assert!(cat.contains("[Unit]"), "cat 输出异常：{cat:.80}");
    }
}
