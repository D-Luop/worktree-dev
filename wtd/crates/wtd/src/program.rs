//! The program dir (`%LOCALAPPDATA%\WorkTreeDev`): `wtd version`, and `wtd setup --migrate`, which
//! moves a dev-mode install (binaries and templates inside the dev-root checkout) into it.
//!
//! The rest of `wtd setup` belongs to the installer; only `--migrate` is handled here.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::{paths, win};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn version_main(_args: &[String]) -> Result<i32> {
    let layout = paths::Layout::detect();
    println!("{}", serde_json::to_string_pretty(&json!({
        "version": VERSION,
        "channel": option_env!("WTD_CHANNEL").unwrap_or("stable"),
        "program_dir": paths::program_dir(),
        "dev_root": layout.dev,
        "commit": option_env!("WTD_COMMIT"),
        "dev_mode": layout.dev_mode(),
    }))?);
    Ok(0)
}

pub fn setup_main(args: &[String]) -> Result<i32> {
    if args.iter().any(|a| a == "--migrate") {
        return migrate();
    }
    bail!("this build only supports `wtd setup --migrate`")
}

const ASSET_DIRS: [&str; 3] = ["templates", "hooks", "scripts"];
const EXES: [&str; 2] = ["wtd.exe", "wtd-tray.exe"];

/// Copy the checkout's program files into `versions\<ver>\`, point `current\` at it, pin the program
/// dir, and repoint configs that name `<dev>\.wtd\{bin,hooks,scripts}`. Worktrees, repos and
/// `.wtd\state` are not touched, and `<dev>\.wtd\bin` is left in place for whatever still runs from it.
fn migrate() -> Result<i32> {
    let dev = paths::dev_root()?;
    let src = dev.join(".wtd");
    if !src.join("templates").is_dir() || !src.join("bin").join("wtd.exe").is_file() {
        bail!("{} is not a dev-mode install (no .wtd\\templates or .wtd\\bin\\wtd.exe)", dev.display());
    }
    let prog = paths::program_dir();
    let ver_dir = prog.join("versions").join(VERSION);
    println!("==> copying program files to {}", ver_dir.display());
    lay_down(&src, &ver_dir)?;
    let current = prog.join("current");
    let how = point_current(&current, &ver_dir)?;
    println!("    {} -> {} ({how})", current.display(), ver_dir.display());

    if let Some(pin) = paths::program_dir_pin() {
        std::fs::create_dir_all(pin.parent().unwrap())?;
        std::fs::write(&pin, format!("{}\n", prog.display()))?;
        println!("    pinned program dir -> {} ({})", prog.display(), pin.display());
    }

    println!("==> repointing hooks, shims and MCP registrations");
    let pairs = repoint_pairs(&src, &current);
    for f in config_files()? {
        match repoint_file(&f, &pairs) {
            Ok(true) => println!("    updated {}", f.display()),
            Ok(false) => {}
            Err(e) => eprintln!("    warning: {}: {e:#}", f.display()),
        }
    }
    for r in crate::repos::all(&dev) {
        if let Err(e) = crate::repos::prepare(&dev, &r) {
            eprintln!("    warning: git hooks for '{}': {e:#}", r.slug);
        }
    }
    crate::tray::refresh_start_at_logon();

    if paths::dev_mode() {
        println!("\nWTD_DEV_SOURCE=1 is set, so this shell still runs from the checkout.");
    }
    println!("\nDone. Restart the daemon and tray to run the program-dir build:\n  \"{}\" daemon restart\n  \"{}\" tray --spawn",
        current.join("bin").join("wtd.exe").display(), current.join("bin").join("wtd.exe").display());
    Ok(0)
}

