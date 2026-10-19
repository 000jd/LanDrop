#!/usr/bin/env bash
# LanDrop one-command installer for Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/000jd/LanDrop/main/install/install.sh | bash
#   curl -fsSL ... | bash -s -- --desktop
#   curl -fsSL ... | bash -s -- --uninstall
#
# Environment:
#   LANDROP_VERSION   Release tag to install (default: latest, fallback v0.1.0)
#   LANDROP_DOWNLOADS Override the release download base URL (mirrors/tests)
#
# No sudo is ever required: everything lands under ~/.local.

set -euo pipefail

REPO="000jd/LanDrop"
DEFAULT_TAG="v0.1.0"
CLI_NAME="landrop"
DESKTOP_NAME="landrop-desktop"

DRY_RUN=0
WANT_DESKTOP=0
UNINSTALL=0

usage() {
  cat <<'EOF'
LanDrop installer

Usage:
  install.sh [options]

Options:
  --desktop     also install the Linux desktop app (window + tray)
  --dry-run     print what would be downloaded and installed, change nothing
  --uninstall   remove the files this installer created
  -h, --help    show this help

Environment:
  LANDROP_VERSION   release tag to install (default: latest, fallback v0.1.0)
  LANDROP_DOWNLOADS override the download base URL (mirrors/tests)
EOF
}

log() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --desktop) WANT_DESKTOP=1 ;;
      --dry-run) DRY_RUN=1 ;;
      --uninstall) UNINSTALL=1 ;;
      -h | --help)
        usage
        exit 0
        ;;
      --)
        shift
        break
        ;;
      *)
        usage >&2
        die "unknown option: $1"
        ;;
    esac
    shift
  done
  if [ "$#" -gt 0 ]; then
    die "unexpected argument: $1"
  fi
}

parse_args "$@"

if [ -z "${HOME:-}" ]; then
  die "HOME is not set"
fi

BIN_DIR="${HOME}/.local/bin"
APPS_DIR="${HOME}/.local/share/applications"
ICON_ROOT="${HOME}/.local/share/icons/hicolor"

do_uninstall() {
  local removed=0
  local path
  for path in \
    "${BIN_DIR}/${CLI_NAME}" \
    "${BIN_DIR}/${DESKTOP_NAME}" \
    "${APPS_DIR}/landrop.desktop" \
    "${APPS_DIR}/LanDrop.desktop" \
    "${ICON_ROOT}/scalable/apps/landrop.svg" \
    "${ICON_ROOT}/256x256/apps/landrop.png" \
    "${ICON_ROOT}/256x256/apps/landrop-desktop.png"; do
    if [ -e "$path" ] || [ -L "$path" ]; then
      if [ "$DRY_RUN" -eq 1 ]; then
        log "would remove $path"
      else
        rm -f "$path"
        log "removed $path"
      fi
      removed=1
    fi
  done
  if [ "$removed" -eq 0 ]; then
    log "LanDrop is not installed under ${HOME}/.local."
  elif [ "$DRY_RUN" -eq 0 ] && command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPS_DIR" >/dev/null 2>&1 || true
  fi
  log "LanDrop uninstalled."
  exit 0
}

if [ "$UNINSTALL" -eq 1 ]; then
  do_uninstall
fi

detect_platform() {
  case "$(uname -s)" in
    Linux) OS="linux" ;;
    Darwin) OS="macos" ;;
    *) die "unsupported operating system: $(uname -s) (Linux and macOS only)" ;;
  esac
  case "$(uname -m)" in
    x86_64 | amd64) ARCH="x86_64" ;;
    aarch64 | arm64) ARCH="arm64" ;;
    *) die "unsupported CPU architecture: $(uname -m)" ;;
  esac
}

detect_platform

normalize_tag() {
  case "$1" in
    v*) printf '%s' "$1" ;;
    *) printf 'v%s' "$1" ;;
  esac
}

