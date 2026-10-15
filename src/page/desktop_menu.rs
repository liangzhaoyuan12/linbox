//! 应用菜单页（GOAL Phase 12 的 12.4 共享组件 + 12.5 页面 A）。
//!
//! 结构：
//! - **共享组件（`pub(crate)`，12.6 开机自启页复用）**：[`open_df_edit_dialog`] /
//!   [`open_df_new_dialog`]（结构化表单对话框，D12.4 五组 + 只读保真区）、
//!   [`df_list_row`]（D12.9 徽章列表行）、[`df_search_hit`]（子串搜索）、
//!   [`DfPageHooks`]（页面 toast/刷新回调，`fn` 指针保证可跨 `idle_add`）；
//! - **页面 A**：工具条（新建/刷新/搜索/过滤 + 状态行）+ 条目列表 + 详情区
//!   （生效层路径 + 覆盖副本只读 + 全键只读 + 操作：编辑/隐藏/物理删除/复制路径）。
//!
//! 线程模型（Phase 7/10/11 同款）：外部命令（`desktop-file-validate` /
//! `update-desktop-database` / `pkexec`）一律 `std::thread::spawn`，结果经
//! `glib::source::idle_add` + 全局 `with_inner` 回主线程；`idle_add` 闭包只搬
//! `Send` 纯数据与 `fn` 指针（页面回调经 [`DfPageHooks`] 传递，不搬 `Rc`）。
//!
//! 对话框窗口登记在 [`DF_FORM_WIN`]：后台写入成功后经 [`close_df_form`] 关闭
//! （`gtk::Window` 非 `Send`，不能进 `idle_add`，只能走 thread_local 取回）。

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use glib::clone;

use crate::model::desktopfile as dfm;
use crate::model::desktopfile::{DesktopDoc, DesktopKind};
use crate::utils::desktopfile as dfu;

// ---------------------------------------------------------------------------
// 页面回调钩子（fn 指针 = Send + 'static，可进 idle_add）
// ---------------------------------------------------------------------------

/// 由每个页面提供自己的 toast / 刷新入口（各自模块的 `with_inner`）。
#[derive(Clone, Copy)]
pub(crate) struct DfPageHooks {
    pub toast: fn(&str),
    pub refresh: fn(),
}

// ---------------------------------------------------------------------------
// 表单对话框（D12.4：结构化，不给文本编辑器）
// ---------------------------------------------------------------------------

/// 表单受控字段（保存时按控件读值）。
enum DfFieldCtl {
    /// 单行文本（含本地化输入 `Name[zh_CN]`——留空 = 不动，D12.4-①）。
    Text(adw::EntryRow),
    /// 下拉；`Vec<String>` = 实际展示的选项（下标 0 恒为「（未设置）」，
    /// 现实值不在清单时按 fstab `select_fstype` 先例动态补入）。
    Select(adw::ComboRow, Vec<String>),
    /// 开关。
    Toggle(adw::SwitchRow),
    /// 分号列表（整组替换）。
    List(Rc<RefCell<Vec<gtk::Entry>>>),
}

/// 一个打开中的表单的状态（保存按钮闭包持有 `Rc`）。
struct DfFormState {
    kind: DesktopKind,
    /// 基线文档（编辑 = 文件原文解析；新建 = 默认模板）。
    original: DesktopDoc,
    /// 编辑目标路径；`None` = 新建（目标由 id + 作用域现算）。
    edit_target: Option<PathBuf>,
    /// 新建：id 输入行（`Rc` 构造后才挂 UI，故用 RefCell）。
    id_row: RefCell<Option<adw::EntryRow>>,
    /// 新建：「安装到系统」开关（默认用户目录，D12.5-②）。
    sys_row: RefCell<Option<adw::SwitchRow>>,
    /// `(键, 控件)`，只作用于 `[Desktop Entry]` 组。
    fields: RefCell<Vec<(String, DfFieldCtl)>>,
}

thread_local! {
    /// 打开中的表单窗口（后台成功后关闭；非 Send 不能进 idle_add）。
    static DF_FORM_WIN: RefCell<Option<gtk::Window>> = const { RefCell::new(None) };
}

/// 关闭当前表单对话框（若开着）。写入成功后由 `idle_add` 调用。
pub(crate) fn close_df_form() {
    if let Some(w) = DF_FORM_WIN.with(|s| s.borrow_mut().take()) {
        w.close();
    }
}

/// 只读保真区内容（D12.4-⑤）：未收录键 + 全部 locale 行（除表单里那两个 zh_CN）
/// + `[Desktop Action *]` 组 + 注释行，逐行原样列出。
fn df_readonly_text(doc: &DesktopDoc, kind: DesktopKind) -> String {
    let editable: Vec<&str> = dfm::specs_for_kind(kind).map(|s| s.key).collect();
    let mut out: Vec<String> = Vec::new();
    let mut in_entry = false;
    for line in &doc.lines {
        match line {
            dfm::DesktopLine::Group { name, .. } => {
                in_entry = name == dfm::ENTRY_GROUP;
                if !in_entry {
                    out.push(format!("【{name}】"));
                }
            }
            dfm::DesktopLine::Entry(e) => {
                if in_entry && editable.iter().any(|k| *k == e.key) {
                    continue; // 表单里已可编辑
                }
                out.push(format!("{}={}", e.key, e.value));
            }
            dfm::DesktopLine::Raw(s) => {
                if !s.trim().is_empty() && in_entry {
                    out.push(s.clone());
                }
            }
        }
    }
    if out.is_empty() {
        "（无未收录键）".to_string()
    } else {
        out.join("\n")
    }
}

