# Harness artwork

The dashboard embeds the 32×32 straight-alpha `.rgba` files. Run
`python3 assets/agents/generate.py` with `rsvg-convert` and ImageMagick's
`magick` on PATH to rebuild them from the checked-in SVGs. The generator
trims transparent margins, preserves proportions, and leaves a small inset.
The runtime removes the baked transparent margins and fits the visible artwork
to a two-column, one-row slot, preserving its proportions and centering it.
Artwork uses at most 85% of the row height, leaving vertical padding between
adjacent logos.

Sources retrieved 2026-10-14:

| Harness | Source |
| --- | --- |
| Claude | [Official Claude favicon](https://claude.ai/favicon.svg) |
| Codex | [OpenAI developer site favicon](https://developers.openai.com/favicon.svg), using the provider's OpenAI mark |
| Reasonix | [Standalone mark](https://github.com/esengine/DeepSeek-Reasonix/blob/main-v2/desktop/frontend/src/assets/logo-symbol.svg), credited upstream to Bernardxu123 |
| Kimi Code | [Extension icon](https://github.com/MoonshotAI/kimi-code/blob/main/apps/vscode/resources/kimi-icon.svg) |
| Grok Build | [xAI monogram archived on Wikimedia Commons](https://commons.wikimedia.org/wiki/File:XAI-Logo.svg), credited to X.AI Corp. and sourced there from x.ai; [SVG](https://upload.wikimedia.org/wikipedia/commons/5/57/XAI-Logo.svg) |
| Pi | [Official site mark](https://pi.dev/logo-auto.svg), linked by the CLI README |
| omp | [Official icon](https://github.com/can1357/oh-my-pi/blob/main/assets/icon.svg) |
| opencode | [Official dark-background mark](https://github.com/anomalyco/opencode/blob/dev/packages/console/app/src/asset/brand/opencode-logo-dark.svg) from its [brand assets](https://opencode.ai/brand) |
| Antigravity | [Full-color icon](https://antigravity.google/assets/image/brand/antigravity-icon__full-color.svg) from its [press kit](https://antigravity.google/press) |

Source SVGs are preserved verbatim. Kimi's `currentColor` and xAI's inherited
fill resolve to neutral gray via `color.css`. Grok Build uses the xAI provider
mark. Unknown agent kinds use `?` in the same slot only when logos are enabled
and supported.

Brand artwork belongs to its respective owners and identifies the integrated
products; the project's MIT license does not relicense their trademarks.
MIT notices for artwork sourced from MIT repositories are in `LICENSES.txt`.
