#!/usr/bin/env bash
# Smoke-test a portable bundle: extraction, cache reuse, library closure,
# and a headless GUI start (offscreen platform) that must load the QML.
set -euo pipefail

PROJECT_ROOT="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
# shellcheck source=scripts/lib/sanitize-host-env.sh
source "$PROJECT_ROOT/scripts/lib/sanitize-host-env.sh"
sanitize_host_env

APP=celeste
RUN_PATH="${1:-}"
[[ -n "$RUN_PATH" ]] || { echo "Usage: $0 <path/to/*-portable.run>" >&2; exit 2; }
[[ -x "$RUN_PATH" ]] || { echo "error: not an executable file: $RUN_PATH" >&2; exit 1; }
grep -qa '^__PAYLOAD_FOLLOWS__$' "$RUN_PATH" || { echo "error: payload marker missing" >&2; exit 1; }
ABS_RUN="$(cd "$(dirname "$RUN_PATH")" && pwd -P)/$(basename "$RUN_PATH")"

# Isolated XDG dirs: clean extraction, no clash with a running instance.
SANDBOX="$(mktemp -d -t "$APP-smoke-XXXXXX")"
trap 'rm -rf "$SANDBOX"' EXIT
export XDG_CACHE_HOME="$SANDBOX/cache" XDG_CONFIG_HOME="$SANDBOX/config"
export XDG_DATA_HOME="$SANDBOX/data" XDG_RUNTIME_DIR="$SANDBOX/runtime"
mkdir -p "$XDG_RUNTIME_DIR" && chmod 0700 "$XDG_RUNTIME_DIR"

echo "--- first launch (clean extraction) ---"
"$ABS_RUN" --version
ROOT="$("$ABS_RUN" --portable-root)"
[[ -x "$ROOT/bin/$APP" && -f "$ROOT/.portable-marker" ]] || { echo "error: bad extraction at $ROOT" >&2; exit 1; }

echo "--- second launch (must reuse the cache) ---"
MTIME="$(stat -c %Y "$ROOT/.portable-marker")"
sleep 1.1
"$ABS_RUN" --version >/dev/null
[[ "$(stat -c %Y "$ROOT/.portable-marker")" == "$MTIME" ]] || { echo "error: re-extracted on cached launch" >&2; exit 1; }

echo "--- library closure ---"
if env -u LD_LIBRARY_PATH ldd "$ROOT/bin/$APP" | grep "not found"; then
  echo "error: extracted binary has missing libraries" >&2
  exit 1
fi

echo "--- headless GUI start ---"
LOG="$SANDBOX/gui.log"
# The watchdog exits 2 within 10 s if QML fails; success = bootstrap log line.
QT_QPA_PLATFORM=offscreen CELESTE_LOG=info timeout 8 "$ABS_RUN" >"$LOG" 2>&1 || true
if ! grep -q "QML bootstrap complete" "$LOG"; then
  cat "$LOG" >&2
  echo "error: QML UI did not load" >&2
  exit 1
fi

echo "=== smoke-portable passed ==="
