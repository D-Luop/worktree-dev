//! Codex sessions: prepare a `CODEX_HOME` before `wtd run` launches `codex`.
//!
//! - hooks.json: the same status hooks as Claude Code (Codex uses Claude-compatible hook events and
//!   stdin JSON), pointing at `wtd hook`. Ours are replaced in place; the user's own are kept. Codex
//!   asks the user to trust new/changed hooks once (`/hooks`) — we deliberately don't bypass that.
//! - config.toml: the `wtd` MCP server (fleet tools), and the worktree marked as a trusted project so
//!   Codex doesn't stop at a trust prompt in every new worktree.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{json, Value};

/// `$CODEX_HOME`, else `~/.codex`.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| crate::paths::home_dir().ok().map(|h| h.join(".codex")))
}

pub fn prepare(codex_home: &Path, cwd: &Path) -> Result<()> {
    std::fs::create_dir_all(codex_home)?;
    let exe = crate::paths::wtd_exe();
    ensure_hooks(codex_home, &exe)?;
    let cfg = codex_home.join("config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap_or_default();
    let before = text.clone();
    set_mcp(&mut text, &exe);
    let key = format!("[projects.{}]", toml_lit(&cwd.to_string_lossy()));
    if !text.contains(&key) {
        push_block(&mut text, &format!("{key}\ntrust_level = \"trusted\"\n"));
    }
    if text != before {
        std::fs::write(&cfg, text)?;
    }
    Ok(())
}

const MCP_HEADER: &str = "[mcp_servers.wtd]";

/// The `[mcp_servers.wtd]` table, rewritten if it points at another exe.
fn set_mcp(text: &mut String, exe: &Path) {
    let block = format!("{MCP_HEADER}\ncommand = {}\nargs = [\"mcp\"]\n", toml_lit(&exe.to_string_lossy()));
    match table_span(text, MCP_HEADER) {
        Some(r) if text[r.clone()].trim_end() == block.trim_end() => {}
        Some(r) => {
            let sep = if r.end < text.len() { "\n" } else { "" };
            text.replace_range(r, &format!("{block}{sep}"));
        }
        None => push_block(text, &block),
    }
}

/// Byte range of a TOML table: its header line up to the next header (or the end).
fn table_span(text: &str, header: &str) -> Option<std::ops::Range<usize>> {
    let mut at = 0;
    let mut start = None;
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        match start {
            Some(s) if t.starts_with('[') => return Some(s..at),
            None if t == header => start = Some(at),
            _ => {}
        }
        at += line.len();
    }
    start.map(|s| s..text.len())
}

/// The fleet MCP server in a Codex home's config.toml (for `wtd setup`).
pub fn ensure_mcp(codex_home: &Path, exe: &Path) -> Result<()> {
    let cfg = codex_home.join("config.toml");
    let before = std::fs::read_to_string(&cfg).unwrap_or_default();
    let mut text = before.clone();
    set_mcp(&mut text, exe);
    if text != before {
        std::fs::write(&cfg, text)?;
    }
    Ok(())
}

pub fn remove_mcp(codex_home: &Path) -> Result<()> {
    let cfg = codex_home.join("config.toml");
    let Ok(mut text) = std::fs::read_to_string(&cfg) else { return Ok(()) };
    if let Some(r) = table_span(&text, MCP_HEADER) {
        text.replace_range(r, "");
        std::fs::write(&cfg, text.trim_end().to_string() + "\n")?;
    }
    Ok(())
}

/// Drop our status hooks from a Codex home's hooks.json, keeping the user's.
pub fn remove_hooks(codex_home: &Path) -> Result<()> {
    let path = codex_home.join("hooks.json");
    let Some(mut doc) = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) else { return Ok(()) };
    let Some(hooks) = doc.get_mut("hooks").and_then(Value::as_object_mut) else { return Ok(()) };
    let before = hooks.clone();
    for list in hooks.values_mut().filter_map(Value::as_array_mut) {
        list.retain(|e| !is_ours(e));
    }
    hooks.retain(|_, l| l.as_array().is_none_or(|a| !a.is_empty()));
    if *hooks != before {
        std::fs::write(&path, serde_json::to_string_pretty(&doc)? + "\n")?;
    }
    Ok(())
}

fn is_ours(entry: &Value) -> bool {
    entry.get("hooks").and_then(Value::as_array).is_some_and(|hs| {
        hs.iter().any(|h| h.get("command").and_then(Value::as_str).is_some_and(|c| c.contains("wtd.exe") && c.contains(" hook ")))
    })
}

/// TOML literal string (no escapes needed for Windows paths); falls back to a basic string if the
/// value itself contains a single quote.
fn toml_lit(s: &str) -> String {
    if s.contains('\'') {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("'{s}'")
    }
}

fn push_block(text: &mut String, block: &str) {
    if !text.is_empty() && !text.ends_with("\n\n") {
        text.push_str(if text.ends_with('\n') { "\n" } else { "\n\n" });
    }
    text.push_str(block);
}

