//! Hand a worktree's work from one agent session to another (another account, or Claude ↔ Codex).
//!
//! The brief is built from the previous session's transcript on disk rather than by asking that agent
//! to summarize, because the usual reason for switching is that its account is out of quota.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use crate::gitx;

/// State key whose presence makes the next `agent` launch start fresh, seeded with this brief.
pub const PENDING: &str = "handoff-pending";

#[derive(Default, Debug)]
pub struct Digest {
    /// The latest compaction summary (Claude), i.e. what happened before the visible transcript.
    pub summary: Option<String>,
    pub asks: Vec<String>,
    pub replies: Vec<String>,
    pub actions: Vec<String>,
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn one_line(s: &str, max: usize) -> String {
    clip(&s.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

/// Injected harness text rather than something the user typed.
fn is_noise(text: &str) -> bool {
    let t = text.trim_start();
    t.is_empty() || t.starts_with('<') || t.starts_with("Caveat:") || t.starts_with("# AGENTS.md") || t.starts_with("[Request interrupted")
}

fn action(name: &str, input: &Value) -> String {
    let field = ["file_path", "path", "command", "pattern", "url", "description"]
        .iter()
        .find_map(|k| input.get(*k))
        .map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))
        .unwrap_or_default();
    if field.is_empty() { name.to_string() } else { format!("{name}: {}", one_line(&field, 160)) }
}

fn lines(path: &Path) -> impl Iterator<Item = Value> {
    std::fs::read_to_string(path).unwrap_or_default().lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).collect::<Vec<_>>().into_iter()
}

pub fn digest_claude(path: &Path) -> Digest {
    let mut d = Digest::default();
    for v in lines(path) {
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let content = v.pointer("/message/content");
        let texts = || -> Vec<String> {
            match content {
                Some(Value::String(s)) => vec![s.clone()],
                Some(Value::Array(a)) => a.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str().map(String::from)).collect(),
                _ => vec![],
            }
        };
        match v["type"].as_str() {
            Some("user") if v.get("isCompactSummary").and_then(Value::as_bool) == Some(true) => d.summary = texts().into_iter().next(),
            Some("user") if v.get("isMeta").and_then(Value::as_bool) != Some(true) => {
                d.asks.extend(texts().into_iter().filter(|t| !is_noise(t)));
            }
            Some("assistant") => {
                d.replies.extend(texts().into_iter().filter(|t| !t.trim().is_empty()));
                if let Some(Value::Array(a)) = content {
                    for b in a.iter().filter(|b| b["type"] == "tool_use") {
                        d.actions.push(action(b["name"].as_str().unwrap_or("tool"), &b["input"]));
                    }
                }
            }
            _ => {}
        }
    }
    d
}

pub fn digest_codex(path: &Path) -> Digest {
    let mut d = Digest::default();
    for v in lines(path).filter(|v| v["type"] == "response_item") {
        let p = &v["payload"];
        match p["type"].as_str() {
            Some("message") => {
                let texts = p["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).map(String::from);
                match p["role"].as_str() {
                    Some("user") => d.asks.extend(texts.filter(|t| !is_noise(t))),
                    Some("assistant") => d.replies.extend(texts.filter(|t| !t.trim().is_empty())),
                    _ => {}
                }
            }
            Some("function_call") | Some("custom_tool_call") => {
                let input = p["arguments"].as_str().and_then(|a| serde_json::from_str(a).ok()).unwrap_or_else(|| p["input"].clone());
                d.actions.push(action(p["name"].as_str().unwrap_or("tool"), &input));
            }
            _ => {}
        }
    }
    d
}

fn same_dir(a: &str, b: &Path) -> bool {
    let norm = |s: &str| s.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    norm(a) == norm(&b.to_string_lossy())
}

/// The newest Codex rollout (`<CODEX_HOME>/sessions/**/rollout-*.jsonl`) whose session ran in `cwd`.
pub fn codex_rollout(codex_home: &Path, cwd: &Path) -> Option<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("rollout-")) {
                out.push(p);
            }
        }
    }
    let mut all = vec![];
    walk(&codex_home.join("sessions"), &mut all);
    let modified = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    all.sort_by_key(|p| std::cmp::Reverse(modified(p)));
    all.into_iter().find(|p| {
        let first = std::fs::read_to_string(p).ok().and_then(|t| t.lines().next().map(String::from)).unwrap_or_default();
        serde_json::from_str::<Value>(&first).ok().and_then(|v| v.pointer("/payload/cwd").and_then(Value::as_str).map(|c| same_dir(c, cwd))).unwrap_or(false)
    })
}

fn git_section(wtp: &Path) -> String {
    let w = gitx::s(wtp);
    let run = |args: &[&str]| gitx::out(&[&["-C", w.as_str()], args].concat()).unwrap_or_default().trim_end().to_string();
    let mut s = format!("- Branch: `{}`\n", run(&["rev-parse", "--abbrev-ref", "HEAD"]));
    let status = run(&["status", "--short"]);
    let status: Vec<&str> = status.lines().collect();
    if status.is_empty() {
        s.push_str("- Working tree clean\n");
    } else {
        s.push_str(&format!("- Uncommitted changes ({}):\n```\n{}\n```\n", status.len(), status.iter().take(40).copied().collect::<Vec<_>>().join("\n")));
    }
    s.push_str(&format!("- Recent commits:\n```\n{}\n```\n", run(&["log", "--oneline", "-8"])));
    s
}

