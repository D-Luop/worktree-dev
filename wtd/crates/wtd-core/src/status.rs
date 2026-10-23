//! A worktree's Claude status: the state machine formerly in `.wtd/hooks/wt-status.sh`.
//!
//! States: working (busy) · input (your turn) · limited (waiting for a usage-limit reset) · reviewing
//! · pr · done · stopped · none.
//! `pr` and `done` are sticky milestones that don't freeze live activity: a new turn shows `working`
//! while stashing the milestone, a read-only turn restores it, and a real source edit drops it.
//! `reviewing` is owned by the reviewer and survives agent turns. `limited` is live like `working`: it
//! holds through turn-end notifications and clears on the next prompt or tool call.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    None,
    Working,
    Input,
    Limited,
    Reviewing,
    Pr,
    Done,
    Stopped,
}

impl Status {
    pub fn parse(s: &str) -> Status {
        match s.trim() {
            "working" => Status::Working,
            "input" => Status::Input,
            "limited" => Status::Limited,
            "reviewing" => Status::Reviewing,
            "pr" => Status::Pr,
            "done" => Status::Done,
            "stopped" => Status::Stopped,
            _ => Status::None,
        }
    }

    /// The sentinel-file spelling (empty for `None`).
    pub fn as_str(self) -> &'static str {
        match self {
            Status::None => "",
            Status::Working => "working",
            Status::Input => "input",
            Status::Limited => "limited",
            Status::Reviewing => "reviewing",
            Status::Pr => "pr",
            Status::Done => "done",
            Status::Stopped => "stopped",
        }
    }

    fn is_milestone(self) -> bool {
        matches!(self, Status::Pr | Status::Done)
    }
}

/// What a hook (or a manual `agent done|pr|wip`, or the reviewer) reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Event {
    /// No transition; re-publish the current state.
    Sync,
    /// Manual revert to working; clears any milestone.
    Wip,
    /// A file edit (PreToolUse Edit/Write). `path` empty = unknown.
    Edit { path: String },
    Reviewing,
    Reviewed,
    Done,
    Pr,
    /// A new prompt (UserPromptSubmit).
    Working,
    /// Any tool call (PreToolUse).
    Tool,
    /// Turn died on a usage limit (StopFailure `rate_limit`). `resets_at`: unix seconds, 0 = unknown.
    Limited { resets_at: i64 },
    /// Turn ended (Stop / Notification).
    Stop,
    /// Session ended (SessionEnd).
    SessionEnd,
}

impl Event {
    /// Parse the hook CLI's event word (`wtd hook <word>`); `path` only matters for `edit`.
    pub fn from_word(word: &str, path: &str) -> Option<Event> {
        Some(match word {
            "sync" => Event::Sync,
            "wip" | "working-manual" => Event::Wip,
            "edit" => Event::Edit { path: path.to_string() },
            "reviewing" => Event::Reviewing,
            "reviewed" => Event::Reviewed,
            "done" => Event::Done,
            "pr" => Event::Pr,
            "working" => Event::Working,
            "tool" => Event::Tool,
            "limited" => Event::Limited { resets_at: 0 },
            "stop" => Event::Stop,
            "sessionend" => Event::SessionEnd,
            _ => return None,
        })
    }
}

/// Persisted state: the visible status, the stashed milestone (`.claude-status.resume`) and, while
/// `limited`, when the limit resets (`.claude-status.limit`; 0 = unknown).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StatusState {
    pub status: Status,
    pub stash: Status,
    pub resets_at: i64,
}

/// Edits to these don't count as "real source edits" (they don't invalidate a milestone).
pub fn is_scratch_path(path: &str) -> bool {
    if path.is_empty() {
        return true;
    }
    let p = path.replace('\\', "/");
    p.ends_with("/pr-notes.md")
        || p.ends_with("/CLAUDE.md")
        || p.rsplit('/').next().is_some_and(|f| f.starts_with(".claude-status"))
        || p.contains("/.claude/")
}

