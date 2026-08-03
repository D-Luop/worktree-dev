#!/usr/bin/env bash
# Re-seed every worktree's live diff log so a panel toggle (e.g. the exclude-tests switch) takes
# effect immediately, not only on the next edit. For each worktree whose repo has a diff hook, run it
# with --seed. Skips worktrees mid-review (status 'reviewing') so the review spinner/findings aren't
# clobbered. Best-effort and silent.
set -u
WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # ~/dev/.wtd
DEV="$(dirname "$WTD")"                                                   # ~/dev
shopt -s nullglob
for d in "$DEV"/worktrees/*/; do
  slug="$(basename "$d")"
  hookfrag="$WTD/repo-hooks/$slug.json"
  [ -f "$hookfrag" ] || continue
  diffcmd="$(sed "s#__DEV__#$DEV#g" "$hookfrag" | jq -r '[.. | objects | .command? // empty] | .[0] // empty' 2>/dev/null)"
  [ -n "$diffcmd" ] && [ -x "$diffcmd" ] || continue
  while IFS= read -r gitpath; do
    wt="$(dirname "$gitpath")"
    case "$wt" in */archive/*) continue;; esac                 # skip the archived namespace
    [ "$(cat "$wt/.claude-status" 2>/dev/null)" = reviewing ] && continue   # don't clobber a live review
    "$diffcmd" --seed "$wt" >/dev/null 2>&1 || true
  done < <(find "$d" -maxdepth 4 -name .git 2>/dev/null)
done