/// `templates\`, `hooks\`, `scripts\`, `bin\wtd*.exe`, `claude-status.vsix`, `version.txt`.
fn lay_down(src: &Path, dst: &Path) -> Result<()> {
    for d in ASSET_DIRS {
        if src.join(d).is_dir() {
            crate::tools::copy_filtered(&src.join(d), &dst.join(d), &|p| p.extension().is_some_and(|e| e == "vsix"))?;
        }
    }
    std::fs::create_dir_all(dst.join("bin"))?;
    for exe in EXES {
        let from = src.join("bin").join(exe);
        if from.is_file() {
            copy_replacing(&from, &dst.join("bin").join(exe))?;
        }
    }
    if let Some(vsix) = newest_vsix(&src.join("templates").join("vscode-claude-status")) {
        std::fs::copy(vsix, dst.join("claude-status.vsix"))?;
    }
    std::fs::write(dst.join("version.txt"), format!("{VERSION}\n"))?;
    Ok(())
}

fn newest_vsix(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "vsix"))
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

/// A running exe can be renamed but not overwritten: move it aside first.
fn copy_replacing(from: &Path, to: &Path) -> Result<()> {
    if std::fs::copy(from, to).is_ok() {
        return Ok(());
    }
    let aside = to.with_extension(format!("exe.old-{}", std::process::id()));
    std::fs::rename(to, &aside).with_context(|| format!("replacing {}", to.display()))?;
    std::fs::copy(from, to).with_context(|| format!("copying to {}", to.display()))?;
    Ok(())
}

/// `current` as a junction to `target`, or a plain copy if junctions fail.
fn point_current(current: &Path, target: &Path) -> Result<&'static str> {
    if let Ok(m) = std::fs::symlink_metadata(current) {
        if m.file_type().is_symlink() {
            std::fs::remove_dir(current)?;
        } else {
            crate::settings::remove_tree(current).with_context(|| format!("removing {}", current.display()))?;
        }
    }
    let mut c = Command::new("cmd");
    c.args(["/C", "mklink", "/J"]).arg(current).arg(target).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    if win::no_window(&mut c).status().is_ok_and(|s| s.success()) && current.join("version.txt").is_file() {
        return Ok("junction");
    }
    crate::tools::copy_filtered(target, current, &|_| false)?;
    Ok("copy")
}

/// Every spelling of `<dev>\.wtd\<d>\` a config might hold, mapped to the same spelling of `current\<d>\`.
fn repoint_pairs(src: &Path, current: &Path) -> Vec<(String, String)> {
    let mut pairs = vec![];
    for d in ["bin", "hooks", "scripts"] {
        let (from, to) = (src.join(d).to_string_lossy().to_string(), current.join(d).to_string_lossy().to_string());
        let fwd = |s: &str| s.replace('\\', "/");
        let msys = |s: &str| {
            let f = fwd(s);
            match f.as_bytes() {
                [l, b':', b'/', ..] if l.is_ascii_alphabetic() => format!("/{}{}", (*l as char).to_ascii_lowercase(), &f[2..]),
                _ => f,
            }
        };
        pairs.push((format!("{from}\\"), format!("{to}\\")));
        pairs.push((format!("{}\\\\", from.replace('\\', "\\\\")), format!("{}\\\\", to.replace('\\', "\\\\"))));
        pairs.push((format!("{}/", fwd(&from)), format!("{}/", fwd(&to))));
        pairs.push((format!("{}/", msys(&from)), format!("{}/", msys(&to))));
    }
    pairs
}

/// Case-insensitive replace of each `from` with its `to` (Windows paths compare without case).
fn repoint(text: &str, pairs: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for (from, to) in pairs {
        let lower = out.to_ascii_lowercase();
        let needle = from.to_ascii_lowercase();
        let mut res = String::with_capacity(out.len());
        let mut last = 0;
        for (i, _) in lower.match_indices(&needle) {
            res.push_str(&out[last..i]);
            res.push_str(to);
            last = i + needle.len();
        }
        res.push_str(&out[last..]);
        out = res;
    }
    out
}

fn repoint_file(f: &Path, pairs: &[(String, String)]) -> Result<bool> {
    let Ok(text) = std::fs::read_to_string(f) else { return Ok(false) };
    let new = repoint(&text, pairs);
    if new == text {
        return Ok(false);
    }
    std::fs::write(f, new)?;
    Ok(true)
}

