#!/usr/bin/env bash
# Publish a self-contained HTML design mockup for the fleet to preview. Run from inside a worktree:
#
#   preview <file.html>
#
# Stages the file to .wtd/state/previews/<slug>/<name>.html, which the claude-status VSCode extension
# watches: a 🖼 glyph appears on this worktree's roster row — click it to open the mockup in an
# editor-side panel. Make the HTML self-contained (inline CSS, data-URI images); it renders sandboxed
# with no network access, so external <link>/<img src=http…>/<script src> won't load.
set -euo pipefail
WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # …/.wtd
DEV="$(dirname "$WTD")"                                                   # dev root (holds worktrees/)

src="${1:-}"
[ -n "$src" ] || { echo "usage: preview <file.html>" >&2; exit 2; }
[ -f "$src" ] || { echo "preview: no such file: $src" >&2; exit 1; }

# worktree root = nearest ancestor of $PWD holding a .claude-status sentinel
wt="$PWD"
while [ "$wt" != "/" ] && [ ! -f "$wt/.claude-status" ]; do wt="$(dirname "$wt")"; done
[ -f "$wt/.claude-status" ] || { echo "preview: not inside a worktree (no .claude-status above $PWD)" >&2; exit 1; }

# slug/name = worktree path relative to <dev>/worktrees (name may contain '/', e.g. feat/x)
rel="${wt#"$DEV"/worktrees/}"
[ "$rel" != "$wt" ] || { echo "preview: $wt is not under $DEV/worktrees" >&2; exit 1; }
slug="${rel%%/*}"
name="${rel#*/}"

dest="$WTD/state/previews/$slug/$name.html"
mkdir -p "$(dirname "$dest")"
cp -f "$src" "$dest"
echo "preview staged for $slug/$name → click the 🖼 on its roster row to open"
