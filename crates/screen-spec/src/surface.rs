//! The shapes the screen is made of, drawn the same on any display (#618,
//! #637): filled and rounded rectangles, the frame's elbows, pills, bars,
//! and text in the bundled typeface, with anti-aliased edges, over a plain
//! `&mut [u32]` with a stride and a pixel format. No allocation.

use ab_glyph::{Font, PxScale, ScaleFont};

use crate::palette::Rgb;

/// How a 32-bit pixel holds its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// `0x00RRGGBB` (bytes B, G, R, X): DRM's XRGB8888, and the UEFI GOP's
    /// `PixelBlueGreenRedReserved8BitPerColor`.
    Xrgb8888,
    /// `0x00BBGGRR` (bytes R, G, B, X): the UEFI GOP's
    /// `PixelRedGreenBlueReserved8BitPerColor`.
    Xbgr8888,
}

impl PixelFormat {
    /// A colour as this format's pixel.
    pub fn pixel(self, c: Rgb) -> u32 {
        match self {
            PixelFormat::Xrgb8888 => c & 0x00FF_FFFF,
            PixelFormat::Xbgr8888 => ((c & 0xFF) << 16) | (c & 0xFF00) | ((c >> 16) & 0xFF),
        }
    }
    /// A pixel of this format as a colour.
    pub fn colour(self, p: u32) -> Rgb {
        // The swap is its own inverse.
        self.pixel(p)
    }
}

/// Which corners of a rectangle are rounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Corners {
    pub tl: bool,
    pub tr: bool,
    pub bl: bool,
    pub br: bool,
}

impl Corners {
    pub const ALL: Corners = Corners { tl: true, tr: true, bl: true, br: true };
    pub const LEFT: Corners = Corners { tl: true, tr: false, bl: true, br: false };
    pub const RIGHT: Corners = Corners { tl: false, tr: true, bl: false, br: true };
    pub const NONE: Corners = Corners { tl: false, tr: false, bl: false, br: false };
}

/// Text alignment against `x`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
}

fn mix(under: Rgb, over: Rgb, a: f32) -> Rgb {
    if a >= 1.0 {
        return over;
    }
    if a <= 0.0 {
        return under;
    }
    let ch = |s: u32| {
        let u = ((under >> s) & 0xFF) as f32;
        let o = ((over >> s) & 0xFF) as f32;
        (libm::roundf(u + (o - u) * a) as u32) << s
    };
    ch(16) | ch(8) | ch(0)
}

fn dist(ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    libm::sqrtf((ax - bx) * (ax - bx) + (ay - by) * (ay - by))
}

/// A display's pixels to draw on.
pub struct Surface<'a> {
    px: &'a mut [u32],
    pub width: u32,
    pub height: u32,
    /// Pixels from the start of one row to the next.
    pub stride: u32,
    pub format: PixelFormat,
}

