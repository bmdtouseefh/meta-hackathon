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
echo "-- grounding-worker deps + unit (lite SoM parser, 4 CPU / 4 GB, uv venv)"
export PATH="$HOME/.local/bin:$PATH"
command -v uv >/dev/null 2>&1 || curl -LsSf https://astral.sh/uv/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"
sudo apt install -y tesseract-ocr tesseract-ocr-eng
VENV="$HOME/.local/share/rose/grounding-venv"
uv venv "$VENV" 2>/dev/null || true
uv pip install --python "$VENV/bin/python" -r /usr/share/rose/grounding-worker/requirements.txt
cp -f /usr/lib/systemd/user/grounding.service ~/.config/systemd/user/grounding.service
# Existing config.toml is never overwritten by rosed install, so enable the
# overlay here (idempotent). Section-aware: only touch [grounding].
python3 - <<'PY'
import pathlib, re
p = pathlib.Path.home() / ".config/rose/config.toml"
text = p.read_text() if p.exists() else ""
m = re.search(r"(?m)^\[grounding\][^\[]*", text)
if m is None:
    text += '\n[grounding]\nbackend = "omniparser"\nendpoint = "http://127.0.0.1:8791"\nmin_conf = 5\n'
else:
    block = m.group(0)
    if re.search(r'(?m)^backend\s*=', block):
        block = re.sub(r'(?m)^backend\s*=.*', 'backend = "omniparser"', block, count=1)
    else:
        block = block.rstrip("\n") + '\nbackend = "omniparser"\n'
    text = text[:m.start()] + block + text[m.end():]
p.parent.mkdir(parents=True, exist_ok=True)
p.write_text(text)
PY
echo "-- restart services"
systemctl --user daemon-reload
systemctl --user restart xvnc.service
systemctl --user restart grounding.service
systemctl --user restart rosed.service
sleep 4
echo "-- verify"
grep -H ExecStart ~/.config/systemd/user/rosed.service
echo "xvnc: $(systemctl --user is-active xvnc.service)"
echo "grounding: $(systemctl --user is-active grounding.service)"
echo "rosed: $(systemctl --user is-active rosed.service)"
echo "grounding health: $(curl -s http://127.0.0.1:8791/health)"
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
