//! 迷你历史曲线控件（展示层 · 可复用复合控件）。
//!
//! 用一个 `gtk::DrawingArea` + cairo 画滚动折线图：单曲线（CPU/内存/显卡）或
//! 双曲线（网络收发）。数据由调用方推进来，控件自己只保留最近 `capacity` 个点。
//!
//! 颜色用固定的 RGBA（浅色/深色主题下都能看清），不依赖主题取色 API。
//! 用法：
//! ```ignore
//! let g = Graph::new(120, 64, Graph::BLUE, true);
//! box.append(g.widget());
//! g.push(12.5);          // 推入一个采样点（会自动重绘）
//! ```

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use gtk::prelude::*;

/// 曲线颜色（r, g, b，0..1）。
pub type Color = (f64, f64, f64);

#[derive(Clone)]
pub struct Graph {
    area: gtk::DrawingArea,
    st: Rc<RefCell<State>>,
}

struct State {
    hist: VecDeque<f64>,
    hist2: VecDeque<f64>,
    capacity: usize,
    /// 固定上限；None 表示按数据自适应
    fixed_max: Option<f64>,
    /// 自适应时的上限（如百分比封顶 100）
    cap: Option<f64>,
    /// 自适应时的下限（避免小波动被放大成满屏噪声）
    floor_max: f64,
    c1: Color,
    c2: Option<Color>,
    percent: bool,
    /// 左上角显示的文字（如单位）
    unit: String,
}

impl Graph {
    pub const BLUE: Color = (0.208, 0.518, 0.894);
    pub const GREEN: Color = (0.204, 0.824, 0.478);
    pub const YELLOW: Color = (0.961, 0.761, 0.067);
    pub const RED: Color = (0.878, 0.271, 0.239);
    pub const PURPLE: Color = (0.569, 0.255, 0.675);
    pub const TEAL: Color = (0.180, 0.757, 0.741);

    /// 单曲线图。`percent` 为 true 时 y 轴按 0..100 处理。
    pub fn new(capacity: usize, height: i32, color: Color, percent: bool) -> Graph {
        Self::build(
            capacity,
            height,
            color,
            None,
            percent,
            if percent { "%" } else { "" },
        )
    }

    /// 双曲线图（如网络接收 / 发送）。
    pub fn new_two(capacity: usize, height: i32, c1: Color, c2: Color, unit: &str) -> Graph {
        Self::build(capacity, height, c1, Some(c2), false, unit)
    }