resolve_tag() {
  if [ -n "${LANDROP_VERSION:-}" ]; then
    normalize_tag "$LANDROP_VERSION"
    return
  fi
  local tag=""
  tag="$(
    curl -fsSL --max-time 10 \
      "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null \
      | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
      | awk 'NR == 1 { print; exit }' || true
  )"
  if [ -n "$tag" ]; then
    normalize_tag "$tag"
  else
    printf '%s' "$DEFAULT_TAG"
  fi
}

TAG="$(resolve_tag)"
DOWNLOAD_BASE="${LANDROP_DOWNLOADS:-https://github.com/${REPO}/releases/download/${TAG}}"

if [ "$DRY_RUN" -eq 1 ]; then
  log "LanDrop installer (dry run)"
  log "  release:  ${TAG}"
  log "  platform: ${OS}/${ARCH}"
  log "  CLI:      LanDrop-${TAG}-${OS}-${ARCH}"
  if [ "$OS" = "linux" ]; then
    log "  desktop:  LanDrop-${TAG}-linux-${ARCH}-desktop (or .AppImage)"
    log "  install:  ${BIN_DIR}/${CLI_NAME}"
    log "            ${BIN_DIR}/${DESKTOP_NAME} (desktop assets, when available)"
    log "            ${APPS_DIR}/landrop.desktop"
  else
    log "  install:  ${BIN_DIR}/${CLI_NAME}"
  fi
  exit 0
fi

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v install >/dev/null 2>&1 || die "the install utility is required"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/landrop-install.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

SUMS="${TMP}/SHA256SUMS.txt"

asset_url() {
  printf '%s/%s' "$DOWNLOAD_BASE" "$1"
}

fetch() {
  curl -fsSL --retry 3 --retry-delay 1 --connect-timeout 15 -o "$2" "$1"
}

asset_exists() {
  curl -fsIL --retry 2 --connect-timeout 10 --max-time 20 "$(asset_url "$1")" \
    >/dev/null 2>&1
}

fetch_sums() {
  fetch "$(asset_url SHA256SUMS.txt)" "$SUMS" \
    || die "could not download SHA256SUMS.txt for ${TAG}"
}

download_asset() {
  local asset="$1" dest="$2"
  log "  downloading ${asset}"
  fetch "$(asset_url "$asset")" "$dest" \
    || die "failed to download ${asset}"
}

verify_checksum() {
  local file="$1" base="${2:-}" expected actual
  if [ -z "$base" ]; then
    base="$(basename "$file")"
  fi
  expected="$(
    awk -v target="$base" '{
      name = $2
      sub(/^\*/, "", name)
      if (name == target) { print $1; exit }
    }' "$SUMS"
  )"
  if [ -z "$expected" ]; then
    die "SHA256SUMS.txt has no entry for ${base}"
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$file" | awk '{ print $1 }')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$file" | awk '{ print $1 }')"
  else
    die "need sha256sum or shasum to verify downloads"
  fi
  if [ "$expected" != "$actual" ]; then
    die "checksum mismatch for ${base} (expected ${expected}, got ${actual})"
  fi
  log "  checksum OK: ${base}"
}

install_from_cargo() {
  command -v cargo >/dev/null 2>&1 || return 1
  log "No prebuilt binary for ${OS}/${ARCH}; installing from source with cargo..."
  cargo install --git "https://github.com/${REPO}" --tag "$TAG" --locked \
    --root "${HOME}/.local" --force
}

install_cli() {
  local asset="LanDrop-${TAG}-${OS}-${ARCH}"
  if asset_exists "$asset"; then
    fetch_sums
    download_asset "$asset" "${TMP}/${asset}"
    verify_checksum "${TMP}/${asset}"
    mkdir -p "$BIN_DIR"
    install -m 0755 "${TMP}/${asset}" "${BIN_DIR}/${CLI_NAME}"
    log "  installed ${BIN_DIR}/${CLI_NAME}"
  elif install_from_cargo; then
    log "  installed ${BIN_DIR}/${CLI_NAME} (cargo)"
  else
    die "no prebuilt binary for ${OS}/${ARCH} at ${TAG}, and cargo is not installed"
  fi
}

