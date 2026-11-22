#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

BIN="$ROOT/target/release/mister-cover-scraper"
if [[ ! -x "$BIN" ]]; then
  echo "No se encontró $BIN. Compila primero con: cargo build --release --locked" >&2
  exit 1
fi

WORK="${WORKDIR:-$ROOT/target/appimage}"
TOOLS="$WORK/tools"
APPDIR="$WORK/AppDir"
mkdir -p "$TOOLS"

fetch_tool() {
  local url="$1"
  local dest="$2"
  if [[ ! -x "$dest" ]]; then
    echo "Descargando $(basename "$dest")..."
    curl -fL --retry 3 -o "$dest" "$url"
    chmod +x "$dest"
  fi
}

fetch_tool \
  "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage" \
  "$TOOLS/linuxdeploy-x86_64.AppImage"
fetch_tool \
  "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage" \
  "$TOOLS/appimagetool-x86_64.AppImage"
fetch_tool \
  "https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-x86_64" \
  "$TOOLS/runtime-x86_64"

rm -rf "$APPDIR"
mkdir -p "$APPDIR"

export PATH="$TOOLS:$PATH"
export APPIMAGE_EXTRACT_AND_RUN=1
export NO_STRIP=true
export ARCH=x86_64

"$TOOLS/linuxdeploy-x86_64.AppImage" \
  --appdir "$APPDIR" \
  --desktop-file "$ROOT/packaging/linux/mister-cover-scraper.desktop" \
  --icon-file "$ROOT/packaging/linux/mister-cover-scraper.svg" \
  --executable "$BIN"

if command -v rsvg-convert >/dev/null 2>&1; then
  rsvg-convert -w 256 -h 256 \
    "$ROOT/packaging/linux/mister-cover-scraper.svg" \
    -o "$APPDIR/usr/share/icons/hicolor/256x256/apps/mister-cover-scraper.png"
  cp "$APPDIR/usr/share/icons/hicolor/256x256/apps/mister-cover-scraper.png" \
    "$APPDIR/mister-cover-scraper.png"
fi

OUTPUT="$ROOT/mister-cover-scraper-linux-x86_64.AppImage"
rm -f "$OUTPUT"
"$TOOLS/appimagetool-x86_64.AppImage" \
  --runtime-file "$TOOLS/runtime-x86_64" \
  "$APPDIR" \
  "$OUTPUT"

echo "AppImage generado: $OUTPUT"
