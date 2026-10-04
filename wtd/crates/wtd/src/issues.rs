//! `wtd issues …`: what New Session offers to work on for a repo, from its configured issue source
//! (Settings → Repositories): GitHub repo issues, or the items of a GitHub Project board.
//!
//!   wtd issues ls <slug>                 → { source, items: [Item] }   (mine first)
//!   wtd issues show <repo> <number>      → { number, title, url, body, labels }
//!   wtd issues start <slug> <item-id>    → move a Project card to the configured "on start" column

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::paths;
use crate::settings::{gh_json, github_repo_from_url, load_config, out, registered};

#[derive(Serialize)]
struct Item {
    kind: &'static str, // "issue" | "draft"
    number: Option<u64>,
    title: String,
    url: Option<String>,
    repo: Option<String>,
    labels: Vec<String>,
    assignees: Vec<String>,
    status: Option<String>,
    mine: bool,
    /// Project item id (to move the card on start).
    item_id: Option<String>,
}

fn strs(v: Option<&Value>, key: &str) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from).or_else(|| x.get(key).and_then(Value::as_str).map(String::from))).collect())
        .unwrap_or_default()
}

fn my_login() -> String {
    gh_json(&["api", "user"]).ok().and_then(|u| u.get("login").and_then(Value::as_str).map(String::from)).unwrap_or_default()
}

/// The repo's issue source; unconfigured = the linked (or detected) GitHub repo's issues.
fn source_for(slug: &str) -> Result<Value> {
    let dev = paths::dev_root()?;
    let url = registered(&dev).into_iter().find(|(s, _)| s == slug).map(|(_, u)| u).with_context(|| format!("no repo '{slug}'"))?;
    let gh = load_config(&dev).pointer(&format!("/repos/{slug}/github")).cloned().unwrap_or(Value::Null);
    if let Some(src) = gh.get("issueSource").filter(|v| v.is_object()) {
        return Ok(src.clone());
    }
    let repo = gh.get("repo").and_then(Value::as_str).map(String::from).or_else(|| github_repo_from_url(&url));
    match repo {
        Some(r) => Ok(json!({ "kind": "repo", "repo": r, "labels": [], "assignee": "" })),
        None => bail!("'{slug}' isn't linked to GitHub — set it up in Settings → Repositories"),
    }
}

fn list(slug: &str) -> Result<Value> {
    let src = source_for(slug)?;
    let me = my_login();
    let mut items = match src.get("kind").and_then(Value::as_str) {
        Some("repo") => {
            let repo = src.get("repo").and_then(Value::as_str).context("issue source has no repo")?;
            let mut args = vec!["issue", "list", "-R", repo, "--state", "open", "--limit", "100", "--json", "number,title,url,labels,assignees"];
            let a = src.get("assignee").and_then(Value::as_str).unwrap_or("");
            if !a.is_empty() {
                args.extend(["--assignee", a]);
            }
            let labels = strs(src.get("labels"), "name").join(",");
            if !labels.is_empty() {
                args.extend(["--label", &labels]);
            }
            gh_json(&args)?
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|i| {
                    let assignees = strs(i.get("assignees"), "login");
                    Item {
                        kind: "issue",
                        number: i.get("number").and_then(Value::as_u64),
                        title: i.get("title").and_then(Value::as_str).unwrap_or("").into(),
                        url: i.get("url").and_then(Value::as_str).map(String::from),
                        repo: Some(repo.to_string()),
                        labels: strs(i.get("labels"), "name"),
                        mine: !me.is_empty() && assignees.iter().any(|a| a == &me),
                        assignees,
                        status: None,
                        item_id: None,
                    }
                })
                .collect::<Vec<_>>()
        }
        Some("project") => {
            let owner = src.get("owner").and_then(Value::as_str).context("project source has no owner")?;
            let num = src.get("number").and_then(Value::as_u64).context("project source has no number")?.to_string();
            let field = src.get("statusField").and_then(Value::as_str).unwrap_or("Status").to_lowercase();
            let offer: Vec<String> = strs(src.get("offer"), "name").iter().map(|s| s.to_lowercase()).collect();
            let v = gh_json(&["project", "item-list", &num, "--owner", owner, "--format", "json", "--limit", "300"])?;
            v.get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|it| {
                    let content = it.get("content").cloned().unwrap_or(Value::Null);
                    let kind = content.get("type").and_then(Value::as_str).unwrap_or("");
                    if kind == "PullRequest" {
                        return None;
                    }
                    // `gh project item-list` flattens single-select fields to lower-cased keys
                    let status = it.get(&field).and_then(Value::as_str).map(String::from);
                    if !offer.is_empty() && !status.as_ref().is_some_and(|s| offer.contains(&s.to_lowercase())) {
                        return None;
                    }
                    let assignees = strs(it.get("assignees"), "login");
                    Some(Item {
                        kind: if kind == "Issue" { "issue" } else { "draft" },
                        number: content.get("number").and_then(Value::as_u64),
                        title: it.get("title").or_else(|| content.get("title")).and_then(Value::as_str).unwrap_or("").into(),
                        url: content.get("url").and_then(Value::as_str).map(String::from),
                        repo: content.get("repository").and_then(Value::as_str).map(String::from),
                        labels: strs(it.get("labels"), "name"),
                        mine: !me.is_empty() && assignees.iter().any(|a| a == &me),
                        assignees,
                        status,
                        item_id: it.get("id").and_then(Value::as_str).map(String::from),
                    })
                })
                .collect()
        }
        k => bail!("unknown issue source kind {k:?}"),
    };
    items.sort_by_key(|i| !i.mine); // stable: assigned-to-me first, gh's order otherwise
    Ok(json!({ "source": src, "items": items }))
}

