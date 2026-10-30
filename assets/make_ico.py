"""Make assets/icon.ico (the icon embedded in syncview.exe) from assets/icon.svg.

Renders each size with rsvg-convert and stores them as PNG entries (Windows Vista and later).
Run from the repository root: python3 assets/make_ico.py
"""

import struct
import subprocess

SIZES = [16, 24, 32, 48, 64, 128, 256]

pngs = [subprocess.run(["rsvg-convert", "-w", str(s), "-h", str(s), "assets/icon.svg"], check=True, capture_output=True).stdout for s in SIZES]
out = struct.pack("<HHH", 0, 1, len(SIZES))
offset = 6 + 16 * len(SIZES)
for s, png in zip(SIZES, pngs):
    # width and height 0 mean 256
    out += struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(png), offset)
    offset += len(png)
with open("assets/icon.ico", "wb") as f:
    f.write(out + b"".join(pngs))
