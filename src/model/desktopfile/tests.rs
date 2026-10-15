//! `model::desktopfile` 测试（GOAL 12.1 / 12.2 验证清单）。
//!
//! golden 全部是本机真实文件的逐字节抄录（2026-10-15 实测）。

use super::*;

// ---------------------------------------------------------------------------
// 12.1 golden 往返
// ---------------------------------------------------------------------------

/// 本机真实菜单文件 `/usr/share/applications/Alacritty.desktop`（338B）。
const GOLDEN_MENU: &str = "[Desktop Entry]\n\
Type=Application\n\
TryExec=alacritty\n\
Exec=alacritty\n\
Icon=Alacritty\n\
Terminal=false\n\
Categories=System;TerminalEmulator;\n\
\n\
Name=Alacritty\n\
GenericName=Terminal\n\
Comment=A fast, cross-platform, OpenGL terminal emulator\n\
StartupNotify=true\n\
StartupWMClass=Alacritty\n\
Actions=New;\n\
\n\
[Desktop Action New]\n\
Name=New Terminal\n\
Exec=alacritty\n";

/// 本机真实自启用户层 `~/.config/autostart/cn.loongnix.lbrowser.desktop`（369B，带 Hidden）。
const GOLDEN_AUTOSTART_USER: &str = "[Desktop Entry]\n\
Exec=/opt/apps/cn.loongnix.lbrowser/files/cn.loongnix.lbrowser --pre-startup\n\
Hidden=true\n\
Name=LBrowser Autostart\n\
Name[zh_CN]=龙芯浏览器 快速启动\n\
NoDisplay=true\n\
Type=Application\n\
X-Deepin-GenerateSource=/etc/xdg/autostart/cn.loongnix.lbrowser.desktop\n\
X-GNOME-AutoStart=true\n\
X-GNOME-Autostart-Phase=Initialization\n\
X-GNOME-Bugzilla-Bugzilla=GNOME\n\
\n";

/// 本机真实自启系统层同名 `/etc/xdg/autostart/cn.loongnix.lbrowser.desktop`（284B，无 Hidden）。
const GOLDEN_AUTOSTART_SYSTEM: &str = "[Desktop Entry]\n\
Type=Application\n\
Name=LBrowser Autostart\n\
Name[zh_CN]=龙芯浏览器 快速启动\n\
Exec=/opt/apps/cn.loongnix.lbrowser/files/cn.loongnix.lbrowser --pre-startup\n\
NoDisplay=true\n\
X-GNOME-AutoStart=true\n\
X-GNOME-Autostart-Phase=Initialization\n\
X-GNOME-Bugzilla-Bugzilla=GNOME\n";

fn roundtrip(text: &str) -> DesktopDoc {
    let doc = parse_desktop_text(text);
    let rendered = render_desktop_text(&doc);
    assert_eq!(rendered, text, "render 必须与原文逐字节一致");
    let doc2 = parse_desktop_text(&rendered);
    assert_eq!(doc, doc2, "parse→render→parse 语义等值");
    doc2
}

#[test]
fn golden_menu_roundtrip() {
    let doc = roundtrip(GOLDEN_MENU);
    assert_eq!(doc.entry_type(), Some("Application"));
    assert_eq!(doc.get("Name").as_deref(), Some("Alacritty"));
    assert_eq!(
        doc.get("Categories").as_deref(),
        Some("System;TerminalEmulator;")
    );
    // Action 组保留
    assert_eq!(
        doc.group_names(),
        vec!["Desktop Entry", "Desktop Action New"]
    );
    assert_eq!(
        doc.get_value("Desktop Action New", "Name").as_deref(),
        Some("New Terminal")
    );
}

