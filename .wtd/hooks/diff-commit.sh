#!/usr/bin/env bash
# Show a commit's diff in a specific tmux session's diff pane (@dpane). Used by the VSCode
# terminal-link provider: click a SHA in an agent's chat → repaint that session's bottom-right diff
# pane with the commit, exactly like double-clicking a SHA in the commit pane. `q` returns the pane
# to its live working diff.
# Usage: diff-commit.sh <tmux-session> <sha>
set -u
here="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
session="${1:-}"
sha="$(printf '%s' "${2:-}" | grep -oiE '[0-9a-f]{7,40}' | head -1)"
[ -n "$session" ] && [ -n "$sha" ] || exit 0

# worktree + diff pane from THIS session's panes (@cpane carries the worktree path; @dpane/@dlog the
# diff pane and its live log)
wt="$(tmux list-panes -t "$session:" -F '#{?#{@cpane},#{pane_current_path},}' 2>/dev/null | grep -m1 .)"
dpane="$(tmux list-panes -t "$session:" -F '#{?#{@dpane},#{pane_id},}' 2>/dev/null | grep -m1 .)"
dlog="$(tmux list-panes -t "$session:" -F '#{?#{@dpane},#{@dlog},}' 2>/dev/null | grep -m1 .)"
[ -n "$wt" ] && [ -n "$dpane" ] || exit 0
: "${dlog:=.claude/diffs.log}"

# only act on a real commit in that worktree
git -C "$wt" rev-parse --verify --quiet "${sha}^{commit}" >/dev/null 2>&1 || exit 0

# drop generated files from the diff (same as commit-diff-show.sh)
# shellcheck source=generated-filter.sh
. "$here/generated-filter.sh"
gx=""
while IFS= read -r p; do
  [ -n "$p" ] || continue
  esc="${p//\'/\'\\\'\'}"; gx+=" '$esc'"
done < <({ git -C "$wt" show --name-only --format= "$sha" | gen_excludes "$wt" "$sha"; test_excludes; })

pager='less -RQ --mouse --wheel-lines=5'
W="$(tmux display-message -p -t "$dpane" '#{pane_width}' 2>/dev/null)"; case "$W" in ''|*[!0-9]*) W=80;; esac
dstyle="--plus-style 'syntax #166e22' --plus-emph-style 'syntax #1f9b34' --minus-style 'syntax #5a1d1d' --minus-emph-style 'syntax #8a2e2e'"
dstyle="$dstyle --file-style 'bold yellow' --file-decoration-style 'yellow ol' --line-numbers"
pfx="--src-prefix='a/$wt/' --dst-prefix='b/$wt/'"
if command -v delta >/dev/null 2>&1; then
  show="git -C '$wt' show --stat --patch $pfx '$sha' -- . $gx | delta --paging=never --width=$((W-2)) $dstyle | sed 's/^/  /' | $pager"
else
  show="git -C '$wt' -c color.ui=always show --stat --patch $pfx '$sha' -- . $gx | sed 's/^/  /' | $pager"
fi

tmux respawn-pane -k -t "$dpane" "$show; exec '$here/diff-pane.sh' '$wt' '$dlog'"
tmux select-pane -t "$dpane"
