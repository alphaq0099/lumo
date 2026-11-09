#!/usr/bin/env sh
# Download a prebuilt pdfium for lumo (Linux x86_64 or aarch64).
# Puts libpdfium.so into ./lib so lumo finds it next to the project root.
set -eu

REV="chromium_7350"   # recent pdfium build; see https://github.com/bblanchon/pdfium-binaries
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64) A="linux-x64" ;;
  aarch64) A="linux-arm64" ;;
  *) echo "Unsupported arch: $ARCH" >&2; exit 1 ;;
esac

URL="https://github.com/bblanchon/pdfium-binaries/releases/download/${REV}/pdfium-${A}-tglc.tgz"
echo "Fetching $URL"
mkdir -p lib
curl -L "$URL" | tar xz
mv -f lib/libpdfium.so ./lib/libpdfium.so 2>/dev/null || true
echo "Done: $(ls -la lib/libpdfium.so)"
