//! A monotonic nanosecond clock that is identical across processes on the same machine.
//!
//! `std::time::Instant` cannot be exchanged between processes, but the server and the SteamVR
//! driver must agree on "now" to the microsecond, so both use this instead.

/// Nanoseconds on the OS monotonic clock (`CLOCK_MONOTONIC` / `QueryPerformanceCounter`).
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // time_t/c_long are 32-bit on some targets
pub fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid pointer to a timespec; CLOCK_MONOTONIC is always available.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

#[cfg(windows)]
pub fn monotonic_ns() -> i64 {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::Performance::{
        QueryPerformanceCounter, QueryPerformanceFrequency,
    };
    static FREQ: OnceLock<i64> = OnceLock::new();
    let freq = *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: valid out pointer.
        unsafe { QueryPerformanceFrequency(&mut f) };
        f
    });
    let mut c = 0i64;
    // SAFETY: valid out pointer.
    unsafe { QueryPerformanceCounter(&mut c) };
    ((c as i128 * 1_000_000_000) / freq as i128) as i64
}

pub fn ns_to_s(ns: i64) -> f64 {
    ns as f64 * 1e-9
}
