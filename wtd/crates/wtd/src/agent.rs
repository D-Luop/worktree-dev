//! `wtd agent` — open/create a worktree and its agent session (the old agent.sh), plus `ls`, `stop`,
//! `rm`, `done|pr|wip`; and `wtd close`, `wtd archive`, `wtd assistant`.
//!
//!   agent <slug> <name> [--from <ref>] [--account <a>|codex:<a>|default] [--issue-file <md>]
//!                       [--no-claude] [--no-auto | --mode <m>] [ref-token…]
//!   agent ls | agent stop [<slug> <name>] | agent rm <slug> <name> [--branch] [--force] [-y]
//!   agent done | agent pr | agent wip            (inside a worktree)

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::json;
use wtd_core::protocol::method;
use wtd_core::status::{apply, Event};

use crate::client::Client;
use crate::gitx::{self, s};
use crate::wt::{self, session_key, session_name};
use crate::{paths, statusfile};

fn confirm(question: &str, default_yes: bool) -> bool {
    if !std::io::stdin().is_terminal() {
        return default_yes;
    }
    print!("{question} {} ", if default_yes { "[Y/n]" } else { "[y/N]" });
    let _ = std::io::stdout().flush();
    let mut a = String::new();
    let _ = std::io::stdin().lock().read_line(&mut a);
    match a.trim().to_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    }
}

/// The worktree the current directory is in.
fn here(dev: &Path) -> Result<paths::WtRef> {
    let cwd = std::env::current_dir()?;
    paths::resolve_worktree(dev, &cwd).with_context(|| format!("{} is not inside a worktree-dev worktree", cwd.display()))
}

fn apply_status(dev: &Path, r: &paths::WtRef, ev: Event, word: &str) -> Result<wtd_core::status::Status> {
    let old = statusfile::read(&r.root);
    let new = apply(old, &ev);
    if new != old {
        statusfile::write(dev, &r.id, &r.root, old, new)?;
    }
    if let Ok(Some(mut c)) = Client::connect() {
        let _ = c.notify(method::HOOK, wtd_core::protocol::HookParams { dir: s(&r.root), event: word.into(), status: new.status, changed: new != old });
    }
    Ok(new.status)
}

fn stop_sessions(id: &str) -> u64 {
    Client::connect().ok().flatten().and_then(|mut c| c.request(method::SESSION_STOP, json!({ "id": id })).ok()).and_then(|v| v["stopped"].as_u64()).unwrap_or(0)
}

pub fn main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    match args.first().map(String::as_str) {
        Some("done") | Some("pr") | Some("wip") | Some("working") => {
            let word = if args[0] == "working" { "wip" } else { args[0].as_str() };
            let r = here(&dev)?;
            if word == "pr" {
                crate::guard::check_pr(&dev, &r)?; // the repo's PR guardrails (no-op when none are enabled)
            }
            let st = apply_status(&dev, &r, Event::from_word(word, "").unwrap(), word)?;
            println!("marked {} as '{}'", r.root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), st.as_str());
            Ok(0)
        }
        Some("ls") | Some("sessions") | Some("ps") => crate::ls_cmd(false),
        Some("stop") | Some("kill") => {
            let id = match (args.get(1), args.get(2)) {
                (Some(slug), Some(name)) => format!("{slug}/{name}"),
                _ => here(&dev)?.id,
            };
            let root = paths::worktree_path(&dev, &id);
            if root.is_dir() {
                apply_status(&dev, &paths::WtRef { id: id.clone(), root }, Event::SessionEnd, "sessionend")?;
            }
            let n = stop_sessions(&id);
            println!("{}", if n > 0 { format!("stopped the session for {id} (worktree kept). Reopen: agent {}", id.replacen('/', " ", 1)) } else { format!("no running session for {id}") });
            Ok(0)
        }
        Some("rm") => rm(&dev, &args[1..]),
        Some(_) => launch(&dev, args),
        None => {
            println!("usage: agent <slug> <name> [--from <ref>] [--account <a>] [--issue-file <md>] [--no-claude] [ref…]");
            println!("       agent ls | agent stop [<slug> <name>] | agent rm <slug> <name> [--branch] [--force] [-y]");
            println!("       agent done | agent pr | agent wip   (inside a worktree)");
            Ok(1)
        }
    }
}

