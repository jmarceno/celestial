#!/usr/bin/env bash
# Turn this template into a new app: rewrites every `celeste` placeholder.
#
# Usage:
#   ./scripts/new-app.sh <slug> "<Display Name>"
#   ./scripts/new-app.sh notes "Quick Notes"
#
# <slug> becomes the crate, binary, QML module (app.<slug>), config dir,
# desktop-entry and icon name. It must be a valid Rust identifier in
# lowercase: [a-z][a-z0-9_]*. Run once, on a fresh copy, then commit.
set -euo pipefail

PROJECT_ROOT="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
SLUG="${1:-}"
DISPLAY_NAME="${2:-}"
[[ "$SLUG" =~ ^[a-z][a-z0-9_]*$ ]] || { echo "error: slug must match [a-z][a-z0-9_]*" >&2; exit 2; }
[[ -n "$DISPLAY_NAME" && "$DISPLAY_NAME" != *[\"\\/\&\|]* ]] || {
  echo "error: give a display name without quotes, slashes, & or |" >&2; exit 2;
}
UPPER="$(tr '[:lower:]' '[:upper:]' <<<"$SLUG")"

cd "$PROJECT_ROOT"
grep -rqI 'celeste' --exclude-dir=.git --exclude-dir=target --exclude-dir=build --exclude=new-app.sh . \
  || { echo "error: no celeste placeholders left (already renamed?)" >&2; exit 1; }

# Order matters: CELESTE before Celeste before celeste.
grep -rlI -e celeste -e Celeste -e CELESTE \
  --exclude-dir=.git --exclude-dir=target --exclude-dir=build --exclude=new-app.sh . \
  | while IFS= read -r f; do
      sed -i -e "s/CELESTE/$UPPER/g" -e "s/Celeste/$DISPLAY_NAME/g" -e "s/celeste/$SLUG/g" "$f"
    done

for f in assets/celeste.svg packaging/desktop/celeste.desktop; do
  [[ -f "$f" ]] || continue
  git mv -f "$f" "${f//celeste/$SLUG}" 2>/dev/null || mv "$f" "${f//celeste/$SLUG}"
done

echo "Renamed to $SLUG (\"$DISPLAY_NAME\")."
echo "Next: replace assets/$SLUG.svg, fill in README.md, then cargo build && git commit."