fn tail<T>(v: &[T], n: usize) -> &[T] {
    &v[v.len().saturating_sub(n)..]
}

pub fn render(d: &Digest, wtp: &Path, from: &str, to: &str) -> String {
    let mut s = format!(
        "# Handoff brief\n\nThis worktree's previous agent session (`{from}`) stopped, most likely at its usage limit. You (`{to}`) are taking over. \
         This brief was extracted from its transcript, so it's raw notes, not a summary.\n\n## Where things stand\n\n{}",
        git_section(wtp)
    );
    let plan = wtp.join(".claude").join("plans").join("active-plan.md");
    if plan.is_file() {
        s.push_str(&format!("- Active plan (read it first; the source of truth): `{}`\n", plan.display()));
    }
    if let Some(sum) = &d.summary {
        s.push_str(&format!("\n## Earlier context (the previous session's compaction summary)\n\n{}\n", clip(sum, 6000)));
    }
    if !d.asks.is_empty() {
        s.push_str("\n## What the user asked (oldest → newest)\n\n");
        for a in tail(&d.asks, 12) {
            s.push_str(&format!("- {}\n", clip(a, 1500).replace('\n', "\n  ")));
        }
    }
    if !d.replies.is_empty() {
        s.push_str("\n## The previous agent's last messages (oldest → newest)\n\n");
        for r in tail(&d.replies, 6) {
            s.push_str(&format!("---\n{}\n", clip(r, 2500)));
        }
    }
    if !d.actions.is_empty() {
        s.push_str("\n## Its most recent actions (oldest → newest)\n\n");
        for a in tail(&d.actions, 30) {
            s.push_str(&format!("- {a}\n"));
        }
    }
    s
}

/// The first prompt of the session that takes over. Kept to one line: it travels as a CLI argument.
pub fn prompt(brief: &Path) -> String {
    format!(
        "You are taking over this worktree from a previous agent session that stopped, most likely at its usage limit. \
         Read the handoff brief at {} and the active plan, check the git state, then tell me in two or three lines where the work stands \
         and what you'll do next, and continue from there.",
        brief.display()
    )
}

/// Write the brief to `.wtd/state/handoffs/<key>.md` and mark it pending for the next launch.
pub fn write(dev: &Path, key: &str, body: &str) -> Result<PathBuf> {
    let dir = crate::wt::state(dev, "handoffs");
    std::fs::create_dir_all(&dir)?;
    let p = dir.join(format!("{key}.md"));
    std::fs::write(&p, body)?;
    crate::wt::write_state(dev, PENDING, key, &p.to_string_lossy())?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, body: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("wtd-handoff-{}-{name}", std::process::id()));
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn claude_transcript() {
        let p = tmp("c.jsonl", concat!(
            r#"{"type":"user","message":{"role":"user","content":"add a login page"}}"#, "\n",
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"meta stuff"}}"#, "\n",
            r#"{"type":"user","message":{"role":"user","content":"<command-name>/model</command-name>"}}"#, "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"On it."},{"type":"tool_use","name":"Edit","input":{"file_path":"src/login.rs"}}]}}"#, "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#, "\n",
            r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":"Summary: built the form"}}"#, "\n",
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"subagent"}]}}"#, "\n",
            "not json\n",
        ));
        let d = digest_claude(&p);
        assert_eq!(d.asks, vec!["add a login page"]);
        assert_eq!(d.replies, vec!["On it."]);
        assert_eq!(d.actions, vec!["Edit: src/login.rs"]);
        assert_eq!(d.summary.as_deref(), Some("Summary: built the form"));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn codex_rollout_digest_and_lookup() {
        let home = std::env::temp_dir().join(format!("wtd-handoff-codex-{}", std::process::id()));
        let day = home.join("sessions").join("2026").join("10").join("22");
        std::fs::create_dir_all(&day).unwrap();
        let body = concat!(
            r#"{"type":"session_meta","payload":{"id":"x","cwd":"D:/dev/wt/a"}}"#, "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>x</environment_context>"}]}}"#, "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fix the tests"}]}}"#, "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"cargo\",\"test\"]}"}}"#, "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Tests pass."}]}}"#, "\n",
        );
        std::fs::write(day.join("rollout-1.jsonl"), body).unwrap();
        std::fs::write(day.join("rollout-2.jsonl"), r#"{"type":"session_meta","payload":{"cwd":"D:/dev/wt/b"}}"#).unwrap();
        let found = codex_rollout(&home, Path::new(r"D:\dev\wt\A")).unwrap();
        assert!(found.ends_with("rollout-1.jsonl"));
        let d = digest_codex(&found);
        assert_eq!(d.asks, vec!["fix the tests"]);
        assert_eq!(d.replies, vec!["Tests pass."]);
        assert_eq!(d.actions, vec![r#"shell: ["cargo","test"]"#]);
        assert!(codex_rollout(&home, Path::new(r"D:\dev\wt\c")).is_none());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn prompt_is_one_line() {
        assert!(!prompt(Path::new(r"C:\x\h.md")).contains('\n'));
        assert_eq!(clip("abcdef", 3), "abc…");
    }
}
