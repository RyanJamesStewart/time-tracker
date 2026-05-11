#!/usr/bin/env bash
# Regenerate src/tray-icon-32.rgba from src/favicon.svg.
#
# The tray icon is loaded at runtime as a raw 32x32 RGBA buffer (4096 bytes)
# embedded via include_bytes! — that way the binary doesn't pull a PNG decoder
# or a font renderer just to draw 1024 pixels in the system tray. Run this
# script whenever favicon.svg changes.
#
# Deps: rsvg-convert (libgssvg), python3 + Pillow.
set -euo pipefail
cd "$(dirname "$0")/.."

SVG=src/favicon.svg
OUT=src/tray-icon-32.rgba
TMP_PNG=$(mktemp --suffix=.png)
trap 'rm -f "$TMP_PNG"' EXIT

rsvg-convert "$SVG" --width 32 --height 32 --format png --output "$TMP_PNG"
python3 - "$TMP_PNG" "$OUT" <<'PY'
import sys
from PIL import Image
src, dst = sys.argv[1], sys.argv[2]
im = Image.open(src).convert('RGBA')
assert im.size == (32, 32), f"expected 32x32, got {im.size}"
with open(dst, 'wb') as f:
    f.write(im.tobytes())  # raw RGBA, row-major
PY

bytes=$(stat -c%s "$OUT")
[ "$bytes" -eq 4096 ] || { echo "expected 4096 bytes, got $bytes"; exit 1; }
echo "wrote $OUT ($bytes bytes)"
