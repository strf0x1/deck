//! Memory of a single process, used for the VMM rather than the local supervisor.

#[cfg(target_os = "macos")]
pub(super) fn process(pid: u32) -> Option<u64> {
    let pid = i32::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::uninit();
    // SAFETY: RUSAGE_INFO_V2 writes exactly a rusage_info_v2 to this buffer.
    let result = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, info.as_mut_ptr().cast()) };
    if result != 0 {
        return None;
    }
    // SAFETY: a successful proc_pid_rusage initialized the buffer.
    Some(unsafe { info.assume_init() }.ri_phys_footprint)
}

#[cfg(target_os = "linux")]
pub(super) fn process(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find_map(|line| line.strip_prefix("VmRSS:"))?;
    let mut fields = line.split_whitespace();
    let kib = fields.next()?.parse::<u64>().ok()?;
    (fields.next()? == "kB").then(|| kib.checked_mul(1024)).flatten()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn process(_pid: u32) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn reads_own_process_and_rejects_a_nonexistent_pid() {
        assert!(super::process(std::process::id()).is_some_and(|bytes| bytes > 0));
        assert_eq!(super::process(u32::MAX), None);
    }
}
