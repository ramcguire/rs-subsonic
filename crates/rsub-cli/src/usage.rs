//! The process's CPU time and peak memory, for benchmark reports.

use std::time::Duration;

pub struct Usage {
    /// User plus system time, all threads.
    pub cpu: Duration,
    /// Peak resident set (working set on Windows), bytes.
    pub peak_bytes: u64,
}

#[cfg(unix)]
pub fn usage() -> Option<Usage> {
    // SAFETY: getrusage fills the zeroed struct it's given.
    let ru = unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut ru) != 0 {
            return None;
        }
        ru
    };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    // ru_maxrss is in bytes on macOS, KiB elsewhere.
    let unit = if cfg!(target_os = "macos") { 1 } else { 1024 };
    Some(Usage {
        cpu: tv(ru.ru_utime) + tv(ru.ru_stime),
        peak_bytes: ru.ru_maxrss as u64 * unit,
    })
}

#[cfg(windows)]
pub fn usage() -> Option<Usage> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    let mut mem: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    mem.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: the current-process pseudo handle needs no closing, and each
    // call writes only the structs it's given.
    let ok = unsafe {
        let p = GetCurrentProcess();
        GetProcessTimes(p, &mut created, &mut exited, &mut kernel, &mut user) != 0
            && K32GetProcessMemoryInfo(p, &mut mem, mem.cb) != 0
    };
    if !ok {
        return None;
    }
    // FILETIME counts 100 ns ticks.
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    Some(Usage {
        cpu: Duration::from_nanos((ticks(kernel) + ticks(user)) * 100),
        peak_bytes: mem.PeakWorkingSetSize as u64,
    })
}

#[cfg(not(any(unix, windows)))]
pub fn usage() -> Option<Usage> {
    None
}
