#!/usr/bin/env python3
"""Rasterize assets/icon.svg into the PNG/RGBA sizes Linux/Tauri still need.

The SVG is the canonical mark. These bitmaps exist only for trays, window
icons, and icon themes that will not load SVG.
"""
import subprocess
import sys
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
SVG = ROOT / "assets" / "icon.svg"
OUT = ROOT / "assets"


def raster(size: int) -> Image.Image:
    tmp = OUT / f".tmp-icon-{size}.png"
    subprocess.check_call(
        [
            "convert",
            "-background",
            "none",
            "-density",
            "384",
            str(SVG),
            "-resize",
            f"{size}x{size}",
            f"PNG32:{tmp}",
        ]
    )
    img = Image.open(tmp).convert("RGBA")
    tmp.unlink(missing_ok=True)
    if img.size != (size, size):
        img = img.resize((size, size), Image.LANCZOS)
    return img


def main() -> int:
    if not SVG.exists():
        print(f"missing {SVG}", file=sys.stderr)
        return 1
    OUT.mkdir(exist_ok=True)

    icon = raster(256)
    icon.save(OUT / "icon.png", "PNG")
    print(f"assets/icon.png {icon.size}")

    tray = raster(32)
    tray.save(OUT / "icon-32.png", "PNG")
    (OUT / "icon-32.rgba").write_bytes(tray.tobytes())
    print(f"assets/icon-32.png {tray.size}, assets/icon-32.rgba {len(tray.tobytes())} bytes")

    soft = Image.new("RGBA", (128, 128), (247, 247, 245, 255))
    soft.alpha_composite(raster(128))
    soft.save(OUT / "icon-soft.png", "PNG")
    print(f"assets/icon-soft.png {soft.size}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
