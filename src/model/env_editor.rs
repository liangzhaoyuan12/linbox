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
