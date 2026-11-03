#!/usr/bin/env bash
# Rose - read-only health check for the VM. Exit 0 ok / 1 problems.
# Usage: ./deploy/status.sh  (or ./deploy/setup.sh --status)
set -euo pipefail

DISPLAY_NUM="${DISPLAY:-:1}"
fail=0
ok()   { echo "  [ok] $*"; }
bad()  { echo "  [FAIL] $*"; fail=$((fail + 1)); }
warn() { echo "  [warn] $*"; }
have() { command -v "$1" >/dev/null 2>&1; }

echo "== packages / binaries =="
for bin in vncserver Xvnc dbus-launch startxfce4 xdpyinfo ffmpeg google-chrome curl; do
  if have "$bin"; then ok "$bin";
  else bad "$bin missing -- rerun ./deploy/setup.sh"; fi
done

echo "== VNC desktop ($DISPLAY_NUM, expect 1600x900) =="
if [ -x "$HOME/.vnc/xstartup" ] && grep -q startxfce4 "$HOME/.vnc/xstartup" 2>/dev/null; then
  ok "xstartup launches xfce4"
else
  bad "~/.vnc/xstartup missing -- rerun ./deploy/setup.sh"
fi
if have xdpyinfo && DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then
  dims="$(DISPLAY="$DISPLAY_NUM" xdpyinfo 2>/dev/null | awk '/dimensions:/{print $2}')"
  [ "$dims" = "1600x900" ] && ok "DISPLAY $DISPLAY_NUM at $dims" || bad "resolution $dims, want 1600x900"
  DISPLAY="$DISPLAY_NUM" xdpyinfo 2>/dev/null | grep -q XTEST && ok "XTEST present" || bad "XTEST missing -- input injection will not work"
else
  bad "DISPLAY $DISPLAY_NUM not reachable -- is xvnc active? (systemctl --user status xvnc)"
fi

echo "== services =="
loginctl show-user "$USER" 2>/dev/null | grep -q 'Linger=yes' \
  && ok "linger enabled" \
  || warn "linger off -- sudo loginctl enable-linger $USER"
for svc in xvnc rosed; do
  state="$(systemctl --user is-active "$svc" 2>/dev/null || echo inactive)"
  state="${state%%$'\n'*}" # is-active can print "unknown" + our fallback on two lines
  [ "$state" = "active" ] && ok "$svc.service active" || bad "$svc.service $state -- journalctl --user -u $svc -n 30"
done

echo "== rosed =="
[ -x "$HOME/.local/bin/rosed" ] && ok "~/.local/bin/rosed exists" || bad "rosed binary missing -- run the tarball install.sh"
[ -f "$HOME/.config/rose/config.toml" ] && ok "config.toml exists" || warn "config.toml missing -- runs on defaults until rosed install"
[ -n "${ROSE_MUSE_API_KEY:-}" ] && ok "ROSE_MUSE_API_KEY is set" || warn "ROSE_MUSE_API_KEY not set (needed for brain=muse)"
health="$(curl -s --max-time 5 http://127.0.0.1:8787/api/health || true)"
[ -n "$health" ] && ok "api/health: $health" || bad "no response on 127.0.0.1:8787 -- is rosed active?"

echo "== listeners (localhost-only) =="
addrs="$( (ss -tlnH 2>/dev/null || netstat -tln 2>/dev/null) | awk '{print $4}' | grep -E ':(5901|8787|9222)$' || true)"
if [ -z "$addrs" ]; then
  warn "nothing listening on 5901/8787 yet (services not started?)"
else
  echo "$addrs" | sed 's/^/  [listener] /'
  echo "$addrs" | sed 's/[][]//g' | grep -vqE '^(127\.0\.0\.1|::1):(5901|8787|9222)$' \
    && bad "non-localhost listener -- must bind localhost + tunnel only" \
    || ok "localhost-only"
fi

[ "$fail" -eq 0 ] && echo "== OK ==" || echo "== $fail check(s) FAILED =="
exit "$fail"