/// 收集表单：**只提交与基线不同的键**（D12.2 只写回改过的键、空值不动）。
fn df_collect(state: &DfFormState) -> dfm::FormValues {
    let mut form = dfm::FormValues::new();
    for (key, ctl) in state.fields.borrow().iter() {
        match ctl {
            DfFieldCtl::Text(row) => {
                let v = row.text().to_string();
                let old = state.original.get(key).unwrap_or_default();
                if v != old {
                    form.fields.push((key.clone(), dfm::FormValue::Text(v)));
                }
            }
            DfFieldCtl::Select(row, opts) => {
                let idx = row.selected() as usize;
                // 索引 0 是 df_build_form 固定插入的「（未设置）」占位行：
                // 选中它 = 保持「空值不动」（D12.2），绝不能把占位文案当值写下去。
                if idx == 0 {
                    continue;
                }
                let new = opts.get(idx).cloned().unwrap_or_default();
                let old = state.original.get(key).unwrap_or_default();
                if new != old {
                    form.fields.push((key.clone(), dfm::FormValue::Text(new)));
                }
            }
            DfFieldCtl::Toggle(row) => {
                let new = row.is_active();
                let old = state.original.get(key).as_deref() == Some("true");
                if new != old {
                    form.fields.push((key.clone(), dfm::FormValue::Bool(new)));
                }
            }
            DfFieldCtl::List(rows) => {
                let new: Vec<String> = rows
                    .borrow()
                    .iter()
                    .map(|e| e.text().to_string())
                    .filter(|t| !t.trim().is_empty())
                    .collect();
                let old = state
                    .original
                    .get(key)
                    .map(|v| dfm::split_semicolon(&v))
                    .unwrap_or_default();
                if new != old {
                    form.fields.push((key.clone(), dfm::FormValue::List(new)));
                }
            }
        }
    }
    form
}

/// 逐键变更摘要（D12.4 保存前确认框）：`Key: 旧 → 新`。
fn df_diff_lines(old: &DesktopDoc, new: &DesktopDoc) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let mut push = |k: &str| {
        if !keys.iter().any(|x| x == k) {
            keys.push(k.to_string());
        }
    };
    for e in old.entries_in_group(dfm::ENTRY_GROUP) {
        push(&e.key);
    }
    for e in new.entries_in_group(dfm::ENTRY_GROUP) {
        push(&e.key);
    }
    let fmt = |v: Option<String>| match v.as_deref() {
        None => "无".to_string(),
        Some("") => "（空）".to_string(),
        Some(s) if s.len() > 80 => format!("{}…", &s[..80]),
        Some(s) => s.to_string(),
    };
    let mut out = Vec::new();
    for k in keys {
        let o = old.get(&k);
        let n = new.get(&k);
        if o != n {
            out.push(format!("{k}: {} → {}", fmt(o), fmt(n)));
        }
    }
    out
}

