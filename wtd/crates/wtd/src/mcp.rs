//! `wtd mcp`: stdio MCP server giving every agent read-only fleet awareness.
//!
//! Tools: `fleet_list`, `fleet_get`, `fleet_read_file`. Sending prompts to other worktrees
//! (`fleet_send`, user-approved) arrives in Phase 2.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use wtd_core::model::{Worktree, DEV_ID};
use wtd_core::protocol::method;

use crate::{client::Client, daemon, paths, win};

const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_PLAN_LINES: usize = 200;

pub fn main() -> Result<i32> {
    let dev = paths::dev_root()?;
    let me = std::env::var("CLAUDE_PROJECT_DIR")
        .ok()
        .map(|s| paths::normalize(&s))
        .or_else(|| std::env::current_dir().ok())
        .and_then(|d| paths::resolve_worktree(&dev, &d))
        .map(|r| r.id);

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // notifications need no reply
        let m = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let reply = match m {
            "initialize" => json!({
                "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "wtd", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "WorkTreeDev fleet tools. Other worktrees (parallel agents on other branches/repos) can be inspected read-only. Never modify another worktree.",
            }),
            "ping" => json!({}),
            "tools/list" => json!({ "tools": tools() }),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match call(&dev, me.as_deref(), name, &args) {
                    Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
                    Err(e) => json!({ "content": [{ "type": "text", "text": format!("error: {e:#}") }], "isError": true }),
                }
            }
            _ => {
                let err = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("method not found: {m}") } });
                writeln!(stdout, "{err}")?;
                stdout.flush()?;
                continue;
            }
        };
        writeln!(stdout, "{}", json!({ "jsonrpc": "2.0", "id": id, "result": reply }))?;
        stdout.flush()?;
    }
    Ok(0)
}

fn tools() -> Value {
    json!([
        {
            "name": "fleet_list",
            "description": "List every WorkTreeDev worktree: other agents working in parallel on other branches/repos. Shows id (slug/name), status (working | input = waiting on the user | reviewing | pr | done | stopped), whether a live agent session is attached, git state (branch, dirty, ahead/behind), and the title of its active plan. Your own worktree is marked self. Read-only.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "fleet_get",
            "description": "Inspect one worktree in depth: its status, git state, active plan (.claude/plans/active-plan.md), recent commits, files changed vs the default branch, and PR notes. Use the id from fleet_list. Read-only.",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "string", "description": "worktree id, e.g. luop/feat/billing" } }, "required": ["id"], "additionalProperties": false },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "fleet_read_file",
            "description": "Read a file from another worktree (path relative to that worktree's root). Read-only; never edit another worktree.",
            "inputSchema": { "type": "object", "properties": {
                "id": { "type": "string", "description": "worktree id from fleet_list" },
                "path": { "type": "string", "description": "file path relative to the worktree root" }
            }, "required": ["id", "path"], "additionalProperties": false },
            "annotations": { "readOnlyHint": true }
        }
    ])
}

fn call(dev: &Path, me: Option<&str>, name: &str, args: &Value) -> Result<String> {
    match name {
        "fleet_list" => {
            let wts = list(dev)?;
            let rows: Vec<Value> = wts
                .iter()
                .map(|w| {
                    json!({
                        "id": w.id, "self": me == Some(w.id.as_str()),
                        "kind": w.kind, "status": w.status.as_str(), "live": w.live,
                        "branch": w.git.branch, "dirty": w.git.dirty, "ahead": w.git.ahead, "behind": w.git.behind,
                        "plan": w.plan_title, "path": w.path,
                    })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&rows)?)
        }
        "fleet_get" => {
            let id = args.get("id").and_then(Value::as_str).context("id is required")?;
            let wt = list(dev)?.into_iter().find(|w| w.id == id).with_context(|| format!("no worktree '{id}' (see fleet_list)"))?;
            get(&wt, me)
        }
        "fleet_read_file" => {
            let id = args.get("id").and_then(Value::as_str).context("id is required")?;
            let rel = args.get("path").and_then(Value::as_str).context("path is required")?;
            read_confined(&paths::worktree_path(dev, id), rel)
        }
        _ => bail!("unknown tool '{name}'"),
    }
}

/// From the daemon when it's up (includes git state); straight from disk otherwise.
fn list(dev: &Path) -> Result<Vec<Worktree>> {
    if let Ok(Some(mut c)) = Client::connect() {
        if let Ok(v) = c.request(method::FLEET_LIST, json!({})) {
            return Ok(serde_json::from_value(v)?);
        }
    }
    Ok(daemon::scan::scan(dev))
}

fn git(dir: &str, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("--no-optional-locks").args(args).current_dir(dir).stdin(Stdio::null()).stderr(Stdio::null());
    win::no_window(&mut cmd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
        .unwrap_or_default()
}

fn get(w: &Worktree, me: Option<&str>) -> Result<String> {
    let mut s = String::new();
    let mut push = |h: &str, body: &str| {
        if !body.trim().is_empty() {
            s.push_str(&format!("## {h}\n{body}\n\n"));
        }
    };
    push(
        &format!("{}{}", w.id, if me == Some(w.id.as_str()) { " (self)" } else { "" }),
        &format!("status: {}\nlive session: {}\npath: {}", w.status.as_str(), w.live, w.path),
    );
    if w.id != DEV_ID {
        let base = git(&w.path, &["rev-parse", "--abbrev-ref", "origin/HEAD"]);
        push("Git", &git(&w.path, &["status", "--short", "--branch"]));
        push("Recent commits", &git(&w.path, &["log", "-10", "--format=%h %ad %s", "--date=relative"]));
        if !base.is_empty() {
            push(&format!("Changed vs {base}"), &git(&w.path, &["diff", "--stat", &format!("{base}...HEAD")]));
        }
    }
    let plan = std::fs::read_to_string(Path::new(&w.path).join(".claude").join("plans").join("active-plan.md")).unwrap_or_default();
    let plan: Vec<&str> = plan.lines().take(MAX_PLAN_LINES).collect();
    push("Active plan", &plan.join("\n"));
    push("PR notes", &std::fs::read_to_string(Path::new(&w.path).join("pr-notes.md")).unwrap_or_default());
    Ok(s)
}

/// Read `rel` under `root`, refusing anything that resolves outside it (`..`, absolute paths, links).
fn read_confined(root: &Path, rel: &str) -> Result<String> {
    let root = root.canonicalize().with_context(|| format!("no worktree at {}", root.display()))?;
    let target: PathBuf = root.join(rel.trim_start_matches(['/', '\\']));
    let target = target.canonicalize().with_context(|| format!("no file '{rel}'"))?;
    if !target.starts_with(&root) {
        bail!("'{rel}' is outside the worktree");
    }
    if !target.is_file() {
        bail!("'{rel}' is not a file");
    }
    let bytes = std::fs::read(&target)?;
    let truncated = bytes.len() > MAX_FILE_BYTES;
    let mut text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_FILE_BYTES)]).to_string();
    if truncated {
        text.push_str(&format!("\n… [truncated: {} of {} bytes shown]", MAX_FILE_BYTES, bytes.len()));
    }
    Ok(text)
}
