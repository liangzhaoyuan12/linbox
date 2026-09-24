//! 环境变量编辑器相关数据模型（纯数据，无 UI 依赖）。
//!
//! 参考 `docs/项目结构规划书.md` §3.8：`model/` 仅定义 `struct`/`enum`，
//! 不依赖任何 UI 框架；`utils` 读写 `model`，`page` 展示 `model`。

/// 识别的 shell 种类（由登录 shell 路径判断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// bash → ~/.bashrc
    Bash,
    /// zsh → ~/.zshrc
    Zsh,
    /// fish → ~/.config/fish/config.fish
    Fish,
    /// 其他无法识别的 shell（按 bash 的 .bashrc 处理，并在界面给出提示）。
    Other,
}

impl ShellKind {
    /// 该 shell 对应用户级配置文件路径。
    pub fn rc_file(&self, home: &str) -> String {
        match self {
            ShellKind::Bash => format!("{home}/.bashrc"),
            ShellKind::Zsh => format!("{home}/.zshrc"),
            ShellKind::Fish => format!("{home}/.config/fish/config.fish"),
            ShellKind::Other => format!("{home}/.bashrc"),
        }
    }
}

/// 从 shell 可执行文件路径（如 `/bin/bash`）识别 shell 种类。
pub fn shell_from_path(shell: &str) -> ShellKind {
    match shell.rsplit('/').next().unwrap_or(shell) {
        "bash" => ShellKind::Bash,
        "zsh" => ShellKind::Zsh,
        "fish" => ShellKind::Fish,
        _ => ShellKind::Other,
    }
}

/// 系统中的一个用户（来自 /etc/passwd）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// 家目录。
    pub home: String,
    /// 登录 shell 路径（/etc/passwd 第 7 字段）。
    pub shell: String,
}

/// rc 文件中一行条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// 环境变量：`export KEY=value` 或裸赋值 `KEY=value`。
    Env {
        key: String,
        value: String,
        /// 是否带 `export` 前缀（保存时保留原形式）。
        exported: bool,
        /// 原始「值部分」文本（含引号/转义/行尾注释），未编辑的行按原样写回；
        /// `None` = 用户新增或已编辑，保存时按标准规则加引号。
        raw: Option<String>,
    },
    /// 别名：`alias name=command`。
    Alias {
        name: String,
        command: String,
        /// 原始「命令部分」文本（同上）。
        raw: Option<String>,
    },
    /// 其他行（注释、函数、条件判断、空行等），按原样保留。
    Other(String),
}

impl Line {
    /// 是否为环境变量条目。
    pub fn is_env(&self) -> bool {
        matches!(self, Line::Env { .. })
    }

    /// 是否为别名条目。
    pub fn is_alias(&self) -> bool {
        matches!(self, Line::Alias { .. })
    }
}

/// 一次「加载某个用户 rc 文件」的结果。
#[derive(Debug, Clone)]
pub struct LoadResult {
    /// 目标用户。
    pub user: SystemUser,
    /// 识别出的 shell。
    pub shell: ShellKind,
    /// rc 文件路径。
    pub path: String,
    /// `Some(文本)` = 文件存在；`None` = 文件不存在（保存时可新建）。
    pub content: Option<String>,
    /// 当前进程能否直接写入该文件（无需提权）。
    pub writable_direct: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rc_file_paths_per_shell() {
        let home = "/home/u";
        assert_eq!(ShellKind::Bash.rc_file(home), "/home/u/.bashrc");
        assert_eq!(ShellKind::Zsh.rc_file(home), "/home/u/.zshrc");
        assert_eq!(
            ShellKind::Fish.rc_file(home),
            "/home/u/.config/fish/config.fish",
            "fish 走 XDG 配置目录"
        );
        assert_eq!(
            ShellKind::Other.rc_file(home),
            "/home/u/.bashrc",
            "未知 shell 按 bash 处理（界面给提示）"
        );
    }

    #[test]
    fn shell_from_path_matrix() {
        assert_eq!(shell_from_path("/bin/bash"), ShellKind::Bash);
        assert_eq!(shell_from_path("/usr/bin/zsh"), ShellKind::Zsh);
        assert_eq!(shell_from_path("/usr/local/bin/fish"), ShellKind::Fish);
        assert_eq!(shell_from_path("bash"), ShellKind::Bash, "无斜杠取全串");
        assert_eq!(shell_from_path(""), ShellKind::Other, "空值 → Other");
        assert_eq!(shell_from_path("/usr/bin/nu"), ShellKind::Other);
        // 大小写敏感：BASH 不是 bash
        assert_eq!(shell_from_path("/bin/BASH"), ShellKind::Other);
    }

    #[test]
    fn line_kind_detection() {
        let env = Line::Env {
            key: "PATH".into(),
            value: "/usr/bin".into(),
            exported: true,
            raw: None,
        };
        assert!(env.is_env() && !env.is_alias());
        let alias = Line::Alias {
            name: "ll".into(),
            command: "ls -l".into(),
            raw: None,
        };
        assert!(alias.is_alias() && !alias.is_env());
        let other = Line::Other("# 注释".into());
        assert!(!other.is_env() && !other.is_alias());
    }
}