/// 表单分组构建（D12.4）：按 `KEY_SPECS.form_group` 出组；**非 Application 原型
/// 只开放「身份与描述」组**（D12.10）；底部只读保真区。
fn df_build_form(container: &gtk::Box, state: &Rc<DfFormState>) {
    let specs: Vec<&'static dfm::KeySpec> = dfm::specs_for_kind(state.kind).collect();
    let ty = state.original.entry_type().map(|s| s.to_string());
    let restricted = ty.as_deref().is_some_and(|t| t != "Application");

    if restricted {
        let hint = gtk::Label::new(Some(&format!(
            "该文件 Type={}（非 Application），执行类键不适用，仅开放「身份与描述」（D12.10）",
            ty.unwrap_or_default()
        )));
        hint.add_css_class("warning");
        hint.set_halign(gtk::Align::Start);
        hint.set_wrap(true);
        hint.set_margin_bottom(6);
        container.append(&hint);
    }

    let mut groups: Vec<(&str, Vec<&'static dfm::KeySpec>)> = Vec::new();
    for s in specs {
        if restricted && s.form_group != "身份与描述" {
            continue;
        }
        match groups.iter_mut().find(|(g, _)| *g == s.form_group) {
            Some((_, v)) => v.push(s),
            None => groups.push((s.form_group, vec![s])),
        }
    }

    for (gname, gspecs) in groups {
        let group = adw::PreferencesGroup::new();
        group.set_title(gname);
        container.append(&group);
        for spec in gspecs {
            let cur = state.original.get(spec.key);
            match spec.control {
                dfm::Control::Text | dfm::Control::Localized => {
                    let row = adw::EntryRow::new();
                    row.set_title(spec.label);
                    row.set_tooltip_text(Some(spec.hint));
                    if let Some(v) = &cur {
                        row.set_text(v);
                    }
                    group.add(&row);
                    state
                        .fields
                        .borrow_mut()
                        .push((spec.key.to_string(), DfFieldCtl::Text(row)));
                }
                dfm::Control::Select => {
                    let mut opts: Vec<String> = vec!["（未设置）".to_string()];
                    if let Some(v) = cur.as_deref()
                        && !v.is_empty()
                        && !spec.options.contains(&v)
                    {
                        opts.push(v.to_string()); // 现实值动态补入（fstab 先例）
                    }
                    opts.extend(spec.options.iter().map(|s| s.to_string()));
                    let strs: Vec<&str> = opts.iter().map(|s| s.as_str()).collect();
                    let row = adw::ComboRow::builder()
                        .title(spec.label)
                        .subtitle(spec.hint)
                        .model(&gtk::StringList::new(&strs))
                        .build();
                    if let Some(v) = cur.as_deref()
                        && let Some(i) = opts.iter().position(|o| o == v)
                    {
                        row.set_selected(i as u32);
                    }
                    group.add(&row);
                    state
                        .fields
                        .borrow_mut()
                        .push((spec.key.to_string(), DfFieldCtl::Select(row, opts)));
                }
                dfm::Control::Toggle => {
                    let row = adw::SwitchRow::builder()
                        .title(spec.label)
                        .subtitle(spec.hint)
                        .build();
                    if cur.as_deref() == Some("true") {
                        row.set_active(true);
                    }
                    group.add(&row);
                    state
                        .fields
                        .borrow_mut()
                        .push((spec.key.to_string(), DfFieldCtl::Toggle(row)));
                }
                dfm::Control::SList => {
                    let wrap = gtk::Box::new(gtk::Orientation::Vertical, 6);
                    wrap.set_margin_top(4);
                    let rows: Rc<RefCell<Vec<gtk::Entry>>> = Rc::new(RefCell::new(Vec::new()));
                    if let Some(v) = &cur {
                        for item in dfm::split_semicolon(v) {
                            df_add_list_row(&wrap, &rows, &item);
                        }
                    }
                    let add = gtk::Button::with_label("添加一项");
                    add.add_css_class("flat");
                    let rows_a = rows.clone();
                    let wrap_a = wrap.clone();
                    add.connect_clicked(move |_| df_add_list_row(&wrap_a, &rows_a, ""));
                    wrap.append(&add);
                    let label = gtk::Label::new(Some(spec.label));
                    label.add_css_class("heading");
                    label.set_halign(gtk::Align::Start);
                    group.add(&label);
                    let hint = gtk::Label::new(Some(spec.hint));
                    hint.add_css_class("dim-label");
                    hint.add_css_class("caption");
                    hint.set_halign(gtk::Align::Start);
                    hint.set_wrap(true);
                    group.add(&hint);
                    group.add(&wrap);
                    state
                        .fields
                        .borrow_mut()
                        .push((spec.key.to_string(), DfFieldCtl::List(rows)));
                }
            }
        }
    }

    // ⑤ 只读保真区（D12.4-⑤）
    let others_group = adw::PreferencesGroup::new();
    others_group.set_title("只读保真区（未收录键 / locale 行 / Action 组）");
    others_group.set_description(Some("保存时原样写回，一行不丢、一行不改（D11.4 同款保真）"));
    container.append(&others_group);
    let others = gtk::Label::new(Some(&df_readonly_text(&state.original, state.kind)));
    others.add_css_class("dim-label");
    others.add_css_class("monospace");
    others.set_halign(gtk::Align::Start);
    others.set_wrap(true);
    others.set_selectable(true);
    others_group.add(&others);
}

/// 分号列表加一行输入（读取时忽略空行）。
fn df_add_list_row(wrap: &gtk::Box, rows: &Rc<RefCell<Vec<gtk::Entry>>>, value: &str) {
    let h = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let e = gtk::Entry::new();
    e.set_hexpand(true);
    e.set_placeholder_text(Some("一行一项，留空行会被忽略"));
    e.set_text(value);
    let del = gtk::Button::with_label("删除");
    del.add_css_class("destructive-action");
    del.add_css_class("flat");
    let h_del = h.clone();
    let e_del = e.clone();
    let rows_d = rows.clone();
    del.connect_clicked(move |_| {
        rows_d.borrow_mut().retain(|x| x.as_ptr() != e_del.as_ptr());
        if let Some(parent) = h_del.parent()
            && let Ok(b) = parent.downcast::<gtk::Box>()
        {
            b.remove(&h_del);
        }
    });
    h.append(&e);
    h.append(&del);
    rows.borrow_mut().push(e);
    wrap.append(&h);
}

/// 表单窗口 + 保存管线（编辑与新建共用；12.4 保存 → 确认框 → 后台链路）。
fn df_present_form(
    parent: &gtk::Window,
    heading: &str,
    state: Rc<DfFormState>,
    hooks: DfPageHooks,
    new_mode: bool,
) {
    let win = gtk::Window::new();
    win.set_transient_for(Some(parent));
    win.set_modal(true);
    win.set_default_size(580, 700);
    win.set_title(Some(heading));

    let header = gtk::HeaderBar::new();
    let cancel = gtk::Button::with_label("取消");
    cancel.add_css_class("flat");
    let save = gtk::Button::with_label(if new_mode { "创建" } else { "保存" });
    save.add_css_class("suggested-action");
    header.pack_start(&cancel);
    header.pack_end(&save);
    win.set_titlebar(Some(&header));

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    vbox.set_margin_top(8);
    vbox.set_margin_bottom(12);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);

    if new_mode {
        let head = adw::PreferencesGroup::new();
        head.set_title("新建");
        let id_row = adw::EntryRow::new();
        id_row.set_title("文件名（id，必须 .desktop 结尾）");
        id_row.set_text("linbox-new-entry.desktop");
        head.add(&id_row);
        let sys_row = adw::SwitchRow::builder()
            .title("安装到系统")
            .subtitle("默认写用户目录（零授权）；打开则写 /usr/local/share/applications（菜单）或 /etc/xdg/autostart（自启），保存时提权")
            .build();
        head.add(&sys_row);
        vbox.append(&head);
        *state.id_row.borrow_mut() = Some(id_row);
        *state.sys_row.borrow_mut() = Some(sys_row);
    } else if let Some(t) = &state.edit_target {
        let head = adw::PreferencesGroup::new();
        head.set_title("目标文件");
        let p = adw::ActionRow::new();
        p.set_title(t.to_string_lossy().as_ref());
        p.set_subtitle("编辑只作用于生效层（D12.2）；低优先级副本不受影响");
        head.add(&p);
        if dfu::source_of(t) != dfu::Source::User {
            let warn = gtk::Label::new(Some("编辑将写系统文件，保存时需授权（pkexec）"));
            warn.add_css_class("warning");
            warn.set_halign(gtk::Align::Start);
            warn.set_wrap(true);
            head.add(&warn);
        }
        vbox.append(&head);
    }

    df_build_form(&vbox, &state);

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_child(Some(&vbox));
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_vexpand(true);
    win.set_child(Some(&scroll));

    let win_c = win.clone();
    cancel.connect_clicked(move |_| win_c.close());

    // 保存：同步纯逻辑校验（collect/apply/validate + id/冲突）→ 确认框 → 后台写
    let st = state.clone();
    let form_win = win.clone();
    save.connect_clicked(move |_| match df_prepare(&st) {
        Err(e) => (hooks.toast)(&e),
        Ok((target, content, diff)) => {
            let id = target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut body = format!("目标：{}\n\n变更：\n", target.display());
            if diff.is_empty() {
                body.push_str("（无实际变化）");
            } else {
                body.push_str(&diff.join("\n"));
            }
            DF_FORM_WIN.with(|s| *s.borrow_mut() = Some(form_win.clone()));
            let dlg = adw::MessageDialog::new(Some(&form_win), Some("确认写入更改？"), Some(&body));
            dlg.add_response("cancel", "取消");
            dlg.add_response("save", "写入");
            dlg.set_response_appearance("save", adw::ResponseAppearance::Suggested);
            dlg.set_default_response(Some("cancel"));
            dlg.set_close_response("cancel");
            let fwin = form_win.clone();
            let kind = st.kind;
            dlg.connect_response(None, move |d, resp| {
                d.close();
                if resp != "save" {
                    DF_FORM_WIN.with(|s| *s.borrow_mut() = None);
                    let _ = &fwin;
                    return;
                }
                // Fn 闭包只能借用捕获值：这里逐个 clone 后交给 FnOnce 的 spawn
                let target_c = target.clone();
                let content_c = content.clone();
                let id_c = id.clone();
                std::thread::spawn(move || {
                    let res = df_run_write_pipeline(kind, id_c.clone(), target_c, content_c);
                    let res = RefCell::new(Some((res, id_c)));
                    glib::source::idle_add(move || {
                        if let Some((res, id)) = res.take() {
                            match res {
                                Ok((elevated, degraded)) => {
                                    let mut msg = format!("已保存 {id}");
                                    if elevated {
                                        msg.push_str("（已提权写入系统目录）");
                                    }
                                    if degraded {
                                        msg.push_str(
                                            "；desktop-file-validate 未安装，仅做了自家校验",
                                        );
                                    }
                                    if kind == DesktopKind::Autostart {
                                        msg.push_str("（下次登录生效）");
                                    }
                                    (hooks.toast)(&msg);
                                    (hooks.refresh)();
                                    close_df_form();
                                }
                                Err(e) => (hooks.toast)(&e),
                            }
                        }
                        glib::ControlFlow::Break
                    });
                });
            });
            dlg.present();
        }
    });

    win.present();
}

