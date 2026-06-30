#!/usr/bin/env bash
# Close the CURRENT session, keeping the worktree on disk. Run it from inside the session — a tmux
# pane (Linux/WSL/mac) or the VSCode terminal (Windows) — or via the /close skill.
# Marks the worktree 'stopped' (red) unless it's 'done'. Reopen later with `agent <slug> <name>`.
set -euo pipefail
WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # ~/dev/.wtd
# shellcheck source=platform-lib.sh
. "$WTD/scripts/platform-lib.sh"
# shellcheck source=session-lib.sh
. "$WTD/scripts/session-lib.sh"
cur="$(wtd_session_current)"
if [ -z "$cur" ]; then
  echo "close: not inside a session (run it from a worktree session terminal/pane)."; exit 1
fi
root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
[ -n "$root" ] && CLAUDE_PROJECT_DIR="$root" "$WTD/hooks/wt-status.sh" sessionend </dev/null 2>/dev/null || true
echo "closing session '$cur' (worktree kept on disk)"
wtd_session_kill "$cur" "$root"
