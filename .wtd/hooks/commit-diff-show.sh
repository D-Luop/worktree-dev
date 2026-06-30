#!/usr/bin/env bash
# tmux helper: double-click a SHA in a commit-history pane (@cpane) -> show that commit's diff
# IN THE DIFF PANE (bottom-right, marked @dpane), not a popup. The double-clicked selection is
# piped in on STDIN; we pull the first SHA out of it. The diff opens in a pager (delta if present,
# else git+less) inside the diff pane; pressing q returns the pane to its live working diff.
set -u

here="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
word="$(cat)"

# worktree from the commit pane; diff pane id + its log from @dpane/@dlog (current window)
wt="$(tmux list-panes -F '#{?#{@cpane},#{pane_current_path},}' 2>/dev/null | grep -m1 .)"
dpane="$(tmux list-panes -F '#{?#{@dpane},#{pane_id},}' 2>/dev/null | grep -m1 .)"
dlog="$(tmux list-panes -F '#{?#{@dpane},#{@dlog},}' 2>/dev/null | grep -m1 .)"
[ -n "$wt" ] && [ -n "$dpane" ] || exit 0
: "${dlog:=.claude/diffs.log}"

# "view_uncommitted_diff" button (commit-pane footer) -> show the FULL working diff in the diff pane
# (everything pending vs HEAD + untracked, all file types — broader than the live filtered diff).
if printf '%s' "$word" | grep -qi 'view_uncommitted_diff'; then
  pager='less -RQ --mouse --wheel-lines=5'
  W="$(tmux display-message -p -t "$dpane" '#{pane_width}' 2>/dev/null)"; case "$W" in ''|*[!0-9]*) W=80;; esac
  tmux respawn-pane -k -t "$dpane" "'$here/worktree-diff.sh' '$wt' '$W' | $pager; exec '$here/diff-pane.sh' '$wt' '$dlog'"
  tmux select-pane -t "$dpane"
  exit 0
fi

# "view_branch_diff" button -> the WHOLE branch vs the default branch: merge-base(HEAD, origin/<def>)
# through the working tree (every committed + uncommitted change unique to this branch).
if printf '%s' "$word" | grep -qi 'view_branch_diff'; then
  defbr="$(git -C "$wt" symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null | sed 's@^origin/@@')"
  [ -z "$defbr" ] && for b in main master dev; do
    git -C "$wt" rev-parse --verify --quiet "origin/$b" >/dev/null 2>&1 && { defbr="$b"; break; }
  done
  base="$(git -C "$wt" merge-base HEAD "origin/$defbr" 2>/dev/null)"
  pager='less -RQ --mouse --wheel-lines=5'
  W="$(tmux display-message -p -t "$dpane" '#{pane_width}' 2>/dev/null)"; case "$W" in ''|*[!0-9]*) W=80;; esac
  if [ -n "$base" ]; then
    tmux respawn-pane -k -t "$dpane" "'$here/worktree-diff.sh' '$wt' '$W' '$base' | $pager; exec '$here/diff-pane.sh' '$wt' '$dlog'"
  else
    tmux respawn-pane -k -t "$dpane" "printf '  no default branch (origin/HEAD) to diff against\n' | $pager; exec '$here/diff-pane.sh' '$wt' '$dlog'"
  fi
  tmux select-pane -t "$dpane"
  exit 0
fi

# "view_plan" token (printed by the plan-hint hook after an agent writes a plan) -> render the
# worktree's active plan markdown NICELY (md-render.py) in the diff pane; q returns to the live diff.
# "view_review_status" token (commit-pane footer, during a live review) -> return the diff pane to its
# LIVE tail: during review that shows the reviewer's progress spinner; on completion, the findings.
if printf '%s' "$word" | grep -qi 'view_review_status'; then
  tmux respawn-pane -k -t "$dpane" "exec '$here/diff-pane.sh' '$wt' '$dlog'"
  tmux select-pane -t "$dpane"
  exit 0
fi

# render a worktree markdown doc (active plan / PR notes) nicely in the diff pane; q returns to live.
render_md_doc() {  # $1 = abs path, $2 = human label
  local doc="$1" label="$2" W
  local pager='less -RQ --mouse --wheel-lines=5'
  W="$(tmux display-message -p -t "$dpane" '#{pane_width}' 2>/dev/null)"; case "$W" in ''|*[!0-9]*) W=80;; esac
  if [ -f "$doc" ]; then
    tmux respawn-pane -k -t "$dpane" "'$here/md-render.py' '$doc' $((W-3)) | sed 's/^/ /' | $pager; exec '$here/diff-pane.sh' '$wt' '$dlog'"
  else
    tmux respawn-pane -k -t "$dpane" "printf '  no %s at %s\n' '$label' '$doc' | $pager; exec '$here/diff-pane.sh' '$wt' '$dlog'"
  fi
  tmux select-pane -t "$dpane"
}
if printf '%s' "$word" | grep -qi 'view_pr_notes'; then
  render_md_doc "$wt/pr-notes.md" "PR notes"; exit 0
fi
if printf '%s' "$word" | grep -qiE 'view_(active_)?plan'; then
  render_md_doc "$wt/.claude/plans/active-plan.md" "plan"; exit 0
fi
# "view_review_<N>" (commit-pane footer) -> render the Nth-newest past review's report (N=1 newest)
if printf '%s' "$word" | grep -qiE 'view_review_[0-9]+'; then
  n="$(printf '%s' "$word" | grep -oiE 'view_review_[0-9]+' | grep -oE '[0-9]+$' | head -1)"
  rd="$(ls -1dt "$wt/.claude/reviews/"*/ 2>/dev/null | sed -n "${n}p")"
  render_md_doc "${rd:-/nonexistent/}review.md" "review"; exit 0
