//! 垃圾清理数据层（GOAL Phase 9 / 任务 9.1）。
//!
//! 纯数据：可清理位置 catalog（GOAL 9.1 附录一～十全量收录）+ 扫描结果数据结构。
//! 架构约束（GOAL Phase 9）：本文件禁止 `use gtk` / `use adw` / `use glib`。
//!
//! 字段约定：
//! - `path` 以 `~` 开头表示用户主目录、含 `$UID` 表示当前用户 id（展开在
//!   `utils::cleaner::expand_path`，本层保持静态字面量）；
//! - `path` 含 `*` 的条目是**通配目标**（如 `/boot/vmlinuz-*`），存在性探测需先展开通配；
//! - `pattern` 为 `Some` 的条目属于「通用文件模式」（第九类），`path` 即模式串本身，
//!   扫描时对所有真实挂载点按文件名匹配（见 `utils::cleaner::scan_pattern`）。

/// 十大类（对应 GOAL 9.1 附录一～十）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    /// 一、包管理器缓存
    PkgCache,
    /// 二、系统日志
    SystemLog,
    /// 三、临时文件
    TempFiles,
    /// 四、用户级缓存
    UserCache,
    /// 五、旧内核（只提示不自删，D9.4）
    OldKernel,
    /// 六、崩溃报告与调试符号
    CrashDebug,
    /// 七、开发工具缓存
    DevCache,
    /// 八、桌面环境与系统服务缓存
    DesktopSvc,
    /// 九、通用可清理文件模式（跨挂载点按文件名匹配）
    GenericPattern,
    /// 十、容器与虚拟化（多数只报不清，D9.9）
    Container,
}

impl Category {
    /// 十类全量（顺序 = 附录顺序），供「十类分类各有条目」等完整性测试。
    pub const ALL: [Category; 10] = [
        Category::PkgCache,
        Category::SystemLog,
        Category::TempFiles,
        Category::UserCache,
        Category::OldKernel,
        Category::CrashDebug,
        Category::DevCache,
        Category::DesktopSvc,
        Category::GenericPattern,
        Category::Container,
    ];

    /// 中文分类名（UI 分组标题）。
    pub fn label(&self) -> &'static str {
        match self {
            Category::PkgCache => "包管理器缓存",
            Category::SystemLog => "系统日志",
            Category::TempFiles => "临时文件",
            Category::UserCache => "用户级缓存",
            Category::OldKernel => "旧内核",
            Category::CrashDebug => "崩溃报告与调试符号",
            Category::DevCache => "开发工具缓存",
            Category::DesktopSvc => "桌面与系统服务缓存",
            Category::GenericPattern => "通用临时文件模式",
            Category::Container => "容器与虚拟化",
        }
    }
}

/// 风险三档（D9.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Risk {
    /// 第一优先级：默认勾选。
    Safe,
    /// 第二优先级：默认不勾，可一键全选。
    OnDemand,
    /// 第三优先级：默认不勾 + 二次确认弹窗列出将执行的命令。
    Caution,
}

impl Risk {
    pub const ALL: [Risk; 3] = [Risk::Safe, Risk::OnDemand, Risk::Caution];

    /// 中文风险徽章。
    pub fn label(&self) -> &'static str {
        match self {
            Risk::Safe => "安全",
            Risk::OnDemand => "按需",
            Risk::Caution => "谨慎",
        }
    }

    /// D9.2：只有【安全】默认勾选。
    pub fn default_checked(&self) -> bool {
        matches!(self, Risk::Safe)
    }
}

/// 清理方式（D9.3 / GOAL 9.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CleanupKind {
    /// 直接删除：用户级 rm 白名单路径（系统级路径由执行层包 pkexec）。
    Direct,
    /// 专用清理命令（journalctl --vacuum、dnf clean 等），命令 argv 在
    /// `utils::cleaner::cleanup_argv` 按条目构造。
    Command,
    /// 只提示：显示体积与建议命令，不提供删除动作（D9.4 旧内核 / D9.9 只报项）。
    Hint,
}

impl CleanupKind {
    pub const ALL: [CleanupKind; 3] =
        [CleanupKind::Direct, CleanupKind::Command, CleanupKind::Hint];

    pub fn label(&self) -> &'static str {
        match self {
            CleanupKind::Direct => "直接删除",
            CleanupKind::Command => "专用命令",
            CleanupKind::Hint => "只提示",
        }
    }
}

/// 一条可清理位置（静态 catalog 数据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanTarget {
    /// 唯一 id（kebab/snake，命令构造与结果回填按 id 匹配）。
    pub id: &'static str,
    /// 所属类别（十大类之一）。
    pub category: Category,
    /// 路径（`~`/`$UID` 待展开）；模式条目 = 模式串本身。
    pub path: &'static str,
    /// 中文说明（UI 列表展示，非空且为中文）。
    pub desc: &'static str,
    /// 风险档（D9.2）。
    pub risk: Risk,
    /// 清理方式（D9.3）。
    pub cleanup_kind: CleanupKind,
    /// `Some(模式)` = 通用文件模式条目（第九类），对所有真实挂载点按文件名匹配。
    pub pattern: Option<&'static str>,
}

