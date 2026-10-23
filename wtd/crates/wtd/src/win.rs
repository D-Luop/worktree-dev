//! Thin Win32 helpers: Job Objects, process memory, pipe security, console control, detached spawn.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::process::Command;

use windows_sys::Win32::Foundation::{CloseHandle, SetHandleInformation, BOOL, FALSE, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, TRUE};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::System::Console::{GetStdHandle, SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, OpenJobObjectW, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

// Job access rights (winnt.h); windows-sys keeps them in SystemServices.
const JOB_OBJECT_QUERY: u32 = 0x0004;
const JOB_OBJECT_TERMINATE: u32 = 0x0008;
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
    DETACHED_PROCESS, PROCESS_QUERY_LIMITED_INFORMATION,
};

pub fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// Job object names are in the session-local namespace.
pub fn job_name(tag: &str) -> String {
    format!("Local\\wtd-job-{tag}")
}

/// An owned handle, closed on drop.
pub struct Handle(pub HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

/// Create a named job that kills every member when its last handle closes, and put the current
/// process in it (children inherit membership). Keep the returned handle alive for the session.
pub fn enter_new_job(name: &str) -> std::io::Result<Handle> {
    let job = new_kill_on_close_job(name)?;
    unsafe {
        if AssignProcessToJobObject(job.0, GetCurrentProcess()) == FALSE {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(job)
}

/// A named job that kills every member when its last handle closes (members added by the caller).
/// Children that ask for CREATE_BREAKAWAY_FROM_JOB may leave it: a tray or daemon started from inside
/// a session must outlive that session.
pub fn new_kill_on_close_job(name: &str) -> std::io::Result<Handle> {
    unsafe {
        let h = CreateJobObjectW(std::ptr::null(), wide(name).as_ptr());
        if h.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let job = Handle(h);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        if SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == FALSE
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(job)
    }
}

/// Kill every process in a job by name. `Ok(false)` if no such job exists.
pub fn terminate_job(name: &str) -> std::io::Result<bool> {
    unsafe {
        let h = OpenJobObjectW(JOB_OBJECT_TERMINATE, FALSE, wide(name).as_ptr());
        if h.is_null() {
            return Ok(false);
        }
        let job = Handle(h);
        if TerminateJobObject(job.0, 1) == FALSE {
            return Err(std::io::Error::last_os_error());
        }
        Ok(true)
    }
}

pub fn terminate_own_job(job: &Handle) {
    unsafe { TerminateJobObject(job.0, 1) };
}

/// (total CPU time in 100ns units, member pids) for a named job.
pub fn job_stats(name: &str) -> Option<(u64, Vec<u32>)> {
    unsafe {
        let h = OpenJobObjectW(JOB_OBJECT_QUERY, FALSE, wide(name).as_ptr());
        if h.is_null() {
            return None;
        }
        let job = Handle(h);
        let mut acct: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
        if QueryInformationJobObject(
            job.0,
            JobObjectBasicAccountingInformation,
            &mut acct as *mut _ as *mut c_void,
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            std::ptr::null_mut(),
        ) == FALSE
        {
            return None;
        }
        let cpu = (acct.TotalUserTime + acct.TotalKernelTime) as u64;
        // JOBOBJECT_BASIC_PROCESS_ID_LIST: two u32 counts then ULONG_PTR ids.
        let mut buf = vec![0usize; 2 + 512];
        let mut pids = Vec::new();
        if QueryInformationJobObject(
            job.0,
            JobObjectBasicProcessIdList,
            buf.as_mut_ptr() as *mut c_void,
            (buf.len() * std::mem::size_of::<usize>()) as u32,
            std::ptr::null_mut(),
        ) != FALSE
        {
            let counts = buf[0];
            let listed = (counts >> 32) as usize; // NumberOfProcessIdsInList (2nd u32 on little-endian)
            for i in 0..listed.min(buf.len() - 1) {
                pids.push(buf[1 + i] as u32);
            }
        }
        Some((cpu, pids))
    }
}

pub fn working_set_bytes(pid: u32) -> u64 {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
        if h.is_null() {
            return 0;
        }
        let p = Handle(h);
        let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if K32GetProcessMemoryInfo(p.0, &mut c, c.cb) == FALSE {
            return 0;
        }
        c.WorkingSetSize as u64
    }
}

/// (total MB, used MB)
pub fn system_memory_mb() -> (u64, u64) {
    unsafe {
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) == FALSE {
            return (0, 0);
        }
        let total = m.ullTotalPhys / (1024 * 1024);
        (total, total - m.ullAvailPhys / (1024 * 1024))
    }
}

/// Security attributes allowing only the pipe's owner (the current user) and SYSTEM.
/// Leaked on purpose: needed for every pipe instance the daemon creates over its lifetime.
pub fn owner_only_security_attributes() -> *mut c_void {
    unsafe {
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let sddl = wide("D:P(A;;GA;;;OW)(A;;GA;;;SY)");
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) == FALSE {
            return std::ptr::null_mut();
        }
        let sa = Box::new(SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: FALSE,
        });
        Box::into_raw(sa) as *mut c_void
    }
}