/// Claude settings and MCP registrations (default login + every account), Codex hooks and config,
/// and the `~/.local/bin` shims.
fn config_files() -> Result<Vec<PathBuf>> {
    let home = paths::home_dir()?;
    let mut files = vec![home.join(".claude.json"), home.join(".claude").join("settings.json")];
    let subdirs = |d: PathBuf| std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect::<Vec<_>>();
    for a in subdirs(home.join(".claude-accounts")) {
        files.extend([a.join(".claude.json"), a.join("settings.json")]);
    }
    let mut codex: Vec<PathBuf> = crate::codex::home().into_iter().collect();
    codex.extend(subdirs(home.join(".codex-accounts")));
    for c in codex {
        files.extend([c.join("hooks.json"), c.join("config.toml")]);
    }
    files.extend(std::fs::read_dir(home.join(".local").join("bin")).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() < 64 * 1024)));
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs() -> Vec<(String, String)> {
        repoint_pairs(Path::new(r"D:\dev\wd\.wtd"), Path::new(r"C:\Users\me\AppData\Local\WorkTreeDev\current"))
    }

    #[test]
    fn repoints_every_spelling() {
        let p = pairs();
        assert_eq!(repoint(r#"{"command":"D:\\dev\\wd\\.wtd\\bin\\wtd.exe hook stop"}"#, &p),
            r#"{"command":"C:\\Users\\me\\AppData\\Local\\WorkTreeDev\\current\\bin\\wtd.exe hook stop"}"#);
        assert_eq!(repoint("command = 'D:\\dev\\wd\\.wtd\\bin\\wtd.exe'", &p), "command = 'C:\\Users\\me\\AppData\\Local\\WorkTreeDev\\current\\bin\\wtd.exe'");
        assert_eq!(repoint("d:/dev/wd/.wtd/hooks/plan-nudge.sh", &p), "C:/Users/me/AppData/Local/WorkTreeDev/current/hooks/plan-nudge.sh");
        assert_eq!(repoint("w=/d/dev/wd/.wtd/bin/wtd.exe", &p), "w=/c/Users/me/AppData/Local/WorkTreeDev/current/bin/wtd.exe");
    }

    #[test]
    fn leaves_user_data_alone() {
        let p = pairs();
        for s in [r"D:\dev\wd\.wtd\state\x", "D:/dev/wd/.wtd/custom/hooks.json", "D:/dev/wd/.wtd/binx/y", "D:/dev/wd/worktrees/a/b"] {
            assert_eq!(repoint(s, &p), s);
        }
    }

    #[test]
    fn lays_down_current_shape() {
        let t = std::env::temp_dir().join(format!("wtd-laydown-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let src = t.join(".wtd");
        for f in ["templates/CLAUDE.md", "templates/vscode-claude-status/claude-status-1.0.0.vsix", "hooks/wt-status.sh", "scripts/agent.sh", "bin/wtd.exe", "state/keep"] {
            let p = src.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        let dst = t.join("versions").join("1.0.0");
        lay_down(&src, &dst).unwrap();
        for f in ["templates/CLAUDE.md", "hooks/wt-status.sh", "scripts/agent.sh", "bin/wtd.exe", "claude-status.vsix", "version.txt"] {
            assert!(dst.join(f).is_file(), "{f}");
        }
        assert!(!dst.join("state").exists());
        assert!(!dst.join("templates/vscode-claude-status/claude-status-1.0.0.vsix").exists());
        let cur = t.join("current");
        point_current(&cur, &dst).unwrap();
        assert!(cur.join("bin/wtd.exe").is_file());
        point_current(&cur, &dst).unwrap();
        assert!(cur.join("version.txt").is_file());
        let _ = std::fs::remove_dir(&cur);
        std::fs::remove_dir_all(&t).ok();
    }
}
