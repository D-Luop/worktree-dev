#!/usr/bin/env bash
# Publish a self-contained HTML design mockup for the fleet to preview. Run from inside a worktree:
#
#   preview <file.html> [label]
#
# Stages the file to .wtd/state/previews/<slug>/<name>/<label>.html. A worktree can hold MANY previews
# at once — the claude-status VSCode extension shows one button per <label> at the top of the preview
# panel to switch between them. <label> defaults to the file's basename; the living plan uses `plan`.
# Make the HTML self-contained (inline CSS, data-URI images); it renders sandboxed with no network, so
# external <link>/<img src=http…>/<script src> won't load.
set -euo pipefail
WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # …/.wtd
DEV="$(dirname "$WTD")"                                                   # dev root (holds worktrees/)

src="${1:-}"
[ -n "$src" ] || { echo "usage: preview <file.html> [label]" >&2; exit 2; }
[ -f "$src" ] || { echo "preview: no such file: $src" >&2; exit 1; }

# label = switcher-button name. Explicit 2nd arg, else the file basename (sans extension). Sanitize to
# a filesystem-safe token so it's a clean single path segment.
if [ -n "${2:-}" ]; then label="$2"; else label="$(basename "$src")"; label="${label%.*}"; fi
label="$(printf '%s' "$label" | tr -c 'A-Za-z0-9._-' '-')"
[ -n "$label" ] || label="preview"

# worktree root = nearest ancestor of $PWD holding a .claude-status sentinel
wt="$PWD"
while [ "$wt" != "/" ] && [ ! -f "$wt/.claude-status" ]; do wt="$(dirname "$wt")"; done
[ -f "$wt/.claude-status" ] || { echo "preview: not inside a worktree (no .claude-status above $PWD)" >&2; exit 1; }

# slug/name = worktree path relative to <dev>/worktrees (name may contain '/', e.g. feat/x)
rel="${wt#"$DEV"/worktrees/}"
[ "$rel" != "$wt" ] || { echo "preview: $wt is not under $DEV/worktrees" >&2; exit 1; }
slug="${rel%%/*}"
name="${rel#*/}"

dest="$WTD/state/previews/$slug/$name/$label.html"
mkdir -p "$(dirname "$dest")"
cp -f "$src" "$dest"
echo "preview '$label' staged for $slug/$name → open the panel (🖼) and pick the '$label' tab"
