"""Generate assets/winstats.ico: a rounded accent-blue square with seven white bars (same as the About logo).
Pure Python, no dependencies. Two plain 32-bit DIB entries: 16 and 32 px."""
import math, struct, os, zlib

ACCENT = (0x2E, 0x7C, 0xF6)
BARS = [0.45, 0.8, 0.35, 0.95, 0.6, 0.25, 0.7]


def coverage(px, py, x, y, w, h, r):
    """anti-aliased coverage of a rounded rect at pixel centre"""
    cx, cy = px + 0.5, py + 0.5
    if cx < x or cx > x + w or cy < y or cy > y + h:
        return 0.0
    qx = x + r if cx < x + r else (x + w - r if cx > x + w - r else cx)
    qy = y + r if cy < y + r else (y + h - r if cy > y + h - r else cy)
    dx, dy = cx - qx, cy - qy
    if dx == 0 or dy == 0:
        return 1.0
    d = math.hypot(dx, dy)
    return max(0.0, min(1.0, r - d + 0.5))


def render(size):
    """rows top-down, each pixel (r, g, b, a) straight (not premultiplied)"""
    s = size
    pad = s * 0.19
    gap = max(1.0, s * 0.03)
    n = len(BARS)
    inner = s - 2 * pad
    bw = (inner - gap * (n - 1)) / n
    total = bw * n + gap * (n - 1)
    x0 = (s - total) / 2
    rows = []
    for y in range(s):
        row = []
        for x in range(s):
            a = coverage(x, y, 0, 0, s, s, s * 0.22)
            if a <= 0:
                row.append((0, 0, 0, 0))
                continue
            r, g, b = ACCENT
            cx, cy = x + 0.5, y + 0.5
            white = 0.0
            for i, v in enumerate(BARS):
                bx = x0 + i * (bw + gap)
                if bx <= cx < bx + bw and pad <= cy <= s - pad:
                    top = s - pad - v * inner
                    white = 1.0 if cy >= top else 0.28
            r = r + (255 - r) * white
            g = g + (255 - g) * white
            b = b + (255 - b) * white
            row.append((int(r), int(g), int(b), int(255 * a)))
        rows.append(row)
    return rows


def dib(size, rows):
    """32-bit BGRA DIB with premultiplied alpha, bottom-up, plus an empty AND mask"""
    px = bytearray()
    for row in reversed(rows):
        for r, g, b, a in row:
            px += struct.pack("<BBBB", b * a // 255, g * a // 255, r * a // 255, a)
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, len(px), 0, 0, 0, 0)
    mask = bytes((size + 31) // 32 * 4 * size)
    return header + bytes(px) + mask


def png(size, rows):
    """PNG entry (allowed in ICO since Vista); straight RGBA, filter type 0 per row"""
    raw = bytearray()
    for row in rows:
        raw.append(0)
        for r, g, b, a in row:
            raw += bytes((r, g, b, a))

    def chunk(tag, data):
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    sig = bytes((0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A))
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(bytes(raw), 9)) + chunk(b"IEND", b"")


def main():
    sizes = [16, 32]
    images = []
    for sz in sizes:
        rows = render(sz)
        images.append((sz, dib(sz, rows) if sz <= 48 else png(sz, rows)))
    out = bytearray(struct.pack("<HHH", 0, 1, len(images)))
    offset = 6 + 16 * len(images)
    for sz, data in images:
        out += struct.pack("<BBBBHHII", sz % 256, sz % 256, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    for _, data in images:
        out += data
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "winstats.ico")
    open(path, "wb").write(out)
    print("wrote", path, len(out), "bytes")


if __name__ == "__main__":
    main()
