#!/usr/bin/env bash
# Build remotely, then expose the native executable with a .run suffix.
set -euo pipefail
PROJECT_ROOT="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$PROJECT_ROOT"
cargo remote-release
install -m 0755 target/release/celestial target/release/celestial.run
