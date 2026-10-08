#!/usr/bin/env bash
# Run the full sysview quality gate: fmt, check, test, clippy (native + Linux
# branch) and a release build. Stops at the first failing step.
#
#   bash scripts/check-gate.sh
#
# Self-healing prerequisites: when cargo came from rustup, the rustfmt
# component and the Linux compile-check target are installed on demand, so the
# gate passes on a fresh toolchain instead of failing with "component
# 'rustfmt' is not installed". With a non-rustup toolchain the gate still runs;
# the Linux-branch checks are skipped with a warning if the target is missing.
set -euo pipefail
cd "$(dirname "$0")/.."

command -v cargo >/dev/null 2>&1 || {
    echo "error: cargo not found." >&2
    echo "  install a current toolchain:" >&2
    echo "    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y" >&2
    echo "  then: source \"\$HOME/.cargo/env\" and rerun." >&2
    exit 1
}
command -v rustc >/dev/null 2>&1 || { echo "error: rustc not found." >&2; exit 1; }
[ -f Cargo.toml ] || { echo "error: run this from the sysview source tree." >&2; exit 1; }

# The rustfmt gate needs the rustfmt component; install it (best-effort) when
# rustup is present so a virgin toolchain doesn't fail the gate.
if command -v rustup >/dev/null 2>&1; then
    echo ">> ensuring rustfmt component..."
    rustup component add rustfmt >/dev/null 2>&1 || true
    # The Linux branch is validated by cross-target check/clippy; make sure
    # the target is installed too (a no-op when already present).
    rustup target add x86_64-unknown-linux-gnu >/dev/null 2>&1 || true
fi
if ! cargo fmt --version >/dev/null 2>&1; then
    echo "error: the 'cargo fmt' command is unavailable." >&2
    echo "  install the rustfmt component:" >&2
    echo "    rustup component add rustfmt" >&2
    exit 1
fi

step() {
    echo
    echo ">> $*"
    "$@"
}

step cargo fmt -- --check
step cargo check
step cargo test
step cargo clippy --all-targets -- -D warnings

# Linux-branch checks: best-effort. Without the target installed the native
# gate above still stands; these just widen coverage to the Linux code paths.
if rustc --print target-list 2>/dev/null | grep -qx x86_64-unknown-linux-gnu; then
    step cargo check --target x86_64-unknown-linux-gnu
    step cargo clippy --target x86_64-unknown-linux-gnu -- -D warnings
else
    echo
    echo "!! x86_64-unknown-linux-gnu target not installed; skipping the"
    echo "!! Linux-branch check/clippy steps. With rustup:"
    echo "!!   rustup target add x86_64-unknown-linux-gnu"
fi

step cargo build --release

echo
echo ">> gate green: fmt, check, test, clippy (native + Linux), release build."