#[test]
fn golden_autostart_user_roundtrip() {
    let doc = roundtrip(GOLDEN_AUTOSTART_USER);
    assert_eq!(doc.get("Hidden").as_deref(), Some("true"));
    assert_eq!(doc.get("NoDisplay").as_deref(), Some("true"));
    assert_eq!(
        doc.get("Name[zh_CN]").as_deref(),
        Some("龙芯浏览器 快速启动")
    );
    // 长尾键 X-Deepin-* / X-GNOME-* 原样在
    assert!(doc.has_key(ENTRY_GROUP, "X-Deepin-GenerateSource"));
    assert!(doc.has_key(ENTRY_GROUP, "X-GNOME-Bugzilla-Bugzilla"));
}

#[test]
fn golden_autostart_system_roundtrip() {
    let doc = roundtrip(GOLDEN_AUTOSTART_SYSTEM);
    assert!(doc.get("Hidden").is_none(), "系统层副本没有 Hidden 键");
    assert_eq!(
        doc.get("Name[zh_CN]").as_deref(),
        Some("龙芯浏览器 快速启动")
    );
}

#[test]
fn same_id_two_layers_compare() {
    // 摸底 3：同一 id 两层内容对比——用户层多 Hidden=true 与 X-Deepin-GenerateSource
    let user = parse_desktop_text(GOLDEN_AUTOSTART_USER);
    let sys = parse_desktop_text(GOLDEN_AUTOSTART_SYSTEM);
    assert_eq!(user.get("Name"), sys.get("Name"));
    assert_eq!(user.get("Name[zh_CN]"), sys.get("Name[zh_CN]"));
    assert_eq!(user.get("Exec"), sys.get("Exec"));
    assert_ne!(
        user.get("Hidden"),
        sys.get("Hidden"),
        "用户层用 Hidden=true 停用了系统要自启的程序（摸底 3 活样本）"
    );
}

#[test]
fn locale_lines_fidelity() {
    // 摸底 5：locale 变体键巨量（Name[locale] 2437 行）→ 往返一行不丢、顺序不变
    let text = "[Desktop Entry]\n\
Type=Application\n\
Name=Test App\n\
Name[zh_CN]=测试应用\n\
Name[fr]=Application de test\n\
Name[sr@latin]=Test aplikacija\n\
Comment=Test\n\
Comment[de]=Testkommentar\n\
Exec=test\n";
    let doc = roundtrip(text);
    let locales: Vec<&str> = doc
        .entries()
        .filter(|e| e.base_key() == "Name" && e.locale().is_some())
        .map(|e| e.locale().unwrap())
        .collect();
    assert_eq!(locales, vec!["zh_CN", "fr", "sr@latin"]);
    assert_eq!(doc.get("Comment[de]").as_deref(), Some("Testkommentar"));
    // base_key / locale 拆分
    let e = doc.entries().find(|e| e.key == "Name[sr@latin]").unwrap();
    assert_eq!(e.base_key(), "Name");
    assert_eq!(e.locale(), Some("sr@latin"));
}

#[test]
fn action_groups_roundtrip() {
    let doc = roundtrip(GOLDEN_MENU);
    let action_entries: Vec<_> = doc
        .entries_in_group("Desktop Action New")
        .map(|e| (e.key.as_str(), e.value.as_str()))
        .collect();
    assert_eq!(
        action_entries,
        vec![("Name", "New Terminal"), ("Exec", "alacritty")]
    );
}

#[test]
fn action_group_is_readonly_for_forms() {
    let doc = parse_desktop_text(GOLDEN_MENU);
    let mut d = doc.clone();
    // set_value 只允许 [Desktop Entry] 组
    assert!(!d.set_value("Desktop Action New", "Exec", "/bin/false"));
    assert_eq!(
        render_desktop_text(&d),
        GOLDEN_MENU,
        "Action 组写入必须被拒且文档不变"
    );
}

// ---------------------------------------------------------------------------
// 12.1 坏输入（不 panic 且保真）
// ---------------------------------------------------------------------------

#[test]
fn bad_input_empty_file() {
    let doc = parse_desktop_text("");
    assert!(doc.lines.is_empty());
    assert!(!doc.trailing_newline);
    assert!(!doc.bom);
    assert_eq!(render_desktop_text(&doc), "");
}

