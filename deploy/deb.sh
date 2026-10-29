#!/usr/bin/env bash
# Build the Rose .deb for the VM ("desktop app" dist).
# Output: dist/rose_<ver>_amd64.deb -- copy to VM, `sudo apt install ./rose*.deb`.
# Assembles the .deb with plain ar/tar (no dpkg needed on the build host).
# The .deb holds the static musl binary + user units + example config;
# per-user wiring (enable units, config, lingering) is `rosed install`.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET="x86_64-unknown-linux-musl"
VER="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
DEB="dist/rose_${VER}_amd64.deb"

if ! rustup target list --installed 2>/dev/null | grep -q "$TARGET"; then
  rustup target add "$TARGET"
fi
if ! command -v musl-gcc >/dev/null 2>&1; then
  echo "ERROR: musl-gcc missing. Fedora: sudo dnf install -y musl-gcc | Ubuntu: sudo apt install -y musl-tools" >&2
  exit 1
fi

echo "==> static build ($TARGET)"
cargo build --release --target "$TARGET" -p rosed

echo "==> staging $DEB"
rm -rf .pkg-stage dist
mkdir -p .pkg-stage/DEBIAN \
  .pkg-stage/usr/bin \
  .pkg-stage/usr/lib/systemd/user \
  .pkg-stage/usr/share/doc/rose \
  dist

cp -f "target/$TARGET/release/rosed" .pkg-stage/usr/bin/rosed
chmod 755 .pkg-stage/usr/bin/rosed
cp -f deploy/systemd/xvnc.service deploy/systemd/rosed.service deploy/systemd/grounding.service .pkg-stage/usr/lib/systemd/user/
chmod 644 .pkg-stage/usr/lib/systemd/user/*.service
cp -f config/rose.example.toml .pkg-stage/usr/share/doc/rose/
# Lite SoM grounding sidecar (no ML weights): unit runs it from here.
mkdir -p .pkg-stage/usr/share/rose/grounding-worker
cp -f grounding-worker/app.py grounding-worker/detector.py grounding-worker/requirements.txt grounding-worker/README.md .pkg-stage/usr/share/rose/grounding-worker/
chmod 644 .pkg-stage/usr/share/rose/grounding-worker/*
cat > .pkg-stage/usr/share/doc/rose/copyright <<'EOF'
Rose - AI agent for an Ubuntu VM (XFCE over TigerVNC).
Upstream: see repository. License: MIT OR Apache-2.0.
Bundled musl libc: MIT licensed (https://musl.libc.org).
EOF

# NOTE: control.tar.gz members must be archive-rooted (./control, ./postinst),
# NOT under DEBIAN/ -- dpkg extracts to tmp.ci/ and looks for tmp.ci/control.
# (Nesting them under DEBIAN/ breaks dpkg and segfaults apt's parser.)
mkdir -p .pkg-stage/control
cat > .pkg-stage/control/control <<'EOF'
Package: rose
Version: $VER
Section: utils
Priority: optional
Architecture: amd64
Depends: xfce4, tigervnc-standalone-server, dbus-x11, x11-xserver-utils, ffmpeg, fonts-liberation, fonts-dejavu, curl
Recommends: google-chrome-stable
Description: Rose - AI agent that operates an XFCE desktop over VNC
 rosed is one static binary: agent loop, Governor, Web UI, API, SQLite state.
 VNC binds localhost only; reach it over SSH/Tailscale.
 After installing, run `rosed install` (no sudo) to wire up user services.
EOF
# Fill in the real version (quoted heredoc above keeps backticks literal).
sed -i "s/^Version: .*/Version: $VER/" .pkg-stage/control/control

cat > .pkg-stage/control/postinst <<'EOF'
#!/bin/sh
set -e
echo "rose installed. Next (as your user, no sudo):"
echo "  rosed install"
echo "Chrome (once, needs Google's repo): see deploy/setup.sh or install google-chrome-stable."
exit 0
EOF
chmod 755 .pkg-stage/control/postinst

printf '2.0\n' > .pkg-stage/debian-binary
(cd .pkg-stage/control && tar --owner=0 --group=0 --numeric-owner -czf ../control.tar.gz ./control ./postinst)
(cd .pkg-stage && tar --owner=0 --group=0 --numeric-owner -czf data.tar.gz usr)
rm -rf .pkg-stage/control .pkg-stage/DEBIAN
(cd .pkg-stage && ar rcs "$OLDPWD/$DEB" debian-binary control.tar.gz data.tar.gz)
rm -rf .pkg-stage

echo "==> contents"
ar t "$DEB"
tar -tzf <(ar p "$DEB" data.tar.gz) | sort | head -20
ls -la "$DEB"
