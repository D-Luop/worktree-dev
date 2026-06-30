#!/usr/bin/env bash
set -euo pipefail
WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # ~/dev/.wtd
BASE="$(dirname "$WTD")"                                                  # ~/dev
BASHRC="$HOME/.bashrc"
SETTINGS="$HOME/.claude/settings.json"
HOOKS_TMPL="$WTD/templates/claude-hooks.json"
VSIX="$WTD/templates/vscode-claude-status/claude-status-0.0.52.vsix"
# Shipped templates carry __DEV__/__USER__/__DISTRO__ placeholders so they're machine-agnostic;
# render them to this host's real values at install time.
WTD_USER="$(id -un)"
WTD_DISTRO="${WSL_DISTRO_NAME:-Ubuntu}"
wtd_render() { sed -e "s#__DEV__#$BASE#g" -e "s#__USER__#$WTD_USER#g" -e "s#__DISTRO__#$WTD_DISTRO#g" "$@"; }
# platform detection: tmux backend on Linux/WSL/mac, VSCode-terminal backend on native Windows.
# shellcheck source=platform-lib.sh
. "$WTD/scripts/platform-lib.sh"
OS="$(wtd_os)"; BACKEND="$(wtd_session_backend)"
echo "==> platform: $OS (session backend: $BACKEND)"
mkdir -p "$WTD/state/sessions"   # the registry the VSCode backend uses to track live sessions

echo "==> A. install 'agent' + 'add-repo' + 'ref' commands into ~/.local/bin"
# Real executables on PATH (exported -> inherited by every child shell). A bashrc
# *function* breaks under exported guards like __BASHRC_LOADED that make child shells
# (tmux, VSCode terminals, subshells) skip the bashrc body before defining functions.
mkdir -p "$HOME/.local/bin"
# On Unix, symlink. On native Windows, Git-Bash symlinks need Developer Mode/admin, so write a tiny
# exec-shim instead (always works, and still dispatches to the live script so edits take effect).
wtd_link() {
  local tgt="$WTD/scripts/$1" dst="$HOME/.local/bin/$2"
  case "$OS" in
    windows) printf '#!/usr/bin/env bash\nexec %q "$@"\n' "$tgt" > "$dst"; chmod +x "$dst" 2>/dev/null || true ;;
    *)       ln -sf "$tgt" "$dst" ;;
  esac
  echo "    $([ "$OS" = windows ] && echo shimmed || echo linked) ~/.local/bin/$2 -> $tgt"
}
wtd_link agent.sh     agent
wtd_link add-repo.sh  add-repo
wtd_link ref.sh       ref
wtd_link review.sh    review
wtd_link archive.sh   archive
wtd_link close.sh     close
wtd_link wt-review.sh wt-review
wtd_link ask.sh       ask
wtd_link account.sh   account
wtd_link ship.sh      ship
wtd_link assistant.sh assistant
# migrate: strip the obsolete bashrc function block if a previous install added it
if grep -q '>>> agent worktree launcher >>>' "$BASHRC" 2>/dev/null; then
  tmp=$(mktemp)
  sed '/# >>> agent worktree launcher >>>/,/# <<< agent worktree launcher <<</d' "$BASHRC" > "$tmp" && mv "$tmp" "$BASHRC"
  echo "    removed obsolete ~/.bashrc agent() function"
fi
if ! printf '%s' "$PATH" | tr ':' '\n' | grep -qxF "$HOME/.local/bin"; then
  echo "    NOTE: ~/.local/bin not on PATH in this shell; ~/.profile adds it for login shells."
fi
# bash tab-completion for agent/archive/review (slugs + branch/worktree names). bash-completion
# auto-sources ~/.local/share/bash-completion/completions/<cmd> on first <cmd><TAB> in a new shell.
CDIR="$HOME/.local/share/bash-completion/completions"; mkdir -p "$CDIR"
for c in agent archive review ask account; do ln -sf "$WTD/completions/wtd-completion.bash" "$CDIR/$c"; done
echo "    installed bash completion for agent/archive/review/ask (open a new shell to use)"

echo "==> B. dependency check"
deps="jq git code node"
[ "$BACKEND" = tmux ] && deps="$deps tmux"   # tmux only on the tmux backend (not native Windows)
# git-delta is optional (diffs fall back to git's colors) but recommended.
missing=()
for d in $deps; do
  if command -v "$d" >/dev/null 2>&1; then
    echo "    ok: $d"
  else
    echo "    MISSING: $d"
    missing+=("$d")
  fi