#[test]
fn bad_input_no_group_header() {
    let text = "Name=orphan\nExec=x\n";
    let doc = roundtrip(text); // 组外键值 → Raw 保真
    assert_eq!(doc.entries().count(), 0);
}

#[test]
fn bad_input_bare_key_no_equals() {
    let text = "[Desktop Entry]\nName\nExec=x\n";
    let doc = roundtrip(text);
    assert!(doc.get("Name").is_none());
    assert_eq!(doc.get("Exec").as_deref(), Some("x"));
}

#[test]
fn bad_input_bom() {
    let text = "\u{feff}[Desktop Entry]\nName=x\n";
    let doc = roundtrip(text);
    assert!(doc.bom);
    assert_eq!(doc.get("Name").as_deref(), Some("x"));
}

#[test]
fn bad_input_crlf() {
    let text = "[Desktop Entry]\r\nName=x\r\nExec=y\r\n";
    let doc = roundtrip(text);
    assert_eq!(doc.get("Name").as_deref(), Some("x"));
    assert_eq!(doc.get("Exec").as_deref(), Some("y"));
}

#[test]
fn bad_input_only_comments() {
    let text = "# 只有注释\n; 也是一种注释写法\n\n";
    let doc = roundtrip(text);
    assert_eq!(doc.entries().count(), 0);
}

#[test]
fn bad_input_no_trailing_newline() {
    let text = "[Desktop Entry]\nName=x";
    let doc = roundtrip(text);
    assert!(!doc.trailing_newline);
    assert_eq!(doc.get("Name").as_deref(), Some("x"));
}

#[test]
fn bad_input_key_leading_space() {
    // 键前空格：trim 后解析成键，raw 保留原文（往返逐字节）
    let text = "[Desktop Entry]\n  Name=x\n";
    let doc = roundtrip(text);
    assert_eq!(doc.get("Name").as_deref(), Some("x"));
    assert_eq!(
        doc.entries().next().unwrap().raw.as_deref(),
        Some("  Name=x")
    );
}

#[test]
fn semicolon_is_not_comment() {
    // desktop 规范里 ; 是列表分隔符，不是注释符
    let text = "[Desktop Entry]\n;Name=commented\n";
    let doc = roundtrip(text);
    // ;Name=... 含 = 且在组内 → 解析成键 ";Name"（保真优先），不是注释
    assert!(doc.entries().next().is_some());
}

#[test]
fn backslash_is_literal_no_continuation() {
    // desktop 规范没有续行：\ 结尾只是普通字符
    let text = "[Desktop Entry]\nName=x\\\nExec=y\n";
    let doc = roundtrip(text);
    assert_eq!(doc.get("Name").as_deref(), Some("x\\"));
    assert_eq!(doc.get("Exec").as_deref(), Some("y"));
}

// ---------------------------------------------------------------------------
// 12.1 desktop_kind 五目录判定
// ---------------------------------------------------------------------------

#[test]
fn desktop_kind_five_dirs() {
    let home = dirs::home_dir().expect("home");
    let cases: &[(PathBuf, DesktopKind)] = &[
        (home.join(".local/share/applications"), DesktopKind::Menu),
        (
            home.join(".local/share/applications/x.desktop"),
            DesktopKind::Menu,
        ),
        (
            PathBuf::from("/usr/local/share/applications"),
            DesktopKind::Menu,
        ),
        (
            PathBuf::from("/usr/share/applications/x.desktop"),
            DesktopKind::Menu,
        ),
        (home.join(".config/autostart"), DesktopKind::Autostart),
        (
            PathBuf::from("/etc/xdg/autostart/y.desktop"),
            DesktopKind::Autostart,
        ),
    ];
    for (p, want) in cases {
        assert_eq!(desktop_kind(p).as_ref(), Some(want), "{p:?}");
    }
}

