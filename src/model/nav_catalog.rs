//! 功能模块目录（Phase 13.1，单一真源）。
//!
//! 左侧 navigation list 的中部功能区共 19 个模块。历史代码把 page id / 图标 /
//! 中文名内联在 `src/lib.rs` 的 19 处 `add_nav_item` 调用里，而 Phase 13 要求
//! 首页与设置页也各自显示「图标 + 名称 + 描述」——三处同源才能不漂移，因此
//! 收敛成本文件的 `NAV_CATALOG`：
//!
//! - **数组顺序 = 默认导航顺序**（`utils::nav_order::normalize_order` 的默认序，
//!   金样测试锁死，顺序被改乱即红）；
//! - `page` 字段必须等于 `src/lib.rs` 里的 `PAGE_*` 常量取值（测试逐条断言）；
//! - `icon` / `label` 从改造前的 `lib.rs` 调用处**原样搬迁**，启动后侧边栏必须
//!   与改造前完全一致（13.1 门禁）；
//! - `desc` 是本 Phase 新增的一句话描述，供首页卡片与列表展示。
//!
//! 以后新增功能模块 = 本文件加一行 + `lib.rs` 的 `build_module` 加一个分支，
//! 首页与设置页自动跟着出现，不需要改三处。
//!
//! 约束：纯数据，**禁止 `use gtk` / `use adw` / `use glib`**（规划书 §3.8）。

/// 一个功能模块的展示元数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavEntry {
    /// 与 `lib.rs` 的 `PAGE_*` 常量、`stack` 里的页面名一致。
    pub page: &'static str,
    /// 主题图标名（全部取自改造前 `lib.rs` 实际使用的图标）。
    pub icon: &'static str,
    /// 侧边栏与首页卡片显示的中文名。
    pub label: &'static str,
    /// 一句话中文描述（首页卡片 / 列表用）。
    pub desc: &'static str,
}

impl NavEntry {
    /// 按 page id 查目录条目；未知 id 返回 `None`（不编造条目）。
    pub fn by_page(page: &str) -> Option<&'static NavEntry> {
        NAV_CATALOG.iter().find(|e| e.page == page)
    }
}

/// 全部功能模块，**数组顺序即默认导航顺序**（立项当日 `lib.rs` 注册序）。
pub const NAV_CATALOG: &[NavEntry] = &[
    NavEntry {
        page: "json",
        icon: "accessories-text-editor-symbolic",
        label: "JSON 解析",
        desc: "解析、格式化与校验 JSON 文本",
    },
    NavEntry {
        page: "media",
        icon: "video-x-generic-symbolic",
        label: "音视频 / 图片转换",
        desc: "音视频与图片格式互转，支持硬件加速",
    },
    NavEntry {
        page: "apikey",
        icon: "dialog-password-symbolic",
        label: "API Key 嗅探",
        desc: "抓取并校验各平台的 API Key",
    },
    NavEntry {
        page: "fcitx",
        icon: "input-keyboard-symbolic",
        label: "输入法修复",
        desc: "修复 fcitx5 / Wayland 输入法环境",
    },
    NavEntry {
        page: "pathscanner",
        icon: "system-search-symbolic",
        label: "域名/IP 路径扫描",
        desc: "扫描域名或 IP 上的常见路径",
    },
    NavEntry {
        page: "archivecracker",
        icon: "changes-prevent-symbolic",
        label: "压缩包爆破",
        desc: "爆破 ZIP / 7z / RAR 压缩包密码",
    },
    NavEntry {
        page: "portscanner",
        icon: "network-transmit-receive-symbolic",
        label: "端口扫描",
        desc: "扫描端口并识别运行的服务",
    },
    NavEntry {
        page: "download",
        icon: "folder-download-symbolic",
        label: "下载",
        desc: "HTTP(S) 多线程下载，支持断点续传",
    },
    NavEntry {
        page: "notepad",
        icon: "edit-paste-symbolic",
        label: "备忘录",
        desc: "本地备忘录，支持图片与 Git 云同步",
    },
    NavEntry {
        page: "systemd",
        icon: "system-run-symbolic",
        label: "systemd 管理",
        desc: "systemd 单元的查看、结构化编辑与增删",
    },
    NavEntry {
        page: "desktopmenu",
        icon: "view-grid-symbolic",
        label: "应用菜单",
        desc: "管理应用菜单里的 .desktop 条目",
    },
    NavEntry {
        page: "desktopautostart",
        icon: "system-run-symbolic",
        label: "开机自启",
        desc: "管理开机 / 登录自启动条目",
    },
    NavEntry {
        page: "monitor",
        icon: "power-profile-performance-symbolic",
        label: "系统监视器",
        desc: "实时查看 CPU / 内存 / 进程 / 传感器 / 磁盘 / 网络",
    },
    NavEntry {
        page: "enveditor",
        icon: "preferences-system-symbolic",
        label: "环境变量编辑",
        desc: "编辑用户环境变量（.bashrc / .profile 等）",
    },
    NavEntry {
        page: "inotify",
        icon: "preferences-desktop-symbolic",
        label: "inotify 调优",
        desc: "调整内核 inotify 监视数上限",
    },
    NavEntry {
        page: "fstab",
        icon: "drive-harddisk-symbolic",
        label: "磁盘挂载",
        desc: "读取与管理 /etc/fstab 自动挂载",
    },
    NavEntry {
        page: "cpufreq",
        icon: "power-profile-balanced-symbolic",
        label: "CPU 调频",
        desc: "切换 CPU 调速器与能效偏好，可持久化",
    },
    NavEntry {
        page: "cleaner",
        icon: "user-trash-symbolic",
        label: "垃圾清理",
        desc: "全盘扫描可回收空间并安全清理",
    },
    NavEntry {
        page: "smart",
        icon: "drive-harddisk-solidstate-symbolic",
        label: "硬盘健康",
        desc: "读取并分析硬盘 SMART 健康信息",
    },
];

