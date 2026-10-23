//! `wtd open`: show a worktree's session in a frontend — VS Code, Zed, or a plain terminal.
//!
//! The default frontend is `frontend` in `.wtd/config.json` (`wtd frontend <name>` sets it); the tray
//! and other launchers go through here so every entry point opens sessions the same way.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use windows_sys::Win32::System::Threading::CREATE_NEW_CONSOLE;

use crate::{paths, settings, win};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Frontend {
    VsCode,
    Zed,
    Terminal,
}

impl Frontend {
    pub const ALL: [Frontend; 3] = [Frontend::VsCode, Frontend::Zed, Frontend::Terminal];

    pub fn parse(s: &str) -> Option<Frontend> {
        match s.trim().to_ascii_lowercase().as_str() {
            "vscode" | "code" => Some(Frontend::VsCode),
            "zed" => Some(Frontend::Zed),
            "terminal" | "term" => Some(Frontend::Terminal),
            _ => None,
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Frontend::VsCode => "vscode",
            Frontend::Zed => "zed",
            Frontend::Terminal => "terminal",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Frontend::VsCode => "VS Code",
            Frontend::Zed => "Zed",
            Frontend::Terminal => "Terminal",
        }
    }
}

pub fn default_frontend(dev: &Path) -> Frontend {
    settings::load_config(dev).get("frontend").and_then(Value::as_str).and_then(Frontend::parse).unwrap_or(Frontend::VsCode)
}

pub fn set_default_frontend(dev: &Path, f: Frontend) -> Result<()> {
    settings::update_config(dev, |cfg| cfg["frontend"] = Value::from(f.key()))
}

/// Open worktree `id` (`<slug>/<name>`) in `f`, starting its session if none is live.
pub fn open(dev: &Path, id: &str, f: Frontend) -> Result<()> {
    let (slug, name) = id.split_once('/').ok_or_else(|| anyhow!("expected <slug>/<name>, got '{id}'"))?;
    match f {
        Frontend::VsCode => {
            // focus the dev-root window first so the URI lands in it, then the extension opens the row
            let _ = win::no_window(Command::new("cmd").args(["/c", "code"]).arg(dev)).status();
            let url = format!("vscode://local.claude-status/open?slug={}&name={}", pct(slug), pct(name));
            win::no_window(Command::new("cmd").args(["/c", "code", "--open-url"]).arg(url)).spawn()?;
        }
        Frontend::Zed => {
            let wt = paths::worktree_path(dev, id);
            if !wt.is_dir() {
                bail!("{id} has no worktree yet — open it in VS Code or a terminal first");
            }
            let zed = zed_exe().ok_or_else(|| anyhow!("Zed not found (install it, or put its `zed` CLI on PATH)"))?;
            win::no_window(Command::new(zed).arg(&wt)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        }
        Frontend::Terminal => {
            let wtd = paths::wtd_exe();
            // Windows Terminal when present (a tab titled after the worktree), else a classic console
            let wt_tab = Command::new("wt.exe").args(["-w", "0", "nt", "--title", name, "-d"]).arg(dev)
                .arg(&wtd).args(["agent", slug, name]).spawn();
            if wt_tab.is_err() {
                Command::new(&wtd).args(["agent", slug, name]).current_dir(dev).creation_flags(CREATE_NEW_CONSOLE).spawn()?;
            }
        }
    }
    Ok(())
}

fn zed_exe() -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH").into_iter().flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .flat_map(|d| [d.join("zed.exe"), d.join("zed.cmd")]);
    let installed = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).into_iter()
        .flat_map(|l| [l.join("Programs").join("Zed").join("bin").join("zed.exe"), l.join("Programs").join("Zed").join("Zed.exe")]);
    on_path.chain(installed).find(|p| p.is_file())
}

fn pct(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        _ => format!("%{b:02X}"),
    }).collect()
}

/// `wtd open <slug>/<name> [--in vscode|zed|terminal]`
pub fn open_main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    let id = args.first().filter(|a| !a.starts_with("--")).ok_or_else(|| anyhow!("usage: wtd open <slug>/<name> [--in vscode|zed|terminal]"))?;
    let f = match args.iter().position(|a| a == "--in") {
        Some(i) => args.get(i + 1).and_then(|s| Frontend::parse(s)).ok_or_else(|| anyhow!("--in takes vscode|zed|terminal"))?,
        None => default_frontend(&dev),
    };
    open(&dev, id, f)?;
    Ok(0)
}

/// `wtd frontend [vscode|zed|terminal]` — show or set the default
pub fn frontend_main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    match args.first() {
        None => println!("{}", default_frontend(&dev).key()),
        Some(s) => {
            let f = Frontend::parse(s).ok_or_else(|| anyhow!("expected vscode|zed|terminal, got '{s}'"))?;
            set_default_frontend(&dev, f)?;
            println!("default frontend: {}", f.key());
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_encodes_slashes_and_spaces() {
        assert_eq!(pct("feat/big changes"), "feat%2Fbig%20changes");
    }

    #[test]
    fn parse_round_trips() {
        for f in Frontend::ALL {
            assert_eq!(Frontend::parse(f.key()), Some(f));
        }
    }
}
