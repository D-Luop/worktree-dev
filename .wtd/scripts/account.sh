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
# the account, then exit. After that: `agent <slug> some-branch --account <name>`.
set -euo pipefail
WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"
DEV="$(dirname "$WTD")"
# shellcheck source=account-lib.sh
. "$WTD/scripts/account-lib.sh"          # ACCROOT, ROLES, account_dir/name_for_role/name, best_other
# shellcheck source=platform-lib.sh
. "$WTD/scripts/platform-lib.sh"         # wtd_state_dir (for the switch subcommand's session lookups)
# shellcheck source=session-lib.sh
. "$WTD/scripts/session-lib.sh"          # wtd_session_account_*, wtd_session_idfile, wtd_claude_transcript
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
  switch)
    # account switch <slug> <name>  — move a worktree session to the logged-in account with the MOST
    # remaining capacity. Copies the session transcript into the target account's store so reopening
    # resumes the SAME conversation there, and records a durable per-session binding. Prints the target
    # account NAME on stdout (the extension reads it, then compacts + relaunches the session). This does
    # NOT itself kill/relaunch the live session.
    slug="${1:-}"; wtname="${2:-}"; shift 2 2>/dev/null || true
    to=""
    while [ "$#" -gt 0 ]; do case "$1" in --to) to="${2:-}"; shift 2;; --to=*) to="${1#*=}"; shift;; *) shift;; esac; done
    [ -n "$slug" ] && [ -n "$wtname" ] || { echo "usage: account switch <slug> <name> [--to <account>]" >&2; exit 1; }
    session="${slug}-${wtname}"; session="${session//[.:]/-}"
    wt="$DEV/worktrees/$slug/$wtname"
    [ -d "$wt" ] || { echo "error: no worktree at $wt" >&2; exit 1; }
    cur="$(wtd_session_account_get "$session")"; [ -n "$cur" ] || cur=default
    curdir="$(account_dir_of "$cur")"
    if [ -n "$to" ]; then            # extension pre-picked the target from its cached usage numbers
      [ "$to" != "$cur" ] || { echo "error: session is already on account '$to'" >&2; exit 1; }
      tgtdir="$(account_dir_of "$to")"
      { [ -n "$tgtdir" ] && [ -d "$tgtdir" ] && [ -n "$(account_token_of "$tgtdir")" ]; } \
        || { echo "error: target account '$to' is not a logged-in account" >&2; exit 1; }
      tgt="$to"
    else
      echo "checking account capacity…" >&2
      tgt="$(account_best_other "$cur")"
      [ -n "$tgt" ] || { echo "error: no other logged-in account with remaining capacity (add one: account add <name>, or log in: account login <name>)" >&2; exit 1; }
      tgtdir="$(account_dir_of "$tgt")"
    fi
    id="$(cat "$(wtd_session_idfile "$session")" 2>/dev/null || true)"
    if [ -n "$id" ]; then
      src="$(wtd_claude_transcript "$wt" "$id" "$curdir")"
      dst="$(wtd_claude_transcript "$wt" "$id" "$tgtdir")"
      if [ -f "$src" ]; then
        mkdir -p "$(dirname "$dst")"; cp -f "$src" "$dst"
        echo "copied conversation transcript into '$tgt' (resume keeps the same chat)" >&2
      else
        echo "note: no transcript yet for this session — the target starts fresh" >&2
      fi
    fi
    wtd_session_account_set "$session" "$tgt"
    echo "bound session '$session' → account '$tgt' ($(account_email_of "$tgtdir"))" >&2
    printf '%s\n' "$tgt"
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
    echo "       account switch <slug> <name>    (move a session to the account with the most capacity)"
    echo "  per-session: agent <slug> <name> --account <name>"
    echo "  per-role:    account use dev <name>   /   account use review <name>"
    exit 1
    ;;
esac
