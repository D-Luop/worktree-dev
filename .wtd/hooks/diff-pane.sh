#!/usr/bin/env bash
# Live diff pane for a worktree: clear + redraw the colored diff log whenever it changes.
# Usage: diff-pane.sh <worktree-dir> <log-relative-path>
#
# A script (not an inline tmux command) so the commit-click helper can re-launch it: clicking a
# SHA respawns this pane to show that commit's diff, then `exec`s back here to resume the live view.
set -u

wt="${1:?usage: diff-pane.sh <worktree> <log-rel>}"
lg="$wt/${2:-.claude/diffs.log}"

last=
while :; do
  c="$(stat -c %y "$lg" 2>/dev/null || echo 0)"
  if [ "$c" != "$last" ]; then
    last="$c"
    printf '\033[H\033[2J\033[3J'        # home, clear screen + scrollback
    sed 's/^/  /' "$lg" 2>/dev/null      # 2-space left margin off the pane border
  fi
  sleep 0.4
done