#[test]
fn desktop_kind_rejects_outsiders() {
    for p in [
        "/opt/apps/x.desktop",
        "/usr/share/applications-old/x.desktop", // 前缀不匹配（/ 边界）
        "/etc/xdg/autostart2/y.desktop",
        "relative/applications/x.desktop",
        "/usr/share/applications",
    ] {
        let pp = PathBuf::from(p);
        if p == "/usr/share/applications" {
            assert_eq!(desktop_kind(&pp), Some(DesktopKind::Menu));
        } else {
            assert_eq!(desktop_kind(&pp), None, "{p}");
        }
    }
}

// ---------------------------------------------------------------------------
// 12.2 KEY_SPEC / apply_form / validate_form
// ---------------------------------------------------------------------------

#[test]
fn key_specs_shape() {
    assert!(!KEY_SPECS.is_empty());
    for s in KEY_SPECS {
        assert!(!s.key.is_empty());
        assert!(!s.label.is_empty());
        assert!(!s.hint.is_empty());
        assert!(!s.form_group.is_empty());
        if s.control == Control::Select {
            assert!(!s.options.is_empty(), "{} 的下拉必须有清单", s.key);
        }
        assert!(!s.kinds.is_empty(), "{} 必须至少适用一个模块", s.key);
    }
    // 分组归属断言（D12.4）：自启专有键只落自启页
    for key in [
        "X-GNOME-Autostart-enabled",
        "X-GNOME-Autostart-Delay",
        "X-GNOME-Autostart-Phase",
        "X-GNOME-AutoRestart",
        "X-GNOME-Autostart-Notify",
        "AutostartCondition",
        "X-KDE-autostart-phase",
        "X-KDE-autostart-after",
        "X-systemd-skip",
    ] {
        let s = spec_for(key).expect(key);
        assert_eq!(s.kinds, KIND_AUTOSTART_ONLY, "{key} 只能是自启专有");
        assert_eq!(s.form_group, "自启专有");
    }
    // D12.4 口径：①②③ 两页共有（Autostart = Menu 超集），无菜单专有键
    for key in [
        "StartupWMClass",
        "Categories",
        "MimeType",
        "Name",
        "Exec",
        "Icon",
    ] {
        let s = spec_for(key).expect(key);
        assert_eq!(s.kinds.len(), 2, "{key} 应两页都有");
    }
    // 没有「仅菜单」键：要么两页共有（len 2），要么仅自启（len 1 且是 Autostart）
    assert!(
        KEY_SPECS
            .iter()
            .all(|s| s.kinds.len() == 2 || *s.kinds == [DesktopKind::Autostart])
    );
}

#[test]
fn specs_for_kind_filtering() {
    let menu_keys: Vec<&str> = specs_for_kind(DesktopKind::Menu).map(|s| s.key).collect();
    let auto_keys: Vec<&str> = specs_for_kind(DesktopKind::Autostart)
        .map(|s| s.key)
        .collect();
    assert!(menu_keys.contains(&"Categories"));
    assert!(!menu_keys.contains(&"X-GNOME-Autostart-Delay"));
    assert!(auto_keys.contains(&"X-GNOME-Autostart-Delay"));
    assert!(auto_keys.contains(&"Name"));
    // Autostart 是 Menu 的超集（①②③共有 + ④自启专有）
    for k in &menu_keys {
        assert!(auto_keys.contains(k), "{k} 应也在自启页");
    }
}

#[test]
fn exec_field_codes_truth_table() {
    // 必过（摸底 6 白名单）
    for ok in [
        "alacritty",
        "env FOO=1 app %f",
        "app %F %u %U",
        "app %d %D %n %N %i %c %k %v %m",
        "app %%percent",
    ] {
        assert!(validate_exec_field_codes(ok).is_ok(), "{ok}");
    }
    // 必拒（摸底 6 实测 %o/%z）
    for bad in [
        "app %o %z",
        "app %z",
        "app %o",
        "app %X",
        "app %",
        "app %5",
        "app 100%", // 以孤立 % 结尾 = 字段码不完整
    ] {
        assert!(validate_exec_field_codes(bad).is_err(), "{bad}");
    }
    let e = validate_exec_field_codes("app %z").unwrap_err();
    assert!(e.contains("%z") && e.contains("非法字段码"), "{e}");
}

