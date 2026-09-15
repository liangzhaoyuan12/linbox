//! 进程标签页：可排序表格 + 搜索 / 过滤 / 树形模式 + 完整信号与进程控制 + 详情面板。

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use glib::clone;
use gtk::gio;
use gtk::glib;

use super::{ToastFn, card};
use crate::model::monitor::{ProcColumn, Process, ProcessDetail, SIGNALS, Snapshot, TreeMode};
use crate::utils::monitor as mon;
use crate::utils::monitor::proc as mproc;
use crate::utils::monitor::signal as msig;

/// 状态下拉选项。
const STATE_FILTERS: &[(&str, char)] = &[
    ("全部状态", '\0'),
    ("运行 (R)", 'R'),
    ("睡眠 (S)", 'S'),
    ("不可中断 (D)", 'D'),
    ("已停止 (T)", 'T'),
    ("僵死 (Z)", 'Z'),
];

/// 详情里长列表最多显示多少行。
const LIST_LIMIT: usize = 500;

pub struct ProcsTab {
    root: gtk::Box,
    toast: ToastFn,
    // ---- 工具条 ----
    search: gtk::SearchEntry,
    user_dd: gtk::DropDown,
    user_dd_uids: RefCell<Vec<Option<u32>>>,
    state_dd: gtk::DropDown,
    kernel_switch: gtk::Switch,
    mode_dd: gtk::DropDown,
    freeze_btn: gtk::ToggleButton,
    signal_dd: gtk::DropDown,
    info: gtk::Label,
    // ---- 表格 ----
    view: gtk::ColumnView,
    store: gio::ListStore,
    sort_model: gtk::SortListModel,
    selection: gtk::SingleSelection,
    col_map: Rc<RefCell<Vec<(ProcColumn, gtk::ColumnViewColumn)>>>,
    // ---- 详情 ----
    d_overview: gtk::Label,
    d_mem: gtk::Label,
    d_threads: gtk::Label,
    d_fds: gtk::Label,
    d_env: gtk::Label,
    d_ns: gtk::Label,
    // ---- 状态 ----
    procs: RefCell<Vec<Process>>,
    shown: RefCell<Vec<Process>>,
    order: RefCell<Vec<i32>>,
    collapsed: Rc<RefCell<HashSet<i32>>>,
    selected_pid: Cell<Option<i32>>,
    sort_col: Cell<ProcColumn>,
    sort_desc: Cell<bool>,
    detail: RefCell<Option<ProcessDetail>>,
    detail_pid: Cell<i32>,
    update_count: Cell<u64>,
    /// 已排入 idle 的重建请求（同一轮主循环里多次请求合并成一次）。
    rebuild_pending: Cell<bool>,
}

