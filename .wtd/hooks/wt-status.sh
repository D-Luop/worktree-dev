#!/usr/bin/env bash
# Single source of truth for a worktree's Claude status: the .claude-status sentinel (read by the
# VSCode claude-status extension for Explorer folder color) AND the tmux tab-title glyph
# (@wt_status, shown in the VSCode terminal tab via set-titles). Called by Claude hooks and by
# `agent done` / `agent wip`.
#
#   wt-status.sh working|tool|stop|sessionend|done|pr|wip|edit|reviewing|reviewed
# States: working(🔵 busy) · input(🟠 your turn) · reviewing(🟣 waiting on review) · pr(🔹 PR-ready,
#         light blue) · done(🟢) · stopped(🔴 no session) · none(no status). pr & done are STICKY
#         MILESTONES that don't freeze live activity: a new turn shows `working` while stashing the
#         milestone in .claude-status.resume, then RESTORES it on a read-only turn, or drops it to
#         `input` once a real source edit makes it stale. `reviewing` stays protected (review.sh owns it).
#
# Hook events pass their JSON on stdin (we read .cwd from it); manual callers pass </dev/null.
ev="${1:-}"
input="$(cat 2>/dev/null)"             # the hook JSON (empty when invoked with </dev/null)
c=$(printf '%s' "$input" | jq -r '.cwd // empty' 2>/dev/null)
fp=$(printf '%s' "$input" | jq -r '.tool_input.file_path // .tool_response.filePath // empty' 2>/dev/null)  # edited file (PreToolUse Edit/Write)
d="${CLAUDE_PROJECT_DIR:-}"
[ -z "$d" ] && d=$(git -C "${c:-$PWD}" rev-parse --show-toplevel 2>/dev/null)
[ -z "$d" ] && d="${c:-$PWD}"
[ -n "$d" ] || exit 0
f="$d/.claude-status"
cur=$(cat "$f" 2>/dev/null)

# Milestone states (done, pr) are STICKY but don't FREEZE live activity. A new turn flips the glyph
# to `working` (so you see it run) while STASHING the milestone in .claude-status.resume; when the
# turn ends, a real source edit means the milestone is now stale → go to `input` (your turn, fires the
# unread highlight) and drop the stash, whereas a read-only turn (e.g. a question) RESTORES the
# stashed milestone. `reviewing` stays protected (review.sh owns it).
rf="$f.resume"
case "$ev" in
  sync)        : ;;                                              # no file change — just refresh the glyph from the file
  wip)         printf working > "$f"; rm -f "$rf" ;;             # manual revert: clears the milestone too
  edit)        case "$fp" in                                     # real source edit → working + milestone is now stale
                 ''|*/pr-notes.md|*/CLAUDE.md|*/.claude-status*|*/.claude/*) ;;  # scratch/non-source → leave it
                 *) printf working > "$f"; rm -f "$rf" ;;
               esac ;;
  reviewing)   case "$cur" in done|pr) ;; *) printf reviewing > "$f";; esac ;;  # review in progress (purple)
  reviewed)    case "$cur" in done|pr) ;; *) printf input     > "$f";; esac ;;  # review finished → your turn (orange)
  done)        printf done     > "$f"; rm -f "$rf" ;;
  pr)          printf pr       > "$f"; rm -f "$rf" ;;                            # PR-ready milestone (light blue)
  working)     case "$cur" in                                                   # new prompt
                 done|pr)    printf '%s' "$cur" > "$rf"; printf working > "$f" ;;  # stash milestone, show working
                 reviewing)  ;;                                                   # review running → leave it
                 *)          printf working > "$f" ;;
               esac ;;
  tool)        case "$cur" in                                                   # read-only tool call
                 done|pr)    printf '%s' "$cur" > "$rf"; printf working > "$f" ;;
                 reviewing)  ;;
                 *)          printf working > "$f" ;;
               esac ;;
  stop)        if [ -s "$rf" ]; then cat "$rf" > "$f"; rm -f "$rf";             # read-only turn → restore milestone
               else case "$cur" in reviewing|done|pr) ;; *) printf input > "$f";; esac; fi ;;  # else turn ended → your turn (unread); keep a freshly-set milestone (matches sessionend)
  sessionend)  if [ -s "$rf" ]; then cat "$rf" > "$f"; rm -f "$rf";             # session gone mid-turn, no edits → restore
               else case "$cur" in done|pr|reviewing) ;; *) printf stopped > "$f";; esac; fi ;;  # else red (sticky if idle milestone)
esac

# Mirror the final state into the tmux tab glyph. Target the worktree's OWN session by name (derived
# from $d), so it's correct even when the caller's $TMUX points elsewhere or is unset (e.g. `agent
# done` run from another pane). Session name = "<slug>-<name>" = the path under worktrees/ with the
# first '/' turned into '-'.
final=$(cat "$f" 2>/dev/null)
case "$final" in working) g=🔵;; input) g=🟡;; reviewing) g=🟣;; pr) g=🔹;; done) g=🟢;; stopped) g=🔴;; *) g="";; esac
sess=""
case "$d" in */worktrees/*) rel="${d#*/worktrees/}"; sess="${rel/\//-}";; esac
if [ -n "$sess" ] && tmux has-session -t "$sess" 2>/dev/null; then
  tmux set -t "$sess" @wt_status "$g" 2>/dev/null || true
  for c in $(tmux list-clients -t "$sess" -F '#{client_name}' 2>/dev/null); do
    tmux refresh-client -t "$c" 2>/dev/null || true     # re-emit the title to that session's terminal now
  done
elif [ -n "${TMUX:-}" ]; then
  tmux set @wt_status "$g" 2>/dev/null || true
  tmux refresh-client 2>/dev/null || true
fi

# Visual "unread" indicator on turn-end: write a BEL to the pane tty. tmux (visual-bell off,
# bell-action any) passes it to the VSCode terminal, which badges the tab while it's unfocused and
# clears it when you focus the tab. SOUND is muted in VSCode settings — we keep our own alert.wav.
[ "$ev" = stop ] && printf '\a' > /dev/tty 2>/dev/null
exit 0