fn rm(dev: &Path, args: &[String]) -> Result<i32> {
    let (mut del_branch, mut force, mut yes) = (false, false, false);
    let mut pos = vec![];
    for a in args {
        match a.as_str() {
            "--branch" | "-b" => del_branch = true,
            "--force" | "-f" => force = true,
            "--yes" | "-y" => yes = true,
            o if o.starts_with('-') => bail!("agent rm: unknown option '{o}'"),
            o => pos.push(o.to_string()),
        }
    }
    let [slug, name] = pos.as_slice() else { bail!("usage: agent rm <slug> <name> [--branch] [--force] [-y]") };
    let wtp = paths::worktree_path(dev, &format!("{slug}/{name}"));
    println!("About to remove:\n  worktree : {}{}", wtp.display(), if wtp.is_dir() { "" } else { "  (missing)" });
    if del_branch {
        println!("  branch   : {name} (force-deleted)");
    }
    if force {
        println!("  forcing  : uncommitted changes will be discarded");
    }
    if !yes && !confirm("Proceed?", false) {
        println!("aborted");
        return Ok(1);
    }
    let log = match Client::connect()? {
        // the daemon owns it: stops the session, releases its file watch, then removes
        Some(mut c) => match c.request(method::WORKTREE_REMOVE, json!({ "id": format!("{slug}/{name}"), "force": force, "branch": del_branch })) {
            Ok(v) => serde_json::from_value::<Vec<String>>(v["log"].clone()).unwrap_or_default(),
            Err(e) if older_daemon(&e) => {
                stop_sessions(&format!("{slug}/{name}"));
                wt::remove(dev, slug, name, force, del_branch)?
            }
            Err(e) => return Err(e),
        },
        None => wt::remove(dev, slug, name, force, del_branch)?,
    };
    for l in log {
        println!("{l}");
    }
    println!("done.");
    Ok(0)
}

/// A daemon started from an older wtd.exe (it keeps running across installs) doesn't know the newer
/// methods. It holds no file watches either, so doing the operation here is safe.
fn older_daemon(e: &anyhow::Error) -> bool {
    e.to_string().contains("unknown method")
}

pub fn archive_main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    let (Some(slug), Some(name)) = (args.first(), args.get(1)) else { bail!("usage: archive <slug> <name>") };
    let name = wt::resolve_name(&dev, slug, name)?;
    let arc = match Client::connect()? {
        Some(mut c) => match c.request(method::WORKTREE_ARCHIVE, json!({ "id": format!("{slug}/{name}") })) {
            Ok(v) => v["path"].as_str().unwrap_or("").to_string(),
            Err(e) if older_daemon(&e) => {
                stop_sessions(&format!("{slug}/{name}"));
                s(&wt::archive(&dev, slug, &name)?)
            }
            Err(e) => return Err(e),
        },
        None => s(&wt::archive(&dev, slug, &name)?),
    };
    println!("archived {slug}/{name} → {arc}\n  (branch, changes and reviews kept; reopen it with: agent {slug} {name})");
    Ok(0)
}

pub fn close_main() -> Result<i32> {
    let dev = paths::dev_root()?;
    let r = here(&dev)?;
    apply_status(&dev, &r, Event::SessionEnd, "sessionend")?;
    println!("closing the session for {} (worktree kept)", r.id);
    stop_sessions(&r.id);
    Ok(0)
}

// --- launch -----------------------------------------------------------------------------------------

struct Account {
    /// Recorded binding for this session: `default`, `<name>`, or `codex:<name>`.
    label: String,
    /// Some(dir) = CLAUDE_CONFIG_DIR for a named Claude account.
    claude_dir: Option<PathBuf>,
    /// Some(dir) = CODEX_HOME: this session runs codex.
    codex_home: Option<PathBuf>,
}

