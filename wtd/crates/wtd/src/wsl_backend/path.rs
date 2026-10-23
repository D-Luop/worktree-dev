//! Path translation between Windows and a WSL distro: `C:\x` ↔ `/mnt/c/x`, and Linux paths ↔ their
//! `\\wsl.localhost\<distro>\…` view.

use std::path::{Path, PathBuf};

const UNC_HOSTS: [&str; 2] = ["wsl.localhost", "wsl$"];

/// `\\wsl.localhost\<distro>\home\u\x` for `/home/u/x`.
pub fn unc(distro: &str, linux: &str) -> PathBuf {
    let rest = linux.trim_start_matches('/').replace('/', "\\");
    PathBuf::from(format!(r"\\wsl.localhost\{distro}\{rest}").trim_end_matches('\\').to_string())
}

/// `(distro, /linux/path)` for a `\\wsl.localhost\…` or `\\wsl$\…` path (either slash, `\\?\UNC\` too).
pub fn from_unc(p: &str) -> Option<(String, String)> {
    let s = p.replace('/', "\\");
    let s = s.strip_prefix(r"\\?\UNC\").map(|r| format!(r"\\{r}")).unwrap_or(s);
    let rest = s.strip_prefix(r"\\")?;
    let (host, rest) = rest.split_once('\\')?;
    if !UNC_HOSTS.iter().any(|h| h.eq_ignore_ascii_case(host)) {
        return None;
    }
    let (distro, path) = rest.split_once('\\').unwrap_or((rest, ""));
    if distro.is_empty() {
        return None;
    }
    let linux = format!("/{}", path.replace('\\', "/").trim_end_matches('/'));
    Some((distro.to_string(), linux))
}

/// An absolute Linux path, as opposed to a Windows one or an MSYS drive path (`/c/…`).
pub fn is_linux_abs(p: &str) -> bool {
    let b = p.as_bytes();
    let msys_drive = b.len() >= 2 && b[1].is_ascii_alphabetic() && (b.len() == 2 || b[2] == b'/');
    b.first() == Some(&b'/') && !p.starts_with("//") && !msys_drive
}

/// `C:\Users\x\wtd.exe` → `/mnt/c/Users/x/wtd.exe` (the default automount root). None for UNC paths.
pub fn to_mnt(win: &Path) -> Option<String> {
    let s = win.to_string_lossy().replace('\\', "/");
    let b = s.as_bytes();
    if b.len() < 2 || !b[0].is_ascii_alphabetic() || b[1] != b':' {
        return None;
    }
    Some(format!("/mnt/{}{}", (b[0] as char).to_ascii_lowercase(), s[2..].trim_end_matches('/')))
}

/// `/mnt/c/x` → `C:\x`.
pub fn from_mnt(linux: &str) -> Option<PathBuf> {
    let rest = linux.strip_prefix("/mnt/")?;
    let (drive, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let mut d = drive.chars();
    let (Some(c), None) = (d.next(), d.next()) else { return None };
    c.is_ascii_alphabetic().then(|| PathBuf::from(format!(r"{}:\{}", c.to_ascii_uppercase(), tail.replace('/', "\\"))))
}

/// Any path a hook or caller hands us, as a Windows path: Linux paths become their UNC view (or
/// `C:\…` for `/mnt/c/…`), the rest goes through [`crate::paths::normalize`].
pub fn host_path(p: &str, distro: Option<&str>) -> PathBuf {
    if is_linux_abs(p) {
        if let Some(w) = from_mnt(p) {
            return w;
        }
        if let Some(d) = distro {
            return unc(d, p);
        }
    }
    crate::paths::normalize(p)
}

/// The VS Code Remote-WSL URI for a file inside the distro.
pub fn vscode_remote_uri(distro: &str, linux: &str) -> String {
    format!("vscode-remote://wsl+{distro}{linux}")
}

/// Single-quoted for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `a/b` joined onto a Linux dir.
pub fn join(base: &str, rel: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), rel.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unc_round_trip() {
        let p = unc("Ubuntu", "/home/u/dev/worktree-dev/worktrees/r/feat/x");
        assert_eq!(p, PathBuf::from(r"\\wsl.localhost\Ubuntu\home\u\dev\worktree-dev\worktrees\r\feat\x"));
        assert_eq!(from_unc(&p.to_string_lossy()), Some(("Ubuntu".into(), "/home/u/dev/worktree-dev/worktrees/r/feat/x".into())));
        assert_eq!(unc("Ubuntu", "/"), PathBuf::from(r"\\wsl.localhost\Ubuntu"));
    }

    #[test]
    fn unc_variants() {
        let want = Some(("Debian".to_string(), "/home/u".to_string()));
        for s in [r"\\wsl$\Debian\home\u", r"\\WSL.LOCALHOST\Debian\home\u\", "//wsl.localhost/Debian/home/u", r"\\?\UNC\wsl.localhost\Debian\home\u"] {
            assert_eq!(from_unc(s), want, "{s}");
        }
        assert_eq!(from_unc(r"\\wsl.localhost\Debian"), Some(("Debian".into(), "/".into())));
        for s in [r"\\server\share\x", r"C:\x", "/home/u", r"\\wsl.localhost\"] {
            assert_eq!(from_unc(s), None, "{s}");
        }
    }

    #[test]
    fn mnt_round_trip() {
        assert_eq!(to_mnt(Path::new(r"C:\Users\x y\AppData\wtd.exe")).as_deref(), Some("/mnt/c/Users/x y/AppData/wtd.exe"));
        assert_eq!(to_mnt(Path::new(r"D:\")).as_deref(), Some("/mnt/d"));
        assert_eq!(to_mnt(Path::new(r"\\wsl.localhost\U\x")), None);
        assert_eq!(from_mnt("/mnt/d/dev/x"), Some(PathBuf::from(r"D:\dev\x")));
        assert_eq!(from_mnt("/mnt/wsl/x"), None);
        assert_eq!(from_mnt("/home/u"), None);
    }

    #[test]
    fn linux_vs_msys_vs_windows() {
        assert!(is_linux_abs("/home/u/x"));
        assert!(is_linux_abs("/mnt/c/x"));
        for s in ["/c/dev/x", "/d", r"C:\x", "C:/x", "relative", "//wsl.localhost/U/x"] {
            assert!(!is_linux_abs(s), "{s}");
        }
    }

    #[test]
    fn host_paths() {
        assert_eq!(host_path("/home/u/w", Some("Ubuntu")), PathBuf::from(r"\\wsl.localhost\Ubuntu\home\u\w"));
        assert_eq!(host_path("/mnt/d/dev", Some("Ubuntu")), PathBuf::from(r"D:\dev"));
        assert_eq!(host_path("/d/dev/x", Some("Ubuntu")), PathBuf::from(r"D:\dev\x"));
        assert_eq!(host_path(r"D:\dev", None), PathBuf::from(r"D:\dev"));
    }

    #[test]
    fn quoting_and_uris() {
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(vscode_remote_uri("Ubuntu", "/home/u/a.rs"), "vscode-remote://wsl+Ubuntu/home/u/a.rs");
        assert_eq!(join("/a/b/", "/c"), "/a/b/c");
    }
}
