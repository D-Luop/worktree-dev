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

const PLAN_MAX: usize = 12_000;
const HANDOFF_MAX: usize = 4_000;

/// Like `clip`, but cuts at a line boundary when one is reasonably close.
fn clip_lines(s: &str, max: usize) -> String {
    let c = clip(s, max);
    let Some(body) = c.strip_suffix('…') else { return c };
    let body = match body.rfind('\n') {
        Some(i) if i > max / 2 => &body[..i],
        _ => body,
    };
    format!("{body}\n…")
}

/// The plan's `## Handoff` section (heading included), and everything else.
fn split_handoff(plan: &str) -> (Option<String>, String) {
    let (mut handoff, mut rest) = (Vec::new(), Vec::new());
    let (mut in_handoff, mut fence) = (false, false);
    for l in plan.lines() {
        if l.trim_start().starts_with("```") {
            fence = !fence;
        } else if !fence && (l.starts_with("# ") || l.starts_with("## ")) {
            in_handoff = l.strip_prefix("## ").is_some_and(|h| h.trim().to_lowercase().starts_with("handoff"));
        }
        if in_handoff { &mut handoff } else { &mut rest }.push(l);
    }
    ((!handoff.is_empty()).then(|| handoff.join("\n")), rest.join("\n"))
}

/// A markdown block fenced so that fences inside the plan can't close it.
fn fenced(body: &str) -> String {
    let longest = body.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let f = "`".repeat((longest + 1).max(3));
    format!("{f}markdown\n{body}\n{f}\n\n")
}

fn plan_section(wtp: &Path) -> String {
    let path = wtp.join(".claude").join("plans").join("active-plan.md");
    let Ok(plan) = std::fs::read_to_string(&path) else {
        return "## Active plan\n\nNone: `.claude/plans/active-plan.md` doesn't exist. Reconstruct the intent from the transcript below, \
                and write one (`/blueprint`) before editing source.\n\n"
            .to_string();
    };
    let (handoff, rest) = split_handoff(&plan);
    let mut s = format!(
        "## Active plan (the source of truth; read this first)\n\nInlined from `{}`, Handoff section first. Long plans are clipped, so open the file for the full text.\n\n",
        path.display()
    );
    let budget = match &handoff {
        Some(h) => {
            let h = clip_lines(h, HANDOFF_MAX);
            s.push_str(&fenced(&h));
            PLAN_MAX.saturating_sub(h.len())
        }
        None => {
            s.push_str(
                "The plan has no `## Handoff` section. Work out the current step and next action from its steps, its Progress Log \
                 and the transcript below, then add one.\n\n",
            );
            PLAN_MAX
        }
    };
    s.push_str(&fenced(&clip_lines(&rest, budget)));
    s
}

pub fn render(d: &Digest, wtp: &Path, from: &str, to: &str) -> String {
    let mut s = format!(
        "# Handoff brief\n\nThis worktree's previous agent session (`{from}`) stopped, most likely at its usage limit. You (`{to}`) are taking over. \
         Below: the active plan, the git state, then raw notes extracted from its transcript (not a summary).\n\n{}## Where things stand\n\n{}",
        plan_section(wtp),
        git_section(wtp)
    );
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
         Read the handoff brief at {} (it starts with the active plan's Handoff section), check the git state, then tell me in two or three lines where the work stands \
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

    fn worktree(name: &str, plan: Option<&str>) -> PathBuf {
        let wt = std::env::temp_dir().join(format!("wtd-handoff-wt-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wt);
        std::fs::create_dir_all(wt.join(".claude").join("plans")).unwrap();
        if let Some(p) = plan {
            std::fs::write(wt.join(".claude").join("plans").join("active-plan.md"), p).unwrap();
        }
        wt
    }

    fn digest() -> Digest {
        Digest { asks: vec!["add a login page".into()], replies: vec!["On it.".into()], ..Digest::default() }
    }

    #[test]
    fn brief_inlines_plan_handoff_first() {
        let wt = worktree("present", Some(concat!(
            "# Plan: login\n\n## Sequential Steps\n- [x] Step 1\n- [ ] Step 2 — form\n\n",
            "## Handoff\n- **Next action:** wire `submit()`\n### Gotchas\n```\n## not a heading\n```\n\n",
            "## Progress Log\n- Step 1 done\n",
        )));
        let b = render(&digest(), &wt, "a", "b");
        let at = |needle: &str| b.find(needle).unwrap_or_else(|| panic!("missing {needle:?} in\n{b}"));
        assert!(at("## Handoff") < at("# Plan: login"), "Handoff before the rest of the plan");
        assert!(at("wire `submit()`") < at("# Plan: login"));
        assert!(at("## not a heading") < at("# Plan: login"), "fenced lines stay in the Handoff section");
        assert!(at("## Progress Log") < at("## Where things stand"));
        assert!(at("## Where things stand") < at("What the user asked"), "transcript tail below the plan");
        assert!(b.contains("````markdown"), "fence longer than the plan's own");
        assert_eq!(b.matches("## Handoff\n").count(), 1);
        std::fs::remove_dir_all(wt).ok();
    }

    #[test]
    fn brief_without_plan() {
        let wt = worktree("absent", None);
        let b = render(&digest(), &wt, "a", "b");
        assert!(b.contains("## Active plan\n\nNone:"), "{b}");
        assert!(!b.contains("```markdown"));
        assert!(b.find("## Active plan").unwrap() < b.find("What the user asked").unwrap());
        std::fs::remove_dir_all(wt).ok();
    }

    #[test]
    fn brief_with_plan_missing_handoff() {
        let wt = worktree("nohandoff", Some("# Plan: x\n\n## Sequential Steps\n- [ ] Step 1\n"));
        let b = render(&digest(), &wt, "a", "b");
        assert!(b.contains("no `## Handoff` section"), "{b}");
        assert!(b.contains("- [ ] Step 1"));
        std::fs::remove_dir_all(wt).ok();
    }

    #[test]
    fn long_plan_is_clipped_at_a_line() {
        let log: String = (0..2000).map(|i| format!("- entry {i}\n")).collect();
        let wt = worktree("long", Some(&format!("# Plan: x\n\n## Handoff\n- next: y\n\n## Progress Log\n{log}")));
        let s = plan_section(&wt);
        assert!(s.len() < PLAN_MAX + 1000, "{}", s.len());
        assert!(s.contains("- next: y") && s.contains("- entry 0\n") && s.contains("\n…\n"));
        std::fs::remove_dir_all(wt).ok();
    }

    #[test]
    fn prompt_is_one_line() {
        assert!(!prompt(Path::new(r"C:\x\h.md")).contains('\n'));
        assert_eq!(clip("abcdef", 3), "abc…");
    }
}
