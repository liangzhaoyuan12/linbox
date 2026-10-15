//! systemd 单元文件的**读写与校验执行层**（GOAL Phase 11 的 11.3）。
//!
//! 职责：
//! - [`read_unit_text`]：直读 `FragmentPath` 单文件（编辑源，见 GOAL 摸底 1）；
//! - [`target_kind`] / [`check_editable`]：写入目标分类与「不可编辑」闸门（D11.2-A / D11.7）；
//! - [`verify_unit`] / [`parse_verify_output`]：`systemd-analyze verify` 写前闸门
//!   （**按单元名过滤输出**、按类型分流，GOAL 摸底 3 / 14 实测语义）；
//! - [`write_unit`]：备份 → 落盘（直写或 `pkexec sh -c`）→ 回读逐字节比对 →
//!   `daemon-reload` → 失败回滚（骨架照抄 `utils/fstab.rs::write_fstab_with_reload`）。
//!
//! 与 GOAL D11.6 的一处实现细化：**不走 heredoc，改用 fstab 同款「临时文件 + pkexec 脚本」**
//! ——内容经 0600 临时文件传给 root，彻底没有 shell 展开面与分隔符白名单问题
//! （「原样写入、不做任何展开」的目标用更安全的方式达成，已记入进度记录）。
//!
//! 约束（规划书 §3.7 同款）：本文件禁止 `use gtk` / `use adw` / `use glib`。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::model::systemd::Scope;
use crate::model::unitfile::{UNIT_TYPES, parse_unit_text, render_unit_text, type_of};
use crate::utils::env_editor::can_write_direct;

// ---------------------------------------------------------------------------
// 写入目标分类（D11.2-A 直改 FragmentPath；D11.7 不可编辑情形）
// ---------------------------------------------------------------------------

/// 写入目标的类别（页面用它出黄条 / 提示文案）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    /// 用户自己的单元目录（`~/.config/systemd/user` 等），直写零授权。
    UserDir,
    /// `/etc/systemd/system`（发行版管理员位置），写入要提权。
    SystemEtc,
    /// `/usr/lib|/lib`（包管理提供的 vendor 文件），**可改但升级会覆盖**（黄条提示）。
    Vendor,
    /// `/run/systemd/…`（运行时生成，重启即失）。
    Runtime,
    /// 其它绝对路径（少见，按可写性处理）。
    Other,
}

impl TargetKind {
    /// 中文标签（页面徽章）。
    pub fn label(self) -> &'static str {
        match self {
            TargetKind::UserDir => "用户",
            TargetKind::SystemEtc => "系统",
            TargetKind::Vendor => "发行版",
            TargetKind::Runtime => "运行时",
            TargetKind::Other => "其它",
        }
    }

    /// 是否要给「软件包升级会覆盖本次修改」的黄条（D11.2-A）。
    pub fn is_vendor(self) -> bool {
        matches!(self, TargetKind::Vendor)
    }
}

/// 按 `FragmentPath` 分类（纯函数，不碰文件系统）。
///
/// vendor 前缀判定实测口径：`/usr/lib/`、`/lib/`、`/usr/lib64/`、`/lib64/`（含
/// `/usr/lib/systemd/system` 与 `/usr/lib/systemd/user`）。
pub fn target_kind(fragment_path: &str, scope: Scope) -> TargetKind {
    let p = fragment_path;
    if p.starts_with("/usr/lib/")
        || p.starts_with("/lib/")
        || p.starts_with("/usr/lib64/")
        || p.starts_with("/lib64/")
    {
        return TargetKind::Vendor;
    }
    if p.starts_with("/run/systemd/") {
        return TargetKind::Runtime;
    }
    if p.starts_with("/etc/") {
        return TargetKind::SystemEtc;
    }
    // 用户作用域下、既非 vendor 也非系统路径 → 用户目录
    if scope == Scope::User {
        return TargetKind::UserDir;
    }
    TargetKind::Other
}

/// 「这个单元能不能编辑」的闸门（D11.7）。
///
/// 返回 `(类别, 警告文案)`；不可编辑时返回 `Err(中文原因)`。
/// - `FragmentPath` 为空（瞬态/udev device/运行时 scope）→ 拒；
/// - `/dev/null`（被 mask 的单元文件指向）→ 拒；
/// - 单元名后缀不在 11 类白名单 → 拒。
pub fn check_editable(
    name: &str,
    fragment_path: &str,
    scope: Scope,
) -> Result<(TargetKind, Option<&'static str>), String> {
    let Some(_t) = type_of(name) else {
        return Err(format!("单元名不合法（不在 11 类白名单内）：{name}"));
    };
    let path = fragment_path.trim();
    if path.is_empty() {
        return Err("该单元没有单元文件（瞬态/运行时生成），无法编辑".to_string());
    }
    if path == "/dev/null" {
        return Err("该单元已被屏蔽（masked），先取消屏蔽才能编辑".to_string());
    }
    if !path.starts_with('/') {
        return Err(format!("单元文件路径异常：{path}"));
    }
    let kind = target_kind(path, scope);
    let warn = if kind.is_vendor() {
        Some("该文件由软件包管理，升级会覆盖本次修改")
    } else {
        None
    };
    Ok((kind, warn))
}

/// 路径安全闸门（防注入到 shell 脚本 / 越界写）：绝对路径、无 `..`、无控制字符与空字节。
pub fn check_path_safety(path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("只允许绝对路径：{path}"));
    }
    if path.contains("..") {
        return Err(format!("路径不允许包含 `..`：{path}"));
    }
    if path.contains('\0') || path.contains('\n') || path.contains('\r') {
        return Err("路径含控制字符".to_string());
    }
    if path.contains('\'') || path.contains('`') || path.contains('$') {
        return Err(format!("路径含 shell 特殊字符，拒绝：{path}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 读取
// ---------------------------------------------------------------------------

/// 直读单元文件。权限不足时把原因讲清楚（提权读由 UI 走 `pkexec`，不在这层自动弹框）。
pub fn read_unit_text(path: &Path) -> Result<String, String> {
    check_path_safety(&path.to_string_lossy())?;
    std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            format!("读取 {} 权限不足：{e}", path.display())
        } else {
            format!("读取 {} 失败：{e}", path.display())
        }
    })
}

// ---------------------------------------------------------------------------
// 写前校验：systemd-analyze verify（D11.5-③，按类型分流）
// ---------------------------------------------------------------------------

/// `systemd-analyze verify` 的输出判定（**纯函数，fixture 可测**）。
///
/// 规则（GOAL 摸底 3/14 实测）：
/// - 输出里**含本单元名**的行 = 本单元的问题 → 拒（原文透传）；
/// - 进程 `rc != 0` 但定位不到本单元行 → 也拒（GOAL 口径：RC≠0 即拒），错误里注明；
/// - 只有夹带的全系统无关告警且 `rc == 0` → 放行，无关行进 warnings。
///
/// `warnings` = 无关告警（页面可展示，不拦截）。
pub fn parse_verify_output(output: &str, rc: i32, name: &str) -> Result<Vec<String>, String> {
    let mut mine: Vec<String> = Vec::new();
    let mut others: Vec<String> = Vec::new();
    for line in output.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if l.contains(name) {
            mine.push(l.to_string());
        } else {
            others.push(l.to_string());
        }
    }
    if !mine.is_empty() {
        return Err(mine.join("\n"));
    }
    if rc != 0 {
        let mut msg =
            format!("systemd-analyze verify 返回非 0（RC={rc}），未能在输出里定位到本单元：");
        if others.is_empty() {
            msg.push_str("（输出为空）");
        } else {
            msg.push('\n');
            msg.push_str(&others.join("\n"));
        }
        return Err(msg);
    }
    Ok(others)
}

/// 跑 `systemd-analyze verify`（临时副本，**文件名必须等于单元名**）。
///
/// - `unit_type == "scope"` → **跳过**（实测报 `Unit … not found.`，GOAL 摸底 14），
///   返回带说明的空 warnings；
/// - 类型与名字不符 → 拒；
/// - `systemd-analyze` 不存在 → 拒（不假装跑过；本机 systemd 必带）。
pub fn verify_unit(text: &str, name: &str, unit_type: &str) -> Result<Vec<String>, String> {
    let Some(kind) = type_of(name) else {
        return Err(format!("单元名不合法：{name}"));
    };
    if kind != unit_type {
        return Err(format!("单元名后缀（.{kind}）与类型（{unit_type}）不一致"));
    }
    if unit_type == "scope" {
        return Ok(vec![
            "已跳过 systemd-analyze verify（该工具不认 .scope 文件，见 GOAL 摸底 14）".to_string(),
        ]);
    }

    let dir = std::env::temp_dir().join(format!("linbox-unit-verify-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败：{e}"))?;
    let file = dir.join(name);
    let res = (|| {
        std::fs::write(&file, text).map_err(|e| format!("写临时副本失败：{e}"))?;
        let out = Command::new("systemd-analyze")
            .arg("verify")
            .arg(&file)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("无法执行 systemd-analyze（请确认已安装 systemd）：{e}"))?;
        let code = out.status.code().unwrap_or(-1);
        let merged = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        parse_verify_output(&merged, code, name)
    })();
    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_dir(&dir);
    res
}

