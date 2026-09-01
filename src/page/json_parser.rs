//! JSON 解析页面（展示层 · 仅 UI）。
//!
//! 功能：
//! - 两种来源：直接粘贴 JSON 原文，或通过 URL 链接拉取。
//!   URL 方式支持 GET / POST，请求头以列表逐条填写，POST 时还可编辑请求体。
//! - 解析后以树形结构（键 / 值 / 类型）美观展示。
//! - 支持「复制所选」单个字段，或「复制全部」整段格式化文本。
//!
//! 本页面不实现任何解析/网络逻辑，统一调用
//! `crate::utils::json`（解析/格式化）与 `crate::utils::http`（请求）的纯函数。

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use glib::clone;
use serde_json::Value;

use crate::utils::http;
use crate::utils::json as json_util;
use crate::widgets::header_list::HeaderList;

pub struct JsonParserPage {
    root: adw::ToastOverlay,
}

/// 请求方法下拉选项，顺序与 `utils::http::Method` 对应。
const METHODS: &[&str] = &["GET", "POST"];
/// 下拉中 POST 所在的位置（用于判断是否展开请求体）。
const METHOD_POST: u32 = 1;

/// 页面内部持有的控件句柄。用 `Rc` 包裹，便于在异步回调中廉价克隆引用。
struct Inner {
    input_stack: gtk::Stack,
    text_buffer: gtk::TextBuffer,
    url_row: adw::EntryRow,
    method_row: adw::ComboRow,
    header_list: Rc<HeaderList>,
    body_buffer: gtk::TextBuffer,
    parse_button: gtk::Button,
    spinner: gtk::Spinner,
    tree_view: gtk::TreeView,
    tree_store: gtk::TreeStore,
    copy_selected_btn: gtk::Button,
    copy_all_btn: gtk::Button,
    status_label: gtk::Label,
    toast_overlay: adw::ToastOverlay,
    pretty: RefCell<String>,
}

impl JsonParserPage {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }
}

/// 取出 TextBuffer 中的全部文本。
fn buffer_text(buffer: &gtk::TextBuffer) -> String {
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string()
}

/// 构造一个带 padding 的卡片容器，返回 (外层卡片, 内层内容盒)。
fn card() -> (gtk::Box, gtk::Box) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.add_css_class("card");
    outer.set_margin_top(12);
    outer.set_margin_bottom(12);
    outer.set_margin_start(12);
    outer.set_margin_end(12);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    outer.append(&content);
    (outer, content)
}

/// 将解析后的 JSON 值递归填入 TreeStore。
fn populate_tree(store: &gtk::TreeStore, value: &Value) {
    store.clear();
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                add_node(store, None, k, v);
            }
        }
        Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                add_node(store, None, &i.to_string(), v);
            }
        }
        other => add_node(store, None, "(root)", other),
    }
}

fn add_node(store: &gtk::TreeStore, parent: Option<&gtk::TreeIter>, key: &str, value: &Value) {
    match value {
        Value::Object(map) => {
            let iter = store.append(parent);
            store.set(
                &iter,
                &[
                    (0, &key.to_string()),
                    (1, &format!("{} 项", map.len())),
                    (2, &"object".to_string()),
                ],
            );
            for (k, v) in map {
                add_node(store, Some(&iter), k, v);
            }
        }
        Value::Array(arr) => {
            let iter = store.append(parent);
            store.set(
                &iter,
                &[
                    (0, &key.to_string()),
                    (1, &format!("{} 项", arr.len())),
                    (2, &"array".to_string()),
                ],
            );
            for (i, v) in arr.iter().enumerate() {
                add_node(store, Some(&iter), &i.to_string(), v);
            }
        }
        Value::String(s) => leaf(store, parent, key, s, "string"),
        Value::Number(n) => leaf(store, parent, key, &n.to_string(), "number"),
        Value::Bool(b) => leaf(store, parent, key, &b.to_string(), "boolean"),
        Value::Null => leaf(store, parent, key, "null", "null"),
    }
}

fn leaf(store: &gtk::TreeStore, parent: Option<&gtk::TreeIter>, key: &str, value: &str, ty: &str) {
    let iter = store.append(parent);
    store.set(
        &iter,
        &[
            (0, &key.to_string()),
            (1, &value.to_string()),
            (2, &ty.to_string()),
        ],
    );
}

