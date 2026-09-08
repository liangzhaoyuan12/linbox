//! 环境变量编辑器逻辑层（纯逻辑，不依赖 GTK / libadwaita / glib）。
//!
//! 负责：枚举系统用户、识别 shell、解析/序列化 rc 文件、权限探测，
//! 以及直接写入或通过 `pkexec` 提权写入。
//!
//! 权限模型：
//! - 改「当前用户」的配置文件：直接读写（文件无写权限时自动提权）；
//! - 改「root / 其他用户」：先探测当前进程是否有写权限（`can_write_direct`），
//!   没有就通过 `pkexec` 提权到 root 写入，并保持文件属主为对应用户。
//!
//! 约束（见 `docs/项目结构规划书.md` §3.7）：本文件禁止 `use gtk` / `use adw` /
//! `use glib`，输入输出均为数据。

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::model::env_editor::{Line, LoadResult, ShellKind, SystemUser, shell_from_path};

// ---------------------------------------------------------------------------
// 用户 / shell 识别
// ---------------------------------------------------------------------------

/// 当前进程的有效 UID（读 /proc/self/status 的 `Uid:` 第二字段，无 libc 依赖）。
pub fn current_uid() -> u32 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    s.lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// 枚举系统用户（/etc/passwd），排除 nologin / false 等不可登录用户。
///
/// 排序：当前用户 → root → 其余按 uid 升序。
pub fn list_users() -> Vec<SystemUser> {
    let me = current_uid();
    let mut all = Vec::new();
    if let Ok(content) = std::fs::read_to_string("/etc/passwd") {
        for line in content.lines() {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() < 7 {
                continue;
            }
            let shell = f[6].to_string();
            if shell.is_empty()
                || shell.ends_with("/nologin")
                || shell == "/bin/false"
                || shell == "/usr/bin/false"
            {
                continue;
            }
            let home = f[5].to_string();
            if home.is_empty() {
                continue;
            }
            let uid: u32 = f[2].parse().unwrap_or(u32::MAX);
            let gid: u32 = f[3].parse().unwrap_or(u32::MAX);
            all.push(SystemUser {
                name: f[0].to_string(),
                uid,
                gid,
                home,
                shell,
            });
        }
    }
    // 当前用户排最前，root 次之，其余按 uid 升序
    all.sort_by_key(|u| {
        (
            if u.uid == me {
                0
            } else if u.uid == 0 {
                1
            } else {
                2
            },
            u.uid,
        )
    });
    all
}

/// 判断某用户使用的 shell。
///
/// 当前用户优先读 `$SHELL` 环境变量（按需求「检测 shell 环境变量作判断」），
/// 失败回退 /etc/passwd；其他用户直接看 passwd。
pub fn shell_of(user: &SystemUser, is_current: bool) -> ShellKind {
    if is_current {
        if let Ok(sh) = std::env::var("SHELL") {
            let sh = sh.trim();
            if !sh.is_empty() {
                return shell_from_path(sh);
            }
        }
    }
    shell_from_path(&user.shell)
}

/// 用户的 rc 文件路径。
pub fn rc_path(home: &str, shell: &ShellKind) -> String {
    shell.rc_file(home)
}

// ---------------------------------------------------------------------------
// 解析 / 序列化
// ---------------------------------------------------------------------------

/// 解析 rc 文件内容为一组有序条目。
pub fn parse(content: &str, shell: &ShellKind) -> Vec<Line> {
    match shell {
        ShellKind::Fish => content.lines().map(parse_fish_line).collect(),
        _ => content.lines().map(parse_shell_line).collect(),
    }
}

