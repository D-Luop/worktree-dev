//! `wtd mcp`: stdio MCP server giving every agent read-only fleet awareness.
//!
//! Tools: `fleet_list`, `fleet_get`, `fleet_read_file` (read-only), `fleet_send` (a prompt to another
//! worktree's agent — held by the daemon until the user approves it in VSCode), `fleet_inbox`,
//! `preset_list` (what presets a repo's sessions get) and `preset_propose` (a preset change the user
//! must accept before anything is written).

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
                "instructions": "WorkTreeDev fleet tools. Other worktrees (parallel agents on other branches/repos) can be inspected read-only — never modify another worktree. To message another worktree's agent, first propose the message and the reason to the user in chat; only call fleet_send if they agree. The user then approves it again in VS Code before anything is delivered — approval said in chat doesn't release it.",
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
            "description": "List every WorkTreeDev worktree: other agents working in parallel on other branches/repos. Shows id (slug/name), status (working | input = waiting on the user | limited = hit a usage limit, waiting for limit_resets_at | reviewing | pr | done | stopped), whether a live agent session is attached, git state (branch, dirty, ahead/behind), the title of its active plan, and its platform (windows | wsl:<distro>, with wsl_path inside the distro). Your own worktree is marked self. Read-only.",
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
            "name": "fleet_send",
            "description": "Send a prompt to ANOTHER worktree's agent (e.g. to coordinate on a shared API or ask about unmerged work). ONLY call this after you proposed the exact message and the reason to the user in chat and they agreed. Nothing is delivered until the user approves it in VS Code (they may edit or deny it); then it reaches the target agent when its current turn ends. Never send unprompted, never to yourself.",
            "inputSchema": { "type": "object", "properties": {
                "to": { "type": "string", "description": "target worktree id from fleet_list, e.g. luop/feat/billing" },
                "message": { "type": "string", "description": "the prompt for the other agent; self-contained (it can't see your conversation)" }
            }, "required": ["to", "message"], "additionalProperties": false }
        },
        {
            "name": "fleet_inbox",
            "description": "Messages this worktree sent or received through fleet_send, with their state (pending = waiting for the user's approval in VS Code, queued = approved and waiting for the target's turn to end, delivered, denied, expired = the target worktree was removed first). Read-only.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "preset_list",
            "description": "The WorkTreeDev presets a repo's sessions get on this platform: skills, subagents, commands, rules (each with the layer it comes from: built-in, global, platform:<os>, repo:<slug>, repo:<slug>+<os>), CLAUDE.md / AGENTS.md additions, hooks and tool dirs. Read-only.",
            "inputSchema": { "type": "object", "properties": {
                "repo": { "type": "string", "description": "repo slug; defaults to this worktree's repo" },
                "platform": { "type": "string", "enum": ["windows", "wsl", "linux", "mac"] }
            }, "additionalProperties": false },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "preset_propose",
            "description": "Propose a change to the user's WorkTreeDev presets (.wtd/custom): a skill, subagent, command, rule, CLAUDE.md / AGENTS.md additions, hooks.json or the disabled list, for every repo, one repo, and/or one platform. Nothing is written until the user accepts it in VS Code (they see a diff) or with `wtd custom accept <id>`. Only propose after the user asked for it or agreed in chat; tell them it is waiting for their approval. `content` is the whole new file (omit it with delete=true to remove the file).",
            "inputSchema": { "type": "object", "properties": {
                "path": { "type": "string", "description": "CLAUDE.md | AGENTS.md | hooks.json | disabled | skills/<name>/<file> | agents/<name>.md | commands/<name>.md | rules/<name>.md" },
                "content": { "type": "string", "description": "the complete new file contents" },
                "delete": { "type": "boolean", "description": "remove the file instead" },
                "repo": { "type": "string", "description": "only this repo's worktrees (omit for every repo)" },
                "platform": { "type": "string", "enum": ["windows", "wsl", "linux", "mac"], "description": "only on this platform (omit for all)" },
                "reason": { "type": "string", "description": "one line the user sees: why this change" }
            }, "required": ["path", "reason"], "additionalProperties": false }
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
                        "plan": w.plan_title, "path": w.path, "platform": w.platform, "wsl_path": w.wsl_path,
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
        "fleet_send" => {
            let me = me.context("can't tell which worktree this session is in")?;
            let to = args.get("to").and_then(Value::as_str).context("to is required")?;
            let body = args.get("message").and_then(Value::as_str).context("message is required")?;
            let mut c = Client::connect()?.context("the WorkTreeDev daemon isn't running, so messages can't be sent — ask the user to start it (tray icon or fleet panel)")?;
            let r = c.request(method::MESSAGE_SEND, json!({ "from_dir": paths::worktree_path(dev, me).to_string_lossy(), "to": to, "body": body }))?;
            Ok(format!(
                "Message #{} to {to} is waiting for the user's approval in VS Code: the notification, the \"awaiting approval\" status-bar item, or the 💬 on your row in the Fleet panel (Approve / Edit / Deny). Saying \"approved\" in chat doesn't release it. Once approved it's delivered when {to}'s agent finishes its current turn. Tell the user where to approve it.",
                r["id"]
            ))
        }
        "fleet_inbox" => {
            let me = me.context("can't tell which worktree this session is in")?;
            let mut c = Client::connect()?.context("the WorkTreeDev daemon isn't running")?;
            let v = c.request(method::MESSAGE_LIST, json!({ "worktree": me }))?;
            let msgs = v.as_array().cloned().unwrap_or_default();
            if msgs.is_empty() {
                return Ok("No messages.".into());
            }
            Ok(msgs.iter().map(|m| {
                let dir = if m["from"] == me { format!("→ to {}", m["to"].as_str().unwrap_or("?")) } else { format!("← from {}", m["from"].as_str().unwrap_or("?")) };
                let state = match m["state"].as_str().unwrap_or("?") {
                    "pending" => "pending — awaiting the user's approval in VS Code",
                    "queued" => "queued — approved, delivered when the target's turn ends",
                    "expired" => "expired — the target worktree was removed",
                    s => s,
                };
                let sent = crate::daemon::fmt_local(m["created"].as_i64().unwrap_or(0));
                format!("#{} {dir} [{state}] · sent {sent}\n{}", m["id"], m["body"].as_str().unwrap_or(""))
            }).collect::<Vec<_>>().join("\n\n"))
        }
        "preset_list" => {
            let repo = args.get("repo").and_then(Value::as_str).map(String::from).or_else(|| me.and_then(|m| m.split_once('/')).map(|(s, _)| s.to_string())).unwrap_or_default();
            let os = args.get("platform").and_then(Value::as_str).unwrap_or(crate::custom::platform());
            Ok(serde_json::to_string_pretty(&crate::custom::effective(dev, &repo, os))?)
        }
        "preset_propose" => {
            let s = |k: &str| args.get(k).and_then(Value::as_str);
            let path = s("path").context("path is required")?;
            let delete = args.get("delete").and_then(Value::as_bool).unwrap_or(false);
            let content = if delete { None } else { Some(s("content").context("content is required (or delete: true)")?) };
            let p = crate::proposal::create(dev, me, s("repo"), s("platform"), path, content, s("reason").unwrap_or(""))?;
            Ok(format!(
                "Proposal {} ({} {}) is waiting for the user's approval in VS Code, or `wtd custom accept {}`. Nothing changes until they accept. Let the user know it's waiting for them.",
                p.id, if delete { "delete" } else { "write" }, crate::proposal::target(dev, &p)?.display(), p.id
            ))
        }
        "fleet_read_file" => {
            let id = args.get("id").and_then(Value::as_str).context("id is required")?;
            let rel = args.get("path").and_then(Value::as_str).context("path is required")?;
            let root = list(dev)?.into_iter().find(|w| w.id == id).map(|w| PathBuf::from(w.path)).unwrap_or_else(|| paths::worktree_path(dev, id));
            read_confined(&root, rel)
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
        &format!(
            "status: {}\nlive session: {}\nplatform: {}\npath: {}{}",
            w.status.as_str(),
            w.live,
            w.platform,
            w.path,
            w.wsl_path.as_deref().map(|p| format!("\nwsl_path: {p}")).unwrap_or_default()
        ),
    );
    // Windows git can't run against a WSL worktree through \\wsl.localhost
    if w.id != DEV_ID && !w.platform.is_wsl() {
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