// ---------------------------------------------------------------------------
// 写入链路（备份 → 落盘 → 回读 → reload → 失败回滚）
// ---------------------------------------------------------------------------

/// 写入结果凭证（页面展示 / 测试证据）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteOutcome {
    /// 备份路径（目标原先不存在 → `None`）。
    pub backup: Option<PathBuf>,
    /// 是否走了 pkexec 提权。
    pub elevated: bool,
    /// `systemctl daemon-reload` 退出码。
    pub reload_rc: i32,
}

/// 备份路径：`<target>.linbox.bak`（与 `fstab::backup_path` 同名同规则）。
pub fn backup_path(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".linbox.bak");
    PathBuf::from(s)
}

/// 单元文件写入全链（`reload` 可注入，单测确定性验证「reload 失败 → 回滚」）。
///
/// 前置：内容应已过 `model::unitfile::validate_form` 与 [`verify_unit`]（页面流程保证）。
/// 这里仍过两道自家防线：**① parse→render→parse 语义等值；② 必须以 `\n` 结尾**。
fn write_unit_with_reload<F: FnOnce() -> i32>(
    target: &Path,
    content: &str,
    reload: F,
) -> Result<WriteOutcome, String> {
    check_path_safety(&target.to_string_lossy())?;
    if !content.ends_with('\n') {
        return Err("内容必须以换行结尾，拒绝写入".to_string());
    }
    // ① 自家往返自检：解析 → 渲染 → 再解析，语义必须等值（坏内容连备份都不做）
    let d1 = parse_unit_text(content);
    let rendered = render_unit_text(&d1);
    let d2 = parse_unit_text(&rendered);
    if d1 != d2 {
        return Err("内容未通过自家往返自检，拒绝写入".to_string());
    }

    let existed = target.exists();
    let bak = backup_path(target);
    let target_s = target.to_string_lossy().into_owned();
    let elevated = !can_write_direct(&target_s);

    // ② 备份 + 落盘
    if elevated {
        elevated_backup_and_write(target, content)?;
    } else if let Err(e) = direct_backup_and_write(target, &bak, existed, content) {
        if existed {
            let _ = restore_from_backup(target, true, false);
        }
        return Err(e);
    }

    // ③ 回读逐字节比对
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

    // ④ daemon-reload（失败回滚）
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

/// 对外写入口：真实 `daemon-reload`（按作用域带不带 `--user`）。
pub fn write_unit(target: &Path, content: &str, scope: Scope) -> Result<WriteOutcome, String> {
    write_unit_with_reload(target, content, || run_daemon_reload(scope))
}

/// 只测链路不碰 systemd 时用（`reload` 注入）。
pub fn write_unit_with<F: FnOnce() -> i32>(
    target: &Path,
    content: &str,
    reload: F,
) -> Result<WriteOutcome, String> {
    write_unit_with_reload(target, content, reload)
}

/// `daemon-reload` 的 argv（纯函数，供单测锁作用域参数）。
pub fn daemon_reload_argv(scope: Scope) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    v.push("systemctl".to_string());
    v.extend(scope.args().iter().map(|s| s.to_string()));
    v.push("daemon-reload".to_string());
    v
}

/// 执行 `daemon-reload`，返回退出码（启动失败按 -1）。
fn run_daemon_reload(scope: Scope) -> i32 {
    let argv = daemon_reload_argv(scope);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdin(Stdio::null());
    match cmd.status() {
        Ok(st) => st.code().unwrap_or(-1),
        Err(_) => -1,
    }
}

/// 直写路径：备份 + 落盘。
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

/// 恢复备份：原先存在 → 拷回；原先不存在 → 删掉误写文件。提权路径经 pkexec。
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

/// 提权写入：内容先落到 0600 临时文件，再由 `pkexec sh -c` 脚本拷进目标
/// （与 `utils/fstab.rs::elevated_backup_and_write` 同构——**不经 shell 展开内容**）。
fn elevated_backup_and_write(target: &Path, content: &str) -> Result<(), String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("linbox-unit-{}-{nanos}.tmp", std::process::id()));
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

/// 拼提权写入脚本（**纯函数，单测锁形状**）：建父目录 → 旧文件备份 →
/// `cat 临时文件 > 目标` → `chmod 0644`。所有路径经 [`sh_q`] 单引号转义。
pub fn elevated_write_script(target: &Path, tmp: &Path) -> String {
    let dst = target.display().to_string();
    let bak = backup_path(target).display().to_string();
    let parent = target
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/".to_string());
    format!(
        "set -e\nmkdir -p -- {p}\nif [ -e {d} ]; then cp -a -- {d} {b}; fi\ncat -- {t} > {d}\nchmod 0644 {d}\n",
        p = sh_q(&parent),
        d = sh_q(&dst),
        b = sh_q(&bak),
        t = sh_q(&tmp.display().to_string()),
    )
}

/// 提权执行的 argv（纯函数）：`pkexec sh -c <script>`。
pub fn unit_write_argv(script: &str) -> Vec<String> {
    vec![
        "pkexec".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        script.to_string(),
    ]
}

/// 执行 `pkexec sh -c script`。
fn run_pkexec_sh(script: &str) -> Result<(), String> {
    let out = Command::new("pkexec")
        .arg("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("无法执行 pkexec（请确认已安装 polkit）：{e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let msg = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        Err(format!(
            "提权写入失败（{}）：{}",
            out.status
                .code()
                .map(|c| format!("RC={c}"))
                .unwrap_or_default(),
            msg.trim()
        ))
    }
}

/// shell 单引号转义（`'` → `'\''`）。
fn sh_q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// 路径信息 / 新建单元（GOAL 11.5 / D11.9）
// ---------------------------------------------------------------------------

/// 一个单元的路径信息（编辑源 + drop-in 覆盖 + 启用状态）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnitPaths {
    /// 单元文件本体路径（编辑源，GOAL 摸底 1）；空 = 无文件（瞬态）。
    pub fragment: String,
    /// drop-in 覆盖文件列表（D11.4-③ 黄条用）。
    pub drop_ins: Vec<String>,
    /// `UnitFileState`（enabled / disabled / static / masked …）。
    pub unit_file_state: String,
    /// `ActiveState`（active / inactive / failed …，11.6 删除链路判 stop 用）。
    pub active_state: String,
}

