//! `agent <slug> <name> --platform wsl:<distro>` (and reopening a registered WSL worktree): create or
//! reopen the worktree inside the distro, seed it through its `\\wsl.localhost` view, then host
//! `wsl.exe -d <distro> --cd <wsl_path> -- <claude|codex> …` in the daemon like any other session.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde_json::json;

use super::{path, repo::Sh, Entry};
use crate::agent;
use crate::wt::{self, session_key, session_name};

/// What `agent` parsed, for a WSL target.
pub struct Opts {
    pub slug: String,
    pub name: String,
    pub from: Option<String>,
    pub account: Option<String>,
    pub issue: Option<PathBuf>,
    pub ticket: Option<String>,
    pub launch_agent: bool,
    pub pmode: String,
    pub detach: bool,
    pub prompt: Option<String>,
    pub parent: Option<String>,
    pub ref_tokens: Vec<String>,
    pub started_from: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct Account {
    /// Recorded binding and roster label: `wsl:<distro>/<default|name|codex:name>`.
    pub label: String,
    /// Linux CLAUDE_CONFIG_DIR for a named Claude login.
    pub claude_dir: Option<String>,
    /// Linux CODEX_HOME: this session runs codex.
    pub codex_home: Option<String>,
}

/// `default` | `<name>` | `codex:<name>` | `wsl:<distro>/<that>` → the login inside the distro. Logins in
/// WSL are separate from Windows ones (`~/.claude-accounts/<name>` in the distro).
pub fn account_for(distro: &str, home: &str, choice: &str) -> Account {
    let prefix = format!("wsl:{distro}/");
    let c = choice.strip_prefix(&prefix).unwrap_or(choice);
    let c = c.strip_prefix("claude:").unwrap_or(c);
    let label = format!("{prefix}{c}");
    match c.strip_prefix("codex:") {
        Some("default") => Account { label, claude_dir: None, codex_home: Some(path::join(home, ".codex")) },
        Some(n) => Account { label, claude_dir: None, codex_home: Some(path::join(home, &format!(".codex-accounts/{n}"))) },
        None if c == "default" => Account { label, claude_dir: None, codex_home: None },
        None => Account { label, claude_dir: Some(path::join(home, &format!(".claude-accounts/{c}"))), codex_home: None },
    }
}

/// --account > this session's WSL binding > the default login. A Windows binding never carries over.
fn resolve_account(dev: &Path, distro: &str, home: &str, flag: Option<&str>, key: &str) -> Result<Account> {
    let bound = wt::read_state(dev, "session-accounts", key).filter(|b| b.starts_with(&format!("wsl:{distro}/")));
    let a = account_for(distro, home, flag.or(bound.as_deref()).unwrap_or("default"));
    let dir = a.claude_dir.as_ref().or(a.codex_home.as_ref().filter(|_| !a.label.ends_with("codex:default")));
    if let Some(d) = dir.filter(|d| !path::unc(distro, d).is_dir()) {
        if flag.is_some() {
            bail!("no login at {d} in '{distro}' (log in there first: wsl -d {distro} -- claude /login)");
        }
        eprintln!("note: {} is gone; using the default login", a.label);
        wt::forget_state(dev, "session-accounts", key);
        return Ok(account_for(distro, home, "default"));
    }
    Ok(a)
}

pub fn launch(dev: &Path, distro: &str, o: Opts) -> Result<i32> {
    let id = format!("{}/{}", o.slug, o.name);
    if let Some(arc) = super::find_any(dev, &id).filter(|e| e.archived) {
        println!("An archived WSL worktree for '{}' exists at {} ({distro})", o.name, arc.wsl_path);
        if !agent::confirm("Reopen it (move it back into the active worktrees)?", true) {
            println!("left it archived.");
            return Ok(1);
        }
        super::restore(dev, &arc)?;
    }
    let (e, created) = super::create(dev, distro, &o.slug, &o.name, o.from.as_deref())?;
    let session = session_name(&o.slug, &o.name);
    let key = session_key(&session);
    let home = super::home(dev, distro)?;
    let acct = resolve_account(dev, distro, &home, o.account.as_deref(), &key)?;
    seed(dev, &e, created, &o)?;
    wt::write_state(dev, "session-accounts", &key, &acct.label)?;
    match o.parent.as_deref() {
        Some("none") => agent::record_parent(&id, None),
        Some(p) => agent::record_parent(&id, Some(p)),
        None if created => {
            if let Some(p) = &o.started_from {
                agent::record_parent(&id, Some(p));
            }
        }
        None => {}
    }
    if !o.launch_agent {
        println!("worktree ready in {distro}: {} ({})", e.wsl_path, e.unc().display());
        return Ok(0);
    }
    if acct.label != format!("wsl:{distro}/default") {
        println!("session '{session}' → account '{}'", acct.label);
    }

    let handoff = wt::read_state(dev, crate::handoff::PENDING, &key).map(PathBuf::from).filter(|p| p.is_file());
    wt::forget_state(dev, crate::handoff::PENDING, &key);
    let fresh = handoff.is_some();
    let prompt = o.prompt.clone().or_else(|| (o.detach && o.issue.is_some()).then(|| agent::WORKER_PROMPT.to_string()));
    // the brief lives on Windows; the agent reads it through /mnt
    let seed_prompt = handoff.as_ref().map(|p| crate::handoff::prompt(&PathBuf::from(path::to_mnt(p).unwrap_or_else(|| p.to_string_lossy().to_string())))).or(prompt);
    let (program, argv) = match &acct.codex_home {
        Some(ch) => {
            let mark = format!("{key}.codex");
            let resume = wt::read_state(dev, "session-ids", &mark).is_some();
            wt::write_state(dev, "session-ids", &mark, "codex")?;
            if let Some(exe) = path::to_mnt(&crate::paths::wtd_exe()) {
                if let Err(err) = crate::codex::prepare_for(&path::unc(distro, ch), &e.wsl_path, Path::new(&exe)) {
                    eprintln!("warning: preparing Codex in {distro}: {err:#}");
                }
            }
            let mut v = vec!["codex".to_string()];
            if resume && !fresh {
                v.extend(["resume".into(), "--last".into()]);
            }
            v.extend(seed_prompt);
            ("codex", v)
        }
        None => {
            let cfg = acct.claude_dir.clone().unwrap_or_else(|| path::join(&home, ".claude"));
            let mut v = vec!["claude".to_string(), "--permission-mode".into(), o.pmode.clone()];
            let sid = match wt::read_state(dev, "session-ids", &key).filter(|_| !fresh) {
                Some(sid) => sid,
                None => {
                    let sid = agent::uuid4();
                    wt::write_state(dev, "session-ids", &key, &sid)?;
                    sid
                }
            };
            let resume = !fresh && agent::transcript(&path::unc(distro, &cfg), Path::new(&e.wsl_path), &sid).is_file();
            v.extend([if resume { "--resume" } else { "--session-id" }.to_string(), sid]);
            v.extend(seed_prompt);
            ("claude", v)
        }
    };
    let mut vars: Vec<(&str, String)> = vec![("WTD_SESSION", session.clone()), ("WTD_WORKTREE", id.clone())];
    if let Some(d) = &acct.claude_dir {
        vars.push(("CLAUDE_CONFIG_DIR", d.clone()));
    }
    if let Some(h) = &acct.codex_home {
        vars.push(("CODEX_HOME", h.clone()));
    }
    let env = super::session_env(crate::session_env::current(), &vars);
    let args = super::login_args(distro, Some(&e.wsl_path), &argv);
    let (cols, rows) = crate::attach::console_size().unwrap_or((120, 30));
    // wsl.exe's own cwd doesn't matter (--cd sets the Linux one); a local folder avoids a UNC cwd
    let win_cwd = crate::paths::home_dir().unwrap_or_else(|_| dev.to_path_buf());
    let params = json!({
        "dir": win_cwd.to_string_lossy(), "worktree": id, "kind": "agent", "account": acct.label, "program": program,
        "exe": super::wsl_exe().to_string_lossy(), "args": args, "env": env, "cols": cols, "rows": rows,
    });
    if o.detach {
        if crate::client::Client::connect()?.is_none() {
            bail!("the wtd daemon isn't running; start it to launch a background session");
        }
        crate::attach::spawn_detached(params)?;
        println!("started {id} in {distro} in the background (open it from the roster, or `wtd attach {id}`)");
        return Ok(0);
    }
    if crate::client::Client::connect()?.is_some() {
        return crate::attach::host_prepared(params);
    }
    // no daemon: the session lives in this terminal
    for (k, v) in &env {
        std::env::set_var(k, v);
    }
    let mut run_args = vec!["--kind".to_string(), "agent".into(), "--account".into(), acct.label, "--".into(), super::wsl_exe().to_string_lossy().to_string()];
    run_args.extend(args);
    crate::run::main(&run_args)
}

/// What `agent` seeds into a worktree, written here through the UNC view (a few small
/// files, never git).
fn seed(dev: &Path, e: &Entry, created: bool, o: &Opts) -> Result<()> {
    let wtp = e.unc();
    let sh = Sh::Wsl(e.distro.clone());
    std::fs::create_dir_all(wtp.join(".claude").join("plans"))?;
    if !wtp.join("CLAUDE.md").exists() {
        std::fs::copy(crate::paths::asset_in(dev, "templates/CLAUDE.md"), wtp.join("CLAUDE.md"))?;
    }
    if created {
        if let Some(f) = o.issue.as_ref().filter(|f| f.is_file()) {
            agent::seed_issue(&wtp, f)?;
            println!("seeded the work item into CLAUDE.md (Context / scope)");
        }
        let envsrc = dev.join(".wtd").join("env").join(&e.slug);
        if envsrc.is_dir() {
            agent::seed_env(&envsrc, &envsrc, &wtp)?;
        }
    }
    let skills = crate::paths::asset_in(dev, "templates/.claude/skills");
    if skills.is_dir() {
        let _ = agent::copy_dir(&skills, &wtp.join(".claude").join("skills"));
    }
    let pvplan = e.name.split('/').fold(crate::paths::state_dir(dev).join("previews").join(&e.slug), |p, c| p.join(c)).join("plan.html");
    if !pvplan.exists() {
        std::fs::create_dir_all(pvplan.parent().unwrap())?;
        let _ = std::fs::copy(crate::paths::asset_in(dev, "templates/plan-placeholder.html"), &pvplan);
    }
    match crate::custom::apply_as(dev, &e.slug, &wtp, created, "wsl", &super::repo::tracked(&sh, &e.wsl_path)) {
        Ok(log) => log.iter().for_each(|l| println!("{l}")),
        Err(err) => eprintln!("warning: customizations: {err:#}"),
    }
    if let Some(spec) = &o.ticket {
        match crate::ticket::parse_spec(dev, &e.slug, spec) {
            Ok((repo, n)) => crate::ticket::link(dev, &e.id, &repo, n)?,
            Err(err) => eprintln!("warning: --ticket {spec}: {err:#}"),
        }
    }
    if crate::ticket::link_of(dev, &e.id).is_some() || crate::repos::github_repo(dev, &e.slug).is_some() {
        let (d, i) = (dev.to_path_buf(), e.id.clone());
        std::thread::spawn(move || {
            let _ = crate::ticket::sync(&d, &i);
        });
    }
    let mut refs = vec![];
    for t in &o.ref_tokens {
        let (rs, rb) = t.split_once('@').map(|(a, b)| (a.to_string(), Some(b.to_string()))).unwrap_or((t.clone(), None));
        match super::ensure_ref(dev, &e.distro, &rs, rb.as_deref()) {
            Ok((b, p)) => refs.push((format!("{rs}@{b}"), PathBuf::from(p))),
            Err(err) => eprintln!("warning: skipping reference '{t}': {err:#}"),
        }
    }
    if !refs.is_empty() {
        agent::announce_refs(&wtp, &refs)?;
        println!("references announced in CLAUDE.md: {}", refs.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>().join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_live_in_the_distro() {
        let a = |c: &str| account_for("Ubuntu", "/home/u", c);
        assert_eq!(a("default"), Account { label: "wsl:Ubuntu/default".into(), claude_dir: None, codex_home: None });
        assert_eq!(a("work").claude_dir.as_deref(), Some("/home/u/.claude-accounts/work"));
        assert_eq!(a("wsl:Ubuntu/work"), a("work"));
        assert_eq!(a("claude:work").label, "wsl:Ubuntu/work");
        let c = a("codex:team");
        assert_eq!((c.label.as_str(), c.codex_home.as_deref(), c.claude_dir), ("wsl:Ubuntu/codex:team", Some("/home/u/.codex-accounts/team"), None));
        assert_eq!(a("codex:default").codex_home.as_deref(), Some("/home/u/.codex"));
    }
}