fi

sha="$(printf '%s' "$word" | grep -oiE '[0-9a-f]{7,40}' | head -1)"
[ -n "$sha" ] || exit 0

# only act on a real commit in that repo
git -C "$wt" rev-parse --verify --quiet "${sha}^{commit}" >/dev/null 2>&1 || exit 0

# merge commit (>=2 parents)? default `git show` on a merge prints a *combined* diff that's almost
# always empty, so clicking a merge would paint a blank pane. Show the first-parent diff instead —
# i.e. what the merge actually brought in. ($mflag is empty for ordinary single-parent commits.)
mflag=""
[ "$(git -C "$wt" rev-list --parents -n1 "$sha" 2>/dev/null | wc -w)" -ge 3 ] && mflag='-m --first-parent'

# drop generated files from the commit's diff (the repo defines what's generated via
# .gitattributes / Code-generated headers — see generated-filter.sh). Build single-quoted
# ':(exclude)<path>' pathspecs to splice into the git-show command below. Generated- and test-file
# excludes are tracked SEPARATELY ($gx vs $tx) so that, if every changed file gets filtered out, we
# can explain *why* (test-only commit hidden by the toggle vs all-generated) instead of a blank pane.
. "$here/generated-filter.sh"
gx=""
while IFS= read -r p; do
  [ -n "$p" ] || continue
  esc="${p//\'/\'\\\'\'}"; gx+=" '$esc'"
done < <(git -C "$wt" show $mflag --name-only --format= "$sha" | gen_excludes "$wt" "$sha")
tx=""
while IFS= read -r p; do
  [ -n "$p" ] || continue
  esc="${p//\'/\'\\\'\'}"; tx+=" '$esc'"
done < <(test_excludes)

# would the diff be empty after the active excludes? A commit that touched files but has none left
# (all generated, or all tests while the test-hide toggle is on) would otherwise show a blank pager.
W="$(tmux display-message -p -t "$dpane" '#{pane_width}' 2>/dev/null)"; case "$W" in ''|*[!0-9]*) W=80;; esac
survive="$(eval "git -C '$wt' show $mflag --name-only --format= '$sha' -- . $gx $tx" 2>/dev/null | grep -c .)"
if [ "${survive:-0}" -eq 0 ]; then
  total="$(eval "git -C '$wt' show $mflag --name-only --format= '$sha'" 2>/dev/null | grep -c .)"
  notests="$(eval "git -C '$wt' show $mflag --name-only --format= '$sha' -- . $gx" 2>/dev/null | grep -c .)"
  if [ "${total:-0}" -eq 0 ]; then
    msg='  this commit changed no files'
  elif [ -n "$tx" ] && [ "${notests:-0}" -gt 0 ]; then
    msg="$(printf '  \033[1;33mTest-only commit.\033[0m All %d changed file(s) are tests, hidden by the\n  test-exclude toggle. Re-enable tests (the tests button beside +agent) to view this diff.' "$notests")"
  else
    msg="$(printf '  Nothing to show: all %d changed file(s) are generated/excluded.' "$total")"
  fi
  pager='less -RQ --mouse --wheel-lines=5'
  tmux respawn-pane -k -t "$dpane" "printf '%s\n' \"$msg\" | $pager; exec '$here/diff-pane.sh' '$wt' '$dlog'"
  tmux select-pane -t "$dpane"
  exit 0
fi

# Pager is less with --mouse so the wheel scrolls inside the pane (tmux forwards wheel events
# once less enables mouse tracking); --wheel-lines makes each notch move several lines instead
# of one (the default, which feels very slow). arrows / PgUp-PgDn / space / g / G work too; q returns.
# A 2-space left margin keeps text off the pane border; delta renders 2 cols narrower so its
# full-width diff backgrounds don't overflow once indented.
pager='less -RQ --mouse --wheel-lines=5'
W="$(tmux display-message -p -t "$dpane" '#{pane_width}' 2>/dev/null)"; case "$W" in ''|*[!0-9]*) W=80;; esac
# delta's default plus background is a near-black dark green (#002800); brighten additions to a
# clearly-green bg and removals to a clearer red (text stays syntax-highlighted).
dstyle="--plus-style 'syntax #166e22' --plus-emph-style 'syntax #1f9b34' --minus-style 'syntax #5a1d1d' --minus-emph-style 'syntax #8a2e2e'"
# Group each file's diff under its name: bold filename with a rule ABOVE it (overline), not the
# default underline (which caps the block above and made the name look like it belonged to it).
dstyle="$dstyle --file-style 'bold yellow' --file-decoration-style 'yellow ol' --line-numbers"
# Absolute src/dst prefixes -> delta prints the full on-disk path, so files are ctrl+click-
# resolvable in the (cwd-less) diff pane. delta strips the leading 'a/'/'b/' leaving '/abs/...';
# distinct a/ vs b/ avoids git's same-name "new file" misrender.
pfx="--src-prefix='a/$wt/' --dst-prefix='b/$wt/'"
if command -v delta >/dev/null 2>&1; then
  show="git -C '$wt' show $mflag --stat --patch $pfx '$sha' -- . $gx $tx | delta --paging=never --width=$((W-2)) $dstyle | sed 's/^/  /' | $pager"
else
  show="git -C '$wt' -c color.ui=always show $mflag --stat --patch $pfx '$sha' -- . $gx $tx | sed 's/^/  /' | $pager"
fi

# render the commit in the diff pane; when the pager exits (q), exec back to the live diff watcher
tmux respawn-pane -k -t "$dpane" "$show; exec '$here/diff-pane.sh' '$wt' '$dlog'"
tmux select-pane -t "$dpane"   # focus it so q / scroll go to the pager
