//! 环境变量编辑器页面（展示层 · 仅 UI）。
//!
//! 像 Windows 环境变量编辑器一样，把用户 rc 文件（.bashrc / .zshrc /
//! fish 的 config.fish）中的环境变量、别名逐条列出，以表格形式编辑，
//! 其余行（注释、函数等）按原样保留。
//!
//! 支持切换编辑的用户：当前用户 / root / 其他用户，以「通讯录卡片」标识；
//! 权限不足时自动通过 pkexec 提权（读取与写入均透明处理）。

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use adw::prelude::*;

use crate::model::env_editor::{Line, LoadResult, ShellKind, SystemUser};
use crate::utils::env_editor;

pub struct EnvEditorPage {
    root: adw::ToastOverlay,
}

impl EnvEditorPage {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }
}

thread_local! {
    /// 当前页面的强引用（见 `page::fcitx_fix` 注释：必须是 Rc 而不是 Weak）。
    static INNER: RefCell<Option<Rc<Inner>>> = const { RefCell::new(None) };
}

fn with_inner<F: FnOnce(&Inner)>(f: F) {
    let Some(inner) = INNER.with(|i| i.try_borrow().ok().and_then(|b| b.clone())) else {
        return;
    };
    f(&*inner);
}

pub fn shutdown() {
    INNER.with(|i| {
        if let Ok(mut b) = i.try_borrow_mut() {
            *b = None;
        }
    });
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn card(title: &str, subtitle: &str) -> (adw::PreferencesGroup, adw::PreferencesGroup) {
    let g = adw::PreferencesGroup::new();
    g.set_title(title);
    if !subtitle.is_empty() {
        g.set_description(Some(subtitle));
    }
    g.add_css_class("card");
    g.set_margin_top(8);
    g.set_margin_bottom(8);
    g.set_margin_start(12);
    g.set_margin_end(12);
    (g.clone(), g)
}

fn row_badge(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("accent");
    l
}

// ---------------------------------------------------------------------------
// Inner
// ---------------------------------------------------------------------------

/// 可编辑列表中的一行：环境变量行与别名行共用。
struct RowHandle {
    row: gtk::ListBoxRow,
    key: gtk::Entry,
    value: gtk::Entry,
    /// 该行在 `lines` 中的绝对下标（重建时重新计算，过滤搜索不影响删除定位）。
    line_idx: usize,
}

/// PATH 路径编辑列表中的一行（只有路径输入框）。
struct PathRow {
    row: gtk::ListBoxRow,
    path: gtk::Entry,
}

struct Inner {
    toast_overlay: adw::ToastOverlay,

    users_flow: gtk::FlowBox,
    users_note: gtk::Label,

    info_label: gtk::Label,
    perm_label: gtk::Label,
    shell_warn: gtk::Label,

    env_search: gtk::SearchEntry,
    env_list: gtk::ListBox,
    env_empty: gtk::Label,
    /// PATH 路径编辑区（fish 用列表语法，该区隐藏并提示）。
    path_editor: gtk::Box,
    path_warn: gtk::Label,
    path_list: gtk::ListBox,
    path_empty: gtk::Label,
    alias_list: gtk::ListBox,
    alias_empty: gtk::Label,
    other_text: gtk::TextView,

    save_btn: gtk::Button,
    status_label: gtk::Label,

    // ------ 状态 ------
    users: RefCell<Vec<SystemUser>>,
    /// (卡片按钮, 用户, 选中勾标)，选中勾标默认隐藏。
    cards: RefCell<Vec<(gtk::Button, SystemUser, gtk::Image)>>,
    current_user: RefCell<Option<SystemUser>>,
    shell: RefCell<ShellKind>,
    rc_path: RefCell<String>,
    /// 解析后的条目（顺序即文件顺序；控件重建的数据源）。
    lines: RefCell<Vec<Line>>,
    env_rows: RefCell<Vec<RowHandle>>,
    path_rows: RefCell<Vec<PathRow>>,
    alias_rows: RefCell<Vec<RowHandle>>,
    dirty: Cell<bool>,
    busy: Cell<bool>,
    rebuilding: Cell<bool>,
    writable_direct: Cell<bool>,
    file_exists: Cell<bool>,
}

impl Inner {
    fn toast(&self, msg: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(msg));
    }

    // ------------------------------------------------------------------
    // 列表重建
    // ------------------------------------------------------------------

    fn clear_list(list: &gtk::ListBox) {
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
    }

    fn rebuild_all(&self) {
        self.rebuild_env_rows();
        self.rebuild_path_rows();
        self.rebuild_alias_rows();
        self.rebuild_other();
        self.update_status();
    }

    fn rebuild_env_rows(&self) {
        self.rebuilding.set(true);
        Self::clear_list(&self.env_list);
        self.env_rows.borrow_mut().clear();

        let filter = self.env_search.text().trim().to_lowercase();
        let is_fish = matches!(*self.shell.borrow(), ShellKind::Fish);
        let lines = self.lines.borrow();
        for (idx, line) in lines.iter().enumerate() {
            if let Line::Env {
                key,
                value,
                exported,
                ..
            } = line
            {
                // bash/zsh：PATH 由「PATH 路径」页签专门编辑，这里不重复显示
                if !is_fish && key == "PATH" {
                    continue;
                }
                if !filter.is_empty() && !key.to_lowercase().contains(&filter) {
                    continue;
                }
                let key = key.clone();
                let value = value.clone();
                let exported = *exported;
                let handle = self.append_env_row(&key, &value, exported, idx);
                self.env_list.append(&handle.row);
                self.env_rows.borrow_mut().push(handle);
            }
        }
        drop(lines);
        self.env_empty
            .set_visible(self.env_rows.borrow().is_empty());
        self.rebuilding.set(false);
    }

    fn rebuild_alias_rows(&self) {
        self.rebuilding.set(true);
        Self::clear_list(&self.alias_list);
        self.alias_rows.borrow_mut().clear();

        let lines = self.lines.borrow();
        for (idx, line) in lines.iter().enumerate() {
            if let Line::Alias { name, command, .. } = line {
                let name = name.clone();
                let command = command.clone();
                let handle = self.append_alias_row(&name, &command, idx);
                self.alias_list.append(&handle.row);
                self.alias_rows.borrow_mut().push(handle);
            }
        }
        drop(lines);
        self.alias_empty
            .set_visible(self.alias_rows.borrow().is_empty());
        self.rebuilding.set(false);
    }

    // ------------------------------------------------------------------
    // PATH 路径编辑（bash/zsh：逐条路径，保存时合并回一行 PATH）
    // ------------------------------------------------------------------

    fn rebuild_path_rows(&self) {
        let is_fish = matches!(*self.shell.borrow(), ShellKind::Fish);
        self.path_warn.set_visible(is_fish);
        self.path_editor.set_visible(!is_fish);
        if is_fish {
            // fish 的 PATH 是空格分隔的列表语法（set -gx PATH a b c），
            // 走「环境变量」页签直接编辑即可，这里只提示。
            return;
        }

        self.rebuilding.set(true);
        Self::clear_list(&self.path_list);
        self.path_rows.borrow_mut().clear();

        // 合并文件中所有 PATH 条目（多行 PATH 时拍平为一个列表）
        let lines = self.lines.borrow();
        let mut all_paths: Vec<String> = Vec::new();
        for line in lines.iter() {
            if let Line::Env { key, value, .. } = line {
                if key == "PATH" {
                    let (ps, _) = env_editor::parse_path_value(value);
                    all_paths.extend(ps);
                }
            }
        }
        drop(lines);

        for p in all_paths {
            let handle = self.append_path_row(&p);
            self.path_list.append(&handle.row);
            self.path_rows.borrow_mut().push(handle);
        }
        self.path_empty
            .set_visible(self.path_rows.borrow().is_empty());
        self.rebuilding.set(false);
    }

    fn append_path_row(&self, path: &str) -> PathRow {
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.set_selectable(false);

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.set_margin_top(5);
        content.set_margin_bottom(5);
        content.set_margin_start(12);
        content.set_margin_end(12);

        let entry = gtk::Entry::new();
        entry.set_text(path);
        entry.set_placeholder_text(Some("路径，如 $HOME/.local/bin"));
        entry.set_hexpand(true);

        let delete = gtk::Button::from_icon_name("user-trash-symbolic");
        delete.add_css_class("flat");
        delete.set_valign(gtk::Align::Center);
        delete.set_tooltip_text(Some("删除该路径"));

        content.append(&entry);
        content.append(&delete);
        row.set_child(Some(&content));

        entry.connect_changed(move |_| with_inner(|i| i.mark_dirty()));

        let row_w = row.downgrade();
        delete.connect_clicked(move |_| {
            if let Some(r) = row_w.upgrade() {
                with_inner(|i| i.delete_path_row(&r));
            }
        });

        PathRow { row, path: entry }
    }

    fn delete_path_row(&self, row: &gtk::ListBoxRow) {
        let pos = self.path_rows.borrow().iter().position(|h| &h.row == row);
        if let Some(pos) = pos {
            let removed = self.path_rows.borrow_mut().remove(pos);
            self.path_list.remove(&removed.row);
        }
        self.path_empty
            .set_visible(self.path_rows.borrow().is_empty());
        self.mark_dirty();
    }

    fn add_path_row(&self, focus: bool) {
        let handle = self.append_path_row("");
        self.path_list.append(&handle.row);
        self.path_rows.borrow_mut().push(handle);
        self.path_empty
            .set_visible(self.path_rows.borrow().is_empty());
        if focus {
            if let Some(h) = self.path_rows.borrow().last() {
                h.path.grab_focus();
            }
        }
        self.mark_dirty();
    }

    fn rebuild_other(&self) {
        let lines = self.lines.borrow();
        let others: Vec<&str> = lines
            .iter()
            .filter_map(|l| match l {
                Line::Other(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        self.other_text.buffer().set_text(&others.join("\n"));
    }

    /// 构建一行（环境变量）。返回句柄，由调用方决定插入位置。
    fn append_env_row(
        &self,
        key: &str,
        value: &str,
        _exported: bool,
        line_idx: usize,
    ) -> RowHandle {
        let (row, content) = Self::row_container();
        let (key_entry, value_entry) = Self::row_entries(
            key,
            value,
            "变量名（如 PATH）",
            "值（如 $HOME/.local/bin:$PATH）",
        );
        Self::finish_row(&row, &content, &key_entry, &value_entry, "删除该环境变量");

        // 输入变化 → 标记未保存（闭包不捕获任何引用，避免引用循环）
        key_entry.connect_changed(move |_| with_inner(|i| i.mark_dirty()));
        value_entry.connect_changed(move |_| with_inner(|i| i.mark_dirty()));

        RowHandle {
            row,
            key: key_entry,
            value: value_entry,
            line_idx,
        }
    }

    /// 构建一行（别名）。
    fn append_alias_row(&self, name: &str, command: &str, line_idx: usize) -> RowHandle {
        let (row, content) = Self::row_container();
        let (name_entry, cmd_entry) =
            Self::row_entries(name, command, "别名（如 ll）", "命令（如 ls -alF）");
        Self::finish_row(&row, &content, &name_entry, &cmd_entry, "删除该别名");

        name_entry.connect_changed(move |_| with_inner(|i| i.mark_dirty()));
        cmd_entry.connect_changed(move |_| with_inner(|i| i.mark_dirty()));

        RowHandle {
            row,
            key: name_entry,
            value: cmd_entry,
            line_idx,
        }
    }

    fn row_container() -> (gtk::ListBoxRow, gtk::Box) {
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.set_selectable(false);
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.set_margin_top(5);
        content.set_margin_bottom(5);
        content.set_margin_start(12);
        content.set_margin_end(12);
        (row, content)
    }

    fn row_entries(a: &str, b: &str, ph_a: &str, ph_b: &str) -> (gtk::Entry, gtk::Entry) {
        let ea = gtk::Entry::new();
        ea.set_text(a);
        ea.set_placeholder_text(Some(ph_a));
        ea.set_width_chars(28);

        let eb = gtk::Entry::new();
        eb.set_text(b);
        eb.set_placeholder_text(Some(ph_b));
        eb.set_hexpand(true);
        (ea, eb)
    }

    fn finish_row(
        row: &gtk::ListBoxRow,
        content: &gtk::Box,
        ea: &gtk::Entry,
        eb: &gtk::Entry,
        del_tip: &str,
    ) {
        let delete = gtk::Button::from_icon_name("user-trash-symbolic");
        delete.add_css_class("flat");
        delete.set_valign(gtk::Align::Center);
        delete.set_tooltip_text(Some(del_tip));

        content.append(ea);
        content.append(eb);
        content.append(&delete);
        row.set_child(Some(content));

        // 删除按钮：弱引用捕获行，配合全局 with_inner，无 Rc 循环
        let row_w = row.downgrade();
        delete.connect_clicked(move |_| {
            if let Some(r) = row_w.upgrade() {
                with_inner(|i| i.delete_row_of(&r));
            }
        });
    }

    /// 通用的「删除某行」：根据行指针找到句柄，删掉对应 lines 条目后整体重建。
    /// 搜索过滤时行号会偏移，因此必须按行指针定位，而不是按显示序号。
    fn delete_row_of(&self, row: &gtk::ListBoxRow) {
        let idx = self
            .env_rows
            .borrow()
            .iter()
            .chain(self.alias_rows.borrow().iter())
            .find(|h| &h.row == row)
            .map(|h| h.line_idx);
        if let Some(idx) = idx {
            self.delete_line_at(idx);
        }
    }

    fn delete_line_at(&self, idx: usize) {
        self.lines.borrow_mut().remove(idx);
        self.rebuild_all();
        self.mark_dirty();
    }

    // ------------------------------------------------------------------
    // 添加行
    // ------------------------------------------------------------------

    fn add_line(&self, line: Line) {
        self.lines.borrow_mut().push(line);
        if !self.env_search.text().trim().is_empty() {
            // 有过滤时清空搜索，让新行可见
            self.env_search.set_text("");
        }
        self.rebuild_all();
        self.mark_dirty();
    }

    fn add_env_row(&self, focus: bool) {
        self.add_line(Line::Env {
            key: String::new(),
            value: String::new(),
            exported: true,
            raw: None,
        });
        if focus {
            if let Some(h) = self.env_rows.borrow().last() {
                h.key.grab_focus();
            }
        }
    }

    fn add_alias_row(&self, focus: bool) {
        self.add_line(Line::Alias {
            name: String::new(),
            command: String::new(),
            raw: None,
        });
        if focus {
            if let Some(h) = self.alias_rows.borrow().last() {
                h.key.grab_focus();
            }
        }
    }

    // ------------------------------------------------------------------
    // 收集 / 保存
    // ------------------------------------------------------------------

    /// 从控件收集回 `lines`：更新 Env/Alias 条目，用文本域重建 Other 块。
    ///
    /// 未改动的行保留其原始写法（`raw`），保证保存后文件不被无谓改写。
    fn collect_lines(&self) -> Result<Vec<Line>, String> {
        let mut lines = self.lines.borrow_mut();

        // 环境变量
        for h in self.env_rows.borrow().iter() {
            let key = h.key.text().trim().to_string();
            let value = h.value.text().trim().to_string();
            if key.is_empty() && value.is_empty() {
                continue; // 全空行 = 待删除
            }
            if key.is_empty() {
                return Err("环境变量名为空（有一行只填了「值」）。".into());
            }
            let (exported, raw) = match &lines[h.line_idx] {
                Line::Env {
                    key: ok,
                    value: ov,
                    exported,
                    raw,
                } if *ok == key && *ov == value => {
                    // 未修改 → 保留原始写法（含引号/转义/行尾注释）
                    (*exported, raw.clone())
                }
                Line::Env { exported, .. } => (*exported, None),
                _ => (true, None),
            };
            lines[h.line_idx] = Line::Env {
                key,
                value,
                exported,
                raw,
            };
        }
        // 别名
        for h in self.alias_rows.borrow().iter() {
            let name = h.key.text().trim().to_string();
            let command = h.value.text().trim().to_string();
            if name.is_empty() && command.is_empty() {
                continue;
            }
            if name.is_empty() {
                return Err("别名为空（有一行只填了「命令」）。".into());
            }
            let raw = match &lines[h.line_idx] {
                Line::Alias {
                    name: on,
                    command: oc,
                    raw,
                } if *on == name && *oc == command => raw.clone(),
                _ => None,
            };
            lines[h.line_idx] = Line::Alias { name, command, raw };
        }
        // PATH：把「PATH 路径」页签的逐条路径合并回一条 PATH 条目。
        // （fish 的 PATH 走环境变量页签，这里跳过——fish 页签只做提示。）
        if !matches!(*self.shell.borrow(), ShellKind::Fish) {
            // 原始 PATH 列表（所有 PATH 行拍平，剔除纯 $PATH 引用段）
            let orig_list: Vec<String> = lines
                .iter()
                .filter_map(|l| match l {
                    Line::Env { key, value, .. } if key == "PATH" => Some(value),
                    _ => None,
                })
                .flat_map(|v| env_editor::parse_path_value(v).0)
                .collect();
            let cur_list: Vec<String> = self
                .path_rows
                .borrow()
                .iter()
                .map(|h| h.path.text().trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();

            if cur_list != orig_list {
                // 用户实际改动了 PATH 列表 → 重写为一条，且末尾必接 $PATH
                let exported = lines
                    .iter()
                    .find_map(|l| match l {
                        Line::Env { key, exported, .. } if key == "PATH" => Some(*exported),
                        _ => None,
                    })
                    .unwrap_or(true);
                let first = lines
                    .iter()
                    .position(|l| matches!(l, Line::Env { key, .. } if key == "PATH"));
                lines.retain(|l| !matches!(l, Line::Env { key, .. } if key == "PATH"));
                let value = env_editor::join_path_value(&cur_list, true); // 必选追加 $PATH
                // 列表全空且原本就不存在 PATH 行 → 不产生无意义的 PATH="$PATH"
                if !(cur_list.is_empty() && lines.is_empty() && first.is_none()) {
                    let insert_at = match first {
                        // retain 之后原位置即相对顺序位置；若 None（原本无 PATH）则追加到末尾
                        Some(idx) => idx.min(lines.len()),
                        None => lines.len(),
                    };
                    lines.insert(
                        insert_at,
                        Line::Env {
                            key: "PATH".into(),
                            value,
                            exported,
                            raw: None, // 合并重写，不再保留原始写法
                        },
                    );
                }
            }
            // 未改动 → PATH 条目保留原始写法（raw），原样写回
        }
        // 其他内容：文本域逐行重建（原 Other 条目整体丢弃）
        let buf = self.other_text.buffer();
        let text = buf
            .text(&buf.start_iter(), &buf.end_iter(), false)
            .to_string();
        lines.retain(|l| !l.is_env() && !l.is_alias());
        for l in text.split('\n') {
            lines.push(Line::Other(l.to_string()));
        }

        Ok((*lines).clone())
    }

    fn g_save(&self) {
        if self.busy.get() {
            return;
        }
        let lines = match self.collect_lines() {
            Ok(l) => l,
            Err(e) => {
                self.toast(&e);
                return;
            }
        };
        let shell = self.shell.borrow().clone();
        let content = env_editor::serialize(&lines, &shell);
        let Some(user) = self.current_user.borrow().clone() else {
            self.toast("尚未选择用户");
            return;
        };
        let path = self.rc_path.borrow().clone();
        let elevated = !self.writable_direct.get() && env_editor::current_uid() != 0;

        self.busy.set(true);
        self.save_btn.set_sensitive(false);
        self.save_btn.set_label(if elevated {
            "正在提权保存…（请在弹窗中授权）"
        } else {
            "正在保存…"
        });

        std::thread::spawn(move || {
            let result = env_editor::save(&path, &content, &user, elevated);
            let result = Cell::new(Some(result));
            glib::source::idle_add(move || {
                if let Some(r) = result.take() {
                    with_inner(|i| i.finish_save(r, &path, &content));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn finish_save(&self, result: Result<(), String>, path: &str, content: &str) {
        self.busy.set(false);
        match result {
            Ok(()) => {
                self.file_exists.set(true);
                // 以刚写入的内容重新解析，保证列表与磁盘一致
                let shell = self.shell.borrow().clone();
                *self.lines.borrow_mut() = env_editor::parse(content, &shell);
                self.rebuild_all();
                self.mark_clean();
                self.toast(&format!(
                    "已保存：{path}（原内容已备份为 {path}.linbox.bak）"
                ));
            }
            Err(e) => {
                self.save_btn.set_sensitive(true);
                self.save_btn.set_label("保存修改");
                self.toast(&e);
            }
        }
    }

    // ------------------------------------------------------------------
    // 用户切换 / 加载
    // ------------------------------------------------------------------

    fn select_user(&self, user: SystemUser) {
        if self.busy.get() {
            self.toast("正在保存中，请稍候…");
            return;
        }
        *self.current_user.borrow_mut() = Some(user.clone());
        self.highlight_user_card(&user);
        self.status_label
            .set_text(&format!("正在加载 {} 的配置文件…", user.name));

        let me = env_editor::current_uid();
        let is_current = user.uid == me;
        std::thread::spawn(move || {
            let res = env_editor::load_user(&user, is_current);
            let res = Cell::new(Some(res));
            glib::source::idle_add(move || {
                if let Some(r) = res.take() {
                    with_inner(|i| i.apply_loaded_result(r));
                }
                glib::ControlFlow::Break
            });
        });
    }

    fn apply_loaded_result(&self, res: Result<LoadResult, String>) {
        match res {
            Ok(r) => self.apply_loaded(r),
            Err(e) => {
                self.toast(&e);
                self.status_label.set_text(&format!("加载失败：{e}"));
            }
        }
    }

    fn apply_loaded(&self, res: LoadResult) {
        *self.current_user.borrow_mut() = Some(res.user.clone());
        *self.shell.borrow_mut() = res.shell;
        *self.rc_path.borrow_mut() = res.path.clone();
        self.writable_direct.set(res.writable_direct);
        self.file_exists.set(res.content.is_some());

        let lines = match &res.content {
            Some(c) => {
                let shell = *self.shell.borrow();
                env_editor::parse(c, &shell)
            }
            None => Vec::new(),
        };
        *self.lines.borrow_mut() = lines;
        self.highlight_user_card(&res.user);
        self.rebuild_all();
        self.mark_clean();
        self.update_labels(&res.user, &res.path);
    }

    fn update_labels(&self, user: &SystemUser, path: &str) {
        let me = env_editor::current_uid();
        let is_current = user.uid == me;
        let shell = self.shell.borrow();

        self.info_label.set_text(&format!(
            "正在编辑：{}（{}）{}{}",
            path,
            user.name,
            if is_current { " · 当前用户" } else { "" },
            if self.file_exists.get() {
                ""
            } else {
                " · 文件不存在，保存时将新建"
            }
        ));

        // 权限提示（先判断有没有权限，没权限才提权 —— 与保存逻辑一致）
        if me == 0 {
            self.perm_label
                .set_text("当前以 root 运行，可以直接修改任意用户的配置文件。");
        } else if self.writable_direct.get() {
            self.perm_label
                .set_text("当前用户对该文件有写权限，保存时直接写入。");
        } else {
            self.perm_label.set_text(&format!(
                "当前用户对该文件没有写权限：保存时将自动通过 pkexec 提权到 root 写入，\
                 文件属主保持为 {}（会弹出授权窗口；取消授权则不保存）。",
                user.name
            ));
        }
        self.perm_label.add_css_class("dim-label");

        if matches!(*shell, ShellKind::Other) {
            self.shell_warn.set_text(&format!(
                "无法识别 {} 的登录 shell「{}」，按 bash 处理。",
                user.name, user.shell
            ));
            self.shell_warn.set_visible(true);
        } else {
            self.shell_warn.set_text("");
            self.shell_warn.set_visible(false);
        }

        self.save_btn.set_tooltip_text(Some(&format!(
            "保存并写入 {path}（写前自动备份 {path}.linbox.bak）"
        )));
    }

    fn update_status(&self) {
        let lines = self.lines.borrow();
        let mut envs = 0;
        let mut aliases = 0;
        for l in lines.iter() {
            if l.is_env() {
                envs += 1;
            } else if l.is_alias() {
                aliases += 1;
            }
        }
        let others = lines.len() - envs - aliases;
        let dirty = if self.dirty.get() {
            "（有未保存的修改）"
        } else {
            ""
        };
        self.status_label.set_text(&format!(
            "共 {} 行：{} 个环境变量 · {} 个别名 · {} 行其他内容{dirty}",
            lines.len(),
            envs,
            aliases,
            others
        ));
    }

    // ------------------------------------------------------------------
    // 用户卡片
    // ------------------------------------------------------------------

    fn rebuild_user_cards(&self) {
        Self::clear_flowbox(&self.users_flow);
        self.cards.borrow_mut().clear();

        let users = self.users.borrow().clone();
        for user in &users {
            let (btn, check) = Self::build_user_card(user);
            let u = user.clone();
            btn.connect_clicked(move |_| with_inner(|i| i.select_user(u.clone())));
            let child = gtk::FlowBoxChild::new();
            child.set_child(Some(&btn));
            self.users_flow.append(&child);
            self.cards.borrow_mut().push((btn, user.clone(), check));
        }

        // 当前用户尚未选中时，默认选中：当前用户 → root → 第一个
        if self.current_user.borrow().is_none() {
            let me = env_editor::current_uid();
            let target = users
                .iter()
                .find(|u| u.uid == me)
                .or_else(|| users.iter().find(|u| u.uid == 0))
                .or_else(|| users.first())
                .cloned()
                .unwrap_or(SystemUser {
                    name: "unknown".into(),
                    uid: 0,
                    gid: 0,
                    home: "/root".into(),
                    shell: "/bin/sh".into(),
                });
            *self.current_user.borrow_mut() = Some(target.clone());
            self.highlight_user_card(&target);
        } else {
            let cur = self.current_user.borrow().clone().unwrap();
            self.highlight_user_card(&cur);
        }
    }

    fn build_user_card(user: &SystemUser) -> (gtk::Button, gtk::Image) {
        let me = env_editor::current_uid();
        let is_current = user.uid == me;

        let btn = gtk::Button::new();
        btn.add_css_class("card");
        btn.set_halign(gtk::Align::Fill);

        let v = gtk::Box::new(gtk::Orientation::Vertical, 4);
        v.set_margin_top(10);
        v.set_margin_bottom(10);
        v.set_margin_start(14);
        v.set_margin_end(14);
        v.set_width_request(190);

        // 头像 + 用户名 + 标识
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let avatar = adw::Avatar::new(32, Some(&user.name), true);
        top.append(&avatar);
        let name = gtk::Label::new(Some(&user.name));
        name.add_css_class("title-4");
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        top.append(&name);
        if is_current {
            let badge = row_badge("(当前用户)");
            top.append(&badge);
        } else if me != 0 {
            let lock = gtk::Image::from_icon_name("dialog-password-symbolic");
            lock.set_tooltip_text(Some(&format!(
                "编辑 {} 的配置文件需要 root 权限",
                user.name
            )));
            top.append(&lock);
        }
        // 选中勾标：默认隐藏，选中该用户时显示（不依赖会改文字颜色的 CSS 类）
        let check = gtk::Image::from_icon_name("object-select-symbolic");
        check.add_css_class("accent");
        check.set_visible(false);
        check.set_opacity(0.9);
        check.set_tooltip_text(Some("正在编辑该用户"));
        top.append(&check);
        v.append(&top);

        let home = gtk::Label::new(Some(&user.home));
        home.add_css_class("dim-label");
        home.add_css_class("caption");
        home.set_xalign(0.0);
        home.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        v.append(&home);

        let shell = gtk::Label::new(Some(&user.shell));
        shell.add_css_class("dim-label");
        shell.add_css_class("caption");
        shell.set_xalign(0.0);
        shell.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        v.append(&shell);

        btn.set_child(Some(&v));
        (btn, check)
    }

    fn highlight_user_card(&self, user: &SystemUser) {
        // 不叠加会改文字颜色的 CSS 类（如 suggested-action 会让 card 按钮的
        // 文字与背景同色 → 看着像文字消失），改用勾选图标标识选中。
        for (_, u, check) in self.cards.borrow().iter() {
            check.set_visible(u.uid == user.uid);
        }
    }

    fn clear_flowbox(flow: &gtk::FlowBox) {
        while let Some(child) = flow.first_child() {
            flow.remove(&child);
        }
    }

    // ------------------------------------------------------------------
    // 其他动作
    // ------------------------------------------------------------------

    fn mark_dirty(&self) {
        if self.rebuilding.get() {
            return;
        }
        if !self.dirty.get() {
            self.dirty.set(true);
        }
        self.save_btn.set_sensitive(true);
        self.update_status();
    }

    fn mark_clean(&self) {
        self.dirty.set(false);
        self.save_btn.set_sensitive(false);
        self.save_btn.set_label("保存修改");
        self.update_status();
    }

    fn g_refresh(&self) {
        if self.busy.get() {
            return;
        }
        if self.dirty.get() {
            self.toast("有未保存的修改，刷新将丢弃");
        }
        // 先取出用户再调用（if-let scrutinee 借用陷阱，与「用户卡片」加载处同理）
        let cur = self.current_user.borrow().clone();
        if let Some(user) = cur {
            self.select_user(user);
        }
    }

    fn open_dir(&self) {
        let path = self.rc_path.borrow().clone();
        let dir = Path::new(&path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        std::thread::spawn(move || {
            let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
        });
    }
}

// ---------------------------------------------------------------------------
// 构建页面
// ---------------------------------------------------------------------------

pub fn build() -> EnvEditorPage {
    let toast_overlay = adw::ToastOverlay::new();
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_propagate_natural_height(true);
    scroller.set_propagate_natural_width(true);

    let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root_box.set_margin_top(12);
    root_box.set_margin_bottom(12);
    root_box.set_margin_start(12);
    root_box.set_margin_end(12);
    scroller.set_child(Some(&root_box));
    toast_overlay.set_child(Some(&scroller));

    // ---------- 标题 ----------
    let title = gtk::Label::new(Some("环境变量编辑器"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    root_box.append(&title);

    let subtitle = gtk::Label::new(Some(
        "像 Windows 环境变量编辑器那样，逐条编辑用户 shell 配置文件中的环境变量与别名。\
         支持 bash（.bashrc）、zsh（.zshrc）、fish（config.fish）；自动检测当前用户 shell，\
         也可切换编辑 root / 其他用户的配置（无权限时自动提权）。",
    ));
    subtitle.add_css_class("dim-label");
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_wrap(true);
    subtitle.set_margin_top(2);
    root_box.append(&subtitle);

    // ---------- 用户卡片区 ----------
    let (users_card, uc) = card(
        "选择用户",
        "点击卡片切换编辑对象；（当前用户）即本机登录用户。",
    );
    root_box.append(&users_card);

    let users_flow = gtk::FlowBox::new();
    users_flow.set_max_children_per_line(3);
    users_flow.set_selection_mode(gtk::SelectionMode::None);
    uc.add(&users_flow);
    let users_note = gtk::Label::new(Some("正在加载系统用户…"));
    users_note.add_css_class("dim-label");
    users_note.set_halign(gtk::Align::Start);
    uc.add(&users_note);

    // ---------- 文件信息 ----------
    let (info_card, ic) = card("文件信息", "");
    root_box.append(&info_card);

    let info_label = gtk::Label::new(Some("尚未选择用户。"));
    info_label.set_halign(gtk::Align::Start);
    info_label.set_wrap(true);
    info_label.set_selectable(true);
    ic.add(&info_label);

    let perm_label = gtk::Label::new(Some(""));
    perm_label.set_halign(gtk::Align::Start);
    perm_label.set_wrap(true);
    ic.add(&perm_label);

    let shell_warn = gtk::Label::new(Some(""));
    shell_warn.set_halign(gtk::Align::Start);
    shell_warn.set_wrap(true);
    shell_warn.set_visible(false);
    ic.add(&shell_warn);

    // ---------- 编辑区 ----------
    let (edit_card, ec) = card(
        "编辑条目",
        "环境变量：支持 export KEY=value 与 KEY=value 两种形式（保存时保留原形式），\
         值含空格/特殊字符时自动补引号。删除行时其内容一并从文件中移除。",
    );
    root_box.append(&edit_card);

    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(150);

    // ---- 环境变量页 ----
    let env_pane = gtk::Box::new(gtk::Orientation::Vertical, 6);
    env_pane.set_margin_top(4);
    env_pane.set_margin_bottom(4);

    let env_search = gtk::SearchEntry::new();
    env_search.set_placeholder_text(Some("搜索环境变量名…"));
    env_pane.append(&env_search);

    // 表头
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.set_margin_start(12);
    header.set_margin_end(12);
    header.set_margin_bottom(2);
    let h_key = gtk::Label::new(Some("变量名"));
    h_key.add_css_class("caption");
    h_key.add_css_class("dim-label");
    h_key.set_width_chars(28);
    h_key.set_xalign(0.0);
    header.append(&h_key);
    let h_val = gtk::Label::new(Some("值"));
    h_val.add_css_class("caption");
    h_val.add_css_class("dim-label");
    h_val.set_hexpand(true);
    h_val.set_xalign(0.0);
    header.append(&h_val);
    let h_del = gtk::Label::new(Some(""));
    h_del.set_width_chars(6);
    header.append(&h_del);
    env_pane.append(&header);

    let env_list = gtk::ListBox::new();
    env_list.add_css_class("boxed-list");
    env_list.set_selection_mode(gtk::SelectionMode::None);
    let env_scroll = gtk::ScrolledWindow::new();
    env_scroll.set_child(Some(&env_list));
    env_scroll.set_min_content_height(120);
    env_scroll.set_max_content_height(420);
    env_scroll.set_propagate_natural_height(true);
    env_pane.append(&env_scroll);

    let env_empty = gtk::Label::new(Some("（没有环境变量条目）"));
    env_empty.add_css_class("dim-label");
    env_empty.set_halign(gtk::Align::Center);
    env_empty.set_margin_top(6);
    env_empty.set_margin_bottom(6);
    env_pane.append(&env_empty);

    let add_env_btn = gtk::Button::with_label("添加环境变量");
    add_env_btn.add_css_class("flat");
    add_env_btn.set_halign(gtk::Align::Start);
    env_pane.append(&add_env_btn);

    stack.add_titled(&env_pane, Some("env"), "环境变量");

    // ---- PATH 路径页（bash/zsh：逐条路径；tab 顺序第二位，别名之前）----
    let path_pane = gtk::Box::new(gtk::Orientation::Vertical, 6);
    path_pane.set_margin_top(4);
    path_pane.set_margin_bottom(4);

    let path_note = gtk::Label::new(Some(
        "像 Windows 环境变量编辑器一样逐条编辑 PATH：每条路径占一行，可随时增删；\
         保存时合并为一行并固定以 :$PATH 结尾（保留系统原有的 PATH 目录），\
         例如 export PATH=\"路径1:路径2:$PATH\"。",
    ));
    path_note.add_css_class("dim-label");
    path_note.set_halign(gtk::Align::Start);
    path_note.set_wrap(true);
    path_pane.append(&path_note);

    let path_warn = gtk::Label::new(Some(
        "当前用户使用 fish：fish 的 PATH 是空格分隔的列表写法（set -gx PATH 值1 值2 …），\
         请在「环境变量」页签中直接编辑 PATH 条目。",
    ));
    path_warn.add_css_class("dim-label");
    path_warn.set_halign(gtk::Align::Start);
    path_warn.set_wrap(true);
    path_warn.set_visible(false);
    path_pane.append(&path_warn);

    let path_editor = gtk::Box::new(gtk::Orientation::Vertical, 6);

    // 表头
    let path_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    path_header.set_margin_start(12);
    path_header.set_margin_end(12);
    path_header.set_margin_bottom(2);
    let ph_path = gtk::Label::new(Some("路径"));
    ph_path.add_css_class("caption");
    ph_path.add_css_class("dim-label");
    ph_path.set_hexpand(true);
    ph_path.set_xalign(0.0);
    path_header.append(&ph_path);
    let ph_del = gtk::Label::new(Some(""));
    ph_del.set_width_chars(6);
    path_header.append(&ph_del);
    path_editor.append(&path_header);

    let path_list = gtk::ListBox::new();
    path_list.add_css_class("boxed-list");
    path_list.set_selection_mode(gtk::SelectionMode::None);
    let path_scroll = gtk::ScrolledWindow::new();
    path_scroll.set_child(Some(&path_list));
    path_scroll.set_min_content_height(120);
    path_scroll.set_max_content_height(360);
    path_scroll.set_propagate_natural_height(true);
    path_editor.append(&path_scroll);

    let path_empty = gtk::Label::new(Some("（文件中没有 PATH 条目，保存时将新建）"));
    path_empty.add_css_class("dim-label");
    path_empty.set_halign(gtk::Align::Center);
    path_empty.set_margin_top(6);
    path_empty.set_margin_bottom(6);
    path_editor.append(&path_empty);

    let add_path_btn = gtk::Button::with_label("添加路径");
    add_path_btn.add_css_class("flat");
    add_path_btn.set_halign(gtk::Align::Start);
    path_editor.append(&add_path_btn);

    path_pane.append(&path_editor);
    stack.add_titled(&path_pane, Some("path"), "PATH");

    // ---- 别名页 ----
    let alias_pane = gtk::Box::new(gtk::Orientation::Vertical, 6);
    alias_pane.set_margin_top(4);
    alias_pane.set_margin_bottom(4);

    let alias_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    alias_header.set_margin_start(12);
    alias_header.set_margin_end(12);
    alias_header.set_margin_bottom(2);
    let ah_name = gtk::Label::new(Some("别名"));
    ah_name.add_css_class("caption");
    ah_name.add_css_class("dim-label");
    ah_name.set_width_chars(28);
    ah_name.set_xalign(0.0);
    alias_header.append(&ah_name);
    let ah_cmd = gtk::Label::new(Some("命令"));
    ah_cmd.add_css_class("caption");
    ah_cmd.add_css_class("dim-label");
    ah_cmd.set_hexpand(true);
    ah_cmd.set_xalign(0.0);
    alias_header.append(&ah_cmd);
    let ah_del = gtk::Label::new(Some(""));
    ah_del.set_width_chars(6);
    alias_header.append(&ah_del);
    alias_pane.append(&alias_header);

    let alias_list = gtk::ListBox::new();
    alias_list.add_css_class("boxed-list");
    alias_list.set_selection_mode(gtk::SelectionMode::None);
    let alias_scroll = gtk::ScrolledWindow::new();
    alias_scroll.set_child(Some(&alias_list));
    alias_scroll.set_min_content_height(120);
    alias_scroll.set_max_content_height(420);
    alias_scroll.set_propagate_natural_height(true);
    alias_pane.append(&alias_scroll);

    let alias_empty = gtk::Label::new(Some("（没有别名条目）"));
    alias_empty.add_css_class("dim-label");
    alias_empty.set_halign(gtk::Align::Center);
    alias_empty.set_margin_top(6);
    alias_empty.set_margin_bottom(6);
    alias_pane.append(&alias_empty);

    let add_alias_btn = gtk::Button::with_label("添加别名");
    add_alias_btn.add_css_class("flat");
    add_alias_btn.set_halign(gtk::Align::Start);
    alias_pane.append(&add_alias_btn);

    stack.add_titled(&alias_pane, Some("alias"), "别名");

    // ---- 其他内容页 ----
    let other_pane = gtk::Box::new(gtk::Orientation::Vertical, 6);
    other_pane.set_margin_top(4);
    other_pane.set_margin_bottom(4);

    let other_note = gtk::Label::new(Some(
        "以下为文件中的其他行（注释、条件判断、函数、空行等），保存时按原样写回，\
         位置统一放在环境变量与别名之后。可直接在此编辑。",
    ));
    other_note.add_css_class("dim-label");
    other_note.set_halign(gtk::Align::Start);
    other_note.set_wrap(true);
    other_pane.append(&other_note);

    let other_text = gtk::TextView::new();
    other_text.set_monospace(true);
    other_text.set_wrap_mode(gtk::WrapMode::WordChar);
    other_text.set_left_margin(8);
    other_text.set_right_margin(8);
    other_text.set_top_margin(6);
    other_text.set_bottom_margin(6);
    other_text.set_size_request(-1, 160);
    let other_scroll = gtk::ScrolledWindow::new();
    other_scroll.set_child(Some(&other_text));
    other_scroll.set_min_content_height(140);
    other_scroll.set_max_content_height(420);
    other_pane.append(&other_scroll);

    let other_hint = gtk::Label::new(Some(
        "提示：rc 文件在 shell 启动时按顺序执行；若 PATH 等变量在 .bashrc 中多次赋值，\
         后者会覆盖前者。",
    ));
    other_hint.add_css_class("dim-label");
    other_hint.set_halign(gtk::Align::Start);
    other_hint.set_wrap(true);
    other_pane.append(&other_hint);

    stack.add_titled(&other_pane, Some("other"), "其他内容");

    let switcher = gtk::StackSwitcher::new();
    switcher.set_stack(Some(&stack));
    switcher.set_halign(gtk::Align::Start);
    ec.add(&switcher);
    ec.add(&stack);

    // ---------- 操作区 ----------
    let (action_card, ac) = card("操作", "");
    root_box.append(&action_card);

    let status_label = gtk::Label::new(Some("尚未加载。"));
    status_label.set_halign(gtk::Align::Start);
    status_label.set_wrap(true);
    status_label.set_selectable(true);
    ac.add(&status_label);

    let btn_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    btn_row.set_margin_top(6);

    let save_btn = gtk::Button::with_label("保存修改");
    save_btn.add_css_class("suggested-action");
    save_btn.set_sensitive(false);
    btn_row.append(&save_btn);

    let refresh_btn = gtk::Button::with_label("重新加载");
    btn_row.append(&refresh_btn);

    let open_dir_btn = gtk::Button::with_label("打开所在目录");
    btn_row.append(&open_dir_btn);

    ac.add(&btn_row);

    // ---------- 组装内部状态 ----------
    let inner = Rc::new(Inner {
        toast_overlay: toast_overlay.clone(),
        users_flow,
        users_note,
        info_label,
        perm_label,
        shell_warn,
        env_search: env_search.clone(),
        env_list,
        env_empty,
        path_editor,
        path_warn,
        path_list,
        path_empty,
        alias_list,
        alias_empty,
        other_text,
        save_btn: save_btn.clone(),
        status_label,
        users: RefCell::new(Vec::new()),
        cards: RefCell::new(Vec::new()),
        current_user: RefCell::new(None),
        shell: RefCell::new(ShellKind::Bash),
        rc_path: RefCell::new(String::new()),
        lines: RefCell::new(Vec::new()),
        env_rows: RefCell::new(Vec::new()),
        path_rows: RefCell::new(Vec::new()),
        alias_rows: RefCell::new(Vec::new()),
        dirty: Cell::new(false),
        busy: Cell::new(false),
        rebuilding: Cell::new(false),
        writable_direct: Cell::new(true),
        file_exists: Cell::new(false),
    });

    // 信号：所有闭包都不捕获 Rc（避免循环引用），统一走全局 with_inner。

    env_search.connect_search_changed(move |_| with_inner(|i| i.rebuild_env_rows()));

    let e = add_env_btn.connect_clicked(move |_| with_inner(|i| i.add_env_row(true)));
    let _ = e;
    let ap = add_path_btn.connect_clicked(move |_| with_inner(|i| i.add_path_row(true)));
    let _ = ap;
    let a = add_alias_btn.connect_clicked(move |_| with_inner(|i| i.add_alias_row(true)));
    let _ = a;

    save_btn.connect_clicked(move |_| with_inner(|i| i.g_save()));
    refresh_btn.connect_clicked(move |_| with_inner(|i| i.g_refresh()));
    open_dir_btn.connect_clicked(move |_| with_inner(|i| i.open_dir()));

    INNER.with(|i| *i.borrow_mut() = Some(Rc::clone(&inner)));

    // 初次加载：后台枚举用户 → 建卡片 → 自动选中当前用户
    std::thread::spawn(move || {
        let users = env_editor::list_users();
        let users = std::cell::Cell::new(Some(users));
        glib::source::idle_add(move || {
            if let Some(users) = users.take() {
                with_inner(|i| {
                    *i.users.borrow_mut() = users.clone();
                    i.users_note
                        .set_text(&format!("共 {} 个可编辑用户", users.len()));
                    i.rebuild_user_cards();
                    // 注意：不能写 `if let Some(u) = i.current_user.borrow().clone()`
                    // —— if-let 的 scrutinee 临时值（RefCell 借用）会存活到整个
                    // 语句结束，导致 select_user 里的 borrow_mut 二次借用 panic。
                    // 先取出值（借用随 let 语句结束释放），再判断、调用。
                    let cur = i.current_user.borrow().clone();
                    if let Some(u) = cur {
                        i.select_user(u);
                    }
                });
            }
            glib::ControlFlow::Break
        });
    });

    EnvEditorPage {
        root: toast_overlay,
    }
}
