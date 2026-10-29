#!/usr/bin/env bash
# Rose - idempotent setup for a fresh Ubuntu Server (local VM or EC2).
# Rerunnable: "reset the machine" = rerun this script.
# Source of truth: architecture.html section 2, IMPLEMENTATION.md T4.2.
#
# Usage:
#   ./deploy/setup.sh [--clean-chrome]   # full install (default)
#   ./deploy/setup.sh --status           # read-only check, exit 0 ok / 1 failures
#   ./deploy/status.sh                   # compat shim -> setup.sh --status
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
GEOMETRY="1600x900"
DISPLAY_NUM="${DISPLAY:-:1}"
LOG_FILE="$HOME/.local/share/rose/setup.log"

MODE="install"
CLEAN_CHROME=0
for arg in "$@"; do
  case "$arg" in
    --status) MODE="status" ;;
    --clean-chrome) CLEAN_CHROME=1 ;;
    -h|--help)
      sed -n '2,8p' "$0"
      exit 0
      ;;
    *) echo "Unknown arg: $arg (try --help)" >&2; exit 2 ;;
  esac
done

mkdir -p "$(dirname "$LOG_FILE")"

# ---------- logging -----------------------------------------------------------
if [ -t 1 ]; then
  C_RESET=$'\033[0m'; C_INFO=$'\033[36m'; C_OK=$'\033[32m'
  C_WARN=$'\033[33m'; C_FAIL=$'\033[31m'; C_STEP=$'\033[1m'
else
  C_RESET=''; C_INFO=''; C_OK=''; C_WARN=''; C_FAIL=''; C_STEP=''
fi
ts() { date '+%Y-%m-%d %H:%M:%S'; }
_log() { printf '%s %s\n' "$(ts)" "$*" >>"$LOG_FILE"; }
say()  { printf '%b\n' "$*"; _log "$(printf '%s' "$*" | sed 's/\x1b\[[0-9;]*m//g')"; }
info() { say "${C_INFO}[info]${C_RESET} $*"; }
ok()   { say "${C_OK}[ok]${C_RESET} $*"; }
warn() { say "${C_WARN}[warn]${C_RESET} $*"; }
fail() { say "${C_FAIL}[fail]${C_RESET} $*"; }
step() { say ""; say "${C_STEP}=== $* ===${C_RESET}"; }

N_OK=0; N_WARN=0; N_FAIL=0
pass() { ok "$*"; N_OK=$((N_OK+1)); }
warnc() { warn "$*"; N_WARN=$((N_WARN+1)); }
failc() { fail "$*"; N_FAIL=$((N_FAIL+1)); }

on_err() {
  local line="$1" cmd="$2"
  fail "command failed at $0:$line: $cmd"
  say "${C_FAIL}[hint]${C_RESET} full log: $LOG_FILE"
  say "${C_FAIL}[hint]${C_RESET} rerun with: bash -x $0 $* for a trace"
}
trap 'on_err "$LINENO" "$BASH_COMMAND"' ERR

# Run a named install step; failures are logged with context, abort the script.
# Usage: run_step "apt packages" <command...>
run_step() {
  local name="$1"; shift
  local start end rc=0
  step "$name"
  info "\$ $*"
  start="$(date +%s)"
  if "$@" 2>&1 | tee -a "$LOG_FILE" | sed 's/^/  | /'; then
    end="$(date +%s)"
    ok "step done: $name ($((end-start))s)"
  else
    rc=${PIPESTATUS[0]:-1}
    end="$(date +%s)"
    fail "step FAILED: $name (exit $rc, ${end-start}s) - see $LOG_FILE"
    exit "$rc"
  fi
}

have() { command -v "$1" >/dev/null 2>&1; }

