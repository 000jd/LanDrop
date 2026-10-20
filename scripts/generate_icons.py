#!/usr/bin/env python3
"""Generate every icon derivative from assets/icon.svg.

The SVG is the canonical mark. This script renders it directly with Pillow
(the mark is a small set of rounded rectangles), so there is no ImageMagick or
rsvg dependency, and writes:

  * the standard Tauri bundle set (32/128/256/512 PNG, ICO, ICNS, SVG)
  * the raw RGBA tray/window icons used by the desktop wrapper
  * the Google Play / Android launcher PNGs and the adaptive-icon XML vectors

Android resources are rewritten on every run, so `cargo tauri android build`
(or a stray `tauri icon`) cannot leave blank or cream-coloured defaults behind.
Pass --verify-android to check the generated resources without rewriting them.
"""
from __future__ import annotations

import argparse
import re
import struct
import sys
from dataclasses import dataclass
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
SVG = ROOT / "assets" / "icon.svg"
OUT = ROOT / "assets"

ANDROID_RES = ROOT / "desktop" / "gen" / "android" / "app" / "src" / "main" / "res"
ANDROID_MANIFEST = ROOT / "desktop" / "gen" / "android" / "app" / "src" / "main" / "AndroidManifest.xml"

BRAND_BLUE = "#2383E2"
PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"

# Launcher icons use the density buckets Android expects for legacy (pre-26)
# icons. Adaptive foreground layers are always 108dp, so they need the
# 108/162/216/324/432 pixel sizes instead.
LAUNCHER_SIZES = {"mdpi": 48, "hdpi": 72, "xhdpi": 96, "xxhdpi": 144, "xxxhdpi": 192}
FOREGROUND_SIZES = {"mdpi": 108, "hdpi": 162, "xhdpi": 216, "xxhdpi": 324, "xxxhdpi": 432}

# The 66x66dp adaptive safe zone is centered in the 108x108dp canvas. The four
# inner squares from icon.svg (a 76x76 unit bounding box) are fitted into a
# 48x48dp box, x/y 30..78, leaving a comfortable margin on every side.
SAFE_ZONE_INSET = 30.0
SAFE_ZONE_SIZE = 48.0
CANVAS_DP = 108.0

RECT_RE = re.compile(r"<rect\b([^>]*)/?>", re.IGNORECASE | re.DOTALL)
ATTR_RE = re.compile(r'([A-Za-z_:][-A-Za-z0-9_:.]*)\s*=\s*"([^"]*)"')
VIEWBOX_RE = re.compile(r'viewBox\s*=\s*"([^"]+)"')


@dataclass(frozen=True)
class Rect:
    x: float
    y: float
    width: float
    height: float
    radius: float
    fill: str

    @property
    def right(self) -> float:
        return self.x + self.width

    @property
    def bottom(self) -> float:
        return self.y + self.height


def parse_svg() -> tuple[list[Rect], float]:
    """Read the rounded-rectangle mark out of assets/icon.svg."""
    text = SVG.read_text(encoding="utf-8")
    viewbox = VIEWBOX_RE.search(text)
    if not viewbox:
        raise SystemExit(f"{SVG}: missing viewBox")
    parts = [p for p in re.split(r"[ ,]+", viewbox.group(1).strip()) if p]
    if len(parts) != 4:
        raise SystemExit(f"{SVG}: unsupported viewBox {viewbox.group(1)!r}")
    side = float(parts[2])

    rects: list[Rect] = []
    for match in RECT_RE.finditer(text):
        attrs = dict(ATTR_RE.findall(match.group(1)))
        try:
            rects.append(
                Rect(
                    x=float(attrs.get("x", 0)),
                    y=float(attrs.get("y", 0)),
                    width=float(attrs.get("width", 0)),
                    height=float(attrs.get("height", 0)),
                    radius=float(attrs.get("rx", attrs.get("ry", 0))),
                    fill=attrs.get("fill", "#000000"),
                )
            )
        except ValueError as e:
            raise SystemExit(f"{SVG}: invalid rect attributes: {e}") from e

    if len(rects) < 2:
        raise SystemExit(f"{SVG}: expected the brand square plus four inner squares")
    return rects, side


def color(value: str) -> tuple[int, int, int, int]:
    """Parse #RRGGBB / #RRGGBBAA (SVG hex order)."""
    raw = value.strip().lstrip("#")
    if len(raw) == 3:
        raw = "".join(c * 2 for c in raw)
    if len(raw) == 6:
        raw += "ff"
    if len(raw) != 8:
        raise ValueError(f"unsupported colour {value!r}")
    return tuple(int(raw[i : i + 2], 16) for i in (0, 2, 4, 6))  # type: ignore[return-value]


