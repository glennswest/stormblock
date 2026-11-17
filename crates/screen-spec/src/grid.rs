//! The design grid (#618, #637): everything is laid out on 1920 × 1080
//! design pixels and stretched to the display across and down apart, so a
//! 4:3 or 5:4 console (a BMC's) is filled, not letterboxed. Type and corners
//! take the smaller of the two scales, so nothing is distorted.

/// The design grid's width and height.
pub const DESIGN_W: f32 = 1920.0;
pub const DESIGN_H: f32 = 1080.0;

/// A display's scale against the design grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grid {
    /// Across.
    pub kx: f32,
    /// Down.
    pub ky: f32,
    /// Type and corners: the smaller.
    pub k: f32,
}

impl Grid {
    pub fn new(width: u32, height: u32) -> Grid {
        let (kx, ky) = (width as f32 / DESIGN_W, height as f32 / DESIGN_H);
        Grid { kx, ky, k: if kx < ky { kx } else { ky } }
    }
    /// A design x as a display column.
    pub fn x(&self, v: f32) -> i64 {
        (v * self.kx) as i64
    }
    /// A design y as a display row.
    pub fn y(&self, v: f32) -> i64 {
        (v * self.ky) as i64
    }
    pub fn xf(&self, v: f32) -> f32 {
        v * self.kx
    }
    pub fn yf(&self, v: f32) -> f32 {
        v * self.ky
    }
    /// A length across.
    pub fn lw(&self, v: f32) -> i64 {
        (v * self.kx) as i64
    }
    /// A length down.
    pub fn lh(&self, v: f32) -> i64 {
        (v * self.ky) as i64
    }
    /// A radius.
    pub fn l(&self, v: f32) -> i64 {
        (v * self.k) as i64
    }
    /// A type size.
    pub fn p(&self, v: f32) -> f32 {
        v * self.k
    }
}

/// The frame's measures, in design pixels (`crate::frame` draws them).
pub mod frame {
    /// The margin around both frames.
    pub const MARGIN: f32 = 24.0;
    /// The right edge of both frames.
    pub const RIGHT: f32 = 1896.0;
    /// The width of the frames' vertical bars.
    pub const BAR_W: f32 = 200.0;
    /// The header frame: top, height, the height of its bar.
    pub const HEAD_TOP: f32 = 24.0;
    pub const HEAD_H: f32 = 300.0;
    pub const HEAD_BAR_H: f32 = 44.0;
    /// The lower frame: top, height, the height of its bar (at the bottom).
    pub const LOWER_TOP: f32 = 348.0;
    pub const LOWER_H: f32 = 708.0;
    pub const LOWER_BAR_H: f32 = 36.0;
    /// The elbows' outer and inner radii.
    pub const OUTER_R: f32 = 70.0;
    pub const INNER_R: f32 = 28.0;
    /// Where the content starts, right of the vertical bars.
    pub const CONTENT_X: f32 = 260.0;
    /// The header's side blocks: the lower one's top and height.
    pub const HEAD_BLOCK_TOP: f32 = 202.0;
    pub const HEAD_BLOCK_H: f32 = 122.0;
    /// The lower frame's side blocks: (top, height) of DRIVES, STEPS, HEALTH.
    pub const BLOCKS: [(f32, f32); 3] = [(348.0, 328.0), (682.0, 186.0), (874.0, 108.0)];
    /// The gap between blocks.
    pub const GAP: f32 = 6.0;
}
