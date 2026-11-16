//! A software canvas for the boot and install screen (#618): XRGB8888
//! pixels, the shapes the screen is made of (filled and rounded rectangles,
//! the "elbow" frames, bars) with anti-aliased edges, and text from the
//! bundled typeface. Drawn the same into a DRM dumb buffer or a PNG.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};

/// The bundled typeface: Antonio Bold (OFL 1.1, `fonts/README.md`).
static FONT_BYTES: &[u8] = include_bytes!("fonts/Antonio-Bold.ttf");

pub fn font() -> FontRef<'static> {
    FontRef::try_from_slice(FONT_BYTES).expect("the bundled typeface parses")
}

/// `0x00RRGGBB`.
pub type Rgb = u32;

pub const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
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
        ((u + (o - u) * a).round() as u32) << s
    };
    ch(16) | ch(8) | ch(0)
}

/// Which corner of a rectangle is rounded.
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

pub struct Canvas {
    pub w: u32,
    pub h: u32,
    pub px: Vec<Rgb>,
    font: FontRef<'static>,
}

impl Canvas {
    pub fn new(w: u32, h: u32, bg: Rgb) -> Canvas {
        Canvas { w, h, px: vec![bg; (w as usize) * (h as usize)], font: font() }
    }

    pub fn clear(&mut self, bg: Rgb) {
        self.px.fill(bg);
    }

    #[inline]
    fn blend(&mut self, x: i64, y: i64, c: Rgb, a: f32) {
        if x < 0 || y < 0 || x >= self.w as i64 || y >= self.h as i64 || a <= 0.0 {
            return;
        }
        let i = y as usize * self.w as usize + x as usize;
        self.px[i] = mix(self.px[i], c, a);
    }

    pub fn get(&self, x: u32, y: u32) -> Rgb {
        self.px[y as usize * self.w as usize + x as usize]
    }

    pub fn fill_rect(&mut self, x: i64, y: i64, w: i64, h: i64, c: Rgb) {
        let (x0, y0) = (x.max(0), y.max(0));
        let (x1, y1) = ((x + w).min(self.w as i64), (y + h).min(self.h as i64));
        for yy in y0..y1 {
            let row = yy as usize * self.w as usize;
            self.px[row + x0 as usize..row + x1.max(x0) as usize].fill(c);
        }
    }

    /// A rectangle with the corners asked for rounded at radius `r`,
    /// anti-aliased on the curves.
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

    /// The frame's "elbow": a vertical bar of width `bw` that turns into a
    /// horizontal bar of height `bh` with a rounded outer corner (`ro`) and
    /// a rounded inner one (`ri`). `top`: the bar turns at the top (┌), else
    /// at the bottom (└). The bars run from (`x`,`y`) for `w` by `h`.
    #[allow(clippy::too_many_arguments)]
    pub fn elbow(&mut self, x: i64, y: i64, w: i64, h: i64, bw: i64, bh: i64, ro: i64, ri: i64, top: bool, c: Rgb) {
        for yy in y..y + h {
            for xx in x..x + w {
                let (lx, ly) = (xx - x, if top { yy - y } else { y + h - 1 - yy });
                let in_vert = lx < bw;
                let in_horz = ly < bh;
                if !in_vert && !in_horz {
                    // The inner fillet, between the two bars.
                    if lx < bw + ri && ly < bh + ri {
                        let (cx, cy) = ((bw + ri) as f32, (bh + ri) as f32);
                        let d = (((lx as f32 + 0.5) - cx).powi(2) + ((ly as f32 + 0.5) - cy).powi(2)).sqrt();
                        let a = (d - ri as f32 + 0.5).clamp(0.0, 1.0);
                        self.blend(xx, yy, c, a);
                    }
                    continue;
                }
                // The outer corner.
                let a = if lx < ro && ly < ro {
                    let d = (((lx as f32 + 0.5) - ro as f32).powi(2) + ((ly as f32 + 0.5) - ro as f32).powi(2)).sqrt();
                    (ro as f32 + 0.5 - d).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                self.blend(xx, yy, c, a);
            }
        }
    }

    /// A pill: a rounded bar with both ends round.
    pub fn pill(&mut self, x: i64, y: i64, w: i64, h: i64, c: Rgb) {
        self.round_rect(x, y, w, h, h / 2, Corners::ALL, c);
    }

    /// A progress bar: the track, then `frac` of it filled.
    #[allow(clippy::too_many_arguments)]
    pub fn bar(&mut self, x: i64, y: i64, w: i64, h: i64, frac: f32, track: Rgb, fill: Rgb) {
        self.pill(x, y, w, h, track);
        let f = frac.clamp(0.0, 1.0);
        if f > 0.0 {
            let fw = ((w as f32) * f).round() as i64;
            self.pill(x, y, fw.max(h), h, fill);
        }
    }

    /// The width text of size `px` takes.
    pub fn text_width(&self, s: &str, px: f32) -> f32 {
        let f = self.font.as_scaled(PxScale::from(px));
        let mut w = 0.0;
        let mut prev = None;
        for ch in s.chars() {
            let id = f.glyph_id(ch);
            if let Some(p) = prev {
                w += f.kern(p, id);
            }
            w += f.h_advance(id);
            prev = Some(id);
        }
        w
    }

    /// Text with its baseline at `y`, aligned against `x`; answers its width.
    pub fn text(&mut self, x: f32, y: f32, px: f32, c: Rgb, align: Align, s: &str) -> f32 {
        let width = self.text_width(s, px);
        let mut pen = match align {
            Align::Left => x,
            Align::Right => x - width,
            Align::Center => x - width / 2.0,
        };
        let font = self.font.clone();
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

    /// Text cut to fit `max` wide, with an ellipsis.
    pub fn fit(&self, s: &str, px: f32, max: f32) -> String {
        if self.text_width(s, px) <= max {
            return s.to_string();
        }
        let mut out: String = s.to_string();
        while !out.is_empty() && self.text_width(&format!("{out}…"), px) > max {
            out.pop();
        }
        format!("{}…", out.trim_end())
    }

    /// The canvas as a PNG (8-bit RGB, one zlib stream): for snapshots.
    pub fn png(&self) -> Vec<u8> {
        use std::io::Write;
        let mut raw = Vec::with_capacity((self.w as usize * 3 + 1) * self.h as usize);
        for y in 0..self.h {
            raw.push(0u8);
            for x in 0..self.w {
                let p = self.get(x, y);
                raw.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
            }
        }
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&raw).expect("in memory");
        let idat = z.finish().expect("in memory");
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let chunk = |out: &mut Vec<u8>, kind: &[u8], data: &[u8]| {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut h = crc32fast::Hasher::new();
            h.update(kind);
            h.update(data);
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            out.extend_from_slice(&h.finalize().to_be_bytes());
        };
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&self.w.to_be_bytes());
        ihdr.extend_from_slice(&self.h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &idat);
        chunk(&mut out, b"IEND", &[]);
        out
    }
}