/// 保存前置（同步纯逻辑）：collect → apply → validate → 目标解析（新建走 id/冲突）。
/// 返回 `(目标, 内容, 变更摘要)`。
fn df_prepare(state: &DfFormState) -> Result<(PathBuf, String, Vec<String>), String> {
    let form = df_collect(state);
    let new_doc = dfm::apply_form(&state.original, &form)?;
    dfm::validate_form(&new_doc, state.kind)?;
    let content = dfm::render_desktop_text(&new_doc);
    let diff = df_diff_lines(&state.original, &new_doc);
    let target = match &state.edit_target {
        Some(t) => t.clone(),
        None => {
            let id = state
                .id_row
                .borrow()
                .as_ref()
                .map(|r| r.text().to_string())
                .unwrap_or_default();
            let scope = if state
                .sys_row
                .borrow()
                .as_ref()
                .is_some_and(|s| s.is_active())
            {
                crate::model::systemd::Scope::System
            } else {
                crate::model::systemd::Scope::User
            };
            dfu::create_desktop(state.kind, &id, scope)?
        }
    };
    Ok((target, content, diff))
}

/// 后台写入管线（外部命令全在线程内）：`desktop-file-validate` → `write_desktop`
/// →（菜单）`update-desktop-database`（失败仅记录，D12.7-④）。
/// 返回 `(elevated, degraded)`。
fn df_run_write_pipeline(
    kind: DesktopKind,
    id: String,
    target: PathBuf,
    content: String,
) -> Result<(bool, bool), String> {
    let v = dfu::run_desktop_file_validate(&id, &content)?;
    if !v.ok {
        let mut msg = String::from("desktop-file-validate 校验未通过：\n");
        for m in &v.messages {
            let lvl = match m.level {
                dfu::VLevel::Error => "error",
                dfu::VLevel::Warning => "warning",
            };
            msg.push_str(&format!("{lvl}: {}\n", m.text));
        }
        return Err(msg.trim_end().to_string());
    }
    let out = dfu::write_desktop(&target, &content)?;
    if kind == DesktopKind::Menu
        && let Some(dir) = target.parent()
    {
        // 缓存刷新失败只记录不回滚（D12.7-④ / D12.11）
        let _ = dfu::update_desktop_database(dir);
    }
    Ok((out.elevated, v.degraded))
}

