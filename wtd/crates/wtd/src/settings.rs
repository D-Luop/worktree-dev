//! Settings backend: repos, Claude/Codex accounts, role defaults, per-repo GitHub links.
//! Every command prints JSON (`--json` is implied) so the Settings page can drive it directly.
//!
//! Storage (unchanged from the bash tooling, so `agent`, `review`, `account` keep working):
//! - repo registry: `.wtd/repos.tsv` (`<slug>\t<url>`), bare clones at `repos/<slug>/.bare`
//! - Claude accounts: `~/.claude` (default) + `~/.claude-accounts/<name>/` (`CLAUDE_CONFIG_DIR`)
//! - Codex accounts: `~/.codex` (default) + `~/.codex-accounts/<name>/` (`CODEX_HOME`)
//! - role defaults: `~/.claude-accounts/roles.conf` (`role=name`, or `role=codex:name`)
//! - everything else (GitHub links, issue sources): `.wtd/config.json` (local, not committed)

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::{daemon, paths, win};

const ROLES: [&str; 3] = ["dev", "review", "assistant"];

// --- shared helpers --------------------------------------------------------------------------------

pub(crate) fn out(v: Value) -> Result<i32> {
    println!("{}", serde_json::to_string(&v)?);
    Ok(0)
}

/// Print rows as an aligned table (for a person at a terminal; scripts get JSON).
fn table(head: &[&str], rows: Vec<Vec<String>>) -> Result<i32> {
    let mut w: Vec<usize> = head.iter().map(|h| h.len()).collect();
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let line = |cells: Vec<String>| {
        let mut l = String::new();
        for (i, c) in cells.iter().enumerate() {
            l.push_str(c);
            if i + 1 < cells.len() {
                l.push_str(&" ".repeat(w[i] - c.chars().count() + 2));
            }
        }
        println!("{}", l.trim_end());
    };
    line(head.iter().map(|h| h.to_uppercase()).collect());
    if rows.is_empty() {
        println!("(none)");
    }
    for r in rows {
        line(r);
    }
    Ok(0)
}

fn human(a: &[&str]) -> bool {
    use std::io::IsTerminal;
    !a.contains(&"--json") && std::io::stdout().is_terminal()
}

pub(crate) fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn git(args: &[&str], cwd: Option<&Path>) -> Command {
    let mut c = Command::new("git");
    c.arg("-c").arg("safe.bareRepository=all").args(args).stdin(Stdio::null());
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    win::no_window(&mut c);
    c
}

/// Run git, streaming its output through to ours (the Settings page shows it as progress).
fn git_run(args: &[&str]) -> Result<()> {
    let st = git(args, None).stdout(Stdio::inherit()).stderr(Stdio::inherit()).status()?;
    if !st.success() {
        bail!("git {} failed ({})", args.join(" "), st);
    }
    Ok(())
}

fn git_out(args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let mut c = git(args, None);
    for (k, v) in env {
        c.env(k, v);
    }
    let o = c.stderr(Stdio::piped()).output()?;
    if !o.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// `remove_dir_all` that also clears the read-only bit git sets on pack files (Windows refuses
/// to delete read-only files otherwise).
pub(crate) fn remove_tree(p: &Path) -> std::io::Result<()> {
    fn clear(p: &Path) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let path = e.path();
                if path.is_dir() {
                    clear(&path);
                } else if let Ok(m) = std::fs::metadata(&path) {
                    let mut perm = m.permissions();
                    if perm.readonly() {
                        #[allow(clippy::permissions_set_readonly_false)]
                        perm.set_readonly(false);
                        let _ = std::fs::set_permissions(&path, perm);
                    }
                }
            }
        }
    }
    clear(p);
    std::fs::remove_dir_all(p)
}

pub(crate) fn which(prog: &str) -> Option<PathBuf> {
    let p = crate::run::resolve_program(prog);
    p.is_file().then_some(p)
}

// --- config.json -----------------------------------------------------------------------------------

fn config_path(dev: &Path) -> PathBuf {
    dev.join(".wtd").join("config.json")
}

pub fn load_config(dev: &Path) -> Value {
    std::fs::read_to_string(config_path(dev))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({ "version": 1, "repos": {} }))
}

fn save_config(dev: &Path, cfg: &Value) -> Result<()> {
    let p = config_path(dev);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(cfg)? + "\n")?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

// --- repos ------------------------------------------------------------------------------------------

fn registry(dev: &Path) -> PathBuf {
    dev.join(".wtd").join("repos.tsv")
}

pub fn registered(dev: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(registry(dev))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut it = l.splitn(2, '\t');
            Some((it.next()?.trim().to_string(), it.next().unwrap_or("").trim().to_string()))
        })
        .filter(|(s, _)| !s.is_empty())
        .collect()
}

