#!/usr/bin/env bash
# PreToolUse(Edit|Write|MultiEdit) hook: keeps .claude/plans/active-plan.md handoff-ready. Never blocks —
# it only adds a reminder to the agent's context:
#   • source edit with no active plan → once per session
#   • every 15 source edits with no plan edit in between → "plan is stale; update Handoff"
# Native Windows: `wtd hook plan-nudge` implements this (unit-tested); delegate when it is installed.
# State: .wtd/state/plan-nudge/<key> = session / warned (0|1) / edits, one per line (same as the Rust).
W="${BASH_SOURCE[0]%/*}/../bin/wtd.exe"; [ -x "$W" ] && exec "$W" hook plan-nudge

in="$(cat)"
{ IFS= read -r sid; IFS= read -r c; IFS= read -r fp; } < <(printf '%s' "$in" \
  | jq -r '(.session_id // ""), (.cwd // ""), (.tool_input.file_path // .tool_input.path // "")' 2>/dev/null)
sid="${sid%$'\r'}"; c="${c%$'\r'}"; fp="${fp%$'\r'}"
d="${CLAUDE_PROJECT_DIR:-}"
[ -z "$d" ] && d=$(git -C "${c:-$PWD}" rev-parse --show-toplevel 2>/dev/null)
[ -n "$d" ] || exit 0
wtd="${BASH_SOURCE[0]%/*}/.."; dev="${wtd%/.wtd/hooks/..}"
case "$d" in
  "$dev")             key=_dev ;;
  "$dev"/worktrees/*) key="${d#"$dev"/worktrees/}"; key="${key//\//__}" ;;
  *) exit 0 ;;
esac
sf="$wtd/state/plan-nudge/$key"
s=""; warned=0; edits=0
[ -f "$sf" ] && { IFS= read -r s; IFS= read -r warned; IFS= read -r edits; } < "$sf"
[ "$s" = "$sid" ] || { s="$sid"; warned=0; edits=0; }

msg=""
p="${fp//'\'/'/'}"
case "$p" in
  */.claude/plans/active-plan.md) edits=0 ;;
  ''|*/pr-notes.md|*/CLAUDE.md|*/.claude-status*|*/.claude/*) ;;
  *) if [ ! -f "$d/.claude/plans/active-plan.md" ]; then
       [ "$warned" = 1 ] || { warned=1; msg="No active plan: write \`.claude/plans/active-plan.md\` (use /blueprint) before editing source. Include the steps, a Progress Log and a \`## Handoff\` section (current step, next action, open questions, decisions, gotchas, files in flux) so another agent could take over at any moment."; }
     else
       edits=$((edits + 1))
       [ $((edits % 15)) -eq 0 ] && msg="Plan is stale: $edits source edits since \`.claude/plans/active-plan.md\` was last updated. Append to its Progress Log and overwrite \`## Handoff\` (current step, exact next action, open questions, decisions + why, gotchas, files in flux) now."
     fi ;;
esac
mkdir -p "${sf%/*}"; printf '%s\n%s\n%s\n' "$s" "$warned" "$edits" > "$sf"
[ -n "$msg" ] && jq -n --arg m "$msg" '{systemMessage: $m, hookSpecificOutput: {hookEventName: "PreToolUse", additionalContext: $m}}'
exit 0
