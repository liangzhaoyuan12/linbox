//! 请求头编辑列表（展示层 · 可复用复合控件）。
//!
//! 每个请求头占一行：左侧填名称、右侧填值，直接输入即可；
//! 可随时「添加请求头」，每行右侧的垃圾桶按钮可删除该行（至少保留一行）。
//!
//! 本控件只负责收集界面上的输入，不做任何校验与网络操作。

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;

/// 一行请求头的控件句柄。
struct HeaderRow {
    row: gtk::ListBoxRow,
    key: gtk::Entry,
    value: gtk::Entry,
}

/// 请求头列表：boxed-list 样式的可编辑列表 + 底部的「添加请求头」按钮。
pub struct HeaderList {
    root: gtk::Box,
    list: gtk::ListBox,
    rows: RefCell<Vec<HeaderRow>>,
}

impl HeaderList {
    /// 构建列表，并预置一行空输入，方便直接填写。
    pub fn new() -> Rc<Self> {
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        // 行内是输入框，不需要选中态，避免点击输入框时整行高亮
        list.set_selection_mode(gtk::SelectionMode::None);

        let add_button = gtk::Button::with_label("添加请求头");
        add_button.add_css_class("flat");
        add_button.set_halign(gtk::Align::Start);
        add_button.set_margin_top(6);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.append(&list);
        root.append(&add_button);

        let this = Rc::new(Self {
            root,
            list,
            rows: RefCell::new(Vec::new()),
        });

        // 预置一行，但不抢焦点
        this.append_row(false);

        add_button.connect_clicked({
            let this = Rc::downgrade(&this);
            move |_| {
                if let Some(this) = this.upgrade() {
                    this.append_row(true);
                }
            }
        });

        this
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// 追加一行。`focus` 为 true 时把光标落在新行的名称输入框里。
    fn append_row(self: &Rc<Self>, focus: bool) {
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.set_selectable(false);

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(12);
        content.set_margin_end(12);

        let key = gtk::Entry::new();
        key.set_placeholder_text(Some("名称，如 Content-Type"));
        key.set_hexpand(true);
        key.set_width_chars(18);

        let value = gtk::Entry::new();
        value.set_placeholder_text(Some("值，如 application/json"));
        value.set_hexpand(true);

        let remove = gtk::Button::from_icon_name("user-trash-symbolic");
        remove.add_css_class("flat");
        remove.set_valign(gtk::Align::Center);
        remove.set_tooltip_text(Some("删除该请求头"));

        content.append(&key);
        content.append(&value);
        content.append(&remove);
        row.set_child(Some(&content));
        self.list.append(&row);

        if focus {
            key.grab_focus();
        }

        // 用弱引用回调，避免「列表 → 行 → 按钮 → 闭包 → 列表」的引用循环
        remove.connect_clicked({
            let this = Rc::downgrade(self);
            let row = row.clone();
            move |_| {
                if let Some(this) = this.upgrade() {
                    this.remove_row(&row);
                }
            }
        });

        self.rows.borrow_mut().push(HeaderRow { row, key, value });
    }

    /// 删除指定行；始终至少保留一行，避免出现空白无入口的状态。
    fn remove_row(&self, row: &gtk::ListBoxRow) {
        let mut rows = self.rows.borrow_mut();
        if rows.len() <= 1 {
            return;
        }
        if let Some(pos) = rows.iter().position(|r| &r.row == row) {
            let removed = rows.remove(pos);
            self.list.remove(&removed.row);
        }
    }

    /// 收集已填写的请求头：跳过名称为空的行，键值均去除首尾空白。
    pub fn collect(&self) -> Vec<(String, String)> {
        self.rows
            .borrow()
            .iter()
            .map(|r| (r.key.text().trim().to_string(), r.value.text().trim().to_string()))
            .filter(|(key, _)| !key.is_empty())
            .collect()
    }
}