/// Hook command line. Codex runs it through a shell; an unquoted path with no spaces works in
/// cmd, PowerShell and bash alike (a quoted one would be a string literal in PowerShell).
fn hook_cmd(exe: &Path, event: &str) -> String {
    let p = exe.to_string_lossy().replace('\\', "/");
    if p.contains(' ') { format!("\"{p}\" hook {event}") } else { format!("{p} hook {event}") }
}

fn ensure_hooks(codex_home: &Path, exe: &Path) -> Result<()> {
    let path = codex_home.join("hooks.json");
    let mut doc: Value = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({}));
    if !doc.is_object() {
        doc = json!({});
    }
    let ours = |ev: &str, matcher: Option<&str>| {
        let mut entry = json!({ "hooks": [{ "type": "command", "command": hook_cmd(exe, ev), "timeout": 10 }] });
        if let Some(m) = matcher {
            entry["matcher"] = json!(m);
        }
        entry
    };
    let wanted: Vec<(&str, Vec<Value>)> = vec![
        ("UserPromptSubmit", vec![ours("working", None)]),
        ("PreToolUse", vec![ours("tool", Some("*")), ours("edit", Some("apply_patch|Edit|Write")), ours("plan-nudge", Some("apply_patch|Edit|Write"))]),
        ("PermissionRequest", vec![ours("stop", None)]), // waiting on the user, like Claude's Notification
        ("Stop", vec![ours("stop", None)]),
        ("SessionEnd", vec![ours("sessionend", None)]),
    ];
    let hooks = doc.as_object_mut().unwrap().entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let mut changed = false;
    for (event, entries) in wanted {
        let list = hooks.as_object_mut().unwrap().entry(event).or_insert_with(|| json!([]));
        let mut keep: Vec<Value> = list.as_array().cloned().unwrap_or_default().into_iter().filter(|e| !is_ours(e)).collect();
        keep.extend(entries);
        if *list != Value::Array(keep.clone()) {
            *list = Value::Array(keep);
            changed = true;
        }
    }
    if changed {
        std::fs::write(&path, serde_json::to_string_pretty(&doc)? + "\n")?;
    }
    Ok(())
}

type Version = [u64; 3];

/// `0.163.0` out of `codex-cli 0.163.0` or a release dir name like `0.163.0-x86_64-pc-windows-msvc`.
fn parse_version(s: &str) -> Option<Version> {
    let tok = s.split_whitespace().find(|t| t.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut it = tok.split('-').next()?.split('.').map(|n| n.parse().ok());
    Some([it.next()??, it.next()??, it.next()??])
}

/// The newest app-server daemon release installed under any of `homes`
/// (`<home>/packages/app-server-daemon/releases/<ver>-<target>/`).
fn newest_daemon(homes: &[PathBuf]) -> Option<Version> {
    homes.iter()
        .filter_map(|h| std::fs::read_dir(h.join("packages").join("app-server-daemon").join("releases")).ok())
        .flat_map(|d| d.flatten())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| parse_version(&e.file_name().to_string_lossy()))
        .max()
}

fn cli_version() -> Option<Version> {
    let (exe, pre) = crate::run::command_for("codex");
    let mut c = std::process::Command::new(exe);
    crate::win::no_window(c.args(pre).arg("--version"));
    let o = c.stdin(std::process::Stdio::null()).output().ok()?;
    parse_version(&String::from_utf8_lossy(&o.stdout))
}

fn fmt(v: Version) -> String {
    format!("{}.{}.{}", v[0], v[1], v[2])
}

fn skew_warning(cli: Version, daemon: Version) -> Option<String> {
    (cli < daemon).then(|| format!(
        "warning: codex CLI {} is older than its background server {} (started by a newer Codex app) — sessions may stop on \
\"incompatible feature settings\". Fix: npm i -g @openai/codex@latest",
        fmt(cli), fmt(daemon)))
}

/// A one-line warning when the `codex` CLI is older than the newest app-server daemon a newer Codex
/// client installed, which then refuses the CLI's sessions.
pub fn version_skew(codex_home: &Path) -> Option<String> {
    let mut homes = vec![codex_home.to_path_buf()];
    homes.extend(crate::paths::home_dir().ok().map(|h| h.join(".codex")));
    let daemon = newest_daemon(&homes)?;
    skew_warning(cli_version()?, daemon)
}