impl ProcsTab {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }

    pub fn new(toast: ToastFn) -> Rc<ProcsTab> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.set_vexpand(true);

        // ==================== 过滤 / 工具条 ====================
        let (c, bar) = card();
        // 工具条横向可滚动：窗口窄时滚动查看，而不是被这一行的最小宽度撑住窗口
        let bar_scroll = gtk::ScrolledWindow::new();
        bar_scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Never);
        bar_scroll.set_child(Some(&c));
        root.append(&bar_scroll);

        let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("搜索：名称 / 命令行 / 用户 / PID…"));
        search.set_hexpand(true);
        search.set_size_request(240, -1);
        top.append(&search);

        let user_dd = gtk::DropDown::new(
            Some(gtk::StringList::new(&["全部用户"])),
            None::<gtk::Expression>,
        );
        user_dd.set_tooltip_text(Some("只看某个用户的进程（第一个是你自己）"));
        top.append(&user_dd);

        let state_names: Vec<&str> = STATE_FILTERS.iter().map(|(n, _)| *n).collect();
        let state_dd = gtk::DropDown::new(
            Some(gtk::StringList::new(&state_names)),
            None::<gtk::Expression>,
        );
        state_dd.set_tooltip_text(Some("按进程状态过滤"));
        top.append(&state_dd);

        let mode_dd = gtk::DropDown::new(
            Some(gtk::StringList::new(&[
                "平铺列表",
                "树形（主进程 + 子进程）",
            ])),
            None::<gtk::Expression>,
        );
        mode_dd.set_tooltip_text(Some(
            "平铺：所有进程排成一层（点表头排序）；树形：按父子关系缩进，同级按当前排序键排列，双击行可折叠/展开",
        ));
        top.append(&mode_dd);

        let freeze_btn = gtk::ToggleButton::with_label("固定顺序");
        freeze_btn.set_tooltip_text(Some(
            "不再随数值变化重排（平铺模式下生效），方便盯住某个进程看",
        ));
        top.append(&freeze_btn);

        let kernel_label = gtk::Label::new(Some("隐藏内核线程"));
        kernel_label.add_css_class("caption");
        kernel_label.add_css_class("dim-label");
        let kernel_switch = gtk::Switch::new();
        kernel_switch.set_valign(gtk::Align::Center);
        kernel_switch.set_tooltip_text(Some("内核线程没有命令行，数量很多；默认显示"));
        top.append(&kernel_label);
        top.append(&kernel_switch);
        bar.append(&top);

        // 操作行
        let act = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let stop_btn = gtk::Button::with_label("冻结");
        stop_btn.set_tooltip_text(Some("向选中进程发送 SIGSTOP（暂停执行）"));
        let cont_btn = gtk::Button::with_label("继续");
        cont_btn.set_tooltip_text(Some("向选中进程发送 SIGCONT（恢复执行）"));
        let term_btn = gtk::Button::with_label("结束");
        term_btn.set_tooltip_text(Some("发送 SIGTERM（可被捕获，进程能优雅退出），会先确认"));
        term_btn.add_css_class("destructive-action");
        let kill_btn = gtk::Button::with_label("强制结束");
        kill_btn.set_tooltip_text(Some("发送 SIGKILL（不可捕获，立即结束），会先确认"));
        kill_btn.add_css_class("destructive-action");
        act.append(&stop_btn);
        act.append(&cont_btn);
        act.append(&term_btn);
        act.append(&kill_btn);

        let sig_names: Vec<String> = SIGNALS
            .iter()
            .map(|s| format!("{} · {}", s.name, s.desc))
            .collect();
        let sig_refs: Vec<&str> = sig_names.iter().map(|s| s.as_str()).collect();
        let signal_dd = gtk::DropDown::new(
            Some(gtk::StringList::new(&sig_refs)),
            None::<gtk::Expression>,
        );
        signal_dd.set_selected(SIGNALS.iter().position(|s| s.num == 15).unwrap_or(0) as u32);
        signal_dd.set_tooltip_text(Some("所有 Linux 信号（1-31 标准 + 34-64 实时）"));
        signal_dd.set_size_request(240, -1);
        let send_btn = gtk::Button::with_label("发送信号");
        act.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        act.append(&signal_dd);
        act.append(&send_btn);

        let more_btn = gtk::MenuButton::new();
        more_btn.set_label("更多操作");
        more_btn.set_tooltip_text(Some(
            "优先级 / CPU 亲和性 / IO 优先级 / 结束整棵进程树 / 复制进程信息",
        ));
        act.append(&more_btn);
        bar.append(&act);

        let info = gtk::Label::new(Some("等待采样…"));
        info.add_css_class("caption");
        info.add_css_class("dim-label");
        info.set_xalign(0.0);
        bar.append(&info);

        // ==================== 表格 ====================
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let sort_model = gtk::SortListModel::new(Some(store.clone()), None::<gtk::Sorter>);
        let selection = gtk::SingleSelection::new(Some(sort_model.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);

        let view = gtk::ColumnView::new(Some(selection.clone()));
        view.set_show_row_separators(true);
        view.set_vexpand(true);
        view.set_hexpand(true);
        // GTK 4.18 的 GtkColumnView 默认允许拖动表头改变列顺序：点表头排序时
        // 稍带一点拖动就会把列悄悄挪走（且本项目不保存列顺序，只能重启恢复），
        // 这里按「只排序、不重排」关掉它。
        view.set_reorderable(false);

        let collapsed: Rc<RefCell<HashSet<i32>>> = Rc::new(RefCell::new(HashSet::new()));
        let col_map: Rc<RefCell<Vec<(ProcColumn, gtk::ColumnViewColumn)>>> =
            Rc::new(RefCell::new(Vec::new()));
        for col in ProcColumn::ALL.iter().copied() {
            let factory = gtk::SignalListItemFactory::new();
            factory.connect_setup(|_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let l = gtk::Label::new(None);
                l.set_ellipsize(gtk::pango::EllipsizeMode::End);
                l.set_single_line_mode(true);
                item.set_child(Some(&l));
            });
            let collapsed_for_bind = collapsed.clone();
            factory.connect_bind(move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let Some(obj) = item.item() else { return };
                let Ok(obj) = obj.downcast::<glib::BoxedAnyObject>() else {
                    return;
                };
                let Some(l) = item.child().and_then(|c| c.downcast::<gtk::Label>().ok()) else {
                    return;
                };
                let p = obj.borrow::<Process>();
                let pid = p.pid;
                l.set_text(&cell_text(&p, col, &collapsed_for_bind.borrow()));
                l.set_tooltip_text(Some(&cell_tooltip(&p, col)));
                if col == ProcColumn::Name {
                    l.set_xalign(0.0);
                    l.set_margin_start(6 + (p.depth as i32) * 14);
                } else if col.is_numeric() {
                    l.set_xalign(1.0);
                    l.set_margin_start(0);
                    l.set_margin_end(8);
                } else {
                    l.set_xalign(0.0);
                    l.set_margin_start(6);
                }
                // 供右键命中测试取回 pid
                unsafe { l.set_data("linbox-pid", pid) };
            });
            let cvc = gtk::ColumnViewColumn::new(Some(col.label()), Some(factory));
            cvc.set_fixed_width(col.width());
            cvc.set_resizable(true);
            // 排序器只给「升序」的比较，升/降序由 GTK 自己翻转
            let sorter = gtk::CustomSorter::new(move |a, b| {
                let (Some(a), Some(b)) = (
                    a.downcast_ref::<glib::BoxedAnyObject>(),
                    b.downcast_ref::<glib::BoxedAnyObject>(),
                ) else {
                    return gtk::Ordering::Equal;
                };
                let pa = a.borrow::<Process>();
                let pb = b.borrow::<Process>();
                match mproc::compare_by(&pa, &pb, col, false) {
                    std::cmp::Ordering::Less => gtk::Ordering::Smaller,
                    std::cmp::Ordering::Equal => gtk::Ordering::Equal,
                    std::cmp::Ordering::Greater => gtk::Ordering::Larger,
                }
            });
            cvc.set_sorter(Some(&sorter));
            cvc.set_visible(ProcColumn::DEFAULT_VISIBLE.contains(&col));
            col_map.borrow_mut().push((col, cvc.clone()));
            view.append_column(&cvc);
        }

        // 列显示 / 隐藏
        let col_menu = gtk::Popover::new();
        let col_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        col_box.set_margin_top(8);
        col_box.set_margin_bottom(8);
        col_box.set_margin_start(8);
        col_box.set_margin_end(8);
        for (col, cvc) in col_map.borrow().iter() {
            let cb = gtk::CheckButton::with_label(col.label());
            cb.set_active(cvc.is_visible());
            let cvc2 = cvc.clone();
            cb.connect_toggled(move |b| cvc2.set_visible(b.is_active()));
            col_box.append(&cb);
        }
        col_menu.set_child(Some(&col_box));
        let col_btn = gtk::MenuButton::new();
        col_btn.set_label("列");
        col_btn.set_tooltip_text(Some("显示 / 隐藏列"));
        col_btn.set_popover(Some(&col_menu));
        top.append(&col_btn);

        // 表格 + 详情（左：进程表；右：详情，只占窄窄一条）
        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_vexpand(true);
        paned.set_hexpand(true);
        // 左：进程表（吃掉多余空间）；右：详情面板（不缩到 0）
        paned.set_resize_start_child(true);
        paned.set_shrink_start_child(true);
        paned.set_resize_end_child(false);
        paned.set_shrink_end_child(false);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        scroll.set_child(Some(&view));
        scroll.set_vexpand(true);
        scroll.set_hexpand(true);
        scroll.set_min_content_width(320);
        paned.set_start_child(Some(&scroll));

        let (d_overview, d_mem, d_threads, d_fds, d_env, d_ns, detail_box) = build_detail();
        paned.set_end_child(Some(&detail_box));
        // 分隔条钉在最右边：详情保持固定窄宽（拖动仍然有效，窗口缩放后会回到窄宽）
        paned.connect_notify_local(Some("max-position"), move |p, _| {
            let max = p.max_position();
            if max > 0 && p.position() != max {
                p.set_position(max);
            }
        });
        root.append(&paned);

        let tab = Rc::new(ProcsTab {
            root,
            toast,
            search,
            user_dd,
            user_dd_uids: RefCell::new(vec![None]),
            state_dd,
            kernel_switch,
            mode_dd,
            freeze_btn,
            signal_dd,
            info,
            view: view.clone(),
            store,
            sort_model,
            selection: selection.clone(),
            col_map: col_map.clone(),
            d_overview,
            d_mem,
            d_threads,
            d_fds,
            d_env,
            d_ns,
            procs: RefCell::new(Vec::new()),
            shown: RefCell::new(Vec::new()),
            order: RefCell::new(Vec::new()),
            collapsed,
            selected_pid: Cell::new(None),
            sort_col: Cell::new(ProcColumn::Cpu),
            sort_desc: Cell::new(true),
            detail: RefCell::new(None),
            detail_pid: Cell::new(-1),
            update_count: Cell::new(0),
            rebuild_pending: Cell::new(false),
        });

        // ==================== 交互接线 ====================
        tab.search.connect_search_changed(clone!(
            #[weak]
            tab,
            move |_| tab.rebuild()
        ));
        tab.user_dd.connect_selected_notify(clone!(
            #[weak]
            tab,
            move |_| tab.rebuild()
        ));
        tab.state_dd.connect_selected_notify(clone!(
            #[weak]
            tab,
            move |_| tab.rebuild()
        ));
        // 注意：clone! 的 #[weak] 在返回非 () 的闭包里插不了 early-return，这里手写 Weak
        {
            let weak = Rc::downgrade(&tab);
            tab.kernel_switch.connect_state_set(move |_, _| {
                if let Some(tab) = weak.upgrade() {
                    tab.rebuild();
                }
                glib::Propagation::Proceed
            });
        }
        tab.mode_dd.connect_selected_notify(clone!(
            #[weak]
            tab,
            move |_| {
                tab.apply_sort_mode();
                tab.schedule_rebuild();
            }
        ));
        tab.freeze_btn.connect_toggled(clone!(
            #[weak]
            tab,
            move |b| {
                tab.apply_sort_mode();
                tab.schedule_rebuild();
                let _ = b;
            }
        ));

        tab.selection.connect_selection_changed(clone!(
            #[weak]
            tab,
            move |_, _, _| tab.on_selection_changed()
        ));

        // 双击行：树形模式下折叠 / 展开子树（重建同样推迟，见 schedule_rebuild）
        tab.view.connect_activate(clone!(
            #[weak]
            tab,
            move |_, pos| tab.toggle_collapse(pos)
        ));

        // 表头点击 → 记录排序键（树形模式要用）
        if let Some(sorter) = view.sorter() {
            let map = col_map.clone();
            sorter.connect_changed(clone!(
                #[weak]
                tab,
                move |s, _change| {
                    let Some(cs) = s.downcast_ref::<gtk::ColumnViewSorter>() else {
                        return;
                    };
                    let order = cs.primary_sort_order();
                    if let Some(col) = cs.primary_sort_column() {
                        if let Some((pc, _)) = map.borrow().iter().find(|(_, c)| *c == col) {
                            tab.sort_col.set(*pc);
                            tab.sort_desc.set(order == gtk::SortType::Descending);
                            // 树形 / 固定顺序模式：排序键决定同级顺序，需要重建。
                            // 但绝不能在这个信号里同步改 ListStore —— GTK 的列视图
                            // 正处在处理本次排序变更的过程中，同步 splice 会把它打
                            // 进不一致状态（实测 SIGSEGV，崩在 GtkColumnView 的
                            // items-changed 处理器里），必须挪到下一轮主循环。
                            tab.schedule_rebuild();
                        }
                    }
                }
            ));
        }

        // 启动时先把排序器接到 SortListModel 上：否则表头点一次只有箭头、行序不动
        // （apply_sort_mode 先前只在切换模式 / 固定顺序时调用）
        tab.apply_sort_mode();
        // 初始按 CPU 降序（与 sort_col / sort_desc 的默认值一致），表头立刻显示箭头
        if let Some((_, cvc)) = col_map.borrow().iter().find(|(c, _)| *c == ProcColumn::Cpu) {
            view.sort_by_column(Some(cvc), gtk::SortType::Descending);
        }

        send_btn.connect_clicked(clone!(
            #[weak]
            tab,
            move |_| {
                let idx = tab.signal_dd.selected() as usize;
                if let Some(spec) = SIGNALS.get(idx) {
                    tab.do_signal(spec.num);
                }
            }
        ));
        stop_btn.connect_clicked(clone!(
            #[weak]
            tab,
            move |_| tab.do_signal(19)
        ));
        cont_btn.connect_clicked(clone!(
            #[weak]
            tab,
            move |_| tab.do_signal(18)
        ));
        term_btn.connect_clicked(clone!(
            #[weak]
            tab,
            move |_| tab.do_signal(15)
        ));
        kill_btn.connect_clicked(clone!(
            #[weak]
            tab,
            move |_| tab.do_signal(9)
        ));

        more_btn.set_popover(Some(&build_more_popover(&tab)));

        // 右键菜单
        let gesture = gtk::GestureClick::new();
        gesture.set_button(3);
        let view_for_click = view.clone();
        let pop = build_more_popover(&tab);
        gesture.connect_pressed(clone!(
            #[weak]
            tab,
            move |_, _, x, y| {
                // GTK4 的 ColumnView 不导出「行 widget」类型，拿不到 GtkColumnViewRow；
                // 于是把 pid 挂在每个单元格控件上（见 bind 里的 set_data），命中测试后取回。
                if let Some(pid) = view_for_click
                    .pick(
                        x,
                        y,
                        gtk::PickFlags::DEFAULT | gtk::PickFlags::NON_TARGETABLE,
                    )
                    .and_then(|w| w.downcast::<gtk::Label>().ok())
                    .and_then(|l| {
                        unsafe { l.data::<i32>("linbox-pid") }.map(|p| unsafe { *p.as_ref() })
                    })
                {
                    tab.select_pid(pid);
                }
                pop.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                pop.popup();
            }
        ));
        view.add_controller(gesture);

        // 键盘快捷键（表格获得焦点时生效）
        let action_group = gio::SimpleActionGroup::new();
        let a_term = gio::SimpleAction::new("term", None);
        a_term.connect_activate(clone!(
            #[weak]
            tab,
            move |_, _| tab.do_signal(15)
        ));
        let a_kill = gio::SimpleAction::new("kill", None);
        a_kill.connect_activate(clone!(
            #[weak]
            tab,
            move |_, _| tab.do_signal(9)
        ));
        let a_prio = gio::SimpleAction::new("prio", None);
        a_prio.connect_activate(clone!(
            #[weak]
            tab,
            move |_, _| tab.open_priority_dialog()
        ));
        let a_aff = gio::SimpleAction::new("affinity", None);
        a_aff.connect_activate(clone!(
            #[weak]
            tab,
            move |_, _| tab.open_affinity_dialog()
        ));
        action_group.add_action(&a_term);
        action_group.add_action(&a_kill);
        action_group.add_action(&a_prio);
        action_group.add_action(&a_aff);
        view.insert_action_group("proc", Some(&action_group));

        let shortcuts = gtk::ShortcutController::new();
        shortcuts.set_scope(gtk::ShortcutScope::Managed);
        for (key, action) in [
            ("Delete", "proc.term"),
            ("<Shift>Delete", "proc.kill"),
            ("F8", "proc.prio"),
            ("F9", "proc.affinity"),
        ] {
            if let Some(trigger) = gtk::ShortcutTrigger::parse_string(key) {
                shortcuts.add_shortcut(gtk::Shortcut::new(
                    Some(trigger),
                    Some(gtk::NamedAction::new(action)),
                ));
            }
        }
        view.add_controller(shortcuts);

        tab
    }

    // -----------------------------------------------------------------------
    // 刷新
    // -----------------------------------------------------------------------

    /// 每轮采样：更新进程集合 → 重建显示 → 刷新详情。
    pub fn update(&self, s: &Snapshot) {
        self.refresh_users(s);
        *self.procs.borrow_mut() = s.processes.clone();
        self.update_count.set(self.update_count.get() + 1);
        self.rebuild();
        self.refresh_detail(false);
    }

    /// 用户下拉框：收录快照里出现的 uid（集合不变就不动，避免每次采样重建模型）。
    fn refresh_users(&self, s: &Snapshot) {
        let me = msig::current_uid();
        let mut seen: Vec<(u32, String)> = Vec::new();
        for p in s.processes.iter() {
            if !seen.iter().any(|(u, _)| *u == p.uid) {
                seen.push((p.uid, p.user.clone()));
            }
        }
        seen.sort_by_key(|(u, _)| if *u == me { (0u8, *u) } else { (1u8, *u) });

        {
            let cur = self.user_dd_uids.borrow();
            let same = cur.len() == seen.len() + 1
                && seen
                    .iter()
                    .enumerate()
                    .all(|(i, (u, _))| cur.get(i + 1) == Some(&Some(*u)));
            if same {
                return;
            }
        }
        let mut labels = vec!["全部用户".to_string()];
        let mut uids: Vec<Option<u32>> = vec![None];
        for (u, name) in seen.iter() {
            labels.push(if *u == me {
                format!("{name}（我）")
            } else {
                format!("{name} ({u})")
            });
            uids.push(Some(*u));
        }
        let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
        let selected = self.user_dd.selected();
        self.user_dd.set_model(Some(&gtk::StringList::new(&refs)));
        *self.user_dd_uids.borrow_mut() = uids;
        self.user_dd
            .set_selected(selected.min(labels.len() as u32 - 1));
    }

    /// 把一次重建推迟到下一轮主循环：供「sorter 的 changed 信号」等
    /// GTK 内部正在更新时调用（同步 splice ListStore 会把列视图打崩）。
    /// 同一轮里的重复请求合并成一次。
    fn schedule_rebuild(self: &Rc<Self>) {
        if self.rebuild_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(tab) = weak.upgrade() else {
                return;
            };
            tab.rebuild_pending.set(false);
            tab.rebuild();
        });
    }

    /// 按当前过滤 / 排序 / 折叠状态重建显示列表并写回表格。
    fn rebuild(&self) {
        let procs = self.procs.borrow();
        if procs.is_empty() {
            return;
        }
        let query = self.search.text().to_string();
        let only_uid = self
            .user_dd_uids
            .borrow()
            .get(self.user_dd.selected() as usize)
            .copied()
            .flatten();
        let hide_kernel = self.kernel_switch.is_active();
        let state_want = STATE_FILTERS
            .get(self.state_dd.selected() as usize)
            .map(|(_, c)| *c)
            .unwrap_or('\0');

        let filtered: Vec<Process> = procs
            .iter()
            .filter(|p| mproc::matches_filter(p, &query, only_uid, hide_kernel))
            .filter(|p| state_want == '\0' || p.state_char == state_want)
            .cloned()
            .collect();

        let tree = self.mode_dd.selected() == 1;
        let frozen = self.freeze_btn.is_active();
        let col = self.sort_col.get();
        let desc = self.sort_desc.get();
        let cmp = move |a: &Process, b: &Process| mproc::compare_by(a, b, col, desc);

        let mut shown = if tree {
            mproc::flatten_tree(filtered, TreeMode::Tree, &self.collapsed.borrow(), &cmp)
        } else if frozen {
            // 固定顺序：沿用上次的顺序，新出现的进程排最后
            let order = self.order.borrow();
            let mut v = filtered;
            v.sort_by_key(|p| {
                order
                    .iter()
                    .position(|pid| *pid == p.pid)
                    .unwrap_or(usize::MAX)
            });
            v
        } else {
            mproc::flatten_tree(filtered, TreeMode::Flat, &self.collapsed.borrow(), &cmp)
        };

        *self.order.borrow_mut() = shown.iter().map(|p| p.pid).collect();

        let items: Vec<glib::BoxedAnyObject> = shown
            .iter_mut()
            .map(|p| glib::BoxedAnyObject::new(p.clone()))
            .collect();
        self.store.splice(0, self.store.n_items(), &items);

        // 恢复选中（顺序变了要按 pid 找回来）
        if let Some(pid) = self.selected_pid.get() {
            match shown.iter().position(|p| p.pid == pid) {
                Some(i) => self.selection.set_selected(i as u32),
                None => {
                    self.selection.set_selected(gtk::INVALID_LIST_POSITION);
                    self.selected_pid.set(None);
                }
            }
        }

        let mut notes = Vec::new();
        if tree {
            notes.push("树形模式");
        }
        if frozen {
            notes.push("顺序已固定");
        }
        self.info.set_text(&format!(
            "共 {} 个进程，显示 {} 个{}",
            procs.len(),
            shown.len(),
            if notes.is_empty() {
                String::new()
            } else {
                format!("（{}）", notes.join(" · "))
            }
        ));
        *self.shown.borrow_mut() = shown;
    }

    /// 平铺且未固定顺序时交给 GTK 排序器；树形 / 固定顺序由我们自己排
    /// （否则 GTK 会把我们排好的树形顺序重新打散）。
    fn apply_sort_mode(&self) {
        let tree = self.mode_dd.selected() == 1;
        let frozen = self.freeze_btn.is_active();
        if !tree && !frozen {
            if let Some(s) = self.view.sorter() {
                self.sort_model.set_sorter(Some(&s));
            }
        } else {
            self.sort_model.set_sorter(None::<&gtk::Sorter>);
        }
    }

    fn on_selection_changed(&self) {
        let pid = self
            .selection
            .selected_item()
            .and_then(|o| o.downcast::<glib::BoxedAnyObject>().ok())
            .map(|o| o.borrow::<Process>().pid);
        if let Some(pid) = pid {
            if self.selected_pid.get() != Some(pid) {
                self.selected_pid.set(Some(pid));
                self.refresh_detail(true);
            }
        }
    }

    fn select_pid(&self, pid: i32) {
        let idx = self.shown.borrow().iter().position(|p| p.pid == pid);
        if let Some(i) = idx {
            self.selection.set_selected(i as u32);
            self.selected_pid.set(Some(pid));
            // 让选中行进入视野（右键 / 程序化选中时都需要）
            self.view.scroll_to(
                i as u32,
                None::<&gtk::ColumnViewColumn>,
                gtk::ListScrollFlags::FOCUS,
                None::<gtk::ScrollInfo>,
            );
        }
    }

    /// 树形模式下折叠 / 展开某个 pid 的子树。
    fn toggle_collapse(self: &Rc<Self>, pos: u32) {
        if self.mode_dd.selected() != 1 {
            return;
        }
        let Some(obj) = self
            .selection
            .model()
            .and_then(|m| m.item(pos).map(|o| (m, o)))
            .and_then(|(_, o)| o.downcast::<glib::BoxedAnyObject>().ok())
        else {
            return;
        };
        let pid = obj.borrow::<Process>().pid;
        let has_children = obj.borrow::<Process>().child_count > 0;
        if !has_children {
            return;
        }
        {
            let mut set = self.collapsed.borrow_mut();
            if !set.remove(&pid) {
                set.insert(pid);
            }
        }
        self.selected_pid.set(Some(pid));
        self.schedule_rebuild();
    }

    /// 当前选中的进程（从最新快照取，数值最新）。
    fn selected_process(&self) -> Option<Process> {
        let pid = self.selected_pid.get()?;
        self.procs.borrow().iter().find(|p| p.pid == pid).cloned()
    }

    // -----------------------------------------------------------------------
    // 详情面板
    // -----------------------------------------------------------------------

    fn refresh_detail(&self, force: bool) {
        let Some(p) = self.selected_process() else {
            self.d_overview
                .set_text("在上方表格里选一个进程查看详情（双击行可折叠 / 展开子树）");
            for l in [
                &self.d_mem,
                &self.d_threads,
                &self.d_fds,
                &self.d_env,
                &self.d_ns,
            ] {
                l.set_text("");
            }
            *self.detail.borrow_mut() = None;
            self.detail_pid.set(-1);
            return;
        };
        // 读 /proc/<pid>/* 开销较大：换进程时读一次，之后每 5 轮刷新一次
        if self.detail_pid.get() != p.pid || force || self.update_count.get() % 5 == 0 {
            *self.detail.borrow_mut() = Some(mproc::process_detail(p.pid));
            self.detail_pid.set(p.pid);
        }
        let d = self.detail.borrow();
        let exe = detail_str(&d, |x| &x.exe, "读不到（需要 root）");
        let cwd = detail_str(&d, |x| &x.cwd, "读不到（需要 root）");
        let aff = mproc::affinity_cpus(&p.affinity);

        self.d_overview.set_text(&format!(
            "名称        {}（PID {}）\n\
             父进程      {}    进程组 {}    会话 {}\n\
             用户        {}（uid {}）\n\
             状态        {} ({})        线程 {}        运行时长 {}\n\
             优先级      {}    nice {}    调度 {}\n\
             CPU 占用    {:.1}%              CPU 时间 {}\n\
             内存        {}（{:.1}%）      虚拟内存 {}\n\
             速率        读 {} / 写 {}\n\
             累计读写    读 {} / 写 {}\n\
             启动时间    {}\n\
             IO 调度     {}\n\
             CPU 亲和    {} 个核 {}\n\
             cgroup      {}\n\
             可执行      {}\n\
             工作目录    {}",
            p.display_name(),
            p.pid,
            p.ppid,
            p.pgid,
            p.sid,
            p.user,
            p.uid,
            p.state,
            p.state_char,
            p.threads,
            mon::human_duration(p.uptime),
            p.priority,
            p.nice,
            p.policy,
            p.cpu,
            mon::human_duration(p.cpu_time as u64),
            mon::human_bytes(p.rss),
            p.mem_pct,
            mon::human_bytes(p.virt),
            mon::human_rate(p.read_rate),
            mon::human_rate(p.write_rate),
            mon::human_bytes(p.read_bytes),
            mon::human_bytes(p.write_bytes),
            mon::human_time(p.started_at),
            if p.io_class.is_empty() {
                "—"
            } else {
                &p.io_class
            },
            aff.len(),
            if aff.len() <= 8 {
                format!(
                    "（{}）",
                    aff.iter()
                        .map(|c| c.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                )
            } else {
                format!("（{}…{}）", aff[0], aff[aff.len() - 1])
            },
            if p.cgroup.is_empty() {
                "—"
            } else {
                &p.cgroup
            },
            exe,
            cwd,
        ));

        self.d_mem.set_text(&match d.as_ref() {
            Some(x) if x.status_text.is_empty() => "读不到 /proc/<pid>/status".to_string(),
            Some(x) => format!(
                "VmPeak  虚拟内存峰值  {}\n\
                 VmSize  虚拟内存      {}\n\
                 VmRSS   常驻内存      {}（{} 页）\n\
                 VmData  数据段        {}\n\
                 VmStk   栈            {}\n\
                 VmExe   可执行文件    {}\n\
                 VmLib   共享库        {}\n\
                 VmSwap  换出到交换    {}\n\
                 \n\
                 root  {}   \n\
                 用户组  {}",
                mon::human_bytes(x.vm_peak),
                mon::human_bytes(x.vm_size),
                mon::human_bytes(x.vm_rss),
                x.vm_rss / 4096,
                mon::human_bytes(x.vm_data),
                mon::human_bytes(x.vm_stk),
                mon::human_bytes(x.vm_exe),
                mon::human_bytes(x.vm_lib),
                mon::human_bytes(x.vm_swap),
                if x.root.is_empty() { "—" } else { &x.root },
                if x.groups.is_empty() {
                    "—".to_string()
                } else {
                    x.groups
                        .iter()
                        .map(|g| g.to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                },
            ),
            None => "读取中…".to_string(),
        });

        self.d_threads.set_text(&match d.as_ref() {
            Some(x) if x.thread_list.is_empty() => "读不到线程列表".to_string(),
            Some(x) => {
                let mut s = format!("共 {} 个线程（TID / 状态 / 名称）\n\n", x.thread_list.len());
                for (tid, st, name) in x.thread_list.iter().take(LIST_LIMIT) {
                    s.push_str(&format!("{tid:<8} {st}   {name}\n"));
                }
                s
            }
            None => "读取中…".to_string(),
        });

        self.d_fds.set_text(&match d.as_ref() {
            Some(x) if x.fds.is_empty() => "读不到（可能权限不足）".to_string(),
            Some(x) => {
                let mut s = format!("共 {} 个打开的文件描述符\n\n", x.fds.len());
                for f in x.fds.iter().take(LIST_LIMIT) {
                    s.push_str(f);
                    s.push('\n');
                }
                s
            }
            None => "读取中…".to_string(),
        });

        self.d_env.set_text(&match d.as_ref() {
            Some(x) if x.env.is_empty() => "读不到（可能权限不足）".to_string(),
            Some(x) => {
                let mut s = format!("共 {} 个环境变量\n\n", x.env.len());
                for e in x.env.iter().take(LIST_LIMIT) {
                    s.push_str(e);
                    s.push('\n');
                }
                s
            }
            None => "读取中…".to_string(),
        });

        self.d_ns.set_text(&match d.as_ref() {
            Some(x) if x.namespaces.is_empty() => "读不到".to_string(),
            Some(x) => x.namespaces.join("\n"),
            None => "读取中…".to_string(),
        });
    }

    // -----------------------------------------------------------------------
    // 信号 / 进程控制
    // -----------------------------------------------------------------------

    fn do_signal(self: &Rc<Self>, sig: i32) {
        let Some(p) = self.selected_process() else {
            self.toast_msg("先在上方表格里选一个进程", true);
            return;
        };
        let spec = crate::model::monitor::signal_by_num(sig);
        let name = spec.map(|s| s.name).unwrap_or("信号");
        let desc = spec.map(|s| s.desc).unwrap_or("");
        // 不可逆 / 会丢数据的信号先确认
        let dangerous = matches!(sig, 9 | 15 | 3 | 6 | 24 | 25);
        if !dangerous {
            self.send_now(p.pid, &p.name, sig, name, desc);
            return;
        }
        let dialog = adw::MessageDialog::new(
            self.window().as_ref(),
            Some(&format!("确认向 {} 发送 {name}？", p.name)),
            Some(&format!(
                "PID {} · 用户 {}\n{}\n\n{desc}",
                p.pid,
                p.user,
                if p.cmdline.is_empty() {
                    "（内核线程）"
                } else {
                    &p.cmdline
                }
            )),
        );
        dialog.add_response("cancel", "取消");
        dialog.add_response(
            "ok",
            if sig == 9 {
                "强制结束"
            } else {
                "确认发送"
            },
        );
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let pid = p.pid;
        let pname = p.name.clone();
        dialog.connect_response(
            None,
            clone!(
                #[weak(rename_to = tab)]
                self,
                move |dlg, resp| {
                    dlg.close();
                    if resp == "ok" {
                        tab.send_now(pid, &pname, sig, name, desc);
                    }
                }
            ),
        );
        dialog.present();
    }

    fn send_now(self: &Rc<Self>, pid: i32, pname: &str, sig: i32, name: &str, desc: &str) {
        match msig::send_signal(pid, sig) {
            Ok(()) => self.toast_msg(
                &format!("已向 {pname}（PID {pid}）发送 {name} —— {desc}"),
                false,
            ),
            Err(e) => {
                if msig::is_root() {
                    self.toast_msg(&format!("发送 {name} 失败：{e}"), true);
                } else {
                    self.ask_elevate(pid, pname, sig, name, &e);
                }
            }
        }
    }

    /// 权限不足时询问是否用 pkexec 提权（Polkit 会弹授权框）。
    fn ask_elevate(self: &Rc<Self>, pid: i32, pname: &str, sig: i32, name: &str, err: &str) {
        let dialog = adw::MessageDialog::new(
            self.window().as_ref(),
            Some(&format!("{name} 发送失败")),
            Some(&format!(
                "{err}\n\n目标进程 {pname}（PID {pid}）不属于当前用户。\
                 可以用 pkexec 提权后再发送（会弹出 Polkit 授权框）。"
            )),
        );
        dialog.add_response("cancel", "取消");
        dialog.add_response("ok", "提权重试");
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        dialog.set_close_response("cancel");
        // 闭包要求 'static，把借用的名字拷一份进去
        let name = name.to_string();
        dialog.connect_response(
            None,
            clone!(
                #[weak(rename_to = tab)]
                self,
                move |dlg, resp| {
                    dlg.close();
                    if resp != "ok" {
                        return;
                    }
                    match msig::pkexec_send_signal(pid, sig) {
                        Ok(()) => {
                            tab.toast_msg(&format!("已通过 pkexec 向 PID {pid} 发送 {name}"), false)
                        }
                        Err(e) => tab.toast_msg(&format!("提权发送失败：{e}"), true),
                    }
                }
            ),
        );
        dialog.present();
    }

    fn toast_msg(&self, msg: &str, err: bool) {
        (self.toast)(msg, err);
    }

    fn window(&self) -> Option<gtk::Window> {
        self.root
            .root()
            .and_then(|r| r.downcast::<gtk::Window>().ok())
    }

    /// 调整 nice。
    fn open_priority_dialog(self: &Rc<Self>) {
        let Some(p) = self.selected_process() else {
            self.toast_msg("先选一个进程", true);
            return;
        };
        let spin = gtk::SpinButton::with_range(-20.0, 19.0, 1.0);
        spin.set_value(p.nice as f64);
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let l = gtk::Label::new(Some("-20 最高（需要 root）… 19 最低。默认 0。"));
        l.add_css_class("caption");
        l.add_css_class("dim-label");
        l.set_xalign(0.0);
        box_.append(&l);
        box_.append(&spin);
        let dialog = adw::MessageDialog::new(
            self.window().as_ref(),
            Some(&format!("调整 {} 的优先级", p.name)),
            Some(&format!("当前 nice = {}", p.nice)),
        );
        dialog.set_extra_child(Some(&box_));
        dialog.add_response("cancel", "取消");
        dialog.add_response("ok", "应用");
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        dialog.set_close_response("cancel");
        let pid = p.pid;
        let pname = p.name.clone();
        dialog.connect_response(
            None,
            clone!(
                #[weak(rename_to = tab)]
                self,
                move |dlg, resp| {
                    let nice = spin.value() as i32;
                    dlg.close();
                    if resp != "ok" {
                        return;
                    }
                    match msig::set_nice(pid, nice) {
                        Ok(()) => tab.toast_msg(&format!("{pname} 的 nice 已设为 {nice}"), false),
                        Err(e) => tab.toast_msg(&format!("设置优先级失败：{e}"), true),
                    }
                }
            ),
        );
        dialog.present();
    }

    /// 设置 CPU 亲和性。
    fn open_affinity_dialog(self: &Rc<Self>) {
        let Some(p) = self.selected_process() else {
            self.toast_msg("先选一个进程", true);
            return;
        };
        let total = std::thread::available_parallelism()
            .map(|v| v.get())
            .unwrap_or(8);
        let cur = mproc::affinity_cpus(&p.affinity);
        let flow = gtk::FlowBox::new();
        flow.set_selection_mode(gtk::SelectionMode::None);
        flow.set_max_children_per_line(8);
        let toggles: Rc<RefCell<Vec<(usize, gtk::CheckButton)>>> =
            Rc::new(RefCell::new(Vec::new()));
        for cpu in 0..total {
            let cb = gtk::CheckButton::with_label(&cpu.to_string());
            cb.set_active(cur.contains(&cpu));
            toggles.borrow_mut().push((cpu, cb.clone()));
            flow.append(&cb);
        }
        let all_btn = gtk::Button::with_label("全选");
        let none_btn = gtk::Button::with_label("全不选");
        all_btn.connect_clicked(clone!(
            #[strong]
            toggles,
            move |_| {
                for (_, cb) in toggles.borrow().iter() {
                    cb.set_active(true);
                }
            }
        ));
        none_btn.connect_clicked(clone!(
            #[strong]
            toggles,
            move |_| {
                for (_, cb) in toggles.borrow().iter() {
                    cb.set_active(false);
                }
            }
        ));
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&all_btn);
        row.append(&none_btn);
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);
        box_.append(&row);
        box_.append(&flow);
        let dialog = adw::MessageDialog::new(
            self.window().as_ref(),
            Some(&format!("设置 {} 的 CPU 亲和性", p.name)),
            Some(&format!(
                "当前允许 {} / {} 个 CPU：{}",
                cur.len(),
                total,
                cur.iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )),
        );
        dialog.set_extra_child(Some(&box_));
        dialog.add_response("cancel", "取消");
        dialog.add_response("ok", "应用");
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        dialog.set_close_response("cancel");
        let pid = p.pid;
        let pname = p.name.clone();
        dialog.connect_response(
            None,
            clone!(
                #[weak(rename_to = tab)]
                self,
                #[strong]
                toggles,
                move |dlg, resp| {
                    let cpus: Vec<usize> = toggles
                        .borrow()
                        .iter()
                        .filter(|(_, cb)| cb.is_active())
                        .map(|(c, _)| *c)
                        .collect();
                    dlg.close();
                    if resp != "ok" {
                        return;
                    }
                    match msig::set_affinity(pid, &cpus) {
                        Ok(()) => tab.toast_msg(
                            &format!("{pname} 现在允许在 {} 个 CPU 上运行", cpus.len()),
                            false,
                        ),
                        Err(e) => tab.toast_msg(&format!("设置亲和性失败：{e}"), true),
                    }
                }
            ),
        );
        dialog.present();
    }

    /// 设置 IO 调度类 / 优先级。
    fn open_ioprio_dialog(self: &Rc<Self>) {
        let Some(p) = self.selected_process() else {
            self.toast_msg("先选一个进程", true);
            return;
        };
        let names: Vec<&str> = msig::IoClass::ALL.iter().map(|c| c.label()).collect();
        let dd = gtk::DropDown::new(Some(gtk::StringList::new(&names)), None::<gtk::Expression>);
        let cur = msig::get_io_priority(p.pid);
        dd.set_selected(
            cur.map(|(c, _)| msig::IoClass::ALL.iter().position(|x| *x == c).unwrap_or(0) as u32)
                .unwrap_or(0),
        );
        let level = gtk::SpinButton::with_range(0.0, 7.0, 1.0);
        level.set_value(cur.map(|(_, l)| l as f64).unwrap_or(4.0));
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let l1 = gtk::Label::new(Some("调度类"));
        l1.set_xalign(0.0);
        box_.append(&l1);
        box_.append(&dd);
        let l2 = gtk::Label::new(Some("优先级档位（0 最高，仅 best-effort / realtime 有效）"));
        l2.set_xalign(0.0);
        l2.add_css_class("caption");
        l2.add_css_class("dim-label");
        box_.append(&l2);
        box_.append(&level);
        let dialog = adw::MessageDialog::new(
            self.window().as_ref(),
            Some(&format!("设置 {} 的 IO 优先级", p.name)),
            Some("realtime 需要 root；idle 适合后台备份 / 索引类进程"),
        );
        dialog.set_extra_child(Some(&box_));
        dialog.add_response("cancel", "取消");
        dialog.add_response("ok", "应用");
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        dialog.set_close_response("cancel");
        let pid = p.pid;
        let pname = p.name.clone();
        dialog.connect_response(
            None,
            clone!(
                #[weak(rename_to = tab)]
                self,
                move |dlg, resp| {
                    let class = msig::IoClass::ALL
                        .get(dd.selected() as usize)
                        .copied()
                        .unwrap_or(msig::IoClass::None);
                    let lv = level.value() as u8;
                    dlg.close();
                    if resp != "ok" {
                        return;
                    }
                    match msig::set_io_priority(pid, class, lv) {
                        Ok(()) => tab.toast_msg(
                            &format!("{pname} 的 IO 调度已设为 {}", class.short()),
                            false,
                        ),
                        Err(e) => tab.toast_msg(&format!("设置 IO 优先级失败：{e}"), true),
                    }
                }
            ),
        );
        dialog.present();
    }

    /// 结束整棵进程树（先子后父）。
    fn kill_tree(self: &Rc<Self>, sig: i32) {
        let Some(p) = self.selected_process() else {
            self.toast_msg("先选一个进程", true);
            return;
        };
        let names: Vec<String>;
        let n: usize;
        {
            let procs = self.procs.borrow();
            let tree = mproc::descendants_of(&procs, p.pid);
            n = tree.len();
            if n <= 1 {
                drop(procs);
                self.send_now(
                    p.pid,
                    &p.name,
                    sig,
                    if sig == 9 { "SIGKILL" } else { "SIGTERM" },
                    "结束进程",
                );
                return;
            }
            let mut v: Vec<String> = procs
                .iter()
                .filter(|x| x.pid != p.pid && tree.contains(&x.pid))
                .map(|x| format!("{} ({})", x.display_name(), x.pid))
                .collect();
            v.sort();
            v.truncate(20);
            names = v;
        }
        let dialog = adw::MessageDialog::new(
            self.window().as_ref(),
            Some(&format!("结束 {} 及其 {} 个子进程？", p.name, n - 1)),
            Some(&format!(
                "将发送 {}（先子后父，避免留下孤儿）\n\n{}{}",
                if sig == 9 { "SIGKILL" } else { "SIGTERM" },
                names.join("\n"),
                if n - 1 > names.len() {
                    format!("\n… 还有 {} 个", n - 1 - names.len())
                } else {
                    String::new()
                }
            )),
        );
        dialog.add_response("cancel", "取消");
        dialog.add_response("ok", "全部结束");
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
        dialog.set_close_response("cancel");
        let pid = p.pid;
        dialog.connect_response(
            None,
            clone!(
                #[weak(rename_to = tab)]
                self,
                move |dlg, resp| {
                    dlg.close();
                    if resp != "ok" {
                        return;
                    }
                    let procs = tab.procs.borrow();
                    let (ok, errs) = msig::send_signal_tree(&procs, pid, sig);
                    drop(procs);
                    if errs.is_empty() {
                        tab.toast_msg(&format!("已向 {ok} 个进程发送信号"), false);
                    } else {
                        tab.toast_msg(
                            &format!("{ok} 个成功，{} 个失败（{}）", errs.len(), errs[0]),
                            true,
                        );
                    }
                }
            ),
        );
        dialog.present();
    }

    /// 复制选中进程的全部信息。
    fn copy_process_info(self: &Rc<Self>) {
        let Some(p) = self.selected_process() else {
            self.toast_msg("先选一个进程", true);
            return;
        };
        let d = self.detail.borrow();
        let mut s = String::new();
        s.push_str(&format!("PID      {}\n名称     {}\n", p.pid, p.name));
        s.push_str(&format!("命令行   {}\n", p.cmdline));
        s.push_str(&format!("用户     {} ({})\n", p.user, p.uid));
        s.push_str(&format!(
            "状态     {} · CPU {:.1}% · 内存 {} · 线程 {} · 运行 {}\n",
            p.state,
            p.cpu,
            mon::human_bytes(p.rss),
            p.threads,
            mon::human_duration(p.uptime)
        ));
        s.push_str(&format!("cgroup   {}\n", p.cgroup));
        if let Some(x) = d.as_ref() {
            s.push_str(&format!(
                "exe      {}\ncwd      {}\nroot     {}\n亲和性   {}（{} 个核）\n",
                x.exe,
                x.cwd,
                x.root,
                x.affinity,
                mproc::affinity_cpus(&x.affinity).len()
            ));
            s.push_str(&format!(
                "环境变量 {} 条 · 打开文件 {} 个 · 线程 {} 个\n",
                x.env.len(),
                x.fds.len(),
                x.thread_list.len()
            ));
            s.push_str("\n=== /proc/<pid>/status ===\n");
            s.push_str(&x.status_text);
        }
        if let Some(display) = gtk::gdk::Display::default() {
            display.clipboard().set_text(&s);
            self.toast_msg("进程信息已复制到剪贴板", false);
        } else {
            self.toast_msg("拿不到剪贴板", true);
        }
    }
}

