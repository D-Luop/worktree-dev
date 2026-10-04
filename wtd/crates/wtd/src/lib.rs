//! WorkTreeDev v2 for Windows: daemon, hook client, session wrapper, tray, MCP server, CLI.
//! Two binaries share this library: `wtd.exe` (console CLI) and `wtd-tray.exe` (windowless tray).

pub mod client;
pub mod daemon;
pub mod hook;
pub mod mcp;
pub mod paths;
pub mod run;
pub mod settings;
pub mod statusfile;
pub mod tray;
pub mod win;

use anyhow::Result;
use serde_json::json;
use wtd_core::model::Worktree;
use wtd_core::protocol::method;

const USAGE: &str = "\
wtd — WorkTreeDev fleet tool

  wtd daemon start|stop|status|run   control the background daemon
  wtd ls [--json]                    list worktrees (status, git, live session)
  wtd stop <slug/name>               end a worktree's live session (kills its process tree)
  wtd refresh [slug/name]            rescan worktrees and re-check git now
  wtd hook <event>                   Claude Code hook handler (reads hook JSON on stdin)
  wtd mcp                            stdio MCP server: read-only fleet tools for agents
  wtd tray [--spawn|--quit|--logon on|off]
                                     notification-area icon: start/stop the daemon, open VS Code
  wtd repo ls|add|rm|fetch|set-github … repos (JSON output; see `wtd repo`)
  wtd account ls|add|rm|use …        Claude / Codex logins and role defaults (JSON output)
  wtd env                            installed CLIs, GitHub login + scopes, tray-at-logon
  wtd run [--kind k] [--account a] -- <program> [args…]
                                     run a session inside a tracked job";

pub fn cli_main() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        // the hook must never fail or print: it runs inside every agent's tool calls
        Some("hook") => hook::main(&args[1..]),
        Some(cmd) => match dispatch(cmd, &args[1..]) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("wtd {cmd}: {e:#}");
                1
            }
        },
        None => {
            println!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

fn dispatch(cmd: &str, rest: &[String]) -> Result<i32> {
    match cmd {
        "daemon" => daemon::main(rest),
        "run" => run::main(rest),
        "mcp" => mcp::main(),
        "tray" => tray::main(rest),
        "repo" => settings::repo_main(rest),
        "account" => settings::account_main(rest),
        "env" => settings::env_main(rest),
        "ls" => ls(rest.iter().any(|a| a == "--json")),
        "stop" => {
            let id = rest.first().ok_or_else(|| anyhow::anyhow!("usage: wtd stop <slug/name>"))?;
            let r = client::Client::connect_required()?.request(method::SESSION_STOP, json!({ "id": id }))?;
            println!("stopped {} session(s) of {id}", r["stopped"]);
            Ok(0)
        }
        "refresh" => {
            let mut c = client::Client::connect_required()?;
            c.request(method::REFRESH, json!({ "id": rest.first() }))?;
            Ok(0)
        }
        "group" => group(rest),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(0)
        }
        _ => {
            eprintln!("{USAGE}");
            Ok(2)
        }
    }
}

fn ls(as_json: bool) -> Result<i32> {
    let v = client::Client::connect_required()?.request(method::FLEET_LIST, json!({}))?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    let wts: Vec<Worktree> = serde_json::from_value(v)?;
    println!("{:<34} {:<10} {:<5} {:<6} {}", "WORKTREE", "STATUS", "LIVE", "GIT", "PLAN");
    for w in wts {
        let git = if !w.git.known { "?".to_string() } else {
            format!("{}{}", if w.git.ahead > 0 { format!("↑{}", w.git.ahead) } else { String::new() }, if w.git.dirty { "●" } else { "" })
        };
        println!("{:<34} {:<10} {:<5} {:<6} {}", w.id, w.status.as_str(), if w.live { "yes" } else { "" }, git, w.plan_title.unwrap_or_default());
    }
    Ok(0)
}

/// `wtd group ls | add <name> | rename <id> <name> | rm <id> | assign <worktree> <id|none> | order <id>…`
fn group(rest: &[String]) -> Result<i32> {
    let mut c = client::Client::connect_required()?;
    let a: Vec<&str> = rest.iter().map(String::as_str).collect();
    let r = match a.as_slice() {
        [] | ["ls"] => {
            let snap = c.request(method::FLEET_LIST, json!({}))?;
            let wts: Vec<Worktree> = serde_json::from_value(snap)?;
            // groups come with the subscription snapshot; ask for one and read it
            c.request(method::SUBSCRIBE, json!({}))?;
            if let Some(wtd_core::protocol::ServerLine::Push(wtd_core::protocol::Push::Snapshot { snapshot, .. })) = c.read()? {
                for g in &snapshot.groups {
                    let n = wts.iter().filter(|w| w.group.as_deref() == Some(g.id.as_str())).count();
                    println!("{:<6} {:<30} {n} worktree(s){}", g.id, g.name, if g.collapsed { " (collapsed)" } else { "" });
                }
                println!("{:<6} {:<30} {} worktree(s)", "-", "Ungrouped", wts.iter().filter(|w| w.group.is_none() && w.id != "_dev").count());
            }
            return Ok(0);
        }
        ["add", name] => c.request(method::GROUP_CREATE, json!({ "name": name }))?,
        ["rename", id, name] => c.request(method::GROUP_UPDATE, json!({ "id": id, "name": name }))?,
        ["rm", id] => c.request(method::GROUP_DELETE, json!({ "id": id }))?,
        ["assign", wt, g] => c.request(method::GROUP_ASSIGN, json!({ "worktree": wt, "group": if *g == "none" { None } else { Some(*g) } }))?,
        ["order", ids @ ..] => c.request(method::GROUP_REORDER, json!({ "ids": ids }))?,
        _ => anyhow::bail!("usage: wtd group ls | add <name> | rename <id> <name> | rm <id> | assign <worktree> <id|none> | order <id>…"),
    };
    println!("{r}");
    Ok(0)
}
