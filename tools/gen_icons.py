#!/usr/bin/env python3
"""按 assets/icon.svg 的几何参数生成图标位图（无外部素材）。

本机没有 rsvg-convert / ImageMagick，因此用 Pillow 以 4 倍超采样绘制同样的
矢量元素，再 Lanczos 下采样，得到 16~1024 的 PNG、Windows .ico 与 macOS .icns。
"""
import os
import shutil
import subprocess

from PIL import Image, ImageDraw

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ASSETS = os.path.join(ROOT, "assets")

BASE = 1024          # SVG 设计坐标系
SS = 4               # 超采样倍数
S = BASE * SS        # 实际绘制分辨率 4096

SIZES = [16, 32, 48, 64, 128, 256, 512, 1024]


def lerp(a, b, t):
    return tuple(int(round(a[i] + (b[i] - a[i]) * t)) for i in range(3))


def draw_master():
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    # 圆角底板：垂直渐变 #1B2434 -> #0A0E16
    r = 190 * SS
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, S - 1, S - 1], radius=r, fill=255)

    top, bottom = (0x1B, 0x24, 0x34), (0x0A, 0x0E, 0x16)
    grad = Image.new("RGBA", (S, S))
    gd = ImageDraw.Draw(grad)
    for y in range(S):
        gd.line([(0, y), (S, y)], fill=lerp(top, bottom, y / (S - 1)) + (255,))
    img.paste(grad, (0, 0), mask)

    # 内描边（白色 7%）
    d.rounded_rectangle(
        [10 * SS, 10 * SS, S - 10 * SS, S - 10 * SS], radius=182 * SS,
        outline=(255, 255, 255, 18), width=8 * SS,
    )

    # 信封主体
    d.rounded_rectangle(
        [232 * SS, 336 * SS, 792 * SS, 688 * SS], radius=44 * SS, fill=(0xEE, 0xF3, 0xFA, 255)
    )

    # 信封折线（两条斜边组成的 V）
    w = 14 * SS
    d.line([(232 * SS, 380 * SS), (512 * SS, 566 * SS)], fill=(0xB7, 0xC6, 0xDA, 255), width=w)
    d.line([(512 * SS, 566 * SS), (792 * SS, 380 * SS)], fill=(0xB7, 0xC6, 0xDA, 255), width=w)
    for p in [(232 * SS, 380 * SS), (512 * SS, 566 * SS), (792 * SS, 380 * SS)]:
        rr = w // 2
        d.ellipse([p[0] - rr, p[1] - rr, p[0] + rr, p[1] + rr], fill=(0xB7, 0xC6, 0xDA, 255))

    # 拦截斜杠：先画深色描边（分离层），再画青绿渐变
    p1, p2 = (560 * SS, 896 * SS), (896 * SS, 560 * SS)

    def stroke(points, width, color):
        d.line([points[0], points[1]], fill=color, width=width)
        rad = width // 2
        for p in points:
            d.ellipse([p[0] - rad, p[1] - rad, p[0] + rad, p[1] + rad], fill=color)

    stroke((p1, p2), 150 * SS, (0x0A, 0x0E, 0x16, 255))
    stroke((p1, p2), 118 * SS, (0x2F, 0xD9, 0xB8, 255))

    return img


def build_icons(master):
    for size in SIZES:
        out = os.path.join(ASSETS, f"icon_{size}.png")
        master.resize((size, size), Image.LANCZOS).save(out, format="PNG")

    # Windows 多尺寸 ICO
    master.resize((256, 256), Image.LANCZOS).save(
        os.path.join(ASSETS, "icon.ico"),
        format="ICO",
        sizes=[(16, 16), (32, 32), (48, 48), (256, 256)],
    )


def build_icns():
    iconset = os.path.join(ASSETS, "MailSender.iconset")
    if os.path.isdir(iconset):
        shutil.rmtree(iconset)
    os.makedirs(iconset)

    mapping = {
        "icon_16x16.png": 16,
        "icon_16x16@2x.png": 32,
        "icon_32x32.png": 32,
        "icon_32x32@2x.png": 64,
        "icon_128x128.png": 128,
        "icon_128x128@2x.png": 256,
        "icon_256x256.png": 256,
        "icon_256x256@2x.png": 512,
        "icon_512x512.png": 512,
        "icon_512x512@2x.png": 1024,
    }
    for name, size in mapping.items():
        shutil.copyfile(
            os.path.join(ASSETS, f"icon_{size}.png"), os.path.join(iconset, name)
        )

    subprocess.run(
        ["iconutil", "-c", "icns", iconset, "-o", os.path.join(ASSETS, "icon.icns")],
        check=True,
    )
    shutil.rmtree(iconset)


if __name__ == "__main__":
    draw_master().save(os.path.join(ASSETS, "icon_1024_master.png"))
    build_icons(draw_master())
    build_icns()
    os.remove(os.path.join(ASSETS, "icon_1024_master.png"))
    print("icons generated:", ", ".join(f"icon_{s}.png" for s in SIZES), "+ icon.ico + icon.icns")