/// `owner/name` from a GitHub remote URL (https, ssh, with or without `.git`).
pub fn github_repo_from_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = rest.split('/');
    let (o, n) = (parts.next()?, parts.next()?);
    (!o.is_empty() && !n.is_empty() && parts.next().is_none()).then(|| format!("{o}/{n}"))
}

fn read_ref(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok()?.trim().strip_prefix("ref: ").map(String::from)
}

fn default_branch(bare: &Path) -> Option<String> {
    read_ref(&bare.join("refs").join("remotes").join("origin").join("HEAD"))
        .and_then(|r| r.strip_prefix("refs/remotes/origin/").map(String::from))
        .or_else(|| read_ref(&bare.join("HEAD")).and_then(|r| r.strip_prefix("refs/heads/").map(String::from)))
}

#[derive(Serialize)]
struct RepoInfo {
    slug: String,
    /// `registered` (bare clone managed by wtd) | `folder` (a clone found in the repos folder)
    kind: &'static str,
    path: String,
    url: String,
    local_only: bool,
    cloned: bool,
    default_branch: Option<String>,
    worktrees: usize,
    archived: usize,
    github_detected: Option<String>,
    github: Value,
    /// the PR guardrails in force for this repo (its own, else the defaults)
    guardrails: Value,
}

fn repo_list(dev: &Path) -> Vec<RepoInfo> {
    let cfg = load_config(dev);
    let wts = daemon::scan::scan(dev);
    crate::repos::all(dev)
        .into_iter()
        .map(|r| {
            let slug = r.slug.clone();
            let archived = std::fs::read_dir(dev.join("worktrees").join(&slug).join("archive")).map(|r| r.count()).unwrap_or(0);
            RepoInfo {
                worktrees: wts.iter().filter(|w| w.slug == slug).count(),
                archived,
                local_only: r.url == "(local)" || r.url.is_empty(),
                cloned: r.path.is_dir(),
                default_branch: default_branch(&r.admin),
                github_detected: github_repo_from_url(&r.url),
                github: cfg.pointer(&format!("/repos/{slug}/github")).cloned().unwrap_or(Value::Null),
                guardrails: crate::guard::effective(dev, &slug),
                kind: r.kind,
                path: r.path.to_string_lossy().to_string(),
                slug,
                url: r.url,
            }
        })
        .collect()
}

