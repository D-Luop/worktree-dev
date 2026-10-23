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
            let wt = paths::worktree_root(dev, id);
            if !wt.is_dir() {
                bail!("{id} has no worktree yet — open it in VS Code or a terminal first");
            }
            let zed = zed_exe().ok_or_else(|| anyhow!("Zed not found (install it, or put its `zed` CLI on PATH)"))?;
            if let Some(r) = crate::repos::find(dev, slug) {
                let _ = crate::repos::prepare(dev, &r); // keeps .zed/tasks.json out of `git status`
            }
            ensure_zed_task(&wt, slug, name)?;
            ensure_zed_key();
            win::no_window(Command::new(zed).arg(&wt)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        }
        // the TUI: roster + this session + its changes; it starts the session if needed
        Frontend::Terminal => console(dev, name, &["tui".into(), id.into()])?,
    }
    Ok(())
}

/// Start the New Session flow in `f`: VS Code's wizard, the TUI's form, or `wtd new` then Zed.
pub fn new_session(dev: &Path, f: Frontend) -> Result<()> {
    if f == Frontend::VsCode {
        let _ = win::no_window(Command::new("cmd").args(["/c", "code"]).arg(dev)).status();
        win::no_window(Command::new("cmd").args(["/c", "code", "--open-url", "vscode://local.claude-status/new"])).spawn()?;
        return Ok(());
    }
    let args: Vec<String> = if f == Frontend::Zed { vec!["new".into(), "--in".into(), "zed".into()] } else { vec!["tui".into(), "--new".into()] };
    console(dev, "New session", &args)
}

/// Run `wtd <args>` in a Windows Terminal tab when present, else a classic console.
fn console(dev: &Path, title: &str, args: &[String]) -> Result<()> {
    let wtd = paths::wtd_exe();
    let tab = Command::new("wt.exe").args(["-w", "0", "nt", "--title", title, "-d"]).arg(dev).arg(&wtd).args(args).spawn();
    if tab.is_err() {
        Command::new(&wtd).args(args).current_dir(dev).creation_flags(CREATE_NEW_CONSOLE).spawn()?;
    }
    Ok(())
}

pub const ZED_TASK: &str = "WorkTreeDev: session";
pub const ZED_KEY: &str = "alt-shift-w";

/// Zed won't run anything when a folder opens, so the session is a task in the worktree's
/// `.zed/tasks.json` (upserted by label, other tasks kept) that ZED_KEY spawns in Zed's terminal:
/// `wtd tui` on this worktree, i.e. its session plus the roster and diffs.
pub fn ensure_zed_task(wt: &Path, slug: &str, name: &str) -> Result<()> {
    let path = wt.join(".zed").join("tasks.json");
    let mut tasks: Vec<Value> = match std::fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(Value::Array(a)) => a,
            _ => bail!("{} isn't plain JSON; not touching it", path.display()),
        },
        Err(_) => Vec::new(),
    };
    let task = serde_json::json!({
        "label": ZED_TASK,
        "command": paths::wtd_exe().to_string_lossy().replace('\\', "/"),
        "args": ["tui", format!("{slug}/{name}")],
        "use_new_terminal": false,
        "allow_concurrent_runs": false,
        "reveal": "always",
        "hide": "never",
        "tags": ["wtd"],
    });
    match tasks.iter_mut().find(|t| t["label"] == ZED_TASK) {
        Some(t) if *t == task => return Ok(()),
        Some(t) => *t = task,
        None => tasks.push(task),
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, serde_json::to_string_pretty(&tasks)? + "\n")?;
    Ok(())
}

/// Bind ZED_KEY to the session task in Zed's user keymap, unless it's already bound or the keymap
/// has comments (JSONC we can't rewrite without losing them).
fn ensure_zed_key() {
    let Some(p) = std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Zed").join("keymap.json")) else { return };
    let mut map: Vec<Value> = match std::fs::read_to_string(&p) {
        Ok(s) if s.trim().is_empty() => Vec::new(),
        Ok(s) => match serde_json::from_str(&s) {
            Ok(Value::Array(a)) => a,
            _ => return,
        },
        Err(_) => Vec::new(),
    };
    if map.iter().any(|b| b["bindings"].get(ZED_KEY).is_some()) {
        return;
    }
    map.push(serde_json::json!({ "bindings": { ZED_KEY: ["task::Spawn", { "task_name": ZED_TASK }] } }));
    if let (Some(dir), Ok(text)) = (p.parent(), serde_json::to_string_pretty(&map)) {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(&p, text + "\n");
    }
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
    fn zed_task_upserts_and_keeps_other_tasks() {
        let wt = std::env::temp_dir().join(format!("wtd-zed-{}", std::process::id()));
        let tasks = wt.join(".zed").join("tasks.json");
        std::fs::create_dir_all(tasks.parent().unwrap()).unwrap();
        std::fs::write(&tasks, r#"[{"label":"theirs","command":"make"}]"#).unwrap();
        ensure_zed_task(&wt, "luop", "feat/x").unwrap();
        ensure_zed_task(&wt, "luop", "feat/x").unwrap();
        let v: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&tasks).unwrap()).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[1]["label"], ZED_TASK);
        assert_eq!(v[1]["args"], serde_json::json!(["tui", "luop/feat/x"]));
        std::fs::write(&tasks, "// comment\n[]").unwrap();
        assert!(ensure_zed_task(&wt, "luop", "feat/x").is_err());
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn parse_round_trips() {
        for f in Frontend::ALL {
            assert_eq!(Frontend::parse(f.key()), Some(f));
        }
    }
}
