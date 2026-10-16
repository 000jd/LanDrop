#!/usr/bin/env bash
# Validate the desktop entries we ship — and, when one exists, the autostart
# entry we generate — with desktop-file-validate (package: desktop-file-utils).
#
# Skips with a note when the tool is not installed, so it is safe to call from a
# developer machine or a minimal container.
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v desktop-file-validate >/dev/null; then
  echo "desktop-file-validate not installed — skipping (apt: desktop-file-utils)" >&2
  exit 0
fi

status=0
files=(assets/landrop.desktop)
autostart="${XDG_CONFIG_HOME:-$HOME/.config}/autostart/landrop.desktop"
if [ -f "$autostart" ]; then
  files+=("$autostart")
else
  echo "note: no autostart entry at $autostart (enable it in Settings to create one)"
fi

for f in "${files[@]}"; do
  echo "== $f"
  desktop-file-validate --no-hints "$f" || status=1
done

exit "$status"
