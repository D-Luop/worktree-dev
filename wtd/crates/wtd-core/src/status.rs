//! A worktree's Claude status: the state machine formerly in `.wtd/hooks/wt-status.sh`.
//!
//! States: working (busy) · input (your turn) · reviewing · pr · done · stopped · none.
//! `pr` and `done` are sticky milestones that don't freeze live activity: a new turn shows `working`
//! while stashing the milestone, a read-only turn restores it, and a real source edit drops it.
//! `reviewing` is owned by the reviewer and survives agent turns.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    None,
    Working,
    Input,
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
            "stop" => Event::Stop,
            "sessionend" => Event::SessionEnd,
            _ => return None,
        })
    }
}

/// Persisted state: the visible status plus the stashed milestone (`.claude-status.resume`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StatusState {
    pub status: Status,
    pub stash: Status,
}

/// Edits to these don't count as "real source edits" (they don't invalidate a milestone).
fn is_scratch_path(path: &str) -> bool {
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
    let StatusState { status, stash } = cur;
    let none = Status::None;
    match ev {
        Event::Sync => cur,
        Event::Wip => StatusState { status: Status::Working, stash: none },
        Event::Edit { path } => {
            if is_scratch_path(path) {
                cur
            } else {
                StatusState { status: Status::Working, stash: none }
            }
        }
        Event::Reviewing if status.is_milestone() => cur,
        Event::Reviewing => StatusState { status: Status::Reviewing, stash },
        Event::Reviewed if status.is_milestone() => cur,
        Event::Reviewed => StatusState { status: Status::Input, stash },
        Event::Done => StatusState { status: Status::Done, stash: none },
        Event::Pr => StatusState { status: Status::Pr, stash: none },
        Event::Working | Event::Tool => match status {
            s if s.is_milestone() => StatusState { status: Status::Working, stash: s },
            Status::Reviewing => cur,
            _ => StatusState { status: Status::Working, stash },
        },
        Event::Stop | Event::SessionEnd => {
            if stash != none {
                return StatusState { status: stash, stash: none };
            }
            let keep = matches!(status, Status::Reviewing | Status::Done | Status::Pr);
            match (ev, keep) {
                (_, true) => cur,
                (Event::Stop, false) => StatusState { status: Status::Input, stash },
                _ => StatusState { status: Status::Stopped, stash },
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
        StatusState { status, stash }
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
