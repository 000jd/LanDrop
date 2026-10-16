#!/usr/bin/env bash
# Install (or refresh) the desktop app for the current user — no root needed.
#
# Copies the freshly built binary and icon into ~/.local and writes a .desktop
# entry using the same filename the distribution package uses, so this copy
# shadows the system one in the application menu (XDG: user entries win).
#
# Use this after `make desktop-build`, or instead of installing the .deb when you
# do not have root.
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/landrop-desktop
if [ ! -x "$BIN" ]; then
  echo "build it first:  make desktop-build" >&2
  exit 1
fi

bindir="$HOME/.local/bin"
appdir="$HOME/.local/share/applications"
icondir="$HOME/.local/share/icons/hicolor/256x256/apps"
mkdir -p "$bindir" "$appdir" "$icondir"

install -m 0755 "$BIN" "$bindir/landrop-desktop"
install -m 0644 assets/icon.png "$icondir/landrop-desktop.png"

cat > "$appdir/LanDrop.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=LanDrop
Comment=LAN file sharing on your own network
Exec="$bindir/landrop-desktop"
Icon=landrop-desktop
Terminal=false
Categories=Network;FileTransfer;
StartupNotify=false
EOF

echo "installed for $(whoami):"
echo "  $bindir/landrop-desktop"
echo "  $appdir/LanDrop.desktop"
echo "  $icondir/landrop-desktop.png"

if command -v desktop-file-validate >/dev/null; then
  desktop-file-validate --no-hints "$appdir/LanDrop.desktop" && echo "  desktop entry: valid"
fi
if command -v update-desktop-database >/dev/null; then
  update-desktop-database "$appdir" >/dev/null 2>&1 || true
fi

echo "launch it from the application menu, or run: $bindir/landrop-desktop"
