#!/usr/bin/env bash
# Launch (or resume) the worktree-dev fleet ASSISTANT — one durable Claude session that runs in the
# dev base and helps the user MANAGE worktrees (launch / stop / archive / review / tear down via the
# wtd PATH commands). Unlike `agent`, it has no worktree of its own: it orchestrates the others. The
# claude-status extension pins it at the top of the roster and runs `assistant` in a VSCode terminal.
#
# Usage: assistant [--account <name>] [--mode <permission-mode>]
#   --account <name> : run under a different Claude login (its own CLAUDE_CONFIG_DIR → bills there).
#                      Defaults to $ASSISTANT_ACCOUNT, else your normal ~/.claude login.
set -euo pipefail

WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # ~/dev/.wtd
DEV="$(dirname "$WTD")"                                                   # ~/dev (the fleet base)
# shellcheck source=account-lib.sh
. "$WTD/scripts/account-lib.sh"          # account_dir_for_name
# shellcheck source=platform-lib.sh
. "$WTD/scripts/platform-lib.sh"         # wtd_state_dir, GIT_CONFIG_PARAMETERS shim
# shellcheck source=session-lib.sh
. "$WTD/scripts/session-lib.sh"          # wtd_uuid, wtd_session_idfile, wtd_claude_transcript

account=""
pmode="${ASSISTANT_PERMISSION_MODE:-auto}"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --account|-a) [ "$#" -ge 2 ] || { echo "error: --account requires a <name>"; exit 1; }; account="$2"; shift 2;;
    --account=*)  account="${1#*=}"; shift;;
    --mode)       [ "$#" -ge 2 ] || { echo "error: --mode requires a value"; exit 1; }; pmode="$2"; shift 2;;
    *) shift;;
  esac
done

# Account routing: --account flag, else $ASSISTANT_ACCOUNT, else the default login (empty ccdir).
ccdir=""
if [ -n "$account" ]; then
  ccdir="$(account_dir_for_name "$account")"
  [ -n "$ccdir" ] || { echo "error: no Claude account '$account'  (create it with: account add $account)"; exit 1; }
elif [ -n "${ASSISTANT_ACCOUNT:-}" ]; then
  ccdir="$(account_dir_for_name "$ASSISTANT_ACCOUNT" || true)"
fi
[ -n "$ccdir" ] && export CLAUDE_CONFIG_DIR="$ccdir"

export WTD_SESSION="assistant"
cd "$DEV" || exit 1

if ! command -v claude >/dev/null 2>&1; then
  echo "note: 'claude' not on PATH; leaving a shell in $DEV"
  exec "${SHELL:-bash}" -i
fi

read -r -d '' SYS <<'EOF' || true
You are the worktree-dev fleet assistant. You run in the worktree-dev base directory and help the
user manage their parallel Claude Code worktrees — you do NOT write feature code inside individual
worktrees (each worktree has its own session for that); you orchestrate the fleet.

Use the worktree-dev PATH commands to do real work (don't just describe them):
- agent <slug> <name> [--from <ref>] [--account <a>]  — launch/open a worktree session
- agent ls | agent stop <slug> <name> | agent rm <slug> <name> [--branch] [--force]
- archive <slug> <name>  — shelve a worktree;  ref add/ls/rm  — read-only cross-repo context
- review <slug> <name> [--main] | wt-review  — the separate pre-push reviewer (only when asked)
- ask <slug>[@<branch>] [question]  — grounded per-repo Q&A
- account ls/usage | tokens | add-repo <slug> <git-url> | make repos

Inspect the fleet from worktrees/<slug>/<name> (each has a .claude-status sentinel) and `agent ls`.
Be concise and act directly. Never start a review on your own — only when the user explicitly asks.
EOF

# Durable session id keyed "assistant": resume the SAME conversation across reopen/reboot (the live
# process dies on a restart, the transcript doesn't). Kept out of any worktree so it's never git-dirty.
# NB: keep every step from tripping `set -e` (a failed cat/uuid must not abort the launch).
idf="$(wtd_session_idfile assistant)"; id=""
if [ -f "$idf" ]; then id="$(cat "$idf" 2>/dev/null || true)"; fi
if [ -n "$id" ]; then
  if [ -f "$(wtd_claude_transcript "$DEV" "$id")" ]; then
    exec claude --permission-mode "$pmode" --append-system-prompt "$SYS" --resume "$id"
  fi
  exec claude --permission-mode "$pmode" --append-system-prompt "$SYS" --session-id "$id"
fi
id="$(wtd_uuid || true)"
if [ -n "$id" ]; then
  mkdir -p "$(wtd_session_idsdir)"
  printf '%s\n' "$id" > "$idf"
  exec claude --permission-mode "$pmode" --append-system-prompt "$SYS" --session-id "$id"
fi
exec claude --permission-mode "$pmode" --append-system-prompt "$SYS"   # no uuid tool → plain session
