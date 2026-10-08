#!/usr/bin/env bash
# Build sysview for Linux and print the resulting binary path.
#
#   bash scripts/build-linux.sh [--static] [--install]
#
#   --static    fully static musl binary: no glibc dependency, runs on any
#               distro (Debian, Ubuntu, RHEL, Alpine, ...). Requires the musl
#               Rust target (rustup target add x86_64-unknown-linux-musl).
#   --install   copy the result to /usr/local/bin/sysview (needs root)
#
# Requires cargo with a recent toolchain (the locked sysinfo needs rustc
# >= 1.95; Debian's stock compiler is behind — install via rustup):
#   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
set -euo pipefail
cd "$(dirname "$0")/.."

STATIC=0
INSTALL=0
for arg in "$@"; do
    case "$arg" in
        --static) STATIC=1 ;;
        --install) INSTALL=1 ;;
        -h | --help)
            sed -n '1,14p' "$0"
            exit 0
            ;;
        *)
            echo "unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

command -v cargo >/dev/null 2>&1 || {
    echo "error: cargo not found." >&2
    echo "  install a current toolchain:" >&2
    echo "    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y" >&2
    echo "  then: source \"\$HOME/.cargo/env\" and rerun." >&2
    exit 1
}
command -v rustc >/dev/null 2>&1 || { echo "error: rustc not found." >&2; exit 1; }

if [ ! -f Cargo.toml ] || [ ! -f Cargo.lock ]; then
    echo "error: run this script from inside the sysview source tree." >&2
    exit 1
fi

mkdir -p dist

if [ "$STATIC" = 1 ]; then
    TARGET=x86_64-unknown-linux-musl
    if command -v rustup >/dev/null 2>&1; then
        rustup target add "$TARGET" >/dev/null
    elif ! rustc --print target-list 2>/dev/null | grep -qx "$TARGET"; then
        echo "error: musl target not installed. With rustup:" >&2
        echo "  rustup target add $TARGET" >&2
        exit 1
    fi
    echo ">> building static $TARGET (release)..."
    cargo build --release --locked --target "$TARGET"
    BIN="target/$TARGET/release/sysview"
    OUT="dist/sysview-linux-$(uname -m)-musl"
else
    echo ">> building glibc-linked release..."
    cargo build --release --locked
    BIN="target/release/sysview"
    OUT="dist/sysview-linux-$(uname -m)"
fi

cp "$BIN" "$OUT"
chmod 0755 "$OUT"
ls -lh "$OUT"
"$OUT" --version

if [ "$INSTALL" = 1 ]; then
    install -m 0755 "$OUT" /usr/local/bin/sysview
    echo ">> installed /usr/local/bin/sysview"
fi
echo ">> done: $OUT"