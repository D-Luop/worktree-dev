//! Thin Win32 helpers: Job Objects, process memory, pipe security, console control, detached spawn.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::process::Command;

use windows_sys::Win32::Foundation::{CloseHandle, BOOL, FALSE, HANDLE, TRUE};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::System::Console::{SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, OpenJobObjectW, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
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
    unsafe {
        let h = CreateJobObjectW(std::ptr::null(), wide(name).as_ptr());
        if h.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let job = Handle(h);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == FALSE
        {
            return Err(std::io::Error::last_os_error());
        }
        if AssignProcessToJobObject(job.0, GetCurrentProcess()) == FALSE {
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
