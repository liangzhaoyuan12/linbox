//! 进程控制：发信号、调优先级、设 CPU 亲和性、设 IO 优先级。
//!
//! 纯逻辑层（不依赖 GTK）。所有系统调用都用本地 `extern "C"` 声明，不引入
//! libc 依赖，与项目其余部分的风格一致。权限不足时给出可执行的 `pkexec`
//! 提权方案。

use std::process::{Command, Stdio};

use crate::model::monitor::Process;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn setpriority(which: i32, who: u32, prio: i32) -> i32;
    fn sched_setaffinity(pid: i32, cpusetsize: usize, mask: *const u8) -> i32;
    fn syscall(num: i64, ...) -> i64;
}

/// PRIO_PROCESS
const PRIO_PROCESS: i32 = 0;

/// ioprio 系统调用号（不同架构编号不同）。
#[cfg(target_arch = "x86_64")]
const SYS_IOPRIO_SET: i64 = 251;
#[cfg(target_arch = "x86_64")]
const SYS_IOPRIO_GET: i64 = 252;
#[cfg(not(target_arch = "x86_64"))]
const SYS_IOPRIO_SET: i64 = 30;
#[cfg(not(target_arch = "x86_64"))]
const SYS_IOPRIO_GET: i64 = 31;

/// errno → 中文说明（映射常见几个就够）。
///
/// 注意：`setpriority` 想「调低 nice」时返回的是 **EACCES(13)** 而不是 EPERM(1)，
/// 两个都要当成权限不足，否则界面上只会看到英文的 "Permission denied"。
fn errno_text(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(1) | Some(13) => "权限不足：需要 root，或该进程属于其他用户".to_string(),
        Some(3) => "进程不存在（可能已退出）".to_string(),
        Some(11) => "内核暂时不接受该操作，稍后重试".to_string(),
        Some(16) => "资源忙".to_string(),
        Some(22) => "参数非法".to_string(),
        _ => e.to_string(),
    }
}

/// 给单个进程发信号。
pub fn send_signal(pid: i32, sig: i32) -> Result<(), String> {
    if pid <= 1 {
        return Err("拒绝操作：目标 pid 非法".to_string());
    }
    let r = unsafe { kill(pid, sig) };
    if r == 0 {
        Ok(())
    } else {
        Err(errno_text(&std::io::Error::last_os_error()))
    }
}

/// 给整个进程组发信号（`kill(-pgid)`）。
pub fn send_signal_group(pgid: i32, sig: i32) -> Result<(), String> {
    if pgid <= 1 {
        return Err("进程组非法".to_string());
    }
    let r = unsafe { kill(-pgid, sig) };
    if r == 0 {
        Ok(())
    } else {
        Err(errno_text(&std::io::Error::last_os_error()))
    }
}

/// 给一个进程及其所有后代发信号（先子后父，避免留下孤儿）。
///
/// 返回 `(成功数, 失败详情)`。
pub fn send_signal_tree(procs: &[Process], pid: i32, sig: i32) -> (usize, Vec<String>) {
    let mut pids = crate::utils::monitor::proc::descendants_of(procs, pid);
    // 反向（深的先发），父子关系上就是从叶子往上
    pids.reverse();
    let mut ok = 0;
    let mut errs = Vec::new();
    for p in pids {
        match send_signal(p, sig) {
            Ok(()) => ok += 1,
            Err(e) => errs.push(format!("{p}: {e}")),
        }
    }
    (ok, errs)
}

/// 通过 `pkexec` 提权给进程发信号（用于其他用户的进程 / 需要 CAP_KILL 的场景）。
pub fn pkexec_send_signal(pid: i32, sig: i32) -> Result<(), String> {
    if !(1..=64).contains(&sig) {
        return Err(format!("信号编号非法：{sig}"));
    }
    let script = format!("kill -{sig} {pid}");
    let output = Command::new("pkexec")
        .arg("sh")
        .arg("-c")
        .arg(&script)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("无法启动 pkexec（请确认已安装 polkit）：{e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "提权失败（退出码 {}）：{}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        ))
    }
}

/// 调整 nice 值（-20 最高，19 最低；调低需要 root）。
pub fn set_nice(pid: i32, nice: i32) -> Result<(), String> {
    if !(-20..=19).contains(&nice) {
        return Err("nice 必须在 -20 ~ 19 之间".to_string());
    }
    let r = unsafe { setpriority(PRIO_PROCESS, pid as u32, nice) };
    if r == 0 {
        Ok(())
    } else {
        Err(errno_text(&std::io::Error::last_os_error()))
    }
}

/// 设置 CPU 亲和性（允许运行的 CPU 列表）。
pub fn set_affinity(pid: i32, cpus: &[usize]) -> Result<(), String> {
    if cpus.is_empty() {
        return Err("至少要选择一个 CPU".to_string());
    }
    let mask = crate::utils::monitor::proc::affinity_mask(cpus);
    let r = unsafe { sched_setaffinity(pid, mask.len(), mask.as_ptr()) };
    if r == 0 {
        Ok(())
    } else {
        Err(errno_text(&std::io::Error::last_os_error()))
    }
}

