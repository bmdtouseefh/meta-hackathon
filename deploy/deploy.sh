#!/usr/bin/env bash
# One-command build + deploy + update the VM:
#   ./deploy/deploy.sh [user@host]   (or export ROSE_VM=user@host)
# Builds the .deb, copies it over, installs, rewires user services,
# restarts everything, and verifies. Idempotent: rerun for every update.
#
# Lessons baked in:
# - apt skips same-version reinstalls, so every deploy bumps the version
#   (deb.sh reads it from the workspace Cargo.toml).
# - Always invoke /usr/bin/rosed by absolute path (stale ~/.local/bin
#   copies once shadowed it on PATH).
# - Non-interactive sudo needs NOPASSWD for apt/dpkg/loginctl on the VM:
#     echo 'rose ALL=(ALL) NOPASSWD: /usr/bin/apt, /usr/bin/apt-get, /usr/bin/dpkg, /usr/bin/loginctl' | sudo tee /etc/sudoers.d/rose
#     sudo chmod 440 /etc/sudoers.d/rose && sudo visudo -c
set -euo pipefail
cd "$(dirname "$0")/.."

VM="${1:-${ROSE_VM:-}}"
[ -n "$VM" ] || { echo "usage: $0 user@host  (or export ROSE_VM=user@host)" >&2; exit 2; }

./deploy/deb.sh
DEB="$(ls -t dist/rose_*_amd64.deb | head -1)"
VER="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
echo "==> copy $DEB (v$VER) -> $VM"
# shellcheck disable=SC2087
scp "$DEB" "$VM":~/

echo "==> install + update on $VM"
ssh "$VM" DEB="$(basename "$DEB")" VER="$VER" bash -s <<'EOF'
set -euo pipefail
echo "-- before: $(dpkg-query -W -f='${Version}' rose 2>/dev/null || echo none)"
echo "-- apt install ./$DEB"
sudo apt install -y "./$DEB"
echo "-- after: $(dpkg-query -W -f='${Version}' rose)"
echo "-- rosed install (per-user wiring, absolute path on purpose)"
/usr/bin/rosed install
cp -f /usr/lib/systemd/user/chrome-cdp.service ~/.config/systemd/user/chrome-cdp.service
echo "-- chrome-cdp (existing google-chrome-stable with --remote-debugging-port=9222)"
# One-time migration: Chrome refuses remote debugging on its default profile
# dir, so chrome-cdp uses ~/.config/rose/chrome-profile (logins kept).
if [ ! -d "$HOME/.config/rose/chrome-profile" ] && [ -d "$HOME/.config/google-chrome" ]; then
  echo "-- migrating Chrome profile to CDP dir (one-time, logins kept)"
  mkdir -p "$HOME/.config/rose"
  mv "$HOME/.config/google-chrome" "$HOME/.config/rose/chrome-profile"
fi
# chrome-cdp must own the profile: a manually started Chrome without the flag
# holds the SingletonLock and the unit would crash-loop.
pkill -f 'chrome' 2>/dev/null || true
sleep 1
echo "-- restart services"
systemctl --user daemon-reload
systemctl --user restart xvnc.service
systemctl --user enable --now chrome-cdp.service
echo "-- wait for CDP http://127.0.0.1:9222/json/version (first launch builds caches)"
for i in $(seq 1 20); do
  if curl -sf http://127.0.0.1:9222/json/version >/dev/null 2>&1; then
    echo "-- CDP endpoint live"
    break
  fi
  sleep 1
  if [ "$i" -eq 20 ]; then
    echo "WARN: CDP endpoint not responding - journalctl --user -u chrome-cdp.service -n 30" >&2
  fi
done
systemctl --user restart rosed.service
sleep 4
echo "-- verify"
grep -H ExecStart ~/.config/systemd/user/rosed.service
echo "xvnc: $(systemctl --user is-active xvnc.service)"
echo "chrome-cdp: $(systemctl --user is-active chrome-cdp.service)"
echo "rosed: $(systemctl --user is-active rosed.service)"
echo "CDP: $(curl -s http://127.0.0.1:9222/json/version | head -c 120)"
HEALTH="$(curl -s http://127.0.0.1:8787/api/health)"
echo "health: $HEALTH"
echo "$HEALTH" | grep -q "\"version\":\"$VER\"" || {
  echo "MISMATCH: serving version != built v$VER" >&2
  journalctl --user -u rosed.service --no-pager -n 10 | tail -10
  exit 1
}
echo "==> deployed v$VER, serving it"
EOF

echo "==> done. UI tunnel: ssh -N -L 8787:127.0.0.1:8787 $VM  ->  http://127.0.0.1:8787"
