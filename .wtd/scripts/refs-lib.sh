#!/usr/bin/env bash
# Shared helpers for read-only reference checkouts at ~/dev/refs/<slug>/<branch>.
# Sourced by agent.sh and ref.sh. Callers must export/set WTD (~/dev/.wtd) and DEV (~/dev).
#
# These checkouts are GLOBAL: every worktree can READ them, because install.sh grants
# read access (+ Edit/Write deny) on the whole ~/dev/refs tree once, in ~/.claude/settings.json.
# Per-worktree wiring is therefore NOT needed for access — `agent` only ANNOUNCES specific
# refs in a worktree's CLAUDE.md (awareness), it no longer grants access.

: "${WTD:?refs-lib.sh: WTD must be set}"
: "${DEV:?refs-lib.sh: DEV must be set}"
REG="$WTD/repos.tsv"
REFROOT="$DEV/refs"

# Is <slug> present in the repo registry?
registered() { awk -F'\t' -v s="$1" '!/^#/ && $1==s{f=1} END{exit !f}' "$REG" 2>/dev/null; }

# Default branch of a repo (origin/HEAD, falling back to main).
default_branch() {
  local b
  b="$(git -C "$DEV/repos/$1/.bare" symbolic-ref --quiet refs/remotes/origin/HEAD 2>/dev/null | sed 's@^refs/remotes/origin/@@')"
  printf '%s\n' "${b:-main}"
}

# Parse a ref token ("<slug>" or "<slug>@<branch>") into globals REF_SLUG / REF_BRANCH.
# A bare slug resolves to that repo's default branch.
parse_ref_token() {
  REF_SLUG="${1%%@*}"
  if [ "$1" = "$REF_SLUG" ]; then REF_BRANCH="$(default_branch "$REF_SLUG")"; else REF_BRANCH="${1#*@}"; fi
}

# Ensure a shared read-only checkout of <slug>@<branch> exists at ~/dev/refs/<slug>/<branch>,
# detached at that branch (origin/<branch> preferred, else local <branch>), refreshed on use.
# Echoes the abs path on success, or returns 1 (with a warning) if the branch can't be found.
ensure_ref() {
  local slug="$1" branch="$2" rbare="$DEV/repos/$1/.bare" rpath="$DEV/refs/$1/$2" ref=""
  git -c safe.bareRepository=all -C "$rbare" fetch --quiet origin || true
  if git -c safe.bareRepository=all -C "$rbare" rev-parse --verify --quiet "refs/remotes/origin/$branch" >/dev/null; then
    ref="origin/$branch"
  elif git -c safe.bareRepository=all -C "$rbare" rev-parse --verify --quiet "refs/heads/$branch" >/dev/null; then
    ref="$branch"
  else
    echo "warning: branch '$branch' not found in repo '$slug' (origin or local); skipping" >&2
    return 1
  fi
  if [ -d "$rpath" ]; then
    git -C "$rpath" checkout --quiet --detach --force "$ref" 2>/dev/null || true
  else
    mkdir -p "$(dirname "$rpath")"
    git -c safe.bareRepository=all -C "$rbare" worktree add --quiet --detach "$rpath" "$ref"
  fi
  printf '%s\n' "$rpath"
}

# Remove a reference checkout for <slug>@<branch> (worktree remove, falling back to rm+prune).
# Returns 0 if something was removed, 1 if nothing was there.
remove_ref() {
  local slug="$1" branch="$2" rbare="$DEV/repos/$1/.bare" rpath="$DEV/refs/$1/$2"
  [ -d "$rpath" ] || return 1
  git -c safe.bareRepository=all -C "$rbare" worktree remove --force "$rpath" 2>/dev/null || { rm -rf "$rpath"; git -c safe.bareRepository=all -C "$rbare" worktree prune 2>/dev/null || true; }
  # tidy now-empty <slug> parent dir
  rmdir "$DEV/refs/$slug" 2>/dev/null || true
  return 0
}

# List every loaded reference checkout under $REFROOT as TAB-separated:  <slug>@<branch>\t<path>\t<short-sha>
# Sourced from each registered bare's `git worktree list` so slashed branch names work.
list_refs() {
  local slug url bare path sha rest line
  while IFS=$'\t' read -r slug url; do
    case "$slug" in ''|'#'*) continue;; esac
    bare="$DEV/repos/$slug/.bare"
    [ -d "$bare" ] || continue
    while IFS= read -r line; do
      path="${line%% *}"
      case "$path/" in "$REFROOT"/*) ;; *) continue;; esac
      sha="$(printf '%s' "$line" | awk '{print $2}')"
      printf '%s@%s\t%s\t%s\n' "$slug" "${path#"$REFROOT/$slug/"}" "$path" "$sha"
    done < <(git -c safe.bareRepository=all -C "$bare" worktree list 2>/dev/null)
  done < "$REG"
}