done
command -v delta >/dev/null 2>&1 && echo "    ok: delta (optional)" || echo "    optional: delta (git-delta) — nicer diffs"
if [ "${#missing[@]}" -gt 0 ]; then
  case "$OS" in
    windows) echo "    install missing deps on Windows:  winget install <name>   (or: scoop install ${missing[*]})" ;;
    mac)     echo "    install missing deps:  brew install ${missing[*]}" ;;
    *)       echo "    install missing deps (may need sudo):  sudo apt-get install -y ${missing[*]}" ;;
  esac
fi

echo "==> C. merge Claude status hooks into ~/.claude/settings.json"
mkdir -p "$(dirname "$SETTINGS")"
HOOKS_RENDERED="$(mktemp)"; wtd_render "$HOOKS_TMPL" > "$HOOKS_RENDERED"   # __DEV__/__DISTRO__ -> real
if [ -f "$SETTINGS" ]; then
  cp "$SETTINGS" "$SETTINGS.bak"
  echo "    backed up to $SETTINGS.bak"
  tmp=$(mktemp)
  jq -s '.[0] as $cur | .[1] as $tmpl | $cur + {hooks: (($cur.hooks // {}) + $tmpl.hooks)}' \
     "$SETTINGS" "$HOOKS_RENDERED" > "$tmp"
  mv "$tmp" "$SETTINGS"
  echo "    merged (existing non-status hooks preserved)"
else
  cp "$HOOKS_RENDERED" "$SETTINGS"
  echo "    created $SETTINGS from template"
fi
rm -f "$HOOKS_RENDERED"

echo "==> D. install VSCode claude-status extension"
# Pin the dev base for the extension. The .vsix ships prebuilt (can't be __DEV__-rendered), so the
# extension reads this file to find the real root regardless of which folder VSCode has open. Store a
# NATIVE path (cygpath -w on Windows) — the extension resolves it with node's fs, not an MSYS path.
DEV_ROOT_PIN="$HOME/.config/wtd/dev-root"
mkdir -p "$(dirname "$DEV_ROOT_PIN")"
if command -v cygpath >/dev/null 2>&1; then cygpath -w "$BASE" > "$DEV_ROOT_PIN"; else printf '%s\n' "$BASE" > "$DEV_ROOT_PIN"; fi
echo "    pinned dev root -> $(cat "$DEV_ROOT_PIN")  ($DEV_ROOT_PIN)"
if ! command -v code >/dev/null 2>&1; then
  echo "    SKIPPED: 'code' not on PATH"
elif code --install-extension "$VSIX" --force 2>/dev/null; then
  echo "    installed via --force"
else
  echo "    WARN: 'code' CLI could not reach a live VSCode server (stale IPC socket)."
  echo "          Run from an integrated VSCode terminal, or it may already be installed:"
  echo "          code --install-extension \"$VSIX\" --force"
fi

echo "==> D2. VSCode terminal tab title + bell prefs"
# On the tmux backend, tmux pushes a title (set-titles, step H) using @wt_label; on Windows the
# extension names each terminal directly. Either way VSCode displays it when the tab title uses
# ${sequence}. Write it to this OS's VSCode user/machine settings. NOTE: reload the window to apply.
MSET="$(wtd_vscode_settings_path)"
mkdir -p "$(dirname "$MSET")"; [ -f "$MSET" ] || echo '{}' > "$MSET"
tmp=$(mktemp)
jq '. + {"terminal.integrated.tabs.title":"${sequence}","terminal.integrated.tabs.description":"${cwdFolder}","terminal.integrated.enableVisualBell":true,"accessibility.signals.terminalBell":{"sound":"off"},"terminal.integrated.confirmOnKill":"never","workbench.editor.showTabs":"none"}' "$MSET" > "$tmp" && mv "$tmp" "$MSET"
echo "    set tab title=\${sequence} + visual terminal bell (sound off) + hid editor tabs (switch via the roster) in $MSET"

echo "==> E. install claude-pace statusline"
# Vendored from github.com/Astro-Han/claude-pace (manual method: no npm / no interactive
# /plugin). Single bash+jq file. Occupies the single statusLine slot.
install -m 0755 "$WTD/templates/claude-pace.sh" "$HOME/.claude/statusline.sh"
echo "    installed $HOME/.claude/statusline.sh"
tmp=$(mktemp)
jq '.statusLine = {type: "command", command: "~/.claude/statusline.sh"}' "$SETTINGS" > "$tmp"
mv "$tmp" "$SETTINGS"
echo "    set statusLine -> claude-pace"

