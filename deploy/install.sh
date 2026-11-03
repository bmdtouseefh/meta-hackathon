#!/usr/bin/env bash
# Rose user-level install/update. ZERO sudo required.
# Run as your normal user on the VM, from the extracted tarball:
#   tar xzf rose_<ver>_x86_64.tar.gz && cd rose-<ver> && ./install.sh
# Or from a repo checkout: ./deploy/install.sh [--expect-version X] [--dry-run]
#
# What it does (idempotent, rerun for every update):
#   1. copies rosed -> ~/.local/bin/rosed
#   2. runs `rosed install` (writes user units + example config, never
#      overwrites config; daemon-reload; restarts xvnc + rosed)
#   3. verifies units active + /api/health serves the new version
#
# Fresh VM system deps (apt, Chrome, lingering) are NOT handled here --
# that is the one-time sudo job: ./deploy/setup.sh
# If lingering is missing, the one sudo command is printed, not run.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
EXPECT_VER=""
DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --expect-version=*) EXPECT_VER="${arg#*=}" ;;
    --expect-version) echo "usage: $0 [--expect-version=X] [--dry-run]" >&2; exit 2 ;;
    --dry-run) DRY_RUN=1 ;;
    -h|--help)
      sed -n '2,12p' "$0"
      exit 0
      ;;
    *) echo "Unknown arg: $arg (try --help)" >&2; exit 2 ;;
  esac
done

# Locate the new binary: tarball dir first, then repo build outputs.
SRC=""
for cand in \
  "$SCRIPT_DIR/rosed" \
  "$SCRIPT_DIR/../target/x86_64-unknown-linux-musl/release/rosed" \
  "$SCRIPT_DIR/../target/release/rosed" \
; do
  if [ -x "$cand" ]; then SRC="$cand"; break; fi
done
[ -n "$SRC" ] || { echo "ERROR: rosed binary not found next to $0 (or build it: cargo build --release -p rosed)" >&2; exit 1; }
if [ -z "$EXPECT_VER" ]; then
  # Tarball dirs are named rose-<ver>; repo checkouts read Cargo.toml.
  case "$SCRIPT_DIR" in
    */rose-*) EXPECT_VER="$(basename "$SCRIPT_DIR" | sed 's/^rose-//')" ;;
    *) EXPECT_VER="$(grep -m1 '^version' "$SCRIPT_DIR/../Cargo.toml" 2>/dev/null | cut -d'"' -f2 || true)" ;;
  esac
fi

BIN="$HOME/.local/bin/rosed"
echo "==> install rosed ($SRC -> $BIN)"
if [ "$DRY_RUN" -eq 1 ]; then
  echo "  [dry-run] copy $SRC -> $BIN"
  "$SRC" install --dry-run
  echo "  [dry-run] skip restart + health check"
  exit 0
fi
mkdir -p "$HOME/.local/bin"
cp -f "$SRC" "$BIN"
chmod +x "$BIN"

echo "==> rosed install (units + config + services)"
"$BIN" install

echo "==> linger check (the only step that ever needs sudo)"
if loginctl show-user "$USER" 2>/dev/null | grep -q 'Linger=yes'; then
  echo "  linger enabled"
else
  echo "  WARNING: linger not enabled - user services stop at logout."
  echo "  One-time fix (needs sudo, once per VM): sudo loginctl enable-linger $USER"
fi

echo "==> verify"
echo "  xvnc:  $(systemctl --user is-active xvnc.service 2>/dev/null || echo unknown)"
echo "  rosed: $(systemctl --user is-active rosed.service 2>/dev/null || echo unknown)"
# The restarted rosed needs a moment to bind; retry instead of racing it.
HEALTH=""
for i in $(seq 1 15); do
  HEALTH="$(curl -s --max-time 5 http://127.0.0.1:8787/api/health || true)"
  [ -n "$HEALTH" ] && break
  sleep 1
done
echo "  health: $HEALTH"
if [ -n "$EXPECT_VER" ]; then
  echo "$HEALTH" | grep -q "\"version\":\"$EXPECT_VER\"" || {
    echo "MISMATCH: serving version != expected v$EXPECT_VER" >&2
    journalctl --user -u rosed.service --no-pager -n 10 2>/dev/null | tail -10 || true
    exit 1
  }
  echo "==> serving v$EXPECT_VER"
fi
echo "==> done. UI tunnel: ssh -N -L 8787:127.0.0.1:8787 <vm> -> http://127.0.0.1:8787"