unsafe extern "system" fn swallow_ctrl_c(ctrl: u32) -> BOOL {
    if ctrl == CTRL_C_EVENT || ctrl == CTRL_BREAK_EVENT {
        TRUE
    } else {
        FALSE // close / logoff / shutdown → default handling (exit → job closes → tree dies)
    }
}

/// Keep Ctrl+C meant for the child (claude) from killing the wrapper. A handler routine — unlike
/// `SetConsoleCtrlHandler(NULL, TRUE)` — isn't inherited, so the child still sees Ctrl+C normally.
pub fn ignore_ctrl_c_in_this_process() {
    unsafe { SetConsoleCtrlHandler(Some(swallow_ctrl_c), TRUE) };
}

/// Start a process fully detached from the caller's console and job (so it outlives VSCode's
/// terminal or the tray), with stdio redirected as configured on `cmd`.
pub fn spawn_detached(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    // CreateProcess hands the child EVERY inheritable handle we hold, not just the stdio we set on
    // `cmd`. Our own stdio is usually the caller's pipe (a VSCode execFile, `… | tail`), and a resident
    // child holding it open means that caller never sees EOF and hangs. Our short-lived launcher
    // doesn't need its stdio inheritable, so turn that off first; `cmd`'s own stdio is unaffected.
    unsafe {
        for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let h = GetStdHandle(id);
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    match cmd.creation_flags(base | CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Ok(c) => Ok(c),
        // The caller's job forbids breakaway: start inside it instead.
        Err(_) => cmd.creation_flags(base).spawn(),
    }
}

/// For short-lived helpers (git) spawned by background processes: never flash a console window.
pub fn no_window(cmd: &mut Command) -> &mut Command {
    cmd.creation_flags(CREATE_NO_WINDOW)
}

#[allow(dead_code)]
pub fn is_null(h: HANDLE) -> bool {
    h.is_null()
}

// --- HKCU registry ---------------------------------------------------------------------------------

pub mod reg {
    use windows_sys::Win32::System::Registry::*;

    use super::wide;

    fn get(key: &str, value: &str, flags: u32, buf: Option<&mut [u8]>) -> Option<u32> {
        let mut size = buf.as_ref().map_or(0, |b| b.len() as u32);
        let ptr = buf.map_or(std::ptr::null_mut(), |b| b.as_mut_ptr() as *mut _);
        let rc = unsafe { RegGetValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), wide(value).as_ptr(), flags, std::ptr::null_mut(), ptr, &mut size) };
        (rc == 0).then_some(size)
    }

    pub fn exists(key: &str, value: &str) -> bool {
        get(key, value, RRF_RT_ANY, None).is_some()
    }

    pub fn get_sz(key: &str, value: &str) -> Option<String> {
        let size = get(key, value, RRF_RT_REG_SZ, None)?;
        let mut buf = vec![0u8; size as usize];
        let n = get(key, value, RRF_RT_REG_SZ, Some(&mut buf))? as usize;
        let units: Vec<u16> = buf[..n].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Some(String::from_utf16_lossy(&units).trim_end_matches('\0').to_string())
    }

    pub fn get_dword(key: &str, value: &str) -> Option<u32> {
        let mut buf = [0u8; 4];
        get(key, value, RRF_RT_REG_DWORD, Some(&mut buf))?;
        Some(u32::from_le_bytes(buf))
    }

    pub fn set_sz(key: &str, value: &str, data: &str) -> bool {
        let data = wide(data);
        unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), wide(value).as_ptr(), REG_SZ, data.as_ptr() as *const _, (data.len() * 2) as u32) == 0 }
    }

    pub fn set_dword(key: &str, value: &str, data: u32) -> bool {
        unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), wide(value).as_ptr(), REG_DWORD, &data as *const u32 as *const _, 4) == 0 }
    }

    pub fn delete(key: &str, value: &str) {
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), wide(value).as_ptr()) };
    }

    /// Create `key` if missing. A volatile key lives in memory only: Windows drops it at sign-out.
    /// Its parent must already exist (and be non-volatile), or missing parents would be volatile too.
    pub fn create(key: &str, volatile: bool) -> bool {
        let opt = if volatile { REG_OPTION_VOLATILE } else { REG_OPTION_NON_VOLATILE };
        let mut h: HKEY = std::ptr::null_mut();
        unsafe {
            let rc = RegCreateKeyExW(HKEY_CURRENT_USER, wide(key).as_ptr(), 0, std::ptr::null(), opt, KEY_WRITE, std::ptr::null(), &mut h, std::ptr::null_mut());
            if rc == 0 {
                RegCloseKey(h);
            }
            rc == 0
        }
    }

    pub fn subkeys(key: &str) -> Vec<String> {
        let mut out = vec![];
        unsafe {
            let mut h: HKEY = std::ptr::null_mut();
            if RegOpenKeyExW(HKEY_CURRENT_USER, wide(key).as_ptr(), 0, KEY_READ, &mut h) != 0 {
                return out;
            }
            let mut name = [0u16; 256];
            for i in 0.. {
                let mut len = name.len() as u32;
                if RegEnumKeyExW(h, i, name.as_mut_ptr(), &mut len, std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) != 0 {
                    break;
                }
                out.push(String::from_utf16_lossy(&name[..len as usize]));
            }
            RegCloseKey(h);
        }
        out
    }
}