/// bash / zsh 单行解析。
fn parse_shell_line(raw: &str) -> Line {
    let t = raw.trim_start();
    if t.is_empty() || t.starts_with('#') {
        return Line::Other(raw.to_string());
    }

    // `export KEY=value`（要求 export 后必须是空白，避免误判 exportABC=x）
    if t.starts_with("export") && t[6..].chars().next().map_or(false, |c| c.is_whitespace()) {
        let rest = t[6..].trim_start();
        if let Some((k, v, r)) = split_kv(rest) {
            if !k.is_empty() {
                return Line::Env {
                    key: k,
                    value: v,
                    exported: true,
                    raw: Some(r),
                };
            }
        }
        return Line::Other(raw.to_string());
    }

    // 裸赋值 `KEY=value`（不带 export；值以 `(` 开头是数组写法，不识别）
    if let Some((k, v, r)) = split_kv(t) {
        if is_ident(&k) && !v.starts_with('(') {
            return Line::Env {
                key: k,
                value: v,
                exported: false,
                raw: Some(r),
            };
        }
    }

    // `alias name=command`
    if t.starts_with("alias") && t[5..].chars().next().map_or(false, |c| c.is_whitespace()) {
        let rest = t[5..].trim_start();
        if let Some((n, c, r)) = split_kv(rest) {
            if is_ident(&n) && !c.is_empty() {
                return Line::Alias {
                    name: n,
                    command: c,
                    raw: Some(r),
                };
            }
        }
        return Line::Other(raw.to_string());
    }

    Line::Other(raw.to_string())
}

/// fish 单行解析（`set -gx KEY 值…` / `alias name 命令`）。
///
/// fish 的值可以是「被引号包裹的单值」或「空格分隔的列表」，两种写法含义不同，
/// 因此值一律按原文保留（所见即所写，界面不自动补引号）。
fn parse_fish_line(raw: &str) -> Line {
    let t = raw.trim_start();
    if t.is_empty() || t.starts_with('#') {
        return Line::Other(raw.to_string());
    }

    for prefix in ["set -gx ", "set -Ux ", "set -x "] {
        if let Some(rest) = t.strip_prefix(prefix) {
            if let Some(pos) = rest.find(|c: char| c.is_whitespace()) {
                let key = rest[..pos].trim();
                let value = rest[pos..].trim().to_string();
                if is_ident(key) {
                    return Line::Env {
                        key: key.to_string(),
                        value: value.clone(),
                        exported: true,
                        raw: Some(value),
                    };
                }
            }
            return Line::Other(raw.to_string());
        }
    }

    // fish: `alias name 'cmd'` 或 `alias name=cmd`；命令部分按原文保留
    if let Some(rest) = t.strip_prefix("alias ") {
        let rest = rest.trim_start();
        if !rest.is_empty() {
            let (name, cmd_raw): (&str, &str) = if let Some(eq) = rest.find('=') {
                (rest[..eq].trim(), rest[eq + 1..].trim())
            } else if let Some(ws) = rest.find(|c: char| c.is_whitespace()) {
                (rest[..ws].trim(), rest[ws..].trim())
            } else {
                return Line::Other(raw.to_string());
            };
            if is_ident(name) {
                return Line::Alias {
                    name: name.to_string(),
                    command: cmd_raw.to_string(),
                    raw: Some(cmd_raw.to_string()),
                };
            }
        }
        return Line::Other(raw.to_string());
    }

    Line::Other(raw.to_string())
}

/// 在第一个 `=` 处拆 `KEY=VALUE`；key 去除首尾空白。
/// 返回 `(键, 解析后的值, 原始值部分文本)`。
fn split_kv(s: &str) -> Option<(String, String, String)> {
    let eq = s.find('=')?;
    let key = s[..eq].trim();
    if key.is_empty() {
        return None;
    }
    let raw_part = s[eq + 1..].trim().to_string();
    let value = unquote_value(&raw_part);
    Some((key.to_string(), value, raw_part))
}

/// 去掉值两侧引号（内容原样保留，不做转义还原），并切掉行尾注释。
fn unquote_value(v: &str) -> String {
    let v = v.trim();
    if v.is_empty() {
        return String::new();
    }
    let first = v.chars().next().unwrap();
    if first == '"' || first == '\'' {
        // 首尾同一引号且引号后是空或注释 → 取引号内内容
        if let Some(pos) = v.rfind(first) {
            if pos > 0 {
                let inner = &v[1..pos];
                let after = v[pos + 1..].trim();
                if after.is_empty() || after.starts_with('#') {
                    return inner.to_string();
                }
            }
        }
    }
    // 未加引号：截掉「空白 + #」起始的注释（`KEY=v # note`）
    let mut cut = v.len();
    let mut prev_ws = false;
    for (i, c) in v.char_indices() {
        if c == '#' && prev_ws {
            cut = i;
            break;
        }
        prev_ws = c.is_whitespace();
    }
    v[..cut].trim_end().to_string()
}

