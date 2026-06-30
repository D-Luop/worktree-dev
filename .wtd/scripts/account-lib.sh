#!/usr/bin/env bash
# Shared multi-account routing helpers. Sourced by account.sh, agent.sh, review.sh.
# An account = a separate CLAUDE_CONFIG_DIR (its own login). The DEFAULT account is ~/.claude.
# Named accounts live at ~/.claude-accounts/<name>/. Roles map a USE (dev / review) to an account
# so e.g. all reviews bill to one account and dev sessions to another, without per-command flags.
ACCROOT="${ACCROOT:-$HOME/.claude-accounts}"
ROLES="$ACCROOT/roles.conf"            # lines: <role>=<account-name>   e.g.  review=work / dev=personal

# CLAUDE_CONFIG_DIR for a role's configured account, or nothing (→ default ~/.claude).
# NOTE: every function returns 0 (the trailing `return 0`) so callers under `set -e` aren't aborted
# by `var=$(account_...)` when the lookup finds nothing — the caller checks for an empty result.
account_dir_for_role() {
  local name; name="$(awk -F= -v r="$1" '$1==r{print $2; exit}' "$ROLES" 2>/dev/null)"
  [ -n "$name" ] && [ -d "$ACCROOT/$name" ] && printf '%s\n' "$ACCROOT/$name"
  return 0
}
# the configured account NAME for a role (or nothing)
account_name_for_role() { awk -F= -v r="$1" '$1==r{print $2; exit}' "$ROLES" 2>/dev/null; return 0; }
# CLAUDE_CONFIG_DIR for an explicit account name — prints it if the account exists, else nothing
account_dir_for_name() { [ -n "${1:-}" ] && [ -d "$ACCROOT/$1" ] && printf '%s\n' "$ACCROOT/$1"; return 0; }