def render_mark(size: int, supersample: int = 4) -> Image.Image:
    """Rasterise the full mark (blue rounded square + four inner squares)."""
    rects, viewbox = parse_svg()
    canvas = size * supersample
    scale = canvas / viewbox
    img = Image.new("RGBA", (canvas, canvas), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    for rect in rects:
        x0 = rect.x * scale
        y0 = rect.y * scale
        x1 = rect.right * scale
        y1 = rect.bottom * scale
        radius = rect.radius * scale
        if x1 <= x0 or y1 <= y0:
            continue
        draw.rounded_rectangle((x0, y0, x1, y1), radius=radius, fill=color(rect.fill))
    return img.resize((size, size), Image.LANCZOS)


def render_inner_squares(size: int, supersample: int = 4) -> Image.Image:
    """Draw only the four inner squares, fitted inside the 108dp safe zone."""
    rects, viewbox = parse_svg()
    inner = rects[1:]
    if len(inner) != 4:
        raise SystemExit(f"{SVG}: expected exactly four inner squares, found {len(inner)}")

    canvas = size * supersample
    img = Image.new("RGBA", (canvas, canvas), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)

    left = min(r.x for r in inner)
    top = min(r.y for r in inner)
    right = max(r.right for r in inner)
    bottom = max(r.bottom for r in inner)
    span = max(right - left, bottom - top)
    if span <= 0:
        raise SystemExit(f"{SVG}: inner squares have no area")

    # The safe-zone box in the same pixel units as this canvas.
    box_x = SAFE_ZONE_INSET / CANVAS_DP * canvas
    box_y = box_x
    box_size = SAFE_ZONE_SIZE / CANVAS_DP * canvas
    scale = box_size / span
    offset_x = box_x - left * scale
    offset_y = box_y - top * scale

    for rect in inner:
        x0 = rect.x * scale + offset_x
        y0 = rect.y * scale + offset_y
        x1 = rect.right * scale + offset_x
        y1 = rect.bottom * scale + offset_y
        draw.rounded_rectangle(
            (x0, y0, x1, y1),
            radius=rect.radius * scale,
            fill=color(rect.fill),
        )
    return img.resize((size, size), Image.LANCZOS)


def render_round(size: int) -> Image.Image:
    """Circular launcher variant with the same brand mark."""
    mark = render_mark(size)
    margin = round(size * 0.04)
    big = size * 4
    mask = Image.new("L", (big, big), 0)
    ImageDraw.Draw(mask).ellipse(
        (margin * 4, margin * 4, big - margin * 4 - 1, big - margin * 4 - 1),
        fill=255,
    )
    mask = mask.resize((size, size), Image.LANCZOS)
    mark.putalpha(Image.composite(mark.getchannel("A"), Image.new("L", (size, size), 0), mask))
    return mark


def rounded_rect_path(rect: Rect, scale: float, offset_x: float, offset_y: float) -> str:
    """Android VectorDrawable path data for a rounded rectangle.

    Android's <path> has no rx/ry attributes, so the corners are expressed with
    elliptical arc commands: M ... A ... Z.
    """
    x0 = rect.x * scale + offset_x
    y0 = rect.y * scale + offset_y
    x1 = rect.right * scale + offset_x
    y1 = rect.bottom * scale + offset_y
    r = rect.radius * scale
    if r <= 0:
        return f"M{x0:.3f},{y0:.3f}L{x1:.3f},{y0:.3f}L{x1:.3f},{y1:.3f}L{x0:.3f},{y1:.3f}Z"
    return (
        f"M{x0 + r:.3f},{y0:.3f}"
        f"L{x1 - r:.3f},{y0:.3f}"
        f"A{r:.3f},{r:.3f} 0 0 1 {x1:.3f},{y0 + r:.3f}"
        f"L{x1:.3f},{y1 - r:.3f}"
        f"A{r:.3f},{r:.3f} 0 0 1 {x1 - r:.3f},{y1:.3f}"
        f"L{x0 + r:.3f},{y1:.3f}"
        f"A{r:.3f},{r:.3f} 0 0 1 {x0:.3f},{y1 - r:.3f}"
        f"L{x0:.3f},{y0 + r:.3f}"
        f"A{r:.3f},{r:.3f} 0 0 1 {x0 + r:.3f},{y0:.3f}Z"
    )


def write_android_vectors() -> None:
    """Write the adaptive-icon layers as real VectorDrawables (not PNGs)."""
    rects, _ = parse_svg()
    inner = rects[1:]
    left = min(r.x for r in inner)
    top = min(r.y for r in inner)
    right = max(r.right for r in inner)
    bottom = max(r.bottom for r in inner)
    span = max(right - left, bottom - top)
    scale = SAFE_ZONE_SIZE / span
    offset_x = SAFE_ZONE_INSET - left * scale
    offset_y = SAFE_ZONE_INSET - top * scale

    background = f"""<?xml version="1.0" encoding="utf-8"?>
<vector xmlns:android="http://schemas.android.com/apk/res/android"
    android:width="108dp"
    android:height="108dp"
    android:viewportWidth="108"
    android:viewportHeight="108">
    <path
        android:fillColor="{BRAND_BLUE}"
        android:pathData="M0,0h108v108h-108z" />
</vector>
"""
    paths = "\n".join(
        f"""    <path
        android:fillColor="{rect.fill.upper()}"
        android:pathData="{rounded_rect_path(rect, scale, offset_x, offset_y)}" />"""
        for rect in inner
    )
    foreground = f"""<?xml version="1.0" encoding="utf-8"?>
<vector xmlns:android="http://schemas.android.com/apk/res/android"
    android:width="108dp"
    android:height="108dp"
    android:viewportWidth="108"
    android:viewportHeight="108">
{paths}
</vector>
"""

    adaptive = """<?xml version="1.0" encoding="utf-8"?>
<adaptive-icon xmlns:android="http://schemas.android.com/apk/res/android">
    <background android:drawable="@drawable/ic_launcher_background" />
    <foreground android:drawable="@drawable/ic_launcher_foreground" />
    <monochrome android:drawable="@drawable/ic_launcher_foreground" />
</adaptive-icon>
"""

    drawable = ANDROID_RES / "drawable"
    drawable.mkdir(parents=True, exist_ok=True)
    (drawable / "ic_launcher_background.xml").write_text(background, encoding="utf-8")
    (drawable / "ic_launcher_foreground.xml").write_text(foreground, encoding="utf-8")

    anydpi = ANDROID_RES / "mipmap-anydpi-v26"
    anydpi.mkdir(parents=True, exist_ok=True)
    for name in ("ic_launcher.xml", "ic_launcher_round.xml"):
        (anydpi / name).write_text(adaptive, encoding="utf-8")

    values = ANDROID_RES / "values"
    values.mkdir(parents=True, exist_ok=True)
    (values / "ic_launcher_background.xml").write_text(
        """<?xml version="1.0" encoding="utf-8"?>
<resources>
    <color name="ic_launcher_background">#2383E2</color>
</resources>
""",
        encoding="utf-8",
    )


def write_android_launcher_icons() -> None:
    """Replace Tauri's default launcher resources with the LanDrop mark."""
    if not ANDROID_RES.is_dir():
        print("android project missing; skipping launcher icons")
        return

    for density, size in LAUNCHER_SIZES.items():
        dest = ANDROID_RES / f"mipmap-{density}"
        dest.mkdir(parents=True, exist_ok=True)
        render_mark(size).save(dest / "ic_launcher.png", "PNG")
        render_round(size).save(dest / "ic_launcher_round.png", "PNG")

        # Adaptive layers are 108dp, not launcher-icon-sized.
        fg_size = FOREGROUND_SIZES[density]
        render_inner_squares(fg_size).save(dest / "ic_launcher_foreground.png", "PNG")

    write_android_vectors()
    print(f"android launcher icons rewritten for {', '.join(LAUNCHER_SIZES)}")


def png_size(path: Path) -> tuple[int, int]:
    data = path.read_bytes()
    if len(data) < 33 or data[:8] != PNG_SIGNATURE or data[12:16] != b"IHDR":
        raise SystemExit(f"{path}: not a valid PNG")
    width, height = struct.unpack(">II", data[16:24])
    if width == 0 or height == 0:
        raise SystemExit(f"{path}: empty PNG ({width}x{height})")
    return width, height


def verify_android_icons() -> None:
    """Fail loudly if a build/tool replaced our launcher resources."""
    if not ANDROID_RES.is_dir() or not ANDROID_MANIFEST.is_file():
        print("android project missing; skipping launcher verification")
        return
    manifest = ANDROID_MANIFEST.read_text(encoding="utf-8")
    for needle in (
        'android:icon="@mipmap/ic_launcher"',
        'android:roundIcon="@mipmap/ic_launcher_round"',
    ):
        if needle not in manifest:
            raise SystemExit(f"{ANDROID_MANIFEST}: missing {needle}")

    for name in ("ic_launcher.xml", "ic_launcher_round.xml"):
        path = ANDROID_RES / "mipmap-anydpi-v26" / name
        text = path.read_text(encoding="utf-8")
        for needle in (
            '<background android:drawable="@drawable/ic_launcher_background"',
            '<foreground android:drawable="@drawable/ic_launcher_foreground"',
            '<monochrome android:drawable="@drawable/ic_launcher_foreground"',
        ):
            if needle not in text:
                raise SystemExit(f"{path}: missing {needle}")

    background = (ANDROID_RES / "drawable" / "ic_launcher_background.xml").read_text(
        encoding="utf-8"
    )
    if "<path" not in background or "#2383E2" not in background:
        raise SystemExit("android background vector is missing the brand path")
    if "<rect" in background.lower():
        raise SystemExit("android background vector uses <rect> (not supported)")

    foreground = (ANDROID_RES / "drawable" / "ic_launcher_foreground.xml").read_text(
        encoding="utf-8"
    )
    if "<rect" in foreground.lower():
        raise SystemExit("android foreground vector uses <rect> (not supported)")
    if foreground.count("<path") != 4:
        raise SystemExit("android foreground vector must contain exactly four <path> elements")
    if 'android:width="108dp"' not in foreground or 'android:viewportWidth="108"' not in foreground:
        raise SystemExit("android foreground vector must be a 108dp/108 viewport")

    for density, size in LAUNCHER_SIZES.items():
        folder = ANDROID_RES / f"mipmap-{density}"
        for name, expected in (
            ("ic_launcher.png", size),
            ("ic_launcher_round.png", size),
            ("ic_launcher_foreground.png", FOREGROUND_SIZES[density]),
        ):
            actual = png_size(folder / name)
            if actual != (expected, expected):
                raise SystemExit(f"{folder / name}: expected {expected}x{expected}, got {actual}")

    print("android launcher resources verified")


def write_desktop_icons() -> None:
    """Standard Tauri/desktop icon set, from the same canonical mark."""
    icon32 = render_mark(32)
    icon32.save(OUT / "icon-32.png", "PNG")
    (OUT / "icon-32.rgba").write_bytes(icon32.tobytes())

    icon128 = render_mark(128)
    icon128.save(OUT / "icon-128.png", "PNG")

    icon256 = render_mark(256)
    icon256.save(OUT / "icon.png", "PNG")

    icon512 = render_mark(512)
    icon512.save(OUT / "icon-512.png", "PNG")

    # Multi-resolution Windows icon. Pillow picks the requested sizes and
    # scales from the 256px source.
    icon256.save(
        OUT / "icon.ico",
        "ICO",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )

    # macOS icon. Pillow's ICNS writer embeds the source at the standard sizes.
    icon512.save(OUT / "icon.icns", "ICNS")

    # Window/taskbar icon for the desktop wrapper (Tauri wants raw RGBA; 64x64
    # is plenty and keeps the committed asset tiny).
    icon64 = render_mark(64)
    (OUT / "icon-64.rgba").write_bytes(icon64.tobytes())

    # Header logo used by older docs/screenshots.
    soft = Image.new("RGBA", (128, 128), (247, 247, 245, 255))
    soft.alpha_composite(icon128)
    soft.save(OUT / "icon-soft.png", "PNG")

    for name in ("icon-32.png", "icon-128.png", "icon.png", "icon-512.png"):
        print(f"assets/{name} {png_size(OUT / name)[0]}x{png_size(OUT / name)[1]}")
    print("assets/icon.ico, assets/icon.icns, assets/icon-32.rgba, assets/icon-64.rgba")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--verify-android",
        action="store_true",
        help="only verify the committed Android launcher resources",
    )
    args = parser.parse_args()

    if not SVG.exists():
        print(f"missing {SVG}", file=sys.stderr)
        return 1
    OUT.mkdir(exist_ok=True)

    if args.verify_android:
        verify_android_icons()
        return 0

    write_desktop_icons()
    write_android_launcher_icons()
    verify_android_icons()
    return 0


if __name__ == "__main__":
    sys.exit(main())
