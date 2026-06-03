#!/usr/bin/env python3
"""Generate the runic-tray executable icon (runic.ico).

Renders the Raidho rune (ᚱ) from the SAME stroke geometry the tray paints at
runtime (`RAIDHO_STROKES` in ../src/main.rs), so the Explorer / alt-tab / taskbar
icon matches the tray icon pixel-for-pixel. Teal, transparent background, no
decoration. Multi-size .ico (16/32/48/256), each size rendered natively (not
downscaled) for crispness.

Usage:  python gen_icon.py            # writes runic.ico next to this script
Deps:   Pillow  (pip install pillow)

Keep this in sync with RAIDHO_STROKES / the teal colour in src/main.rs.
"""
import io
import struct
from pathlib import Path

from PIL import Image

# Mirror of RAIDHO_STROKES in ../src/main.rs: (x0, y0, x1, y1) in a [0,1] box.
RAIDHO_STROKES = [
    (0.20, 0.06, 0.20, 0.94),  # stave - left vertical, full height
    (0.20, 0.06, 0.66, 0.28),  # top of stave -> upper-right peak
    (0.66, 0.28, 0.20, 0.50),  # peak -> middle of stave (closes the bowl)
    (0.20, 0.50, 0.70, 0.94),  # middle -> bottom-right leg
]
TEAL = (0x14, 0x9E, 0x9E, 0xFF)  # IconState::Running colour
SIZES = [16, 32, 48, 256]


def render(size: int) -> Image.Image:
    """Stamp the rune into a transparent square, matching raidho_icon()."""
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    px = img.load()
    half = max(size / 8.0, 1.5) / 2.0  # stroke thickness/2, same as the tray
    for (x0n, y0n, x1n, y1n) in RAIDHO_STROKES:
        x0, y0, x1, y1 = x0n * size, y0n * size, x1n * size, y1n * size
        dx, dy = x1 - x0, y1 - y0
        len2 = max(dx * dx + dy * dy, 1e-6)
        for py in range(size):
            for pxx in range(size):
                fx, fy = pxx + 0.5, py + 0.5
                t = ((fx - x0) * dx + (fy - y0) * dy) / len2
                t = 0.0 if t < 0.0 else (1.0 if t > 1.0 else t)
                cx, cy = x0 + t * dx, y0 + t * dy
                if ((fx - cx) ** 2 + (fy - cy) ** 2) ** 0.5 <= half:
                    px[pxx, py] = TEAL
    return img


def main() -> None:
    out = Path(__file__).with_name("runic.ico")
    # Assemble the .ico by hand: Pillow's ICO writer collapses to a single size
    # here, so we render each size natively, PNG-encode it, and lay out the
    # ICONDIR / ICONDIRENTRY table ourselves. PNG-compressed entries are valid
    # since Windows Vista and handled by windres.
    blobs = []
    for s in SIZES:
        buf = io.BytesIO()
        render(s).save(buf, format="PNG")
        blobs.append(buf.getvalue())

    header = struct.pack("<HHH", 0, 1, len(SIZES))  # reserved, type=icon, count
    offset = 6 + 16 * len(SIZES)
    entries = b""
    for s, data in zip(SIZES, blobs):
        dim = 0 if s >= 256 else s  # 0 encodes 256 in the ICONDIRENTRY
        entries += struct.pack(
            "<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset
        )
        offset += len(data)

    out.write_bytes(header + entries + b"".join(blobs))
    print(f"wrote {out} ({', '.join(f'{s}x{s}' for s in SIZES)})")


if __name__ == "__main__":
    main()
