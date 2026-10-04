//! A process hosted in a Windows pseudo console (ConPTY), inside a kill-on-close Job Object.
//!
//! The daemon owns these, so a session outlives the VSCode terminal that started it: terminals only
//! attach (`wtd host` / `wtd attach`) and relay bytes. Output must be drained continuously (a full
//! pipe stalls the child), so a reader thread runs for the session's whole life.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{bail, Result};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, FALSE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON};
use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, InitializeProcThreadAttributeList, ResumeThread,
    UpdateProcThreadAttribute, WaitForSingleObject, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
    INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

use crate::win::{self, Handle};

const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;

pub struct Pty {
    hpc: Mutex<HPCON>,
    input: Handle,
    process: Handle,
    pub job: Handle,
    pub job_name: String,
    pub pid: u32,
    size: Mutex<(u16, u16)>,
}
unsafe impl Send for Pty {}
unsafe impl Sync for Pty {}

fn wide_os(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

/// Windows command-line quoting (the rules CommandLineToArgvW / the CRT parse back).
pub fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"', '\n']) {
        return a.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in a.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat('\\').take(backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    out.extend(std::iter::repeat('\\').take(backslashes * 2));
    out.push('"');
    out
}

/// A CREATE_UNICODE_ENVIRONMENT block, sorted case-insensitively as Windows expects.
fn env_block(env: &[(String, String)]) -> Vec<u16> {
    let mut vars: Vec<&(String, String)> = env.iter().filter(|(k, _)| !k.is_empty() && !k.contains('=')).collect();
    vars.sort_by_key(|(k, _)| k.to_uppercase());
    let mut block = Vec::new();
    for (k, v) in vars {
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

impl Pty {
    /// Start `exe args…` in a new pseudo console of `cols`×`rows`. `on_output` gets every chunk the
    /// console writes; `on_exit` runs once with the exit code after the process (and its output) ends.
    pub fn spawn(
        exe: &Path,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
        tag: &str,
        on_output: impl Fn(&[u8]) + Send + 'static,
        on_exit: impl FnOnce(i32) + Send + 'static,
    ) -> Result<std::sync::Arc<Pty>> {
        unsafe {
            // pipes: we write input → conpty reads it; conpty writes output → we read it
            let (mut in_read, mut in_write, mut out_read, mut out_write): (HANDLE, HANDLE, HANDLE, HANDLE) =
                (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            if CreatePipe(&mut in_read, &mut in_write, std::ptr::null(), 0) == FALSE
                || CreatePipe(&mut out_read, &mut out_write, std::ptr::null(), 0) == FALSE
            {
                bail!("CreatePipe failed ({})", GetLastError());
            }
            let (in_read, in_write, out_read, out_write) = (Handle(in_read), Handle(in_write), Handle(out_read), Handle(out_write));
            let mut hpc: HPCON = Default::default();
            let size = COORD { X: cols.max(20) as i16, Y: rows.max(5) as i16 };
            let hr = CreatePseudoConsole(size, in_read.0, out_write.0, 0, &mut hpc);
            if hr != 0 {
                bail!("CreatePseudoConsole failed (0x{hr:08x})");
            }
            // conpty holds its own copies of these two ends
            drop(in_read);
            drop(out_write);

            let mut attr_size: usize = 0;
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attr_size);
            let mut attr_buf = vec![0u8; attr_size];
            let attrs = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
            if InitializeProcThreadAttributeList(attrs, 1, 0, &mut attr_size) == FALSE {
                ClosePseudoConsole(hpc);
                bail!("InitializeProcThreadAttributeList failed ({})", GetLastError());
            }
            if UpdateProcThreadAttribute(attrs, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, hpc as *const c_void, std::mem::size_of::<HPCON>(), std::ptr::null_mut(), std::ptr::null()) == FALSE {
                DeleteProcThreadAttributeList(attrs);
                ClosePseudoConsole(hpc);
                bail!("UpdateProcThreadAttribute failed ({})", GetLastError());
            }
            let mut si: STARTUPINFOEXW = std::mem::zeroed();
            si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            // Don't let the child pick up the daemon's own (redirected-to-log) stdio: with these set to
            // INVALID and STARTF_USESTDHANDLES, it gets the pseudo console's handles instead.
            si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
            si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
            si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
            si.lpAttributeList = attrs;

            let mut cmdline: Vec<u16> = std::iter::once(quote_arg(&exe.to_string_lossy()))
                .chain(args.iter().map(|a| quote_arg(a)))
                .collect::<Vec<_>>()
                .join(" ")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut envb = env_block(env);
            let cwdw = wide_os(cwd.as_os_str());
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = CreateProcessW(
                std::ptr::null(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                FALSE,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
                envb.as_mut_ptr() as *mut c_void,
                cwdw.as_ptr(),
                &si.StartupInfo,
                &mut pi,
            );
            DeleteProcThreadAttributeList(attrs);
            if ok == FALSE {
                let e = GetLastError();
                ClosePseudoConsole(hpc);
                bail!("starting {} failed (error {e})", exe.display());
            }
            let (process, thread) = (Handle(pi.hProcess), Handle(pi.hThread));

            // job before the first instruction runs, so every descendant is tracked and killable
            let job_name = win::job_name(&format!("host-{}", pi.dwProcessId));
            let job = win::new_kill_on_close_job(&job_name)?;
            if AssignProcessToJobObject(job.0, process.0) == FALSE {
                eprintln!("[pty] AssignProcessToJobObject failed ({}) for {tag}", GetLastError());
            }
            ResumeThread(thread.0);
            drop(thread);

            let pty = std::sync::Arc::new(Pty {
                hpc: Mutex::new(hpc),
                input: in_write,
                process,
                job,
                job_name,
                pid: pi.dwProcessId,
                size: Mutex::new((size.X as u16, size.Y as u16)),
            });

            // reader: drain output for the session's whole life (until conpty closes the pipe)
            let reader = std::thread::Builder::new().name(format!("pty-out-{tag}"));
            let out_read_raw = out_read.0 as usize;
            std::mem::forget(out_read); // owned by the thread now
            reader.spawn(move || {
                let h = out_read_raw as HANDLE;
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let mut n: u32 = 0;
                    if ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) == FALSE || n == 0 {
                        break;
                    }
                    on_output(&buf[..n as usize]);
                }
                CloseHandle(h);
            })?;

            // waiter: process exit → close the pseudo console (ends the reader) → report
            let w = pty.clone();
            std::thread::Builder::new().name(format!("pty-wait-{tag}")).spawn(move || {
                let mut code: u32 = 1;
                if WaitForSingleObject(w.process.0, INFINITE) == WAIT_OBJECT_0 {
                    GetExitCodeProcess(w.process.0, &mut code);
                }
                w.close();
                on_exit(code as i32);
            })?;
            Ok(pty)
        }
    }

    pub fn write(&self, data: &[u8]) -> std::io::Result<()> {
        let mut off = 0;
        while off < data.len() {
            let mut n: u32 = 0;
            let ok = unsafe { WriteFile(self.input.0, data[off..].as_ptr(), (data.len() - off) as u32, &mut n, std::ptr::null_mut()) };
            if ok == FALSE {
                return Err(std::io::Error::last_os_error());
            }
            off += n as usize;
        }
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let hpc = *self.hpc.lock().unwrap();
        if hpc == HPCON::default() {
            return;
        }
        *self.size.lock().unwrap() = (cols, rows);
        unsafe { ResizePseudoConsole(hpc, COORD { X: cols.max(20) as i16, Y: rows.max(5) as i16 }) };
    }

    /// Make conpty repaint the whole viewport (a newly attached terminal starts blank): a size change
    /// forces a full redraw, so step one row down and back.
    pub fn repaint(&self, cols: u16, rows: u16) {
        self.resize(cols, rows.saturating_sub(1).max(5));
        self.resize(cols, rows);
    }

    pub fn size(&self) -> (u16, u16) {
        *self.size.lock().unwrap()
    }

    /// Kill the whole process tree.
    pub fn kill(&self) {
        win::terminate_own_job(&self.job);
    }

    fn close(&self) {
        let mut g = self.hpc.lock().unwrap();
        if *g != HPCON::default() {
            unsafe { ClosePseudoConsole(*g) };
            *g = HPCON::default();
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::quote_arg;

    #[test]
    fn quoting() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg("has space"), "\"has space\"");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(r"C:\dir with space\"), r#""C:\dir with space\\""#);
    }
}
