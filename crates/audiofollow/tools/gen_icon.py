"""生成应用图标 ui/icon.png 与 PE 资源用的 ui/icon.ico（唯一图标源）。

几何与配色在这里是**唯一定义**，各处（窗口/任务栏/托盘/exe 文件图标）都消费同一套几何：
  - slint 的 `Window.icon` 属性 → winit 设窗口图标 + 任务栏图标
  - slint 标题栏左上角的 Image
  - 托盘图标（icon-tray.png，Rust 侧经 icon.rs 读入）
  - exe 的文件图标（ui/icon.ico，由 build.rs 交给 rc.exe 编进 .rsrc 节）

形状：蓝色圆角方底 + 白色扬声器（箱体 + 向右张开的喇叭口）+ 两道声波弧。
坐标体系用 32x32 网格（朴素、好手算）。
每个尺寸**单独渲染**（不是把 256 的缩下来）——16x16 直接缩会糊成一团，
按目标尺寸重画再用 4 倍超采样降采样，小尺寸才锐利。

用法：uv run --with pillow python crates/audiofollow/tools/gen_icon.py
（在仓库根目录运行）
"""
import io
import os
import struct

from PIL import Image, ImageDraw

OUT = os.path.join(os.path.dirname(__file__), "..", "ui", "icon.png")
OUT_TRAY = os.path.join(os.path.dirname(__file__), "..", "ui", "icon-tray.png")
OUT_ICO = os.path.join(os.path.dirname(__file__), "..", "ui", "icon.ico")
GRID = 32          # 设计基准网格
SCALE = 8          # 32 → 256
SS = 4             # 超采样倍数（硬边绘制 + 降采样 = 抗锯齿）

BG = (0x35, 0x65, 0xE3, 0xFF)   # Theme.accent
FG = (0xFF, 0xFF, 0xFF, 0xFF)

# exe 图标里要放的尺寸。Windows 会按显示场景挑，16/32/48/256 最常用。
ICO_SIZES = (16, 20, 24, 32, 40, 48, 64, 96, 128, 256)


def render(px):
    """按目标像素边长渲染一张图标（独立绘制 + 超采样抗锯齿）。"""
    scale = px / GRID
    side = px * SS

    def s(*v):
        """32 网格坐标 → 本尺寸超采样画布像素。"""
        out = tuple(int(round(x * scale * SS)) for x in v)
        return out[0] if len(out) == 1 else out

    img = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    # 圆角方底（圆角半径 56/256 = 7/32）
    d.rounded_rectangle([0, 0, side - 1, side - 1], radius=s(7), fill=BG)

    # 扬声器箱体
    d.rectangle([s(7), s(12), s(10) - 1, s(20) - 1], fill=FG)

    # 喇叭口：左侧窄、右侧宽的梯形（与箱体右缘无缝相接）
    d.polygon(
        [(s(10), s(14)), (s(17), s(8)), (s(17), s(24)), (s(10), s(18))],
        fill=FG,
    )

    # 两道声波弧，圆心 (16,16)，张开 ±50°。线宽至少 1 个超采样像素，
    # 否则 16x16 时 s(1) 会被四舍五入成 0，弧直接消失。
    width = max(1, s(1))
    for radius in (6, 9):
        d.arc(
            [s(16 - radius), s(16 - radius), s(16 + radius), s(16 + radius)],
            start=-50,
            end=50,
            fill=FG,
            width=width,
        )

    return img.resize((px, px), Image.LANCZOS)


def ico_bytes(images):
    """手搓 ICO 容器。

    Pillow 的 `save(sizes=...)` 只会把同一张源图缩成多个尺寸，而我们要每个尺寸
    各画一遍；所以目录项自己写。图像数据用 PNG（Vista 起支持，rc.exe 也认），
    比 BMP 小一个数量级，而 BMP 版的 256x256 一项就要 256 KB。
    """
    blobs = []
    for _, im in images:
        buf = io.BytesIO()
        im.save(buf, format="PNG", optimize=True)
        blobs.append(buf.getvalue())

    out = struct.pack("<HHH", 0, 1, len(images))   # reserved / type=icon / count
    offset = 6 + 16 * len(images)
    for (px, _), blob in zip(images, blobs):
        dim = 0 if px >= 256 else px               # 0 表示 256
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
    return out + b"".join(blobs)


# ---- 主图（256，窗口与任务栏用）与托盘小图（32）----
final = render(GRID * SCALE)
final.save(OUT)
final.resize((32, 32), Image.LANCZOS).save(OUT_TRAY, optimize=True)
print(f"wrote {os.path.normpath(OUT)}  {final.size[0]}x{final.size[1]}")
print(f"wrote {os.path.normpath(OUT_TRAY)}  32x32")

# ---- exe 文件图标：每个尺寸各渲染一遍 ----
ico = ico_bytes([(px, render(px)) for px in ICO_SIZES])
with open(OUT_ICO, "wb") as f:
    f.write(ico)
print(f"wrote {os.path.normpath(OUT_ICO)}  {len(ico)} bytes, sizes={list(ICO_SIZES)}")