pub fn build() -> JsonParserPage {
    // ---------- 根容器（ToastOverlay 用于解析错误提示） ----------
    let toast_overlay = adw::ToastOverlay::new();
    // 页面内容较高，外层套一个滚动容器：空间足够时按自然高度铺开，
    // 窗口变矮时允许滚动，避免底部控件被裁剪成"看不到"。
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_propagate_natural_height(true);
    scroller.set_propagate_natural_width(true);

    let root_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root_box.set_vexpand(true);
    root_box.set_margin_top(12);
    root_box.set_margin_bottom(12);
    root_box.set_margin_start(12);
    root_box.set_margin_end(12);
    scroller.set_child(Some(&root_box));
    toast_overlay.set_child(Some(&scroller));

    // ---------- 页面标题 ----------
    let title = gtk::Label::new(Some("JSON 解析"));
    title.add_css_class("title-1");
    title.set_halign(gtk::Align::Start);
    root_box.append(&title);

    let subtitle = gtk::Label::new(Some("粘贴 JSON 文本或填入 URL 链接，解析后复制单个字段或整段结果。"));
    subtitle.add_css_class("dim-label");
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_wrap(true);
    root_box.append(&subtitle);

    // ---------- 输入卡片 ----------
    let (input_card, input_content) = card();
    root_box.append(&input_card);

    // 输入方式切换（粘贴 / URL）
    let input_stack = gtk::Stack::new();
    input_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    input_stack.set_transition_duration(150);
    input_stack.set_margin_bottom(12);
    // 两个页面高度需求不同（URL 页还可以展开请求头/请求体），
    // 因此关闭纵向等高，让各自按内容取高，并用 interpolate-size 做平滑过渡。
    input_stack.set_vhomogeneous(false);
    input_stack.set_interpolate_size(true);

    // 方式一：粘贴文本
    let text_view = gtk::TextView::new();
    text_view.set_monospace(true);
    text_view.set_wrap_mode(gtk::WrapMode::WordChar);
    text_view.set_accepts_tab(true);
    let text_buffer = text_view.buffer().clone();
    let text_scroll = gtk::ScrolledWindow::new();
    text_scroll.set_child(Some(&text_view));
    text_scroll.set_min_content_height(160);
    text_scroll.set_vexpand(true);
    input_stack.add_titled(&text_scroll, Some("text"), "粘贴文本");

    // 方式二：URL 链接（GET / POST + 自定义请求头 + POST 请求体）
    let url_box = gtk::Box::new(gtk::Orientation::Vertical, 12);

    // 请求行：URL 与请求方法
    let request_group = adw::PreferencesGroup::new();
    let url_row = adw::EntryRow::builder().title("URL").build();
    url_row.set_input_purpose(gtk::InputPurpose::Url);
    request_group.add(&url_row);

    let method_row = adw::ComboRow::builder()
        .model(&gtk::StringList::new(METHODS))
        .selected(0)
        .build();
    method_row.set_title("请求方法");
    request_group.add(&method_row);
    url_box.append(&request_group);

    // 请求头：一行一条，名称与值直接输入，可增删行
    let headers_title = gtk::Label::new(Some("请求头"));
    headers_title.add_css_class("title-4");
    headers_title.set_halign(gtk::Align::Start);
    url_box.append(&headers_title);

    let header_list = HeaderList::new();
    url_box.append(header_list.widget());

    // 请求体：仅 POST 时展开（Revealer 下滑 200ms，见 UI 规范 §6.2）
    let body_revealer = gtk::Revealer::new();
    body_revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    body_revealer.set_transition_duration(200);
    body_revealer.set_reveal_child(false);

    let body_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let body_title = gtk::Label::new(Some("请求体"));
    body_title.add_css_class("title-4");
    body_title.set_halign(gtk::Align::Start);
    body_box.append(&body_title);

    let body_hint = gtk::Label::new(Some(
        "POST 请求体，例如 JSON 文本；未指定 Content-Type 时默认按 application/json 发送。",
    ));
    body_hint.add_css_class("dim-label");
    body_hint.set_halign(gtk::Align::Start);
    body_hint.set_wrap(true);
    body_box.append(&body_hint);

    let body_view = gtk::TextView::new();
    body_view.set_monospace(true);
    body_view.set_wrap_mode(gtk::WrapMode::WordChar);
    body_view.set_accepts_tab(true);
    let body_buffer = body_view.buffer().clone();
    let body_scroll = gtk::ScrolledWindow::new();
    body_scroll.set_child(Some(&body_view));
    body_scroll.set_min_content_height(120);
    body_box.append(&body_scroll);

    body_revealer.set_child(Some(&body_box));
    url_box.append(&body_revealer);

    // 切换到 POST 时展开请求体编辑区，切回 GET 时收起
    method_row.connect_selected_notify(clone!(#[weak] body_revealer, move |row| {
        body_revealer.set_reveal_child(row.selected() == METHOD_POST);
    }));

    let url_hint = gtk::Label::new(Some(
        "仅支持 http:// 与 https:// 链接；GET 直接拉取文本，POST 可携带自定义请求体。",
    ));
    url_hint.add_css_class("dim-label");
    url_hint.set_halign(gtk::Align::Start);
    url_hint.set_wrap(true);
    url_box.append(&url_hint);
    input_stack.add_titled(&url_box, Some("url"), "URL 链接");

    input_content.append(&input_stack);

    // 顶部操作栏：切换器 + 解析按钮
    let action_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let switcher = gtk::StackSwitcher::new();
    switcher.set_stack(Some(&input_stack));
    switcher.set_halign(gtk::Align::Start);
    action_bar.append(&switcher);

    let spacer = gtk::Label::new(None);
    spacer.set_hexpand(true);
    action_bar.append(&spacer);

    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);
    spinner.set_margin_end(8);
    action_bar.append(&spinner);

    let parse_button = gtk::Button::with_label("解析");
    parse_button.add_css_class("suggested-action");
    parse_button.set_halign(gtk::Align::End);
    action_bar.append(&parse_button);
    input_content.append(&action_bar);

    // ---------- 输出卡片 ----------
    let (output_card, output_content) = card();
    root_box.append(&output_card);

    let out_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let out_title = gtk::Label::new(Some("解析结果"));
    out_title.add_css_class("title-4");
    out_title.set_halign(gtk::Align::Start);
    out_header.append(&out_title);

    let out_spacer = gtk::Label::new(None);
    out_spacer.set_hexpand(true);
    out_header.append(&out_spacer);

    let copy_selected_btn = gtk::Button::with_label("复制所选");
    copy_selected_btn.set_sensitive(false);
    copy_selected_btn.set_tooltip_text(Some("复制选中的单个字段值"));
    out_header.append(&copy_selected_btn);

    let copy_all_btn = gtk::Button::with_label("复制全部");
    copy_all_btn.set_sensitive(false);
    copy_all_btn.add_css_class("suggested-action");
    copy_all_btn.set_tooltip_text(Some("复制整段格式化后的 JSON"));
    out_header.append(&copy_all_btn);
    output_content.append(&out_header);

    // 树形结果视图
    let tree_store =
        gtk::TreeStore::new(&[glib::Type::STRING, glib::Type::STRING, glib::Type::STRING]);
    let tree_view = gtk::TreeView::with_model(&tree_store);
    tree_view.set_headers_visible(true);
    tree_view.set_enable_tree_lines(true);
    tree_view.set_vexpand(true);
    tree_view.set_grid_lines(gtk::TreeViewGridLines::None);

    fn add_column(tv: &gtk::TreeView, title: &str, col: i32, expand: bool) {
        let renderer = gtk::CellRendererText::new();
        let column = gtk::TreeViewColumn::new();
        column.set_title(title);
        column.pack_start(&renderer, true);
        column.add_attribute(&renderer, "text", col);
        column.set_expand(expand);
        column.set_resizable(true);
        tv.append_column(&column);
    }
    add_column(&tree_view, "键 / 路径", 0, true);
    add_column(&tree_view, "值", 1, true);
    add_column(&tree_view, "类型", 2, false);

    let tree_scroll = gtk::ScrolledWindow::new();
    tree_scroll.set_child(Some(&tree_view));
    tree_scroll.set_min_content_height(240);
    tree_scroll.set_vexpand(true);
    output_content.append(&tree_scroll);

    let status_label = gtk::Label::new(Some("尚未解析"));
    status_label.add_css_class("dim-label");
    status_label.set_halign(gtk::Align::Start);
    status_label.set_margin_top(4);
    output_content.append(&status_label);

    // ---------- 组装内部状态 ----------
    let inner = Rc::new(Inner {
        input_stack,
        text_buffer,
        url_row,
        method_row,
        header_list,
        body_buffer,
        parse_button,
        spinner,
        tree_view,
        tree_store,
        copy_selected_btn,
        copy_all_btn,
        status_label,
        toast_overlay: toast_overlay.clone(),
        pretty: RefCell::new(String::new()),
    });

    // 选中行变化时启用/禁用「复制所选」
    {
        let inner = Rc::clone(&inner);
        inner
            .tree_view
            .selection()
            .connect_changed(move |sel| {
                inner.copy_selected_btn.set_sensitive(sel.selected().is_some());
            });
    }

    // 「复制所选」：复制选中行的字段值（容器行则复制键名）
    {
        let inner = Rc::clone(&inner);
        inner.copy_selected_btn.connect_clicked(clone!(#[strong] inner, move |_| {
            if let Some((model, iter)) = inner.tree_view.selection().selected() {
                let value = model.get_value(&iter, 1).get::<String>().unwrap_or_default();
                let text = if value.is_empty() {
                    model.get_value(&iter, 0).get::<String>().unwrap_or_default()
                } else {
                    value
                };
                inner.toast_overlay.clipboard().set_text(&text);
                inner.toast(&format!("已复制：{text}"));
            }
        }));
    }

    // 「复制全部」：复制整段格式化 JSON
    {
        let inner = Rc::clone(&inner);
        inner.copy_all_btn.connect_clicked(clone!(#[weak] inner, move |_| {
            let text = inner.pretty.borrow().clone();
            inner.toast_overlay.clipboard().set_text(&text);
            inner.toast("已复制全部 JSON");
        }));
    }

    // 「解析」按钮
    {
        let inner = Rc::clone(&inner);
        inner.parse_button.connect_clicked(clone!(#[strong] inner, move |_| {
            let mode = inner.input_stack.visible_child_name();
            let is_url = mode.as_deref() == Some("url");

            let raw = if is_url {
                inner.url_row.text().to_string()
            } else {
                buffer_text(&inner.text_buffer)
            };

            if raw.trim().is_empty() {
                inner.toast("输入为空，请先粘贴文本或填写 URL");
                return;
            }

            // 进入加载态
            inner.spinner.set_visible(true);
            inner.spinner.start();
            inner.parse_button.set_sensitive(false);

            if is_url {
                // URL 模式：组装请求描述 → 后台线程发送 → idle 回调回到主线程解析
                let spec = inner.build_request(&raw);

                inner
                    .status_label
                    .set_text(&format!("正在以 {} 请求…", spec.method.as_str()));

                let (tx, rx) = std::sync::mpsc::channel::<Result<String, String>>();
                std::thread::spawn(move || {
                    let _ = tx.send(http::send(&spec));
                });
                let inner = Rc::clone(&inner);
                glib::source::idle_add_local(move || {
                    match rx.try_recv() {
                        Ok(fetched) => {
                            let parsed = match fetched {
                                Ok(text) => json_util::parse(&text),
                                Err(e) => Err(e),
                            };
                            inner.apply_parsed(parsed);
                            glib::ControlFlow::Break
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                        Err(_) => glib::ControlFlow::Break,
                    }
                });
            } else {
                // 文本模式：直接在主线程解析（开销很小）
                let parsed = json_util::parse(&raw);
                inner.apply_parsed(parsed);
            }
        }));
    }

    JsonParserPage { root: toast_overlay }
}

