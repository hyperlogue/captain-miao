# Title-bar kittens

The paw summons a ginger tabby or tuxedo kitten; one in twenty summons picks
the pink kitten. Each walks left to right inside the empty row below the title
bar. The dashboard embeds these assets, so no API calls or credentials are
needed at runtime.

The character references were generated through xAI's Grok Imagine image API
with `grok-imagine-image-2.0`. The walk motion was generated separately with
`grok-imagine-video-1.5`, using each reference as both the first and last frame
of a three-second, 720p clip. The exact character and motion prompts, request
settings and selected frame indices are in [prompts.json](prompts.json). See
xAI's [image generation documentation](https://docs.x.ai/developers/model-capabilities/images/generation)
and [image-to-video documentation](https://docs.x.ai/developers/model-capabilities/video/image-to-video).

The earlier animation rotated rigid leg cutouts from a single pose. These
sheets instead retain eight complete generated poses from a middle walk cycle,
including bent-leg recovery, alternating foot contacts and body weight shifts.
The committed `*-source.png` files hold those selected poses in a horizontal
strip of eight 256×256 cells. Video frames were sampled at 24 fps: frames 24,
27, 30, 33, 36, 39, 42 and 45, with zero-based indices. Original character
references remain available in commit `64d00bf`.

The preparation script keys out the green backdrop and packs complete frames;
it does not synthesize motion. One crop and scale are shared by the whole cycle
so the original ground plane and body movement survive. Each final `.png` is
a transparent horizontal strip of eight 96×64 frames. Its `.rgba` sibling
contains the same pixels as row-major, straight-alpha RGBA;
`src/app/logo/cat.rs` embeds those bytes and downsamples each frame independently.

One cycle takes one second. Travel is 40 source pixels per cycle, scaled with
the sprite height: the planted paws move backward about five source pixels
per frame. Using cell width to set travel speed would make the feet slide at
different font aspect ratios. The outgoing frame is cropped at the lane edge.

To rebuild a sheet from its committed source, install Pillow 12.1 or newer
in a Python environment and run from the repository root:

```sh
python3 scripts/prepare-cat-sprites.py assets/logo/cats/tabby-source.png assets/logo/cats/tabby
python3 scripts/prepare-cat-sprites.py assets/logo/cats/tuxedo-source.png assets/logo/cats/tuxedo
python3 scripts/prepare-cat-sprites.py assets/logo/cats/pink-source.png assets/logo/cats/pink
```

Keep the frame dimensions/count and Rust constants synchronized. Review all
three coats frame by frame on light and dark backgrounds, including the last
to first transition. Compare foot contacts against a fixed ground reference
with travel enabled, and inspect at 16, 24 and 32 pixels high. Distinct-frame
and playback tests alone do not establish that a gait reads as walking.
[The animation preview](walk-preview.gif) shows the packed cycles; actual size
depends on the terminal font. HTML review artifacts belong outside this repo.