/// 详情字段取值（带兜底文案）。
fn detail_str<'a>(
    d: &'a Option<ProcessDetail>,
    f: impl Fn(&'a ProcessDetail) -> &'a String,
    fallback: &'a str,
) -> &'a str {
    match d.as_ref() {
        Some(x) => {
            let v = f(x);
            if v.is_empty() { fallback } else { v }
        }
        None => "读取中…",
    }
}

/// 单元格文本。
fn cell_text(p: &Process, col: ProcColumn, collapsed: &HashSet<i32>) -> String {
    match col {
        ProcColumn::Name => {
            let mark = if p.child_count > 0 {
                if collapsed.contains(&p.pid) {
                    "▸ "
                } else {
                    "▾ "
                }
            } else {
                ""
            };
            format!("{mark}{}", p.display_name())
        }
        ProcColumn::Pid => p.pid.to_string(),
        ProcColumn::User => p.user.clone(),
        ProcColumn::State => format!("{} ({})", p.state, p.state_char),
        ProcColumn::Cpu => format!("{:.1}", p.cpu),
        ProcColumn::Mem => mon::human_bytes(p.rss),
        ProcColumn::MemPct => format!("{:.1}", p.mem_pct),
        ProcColumn::Threads => p.threads.to_string(),
        ProcColumn::Priority => p.priority.to_string(),
        ProcColumn::Nice => p.nice.to_string(),
        ProcColumn::Policy => p.policy.clone(),
        ProcColumn::Io => {
            if p.io_class.is_empty() {
                "—".to_string()
            } else {
                p.io_class.clone()
            }
        }
        ProcColumn::ReadRate => mon::human_rate(p.read_rate),
        ProcColumn::WriteRate => mon::human_rate(p.write_rate),
        ProcColumn::CpuTime => mon::human_duration(p.cpu_time as u64),
        ProcColumn::Started => mon::human_duration(p.uptime),
        ProcColumn::Command => {
            if p.cmdline.is_empty() {
                "（内核线程）".to_string()
            } else {
                p.cmdline.clone()
            }
        }
        ProcColumn::Cgroup => {
            let app = mon::cgroup_app(&p.cgroup);
            if app.is_empty() {
                "—".to_string()
            } else {
                app
            }
        }
    }
}