/// The first file an `apply_patch` call touches (Codex's edit tool), from any string in its input.
pub fn patched_path(tool_input: &Value) -> Option<String> {
    fn scan(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => s.lines().find_map(|l| {
                ["*** Update File: ", "*** Add File: ", "*** Delete File: "].iter().find_map(|p| l.strip_prefix(p)).map(|p| p.trim().to_string())
            }),
            Value::Array(a) => a.iter().find_map(scan),
            Value::Object(o) => o.values().find_map(scan),
            _ => None,
        }
    }
    scan(tool_input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_patched_file() {
        let v = json!({ "input": "*** Begin Patch\n*** Update File: src/app.ts\n@@\n-a\n+b\n*** End Patch" });
        assert_eq!(patched_path(&v).as_deref(), Some("src/app.ts"));
        assert_eq!(patched_path(&json!({ "command": ["ls"] })), None);
    }

    #[test]
    fn version_compare() {
        assert_eq!(parse_version("codex-cli 0.160.0\n"), Some([0, 160, 0]));
        assert_eq!(parse_version("0.163.0-x86_64-pc-windows-msvc"), Some([0, 163, 0]));
        assert_eq!(parse_version("codex-cli"), None);
        assert_eq!(parse_version("0.163"), None);
        assert!(skew_warning([0, 160, 0], [0, 163, 0]).is_some_and(|w| w.contains("npm i -g @openai/codex@latest")));
        assert!(skew_warning([0, 163, 0], [0, 163, 0]).is_none());
        assert!(skew_warning([0, 164, 0], [0, 163, 0]).is_none());
        assert!(skew_warning([0, 99, 0], [0, 100, 0]).is_some(), "numeric, not lexical");
    }

    #[test]
    fn newest_daemon_release() {
        let t = std::env::temp_dir().join(format!("wtd-codex-daemon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let rel = |home: &str, v: &str| std::fs::create_dir_all(t.join(home).join("packages/app-server-daemon/releases").join(v)).unwrap();
        rel("a", "0.160.0-x86_64-pc-windows-msvc");
        rel("a", "0.99.0-x86_64-pc-windows-msvc");
        rel("b", "0.163.0-aarch64-apple-darwin");
        std::fs::write(t.join("a/packages/app-server-daemon/releases/9.9.9.tmp"), "").unwrap(); // not a dir
        assert_eq!(newest_daemon(&[t.join("a")]), Some([0, 160, 0]));
        assert_eq!(newest_daemon(&[t.join("a"), t.join("b"), t.join("missing")]), Some([0, 163, 0]));
        assert_eq!(newest_daemon(&[t.join("missing")]), None);
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn prepares_codex_home() {
        let t = std::env::temp_dir().join(format!("wtd-codex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("config.toml"), "model = \"o3\"\n").unwrap();
        std::fs::write(t.join("hooks.json"), r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"notify-send done"}]}]}}"#).unwrap();
        let wt = Path::new(r"D:\dev\worktree-dev\worktrees\luop\x");
        prepare(&t, wt).unwrap();
        prepare(&t, wt).unwrap(); // idempotent
        let cfg = std::fs::read_to_string(t.join("config.toml")).unwrap();
        assert!(cfg.starts_with("model = \"o3\"\n"), "{cfg}");
        assert_eq!(cfg.matches("[mcp_servers.wtd]").count(), 1);
        assert_eq!(cfg.matches(r"[projects.'D:\dev\worktree-dev\worktrees\luop\x']").count(), 1);
        let hooks: Value = serde_json::from_str(&std::fs::read_to_string(t.join("hooks.json")).unwrap()).unwrap();
        let stop = hooks["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "user's own Stop hook kept + ours");
        assert!(stop[0]["hooks"][0]["command"] == "notify-send done");
        assert_eq!(hooks["hooks"]["PreToolUse"].as_array().unwrap().len(), 3);
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn setup_mcp_and_uninstall() {
        let t = std::env::temp_dir().join(format!("wtd-codex-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        let projects = "[projects.'D:\\x']\ntrust_level = \"trusted\"\n";
        std::fs::write(t.join("config.toml"), format!("model = \"o3\"\n\n[mcp_servers.wtd]\ncommand = 'C:\\old\\wtd.exe'\nargs = [\"mcp\"]\n\n{projects}")).unwrap();
        ensure_mcp(&t, Path::new(r"D:\new\wtd.exe")).unwrap();
        let cfg = std::fs::read_to_string(t.join("config.toml")).unwrap();
        assert_eq!(cfg, format!("model = \"o3\"\n\n[mcp_servers.wtd]\ncommand = 'D:\\new\\wtd.exe'\nargs = [\"mcp\"]\n\n{projects}"));
        ensure_mcp(&t, Path::new(r"D:\new\wtd.exe")).unwrap();
        assert_eq!(std::fs::read_to_string(t.join("config.toml")).unwrap(), cfg, "idempotent");
        remove_mcp(&t).unwrap();
        assert_eq!(std::fs::read_to_string(t.join("config.toml")).unwrap(), format!("model = \"o3\"\n\n{projects}"));

        let user = json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"notify-send done"}]}]}});
        std::fs::write(t.join("hooks.json"), user.to_string()).unwrap();
        ensure_hooks(&t, Path::new(r"D:\new\wtd.exe")).unwrap();
        remove_hooks(&t).unwrap();
        let hooks: Value = serde_json::from_str(&std::fs::read_to_string(t.join("hooks.json")).unwrap()).unwrap();
        assert_eq!(hooks, user);
        std::fs::remove_dir_all(&t).ok();
    }
}
