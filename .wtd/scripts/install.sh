#!/usr/bin/env bash
# Install from a source checkout: build wtd.exe, then `wtd setup` does the rest (shims, hooks, VS Code
# extension, MCP, tray). Extra args go to it: --repair, --uninstall [--keep-data], --skip <steps>.
# Not Windows (tmux backend): the bash installer, install-unix.sh.
set -euo pipefail
WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # <dev>/.wtd
BASE="$(dirname "$WTD")"
# shellcheck source=platform-lib.sh
. "$WTD/scripts/platform-lib.sh"
[ "$(wtd_os)" = windows ] || exec "$WTD/scripts/install-unix.sh" "$@"

exe="$WTD/bin/wtd.exe"
if command -v cargo >/dev/null 2>&1; then
  echo "==> cargo build --release"
  (cd "$BASE/wtd" && cargo build --release -q)
  exe="$BASE/wtd/target/release/wtd.exe"   # setup copies it (and wtd-tray.exe) into .wtd/bin
elif [ ! -x "$exe" ]; then
  echo "cargo not found and no wtd.exe yet: install Rust (winget install Rustlang.Rustup + VS C++ Build Tools)," >&2
  echo "or install a release build (WorkTreeDev-Setup.exe)." >&2
  exit 1
fi
WTD_DEV_SOURCE=1 exec "$exe" setup --dev-root "$(cygpath -w "$BASE")" "$@"
