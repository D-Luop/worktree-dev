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
    if !text.contains("[mcp_servers.wtd]") {
        push_block(&mut text, &format!("[mcp_servers.wtd]\ncommand = {}\nargs = [\"mcp\"]\n", toml_lit(&exe.to_string_lossy())));
    }
    let key = format!("[projects.{}]", toml_lit(&cwd.to_string_lossy()));
    if !text.contains(&key) {
        push_block(&mut text, &format!("{key}\ntrust_level = \"trusted\"\n"));
    }
    if text != before {
        std::fs::write(&cfg, text)?;
    }
    Ok(())
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
        ("PreToolUse", vec![ours("tool", Some("*")), ours("edit", Some("apply_patch|Edit|Write"))]),
        ("PermissionRequest", vec![ours("stop", None)]), // waiting on the user, like Claude's Notification
        ("Stop", vec![ours("stop", None)]),
        ("SessionEnd", vec![ours("sessionend", None)]),
    ];
    let is_ours = |entry: &Value| {
        entry.get("hooks").and_then(Value::as_array).is_some_and(|hs| {
            hs.iter().any(|h| h.get("command").and_then(Value::as_str).is_some_and(|c| c.contains("wtd.exe") && c.contains(" hook ")))
        })
    };
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
        assert_eq!(hooks["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        std::fs::remove_dir_all(&t).ok();
    }
}
