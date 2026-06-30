#!/usr/bin/env bash
# PostToolUse(Write|Edit|MultiEdit) hook: when an agent writes/updates a plan doc, print a nudge with
# a double-clickable token so the user can render the plan nicely in the diff pane. The token
# `view_plan` is caught by the @claudepane DoubleClick handler -> commit-diff-show.sh -> md-render.py.
# Global (all repos). Reads the hook payload JSON on stdin.
in="$(cat)"
fp="$(printf '%s' "$in" | jq -r '.tool_input.file_path // .tool_input.path // empty' 2>/dev/null)"
case "$fp" in
  */.claude/plans/*.md|*active-plan.md)
    jq -n '{systemMessage: "📋 plan updated — double-click  view_plan  to render it in the diff pane", suppressOutput: true}' ;;
  *) : ;;
esac
