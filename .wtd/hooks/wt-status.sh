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
#
# HOT PATH: this runs on EVERY tool call of every agent, and Claude blocks on it. Each external
# process costs ~80-150ms on Windows (Git Bash), so the common case must spawn nothing: builtins only
# (read, not cat), jq only when a field is actually needed, and no write when the state is unchanged
# (an unchanged write still fires every file watcher → the roster rebuilds → git status everywhere).
ev="${1:-}"
input=""; IFS= read -r -d '' input || true   # the hook JSON (empty when invoked with </dev/null)
d="${CLAUDE_PROJECT_DIR:-}"
c=""; fp=""
if [ -n "$input" ] && { [ -z "$d" ] || [ "$ev" = edit ]; }; then
  { IFS= read -r c; IFS= read -r fp; } < <(printf '%s' "$input" \
    | jq -r '(.cwd // ""), (.tool_input.file_path // .tool_response.filePath // "")' 2>/dev/null)
  c="${c%$'\r'}"; fp="${fp%$'\r'}"   # jq.exe on Windows emits CRLF
fi
[ -z "$d" ] && d=$(git -C "${c:-$PWD}" rev-parse --show-toplevel 2>/dev/null)
[ -z "$d" ] && d="${c:-$PWD}"
[ -n "$d" ] || exit 0
f="$d/.claude-status"
cur=""; [ -f "$f" ] && { IFS= read -r cur < "$f" || true; }
# Write only on change, and mirror each change into ONE central dir ($WTD/state/status/<key>) — the
# extension watches that single folder instead of every worktree (no handles held on worktrees, which
# on Windows block `git worktree move/remove`; and works with worktrees/** excluded from VSCode's
# watcher). key = the path under worktrees/ with '/' → '__'; '_dev' = the dev base (assistant).
# Sessions outside the dev tree (the hooks are global) aren't mirrored.
put() {
  [ "$cur" = "$1" ] && return 0
  printf '%s' "$1" > "$f"
  local wtd="${BASH_SOURCE[0]%/*}/.." dev key
  dev="${wtd%/.wtd/hooks/..}"
  case "$d" in
    "$dev")             key=_dev ;;
    "$dev"/worktrees/*) key="${d#"$dev"/worktrees/}"; key="${key//\//__}" ;;
    *) return 0 ;;
  esac
  [ -d "$wtd/state/status" ] || mkdir -p "$wtd/state/status"
  printf '%s' "$1" > "$wtd/state/status/$key"
}

# Milestone states (done, pr) are STICKY but don't FREEZE live activity. A new turn flips the glyph
# to `working` (so you see it run) while STASHING the milestone in .claude-status.resume; when the
# turn ends, a real source edit means the milestone is now stale → go to `input` (your turn, fires the
# unread highlight) and drop the stash, whereas a read-only turn (e.g. a question) RESTORES the
# stashed milestone. `reviewing` stays protected (review.sh owns it).
rf="$f.resume"
case "$ev" in
  sync)        : ;;                                              # no file change — just refresh the glyph from the file
  wip)         put working; rm -f "$rf" ;;                       # manual revert: clears the milestone too
  edit)        case "$fp" in                                     # real source edit → working + milestone is now stale
                 ''|*/pr-notes.md|*/CLAUDE.md|*/.claude-status*|*/.claude/*) ;;  # scratch/non-source → leave it
                 *) put working; [ -e "$rf" ] && rm -f "$rf" ;;
               esac ;;
  reviewing)   case "$cur" in done|pr) ;; *) put reviewing;; esac ;;  # review in progress (purple)
  reviewed)    case "$cur" in done|pr) ;; *) put input;; esac ;;      # review finished → your turn (orange)
  done)        put done; rm -f "$rf" ;;
  pr)          put pr;   rm -f "$rf" ;;                                # PR-ready milestone (light blue)
  working|tool)                                                        # new prompt / any tool call
               case "$cur" in
                 done|pr)    printf '%s' "$cur" > "$rf"; put working ;;  # stash milestone, show working
                 reviewing)  ;;                                          # review running → leave it
                 *)          put working ;;
               esac ;;
  stop|sessionend)
               if [ -s "$rf" ]; then IFS= read -r m < "$rf" || true; put "$m"; rm -f "$rf"   # read-only turn → restore milestone
               elif [ "$ev" = stop ]; then case "$cur" in reviewing|done|pr) ;; *) put input;; esac   # turn ended → your turn (unread)
               else case "$cur" in done|pr|reviewing) ;; *) put stopped;; esac; fi ;;              # session gone → red (sticky if idle milestone)
esac

final=""; [ -f "$f" ] && { IFS= read -r final < "$f" || true; }
# Nothing changed and nothing to re-sync → done (the common tool-call case: zero external processes).
[ "$final" = "$cur" ] && [ "$ev" != sync ] && [ "$ev" != stop ] && exit 0
# No tmux (native Windows) → no tab glyph to mirror; only the turn-end bell below still applies.
if ! command -v tmux >/dev/null 2>&1; then
  [ "$ev" = stop ] && printf '\a' > /dev/tty 2>/dev/null
  exit 0
fi

# Mirror the final state into the tmux tab glyph. Target the worktree's OWN session by name (derived
# from $d), so it's correct even when the caller's $TMUX points elsewhere or is unset (e.g. `agent
# done` run from another pane). Session name = "<slug>-<name>" = the path under worktrees/ with the
# first '/' turned into '-'.
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
# clears it when you focus the tab. This is VISUAL only — the bell SOUND is kept off
# (accessibility.signals.terminalBell.sound="off" in .vscode/settings.json) so it doesn't beep.
[ "$ev" = stop ] && printf '\a' > /dev/tty 2>/dev/null
exit 0