/// D9.5 个人数据红线：永不进 catalog、永不被通用模式扫描删除。
///
/// 匹配规则见 `utils::cleaner::is_red_line`（路径在主目录下且落在这六个目录内，
/// 或位于含 `.git` 的源码仓库子树中）。
pub const RED_LINE_DIRS: &[&str] = &[
    "~/Documents",
    "~/Desktop",
    "~/Pictures",
    "~/Music",
    "~/Videos",
    "~/Downloads",
];

/// 可清理位置全量 catalog（GOAL 9.1 附录，用户给定十类为最低要求）。
///
/// 每条 `path` 全局唯一（父子包含关系除外，父子去重由 9.4 聚合层处理，D9.9）。
/// 风险标注 = 附录【安全】【按需】【谨慎】。
pub const CATALOG: &[CleanTarget] = &[
    // ── 一、包管理器缓存 ──────────────────────────────────────────────
    CleanTarget {
        id: "apt_archives",
        category: Category::PkgCache,
        path: "/var/cache/apt/archives/",
        desc: "APT 已下载的 .deb 安装包缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Command,
        pattern: None,
    },
    CleanTarget {
        id: "apt_partial",
        category: Category::PkgCache,
        path: "/var/cache/apt/archives/partial/",
        desc: "APT 未下载完的临时文件",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Command,
        pattern: None,
    },
    CleanTarget {
        id: "apt_lists",
        category: Category::PkgCache,
        path: "/var/lib/apt/lists/",
        desc: "APT 软件源索引（清理后可重建）",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "apt_xapian_bin",
        category: Category::PkgCache,
        path: "/var/cache/apt/*.bin",
        desc: "apt-xapian 索引二进制",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "apt_xapian_index",
        category: Category::PkgCache,
        path: "/var/cache/apt-xapian-index/",
        desc: "apt-xapian 索引目录",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "dnf_cache",
        category: Category::PkgCache,
        path: "/var/cache/dnf/",
        desc: "Fedora/RHEL DNF 下载缓存（含 dnf5）",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Command,
        pattern: None,
    },
    CleanTarget {
        id: "yum_cache",
        category: Category::PkgCache,
        path: "/var/cache/yum/",
        desc: "CentOS/RHEL YUM 下载缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Command,
        pattern: None,
    },
    CleanTarget {
        id: "packagekit_cache",
        category: Category::PkgCache,
        path: "/var/cache/PackageKit/",
        desc: "PackageKit 元数据缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "tdnf_cache",
        category: Category::PkgCache,
        path: "/var/cache/tdnf/",
        desc: "openEuler/Phoenix tdnf 缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "pacman_pkg",
        category: Category::PkgCache,
        path: "/var/cache/pacman/pkg/",
        desc: "Arch/manjaro pacman 已下载包缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Command,
        pattern: None,
    },
    CleanTarget {
        id: "paru_cache",
        category: Category::PkgCache,
        path: "/var/cache/paru/",
        desc: "AUR 助手 paru 缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "yay_cache",
        category: Category::PkgCache,
        path: "/var/cache/yay/",
        desc: "AUR 助手 yay 缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "zypp_cache",
        category: Category::PkgCache,
        path: "/var/cache/zypp/",
        desc: "openSUSE zypper 下载缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "xbps_cache",
        category: Category::PkgCache,
        path: "/var/cache/xbps/",
        desc: "Void Linux xbps 缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "apk_cache",
        category: Category::PkgCache,
        path: "/var/cache/apk/",
        desc: "Alpine apk 缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "opkg_cache",
        category: Category::PkgCache,
        path: "/var/cache/opkg/",
        desc: "OpenWrt opkg 缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "pisi_cache",
        category: Category::PkgCache,
        path: "/var/cache/pisi/",
        desc: "Pardus pisi 缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "eopkg_cache",
        category: Category::PkgCache,
        path: "/var/cache/eopkg/",
        desc: "Solus eopkg 缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "portage_distfiles",
        category: Category::PkgCache,
        path: "/var/cache/gentoo/distfiles/",
        desc: "Gentoo Portage 源码包缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "portage_binpkgs",
        category: Category::PkgCache,
        path: "/var/cache/binpkgs/",
        desc: "Gentoo Portage 二进制包缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "nix_cache",
        category: Category::PkgCache,
        path: "/var/cache/nix/",
        desc: "Nix 包管理器缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "flatpak_user_repo",
        category: Category::PkgCache,
        path: "~/.local/share/flatpak/repo/",
        desc: "用户级 Flatpak 仓库（应用与运行时存储）",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "flatpak_system_repo",
        category: Category::PkgCache,
        path: "/var/lib/flatpak/repo/",
        desc: "系统级 Flatpak 仓库（应用与运行时存储）",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "flatpak_unused",
        category: Category::PkgCache,
        path: "~/.local/share/flatpak/",
        desc: "未使用的 Flatpak 运行时与应用",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Command,
        pattern: None,
    },
    CleanTarget {
        id: "snapd_cache",
        category: Category::PkgCache,
        path: "/var/lib/snapd/cache/",
        desc: "Snap 下载的 squashfs 缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "snapd_tmp",
        category: Category::PkgCache,
        path: "/var/tmp/snapd/",
        desc: "Snapd 临时文件",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    // ── 二、系统日志 ─────────────────────────────────────────────────
    CleanTarget {
        id: "var_log",
        category: Category::SystemLog,
        path: "/var/log/",
        desc: "系统日志主目录（重点为 .gz/.old/.1/.2 轮转文件）",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Command,
        pattern: None,
    },
    CleanTarget {
        id: "journal_log",
        category: Category::SystemLog,
        path: "/var/log/journal/",
        desc: "systemd 持久化日志（journalctl --vacuum-size=200M）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Command,
        pattern: None,
    },
    CleanTarget {
        id: "log_apt",
        category: Category::SystemLog,
        path: "/var/log/apt/",
        desc: "APT 操作历史日志",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_dnf",
        category: Category::SystemLog,
        path: "/var/log/dnf.log*",
        desc: "DNF 日志及其轮转文件",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_yum",
        category: Category::SystemLog,
        path: "/var/log/yum.log*",
        desc: "YUM 日志及其轮转文件",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_xorg",
        category: Category::SystemLog,
        path: "/var/log/Xorg.*.log*",
        desc: "Xorg 旧显示服务器日志",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_messages",
        category: Category::SystemLog,
        path: "/var/log/messages*",
        desc: "系统消息日志及轮转文件",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_syslog",
        category: Category::SystemLog,
        path: "/var/log/syslog*",
        desc: "syslog 及轮转文件",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_kern",
        category: Category::SystemLog,
        path: "/var/log/kern.log*",
        desc: "内核日志及轮转文件",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_boot",
        category: Category::SystemLog,
        path: "/var/log/boot.log*",
        desc: "启动日志及轮转文件",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_firewalld",
        category: Category::SystemLog,
        path: "/var/log/firewalld*",
        desc: "firewalld 日志及轮转文件",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_bluetooth",
        category: Category::SystemLog,
        path: "/var/log/bluetooth/",
        desc: "蓝牙服务日志",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_gdm",
        category: Category::SystemLog,
        path: "/var/log/gdm/",
        desc: "GDM 显示管理器日志",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_anaconda",
        category: Category::SystemLog,
        path: "/var/log/anaconda/",
        desc: "Fedora 安装器日志",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_setroubleshoot",
        category: Category::SystemLog,
        path: "/var/log/setroubleshoot/",
        desc: "SELinux 排错日志",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_sudo",
        category: Category::SystemLog,
        path: "/var/log/sudo.log",
        desc: "sudo 使用日志",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_timedated",
        category: Category::SystemLog,
        path: "/var/log/timedated/",
        desc: "时间同步服务日志",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "xsession_errors",
        category: Category::SystemLog,
        path: "~/.xsession-errors*",
        desc: "图形会话错误日志及轮转文件",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_wtmp",
        category: Category::SystemLog,
        path: "/var/log/wtmp",
        desc: "登录统计记录（清理将丢失登录历史）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Command,
        pattern: None,
    },
    CleanTarget {
        id: "log_btmp",
        category: Category::SystemLog,
        path: "/var/log/btmp",
        desc: "失败登录记录（清理将丢失历史）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Command,
        pattern: None,
    },
    CleanTarget {
        id: "log_lastlog",
        category: Category::SystemLog,
        path: "/var/log/lastlog",
        desc: "最近登录记录（清理将丢失历史）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Command,
        pattern: None,
    },
    CleanTarget {
        id: "log_samba",
        category: Category::SystemLog,
        path: "/var/log/samba/",
        desc: "Samba 服务访问日志",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_httpd",
        category: Category::SystemLog,
        path: "/var/log/httpd/",
        desc: "Apache 服务访问日志",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "log_nginx",
        category: Category::SystemLog,
        path: "/var/log/nginx/",
        desc: "Nginx 服务访问日志",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    // ── 三、临时文件 ─────────────────────────────────────────────────
    CleanTarget {
        id: "tmp_dir",
        category: Category::TempFiles,
        path: "/tmp/",
        desc: "系统临时目录（只清超过阈值未修改的文件，D9.7）",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "var_tmp",
        category: Category::TempFiles,
        path: "/var/tmp/",
        desc: "跨重启临时目录（只清超过阈值未修改的文件，D9.7）",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "dev_shm",
        category: Category::TempFiles,
        path: "/dev/shm/",
        desc: "共享内存临时文件",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "run_user",
        category: Category::TempFiles,
        path: "/run/user/$UID/",
        desc: "当前用户运行时目录（会话期间文件）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "run_lock",
        category: Category::TempFiles,
        path: "/run/lock/",
        desc: "系统锁文件目录",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Hint,
        pattern: None,
    },
    // ── 四、用户级缓存 ───────────────────────────────────────────────
    CleanTarget {
        id: "user_cache",
        category: Category::UserCache,
        path: "~/.cache/",
        desc: "用户缓存总目录（含子项，合计去重计，D9.9）",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "thumbnails_home",
        category: Category::UserCache,
        path: "~/.thumbnails/",
        desc: "旧版缩略图缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "thumbnails_cache",
        category: Category::UserCache,
        path: "~/.cache/thumbnails/",
        desc: "文件管理器缩略图缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "firefox_cache",
        category: Category::UserCache,
        path: "~/.mozilla/firefox/*.default*/cache2/",
        desc: "Firefox 网页缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "mozilla_cache",
        category: Category::UserCache,
        path: "~/.cache/mozilla/",
        desc: "Mozilla 系浏览器缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "chrome_cache",
        category: Category::UserCache,
        path: "~/.cache/google-chrome/",
        desc: "Google Chrome 缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "chromium_cache",
        category: Category::UserCache,
        path: "~/.cache/chromium/",
        desc: "Chromium 缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "chromium_cfg_cache",
        category: Category::UserCache,
        path: "~/.config/chromium/Cache/",
        desc: "Chromium 配置目录内的缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "edge_cache",
        category: Category::UserCache,
        path: "~/.cache/microsoft-edge/",
        desc: "Microsoft Edge 缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "brave_cache",
        category: Category::UserCache,
        path: "~/.cache/BraveSoftware/Brave-Browser/",
        desc: "Brave 浏览器缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "opera_cache",
        category: Category::UserCache,
        path: "~/.cache/opera/",
        desc: "Opera 浏览器缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "epiphany_cache",
        category: Category::UserCache,
        path: "~/.cache/epiphany/",
        desc: "GNOME Web（Epiphany）缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "trash_files",
        category: Category::UserCache,
        path: "~/.local/share/Trash/files/",
        desc: "回收站文件（清空需确认，D9.3）",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "trash_info",
        category: Category::UserCache,
        path: "~/.local/share/Trash/info/",
        desc: "回收站元数据（与文件一并清空）",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "gnome_software",
        category: Category::UserCache,
        path: "~/.cache/gnome-software/",
        desc: "GNOME 软件中心缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "update_manager",
        category: Category::UserCache,
        path: "~/.cache/update-manager/",
        desc: "更新管理器缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "dconf_cache",
        category: Category::UserCache,
        path: "~/.cache/dconf/",
        desc: "dconf 设置缓存（重建需重启应用）",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "fontconfig_cache",
        category: Category::UserCache,
        path: "~/.cache/fontconfig/",
        desc: "字体配置缓存（可重建）",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "glib_schemas",
        category: Category::UserCache,
        path: "~/.cache/glib-2.0/schemas/",
        desc: "GLib schema 编译缓存（重建代价小）",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "recently_used",
        category: Category::UserCache,
        path: "~/.local/share/RecentlyUsed",
        desc: "最近使用文件记录（清理丢历史）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "plasmashell_cache",
        category: Category::UserCache,
        path: "~/.cache/plasmashell",
        desc: "KDE Plasma 壳缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "gwenview_cache",
        category: Category::UserCache,
        path: "~/.cache/gwenview/",
        desc: "KDE 图片查看器缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "klipper_cache",
        category: Category::UserCache,
        path: "~/.local/share/klipper",
        desc: "KDE 剪贴板历史",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "ibus_cache",
        category: Category::UserCache,
        path: "~/.cache/ibus/",
        desc: "ibus 输入法缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "fcitx_cache",
        category: Category::UserCache,
        path: "~/.cache/fcitx/",
        desc: "fcitx 输入法缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "fcitx5_cache",
        category: Category::UserCache,
        path: "~/.cache/fcitx5/",
        desc: "fcitx5 输入法缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    // ── 五、旧内核（D9.4：只识别、只提示，不自己删） ─────────────────
    CleanTarget {
        id: "boot_vmlinuz",
        category: Category::OldKernel,
        path: "/boot/vmlinuz-*",
        desc: "旧版本内核镜像（用包管理器卸载，D9.4）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Hint,
        pattern: None,
    },
    CleanTarget {
        id: "boot_initramfs",
        category: Category::OldKernel,
        path: "/boot/initramfs-*",
        desc: "旧版本 initramfs（用包管理器卸载，D9.4）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Hint,
        pattern: None,
    },
    CleanTarget {
        id: "boot_systemmap",
        category: Category::OldKernel,
        path: "/boot/System.map-*",
        desc: "旧版本内核符号表（随内核包卸载）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Hint,
        pattern: None,
    },
    CleanTarget {
        id: "boot_config",
        category: Category::OldKernel,
        path: "/boot/config-*",
        desc: "旧版本内核配置（随内核包卸载）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Hint,
        pattern: None,
    },
    CleanTarget {
        id: "boot_symvers",
        category: Category::OldKernel,
        path: "/boot/symvers-*",
        desc: "旧版本内核符号版本（随内核包卸载）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Hint,
        pattern: None,
    },
    CleanTarget {
        id: "lib_modules",
        category: Category::OldKernel,
        path: "/lib/modules/",
        desc: "内核模块目录（仅识别旧版本并提示，不自删）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Hint,
        pattern: None,
    },
    CleanTarget {
        id: "linux_headers",
        category: Category::OldKernel,
        path: "/usr/src/linux-headers-*",
        desc: "旧版本内核头文件（随内核包卸载）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Hint,
        pattern: None,
    },
    // ── 六、崩溃报告与调试符号 ───────────────────────────────────────
    CleanTarget {
        id: "var_crash",
        category: Category::CrashDebug,
        path: "/var/crash/",
        desc: "Apport 崩溃转储",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "abrt_spool",
        category: Category::CrashDebug,
        path: "/var/spool/abrt/",
        desc: "ABRT 崩溃转储队列",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "abrt_cache",
        category: Category::CrashDebug,
        path: "~/.cache/abrt/",
        desc: "用户级 ABRT 缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "coredump_dir",
        category: Category::CrashDebug,
        path: "/var/lib/systemd/coredump/",
        desc: "systemd coredump 存储（coredumpctl vacuum）",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Command,
        pattern: None,
    },
    // ── 七、开发工具缓存 ─────────────────────────────────────────────
    CleanTarget {
        id: "gradle_caches",
        category: Category::DevCache,
        path: "~/.gradle/caches/",
        desc: "Gradle 构建缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "m2_repo",
        category: Category::DevCache,
        path: "~/.m2/repository/",
        desc: "Maven 本地仓库（重新构建时会重新下载）",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "npm_cache",
        category: Category::DevCache,
        path: "~/.npm/",
        desc: "npm 下载缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Command,
        pattern: None,
    },
    CleanTarget {
        id: "pip_cache",
        category: Category::DevCache,
        path: "~/.cache/pip/",
        desc: "pip 下载缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "uv_cache",
        category: Category::DevCache,
        path: "~/.cache/uv/",
        desc: "uv 包管理器缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "uv_share",
        category: Category::DevCache,
        path: "~/.local/share/uv/",
        desc: "uv 解包后的工具与 Python 存储",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "cargo_registry",
        category: Category::DevCache,
        path: "~/.cargo/registry/",
        desc: "Cargo 拉取的 crate 源码缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "cargo_git",
        category: Category::DevCache,
        path: "~/.cargo/git/",
        desc: "Cargo git 依赖缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "go_build",
        category: Category::DevCache,
        path: "~/.cache/go-build/",
        desc: "Go 构建缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "go_mod",
        category: Category::DevCache,
        path: "~/go/pkg/mod/",
        desc: "Go 模块下载缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "ccache",
        category: Category::DevCache,
        path: "~/.ccache/",
        desc: "ccache 编译缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "sccache",
        category: Category::DevCache,
        path: "~/.cache/sccache/",
        desc: "sccache 分布式编译缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "electron_cache",
        category: Category::DevCache,
        path: "~/.cache/electron/",
        desc: "Electron 二进制下载缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "yarn_cache",
        category: Category::DevCache,
        path: "~/.cache/yarn/",
        desc: "yarn 下载缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "pnpm_cache",
        category: Category::DevCache,
        path: "~/.cache/pnpm/",
        desc: "pnpm 下载缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "bun_cache",
        category: Category::DevCache,
        path: "~/.bun/install/cache/",
        desc: "bun 下载缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "pypoetry_cache",
        category: Category::DevCache,
        path: "~/.cache/pypoetry/",
        desc: "Poetry 下载缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "coursier_cache",
        category: Category::DevCache,
        path: "~/.cache/coursier/",
        desc: "Coursier（JVM 依赖）缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "stack_cache",
        category: Category::DevCache,
        path: "~/.stack/",
        desc: "Haskell Stack 构建缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "composer_cache",
        category: Category::DevCache,
        path: "~/.cache/composer/",
        desc: "PHP Composer 下载缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "gem_cache",
        category: Category::DevCache,
        path: "~/.gem/",
        desc: "RubyGems 下载缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "conan_cache",
        category: Category::DevCache,
        path: "~/.cache/conan/",
        desc: "Conan C/C++ 包缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "pants_cache",
        category: Category::DevCache,
        path: "~/.cache/pants/",
        desc: "Pants 构建系统缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "huggingface_cache",
        category: Category::DevCache,
        path: "~/.cache/huggingface/",
        desc: "HuggingFace 模型缓存（体积大，只报不清，D9.9）",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Hint,
        pattern: None,
    },
    CleanTarget {
        id: "precommit_cache",
        category: Category::DevCache,
        path: "~/.cache/pre-commit/",
        desc: "pre-commit 框架缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    // ── 八、桌面环境与系统服务缓存 ───────────────────────────────────
    CleanTarget {
        id: "app_info",
        category: Category::DesktopSvc,
        path: "/var/cache/app-info/",
        desc: "AppStream 应用元数据缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "swcatalog",
        category: Category::DesktopSvc,
        path: "/var/cache/swcatalog/",
        desc: "Debian AppStream 目录缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "cups_cache",
        category: Category::DesktopSvc,
        path: "/var/cache/cups/",
        desc: "CUPS 打印缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "cups_spool",
        category: Category::DesktopSvc,
        path: "/var/spool/cups/",
        desc: "CUPS 打印队列（有排队作业时应跳过）",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "mail_spool",
        category: Category::DesktopSvc,
        path: "/var/spool/mail/",
        desc: "本地邮件池（不用本地邮件才清理）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "man_cache",
        category: Category::DesktopSvc,
        path: "/var/cache/man/",
        desc: "man 手册预格式化缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "packagekit_lib",
        category: Category::DesktopSvc,
        path: "/var/lib/PackageKit/",
        desc: "PackageKit 本地数据",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "dnf5_cache",
        category: Category::DesktopSvc,
        path: "/var/cache/dnf5/",
        desc: "dnf5 下载缓存",
        risk: Risk::Safe,
        cleanup_kind: CleanupKind::Command,
        pattern: None,
    },
    // ── 九、通用可清理文件模式（在各挂载点内按文件名匹配） ───────────
    CleanTarget {
        id: "pat_tmp",
        category: Category::GenericPattern,
        path: "*.tmp",
        desc: "临时文件 *.tmp",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: Some("*.tmp"),
    },
    CleanTarget {
        id: "pat_bak",
        category: Category::GenericPattern,
        path: "*.bak",
        desc: "备份残留 *.bak",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: Some("*.bak"),
    },
    CleanTarget {
        id: "pat_swp",
        category: Category::GenericPattern,
        path: "*.swp",
        desc: "编辑器交换文件 *.swp",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: Some("*.swp"),
    },
    CleanTarget {
        id: "pat_swo",
        category: Category::GenericPattern,
        path: "*.swo",
        desc: "编辑器交换文件 *.swo",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: Some("*.swo"),
    },
    CleanTarget {
        id: "pat_old",
        category: Category::GenericPattern,
        path: "*.old",
        desc: "旧版本残留文件 *.old",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: Some("*.old"),
    },
    CleanTarget {
        id: "pat_core",
        category: Category::GenericPattern,
        path: "core.*",
        desc: "内核转储文件 core.*",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: Some("core.*"),
    },
    CleanTarget {
        id: "pat_log_gz",
        category: Category::GenericPattern,
        path: "*.log.gz",
        desc: "压缩轮转日志 *.log.gz",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: Some("*.log.gz"),
    },
    CleanTarget {
        id: "pat_log_old",
        category: Category::GenericPattern,
        path: "*.log.old",
        desc: "轮转日志 *.log.old",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: Some("*.log.old"),
    },
    CleanTarget {
        id: "pat_tilde",
        category: Category::GenericPattern,
        path: "*~",
        desc: "编辑器备份文件 *~",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: Some("*~"),
    },
    CleanTarget {
        id: "pat_orig",
        category: Category::GenericPattern,
        path: "*.orig",
        desc: "补丁残留 *.orig",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: Some("*.orig"),
    },
    CleanTarget {
        id: "pat_rej",
        category: Category::GenericPattern,
        path: "*.rej",
        desc: "补丁拒绝文件 *.rej",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: Some("*.rej"),
    },
    CleanTarget {
        id: "pat_dpkg_old",
        category: Category::GenericPattern,
        path: "*.dpkg-old",
        desc: "dpkg 配置备份 *.dpkg-old",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: Some("*.dpkg-old"),
    },
    CleanTarget {
        id: "pat_dpkg_dist",
        category: Category::GenericPattern,
        path: "*.dpkg-dist",
        desc: "dpkg 新配置模板 *.dpkg-dist",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: Some("*.dpkg-dist"),
    },
    CleanTarget {
        id: "pat_rpmnew",
        category: Category::GenericPattern,
        path: "*.rpmnew",
        desc: "rpm 升级新配置 *.rpmnew（只记录不删）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Hint,
        pattern: Some("*.rpmnew"),
    },
    CleanTarget {
        id: "pat_rpmsave",
        category: Category::GenericPattern,
        path: "*.rpmsave",
        desc: "rpm 升级旧配置保存 *.rpmsave（只记录不删）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Hint,
        pattern: Some("*.rpmsave"),
    },
    // ── 十、容器与虚拟化（D9.9：只报不清） ──────────────────────────
    CleanTarget {
        id: "docker_storage",
        category: Category::Container,
        path: "/var/lib/docker/",
        desc: "Docker 镜像与容器存储（建议 docker system prune，只报不清）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Hint,
        pattern: None,
    },
    CleanTarget {
        id: "podman_system",
        category: Category::Container,
        path: "/var/lib/containers/storage/",
        desc: "系统级 Podman 存储（只报不清）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Hint,
        pattern: None,
    },
    CleanTarget {
        id: "podman_user",
        category: Category::Container,
        path: "~/.local/share/containers/",
        desc: "用户级 Podman 存储（只报不清）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Hint,
        pattern: None,
    },
    CleanTarget {
        id: "libvirt_cache",
        category: Category::Container,
        path: "/var/cache/libvirt/",
        desc: "libvirt 虚拟化缓存（只报不清）",
        risk: Risk::Caution,
        cleanup_kind: CleanupKind::Hint,
        pattern: None,
    },
    CleanTarget {
        id: "steam_shader",
        category: Category::Container,
        path: "~/.cache/steam/",
        desc: "Steam 着色器缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
    CleanTarget {
        id: "steam_appcache",
        category: Category::Container,
        path: "~/.local/share/Steam/appcache/",
        desc: "Steam 应用缓存",
        risk: Risk::OnDemand,
        cleanup_kind: CleanupKind::Direct,
        pattern: None,
    },
];

/// catalog 引用（`utils`/`page` 统一从这里取，避免散落字面量）。
pub fn catalog() -> &'static [CleanTarget] {
    CATALOG
}

/// 按 id 查找条目（命令构造与结果回填用）。
pub fn by_id(id: &str) -> Option<&'static CleanTarget> {
    CATALOG.iter().find(|t| t.id == id)
}

/// catalog 条目总数。
pub fn count() -> usize {
    CATALOG.len()
}

/// 参与**深度扫描**的通用模式串（第九类中非「只提示」的条目）。
///
/// `*.rpmnew` / `*.rpmsave` 是 `Hint`（附录：不删、记录在案），不进扫描集合。
pub fn scan_patterns() -> Vec<&'static str> {
    CATALOG
        .iter()
        .filter(|t| t.pattern.is_some() && t.cleanup_kind != CleanupKind::Hint)
        .filter_map(|t| t.pattern)
        .collect()
}

/// 单条扫描结果（9.2 产出、9.4 聚合消费、9.6 展示）。
///
/// `path` 是展开后的真实路径（模式条目 = 匹配统计的根或单个命中文件）。
#[derive(Debug, Clone)]
pub struct ScanEntry {
    /// 来源条目 id（模式命中为 `pat_*`）。
    pub id: String,
    /// 展开后的真实路径。
    pub path: String,
    /// 中文说明。
    pub desc: String,
    pub category: Category,
    pub risk: Risk,
    pub cleanup_kind: CleanupKind,
    /// 可回收字节。
    pub bytes: u64,
    /// 文件数（模式条目 = 命中数）。
    pub files: u64,
    /// D9.9 只报不清：不进勾选框、不进合计。
    pub report_only: bool,
    /// 父子去重：本体积已含子条目（D9.9 UI 标注「已含子项」）。
    pub contains_children: bool,
    /// 路径不存在（探测未命中，UI 不展示或灰显）。
    pub missing: bool,
}

impl ScanEntry {
    /// 从 catalog 条目构造（体积稍后回填）。
    pub fn from_target(target: &'static CleanTarget, path: String) -> Self {
        ScanEntry {
            id: target.id.to_string(),
            path,
            desc: target.desc.to_string(),
            category: target.category,
            risk: target.risk,
            cleanup_kind: target.cleanup_kind,
            bytes: 0,
            files: 0,
            report_only: target.cleanup_kind == CleanupKind::Hint,
            contains_children: false,
            missing: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// 9.1 验证①：条目数 ≥100（用户给定 40+ 条为最低要求，附录全量收录）。
    #[test]
    fn catalog_has_at_least_100_entries() {
        assert!(
            CATALOG.len() >= 100,
            "catalog 只有 {} 条，要求 ≥100",
            CATALOG.len()
        );
    }

    /// 9.1 验证②：id 与 path 全局唯一。
    #[test]
    fn ids_and_paths_are_unique() {
        let mut ids = HashSet::new();
        let mut paths = HashSet::new();
        for t in CATALOG {
            assert!(ids.insert(t.id), "重复 id: {}", t.id);
            assert!(paths.insert(t.path), "重复 path: {}", t.path);
        }
    }

    /// 9.1 验证③：每条说明非空且为中文；risk 三值合法（label 固定三个中文词）。
    #[test]
    fn every_desc_is_nonempty_chinese() {
        for t in CATALOG {
            assert!(!t.desc.is_empty(), "{} 说明为空", t.id);
            assert!(
                t.desc
                    .chars()
                    .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "{} 说明不含中文: {}",
                t.id,
                t.desc
            );
        }
        assert_eq!(
            Risk::ALL.map(|r| r.label()),
            ["安全", "按需", "谨慎"],
            "风险三档中文标签被改动"
        );
        // risk 三值合法：catalog 实际用到全部三档。
        for r in Risk::ALL {
            assert!(
                CATALOG.iter().any(|t| t.risk == r),
                "风险档 {} 没有任何条目",
                r.label()
            );
        }
    }

    /// 9.1 验证④：十大类分类各有条目。
    #[test]
    fn every_category_has_entries() {
        for cat in Category::ALL {
            let n = CATALOG.iter().filter(|t| t.category == cat).count();
            assert!(n > 0, "分类「{}」没有任何条目", cat.label());
        }
        assert_eq!(Category::ALL.len(), 10, "应为十大类");
    }

    /// 9.1 验证⑤：用户给定的 40+ 条路径逐条 contains（缺一条即失败）。
    #[test]
    fn user_given_paths_all_present() {
        let required = [
            // 包管理器缓存
            "/var/cache/apt/archives/",
            "/var/cache/apt/archives/partial/",
            "/var/lib/apt/lists/",
            "/var/cache/dnf/",
            "/var/cache/yum/",
            "/var/cache/pacman/pkg/",
            "/var/cache/zypp/",
            "/var/cache/apk/",
            "/var/cache/xbps/",
            "/var/cache/PackageKit/",
            "~/.local/share/flatpak/repo/",
            "/var/lib/flatpak/repo/",
            "/var/lib/snapd/cache/",
            "/var/tmp/snapd/",
            // 系统日志
            "/var/log/",
            "/var/log/journal/",
            "/var/log/apt/",
            "/var/log/wtmp",
            "/var/log/btmp",
            "/var/log/lastlog",
            "~/.xsession-errors*",
            // 临时文件
            "/tmp/",
            "/var/tmp/",
            "/dev/shm/",
            // 用户级缓存
            "~/.cache/",
            "~/.thumbnails/",
            "~/.cache/thumbnails/",
            "~/.cache/google-chrome/",
            "~/.cache/chromium/",
            "~/.local/share/Trash/files/",
            "~/.local/share/Trash/info/",
            "~/.cache/fontconfig/",
            // 旧内核
            "/boot/vmlinuz-*",
            "/lib/modules/",
            // 崩溃报告
            "/var/crash/",
            "/var/spool/abrt/",
            "/var/lib/systemd/coredump/",
            // 开发工具缓存
            "~/.gradle/caches/",
            "~/.m2/repository/",
            "~/.npm/",
            "~/.cache/pip/",
            "~/.cargo/registry/",
            "~/.cargo/git/",
            "~/.cache/go-build/",
            "~/.ccache/",
            "~/.cache/yarn/",
            "~/.cache/huggingface/",
            // 桌面与系统服务
            "/var/cache/cups/",
            "/var/spool/cups/",
            "/var/spool/mail/",
            "/var/cache/man/",
            // 通用模式
            "*.tmp",
            "*.bak",
            "core.*",
            "*~",
            "*.rpmnew",
            // 容器与虚拟化
            "/var/lib/docker/",
            "/var/cache/libvirt/",
        ];
        assert!(
            required.len() >= 40,
            "断言集本身应 ≥40 条，实为 {}",
            required.len()
        );
        for p in required {
            assert!(
                CATALOG.iter().any(|t| t.path == p),
                "用户给定路径缺失: {}",
                p
            );
        }
    }

    /// 9.1 附带：D9.5 红线目录不得出现在 catalog（9.7-② 的数据层闸门）。
    #[test]
    fn red_line_dirs_never_in_catalog() {
        for red in RED_LINE_DIRS {
            assert!(
                !CATALOG.iter().any(|t| t.path == *red),
                "红线路径混进 catalog: {}",
                red
            );
            // 也不允许以红线目录为前缀的子路径。
            let prefix = format!("{}/", red.trim_end_matches('/'));
            assert!(
                !CATALOG.iter().any(|t| t.path.starts_with(&prefix)),
                "红线目录子路径混进 catalog: {}",
                prefix
            );
        }
    }

    /// 模式条目自洽：pattern ↔ 第九类 ↔ path；Hint 模式仅 rpmnew/rpmsave。
    #[test]
    fn pattern_entries_are_consistent() {
        for t in CATALOG {
            match t.pattern {
                Some(p) => {
                    assert_eq!(
                        t.category,
                        Category::GenericPattern,
                        "{} 是模式条目但不在第九类",
                        t.id
                    );
                    assert_eq!(t.path, p, "{} path 应等于模式串", t.id);
                }
                None => assert_ne!(
                    t.category,
                    Category::GenericPattern,
                    "{} 在第九类却没有模式串",
                    t.id
                ),
            }
        }
        // 「不删」的两个模式必须是 Hint，且不出现在可扫描集合里。
        for id in ["pat_rpmnew", "pat_rpmsave"] {
            let t = by_id(id).unwrap_or_else(|| panic!("缺少 {}", id));
            assert_eq!(t.cleanup_kind, CleanupKind::Hint);
        }
        let pats = scan_patterns();
        assert!(!pats.contains(&"*.rpmnew"), "*.rpmnew 不应进扫描集合");
        assert!(!pats.contains(&"*.rpmsave"), "*.rpmsave 不应进扫描集合");
        assert!(pats.contains(&"*.tmp"), "*.tmp 应进扫描集合");
        assert!(pats.contains(&"*~"), "*~ 应进扫描集合");
        assert!(pats.contains(&"core.*"), "core.* 应进扫描集合");
    }

    /// by_id 查询：命中返回原条目，未知返回 None。
    #[test]
    fn by_id_lookup_works() {
        let t = by_id("dnf_cache").expect("dnf_cache 应存在");
        assert_eq!(t.path, "/var/cache/dnf/");
        assert_eq!(t.cleanup_kind, CleanupKind::Command);
        assert!(by_id("no_such_target").is_none());
        assert_eq!(count(), CATALOG.len());
    }

    /// D9.2 默认勾选口径：只有【安全】默认勾选。
    #[test]
    fn default_checked_only_safe() {
        assert!(Risk::Safe.default_checked());
        assert!(!Risk::OnDemand.default_checked());
        assert!(!Risk::Caution.default_checked());
        // 只报项（Hint）必须 report_only。
        for t in CATALOG
            .iter()
            .filter(|t| t.cleanup_kind == CleanupKind::Hint)
        {
            let e = ScanEntry::from_target(t, t.path.to_string());
            assert!(e.report_only, "{} 是 Hint 却非只报项", t.id);
        }
    }
}