#[test]
fn type_four_values_and_invalid() {
    let mut doc = parse_desktop_text(GOLDEN_MENU);
    for ty in TYPE_OPTIONS {
        doc.set_value(ENTRY_GROUP, "Type", ty);
        // Application 时保持 Name/Exec 在；其余类型不强制
        assert!(validate_form(&doc, DesktopKind::Menu).is_ok(), "{ty}");
    }
    doc.set_value(ENTRY_GROUP, "Type", "Application2");
    let e = validate_form(&doc, DesktopKind::Menu).unwrap_err();
    assert!(e.contains("Type 取值非法"), "{e}");
    doc.remove_key(ENTRY_GROUP, "Type");
    let e = validate_form(&doc, DesktopKind::Menu).unwrap_err();
    assert!(e.contains("缺少必填键 Type"), "{e}");
}

#[test]
fn non_application_edit_restriction_matrix() {
    // Type=Service（本机 2 个）→ 只开放身份与描述组
    let text = "[Desktop Entry]\nType=Service\nName=Dbus Service\n";
    let doc = parse_desktop_text(text);
    // 身份与描述组 → 放行
    let f = FormValues::new().set_text("Comment", "备注");
    assert!(apply_form(&doc, &f).is_ok());
    // 执行组 → 拒
    let f = FormValues::new().set_text("Exec", "/bin/true");
    let e = apply_form(&doc, &f).unwrap_err();
    assert!(
        e.contains("非 Application") && e.contains("身份与描述"),
        "{e}"
    );
    // 分类与菜单组 → 拒
    let f = FormValues::new().set_bool("NoDisplay", true);
    let e = apply_form(&doc, &f).unwrap_err();
    assert!(e.contains("非 Application"), "{e}");
    // Application → 全放行
    let doc = parse_desktop_text(GOLDEN_MENU);
    let f = FormValues::new()
        .set_text("Exec", "alacritty --new")
        .set_bool("Terminal", true);
    assert!(apply_form(&doc, &f).is_ok());
}

#[test]
fn application_requires_name_and_exec() {
    let doc = parse_desktop_text("[Desktop Entry]\nType=Application\nName=x\n");
    let e = validate_form(&doc, DesktopKind::Menu).unwrap_err();
    assert!(e.contains("必须有 Exec"), "{e}");
    let mut doc2 = parse_desktop_text("[Desktop Entry]\nType=Application\nExec=x\n");
    let e = validate_form(&doc2, DesktopKind::Menu).unwrap_err();
    assert!(e.contains("必须有 Name"), "{e}");
    doc2.set_value(ENTRY_GROUP, "Name", "x");
    assert!(validate_form(&doc2, DesktopKind::Menu).is_ok());
    // Service 不要求 Exec
    let svc = parse_desktop_text("[Desktop Entry]\nType=Service\nName=x\n");
    assert!(validate_form(&svc, DesktopKind::Menu).is_ok());
}

#[test]
fn boolean_keys_true_false_only() {
    let mut doc = parse_desktop_text(GOLDEN_MENU);
    doc.set_value(ENTRY_GROUP, "Terminal", "TRUE");
    let e = validate_form(&doc, DesktopKind::Menu).unwrap_err();
    assert!(e.contains("Terminal") && e.contains("true/false"), "{e}");
    doc.set_value(ENTRY_GROUP, "Terminal", "1");
    assert!(validate_form(&doc, DesktopKind::Menu).is_err());
    doc.set_value(ENTRY_GROUP, "Terminal", "false");
    assert!(validate_form(&doc, DesktopKind::Menu).is_ok());
}

