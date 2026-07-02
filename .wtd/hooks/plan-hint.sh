#!/usr/bin/env bash
# PostToolUse(Write|Edit|MultiEdit) hook, global (all repos). Reacts to plan-doc edits:
#   • .claude/plans/*.html (the /plan LIVING plan) → re-stage its preview so the roster panel reflects
#     the change immediately (the agent doesn't have to remember to re-run `preview` after ticking a
#     step). The extension live-refreshes the open panel when the staged file changes.
#   • .claude/plans/*.md  (the /blueprint markdown plan) → print a double-clickable `view_plan` token
#     so the user can render it in the diff pane (@claudepane DoubleClick → commit-diff-show.sh → md-render.py).
# Reads the hook payload JSON on stdin.
in="$(cat)"
fp="$(printf '%s' "$in" | jq -r '.tool_input.file_path // .tool_input.path // empty' 2>/dev/null)"
case "$fp" in
  */.claude/plans/*.html|*active-plan.html)
    pv="$HOME/.local/bin/preview"; [ -x "$pv" ] || pv=preview
    ( "$pv" "$fp" >/dev/null 2>&1 || true ) &    # re-stage in the background; PWD is the worktree here
    jq -n '{systemMessage: "🗺 living plan updated — preview panel refreshed", suppressOutput: true}' ;;
  */.claude/plans/*.md|*active-plan.md)
    jq -n '{systemMessage: "📋 plan updated — double-click  view_plan  to render it in the diff pane", suppressOutput: true}' ;;
  *) : ;;
esac
