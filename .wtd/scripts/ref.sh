#!/usr/bin/env bash
# Manage shared read-only reference checkouts at ~/dev/refs/<slug>/<branch>.
#
# These are GLOBAL context: every worktree can READ any loaded ref (access is granted once,
# fleet-wide, in ~/.claude/settings.json by install.sh; they are never editable). Bulk-load the
# refs you care about, then point an agent at a specific path in-session — or use
# `agent <slug> <name> <ref-token>...` to also announce specific refs in that worktree's CLAUDE.md.
#
# Usage:
#   ref add   <slug>[@<branch>] ...    create/refresh checkout(s)  (bare slug = default branch)
#   ref rm    <slug>[@<branch>] ...    remove checkout(s)
#   ref sync  [<slug>[@<branch>] ...]  re-fetch + re-checkout tip(s); no args = every loaded ref
#   ref ls                             list loaded refs (slug@branch, path, short sha)
set -euo pipefail

WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # ~/dev/.wtd
DEV="$(dirname "$WTD")"                                                   # ~/dev
. "$WTD/scripts/platform-lib.sh"   # lets git use our bare repos under safe.bareRepository=explicit
# shellcheck source=refs-lib.sh
. "$WTD/scripts/refs-lib.sh"

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit "${1:-0}"; }

# Resolve & validate a ref token -> sets REF_SLUG / REF_BRANCH, or returns 1 with a warning.
resolve() {
  parse_ref_token "$1"
  if ! registered "$REF_SLUG" || [ ! -d "$DEV/repos/$REF_SLUG/.bare" ]; then
    echo "warning: skipping '$1' (repo '$REF_SLUG' not registered/cloned)" >&2
    return 1
  fi
}

cmd_add() {
  [ "$#" -ge 1 ] || { echo "usage: ref add <slug>[@<branch>] ..." >&2; exit 1; }
  for token in "$@"; do
    resolve "$token" || continue
    if p="$(ensure_ref "$REF_SLUG" "$REF_BRANCH")"; then
      printf 'added   %-24s %s\n' "$REF_SLUG@$REF_BRANCH" "$p"
    fi
  done
}

cmd_rm() {
  [ "$#" -ge 1 ] || { echo "usage: ref rm <slug>[@<branch>] ..." >&2; exit 1; }
  for token in "$@"; do
    parse_ref_token "$token"   # don't require still-registered to allow cleanup
    if remove_ref "$REF_SLUG" "$REF_BRANCH"; then
      printf 'removed %s\n' "$REF_SLUG@$REF_BRANCH"
    else
      printf 'absent  %s (nothing loaded)\n' "$REF_SLUG@$REF_BRANCH"
    fi
  done
}

cmd_sync() {
  if [ "$#" -ge 1 ]; then
    for token in "$@"; do
      resolve "$token" || continue
      if p="$(ensure_ref "$REF_SLUG" "$REF_BRANCH")"; then
        printf 'synced  %-24s %s\n' "$REF_SLUG@$REF_BRANCH" "$p"
      fi
    done
  else
    local any=0 label
    while IFS=$'\t' read -r label _path _sha; do
      any=1
      parse_ref_token "$label"
      if p="$(ensure_ref "$REF_SLUG" "$REF_BRANCH")"; then
        printf 'synced  %-24s %s\n' "$label" "$p"
      fi
    done < <(list_refs)
    [ "$any" = 1 ] || echo "no refs loaded (add one with: ref add <slug>[@<branch>])"
  fi
}

cmd_ls() {
  local out; out="$(list_refs)"
  if [ -z "$out" ]; then
    echo "no refs loaded (add one with: ref add <slug>[@<branch>])"
    return 0
  fi
  printf '%s\n' "$out" | while IFS=$'\t' read -r label path sha; do
    printf '%-24s %-12s %s\n' "$label" "$sha" "$path"
  done
}

sub="${1:-}"; [ "$#" -ge 1 ] && shift || true
case "$sub" in
  add)            cmd_add "$@" ;;
  rm|remove)      cmd_rm "$@" ;;
  sync|refresh)   cmd_sync "$@" ;;
  ls|list)        cmd_ls "$@" ;;
  ''|-h|--help)   usage 0 ;;
  *)              echo "error: unknown subcommand '$sub'" >&2; usage 1 ;;
esac
