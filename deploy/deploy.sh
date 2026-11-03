#!/usr/bin/env bash
# One-command build + deploy + update the VM (zero sudo):
#   ./deploy/deploy.sh [user@host]   (or export ROSE_VM=user@host)
# Builds the tarball, copies it over, runs install.sh as the user,
# and verifies. Idempotent: rerun for every update.
#
# Fresh VM system deps (apt, Chrome) are the one-time sudo job and live
# in ./deploy/setup.sh -- this script never touches sudo.
# Legacy .deb flow is kept in ./deploy/deb.sh for reference only.
set -euo pipefail
cd "$(dirname "$0")/.."

VM="${1:-${ROSE_VM:-}}"
[ -n "$VM" ] || { echo "usage: $0 user@host  (or export ROSE_VM=user@host)" >&2; exit 2; }

./deploy/package.sh
TARBALL="$(ls -t dist/rose_*_x86_64.tar.gz | head -1)"
VER="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
echo "==> copy $TARBALL (v$VER) -> $VM"
scp "$TARBALL" "$VM":~/

echo "==> install + update on $VM (no sudo)"
# shellcheck disable=SC2087
ssh "$VM" TARBALL="$(basename "$TARBALL")" VER="$VER" bash -s <<'EOF'
set -euo pipefail
rm -rf "rose-$VER"
tar xzf "$TARBALL"
cd "rose-$VER"
./install.sh --expect-version="$VER"
EOF

echo "==> done. UI tunnel: ssh -N -L 8787:127.0.0.1:8787 $VM  ->  http://127.0.0.1:8787"