/// 是否为合法的 shell 变量名。
fn is_ident(s: &str) -> bool {
    let mut it = s.chars();
    let Some(first) = it.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && it.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// bash / zsh 环境变量值引号处理：含空白或 shell 特殊字符时用双引号包裹并转义。
fn quote_value(v: &str) -> String {
    if v.is_empty() {
        return String::new();
    }
    let special = |c: char| c.is_whitespace() || "\\\"'$`;&|()<>*?[]#~".contains(c);
    if !v.chars().any(special) {
        return v.to_string();
    }
    let esc = v
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`");
    format!("\"{esc}\"")
}

/// bash / zsh 别名命令：始终用单引号包裹（转义内部单引号）。
fn quote_alias(v: &str) -> String {
    format!("'{}'", v.replace('\'', "'\\''"))
}

/// 解析 PATH 值 → (逐条路径, 是否引用了 $PATH)。
///
/// 按 `:` 分割；恰好等于 `$PATH` / `${PATH}` 的段视为「引用」并从路径里剔除，
/// 其余段（含 `$PATH/子目录` 这种内联引用）原样保留为一条路径。
pub fn parse_path_value(value: &str) -> (Vec<String>, bool) {
    let mut paths = Vec::new();
    let mut has_ref = false;
    for seg in value.split(':') {
        let t = seg.trim();
        if t == "$PATH" || t == "${PATH}" {
            has_ref = true;
        } else {
            paths.push(seg.to_string());
        }
    }
    (paths, has_ref)
}

/// 拼接 PATH 值；`append_path` 为 true 时把 `$PATH` 引用统一追加到末尾
/// （保存为 `export PATH="路径1:路径2:$PATH"` 的形式）。
pub fn join_path_value(paths: &[String], append_path: bool) -> String {
    let mut out = paths.join(":");
    if append_path {
        if !out.is_empty() {
            out.push(':');
        }
        out.push_str("$PATH");
    }
    out
}

/// fish 值引号处理。
fn fish_quote(v: &str) -> String {
    if v.is_empty() {
        return String::new();
    }
    let special = |c: char| c.is_whitespace() || "\\\"'$`;&|()<>*?[]#~".contains(c);
    if !v.chars().any(special) {
        return v.to_string();
    }
    let esc = v
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$");
    format!("\"{esc}\"")
}

/// 把条目序列化为 rc 文件内容。
///
/// 顺序：环境变量块 → 别名块 → 其他块，逐行写出。各块内相对顺序保留；
/// 未编辑过的行使用其原始写法（`raw`），保证文件逐字节不被改写。
pub fn serialize(lines: &[Line], shell: &ShellKind) -> String {
    let mut out = String::new();
    for line in lines {
        let s = match (shell, line) {
            (
                ShellKind::Fish,
                Line::Env {
                    key, value, raw, ..
                },
            ) => match raw {
                Some(r) => format!("set -gx {key} {r}"),
                None => format!("set -gx {key} {}", fish_quote(value)),
            },
            (ShellKind::Fish, Line::Alias { name, command, raw }) => match raw {
                Some(r) => format!("alias {name} {r}"),
                None => format!("alias {name} {}", fish_quote(command)),
            },
            (
                _,
                Line::Env {
                    key,
                    value,
                    exported,
                    raw,
                },
            ) => {
                let prefix = if *exported { "export " } else { "" };
                match raw {
                    Some(r) => format!("{prefix}{key}={r}"),
                    None => format!("{prefix}{key}={}", quote_value(value)),
                }
            }
            (_, Line::Alias { name, command, raw }) => match raw {
                Some(r) => format!("alias {name}={r}"),
                None => format!("alias {name}={}", quote_alias(command)),
            },
            (_, Line::Other(s)) => s.clone(),
        };
        out.push_str(&s);
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// 读取 / 权限探测
// ---------------------------------------------------------------------------

/// 直接读取文件：`Ok(None)` = 不存在，`Ok(Some)` = 内容，`Err` = 失败。
pub fn read_target(path: &str) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("读取 {path} 失败：{e}")),
    }
}