fn show(repo: &str, number: &str) -> Result<Value> {
    let v = gh_json(&["issue", "view", number, "-R", repo, "--json", "number,title,body,url,labels"])?;
    Ok(json!({
        "number": v.get("number"), "title": v.get("title"), "url": v.get("url"),
        "body": v.get("body"), "labels": strs(v.get("labels"), "name"),
    }))
}

/// Move a Project card to the source's `onStart` column (no-op when none is configured).
fn start(slug: &str, item_id: &str) -> Result<Value> {
    let src = source_for(slug)?;
    if src.get("kind").and_then(Value::as_str) != Some("project") {
        return Ok(json!({ "moved": false, "reason": "not a project source" }));
    }
    let target = src.get("onStart").and_then(Value::as_str).unwrap_or("");
    if target.is_empty() {
        return Ok(json!({ "moved": false, "reason": "no on-start column configured" }));
    }
    let owner = src.get("owner").and_then(Value::as_str).context("no owner")?;
    let num = src.get("number").and_then(Value::as_u64).context("no number")?.to_string();
    let field_name = src.get("statusField").and_then(Value::as_str).unwrap_or("Status");
    let project = gh_json(&["project", "view", &num, "--owner", owner, "--format", "json"])?;
    let pid = project.get("id").and_then(Value::as_str).context("project id not found")?.to_string();
    let fields = gh_json(&["project", "field-list", &num, "--owner", owner, "--format", "json"])?;
    let field = fields
        .get("fields")
        .and_then(Value::as_array)
        .and_then(|fs| fs.iter().find(|f| f.get("name").and_then(Value::as_str) == Some(field_name)))
        .with_context(|| format!("field '{field_name}' not found on the project"))?;
    let fid = field.get("id").and_then(Value::as_str).context("field id")?;
    let oid = field
        .get("options")
        .and_then(Value::as_array)
        .and_then(|os| os.iter().find(|o| o.get("name").and_then(Value::as_str) == Some(target)))
        .and_then(|o| o.get("id").and_then(Value::as_str))
        .with_context(|| format!("column '{target}' not found in '{field_name}'"))?;
    gh_json(&["project", "item-edit", "--id", item_id, "--project-id", &pid, "--field-id", fid, "--single-select-option-id", oid, "--format", "json"])?;
    Ok(json!({ "moved": true, "to": target }))
}

pub fn main(args: &[String]) -> Result<i32> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    out(match a.as_slice() {
        ["ls", slug] => list(slug)?,
        ["show", repo, number] => show(repo, number)?,
        ["start", slug, item] => start(slug, item)?,
        _ => bail!("usage: wtd issues ls <slug> | show <owner/repo> <number> | start <slug> <project-item-id>"),
    })
}
