#!/bin/bash

# Download ZwiftSetup.exe from https://www.zwift.com/download and place in downloads directory
# Download and uncompress RunFromProcess-x64.exe from https://www.nirsoft.net/utils/runfromprocess.zip and place all the 
# files from the zip in downloads/.

# downloads/ should look like:
# MicrosoftEdgeWebview2Setup.exe
# RunFromProcess.exe
# RunFromProcess-x64.exe
# ZwiftSetup.exe


# This file is meant to be sourced from run-zwift.sh (which calls install_zwift for its
# --recreate/--reinstall/--prefix options and already defines PREFIX_NAME/RELAY_KEY/RELAY_VALUE
# etc.), but it can also be run standalone: ./install-zwift.sh [--reinstall] [--prefix NAME].
_install_zwift_standalone_reinstall=0
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
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
      --reinstall)
         _install_zwift_standalone_reinstall=1
         shift
         ;;
      -h | --help)
         echo "Usage: $(basename "$0") [--prefix NAME] [--reinstall]"
         exit 0
         ;;
      *)
         echo "Unknown option: $1" >&2
         exit 1
         ;;
      esac
   done
fi

# ${VAR:=...} defaults below let this still work when run standalone (./install-zwift.sh)
# without --prefix, or when sourced without PREFIX_NAME already set.
: "${PREFIX_NAME:=.wine-zwift}"
: "${WINEPREFIX:=$HOME/$PREFIX_NAME}"
: "${ZWIFT_DIR:=$WINEPREFIX/drive_c/Program Files (x86)/Zwift}"
: "${WINE_HOME_DIR:=$WINEPREFIX/drive_c/users/$USER}"
: "${RELAY_KEY:=HKCU\Software\Wine\Debug}"
: "${RELAY_VALUE:=BleWin10Lib*;windows.devices.bluetooth*;windows.devices.enumeration*;bluetoothapis*;combase*}"
export WINEPREFIX

ZWIFT_DIR_BACKUP="$WINEPREFIX/drive_c/Program Files (x86)/Zwift-Install"
DOWNLOAD_RUNNER=$(realpath downloads/RunFromProcess-x64.exe) # https://www.nirsoft.net/utils/runfromprocess.zip
RUNNER="$ZWIFT_DIR/RunFromProcess-x64.exe"

