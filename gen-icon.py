# 生成应用图标 PNG（1024x1024）：圆角方块 + 放大镜，启动台风格渐变
# 仅用标准库（zlib + struct 手写 PNG），无需 Pillow
import struct, zlib, math

S = 1024
px = bytearray(S * S * 4)

def put(x, y, r, g, b, a):
    i = (y * S + x) * 4
    px[i] = r; px[i+1] = g; px[i+2] = b; px[i+3] = a

def rounded_rect_alpha(x, y, rad=180):
    # 圆角矩形内 = 1
    if rad <= x <= S - rad or rad <= y <= S - rad:
        return 255 if 0 <= x < S and 0 <= y < S else 0
    # 四角
    for cx, cy in [(rad, rad), (S - rad, rad), (rad, S - rad), (S - rad, S - rad)]:
        dx, dy = x - cx, y - cy
        d = math.hypot(dx, dy)
        if d <= rad:
            return 255
    return 0

# 主渐变：深蓝紫 → 亮蓝（macOS 风格）
for y in range(S):
    for x in range(S):
        t = (x / S + y / S) / 2
        r = int(52 + (86 - 52) * t)
        g = int(88 + (160 - 88) * t)
        b = int(235 + (255 - 235) * t)
        a = rounded_rect_alpha(x, y)
        put(x, y, r, g, b, a)

# 放大镜：圆环 + 手柄（白色，中心偏左上）
cx, cy, R, ring = 448, 448, 210, 52
hx0, hy0, hx1, hy1, hw = 590, 590, 790, 790, 62

def dist_seg(px_, py_, x0, y0, x1, y1):
    vx, vy = x1 - x0, y1 - y0
    wx, wy = px_ - x0, py_ - y0
    t = max(0, min(1, (vx * wx + vy * wy) / (vx * vx + vy * vy)))
    return math.hypot(px_ - (x0 + t * vx), py_ - (y0 + t * vy))

for y in range(S):
    for x in range(S):
        i = (y * S + x) * 4
        if px[i+3] == 0:
            continue
        d = math.hypot(x - cx, y - cy)
        if abs(d - R) <= ring / 2:
            put(x, y, 255, 255, 255, 255)
        elif d < R - ring / 2:
            # 镜片微透明白
            put(x, y, 255, 255, 255, 36)
        if dist_seg(x, y, hx0, hy0, hx1, hy1) <= hw / 2:
            put(x, y, 255, 255, 255, 255)

# 写 PNG（RGBA，无滤波）
def chunk(tag, data):
    c = struct.pack(">I", len(data)) + tag + data
    return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

raw = b"".join(b"\x00" + bytes(px[y * S * 4:(y + 1) * S * 4]) for y in range(S))
png = (b"\x89PNG\r\n\x1a\n"
       + chunk(b"IHDR", struct.pack(">IIBBBBB", S, S, 8, 6, 0, 0, 0))
       + chunk(b"IDAT", zlib.compress(raw, 6))
       + chunk(b"IEND", b""))
with open("app-icon.png", "wb") as f:
    f.write(png)
print("app-icon.png written:", len(png), "bytes")
