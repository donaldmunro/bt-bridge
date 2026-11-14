#!/usr/bin/env bash
set -euo pipefail

# Usage: ./install-service.sh "AA:BB:CC:DD:EE:FF,BB:CC:DD:EE:FF:00" [system|user]
DEVICE_LIST="${1:-}"
MODE="${2:-user}"

if [[ -z "$DEVICE_LIST" ]]; then
    echo "Usage: $0 \"MAC1,MAC2,...\" [system|user]" >&2
    exit 1
fi

TEMPLATE="$(dirname "$0")/bt-bridge.service"
if [[ ! -f "$TEMPLATE" ]]; then
    echo "Template not found: $TEMPLATE" >&2
    exit 1
fi

if [[ "$MODE" == "system" ]]; then
    UNIT_PATH="/etc/systemd/system/bt-bridge.service"
    SUDO="sudo"
else
    UNIT_PATH="$HOME/.config/systemd/user/bt-bridge.service"
    SUDO=""
    mkdir -p "$(dirname "$UNIT_PATH")"
fi

sed "s|%DEVICE_LIST%|$DEVICE_LIST|g" "$TEMPLATE" | $SUDO tee "$UNIT_PATH" >/dev/null

if [[ "$MODE" == "system" ]]; then
    sudo systemctl daemon-reload
    sudo systemctl enable --now bt-bridge
else
    systemctl --user daemon-reload
    systemctl --user enable --now bt-bridge
fi

echo "Installed and started bt-bridge service ($MODE)."
echo "Stop with: systemctl ${MODE:+--user} stop bt-bridge"