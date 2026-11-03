#!/usr/bin/env bash
# Build the Rose tarball for the VM ("desktop app" dist, no sudo anywhere).
# Output: dist/rose_<ver>_x86_64.tar.gz -- copy to VM, extract, run ./install.sh.
#
# Contents: rosed (static musl binary) + install.sh + systemd units +
# example config. The binary embeds the units + example config too, so
# `rosed install` alone can wire up user services; the loose files are
# there for inspection and for air-gapped boxes.
#
# Fresh VM (once, needs sudo for apt): ./deploy/setup.sh
# Every update after that (zero sudo): ./deploy/deploy.sh [user@host]
#   or by hand: scp tarball, tar xzf, ./install.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TARGET="x86_64-unknown-linux-musl"
VER="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
TARBALL="dist/rose_${VER}_x86_64.tar.gz"

if ! rustup target list --installed 2>/dev/null | grep -q "$TARGET"; then
  rustup target add "$TARGET"
fi
if ! command -v musl-gcc >/dev/null 2>&1; then
  echo "ERROR: musl-gcc missing. Fedora: sudo dnf install -y musl-gcc | Ubuntu: sudo apt install -y musl-tools" >&2
  exit 1
fi

echo "==> static build ($TARGET)"
cargo build --release --target "$TARGET" -p rosed

echo "==> staging $TARBALL"
rm -rf .pkg-stage
mkdir -p .pkg-stage/rose-"$VER" dist
cp -f "target/$TARGET/release/rosed" .pkg-stage/rose-"$VER"/rosed
chmod 755 .pkg-stage/rose-"$VER"/rosed
cp -f deploy/install.sh .pkg-stage/rose-"$VER"/install.sh
chmod 755 .pkg-stage/rose-"$VER"/install.sh
cp -f deploy/systemd/xvnc.service deploy/systemd/rosed.service .pkg-stage/rose-"$VER"/
cp -f config/rose.example.toml .pkg-stage/rose-"$VER"/
cat > .pkg-stage/rose-"$VER"/README.txt <<EOF
Rose v$VER - VM agent (XFCE over TigerVNC, localhost-only + SSH tunnel).
No sudo needed for updates. Fresh VM system deps (once):
  ./deploy/setup.sh   (or: sudo apt install xfce4 tigervnc-standalone-server
  dbus-x11 x11-xserver-utils ffmpeg fonts-liberation fonts-dejavu curl)

Update from this tarball (as your user, on the VM):
  tar xzf $(basename "$TARBALL")
  cd rose-$VER && ./install.sh

What install.sh does: copies rosed to ~/.local/bin, runs
'rosed install' (writes user units + example config, never overwrites),
daemon-reloads, restarts xvnc + rosed, verifies /api/health.
Lingering (one sudo, once): sudo loginctl enable-linger \$USER
EOF

tar --owner=0 --group=0 --numeric-owner -czf "$ROOT/$TARBALL" -C .pkg-stage rose-"$VER"
rm -rf .pkg-stage

echo "==> contents"
tar -tzf "$TARBALL" | sort
ls -la "$TARBALL"