impl Inner {
    fn toast(&self, msg: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(msg));
    }

    /// 把界面上填写的 URL / 请求方法 / 请求头 / 请求体组装成纯数据的请求描述。
    fn build_request(&self, url: &str) -> http::RequestSpec {
        let method = if self.method_row.selected() == METHOD_POST {
            http::Method::Post
        } else {
            http::Method::Get
        };

        http::RequestSpec {
            url: url.to_string(),
            method,
            headers: self.header_list.collect(),
            body: buffer_text(&self.body_buffer),
        }
    }

    /// 处理解析结果：成功则渲染树并更新「复制全部」，失败则提示错误。
    fn apply_parsed(&self, result: Result<Value, String>) {
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.parse_button.set_sensitive(true);

        match result {
            Ok(value) => {
                *self.pretty.borrow_mut() = json_util::format_pretty(&value);
                populate_tree(&self.tree_store, &value);
                self.tree_view.expand_to_path(&gtk::TreePath::new_first());
                self.copy_all_btn.set_sensitive(true);
                self.status_label.set_text("解析成功");
            }
            Err(e) => {
                self.tree_store.clear();
                self.pretty.borrow_mut().clear();
                self.copy_all_btn.set_sensitive(false);
                self.copy_selected_btn.set_sensitive(false);
                self.status_label.set_text(&format!("解析失败：{e}"));
                self.toast(&e);
            }
        }
    }
}