/// 默认导航顺序（= `NAV_CATALOG` 的 page id 序列）。
pub fn default_order() -> Vec<String> {
    NAV_CATALOG.iter().map(|e| e.page.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 改造前 `src/lib.rs` 19 处 `add_nav_item(&nav_list, …)` 的注册顺序金样。
    /// 顺序被改乱即红（GOAL 13.1-④ / D13.3）。
    fn golden_order() -> Vec<&'static str> {
        vec![
            crate::PAGE_JSON,
            crate::PAGE_MEDIA,
            crate::PAGE_APIKEY,
            crate::PAGE_FCITX,
            crate::PAGE_PATHSCANNER,
            crate::PAGE_ARCHCRACKER,
            crate::PAGE_PORTSCANNER,
            crate::PAGE_DOWNLOAD,
            crate::PAGE_NOTEPAD,
            crate::PAGE_SYSTEMD,
            crate::PAGE_DESKTOP_MENU,
            crate::PAGE_DESKTOP_AUTOSTART,
            crate::PAGE_MONITOR,
            crate::PAGE_ENVEDITOR,
            crate::PAGE_INOTIFY,
            crate::PAGE_FSTAB,
            crate::PAGE_CPUFREQ,
            crate::PAGE_CLEANER,
            crate::PAGE_SMART,
        ]
    }

    /// ① 条目数 = 19（立项摸底实测的模块数）。
    #[test]
    fn catalog_has_exactly_19_entries() {
        assert_eq!(NAV_CATALOG.len(), 19);
    }

    /// ② page 唯一 + ④ 与 `PAGE_*` 常量取值逐条一致（顺序金样一并锁定）。
    #[test]
    fn pages_unique_and_match_page_constants_in_order() {
        let got: Vec<&str> = NAV_CATALOG.iter().map(|e| e.page).collect();
        assert_eq!(
            got,
            golden_order(),
            "catalog 顺序或取值与 PAGE_* 常量不一致"
        );
        let mut seen: Vec<&str> = Vec::new();
        for p in &got {
            assert!(!seen.contains(p), "page id 重复：{p}");
            seen.push(p);
        }
    }

    /// ② 附：page id 形态合法（小写字母数字，无空白、无路径分隔符）。
    #[test]
    fn page_ids_are_safe_tokens() {
        for e in NAV_CATALOG {
            assert!(
                !e.page.is_empty()
                    && e.page
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
                "非法 page id：{:?}",
                e.page
            );
        }
    }

    /// ③ label 非空且含中文。
    #[test]
    fn labels_nonempty_and_chinese() {
        for e in NAV_CATALOG {
            assert!(!e.label.trim().is_empty(), "空 label：{}", e.page);
            assert!(
                e.label
                    .chars()
                    .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "label 不含中文：{}",
                e.page
            );
        }
    }

    /// ③ desc 非空且含中文。
    #[test]
    fn descs_nonempty_and_chinese() {
        for e in NAV_CATALOG {
            assert!(!e.desc.trim().is_empty(), "空 desc：{}", e.page);
            assert!(
                e.desc
                    .chars()
                    .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "desc 不含中文：{}",
                e.page
            );
        }
    }

    /// ③ icon 非空且为 `-symbolic` 主题图标名（不编造图标名）。
    #[test]
    fn icons_nonempty_symbolic_names() {
        for e in NAV_CATALOG {
            assert!(
                e.icon.ends_with("-symbolic"),
                "icon 非 symbolic：{}",
                e.page
            );
        }
    }

    /// default_order() 与 catalog 顺序一致。
    #[test]
    fn default_order_matches_catalog() {
        let order = default_order();
        assert_eq!(order.len(), NAV_CATALOG.len());
        for (a, b) in order.iter().zip(NAV_CATALOG.iter()) {
            assert_eq!(a, b.page);
        }
    }

    /// by_page() 命中与落空。
    #[test]
    fn by_page_lookup_hit_and_miss() {
        let e = NavEntry::by_page(crate::PAGE_MONITOR).expect("monitor 应在目录里");
        assert_eq!(e.label, "系统监视器");
        assert!(NavEntry::by_page("nonexistent-page").is_none());
    }
}
