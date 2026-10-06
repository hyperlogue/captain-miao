# Title-bar kittens

The paw summons a ginger tabby, tuxedo or calico kitten with a 32% chance
each; the pink kitten appears with a 4% chance. Each walks left to right inside the empty row below the title
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
27, 30, 33, 36, 39, 42 and 45 for the original coats; frames 32, 36, 40, 44,
48, 52, 56 and 60 for calico (zero-based indices). Original character
references remain available in commit `64d00bf`; the new calico reference is
[calico-reference.png](calico-reference.png).

The preparation script keys out the green backdrop and packs complete frames;
it does not synthesize motion. One crop and scale are shared by the whole cycle
so the original ground plane and body movement survive. It first packs 96×64
frames, then applies the dashboard's alpha-aware box filter to produce eight
48×32 frames. This preserves the approved coats' appearance at a 32px row
height. The final `.png` is a transparent 384×32 strip for inspection.

The runtime embeds only `.rgba.xz`: lossless XZ-compressed, row-major,
straight-alpha pixels, with a 256 KiB dictionary and CRC32 check. All four
sheets together occupy 63,240 bytes, versus 589,824 raw bytes for the previous
three cats. Lossless WebP saved only about 1 KiB over XZ for the three existing
32px sheets; XZ reuses the dashboard's existing pure-Rust decoder without
adding an image codec. JPEG would discard the transparency.

`src/app/logo/cat.rs` decodes each coat on first use and caches its 49,152 RGBA
bytes. It resamples frames independently, retaining the same displayed size
and travel speed. Rows above 32px repeat source pixels up to the existing
64px display cap; the lower stored resolution is visible at those sizes.

Eight poses play at 20 fps. Travel is 20 stored pixels per cycle
(50 pixels per second, or 250 pixels over five seconds), scaled with the
sprite height: the planted paws move backward about 2.5 stored pixels per
frame. Using cell width to set travel speed would make the feet slide at
different font aspect ratios. The outgoing frame is cropped at the lane edge.

To rebuild a sheet from its committed source, install Pillow 12.1 or newer
in a Python environment and run from the repository root:

```sh
python3 scripts/prepare-cat-sprites.py assets/logo/cats/tabby-source.png assets/logo/cats/tabby
python3 scripts/prepare-cat-sprites.py assets/logo/cats/tuxedo-source.png assets/logo/cats/tuxedo
python3 scripts/prepare-cat-sprites.py assets/logo/cats/calico-source.png assets/logo/cats/calico
python3 scripts/prepare-cat-sprites.py assets/logo/cats/pink-source.png assets/logo/cats/pink
```

Keep the frame dimensions/count and Rust constants synchronized. Review all
four coats frame by frame on light and dark backgrounds, including the last
to first transition. Compare foot contacts against a fixed ground reference
with travel enabled, and inspect at 16, 24, 32 and 64 pixels high. Distinct-frame
and playback tests alone do not establish that a gait reads as walking.
[The animation preview](walk-preview.gif) shows the packed cycles; actual size
depends on the terminal font. HTML review artifacts belong outside this repo.