    fn build(
        capacity: usize,
        height: i32,
        c1: Color,
        c2: Option<Color>,
        percent: bool,
        unit: &str,
    ) -> Graph {
        let area = gtk::DrawingArea::new();
        area.set_content_height(height);
        area.set_hexpand(true);
        area.set_vexpand(false);
        let st = Rc::new(RefCell::new(State {
            hist: VecDeque::with_capacity(capacity),
            hist2: VecDeque::with_capacity(capacity),
            capacity: capacity.max(2),
            fixed_max: if percent { Some(100.0) } else { None },
            cap: Some(100.0).filter(|_| percent),
            floor_max: if percent { 100.0 } else { 1024.0 },
            c1,
            c2,
            percent,
            unit: unit.to_string(),
        }));
        let st2 = st.clone();
        area.set_draw_func(move |_, ctx, w, h| draw(&st2.borrow(), ctx, w, h));
        Graph { area, st }
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    /// 推入第一路数据。
    pub fn push(&self, v: f64) {
        let mut st = self.st.borrow_mut();
        let cap = st.capacity;
        push_hist(&mut st.hist, cap, v);
        drop(st);
        self.area.queue_draw();
    }

    /// 推入双路数据（单曲线图只用第一路）。
    pub fn push_pair(&self, a: f64, b: f64) {
        let mut st = self.st.borrow_mut();
        let cap = st.capacity;
        let two = st.c2.is_some();
        push_hist(&mut st.hist, cap, a);
        if two {
            push_hist(&mut st.hist2, cap, b);
        }
        drop(st);
        self.area.queue_draw();
    }

    /// 设置/取消固定上限（`None` = 自适应）。
    pub fn set_max(&self, v: Option<f64>) {
        self.st.borrow_mut().fixed_max = v;
        self.area.queue_draw();
    }

    /// 设置自适应模式的下限（例如网络速率至少按 1 MB/s 缩放，免得小流量刷屏）。
    pub fn set_floor(&self, v: f64) {
        self.st.borrow_mut().floor_max = v;
    }

    /// 改成「自适应缩放」：`floor` 是显示下限（小波动不会被放大成满屏），
    /// `cap` 是上限（如百分比封顶 100）。低占用时才看得见曲线。
    pub fn set_auto(&self, floor: f64, cap: Option<f64>) {
        let mut st = self.st.borrow_mut();
        st.fixed_max = None;
        st.floor_max = floor;
        st.cap = cap;
        drop(st);
        self.area.queue_draw();
    }

    /// 清空历史（暂停 / 重置时调用）。
    pub fn clear(&self) {
        let mut st = self.st.borrow_mut();
        st.hist.clear();
        st.hist2.clear();
        drop(st);
        self.area.queue_draw();
    }

    /// 当前是否还没有数据。
    pub fn is_empty(&self) -> bool {
        self.st.borrow().hist.is_empty()
    }
}

fn push_hist(hist: &mut VecDeque<f64>, cap: usize, v: f64) {
    if hist.len() >= cap {
        hist.pop_front();
    }
    hist.push_back(if v.is_finite() { v.max(0.0) } else { 0.0 });
}

/// 自适应上限：数据最大值上浮 15%，且不低于 `floor_max`，并向上取整到好看的刻度。
fn auto_max(st: &State) -> f64 {
    let m = st
        .hist
        .iter()
        .chain(st.hist2.iter())
        .cloned()
        .fold(0.0f64, f64::max);
    let m = m * 1.15;
    let m = if m < st.floor_max { st.floor_max } else { m };
    let m = match st.cap {
        Some(c) if m > c => c,
        _ => m,
    };
    // 取整到 1/2/5 * 10^n，让刻度线看起来稳定
    let mag = 10f64.powf(m.log10().floor());
    let n = m / mag;
    let step = if n <= 1.0 {
        1.0
    } else if n <= 2.0 {
        2.0
    } else if n <= 5.0 {
        5.0
    } else {
        10.0
    };
    step * mag
}

fn draw(st: &State, ctx: &gtk::cairo::Context, w: i32, h: i32) {
    let (w, h) = (w as f64, h as f64);
    if w <= 2.0 || h <= 2.0 {
        return;
    }
    // 背景
    ctx.set_source_rgba(0.5, 0.5, 0.5, 0.10);
    rounded_rect(ctx, 0.0, 0.0, w, h, 6.0);
    let _ = ctx.fill();

    let max = st.fixed_max.unwrap_or_else(|| auto_max(st));
    let max = if max <= 0.0 { 1.0 } else { max };

    // 网格线（25% / 50% / 75%）
    ctx.set_source_rgba(0.5, 0.5, 0.5, 0.25);
    ctx.set_line_width(1.0);
    for i in 1..4 {
        let y = h * (i as f64) / 4.0;
        ctx.move_to(0.0, y.floor() + 0.5);
        ctx.line_to(w, y.floor() + 0.5);
        let _ = ctx.stroke();
    }

    // 曲线：从右往左铺，最新点在右边
    let n = st.hist.len();
    let capacity = st.capacity;
    let x_at = |i: usize| -> f64 {
        let offset = capacity.saturating_sub(n) as f64;
        ((offset + i as f64) / (capacity as f64 - 1.0).max(1.0)) * w
    };
    let y_at = |v: f64| h - (v / max).clamp(0.0, 1.0) * h;

    let series: [(&VecDeque<f64>, Color, bool); 2] = [
        (&st.hist, st.c1, true),
        (&st.hist2, st.c2.unwrap_or(st.c1), false),
    ];
    for (hist, color, primary) in series {
        if hist.is_empty() || (!primary && st.c2.is_none()) {
            continue;
        }
        // 填充
        if primary {
            ctx.set_source_rgba(color.0, color.1, color.2, 0.22);
            ctx.move_to(x_at(0), h);
            for (i, v) in hist.iter().enumerate() {
                ctx.line_to(x_at(i), y_at(*v));
            }
            ctx.line_to(x_at(hist.len() - 1), h);
            ctx.close_path();
            let _ = ctx.fill();
        }
        // 描边
        ctx.set_source_rgba(color.0, color.1, color.2, 0.95);
        ctx.set_line_width(1.5);
        for (i, v) in hist.iter().enumerate() {
            let (x, y) = (x_at(i), y_at(*v));
            if i == 0 {
                ctx.move_to(x, y);
            } else {
                ctx.line_to(x, y);
            }
        }
        let _ = ctx.stroke();
    }

    // 上限文字
    let txt = if st.percent {
        format!("{:.0}{}", max, st.unit)
    } else if st.unit.is_empty() {
        format!("{:.0}", max)
    } else {
        format!("{:.1}{}", max, st.unit)
    };
    ctx.set_source_rgba(0.5, 0.5, 0.5, 0.85);
    ctx.set_font_size(10.0);
    ctx.move_to(5.0, 12.0);
    let _ = ctx.show_text(&txt);
}

fn rounded_rect(ctx: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    ctx.new_sub_path();
    ctx.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    ctx.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    ctx.arc(
        x + r,
        y + h - r,
        r,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    ctx.arc(
        x + r,
        y + r,
        r,
        std::f64::consts::PI,
        3.0 * std::f64::consts::FRAC_PI_2,
    );
    ctx.close_path();
}