fn cell_tooltip(p: &Process, col: ProcColumn) -> String {
    match col {
        ProcColumn::Name => format!(
            "{}\nPID {} · PPID {}\n命令行：{}",
            p.display_name(),
            p.pid,
            p.ppid,
            if p.cmdline.is_empty() {
                "（内核线程）"
            } else {
                &p.cmdline
            }
        ),
        ProcColumn::Command => p.cmdline.clone(),
        ProcColumn::Cgroup => p.cgroup.clone(),
        ProcColumn::Mem => format!("{}（{} 页）", mon::human_bytes(p.rss), p.rss / 4096),
        ProcColumn::Cpu => format!(
            "{:.2}%（100% = 一个核心跑满，多线程会超过 100%）\nCPU 时间 {}",
            p.cpu,
            mon::human_duration(p.cpu_time as u64)
        ),
        ProcColumn::Started => format!("启动于 {}", mon::human_time(p.started_at)),
        _ => cell_text(p, col, &HashSet::new()),
    }
}

/// 详情面板：返回 (概况, 内存, 线程, 文件, 环境, 命名空间, 容器)。
#[allow(clippy::type_complexity)]
fn build_detail() -> (
    gtk::Label,
    gtk::Label,
    gtk::Label,
    gtk::Label,
    gtk::Label,
    gtk::Label,
    gtk::Box,
) {
    let nb = gtk::Notebook::new();
    nb.set_scrollable(true);
    let mk = |text: &str| {
        let l = gtk::Label::new(Some(text));
        l.set_xalign(0.0);
        l.set_yalign(0.0);
        l.set_selectable(true);
        // 右侧详情面板只有 300px 宽：命令行 / 路径这类长文本必须换行，
        // 否则标签的自然宽度会把面板顶宽、内容溢出到窗口外面（还会多出横向滚动条）。
        // 用 WordChar：优先在空格处断行（键值对不会从中间被切成两半），
        // 遇到长路径这种没有空格的内容再按字符断。
        l.set_wrap(true);
        l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        l.add_css_class("monospace");
        l.add_css_class("caption");
        l
    };
    let d_overview = mk("在上方表格里选一个进程查看详情");
    let d_mem = mk("");
    let d_threads = mk("");
    let d_fds = mk("");
    let d_env = mk("");
    let d_ns = mk("");
    for (title, label) in [
        ("概况", &d_overview),
        ("内存", &d_mem),
        ("线程", &d_threads),
        ("打开的文件", &d_fds),
        ("环境变量", &d_env),
        ("命名空间", &d_ns),
    ] {
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        scroll.set_child(Some(label));
        nb.append_page(&scroll, Some(&gtk::Label::new(Some(title))));
    }
    let box_ = gtk::Box::new(gtk::Orientation::Vertical, 0);
    // 右侧详情面板：固定窄宽（高度跟随窗口）
    box_.set_size_request(300, -1);
    box_.set_vexpand(true);
    // 竖向 Box 里没设 vexpand 的子控件只拿自然高度，剩下的空间就留成一大块空白：
    // 让 Notebook（含各页的滚动区）吃掉整个面板高度。
    nb.set_vexpand(true);
    box_.append(&nb);
    (d_overview, d_mem, d_threads, d_fds, d_env, d_ns, box_)
}