pub fn repo_main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        [] | ["ls"] if human(&a) => table(
            &["repo", "remote", "default", "worktrees", "issues from"],
            repo_list(&dev)
                .into_iter()
                .map(|r| {
                    let g = |k: &str| r.github.pointer(&format!("/issueSource/{k}")).map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
                    let src = match g("kind").as_deref() {
                        Some("repo") => format!("issues: {}", g("repo").unwrap_or_default()),
                        Some("project") => format!("project: {} #{}", g("owner").unwrap_or_default(), g("number").unwrap_or_default()),
                        _ => String::new(),
                    };
                    vec![
                        r.slug,
                        if r.local_only { "(local only)".into() } else { r.url },
                        r.default_branch.unwrap_or_default(),
                        if r.archived > 0 { format!("{} (+{} archived)", r.worktrees, r.archived) } else { r.worktrees.to_string() },
                        src,
                    ]
                })
                .collect(),
        ),
        [] | ["ls"] | ["ls", "--json"] => out(serde_json::to_value(repo_list(&dev))?),
        ["add", "--new", slug] => repo_add(&dev, slug, None),
        ["add", slug, url] => repo_add(&dev, slug, Some(url)),
        ["rm", slug] => repo_rm(&dev, slug, false),
        ["rm", slug, "--delete-clone"] => repo_rm(&dev, slug, true),
        // the install-wide repo settings: the repos folder and the default guardrails
        ["config"] => {
            let cfg = load_config(&dev);
            let mut g = cfg.get("guardrails").filter(|v| v.is_object()).cloned().unwrap_or_else(crate::guard::defaults);
            for (k, v) in crate::guard::defaults().as_object().unwrap() {
                if g.get(k).is_none() {
                    g[k] = v.clone();
                }
            }
            out(json!({ "reposDir": cfg.get("reposDir"), "reposDirOk": crate::repos::repos_dir(&dev).is_some(), "guardrails": g }))
        }
        ["fetch", slug] => {
            let r = crate::repos::require(&dev, slug)?;
            git_run(&["-C", &r.path.to_string_lossy(), "fetch", "--prune", "origin"])?;
            out(json!({ "ok": true }))
        }
        // a folder of clones: every git clone in it (and one level of subfolders) is a repo
        ["set-dir", "--clear"] | ["set-dir", ""] => {
            let mut cfg = load_config(&dev);
            cfg.as_object_mut().unwrap().remove("reposDir");
            save_config(&dev, &cfg)?;
            out(json!({ "ok": true }))
        }
        ["set-dir", dir] => {
            let p = paths::normalize(dir);
            if !p.is_dir() {
                bail!("not a folder: {}", p.display());
            }
            let mut cfg = load_config(&dev);
            cfg["reposDir"] = json!(p.to_string_lossy());
            save_config(&dev, &cfg)?;
            let found: Vec<String> = crate::repos::discovered(&dev).into_iter().map(|r| r.slug).collect();
            out(json!({ "ok": true, "found": found }))
        }
        ["set-guardrails", slug, value] => {
            let v: Value = serde_json::from_str(value).context("guardrails must be JSON")?;
            crate::guard::validate(&v)?;
            let mut cfg = load_config(&dev);
            if *slug == "*" {
                cfg["guardrails"] = v;
            } else {
                if crate::repos::find(&dev, slug).is_none() {
                    bail!("no repo '{slug}'");
                }
                let repos = cfg.as_object_mut().unwrap().entry("repos").or_insert_with(|| json!({}));
                let entry = repos.as_object_mut().context("config.repos is not an object")?.entry(slug.to_string()).or_insert_with(|| json!({}));
                if v.is_null() {
                    entry.as_object_mut().map(|o| o.remove("guardrails"));
                } else {
                    entry["guardrails"] = v;
                }
            }
            save_config(&dev, &cfg)?;
            out(json!({ "ok": true }))
        }
        ["set-github", slug, value] => {
            if crate::repos::find(&dev, slug).is_none() {
                bail!("no repo '{slug}'");
            }
            let v: Value = serde_json::from_str(value).context("github settings must be JSON")?;
            validate_github(&v)?;
            let mut cfg = load_config(&dev);
            let repos = cfg.as_object_mut().unwrap().entry("repos").or_insert_with(|| json!({}));
            let entry = repos.as_object_mut().context("config.repos is not an object")?.entry(slug.to_string()).or_insert_with(|| json!({}));
            entry["github"] = v;
            save_config(&dev, &cfg)?;
            out(json!({ "ok": true }))
        }
        ["test-issues", slug] => test_issues(&dev, slug),
        ["project-fields", owner, number] => project_fields(owner, number),
        _ => bail!("usage: wtd repo ls | add <slug> <url> | add --new <slug> | rm <slug> [--delete-clone] | fetch <slug> | set-dir <folder>|--clear | set-github <slug> <json> | set-guardrails <slug>|* <json> | test-issues <slug> | project-fields <owner> <number>"),
    }
}

fn validate_github(v: &Value) -> Result<()> {
    if v.is_null() {
        return Ok(());
    }
    let repo = v.get("repo").and_then(Value::as_str).unwrap_or("");
    if !repo.is_empty() && repo.split('/').count() != 2 {
        bail!("GitHub repo must look like owner/name");
    }
    match v.pointer("/issueSource/kind").and_then(Value::as_str) {
        None => Ok(()),
        Some("repo") => {
            let r = v.pointer("/issueSource/repo").and_then(Value::as_str).unwrap_or("");
            if r.split('/').count() != 2 {
                bail!("issue repo must look like owner/name");
            }
            Ok(())
        }
        Some("project") => {
            let ok = v.pointer("/issueSource/owner").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
                && v.pointer("/issueSource/number").and_then(Value::as_u64).is_some();
            if !ok {
                bail!("a Project source needs an owner and a project number (paste the project URL)");
            }
            Ok(())
        }
        Some(k) => bail!("unknown issue source kind '{k}'"),
    }
}