/// 探测当前进程能否直接写入目标文件（先判断有没有权限）。
///
/// - euid = 0 恒可写；
/// - 文件存在：尝试以写模式打开；
/// - 文件不存在：在父目录创建瞬时探测文件再删除（不留下副作用）。
pub fn can_write_direct(path: &str) -> bool {
    if current_uid() == 0 {
        return true;
    }
    let p = Path::new(path);
    if let Ok(md) = std::fs::metadata(p) {
        if md.permissions().readonly() {
            return false;
        }
        return std::fs::OpenOptions::new().write(true).open(p).is_ok();
    }
    let Some(parent) = p.parent() else {
        return false;
    };
    if parent.as_os_str().is_empty() {
        return false;
    }
    let probe = parent.join(format!(".linbox-write-test-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(f) => {
            drop(f);
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// shell 单引号转义（把路径安全地嵌入 `sh -c` 脚本）。
fn sh_q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 通过 pkexec 提权读取（用于 /root 等当前用户无权限的目录）。
fn read_target_elevated(path: &str) -> Result<Option<String>, String> {
    let script = format!("cat {}", sh_q(path));
    let output = Command::new("pkexec")
        .arg("sh")
        .arg("-c")
        .arg(&script)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("无法启动 pkexec（请确认已安装 polkit）：{e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such file") {
            return Ok(None);
        }
        return Err(format!(
            "提权读取失败（pkexec 退出码 {}）：{}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        ));
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

/// 加载一个 rc 文件：优先直接读；权限不足时自动通过 pkexec 提权读。
///
/// 返回 `(内容（None=不存在）, 是否可直接写入)`。
pub fn load(path: &str) -> Result<(Option<String>, bool), String> {
    let content = if current_uid() == 0 {
        read_target(path)?
    } else {
        match read_target(path) {
            Ok(c) => c,
            Err(_) => read_target_elevated(path)?,
        }
    };
    let writable = can_write_direct(path);
    Ok((content, writable))
}

/// 一次完整的「为用户加载 rc」：识别 shell、拼路径并读取。
pub fn load_user(user: &SystemUser, is_current: bool) -> Result<LoadResult, String> {
    let shell = shell_of(user, is_current);
    let path = rc_path(&user.home, &shell);
    let (content, writable) = load(&path)?;
    Ok(LoadResult {
        user: user.clone(),
        shell,
        path,
        content,
        writable_direct: writable,
    })
}

// ---------------------------------------------------------------------------
// 写入
// ---------------------------------------------------------------------------

/// 备份文件路径（同一目录下滚动覆盖）。
fn backup_path(path: &str) -> String {
    format!("{path}.linbox.bak")
}

/// 保存 rc 文件。`elevated` 为 true 时通过 pkexec 提权写入，
/// 写入后把文件属主修正为 `user`，保证目标用户可继续编辑。
pub fn save(path: &str, content: &str, user: &SystemUser, elevated: bool) -> Result<(), String> {
    if elevated && current_uid() != 0 {
        pkexec_write(path, content, user)
    } else {
        write_direct(path, content)
    }
}

/// 普通用户直写（当前用户自己的文件）。先备份，再原子落盘。
fn write_direct(path: &str, content: &str) -> Result<(), String> {
    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录 {} 失败：{e}", parent.display()))?;
        }
    }
    if p.exists() {
        let bak = backup_path(path);
        std::fs::copy(p, &bak)
            .map_err(|e| format!("备份 {path} 到 {bak} 失败（已中止保存，避免覆盖丢失）：{e}"))?;
    }
    std::fs::write(p, content).map_err(|e| format!("写入 {path} 失败：{e}"))
}

/// 通过 pkexec 提权写入：内容先落到本用户临时文件（0600），
/// 再以 root 执行「备份 → 写入 → 修正属主/权限 → 清理临时文件」脚本。
///
/// 写入用 `cat tmp > dst` 而非 install：目标若是符号链接（dotfiles 场景）
/// 会跟随链接写入目标文件，不会破坏链接本身。
fn pkexec_write(path: &str, content: &str, user: &SystemUser) -> Result<(), String> {
    // 1) 待写入内容落盘到临时文件（0600，root 可读）
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("linbox-rc-{}-{nanos}.tmp", current_uid()));
    let tmp_str = tmp.to_string_lossy().into_owned();

    {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("创建临时文件失败：{e}"))?;
        let mut w = std::io::BufWriter::new(f);
        w.write_all(content.as_bytes())
            .map_err(|e| format!("写入临时文件失败：{e}"))?;
        w.flush().ok();
    }

    // 2) 拼提权脚本（全部路径单引号转义；最后 `;` 保证临时文件必被清理）
    let parent = Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let bak = backup_path(path);
    let script = format!(
        "mkdir -p -- {} && if [ -e {} ]; then cp -a {} {}; fi && cat {} > {} && chown {}:{} {} && chmod 0644 {} ; rm -f {}",
        sh_q(&parent),
        sh_q(path),
        sh_q(path),
        sh_q(&bak),
        sh_q(&tmp_str),
        sh_q(path),
        user.uid,
        user.gid,
        sh_q(path),
        sh_q(path),
        sh_q(&tmp_str),
    );

    let output = Command::new("pkexec")
        .arg("sh")
        .arg("-c")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("无法启动 pkexec（请确认已安装 polkit）：{e}"))?;

    let _ = std::fs::remove_file(&tmp); // 进程侧兜底清理
    if !output.status.success() {
        return Err(format!(
            "提权写入失败（pkexec 退出码 {}）：{}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bash_env_variants() {
        let shell = ShellKind::Bash;
        let lines = parse(
            "# 注释行\nexport PATH=\"$HOME/.local/bin:$PATH\"\nEDITOR=vim\nFOO=\"a b\"\nalias ll='ls -alF'\nempty line:---\n",
            &shell,
        );
        assert_eq!(lines.len(), 6);
        match &lines[1] {
            Line::Env {
                key,
                value,
                exported,
                raw,
            } => {
                assert_eq!(key, "PATH");
                assert_eq!(value, "$HOME/.local/bin:$PATH");
                assert!(*exported);
                assert_eq!(raw.as_deref(), Some("\"$HOME/.local/bin:$PATH\""));
            }
            _ => panic!("expect env"),
        }
        match &lines[2] {
            Line::Env {
                key,
                value,
                exported,
                ..
            } => {
                assert_eq!(key, "EDITOR");
                assert_eq!(value, "vim");
                assert!(!*exported);
            }
            _ => panic!("expect env"),
        }
        match &lines[3] {
            Line::Env { key, value, .. } => {
                assert_eq!(key, "FOO");
                assert_eq!(value, "a b");
            }
            _ => panic!("expect env"),
        }
        match &lines[4] {
            Line::Alias { name, command, .. } => {
                assert_eq!(name, "ll");
                assert_eq!(command, "ls -alF");
            }
            _ => panic!("expect alias"),
        }
        assert!(matches!(&lines[5], Line::Other(s) if s == "empty line:---"));
    }

    #[test]
    fn roundtrip_bash_preserves_raw_forms() {
        let shell = ShellKind::Bash;
        // 常见写法：$HOME 引用、单引号别名 —— 未编辑时逐字节原样写回
        let src =
            "export PATH=\"$HOME/.local/bin:$PATH\" # 追加到 PATH\nalias ll='ls -alF'\nA='x'\n";
        let lines = parse(src, &shell);
        let out = serialize(&lines, &shell);
        assert_eq!(out, src);
        // 再解析一次应与原内容等值
        let lines2 = parse(&out, &shell);
        assert_eq!(lines, lines2);
    }

    #[test]
    fn edited_value_gets_standard_quoting() {
        let shell = ShellKind::Bash;
        // 模拟「用户改了值」：raw 置空 → 按标准规则加引号
        let lines = vec![Line::Env {
            key: "PATH".into(),
            value: "$HOME/.local/bin:$PATH".into(),
            exported: true,
            raw: None,
        }];
        let out = serialize(&lines, &shell);
        assert_eq!(out, "export PATH=\"\\$HOME/.local/bin:\\$PATH\"\n");
    }

    #[test]
    fn trailing_comment_cut() {
        assert_eq!(unquote_value("v # note"), "v");
        assert_eq!(unquote_value("v#x"), "v#x");
        assert_eq!(unquote_value("\"v\" # note"), "v");
        assert_eq!(unquote_value("\"a b\""), "a b");
    }

    #[test]
    fn fish_parse_roundtrip() {
        let shell = ShellKind::Fish;
        let src = "set -gx PATH \"$HOME/.local/bin\" $PATH\nalias ll 'ls -alF'\n";
        let lines = parse(src, &shell);
        assert_eq!(lines.len(), 2);
        match &lines[0] {
            Line::Env { key, value, .. } => {
                assert_eq!(key, "PATH");
                // fish 值按原文保留（可能是列表写法）
                assert_eq!(value, "\"$HOME/.local/bin\" $PATH");
            }
            _ => panic!("expect env"),
        }
        let out = serialize(&lines, &shell);
        assert_eq!(out, src);
    }

    #[test]
    fn arrays_and_weird_lines_kept_as_other() {
        let shell = ShellKind::Bash;
        let src = "arr=(1 2 3)\nexport\nFOO+=x\nif [ -f /etc/x ]; then\n  echo hi\nfi\n";
        let lines = parse(src, &shell);
        assert!(lines.iter().all(|l| matches!(l, Line::Other(_))));
    }

    #[test]
    fn serialize_orders_blocks() {
        let shell = ShellKind::Bash;
        let lines = vec![
            Line::Other("# head".to_string()),
            Line::Env {
                key: "A".into(),
                value: "1".into(),
                exported: true,
                raw: None,
            },
            Line::Other("# mid".to_string()),
            Line::Alias {
                name: "x".into(),
                command: "echo hi".into(),
                raw: None,
            },
        ];
        let out = serialize(&lines, &shell);
        let parts: Vec<&str> = out.trim_end().lines().collect();
        assert_eq!(parts[0], "# head");
        assert_eq!(parts[1], "export A=1");
        assert_eq!(parts[2], "# mid");
        assert_eq!(parts[3], "alias x='echo hi'");
    }

    #[test]
    fn path_parse_join() {
        // 纯引用段（$PATH / ${PATH}）从路径里剔除，其余路径保留
        let (p, r) = parse_path_value("/a:$PATH:/b:${PATH}");
        assert_eq!(p, vec!["/a", "/b"]);
        assert!(r);
        // 引用在最前（如 PATH="$PATH:$HOME/bin"）→ 同样识别为引用，路径取 $HOME/bin
        let (p0, r0) = parse_path_value("$PATH:$HOME/bin");
        assert_eq!(p0, vec!["$HOME/bin"]);
        assert!(r0);
        // 内含 $PATH 子串的段（如 $PATH/sub）不是「引用锚点」，
        // 按字面路径保留（避免保存时重复追加 $PATH 或改变其语义）
        let (p2, r2) = parse_path_value("/usr/local/bin:$PATH/sub:/usr/bin");
        assert_eq!(p2, vec!["/usr/local/bin", "$PATH/sub", "/usr/bin"]);
        assert!(!r2);
        // 无引用
        let (p3, r3) = parse_path_value("/usr/local/bin:/usr/bin");
        assert_eq!(p3, vec!["/usr/local/bin", "/usr/bin"]);
        assert!(!r3);
        // 拼接
        assert_eq!(
            join_path_value(&["/a".into(), "/b".into()], true),
            "/a:/b:$PATH"
        );
        assert_eq!(join_path_value(&[].to_vec(), true), "$PATH");
        assert_eq!(join_path_value(&["/a".into()], false), "/a");
    }

    #[test]
    fn shell_path_detection() {
        assert_eq!(shell_from_path("/bin/bash"), ShellKind::Bash);
        assert_eq!(shell_from_path("/usr/bin/zsh"), ShellKind::Zsh);
        assert_eq!(shell_from_path("/usr/bin/fish"), ShellKind::Fish);
        assert_eq!(shell_from_path("/bin/dash"), ShellKind::Other);
    }
}
