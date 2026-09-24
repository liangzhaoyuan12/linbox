//! 网络标签页：每张网卡的状态、速率曲线、累计流量、地址与错误。

use std::cell::RefCell;
use std::collections::HashMap;

use gtk::prelude::*;

use super::{card, card_title, kv_row, value_label};
use crate::model::monitor::Snapshot;
use crate::utils::monitor as mon;
use crate::widgets::graph::Graph;

pub struct NetView {
    root: gtk::Box,
    summary: gtk::Label,
    cards: gtk::Box,
    slots: RefCell<HashMap<String, IfaceCard>>,
}

struct IfaceCard {
    state: gtk::Label,
    rx: gtk::Label,
    tx: gtk::Label,
    graph: Graph,
    detail: gtk::Label,
    addr: gtk::Label,
}

impl NetView {
    pub fn widget(&self) -> &impl IsA<gtk::Widget> {
        &self.root
    }

    pub fn new() -> NetView {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let (c, box_) = card();
        root.append(&c);
        box_.append(&card_title("网络接口"));
        let summary = gtk::Label::new(Some("正在读取 /proc/net/dev…"));
        summary.set_xalign(0.0);
        summary.add_css_class("caption");
        summary.add_css_class("dim-label");
        box_.append(&summary);
        let cards = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&cards);
        NetView {
            root,
            summary,
            cards,
            slots: RefCell::new(HashMap::new()),
        }
    }

    pub fn update(&self, s: &Snapshot) {
        let mut slots = self.slots.borrow_mut();
        let need_rebuild =
            slots.len() != s.net.len() || s.net.iter().any(|i| !slots.contains_key(&i.name));
        if need_rebuild {
            slots.clear();
            while let Some(child) = self.cards.first_child() {
                self.cards.remove(&child);
            }
            for i in s.net.iter() {
                let (c, box_) = card();
                let title = gtk::Label::new(Some(&i.name));
                title.add_css_class("heading");
                title.set_halign(gtk::Align::Start);
                box_.append(&title);

                let head = gtk::Box::new(gtk::Orientation::Horizontal, 12);
                let rx = gtk::Label::new(None);
                rx.add_css_class("title-1");
                let tx = gtk::Label::new(None);
                tx.add_css_class("title-1");
                let col1 = gtk::Box::new(gtk::Orientation::Vertical, 0);
                let l1 = gtk::Label::new(Some("接收"));
                l1.add_css_class("caption");
                l1.add_css_class("dim-label");
                l1.set_xalign(0.0);
                col1.append(&l1);
                col1.append(&rx);
                let col2 = gtk::Box::new(gtk::Orientation::Vertical, 0);
                let l2 = gtk::Label::new(Some("发送"));
                l2.add_css_class("caption");
                l2.add_css_class("dim-label");
                l2.set_xalign(0.0);
                col2.append(&l2);
                col2.append(&tx);
                head.append(&col1);
                head.append(&col2);
                let graph = Graph::new_two(200, 64, Graph::TEAL, Graph::PURPLE, "/s");
                head.append(graph.widget());
                box_.append(&head);

                let state = value_label();
                let detail = value_label();
                let addr = value_label();
                addr.add_css_class("dim-label");
                box_.append(&kv_row("状态", &state));
                box_.append(&kv_row("累计流量", &detail));
                box_.append(&kv_row("地址 / 错误", &addr));
                self.cards.append(&c);
                slots.insert(
                    i.name.clone(),
                    IfaceCard {
                        state,
                        rx,
                        tx,
                        graph,
                        detail,
                        addr,
                    },
                );
            }
        }

        let (mut rx_all, mut tx_all) = (0.0f64, 0.0f64);
        let mut active = 0;
        for i in s.net.iter() {
            if !i.is_loopback {
                rx_all += i.rx_rate;
                tx_all += i.tx_rate;
                if i.operstate == "up" {
                    active += 1;
                }
            }
            let Some(sc) = slots.get(&i.name) else {
                continue;
            };
            sc.rx.set_text(&mon::human_rate(i.rx_rate));
            sc.tx.set_text(&mon::human_rate(i.tx_rate));
            sc.graph.push_pair(i.rx_rate, i.tx_rate);
            let link = if i.speed.is_empty() {
                if i.is_wireless() {
                    "无线".to_string()
                } else {
                    "未知速率".to_string()
                }
            } else {
                i.speed.clone()
            };
            let sig = i.signal_text();
            sc.state.set_text(&format!(
                "{} · {}{}{} · MTU {}{}",
                i.operstate,
                link,
                if sig.is_empty() {
                    String::new()
                } else {
                    format!(" · {sig}")
                },
                if i.mac.is_empty() {
                    String::new()
                } else {
                    format!(" · MAC {}", i.mac)
                },
                i.mtu,
                if i.is_loopback { " · 回环" } else { "" }
            ));
            sc.detail.set_text(&format!(
                "↓ {}（{} 包，{} 包/秒）  ↑ {}（{} 包，{} 包/秒）",
                mon::human_bytes(i.rx_bytes),
                mon::fmt_thousands(i.rx_packets),
                mon::fmt_thousands(i.rx_pps.round() as u64),
                mon::human_bytes(i.tx_bytes),
                mon::fmt_thousands(i.tx_packets),
                mon::fmt_thousands(i.tx_pps.round() as u64),
            ));
            let errs = i.errors();
            let mut a = Vec::new();
            if !i.ipv4.is_empty() {
                a.push(format!("IPv4 {}", i.ipv4.join(", ")));
            }
            if !i.ipv6.is_empty() {
                a.push(format!("IPv6 {}", i.ipv6.join(", ")));
            }
            if !errs.is_empty() {
                a.push(errs);
            }
            sc.addr.set_text(&if a.is_empty() {
                "—".to_string()
            } else {
                a.join(" · ")
            });
            sc.addr.set_tooltip_text(Some(&a.join("\n")));
        }
        self.summary.set_text(&format!(
            "{} 张网卡（{} 张在线）· 合计 ↓{} ↑{} · 回环流量已排除",
            s.net.len(),
            active,
            mon::human_rate(rx_all),
            mon::human_rate(tx_all)
        ));
    }
}