// --- processes holding a folder as their working directory ------------------------------------------

#[repr(C)]
struct ProcessBasicInformation {
    exit_status: isize,
    peb: usize,
    affinity: usize,
    base_priority: isize,
    pid: usize,
    parent: usize,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationProcess(h: HANDLE, class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn ReadProcessMemory(h: HANDLE, addr: *const c_void, buf: *mut c_void, size: usize, read: *mut usize) -> BOOL;
}

unsafe fn read<T: Copy>(h: HANDLE, addr: usize) -> Option<T> {
    let mut v = std::mem::MaybeUninit::<T>::uninit();
    let mut n = 0usize;
    (ReadProcessMemory(h, addr as *const c_void, v.as_mut_ptr() as *mut c_void, std::mem::size_of::<T>(), &mut n) != FALSE
        && n == std::mem::size_of::<T>())
    .then(|| v.assume_init())
}

/// A 64-bit process's current directory, read from its PEB (None for 32-bit or inaccessible ones).
fn process_cwd(pid: u32) -> Option<String> {
    use windows_sys::Win32::System::Threading::{IsWow64Process, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, FALSE, pid);
        if h.is_null() {
            return None;
        }
        let _close = Handle(h);
        let mut wow: BOOL = FALSE;
        if IsWow64Process(h, &mut wow) == FALSE || wow != FALSE {
            return None;
        }
        let mut pbi: ProcessBasicInformation = std::mem::zeroed();
        let mut ret = 0u32;
        if NtQueryInformationProcess(h, 0, &mut pbi as *mut _ as *mut c_void, std::mem::size_of::<ProcessBasicInformation>() as u32, &mut ret) != 0 {
            return None;
        }
        // PEB.ProcessParameters @0x20; RTL_USER_PROCESS_PARAMETERS.CurrentDirectory.DosPath @0x38
        let params: usize = read(h, pbi.peb + 0x20)?;
        let len: u16 = read(h, params + 0x38)?;
        let buf: usize = read(h, params + 0x40)?;
        let mut s = vec![0u16; len as usize / 2];
        let mut n = 0usize;
        if ReadProcessMemory(h, buf as *const c_void, s.as_mut_ptr() as *mut c_void, len as usize, &mut n) == FALSE {
            return None;
        }
        Some(String::from_utf16_lossy(&s))
    }
}

/// End every process whose working directory is `dir` or inside it (e.g. a shell left in a worktree
/// being archived): on Windows that alone blocks moving or deleting the folder. Returns their pids.
pub fn kill_processes_in(dir: &std::path::Path) -> Vec<u32> {
    use windows_sys::Win32::System::ProcessStatus::K32EnumProcesses;
    use windows_sys::Win32::System::Threading::{TerminateProcess, PROCESS_TERMINATE};
    let norm = |s: &str| s.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let root = norm(&dir.to_string_lossy());
    let mut pids = vec![0u32; 4096];
    let mut bytes = 0u32;
    if unsafe { K32EnumProcesses(pids.as_mut_ptr(), (pids.len() * 4) as u32, &mut bytes) } == FALSE {
        return vec![];
    }
    pids.truncate(bytes as usize / 4);
    let me = std::process::id();
    let mut killed = vec![];
    for pid in pids.into_iter().filter(|p| *p != 0 && *p != me) {
        let Some(cwd) = process_cwd(pid) else { continue };
        let c = norm(&cwd);
        if c == root || c.starts_with(&format!("{root}\\")) {
            unsafe {
                let h = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
                if !h.is_null() {
                    let _close = Handle(h);
                    if TerminateProcess(h, 1) != FALSE {
                        killed.push(pid);
                    }
                }
            }
        }
    }
    killed
}

#[cfg(test)]
mod tests {
    #[test]
    fn kills_processes_working_in_a_folder() {
        let dir = std::env::temp_dir().join(format!("wtd-hold-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let mut child = std::process::Command::new("cmd").args(["/c", "ping -n 30 127.0.0.1 >nul"]).current_dir(dir.join("sub")).spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let killed = super::kill_processes_in(&dir);
        assert!(killed.contains(&child.id()), "{killed:?}");
        child.wait().unwrap();
        assert!(super::kill_processes_in(&dir.join("elsewhere")).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
