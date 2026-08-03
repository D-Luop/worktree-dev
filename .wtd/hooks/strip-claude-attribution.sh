#!/usr/bin/env bash
# git commit-msg hook: strip Claude / AI attribution from every commit message so commits
# are never tied to Claude. Deterministic and repo-side — independent of the model's
# behavior or Claude Code settings. Shared by all worktrees of the repo (lives in the
# bare's hooks/ dir, which is the common hooks path).
#
# Removes:  "Co-authored-by: Claude ..."  /  "Generated with [Claude Code] ..."  /  the 🤖
#           line  /  any line with an anthropic noreply address.
# Real human co-authors (e.g. "Co-authored-by: Jane <jane@corp.com>") are left untouched.
set -euo pipefail
msg="${1:?commit-msg file path expected}"

sed -i -E \
  -e '/^[[:space:]]*Co-authored-by:[[:space:]].*[Cc]laude/Id' \
  -e '/noreply@anthropic\.com/d' \
  -e '/Generated with .*Claude Code/Id' \
  -e '/🤖[[:space:]]*Generated/d' \
  "$msg"

# drop any blank lines left dangling at the very end
sed -i -e :a -e '/^[[:space:]]*$/{$d;N;ba' -e '}' "$msg" 2>/dev/null || true
exit 0
