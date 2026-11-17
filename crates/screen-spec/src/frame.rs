//! The frame the screen draws first (#618, #637): the header's elbow with
//! the title cut into its bar, the lower elbow, their end caps, the side
//! blocks with their labels, and the key line. stormbootx's last frame is
//! this, so the hand-over shows no change of frame.

use crate::font::size;
use crate::grid::{frame as m, Grid};
use crate::palette::{self, Rgb};
use crate::surface::{Align, Corners, Surface};

/// What the frame says.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    /// Cut into the header's bar, on the right: `STORMCOS  INSTALL`.
    pub title: &'a str,
    /// The header's upper side block: `INSTALL` or `BOOT`.
    pub kind: &'a str,
    /// The header's lower side block: the release.
    pub release: &'a str,
    /// The lower frame's side blocks, top to bottom (`DRIVES`, `STEPS`,
    /// `HEALTH`), in `grid::frame::BLOCKS`' places.
    pub blocks: [&'a str; 3],
    /// Along the bottom bar, on the right: the keys.
    pub keys: &'a str,
    /// Both frames red.
    pub failed: bool,
}

impl Default for Frame<'_> {
    fn default() -> Self {
        Frame {
            title: "STORMCOS  BOOT",
            kind: "BOOT",
            release: "",
            blocks: ["DRIVES", "STEPS", "HEALTH"],
            keys: "F2  LOG    F1  THIS SCREEN",
            failed: false,
        }
    }
}

/// The colours of the lower frame's side blocks, top to bottom.
pub const BLOCK_COLOURS: [Rgb; 3] = [palette::BLUE, palette::AMBER, palette::PEACH];

/// Draw the frame (on a cleared surface).
pub fn draw(s: &mut Surface, g: &Grid, f: &Frame) {
    let head = if f.failed { palette::FRAME_FAILED } else { palette::FRAME_HEAD };
    let lower = if f.failed { palette::FRAME_FAILED } else { palette::FRAME_LOWER };
    // The header frame.
    s.elbow(
        g.x(m::MARGIN), g.y(m::HEAD_TOP), g.lw(m::RIGHT - m::MARGIN), g.lh(m::HEAD_H),
        g.lw(m::BAR_W), g.lh(m::HEAD_BAR_H), g.l(m::OUTER_R), g.l(m::INNER_R), true, head,
    );
    // The title, cut into the bar, and the bar's end cap.
    let tw = crate::font::text_width(f.title, g.p(size::TITLE));
    s.fill_rect(g.x(m::RIGHT) - tw as i64 - g.lw(48.0), g.y(m::HEAD_TOP), tw as i64 + g.lw(24.0), g.lh(m::HEAD_BAR_H), palette::BG);
    s.text(g.xf(m::RIGHT - 36.0), g.yf(66.0), g.p(size::TITLE), head, Align::Right, f.title);
    s.round_rect(g.x(m::RIGHT) - g.lw(16.0), g.y(m::HEAD_TOP), g.lw(16.0), g.lh(m::HEAD_BAR_H), g.l(8.0), Corners::RIGHT, head);
    // The header's side blocks.
    s.fill_rect(g.x(m::MARGIN), g.y(m::HEAD_BLOCK_TOP - m::GAP), g.lw(m::BAR_W), g.lh(m::GAP), palette::BG);
    s.fill_rect(g.x(m::MARGIN), g.y(m::HEAD_BLOCK_TOP), g.lw(m::BAR_W), g.lh(m::HEAD_BLOCK_H), palette::GREY);
    let label_x = g.xf(m::MARGIN + m::BAR_W - 10.0);
    s.text(label_x, g.yf(m::HEAD_BLOCK_TOP - 12.0), g.p(size::LABEL), palette::INK, Align::Right, f.kind);
    s.text_fit(label_x, g.yf(m::HEAD_BLOCK_TOP + m::HEAD_BLOCK_H - 8.0), g.p(size::LABEL), palette::INK, Align::Right, f.release, g.p(m::BAR_W - 20.0));

    // The lower frame.
    s.elbow(
        g.x(m::MARGIN), g.y(m::LOWER_TOP), g.lw(m::RIGHT - m::MARGIN), g.lh(m::LOWER_H),
        g.lw(m::BAR_W), g.lh(m::LOWER_BAR_H), g.l(m::OUTER_R), g.l(m::INNER_R), false, lower,
    );
    let bar_top = m::LOWER_TOP + m::LOWER_H - m::LOWER_BAR_H;
    s.round_rect(g.x(m::RIGHT) - g.lw(16.0), g.y(bar_top), g.lw(16.0), g.lh(m::LOWER_BAR_H), g.l(8.0), Corners::RIGHT, lower);
    for (i, (top, h)) in m::BLOCKS.iter().enumerate() {
        s.fill_rect(g.x(m::MARGIN), g.y(*top), g.lw(m::BAR_W), g.lh(*h), BLOCK_COLOURS[i]);
        s.fill_rect(g.x(m::MARGIN), g.y(top + h), g.lw(m::BAR_W), g.lh(m::GAP), palette::BG);
        s.text(label_x, g.yf(top + h - 12.0), g.p(size::LABEL), palette::INK, Align::Right, f.blocks[i]);
    }
    // The key line along the bottom bar.
    let kw = crate::font::text_width(f.keys, g.p(size::SMALL));
    s.fill_rect(g.x(m::RIGHT) - kw as i64 - g.lw(60.0), g.y(bar_top), kw as i64 + g.lw(24.0), g.lh(m::LOWER_BAR_H), palette::BG);
    s.text(g.xf(m::RIGHT - 48.0), g.yf(bar_top + 30.0), g.p(size::SMALL), lower, Align::Right, f.keys);
}
