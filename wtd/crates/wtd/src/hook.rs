//! `wtd hook <event>`: Claude Code hook handler (and `agent done|pr|wip`, the reviewer's
//! `reviewing|reviewed`). Runs on every tool call of every agent, so it does the minimum: read the
//! hook JSON, apply the status machine to the worktree's files, ping the daemon, exit 0.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use serde::Deserialize;
use wtd_core::protocol::{method, HookParams};
use wtd_core::status::{apply, Event};

use crate::{client::Client, paths, statusfile};

#[derive(Deserialize, Default)]
struct HookInput {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
    #[serde(default)]
    tool_response: Option<ToolResponse>,
}
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ToolResponse {
    #[serde(default)]
    file_path: Option<String>,
}

/// Never fails the caller: a hook error must not disturb the agent.
pub fn main(args: &[String]) -> i32 {
    if let Err(e) = run(args) {
        if std::env::var_os("WTD_DEBUG").is_some() {
            eprintln!("wtd hook: {e:#}");
        }
    }
    0
}

fn run(args: &[String]) -> anyhow::Result<()> {
    let word = args.first().map(String::as_str).unwrap_or("");
    let mut input = HookInput::default();
    if !std::io::stdin().is_terminal() {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        if !buf.trim().is_empty() {
            input = serde_json::from_str(&buf).unwrap_or_default();
        }
    }
    // Claude: tool_input.file_path. Codex edits through apply_patch: the path is inside the patch.
    let mut path = input
        .tool_input
        .as_ref()
        .and_then(|t| t.get("file_path").and_then(|p| p.as_str()).map(String::from))
        .or_else(|| input.tool_response.and_then(|t| t.file_path))
        .or_else(|| input.tool_input.as_ref().and_then(crate::codex::patched_path))
        .unwrap_or_default();
    // relative paths (Codex patches) → absolute, so the scratch-file checks see `/pr-notes.md` etc.
    if !path.is_empty() && !std::path::Path::new(&path).is_absolute() && !path.starts_with('/') {
        if let Some(c) = input.cwd.as_deref() {
            path = format!("{}/{}", c.trim_end_matches(['/', '\\']), path);
        }
    }
    let Some(ev) = Event::from_word(word, &path) else { anyhow::bail!("unknown event '{word}'") };

    let dir: PathBuf = std::env::var("CLAUDE_PROJECT_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .or(input.cwd)
        .map(|s| paths::normalize(&s))
        .unwrap_or(std::env::current_dir()?);
    let dev = paths::dev_root()?;
    let Some(wt) = paths::resolve_worktree(&dev, &dir) else { return Ok(()) }; // not ours

    let old = statusfile::read(&wt.root);
    let new = apply(old, &ev);
    let changed = new != old;
    if changed {
        statusfile::write(&dev, &wt.id, &wt.root, old, new)?;
    }
    // Ping the daemon so the UI updates instantly. Unchanged tool calls still count as activity (they
    // schedule a git refresh), but skip the connect for them unless the daemon is up — connect() on a
    // missing pipe fails in microseconds, so this stays cheap either way.
    if let Ok(Some(mut c)) = Client::connect() {
        let _ = c.notify(method::HOOK, HookParams {
            dir: wt.root.to_string_lossy().into(),
            event: word.into(),
            status: new.status,
            changed,
        });
    }
    // Turn ended: ring the terminal bell so VSCode badges the session's tab while it's unfocused.
    if matches!(ev, Event::Stop) {
        if let Ok(mut con) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
            let _ = con.write_all(b"\x07");
        }
    }
    Ok(())
}