/// 打开「编辑」表单（D12.4 共享入口；**编辑只作用于生效层** D12.2）。
pub(crate) fn open_df_edit_dialog(
    parent: &gtk::Window,
    item: &dfu::DesktopItem,
    hooks: DfPageHooks,
) {
    let content = match std::fs::read_to_string(&item.effective) {
        Ok(s) => s,
        Err(e) => {
            (hooks.toast)(&format!("读取 {} 失败：{e}", item.effective.display()));
            return;
        }
    };
    let original = dfm::parse_desktop_text(&content);
    let state = Rc::new(DfFormState {
        kind: item.kind,
        original,
        edit_target: Some(item.effective.clone()),
        id_row: RefCell::new(None),
        sys_row: RefCell::new(None),
        fields: RefCell::new(Vec::new()),
    });
    df_present_form(parent, &format!("编辑 {}", item.id), state, hooks, false);
}

/// 打开「新建」表单（D12.5 共享入口：默认模板 + id 行 + 可选安装到系统）。
pub(crate) fn open_df_new_dialog(parent: &gtk::Window, kind: DesktopKind, hooks: DfPageHooks) {
    let baseline = dfm::default_desktop_text(kind);
    let state = Rc::new(DfFormState {
        kind,
        original: dfm::parse_desktop_text(&baseline),
        edit_target: None,
        id_row: RefCell::new(None),
        sys_row: RefCell::new(None),
        fields: RefCell::new(Vec::new()),
    });
    df_present_form(parent, "新建 desktop 条目", state, hooks, true);
}

// ---------------------------------------------------------------------------
// 共享列表行与搜索（D12.9）
// ---------------------------------------------------------------------------

/// 小徽章标签。
pub(crate) fn df_badge(text: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("caption");
    l.add_css_class(class);
    l
}

/// 条目列表行：图标 + Name + Comment + Exec + 来源/状态徽章（D12.9）。
/// 行的 `widget_name` = id，`row-activated` 回调据此取条目。
pub(crate) fn df_list_row(item: &dfu::DesktopItem, kind: DesktopKind) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_widget_name(&item.id);

    let h = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    h.set_margin_top(6);
    h.set_margin_bottom(6);
    h.set_margin_start(8);
    h.set_margin_end(8);

    // 图标（Icon= 名称或路径；取不到用通用图标占位，D12.9）
    let icon = item
        .doc
        .as_ref()
        .and_then(|d| d.get("Icon"))
        .unwrap_or_default();
    let img = if icon.starts_with('/') && std::path::Path::new(&icon).exists() {
        gtk::Image::from_file(&icon)
    } else if icon.is_empty() {
        gtk::Image::from_icon_name("application-x-executable-symbolic")
    } else {
        gtk::Image::from_icon_name(&icon)
    };
    img.set_pixel_size(32);
    h.append(&img);

    let mid = gtk::Box::new(gtk::Orientation::Vertical, 2);
    mid.set_hexpand(true);
    let name = gtk::Label::new(Some(&item.display_name()));
    name.add_css_class("heading");
    name.set_halign(gtk::Align::Start);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    mid.append(&name);
    if let Some(c) = item.comment() {
        let sub = gtk::Label::new(Some(&c));
        sub.add_css_class("dim-label");
        sub.set_halign(gtk::Align::Start);
        sub.set_ellipsize(gtk::pango::EllipsizeMode::End);
        mid.append(&sub);
    }
    if let Some(x) = item.exec() {
        let ex = gtk::Label::new(Some(&x));
        ex.add_css_class("caption");
        ex.add_css_class("dim-label");
        ex.set_halign(gtk::Align::Start);
        ex.set_ellipsize(gtk::pango::EllipsizeMode::End);
        ex.set_tooltip_text(Some(&x));
        mid.append(&ex);
    }
    h.append(&mid);

    let badges = gtk::Box::new(gtk::Orientation::Vertical, 3);
    badges.set_valign(gtk::Align::Center);
    badges.append(&df_badge(
        dfu::source_of(&item.effective).label(),
        match dfu::source_of(&item.effective) {
            dfu::Source::User => "success",
            dfu::Source::Local => "accent",
            dfu::Source::System => "warning",
        },
    ));
    match kind {
        // 菜单页：NoDisplay/Hidden 转徽章不隐藏（D12.3）
        DesktopKind::Menu => {
            if item.no_display() {
                badges.append(&df_badge("不在菜单显示", "warning"));
            }
            if item.hidden() {
                badges.append(&df_badge("已禁用", "error"));
            }
        }
        // 自启页：显式「已启用/已停用」状态列（D12.9）
        DesktopKind::Autostart => {
            if item.hidden() {
                badges.append(&df_badge("已停用", "error"));
            } else {
                badges.append(&df_badge("已启用", "success"));
            }
        }
    }
    if item.covered() {
        // 「覆盖 N 份」= 生效层盖住了几份低优先级副本（lbrowser 两层 → 覆盖 1 份，
        // GOAL 12.6 硬口径）；总数 = N+1（含生效层自己）。
        badges.append(&df_badge(
            &format!("覆盖 {} 份", item.overridden.len()),
            "accent",
        ));
    }
    h.append(&badges);

    row.set_child(Some(&h));
    row
}

/// 子串搜索（D12.9：Name/GenericName/Comment/Exec/Categories；大小写不敏感）。
pub(crate) fn df_search_hit(item: &dfu::DesktopItem, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let q = query.trim().to_lowercase();
    let Some(doc) = &item.doc else {
        return item.id.to_lowercase().contains(&q);
    };
    ["Name", "GenericName", "Comment", "Exec", "Categories"]
        .iter()
        .filter_map(|k| doc.get(k))
        .chain(std::iter::once(item.id.clone()))
        .any(|v| v.to_lowercase().contains(&q))
}

// ---------------------------------------------------------------------------
// 页面 A：应用菜单（12.5）
// ---------------------------------------------------------------------------

thread_local! {
    /// 页面强引用（fcitx_fix 注释：必须是 Rc 不是 Weak）。
    static INNER: RefCell<Option<Rc<Inner>>> = const { RefCell::new(None) };
}

