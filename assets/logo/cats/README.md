# Title-bar kittens

The paw summons a ginger tabby or tuxedo kitten; one in twenty summons picks
the pink kitten. Each walks left to right inside the empty row below the title
bar. The dashboard embeds these assets, so no API calls or credentials are
needed at runtime.

The character artwork was generated through xAI's Grok Imagine API with
`grok-imagine-image-2.0`, medium quality, 2k resolution, and a 2:1 canvas.
The exact requests are in [prompts.json](prompts.json). See xAI's
[image generation documentation](https://docs.x.ai/developers/model-capabilities/images/generation).

Grok repeated the reference stance in its animation grids, including after a
gait-edit attempt. The committed `*-source.png` files retain the first complete
character from each original grid, with image metadata removed. The preparation
script removes the green background and rigs four leg cutouts around fixed
joints, with alternating paw lifts, to produce a controlled eight-frame loop.
These are prepared animations of Grok artwork, not untouched generated frames.

Each final `.png` is a transparent horizontal strip of eight 96×64 frames.
Its `.rgba` sibling contains the same pixels as row-major, straight-alpha RGBA;
`src/app/logo/cat.rs` embeds those bytes. The runtime downsamples the strip to
the current terminal row height, preserving alpha and frame boundaries. The
common image size keeps the artwork within the lane and the outgoing frame is
cropped at its right edge.

To rebuild a sheet from its committed reference, install Pillow 12.1 or newer
in a Python environment and run from the repository root:

```sh
python3 scripts/prepare-cat-sprites.py assets/logo/cats/tabby-source.png assets/logo/cats/tabby
python3 scripts/prepare-cat-sprites.py assets/logo/cats/tuxedo-source.png assets/logo/cats/tuxedo
python3 scripts/prepare-cat-sprites.py assets/logo/cats/pink-source.png assets/logo/cats/pink
```

The joint coordinates are fitted to these references; new character shapes
need their cutout polygons adjusted. Keep the script's frame dimensions/count
and the Rust constants synchronized. Inspect each sheet against light and dark
backgrounds and review [the animation preview](walk-preview.gif) when changing
the artwork. The preview illustrates the walk; actual size depends on the font.
