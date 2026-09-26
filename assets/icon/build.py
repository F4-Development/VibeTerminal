"""Иконка VibeTerminal из макета дизайнера (vibe.jpg).

В макете одна и та же иконка нарисована на тёмном и на белом фоне. По двум
версиям прозрачность считается точно (difference matting), а не вырезается
на глаз. Большая иконка идёт на крупные размеры, пиксельные мастера 32 и 16 —
на мелкие, как задумал дизайнер.

    python build.py ~/Downloads/vibe.jpg
"""

import subprocess
import sys
from pathlib import Path

import numpy as np
from PIL import Image, ImageFilter

HERE = Path(__file__).parent
DARK_BG, LIGHT_BG = 22.0, 255.0
LIGHT_OFFSET = 600  # белая половина макета — справа, со сдвигом 600 px
BIG = (80, 30, 440)  # x, y, размер большой иконки на тёмной половине
# Сетка иконок macOS: тело 824 из 1024, по 100 px отступа со всех сторон.
CANVAS, BODY = 1024, 824
# Мягкая тень, как у системных иконок, — чтобы светлая плитка не терялась на светлом фоне.
SHADOW_BLUR, SHADOW_OFFSET, SHADOW_OPACITY = 18, 12, 0.28


def matte(sheet: np.ndarray, x: int, y: int, w: int, h: int) -> Image.Image:
    """RGBA из пары «на тёмном» / «на белом»."""
    dark = sheet[y : y + h, x : x + w]
    light = sheet[y : y + h, x + LIGHT_OFFSET : x + LIGHT_OFFSET + w]
    alpha = 1.0 - (light - dark).mean(axis=2) / (LIGHT_BG - DARK_BG)
    alpha = np.clip(alpha, 0.0, 1.0)
    alpha[alpha < 0.04] = 0.0  # шум JPEG за краем
    alpha[alpha > 0.96] = 1.0
    safe = np.maximum(alpha, 1e-3)[..., None]
    color = np.clip((dark - (1.0 - alpha[..., None]) * DARK_BG) / safe, 0, 255)
    rgba = np.dstack([color, alpha * 255.0]).round().astype(np.uint8)
    return Image.fromarray(rgba, "RGBA")


def find_box(sheet: np.ndarray, y0: int, y1: int, x0: int, x1: int) -> tuple[int, int, int, int]:
    """Рамка иконки в полосе макета: всё, что светлее тёмного фона."""
    region = sheet[y0:y1, x0:x1].sum(axis=2) > 3 * DARK_BG + 60
    ys, xs = np.where(region)
    return x0 + xs.min(), y0 + ys.min(), xs.max() - xs.min() + 1, ys.max() - ys.min() + 1


def pixel_master(sheet: np.ndarray, box: tuple[int, int, int, int], size: int) -> Image.Image:
    """Пиксельный мастер из «pixel view ×4»: берём центр каждого блока 4×4."""
    x, y, w, h = box
    big = np.asarray(matte(sheet, x, y, w, h))
    step_x, step_y = w / size, h / size
    rows = [[big[int((j + 0.5) * step_y), int((i + 0.5) * step_x)] for i in range(size)] for j in range(size)]
    return Image.fromarray(np.array(rows, dtype=np.uint8), "RGBA")


def main() -> None:
    source = Path(sys.argv[1]).expanduser()
    sheet = np.asarray(Image.open(source).convert("RGB")).astype(float)

    x, y, s = BIG
    body = matte(sheet, x, y, s, s)
    body.save(HERE / "vibe-body-440.png")
    master = Image.new("RGBA", (CANVAS, CANVAS))
    pad = (CANVAS - BODY) // 2
    scaled = body.resize((BODY, BODY), Image.LANCZOS)
    shadow_alpha = Image.new("L", (CANVAS, CANVAS))
    shadow_alpha.paste(scaled.getchannel("A"), (pad, pad + SHADOW_OFFSET))
    shadow_alpha = shadow_alpha.filter(ImageFilter.GaussianBlur(SHADOW_BLUR)).point(lambda a: int(a * SHADOW_OPACITY))
    master.putalpha(shadow_alpha)
    master.alpha_composite(scaled, (pad, pad))
    master.save(HERE / "vibe-icon-1024.png")

    small32 = pixel_master(sheet, find_box(sheet, 690, 835, 180, 330), 32)
    small16 = pixel_master(sheet, find_box(sheet, 690, 835, 335, 420), 16)
    small32.save(HERE / "vibe-small-32.png")
    small16.save(HERE / "vibe-small-16.png")

    iconset = HERE / "VibeTerminal.iconset"
    iconset.mkdir(exist_ok=True)
    for points in (16, 32, 128, 256, 512):
        for scale in (1, 2):
            px = points * scale
            if px == 16:
                image = small16
            elif px == 32 and scale == 1:
                image = small32
            else:
                image = master.resize((px, px), Image.LANCZOS)
            suffix = "@2x" if scale == 2 else ""
            image.save(iconset / f"icon_{points}x{points}{suffix}.png")
    subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(HERE / "VibeTerminal.icns")], check=True)
    print("готово:", *sorted(p.name for p in HERE.iterdir()))


if __name__ == "__main__":
    main()
