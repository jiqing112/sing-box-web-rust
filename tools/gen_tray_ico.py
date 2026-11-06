#!/usr/bin/env python3
"""生成 assets/tray.ico —— 托盘图标，与 web/dist/index.html 的 SVG logo 同款：
深色圆角方块底 + 荧光绿六边形描边 + 中心圆点。
纯标准库：几何光栅化（带抗锯齿）→ 32bit BMP 条目的多尺寸 ICO。
用法: python tools/gen_tray_ico.py  (在仓库根目录执行)
"""
import math
import struct
from pathlib import Path

BG = (0x0B, 0x0D, 0x10)          # 深底 #0b0d10
FG = (0xC6, 0xFF, 0x4D)          # 荧光绿 #c6ff4d
SIZES = [16, 24, 32, 48]

# 32x32 视图里的六边形顶点（同 SVG path M16 6l8 4.6v10.8L16 26l-8-4.6V10.6z）
HEX = [(16.0, 6.0), (24.0, 10.6), (24.0, 21.4), (16.0, 26.0), (8.0, 21.4), (8.0, 10.6)]
CX, CY, R_SQ = 16.0, 16.0, 8.0    # 圆角方块半径
CIRCLE_R = 2.6


def dist_to_segment(px, py, ax, ay, bx, by):
    vx, vy = bx - ax, by - ay
    wx, wy = px - ax, py - ay
    t = max(0.0, min(1.0, (wx * vx + wy * vy) / (vx * vx + vy * vy)))
    dx, dy = px - (ax + t * vx), py - (ay + t * vy)
    return math.hypot(dx, dy)


def hexagon_inside(px, py):
    """凸多边形：所有边同侧为内部；返回带符号距离（内正外负）。"""
    sign = 1
    dmin = 1e9
    n = len(HEX)
    for i in range(n):
        ax, ay = HEX[i]
        bx, by = HEX[(i + 1) % n]
        d = dist_to_segment(px, py, ax, ay, bx, by)
        dmin = min(dmin, d)
        # 叉积判断内外（顶点按顺时针给出）
        cross = (bx - ax) * (py - ay) - (by - ay) * (px - ax)
        if cross > 0:
            sign = -1
    return sign * dmin


def rounded_square_cov(px, py):
    """圆角方块覆盖率（1px 抗锯齿过渡）。"""
    nx = min(max(px, R_SQ), 32.0 - R_SQ)
    ny = min(max(py, R_SQ), 32.0 - R_SQ)
    d = R_SQ - math.hypot(px - nx, py - ny)  # 内正外负
    return max(0.0, min(1.0, d + 0.5))


def render(size):
    scale = size / 32.0
    # 小尺寸下相对加粗描边/圆点，保证 16px 可见
    stroke = 1.6 if size >= 32 else 2.2
    dot_r = CIRCLE_R if size >= 32 else 3.2
    px_row = bytearray()
    for y in range(size - 1, -1, -1):  # BMP 自底向上
        for x in range(size):
            sx = (x + 0.5) / scale
            sy = (y + 0.5) / scale
            bg_cov = rounded_square_cov(sx, sy)
            d_hex = abs(hexagon_inside(sx, sy))
            hex_cov = max(0.0, min(1.0, stroke / 2.0 - d_hex + 0.5))
            d_dot = math.hypot(sx - CX, sy - CY) - dot_r
            dot_cov = max(0.0, min(1.0, -d_dot + 0.5))
            fg_cov = max(hex_cov, dot_cov)
            # 合成：先 BG 底，再叠 FG
            r = BG[0] + (FG[0] - BG[0]) * fg_cov
            g = BG[1] + (FG[1] - BG[1]) * fg_cov
            b = BG[2] + (FG[2] - BG[2]) * fg_cov
            a = 255 * bg_cov
            px_row += struct.pack("<BBBB", round(b), round(g), round(r), round(a))
    # AND 掩码：32bit 有 alpha 时全 0，行宽按 4 字节对齐
    mask_row = (size + 31) // 32 * 4
    px_row += b"\x00" * (mask_row * size)
    return px_row


def bmp_header(size, data_len):
    # biHeight = 2*height（XOR + AND 两段）
    return struct.pack(
        "<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, data_len, 0, 0, 0, 0
    )


def build_ico():
    images = [render(s) for s in SIZES]
    headers = [bmp_header(s, len(d)) for s, d in zip(SIZES, images)]
    blobs = [h + d for h, d in zip(headers, images)]
    out = struct.pack("<HHH", 0, 1, len(SIZES))
    offset = 6 + 16 * len(SIZES)
    for s, blob in zip(SIZES, blobs):
        out += struct.pack(
            "<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(blob), offset
        )
        offset += len(blob)
    return out + b"".join(blobs)


if __name__ == "__main__":
    root = Path(__file__).resolve().parent.parent
    dest = root / "assets" / "tray.ico"
    dest.parent.mkdir(exist_ok=True)
    dest.write_bytes(build_ico())
    print(f"written {dest} ({dest.stat().st_size} bytes)")
