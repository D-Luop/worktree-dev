#!/usr/bin/env bash
# Live commit-history pane for a worktree: redraw `git log --graph` whenever HEAD's reflog
# moves (commit/checkout/fetch). Oldest-at-top, newest-at-bottom: long commit subjects WRAP,
# and the pane is allowed to scroll so the newest commit always stays on the bottom row.
# Usage: commit-pane.sh <worktree-dir>
#
# (A script, not an inline tmux command, so there's no nested-quote escaping to get wrong.)
# Note: --graph can't combine with --reverse, so we reverse the (linear) graph with `tac`.
set -u

wt="${1:?usage: commit-pane.sh <worktree-dir>}"
here="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"   # for commit-fmt.awk alongside this script
hl="$(git -C "$wt" rev-parse --git-path logs/HEAD 2>/dev/null)"
case "$hl" in /*) ;; *) hl="$wt/$hl";; esac
# slug + name, to detect a live `review.sh` of THIS worktree (its args are "<slug> <name>")
rel="${wt#*/worktrees/}"; rslug="${rel%%/*}"; rname="${rel#*/}"

last=
lastdraw=0
lastdirty=__init__
lastcols=; lastrows=; lastrev=
while :; do
  c="$(stat -c %y "$hl" 2>/dev/null || echo 0)"
  now="$(date +%s)"
  # is the worktree dirty? (any staged/unstaged/untracked change). Drives the footer button below.
  dirty="$(git -C "$wt" status --porcelain 2>/dev/null | head -c1)"
  # is a reviewer (review.sh) live for THIS worktree? ([r] keeps pgrep from matching its own args)
  revnow=""; pgrep -fa '[r]eview\.sh' 2>/dev/null | grep -qF " $rslug $rname" && revnow=1
  rows="$(tput lines 2>/dev/null || echo 20)"
  cols="$(tput cols 2>/dev/null || echo 80)"
  # redraw on a reflog move, every 60s (ages stay current), on a dirty-state flip, a live-review
  # flip, OR on a RESIZE (e.g. a VSCode reload re-attaches the client at a new size).
  if [ "$c" != "$last" ] || [ "$(( now - lastdraw ))" -ge 60 ] || [ "$dirty" != "$lastdirty" ] \
     || [ "$revnow" != "$lastrev" ] || [ "$cols" != "$lastcols" ] || [ "$rows" != "$lastrows" ]; then
    last="$c"; lastdraw="$now"; lastdirty="$dirty"; lastrev="$revnow"; lastcols="$cols"; lastrows="$rows"
    # short SHAs of commits not on any remote (UNPUSHED) — the awk colors these cyan, not yellow
    unpushed="$(git -C "$wt" log --format='%h' HEAD --not --remotes 2>/dev/null)"
    # divergence from the repo's default branch: commits on HEAD not reachable from origin/<default>.
    # The awk draws a "⌲ diverges from <default>" rule above the oldest such commit (none if on-default).
    defbr="$(git -C "$wt" symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null | sed 's@^origin/@@')"
    [ -z "$defbr" ] && for b in main master dev; do
      git -C "$wt" rev-parse --verify --quiet "origin/$b" >/dev/null 2>&1 && { defbr="$b"; break; }
    done
    diverged=""; [ -n "$defbr" ] && diverged="$(git -C "$wt" log --format='%h' HEAD --not "origin/$defbr" 2>/dev/null)"
    # reserve bottom rows for the footer buttons: "uncommitted" sep+button (when dirty) + the
    # "view active plan" button (when an active plan exists).
    plan=""; [ -f "$wt/.claude/plans/active-plan.md" ] && plan=1
    prnotes=""; [ -f "$wt/pr-notes.md" ] && prnotes=1
    reviewing="$revnow"; [ -z "$reviewing" ] && [ "$(cat "$wt/.claude-status" 2>/dev/null)" = reviewing ] && reviewing=1
    # past completed reviews (newest first, capped) — shown INLINE in the commit list as pink rows
    # (view_review_<N> tokens), positioned by when each ran. NOT footer buttons.
    # only reviews that actually produced their review.md artifact — a failed/aborted run leaves a bare
    # timestamp dir with no review.md, which would otherwise show a dead view_review_<N> row that errors
    # with "no review at .../review.md" on double-click. Filter BEFORE the cap so 6 means 6 real reviews.
    revdirs=(); while IFS= read -r rd; do
      [ -n "$rd" ] && [ -s "$rd/review.md" ] && revdirs+=("$rd")
      [ "${#revdirs[@]}" -ge 6 ] && break
    done < <(ls -1dt "$wt/.claude/reviews/"*/ 2>/dev/null)
    nrev=${#revdirs[@]}
    # footer = a "── actions ──" section rule + one line per button (uncommitted, full-branch-vs-default,
    # active plan, PR notes, live review)
    nbtn=0
    [ -n "$dirty" ] && nbtn=$((nbtn + 1)); [ -n "$diverged" ] && nbtn=$((nbtn + 1))
    [ -n "$plan" ] && nbtn=$((nbtn + 1)); [ -n "$prnotes" ] && nbtn=$((nbtn + 1)); [ -n "$reviewing" ] && nbtn=$((nbtn + 1))
    foot=0; [ "$nbtn" -gt 0 ] && foot=$((nbtn + 1))
    # reserve rows: commits fill what's left after the footer AND the inline review rows
    nlog=$((rows - foot - nrev)); [ "$nlog" -lt 1 ] && nlog=1
    # one timeline: commits (type C) + past reviews (type R, epoch = dir mtime), sorted oldest->newest
    cstream="$(git -C "$wt" --no-pager -c color.ui=always log \
            --format='%ct%x1fC%x1f%h%x1f%C(auto)%D%C(reset)%x1f%s%x1f%cs' -"$nlog" 2>/dev/null)"
    rstream=""; i=0
    for rd in ${revdirs[@]+"${revdirs[@]}"}; do
      i=$((i + 1)); rb="$(basename "$rd")"; mt="$(stat -c %Y "$rd" 2>/dev/null)" || mt=""
      [ -n "$mt" ] || continue
      rstream="$rstream$(printf '%s\037R\037view_review_%d\037%s' "$mt" "$i" "${rb%%__*}")"$'\n'
    done
    out="$(printf '%s\n%s' "$cstream" "$rstream" | sed '/^$/d' | LC_ALL=C sort -t "$(printf '\037')" -k1,1n \
            | awk -v cols="$cols" -v now="$now" -v unpushed="$unpushed" -v diverged="$diverged" -v base="$defbr" -f "$here/commit-fmt.awk")"
    printf '\033[H\033[2J\033[3J\033[?7l'   # home, clear screen+scrollback, autowrap OFF (we wrap)
    printf '%s' "$out"
    if [ "$foot" -gt 0 ]; then
      # pad, then the footer buttons on their own lines at the bottom. The button tokens
      # (view_uncommitted_diff / view_active_plan) are what tmux's double-click handler pipes to
      # commit-diff-show.sh.
      used="$(printf '%s' "$out" | awk 'END{print NR}')"; : "${used:=0}"
      pad=$(( rows - used - foot )); [ "$pad" -lt 1 ] && pad=1
      awk -v n="$pad" 'BEGIN{while (n-- > 0) print ""}'
      rlen=$(( cols > 12 ? cols - 12 : 4 )); rule="$(printf '\342\224\200%.0s' $(seq 1 "$rlen"))"
      # section break between the commit list and the action buttons
      printf '\033[90m\342\224\200\342\224\200 actions %s\033[0m\n' "$rule"
      printed=0
      if [ -n "$dirty" ]; then
        [ "$printed" = 1 ] && printf '\n'
        printf '\033[2;33m▸\033[0m \033[1;30;43m view_uncommitted_diff \033[0m \033[2;33mdouble-click to review uncommitted changes\033[0m'
        printed=1
      fi
      if [ -n "$diverged" ]; then
        [ "$printed" = 1 ] && printf '\n'
        printf '\033[2;31m▸\033[0m \033[1;30;41m view_branch_diff \033[0m \033[2;31mdouble-click for the full branch diff vs %s\033[0m' "${defbr:-main}"
        printed=1
      fi
      if [ -n "$plan" ]; then
        [ "$printed" = 1 ] && printf '\n'
        printf '\033[2;36m▸\033[0m \033[1;30;46m view_active_plan \033[0m \033[2;36mdouble-click to render · ctrl-click to open as text\033[0m'
        printed=1
      fi
      if [ -n "$prnotes" ]; then
        [ "$printed" = 1 ] && printf '\n'
        printf '\033[2;32m▸\033[0m \033[1;30;42m view_pr_notes \033[0m \033[2;32mdouble-click to render · ctrl-click to open as text\033[0m'
        printed=1
      fi
      if [ -n "$reviewing" ]; then
        [ "$printed" = 1 ] && printf '\n'
        printf '\033[2;35m▸\033[0m \033[1;30;45m view_review_status \033[0m \033[2;35mdouble-click to watch the live review\033[0m'
        printed=1
      fi
    fi
  fi
  sleep 1
done