fn with_inner<F: FnOnce(&Inner)>(f: F) {
    let Some(inner) = INNER.with(|i| i.try_borrow().ok().and_then(|b| b.clone())) else {
        return;
    };
    f(&inner);
}

/// 页面钩子（`DfPageHooks` 的函数指针实现）。
fn menu_toast(msg: &str) {
    with_inner(|i| i.toast(msg));
}
fn menu_refresh() {
    with_inner(|i| i.df_load());
}

/// 页面根部件（`lib.rs` 注册用）。
pub struct DesktopMenuPage {
    root: adw::ToastOverlay,
}

impl DesktopMenuPage {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }
}

/// 菜单页过滤下拉项。
const MENU_FILTERS: &[&str] = &["全部", "Application", "Service", "已隐藏"];

struct Inner {
    toast_overlay: adw::ToastOverlay,
    search: gtk::SearchEntry,
    filter: gtk::DropDown,
    status: gtk::Label,
    list: gtk::ListBox,
    detail_empty: gtk::Label,
    detail: gtk::Box,
    selected: RefCell<Option<String>>,
    items: RefCell<Vec<dfu::DesktopItem>>,
    busy: Cell<bool>,
}

impl Inner {
    fn toast(&self, msg: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(msg));
    }

    fn parent_window(&self) -> Option<gtk::Window> {
        self.toast_overlay.root()?.downcast().ok()
    }

    /// 后台扫描（外部读盘不进 UI 线程）→ idle 重建列表。
    fn df_load(&self) {
        if self.busy.replace(true) {
            self.toast("正在扫描，请稍候");
            return;
        }
        std::thread::spawn(|| {
            let items = dfu::scan(DesktopKind::Menu);
            let items = RefCell::new(Some(items));
            glib::source::idle_add(move || {
                if let Some(items) = items.take() {
                    with_inner(|i| {
                        *i.items.borrow_mut() = items;
                        i.busy.set(false);
                        i.df_refilter();
                    });
                }
                glib::ControlFlow::Break
            });
        });
    }

    /// 按搜索 + 过滤重建列表行并更新状态行。
    fn df_refilter(&self) {
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        let q = self.search.text().to_string();
        let fidx = self.filter.selected() as usize;
        let items = self.items.borrow();
        let mut shown = 0;
        for it in items.iter() {
            if !df_search_hit(it, &q) {
                continue;
            }
            match fidx {
                1 => {
                    if it.doc.as_ref().and_then(|d| d.get("Type")).as_deref() != Some("Application")
                    {
                        continue;
                    }
                }
                2 => {
                    if it.doc.as_ref().and_then(|d| d.get("Type")).as_deref() != Some("Service") {
                        continue;
                    }
                }
                // 「已隐藏」= NoDisplay 或 Hidden 任一为真（D12.9）
                3 if !it.no_display() && !it.hidden() => {
                    continue;
                }
                _ => {}
            }
            shown += 1;
            self.list.append(&df_list_row(it, DesktopKind::Menu));
        }
        self.status
            .set_text(&format!("共 {} 个条目，显示 {shown} 个", items.len()));
        // 选中项被过滤掉 → 清详情。
        // 必须先让 borrow() 的 Ref 在本语句内结束，再 replace——
        // `if let ..= x.borrow()..` 会把借用延长进整个块，块内 borrow_mut 必 panic
        // （env_editor.rs:1370 同款教训，2026-10-15 删除闪退根因）。
        let sel_gone = self
            .selected
            .borrow()
            .clone()
            .is_some_and(|sel| !items.iter().any(|i| i.id == sel));
        if sel_gone {
            self.selected.replace(None);
        }
        if self.selected.borrow().is_none() {
            self.df_show_empty();
        }
    }

    fn df_show_empty(&self) {
        while let Some(c) = self.detail.first_child() {
            self.detail.remove(&c);
        }
        self.detail.append(&self.detail_empty);
        self.detail_empty.set_visible(true);
    }

    /// 选中条目 → 详情（生效层路径 + 覆盖副本只读 + 全键只读 + 操作）。
    fn df_show_detail(&self, id: &str) {
        let items = self.items.borrow();
        let Some(item) = items.iter().find(|i| i.id == id).cloned() else {
            return;
        };
        drop(items);
        self.selected.replace(Some(id.to_string()));
        self.detail_empty.set_visible(false);
        while let Some(c) = self.detail.first_child() {
            self.detail.remove(&c);
        }
        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 10);
        vbox.set_margin_top(6);

        // 标题
        let title = gtk::Label::new(Some(&item.display_name()));
        title.add_css_class("title-3");
        title.set_halign(gtk::Align::Start);
        title.set_wrap(true);
        vbox.append(&title);
        let sub = gtk::Label::new(Some(&item.id));
        sub.add_css_class("dim-label");
        sub.set_halign(gtk::Align::Start);
        vbox.append(&sub);

        // 系统来源黄条（D12.5：编辑将写系统文件需授权）
        let src = dfu::source_of(&item.effective);
        if src != dfu::Source::User {
            let warn = gtk::Label::new(Some("编辑将写系统文件，需授权（pkexec）"));
            warn.add_css_class("warning");
            warn.set_halign(gtk::Align::Start);
            warn.set_wrap(true);
            vbox.append(&warn);
        }

        // 状态徽章行
        let pills = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        pills.append(&df_badge(src.label(), "accent"));
        if item.no_display() {
            pills.append(&df_badge("不在菜单显示", "warning"));
        }
        if item.hidden() {
            pills.append(&df_badge("已禁用", "error"));
        }
        vbox.append(&pills);

        // 生效层路径 + 覆盖副本（D12.2 只读）
        let (pcard, pbox) = df_card();
        let path_row = adw::ActionRow::new();
        path_row.set_title("生效层路径");
        path_row.set_subtitle(&item.effective.to_string_lossy());
        pbox.append(&path_row);
        if item.covered() {
            for p in &item.overridden {
                let r = adw::ActionRow::new();
                r.set_title("被覆盖副本（低优先级，不生效）");
                r.set_subtitle(&p.to_string_lossy());
                r.add_css_class("dim-label");
                pbox.append(&r);
            }
        }
        vbox.append(&pcard);

        // 操作行
        let ops = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let edit = gtk::Button::with_label("编辑…");
        edit.add_css_class("suggested-action");
        let hide = gtk::Button::with_label(if item.hidden() {
            "显示（启用）"
        } else {
            "隐藏（写 Hidden=true）"
        });
        let del = gtk::Button::with_label("物理删除…");
        del.add_css_class("destructive-action");
        let copy = gtk::Button::with_label("复制路径");
        ops.append(&edit);
        ops.append(&hide);
        ops.append(&del);
        ops.append(&copy);
        vbox.append(&ops);

        // 编辑
        let hooks = DfPageHooks {
            toast: menu_toast,
            refresh: menu_refresh,
        };
        let item_e = item.clone();
        let overlay = self.toast_overlay.clone();
        edit.connect_clicked(move |_| {
            let Some(win) = overlay
                .root()
                .and_then(|r| r.downcast::<gtk::Window>().ok())
            else {
                return;
            };
            open_df_edit_dialog(&win, &item_e, hooks);
        });
        // 隐藏 / 显示（D12.6-① 可逆档）
        let p = item.effective.clone();
        let was_hidden = item.hidden();
        let hooks_h = DfPageHooks {
            toast: menu_toast,
            refresh: menu_refresh,
        };
        hide.connect_clicked(move |_| {
            let p = p.clone();
            std::thread::spawn(move || {
                let res = dfu::set_hidden(&p, !was_hidden);
                let res = RefCell::new(Some(res));
                glib::source::idle_add(move || {
                    if let Some(res) = res.take() {
                        match res {
                            Ok(_) => {
                                (hooks_h.toast)(if was_hidden {
                                    "已显示该条目"
                                } else {
                                    "已隐藏（写 Hidden=true，可再点「显示」恢复）"
                                });
                                (hooks_h.refresh)();
                            }
                            Err(e) => (hooks_h.toast)(&e),
                        }
                    }
                    glib::ControlFlow::Break
                });
            });
        });
        // 物理删除（D12.6-② 必弹确认框）
        let item_d = item.clone();
        let overlay = self.toast_overlay.clone();
        del.connect_clicked(move |_| {
            let Some(win) = overlay
                .root()
                .and_then(|r| r.downcast::<gtk::Window>().ok())
            else {
                return;
            };
            df_confirm_delete(
                &win,
                &item_d,
                DfPageHooks {
                    toast: menu_toast,
                    refresh: menu_refresh,
                },
                String::new(),
            );
        });
        // 复制路径
        let cp = item.effective.clone();
        let ov_txt = item
            .overridden
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        let overlay = self.toast_overlay.clone();
        copy.connect_clicked(move |_| {
            if ov_txt.is_empty() {
                overlay.clipboard().set_text(&cp.to_string_lossy());
            } else {
                overlay
                    .clipboard()
                    .set_text(&format!("{}\n{ov_txt}", cp.to_string_lossy()));
            }
        });

        // 全键只读（D12.9 查）
        let (kcard, kbox) = df_card();
        let ktitle = gtk::Label::new(Some("全键只读（生效层内容）"));
        ktitle.add_css_class("title-4");
        ktitle.set_halign(gtk::Align::Start);
        kbox.append(&ktitle);
        let body = item
            .doc
            .as_ref()
            .map(dfm::render_desktop_text)
            .unwrap_or_else(|| "（文件不可读）".to_string());
        let tv = gtk::TextView::new();
        tv.set_editable(false);
        tv.set_cursor_visible(false);
        tv.set_monospace(true);
        tv.set_wrap_mode(gtk::WrapMode::WordChar);
        tv.buffer().set_text(&body);
        let sw = gtk::ScrolledWindow::new();
        sw.set_child(Some(&tv));
        sw.set_min_content_height(160);
        sw.set_max_content_height(360);
        sw.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        kbox.append(&sw);
        vbox.append(&kcard);

        self.detail.append(&vbox);
    }
}

