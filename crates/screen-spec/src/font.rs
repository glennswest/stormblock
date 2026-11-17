//! The typeface (#618, #637): Antonio Bold, the one face the screen and the
//! boot panel draw in, included by bytes (`fonts/README.md`: source commit,
//! sha256, OFL 1.1). Glyphs are drawn by ab_glyph (no_std, libm), so both
//! sides draw the same pixels.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};

/// The font file.
pub static FONT: &[u8] = include_bytes!("../fonts/Antonio-Bold.ttf");
/// Where it is from.
pub const SOURCE: &str = "https://github.com/googlefonts/antonioFont 08562998b10c6c21a3505f47236b42f5d58c9909 Fonts/TTF/Antonio-Bold.ttf";
/// Its sha256, checked by this crate's tests.
pub const SHA256: &str = "f34e6f91b7cfd5d1d8e4fb2784d8ce9b198ca682014fe0f0886e51c0381b0d4d";
/// Its licence.
pub const LICENCE: &str = "SIL Open Font License 1.1";

/// The sizes the screen uses, in design pixels (1920 × 1080; scaled by
/// [`crate::Grid::p`]).
pub mod size {
    /// The big readout (percent, state).
    pub const BIG: f32 = 124.0;
    /// The node's name.
    pub const NAME: f32 = 72.0;
    /// A panel's heading.
    pub const PANEL: f32 = 64.0;
    /// A large reading (health).
    pub const READING: f32 = 58.0;
    /// The title cut into the header bar.
    pub const TITLE: f32 = 54.0;
    /// The ETA.
    pub const ETA: f32 = 44.0;
    /// The address.
    pub const ADDRESS: f32 = 40.0;
    /// A drive row.
    pub const ROW: f32 = 34.0;
    /// A panel's words, the machine line.
    pub const BODY: f32 = 32.0;
    /// A side block's label, a step's name.
    pub const LABEL: f32 = 30.0;
    /// A state pill.
    pub const PILL: f32 = 28.0;
    /// Under a step, the keys line.
    pub const SMALL: f32 = 26.0;
    /// Column headings.
    pub const HEADING: f32 = 24.0;
}

/// The face, parsed (no allocation).
pub fn face() -> FontRef<'static> {
    FontRef::try_from_slice(FONT).expect("the bundled typeface parses")
}

/// How wide `s` is at `px` pixels.
pub fn text_width(s: &str, px: f32) -> f32 {
    let font = face();
    let f = font.as_scaled(PxScale::from(px));
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

/// The longest prefix of `s` (in bytes) that fits `max` wide at `px`, and
/// whether it was cut (then an ellipsis follows it).
pub fn fit(s: &str, px: f32, max: f32) -> (usize, bool) {
    if text_width(s, px) <= max {
        return (s.len(), false);
    }
    let ell = text_width("…", px);
    let mut end = s.len();
    while end > 0 {
        end -= 1;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        if text_width(s[..end].trim_end(), px) + ell <= max {
            return (s[..end].trim_end().len(), true);
        }
    }
    (0, true)
}