# install_zwift <recreate: 1|0>
# recreate=1 wipes $WINEPREFIX first (fresh install); recreate=0 reinstalls/repatches into
# the existing prefix (eg. after a Zwift update breaks something, without losing Wine config).
install_zwift() {
   local recreate="${1:-1}"

   if [ "$recreate" = "1" ]; then
      echo "Deleting $WINEPREFIX ..."
      rm -rf "$WINEPREFIX"
   fi

   #Install winetricks: dnf install winetricks or pacman -S winetricks
   if ! command -v winetricks &>/dev/null; then
      echo "Error: winetricks not found in PATH. Please install it (e.g., 'dnf install winetricks' or 'pacman -S winetricks')"
      wineserver -k 15
      exit 1
   fi

   WINEARCH=win64 WINEDLLOVERRIDES="mscoree,mshtml=" wineboot --init
   #WINEARCH=win64 WINEDLLOVERRIDES="mshtml=" wineboot --init

   #Wine seems to create a symlink from your account Documents directory to the default
   #$WINEPREFIX/drive_c/users/$USER/Documents directory. This seems to result in the Zwift installer giving
   #a `Error code Z117 at Line 616 in Patcher.cpp` error even when the target directory has 0777 permissions.
   #To avoid this, delete the symlink and create a new Documents directory in the Zwift Wine prefix
   if [ -L "$WINE_HOME_DIR/Documents" ]; then
      echo "Removing symlink $WINE_HOME_DIR/Documents and creating a new Documents directory (Zwift error Z117 fix)"
      rm -f "$WINE_HOME_DIR/Documents"
      mkdir -p "$WINE_HOME_DIR/Documents"
   fi

   winetricks settings win10
   winetricks -q win10
   winetricks corefonts vcredist2022 d3dcompiler_47 dxvk


   # Fedora only wine-mono iconv.dll fix if iconv.dll not found for wine-mono
   # local iconv64="/usr/x86_64-w64-mingw32/sys-root/mingw/bin/iconv.dll" # mingw64-win-iconv
   # local iconv32="/usr/i686-w64-mingw32/sys-root/mingw/bin/iconv.dll"   # mingw32-win-iconv
   # if [ -e "$iconv64" ] && [ ! -e "$WINEPREFIX/drive_c/windows/system32/iconv.dll" ]; then
   #    echo "Copying $iconv64 into the Wine prefix (Fedora wine-mono iconv.dll fix)"
   #    cp "$iconv64" "$WINEPREFIX/drive_c/windows/system32/"
   # fi
   # if [ -e "$iconv32" ] && [ -d "$WINEPREFIX/drive_c/windows/syswow64" ] &&
   #    [ ! -e "$WINEPREFIX/drive_c/windows/syswow64/iconv.dll" ]; then
   #    echo "Copying $iconv32 into the Wine prefix (Fedora wine-mono iconv.dll fix, 32-bit)"
   #    cp "$iconv32" "$WINEPREFIX/drive_c/windows/syswow64/"
   # fi

   # if [ ! -d "/usr/share/wine/mono" ]; then
   #    read -p "wine-mono not installed. Continue with winetricks dotnet or N to exit and install? (Y to continue, N to exit)" -n 1 -r
   #    echo
   #    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
   #       wineserver -k 15
   #       exit 1
   #    fi
   #    winetricks dotnet472 # in installed directory
   #    # winetricks dotnet48
   # fi
   #winetricks dotnet48
   winetricks dotnet472 # in Zwifts installed directory as well

   local current
   current=$(wine reg query "$RELAY_KEY" /v RelayInclude 2>/dev/null |
      grep -i RelayInclude | sed 's/.*REG_SZ[[:space:]]*//')
   if [ "$current" != "$RELAY_VALUE" ]; then
      wine reg add "$RELAY_KEY" /v RelayInclude /t REG_SZ /d "$RELAY_VALUE" /f
   fi
   export WINEDEBUG=+relay,+loaddll,+seh,fixme-all

   wine "$(realpath downloads/MicrosoftEdgeWebview2Setup.exe)" # This is the one the Zwift run tries to download if it says webview not found.

   wine "$(realpath downloads/ZwiftSetup.exe)" /SP- /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /NOCANCEL >&/tmp/install.log

   if [ $? -eq 0 ]; then
      echo "Zwift install OK"
   else
      echo "Install Error"
      wineserver -k 15
      exit 1
   fi

   cd "$ZWIFT_DIR" || {
      echo "Install failed. Could not change directory to $ZWIFT_DIR"
      wineserver -k 15
      exit 1
   }

   if [ ! -e "$RUNNER" ]; then
      if [ ! -e "$DOWNLOAD_RUNNER" ]; then
         echo "Error: $DOWNLOAD_RUNNER does not exist. Could not copy to $RUNNER"
         wineserver -k 15
         exit 1
      fi
      cp "$DOWNLOAD_RUNNER" "$RUNNER"
   fi

   # Zwift writes the version file under a version-suffixed name (eg. Zwift_ver_cur.162952.xml),
   local verfile
   verfile=$(ls -1 Zwift_ver_cur.*.xml 2>/dev/null | head -1)
   [ -n "$verfile" ] && cat "$verfile"

   wineserver -k 15
#   mkdir -p "$ZWIFT_DIR_BACKUP"
#   rsync -avzz --delete "$ZWIFT_DIR"/ "$ZWIFT_DIR_BACKUP"/ # Make a backup of the initial install directory to compare against after running.

   echo "Wine prefix: $WINEPREFIX"
   echo "Zwift install directory: $ZWIFT_DIR"
}

# Allow direct standalone use (./install-zwift.sh [--reinstall] [--prefix NAME]) as well as
# being sourced; --prefix/--reinstall are parsed at the top of this file, before PREFIX_NAME's
# default is applied.
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
   if [ "$_install_zwift_standalone_reinstall" = "1" ]; then
      install_zwift 0
   else
      install_zwift 1
   fi
fi
