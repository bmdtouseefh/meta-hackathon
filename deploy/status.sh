#!/usr/bin/env bash
# Compat shim: ./deploy/status.sh -> ./deploy/setup.sh --status
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "$SCRIPT_DIR/setup.sh" --status "$@"
