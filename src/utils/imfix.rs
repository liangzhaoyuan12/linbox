//! 输入法修复逻辑层（纯逻辑，不依赖 GTK / libadwaita / glib）。
//!
//! 解决 fcitx5 在 Wayland 下因 `/etc/environment` 缺少输入法环境变量，导致部分窗口
//! （GTK / Qt / SDL / GLFW 等）无法使用输入法的问题。
//!
//! 本模块只做检测与生成写入内容，页面层（`page::fcitx_fix`）只调用 [`detect`] 与
//! [`apply`]，自身不碰任何控件、不读文件、不启进程。需要 root 时通过 `pkexec` 提权，
//! 但这一切对页面层透明。
//!
//! 约束（见 `docs/项目结构规划书.md` §3.7）：本文件禁止 `use gtk` / `use adw` /
//! `use glib`，输入输出均为数据。

use crate::model::imfix::{ImfixReport, REQUIRED};

/// 目标配置文件路径。
const ENV_PATH: &str = "/etc/environment";

/// 检测 `fcitx5` 是否安装。
pub fn detect_fcitx5() -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v fcitx5 >/dev/null 2>&1")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 读取 `/etc/environment` 文本（不可读时返回空字符串）。
pub fn read_env() -> String {
    std::fs::read_to_string(ENV_PATH).unwrap_or_default()
}

/// 逐行判断哪些必需变量缺失：返回 (已配置数量, 缺失列表)。
///
/// 判定标准：存在以 `NAME=` 开头的行（忽略前导空白）即视为「已配置」。
/// 已存在的行原样保留，不做覆盖。
pub fn compute_missing(content: &str) -> (usize, Vec<(String, String)>) {
    let mut configured = 0usize;
    let mut missing = Vec::new();
    for (name, value) in REQUIRED {
        let present = content
            .lines()
            .any(|l| l.trim_start().starts_with(&format!("{name}=")));
        if present {
            configured += 1;
        } else {
            missing.push(((*name).to_string(), (*value).to_string()));
        }
    }
    (configured, missing)
}

/// 生成需要追加的内容（仅缺失项 + 一行说明注释）。
pub fn build_additions(missing: &[(String, String)]) -> String {
    if missing.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "\n# fcitx5 输入法环境变量（由 linbox 添加，修复 Wayland 下部分窗口无法使用输入法）\n",
    );
    for (name, value) in missing {
        s.push_str(&format!("{name}={value}\n"));
    }
    s
}

/// 以 root 权限把完整新内容写回 `/etc/environment`（通过 `pkexec` 提权）。
///
/// 新内容由调用方算好（原内容 + 追加项），通过 stdin 传给提权后的 `cat`，
/// 避免命令行参数里嵌入特殊字符（`@`、`=` 等），也避免临时文件残留。
pub fn write_env_as_root(new_content: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("pkexec")
        .arg("sh")
        .arg("-c")
        .arg(format!("cat > {ENV_PATH}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法启动 pkexec（请确认已安装 polkit）：{e}"))?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "无法获取 pkexec 标准输入".to_string())?;
        stdin
            .write_all(new_content.as_bytes())
            .map_err(|e| format!("写入失败：{e}"))?;
        // `stdin` 在此处被丢弃 → 给 `cat` 发送 EOF
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("等待 pkexec 失败：{e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "修复失败（pkexec 退出码 {}）：{}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        ));
    }
    Ok(())
}

/// 执行一次完整检测，返回结构化结论。
pub fn detect() -> ImfixReport {
    let fcitx_installed = detect_fcitx5();
    let (configured, missing) = compute_missing(&read_env());
    ImfixReport {
        fcitx_installed,
        configured,
        total: REQUIRED.len(),
        missing,
    }
}

/// 根据检测报告把缺失变量追加写入 `/etc/environment`（保留原有内容）。
///
/// 返回成功补齐的变量条数。报告本身不携带文件内容，故写入前重新读取最新文件，
/// 确保基于最新内容追加，避免外部改动导致覆盖。
pub fn apply(report: &ImfixReport) -> Result<usize, String> {
    if report.missing.is_empty() {
        return Ok(0);
    }
    let mut new_content = read_env();
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str(&build_additions(&report.missing));
    write_env_as_root(&new_content)?;
    Ok(report.missing.len())
}
