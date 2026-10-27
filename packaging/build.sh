#!/usr/bin/env bash
# Builds the release binary and packages it as an AppImage, .deb and .rpm into
# dist/. Used by CI (.github/workflows/release.yml) and runnable locally.
#
# Needs: cargo, cargo-deb, cargo-generate-rpm, and appimagetool on PATH
# (or APPIMAGETOOL=/path/to/it). Skip a format with SKIP="appimage deb rpm".
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(cargo metadata --no-deps --format-version 1 | python3 -c \
  'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="sophon-app"))')
ARCH=$(uname -m)
SKIP=" ${SKIP:-} "
rm -rf dist && mkdir -p dist

cargo build --release --locked -p sophon-app

if [[ $SKIP != *" deb "* ]]; then
  cargo deb --no-build -p sophon-app --output "dist/sophon_${VERSION}_amd64.deb"
fi

if [[ $SKIP != *" rpm "* ]]; then
  cargo generate-rpm -p crates/sophon-app --output "dist/sophon-${VERSION}-1.${ARCH}.rpm"
fi

if [[ $SKIP != *" appimage "* ]]; then
  APPDIR=$(mktemp -d)/Sophon.AppDir
  mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/256x256/apps"
  install -m755 target/release/sophon "$APPDIR/usr/bin/sophon"
  install -m644 packaging/sophon.desktop "$APPDIR/usr/share/applications/sophon.desktop"
  install -m644 packaging/icons/sophon-256.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/sophon.png"
  cp packaging/sophon.desktop "$APPDIR/sophon.desktop"
  cp packaging/icons/sophon-256.png "$APPDIR/sophon.png"
  ln -s usr/bin/sophon "$APPDIR/AppRun"
  ARCH=$ARCH APPIMAGE_EXTRACT_AND_RUN=1 "${APPIMAGETOOL:-appimagetool}" "$APPDIR" "dist/Sophon-${VERSION}-${ARCH}.AppImage"
fi

(cd dist && sha256sum * > SHA256SUMS)
ls -lh dist
