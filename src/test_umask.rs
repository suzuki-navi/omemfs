//! Test helper: run a closure under a chosen process umask.
//!
//! `umask(2)` is process-global, so every test that changes it must hold this
//! lock, or parallel tests would restore each other's value and leak modes.

use std::sync::Mutex;

static UMASK_LOCK: Mutex<()> = Mutex::new(());

pub fn with_umask<T>(mask: libc::mode_t, f: impl FnOnce() -> T) -> T {
    let _guard = UMASK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let old = unsafe { libc::umask(mask) };
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    unsafe { libc::umask(old) };
    match out {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(e),
    }
}