/// IO 调度类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoClass {
    None,
    Realtime,
    BestEffort,
    Idle,
}

impl IoClass {
    pub fn label(&self) -> &'static str {
        match self {
            IoClass::None => "none（继承）",
            IoClass::Realtime => "realtime（实时，需 root）",
            IoClass::BestEffort => "best-effort（尽力而为）",
            IoClass::Idle => "idle（空闲时才做）",
        }
    }

    pub fn short(&self) -> &'static str {
        match self {
            IoClass::None => "none",
            IoClass::Realtime => "realtime",
            IoClass::BestEffort => "best-effort",
            IoClass::Idle => "idle",
        }
    }

    pub fn code(&self) -> i32 {
        match self {
            IoClass::None => 0,
            IoClass::Realtime => 1,
            IoClass::BestEffort => 2,
            IoClass::Idle => 3,
        }
    }

    pub fn from_code(c: i32) -> IoClass {
        match c {
            1 => IoClass::Realtime,
            2 => IoClass::BestEffort,
            3 => IoClass::Idle,
            _ => IoClass::None,
        }
    }

    pub const ALL: &'static [IoClass] = &[
        IoClass::None,
        IoClass::BestEffort,
        IoClass::Idle,
        IoClass::Realtime,
    ];

    /// 该 class 是否支持优先级档位（只有 best-effort / realtime 有）。
    pub fn has_level(&self) -> bool {
        matches!(self, IoClass::BestEffort | IoClass::Realtime)
    }
}

/// 读某个进程的 IO 调度类与优先级：返回 (类, 档位)。
pub fn get_io_priority(pid: i32) -> Option<(IoClass, u8)> {
    let v = unsafe { syscall(SYS_IOPRIO_GET, 1i64, pid as i64) };
    if v < 0 {
        return None;
    }
    let v = v as i32;
    if v == 0 {
        return None;
    }
    let class = (v >> 13) & 0x7;
    let level = (v & 0x1fff) as u8;
    Some((IoClass::from_code(class), level))
}

/// 设置 IO 调度类与优先级（档位 0..=7，仅 best-effort/realtime 有意义）。
pub fn set_io_priority(pid: i32, class: IoClass, level: u8) -> Result<(), String> {
    let level = if class.has_level() {
        level.min(7) as i64
    } else {
        0
    };
    let ioprio = ((class.code() as i64) << 13) | level;
    let r = unsafe { syscall(SYS_IOPRIO_SET, 1i64, pid as i64, ioprio) };
    if r == 0 {
        Ok(())
    } else {
        Err(errno_text(&std::io::Error::last_os_error()))
    }
}

/// 当前进程的 uid（判断目标进程是否属于自己）。
pub fn current_uid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

/// 是否是 root。
pub fn is_root() -> bool {
    current_uid() == 0
}

