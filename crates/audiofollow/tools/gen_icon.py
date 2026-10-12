"""生成应用图标 ui/icon.png（唯一图标源）。

几何与配色在这里是**唯一定义**，各处（窗口/任务栏/托盘）都消费这一张图：
  - slint 的 `Window.icon` 属性 → winit 设窗口图标 + 任务栏图标
  - slint 标题栏左上角的 Image
  - 托盘图标（icon-tray.png，Rust 侧经 icon.rs 读入）

形状：蓝色圆角方底 + 白色扬声器（箱体 + 向右张开的喇叭口）+ 两道声波弧。
坐标体系用 32x32 网格（朴素、好手算），放大 8 倍到 256x256 输出；
先在 4 倍超采样画布上硬边绘制再降采样，等于免费拿到抗锯齿。

用法：uv run --with pillow python crates/audiofollow/tools/gen_icon.py
（在仓库根目录运行；托盘用的 32×32 LANCZOS 缩图会一并生成）
"""
import os
from PIL import Image, ImageDraw

OUT = os.path.join(os.path.dirname(__file__), "..", "ui", "icon.png")
OUT_TRAY = os.path.join(os.path.dirname(__file__), "..", "ui", "icon-tray.png")
GRID = 32          # 设计基准网格
SCALE = 8          # 32 → 256
SS = 4             # 超采样倍数（硬边绘制 + 降采样 = 抗锯齿）

BG = (0x35, 0x65, 0xE3, 0xFF)   # Theme.accent
FG = (0xFF, 0xFF, 0xFF, 0xFF)

SIDE = GRID * SCALE
W = SIDE * SS


def s(*v):
    """32 网格坐标 → 超采样画布像素。"""
    out = tuple(int(round(x * SCALE * SS)) for x in v)
    return out[0] if len(out) == 1 else out


img = Image.new("RGBA", (W, W), (0, 0, 0, 0))
d = ImageDraw.Draw(img)

# 圆角方底（圆角半径 56/256 = 7/32）
d.rounded_rectangle([0, 0, W - 1, W - 1], radius=s(7), fill=BG)

# 扬声器箱体
d.rectangle([s(7), s(12), s(10) - 1, s(20) - 1], fill=FG)

# 喇叭口：左侧窄、右侧宽的梯形（与箱体右缘无缝相接）
d.polygon(
    [
        (s(10), s(14)),
        (s(17), s(8)),
        (s(17), s(24)),
        (s(10), s(18)),
    ],
    fill=FG,
)

# 两道声波弧，圆心 (16,16)，张开 ±50°
for radius in (6, 9):
    d.arc(
        [s(16 - radius), s(16 - radius), s(16 + radius), s(16 + radius)],
        start=-50,
        end=50,
        fill=FG,
        width=s(1),   # 线宽 = 1 个网格单位
    )

final = img.resize((SIDE, SIDE), Image.LANCZOS)
final.save(OUT)
final.resize((32, 32), Image.LANCZOS).save(OUT_TRAY, optimize=True)
print(f"wrote {os.path.normpath(OUT)}  {final.size[0]}x{final.size[1]}")
print(f"wrote {os.path.normpath(OUT_TRAY)}  32x32")
