//! `wtd hook plan-nudge`: PreToolUse(Edit/Write/apply_patch) reminders that keep
//! `.claude/plans/active-plan.md` handoff-ready. Never blocks: it only adds context for the agent.
//!
//! - Source edit with no active plan → remind once per session.
//! - Every `STALE_EVERY` source edits with no plan edit in between → "plan is stale".
//!
//! State (`<state>/plan-nudge/<key>`): `session`, `warned` (0/1), `edits` — one per line, the same
//! format `.wtd/hooks/plan-nudge.sh` writes on hosts without wtd.exe.

use std::path::{Path, PathBuf};

use wtd_core::model::id_to_key;
use wtd_core::status::is_scratch_path;

use crate::paths::WtRef;

pub const STALE_EVERY: u32 = 15;

pub const NO_PLAN: &str = "No active plan: write `.claude/plans/active-plan.md` (use /blueprint) before editing source. \
    Include the steps, a Progress Log and a `## Handoff` section (current step, next action, open questions, decisions, gotchas, \
    files in flux) so another agent could take over at any moment.";

#[derive(Debug, Default, PartialEq)]
pub struct State {
    pub session: String,
    pub warned: bool,
    pub edits: u32,
}

impl State {
    pub fn parse(s: &str) -> State {
        let mut l = s.lines();
        State {
            session: l.next().unwrap_or("").trim().to_string(),
            warned: l.next().is_some_and(|w| w.trim() == "1"),
            edits: l.next().and_then(|n| n.trim().parse().ok()).unwrap_or(0),
        }
    }

    pub fn render(&self) -> String {
        format!("{}\n{}\n{}\n", self.session, u8::from(self.warned), self.edits)
    }
}

pub fn plan_path(root: &Path) -> PathBuf {
    root.join(".claude").join("plans").join("active-plan.md")
}

fn is_plan(path: &str) -> bool {
    path.replace('\\', "/").to_lowercase().ends_with("/.claude/plans/active-plan.md")
}

fn stale(edits: u32) -> String {
    format!(
        "Plan is stale: {edits} source edits since `.claude/plans/active-plan.md` was last updated. Append to its Progress Log and \
         overwrite `## Handoff` (current step, exact next action, open questions, decisions + why, gotchas, files in flux) now."
    )
}

/// Apply one edit of `path` to `st`; returns the reminder to show, if any.
pub fn step(st: &mut State, session: &str, path: &str, plan_exists: bool) -> Option<String> {
    if st.session != session {
        *st = State { session: session.to_string(), ..State::default() };
    }
    if is_plan(path) {
        st.edits = 0;
        return None;
    }
    if is_scratch_path(path) {
        return None;
    }
    if !plan_exists {
        return (!std::mem::replace(&mut st.warned, true)).then(|| NO_PLAN.to_string());
    }
    st.edits += 1;
    st.edits.is_multiple_of(STALE_EVERY).then(|| stale(st.edits))
}

/// Hook output: shown to the user (`systemMessage`) and added to the agent's context.
pub fn output(msg: &str) -> String {
    serde_json::json!({
        "systemMessage": msg,
        "hookSpecificOutput": { "hookEventName": "PreToolUse", "additionalContext": msg },
    })
    .to_string()
}

pub fn hook(dev: &Path, wt: &WtRef, session: &str, path: &str) -> anyhow::Result<()> {
    let file = crate::wt::state(dev, "plan-nudge").join(id_to_key(&wt.id));
    let before = std::fs::read_to_string(&file).unwrap_or_default();
    let mut st = State::parse(&before);
    let msg = step(&mut st, session, path, plan_path(&wt.root).is_file());
    if st.render() != before {
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(&file, st.render())?;
    }
    if let Some(m) = msg {
        println!("{}", output(&m));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "D:/dev/wt/a/src/main.rs";
    const PLAN: &str = r"D:\dev\wt\a\.claude\plans\active-plan.md";

    #[test]
    fn no_plan_warns_once_per_session() {
        let mut st = State::default();
        assert_eq!(step(&mut st, "s1", SRC, false).as_deref(), Some(NO_PLAN));
        assert_eq!(step(&mut st, "s1", SRC, false), None);
        assert_eq!(step(&mut st, "s2", SRC, false).as_deref(), Some(NO_PLAN), "new session warns again");
    }

    #[test]
    fn scratch_and_plan_edits_never_warn() {
        let mut st = State::default();
        assert_eq!(step(&mut st, "s", PLAN, false), None);
        assert_eq!(step(&mut st, "s", "D:/dev/wt/a/pr-notes.md", false), None);
        assert_eq!(step(&mut st, "s", "D:/dev/wt/a/.claude/plans/other.md", false), None);
        assert!(!st.warned);
    }

    #[test]
    fn stale_every_n_source_edits_until_plan_edit() {
        let mut st = State::default();
        for _ in 1..STALE_EVERY {
            assert_eq!(step(&mut st, "s", SRC, true), None);
        }
        assert!(step(&mut st, "s", SRC, true).unwrap().starts_with("Plan is stale: 15 "));
        assert_eq!(step(&mut st, "s", SRC, true), None);
        assert_eq!(step(&mut st, "s", PLAN, true), None);
        assert_eq!(st.edits, 0);
        for _ in 1..STALE_EVERY {
            assert_eq!(step(&mut st, "s", SRC, true), None);
        }
        assert!(step(&mut st, "s", SRC, true).is_some());
    }

    #[test]
    fn state_round_trips() {
        let st = State { session: "abc".into(), warned: true, edits: 7 };
        assert_eq!(State::parse(&st.render()), st);
        assert_eq!(State::parse(""), State::default());
    }

    #[test]
    fn output_is_non_blocking_context() {
        let v: serde_json::Value = serde_json::from_str(&output("hi")).unwrap();
        assert_eq!(v["systemMessage"], "hi");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "hi");
        assert!(v["hookSpecificOutput"].get("permissionDecision").is_none());
    }
}
