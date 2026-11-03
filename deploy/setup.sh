#!/usr/bin/env bash
# Rose - one-time OS setup for a fresh Ubuntu Server box.
# Installs system deps (sudo), Chrome, VNC xstartup, lingering.
# rosed itself ships as a static binary in the tarball -- install it
# afterwards with zero sudo: tar xzf rose_*_x86_64.tar.gz && ./rose-*/install.sh
#
# Usage:
#   ./deploy/setup.sh [--tarball PATH]   # OS setup, then install rosed from tarball
#   ./deploy/setup.sh --status           # read-only check, exit 0 ok / 1 problems
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DISPLAY_NUM="${DISPLAY:-:1}"
TARBALL=""

for arg in "$@"; do
  case "$arg" in
    --status) exec "$SCRIPT_DIR/status.sh" ;;
    --tarball=*) TARBALL="${arg#*=}" ;;
    -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
    *) echo "Unknown arg: $arg (try --help)" >&2; exit 2 ;;
  esac
done

have() { command -v "$1" >/dev/null 2>&1; }

echo "==> 1/4 system packages (sudo, once)"
sudo apt update
sudo apt install -y \
  xfce4 tigervnc-standalone-server dbus-x11 x11-xserver-utils \
  ffmpeg fonts-liberation fonts-dejavu curl

echo "==> 2/4 Google Chrome (.deb, not snap)"
if have google-chrome; then
  echo "  already installed: $(google-chrome --version)"
else
  wget -O /tmp/google-chrome-stable_current_amd64.deb \
    https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb
  sudo dpkg -i /tmp/google-chrome-stable_current_amd64.deb || true
  sudo apt -f install -y
  echo "  installed: $(google-chrome --version)"
fi

echo "==> 3/4 VNC xstartup + lingering"
mkdir -p "$HOME/.vnc"
cat > "$HOME/.vnc/xstartup" <<'EOF'
#!/bin/sh
unset SESSION_MANAGER
unset DBUS_SESSION_BUS_ADDRESS
exec dbus-launch startxfce4
EOF
chmod +x "$HOME/.vnc/xstartup"
echo "  wrote ~/.vnc/xstartup"
# The only other sudo command. Non-fatal: without it user services
# stop at logout, everything else still works.
if sudo loginctl enable-linger "$USER"; then
  echo "  linger enabled"
else
  echo "  WARNING: linger failed -- rerun: sudo loginctl enable-linger $USER"
fi
# Best-effort: keep the VNC session from locking/blanking (needs a
# running session, so failures here just mean "applies on login").
DISPLAY="$DISPLAY_NUM" xfconf-query -c xfce4-power-manager \
  -p /xfce4-power-manager/blank-on-ac -s 0 2>/dev/null || true
DISPLAY="$DISPLAY_NUM" xfconf-query -c xfce4-screensaver \
  -p /saver/enabled -s false 2>/dev/null || true

echo "==> 4/4 rosed (zero sudo, via tarball)"
if [ -n "$TARBALL" ]; then
  tmp="$(mktemp -d)"
  tar xzf "$TARBALL" -C "$tmp"
  "$tmp"/rose-*/install.sh
  rm -rf "$tmp"
else
  echo "  OS ready. Now install rosed (as your user, no sudo):"
  echo "    tar xzf rose_<ver>_x86_64.tar.gz && cd rose-<ver> && ./install.sh"
  echo "  or rerun with: ./deploy/setup.sh --tarball=dist/rose_<ver>_x86_64.tar.gz"
fi

echo ""
echo "Tunnels (never expose ports, localhost only):"
echo "  ssh -L 5901:127.0.0.1:5901 <vm>  # VNC, DISPLAY $DISPLAY_NUM 1600x900"
echo "  ssh -L 8787:127.0.0.1:8787 <vm>  # Rose UI http://127.0.0.1:8787"
echo "Key (env only, never in files): export ROSE_MUSE_API_KEY=..."
