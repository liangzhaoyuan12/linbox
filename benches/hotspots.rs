//! 热点基准测试（GOAL.md Phase 1.2 / 1.3）。
//!
//! 运n行：`cargo bench --bench hotspots`
//!
//! 覆盖 GOAL.md 列出的第一批热点：/proc 解析、进程树扁平化、过滤、
//! JSON 往返、rc 文件解析/序列化、路径扫描 URL 拼接、真实 /proc 全量采集。

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashSet;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// ---- 分配计数器（GOAL.md 2.2 验证：collect 分配次数前后对比）----
static ALLOC_N: AtomicU64 = AtomicU64::new(0);
static ALLOC_B: AtomicU64 = AtomicU64::new(0);

struct CountingAlloc;

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOC_N.fetch_add(1, Ordering::Relaxed);
        ALLOC_B.fetch_add(l.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        ALLOC_N.fetch_add(1, Ordering::Relaxed);
        ALLOC_B.fetch_add(l.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_l: usize) -> *mut u8 {
        ALLOC_N.fetch_add(1, Ordering::Relaxed);
        ALLOC_B.fetch_add(new_l as u64, Ordering::Relaxed);
        unsafe { System.realloc(p, l, new_l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}

/// 采集 20 次的分配次数/字节数（打印到 stderr，进 bench 日志）。
fn report_collect_allocs() {
    let mut s = monitor::BenchSampler::new();
    s.collect_timed(); // 预热（首轮含冷缓存，不计入）
    let n0 = ALLOC_N.load(Ordering::Relaxed);
    let b0 = ALLOC_B.load(Ordering::Relaxed);
    for _ in 0..20 {
        s.collect_timed();
    }
    let n = ALLOC_N.load(Ordering::Relaxed) - n0;
    let b = ALLOC_B.load(Ordering::Relaxed) - b0;
    eprintln!(
        "[alloc] collect×20: {n} allocs (avg {} /次), {b} bytes",
        n / 20
    );

    // 归因：进程循环段单独计数（collect_processes 是 pub，可直接驱动）
    let users = monitor::proc::uid_names();
    let mut caches: std::collections::HashMap<i32, monitor::proc::PidCache> =
        std::collections::HashMap::new();
    let prev: std::collections::HashMap<i32, (u64, u64, u64)> = std::collections::HashMap::new();
    // 预热一次（填缓存）
    let warm = monitor::proc::collect_processes(
        &prev,
        100,
        1.0,
        8 * 1024 * 1024 * 1024,
        0,
        1000,
        &users,
        &mut caches,
        false,
        false,
    );
    drop(warm);
    let n0 = ALLOC_N.load(Ordering::Relaxed);
    let b0 = ALLOC_B.load(Ordering::Relaxed);
    for _ in 0..20 {
        let v = monitor::proc::collect_processes(
            &prev,
            100,
            1.0,
            8 * 1024 * 1024 * 1024,
            0,
            1000,
            &users,
            &mut caches,
            false,
            false,
        );
        drop(v);
    }
    let n = ALLOC_N.load(Ordering::Relaxed) - n0;
    let b = ALLOC_B.load(Ordering::Relaxed) - b0;
    eprintln!(
        "[alloc] collect_processes×20: {n} allocs (avg {} /次), {b} bytes",
        n / 20
    );
}

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

use linbox::model::env_editor::ShellKind;
use linbox::model::monitor::{Process, TreeMode};
use linbox::utils::env_editor as env;
use linbox::utils::json;
use linbox::utils::monitor;
use linbox::utils::monitor::proc;
use linbox::utils::path_scanner::scan;

/// 合成 n 个进程：浅层树形（每 7 个挂一个父），字段带真实感的分布。
fn make_procs(n: usize) -> Vec<Process> {
    (0..n)
        .map(|i| {
            let pid = (i + 2) as i32;
            let mut ppid = ((i / 7) + 2) as i32;
            if ppid == pid {
                ppid = 1;
            }
            Process {
                pid,
                ppid,
                name: format!("worker-{i}"),
                cmdline: format!("/usr/bin/worker-{i} --jobs {} --log level{}", i % 16, i % 3),
                uid: (i % 1000) as u32,
                user: format!("user{}", i % 7),
                state: if i % 50 == 0 { "R" } else { "S" }.to_string(),
                cpu: ((i * 37) % 1000) as f32 / 10.0,
                mem_pct: ((i * 53) % 1000) as f32 / 10.0,
                rss: (i as u64 % 4_000_000) * 4096,
                cgroup: format!("/user.slice/user-{}.slice/session-{}.scope", i % 3, i % 5),
                ..Process::default()
            }
        })
        .collect()
}

/// ~1MB 的 JSON 文本。
fn make_big_json() -> String {
    let mut s = String::with_capacity(1_200_000);
    s.push_str("{\"items\":[");
    for i in 0..12_000 {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"id\":{i},\"name\":\"item-{i}\",\"active\":{},\"score\":{},\"tags\":[\"alpha\",\"beta\",\"gamma\"]}}",
            i % 2 == 0,
            i as f64 * 1.5
        ));
    }
    s.push_str("]}");
    s
}

/// 典型 bashrc 样本（env_editor 解析/序列化往返）。
const BASHRC: &str = r#"
# ~/.bashrc: executed by bash(1) for non-login shells.
export PS1="\u@\h:\w\$ "
alias ll='ls -alF'
alias la='ls -A'
alias l='ls -CF'

export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
export EDITOR=nano
export LANG=en_US.UTF-8

if [ -f ~/.bash_aliases ]; then
    . ~/.bash_aliases
fi

export HISTSIZE=10000
export HISTCONTROL=ignoredups:ignorespace
export FZF_DEFAULT_COMMAND='fd --type f'

# 函数
mkcd() {
    mkdir -p "$1" && cd "$1"
}

parse_git_branch() {
    git branch 2>/dev/null | sed -n 's/* //p'
}

export GREP_OPTIONS="--color=auto"
stty -ixon
shopt -s histappend
"#;

fn bench_proc_parse(c: &mut Criterion) {
    // 真实 /proc 文本（本机采样），形状与生产一致
    let stat_text = std::fs::read_to_string("/proc/stat").expect("读 /proc/stat");
    c.bench_function("proc/parse_stat", |b| {
        b.iter(|| proc::parse_stat(black_box(&stat_text)))
    });

    let pid_stat = std::fs::read_to_string("/proc/self/stat").expect("读 /proc/self/stat");
    c.bench_function("proc/parse_pid_stat", |b| {
        b.iter(|| proc::parse_pid_stat(black_box(&pid_stat)))
    });

    let io_text = std::fs::read_to_string("/proc/self/io").expect("读 /proc/self/io");
    c.bench_function("proc/parse_pid_io", |b| {
        b.iter(|| proc::parse_pid_io(black_box(&io_text)))
    });
}

fn bench_proc_tree(c: &mut Criterion) {
    let collapsed = HashSet::new();
    let cmp = |a: &Process, b: &Process| b.cpu.total_cmp(&a.cpu);

    c.bench_function("proc/flatten_tree_2000/tree", |b| {
        b.iter_batched(
            || make_procs(2000),
            |p| proc::flatten_tree(p, TreeMode::Tree, &collapsed, &cmp),
            BatchSize::LargeInput,
        )
    });
    c.bench_function("proc/flatten_tree_2000/flat", |b| {
        b.iter_batched(
            || make_procs(2000),
            |p| proc::flatten_tree(p, TreeMode::Flat, &collapsed, &cmp),
            BatchSize::LargeInput,
        )
    });

    // UI 每帧对整个列表跑一遍过滤
    let procs = make_procs(2000);
    c.bench_function("proc/matches_filter_2000", |b| {
        b.iter(|| {
            procs
                .iter()
                .filter(|p| proc::matches_filter(p, black_box("worker-1"), None, true))
                .count()
        })
    });
}

fn bench_json(c: &mut Criterion) {
    let big = make_big_json();
    assert!(
        big.len() >= 1_000_000,
        "JSON 样本应 ≥1MB，实际 {}",
        big.len()
    );
    c.bench_function("json/parse_1mb", |b| {
        b.iter(|| json::parse(black_box(&big)).expect("样本必须是合法 JSON"))
    });
    let value = json::parse(&big).expect("样本必须是合法 JSON");
    c.bench_function("json/format_pretty_1mb", |b| {
        b.iter(|| json::format_pretty(black_box(&value)))
    });
    c.bench_function("json/format_compact_1mb", |b| {
        b.iter(|| json::format_compact(black_box(&value)))
    });
}

fn bench_env_editor(c: &mut Criterion) {
    let shell = ShellKind::Bash;
    c.bench_function("env_editor/parse_bashrc", |b| {
        b.iter(|| env::parse(black_box(BASHRC), &shell))
    });
    let lines = env::parse(BASHRC, &shell);
    c.bench_function("env_editor/serialize_bashrc", |b| {
        b.iter(|| env::serialize(black_box(&lines), &shell))
    });
}

fn bench_path_scanner(c: &mut Criterion) {
    let base = "https://example.com/api/v1/";
    let paths = ["/admin/login", "metrics", "  ./health  ", "/", ""];
    c.bench_function("path_scanner/join_url", |b| {
        b.iter(|| {
            paths
                .iter()
                .map(|p| scan::join_url(black_box(base), black_box(p)))
                .collect::<Vec<_>>()
        })
    });
}

fn bench_monitor_collect(c: &mut Criterion) {
    // 真实 /proc 全量采集（GOAL.md 1.4：单次采样耗时基线）。
    // 自计时：iter_custom 只累计 collect_timed 的纯采集时间。
    let mut sampler = monitor::BenchSampler::new();
    sampler.collect_timed(); // 预热（首次含 /proc 冷缓存与静态信息初始化）
    c.bench_function("monitor/collect_real_proc", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += sampler.collect_timed();
            }
            total
        })
    });

    // 分层采样打开速率列后的采集成本（与上面的「默认无选中无速率列」对照）
    monitor::set_io_columns(true);
    let mut sampler_io = monitor::BenchSampler::new();
    sampler_io.collect_timed();
    c.bench_function("monitor/collect_real_proc_io_on", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += sampler_io.collect_timed();
            }
            total
        })
    });
    monitor::set_io_columns(false);
}

fn benches(c: &mut Criterion) {
    report_collect_allocs();
    bench_proc_parse(c);
    bench_proc_tree(c);
    bench_json(c);
    bench_env_editor(c);
    bench_path_scanner(c);
    bench_monitor_collect(c);
}

criterion_group!(hotspots, benches);
criterion_main!(hotspots);
