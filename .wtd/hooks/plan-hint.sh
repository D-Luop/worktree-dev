#!/usr/bin/env bash
# PostToolUse(Write|Edit|MultiEdit) hook, global (all repos). Reacts to plan-doc edits:
#   • .claude/plans/*.html (the /plan LIVING plan) → re-stage its preview so the roster panel reflects
#     the change immediately (the agent doesn't have to remember to re-run `preview` after ticking a
#     step). The extension live-refreshes the open panel when the staged file changes.
#   • .claude/plans/active-plan.md (the /blueprint markdown plan — the documented SOURCE OF TRUTH) →
#     render it to the living-plan HTML (plan-md-to-html.py) and stage it as the `plan` tab, so the
#     markdown plan shows in the roster preview panel too (with checkboxes + ▶ Start), AND print the
#     double-clickable `view_plan` token so the user can also render it in the diff pane
#     (@claudepane DoubleClick → commit-diff-show.sh → md-render.py).
# Reads the hook payload JSON on stdin.
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
in="$(cat)"
fp="$(printf '%s' "$in" | jq -r '.tool_input.file_path // .tool_input.path // empty' 2>/dev/null)"
pv="$HOME/.local/bin/preview"; [ -x "$pv" ] || pv=preview
case "$fp" in
  */.claude/plans/*.html|*active-plan.html)
    ( "$pv" "$fp" plan >/dev/null 2>&1 || true ) &    # re-stage the `plan` tab; PWD is the worktree here
    jq -n '{systemMessage: "🗺 living plan updated — preview panel refreshed", suppressOutput: true}' ;;
  */.claude/plans/active-plan.md)
    # render the markdown plan → the `plan` preview tab (PWD is the worktree, so preview resolves it)
    py="$(command -v python3 || command -v python)"
    if [ -n "$py" ]; then
      ( tmp="$(mktemp -t plan-XXXXXX.html 2>/dev/null || echo "${TMPDIR:-/tmp}/plan-$$.html")"
        "$py" "$here/plan-md-to-html.py" "$fp" "$tmp" >/dev/null 2>&1 \
          && "$pv" "$tmp" plan >/dev/null 2>&1
        rm -f "$tmp" ) &
    fi
    jq -n '{systemMessage: "📋 plan updated — preview panel refreshed; double-click  view_plan  to render it in the diff pane", suppressOutput: true}' ;;
  */.claude/plans/*.md)
    jq -n '{systemMessage: "📋 plan updated — double-click  view_plan  to render it in the diff pane", suppressOutput: true}' ;;
  *) : ;;
esac
