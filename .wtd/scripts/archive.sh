#!/usr/bin/env bash
# Archive a worktree: move it out of the active rotation into the slug's archive/ folder,
# ~/dev/worktrees/<slug>/archive/<name>,
# keeping it a fully valid git worktree (via `git worktree move`, which updates the bare's tracking).
# Its tmux session is killed first; the branch, commits, uncommitted changes, and .claude/reviews all
# move with it. Archived worktrees drop out of `agent ls` / `tokens` / `review` (they live under
# archive/, not worktrees/). Reopen later by moving it back (or `git worktree move`).
#
# Usage:  archive <repo-slug> <name>
set -euo pipefail

WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # ~/dev/.wtd
DEV="$(dirname "$WTD")"                                                   # ~/dev
REG="$WTD/repos.tsv"
# shellcheck source=refs-lib.sh
. "$WTD/scripts/refs-lib.sh"   # registered

slug="${1:-}"; name="${2:-}"
if [ -z "$slug" ] || [ -z "$name" ]; then
  echo "usage: archive <repo-slug> <name>   (move a worktree into worktrees/<slug>/archive/, keep everything)"
  exit 1
fi
registered "$slug" || { echo "error: repo slug '$slug' is not registered."; exit 1; }

bare="$DEV/repos/$slug/.bare"
[ -d "$bare" ] || { echo "error: repo '$slug' not cloned ($bare missing)"; exit 1; }

# Resolve the worktree. Accept the full name (feat/data-set-testing) OR a bare leaf
# (data-set-testing) — searching one level of namespace (feat/fix/module/…) if needed.
wt="$DEV/worktrees/$slug/$name"
if [ ! -d "$wt" ]; then
  found=(); for m in "$DEV"/worktrees/"$slug"/*/"$name"; do
    case "$m" in "$DEV"/worktrees/"$slug"/archive/*) continue;; esac   # skip already-archived
    [ -d "$m" ] && found+=("$m")
  done
  if [ "${#found[@]}" -eq 1 ]; then
    wt="${found[0]}"; name="${wt#"$DEV/worktrees/$slug/"}"
  elif [ "${#found[@]}" -gt 1 ]; then
    echo "error: '$name' is ambiguous in '$slug':"; printf '   %s\n' "${found[@]#"$DEV/worktrees/$slug/"}"
    echo "       pass the full name, e.g. archive $slug ${found[0]#"$DEV/worktrees/$slug/"}"; exit 1
  fi
fi
[ -d "$wt" ] || { echo "error: no worktree '$name' in '$slug' (looked in $DEV/worktrees/$slug/)"; exit 1; }

arc="$DEV/worktrees/$slug/archive/$name"
session="${slug}-${name}"; session="${session//[.:]/-}"
[ -e "$arc" ] && { echo "error: archive target already exists: $arc"; exit 1; }

# Kill the session so nothing holds the worktree open during the move.
tmux kill-session -t "$session" 2>/dev/null && echo "killed session $session" || true

mkdir -p "$(dirname "$arc")"
if git -c safe.bareRepository=all -C "$bare" worktree move "$wt" "$arc"; then
  echo "archived: $wt  →  $arc"
  echo "  (branch + changes + reviews kept; it's out of the active rotation. To bring it back:"
  echo "   git -c safe.bareRepository=all -C $bare worktree move '$arc' '$wt')"
else
  echo "error: 'git worktree move' failed (worktree locked, or a process is using it?)."
  echo "  close anything in $wt and retry; or 'git -c safe.bareRepository=all -C $bare worktree move --force ...'."
  exit 1
fi
