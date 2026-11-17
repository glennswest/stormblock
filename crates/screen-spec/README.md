# stormblock-screen-spec

The boot and install screen's look as one `no_std` spec (stormblock#637).
Two drawers build from it: stormblock's `screen` (#618, on DRM once the
kernel has the display) and stormbootx's boot panel (stormbootx#220, on the
UEFI GOP before it). They share one palette, one typeface, one grid, one
frame and one set of shapes, so the machine looks like one product from
power-on and the hand-over shows no change.

`no_std`, `core` only, no allocator. It builds for `x86_64-unknown-uefi` and
`aarch64-unknown-uefi`. Its dependencies are `ab_glyph` (`default-features =
false, features = ["libm"]`, with `ab_glyph_rasterizer`, `owned_ttf_parser`,
`ttf-parser` and `core_maths`) and `libm`.

## What is in it

| module | what |
|---|---|
| `palette` | the colours (`BG`, `AMBER`, `LAVENDER`, `BLUE`, `PEACH`, `GREY`, `RED`, `TRACK`, `INK`) and what each state is drawn in (`FRAME_HEAD`, `FRAME_LOWER`, `FRAME_FAILED`, `DONE`, `RUNNING`, `PENDING`, `FAILED`, `WANTING`, `NEUTRAL`), as `0x00RRGGBB` |
| `font` | `FONT` (Antonio Bold, OFL 1.1, by bytes), `SOURCE`, `SHA256`, `LICENCE`, the sizes (`size::BIG` … `size::HEADING`), `text_width`, `fit` |
| `grid` | the 1920 × 1080 design grid; `Grid::new(w, h)` scales it across and down apart, with type and corners at the smaller scale; `grid::frame`: the frame's measures |
| `surface` | `Surface::new(&mut [u32], width, height, stride, PixelFormat)`: `clear`, `fill_rect`, `round_rect`, `elbow`, `pill`, `bar`, `text`, `text_fit`, anti-aliased. `PixelFormat::Xrgb8888` is DRM's XRGB8888 and the GOP's `PixelBlueGreenRedReserved8BitPerColor`; `Xbgr8888` is the GOP's `PixelRedGreenBlueReserved8BitPerColor` |
| `frame` | `frame::draw(&mut surface, &grid, &Frame { title, kind, release, blocks, keys, failed })`: the frame the screen draws first, which stormbootx's last frame should be |

## Using it from firmware

Pin a commit of this repo, as stormbootx pins stormboot-serve:

```toml
stormblock-screen-spec = { git = "https://github.com/glennswest/stormblock", rev = "<commit>" }
```

then draw on the GOP's framebuffer:

```rust
let mut s = Surface::new(fb, mode.width, mode.height, mode.pixels_per_scan_line, PixelFormat::Xrgb8888);
s.clear(palette::BG);
frame::draw(&mut s, &Grid::new(mode.width, mode.height), &Frame::default());
```

`cargo test -p stormblock-screen-spec` checks the font against its pin, the
pixel formats, the shapes on a padded stride in both formats, the frame at
its places, and the grid.