/// 确认物理删除（D12.6-②：列 id/Name/路径/生效层/下层副本；取消默认、确认 Destructive）。
pub(crate) fn df_confirm_delete(
    win: &gtk::Window,
    item: &dfu::DesktopItem,
    hooks: DfPageHooks,
    success_tail: String,
) {
    let mut body = format!(
        "id：{}\nName：{}\n路径：{}\n生效层：是（删除的就是生效那份）\n下层副本：{}\n\n将把该文件 mv 成 <文件名>.linbox.bak（可手工恢复），不使用 rm、不通配、不碰其它层副本。",
        item.id,
        item.display_name(),
        item.effective.display(),
        if item.overridden.is_empty() {
            "无".to_string()
        } else {
            format!("{} 份（不受影响）", item.overridden.len())
        }
    );
    if dfu::source_of(&item.effective) != dfu::Source::User {
        body.push_str("\n\n⚠ 该文件由系统目录提供，删除需授权（pkexec），软件包升级可能重装它。");
    }
    let dlg = adw::MessageDialog::new(Some(win), Some("确认物理删除该条目？"), Some(&body));
    dlg.add_response("cancel", "取消");
    dlg.add_response("del", "删除");
    dlg.set_response_appearance("del", adw::ResponseAppearance::Destructive);
    dlg.set_default_response(Some("cancel"));
    dlg.set_close_response("cancel");
    let p = item.effective.clone();
    let id = item.id.clone();
    dlg.connect_response(None, move |d, resp| {
        d.close();
        if resp != "del" {
            return;
        }
        // Fn 闭包不能 move 出捕获值：clone 后交给 FnOnce 的 spawn
        let p_c = p.clone();
        let id_c = id.clone();
        let tail = success_tail.clone();
        std::thread::spawn(move || {
            let res = dfu::delete_desktop(&p_c);
            let res = RefCell::new(Some((res, id_c, tail)));
            glib::source::idle_add(move || {
                if let Some((res, id, tail)) = res.take() {
                    match res {
                        Ok(out) => {
                            (hooks.toast)(&format!(
                                "已删除 {id}（备份在 {}）{tail}",
                                out.backup.display()
                            ));
                            (hooks.refresh)();
                        }
                        Err(e) => (hooks.toast)(&e),
                    }
                }
                glib::ControlFlow::Break
            });
        });
    });
    dlg.present();
}