impl<'a> Surface<'a> {
    /// `px` holds `stride × height` pixels (at least).
    pub fn new(px: &'a mut [u32], width: u32, height: u32, stride: u32, format: PixelFormat) -> Surface<'a> {
        assert!(stride >= width && px.len() >= stride as usize * height as usize, "a surface's pixels cover stride × height");
        Surface { px, width, height, stride, format }
    }

    #[inline]
    fn at(&self, x: i64, y: i64) -> Option<usize> {
        (x >= 0 && y >= 0 && x < self.width as i64 && y < self.height as i64).then(|| y as usize * self.stride as usize + x as usize)
    }

    /// The colour at (`x`, `y`).
    pub fn get(&self, x: u32, y: u32) -> Rgb {
        self.at(x as i64, y as i64).map(|i| self.format.colour(self.px[i])).unwrap_or(0)
    }

    #[inline]
    fn blend(&mut self, x: i64, y: i64, c: Rgb, a: f32) {
        if a <= 0.0 {
            return;
        }
        if let Some(i) = self.at(x, y) {
            let under = self.format.colour(self.px[i]);
            self.px[i] = self.format.pixel(mix(under, c, a));
        }
    }

    pub fn clear(&mut self, c: Rgb) {
        let p = self.format.pixel(c);
        for y in 0..self.height as usize {
            let row = y * self.stride as usize;
            self.px[row..row + self.width as usize].fill(p);
        }
    }

    pub fn fill_rect(&mut self, x: i64, y: i64, w: i64, h: i64, c: Rgb) {
        let (x0, y0) = (x.max(0), y.max(0));
        let (x1, y1) = ((x + w).min(self.width as i64), (y + h).min(self.height as i64));
        if x1 <= x0 {
            return;
        }
        let p = self.format.pixel(c);
        for yy in y0..y1 {
            let row = yy as usize * self.stride as usize;
            self.px[row + x0 as usize..row + x1 as usize].fill(p);
        }
    }

    /// A rectangle with the corners asked for rounded at radius `r`.
    #[allow(clippy::too_many_arguments)]
    pub fn round_rect(&mut self, x: i64, y: i64, w: i64, h: i64, r: i64, corners: Corners, c: Rgb) {
        let r = r.min(w / 2).min(h / 2).max(0);
        for yy in y..y + h {
            for xx in x..x + w {
                let a = corner_cover(xx, yy, x, y, w, h, r, corners);
                if a > 0.0 {
                    self.blend(xx, yy, c, a);
                }
            }
        }
    }

    /// The frame's elbow: a vertical bar `bw` wide turning into a horizontal
    /// bar `bh` high, the outer corner rounded at `ro` and the inner at `ri`;
    /// `top`: it turns at the top (┌), else at the bottom (└).
    #[allow(clippy::too_many_arguments)]
    pub fn elbow(&mut self, x: i64, y: i64, w: i64, h: i64, bw: i64, bh: i64, ro: i64, ri: i64, top: bool, c: Rgb) {
        for yy in y..y + h {
            for xx in x..x + w {
                let (lx, ly) = (xx - x, if top { yy - y } else { y + h - 1 - yy });
                let in_vert = lx < bw;
                let in_horz = ly < bh;
                if !in_vert && !in_horz {
                    if lx < bw + ri && ly < bh + ri {
                        let d = dist(lx as f32 + 0.5, ly as f32 + 0.5, (bw + ri) as f32, (bh + ri) as f32);
                        self.blend(xx, yy, c, (d - ri as f32 + 0.5).clamp(0.0, 1.0));
                    }
                    continue;
                }
                let a = if lx < ro && ly < ro {
                    let d = dist(lx as f32 + 0.5, ly as f32 + 0.5, ro as f32, ro as f32);
                    (ro as f32 + 0.5 - d).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                self.blend(xx, yy, c, a);
            }
        }
    }

    /// A bar with both ends round.
    pub fn pill(&mut self, x: i64, y: i64, w: i64, h: i64, c: Rgb) {
        self.round_rect(x, y, w, h, h / 2, Corners::ALL, c);
    }

    /// A progress bar: the track, then `frac` of it filled.
    #[allow(clippy::too_many_arguments)]
    pub fn bar(&mut self, x: i64, y: i64, w: i64, h: i64, frac: f32, track: Rgb, fill: Rgb) {
        self.pill(x, y, w, h, track);
        let f = frac.clamp(0.0, 1.0);
        if f > 0.0 {
            let fw = libm::roundf(w as f32 * f) as i64;
            self.pill(x, y, fw.max(h), h, fill);
        }
    }

    /// Text with its baseline at `y`, aligned against `x`; answers its width.
    pub fn text(&mut self, x: f32, y: f32, px: f32, c: Rgb, align: Align, s: &str) -> f32 {
        let width = crate::font::text_width(s, px);
        let mut pen = match align {
            Align::Left => x,
            Align::Right => x - width,
            Align::Center => x - width / 2.0,
        };
        let font = crate::font::face();
        let f = font.as_scaled(PxScale::from(px));
        let mut prev = None;
        for ch in s.chars() {
            let id = f.glyph_id(ch);
            if let Some(p) = prev {
                pen += f.kern(p, id);
            }
            let g = id.with_scale_and_position(PxScale::from(px), ab_glyph::point(pen, y));
            if let Some(o) = font.outline_glyph(g) {
                let b = o.px_bounds();
                o.draw(|gx, gy, cov| self.blend(b.min.x as i64 + gx as i64, b.min.y as i64 + gy as i64, c, cov));
            }
            pen += f.h_advance(id);
            prev = Some(id);
        }
        width
    }

    /// Text cut to fit `max` wide, an ellipsis after it when cut.
    #[allow(clippy::too_many_arguments)]
    pub fn text_fit(&mut self, x: f32, y: f32, px: f32, c: Rgb, align: Align, s: &str, max: f32) -> f32 {
        let (n, cut) = crate::font::fit(s, px, max);
        if !cut {
            return self.text(x, y, px, c, align, s);
        }
        let w = crate::font::text_width(&s[..n], px) + crate::font::text_width("…", px);
        let start = match align {
            Align::Left => x,
            Align::Right => x - w,
            Align::Center => x - w / 2.0,
        };
        let w1 = self.text(start, y, px, c, Align::Left, &s[..n]);
        self.text(start + w1, y, px, c, Align::Left, "…");
        w
    }
}

/// How much of the pixel at (`px`, `py`) a rectangle with rounded corners covers.
#[allow(clippy::too_many_arguments)]
fn corner_cover(px: i64, py: i64, x: i64, y: i64, w: i64, h: i64, r: i64, c: Corners) -> f32 {
    if r == 0 {
        return 1.0;
    }
    let (lx, ly) = (px - x, py - y);
    let (cx, cy, round) = if lx < r && ly < r {
        (r, r, c.tl)
    } else if lx >= w - r && ly < r {
        (w - r, r, c.tr)
    } else if lx < r && ly >= h - r {
        (r, h - r, c.bl)
    } else if lx >= w - r && ly >= h - r {
        (w - r, h - r, c.br)
    } else {
        return 1.0;
    };
    if !round {
        return 1.0;
    }
    let d = dist(lx as f32 + 0.5, ly as f32 + 0.5, cx as f32, cy as f32);
    (r as f32 + 0.5 - d).clamp(0.0, 1.0)
}
