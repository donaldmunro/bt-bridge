#!/bin/bash

# Pull --prefix NAME/--prefix=NAME out of the arguments (if given) before anything else is
# computed from it, leaving the remaining args (--recreate/--reinstall/--debug/etc.) untouched
# for the mode dispatch at the bottom of this file.
PREFIX_NAME=".wine-zwift"
_run_zwift_args=()
while [[ $# -gt 0 ]]; do
   case "$1" in
   --prefix)
      if [ -z "${2:-}" ]; then
         echo "Error: --prefix requires a value" >&2
         exit 1
      fi
      PREFIX_NAME="$2"
      shift 2
      ;;
   --prefix=*)
      PREFIX_NAME="${1#--prefix=}"
      shift
      ;;
   *)
      _run_zwift_args+=("$1")
      shift
      ;;
   esac
done
set -- "${_run_zwift_args[@]}"
unset _run_zwift_args

export WINEPREFIX="$HOME/$PREFIX_NAME"

ZWIFT_DIR="$WINEPREFIX/drive_c/Program Files (x86)/Zwift"
ZWIFT_LAUNCHER="$ZWIFT_DIR/ZwiftLauncher.exe"
ZWIFT_APP="$ZWIFT_DIR/ZwiftApp.exe"
WINE_HOME_DIR="$WINEPREFIX/drive_c/users/$USER"
LAUNCHER_LOG="$WINE_HOME_DIR/AppData/Local/Zwift/Logs/Launcher_log.txt"
LOG="/tmp/run-zwift.log"

on_interrupt() {
   echo
   echo "Caught interrupt - running wineserver -k to stop all wine processes in $WINEPREFIX ..."
   wineserver -k 15
   exit 130
}
trap on_interrupt INT

RELAY_KEY='HKCU\Software\Wine\Debug'
RELAY_VALUE='BleWin10Lib*;windows.devices.bluetooth*;windows.devices.enumeration*;bluetoothapis*;combase*'

# Provides install_zwift(); RELAY_KEY/RELAY_VALUE/WINEPREFIX/ZWIFT_DIR/WINE_HOME_DIR above are
# already set, so install-zwift.sh inherits them instead of using its own standalone defaults.
source "$(dirname "${BASH_SOURCE[0]}")/install-zwift.sh"

usage() {
   cat <<EOF
Usage: $(basename "$0") [--prefix NAME] [OPTIONS]

  --prefix NAME  Wine prefix directory name under \$HOME (default: .wine-zwift)
  (none)        Launch TPVirtual without debug logs
  --debug       Launch TPVirtual with WINEDEBUG logging to ./log
  --recreate    Delete Wine prefix, then install Zwift from scratch, then exit
  --reinstall   Reinstall/repatch Zwift without deleting the Wine prefix, then exit
  -h, --help    Show this help
EOF
   exit 0
}

function get_current_version() {
   # Zwift writes the version file under a version-suffixed name (eg. Zwift_ver_cur.162952.xml)
   # and records which one is authoritative in Zwift_ver_cur_filename.txt. Prefer that pointer;
   # fall back to globbing only if it's missing or stale.
   local match=""
   if [ -f Zwift_ver_cur_filename.txt ]; then
      # Zwift writes a trailing NUL byte into this file; strip it along with whitespace
      # or bash's command substitution warns "ignored null byte in input".
      match=$(tr -d '[:space:]\0' <Zwift_ver_cur_filename.txt)
   fi
   if [ -z "$match" ] || [ ! -f "$match" ]; then
      match=$(ls -1 Zwift_ver_cur.*.xml 2>/dev/null | head -1)
   fi
   ZWIFT_VERSION_CURRENT="0.0.0"
   if [ -n "$match" ] && [ -f "$match" ]; then
      ZWIFT_VERSION_CURRENT=$(grep -oP 'sversion="\K.*?(?=")' "$match" | cut -f 1 -d ' ')
      [ -z "$ZWIFT_VERSION_CURRENT" ] && ZWIFT_VERSION_CURRENT="0.0.0"
   fi
}