download_optional() {
  local asset="$1" dest="$2"
  asset_exists "$asset" || return 1
  fetch_sums
  download_asset "$asset" "$dest"
  verify_checksum "$dest" "$asset"
}

install_desktop() {
  [ "$OS" = "linux" ] || return 0

  local want=0
  if [ "$WANT_DESKTOP" -eq 1 ]; then
    want=1
  elif [ -n "${DISPLAY:-}" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; then
    want=1
  fi
  [ "$want" -eq 1 ] || return 0

  local desktop_asset="LanDrop-${TAG}-linux-${ARCH}-desktop"
  local appimage_asset="LanDrop-${TAG}-linux-${ARCH}.AppImage"
  local target="${TMP}/${DESKTOP_NAME}"

  if download_optional "$desktop_asset" "$target"; then
    :
  elif download_optional "$appimage_asset" "$target"; then
    :
  else
    if [ "$WANT_DESKTOP" -eq 1 ]; then
      warn "no Linux desktop asset found for ${TAG}; installed the CLI only"
    fi
    return 0
  fi

  mkdir -p "$BIN_DIR" "$APPS_DIR" \
    "${ICON_ROOT}/scalable/apps" "${ICON_ROOT}/256x256/apps"
  install -m 0755 "$target" "${BIN_DIR}/${DESKTOP_NAME}"

  # Icons come from the tagged source tree (best effort; the app still runs
  # without them).
  local icon_base="https://raw.githubusercontent.com/${REPO}/${TAG}/assets"
  if fetch "${icon_base}/icon.svg" "${TMP}/icon.svg" 2>/dev/null; then
    install -m 0644 "${TMP}/icon.svg" "${ICON_ROOT}/scalable/apps/landrop.svg"
  fi
  if fetch "${icon_base}/icon.png" "${TMP}/icon.png" 2>/dev/null; then
    install -m 0644 "${TMP}/icon.png" "${ICON_ROOT}/256x256/apps/landrop.png"
  fi

  cat >"${APPS_DIR}/landrop.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=LanDrop
Comment=LAN file sharing on your own network
Exec=${BIN_DIR}/${DESKTOP_NAME}
Icon=landrop
Terminal=false
Categories=Network;FileTransfer;
StartupNotify=false
EOF
  chmod 0644 "${APPS_DIR}/landrop.desktop"
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPS_DIR" >/dev/null 2>&1 || true
  fi

  log "  installed ${BIN_DIR}/${DESKTOP_NAME}"
  log "  installed ${APPS_DIR}/landrop.desktop"
  DESKTOP_INSTALLED=1
}

DESKTOP_INSTALLED=0
install_cli
install_desktop

path_has_bin=0
case ":${PATH:-}:" in
  *":${BIN_DIR}:"*) path_has_bin=1 ;;
esac

log ""
log "LanDrop ${TAG} installed."
log "  CLI: ${BIN_DIR}/${CLI_NAME}"
if [ "$DESKTOP_INSTALLED" -eq 1 ]; then
  log "  Desktop: ${BIN_DIR}/${DESKTOP_NAME} (also in your application menu)"
fi
log ""
if [ "$path_has_bin" -eq 1 ]; then
  log "Run: ${CLI_NAME}"
  if [ "$DESKTOP_INSTALLED" -eq 1 ]; then
    log "Or launch LanDrop from your application menu."
  fi
else
  log "Run: ${BIN_DIR}/${CLI_NAME}"
  log ""
  log "${BIN_DIR} is not on your PATH. Add it to your shell profile:"
  log "  export PATH=\"\$HOME/.local/bin:\$PATH\""
  log "Then restart your shell (or run: source ~/.bashrc)."
fi