fn repo_add(dev: &Path, slug: &str, url: Option<&str>) -> Result<i32> {
    if !valid_name(slug) {
        bail!("slug must be letters, digits, - or _ (got '{slug}')");
    }
    if slug == "plan" {
        bail!("'plan' is reserved for repo-less planning agents");
    }
    let bare = dev.join("repos").join(slug).join(".bare");
    let bare_s = bare.to_string_lossy().to_string();
    if bare.exists() {
        bail!("{} already exists", bare.display());
    }
    // register (idempotent)
    if !registered(dev).iter().any(|(s, _)| s == slug) {
        let reg = registry(dev);
        let mut text = std::fs::read_to_string(&reg).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("{slug}\t{}\n", url.unwrap_or("(local)")));
        std::fs::write(&reg, text)?;
        println!("registered {slug}");
    }
    std::fs::create_dir_all(bare.parent().unwrap())?;
    git_run(&["init", "--bare", "--quiet", &bare_s])?;
    match url {
        Some(url) => {
            git_run(&["-C", &bare_s, "remote", "add", "origin", url])?;
            git_run(&["-C", &bare_s, "config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"])?;
            println!("fetching {url} …");
            std::io::stdout().flush().ok();
            if let Err(e) = git_run(&["-C", &bare_s, "fetch", "--prune", "--progress", "origin"]) {
                // leave nothing half-made behind: unregister and remove the empty clone
                let _ = remove_tree(&dev.join("repos").join(slug));
                let keep: Vec<String> = std::fs::read_to_string(registry(dev))
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| l.split('\t').next() != Some(slug))
                    .map(String::from)
                    .collect();
                let _ = std::fs::write(registry(dev), keep.join("\n") + "\n");
                return Err(e.context("clone failed (check the URL and that git can authenticate)"));
            }
            let _ = git_run(&["-C", &bare_s, "remote", "set-head", "origin", "-a"]);
        }
        None => {
            // a brand-new local repo: one empty root commit on main, no remote yet
            let empty = git_out(&["-C", &bare_s, "mktree"], &[])?; // stdin is empty → the empty tree
            let ident = [("GIT_AUTHOR_NAME", "worktree-dev"), ("GIT_AUTHOR_EMAIL", "wtd@localhost"),
                         ("GIT_COMMITTER_NAME", "worktree-dev"), ("GIT_COMMITTER_EMAIL", "wtd@localhost")];
            let root = git_out(&["-C", &bare_s, "commit-tree", &empty, "-m", "chore: initial commit"], &ident)?;
            git_run(&["-C", &bare_s, "update-ref", "refs/heads/main", &root])?;
            git_run(&["-C", &bare_s, "symbolic-ref", "HEAD", "refs/heads/main"])?;
        }
    }
    // same per-repo setup add-repo.sh does
    git_run(&["-C", &bare_s, "config", "core.untrackedCache", "true"])?;
    git_run(&["-C", &bare_s, "config", "feature.manyFiles", "true"])?;
    if let Some(r) = crate::repos::find(dev, slug) {
        crate::repos::prepare(dev, &r)?;
    }
    let branch = default_branch(&bare).unwrap_or_else(|| "?".into());
    println!("ready: {slug} (default branch {branch})");
    Ok(0)
}

fn repo_rm(dev: &Path, slug: &str, delete_clone: bool) -> Result<i32> {
    if crate::repos::find(dev, slug).is_some_and(|r| r.is_folder()) {
        bail!("'{slug}' is a clone in your repos folder — it isn't registered, so there's nothing to remove here");
    }
    let info = repo_list(dev).into_iter().find(|r| r.slug == slug).with_context(|| format!("no repo '{slug}'"))?;
    if info.worktrees > 0 || info.archived > 0 {
        bail!("'{slug}' still has {} worktree(s) and {} archived — remove them first", info.worktrees, info.archived);
    }
    let keep: Vec<String> = std::fs::read_to_string(registry(dev))?
        .lines()
        .filter(|l| l.starts_with('#') || l.split('\t').next().map(str::trim) != Some(slug))
        .map(String::from)
        .collect();
    std::fs::write(registry(dev), keep.join("\n") + "\n")?;
    let mut cfg = load_config(dev);
    if let Some(r) = cfg.get_mut("repos").and_then(Value::as_object_mut) {
        r.remove(slug);
    }
    save_config(dev, &cfg)?;
    if delete_clone {
        let d = dev.join("repos").join(slug);
        if d.exists() {
            remove_tree(&d).with_context(|| format!("removing {}", d.display()))?;
        }
    }
    out(json!({ "ok": true }))
}

fn gh() -> Result<PathBuf> {
    which("gh").context("GitHub CLI (gh) not found — install it: winget install GitHub.cli")
}

pub(crate) fn gh_json(args: &[&str]) -> Result<Value> {
    let mut c = Command::new(gh()?);
    c.args(args).stdin(Stdio::null());
    win::no_window(&mut c);
    let o = c.output()?;
    if !o.status.success() {
        let e = String::from_utf8_lossy(&o.stderr).trim().to_string();
        if e.contains("auth login") || e.contains("not logged") {
            bail!("gh isn't logged in — use Log in under GitHub");
        }
        if e.contains("read:project") || e.contains("scope") {
            bail!("{e}\n→ grant project access: gh auth refresh -s read:project,project");
        }
        bail!("{e}");
    }
    Ok(serde_json::from_slice(&o.stdout).unwrap_or(Value::Null))
}

