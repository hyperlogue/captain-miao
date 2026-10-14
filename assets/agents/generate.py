#!/usr/bin/env python3
"""Bake upstream SVGs into 32×32 straight-alpha RGBA for Kitty graphics.

Requires rsvg-convert (librsvg) and magick (ImageMagick) on PATH.
"""

from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent

for source in sorted((ROOT / "source").glob("*.svg")):
    with tempfile.TemporaryDirectory() as tmp:
        png = Path(tmp) / "icon.png"
        subprocess.run(
            ["rsvg-convert", "--width", "128", "--height", "128",
             "--keep-aspect-ratio", "--stylesheet", str(ROOT / "color.css"),
             "--output", str(png), str(source)], check=True,
        )
        subprocess.run(
            ["magick", str(png), "-trim", "+repage", "-resize", "30x30",
             "-gravity", "center", "-background", "none", "-extent", "32x32",
             "-depth", "8", f"RGBA:{ROOT / (source.stem + '.rgba')}"], check=True,
        )
