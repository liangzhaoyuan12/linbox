//! systemd 管理的数据模型（纯数据，无 UI 依赖）。

/// systemd 的作用域：系统级（`systemctl`）或当前用户的用户级（`systemctl --user`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// 系统级单元，需要 root 的操作走 pkexec 提权。
    #[default]
    System,
    /// 当前用户的用户级单元（开机自启的桌面应用、pipewire、gnome-keyring 等），
    /// 由用户自己的 systemd 实例管理，**永远不需要提权**。
    User,
}

impl Scope {
    /// 该作用域是否需要 root 权限才能操作单元。
    pub fn needs_root(&self) -> bool {
        matches!(self, Scope::System)
    }

    /// 传给 systemctl 的作用域参数。
    pub fn args(&self) -> &'static [&'static str] {
        match self {
            Scope::System => &[],
            Scope::User => &["--user"],
        }
    }
}

/// 一个定时器（`systemctl list-timers`）。
#[derive(Debug, Clone, Default)]
pub struct Timer {
    /// 定时器单元名，如 `apt-daily.timer`。
    pub unit: String,
    /// 到点后激活的单元，如 `apt-daily.service`。
    pub activates: String,
    /// 距下次触发的可读时长（如 `3 小时 41 分`）；未排定时为空。
    pub next_in: String,
    /// 距上次触发的可读时长（如 `23 小时 5 分`）；从未触发时为空。
    pub last_ago: String,
}

/// 一个单元（service / timer / socket / mount …）的概要信息。
#[derive(Debug, Clone)]
pub struct Unit {
    /// 单元名，如 `sshd.service`。
    pub name: String,
    /// LoadState（loaded / not-found / masked / error / bad-setting …）。
    pub load: String,
    /// ActiveState（active / inactive / failed …）。
    pub active: String,
    /// 启用状态（来自 `list-unit-files`：enabled / disabled / static / masked …）。
    pub enabled: String,
    /// 描述。
    pub description: String,
}

/// 单元详情（`systemctl show` 解析出的关键字段 + 原始 `systemctl status` 文本）。
#[derive(Debug, Clone, Default)]
pub struct UnitDetail {
    /// 描述。
    pub description: String,
    /// LoadState。
    pub load: String,
    /// ActiveState。
    pub active: String,
    /// SubState。
    pub sub: String,
    /// 主进程 PID（无则空）。
    pub main_pid: String,
    /// 内存占用（字节，字符串形式，无则空）。
    pub memory: String,
    /// 单元文件路径（FragmentPath，无则空）。
    pub fragment_path: String,
    /// 开机自启状态（UnitFileState）。
    pub unit_file_state: String,
    /// 最近启动时间（ActiveEnterTimestamp）。
    pub active_enter: String,
    /// 重启次数（NRestarts）。
    pub restarts: String,
    /// `systemctl status` 原始文本（已去除分页控制）。
    pub status_text: String,
}

/// `journalctl` 的查询结果：正文与提示必须分开——
/// 普通用户运行 journalctl 时会往 stderr 打一段「你看不到系统日志」的 Hint，
/// 若与正文合并，会被当成一行日志显示出来。
#[derive(Debug, Clone, Default)]
pub struct JournalOutput {
    /// 日志正文（stdout）。
    pub text: String,
    /// stderr 提示（权限 Hint 等），可能为空。
    pub hint: String,
}

/// 系统电源动作。
#[derive(Debug, Clone, Copy)]
pub enum PowerAction {
    Shutdown,
    Reboot,
    Suspend,
    Hibernate,
}

impl PowerAction {
    /// 对应的 `systemctl` 子命令。
    pub fn systemctl_verb(&self) -> &'static str {
        match self {
            PowerAction::Shutdown => "poweroff",
            PowerAction::Reboot => "reboot",
            PowerAction::Suspend => "suspend",
            PowerAction::Hibernate => "hibernate",
        }
    }

    /// 中文标签（用于确认对话框）。
    pub fn label(&self) -> &'static str {
        match self {
            PowerAction::Shutdown => "关机",
            PowerAction::Reboot => "重启",
            PowerAction::Suspend => "挂起",
            PowerAction::Hibernate => "休眠",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_needs_root_and_args() {
        assert!(Scope::System.needs_root());
        assert!(!Scope::User.needs_root());
        // systemctl 默认即 system 作用域，System 不加参数
        assert_eq!(Scope::System.args(), &[] as &[&str]);
        assert_eq!(Scope::User.args(), &["--user"]);
    }

    #[test]
    fn power_action_verbs_and_labels() {
        assert_eq!(PowerAction::Shutdown.systemctl_verb(), "poweroff");
        assert_eq!(PowerAction::Reboot.systemctl_verb(), "reboot");
        assert_eq!(PowerAction::Suspend.systemctl_verb(), "suspend");
        assert_eq!(PowerAction::Hibernate.systemctl_verb(), "hibernate");
        assert_eq!(PowerAction::Shutdown.label(), "关机");
        assert_eq!(PowerAction::Reboot.label(), "重启");
        assert_eq!(PowerAction::Suspend.label(), "挂起");
        assert_eq!(PowerAction::Hibernate.label(), "休眠");
    }
}
