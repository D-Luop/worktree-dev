#!/usr/bin/env bash
# Review the CURRENT worktree's changes (infers <slug> <name> from cwd) — a cwd-aware wrapper around
# `review <slug> <name>`. Run it directly from a worktree pane (e.g. the top command pane) or via the
# /wt-review skill. Pass-through flags, e.g. `wt-review --main`.
set -euo pipefail
WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # ~/dev/.wtd
DEV="$(dirname "$WTD")"                                                   # ~/dev
root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
case "${root:-}/" in
  "$DEV/worktrees/"*/*) rel="${root#"$DEV/worktrees/"}"; slug="${rel%%/*}"; name="${rel#*/}" ;;
  *) echo "wt-review: run from inside a worktree (cwd is not under $DEV/worktrees/<slug>/)."; exit 1 ;;
esac
exec "$WTD/scripts/review.sh" "$slug" "$name" "$@"
