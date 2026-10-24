//! Thread CPU accounting outside the observed callback window.

// Rust guideline compliant 2026-02-21

#[cfg(unix)]
pub(crate) fn thread_cpu_ns() -> u64 {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid, writable scalar output is provided for this thread's
    // clock; clock_gettime retains no pointer and owns no caller resource.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut value) },
        0
    );
    (value.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|base| base.checked_add(value.tv_nsec as u64))
        .expect("thread CPU clock overflow")
}

#[cfg(windows)]
pub(crate) fn thread_cpu_ns() -> u64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let mut created = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exited = created;
    let mut kernel = created;
    let mut user = created;
    // SAFETY: the current-thread pseudo handle needs no closing. All four
    // outputs are initialized, writable and retained only for the call.
    assert_ne!(
        unsafe {
            GetThreadTimes(
                GetCurrentThread(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        },
        0
    );
    let ticks =
        |value: FILETIME| u64::from(value.dwLowDateTime) | (u64::from(value.dwHighDateTime) << 32);
    ticks(kernel)
        .checked_add(ticks(user))
        .and_then(|value| value.checked_mul(100))
        .expect("thread CPU clock overflow")
}