# ---------- status (read-only doctor) ------------------------------------------
cmd_status() {
  say "== Rose status =="
  say "host: $(lsb_release -ds 2>/dev/null || grep PRETTY_NAME /etc/os-release | cut -d= -f2 | tr -d '\"') | user: $USER | DISPLAY: $DISPLAY_NUM | log: $LOG_FILE"
  _log "--- status run by $USER on $(hostname) ---"

  step "packages / binaries"
  for bin in vncserver Xvnc dbus-launch startxfce4 xdpyinfo xrandr ffmpeg google-chrome curl git cargo gcc sqlite3; do
    if have "$bin"; then pass "$bin ($(command -v "$bin"))";
    else
      case "$bin" in
        xdpyinfo|xrandr) warnc "$bin missing (pkg x11-xserver-utils) - display checks limited" ;;
        cargo|gcc|sqlite3) warnc "$bin missing - only needed to build rosed" ;;
        *) failc "$bin missing - rerun: ./deploy/setup.sh" ;;
      esac
    fi
  done
  have google-chrome && info "$(google-chrome --version 2>/dev/null)" || true
  have cargo && info "$(cargo --version 2>/dev/null)" || true

  step "VNC desktop ($DISPLAY_NUM, expect $GEOMETRY)"
  if [ -x "$HOME/.vnc/xstartup" ] && grep -q startxfce4 "$HOME/.vnc/xstartup" 2>/dev/null; then
    pass "~/.vnc/xstartup executable, launches xfce4"
  else
    failc "~/.vnc/xstartup missing/not executable/no startxfce4 - rerun ./deploy/setup.sh"
  fi
  if have xdpyinfo && DISPLAY="$DISPLAY_NUM" xdpyinfo >>"$LOG_FILE" 2>&1; then
    pass "DISPLAY $DISPLAY_NUM reachable"
    DIMS="$(DISPLAY="$DISPLAY_NUM" xdpyinfo 2>/dev/null | awk '/dimensions:/{print $2}')"
    if [ "$DIMS" = "$GEOMETRY" ]; then pass "resolution $DIMS";
    else failc "resolution is ${DIMS:-unknown}, want $GEOMETRY (vncserver -geometry $GEOMETRY)"; fi
    if DISPLAY="$DISPLAY_NUM" xdpyinfo 2>/dev/null | grep -q XTEST; then pass "XTEST present";
    else
      failc "XTEST missing - input injection will not work"
      DISPLAY="$DISPLAY_NUM" xdpyinfo 2>/dev/null | tail -n 35 | sed 's/^/  [xdpyinfo-ext] /' | tee -a "$LOG_FILE"
    fi
  else
    failc "DISPLAY $DISPLAY_NUM not reachable (is xvnc active? systemctl --user status xvnc; journalctl --user -u xvnc -n 50)"
  fi

  step "services (systemd --user)"
  if loginctl show-user "$USER" 2>/dev/null | grep -q 'Linger=yes'; then pass "linger enabled";
  else warnc "linger not enabled (sudo loginctl enable-linger $USER)"; fi
  for svc in xvnc rosed; do
    if systemctl --user list-unit-files 2>/dev/null | grep -q "^$svc.service"; then
      STATE="$(systemctl --user is-active "$svc" 2>/dev/null || true)"
      ENABLED="$(systemctl --user is-enabled "$svc" 2>/dev/null || true)"
      if [ "$STATE" = "active" ]; then pass "$svc.service active (enabled: $ENABLED)";
      else
        DETAIL="$(systemctl --user status "$svc" --no-pager 2>&1 | tail -n 5 | tr '\n' ' ' || true)"
        if [ "$svc" = "rosed" ] && [ ! -x "$HOME/.local/bin/rosed" ]; then
          warnc "rosed.service $STATE (binary not built yet - expected before M0) :: $DETAIL"
        else
          failc "$svc.service $STATE (enabled: $ENABLED) - journalctl --user -u $svc -n 50 :: $DETAIL"
        fi
      fi
    else
      [ "$svc" = "rosed" ] \
        && warnc "$svc.service not installed (rerun setup.sh after M0)" \
        || failc "$svc.service not installed - rerun ./deploy/setup.sh"
    fi
  done

  step "rosed binary + config"
  if [ -x "$HOME/.local/bin/rosed" ]; then
    pass "~/.local/bin/rosed exists"
    if DISPLAY="$DISPLAY_NUM" "$HOME/.local/bin/rosed" doctor >>"$LOG_FILE" 2>&1; then pass "rosed doctor green";
    else warnc "rosed doctor reported issues - rerun DISPLAY=$DISPLAY_NUM ~/.local/bin/rosed doctor"; fi
  else
    warnc "~/.local/bin/rosed not built yet (expected before M0 scaffold)"
  fi
  if [ -f "$HOME/.config/rose/config.toml" ]; then pass "~/.config/rose/config.toml exists";
  else warnc "~/.config/rose/config.toml missing (setup.sh copies config/rose.example.toml)"; fi
  if [ -n "${ROSE_MUSE_API_KEY:-}" ]; then pass "ROSE_MUSE_API_KEY is set (value hidden)";
  else warnc "ROSE_MUSE_API_KEY not set in env (needed for brain=muse)"; fi

  step "network (localhost-only is correct)"
  # $4 is the local address in both ss -tlnH and netstat -tln output.
  local_addrs="$( (ss -tlnH 2>/dev/null || netstat -tln 2>/dev/null) | awk '{print $4}' | grep -E ':(5901|8787)$' || true)"
  if [ -z "$local_addrs" ]; then
    warnc "nothing listening on 5901/8787 yet (services not started?)"
  else
    echo "$local_addrs" | sed 's/^/  [listener] /' | tee -a "$LOG_FILE"
    # Strip [] so [::1]:5901 compares equal to loopback ::1 (ss brackets IPv6).
    bad_addrs="$(echo "$local_addrs" | sed 's/[][]//g' | grep -vE '^(127\.0\.0\.1|::1):(5901|8787)$' || true)"
    if [ -n "$bad_addrs" ]; then
      failc "non-localhost listener (must bind localhost + tunnel only): $bad_addrs"
    else pass "5901/8787 localhost-only"; fi
  fi

  say ""
  say "== result: ${C_OK}$N_OK ok${C_RESET} / ${C_WARN}$N_WARN warnings${C_RESET} / ${C_FAIL}$N_FAIL failures${C_RESET} (log: $LOG_FILE) =="
  [ "$N_FAIL" -eq 0 ]
}

