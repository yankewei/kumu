#!/usr/bin/env bash
# Regenerates AppIcon.icns from app-icon.png.
#
# macOS does not round an app icon for you: the squircle is baked into the
# artwork, and AppKit only reads the image. A full-bleed opaque square (which is
# what app-icon.png is) therefore shows up in the Dock as a literal square. This
# composites the art onto Apple's macOS icon grid — an 824pt rounded tile
# centred on a 1024pt canvas, transparent outside — before packing the iconset.
#
# Usage: assets/make-icon.sh
set -euo pipefail
cd "$(dirname "$0")"

python3 - <<'PY'
from PIL import Image, ImageDraw, ImageFilter

CANVAS, TILE, RADIUS, INSET, SS = 1024, 824, 185, 100, 4

def rounded_mask(size, radius, ss):
    m = Image.new("L", (size * ss, size * ss), 0)
    ImageDraw.Draw(m).rounded_rectangle(
        [0, 0, size * ss - 1, size * ss - 1], radius=radius * ss, fill=255
    )
    return m.resize((size, size), Image.LANCZOS)

art = Image.open("app-icon.png").convert("RGB").resize((TILE, TILE), Image.LANCZOS)
mask = rounded_mask(TILE, RADIUS, SS)

canvas = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))

# Soft drop shadow.
shadow_alpha = Image.new("L", (CANVAS, CANVAS), 0)
shadow_alpha.paste(mask, (INSET, INSET + 12))
shadow_alpha = shadow_alpha.filter(ImageFilter.GaussianBlur(16)).point(lambda a: int(a * 0.35))
canvas.alpha_composite(Image.merge("RGBA", (Image.new("L", (CANVAS, CANVAS), 0),) * 3 + (shadow_alpha,)))

# The tile.
tile = Image.new("RGBA", (TILE, TILE), (0, 0, 0, 0))
tile.paste(art, (0, 0), mask)
canvas.alpha_composite(tile, (INSET, INSET))

# Faint rim so it reads as a macOS tile.
rim = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
ImageDraw.Draw(rim).rounded_rectangle(
    [INSET, INSET, INSET + TILE - 1, INSET + TILE - 1],
    radius=RADIUS, outline=(255, 255, 255, 26), width=3,
)
canvas = Image.alpha_composite(canvas, rim)
canvas.save("app-icon-1024.png")
PY

rm -rf AppIcon.iconset && mkdir AppIcon.iconset
for spec in 16:icon_16x16 32:icon_16x16@2x 32:icon_32x32 64:icon_32x32@2x \
            128:icon_128x128 256:icon_128x128@2x 256:icon_256x256 \
            512:icon_256x256@2x 512:icon_512x512; do
  px=${spec%%:*}; name=${spec#*:}
  sips -z "$px" "$px" app-icon-1024.png --out "AppIcon.iconset/$name.png" >/dev/null
done
cp app-icon-1024.png AppIcon.iconset/icon_512x512@2x.png
iconutil -c icns AppIcon.iconset -o AppIcon.icns
rm -rf AppIcon.iconset app-icon-1024.png
echo "wrote assets/AppIcon.icns"
