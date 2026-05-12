#!/usr/bin/env bash
# Regenerate the MSIX visual assets from src/favicon.svg.
#
# These are the *real* app icons (the same "db" mark the WebView surfaces use),
# not the procedural placeholders from msix/generate-placeholder-icons.ps1.
# They're committed to the repo (git add -f, since /msix/Assets/ is otherwise
# ignored) so a clean checkout can build a branded MSIX without a Windows box.
#
# Requires: rsvg-convert + ImageMagick `convert` (apt: librsvg2-bin imagemagick).
# Run from the repo root:  ./scripts/regen-msix-assets.sh
set -euo pipefail

cd "$(dirname "$0")/.."
SVG="src/favicon.svg"
OUT="msix/Assets"
mkdir -p "$OUT"

[ -f "$SVG" ] || { echo "missing $SVG" >&2; exit 1; }
command -v rsvg-convert >/dev/null || { echo "need rsvg-convert (apt install librsvg2-bin)" >&2; exit 1; }
command -v convert >/dev/null || { echo "need ImageMagick convert" >&2; exit 1; }

# Square logos: straight render of the rounded-rect mark.
rsvg-convert -w 150 -h 150 "$SVG" -o "$OUT/Square150x150Logo.png"
rsvg-convert -w  44 -h  44 "$SVG" -o "$OUT/Square44x44Logo.png"
rsvg-convert -w  50 -h  50 "$SVG" -o "$OUT/StoreLogo.png"

# Wide tile: 150x150 glyph centered on a 310x150 transparent canvas.
rsvg-convert -w 150 -h 150 "$SVG" -o "$OUT/_wide-tmp.png"
convert "$OUT/_wide-tmp.png" -background none -gravity center -extent 310x150 "$OUT/Wide310x150Logo.png"
rm -f "$OUT/_wide-tmp.png"

echo "regenerated:"
for f in Square150x150Logo Square44x44Logo StoreLogo Wide310x150Logo; do
  identify "$OUT/$f.png" 2>/dev/null || file "$OUT/$f.png"
done
echo
echo "commit them with:  git add -f msix/Assets/*.png"