/// 解析 `systemctl show -p FragmentPath -p DropInPaths -p UnitFileState` 的输出（纯函数）。
pub fn parse_unit_paths(out: &str) -> UnitPaths {
    let mut p = UnitPaths::default();
    for line in out.lines() {
        if let Some(v) = line.strip_prefix("FragmentPath=") {
            p.fragment = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("DropInPaths=") {
            p.drop_ins = v.split_whitespace().map(|s| s.to_string()).collect();
        } else if let Some(v) = line.strip_prefix("UnitFileState=") {
            p.unit_file_state = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("ActiveState=") {
            p.active_state = v.trim().to_string();
        }
    }
    p
}

/// 取单元路径信息（`systemctl show`，只读、免提权）。
pub fn unit_paths(scope: Scope, name: &str) -> Result<UnitPaths, String> {
    let args = vec![
        "show".to_string(),
        "--no-pager".to_string(),
        "--property=FragmentPath,DropInPaths,UnitFileState,ActiveState".to_string(),
        name.to_string(),
    ];
    let out = run_systemctl_capture(scope, &args)?;
    if !out.contains("FragmentPath=") {
        return Err(if out.trim().is_empty() {
            format!("拿不到 {name} 的路径信息")
        } else {
            format!("拿不到 {name} 的路径信息：{}", out.trim())
        });
    }
    Ok(parse_unit_paths(&out))
}

/// 跑一次 `systemctl`（只读用途），返回 stdout+stderr 合并文本。
fn run_systemctl_capture(scope: Scope, args: &[String]) -> Result<String, String> {
    let mut argv: Vec<String> = scope.args().iter().map(|s| s.to_string()).collect();
    argv.extend(args.iter().cloned());
    let out = Command::new("systemctl")
        .args(&argv)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("无法执行 systemctl：{e}"))?;
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

/// 单元名校验（D11.9-②）。
///
/// 规则：字符白名单 `[A-Za-z0-9:_.@-]`；禁 `/`、空白、控制字符、空字节、`..`、
/// 以 `-` / `.` 开头；**后缀必须等于所选类型**；模板名 `foo@.service` 放行。
pub fn validate_unit_name(name: &str, unit_type: &str) -> Result<(), String> {
    if !UNIT_TYPES.contains(&unit_type) {
        return Err(format!("未知单元类型：{unit_type}"));
    }
    if name.is_empty() {
        return Err("单元名不能为空".to_string());
    }
    if name != name.trim() {
        return Err("单元名首尾不能有空白".to_string());
    }
    if name.contains('/') || name.contains('\0') || name.contains('\n') || name.contains('\r') {
        return Err(format!("单元名含非法字符：{name:?}"));
    }
    if name.starts_with('-') || name.starts_with('.') {
        return Err(format!("单元名不能以 {:?} 开头", &name[..1]));
    }
    if name.contains("..") {
        return Err(format!("单元名不允许包含 `..`：{name}"));
    }
    for c in name.chars() {
        if !(c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.' | '@' | '-')) {
            return Err(format!("单元名含白名单外字符 {c:?}：{name}"));
        }
    }
    let Some(kind) = type_of(name) else {
        return Err(format!(
            "单元名缺少合法类型后缀（.service / .timer / …）：{name}"
        ));
    };
    if kind != unit_type {
        return Err(format!(
            "后缀 .{kind} 与所选类型 {unit_type} 不一致（后缀由类型决定，不用手打）"
        ));
    }
    let base = &name[..name.len() - kind.len() - 1];
    if base.is_empty() {
        return Err(format!("单元名不完整：{name}"));
    }
    Ok(())
}

/// 新建单元的目标路径（D11.9-①）：系统级 `/etc/systemd/system/<name>`、
/// 用户级 `<home>/.config/systemd/user/<name>`；**绝不写 `/usr/lib`**。
pub fn create_path(scope: Scope, name: &str) -> Result<PathBuf, String> {
    let kind = type_of(name).ok_or_else(|| format!("单元名缺少合法类型后缀：{name}"))?;
    validate_unit_name(name, kind)?;
    match scope {
        Scope::System => Ok(PathBuf::from("/etc/systemd/system").join(name)),
        Scope::User => {
            let home = dirs::home_dir().ok_or_else(|| "无法确定用户主目录".to_string())?;
            Ok(home.join(".config/systemd/user").join(name))
        }
    }
}

/// 目标目录里是否已存在同名单元（D11.9-②：同名会静默遮蔽上层副本，GOAL 摸底 2/11）。
///
/// 返回 `Some(原因)` = 冲突；`Ok(None)` = 可以新建。
pub fn name_conflict(scope: Scope, name: &str) -> Result<Option<String>, String> {
    // ① list-unit-files（systemd 认得的全部单元文件）
    let args = vec![
        "list-unit-files".to_string(),
        "--no-legend".to_string(),
        "--no-pager".to_string(),
    ];
    if let Ok(out) = run_systemctl_capture(scope, &args) {
        for line in out.lines() {
            if line.split_whitespace().next() == Some(name) {
                return Ok(Some(format!(
                    "{name} 已存在（systemd 已收录该单元文件），请改用「编辑」"
                )));
            }
        }
        // 命令本身失败（例如 systemctl 缺失）→ 继续走 ② 兜底
        if !out.contains("Failed to") && !out.contains("无法执行") {
            return Ok(None);
        }
    }
    // ② FragmentPath 兜底
    let paths = unit_paths(scope, name).unwrap_or_default();
    if !paths.fragment.is_empty() {
        return Ok(Some(format!(
            "{name} 已存在（{}），请改用「编辑」",
            paths.fragment
        )));
    }
    Ok(None)
}

/// `.socket` → 同名 `.service`（socket 新建前置检查，D11.9-③）。
pub fn socket_peer_name(socket_name: &str) -> Result<String, String> {
    let Some(base) = socket_name.strip_suffix(".socket") else {
        return Err(format!("{socket_name} 不是 .socket 单元"));
    };
    if base.is_empty() {
        return Err("单元名不完整".to_string());
    }
    Ok(format!("{base}.service"))
}

/// 同名 service 是否已加载（`systemd-analyze verify` 也会查，这里给更早的中文提示）。
pub fn socket_peer_service_loaded(scope: Scope, socket_name: &str) -> Result<bool, String> {
    let peer = socket_peer_name(socket_name)?;
    let args = vec![
        "show".to_string(),
        "--no-pager".to_string(),
        "--property=LoadState".to_string(),
        peer.clone(),
    ];
    let out = run_systemctl_capture(scope, &args)?;
    Ok(out.lines().any(|l| l.trim() == "LoadState=loaded"))
}

/// 新建单元的默认模板（D11.9-③，**9 类**；`.device` / `.scope` 拒绝）。
///
/// 必填项在模板里**故意留空**（service 不带 `ExecStart`、mount 不带 `What`/`Where`），
/// 让 [`crate::model::unitfile::validate_form`] 在用户填完之前把关。
pub fn default_new_doc(scope: Scope, unit_type: &str) -> Result<String, String> {
    if !crate::model::unitfile::NEWABLE_TYPES.contains(&unit_type) {
        return Err(format!(
            "{unit_type} 不支持新建（.device 由 udev 动态生成、.scope 由 systemd 运行时创建，见 GOAL D11.9-⑤）"
        ));
    }
    let mut s = String::from("[Unit]\nDescription=我的服务\n");
    match unit_type {
        "service" => s.push_str("[Service]\nType=simple\n"),
        "timer" => s.push_str("[Timer]\nOnBootSec=1min\n"),
        "socket" => s.push_str("[Socket]\n"),
        "mount" => s.push_str("[Mount]\n"),
        "automount" => s.push_str("[Automount]\n"),
        "path" => s.push_str("[Path]\n"),
        "swap" => s.push_str("[Swap]\n"),
        // target / slice 只有 [Unit]
        _ => {}
    }
    match unit_type {
        "timer" => s.push_str("[Install]\nWantedBy=timers.target\n"),
        "target" | "service" => {
            let wb = match scope {
                Scope::System => "multi-user.target",
                Scope::User => "default.target",
            };
            s.push_str(&format!("[Install]\nWantedBy={wb}\n"));
        }
        _ => {}
    }
    Ok(s)
}

/// `Where=` → mount / automount 单元名（systemd 规则：去开头 `/`、`/`→`-`）。
pub fn unit_name_from_where(path: &str, unit_type: &str) -> String {
    crate::model::unitfile::unit_name_from_path(path, unit_type)
}

/// `What=` → swap 单元名。
pub fn unit_name_from_what(what: &str) -> String {
    crate::model::unitfile::unit_name_from_path(what, "swap")
}

// ---------------------------------------------------------------------------
// 删除单元（GOAL 11.6 / D11.10）
// ---------------------------------------------------------------------------

/// 删除计划：读出来的状态 + 步骤序列（页面确认框与执行层共用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletePlan {
    pub name: String,
    pub scope: Scope,
    /// 单元文件本体（mv 成 `.linbox.bak` 的对象，**绝不 rm**）。
    pub fragment: PathBuf,
    /// drop-in 目录（随删，同样 mv 成 `.linbox.bak`）。
    pub drop_in_dirs: Vec<PathBuf>,
    /// `ActiveState == active`（决定要不要先 stop）。
    pub active: bool,
    /// `UnitFileState` 以 `enabled` 开头（决定要不要先 disable 清 `.wants` 链接）。
    pub enabled: bool,
    /// vendor 文件的强警告（D11.10：软件包升级会重装它）。
    pub vendor_warning: Option<String>,
    /// 中文步骤序列（确认框展示；顺序 = 执行顺序）。
    pub steps: Vec<String>,
}

/// 读状态并产出删除计划（后台线程调用；瞬态/masked/非法名 → Err）。
pub fn delete_plan(scope: Scope, name: &str) -> Result<DeletePlan, String> {
    let paths = unit_paths(scope, name)?;
    build_delete_plan(
        scope,
        name,
        &paths.fragment,
        &drop_in_dirs_of(&paths.fragment, &paths.drop_ins),
        &paths.unit_file_state,
        &paths.active_state,
    )
}

/// drop-in 目录集合：`DropInPaths` 各文件的 `.d` 父目录（去重排序）+
/// fragment 同名 `.d` 目录（存在才收）。
fn drop_in_dirs_of(fragment: &str, drop_ins: &[String]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = drop_ins
        .iter()
        .filter_map(|f| {
            let p = Path::new(f);
            let d = p.parent()?;
            d.to_str()?.ends_with(".d").then(|| d.to_path_buf())
        })
        .collect();
    let sibling = PathBuf::from(format!("{fragment}.d"));
    if sibling.is_dir() {
        dirs.push(sibling);
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// 纯函数核心（单测直喂状态）：校验 + 组装计划。
///
/// - 名字必须过 11 类白名单（`validate_unit_name`，命令注入形态全拒）；
/// - `FragmentPath` 为空（瞬态/生成单元，含 device/scope）→ Err 引导用「停止」；
/// - masked（`/dev/null` 或 `UnitFileState=masked`）→ Err 提示先「取消屏蔽」。
pub fn build_delete_plan(
    scope: Scope,
    name: &str,
    fragment: &str,
    drop_in_dirs: &[PathBuf],
    unit_file_state: &str,
    active_state: &str,
) -> Result<DeletePlan, String> {
    let kind = type_of(name).ok_or_else(|| format!("单元名不在 11 类白名单：{name}"))?;
    validate_unit_name(name, kind)?;
    if fragment.is_empty() {
        return Err(format!(
            "{name} 没有单元文件（瞬态/生成单元），无法删除；如需停用请用「停止」"
        ));
    }
    if fragment == "/dev/null" || unit_file_state == "masked" {
        return Err(format!("{name} 已被屏蔽（masked），请先「取消屏蔽」再删除"));
    }
    check_path_safety(fragment)?;
    for d in drop_in_dirs {
        check_path_safety(&d.to_string_lossy())?;
    }

    let active = active_state == "active";
    let enabled = unit_file_state.starts_with("enabled");
    let kind_t = target_kind(fragment, scope);
    let vendor_warning = kind_t
        .is_vendor()
        .then(|| "该文件由软件包管理，删除后软件包升级会重装它".to_string());

    let mut steps = Vec::new();
    if active {
        steps.push(format!("停止 {name}（当前正在运行）"));
    }
    if enabled {
        steps.push(format!("禁用 {name}（清除 .wants 启用链接，防悬空）"));
    }
    steps.push(format!("移动单元文件 → {fragment}.linbox.bak"));
    for d in drop_in_dirs {
        steps.push(format!("移动 drop-in 目录 → {}.linbox.bak", d.display()));
    }
    steps.push("daemon-reload 让 systemd 重新读取".to_string());

    Ok(DeletePlan {
        name: name.to_string(),
        scope,
        fragment: PathBuf::from(fragment),
        drop_in_dirs: drop_in_dirs.to_vec(),
        active,
        enabled,
        vendor_warning,
        steps,
    })
}

/// 删除脚本（纯函数，单测锁形状）：stop → disable → mv 文件 → mv .d 目录 → daemon-reload。
///
/// **红线**：只 mv 计划里点名的路径，绝不 glob 通配、绝不 `rm`、绝不碰别名链接
/// （`.wants` 清理由 `systemctl disable` 完成）。
pub fn delete_script(plan: &DeletePlan) -> String {
    let sc = plan.scope.args();
    let sysctl = |verb: &str| -> String {
        let mut c = String::from("systemctl");
        for a in sc {
            c.push(' ');
            c.push_str(a);
        }
        c.push(' ');
        c.push_str(verb);
        c
    };
    let mut s = String::new();
    if plan.active {
        s.push_str(&format!(
            "{} >/dev/null 2>&1 || true\n",
            sysctl(&format!("stop {}", sh_q(&plan.name)))
        ));
    }
    if plan.enabled {
        s.push_str(&format!(
            "{} >/dev/null 2>&1 || true\n",
            sysctl(&format!("disable {}", sh_q(&plan.name)))
        ));
    }
    let frag_s = plan.fragment.display().to_string();
    let bak = format!("{frag_s}.linbox.bak");
    s.push_str(&format!("mv -f {} {}\n", sh_q(&frag_s), sh_q(&bak)));
    for d in &plan.drop_in_dirs {
        let d_s = d.display().to_string();
        let dbak = format!("{d_s}.linbox.bak");
        s.push_str(&format!("mv -f {} {}\n", sh_q(&d_s), sh_q(&dbak)));
    }
    s.push_str(&format!("{}\n", sysctl("daemon-reload")));
    s
}

/// 删除命令 argv（纯函数）：系统级一条 `pkexec sh -c`；用户级直跑（零提权）。
pub fn delete_cmd(plan: &DeletePlan) -> Vec<String> {
    let script = delete_script(plan);
    match plan.scope {
        Scope::System => unit_write_argv(&script),
        Scope::User => vec!["sh".to_string(), "-c".to_string(), script],
    }
}

/// 删除执行结果凭证。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteOutcome {
    /// 产生的备份路径（单元文件 + drop-in 目录）。
    pub backups: Vec<PathBuf>,
    /// 脚本输出（取证用）。
    pub output: String,
}

/// 执行删除全链 + **回读断言**：单元文件必须已不在；
/// 仍在（提权被拒/mv 失败）→ 若备份在则回滚恢复，返回 Err。
pub fn run_delete(plan: &DeletePlan) -> Result<DeleteOutcome, String> {
    let argv = delete_cmd(plan);
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("无法执行删除：{e}"))?;
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if plan.fragment.exists() {
        let bak = PathBuf::from(format!("{}.linbox.bak", plan.fragment.display()));
        if bak.exists() {
            restore_from_delete_backup(&plan.fragment, &bak, plan.scope)?;
        }
        return Err(format!(
            "删除未生效（{} 仍在），可能提权被拒或 mv 失败：{}",
            plan.fragment.display(),
            output.trim()
        ));
    }
    let mut backups = vec![PathBuf::from(format!(
        "{}.linbox.bak",
        plan.fragment.display()
    ))];
    for d in &plan.drop_in_dirs {
        backups.push(PathBuf::from(format!("{}.linbox.bak", d.display())));
    }
    Ok(DeleteOutcome { backups, output })
}

/// 回滚：把 `.linbox.bak` 搬回原名（系统级经 pkexec）。
fn restore_from_delete_backup(fragment: &Path, backup: &Path, scope: Scope) -> Result<(), String> {
    let script = format!(
        "mv -f {} {}\n",
        sh_q(&backup.display().to_string()),
        sh_q(&fragment.display().to_string())
    );
    match scope {
        Scope::User => {
            std::fs::rename(backup, fragment)
                .map_err(|e| format!("回滚失败（{}）：{e}", backup.display()))?;
            Ok(())
        }
        Scope::System => run_pkexec_sh(&script).map_err(|e| format!("回滚失败：{e}")),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::unitfile::NEWABLE_TYPES;

    // ── 目标分类与编辑闸门 ─────────────────────────────────────────────

    /// `target_kind` 真值表（D11.2-A：vendor / 系统 / 用户 / 运行时 / 其它）。
    #[test]
    fn target_kind_truth_table() {
        let cases: &[(&str, Scope, TargetKind)] = &[
            (
                "/usr/lib/systemd/system/cron.service",
                Scope::System,
                TargetKind::Vendor,
            ),
            (
                "/lib/systemd/system/x.service",
                Scope::System,
                TargetKind::Vendor,
            ),
            (
                "/usr/lib/systemd/user/pulseaudio.service",
                Scope::User,
                TargetKind::Vendor,
            ),
            (
                "/etc/systemd/system/my.service",
                Scope::System,
                TargetKind::SystemEtc,
            ),
            (
                "/etc/xdg/autostart/x.desktop",
                Scope::System,
                TargetKind::SystemEtc,
            ),
            (
                "/run/systemd/system/x.service",
                Scope::System,
                TargetKind::Runtime,
            ),
            (
                "/home/u/.config/systemd/user/x.service",
                Scope::User,
                TargetKind::UserDir,
            ),
            ("/opt/weird/x.service", Scope::System, TargetKind::Other),
        ];
        for (p, scope, want) in cases {
            assert_eq!(target_kind(p, *scope), *want, "{p} / {scope:?}");
        }
        assert!(TargetKind::Vendor.is_vendor());
        assert!(!TargetKind::UserDir.is_vendor());
        assert_eq!(TargetKind::Vendor.label(), "发行版");
        assert_eq!(TargetKind::UserDir.label(), "用户");
    }

    /// `check_editable` 三类拒绝（D11.7）+ vendor 警告。
    #[test]
    fn check_editable_gates() {
        // 瞬态：FragmentPath 为空
        let e = check_editable("x.service", "", Scope::System).unwrap_err();
        assert!(e.contains("没有单元文件"), "{e}");
        // masked → /dev/null
        let e = check_editable("x.service", "/dev/null", Scope::System).unwrap_err();
        assert!(e.contains("屏蔽"), "{e}");
        // 名字不在 11 类白名单
        let e = check_editable("x.conf", "/etc/systemd/system/x.conf", Scope::System).unwrap_err();
        assert!(e.contains("白名单"), "{e}");
        // 相对路径
        let e = check_editable("x.service", "relative/path.service", Scope::System).unwrap_err();
        assert!(e.contains("路径异常"), "{e}");
        // 正常：vendor 带警告
        let (kind, warn) = check_editable(
            "cron.service",
            "/usr/lib/systemd/system/cron.service",
            Scope::System,
        )
        .unwrap();
        assert_eq!(kind, TargetKind::Vendor);
        assert!(warn.is_some_and(|w| w.contains("升级会覆盖")));
        // 正常：用户目录无警告
        let (kind, warn) = check_editable(
            "x.service",
            "/home/u/.config/systemd/user/x.service",
            Scope::User,
        )
        .unwrap();
        assert_eq!(kind, TargetKind::UserDir);
        assert!(warn.is_none());
    }

    /// 路径安全闸门：`..` / 控制字符 / 相对路径 / shell 特殊字符全拒。
    #[test]
    fn path_safety_rejects_injection_shapes() {
        assert!(check_path_safety("/etc/systemd/system/x.service").is_ok());
        for bad in [
            "relative.service",
            "/etc/../etc/passwd",
            "/tmp/x\ny",
            "/tmp/x\0y",
            "/tmp/x'y",
            "/tmp/`id`.service",
            "/tmp/$HOME.service",
        ] {
            assert!(check_path_safety(bad).is_err(), "{bad:?} 应被拒");
        }
    }

    // ── 读取 ───────────────────────────────────────────────────────────

    /// 读取：不存在 → Err（带路径）；存在 → 原文读回。
    #[test]
    fn read_unit_text_missing_and_ok() {
        let missing = std::env::temp_dir().join("linbox-no-such-unit.service");
        let _ = std::fs::remove_file(&missing);
        let e = read_unit_text(&missing).unwrap_err();
        assert!(e.contains("失败"), "{e}");

        let dir = std::env::temp_dir().join(format!("linbox-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.service");
        std::fs::write(&f, "[Unit]\nDescription=x\n").unwrap();
        assert_eq!(read_unit_text(&f).unwrap(), "[Unit]\nDescription=x\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── verify 输出判定（fixture = GOAL 摸底 14 真实输出原文）───────────

    /// fixture：真实 systemd 255 输出（含 4 条全系统无关告警）——
    /// ① service 缺 ExecStart → 拒，原文透传。
    #[test]
    fn verify_fixture_service_missing_execstart() {
        let out = "\
/usr/lib/systemd/system/deepin-home-appstore-daemon.service:8: Special user nobody configured, this is not safe!
Configuration file /usr/lib/systemd/system/spark-update-notifier.service is marked executable. Please remove executable permission bits. Proceeding anyway.
/usr/lib/systemd/system/spark-update-notifier.service:10: Failed to parse sec value, ignoring: 15
linbox-probe.service: Service has no ExecStart=, ExecStop=, or SuccessAction=. Refusing.
Unit linbox-probe.service has a bad unit file setting.
";
        let e = parse_verify_output(out, 1, "linbox-probe.service").unwrap_err();
        assert!(e.contains("Service has no ExecStart="), "{e}");
        assert!(e.contains("bad unit file setting"), "{e}");
        assert!(
            !e.contains("deepin-home-appstore"),
            "无关告警不能混进错误：{e}"
        );
    }

    /// fixture：② 合法 service（RC=0 + 无关告警）→ 放行，告警进 warnings。
    #[test]
    fn verify_fixture_ok_with_unrelated_warnings() {
        let out = "\
/usr/lib/systemd/system/deepin-home-appstore-daemon.service:8: Special user nobody configured, this is not safe!
/usr/lib/systemd/system/spark-update-notifier.service:11: Unknown key name 'StartLimitIntervalSec' in section 'Service', ignoring.
";
        let w = parse_verify_output(out, 0, "cron.service").unwrap();
        assert_eq!(w.len(), 2, "无关告警透传给页面展示");
        assert!(w[0].contains("deepin-home-appstore"));
    }

    /// fixture：③ socket 缺同名 service（实测原文）→ 拒。
    #[test]
    fn verify_fixture_socket_peer_missing() {
        let out = "linbox-probe.socket: service linbox-probe.service not loaded, socket cannot be started.\n";
        let e = parse_verify_output(out, 1, "linbox-probe.socket").unwrap_err();
        assert!(e.contains("socket cannot be started"), "{e}");
    }

    /// fixture：④ mount 的 Where 不匹配（实测原文）→ 拒。
    #[test]
    fn verify_fixture_mount_where_mismatch() {
        let out = "linbox-probe.mount: Where= setting doesn't match unit name. Refusing.\nUnit linbox-probe.mount has a bad unit file setting.\n";
        let e = parse_verify_output(out, 1, "linbox-probe.mount").unwrap_err();
        assert!(e.contains("Where= setting doesn't match unit name"), "{e}");
        // 同一条输出喂给无关单元名 → 不算它的问题
        let w = parse_verify_output(out, 1, "other.service").unwrap_err();
        assert!(w.contains("返回非 0"), "RC≠0 且定位不到本单元也要拒：{w}");
    }

    /// fixture：⑤ automount 同 Where 规则 → 拒。
    #[test]
    fn verify_fixture_automount_where_mismatch() {
        let out = "linbox-probe.automount: Where= setting doesn't match unit name. Refusing.\nUnit linbox-probe.automount has a bad unit file setting.\n";
        let e = parse_verify_output(out, 1, "linbox-probe.automount").unwrap_err();
        assert!(e.contains("Where="), "{e}");
    }

    /// fixture：⑥ swap 的 What 不匹配（实测原文）→ 拒。
    #[test]
    fn verify_fixture_swap_what_mismatch() {
        let out = "linbox-probe.swap: Value of What= and unit name do not match, not loading.\nUnit linbox-probe.swap has a bad unit file setting.\n";
        let e = parse_verify_output(out, 1, "linbox-probe.swap").unwrap_err();
        assert!(e.contains("What= and unit name do not match"), "{e}");
    }

    /// fixture：⑦ timer / target / path / slice / device 的 RC=0 路径（摸底 14 实测全放行）。
    #[test]
    fn verify_fixture_passing_types_rc0() {
        let unrelated = "/usr/lib/systemd/system/deepin-home-appstore-daemon.service:8: Special user nobody configured, this is not safe!\n";
        for name in [
            "t.timer",
            "t.target",
            "t.path",
            "t.slice",
            "t.device",
            "svc.service",
        ] {
            let w = parse_verify_output(unrelated, 0, name).unwrap();
            assert_eq!(w.len(), 1, "{name} 应放行且只带无关告警");
        }
    }

    /// fixture：⑧ RC≠0 但输出里没有本单元行 → 仍拒（GOAL 口径）并注明。
    #[test]
    fn verify_fixture_rc_nonzero_without_own_lines() {
        let e = parse_verify_output("some/other.service: boom\n", 1, "mine.service").unwrap_err();
        assert!(e.contains("RC=1"), "{e}");
        assert!(e.contains("some/other.service"), "原文带上供排查：{e}");
    }

    /// verify_unit：scope 类型**跳过工具**（确定性，不碰 systemd-analyze）。
    #[test]
    fn verify_unit_scope_skips_tool() {
        let w = verify_unit("[Unit]\nDescription=x\n", "run.scope", "scope").unwrap();
        assert!(w[0].contains("跳过"), "{w:?}");
        // 名字与类型不符 → 拒
        let e = verify_unit("[Unit]\n", "x.service", "timer").unwrap_err();
        assert!(e.contains("不一致"), "{e}");
        let e = verify_unit("[Unit]\n", "bad name", "service").unwrap_err();
        assert!(e.contains("不合法"), "{e}");
    }

    /// 真跑一次 verify（本机 systemd 在位；缺工具的机器自动放行跳过，防环境敏感红）。
    #[test]
    fn verify_unit_runs_real_tool() {
        if Command::new("systemd-analyze")
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .is_err()
        {
            eprintln!("本机无 systemd-analyze，跳过真机 verify 断言");
            return;
        }
        // 坏单元 → 拒，且原文含 ExecStart 提示
        let bad = "[Service]\nType=simple\n";
        let e = verify_unit(bad, "linbox-vprobe.service", "service").unwrap_err();
        assert!(e.contains("ExecStart"), "{e}");
        // 好单元 → 过
        let good = "[Unit]\nDescription=probe\n\n[Service]\nType=oneshot\nExecStart=/bin/true\n";
        verify_unit(good, "linbox-vprobe.service", "service").expect("好单元应通过 verify");
    }

    // ── 写入链路 ───────────────────────────────────────────────────────

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("linbox-unitfile-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 写入：新建（无旧文件 → 无备份）+ 已有（备份 = 旧内容）+ outcome 字段。
    #[test]
    fn write_creates_backup_and_outcome() {
        let dir = tmpdir("write");
        // 新建
        let p = dir.join("new.service");
        let out = write_unit_with(&p, "[Unit]\nDescription=new\n", || 0).unwrap();
        assert_eq!(out.backup, None, "新建没有旧文件可备份");
        assert!(!out.elevated, "/tmp 可直写");
        assert_eq!(out.reload_rc, 0);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "[Unit]\nDescription=new\n"
        );
        // 覆盖：备份 = 旧内容
        let out = write_unit_with(&p, "[Unit]\nDescription=changed\n", || 0).unwrap();
        let bak = out.backup.unwrap();
        assert_eq!(bak, backup_path(&p));
        assert_eq!(
            std::fs::read_to_string(&bak).unwrap(),
            "[Unit]\nDescription=new\n"
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "[Unit]\nDescription=changed\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// reload 失败 → 从备份回滚（GOAL 11.3 要求的注入断言）。
    #[test]
    fn write_reload_failure_rolls_back() {
        let dir = tmpdir("rollback");
        let p = dir.join("r.service");
        std::fs::write(&p, "[Unit]\nDescription=old\n").unwrap();
        let e = write_unit_with(&p, "[Unit]\nDescription=new\n", || 1).unwrap_err();
        assert!(e.contains("daemon-reload 失败"), "{e}");
        assert!(e.contains("回滚"), "{e}");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "[Unit]\nDescription=old\n",
            "目标必须恢复旧内容"
        );
        // 备份仍在（取证用）
        assert!(backup_path(&p).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 缺尾换行拒绝（D11.6）。
    #[test]
    fn write_requires_trailing_newline() {
        let dir = tmpdir("newline");
        let p = dir.join("n.service");
        let e = write_unit_with(&p, "[Unit]\nDescription=x", || 0).unwrap_err();
        assert!(e.contains("换行"), "{e}");
        assert!(!p.exists(), "被拒的内容不落盘");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 内容必须过自家往返自检：奇排版（缩进键 / 多空格 / CRLF）解析后语义等值 → 放行。
    #[test]
    fn write_accepts_semantically_roundtrippable_content() {
        let dir = tmpdir("roundtrip");
        let p = dir.join("rt.service");
        let content = "[Unit]\r\n    Description=带缩进与\r\n  多   空格  \r\n\r\n[Service]\r\nExecStart=/bin/true\r\n";
        write_unit_with(&p, content, || 0).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), content, "原样落盘");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 备份命名与覆盖策略：`.linbox.bak` 同名直接覆盖（与 fstab 一致）。
    #[test]
    fn backup_naming_and_overwrite() {
        let dir = tmpdir("bak");
        let p = dir.join("b.service");
        std::fs::write(&p, "v1\n").unwrap();
        write_unit_with(&p, "v2\n", || 0).unwrap();
        assert_eq!(std::fs::read_to_string(backup_path(&p)).unwrap(), "v1\n");
        write_unit_with(&p, "v3\n", || 0).unwrap();
        assert_eq!(
            std::fs::read_to_string(backup_path(&p)).unwrap(),
            "v2\n",
            "第二次写入覆盖上一次的备份"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 提权脚本形状：建目录 / 备份 / cat 落盘 / 0644，一条 `set -e` 脚本。
    #[test]
    fn elevated_write_script_shape() {
        let target = Path::new("/etc/systemd/system/my.service");
        let tmp = Path::new("/tmp/linbox-unit-1-2.tmp");
        let s = elevated_write_script(target, tmp);
        assert!(s.starts_with("set -e"));
        assert!(s.contains("mkdir -p -- '/etc/systemd/system'"));
        assert!(s.contains("if [ -e '/etc/systemd/system/my.service' ]"));
        assert!(s.contains(
            "cp -a -- '/etc/systemd/system/my.service' '/etc/systemd/system/my.service.linbox.bak'"
        ));
        assert!(s.contains("cat -- '/tmp/linbox-unit-1-2.tmp' > '/etc/systemd/system/my.service'"));
        assert!(s.contains("chmod 0644 '/etc/systemd/system/my.service'"));
        // 绝不含内容本身（内容走临时文件，零 shell 展开面）
        assert!(!s.contains("ExecStart"));
    }

    /// 提权脚本：路径含单引号也要被正确转义（不注入）。
    #[test]
    fn elevated_write_script_quotes_paths() {
        let target = Path::new("/etc/systemd/system/a'b.service");
        let tmp = Path::new("/tmp/x'z");
        let s = elevated_write_script(target, tmp);
        // ' → '\''
        assert!(s.contains("a'\\''b.service"), "{s}");
        assert!(s.contains("/tmp/x'\\''z"));
        // 危险字符路径本就被 check_path_safety 拦，这里验证脚本层转义兜底
        assert_eq!(sh_q("plain"), "'plain'");
        assert_eq!(sh_q("a'b"), "'a'\\''b'");
    }

    /// 提权 argv：`pkexec sh -c <script>`。
    #[test]
    fn unit_write_argv_is_pkexec_sh() {
        let argv = unit_write_argv("set -e");
        assert_eq!(argv[0], "pkexec");
        assert_eq!(argv[1], "sh");
        assert_eq!(argv[2], "-c");
        assert_eq!(argv[3], "set -e");
        assert_eq!(argv.len(), 4);
    }

    /// daemon-reload argv 按作用域带不带 `--user`（作用域搞错会 reload 错实例）。
    #[test]
    fn daemon_reload_argv_scopes() {
        assert_eq!(
            daemon_reload_argv(Scope::System),
            vec!["systemctl", "daemon-reload"]
        );
        assert_eq!(
            daemon_reload_argv(Scope::User),
            vec!["systemctl", "--user", "daemon-reload"]
        );
    }

    /// 名字与类型一致性闸门（11 类白名单，防止给 .timer 文件按 service 校验）。
    #[test]
    fn name_type_consistency_gate() {
        for t in UNIT_TYPES {
            let name = format!("probe.{t}");
            // scope 短路、其它类型要真跑工具 —— 这里只断言「类型不一致」这一支
            let e = verify_unit("[Unit]\n", &name, "not-a-type").unwrap_err();
            assert!(e.contains("不一致"), "{t}: {e}");
        }
        // 11 类里除 scope 外都能走到真工具调用；scope 短路
        assert!(verify_unit("[Unit]\n", "x.scope", "scope").is_ok());
        // 新建白名单 = 9 类（GOAL 11.8 断言基础，与 model 常量同源）
        assert_eq!(NEWABLE_TYPES.len(), 9);
    }

    // ── 新建单元（11.5 / D11.9）─────────────────────────────────────────

    /// 命名白名单：合法 9 形态（含模板名）。
    #[test]
    fn validate_unit_name_legal_shapes() {
        let ok: &[(&str, &str)] = &[
            ("mysvc.service", "service"),
            ("a.service", "service"),
            ("my-svc_2.service", "service"),
            ("foo@.service", "service"),
            ("app-x@autostart.service", "service"),
            ("backup.timer", "timer"),
            ("mnt-foo.mount", "mount"),
            ("dev-sda2.swap", "swap"),
            ("a:b.c.service", "service"),
        ];
        for (n, t) in ok {
            assert!(validate_unit_name(n, t).is_ok(), "{n} / {t} 应放行");
        }
    }

    /// 命名白名单：非法 12 形态（GOAL 11.5 清单 + 边界）。
    #[test]
    fn validate_unit_name_illegal_shapes() {
        let bad: &[(&str, &str)] = &[
            ("../x.service", "service"),
            ("a/b.service", "service"),
            ("has space.service", "service"),
            ("", "service"),
            ("x.timer", "service"), // 后缀与所选类型不符
            ("..", "service"),
            ("bad\nchar.service", "service"),
            ("-lead.service", "service"),
            (".lead.service", "service"),
            ("nul\0.service", "service"),
            ("x.conf", "service"), // 非 11 类后缀
            ("x", "service"),      // 无后缀
        ];
        for (n, t) in bad {
            assert!(validate_unit_name(n, t).is_err(), "{n:?} 应被拒");
        }
    }

    /// 类型必须在 11 类白名单内。
    #[test]
    fn validate_unit_name_requires_whitelisted_type() {
        assert!(validate_unit_name("x.service", "socket.io").is_err());
        assert!(validate_unit_name("x.service", "").is_err());
        assert!(validate_unit_name("x.service", "Service").is_err());
    }

    /// 9 类默认模板：parse → render 逐字节往返。
    #[test]
    fn default_new_doc_all_9_types_roundtrip() {
        for t in NEWABLE_TYPES {
            for scope in [Scope::System, Scope::User] {
                let txt = default_new_doc(scope, t).unwrap();
                let d = parse_unit_text(&txt);
                assert_eq!(render_unit_text(&d), txt, "{t} 往返必须逐字节一致");
            }
        }
    }

    /// 9 类默认模板的节结构（target/slice 无专用节；socket/mount/… 无 [Install] 默认不写）。
    #[test]
    fn default_new_doc_sections_per_type() {
        let cases: &[(&str, &[&str])] = &[
            ("service", &["Unit", "Service", "Install"]),
            ("timer", &["Unit", "Timer", "Install"]),
            ("target", &["Unit", "Install"]),
            ("socket", &["Unit", "Socket"]),
            ("mount", &["Unit", "Mount"]),
            ("automount", &["Unit", "Automount"]),
            ("path", &["Unit", "Path"]),
            ("slice", &["Unit"]),
            ("swap", &["Unit", "Swap"]),
        ];
        assert_eq!(cases.len(), 9);
        for (t, secs) in cases {
            let d = parse_unit_text(&default_new_doc(Scope::System, t).unwrap());
            assert_eq!(&d.section_names(), secs, "{t} 的节序列");
        }
    }

    /// WantedBy 按作用域分流；timer 恒为 timers.target。
    #[test]
    fn default_new_doc_wantedby_by_scope_and_timer() {
        let s = default_new_doc(Scope::System, "service").unwrap();
        assert!(s.contains("WantedBy=multi-user.target"), "{s}");
        let u = default_new_doc(Scope::User, "service").unwrap();
        assert!(u.contains("WantedBy=default.target"), "{u}");
        let timer = default_new_doc(Scope::User, "timer").unwrap();
        assert!(timer.contains("WantedBy=timers.target"), "{timer}");
        assert!(timer.contains("OnBootSec=1min"), "{timer}");
    }

    /// 必填项在模板里故意留空 → validate 必须拦住「不填关键键就保存」。
    #[test]
    fn default_new_doc_required_gaps_block_validate() {
        let d = parse_unit_text(&default_new_doc(Scope::System, "service").unwrap());
        let e = crate::model::unitfile::validate_form(&d, "service", None).unwrap_err();
        assert!(e.contains("ExecStart"), "{e}");
        let d = parse_unit_text(&default_new_doc(Scope::System, "mount").unwrap());
        let e = crate::model::unitfile::validate_form(&d, "mount", None).unwrap_err();
        assert!(e.contains("What") || e.contains("Where"), "{e}");
        // 跨类型防护：给 mount 单元写 service 专用键 → 拒（键不适用于该类型）
        let form =
            crate::model::unitfile::FormValues::new().set_text("Service", "ExecStart", "/bin/true");
        let e = crate::model::unitfile::apply_form(&d, &form, "mount").unwrap_err();
        assert!(e.contains("不适用"), "{e}");
        let d = parse_unit_text(&default_new_doc(Scope::System, "service").unwrap());
        let form =
            crate::model::unitfile::FormValues::new().set_text("Service", "ExecStart", "/bin/true");
        let d2 = crate::model::unitfile::apply_form(&d, &form, "service").unwrap();
        crate::model::unitfile::validate_form(&d2, "service", None).unwrap();
    }

    /// timer 默认模板直接过 validate（无需额外必填）。
    #[test]
    fn default_new_doc_timer_passes_validate() {
        let t = default_new_doc(Scope::User, "timer").unwrap();
        let d = parse_unit_text(&t);
        crate::model::unitfile::validate_form(&d, "timer", Some("linbox-probe.timer")).unwrap();
    }

    /// device / scope 不给新建（D11.9-⑤）；未知类型同样拒。
    #[test]
    fn default_new_doc_rejects_unnewable_types() {
        assert!(default_new_doc(Scope::System, "device").is_err());
        assert!(default_new_doc(Scope::System, "scope").is_err());
        assert!(default_new_doc(Scope::System, "socket.io").is_err());
    }

    /// 目标路径：系统 / 用户两形态，**绝不落 /usr/lib**；非法名直接拒。
    #[test]
    fn create_path_shapes_never_usr_lib() {
        let sys = create_path(Scope::System, "linbox-x.service").unwrap();
        assert_eq!(sys, PathBuf::from("/etc/systemd/system/linbox-x.service"));
        let user = create_path(Scope::User, "linbox-x.service").unwrap();
        let s = user.display().to_string();
        assert!(s.ends_with("/.config/systemd/user/linbox-x.service"), "{s}");
        assert!(!s.starts_with("/usr/lib"), "绝不写 vendor 目录");
        assert!(create_path(Scope::System, "../evil.service").is_err());
        assert!(create_path(Scope::System, "x.conf").is_err());
    }

    /// `unit_paths` 输出解析（fixture = 摸底 2 的真实 logind / cron 输出形态）。
    #[test]
    fn parse_unit_paths_fixture() {
        let out = "FragmentPath=/usr/lib/systemd/system/systemd-logind.service\nDropInPaths=/etc/systemd/system/systemd-logind.service.d/98-deepin-debug.conf /usr/lib/systemd/system/systemd-logind.service.d/dbus.conf\nUnitFileState=static\n";
        let p = parse_unit_paths(out);
        assert_eq!(p.fragment, "/usr/lib/systemd/system/systemd-logind.service");
        assert_eq!(p.drop_ins.len(), 2);
        assert!(p.drop_ins[0].ends_with("98-deepin-debug.conf"));
        assert_eq!(p.unit_file_state, "static");
        let out = "FragmentPath=/usr/lib/systemd/system/cron.service\nDropInPaths=\nUnitFileState=enabled\n";
        let p = parse_unit_paths(out);
        assert!(p.drop_ins.is_empty());
        assert_eq!(p.unit_file_state, "enabled");
        assert_eq!(parse_unit_paths(""), UnitPaths::default());
    }

    /// 真机：取用户级真实单元的路径信息（环境无 systemctl 时自动跳过）。
    #[test]
    fn unit_paths_real_user_unit() {
        if !systemctl_present() {
            return;
        }
        match unit_paths(Scope::User, "pulseaudio.service") {
            Ok(p) => {
                assert!(p.fragment.ends_with("pulseaudio.service"), "{p:?}");
                assert!(
                    matches!(
                        p.unit_file_state.as_str(),
                        "enabled" | "disabled" | "static" | "masked"
                    ),
                    "{:?}",
                    p.unit_file_state
                );
            }
            Err(e) => assert!(!e.contains("无法执行"), "{e}"),
        }
    }

    /// 真机：同名冲突检测（不存在 → None；用户级真实存在的 pulseaudio.service → Some）。
    #[test]
    fn name_conflict_real_machine() {
        if !systemctl_present() {
            return;
        }
        let c = name_conflict(Scope::User, "linbox-zzz-nonexistent.service").unwrap();
        assert!(c.is_none(), "{c:?}");
        if let Ok(p) = unit_paths(Scope::User, "pulseaudio.service")
            && !p.fragment.is_empty()
        {
            let c = name_conflict(Scope::User, "pulseaudio.service").unwrap();
            assert!(
                c.as_ref().is_some_and(|m| m.contains("请改用「编辑」")),
                "{c:?}"
            );
        }
    }

    /// Where/What → 单元名推导（mount/automount/swap）。
    #[test]
    fn unit_name_from_where_and_what() {
        assert_eq!(unit_name_from_where("/mnt/foo", "mount"), "mnt-foo.mount");
        assert_eq!(
            unit_name_from_where("/a/b/c", "automount"),
            "a-b-c.automount"
        );
        assert_eq!(unit_name_from_what("/dev/sda2"), "dev-sda2.swap");
    }

    /// socket → 同名 service 推导。
    #[test]
    fn socket_peer_name_shape() {
        assert_eq!(socket_peer_name("foo.socket").unwrap(), "foo.service");
        assert!(socket_peer_name("foo.service").is_err());
        assert!(socket_peer_name(".socket").is_err());
    }

    // ── 删除计划与命令（11.6 / D11.10）─────────────────────────────────

    /// 删除计划真值表：active×enabled 四象限的步骤顺序。
    #[test]
    fn delete_plan_quadrants() {
        let mk = |active: bool, enabled: bool| {
            build_delete_plan(
                Scope::System,
                "x.service",
                "/etc/systemd/system/x.service",
                &[],
                if enabled { "enabled" } else { "disabled" },
                if active { "active" } else { "inactive" },
            )
            .unwrap()
        };
        let p = mk(true, true);
        assert!(p.steps[0].contains("停止"), "{:?}", p.steps);
        assert!(p.steps[1].contains("禁用"), "{:?}", p.steps);
        assert!(p.steps[2].contains(".linbox.bak"), "{:?}", p.steps);
        assert!(p.steps[3].contains("daemon-reload"), "{:?}", p.steps);
        let p = mk(true, false);
        assert!(p.steps[0].contains("停止") && !p.steps.iter().any(|s| s.contains("禁用")));
        let p = mk(false, true);
        assert!(p.steps[0].contains("禁用") && !p.steps.iter().any(|s| s.contains("停止")));
        let p = mk(false, false);
        assert_eq!(p.steps.len(), 2, "{:?}", p.steps); // mv + reload
        // enabled-runtime 也算启用
        let p = build_delete_plan(
            Scope::User,
            "x.service",
            "/home/u/.config/systemd/user/x.service",
            &[],
            "enabled-runtime",
            "active",
        )
        .unwrap();
        assert!(p.enabled);
    }

    /// 删除闸门：瞬态 / masked / 非法名 → Err。
    #[test]
    fn delete_plan_gates() {
        let e = build_delete_plan(Scope::System, "x.service", "", &[], "enabled", "active")
            .unwrap_err();
        assert!(e.contains("没有单元文件") && e.contains("停止"), "{e}");
        let e = build_delete_plan(
            Scope::System,
            "x.service",
            "/dev/null",
            &[],
            "masked",
            "inactive",
        )
        .unwrap_err();
        assert!(e.contains("取消屏蔽"), "{e}");
        let e = build_delete_plan(
            Scope::System,
            "x.service",
            "/etc/systemd/system/x.service",
            &[],
            "masked",
            "inactive",
        )
        .unwrap_err();
        assert!(e.contains("取消屏蔽"), "{e}");
        let e = build_delete_plan(
            Scope::System,
            "x.conf",
            "/etc/systemd/system/x.conf",
            &[],
            "disabled",
            "inactive",
        )
        .unwrap_err();
        assert!(e.contains("白名单"), "{e}");
    }

    /// vendor 警告置位 + drop-in 目录随删进步骤。
    #[test]
    fn delete_plan_vendor_and_dropin() {
        let dirs = vec![PathBuf::from("/etc/systemd/system/cron.service.d")];
        let p = build_delete_plan(
            Scope::System,
            "cron.service",
            "/usr/lib/systemd/system/cron.service",
            &dirs,
            "disabled",
            "inactive",
        )
        .unwrap();
        assert!(
            p.vendor_warning
                .as_ref()
                .is_some_and(|w| w.contains("软件包升级会重装")),
            "{:?}",
            p.vendor_warning
        );
        assert!(
            p.steps.iter().any(|s| s.contains("cron.service.d")),
            "{:?}",
            p.steps
        );
        // 用户目录无警告
        let p = build_delete_plan(
            Scope::User,
            "x.service",
            "/home/u/.config/systemd/user/x.service",
            &[],
            "disabled",
            "inactive",
        )
        .unwrap();
        assert!(p.vendor_warning.is_none());
    }

    /// 类型无关：同一 FragmentPath 喂 service/timer/target 输出同构计划（11 类通用）。
    #[test]
    fn delete_plan_type_agnostic() {
        let shape = |name: &str| {
            let p = build_delete_plan(
                Scope::System,
                name,
                "/etc/systemd/system/probe",
                &[],
                "enabled",
                "active",
            )
            .unwrap();
            (
                p.active,
                p.enabled,
                p.steps
                    .iter()
                    .map(|s| s.replace(name, "N"))
                    .collect::<Vec<_>>(),
            )
        };
        let a = shape("a.service");
        let b = shape("a.timer");
        let c = shape("a.target");
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    /// 命令构造：系统级单条 pkexec 含 disable+daemon-reload；用户级零 pkexec；
    /// 注入形态在 validate_unit_name 就被拒，进不了命令。
    #[test]
    fn delete_cmd_shape() {
        let p = build_delete_plan(
            Scope::System,
            "x.service",
            "/etc/systemd/system/x.service",
            &[PathBuf::from("/etc/systemd/system/x.service.d")],
            "enabled",
            "active",
        )
        .unwrap();
        let argv = delete_cmd(&p);
        assert_eq!(argv[0], "pkexec");
        assert_eq!(argv[1], "sh");
        assert_eq!(argv.len(), 4, "必须是一条命令：{argv:?}");
        let script = &argv[3];
        assert!(script.contains("systemctl stop 'x.service'"), "{script}");
        assert!(script.contains("systemctl disable 'x.service'"), "{script}");
        assert!(script.contains("daemon-reload"), "{script}");
        assert!(script.contains("x.service.d"), "{script}");
        assert!(!script.contains('*'), "绝不 glob：{script}");

        let p = build_delete_plan(
            Scope::User,
            "x.timer",
            "/home/u/.config/systemd/user/x.timer",
            &[],
            "disabled",
            "inactive",
        )
        .unwrap();
        let argv = delete_cmd(&p);
        assert_eq!(argv[0], "sh", "用户级零 pkexec：{argv:?}");
        assert!(
            argv[2].contains("systemctl --user daemon-reload"),
            "{argv:?}"
        );

        // 注入 10 形态全拒（validate_unit_name 先行）
        for bad in [
            "../x.service",
            "a/b.service",
            "a b.service",
            "",
            ".service", // 空基名
            "a..b.service",
            "a\u{1}b.service",
            "-x.service",
            "a\0b.service",
            "x.conf",
        ] {
            let r = build_delete_plan(
                Scope::System,
                bad,
                "/etc/systemd/system/x.service",
                &[],
                "disabled",
                "inactive",
            );
            assert!(r.is_err(), "应拒绝：{bad:?}");
        }
    }

    /// `.wants` 清理由 disable 承担，对 service/timer/socket 同构（摸底 12/15）。
    #[test]
    fn delete_wants_cleanup_isomorphic() {
        for name in ["a.service", "a.timer", "a.socket"] {
            let p = build_delete_plan(
                Scope::System,
                name,
                &format!("/etc/systemd/system/{name}"),
                &[],
                "enabled",
                "inactive",
            )
            .unwrap();
            let script = delete_script(&p);
            assert!(
                script.contains(&format!("systemctl disable '{name}'")),
                "{name}: {script}"
            );
        }
    }

    /// 回滚：备份在 → 搬回原名。
    #[test]
    fn delete_rollback_restores_backup() {
        let dir = std::env::temp_dir().join(format!("linbox-delrb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let frag = dir.join("r.service");
        let bak = PathBuf::from(format!("{}.linbox.bak", frag.display()));
        std::fs::write(&frag, "[Unit]\nDescription=orig\n").unwrap();
        std::fs::rename(&frag, &bak).unwrap(); // 模拟 mv 之后
        assert!(!frag.exists() && bak.exists());
        restore_from_delete_backup(&frag, &bak, Scope::User).unwrap();
        assert!(frag.exists() && !bak.exists(), "备份应已搬回");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 执行层回读断言：mv 失败（目录只读）→ 文件仍在 → Err，不产生假成功。
    #[test]
    fn delete_run_readback_asserts() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("linbox-delro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let frag = dir.join("r.service");
        std::fs::write(&frag, "[Unit]\n").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let p = build_delete_plan(
            Scope::User,
            "r.service",
            &frag.display().to_string(),
            &[],
            "disabled",
            "inactive",
        )
        .unwrap();
        let e = run_delete(&p).unwrap_err();
        assert!(e.contains("删除未生效"), "{e}");
        assert!(frag.exists(), "文件必须还在");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真机 dry-run（用户级，零提权）：建临时单元 → enable 落 .wants → 删除链路 →
    /// 断言单元文件与 .wants 链接都不在、.linbox.bak 在 → 清理现场。
    #[test]
    fn delete_user_scope_real_dry_run() {
        if !systemctl_present() {
            return;
        }
        let name = format!("linbox-ufdel-test-{}.service", std::process::id());
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let frag = home.join(".config/systemd/user").join(&name);
        let content = "[Unit]\nDescription=linbox delete dry-run\n\n[Service]\nType=oneshot\nExecStart=/bin/true\n\n[Install]\nWantedBy=default.target\n"
            .to_string();
        if std::fs::write(&frag, &content).is_err() {
            return;
        }
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        let _ = Command::new("systemctl")
            .args(["--user", "enable", &name])
            .output();
        let wants = home
            .join(".config/systemd/user/default.target.wants")
            .join(&name);

        let plan = delete_plan(Scope::User, &name).expect("计划应产出");
        assert!(plan.enabled, "enable 后应判启用：{:?}", plan);
        assert!(wants.exists(), ".wants 链接应已落下");
        let out = run_delete(&plan).expect("用户级删除应成功");

        let bak = PathBuf::from(format!("{}.linbox.bak", frag.display()));
        assert!(!frag.exists(), "单元文件应已移走");
        assert!(!wants.exists(), ".wants 链接应已清除（无悬空）");
        assert!(bak.exists(), ".linbox.bak 应在");
        assert!(out.backups.contains(&bak));

        // 清理现场
        let _ = std::fs::remove_file(&bak);
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
    }

    /// systemctl 是否在位（真机用例的自适应跳过）。
    fn systemctl_present() -> bool {
        Command::new("systemctl")
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .is_ok()
    }
}