/// --account flag > this session's binding > the `dev` role > the default Claude login.
fn resolve_account(dev: &Path, flag: Option<&str>, session: &str) -> Result<Account> {
    let home = paths::home_dir()?;
    let key = session_key(session);
    let bound = wt::read_state(dev, "session-accounts", &key);
    let role = crate::settings::read_roles(&home).get("dev").cloned(); // provider:name
    let cand: Option<String> = flag.map(String::from).or(bound.clone()).or_else(|| role.clone());
    let cand = cand.unwrap_or_else(|| "default".into());
    let cand = cand.strip_prefix("claude:").unwrap_or(&cand).to_string();
    if let Some(name) = cand.strip_prefix("codex:") {
        let dir = if name == "default" { home.join(".codex") } else { home.join(".codex-accounts").join(name) };
        if name != "default" && !dir.is_dir() {
            bail!("no Codex account '{name}' (add it in Settings → Accounts)");
        }
        if crate::settings::which("codex").is_none() {
            bail!("the Codex CLI isn't installed (npm i -g @openai/codex)");
        }
        return Ok(Account { label: format!("codex:{name}"), claude_dir: None, codex_home: Some(dir) });
    }
    if cand == "default" {
        return Ok(Account { label: "default".into(), claude_dir: None, codex_home: None });
    }
    let dir = home.join(".claude-accounts").join(&cand);
    if !dir.is_dir() {
        if flag.is_some() {
            bail!("no Claude account '{cand}' (add it in Settings → Accounts)");
        }
        eprintln!("note: account '{cand}' is gone; using the default login");
        wt::forget_state(dev, "session-accounts", &key);
        return Ok(Account { label: "default".into(), claude_dir: None, codex_home: None });
    }
    Ok(Account { label: cand, claude_dir: Some(dir), codex_home: None })
}

/// ~/.claude/projects/<cwd, non-alphanumerics → '-'>/<id>.jsonl — where Claude keeps a transcript.
fn transcript(config_dir: &Path, cwd: &Path, id: &str) -> PathBuf {
    let enc: String = cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    config_dir.join("projects").join(enc).join(format!("{id}.jsonl"))
}