/// 「更多操作」浮层。
fn build_more_popover(tab: &Rc<ProcsTab>) -> gtk::Popover {
    let pop = gtk::Popover::new();
    let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
    b.set_margin_top(6);
    b.set_margin_bottom(6);
    b.set_margin_start(6);
    b.set_margin_end(6);
    let add = |label: &str, f: Box<dyn Fn()>| {
        let btn = gtk::Button::with_label(label);
        btn.add_css_class("flat");
        btn.set_halign(gtk::Align::Fill);
        let pop2 = pop.clone();
        btn.connect_clicked(move |_| {
            pop2.popdown();
            f();
        });
        b.append(&btn);
    };
    add("调整优先级 (nice)…", {
        let t = tab.clone();
        Box::new(move || t.open_priority_dialog())
    });
    add("设置 CPU 亲和性…", {
        let t = tab.clone();
        Box::new(move || t.open_affinity_dialog())
    });
    add("设置 IO 优先级…", {
        let t = tab.clone();
        Box::new(move || t.open_ioprio_dialog())
    });
    b.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    add("结束整棵进程树 (SIGTERM)", {
        let t = tab.clone();
        Box::new(move || t.kill_tree(15))
    });
    add("强制结束整棵进程树 (SIGKILL)", {
        let t = tab.clone();
        Box::new(move || t.kill_tree(9))
    });
    b.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    add("复制进程全部信息", {
        let t = tab.clone();
        Box::new(move || t.copy_process_info())
    });
    pop.set_child(Some(&b));
    pop
}
