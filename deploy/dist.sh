#!/usr/bin/env bash
# Build the single-file static release for the VM ("desktop app" dist).
# Output: dist/rosed -- copy this ONE file to the VM, then `rosed install`.
# Uses musl so the binary has no glibc dependency (build host != VM distro).
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET="x86_64-unknown-linux-musl"

if ! rustup target list --installed 2>/dev/null | grep -q "$TARGET"; then
  echo "Adding Rust target $TARGET..."
  rustup target add "$TARGET"
fi
if ! command -v musl-gcc >/dev/null 2>&1; then
  echo "ERROR: musl-gcc not found (needed to link $TARGET)." >&2
  echo "  Fedora: sudo dnf install -y musl-gcc" >&2
  echo "  Ubuntu: sudo apt install -y musl-tools" >&2
  exit 1
fi

cargo build --release --target "$TARGET" -p rosed
mkdir -p dist
cp -f "target/$TARGET/release/rosed" dist/rosed
ls -la dist/rosed
file dist/rosed || true
echo "OK: copy dist/rosed to the VM (~/.local/bin/rosed) and run: rosed install"
