//! Classify the process behind a filesystem request (design §4.5).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use xof_core::engine::ReaderClass;

/// Default executable names (lowercase, without extension) treated as X-Plane.
pub const DEFAULT_XPLANE_PROCESSES: &[&str] = &["x-plane", "x-plane-x86_64", "x-plane 12"];

/// Caches pid → class for a short time (pids get reused).
pub struct ProcessClassifier {
    names: Vec<String>,
    cache: Mutex<HashMap<u32, (ReaderClass, Instant)>>,
}

impl ProcessClassifier {
    pub fn new(names: &[String]) -> Self {
        let names = if names.is_empty() {
            DEFAULT_XPLANE_PROCESSES.iter().map(|s| s.to_string()).collect()
        } else {
            names.iter().map(|s| s.to_ascii_lowercase()).collect()
        };
        Self { names, cache: Mutex::new(HashMap::new()) }
    }

    pub fn classify(&self, pid: u32) -> ReaderClass {
        if pid == 0 {
            // Unknown requester (e.g. kernel paging I/O): treat as X-Plane so
            // we never starve the simulator.
            return ReaderClass::XPlane;
        }
        let now = Instant::now();
        if let Some((c, at)) = self.cache.lock().get(&pid) {
            if now.duration_since(*at) < Duration::from_secs(30) {
                return *c;
            }
        }
        let class = match process_name(pid) {
            Some(name) if self.is_xplane(&name) => ReaderClass::XPlane,
            Some(_) => ReaderClass::Other,
            // Can't tell (permissions, exited): be generous.
            None => ReaderClass::XPlane,
        };
        let mut c = self.cache.lock();
        if c.len() > 4096 {
            c.clear();
        }
        c.insert(pid, (class, now));
        class
    }

    pub fn is_xplane(&self, exe_name: &str) -> bool {
        let base = exe_name.rsplit(['/', '\\']).next().unwrap_or(exe_name).to_ascii_lowercase();
        let base = base.strip_suffix(".exe").unwrap_or(&base);
        self.names.iter().any(|n| n == base)
    }
}

/// Executable name (file name only) of a process.
#[cfg(windows)]
pub fn process_name(pid: u32) -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_FORMAT(0), PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return None;
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        Some(full.rsplit('\\').next().unwrap_or(&full).to_string())
    }
}

#[cfg(target_os = "linux")]
pub fn process_name(pid: u32) -> Option<String> {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    match exe {
        Some(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()),
        None => std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string()),
    }
}

#[cfg(target_os = "macos")]
pub fn process_name(pid: u32) -> Option<String> {
    extern "C" {
        fn proc_pidpath(pid: libc::c_int, buf: *mut libc::c_void, size: u32) -> libc::c_int;
    }
    let mut buf = vec![0u8; 4096];
    let n = unsafe { proc_pidpath(pid as libc::c_int, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    let full = String::from_utf8_lossy(&buf).into_owned();
    Some(full.rsplit('/').next().unwrap_or(&full).to_string())
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn process_name(_pid: u32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let c = ProcessClassifier::new(&[]);
        assert!(c.is_xplane("X-Plane.exe"));
        assert!(c.is_xplane("C:\\X-Plane 12\\X-Plane.exe"));
        assert!(c.is_xplane("/opt/X-Plane 12/X-Plane-x86_64"));
        assert!(!c.is_xplane("MsMpEng.exe"));
        assert!(!c.is_xplane("mds_stores"));
    }

    #[test]
    fn own_process_is_other() {
        let c = ProcessClassifier::new(&[]);
        assert_eq!(c.classify(std::process::id()), ReaderClass::Other);
        assert!(process_name(std::process::id()).is_some());
    }
}
