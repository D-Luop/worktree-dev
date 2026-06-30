#!/usr/bin/env bash
# Register and bare-clone a repo for the worktree-dev workflow.
# Usage: add-repo <slug> <git-url>
#   slug : short name used by `agent <slug> <name>` and `tokens`
# Creates a clean bare repo at ~/dev/repos/<slug>/.bare with origin/* tracking refs
# (no local heads — branches are created on demand by `agent`).
set -euo pipefail

WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # ~/dev/.wtd
DEV="$(dirname "$WTD")"                                                   # ~/dev
REG="$WTD/repos.tsv"

slug="${1:-}"
url="${2:-}"
if [ -z "$slug" ] || [ -z "$url" ]; then
  echo "usage: add-repo <slug> <git-url>"
  exit 1
fi
case "$slug" in *[!a-zA-Z0-9_-]*)
  echo "error: slug must be [a-zA-Z0-9_-] (got '$slug')"; exit 1 ;;
esac

bare="$DEV/repos/$slug/.bare"

# --- register (idempotent) ---
touch "$REG"
if awk -F'\t' -v s="$slug" '!/^#/ && $1==s{f=1} END{exit !f}' "$REG"; then
  echo "slug '$slug' already registered"
else
  [ -s "$REG" ] && [ -n "$(tail -c1 "$REG")" ] && printf '\n' >> "$REG"
  printf '%s\t%s\n' "$slug" "$url" >> "$REG"
  echo "registered: $slug -> $url"
fi

# --- clone (clean bare with remote-tracking layout) ---
if [ -d "$bare" ]; then
  echo "bare already exists at $bare; skipping clone"
else
  mkdir -p "$(dirname "$bare")"
  git init --bare "$bare" >/dev/null
  git -c safe.bareRepository=all -C "$bare" remote add origin "$url"
  git -c safe.bareRepository=all -C "$bare" config remote.origin.fetch '+refs/heads/*:refs/remotes/origin/*'
  echo "fetching $url ..."
  git -c safe.bareRepository=all -C "$bare" fetch --prune origin
  git -c safe.bareRepository=all -C "$bare" remote set-head origin -a >/dev/null 2>&1 || true
  echo "cloned -> $bare"
fi

# --- seed exclude so seeded CLAUDE.md is never committed ---
exclude="$bare/info/exclude"
if [ -f "$exclude" ] && ! grep -qxF 'CLAUDE.md' "$exclude"; then
  [ -s "$exclude" ] && [ -n "$(tail -c1 "$exclude")" ] && printf '\n' >> "$exclude"
  printf '%s\n' 'CLAUDE.md' >> "$exclude"
fi

# --- commit-msg hook: strip Claude/AI attribution from all commits (shared by all worktrees) ---
# Only install if there isn't already a real (non-symlink) commit-msg hook to respect.
if [ ! -e "$bare/hooks/commit-msg" ] || [ -L "$bare/hooks/commit-msg" ]; then
  mkdir -p "$bare/hooks"
  ln -sf "$WTD/hooks/strip-claude-attribution.sh" "$bare/hooks/commit-msg"
  echo "installed commit-msg attribution stripper"
else
  echo "note: existing commit-msg hook left intact; add attribution-stripping there manually if needed"
fi

dflt="$(git -c safe.bareRepository=all -C "$bare" symbolic-ref --quiet refs/remotes/origin/HEAD 2>/dev/null | sed 's@^refs/remotes/origin/@@')"
echo "done: '$slug' ready (default branch: ${dflt:-unknown}).  launch with:  agent $slug <name>"
