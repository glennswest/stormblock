//! The screen's pixels (#618): a buffer the size of the display, drawn on
//! through the shared spec's `Surface` (#637, `stormblock-screen-spec`: the
//! shapes, the typeface and their pixels are the spec's, the same as
//! stormbootx's boot panel), copied to DRM or written as a PNG.

pub use stormblock_screen_spec::palette::{rgb, Rgb};
pub use stormblock_screen_spec::{Align, Corners};
use stormblock_screen_spec::{font, PixelFormat, Surface};

pub struct Canvas {
    pub w: u32,
    pub h: u32,
    /// XRGB8888 (`0x00RRGGBB`), `w` to a row: DRM's format, and the spec's
    /// colours as they are.
    pub px: Vec<u32>,
}

impl Canvas {
    pub fn new(w: u32, h: u32, bg: Rgb) -> Canvas {
        Canvas { w, h, px: vec![bg; (w as usize) * (h as usize)] }
    }

    /// The spec's surface over these pixels.
    pub fn surface(&mut self) -> Surface<'_> {
        Surface::new(&mut self.px, self.w, self.h, self.w, PixelFormat::Xrgb8888)
    }

    pub fn clear(&mut self, bg: Rgb) {
        self.px.fill(bg);
    }

    pub fn get(&self, x: u32, y: u32) -> Rgb {
        self.px[y as usize * self.w as usize + x as usize]
    }

    pub fn fill_rect(&mut self, x: i64, y: i64, w: i64, h: i64, c: Rgb) {
        self.surface().fill_rect(x, y, w, h, c)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn round_rect(&mut self, x: i64, y: i64, w: i64, h: i64, r: i64, corners: Corners, c: Rgb) {
        self.surface().round_rect(x, y, w, h, r, corners, c)
    }

    pub fn pill(&mut self, x: i64, y: i64, w: i64, h: i64, c: Rgb) {
        self.surface().pill(x, y, w, h, c)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn bar(&mut self, x: i64, y: i64, w: i64, h: i64, frac: f32, track: Rgb, fill: Rgb) {
        self.surface().bar(x, y, w, h, frac, track, fill)
    }

    pub fn text(&mut self, x: f32, y: f32, px: f32, c: Rgb, align: Align, s: &str) -> f32 {
        self.surface().text(x, y, px, c, align, s)
    }

    pub fn text_width(&self, s: &str, px: f32) -> f32 {
        font::text_width(s, px)
    }

    /// Text cut to fit `max` wide, with an ellipsis.
    pub fn fit(&self, s: &str, px: f32, max: f32) -> String {
        match font::fit(s, px, max) {
            (n, false) => s[..n].to_string(),
            (n, true) => format!("{}…", &s[..n]),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The shapes are the spec's (its own tests cover them); here, that a
    /// canvas draws through it and writes a PNG.
    #[test]
    fn a_canvas_draws_through_the_spec() {
        let mut c = Canvas::new(300, 60, 0);
        c.fill_rect(10, 10, 20, 20, rgb(255, 0, 0));
        assert_eq!(c.get(15, 15), rgb(255, 0, 0));
        let w = c.text(40.0, 45.0, 40.0, rgb(255, 255, 255), Align::Left, "FORMATTING");
        assert!(w > 50.0 && w < 250.0, "{w}");
        assert!(c.px.contains(&rgb(255, 255, 255)));
        assert!(c.fit("A VERY LONG MODEL NAME INDEED", 40.0, 120.0).ends_with('…'));
        assert_eq!(&c.png()[1..4], b"PNG");
    }
}
