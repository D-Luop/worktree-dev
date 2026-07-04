#!/usr/bin/env bash
# Register and bare-clone (or freshly create) a repo for the worktree-dev workflow.
# Usage: add-repo <slug> <git-url>     clone an EXISTING remote into a bare repo
#        add-repo --new <slug>         create a BRAND-NEW empty LOCAL repo (no remote) for a new app
#   slug : short name used by `agent <slug> <name>` and `tokens`
# Clone mode  : bare repo at ~/dev/repos/<slug>/.bare with origin/* tracking refs (no local heads —
#               branches are created on demand by `agent`).
# --new mode  : empty bare repo with a single `main` branch (one empty root commit) and NO origin, so
#               `agent <slug> <name>` can branch worktrees off it immediately. Add a remote later
#               (git remote add origin … && git push -u origin main) to graduate it to GitHub.
set -euo pipefail

WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # ~/dev/.wtd
DEV="$(dirname "$WTD")"                                                   # ~/dev
REG="$WTD/repos.tsv"
. "$WTD/scripts/platform-lib.sh"   # lets git use our bare repos under safe.bareRepository=explicit

is_new=0
if [ "${1:-}" = "--new" ] || [ "${1:-}" = "-n" ]; then is_new=1; shift; fi

slug="${1:-}"
url="${2:-}"
if [ "$is_new" = 1 ]; then
  [ -n "$slug" ] || { echo "usage: add-repo --new <slug>"; exit 1; }
  url="(local)"                                     # placeholder — no remote yet
elif [ -z "$slug" ] || [ -z "$url" ]; then
  echo "usage: add-repo <slug> <git-url>        (clone an existing remote)"
  echo "       add-repo --new <slug>            (create a brand-new empty local repo)"
  exit 1
fi
case "$slug" in *[!a-zA-Z0-9_-]*)
  echo "error: slug must be [a-zA-Z0-9_-] (got '$slug')"; exit 1 ;;
esac
case "$slug" in plan)
  echo "error: 'plan' is a reserved slug (repo-less planning agents — 'agent plan <name>')"; exit 1 ;;
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

# --- create the bare repo (fresh-local for --new, else clone the remote) ---
if [ -d "$bare" ]; then
  echo "bare already exists at $bare; skipping"
elif [ "$is_new" = 1 ]; then
  # brand-new empty local repo: init bare, seed ONE empty root commit on `main`, no remote. This gives
  # `agent` a commit to branch worktrees off immediately (git worktree add needs a commit-ish).
  mkdir -p "$(dirname "$bare")"
  git init --bare "$bare" >/dev/null
  empty="$(git -c safe.bareRepository=all -C "$bare" mktree </dev/null)"
  root="$(git -c safe.bareRepository=all -c user.name='worktree-dev' -c user.email='wtd@localhost' \
            -C "$bare" commit-tree "$empty" -m 'chore: initial commit')"
  git -c safe.bareRepository=all -C "$bare" update-ref refs/heads/main "$root"
  git -c safe.bareRepository=all -C "$bare" symbolic-ref HEAD refs/heads/main
  echo "created new local repo -> $bare (branch: main, no remote yet)"
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

if [ "$is_new" = 1 ]; then
  dflt="$(git -c safe.bareRepository=all -C "$bare" symbolic-ref --quiet HEAD 2>/dev/null | sed 's@^refs/heads/@@')"
else
  dflt="$(git -c safe.bareRepository=all -C "$bare" symbolic-ref --quiet refs/remotes/origin/HEAD 2>/dev/null | sed 's@^refs/remotes/origin/@@')"
fi
echo "done: '$slug' ready (default branch: ${dflt:-unknown}).  launch with:  agent $slug <name>"
