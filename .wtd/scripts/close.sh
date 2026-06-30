#!/usr/bin/env bash
# Close the CURRENT tmux session (end all its panes), keeping the worktree on disk. Run it directly
# from any pane of the session — e.g. the small command pane at the top-left, or via the /close skill.
# Marks the worktree 'stopped' (red) unless it's 'done'. Reopen later with `agent <slug> <name>`.
set -euo pipefail
WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # ~/dev/.wtd
cur="$(tmux display-message -p '#S' 2>/dev/null || true)"
if [ -z "${TMUX:-}" ] || [ -z "$cur" ]; then
  echo "close: not inside a tmux session (run it from a worktree session pane)."; exit 1
fi
root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
[ -n "$root" ] && CLAUDE_PROJECT_DIR="$root" "$WTD/hooks/wt-status.sh" sessionend </dev/null 2>/dev/null || true
echo "closing session '$cur' (worktree kept on disk)"
tmux kill-session -t "$cur"
