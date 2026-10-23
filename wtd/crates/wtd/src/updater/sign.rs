//! Authenticode: is a file validly signed (WinVerifyTrust, no UI, no network revocation check)?

use std::path::Path;

use anyhow::{bail, Result};
use windows_sys::Win32::Security::WinTrust::{
    WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO, WTD_CHOICE_FILE, WTD_REVOKE_NONE,
    WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
};

use crate::win;

fn trust(p: &Path) -> i32 {
    let path = win::wide(&p.to_string_lossy());
    unsafe {
        let mut file = WINTRUST_FILE_INFO { cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32, pcwszFilePath: path.as_ptr(), ..std::mem::zeroed() };
        let mut data = WINTRUST_DATA {
            cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
            dwUIChoice: WTD_UI_NONE,
            fdwRevocationChecks: WTD_REVOKE_NONE,
            dwUnionChoice: WTD_CHOICE_FILE,
            Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
            dwStateAction: WTD_STATEACTION_VERIFY,
            ..std::mem::zeroed()
        };
        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        let r = WinVerifyTrust(std::ptr::null_mut(), &mut action, &mut data as *mut _ as *mut _);
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        WinVerifyTrust(std::ptr::null_mut(), &mut action, &mut data as *mut _ as *mut _);
        r
    }
}

pub fn is_signed(p: &Path) -> bool {
    trust(p) == 0
}

pub fn verify(p: &Path) -> Result<()> {
    match trust(p) {
        0 => Ok(()),
        r => bail!("Authenticode check failed for {} (0x{:08x})", p.display(), r as u32),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn unsigned_files_fail() {
        let t = super::super::tests::temp("sign");
        let f = t.join("fake.exe");
        std::fs::write(&f, b"MZ not really").unwrap();
        assert!(!super::is_signed(&f));
        assert!(super::verify(&f).is_err());
        std::fs::remove_dir_all(&t).ok();
    }
}