/// Apply one event. Returns the new state; callers persist only when it differs.
pub fn apply(cur: StatusState, ev: &Event) -> StatusState {
    let (status, stash) = transition(cur.status, cur.stash, ev);
    let resets_at = match (status, ev) {
        (Status::Limited, Event::Limited { resets_at }) => *resets_at,
        (Status::Limited, _) => cur.resets_at,
        _ => 0,
    };
    StatusState { status, stash, resets_at }
}

fn transition(status: Status, stash: Status, ev: &Event) -> (Status, Status) {
    let cur = (status, stash);
    let none = Status::None;
    match ev {
        Event::Sync => cur,
        Event::Wip => (Status::Working, none),
        Event::Edit { path } => {
            if is_scratch_path(path) {
                cur
            } else {
                (Status::Working, none)
            }
        }
        Event::Reviewing if status.is_milestone() => cur,
        Event::Reviewing => (Status::Reviewing, stash),
        Event::Reviewed if status.is_milestone() => cur,
        Event::Reviewed => (Status::Input, stash),
        Event::Done => (Status::Done, none),
        Event::Pr => (Status::Pr, none),
        Event::Working | Event::Tool | Event::Limited { .. } => {
            let next = if matches!(ev, Event::Limited { .. }) { Status::Limited } else { Status::Working };
            match status {
                s if s.is_milestone() => (next, s),
                Status::Reviewing => cur,
                _ => (next, stash),
            }
        }
        // a turn-end notification (e.g. idle) after the limit hit must not hide it
        Event::Stop if status == Status::Limited => cur,
        Event::Stop | Event::SessionEnd => {
            if stash != none {
                return (stash, none);
            }
            let keep = matches!(status, Status::Reviewing | Status::Done | Status::Pr);
            match (ev, keep) {
                (_, true) => cur,
                (Event::Stop, false) => (Status::Input, stash),
                _ => (Status::Stopped, stash),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Status::*;

    fn run(start: StatusState, evs: &[Event]) -> StatusState {
        evs.iter().fold(start, |s, e| apply(s, e))
    }
    fn edit(p: &str) -> Event {
        Event::Edit { path: p.into() }
    }
    fn st(status: Status, stash: Status) -> StatusState {
        StatusState { status, stash, resets_at: 0 }
    }
    fn limited(resets_at: i64) -> Event {
        Event::Limited { resets_at }
    }
    fn lim(resets_at: i64, stash: Status) -> StatusState {
        StatusState { status: Limited, stash, resets_at }
    }

    #[test]
    fn turn_cycle() {
        let s = run(StatusState::default(), &[Event::Working, Event::Tool]);
        assert_eq!(s, st(Working, None));
        assert_eq!(apply(s, &Event::Stop), st(Input, None));
        assert_eq!(apply(s, &Event::SessionEnd), st(Stopped, None));
    }

    #[test]
    fn milestone_survives_read_only_turn() {
        for m in [Done, Pr] {
            let s = run(st(m, None), &[Event::Working, Event::Tool]);
            assert_eq!(s, st(Working, m));
            assert_eq!(apply(s, &Event::Stop), st(m, None));
            assert_eq!(apply(s, &Event::SessionEnd), st(m, None));
        }
    }

    #[test]
    fn source_edit_drops_milestone() {
        let s = run(st(Done, None), &[Event::Working, edit("D:/w/src/a.go"), Event::Stop]);
        assert_eq!(s, st(Input, None));
    }

    #[test]
    fn scratch_edits_ignored() {
        for p in ["", "/w/pr-notes.md", "/w/CLAUDE.md", "/w/.claude-status", "/w/.claude/plans/active-plan.md",
                  "D:\\w\\pr-notes.md", "D:\\w\\.claude\\x.md"] {
            assert_eq!(apply(st(Done, None), &edit(p)), st(Done, None), "{p}");
            assert_eq!(apply(StatusState::default(), &edit(p)), StatusState::default(), "{p}");
        }
    }

    #[test]
    fn reviewing_is_protected() {
        let s = st(Reviewing, None);
        assert_eq!(apply(s, &Event::Working), s);
        assert_eq!(apply(s, &Event::Tool), s);
        assert_eq!(apply(s, &Event::Stop), s);
        assert_eq!(apply(s, &Event::SessionEnd), s);
        assert_eq!(apply(s, &Event::Reviewed), st(Input, None));
        assert_eq!(apply(s, &edit("/w/src/x")), st(Working, None));
    }

    #[test]
    fn review_events_dont_override_milestones() {
        assert_eq!(apply(st(Pr, None), &Event::Reviewing), st(Pr, None));
        assert_eq!(apply(st(Done, None), &Event::Reviewed), st(Done, None));
        assert_eq!(apply(st(Input, None), &Event::Reviewing), st(Reviewing, None));
    }

    #[test]
    fn manual_marks() {
        assert_eq!(apply(st(Working, Done), &Event::Pr), st(Pr, None));
        assert_eq!(apply(st(Working, Pr), &Event::Done), st(Done, None));
        assert_eq!(apply(st(Done, None), &Event::Wip), st(Working, None));
    }

    #[test]
    fn limit_then_new_turn() {
        let s = run(StatusState::default(), &[Event::Working, Event::Tool, limited(1_800_000_000)]);
        assert_eq!(s, lim(1_800_000_000, None));
        assert_eq!(apply(s, &Event::Stop), s, "idle notification keeps it limited");
        assert_eq!(apply(s, &Event::Sync), s);
        assert_eq!(apply(s, &edit("/w/pr-notes.md")), s);
        assert_eq!(apply(s, &Event::Working), st(Working, None));
        assert_eq!(apply(s, &Event::Tool), st(Working, None));
        assert_eq!(apply(s, &edit("/w/src/a.rs")), st(Working, None));
        assert_eq!(apply(s, &Event::SessionEnd), st(Stopped, None));
        assert_eq!(apply(lim(0, None), &limited(5)), lim(5, None), "a later hit updates the reset");
    }

    #[test]
    fn limit_stashes_milestone_like_working() {
        for m in [Done, Pr] {
            assert_eq!(apply(st(m, None), &limited(7)), lim(7, m));
            let s = run(st(m, None), &[Event::Working, limited(7)]);
            assert_eq!(s, lim(7, m));
            assert_eq!(apply(s, &Event::Stop), s);
            assert_eq!(apply(s, &Event::Working), st(Working, m));
            assert_eq!(run(s, &[Event::Working, Event::Stop]), st(m, None));
            assert_eq!(apply(s, &Event::SessionEnd), st(m, None));
            assert_eq!(apply(s, &Event::Done), st(Done, None));
            assert_eq!(apply(s, &Event::Pr), st(Pr, None));
        }
    }

    #[test]
    fn limit_leaves_review_alone() {
        assert_eq!(apply(st(Reviewing, None), &limited(7)), st(Reviewing, None));
    }

    #[test]
    fn limited_word_roundtrip() {
        assert_eq!(Status::parse(Limited.as_str()), Limited);
        assert_eq!(Event::from_word("limited", ""), Some(limited(0)));
    }

    // Sequences the bash differential test found worth pinning (old vs new hook, 400 transitions).
    #[test]
    fn regression_sequences() {
        // edit-src done reviewed reviewed edit-scratch → done
        assert_eq!(run(StatusState::default(),
            &[edit("/w/src/a.go"), Event::Done, Event::Reviewed, Event::Reviewed, edit("/w/pr-notes.md")]),
            st(Done, None));
        // sync done sessionend pr tool edit-scratch → working with pr stashed
        assert_eq!(run(StatusState::default(),
            &[Event::Sync, Event::Done, Event::SessionEnd, Event::Pr, Event::Tool, edit("/w/pr-notes.md")]),
            st(Working, Pr));
        // stop sessionend edit-scratch → stopped
        assert_eq!(run(StatusState::default(), &[Event::Stop, Event::SessionEnd, edit("/w/pr-notes.md")]),
            st(Stopped, None));
    }
}
