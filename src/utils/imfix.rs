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

/// 重启 fcitx5（`fcitx5 -rd`：`-r` 重启 / 启动、`-d` 作为守护进程运行）。
///
/// 不需要 root 权限。
///
/// 关键：**不能**用 `.output()` / 捕获 stdout/stderr —— `fcitx5 -rd` 守护化后
/// 新实例会继承管道 fd，导致等待 EOF 永远不返回（UI 冻结）。因此直接丢弃
/// 输出、仅 `status()` 等待父进程退出（守护化后父进程立即退出）。
pub fn restart_fcitx5() -> Result<String, String> {
    use std::process::{Command, Stdio};

    let status = Command::new("fcitx5")
        .arg("-rd")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("无法启动 fcitx5：{e}（请确认已安装）"))?;

    if !status.success() {
        return Err(format!("fcitx5 -rd 退出码 {}", status.code().unwrap_or(-1)));
    }
    Ok("fcitx5 -rd 已执行，输入法进程已重启。".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_set() -> String {
        REQUIRED.iter().map(|(n, v)| format!("{n}={v}\n")).collect()
    }

    #[test]
    fn missing_all_when_empty() {
        let (configured, missing) = compute_missing("");
        assert_eq!(configured, 0);
        assert_eq!(missing.len(), REQUIRED.len());
        // 顺序即 REQUIRED 顺序，首个是 GTK_IM_MODULE
        assert_eq!(missing[0].0, "GTK_IM_MODULE");
        assert_eq!(missing[0].1, "fcitx");
        // 值含 @ 的不能被转义/截断
        let xm = missing.iter().find(|(n, _)| n == "XMODIFIERS").unwrap();
        assert_eq!(xm.1, "@im=fcitx");
    }

    #[test]
    fn counts_configured_partial() {
        let content = "GTK_IM_MODULE=fcitx\nQT_IM_MODULE=ibus\n";
        let (configured, missing) = compute_missing(content);
        assert_eq!(configured, 2);
        assert_eq!(missing.len(), REQUIRED.len() - 2);
        assert!(!missing.iter().any(|(n, _)| n == "GTK_IM_MODULE"));
    }

    #[test]
    fn indented_line_counts() {
        let (configured, _) = compute_missing("    GTK_IM_MODULE=fcitx\n");
        assert_eq!(configured, 1, "前导空白应被忽略");
    }

    #[test]
    fn longer_name_with_suffix_not_counted() {
        // `GTK_IM_MODULE_EXTRA=1` 不是以 `GTK_IM_MODULE=` 开头，不能算已配置
        let (configured, missing) = compute_missing("GTK_IM_MODULE_EXTRA=1\n");
        assert_eq!(configured, 0);
        assert_eq!(missing.len(), REQUIRED.len());
    }

    #[test]
    fn case_sensitive() {
        let (configured, _) = compute_missing("gtk_im_module=fcitx\n");
        assert_eq!(configured, 0, "变量名区分大小写");
    }

    #[test]
    fn no_equals_not_counted() {
        let (configured, _) = compute_missing("GTK_IM_MODULE\n");
        assert_eq!(configured, 0, "没有 = 的行不算已配置");
    }

    #[test]
    fn comment_not_counted() {
        let (configured, _) = compute_missing("# GTK_IM_MODULE=fcitx\n");
        assert_eq!(configured, 0, "注释行不算已配置");
    }

    #[test]
    fn all_configured_yields_no_missing() {
        let (configured, missing) = compute_missing(&all_set());
        assert_eq!(configured, REQUIRED.len());
        assert!(missing.is_empty());
    }

    /// GOAL 4.2：真实样本 —— 本机 /etc/environment 全文（fcitx 输入法七件套），
    /// 逐字节抄录为测试常量（不依赖运行时读文件）。
    const REAL_ETC_ENVIRONMENT: &str = "GTK_IM_MODULE=fcitx\nQT_IM_MODULE=fcitx\nXMODIFIERS=@im=fcitx\nINPUT_METHOD=fcitx\nSDL_IM_MODULE=fcitx\nGLFW_IM_MODULE=fcitx\nXIM=fcitx\n";

    #[test]
    fn real_etc_environment_fully_configured() {
        let (configured, missing) = compute_missing(REAL_ETC_ENVIRONMENT);
        assert_eq!(configured, 7, "7 行 KEY=VALUE 全部计为已配置");
        let names: Vec<&str> = missing.iter().map(|(n, _)| n.as_str()).collect();
        // 本机七项全是 fcitx：REQUIRED 里的输入法三项必须已配置
        for present in ["GTK_IM_MODULE", "QT_IM_MODULE", "XMODIFIERS"] {
            assert!(
                !names.contains(&present),
                "{present} 本机已配置不该进缺失列表"
            );
        }
        // 已配置齐的环境再生成追加内容应为空
        assert!(build_additions(&missing).is_empty(), "缺失为空则不追加");
    }

    #[test]
    fn build_additions_empty_is_empty() {
        assert_eq!(build_additions(&[]), "");
    }

    #[test]
    fn build_additions_header_and_lines() {
        let s = build_additions(&[("A".into(), "1".into()), ("B".into(), "2".into())]);
        assert!(s.contains("linbox 添加"), "应带说明注释头");
        assert!(s.contains("\nA=1\n"));
        assert!(s.contains("\nB=2\n"));
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn build_additions_keeps_at_sign() {
        let s = build_additions(&[("XMODIFIERS".into(), "@im=fcitx".into())]);
        assert!(s.contains("XMODIFIERS=@im=fcitx"), "@ 不能被转义");
    }

    #[test]
    fn roundtrip_missing_to_content_then_clean() {
        // 空内容 → 算缺失 → 生成追加 → 再算应全部配置（GOAL 4.2 golden 往返）
        let (configured0, missing) = compute_missing("");
        assert_eq!(configured0, 0);
        let additions = build_additions(&missing);
        let full = format!("{}{}", all_set_already_absent(&additions), additions);
        let (configured1, missing1) = compute_missing(&full);
        assert_eq!(configured1, REQUIRED.len());
        assert!(missing1.is_empty());
    }

    /// 测试辅助：原内容为空时就是追加文本本身。
    fn all_set_already_absent(additions: &str) -> String {
        let _ = additions;
        String::new()
    }
}