/// 能否直接操作该进程（自己的进程或 root）。
pub fn can_control(p: &Process) -> bool {
    is_root() || p.uid == current_uid()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::monitor::proc as mproc;
    use std::time::{Duration, Instant};

    fn spawn_sleeper() -> std::process::Child {
        std::process::Command::new("sleep")
            .arg("120")
            .spawn()
            .expect("无法启动 sleep 用于测试")
    }

    fn state_of(pid: i32) -> char {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("读不到 stat");
        mproc::parse_pid_stat(&text).expect("解析失败").state
    }

    fn nice_of(pid: i32) -> i64 {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        mproc::parse_pid_stat(&text).unwrap().nice
    }

    /// 等某个状态出现（最多等 2 秒）。
    fn wait_state(pid: i32, want: char) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(2) {
            if state_of(pid) == want {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn signal_stop_cont_term_on_own_child() {
        let mut child = spawn_sleeper();
        let pid = child.id() as i32;

        // SIGSTOP → T（已停止）
        send_signal(pid, 19).unwrap();
        assert!(wait_state(pid, 'T'), "SIGSTOP 之后状态不是 T");

        // SIGCONT → 回到 S
        send_signal(pid, 18).unwrap();
        assert!(wait_state(pid, 'S'), "SIGCONT 之后状态不是 S");

        // SIGTERM → 进程被信号杀死（退出码 None、signal = 15）
        send_signal(pid, 15).unwrap();
        let status = child.wait().expect("等待子进程失败");
        assert!(!status.success());
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(), Some(15), "应是被 SIGTERM 终止");
        }

        // 不存在的 pid：报「进程不存在」
        let err = send_signal(999_999, 15).unwrap_err();
        assert!(
            err.contains("不存在") || err.contains("No such"),
            "错误文案不合理：{err}"
        );

        // 拒绝 pid <= 1
        assert!(send_signal(1, 15).is_err());
        assert!(send_signal(0, 15).is_err());
        assert!(send_signal(-1, 15).is_err());
    }

    #[test]
    fn nice_affinity_and_ioprio_roundtrip() {
        let mut child = spawn_sleeper();
        let pid = child.id() as i32;

        // nice：调大 nice（降低优先级）普通用户即可
        set_nice(pid, 10).unwrap();
        assert_eq!(nice_of(pid), 10);
        set_nice(pid, 19).unwrap();
        assert_eq!(nice_of(pid), 19);
        // 反过来调小 nice 需要 CAP_SYS_NICE：普通用户必须失败且提示权限不足
        if !is_root() {
            let err = set_nice(pid, 0).unwrap_err();
            assert!(err.contains("权限"), "错误文案应提示权限问题：{err}");
            assert_eq!(nice_of(pid), 19, "失败后 nice 不应变化");
        }
        // 越界拒绝
        assert!(set_nice(pid, -21).is_err());
        assert!(set_nice(pid, 20).is_err());

        // CPU 亲和性：先读全部，再限制到第一个核
        let all = mproc::affinity_cpus(&mproc::affinity_hex(pid));
        assert!(!all.is_empty(), "读不到亲和性");
        assert!(all.len() > 1, "本机应多于一个 CPU");
        set_affinity(pid, &[all[0]]).unwrap();
        assert_eq!(
            mproc::affinity_cpus(&mproc::affinity_hex(pid)),
            vec![all[0]]
        );
        // 恢复
        set_affinity(pid, &all).unwrap();
        assert_eq!(
            mproc::affinity_cpus(&mproc::affinity_hex(pid)).len(),
            all.len()
        );
        assert!(set_affinity(pid, &[]).is_err());

        // IO 优先级：best-effort 档位 5 → 读回来一致
        set_io_priority(pid, IoClass::BestEffort, 5).unwrap();
        let (class, level) = get_io_priority(pid).expect("读不到 IO 优先级");
        assert_eq!(class, IoClass::BestEffort);
        assert_eq!(level, 5);

        // idle 类
        set_io_priority(pid, IoClass::Idle, 0).unwrap();
        assert_eq!(get_io_priority(pid).unwrap().0, IoClass::Idle);

        // 档位越界会被夹到 0..=7
        set_io_priority(pid, IoClass::BestEffort, 99).unwrap();
        assert!(get_io_priority(pid).unwrap().1 <= 7);

        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    fn kill_whole_process_tree() {
        // sh 拉起两个 sleep，然后对整棵树发 SIGTERM（先子后父）
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 120 & sleep 120 & wait")
            .spawn()
            .expect("无法启动 sh");
        let pid = child.id() as i32;
        std::thread::sleep(Duration::from_millis(300));

        // 找子进程（pgrep 只用于构造列表，杀进程仍走我们自己的实现）
        let out = std::process::Command::new("pgrep")
            .args(["-P", &pid.to_string()])
            .output()
            .expect("pgrep 失败");
        let kids: Vec<i32> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect();
        assert_eq!(kids.len(), 2, "sh 应有两个 sleep 子进程，实际 {kids:?}");

        let mut procs: Vec<crate::model::monitor::Process> = Vec::new();
        for (i, k) in kids.iter().enumerate() {
            let p = crate::model::monitor::Process {
                pid: *k,
                ppid: pid,
                name: format!("sleep{i}"),
                ..Default::default()
            };
            procs.push(p);
        }
        let sh = crate::model::monitor::Process {
            pid,
            ppid: 0,
            name: "sh".into(),
            ..Default::default()
        };
        procs.push(sh);

        // descendants_of 顺序：父 + 子
        let mut d = mproc::descendants_of(&procs, pid);
        d.sort();
        assert_eq!(d.len(), 3);

        let (ok, errs) = send_signal_tree(&procs, pid, 15);
        assert_eq!(ok, 3, "应成功 3 个，实际 {ok}，错误 {errs:?}");
        assert!(errs.is_empty(), "不应有失败：{errs:?}");

        // 回收 sh（僵尸进程的 /proc/<pid> 不会消失，必须先 wait）
        let status = child.wait().expect("等待 sh 失败");
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(), Some(15), "sh 应是被 SIGTERM 结束");
        }
        // 两个 sleep 也应随之消失
        let t0 = Instant::now();
        loop {
            let alive: Vec<i32> = kids
                .iter()
                .copied()
                .filter(|k| std::path::Path::new(&format!("/proc/{k}")).exists())
                .collect();
            if alive.is_empty() {
                break;
            }
            if t0.elapsed() > Duration::from_secs(3) {
                panic!("子进程没有退出：{alive:?}");
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    #[test]
    fn permission_and_uid_helpers() {
        // 当前用户/ root 判断必须自洽
        assert_eq!(is_root(), current_uid() == 0);
        // 自己的进程一定可控制
        let mut p = crate::model::monitor::Process {
            uid: current_uid(),
            ..Default::default()
        };
        assert!(can_control(&p));
        // pid 1（root）在普通用户下不可控制
        p.uid = 0;
        if !is_root() {
            assert!(!can_control(&p));
        }
        // pkexec 拒绝非法信号编号（不会真的调用 pkexec）
        assert!(pkexec_send_signal(1, 99).is_err());
        assert!(pkexec_send_signal(1, 0).is_err());
    }
}