echo "==> F. strip Claude attribution from commits + PRs"
# No agent co-author / "Generated with Claude Code" lines, fleet-wide.
tmp=$(mktemp)
jq '.attribution = ((.attribution // {}) + {commit: "", pr: ""})' "$SETTINGS" > "$tmp"
mv "$tmp" "$SETTINGS"
echo "    set attribution.commit=\"\" and attribution.pr=\"\""

echo "==> G. grant fleet-wide READ access to the reference checkouts (~/dev/refs)"
# Reference checkouts (managed by `ref add` / `agent <slug> <name> <ref>...`) are GLOBAL context:
# every worktree can read any of them, granted once here instead of per-worktree. Whole-tree
# additionalDirectories covers refs added later; Edit/Write are denied to keep them read-only.
REFROOT="$BASE/refs"
mkdir -p "$REFROOT"
tmp=$(mktemp)
jq --arg root "$REFROOT" '
  .permissions = (.permissions // {})
  | .permissions.additionalDirectories = (((.permissions.additionalDirectories // []) + [$root]) | unique)
  | .permissions.deny =
      (((.permissions.deny // []) + ["Edit(\($root)/**)", "Write(\($root)/**)"]) | unique)
' "$SETTINGS" > "$tmp"
mv "$tmp" "$SETTINGS"
echo "    granted read on $REFROOT (Edit/Write denied) in $SETTINGS"

# Section H configures tmux (mouse, pane-lock, titles, bell, commit-click). Only the tmux backend
# needs it — native Windows runs sessions in VSCode terminals, so skip the whole block there.
if [ "$BACKEND" != tmux ]; then
  echo "==> H. tmux config — SKIPPED (backend: $BACKEND; sessions run in VSCode integrated terminals)"
else
echo "==> H. tmux mouse scrollback (so the wheel scrolls history, not the app)"
TMUXCONF="$HOME/.tmux.conf"
touch "$TMUXCONF"
if grep -qE '^[[:space:]]*set(-option)?[[:space:]]+-g[[:space:]]+mouse[[:space:]]+on' "$TMUXCONF"; then
  echo "    already set in $TMUXCONF"
else
  { [ -s "$TMUXCONF" ] && [ -n "$(tail -c1 "$TMUXCONF")" ] && printf '\n'
    printf '# scroll wheel scrolls pane scrollback instead of sending arrows to the app\n'
    printf 'set -g mouse on\n'
    printf 'set -g history-limit 50000\n'; } >> "$TMUXCONF"
  echo "    added mouse on + history-limit to $TMUXCONF"
fi
tmux set -g mouse on 2>/dev/null && echo "    applied to running tmux server" || true
# Mouse border-drag RESIZE stays enabled (default) — it's a deliberate, useful gesture. We only
# disable the keyboard shortcuts + menus that REARRANGE which pane holds what (accidental swap (`{`
# `}`), rotate (C-o / M-o), break-pane (`!`), relayout (Space / E / M-1..M-5), and the `>` context
# menu). Selection/scroll/resize are untouched.
if ! grep -q 'wtd: lock pane arrangement' "$TMUXCONF"; then
  { printf '# wtd: lock pane arrangement — disable accidental pane swap/rotate/break/relayout keys\n'
    # quote every key: '{' '}' '>' are command-block/parse delimiters in tmux 3.x, so unquoted
    # `unbind {` aborts the whole config load (mouse + click bindings then never apply). Quoting is
    # harmless for the others and makes the key literal.
    for k in '{' '}' 'C-o' 'M-o' '!' 'Space' 'E' '>' 'M-1' 'M-2' 'M-3' 'M-4' 'M-5'; do printf "unbind '%s'\n" "$k"; done
  } >> "$TMUXCONF"
  echo "    disabled pane-rearrange keybindings in $TMUXCONF"
fi
for k in '{' '}' 'C-o' 'M-o' '!' 'Space' 'E' '>' 'M-1' 'M-2' 'M-3' 'M-4' 'M-5'; do tmux unbind "$k" 2>/dev/null || true; done
# and disable the right-click context menus — they include Swap entries that let panes get
# rearranged by accident; right-click then falls through to the terminal (paste).
RCBINDS="MouseDown3Pane MouseDown3Status MouseDown3StatusLeft MouseDown3StatusRight M-MouseDown3Pane M-MouseDown3Status M-MouseDown3StatusLeft"
if ! grep -q 'wtd: disable right-click menus' "$TMUXCONF"; then
  { printf '# wtd: disable right-click menus (they let panes get swapped by accident)\n'
    for b in $RCBINDS; do printf 'unbind -n %s\n' "$b"; done
  } >> "$TMUXCONF"
  echo "    disabled right-click menus in $TMUXCONF"
fi
for b in $RCBINDS; do tmux unbind -n "$b" 2>/dev/null || true; done
# terminal tab title: let tmux push a title to the outer terminal (VSCode), using the worktree
# label agent.sh stamps on each session (@wt_label), falling back to the session name. VSCode only
# shows it when terminal.integrated.tabs.title includes ${sequence} (set in .vscode/settings.json).
TITLESTR='#{?#{@wt_label},#{@wt_label},#S}'   # worktree label only — NO status glyph/dot in the tab
if ! grep -q 'set-titles-string' "$TMUXCONF"; then
  { [ -s "$TMUXCONF" ] && [ -n "$(tail -c1 "$TMUXCONF")" ] && printf '\n'
    printf '# show the worktree label (set by agent.sh) as the outer terminal/VSCode tab title\n'
    printf 'set -g set-titles on\n'
    printf "set -g set-titles-string '%s'\n" "$TITLESTR"; } >> "$TMUXCONF"
  echo "    added set-titles to $TMUXCONF"
else
  # migrate an older set-titles-string (which prefixed the @wt_status dot) to the dot-free one.
  # NB: the title contains '#', so use '|' as the sed delimiter, not '#'.
  sed -i "s|^set -g set-titles-string .*|set -g set-titles-string '$TITLESTR'|" "$TMUXCONF"
fi
tmux set -g set-titles on 2>/dev/null || true
tmux set -g set-titles-string "$TITLESTR" 2>/dev/null && echo "    applied set-titles to running tmux server" || true
# pass pane bells through to the VSCode terminal (visual "unread" tab badge on turn-end). visual-bell
# off = bell is forwarded to the client terminal (not swallowed by tmux); bell-action any flags any
# window; monitor-bell on so panes are watched. The BEL itself is emitted by wt-status.sh on Stop.
if ! grep -q 'bell-action' "$TMUXCONF"; then
  { printf 'set -g visual-bell off\n'; printf 'set -g bell-action any\n'; printf 'setw -g monitor-bell on\n'; } >> "$TMUXCONF"
  echo "    added bell pass-through to $TMUXCONF"
fi
tmux set -g visual-bell off 2>/dev/null || true
tmux set -g bell-action any 2>/dev/null || true
tmux setw -g monitor-bell on 2>/dev/null || true
# double-click-a-SHA-to-diff-popup binding: render the placeholder snippet to real paths, then
# source THAT from ~/.tmux.conf (idempotent). Keeping the rendered copy out of the shipped tree.
CLICKSRC="$WTD/templates/tmux-commit-click.conf"      # placeholder template (__DEV__)
CLICKCONF="$HOME/.config/wtd/tmux-commit-click.conf"  # rendered, real paths
mkdir -p "$(dirname "$CLICKCONF")"
wtd_render "$CLICKSRC" > "$CLICKCONF"
sed -i "\#source-file $CLICKSRC#d" "$TMUXCONF" 2>/dev/null || true   # migrate any old direct-template source
if ! grep -qF "source-file $CLICKCONF" "$TMUXCONF" 2>/dev/null; then
  { [ -s "$TMUXCONF" ] && [ -n "$(tail -c1 "$TMUXCONF")" ] && printf '\n'
    printf 'source-file %s\n' "$CLICKCONF"; } >> "$TMUXCONF"
  echo "    added 'source-file $CLICKCONF' to $TMUXCONF"
fi
tmux source-file "$CLICKCONF" 2>/dev/null && echo "    loaded commit-click binding into running server" || true
fi   # end Section H (tmux backend only)

echo "==> I. commit-msg attribution stripper on all registered bares"
# Belt-and-suspenders with attribution="": deterministically strip any Claude/AI lines from
# commit messages, repo-side, so commits are never tied to Claude regardless of the model.
if [ -f "$WTD/repos.tsv" ]; then
  while IFS=$'\t' read -r slug url; do
    case "$slug" in ''|'#'*) continue;; esac
    bare="$BASE/repos/$slug/.bare"
    [ -d "$bare" ] || continue
    if [ ! -e "$bare/hooks/commit-msg" ] || [ -L "$bare/hooks/commit-msg" ]; then
      mkdir -p "$bare/hooks"
      ln -sf "$WTD/hooks/strip-claude-attribution.sh" "$bare/hooks/commit-msg"
      echo "    [$slug] commit-msg hook linked"
    else
      echo "    [$slug] existing commit-msg hook left intact"
    fi
  done < "$WTD/repos.tsv"
fi

echo "==> done"
