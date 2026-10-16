#!/usr/bin/env python3
"""Rasterize assets/icon.svg into the PNG/RGBA sizes Linux/Tauri still need.

The SVG is the canonical mark. These bitmaps exist only for trays, window
icons, and icon themes that will not load SVG.
"""
import subprocess
import sys
from pathlib import Path

from PIL import Image, ImageDraw

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

    # Window/taskbar icon for the desktop wrapper (Tauri wants raw RGBA; 64x64
    # is plenty and keeps the committed asset tiny).
    window = raster(64)
    (OUT / "icon-64.rgba").write_bytes(window.tobytes())
    print(f"assets/icon-64.rgba {len(window.tobytes())} bytes")

    soft = Image.new("RGBA", (128, 128), (247, 247, 245, 255))
    soft.alpha_composite(raster(128))
    soft.save(OUT / "icon-soft.png", "PNG")
    print(f"assets/icon-soft.png {soft.size}")

    write_android_launcher_icons()

    return 0


# Android launcher icons live in the generated project, which is committed; the
# sizes are the standard density buckets.
ANDROID_RES = ROOT / "desktop" / "gen" / "android" / "app" / "src" / "main" / "res"
LAUNCHER_SIZES = {"mdpi": 48, "hdpi": 72, "xhdpi": 96, "xxhdpi": 144, "xxxhdpi": 192}
CREAM = (251, 241, 224, 255)


def write_android_launcher_icons() -> None:
    """Replace Tauri's default launcher icon with the LanDrop mark."""
    if not ANDROID_RES.is_dir():
        print("android project missing; skipping launcher icons")
        return

    mark = Image.open(OUT / "icon.png").convert("RGBA")
    for density, size in LAUNCHER_SIZES.items():
        dest = ANDROID_RES / f"mipmap-{density}"
        dest.mkdir(parents=True, exist_ok=True)

        # Legacy icon: the mark on the brand cream.
        square = Image.new("RGBA", (size, size), CREAM)
        inner = int(size * 0.86)
        offset = ((size - inner) // 2, (size - inner) // 2)
        square.alpha_composite(mark.resize((inner, inner), Image.LANCZOS), offset)
        square.save(dest / "ic_launcher.png", "PNG")

        # Round variant: same, masked to a circle.
        mask = Image.new("L", (size, size), 0)
        ImageDraw.Draw(mask).ellipse((0, 0, size - 1, size - 1), fill=255)
        rounded = square.copy()
        rounded.putalpha(mask)
        rounded.save(dest / "ic_launcher_round.png", "PNG")

        # Adaptive foreground: mark inside the safe zone, transparent around it.
        fg = Image.new("RGBA", (size, size), (0, 0, 0, 0))
        inner = int(size * 0.60)
        offset = ((size - inner) // 2, (size - inner) // 2)
        fg.alpha_composite(mark.resize((inner, inner), Image.LANCZOS), offset)
        fg.save(dest / "ic_launcher_foreground.png", "PNG")

    print(f"android launcher icons rewritten for {', '.join(LAUNCHER_SIZES)}")


if __name__ == "__main__":
    sys.exit(main())