pub fn uuid4() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut b = [0u8; 16];
    for (i, chunk) in b.chunks_mut(8).enumerate() {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        h.write_usize(i ^ std::process::id() as usize);
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let x: String = b.iter().map(|v| format!("{v:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &x[0..8], &x[8..12], &x[12..16], &x[16..20], &x[20..32])
}

/// Claude's args for this session: `--resume <id>` once its transcript exists, else `--session-id <id>`
/// (minting a durable id on first open) — so reopening a worktree continues its conversation.
fn claude_session_args(dev: &Path, key: &str, cwd: &Path, config_dir: &Path) -> Result<Vec<String>> {
    let id = match wt::read_state(dev, "session-ids", key) {
        Some(id) => id,
        None => {
            let id = uuid4();
            wt::write_state(dev, "session-ids", key, &id)?;
            id
        }
    };
    Ok(if transcript(config_dir, cwd, &id).is_file() { vec!["--resume".into(), id] } else { vec!["--session-id".into(), id] })
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if src.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

fn add_excludes(exclude: &Path) -> Result<()> {
    std::fs::create_dir_all(exclude.parent().unwrap())?;
    let mut text = std::fs::read_to_string(exclude).unwrap_or_default();
    let before = text.len();
    for ign in crate::repos::WORKTREE_LOCAL {
        if !text.lines().any(|l| l.trim() == *ign) {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(ign);
            text.push('\n');
        }
    }
    if text.len() != before {
        std::fs::write(exclude, text)?;
    }
    Ok(())
}

/// Insert the work item into CLAUDE.md's "Context / scope" placeholder.
fn seed_issue(wtp: &Path, issue: &Path) -> Result<()> {
    let body = std::fs::read_to_string(issue)?;
    let p = wtp.join("CLAUDE.md");
    let text = std::fs::read_to_string(&p)?;
    let marker = "<!-- What this worktree is for. Fill in per task. -->";
    let new = if text.contains(marker) { text.replacen(marker, body.trim_end(), 1) } else { format!("{text}\n## Context / scope\n\n{body}") };
    std::fs::write(&p, new)?;
    std::fs::create_dir_all(wtp.join(".claude"))?;
    std::fs::copy(issue, wtp.join(".claude").join("issue.md"))?;
    Ok(())
}

/// Announce read-only reference checkouts in CLAUDE.md (additive across runs).
fn announce_refs(wtp: &Path, refs: &[(String, PathBuf)]) -> Result<()> {
    let p = wtp.join("CLAUDE.md");
    let text = std::fs::read_to_string(&p).unwrap_or_default();
    let (begin, end) = ("<!-- BEGIN agent-references -->", "<!-- END agent-references -->");
    let mut entries: std::collections::BTreeMap<String, String> = Default::default();
    let mut kept = String::new();
    let mut inside = false;
    for line in text.lines() {
        if line.trim() == begin {
            inside = true;
            continue;
        }
        if line.trim() == end {
            inside = false;
            continue;
        }
        if inside {
            if let Some(rest) = line.strip_prefix("- **") {
                if let Some((label, path)) = rest.split_once("** → `") {
                    entries.insert(label.to_string(), path.trim_end_matches('`').to_string());
                }
            }
        } else {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    for (label, path) in refs {
        entries.insert(label.clone(), s(path));
    }
    kept.push_str(&format!("\n{begin}\n## Reference repos/branches (read-only context)\n\nThe repo@branch checkouts below are available READ-ONLY for cross-repo context. Read / grep / glob them freely; do NOT edit them:\n\n"));
    for (l, p) in &entries {
        kept.push_str(&format!("- **{l}** → `{p}`\n"));
    }
    kept.push_str(end);
    kept.push('\n');
    std::fs::write(&p, kept)?;
    Ok(())
}

fn launch(dev: &Path, args: &[String]) -> Result<i32> {
    let (mut from, mut account, mut issue, mut ticket) = (None::<String>, None::<String>, None::<PathBuf>, None::<String>);
    let mut launch_agent = std::env::var_os("AGENT_NO_CLAUDE").is_none();
    let mut pmode = std::env::var("AGENT_PERMISSION_MODE").unwrap_or_else(|_| "auto".into());
    let mut pos: Vec<String> = vec![];
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let val = |i: usize| args.get(i + 1).cloned().with_context(|| format!("{a} needs a value"));
        match a {
            "--from" => { from = Some(val(i)?); i += 2; }
            "--account" | "-a" => { account = Some(val(i)?); i += 2; }
            "--issue-file" => { issue = Some(paths::normalize(&val(i)?)); i += 2; }
            "--ticket" => { ticket = Some(val(i)?); i += 2; }
            "--mode" => { pmode = val(i)?; i += 2; }
            "--no-auto" => { pmode = "default".into(); i += 1; }
            "--no-claude" => { launch_agent = false; i += 1; }
            _ if a.starts_with("--from=") => { from = Some(a[7..].into()); i += 1; }
            _ if a.starts_with("--account=") => { account = Some(a[10..].into()); i += 1; }
            _ => { pos.push(a.to_string()); i += 1; }
        }
    }
    let (Some(slug), Some(name)) = (pos.first().cloned(), pos.get(1).cloned()) else { bail!("usage: agent <slug> <name> [options] [ref…]") };
    let ref_tokens = pos[2..].to_vec();
    let is_plan = slug == "plan";
    let repo = if is_plan { None } else { Some(crate::repos::require(dev, &slug)?) };
    let bare = repo.as_ref().map(|r| r.path.clone());
    let id = format!("{slug}/{name}");
    let wtp = paths::worktree_path(dev, &id);
    let session = session_name(&slug, &name);
    let key = session_key(&session);
    let acct = resolve_account(dev, account.as_deref(), &session)?;

    // archived? offer to bring it back instead of creating a new one
    let arc = name.split('/').fold(dev.join("worktrees").join(&slug).join("archive"), |p, c| p.join(c));
    if !is_plan && !wtp.exists() && arc.is_dir() {
        println!("An archived worktree for '{name}' exists at {}", arc.display());
        if !confirm("Reopen it (move it back into the active worktrees)?", true) {
            println!("left it archived.");
            return Ok(1);
        }
        wt::restore(dev, &slug, &name)?;
        println!("reopened from the archive → {}", wtp.display());
    }

    let created = !wtp.exists();
    if !wtp.exists() {
        if is_plan {
            std::fs::create_dir_all(&wtp)?;
            gitx::run(&["-C", &s(&wtp), "init", "-q"])?;
            let _ = gitx::ok(&["-C", &s(&wtp), "symbolic-ref", "HEAD", "refs/heads/main"]);
        } else {
            let b = bare.as_ref().unwrap();
            let bs = s(b);
            if gitx::has_origin(b) {
                let _ = gitx::run(&["-C", &bs, "fetch", "origin"]);
            }
            std::fs::create_dir_all(wtp.parent().unwrap())?;
            let ws = s(&wtp);
            if gitx::ref_exists(b, &format!("refs/heads/{name}")) {
                if from.is_some() {
                    println!("note: local branch '{name}' already exists; ignoring --from");
                }
                gitx::run(&["-C", &bs, "worktree", "add", &ws, &name])?;
            } else if gitx::ref_exists(b, &format!("refs/remotes/origin/{name}")) {
                if from.is_some() {
                    println!("note: origin/{name} already exists; ignoring --from");
                }
                gitx::run(&["-C", &bs, "worktree", "add", "--track", "-b", &name, &ws, &format!("origin/{name}")])?;
            } else {
                let base = match &from {
                    Some(f) if gitx::ref_exists(b, &format!("refs/remotes/origin/{f}")) => format!("origin/{f}"),
                    Some(f) if gitx::ref_exists(b, f) => f.clone(),
                    Some(f) => bail!("--from ref '{f}' not found in '{slug}' (tried origin/{f} and {f})"),
                    None if gitx::has_origin(b) => format!("origin/{}", gitx::default_branch(b)),
                    None => gitx::default_branch(b),
                };
                gitx::run(&["-C", &bs, "worktree", "add", &ws, "-b", &name, &base])?;
            }
        }
        std::fs::create_dir_all(wtp.join(".claude").join("plans"))?;
        if !wtp.join("CLAUDE.md").exists() {
            std::fs::copy(dev.join(".wtd").join("templates").join("CLAUDE.md"), wtp.join("CLAUDE.md"))?;
        }
        if let Some(f) = issue.as_ref().filter(|f| f.is_file()) {
            seed_issue(&wtp, f)?;
            println!("seeded the work item into CLAUDE.md (Context / scope)");
        }
        // host-local env files (e.g. .env) stashed under .wtd/env/<slug>/, copied only when absent
        let envsrc = dev.join(".wtd").join("env").join(&slug);
        if envsrc.is_dir() {
            seed_env(&envsrc, &envsrc, &wtp)?;
        }
        if is_plan {
            add_excludes(&wtp.join(".git").join("info").join("exclude"))?;
        }
    }

    // per-repo git setup (ignore rules, commit-msg + guardrail hooks) — idempotent, so repos found in
    // the repos folder get it on first use and existing ones pick up changes
    if let Some(r) = &repo {
        if let Err(e) = crate::repos::prepare(dev, r) {
            eprintln!("warning: repo setup for '{slug}': {e:#}");
        }
    }
    // template skills refresh on every open, so new/updated skills reach existing worktrees
    let skills = dev.join(".wtd").join("templates").join(".claude").join("skills");
    if skills.is_dir() {
        let _ = copy_dir(&skills, &wtp.join(".claude").join("skills"));
    }
    // every worktree gets a 'plan' preview tab (placeholder until the agent writes one)
    let pvplan = name.split('/').fold(dev.join(".wtd").join("state").join("previews").join(&slug), |p, c| p.join(c)).join("plan.html");
    if !pvplan.exists() {
        std::fs::create_dir_all(pvplan.parent().unwrap())?;
        let _ = std::fs::copy(dev.join(".wtd").join("templates").join("plan-placeholder.html"), &pvplan);
    }
    // your own skills / CLAUDE.md additions / hooks (.wtd/custom), global and for this repo
    match crate::custom::apply(dev, &slug, &wtp, created) {
        Ok(log) => log.iter().for_each(|l| println!("{l}")),
        Err(e) => eprintln!("warning: customizations: {e:#}"),
    }
    // the linked ticket: link it (New Session passes the picked issue), then refresh
    // .claude-ticket.md in the background so the session doesn't wait on GitHub
    if let (Some(spec), false) = (&ticket, is_plan) {
        match crate::ticket::parse_spec(dev, &slug, spec) {
            Ok((repo, n)) => crate::ticket::link(dev, &id, &repo, n)?,
            Err(e) => eprintln!("warning: --ticket {spec}: {e:#}"),
        }
    }
    if !is_plan && (crate::ticket::link_of(dev, &id).is_some() || crate::repos::github_repo(dev, &slug).is_some()) {
        let (d, i) = (dev.to_path_buf(), id.clone());
        std::thread::spawn(move || { let _ = crate::ticket::sync(&d, &i); });
    }
    if !is_plan && wt::merge_repo_hooks(dev, &slug, &wtp)? {
        println!("wired repo hooks for '{slug}' into .claude/settings.json");
    }
    if !ref_tokens.is_empty() {
        let mut found = vec![];
        for t in &ref_tokens {
            let (rs, rb) = gitx::parse_ref_token(dev, t);
            match gitx::ensure_ref(dev, &rs, &rb) {
                Ok(p) => found.push((format!("{rs}@{rb}"), p)),
                Err(e) => eprintln!("warning: skipping reference '{t}': {e:#}"),
            }
        }
        if !found.is_empty() {
            announce_refs(&wtp, &found)?;
            println!("references announced in CLAUDE.md: {}", found.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>().join(", "));
        }
    }
    wt::write_state(dev, "session-accounts", &key, &acct.label)?;

    std::env::set_current_dir(&wtp)?;
    if !launch_agent {
        println!("worktree ready: {}", wtp.display());
        return Ok(0);
    }
    if acct.label != "default" {
        println!("session '{session}' → account '{}'", acct.label);
    }
    // the session's environment: whichever login it runs under, plus our own commands on PATH
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    if let Some(d) = &acct.claude_dir {
        std::env::set_var("CLAUDE_CONFIG_DIR", d);
    }
    if let Some(h) = &acct.codex_home {
        std::env::set_var("CODEX_HOME", h);
    }
    std::env::set_var("WTD_SESSION", &session);
    // custom tools (.wtd/custom/tools, …/repos/<slug>/tools) on the session's PATH
    let tools = crate::custom::tool_dirs(dev, &slug);
    if !tools.is_empty() {
        let cur = std::env::var_os("PATH").unwrap_or_default();
        let joined = std::env::join_paths(tools.iter().cloned().chain(std::env::split_paths(&cur)))?;
        std::env::set_var("PATH", joined);
    }
    let host_args: Vec<String> = if acct.codex_home.is_some() {
        let mark = format!("{key}.codex");
        let resume = wt::read_state(dev, "session-ids", &mark).is_some() || wt::state(dev, "session-ids").join(&mark).exists();
        wt::write_state(dev, "session-ids", &mark, "codex")?;
        let mut v = vec!["codex".to_string()];
        if resume {
            v.extend(["resume".into(), "--last".into()]);
        }
        v
    } else {
        let cfg = acct.claude_dir.clone().unwrap_or(paths::home_dir()?.join(".claude"));
        let mut v = vec!["claude".to_string(), "--permission-mode".into(), pmode];
        v.extend(claude_session_args(dev, &key, &wtp, &cfg)?);
        v
    };
    let mut hargs = vec!["--kind".to_string(), "agent".into(), "--account".into(), acct.label.clone(), "--".into()];
    hargs.extend(host_args);
    crate::attach::host_main(&hargs)
}

fn seed_env(root: &Path, dir: &Path, wtp: &Path) -> Result<()> {
    for e in std::fs::read_dir(dir)?.flatten() {
        let p = e.path();
        if p.is_dir() {
            seed_env(root, &p, wtp)?;
        } else {
            let rel = p.strip_prefix(root).unwrap();
            let dest = wtp.join(rel);
            if !dest.exists() {
                std::fs::create_dir_all(dest.parent().unwrap())?;
                std::fs::copy(&p, &dest)?;
                println!("seeded env: {}", rel.display());
            }
        }
    }
    Ok(())
}

// --- the fleet assistant -----------------------------------------------------------------------------

const ASSISTANT_PROMPT: &str = "You are the worktree-dev fleet assistant. You run in the worktree-dev base directory and help the
user manage their parallel agent worktrees — you do NOT write feature code inside individual worktrees
(each has its own session for that); you orchestrate the fleet.

Use the worktree-dev commands to do real work (don't just describe them):
- agent <slug> <name> [--from <ref>] [--account <a>]  — open/create a worktree session
- agent ls | agent stop <slug> <name> | agent rm <slug> <name> [--branch] [--force]
- archive <slug> <name> — shelve a worktree;  ref add/ls/rm — read-only cross-repo context
- review <slug> <name> [--main] — the separate pre-push reviewer (only when asked)
- ask <slug>[@<branch>] [question] — grounded per-repo Q&A
- wtd ls | wtd group … | wtd issues ls <slug> | account ls | tokens
The wtd MCP tools (fleet_list, fleet_get) show every worktree's state.
Be concise and act directly. Never start a review on your own — only when the user explicitly asks.";

pub fn assistant_main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    let home = paths::home_dir()?;
    let mut account = std::env::var("ASSISTANT_ACCOUNT").ok().filter(|s| !s.is_empty());
    let mut pmode = "auto".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--account" | "-a" => { account = args.get(i + 1).cloned(); i += 2; }
            "--mode" => { pmode = args.get(i + 1).cloned().unwrap_or(pmode); i += 2; }
            _ => i += 1,
        }
    }
    let role = crate::settings::read_roles(&home).get("assistant").cloned().and_then(|r| r.strip_prefix("claude:").map(String::from));
    let account = account.or(role).filter(|a| a != "default");
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    let cfg = match &account {
        Some(a) => {
            let d = home.join(".claude-accounts").join(a);
            if !d.is_dir() {
                bail!("no Claude account '{a}' (add it in Settings → Accounts)");
            }
            std::env::set_var("CLAUDE_CONFIG_DIR", &d);
            d
        }
        None => home.join(".claude"),
    };
    std::env::set_current_dir(&dev)?;
    std::env::set_var("WTD_SESSION", "assistant");
    let mut hargs = vec!["--kind".to_string(), "assistant".into(), "--account".into(), account.clone().unwrap_or_else(|| "default".into()), "--".into(),
        "claude".into(), "--permission-mode".into(), pmode, "--append-system-prompt".into(), ASSISTANT_PROMPT.into()];
    hargs.extend(claude_session_args(&dev, "assistant", &dev, &cfg)?);
    crate::attach::host_main(&hargs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_shape() {
        let u = uuid4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert_ne!(uuid4(), uuid4());
    }

    #[test]
    fn transcript_path() {
        let p = transcript(Path::new(r"C:\Users\x\.claude"), Path::new(r"D:\dev\worktree-dev\worktrees\luop\feat\a"), "id1");
        assert!(p.to_string_lossy().ends_with(r"projects\D--dev-worktree-dev-worktrees-luop-feat-a\id1.jsonl"), "{}", p.display());
    }
}
