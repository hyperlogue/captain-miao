#!/usr/bin/env python3
"""Rig a Grok kitten reference into an eight-frame RGBA walk strip (Pillow 12.1+)."""

import argparse
import math
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw


FRAME_SIZE = (96, 64)
FRAMES = 8
# Joint positions and cutout polygons in the 512px reference cell. Far legs are
# drawn behind the torso; near legs overlap it at the hip and shoulder.
LEGS = [
    ((199, 355), [(174, 349), (220, 354), (225, 392), (250, 406), (253, 436), (202, 438), (184, 414), (173, 382)], -35, 0),
    ((291, 357), [(275, 350), (313, 351), (308, 385), (323, 407), (323, 436), (271, 437), (260, 419), (263, 385)], 35, 0),
    ((160, 337), [(130, 308), (193, 317), (184, 354), (150, 378), (126, 405), (140, 417), (139, 435), (91, 438), (78, 420), (79, 392), (95, 357), (116, 340)], -38, 30),
    ((323, 350), [(297, 328), (325, 327), (349, 344), (376, 369), (424, 383), (440, 400), (440, 425), (416, 436), (382, 436), (364, 423), (330, 398), (302, 374)], 38, -30),
]


def remove_green(image):
    """Unmatte green edges as well as the background; keep white fur opaque."""
    image = image.convert("RGB")
    background = image.getpixel((0, 0))
    key = max(1, background[1] - max(background[0], background[2]))
    pixels = []
    for red, green, blue in image.get_flattened_data():
        alpha = max(0.0, min(1.0, 1 - max(0, green - max(red, blue)) / key))
        if alpha < 0.08:
            pixels.append((0, 0, 0, 0))
            continue
        color = tuple(
            round(max(0, min(255, (value - bg * (1 - alpha)) / alpha)))
            for value, bg in zip((red, green, blue), background)
        )
        pixels.append((*color, round(alpha * 255)))
    result = Image.new("RGBA", image.size)
    result.putdata(pixels)
    return result


def walk_frames(image):
    legs = []
    for pivot, polygon, swing, rest in LEGS:
        mask = Image.new("L", image.size)
        ImageDraw.Draw(mask).polygon(polygon, fill=255)
        leg = image.copy()
        leg.putalpha(ImageChops.multiply(image.getchannel("A"), mask))
        legs.append((leg, pivot, swing, rest))
    # Remove the whole original lower silhouette, including outline pixels
    # outside the cutout polygons, so rotated paws leave no stationary fragments.
    torso_mask = Image.new("L", image.size)
    ImageDraw.Draw(torso_mask).polygon(
        [(0, 0), (512, 0), (512, 330), (365, 330), (345, 325),
         (310, 320), (298, 336), (280, 348), (245, 354), (205, 347),
         (180, 335), (150, 316), (120, 310), (0, 310)], fill=255,
    )
    body = image.copy()
    body.putalpha(ImageChops.multiply(image.getchannel("A"), torso_mask))
    frames = []
    for index in range(FRAMES):
        phase = 2 * math.pi * index / FRAMES
        frame = Image.new("RGBA", image.size)
        for i, (leg, pivot, swing, rest) in enumerate(legs):
            if i == 2:
                frame.alpha_composite(body)
            angle = rest + swing * math.cos(phase)
            # Lift the swinging diagonal pair while the opposite pair bears
            # weight. Scaling about the joint keeps the attachment fixed.
            direction = 1 if i in (0, 3) else -1
            stretch = 1 - 0.16 * max(0, direction * math.sin(phase))
            pose = leg.transform(
                leg.size, Image.Transform.AFFINE,
                (1, 0, 0, 0, 1 / stretch, pivot[1] * (1 - 1 / stretch)),
                Image.Resampling.BICUBIC,
            ).rotate(angle, Image.Resampling.BICUBIC, center=pivot)
            frame.alpha_composite(pose)
        frames.append(frame)
    return frames


def prepare(source, output):
    image = remove_green(Image.open(source).resize((512, 512), Image.Resampling.LANCZOS))
    frames = walk_frames(image)
    bounds = [frame.getchannel("A").getbbox() for frame in frames]

    # One scale for the entire cycle, with grounded paws on a shared baseline.
    # Individual scale-to-fit would make the kitten breathe in width each step.
    box = (min(b[0] for b in bounds), min(b[1] for b in bounds), max(b[2] for b in bounds), max(b[3] for b in bounds))
    width, height = box[2] - box[0], box[3] - box[1]
    scale = min((FRAME_SIZE[0] - 4) / width, (FRAME_SIZE[1] - 4) / height)
    sheet = Image.new("RGBA", (FRAME_SIZE[0] * len(frames), FRAME_SIZE[1]))
    for index, frame in enumerate(frames):
        frame = frame.crop(box)
        frame = frame.resize((round(frame.width * scale), round(frame.height * scale)), Image.Resampling.LANCZOS)
        x = index * FRAME_SIZE[0] + (FRAME_SIZE[0] - frame.width) // 2
        y = FRAME_SIZE[1] - 2 - frame.height
        sheet.alpha_composite(frame, (x, y))

    # Canonical straight alpha: transparent RGB is zero in the PNG and raw sheet.
    sheet.putdata([p if p[3] else (0, 0, 0, 0) for p in sheet.get_flattened_data()])
    output.parent.mkdir(parents=True, exist_ok=True)
    sheet.save(output.with_suffix(".png"))
    output.with_suffix(".rgba").write_bytes(sheet.tobytes())
    print(f"Prepared {output.name}: {len(frames)} frames, {FRAME_SIZE[0]}x{FRAME_SIZE[1]} each")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path, help="Output stem; writes .png and .rgba")
    args = parser.parse_args()
    prepare(args.source, args.output)