fn test_issues(dev: &Path, slug: &str) -> Result<i32> {
    let cfg = load_config(dev);
    let src = cfg.pointer(&format!("/repos/{slug}/github/issueSource")).cloned().context("no issue source set for this repo")?;
    match src.get("kind").and_then(Value::as_str) {
        Some("repo") => {
            let repo = src.get("repo").and_then(Value::as_str).context("missing repo")?;
            let mut args = vec!["issue", "list", "-R", repo, "--state", "open", "--limit", "200", "--json", "number,title"];
            let a = src.get("assignee").and_then(Value::as_str).unwrap_or("");
            if !a.is_empty() {
                args.extend(["--assignee", a]);
            }
            let labels: Vec<String> = src.get("labels").and_then(Value::as_array).map(|l| l.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            let joined = labels.join(",");
            if !joined.is_empty() {
                args.extend(["--label", &joined]);
            }
            let v = gh_json(&args)?;
            let items = v.as_array().cloned().unwrap_or_default();
            out(json!({ "ok": true, "count": items.len(), "sample": items.iter().take(3).collect::<Vec<_>>() }))
        }
        Some("project") => {
            let owner = src.get("owner").and_then(Value::as_str).context("missing owner")?;
            let num = src.get("number").and_then(Value::as_u64).context("missing number")?.to_string();
            let v = gh_json(&["project", "item-list", &num, "--owner", owner, "--format", "json", "--limit", "300"])?;
            let items = v.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            let field = src.get("statusField").and_then(Value::as_str).unwrap_or("Status").to_lowercase();
            let offer: Vec<String> = src.get("offer").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_lowercase)).collect()).unwrap_or_default();
            // `gh project item-list` flattens single-select fields to lower-cased keys
            let matching: Vec<&Value> = items
                .iter()
                .filter(|it| offer.is_empty() || it.get(&field).and_then(Value::as_str).is_some_and(|s| offer.contains(&s.to_lowercase())))
                .collect();
            let sample: Vec<Value> = matching.iter().take(3).map(|it| json!({ "title": it.get("title"), "status": it.get(&field) })).collect();
            out(json!({ "ok": true, "count": matching.len(), "total": items.len(), "sample": sample }))
        }
        _ => bail!("issue source kind must be 'repo' or 'project'"),
    }
}

/// Single-select fields (with their options) of a Project, so Settings can offer real column names.
fn project_fields(owner: &str, number: &str) -> Result<i32> {
    let v = gh_json(&["project", "field-list", number, "--owner", owner, "--format", "json"])?;
    let fields: Vec<Value> = v
        .get("fields")
        .and_then(Value::as_array)
        .map(|fs| {
            fs.iter()
                .filter(|f| f.get("options").and_then(Value::as_array).is_some())
                .map(|f| json!({
                    "name": f.get("name"),
                    "options": f.get("options").and_then(Value::as_array).map(|o| o.iter().filter_map(|x| x.get("name").cloned()).collect::<Vec<_>>()),
                }))
                .collect()
        })
        .unwrap_or_default();
    out(json!({ "fields": fields }))
}

// --- accounts ---------------------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct AccountInfo {
    provider: &'static str, // "claude" | "codex"
    name: String,
    dir: String,
    email: String,
    plan: String,
    logged_in: bool,
    /// roles this account is the default for
    roles: Vec<String>,
}

fn roles_path(home: &Path) -> PathBuf {
    home.join(".claude-accounts").join("roles.conf")
}

/// role → "claude:<name>" | "codex:<name>" (absent = the default Claude login)
pub(crate) fn read_roles(home: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(roles_path(home))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(r, v)| {
            let v = v.trim();
            (r.trim().to_string(), if v.contains(':') { v.to_string() } else { format!("claude:{v}") })
        })
        .collect()
}

