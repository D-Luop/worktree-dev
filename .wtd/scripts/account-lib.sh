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

# --- capacity-aware selection (for `account switch`) ------------------------------------------
# An account NAME of "default" maps to ~/.claude; anything else to ~/.claude-accounts/<name>.
account_dir_of() { [ "${1:-}" = default ] && printf '%s\n' "$HOME/.claude" || account_dir_for_name "$1"; return 0; }
# the .claude.json (holds oauthAccount) for an account dir: default keeps it at ~/.claude.json (HOME).
account_cfgjson() { [ "$1" = "$HOME/.claude" ] && printf '%s\n' "$HOME/.claude.json" || printf '%s\n' "$1/.claude.json"; }
account_email_of() { jq -r '.oauthAccount.emailAddress // ""' "$(account_cfgjson "$1")" 2>/dev/null || printf ''; return 0; }
account_token_of() { jq -r '.claudeAiOauth.accessToken // ""' "$1/.credentials.json" 2>/dev/null || printf ''; return 0; }

# every account NAME, default first (one per line).
account_names() {
  printf 'default\n'
  local d
  for d in "$ACCROOT"/*/; do [ -d "$d" ] && printf '%s\n' "$(basename "$d")"; done
  return 0
}

# account_util <dir> → prints "<max-utilization-int>" (max of 5h/7d %) for a logged-in account, or
# nothing if not logged in / the fetch fails. Higher = less capacity remaining. The usage endpoint
# itself rate-limits rapid polls (429) — that is NOT "out of capacity", so retry a couple of times
# before giving up (the extension avoids this path by passing a pre-chosen --to target).
account_util() {
  local dir="$1" tok body code json u5 u7 try
  tok="$(account_token_of "$dir")"; [ -n "$tok" ] || return 0
  for try in 1 2 3; do
    body="$(curl -s -m 15 -w $'\n%{http_code}' -H "Authorization: Bearer $tok" \
            -H "anthropic-beta: oauth-2025-04-20" </dev/null https://api.anthropic.com/api/oauth/usage 2>/dev/null)" || return 0
    code="${body##*$'\n'}"; json="${body%$'\n'*}"
    [ "$code" = 429 ] || break
    sleep 2
  done
  [ "$code" = 200 ] || return 0
  u5="$(printf '%s' "$json" | jq -r '.five_hour.utilization // 0'  2>/dev/null)"; u5="${u5%.*}"
  u7="$(printf '%s' "$json" | jq -r '.seven_day.utilization // 0' 2>/dev/null)"; u7="${u7%.*}"
  [ -n "$u5" ] || u5=0; [ -n "$u7" ] || u7=0
  [ "$u5" -ge "$u7" ] 2>/dev/null && printf '%s\n' "$u5" || printf '%s\n' "$u7"
  return 0
}

# account_best_other <current-name> → the logged-in account (name) with the MOST remaining capacity,
# excluding <current-name> and any that are fully maxed (util >= 100). Prints nothing if none qualify.
account_best_other() {
  local cur="${1:-}" name dir util best="" bestutil=101
  while IFS= read -r name; do
    [ -n "$name" ] || continue
    [ "$name" = "$cur" ] && continue
    dir="$(account_dir_of "$name")"; [ -n "$dir" ] && [ -d "$dir" ] || continue
    util="$(account_util "$dir")"; [ -n "$util" ] || continue     # skip not-logged-in / fetch failures
    [ "$util" -ge 100 ] 2>/dev/null && continue                   # skip fully-maxed
    if [ "$util" -lt "$bestutil" ] 2>/dev/null; then bestutil="$util"; best="$name"; fi
  done < <(account_names)
  [ -n "$best" ] && printf '%s\n' "$best"
  return 0
}
