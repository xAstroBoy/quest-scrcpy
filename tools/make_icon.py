#!/usr/bin/env python3
"""Generate the app icon (a VR-headset glyph on a gradient) at every size and
write a multi-resolution Windows .ico plus a 256px PNG for the runtime window
icon. Re-run after tweaking the design:  python tools/make_icon.py
"""
from PIL import Image, ImageDraw

# Render the master big, then downsample — keeps the small sizes crisp.
MASTER = 1024
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]

TOP = (0x6D, 0x5B, 0xD0)     # indigo
BOTTOM = (0x2C, 0x9C, 0xE8)  # blue


def gradient(size, top, bottom):
    img = Image.new("RGB", (size, size))
    px = img.load()
    for y in range(size):
        t = y / (size - 1)
        r = round(top[0] * (1 - t) + bottom[0] * t)
        g = round(top[1] * (1 - t) + bottom[1] * t)
        b = round(top[2] * (1 - t) + bottom[2] * t)
        row = (r, g, b)
        for x in range(size):
            px[x, y] = row
    return img


def master():
    s = MASTER
    # Rounded-square background filled with the gradient.
    bg = gradient(s, TOP, BOTTOM).convert("RGBA")
    corner = Image.new("L", (s, s), 0)
    ImageDraw.Draw(corner).rounded_rectangle([0, 0, s - 1, s - 1], radius=int(s * 0.22), fill=255)
    bg.putalpha(corner)

    # White VR-headset visor as an alpha mask: visor solid, lenses punched out.
    visor = Image.new("L", (s, s), 0)
    d = ImageDraw.Draw(visor)
    # Visor body (wide rounded capsule), nudged just below centre.
    d.rounded_rectangle([s * 0.17, s * 0.36, s * 0.83, s * 0.66], radius=int(s * 0.14), fill=255)
    # Side strap tabs for a headset silhouette.
    d.rounded_rectangle([s * 0.10, s * 0.45, s * 0.20, s * 0.57], radius=int(s * 0.04), fill=255)
    d.rounded_rectangle([s * 0.80, s * 0.45, s * 0.90, s * 0.57], radius=int(s * 0.04), fill=255)
    # Two lens cut-outs (reveal the gradient), nose bridge stays white between them.
    d.rounded_rectangle([s * 0.25, s * 0.43, s * 0.475, s * 0.60], radius=int(s * 0.075), fill=0)
    d.rounded_rectangle([s * 0.525, s * 0.43, s * 0.75, s * 0.60], radius=int(s * 0.075), fill=0)

    white = Image.new("RGBA", (s, s), (255, 255, 255, 255))
    bg.paste(white, (0, 0), visor)
    return bg


def main():
    import os
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    assets = os.path.join(here, "assets")
    os.makedirs(assets, exist_ok=True)

    m = master()
    m.resize((256, 256), Image.LANCZOS).save(os.path.join(assets, "icon-256.png"))
    m.save(
        os.path.join(assets, "icon.ico"),
        format="ICO",
        sizes=[(n, n) for n in ICO_SIZES],
    )
    print("wrote assets/icon.ico and assets/icon-256.png")


if __name__ == "__main__":
    main()
