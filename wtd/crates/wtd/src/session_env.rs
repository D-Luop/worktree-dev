//! Strips a parent Claude Code session's identity from the environment of agents wtd launches, so
//! an agent started from inside a session (a delegator's `agent … --detach`) runs as a fresh,
//! top-level session. Inheriting `CLAUDE_CODE_CHILD_SESSION` turns transcript saving off, which
//! breaks `--resume` and account handoff; the messaging/IDE vars would route the child to the
//! parent's socket and editor.
//!
//! Deny-list, not a prefix wipe: account and user config (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
//! `CLAUDE_CODE_USE_*`, `ANTHROPIC_*`, …) must survive. Taken from Claude Code 2.1.309: the vars it
//! injects into tool subprocesses plus those its own detached-session spawn deletes. Keep
//! `.wtd/scripts/platform-lib.sh` (`WTD_SESSION_ENV_VARS`) in step.

pub const SESSION_VARS: &[&str] = &[
    // injected into every tool subprocess
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "AI_AGENT",
    "TRACEPARENT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_INVOKED_SKILLS",
    // removed by Claude's own detached-session spawn
    "CLAUDE_CODE_BRIDGE_SESSION_ID",
    "CLAUDE_CODE_CHROME_MCP_ORG_DENIED",
    "CLAUDE_CODE_EVAL_INTERVIEW_SESSION",
    "CLAUDE_CODE_HOST_WORKTREE",
    "CLAUDE_CODE_HOST_WORKTREE_FENCE",
    // per-session routing: peer messaging, IDE connection, launch surface
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_CODE_ENTRYPOINT",
];

pub fn is_session_var(name: &str) -> bool {
    SESSION_VARS.iter().any(|v| v.eq_ignore_ascii_case(name))
}

pub fn scrub(env: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
    env.into_iter().filter(|(k, _)| !is_session_var(k)).collect()
}

/// This process's environment, minus the parent session's identity.
pub fn current() -> Vec<(String, String)> {
    scrub(std::env::vars_os().map(|(k, v)| (k.to_string_lossy().into(), v.to_string_lossy().into())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(keys: &[&str]) -> Vec<(String, String)> {
        keys.iter().map(|k| (k.to_string(), "x".to_string())).collect()
    }

    #[test]
    fn strips_parent_session_identity() {
        let parent = env(&[
            "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_SESSION_ID", "CLAUDE_PID", "CLAUDE_EFFORT",
            "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDE_CODE_MESSAGING_TOKEN", "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ATTENDED", "CLAUDE_CODE_SSE_PORT", "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_EXECPATH",
        ]);
        assert!(scrub(parent).is_empty());
    }

    #[test]
    fn keeps_accounts_and_user_config() {
        let kept = env(&[
            "PATH", "CLAUDE_CONFIG_DIR", "CODEX_HOME", "CLAUDE_CODE_USE_BEDROCK", "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL", "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE", "WTD_WORKTREE",
        ]);
        assert_eq!(scrub(kept.clone()), kept);
    }

    #[test]
    fn matches_case_insensitively() {
        assert!(is_session_var("ClaudeCode"));
        assert!(is_session_var("claude_code_child_session"));
        assert!(!is_session_var("CLAUDE_CODE_CHILD_SESSIONS"));
    }
}