# ---------- install -------------------------------------------------------------
cmd_install() {
  say "== Rose setup (install) =="
  say "log: $LOG_FILE"
  _log "--- install run by $USER on $(hostname): args: $* ---"

  run_step "1/9 apt packages" sudo apt update
  run_step "1/9 apt packages (install)" sudo apt install -y \
    xfce4 tigervnc-standalone-server dbus-x11 x11-xserver-utils \
    ffmpeg fonts-liberation fonts-dejavu curl git \
    build-essential pkg-config libssl-dev sqlite3 \
    tesseract-ocr tesseract-ocr-eng

  step "2/9 Google Chrome (.deb, not snap)"
  if have google-chrome; then
    ok "already installed: $(google-chrome --version)"
  else
    info "\$ wget google-chrome .deb && dpkg -i && apt -f install"
    if ! wget -O /tmp/google-chrome-stable_current_amd64.deb \
      https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb 2>&1 | tee -a "$LOG_FILE" | sed 's/^/  | /'; then
      fail "download of Chrome .deb failed - check network/proxy, see $LOG_FILE"; exit 1
    fi
    sudo dpkg -i /tmp/google-chrome-stable_current_amd64.deb >>"$LOG_FILE" 2>&1 || true
    run_step "2/9 Chrome deps (apt -f)" sudo apt -f install -y
    have google-chrome || { fail "google-chrome still missing after install - see $LOG_FILE"; exit 1; }
    ok "installed: $(google-chrome --version)"
  fi

  step "3/9 Rust toolchain (builds rosed)"
  if have cargo; then ok "already installed: $(cargo --version)";
  else
    info "installing rustup stable..."
    run_step "3/9 rustup" bash -c "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable"
  fi
  # shellcheck disable=SC1090
  source "$HOME/.cargo/env" 2>/dev/null || true

  step "4/9 Chrome profile"
  if [ "$CLEAN_CHROME" -eq 1 ]; then
    warn "wiping Chrome profile for clean state..."
    pkill -f chrome || true
    rm -rf "$HOME/.config/google-chrome"
  fi
  mkdir -p "$HOME/.config/google-chrome"
  ok "profile dir ready"

  step "5/9 VNC xstartup (dbus-launch startxfce4)"
  mkdir -p "$HOME/.vnc"
  cat > "$HOME/.vnc/xstartup" <<'EOF'
#!/bin/sh
unset SESSION_MANAGER
unset DBUS_SESSION_BUS_ADDRESS
exec dbus-launch startxfce4
EOF
  chmod +x "$HOME/.vnc/xstartup"
  ok "wrote ~/.vnc/xstartup"
  info "VNC auth is out of scope: localhost-only + tunnel is the perimeter."

  step "6/9 disable screen lock / blanking / saver (best-effort)"
  if have xfconf-query; then
    DISPLAY="$DISPLAY_NUM" xfconf-query -c xfce4-power-manager \
      -p /xfce4-power-manager/blank-on-ac -s 0 >>"$LOG_FILE" 2>&1 || warn "power-manager key not writable yet (no session) - will apply on login"
    DISPLAY="$DISPLAY_NUM" xfconf-query -c xfce4-screensaver \
      -p /saver/enabled -s false >>"$LOG_FILE" 2>&1 || warn "screensaver key not writable yet (no session) - will apply on login"
  fi
  if have gsettings; then
    gsettings set org.gnome.desktop.screensaver lock-enabled false >>"$LOG_FILE" 2>&1 || true
    gsettings set org.gnome.desktop.session idle-delay 0 >>"$LOG_FILE" 2>&1 || true
  fi
  ok "saver/lock step done"

  step "7/9 rosed binary -> ~/.local/bin/rosed"
  mkdir -p "$HOME/.local/bin" "$HOME/.local/share/rose"
  if [ -f "$REPO_ROOT/Cargo.toml" ] && grep -q 'name = "rosed"' "$REPO_ROOT/Cargo.toml" 2>/dev/null; then
    run_step "7/9 cargo build --release -p rosed" bash -c "cd '$REPO_ROOT' && cargo build --release -p rosed"
    cp -f "$REPO_ROOT/target/release/rosed" "$HOME/.local/bin/rosed"
    chmod +x "$HOME/.local/bin/rosed"
    ok "installed ~/.local/bin/rosed"
  else
    warn "rosed crate not found yet (M0 scaffold pending) - skipping binary install."
    warn "once apps/rosed exists, rerun ./deploy/setup.sh to build + install it."
  fi

  step "8/9 config (never overwrite)"
  if [ -f "$REPO_ROOT/config/rose.example.toml" ]; then
    mkdir -p "$HOME/.config/rose"
    if [ ! -f "$HOME/.config/rose/config.toml" ]; then
      cp "$REPO_ROOT/config/rose.example.toml" "$HOME/.config/rose/config.toml"
      ok "wrote ~/.config/rose/config.toml from example (set ROSE_MUSE_API_KEY in env)"
    else
      ok "~/.config/rose/config.toml already exists - leaving it alone"
    fi
  else
    warn "no config/rose.example.toml in repo yet - skipping config step"
  fi

  step "9/9 systemd --user units + lingering"
  for unit in xvnc rosed grounding; do
    if [ ! -f "$SCRIPT_DIR/systemd/$unit.service" ]; then
      fail "missing $SCRIPT_DIR/systemd/$unit.service"
      fail "you copied setup.sh without its sibling systemd/ dir - copy the whole deploy/ folder, preserving structure:"
      fail "  scp -r deploy/ $USER@<vm>:~/deploy/"
      exit 1
    fi
  done
  run_step "9/9 enable-linger" sudo loginctl enable-linger "$USER"
  mkdir -p "$HOME/.config/systemd/user"
  cp -f "$SCRIPT_DIR/systemd/xvnc.service" "$HOME/.config/systemd/user/xvnc.service"
  cp -f "$SCRIPT_DIR/systemd/rosed.service" "$HOME/.config/systemd/user/rosed.service"
  cp -f "$SCRIPT_DIR/systemd/grounding.service" "$HOME/.config/systemd/user/grounding.service"
  ok "installed user units"
  run_step "9/9 daemon-reload" systemctl --user daemon-reload
  # Stale locks from a killed run make vncserver exit 1 on restart. Clean first.
  info "clearing stale :1 locks (if any)..."
  vncserver -kill :1 >>"$LOG_FILE" 2>&1 || true
  rm -f /tmp/.X1-lock /tmp/.X11-unix/X1 || true
  run_step "9/9 enable+start xvnc" systemctl --user enable --now xvnc.service
  if [ -x "$HOME/.local/bin/rosed" ]; then
    if ! systemctl --user enable --now rosed.service 2>&1 | tee -a "$LOG_FILE" | sed 's/^/  | /'; then
      # Expected before M1/M2: rosed bails on unimplemented backends (T1.2/T3.1).
      warn "rosed.service failed to start (expected until M1/M2: journalctl --user -u rosed -n 30)"
      warn "leaving it enabled - it will come up on the next successful setup.sh run"
    fi
  else
    warn "skipping rosed.service start (binary not installed yet)"
  fi
  step "10/10 grounding-worker (lite SoM parser, 4 CPU / 4 GB)"
  if have python3 && [ -f "$REPO_ROOT/grounding-worker/requirements.txt" ]; then
    export PATH="$HOME/.local/bin:$PATH"
    have uv || curl -LsSf https://astral.sh/uv/install.sh | sh
    export PATH="$HOME/.local/bin:$PATH"
    VENV="$HOME/.local/share/rose/grounding-venv"
    info "\$ uv venv + uv pip install -r grounding-worker/requirements.txt"
    if uv venv "$VENV" 2>&1 | tee -a "$LOG_FILE" | sed 's/^/  | /' \
      && uv pip install --python "$VENV/bin/python" -r "$REPO_ROOT/grounding-worker/requirements.txt" 2>&1 | tee -a "$LOG_FILE" | sed 's/^/  | /'; then
      run_step "10/10 enable+start grounding" systemctl --user enable --now grounding.service
    else
      warn "grounding-worker uv install failed - grounding backend stays off (config grounding.backend)"
    fi
  else
    warn "skipping grounding-worker (no python3 or no grounding-worker/requirements.txt)"
  fi

  say ""
  ok "setup done. Verifying with --status..."
  if ! cmd_status; then
    warn "setup finished but status reports failures above - see hints + $LOG_FILE"
    exit 1
  fi

  say ""
  say "Next steps:"
  say "  1. VNC (tunnel only, never expose 5901):  ssh -L 5901:127.0.0.1:5901 <vm>"
  say "     then connect a VNC client to localhost:5901 (DISPLAY $DISPLAY_NUM, $GEOMETRY)."
  say "  2. Rose UI (tunnel only):                 ssh -L 8787:127.0.0.1:8787 <vm>"
  say "     then open http://127.0.0.1:8787"
  say "  3. Hands test:  DISPLAY=$DISPLAY_NUM ~/.local/bin/rosed doctor"
  say "  4. Secrets from env only:  export ROSE_MUSE_API_KEY=...  (never commit keys)"
}

if [ "$MODE" = "status" ]; then
  cmd_status
else
  cmd_install "$@"
fi