#[test]
fn semicolon_keys_validation() {
    // Categories：合法
    assert!(validate_semicolon_list("Categories", "System;TerminalEmulator;").is_ok());
    assert!(validate_semicolon_list("Categories", "AudioVideo").is_ok());
    // 非法字符段
    assert!(validate_semicolon_list("Categories", "Sys tem;X;").is_err());
    assert!(validate_semicolon_list("Categories", "a=b;").is_err());
    // 连续分号（空段）
    assert!(validate_semicolon_list("Categories", "a;;b;").is_err());
    // MimeType：必须「类型/子类型」
    assert!(validate_semicolon_list("MimeType", "text/plain;").is_ok());
    assert!(validate_semicolon_list("MimeType", "x-scheme-handler/https;").is_ok());
    assert!(validate_semicolon_list("MimeType", "text").is_err());
    assert!(validate_semicolon_list("MimeType", "/text").is_err());
    // Keywords：中文段合法、首尾空白拒
    assert!(validate_semicolon_list("Keywords", "编辑器;文本编辑器;").is_ok());
    assert!(validate_semicolon_list("Keywords", " editor ;").is_err());
    // OnlyShowIn：桌面环境段
    assert!(validate_semicolon_list("OnlyShowIn", "GNOME;Deepin;").is_ok());
}

#[test]
fn icon_validation() {
    assert!(validate_icon("Alacritty").is_ok());
    assert!(validate_icon("org.gnome.Terminal").is_ok());
    assert!(validate_icon("/usr/share/icons/x.png").is_ok());
    assert!(validate_icon("bad name").is_err());
    assert!(validate_icon("relative/path.png").is_err());
    assert!(validate_icon("").is_err());
}

#[test]
fn no_display_hidden_truth_table() {
    // NoDisplay=true → 「不在菜单显示」徽章语义（D12.3）
    let doc =
        parse_desktop_text("[Desktop Entry]\nType=Application\nName=x\nExec=y\nNoDisplay=true\n");
    assert_eq!(doc.get("NoDisplay").as_deref(), Some("true"));
    // Hidden=true → 「已禁用」
    let doc =
        parse_desktop_text("[Desktop Entry]\nType=Application\nName=x\nExec=y\nHidden=true\n");
    assert_eq!(doc.get("Hidden").as_deref(), Some("true"));
    // Hidden=false 且原键不存在 → 不写行（apply 语义）
    let base = parse_desktop_text(GOLDEN_MENU);
    assert!(base.get("Hidden").is_none());
    let f = FormValues::new().set_bool("Hidden", false);
    let out = apply_form(&base, &f).unwrap();
    assert!(
        out.get("Hidden").is_none(),
        "不存在的 Hidden 不该被写 false"
    );
    // Hidden=false 且原键存在 → 改为 false（不删行，回读可断言）
    let hid =
        parse_desktop_text("[Desktop Entry]\nType=Application\nName=x\nExec=y\nHidden=true\n");
    let out = apply_form(&hid, &f).unwrap();
    assert_eq!(out.get("Hidden").as_deref(), Some("false"));
}

#[test]
fn apply_form_zero_change_assertion() {
    // 只写回动过的键：未收录键 / locale 行 / Action 组零改动
    let doc = parse_desktop_text(GOLDEN_MENU);
    let f = FormValues::new().set_text("Comment", "新注释");
    let out = apply_form(&doc, &f).unwrap();
    let before: Vec<String> = doc.lines.iter().map(|l| l.to_line_string()).collect();
    let after: Vec<String> = out.lines.iter().map(|l| l.to_line_string()).collect();
    assert_eq!(before.len(), after.len(), "行数不该变（无新增行）");
    let mut diff_idx = vec![];
    for (i, (a, b)) in before.iter().zip(after.iter()).enumerate() {
        if a != b {
            diff_idx.push(i);
        }
    }
    assert_eq!(diff_idx.len(), 1, "只应一行变化：{diff_idx:?}");
    assert!(after[diff_idx[0]].starts_with("Comment="));
    // 原 doc 不变
    assert_eq!(
        doc.get("Comment").as_deref(),
        Some("A fast, cross-platform, OpenGL terminal emulator")
    );
}

