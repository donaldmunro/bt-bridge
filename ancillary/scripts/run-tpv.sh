#!/bin/bash
PREFIX_NAME=".wine-tpv"
INSTALLER_NAME='TPVirtual-Installer_v4b.exe'
#USER="someone" # uncomment if Wine account is not the same as your Linux username.

export WINEPREFIX=~/$PREFIX_NAME
export WINEDLLPATH=/usr/lib/wine:/src/Linux/wine-devel/dlls

INSTALLER="downloads/$INSTALLER_NAME"
RELAY_KEY='HKCU\Software\Wine\Debug'
RELAY_VALUE='BleWin10Lib*;windows.devices.bluetooth*;windows.devices.enumeration*;bluetoothapis*;combase*'

usage() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS]

  (none)        Launch TPVirtual without debug logs
  --debug       Launch TPVirtual with WINEDEBUG logging to ./log
  --recreate    Delete Wine prefix, reinstall, then exit
  --reinstall   Reinstall without deleting prefix, then exit
  -h, --help    Show this help
EOF
    exit 0
}

run_app() {
    local debug_mode=$1
    if [ "$debug_mode" = "1" ]; then
        # Update relay registry key only when the stored value differs from RELAY_VALUE
        current=$(wine reg query "$RELAY_KEY" /v RelayInclude 2>/dev/null \
                  | grep -i RelayInclude | sed 's/.*REG_SZ[[:space:]]*//')
        if [ "$current" != "$RELAY_VALUE" ]; then
            wine reg add "$RELAY_KEY" /v RelayInclude /t REG_SZ /d "$RELAY_VALUE" /f
        fi
        export WINEDEBUG=+relay,+loaddll,+seh,+bluetoothapis,+winebth,+bluetooth,fixme-all
        echo "TPVirtual launched with WINEDEBUG (log -> ./log)"
        wine "C:\users/$USER/AppData/Local/TPVirtual/TPVirtual-Launcher.exe" >& log &
    else
        unset WINEDEBUG
        echo "TPVirtual launched"
        wine "C:\users/$USER/AppData/Local/TPVirtual/TPVirtual-Launcher.exe" > /dev/null 2>&1 &
    fi
}

case "${1:-}" in
    --recreate)
        echo "Deleting $WINEPREFIX ..."
        rm -rf "$WINEPREFIX"
        echo "Running installer ..."
        winetricks d3dcompiler_47 dxvk # dxvk (use Vulkan) improves frame rate markedly over Wine's default Direct3D implementation, and d3dcompiler_47 is required by TPVirtual.
        wine "$INSTALLER"
        ;;
    --reinstall)
        echo "Running installer (keeping existing prefix) ..."
        wine "$INSTALLER"
        ;;
    -h|--help)
        usage
        ;;
    --debug)
        run_app 1
        ;;
    "")
#        winetricks d3dcompiler_47 dxvk
        run_app 0
        ;;
    *)
        echo "Unknown option: $1"
        usage
        ;;
esac