fn write_roles(home: &Path, roles: &BTreeMap<String, String>) -> Result<()> {
    let p = roles_path(home);
    std::fs::create_dir_all(p.parent().unwrap())?;
    // Claude accounts are stored bare (`dev=work`) so the bash tooling keeps reading them unchanged.
    let text: String = roles
        .iter()
        .map(|(r, v)| format!("{r}={}\n", v.strip_prefix("claude:").unwrap_or(v)))
        .collect();
    std::fs::write(p, text)?;
    Ok(())
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

fn jwt_claims(token: &str) -> Option<Value> {
    serde_json::from_slice(&b64url_decode(token.split('.').nth(1)?)?).ok()
}

fn claude_account(name: &str, dir: PathBuf, json: PathBuf) -> AccountInfo {
    let cfg = read_json(&json);
    let creds = read_json(&dir.join(".credentials.json"));
    let email = cfg.as_ref().and_then(|j| j.pointer("/oauthAccount/emailAddress")).and_then(Value::as_str).unwrap_or("").to_string();
    let plan = creds.as_ref().and_then(|c| c.pointer("/claudeAiOauth/subscriptionType")).and_then(Value::as_str).unwrap_or("").to_string();
    let logged_in = creds.as_ref().and_then(|c| c.pointer("/claudeAiOauth/accessToken")).and_then(Value::as_str).is_some_and(|t| !t.is_empty());
    AccountInfo { provider: "claude", name: name.into(), dir: dir.to_string_lossy().into(), email, plan, logged_in, roles: vec![] }
}

fn codex_account(name: &str, dir: PathBuf) -> AccountInfo {
    let auth = read_json(&dir.join("auth.json"));
    let api_key = auth.as_ref().and_then(|a| a.get("OPENAI_API_KEY")).and_then(Value::as_str).is_some_and(|k| !k.is_empty());
    let claims = auth.as_ref().and_then(|a| a.pointer("/tokens/id_token")).and_then(Value::as_str).and_then(jwt_claims);
    let email = claims.as_ref().and_then(|c| c.get("email")).and_then(Value::as_str).unwrap_or("").to_string();
    let plan = claims
        .as_ref()
        .and_then(|c| c.pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type"))
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| if api_key { "API key".into() } else { String::new() });
    let logged_in = api_key || claims.is_some();
    AccountInfo { provider: "codex", name: name.into(), dir: dir.to_string_lossy().into(), email, plan, logged_in, roles: vec![] }
}

fn subdirs(p: &Path) -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(p)
        .map(|rd| rd.flatten().filter(|e| e.path().is_dir()).map(|e| (e.file_name().to_string_lossy().to_string(), e.path())).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn account_list(home: &Path) -> Vec<AccountInfo> {
    let mut v = vec![claude_account("default", home.join(".claude"), home.join(".claude.json"))];
    for (n, d) in subdirs(&home.join(".claude-accounts")) {
        let j = d.join(".claude.json");
        v.push(claude_account(&n, d, j));
    }
    v.push(codex_account("default", home.join(".codex")));
    for (n, d) in subdirs(&home.join(".codex-accounts")) {
        v.push(codex_account(&n, d));
    }
    let roles = read_roles(home);
    for a in &mut v {
        let key = format!("{}:{}", a.provider, a.name);
        a.roles = ROLES
            .iter()
            .filter(|r| match roles.get(**r) {
                Some(v) => *v == key,
                None => a.provider == "claude" && a.name == "default",
            })
            .map(|r| r.to_string())
            .collect();
    }
    v
}

fn account_dir(home: &Path, provider: &str, name: &str) -> Result<PathBuf> {
    Ok(match (provider, name) {
        ("claude", "default") => home.join(".claude"),
        ("codex", "default") => home.join(".codex"),
        ("claude", n) => home.join(".claude-accounts").join(n),
        ("codex", n) => home.join(".codex-accounts").join(n),
        (p, _) => bail!("provider must be claude or codex (got '{p}')"),
    })
}

pub fn account_main(args: &[String]) -> Result<i32> {
    let home = paths::home_dir()?;
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        [] | ["ls"] if human(&a) => table(
            &["provider", "name", "email", "plan", "roles"],
            account_list(&home)
                .into_iter()
                .map(|x| {
                    let email = if x.logged_in { x.email } else { "(not logged in)".into() };
                    vec![x.provider.into(), x.name, email, x.plan, x.roles.join(",")]
                })
                .collect(),
        ),
        [] | ["ls"] | ["ls", "--json"] => out(serde_json::to_value(account_list(&home))?),
        ["add", provider, name] => {
            if !valid_name(name) || *name == "default" {
                bail!("account name must be letters, digits, - or _ (and not 'default')");
            }
            let dir = account_dir(&home, provider, name)?;
            if dir.exists() {
                bail!("account '{name}' already exists ({})", dir.display());
            }
            std::fs::create_dir_all(&dir)?;
            // share the default login's settings (hooks, statusline, permissions) with the new one
            let seed = match *provider {
                "claude" => Some((home.join(".claude").join("settings.json"), dir.join("settings.json"))),
                _ => Some((home.join(".codex").join("config.toml"), dir.join("config.toml"))),
            };
            if let Some((from, to)) = seed {
                if from.is_file() {
                    let _ = std::fs::copy(from, to);
                }
            }
            if *provider == "claude" {
                register_mcp(&dir);
            }
            out(json!({ "ok": true, "dir": dir }))
        }
        ["rm", provider, name] => {
            if *name == "default" {
                bail!("the default login can't be removed");
            }
            let dir = account_dir(&home, provider, name)?;
            if !dir.is_dir() {
                bail!("no {provider} account '{name}'");
            }
            let key = format!("{provider}:{name}");
            let mut roles = read_roles(&home);
            let before = roles.len();
            roles.retain(|_, v| *v != key);
            if roles.len() != before {
                write_roles(&home, &roles)?; // its roles fall back to the default login
            }
            remove_tree(&dir).with_context(|| format!("removing {}", dir.display()))?;
            out(json!({ "ok": true }))
        }
        ["login", provider, name] | ["login", provider, name, ..] => {
            // an interactive sign-in under that account's config dir, in this terminal
            let dir = account_dir(&home, provider, name)?;
            let (prog, var) = if *provider == "codex" { ("codex", "CODEX_HOME") } else { ("claude", "CLAUDE_CONFIG_DIR") };
            let (exe, prefix) = crate::run::command_for(prog);
            let mut c = Command::new(exe);
            c.args(&prefix);
            if *provider == "codex" {
                c.arg("login");
            }
            if *name != "default" {
                std::fs::create_dir_all(&dir)?;
                c.env(var, &dir);
            }
            Ok(c.status()?.code().unwrap_or(1))
        }
        ["usage"] | ["usage", _] => {
            let name = a.get(1).copied().unwrap_or("default");
            let dir = account_dir(&home, "claude", name)?;
            match crate::daemon::usage_fetch_dir(&dir, &mut Default::default()) {
                Some(u) => {
                    let f = |l: &Option<wtd_core::model::Limit>| l.as_ref().and_then(|l| l.used).map(|v| format!("{v:.0}%")).unwrap_or("--".into());
                    let r = |l: &Option<wtd_core::model::Limit>| l.as_ref().map(|l| crate::daemon::fmt_local(l.resets_at)).unwrap_or_default();
                    println!("usage — {name}\n  5h  {:>5}   resets {}\n  7d  {:>5}   resets {}", f(&u.five_hour), r(&u.five_hour), f(&u.seven_day), r(&u.seven_day));
                    Ok(0)
                }
                None => bail!("no usage for '{name}' (not logged in, offline, or rate-limited — try again shortly)"),
            }
        }
        ["switch", slug, wtname, rest @ ..] => {
            // move a worktree's session to another Claude account (`--to <name>`, else the logged-in one
            // with the most headroom): copy its transcript there so reopening resumes the same chat,
            // and rebind the session. Prints the target name (the extension then relaunches + compacts).
            let dev = paths::dev_root()?;
            let session = crate::wt::session_name(slug, wtname);
            let key = crate::wt::session_key(&session);
            let cur = crate::wt::read_state(&dev, "session-accounts", &key).unwrap_or_else(|| "default".into());
            let dir_of = |n: &str| if n == "default" { home.join(".claude") } else { home.join(".claude-accounts").join(n) };
            let to = match rest {
                ["--to", t, ..] => t.to_string(),
                _ => {
                    let mut best: Option<(f64, String)> = None;
                    for acc in account_list(&home).into_iter().filter(|x| x.provider == "claude" && x.logged_in && x.name != cur) {
                        if let Some(u) = crate::daemon::usage_fetch_dir(&dir_of(&acc.name), &mut Default::default()) {
                            let util = [u.five_hour, u.seven_day].iter().filter_map(|l| l.as_ref().and_then(|l| l.used)).fold(0.0, f64::max);
                            if util < 100.0 && best.as_ref().is_none_or(|(b, _)| util < *b) {
                                best = Some((util, acc.name.clone()));
                            }
                        }
                    }
                    best.map(|(_, n)| n).context("no other logged-in account has capacity (add one in Settings → Accounts)")?
                }
            };
            if to == cur {
                bail!("the session is already on '{to}'");
            }
            let tdir = dir_of(&to);
            if !tdir.join(".credentials.json").is_file() {
                bail!("'{to}' isn't a logged-in Claude account");
            }
            let wtp = paths::worktree_path(&dev, &format!("{slug}/{wtname}"));
            if let Some(id) = crate::wt::read_state(&dev, "session-ids", &key) {
                let enc: String = wtp.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
                let src = dir_of(&cur).join("projects").join(&enc).join(format!("{id}.jsonl"));
                let dst = tdir.join("projects").join(&enc).join(format!("{id}.jsonl"));
                if src.is_file() {
                    std::fs::create_dir_all(dst.parent().unwrap())?;
                    std::fs::copy(&src, &dst)?;
                    eprintln!("copied the conversation into '{to}' (reopening resumes the same chat)");
                }
            }
            crate::wt::write_state(&dev, "session-accounts", &key, &to)?;
            println!("{to}");
            Ok(0)
        }
        ["use", role, target] => {
            if !ROLES.contains(role) {
                bail!("role must be one of {}", ROLES.join(", "));
            }
            let mut roles = read_roles(&home);
            if *target == "default" || *target == "claude:default" {
                roles.remove(*role);
            } else {
                let (provider, name) = target.split_once(':').unwrap_or(("claude", target));
                if !account_dir(&home, provider, name)?.is_dir() {
                    bail!("no {provider} account '{name}'");
                }
                if provider == "codex" && *role != "dev" {
                    bail!("{role} runs on Claude; pick a Claude account");
                }
                roles.insert(role.to_string(), format!("{provider}:{name}"));
            }
            write_roles(&home, &roles)?;
            out(json!({ "ok": true }))
        }
        _ => bail!("usage: wtd account ls | add <claude|codex> <name> | rm <claude|codex> <name> | use <dev|review|assistant> <provider:name|default>"),
    }
}

/// Give a new Claude login the fleet MCP tools (each config dir has its own MCP registry).
fn register_mcp(dir: &Path) {
    let (Some(claude), Ok(exe)) = (which("claude"), std::env::current_exe()) else { return };
    let mut c = Command::new(claude);
    c.args(["mcp", "add", "--scope", "user", "wtd", "--"]).arg(exe).arg("mcp").env("CLAUDE_CONFIG_DIR", dir)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let _ = win::no_window(&mut c).status();
}

// --- environment / GitHub status -------------------------------------------------------------------

pub fn env_main(_args: &[String]) -> Result<i32> {
    let (claude, codex, gh) = (which("claude"), which("codex"), which("gh"));
    let mut github = json!({ "installed": gh.is_some(), "logged_in": false });
    if let Some(gh) = gh {
        // one call gives both the login (body) and the token's scopes (X-Oauth-Scopes header)
        let mut c = Command::new(gh);
        c.args(["api", "-i", "user"]).stdin(Stdio::null()).stderr(Stdio::null());
        if let Ok(o) = win::no_window(&mut c).output() {
            if o.status.success() {
                let text = String::from_utf8_lossy(&o.stdout);
                let (head, body) = text.split_once("\r\n\r\n").or_else(|| text.split_once("\n\n")).unwrap_or(("", &text));
                let scopes: Vec<String> = head
                    .lines()
                    .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("x-oauth-scopes")).map(|(_, v)| v))
                    .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
                    .unwrap_or_default();
                let login = serde_json::from_str::<Value>(body.trim()).ok().and_then(|b| b.get("login").and_then(Value::as_str).map(String::from));
                github = json!({ "installed": true, "logged_in": login.is_some(), "login": login, "scopes": scopes,
                                 "has_project_scope": scopes.iter().any(|s| s == "read:project" || s == "project") });
            }
        }
    }
    out(json!({
        "claude": claude.map(|p| p.to_string_lossy().to_string()),
        "codex": codex.map(|p| p.to_string_lossy().to_string()),
        "github": github,
        "tray_logon": crate::tray::start_at_logon(),
        "wtd": std::env::current_exe()?.to_string_lossy(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_urls() {
        assert_eq!(github_repo_from_url("https://github.com/D-Luop/luop-software-mono-repo").as_deref(), Some("D-Luop/luop-software-mono-repo"));
        assert_eq!(github_repo_from_url("git@github.com:a/b.git").as_deref(), Some("a/b"));
        assert_eq!(github_repo_from_url("https://gitlab.com/a/b"), None);
        assert_eq!(github_repo_from_url("(local)"), None);
    }

    #[test]
    fn jwt_email() {
        // header.{"email":"x@y.z","https://api.openai.com/auth":{"chatgpt_plan_type":"plus"}}.sig
        let payload = "eyJlbWFpbCI6InhAeS56IiwiaHR0cHM6Ly9hcGkub3BlbmFpLmNvbS9hdXRoIjp7ImNoYXRncHRfcGxhbl90eXBlIjoicGx1cyJ9fQ";
        let c = jwt_claims(&format!("h.{payload}.s")).unwrap();
        assert_eq!(c["email"], "x@y.z");
        assert_eq!(c.pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type").unwrap(), "plus");
    }

    #[test]
    fn github_validation() {
        assert!(validate_github(&json!({"repo":"a/b","issueSource":{"kind":"repo","repo":"a/b"}})).is_ok());
        assert!(validate_github(&json!({"issueSource":{"kind":"project","owner":"o","number":3}})).is_ok());
        assert!(validate_github(&json!({"issueSource":{"kind":"project","owner":"o"}})).is_err());
        assert!(validate_github(&json!({"repo":"nope"})).is_err());
    }
}
