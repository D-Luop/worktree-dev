#!/usr/bin/env bash
# PostToolUse hook (matcher: Write|Edit|MultiEdit): (re)write the worktree's
# .claude/diffs.log with the CURRENT diff of everything pending — all uncommitted
# tracked changes vs HEAD plus untracked files (truncating, not appending). `agent`
# runs a side pane that clears and redraws on each change, so the pane always shows
# the current working-tree state — never an ever-growing log.
#
# General (repo-agnostic): it does not filter by extension — it shows anything the
# agent touched. This is the default diff hook for any repo.
#
# Seed mode:  repo-diff.sh --seed <worktree-root>   (regenerate now, no stdin payload)
# Highlighter: delta if installed, else git's own diff colors.
set -uo pipefail

if [ "${1:-}" = "--seed" ]; then
  root="${2:-}"; f=""
  [ -n "$root" ] || exit 0
else
  input="$(cat)"
  f="$(printf '%s' "$input" | jq -r '.tool_input.file_path // .tool_response.filePath // empty')"
  [ -n "$f" ] || exit 0
  dir="$(dirname "$f")"
  root="$(git -C "$dir" rev-parse --show-toplevel 2>/dev/null)" || exit 0
fi
[ -d "$root" ] || exit 0

# Tooling artifacts the agent doesn't "touch" meaningfully — keep them out of the diff
# (the .claude dir holds the diff log itself, status sentinel, settings, seeded CLAUDE.md).
EXCL=(':(exclude).claude' ':(exclude).claude-status' ':(exclude)CLAUDE.md' ':(exclude)pr-notes.md')

# also exclude generated files (repo decides via .gitattributes / Code-generated headers).
# Compute over the current pending set (tracked changes vs HEAD + untracked) and append.
. "$(cd "$(dirname "$(readlink -f "$0")")" && pwd)/generated-filter.sh"
mapfile -t GEXCL < <(
  { git -C "$root" diff --name-only HEAD -- . "${EXCL[@]}" 2>/dev/null
    git -C "$root" ls-files --others --exclude-standard -- . "${EXCL[@]}" 2>/dev/null; } \
    | sort -u | gen_excludes "$root" -
)
EXCL+=(${GEXCL[@]+"${GEXCL[@]}"})
# plus test-file excludes when the panel's "exclude tests" toggle is on
mapfile -t TEXCL < <(test_excludes); EXCL+=(${TEXCL[@]+"${TEXCL[@]}"})

# Absolute src/dst prefixes so the file path delta prints is the full path on disk, which makes
# it ctrl+click-resolvable in the (cwd-less) tmux diff pane. delta strips the leading 'a/'/'b/',
# leaving '/abs/root/relpath'; keeping a/ vs b/ distinct avoids git's same-name "new file"
# misrender. NPFX is for `--no-index` (untracked): git already strips the absolute "$root/$u"'s
# leading slash before applying the prefix, so 'a//' restores it after delta strips 'a/'.
PFX=(--src-prefix="a/$root/" --dst-prefix="b/$root/")
NPFX=(--src-prefix="a//" --dst-prefix="b//")

# Emit the pending diff. $1 = "color" forces git colors; otherwise raw (for piping into delta).
emit() {
  local ca=() cf=()
  [ "${1:-}" = color ] && { ca=(-c color.ui=always); cf=(--color=always); }
  git -C "$root" "${ca[@]}" diff "${cf[@]}" "${PFX[@]}" HEAD -- . "${EXCL[@]}" 2>/dev/null
  git -C "$root" ls-files --others --exclude-standard -- . "${EXCL[@]}" 2>/dev/null | while IFS= read -r u; do
    [ -n "$u" ] || continue
    git -C "$root" "${ca[@]}" diff "${cf[@]}" "${NPFX[@]}" --no-index -- /dev/null "$root/$u" 2>/dev/null
  done
  true
}

log="$root/.claude/diffs.log"
mkdir -p "$(dirname "$log")"
tmp="$(mktemp)"
{
  printf '\033[1;90mpending diffs · %s\033[0m\n' "$(date +%H:%M:%S)"
  if command -v delta >/dev/null 2>&1; then
    emit | delta --no-gitconfig --paging=never --dark --hunk-header-decoration-style=none --line-numbers \
                 --file-style 'bold yellow' --file-decoration-style 'yellow ol' \
                 --plus-style 'syntax #166e22' --plus-emph-style 'syntax #1f9b34' --minus-style 'syntax #5a1d1d' --minus-emph-style 'syntax #8a2e2e' 2>/dev/null
  else
    emit color
  fi
  true
} > "$tmp"
mv "$tmp" "$log"

[ "${1:-}" = "--seed" ] || jq -n --arg m "diff refreshed (edited ${f##*/}) — see the diff pane" '{systemMessage: $m, suppressOutput: true}'
