#!/usr/bin/env python3
"""Pack eight sampled Grok animation frames into an RGBA walk strip (Pillow 12.1+)."""

import argparse
from pathlib import Path

from PIL import Image


FRAME_SIZE = (96, 64)
FRAMES = 8


def remove_green(image):
    """Key the green video backdrop, including compression noise and edge spill."""
    pixels = []
    for red, green, blue in image.convert("RGB").get_flattened_data():
        alpha = max(0.0, min(1.0, 1 - max(0, green - max(red, blue)) / 180))
        if alpha < 0.12:
            pixels.append((0, 0, 0, 0))
        else:
            pixels.append((red, min(green, max(red, blue)), blue, round(alpha * 255)))
    result = Image.new("RGBA", image.size)
    result.putdata(pixels)
    return result


def prepare(source, output):
    image = Image.open(source)
    if image.width != image.height * FRAMES:
        raise ValueError("Expected a horizontal strip of eight square source frames")
    size = image.height
    frames = [
        remove_green(image.crop((index * size, 0, (index + 1) * size, size)))
        for index in range(FRAMES)
    ]
    bounds = [frame.getchannel("A").getbbox() for frame in frames]
    if any(box is None for box in bounds):
        raise ValueError("Every source frame must contain a kitten")
    # Preserve the animation's weight shift and ground plane. Per-frame crops
    # would erase body bob, move the feet, and introduce scale jitter.
    box = (
        min(b[0] for b in bounds), min(b[1] for b in bounds),
        max(b[2] for b in bounds), max(b[3] for b in bounds),
    )
    width, height = box[2] - box[0], box[3] - box[1]
    scale = min((FRAME_SIZE[0] - 4) / width, (FRAME_SIZE[1] - 4) / height)
    sheet = Image.new("RGBA", (FRAME_SIZE[0] * FRAMES, FRAME_SIZE[1]))
    for index, frame in enumerate(frames):
        frame = frame.crop(box).resize(
            (round(width * scale), round(height * scale)), Image.Resampling.LANCZOS,
        )
        x = index * FRAME_SIZE[0] + (FRAME_SIZE[0] - frame.width) // 2
        sheet.alpha_composite(frame, (x, FRAME_SIZE[1] - 2 - frame.height))

    sheet.putdata([p if p[3] else (0, 0, 0, 0) for p in sheet.get_flattened_data()])
    output.parent.mkdir(parents=True, exist_ok=True)
    sheet.save(output.with_suffix(".png"))
    output.with_suffix(".rgba").write_bytes(sheet.tobytes())
    print(f"Prepared {output.name}: {FRAMES} frames, {FRAME_SIZE[0]}x{FRAME_SIZE[1]} each")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path, help="Output stem; writes .png and .rgba")
    args = parser.parse_args()
    prepare(args.source, args.output)
