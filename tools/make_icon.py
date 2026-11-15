"""Generates assets/icon.png (256x256) and assets/icon.ico (16-256 px) for SwiftPDF.
Pure Python, no dependencies:  python tools/make_icon.py   (run from the project root)"""
import math
import os
import struct
import zlib


def lerp(a, b, t):
    return a + (b - a) * t


def sdf_round_rect(px, py, cx, cy, hw, hh, r):
    qx, qy = abs(px - cx) - (hw - r), abs(py - cy) - (hh - r)
    return math.hypot(max(qx, 0.0), max(qy, 0.0)) + min(max(qx, qy), 0.0) - r


def shade(x, y):
    """Colour (r, g, b, a) at normalised point (x, y); a == 0 outside the icon shape."""
    if sdf_round_rect(x, y, 0.5, 0.5, 0.47, 0.47, 0.22) > 0:
        return (0, 0, 0, 0)
    t = (x + y) / 2.0
    r, g, b = lerp(104, 58, t), lerp(148, 86, t), lerp(255, 232, t)  # blue gradient
    if sdf_round_rect(x, y, 0.5, 0.52, 0.19, 0.25, 0.045) < 0:  # white page
        r, g, b = 255, 255, 255
        for ly, x1 in ((0.40, 0.62), (0.51, 0.62), (0.62, 0.53)):  # text lines
            if abs(y - ly) < 0.02 and 0.385 <= x <= x1:
                r, g, b = 70, 100, 240
    return (r, g, b, 255)


def render(size):
    ss = 4 if size <= 64 else 3
    out = bytearray()
    for j in range(size):
        out.append(0)  # PNG row filter: none
        for i in range(size):
            ar = ag = ab = aa = 0.0
            for sj in range(ss):
                for si in range(ss):
                    x = (i + (si + 0.5) / ss) / size
                    y = (j + (sj + 0.5) / ss) / size
                    r, g, b, a = shade(x, y)
                    ar += r * a
                    ag += g * a
                    ab += b * a
                    aa += a
            n = ss * ss
            if aa == 0:
                out += bytes((0, 0, 0, 0))
            else:
                out += bytes((int(ar / aa), int(ag / aa), int(ab / aa), int(aa / n)))
    return bytes(out)


def png_bytes(size):
    raw = render(size)

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")


os.makedirs("assets", exist_ok=True)
sizes = [16, 24, 32, 48, 64, 128, 256]
images = {sz: png_bytes(sz) for sz in sizes}
open("assets/icon.png", "wb").write(images[256])

header = struct.pack("<HHH", 0, 1, len(sizes))
entries, data, offset = b"", b"", 6 + 16 * len(sizes)
for sz in sizes:
    png = images[sz]
    dim = 0 if sz == 256 else sz
    entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(png), offset + len(data))
    data += png
open("assets/icon.ico", "wb").write(header + entries + data)
print("wrote assets/icon.png and assets/icon.ico")
