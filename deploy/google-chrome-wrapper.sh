#!/bin/sh
# google-chrome wrapper for the Rose VM (installed at /usr/local/bin/google-chrome,
# ahead of /usr/bin on PATH). Forces every launch — agent shell steps included —
# into the shared CDP profile, so `setsid google-chrome '<url>' &` opens a new
# window in the browser rosed drives over CDP instead of a second Chrome on a
# throwaway default profile that CDP can neither see nor touch.
#
# Pass-through: explicit --user-data-dir wins (no duplicate flag); everything
# else (flags, URLs, --version) is forwarded untouched to the real binary.
set -u
REAL=/usr/bin/google-chrome
PROFILE="$HOME/.config/rose/chrome-profile"
for a in "$@"; do
  case "$a" in
    --user-data-dir=*) exec "$REAL" "$@" ;;
  esac
done
exec "$REAL" --user-data-dir="$PROFILE" "$@"
