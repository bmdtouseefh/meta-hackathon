#!/usr/bin/env bash
# One-command build + deploy + update the VM:
#   ./deploy/deploy.sh [user@host]   (or export ROSE_VM=user@host)
# Builds the .deb, copies it over, installs, rewires user services,
# restarts everything, and verifies. Idempotent: rerun for every update.
set -euo pipefail
cd "$(dirname "$0")/.."

VM="${1:-${ROSE_VM:-}}"
[ -n "$VM" ] || { echo "usage: $0 user@host  (or export ROSE_VM=user@host)" >&2; exit 2; }

./deploy/deb.sh
DEB="$(ls -t dist/rose_*_amd64.deb | head -1)"
echo "==> copy $DEB -> $VM"
# shellcheck disable=SC2087
scp "$DEB" "$VM":~/

echo "==> install + update on $VM"
ssh "$VM" DEB="$(basename "$DEB")" bash -s <<'EOF'
set -euo pipefail
echo "-- apt install ./$DEB (pulls desktop stack as deps)"
sudo apt install -y "./$DEB"
echo "-- rosed install (per-user wiring: units, config, linger, start)"
rosed install || true
echo "-- restart services"
systemctl --user restart xvnc.service
systemctl --user restart rosed.service || true
sleep 2
echo "-- verify"
rosed --print-config | tail -2
echo "xvnc: $(systemctl --user is-active xvnc.service)"
echo "rosed: $(systemctl --user is-active rosed.service)"
curl -s http://127.0.0.1:8787/api/health || echo '{"ui":"not serving (expected until brain=fake or Muse key set)"}'
echo "-- rosed logs (tail)"
journalctl --user -u rosed.service --no-pager -n 5 | tail -5 || true
EOF

echo "==> done. UI tunnel: ssh -N -L 8787:127.0.0.1:8787 $VM  ->  http://127.0.0.1:8787"