function get_latest_version() {
   ZWIFT_VERSION_LATEST=$(wget --quiet -O - http://cdn.zwift.com/gameassets/Zwift_Updates_Root/Zwift_ver_cur.xml | grep -oP 'sversion="\K.*?(?=")' | cut -f 1 -d ' ')
}

function wait_for_update() {
   local timeout_seconds=${1:-300}
   local timeout_counter=0
   local sleep_interval=2
   local timeout_iterations=$((timeout_seconds / sleep_interval))
   get_current_version
   get_latest_version
   until [ "$ZWIFT_VERSION_CURRENT" = "$ZWIFT_VERSION_LATEST" ]; do
      if [ "$timeout_counter" -ge "$timeout_iterations" ]; then
         echo "Timeout reached (${timeout_seconds} seconds). Breaking from update loop."
         break
      fi
      echo "updating in progress: current=$ZWIFT_VERSION_CURRENT, latest=$ZWIFT_VERSION_LATEST"
      ((timeout_counter++))
      sleep $sleep_interval
      get_current_version
   done
}

# Zwift's own Launcher/Patcher log line ("Launcher: Starting Zwift App.") is the authoritative,
# local signal that the update finished and the launcher is handing off to ZwiftApp.exe -
# no CDN round-trip and no guessing from version-string formats that can drift out of sync
# (see get_current_version/get_latest_version above, which use it only as a fallback).
function wait_for_launcher_start() {
   local timeout_seconds=${1:-300}
   local waited=0
   while [ ! -f "$LAUNCHER_LOG" ] && [ "$waited" -lt "$timeout_seconds" ]; do
      sleep 1
      ((waited++))
   done
   if [ ! -f "$LAUNCHER_LOG" ]; then
      echo "Launcher log never appeared at $LAUNCHER_LOG; falling back to version polling."
      wait_for_update "$((timeout_seconds - waited))"
      return
   fi
   echo "Waiting for \"Launcher: Starting Zwift App.\" in $LAUNCHER_LOG (timeout $((timeout_seconds - waited))s)..."
   if timeout "$((timeout_seconds - waited))" tail -n0 -F "$LAUNCHER_LOG" 2>/dev/null | grep -qm1 'Launcher: Starting Zwift App\.'; then
      echo "Launcher reported Zwift App start."
   else
      echo "Timed out waiting for launcher log line; falling back to version polling."
      wait_for_update 30
   fi
}

run_app() {
   cd "$ZWIFT_DIR" || {
      echo "Failed to change directory to $ZWIFT_DIR"
      wineserver -k 15
      exit 1
   }
   rm -f $LOG
   local debug_mode=$1
   if [ "$debug_mode" = "1" ]; then
      # Update relay registry key only when the stored value differs from RELAY_VALUE
      current=$(wine reg query "$RELAY_KEY" /v RelayInclude 2>/dev/null |
         grep -i RelayInclude | sed 's/.*REG_SZ[[:space:]]*//')
      if [ "$current" != "$RELAY_VALUE" ]; then
         wine reg add "$RELAY_KEY" /v RelayInclude /t REG_SZ /d "$RELAY_VALUE" /f
      fi
      export WINEDEBUG=+relay,+loaddll,+seh,fixme-all
      # wine "C:\Program Files (x86)\Zwift\ZwiftLauncher.exe" >& log &
   else
      unset WINEDEBUG
      # wine "C:\Program Files (x86)\Zwift\ZwiftLauncher.exe" >& log &
   fi

   wine start ZwiftLauncher.exe >&$LOG #SilentLaunch >& $LOG
   if [ $? -eq 0 ]; then
      echo "ZwiftLauncher.exe started successfully"
   else
      echo "Error: Failed to start ""$ZWIFT_LAUNCHER"
      wineserver -k 15
      exit 1
   fi

   # Adapted from https://b-ark.ca/assets/files/zwift
   get_current_version
   get_latest_version
   if [ "$ZWIFT_VERSION_CURRENT" != "$ZWIFT_VERSION_LATEST" ]; then
      # Update seems to happen after initial install, but may or may not happen for existing install.
      if [ "$ZWIFT_VERSION_CURRENT" == "0.0.0" ]; then
         wait_for_launcher_start 600
      else
         echo "Updating may be in progress and may take some time: current=$ZWIFT_VERSION_CURRENT, latest=$ZWIFT_VERSION_LATEST. Check $LAUNCHER_LOG."
         wait_for_launcher_start 30
      fi
   fi
   wine start RunFromProcess-x64.exe ZwiftLauncher.exe ZwiftApp.exe >&$LOG
   if [ $? -eq 0 ]; then
      echo "ZwiftApp.exe started successfully"
   else
      echo "Error: Failed to start ""$ZWIFT_LAUNCHER" "$ZWIFT_APP"
      wineserver -k 15
      exit 1
   fi
   sleep 10
   until pgrep -f ZwiftApp.exe &>/dev/null; do
      echo "Waiting for Zwift to start: current version=$ZWIFT_VERSION_CURRENT, latest=$ZWIFT_VERSION_LATEST"
      sleep 1
      get_current_version
   done
   echo "Killing unecessary applications"
   pkill ZwiftLauncher
   pkill MicrosoftEdgeUp
   pkill ZwiftWindowsCra

   # Wait for the user to quit Zwift, then shut the prefix down. A bare "wineserver -w"
   # is not enough: leftover background processes (crash handler, WebView helpers,
   # services.exe) keep the wineserver alive after ZwiftApp.exe exits, so -w can hang
   # forever. Instead poll for ZwiftApp.exe, then send SIGTERM to everything in the
   # prefix, and give the teardown a bounded wait with a SIGKILL fallback.
   echo "Zwift running - waiting for ZwiftApp.exe to exit..."
   while pgrep -f ZwiftApp.exe &>/dev/null; do
      sleep 5
   done
   echo "ZwiftApp.exe exited - stopping remaining wine processes in $WINEPREFIX ..."
   wineserver -k 15
   if ! timeout 30 wineserver -w; then
      echo "wineserver still up after 30s - force killing"
      wineserver -k 9
   fi
}

case "${1:-}" in
--recreate)
   install_zwift 1
   ;;
--reinstall)
   install_zwift 0
   ;;
-h | --help)
   usage
   ;;
--debug)
   run_app 1
   ;;
"")
   run_app 0
   ;;
*)
   echo "Unknown option: $1"
   usage
   ;;
esac
