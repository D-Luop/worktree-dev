//! The tray flyout's New Session form: what it offers (repos, platforms, accounts, issues), its
//! validation, and the `wtd agent … --detach` command Create runs. Drawing and input are in flyout.rs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Result};
use serde_json::{json, Value};
use wtd_core::model::Account;
use wtd_core::protocol::method;

use crate::frontend::Frontend;
use crate::{gitx, paths, platforms, win};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Branch,
    Plan,
    Delegator,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Branch, Kind::Plan, Kind::Delegator];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Branch => "Branch",
            Kind::Plan => "Plan",
            Kind::Delegator => "Delegator",
        }
    }
}

pub const PREFIXES: [&str; 3] = ["feat/", "fix/", "docs/"];
const PLAN_NAME: &str = "new-app";

#[derive(Clone, Debug, PartialEq)]
pub struct RepoOpt {
    pub slug: String,
    pub path: PathBuf,
    pub default_branch: String,
    /// `windows` | `wsl:<distro>`: the repo default, else the global one
    pub platform: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlatformOpt {
    pub id: String,
    pub label: String,
    pub ok: bool,
    pub hint: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AccountOpt {
    /// what `agent --account` takes: `<name>` (Claude) or `codex:<name>`
    pub label: String,
    pub provider: &'static str,
    pub email: String,
    pub logged_in: bool,
    /// max(5h, 7d) % used, when the daemon has polled it
    pub used: Option<f64>,
    /// the `dev` role: new sessions' configured default
    pub dev_default: bool,
}

impl AccountOpt {
    pub fn maxed(&self) -> bool {
        self.used.is_some_and(|u| u >= 100.0)
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct IssueOpt {
    pub number: Option<u64>,
    pub title: String,
    pub url: Option<String>,
    /// `owner/name`
    pub repo: Option<String>,
    pub labels: Vec<String>,
    /// a Project card to move on start
    pub item_id: Option<String>,
}

impl IssueOpt {
    pub fn label(&self) -> String {
        match self.number {
            Some(n) => format!("#{n} {}", self.title),
            None => self.title.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Load::Ready(v) => Some(v),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Repo,
    Kind,
    Name,
    Base,
    Platform,
    Account,
    Issue,
    Prompt,
    Open,
}

/// An inline message under a field; a warning doesn't block Create.
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub field: Field,
    pub text: String,
    pub warn: bool,
}

pub struct Form {
    pub kind: Kind,
    pub repos: Load<Vec<RepoOpt>>,
    pub repo: Option<String>,
    pub name: String,
    /// None = the repo's default branch
    pub base: Option<String>,
    /// the selected repo's local and origin branches, newest first
    pub branches: Load<Vec<String>>,
    pub platforms: Load<Vec<PlatformOpt>>,
    pub platform: Option<String>,
    pub accounts: Vec<AccountOpt>,
    pub account: Option<String>,
    pub issues: Load<Vec<IssueOpt>>,
    pub issue: Option<IssueOpt>,
    pub prompt: String,
    /// None = don't open
    pub open: Option<Frontend>,
    /// worktree ids that already exist (the fleet)
    pub existing: HashSet<String>,
    pub busy: bool,
    pub error: Option<String>,
}

impl Form {
    pub fn new(open: Frontend, accounts: Vec<AccountOpt>, existing: HashSet<String>) -> Form {
        let account = default_account(&accounts);
        Form {
            kind: Kind::Branch,
            repos: Load::Loading,
            repo: None,
            name: "feat/".into(),
            base: None,
            branches: Load::Loading,
            platforms: Load::Loading,
            platform: None,
            accounts,
            account,
            issues: Load::Loading,
            issue: None,
            prompt: String::new(),
            open: Some(open),
            existing,
            busy: false,
            error: None,
        }
    }

    pub fn repo_opt(&self) -> Option<&RepoOpt> {
        let slug = self.repo.as_deref()?;
        self.repos.ready()?.iter().find(|r| r.slug == slug)
    }

    /// Which fields this kind uses.
    pub fn uses(&self, f: Field) -> bool {
        match self.kind {
            Kind::Branch => true,
            Kind::Plan => !matches!(f, Field::Repo | Field::Base | Field::Platform | Field::Issue),
            Kind::Delegator => !matches!(f, Field::Name | Field::Base | Field::Platform | Field::Issue),
        }
    }

    pub fn set_kind(&mut self, k: Kind) {
        if k == self.kind {
            return;
        }
        let was_plan_default = self.kind == Kind::Plan && self.name == PLAN_NAME;
        if k == Kind::Plan && (self.name.is_empty() || self.name == "feat/") {
            self.name = PLAN_NAME.into();
        } else if k == Kind::Branch && (was_plan_default || self.name.is_empty()) {
            self.name = "feat/".into();
        }
        self.kind = k;
    }

    /// The repo changed: its branches, issues and default platform are reloaded by the caller.
    pub fn set_repo(&mut self, slug: &str) {
        if self.repo.as_deref() == Some(slug) {
            return;
        }
        self.repo = Some(slug.to_string());
        self.base = None;
        self.branches = Load::Loading;
        self.issues = Load::Loading;
        self.issue = None;
        self.platform = self.repo_opt().map(|r| r.platform.clone());
    }

    /// Swap the type prefix (`feat/` → `fix/`), or put one in front.
    pub fn set_prefix(&mut self, p: &str) {
        let rest = PREFIXES.iter().find_map(|q| self.name.strip_prefix(q)).unwrap_or(&self.name).to_string();
        self.name = format!("{p}{rest}");
    }

    /// An issue picked while the name is still just a prefix proposes `feat/<n>-<title>`.
    pub fn set_issue(&mut self, i: Option<IssueOpt>) {
        if let Some(i) = &i {
            if self.name.is_empty() || PREFIXES.contains(&self.name.as_str()) {
                self.name = branch_for(i);
            }
        }
        self.issue = i;
    }

    /// `<slug>/<name>` this form creates (or opens).
    pub fn id(&self) -> Option<String> {
        match self.kind {
            Kind::Branch => Some(format!("{}/{}", self.repo.as_deref()?, self.name.trim())),
            Kind::Plan => Some(format!("plan/{}", self.name.trim())),
            Kind::Delegator => Some(format!("{}/{}", paths::DELEGATE, self.repo.as_deref()?)),
        }
    }

    pub fn exists(&self) -> bool {
        self.id().is_some_and(|id| self.existing.contains(&id))
    }

    pub fn notes(&self) -> Vec<Note> {
        let mut out = vec![];
        let mut note = |field, text: &str, warn| out.push(Note { field, text: text.into(), warn });
        if self.uses(Field::Repo) {
            match &self.repos {
                Load::Ready(r) if r.is_empty() => note(Field::Repo, "No repos yet: add one in VS Code → Settings → Repositories", false),
                Load::Failed(e) => note(Field::Repo, e, false),
                Load::Ready(_) if self.repo_opt().is_none() => note(Field::Repo, "Pick a repo", false),
                Load::Ready(_) if !self.repo_opt().unwrap().path.is_dir() => note(Field::Repo, "This repo isn't cloned", false),
                _ => {}
            }
        }
        if self.uses(Field::Name) {
            let name = self.name.trim();
            if let Some(p) = PREFIXES.iter().find(|p| name == **p || name == p.trim_end_matches('/')) {
                note(Field::Name, &format!("Type a name after {}", p), false);
            } else if let Some(e) = branch_error(name) {
                note(Field::Name, e, false);
            } else if self.exists() && !self.busy {
                note(Field::Name, "This worktree exists: Create opens it", true);
            } else if self.kind == Kind::Branch && self.branches.ready().is_some_and(|b| b.iter().any(|b| b == name)) {
                note(Field::Name, "This branch exists: Create checks it out", true);
            }
        }
        if self.kind == Kind::Delegator && self.exists() && !self.busy {
            note(Field::Repo, "This repo has a delegator: Create opens it", true);
        }
        if self.uses(Field::Base) {
            if let (Some(b), Some(list)) = (&self.base, self.branches.ready()) {
                if !list.contains(b) {
                    note(Field::Base, "This branch isn't in the repo", false);
                }
            }
        }
        if self.uses(Field::Platform) && !self.exists() {
            if let (Some(p), Some(list)) = (&self.platform, self.platforms.ready()) {
                match list.iter().find(|o| &o.id == p) {
                    Some(o) if !o.ok => note(Field::Platform, &format!("{} isn't ready: {}", o.label, o.hint), false),
                    None => note(Field::Platform, &format!("{p} isn't installed"), false),
                    _ => {}
                }
            }
        }
        if let Some(a) = self.account.as_ref().and_then(|l| self.accounts.iter().find(|a| &a.label == l)) {
            if !a.logged_in {
                note(Field::Account, "Not logged in: log in from VS Code → Settings → Accounts", false);
            } else if a.maxed() {
                note(Field::Account, "This account is at its usage limit", true);
            }
        }
        out
    }

    pub fn can_create(&self) -> bool {
        !self.busy && self.notes().iter().all(|n| n.warn) && (!self.uses(Field::Repo) || self.repo_opt().is_some())
    }

    /// `wtd` argv for Create. `issue_file` is the work item written for `--issue-file`.
    pub fn agent_args(&self, issue_file: Option<&Path>) -> Result<Vec<String>> {
        let Some(id) = self.id() else { bail!("pick a repo") };
        let (slug, name) = id.split_once('/').unwrap();
        let mut a: Vec<String> = vec!["agent".into(), slug.into(), name.into(), "--detach".into()];
        let mut opt = |k: &str, v: &str| a.extend([k.to_string(), v.to_string()]);
        if let Some(acct) = &self.account {
            opt("--account", acct);
        }
        if self.kind == Kind::Branch {
            let default = self.repo_opt().map(|r| r.default_branch.as_str());
            if let Some(b) = self.base.as_deref().filter(|b| Some(*b) != default) {
                opt("--from", b);
            }
            // an existing worktree opens where it lives; naming another platform is an error
            if let Some(p) = self.platform.as_deref().filter(|_| !self.exists()) {
                opt("--platform", p);
            }
            if let Some(f) = issue_file {
                opt("--issue-file", &f.to_string_lossy().replace('\\', "/"));
            }
            if let Some(i) = &self.issue {
                if let (Some(r), Some(n)) = (&i.repo, i.number) {
                    opt("--ticket", &format!("{r}#{n}"));
                }
            }
        }
        let p = self.prompt.trim();
        if !p.is_empty() {
            opt("--prompt", p);
        }
        Ok(a)
    }

    /// The fields this kind shows, in tab order.
    pub fn fields(&self) -> Vec<Field> {
        Field::ALL.into_iter().filter(|f| self.uses(*f)).collect()
    }

    /// A select field's choices (empty for text fields and the kind switch).
    pub fn options(&self, f: Field) -> Vec<Opt> {
        let opt = |label: String, choice: Choice, on: bool| Opt { label, enabled: true, checked: on, choice };
        let mut v = match f {
            Field::Repo => self.repos.ready().map(|r| r.iter().map(|r| opt(r.slug.clone(), Choice::Repo(r.slug.clone()), self.repo.as_ref() == Some(&r.slug))).collect()).unwrap_or_default(),
            Field::Base => {
                let def = self.repo_opt().map(|r| r.default_branch.clone()).unwrap_or_default();
                let mut v = vec![opt(format!("{def} (default)"), Choice::Base(None), self.base.is_none())];
                let others = self.branches.ready().into_iter().flatten().filter(|b| **b != def).take(MENU_MAX);
                v.extend(others.map(|b| opt(b.clone(), Choice::Base(Some(b.clone())), self.base.as_ref() == Some(b))));
                v
            }
            Field::Platform => self
                .platforms
                .ready()
                .into_iter()
                .flatten()
                .map(|p| Opt {
                    label: if p.ok { p.label.clone() } else { format!("{} — {}", p.label, p.hint) },
                    enabled: p.ok,
                    checked: self.platform.as_ref() == Some(&p.id),
                    choice: Choice::Platform(p.id.clone()),
                })
                .collect(),
            Field::Account => {
                let best = self.accounts.iter().find(|a| a.logged_in && !a.maxed()).map(|a| a.label.clone());
                self.accounts
                    .iter()
                    .map(|a| Opt {
                        label: format!("{}{}", if best.as_ref() == Some(&a.label) { "★ " } else { "" }, account_text(a)),
                        enabled: a.logged_in,
                        checked: self.account.as_ref() == Some(&a.label),
                        choice: Choice::Account(a.label.clone()),
                    })
                    .collect()
            }
            Field::Issue => {
                let mut v = vec![opt("None".into(), Choice::Issue(None), self.issue.is_none())];
                let picked = |i: &IssueOpt| self.issue.as_ref().is_some_and(|x| x.number == i.number && x.title == i.title);
                v.extend(self.issues.ready().into_iter().flatten().take(MENU_MAX).map(|i| opt(i.label(), Choice::Issue(Some(i.clone())), picked(i))));
                v
            }
            Field::Open => [Some(Frontend::VsCode), Some(Frontend::Terminal), Some(Frontend::Zed), None]
                .into_iter()
                .map(|fe| opt(open_text(fe).into(), Choice::Open(fe), self.open == fe))
                .collect(),
            Field::Kind | Field::Name | Field::Prompt => return vec![],
        };
        let pending = match f {
            Field::Repo => state("repos", &self.repos),
            Field::Base => match &self.branches {
                Load::Ready(_) => None,
                l => state("branches", l),
            },
            Field::Platform => state("platforms", &self.platforms),
            Field::Issue => match &self.issues {
                Load::Ready(_) => None,
                l => state("issues", l),
            },
            _ => None,
        };
        if let Some(p) = pending {
            v.push(Opt { label: p, enabled: false, checked: false, choice: Choice::None });
        }
        v
    }

    pub fn choose(&mut self, c: Choice) {
        match c {
            Choice::Repo(s) => self.set_repo(&s),
            Choice::Base(b) => self.base = b,
            Choice::Platform(p) => self.platform = Some(p),
            Choice::Account(a) => self.account = Some(a),
            Choice::Issue(i) => self.set_issue(i),
            Choice::Open(o) => self.open = o,
            Choice::None => {}
        }
    }

    /// The next (or previous) enabled choice after the current one, for arrow keys on a select.
    pub fn step(&self, f: Field, forward: bool) -> Option<Choice> {
        let opts: Vec<Opt> = self.options(f).into_iter().filter(|o| o.enabled).collect();
        let cur = opts.iter().position(|o| o.checked);
        let i = match (cur, forward) {
            (None, _) => 0,
            (Some(i), true) => (i + 1).min(opts.len().checked_sub(1)?),
            (Some(i), false) => i.saturating_sub(1),
        };
        opts.get(i).map(|o| o.choice.clone())
    }

    /// What a select shows when closed.
    pub fn value_text(&self, f: Field) -> String {
        match f {
            Field::Repo => match (&self.repos, &self.repo) {
                (_, Some(r)) => r.clone(),
                (Load::Loading, _) => "Loading…".into(),
                (Load::Ready(v), _) if v.is_empty() => "No repos".into(),
                _ => "Pick a repo".into(),
            },
            Field::Base => match (&self.base, self.repo_opt()) {
                (Some(b), _) => b.clone(),
                (None, Some(r)) => format!("{} (default)", r.default_branch),
                (None, None) => "Default branch".into(),
            },
            Field::Platform => match &self.platform {
                Some(p) => self.platforms.ready().and_then(|l| l.iter().find(|o| &o.id == p)).map(|o| o.label.clone()).unwrap_or_else(|| platform_label(p)),
                None => "Default".into(),
            },
            Field::Account => match self.account.as_ref().and_then(|l| self.accounts.iter().find(|a| &a.label == l)) {
                Some(a) => account_text(a),
                None => "No logged-in account".into(),
            },
            Field::Issue => match (&self.issue, &self.issues) {
                (Some(i), _) => i.label(),
                (None, Load::Loading) => "None (loading issues…)".into(),
                (None, _) => "None".into(),
            },
            Field::Open => open_text(self.open).into(),
            Field::Kind => self.kind.label().into(),
            Field::Name => self.name.clone(),
            Field::Prompt => self.prompt.clone(),
        }
    }
}

const MENU_MAX: usize = 40;

/// The status line a select shows while its list isn't usable.
fn state<T>(what: &str, l: &Load<Vec<T>>) -> Option<String> {
    match l {
        Load::Loading => Some(format!("Loading {what}…")),
        Load::Failed(e) => Some(e.clone()),
        Load::Ready(v) if v.is_empty() => Some(format!("No {what}")),
        Load::Ready(_) => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Choice {
    Repo(String),
    Base(Option<String>),
    Platform(String),
    Account(String),
    Issue(Option<IssueOpt>),
    Open(Option<Frontend>),
    /// a status line ("Loading…"), not a choice
    None,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Opt {
    pub label: String,
    pub enabled: bool,
    pub checked: bool,
    pub choice: Choice,
}

impl Field {
    pub const ALL: [Field; 9] = [Field::Repo, Field::Kind, Field::Name, Field::Base, Field::Platform, Field::Account, Field::Issue, Field::Prompt, Field::Open];
}

fn account_text(a: &AccountOpt) -> String {
    let (provider, name) = match a.label.strip_prefix("codex:") {
        Some(n) => ("Codex", n),
        None => ("Claude", a.label.as_str()),
    };
    let state = match (a.logged_in, a.used) {
        (false, _) => " · not logged in".to_string(),
        (true, Some(u)) => format!(" · {u:.0}% used"),
        (true, None) => String::new(),
    };
    format!("{provider} · {name}{state}")
}

fn open_text(f: Option<Frontend>) -> &'static str {
    f.map(Frontend::label).unwrap_or("Don't open")
}

/// git's branch-name rules, as the New Session wizards check them.
pub fn branch_error(v: &str) -> Option<&'static str> {
    if v.contains("@{") {
        return Some("not a valid git branch name");
    }
    crate::new::branch_error(v)
}

fn slugify(t: &str, max: usize) -> String {
    let mut s = String::new();
    for c in t.to_lowercase().chars().filter(|c| !"`'\"".contains(*c)) {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    let s: String = s.chars().take(max).collect();
    s.trim_end_matches('-').to_string()
}

/// `fix/…` for bugs, `docs/…` for docs, else `feat/…`; the issue number first so branches sort by it.
pub fn branch_for(i: &IssueOpt) -> String {
    let labels = i.labels.join(" ").to_lowercase();
    let ty = if ["bug", "fix", "regression", "crash"].iter().any(|w| labels.contains(w)) {
        "fix"
    } else if labels.contains("doc") {
        "docs"
    } else {
        "feat"
    };
    let s = slugify(&i.title, 40);
    match i.number {
        Some(n) if s.is_empty() => format!("{ty}/{n}"),
        Some(n) => format!("{ty}/{n}-{s}"),
        None => format!("{ty}/{s}"),
    }
}

/// The work item `agent --issue-file` seeds into the worktree's CLAUDE.md. `detail` is `issues show`.
pub fn issue_markdown(i: &IssueOpt, detail: Option<&Value>) -> String {
    let r = i.number.map(|n| format!("#{n}")).unwrap_or_default();
    let title = detail.and_then(|d| d["title"].as_str()).unwrap_or(&i.title);
    let lead = if r.is_empty() { title.to_string() } else { format!("{r} {title}") };
    let head = match &i.url {
        Some(u) => format!("[{lead}]({u})"),
        None => lead,
    };
    let mut out = format!("**Work item:** {head}{}\n\n", i.repo.as_ref().map(|r| format!(" — {r}")).unwrap_or_default());
    if !i.labels.is_empty() {
        out.push_str(&format!("Labels: {}\n\n", i.labels.join(", ")));
    }
    if let Some(n) = i.number {
        out.push_str(&format!("When this work is ready for a PR, include `Closes {}#{n}` in pr-notes.md.\n\n", i.repo.as_deref().unwrap_or("")));
    }
    let body = detail.and_then(|d| d["body"].as_str()).unwrap_or("").trim();
    if !body.is_empty() {
        let body = if body.chars().count() > 8000 {
            format!("{}\n\n… (truncated — see the issue)", body.chars().take(8000).collect::<String>())
        } else {
            body.to_string()
        };
        out.push_str(&format!("### Issue description\n\n{body}\n\n"));
    }
    out
}

// --- options ----------------------------------------------------------------------------------------

/// Logged-in accounts first, most headroom first (unknown usage ranks mid-pack, as in the ⇄ picker).
pub fn rank_accounts(list: Vec<AccountOpt>) -> Vec<AccountOpt> {
    let mut v = list;
    let key = |a: &AccountOpt| (!a.logged_in, a.maxed(), (a.used.unwrap_or(50.0) * 10.0) as i64);
    v.sort_by(|a, b| key(a).cmp(&key(b)).then_with(|| a.label.cmp(&b.label)));
    v
}

/// The configured default unless it can't run (logged out or maxed), else the most headroom.
pub fn default_account(ranked: &[AccountOpt]) -> Option<String> {
    ranked
        .iter()
        .find(|a| a.dev_default && a.logged_in && !a.maxed())
        .or_else(|| ranked.iter().find(|a| a.logged_in))
        .map(|a| a.label.clone())
}

pub fn accounts(usage: &[Account]) -> Vec<AccountOpt> {
    let Ok(home) = paths::home_dir() else { return vec![] };
    let list = crate::settings::account_list(&home)
        .into_iter()
        .map(|a| {
            let label = if a.provider == "codex" { format!("codex:{}", a.name) } else { a.name.clone() };
            let used = usage.iter().find(|u| u.name == label).and_then(|u| {
                [&u.five_hour, &u.seven_day].into_iter().filter_map(|l| l.as_ref()?.used).reduce(f64::max)
            });
            AccountOpt { label, provider: a.provider, email: a.email, logged_in: a.logged_in, used, dev_default: a.roles.iter().any(|r| r == "dev") }
        })
        .collect();
    rank_accounts(list)
}

pub fn load_repos(dev: &Path) -> Vec<RepoOpt> {
    crate::repos::all(dev)
        .into_iter()
        .map(|r| RepoOpt {
            default_branch: if r.path.is_dir() { gitx::default_branch(&r.path) } else { "main".into() },
            platform: platforms::resolve_for(dev, &r.slug, None).to_string(),
            slug: r.slug,
            path: r.path,
        })
        .collect()
}

/// `for-each-ref` lines (`refs/heads/x`, `refs/remotes/origin/x`) → branch names, first seen wins.
pub fn parse_branches(out: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    out.lines()
        .filter_map(|l| l.trim().strip_prefix("refs/heads/").or_else(|| l.trim().strip_prefix("refs/remotes/origin/")))
        .filter(|b| *b != "HEAD" && !b.is_empty())
        .filter(|b| seen.insert(b.to_string()))
        .map(String::from)
        .collect()
}

pub fn load_branches(repo: &Path) -> Result<Vec<String>> {
    let out = gitx::out(&["-C", &gitx::s(repo), "for-each-ref", "--sort=-committerdate", "--format=%(refname)", "refs/heads", "refs/remotes/origin"])?;
    Ok(parse_branches(&out))
}

fn platform_label(id: &str) -> String {
    match id.strip_prefix("wsl:") {
        Some(d) => format!("WSL · {d}"),
        None => "Windows".into(),
    }
}

/// `wtd platforms` JSON → options.
pub fn parse_platforms(v: &Value) -> Vec<PlatformOpt> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let id = p["id"].as_str()?.to_string();
                    let label = platform_label(&id);
                    let hint = p["hints"].as_array().and_then(|h| h.first()).and_then(Value::as_str).unwrap_or("not available").to_string();
                    Some(PlatformOpt { id, label, ok: p["ok"].as_bool().unwrap_or(false), hint })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// From the daemon's cache when it's up (probing a distro can boot it).
pub fn load_platforms() -> Vec<PlatformOpt> {
    let v = crate::client::Client::connect()
        .ok()
        .flatten()
        .and_then(|mut c| c.request(method::PLATFORMS_LIST, json!({ "refresh": false })).ok())
        .unwrap_or_else(|| serde_json::to_value(platforms::detect()).unwrap_or_default());
    parse_platforms(&v)
}

/// `wtd issues ls` JSON → options.
pub fn parse_issues(v: &Value) -> Vec<IssueOpt> {
    let strs = |x: &Value| x.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default();
    v["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|i| IssueOpt {
                    number: i["number"].as_u64(),
                    title: i["title"].as_str().unwrap_or("").into(),
                    url: i["url"].as_str().map(String::from),
                    repo: i["repo"].as_str().map(String::from),
                    labels: strs(&i["labels"]),
                    item_id: i["item_id"].as_str().map(String::from),
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn load_issues(slug: &str) -> Result<Vec<IssueOpt>> {
    Ok(parse_issues(&crate::issues::list(slug)?))
}

// --- create -----------------------------------------------------------------------------------------

/// What Create runs, taken from the form on the UI thread.
pub struct Job {
    pub id: String,
    pub slug: String,
    pub issue: Option<(IssueOpt, PathBuf)>,
    pub args: Vec<String>,
}

impl Form {
    pub fn job(&self, dev: &Path) -> Result<Job> {
        let Some(id) = self.id() else { bail!("pick a repo") };
        let issue = self.issue.clone().filter(|_| self.kind == Kind::Branch);
        let file = issue.as_ref().map(|_| dev.join(".wtd").join("state").join("issues").join(format!("{}.md", id.replace('/', "__"))));
        let args = self.agent_args(file.as_deref())?;
        Ok(Job { slug: id.split_once('/').unwrap().0.to_string(), id, issue: issue.zip(file), args })
    }
}

/// Write the work item, run `wtd agent … --detach`, move the Project card. Err = the line to show.
pub fn create(dev: &Path, job: &Job) -> Result<()> {
    if let Some((i, file)) = &job.issue {
        let detail = match (&i.repo, i.number) {
            (Some(r), Some(n)) => crate::issues::show(r, &n.to_string()).ok(),
            _ => None,
        };
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(file, issue_markdown(i, detail.as_ref()))?;
    }
    // only stderr is read: the session it starts mustn't hold a pipe we wait on
    let out = win::no_window(std::process::Command::new(paths::wtd_exe()).args(&job.args))
        .current_dir(dev)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?
        .wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("wtd agent failed");
        // `wtd agent: <error>`
        bail!("{}", line.split_once(": ").filter(|(cmd, _)| cmd.starts_with("wtd ")).map_or(line, |(_, e)| e));
    }
    if let Some(item) = job.issue.as_ref().and_then(|(i, _)| i.item_id.as_deref()) {
        let _ = crate::issues::start(&job.slug, item);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(slug: &str) -> RepoOpt {
        RepoOpt { slug: slug.into(), path: std::env::temp_dir(), default_branch: "main".into(), platform: "windows".into() }
    }

    fn acct(label: &str, used: Option<f64>, logged_in: bool, dev_default: bool) -> AccountOpt {
        AccountOpt { label: label.into(), provider: "claude", email: String::new(), logged_in, used, dev_default }
    }

    fn form() -> Form {
        let mut f = Form::new(Frontend::VsCode, vec![acct("default", Some(10.0), true, true)], HashSet::new());
        f.repos = Load::Ready(vec![repo("app"), repo("web")]);
        f.set_repo("app");
        f.branches = Load::Ready(vec!["main".into(), "feat/old".into(), "release".into()]);
        f.platforms = Load::Ready(vec![
            PlatformOpt { id: "windows".into(), label: "Windows".into(), ok: true, hint: String::new() },
            PlatformOpt { id: "wsl:Ubuntu".into(), label: "WSL · Ubuntu".into(), ok: false, hint: "install git".into() },
        ]);
        f
    }

    fn errors(f: &Form) -> Vec<(Field, bool)> {
        f.notes().into_iter().map(|n| (n.field, n.warn)).collect()
    }

    #[test]
    fn branch_validation() {
        let mut f = form();
        assert_eq!(f.notes()[0].text, "Type a name after feat/", "a bare prefix needs a name");
        f.name = "feat/thing".into();
        assert!(errors(&f).is_empty());
        assert!(f.can_create());
        for bad in ["", "feat thing", "/x", "x/", "a..b", "x.lock", "a@{b}"] {
            f.name = bad.into();
            assert_eq!(errors(&f), [(Field::Name, false)], "{bad:?}");
            assert!(!f.can_create());
        }
        f.name = "feat/old".into();
        assert_eq!(errors(&f), [(Field::Name, true)], "an existing branch is a warning");
        assert!(f.can_create());
        f.existing.insert("app/feat/old".into());
        assert_eq!(f.notes()[0].text, "This worktree exists: Create opens it");
    }

    #[test]
    fn repo_platform_account_errors() {
        let mut f = form();
        f.name = "feat/x".into();
        f.platform = Some("wsl:Ubuntu".into());
        assert_eq!(f.notes()[0].text, "WSL · Ubuntu isn't ready: install git");
        f.platform = Some("wsl:Gone".into());
        assert_eq!(errors(&f), [(Field::Platform, false)]);
        f.platform = Some("windows".into());
        f.base = Some("nope".into());
        assert_eq!(errors(&f), [(Field::Base, false)]);
        f.base = None;
        f.accounts = vec![acct("work", None, false, false)];
        f.account = Some("work".into());
        assert_eq!(errors(&f), [(Field::Account, false)]);
        f.account = None;
        f.repos = Load::Ready(vec![]);
        assert!(!f.can_create());
        assert_eq!(errors(&f), [(Field::Repo, false)]);
    }

    #[test]
    fn kinds_use_their_fields() {
        let mut f = form();
        f.set_kind(Kind::Plan);
        assert_eq!(f.name, "new-app");
        assert_eq!(f.id().as_deref(), Some("plan/new-app"));
        f.platform = Some("wsl:Gone".into());
        assert!(f.can_create(), "a plan ignores the platform");
        f.set_kind(Kind::Delegator);
        assert_eq!(f.id().as_deref(), Some("delegate/app"));
        f.name = "not valid..".into();
        assert!(f.can_create(), "a delegator has no name to check");
        f.set_kind(Kind::Plan);
        f.set_kind(Kind::Branch);
        assert_eq!(f.name, "not valid..");
    }

    #[test]
    fn prefixes_and_issue_names() {
        let mut f = form();
        f.name = "feat/thing".into();
        f.set_prefix("fix/");
        assert_eq!(f.name, "fix/thing");
        f.name = "thing".into();
        f.set_prefix("docs/");
        assert_eq!(f.name, "docs/thing");
        f.name = "fix/".into();
        let i = IssueOpt { number: Some(42), title: "Crash on `open` — Zed's path!".into(), labels: vec!["bug".into()], ..Default::default() };
        f.set_issue(Some(i));
        assert_eq!(f.name, "fix/42-crash-on-open-zeds-path");
        f.set_issue(Some(IssueOpt { title: "Doc it".into(), labels: vec!["documentation".into()], ..Default::default() }));
        assert_eq!(f.name, "fix/42-crash-on-open-zeds-path", "a typed name is kept");
        assert_eq!(branch_for(&IssueOpt { title: "Doc it".into(), labels: vec!["docs".into()], ..Default::default() }), "docs/doc-it");
    }

    #[test]
    fn args_for_a_branch() {
        let mut f = form();
        f.name = "feat/x".into();
        assert_eq!(f.agent_args(None).unwrap(), ["agent", "app", "feat/x", "--detach", "--account", "default", "--platform", "windows"]);
        f.base = Some("main".into());
        f.prompt = "  go  ".into();
        f.issue = Some(IssueOpt { number: Some(7), repo: Some("o/app".into()), ..Default::default() });
        let file = Path::new(r"C:\dev\.wtd\state\issues\app__feat__x.md");
        assert_eq!(
            f.agent_args(Some(file)).unwrap(),
            ["agent", "app", "feat/x", "--detach", "--account", "default", "--platform", "windows", "--issue-file", "C:/dev/.wtd/state/issues/app__feat__x.md",
                "--ticket", "o/app#7", "--prompt", "go"]
        );
        f.base = Some("release".into());
        f.existing.insert("app/feat/x".into());
        let a = f.agent_args(None).unwrap();
        assert!(a.windows(2).any(|w| w == ["--from", "release"]));
        assert!(!a.contains(&"--platform".to_string()), "an existing worktree keeps its platform");
    }

    #[test]
    fn args_for_plan_and_delegator() {
        let mut f = form();
        f.base = Some("release".into());
        f.issue = Some(IssueOpt { number: Some(7), repo: Some("o/app".into()), ..Default::default() });
        f.set_kind(Kind::Plan);
        assert_eq!(f.agent_args(Some(Path::new("x.md"))).unwrap(), ["agent", "plan", "new-app", "--detach", "--account", "default"]);
        f.set_kind(Kind::Delegator);
        f.account = None;
        assert_eq!(f.agent_args(None).unwrap(), ["agent", "delegate", "app", "--detach"]);
        let job = f.job(Path::new("C:/dev")).unwrap();
        assert_eq!((job.id.as_str(), job.slug.as_str()), ("delegate/app", "delegate"));
        assert!(job.issue.is_none());
        f.set_kind(Kind::Branch);
        f.name = "feat/x".into();
        let job = f.job(Path::new("C:/dev")).unwrap();
        assert_eq!(job.issue.unwrap().1, Path::new("C:/dev/.wtd/state/issues/app__feat__x.md"));
        assert!(job.args.contains(&"C:/dev/.wtd/state/issues/app__feat__x.md".to_string()));
    }

    #[test]
    fn account_ranking() {
        let ranked = rank_accounts(vec![
            acct("busy", Some(80.0), true, true),
            acct("out", None, false, false),
            acct("fresh", Some(5.0), true, false),
            acct("unknown", None, true, false),
            acct("maxed", Some(100.0), true, false),
        ]);
        let order: Vec<&str> = ranked.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(order, ["fresh", "unknown", "busy", "maxed", "out"]);
        assert_eq!(default_account(&ranked).as_deref(), Some("busy"), "the configured default wins while it can run");
        let ranked = rank_accounts(vec![acct("busy", Some(100.0), true, true), acct("fresh", Some(5.0), true, false)]);
        assert_eq!(default_account(&ranked).as_deref(), Some("fresh"));
        assert_eq!(default_account(&[acct("out", None, false, true)]), None);
    }

    #[test]
    fn parsing() {
        let b = parse_branches("refs/heads/main\nrefs/remotes/origin/HEAD\nrefs/remotes/origin/main\nrefs/remotes/origin/feat/a\nrefs/heads/feat/b\n");
        assert_eq!(b, ["main", "feat/a", "feat/b"]);
        let p = parse_platforms(&json!([
            { "id": "windows", "kind": "windows", "ok": true, "hints": [] },
            { "id": "wsl:Ubuntu", "kind": "wsl", "distro": "Ubuntu", "ok": false, "hints": ["install claude"] },
        ]));
        assert_eq!(p[1], PlatformOpt { id: "wsl:Ubuntu".into(), label: "WSL · Ubuntu".into(), ok: false, hint: "install claude".into() });
        assert!(p[0].ok);
        let i = parse_issues(&json!({ "items": [{ "kind": "issue", "number": 3, "title": "T", "url": "u", "repo": "o/r", "labels": ["bug"], "item_id": null }] }));
        assert_eq!(i, [IssueOpt { number: Some(3), title: "T".into(), url: Some("u".into()), repo: Some("o/r".into()), labels: vec!["bug".into()], item_id: None }]);
        assert_eq!(i[0].label(), "#3 T");
    }

    #[test]
    fn select_options() {
        let mut f = form();
        f.accounts = rank_accounts(vec![acct("default", Some(60.0), true, true), acct("spare", Some(5.0), true, false), acct("out", None, false, false)]);
        let labels: Vec<(String, bool, bool)> = f.options(Field::Account).into_iter().map(|o| (o.label, o.enabled, o.checked)).collect();
        assert_eq!(labels, [
            ("★ Claude · spare · 5% used".into(), true, false),
            ("Claude · default · 60% used".into(), true, true),
            ("Claude · out · not logged in".into(), false, false),
        ]);
        let p = f.options(Field::Platform);
        assert_eq!((p[1].label.as_str(), p[1].enabled), ("WSL · Ubuntu — install git", false));
        assert_eq!(f.step(Field::Platform, true), Some(Choice::Platform("windows".into())), "disabled platforms are skipped");
        let base: Vec<String> = f.options(Field::Base).into_iter().map(|o| o.label).collect();
        assert_eq!(base, ["main (default)", "feat/old", "release"]);
        f.choose(f.step(Field::Base, true).unwrap());
        assert_eq!(f.value_text(Field::Base), "feat/old");
        assert_eq!(f.value_text(Field::Issue), "None (loading issues…)");
        assert_eq!(f.options(Field::Issue).last().unwrap().label, "Loading issues…");
        f.issues = Load::Failed("gh isn't installed".into());
        assert_eq!(f.options(Field::Issue).last().unwrap().choice, Choice::None);
        f.platforms = Load::Loading;
        assert_eq!(f.value_text(Field::Platform), "Windows");
        f.choose(Choice::Open(None));
        assert_eq!(f.value_text(Field::Open), "Don't open");
        f.choose(Choice::Repo("web".into()));
        assert_eq!((f.base.clone(), f.branches.clone()), (None, Load::Loading), "a new repo resets its branches");
        assert_eq!(f.fields(), Field::ALL);
        f.set_kind(Kind::Delegator);
        assert_eq!(f.fields(), [Field::Repo, Field::Kind, Field::Account, Field::Prompt, Field::Open]);
    }

    #[test]
    fn work_item_markdown() {
        let i = IssueOpt { number: Some(3), title: "T".into(), url: Some("https://x/3".into()), repo: Some("o/r".into()), labels: vec!["bug".into()], item_id: None };
        let md = issue_markdown(&i, Some(&json!({ "title": "Title", "body": " Body \n" })));
        assert_eq!(md, "**Work item:** [#3 Title](https://x/3) — o/r\n\nLabels: bug\n\nWhen this work is ready for a PR, include `Closes o/r#3` in pr-notes.md.\n\n### Issue description\n\nBody\n\n");
        let draft = IssueOpt { title: "Draft".into(), ..Default::default() };
        assert_eq!(issue_markdown(&draft, None), "**Work item:** Draft\n\n");
    }
}
