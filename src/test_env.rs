//! 单测专用：进程级 `XDG_DATA_HOME` 互斥。
//!
//! `XDG_DATA_HOME` 是**进程全局**变量：`page::notepad` 的场景单测和
//! `utils::sniffer::store` 的 sqlite 单测都会改它，并发跑时会在对方
//! `create → load_meta` 的窗口里把目录翻掉，表现为「刚建的条目应能读到
//! meta」偶发失败（2026-10-05 诊断，连续 2/2 复现后确认根因）。
//!
//! 规则：**任何要改 `XDG_DATA_HOME` 的单测，必须在整个测试体内持有
//! [`lock_xdg`] 返回的守卫**；守卫 drop 时释放锁并把变量还原成进入前的值。

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

/// 保护 `XDG_DATA_HOME` 的全局互斥锁。
///
/// 中毒不豁免：前一个单测失败（panic 持锁退出）不应让后续所有相关单测
/// 都拿不到锁，因此用 `PoisonError::into_inner` 继续。
pub fn lock_xdg() -> XdgGuard {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let mutex = LOCK.get_or_init(|| Mutex::new(()));
    let guard = mutex.lock().unwrap_or_else(PoisonError::into_inner);
    XdgGuard {
        _guard: guard,
        old: std::env::var_os("XDG_DATA_HOME"),
    }
}

/// RAII 守卫：持有期间独占 `XDG_DATA_HOME`；drop 时先还原变量再释放锁。
pub struct XdgGuard {
    _guard: MutexGuard<'static, ()>,
    old: Option<OsString>,
}

impl Drop for XdgGuard {
    fn drop(&mut self) {
        // SAFETY: 与 `std::env::set_var` 在单测中的既有用法一致——
        // 本守卫持有全局锁，此窗口内不会有其它线程在读写该变量。
        unsafe {
            match &self.old {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
    }
}
