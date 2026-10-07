# Include .cargo/config.toml so the remote build uses the Qt-compatible linker.
build:
    cargo remote -h -- build --release --locked

test:
    cargo remote -h -- test --locked

clippy:
    cargo remote -h -- clippy --locked --all-targets

fmt:
    cargo fmt --check

get-version:
    python3 -c 'import re; print(re.search(r'^version = "([^"]+)"', open("Cargo.toml").read(), re.M)[1])'

portable:
    ./scripts/build-portable.sh
