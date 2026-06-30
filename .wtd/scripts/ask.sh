#!/usr/bin/env bash
# ask <slug>[@<branch>] [question...]
#
# Spin up a per-repo EXPERT agent: an accurate, READ-ONLY Q&A agent grounded in the repo's actual
# code/docs (it reads them, cites file:line, and won't guess — see .wtd/expert/.claude/agents/expert.md).
# It reads the read-only reference checkout at refs/<slug>/<branch> (the repo's default branch unless
# you pass @branch). With a question on the command line → one-shot answer; with no question → an
# interactive expert session you can keep asking in.
#
#   ask <slug> "how does contract status-change validation work end to end?"
#   ask <slug>@feat/contract-change-status "what does this branch change?"
#   ask <slug>                    # interactive expert session for a repo
set -euo pipefail

WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # ~/dev/.wtd
DEV="$(dirname "$WTD")"                                                   # ~/dev
REG="$WTD/repos.tsv"
# shellcheck source=refs-lib.sh
. "$WTD/scripts/refs-lib.sh"   # registered, default_branch, parse_ref_token, ensure_ref
EXPERTDIR="$WTD/expert"

token="${1:-}"
if [ -z "$token" ]; then
  echo "usage: ask <slug>[@<branch>] [question]    (per-repo expert; omit the question for interactive)"
  echo "registered repos:"; awk -F'\t' '!/^#/ && NF{print "  "$1}' "$REG" 2>/dev/null
  exit 1
fi
shift || true

parse_ref_token "$token"
registered "$REF_SLUG" || { echo "error: repo slug '$REF_SLUG' is not registered (see: make repos)."; exit 1; }

echo "preparing read-only checkout of $REF_SLUG@$REF_BRANCH …" >&2
ref="$(ensure_ref "$REF_SLUG" "$REF_BRANCH")" || { echo "error: could not check out $REF_SLUG@$REF_BRANCH."; exit 1; }

base="You are the expert on the '$REF_SLUG' repository, checked out READ-ONLY at: $ref (branch $REF_BRANCH). Answer strictly from that checkout's real code and docs — read them first, cite file:line, verify before you finalize, and never guess. If you can't confirm something, say so."

cd "$EXPERTDIR"
if [ "$#" -gt 0 ]; then
  prompt="$base"$'\n\n'"Question: $*"
  exec claude -p "$prompt" --agent expert --model opus --add-dir "$ref" \
    --permission-mode acceptEdits --allowedTools Read Grep Glob Bash \
    --disallowedTools "Write(**)" "Edit(**)"
else
  exec claude "$base"$'\n\nAsk me anything about this repo — I\'ll ground every answer in it.' \
    --agent expert --model opus --add-dir "$ref" \
    --allowedTools Read Grep Glob Bash \
    --disallowedTools "Write(**)" "Edit(**)"
fi