#[test]
fn apply_form_semicolon_list_whole_replace() {
    let doc = parse_desktop_text(GOLDEN_MENU);
    let f = FormValues::new().set_list("Categories", vec!["AudioVideo".into(), "Player".into()]);
    let out = apply_form(&doc, &f).unwrap();
    assert_eq!(out.get("Categories").as_deref(), Some("AudioVideo;Player;"));
    // 空表 = 不动
    let f = FormValues::new().set_list("Categories", vec![]);
    let out = apply_form(&doc, &f).unwrap();
    assert_eq!(
        out.get("Categories").as_deref(),
        Some("System;TerminalEmulator;")
    );
}

#[test]
fn apply_form_empty_text_is_noop() {
    let doc = parse_desktop_text(GOLDEN_MENU);
    let f = FormValues::new().set_text("Comment", "");
    let out = apply_form(&doc, &f).unwrap();
    assert_eq!(
        out.get("Comment").as_deref(),
        Some("A fast, cross-platform, OpenGL terminal emulator"),
        "空文本 = 不动"
    );
}

#[test]
fn apply_form_localized_empty_is_noop() {
    let doc = parse_desktop_text(GOLDEN_AUTOSTART_USER);
    let f = FormValues::new()
        .set_text("Name[zh_CN]", "")
        .set_text("Comment[zh_CN]", "新中文注释");
    let out = apply_form(&doc, &f).unwrap();
    assert_eq!(
        out.get("Name[zh_CN]").as_deref(),
        Some("龙芯浏览器 快速启动")
    );
    assert_eq!(out.get("Comment[zh_CN]").as_deref(), Some("新中文注释"));
}

#[test]
fn apply_form_unknown_key_rejected() {
    let doc = parse_desktop_text(GOLDEN_MENU);
    let f = FormValues::new().set_text("X-Not-In-Spec", "v");
    let e = apply_form(&doc, &f).unwrap_err();
    assert!(e.contains("未收录"), "{e}");
}

#[test]
fn apply_form_newline_rejected() {
    let doc = parse_desktop_text(GOLDEN_MENU);
    let f = FormValues::new().set_text("Name", "a\nb");
    assert!(apply_form(&doc, &f).is_err());
    let f = FormValues::new().set_list("Categories", vec!["a\nb".into()]);
    assert!(apply_form(&doc, &f).is_err());
}

#[test]
fn apply_form_unset_is_noop() {
    let doc = parse_desktop_text(GOLDEN_MENU);
    let f = FormValues::new().unset("Name");
    let out = apply_form(&doc, &f).unwrap();
    assert_eq!(out, doc);
}

#[test]
fn set_value_creates_group_when_missing() {
    let mut doc = DesktopDoc::new();
    assert!(doc.set_value(ENTRY_GROUP, "Name", "x"));
    // 新建 doc 默认无尾换行（与解析「无尾换行」样本同语义）
    assert_eq!(render_desktop_text(&doc), "[Desktop Entry]\nName=x");
    // 已有键改第一个出现、清 raw
    let mut doc = parse_desktop_text("[Desktop Entry]\nName=x\nExec=y\n");
    doc.set_value(ENTRY_GROUP, "Name", "z");
    assert_eq!(
        render_desktop_text(&doc),
        "[Desktop Entry]\nName=z\nExec=y\n"
    );
}

#[test]
fn validate_form_full_golden_menu() {
    // 真实菜单 golden 直接过 validate
    let doc = parse_desktop_text(GOLDEN_MENU);
    assert!(validate_form(&doc, DesktopKind::Menu).is_ok());
}

#[test]
fn validate_form_full_golden_autostart() {
    let doc = parse_desktop_text(GOLDEN_AUTOSTART_USER);
    assert!(validate_form(&doc, DesktopKind::Autostart).is_ok());
    let doc = parse_desktop_text(GOLDEN_AUTOSTART_SYSTEM);
    assert!(validate_form(&doc, DesktopKind::Autostart).is_ok());
}

#[test]
fn validate_form_exec_field_code_gate() {
    let mut doc = parse_desktop_text(GOLDEN_MENU);
    doc.set_value(ENTRY_GROUP, "Exec", "alacritty %z");
    let e = validate_form(&doc, DesktopKind::Menu).unwrap_err();
    assert!(e.contains("%z") && e.contains("非法字段码"), "{e}");
}