/// 卡片容器（systemd `card()` 同款骨架）。
pub(crate) fn df_card() -> (gtk::Box, gtk::Box) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.add_css_class("card");
    outer.set_margin_top(6);
    outer.set_margin_bottom(6);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    outer.append(&content);
    (outer, content)
}

/// 构建应用菜单页（12.5）。
pub fn build() -> DesktopMenuPage {
    // 顶部标题
    let title = gtk::Label::new(Some("应用菜单"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    let desc = gtk::Label::new(Some(
        "管理 .desktop 启动器：~/.local/share/applications > /usr/local/share/applications > /usr/share/applications（同名前面覆盖后面）。同 id 只显示生效层；编辑/删除只作用于生效层。按属性表单修改，不提供文本编辑器。",
    ));
    desc.add_css_class("dim-label");
    desc.set_halign(gtk::Align::Start);
    desc.set_wrap(true);

    // 工具条：新建 / 刷新 / 搜索 / 过滤
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let new_btn = gtk::Button::with_label("新建…");
    new_btn.add_css_class("suggested-action");
    new_btn.set_tooltip_text(Some(
        "新建 .desktop 条目：默认落用户目录（零授权），可勾选安装到系统（需授权）",
    ));
    let refresh = gtk::Button::with_label("刷新");
    bar.append(&new_btn);
    bar.append(&refresh);

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("搜索 Name / 注释 / 命令 / 分类…"));
    search.set_hexpand(true);
    bar.append(&search);

    let filter = gtk::DropDown::from_strings(MENU_FILTERS);
    filter.set_tooltip_text(Some("按类型与隐藏状态过滤"));
    bar.append(&filter);

    let status = gtk::Label::new(Some("正在扫描…"));
    status.add_css_class("dim-label");
    status.set_halign(gtk::Align::End);
    status.set_hexpand(true);
    bar.append(&status);

    // 列表 + 详情
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    list.set_selection_mode(gtk::SelectionMode::Single);
    let list_sw = gtk::ScrolledWindow::new();
    list_sw.set_child(Some(&list));
    list_sw.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    list_sw.set_size_request(360, -1);

    let detail_empty = gtk::Label::new(Some(
        "从左侧选择一个条目查看详情、编辑或删除；也可点「新建…」创建启动器。",
    ));
    detail_empty.add_css_class("dim-label");
    detail_empty.set_wrap(true);
    detail_empty.set_halign(gtk::Align::Center);
    detail_empty.set_margin_top(40);

    let detail = gtk::Box::new(gtk::Orientation::Vertical, 0);
    detail.append(&detail_empty);
    let detail_sw = gtk::ScrolledWindow::new();
    detail_sw.set_child(Some(&detail));
    detail_sw.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&list_sw));
    paned.set_end_child(Some(&detail_sw));
    paned.set_position(400);
    paned.set_vexpand(true);

    // 根布局
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 10);
    vbox.set_margin_top(12);
    vbox.set_margin_bottom(12);
    vbox.set_margin_start(14);
    vbox.set_margin_end(14);
    vbox.append(&title);
    vbox.append(&desc);
    vbox.append(&bar);
    vbox.append(&paned);

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&vbox));
    toast_overlay.set_vexpand(true);

    let inner = Rc::new(Inner {
        toast_overlay: toast_overlay.clone(),
        search,
        filter,
        status,
        list,
        detail_empty,
        detail,
        selected: RefCell::new(None),
        items: RefCell::new(Vec::new()),
        busy: Cell::new(false),
    });

    INNER.with(|i| *i.borrow_mut() = Some(inner.clone()));

    // 接线
    inner.list.connect_row_activated(|_, row| {
        let id = row.widget_name().to_string();
        with_inner(|i| i.df_show_detail(&id));
    });
    // 三个回调只走 thread_local，无需捕获 inner
    inner
        .search
        .connect_search_changed(|_| with_inner(|i| i.df_refilter()));
    inner
        .filter
        .connect_selected_notify(|_| with_inner(|i| i.df_refilter()));
    refresh.connect_clicked(|_| with_inner(|i| i.df_load()));
    new_btn.connect_clicked(clone!(
        #[weak]
        inner,
        move |_| {
            let Some(win) = inner.parent_window() else {
                return;
            };
            open_df_new_dialog(
                &win,
                DesktopKind::Menu,
                DfPageHooks {
                    toast: menu_toast,
                    refresh: menu_refresh,
                },
            );
        }
    ));

    // 首次加载
    with_inner(|i| i.df_load());

    DesktopMenuPage {
        root: toast_overlay,
    }
}