/// How much of the pixel at (`px`,`py`) a rectangle with rounded corners covers.
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
    let d = (((lx as f32 + 0.5) - cx as f32).powi(2) + ((ly as f32 + 0.5) - cy as f32).powi(2)).sqrt();
    (r as f32 + 0.5 - d).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_and_text_land_where_asked() {
        let mut c = Canvas::new(200, 100, 0);
        c.fill_rect(10, 10, 20, 20, rgb(255, 0, 0));
        assert_eq!(c.get(15, 15), rgb(255, 0, 0));
        assert_eq!(c.get(31, 15), 0);
        // A rounded corner leaves the very corner pixel dark.
        c.round_rect(100, 10, 40, 40, 15, Corners::ALL, rgb(0, 255, 0));
        assert_eq!(c.get(100, 10), 0);
        assert_eq!(c.get(120, 30), rgb(0, 255, 0));
        // A bar half full: its left half the fill, its right the track.
        c.bar(10, 60, 100, 10, 0.5, rgb(40, 40, 40), rgb(255, 160, 0));
        assert_eq!(c.get(30, 65), rgb(255, 160, 0));
        assert_eq!(c.get(90, 65), rgb(40, 40, 40));
        // Text draws, and is as wide as it measures.
        let mut t = Canvas::new(300, 60, 0);
        let w = t.text(10.0, 45.0, 40.0, rgb(255, 255, 255), Align::Left, "FORMATTING");
        assert!(w > 50.0 && w < 290.0, "{w}");
        assert!(t.px.iter().any(|p| *p == rgb(255, 255, 255)));
        assert!(t.fit("A VERY LONG MODEL NAME INDEED", 40.0, 120.0).ends_with('…'));
        // A PNG of it.
        let png = t.png();
        assert_eq!(&png[1..4], b"PNG");
    }

    #[test]
    fn an_elbow_turns() {
        let mut c = Canvas::new(300, 200, 0);
        c.elbow(0, 0, 300, 200, 60, 20, 40, 15, true, rgb(255, 153, 0));
        assert_eq!(c.get(0, 0), 0, "the outer corner is round");
        assert_eq!(c.get(30, 150), rgb(255, 153, 0), "the vertical bar");
        assert_eq!(c.get(250, 10), rgb(255, 153, 0), "the horizontal bar");
        assert_eq!(c.get(250, 100), 0, "inside the frame");
        assert_eq!(c.get(62, 22), rgb(255, 153, 0), "the inner fillet");
    }
}