#[test]
fn default_templates() {
    // 菜单模板：Type=Application + 空 Name/Exec → validate 必拦（必填把关）
    let t = default_desktop_text(DesktopKind::Menu);
    let doc = parse_desktop_text(&t);
    let e = validate_form(&doc, DesktopKind::Menu).unwrap_err();
    assert!(
        e.contains("必须有 Name") || e.contains("必须有 Exec"),
        "{e}"
    );
    assert!(doc.get("NoDisplay").is_none(), "菜单模板不带 NoDisplay");
    // 自启模板：带 NoDisplay=true（D12.5-④，本机 16/20 先例）
    let t = default_desktop_text(DesktopKind::Autostart);
    let doc = parse_desktop_text(&t);
    assert_eq!(doc.get("NoDisplay").as_deref(), Some("true"));
    assert!(validate_form(&doc, DesktopKind::Autostart).is_err());
    // 填齐必填后两模板都过
    let mut doc = parse_desktop_text(&default_desktop_text(DesktopKind::Menu));
    doc.set_value(ENTRY_GROUP, "Name", "测试");
    doc.set_value(ENTRY_GROUP, "Exec", "/bin/true");
    assert!(validate_form(&doc, DesktopKind::Menu).is_ok());
}

#[test]
fn split_join_semicolon_roundtrip() {
    let items = vec!["AudioVideo".to_string(), "中文".to_string()];
    assert_eq!(join_semicolon(&items), "AudioVideo;中文;");
    assert_eq!(split_semicolon("AudioVideo;中文;"), items);
    assert!(split_semicolon("").is_empty());
    assert!(join_semicolon(&[]).is_empty());
}

#[test]
fn apply_form_scope_by_original_type() {
    // D12.10 语义：限制按**原文件类型**判定——Service 只开身份与描述组，
    // 连 Type 都在执行组不可改（要转 Application 只能重建，v1 口径）
    let svc = parse_desktop_text("[Desktop Entry]\nType=Service\nName=x\n");
    let f = FormValues::new().set_text("Type", "Application");
    let e = apply_form(&svc, &f).unwrap_err();
    assert!(e.contains("非 Application"), "{e}");
    let f = FormValues::new().set_text("Exec", "/bin/true");
    assert!(apply_form(&svc, &f).is_err());
    // 身份与描述组放行
    let f = FormValues::new().set_text("Name", "y");
    assert_eq!(
        apply_form(&svc, &f).unwrap().get("Name").as_deref(),
        Some("y")
    );
    // Application 原型：同一次提交里切 Type + 改 Exec 都放行（按原类型判定）
    let doc = parse_desktop_text(GOLDEN_MENU);
    let f = FormValues::new()
        .set_text("Type", "Service")
        .set_text("Exec", "/bin/false");
    let out = apply_form(&doc, &f).unwrap();
    assert_eq!(out.get("Type").as_deref(), Some("Service"));
    assert_eq!(out.get("Exec").as_deref(), Some("/bin/false"));
}

#[test]
fn localized_controls_exactly_two_zh_cn() {
    // D12.4-①：本地化输入只提供简体中文两个（其余 locale 行只读保真）
    let localized: Vec<&str> = KEY_SPECS
        .iter()
        .filter(|s| s.control == Control::Localized)
        .map(|s| s.key)
        .collect();
    assert_eq!(localized, vec!["Name[zh_CN]", "Comment[zh_CN]"]);
}

#[test]
fn get_values_and_duplicates() {
    // 重复键保序取全部
    let doc = parse_desktop_text("[Desktop Entry]\nName=a\nName=b\n");
    assert_eq!(doc.get_values(ENTRY_GROUP, "Name"), vec!["a", "b"]);
    // set_value 改第一个出现
    let mut d = doc.clone();
    d.set_value(ENTRY_GROUP, "Name", "c");
    assert_eq!(d.get_values(ENTRY_GROUP, "Name"), vec!["c", "b"]);
}
