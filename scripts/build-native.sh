#!/usr/bin/env bash
# Remote build: copy back and stage the binary, desktop entry and icon.
#
# Usage:
#   ./scripts/build-native.sh [--debug|--release] [--stage <absolute-path>] [--features <feats>]
set -euo pipefail

PROJECT_ROOT="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
# shellcheck source=scripts/lib/sanitize-host-env.sh
source "$PROJECT_ROOT/scripts/lib/sanitize-host-env.sh"
sanitize_host_env

APP=celestial
BUILD_TYPE="debug"
STAGE=""
FEATURES=""

usage() {
  cat <<EOF
Usage: $0 [--debug|--release] [--stage <absolute-path>] [--features <feats>]

  --debug            Debug build (default)
  --release          Release build
  --stage <path>     Absolute stage directory (default: \$PROJECT_ROOT/build/stage)
  --features <feats> Extra cargo features
EOF
}

while (($#)); do
  case "$1" in
    --debug) BUILD_TYPE="debug"; shift ;;
    --release) BUILD_TYPE="release"; shift ;;
    --stage) STAGE="${2:?--stage needs a path}"; shift 2 ;;
    --stage=*) STAGE="${1#*=}"; shift ;;
    --features) FEATURES="${2:?--features needs a value}"; shift 2 ;;
    --features=*) FEATURES="${1#*=}"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -z "$STAGE" ]] && STAGE="$PROJECT_ROOT/build/stage"
[[ "$STAGE" == /* ]] || { echo "error: --stage must be absolute: $STAGE" >&2; exit 2; }

echo "=== build-native ($BUILD_TYPE) -> $STAGE ==="
CARGO_ARGS=(build --locked)
[[ "$BUILD_TYPE" == "release" ]] && CARGO_ARGS+=(--release)
[[ -n "$FEATURES" ]] && CARGO_ARGS+=(--features "$FEATURES")
(cd "$PROJECT_ROOT" && cargo remote -h -- "${CARGO_ARGS[@]}")

# cargo remote copies back only the finished executable.
TARGET_DIR="$PROJECT_ROOT/target"
BIN="$TARGET_DIR/$BUILD_TYPE/$APP"
[[ -x "$BIN" ]] || { echo "error: binary missing: $BIN" >&2; exit 1; }

rm -rf "$STAGE"
mkdir -p "$STAGE/bin" "$STAGE/share/applications" \
         "$STAGE/share/icons/hicolor/scalable/apps" "$STAGE/share/doc/$APP"
install -m 0755 "$BIN" "$STAGE/bin/$APP"
install -m 0644 "$PROJECT_ROOT/assets/com.unboundmark.Celestial.desktop" "$STAGE/share/applications/com.unboundmark.Celestial.desktop"
install -m 0644 "$PROJECT_ROOT/assets/com.unboundmark.Celestial-regular.svg" "$STAGE/share/icons/hicolor/scalable/apps/com.unboundmark.Celestial.svg"
for f in LICENSE THIRD_PARTY_LICENSES.md; do
  [[ -f "$PROJECT_ROOT/$f" ]] && install -m 0644 "$PROJECT_ROOT/$f" "$STAGE/share/doc/$APP/$f"
done

LDD_OUT="$(env -u LD_LIBRARY_PATH ldd "$STAGE/bin/$APP" 2>&1 || true)"
if grep -q "not found" <<<"$LDD_OUT"; then
  echo "error: missing shared libraries:" >&2
  grep "not found" <<<"$LDD_OUT" >&2
  exit 1
fi
echo "=== build-native complete: $STAGE/bin/$APP ==="
