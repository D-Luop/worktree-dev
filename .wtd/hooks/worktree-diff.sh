#!/usr/bin/env bash
# Print a worktree's FULL uncommitted diff for review BEFORE an agent commits:
#   - tracked changes vs HEAD (staged + unstaged), all file types
#   - untracked new files (shown as additions)
# Generated files are excluded; delta-rendered if available, else git color. Unlike the live diff
# pane (which, for mod, is filtered to .sql/.proto), this shows everything pending.
# Used by the commit pane's "view_uncommitted_diff" button via commit-diff-show.sh.
# Usage: worktree-diff.sh <worktree-dir> [width]
set -u

wt="${1:?usage: worktree-diff.sh <worktree-dir> [width] [base-ref]}"
width="${2:-80}"
base="${3:-HEAD}"   # diff base: HEAD = uncommitted only; a merge-base = whole branch vs that ref
here="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
# shellcheck source=generated-filter.sh
. "$here/generated-filter.sh"

# generated-file excludes, computed from the working tree (ref "-")
gx=()
while IFS= read -r p; do [ -n "$p" ] && gx+=("$p"); done < <(
  git -C "$wt" diff --name-only "$base" 2>/dev/null | gen_excludes "$wt" -
)
# plus test-file excludes when the panel's "exclude tests" toggle is on
while IFS= read -r p; do [ -n "$p" ] && gx+=("$p"); done < <(test_excludes)
# absolute prefixes so files stay ctrl+click-resolvable in the (cwd-less) diff pane
pfx=(--src-prefix="a/$wt/" --dst-prefix="b/$wt/")

emit() {  # $1 = color|raw
  local c=(); [ "$1" = color ] && c=(-c color.ui=always)
  git -C "$wt" "${c[@]}" diff --stat --patch "$base" "${pfx[@]}" -- . "${gx[@]}" 2>/dev/null
  # untracked new files, one diff each (read-only; never touches the index). Pass the path RELATIVE
  # to $wt (git -C sets cwd) so the b/$wt/ prefix yields the absolute path once, not doubled.
  git -C "$wt" ls-files --others --exclude-standard -z -- . "${gx[@]}" 2>/dev/null | while IFS= read -r -d '' f; do
    git -C "$wt" "${c[@]}" diff --no-index "${pfx[@]}" -- /dev/null "$f" 2>/dev/null
  done
}

if command -v delta >/dev/null 2>&1; then
  out="$(emit raw | delta --paging=never --width=$((width - 2)) \
      --plus-style 'syntax #166e22' --plus-emph-style 'syntax #1f9b34' \
      --minus-style 'syntax #5a1d1d' --minus-emph-style 'syntax #8a2e2e' \
      --file-style 'bold yellow' --file-decoration-style 'yellow ol' --line-numbers \
      | sed 's/^/  /')"
else
  out="$(emit color | sed 's/^/  /')"
fi

if [ -n "$out" ]; then printf '%s\n' "$out"; else printf '  (no changes)\n'; fi
