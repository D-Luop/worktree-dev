#!/usr/bin/env bash
# Manage extra Claude Code accounts for `agent --account <name>`.
#
# Each account is a separate CLAUDE_CONFIG_DIR with its own login/credentials, so a session started
# with `agent <slug> <name> --account <acct>` runs entirely under that login and bills ALL of its
# usage/cost to that account. The DEFAULT account is the normal ~/.claude (used when no --account is
# given). Named accounts live at ~/.claude-accounts/<name>/.
#
#   account ls                 list accounts + the email each is logged into
#   account add <name>         create the account, seed settings, open a login session (run /login)
#   account login <name>       re-open a login session for an existing account
#   account rm <name>          delete an account dir (its login + history); confirms
#
# One-time per account: `account add <name>` opens Claude with that config dir — run `/login`, pick
# the account, then exit. After that: `agent mod some-branch --account <name>`.
set -euo pipefail
WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"
# shellcheck source=account-lib.sh
. "$WTD/scripts/account-lib.sh"          # ACCROOT, ROLES, account_dir/name_for_role/name
DEFAULT="$HOME/.claude"

# the .claude.json (holds oauthAccount) for an account dir. The DEFAULT account keeps it at
# ~/.claude.json (HOME), NOT inside ~/.claude/; named accounts keep it inside their config dir.
cfgjson() { [ "$1" = "$DEFAULT" ] && echo "$HOME/.claude.json" || echo "$1/.claude.json"; }
emailof() { jq -r '.oauthAccount.emailAddress // "(not logged in)"' "$(cfgjson "$1")" 2>/dev/null || echo "(not logged in)"; }
seed()    { [ -f "$DEFAULT/settings.json" ] && cp "$DEFAULT/settings.json" "$1/settings.json" || true; }  # share hooks/statusline/attribution (absolute paths)

cmd="${1:-ls}"; shift || true
case "$cmd" in
  ls|list)
    printf '%-16s %-14s %s\n' NAME LOCATION EMAIL
    printf '%-16s %-14s %s\n' default '~/.claude' "$(emailof "$DEFAULT")"
    for d in "$ACCROOT"/*/; do
      [ -d "$d" ] || continue
      printf '%-16s %-14s %s\n' "$(basename "$d")" "~/.claude-accounts" "$(emailof "$d")"
    done
    if [ -s "$ROLES" ]; then
      echo; echo "roles (which account each use runs under; unset = default):"
      sed 's/^/  /; s/=/ → /' "$ROLES"
    fi
    ;;
  use)
    role="${1:-}"; aname="${2:-}"
    if [ -z "$role" ]; then
      echo "roles (unset = default ~/.claude):"; [ -s "$ROLES" ] && sed 's/^/  /; s/=/ → /' "$ROLES" || echo "  (none)"; exit 0
    fi
    if [ -z "$aname" ]; then cur="$(account_name_for_role "$role")"; echo "$role → ${cur:-default}"; exit 0; fi
    mkdir -p "$ACCROOT"; touch "$ROLES"
    tmp="$(mktemp)"; grep -vE "^${role}=" "$ROLES" > "$tmp" 2>/dev/null || true
    if [ "$aname" = default ]; then
      mv "$tmp" "$ROLES"; echo "$role → default"
    else
      [ -d "$ACCROOT/$aname" ] || { rm -f "$tmp"; echo "no account '$aname' — create with: account add $aname"; exit 1; }
      printf '%s=%s\n' "$role" "$aname" >> "$tmp"; mv "$tmp" "$ROLES"; echo "$role → $aname"
    fi
    ;;
  add)
    name="${1:?usage: account add <name>}"; dir="$ACCROOT/$name"
    [ -e "$dir" ] && { echo "account '$name' already exists ($dir). Log in with: account login $name"; exit 1; }
    command -v claude >/dev/null 2>&1 || { echo "error: 'claude' not on PATH"; exit 1; }
    mkdir -p "$dir"; seed "$dir"
    echo "created $dir"
    echo "opening Claude under this account — run /login, choose the account, then exit (Ctrl-D)."
    exec env CLAUDE_CONFIG_DIR="$dir" claude
    ;;
  login)
    name="${1:?usage: account login <name>}"; dir="$ACCROOT/$name"
    [ -d "$dir" ] || { echo "no account '$name' ($dir). Create it with: account add $name"; exit 1; }
    command -v claude >/dev/null 2>&1 || { echo "error: 'claude' not on PATH"; exit 1; }
    exec env CLAUDE_CONFIG_DIR="$dir" claude
    ;;
  rm|remove)
    name="${1:?usage: account rm <name>}"; dir="$ACCROOT/$name"
    [ -d "$dir" ] || { echo "no account '$name' ($dir)"; exit 1; }
    printf "remove account '%s' (%s), its login + history? [y/N] " "$name" "$dir"
    read -r a </dev/tty || a=""
    case "$a" in y|Y|yes|YES) rm -rf "$dir"; echo "removed '$name'";; *) echo "kept";; esac
    ;;
  usage)
    name="${1:-default}"
    [ "$name" = default ] && dir="$DEFAULT" || dir="$ACCROOT/$name"
    [ -d "$dir" ] || { echo "no account '$name'"; exit 1; }
    tok="$(jq -r '.claudeAiOauth.accessToken // empty' "$dir/.credentials.json" 2>/dev/null)"
    [ -n "$tok" ] || { echo "'$name' not logged in. Run: account login $name"; exit 1; }
    echo "usage — $name ($(emailof "$dir")):"
    body="$(curl -s -m 15 -w $'\n%{http_code}' -H "Authorization: Bearer $tok" \
            -H "anthropic-beta: oauth-2025-04-20" https://api.anthropic.com/api/oauth/usage 2>/dev/null)"
    code="${body##*$'\n'}"; json="${body%$'\n'*}"
    case "$code" in
      200) printf '%s' "$json" | jq -r '"  5h        \(.five_hour.utilization)%   resets \(.five_hour.resets_at)",
               "  7d        \(.seven_day.utilization)%   resets \(.seven_day.resets_at)",
               (if .seven_day_sonnet then "  7d sonnet \(.seven_day_sonnet.utilization)%" else empty end)' 2>/dev/null || echo "  (unexpected response)";;
      429)     echo "  rate limited (HTTP 429) — try again shortly";;
      401|403) echo "  auth failed (HTTP $code) — re-login: account login $name";;
      *)       echo "  fetch failed (HTTP ${code:-?})";;
    esac
    ;;
  *)
    echo "usage: account ls | add <name> | login <name> | rm <name> | use <role> <name|default> | usage [name]"
    echo "  per-session: agent <slug> <name> --account <name>"
    echo "  per-role:    account use dev <name>   /   account use review <name>"
    exit 1
    ;;
esac
