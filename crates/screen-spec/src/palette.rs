//! The palette (#618, #637): a black field and five colours, each with a
//! meaning, plus the track and the ink. Values are `0x00RRGGBB`; a
//! [`crate::Surface`] writes them in its own pixel format.

/// `0x00RRGGBB`.
pub type Rgb = u32;

pub const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

// ---- the colours ----

/// The field everything sits on.
pub const BG: Rgb = rgb(0, 0, 0);
/// Amber: the header frame, what is running now, the big readout.
pub const AMBER: Rgb = rgb(255, 153, 0);
/// Lavender: the lower frame, the time line.
pub const LAVENDER: Rgb = rgb(204, 153, 204);
/// Pale blue: done, an address, a drive's name, the ETA.
pub const BLUE: Rgb = rgb(153, 204, 255);
/// Peach: the node's name, a slot, a reading.
pub const PEACH: Rgb = rgb(255, 204, 153);
/// Warm grey: labels and what is said in passing.
pub const GREY: Rgb = rgb(170, 160, 150);
/// Red: failed, stopped; the frames turn red on a failure.
pub const RED: Rgb = rgb(221, 68, 68);
/// The empty part of a bar, a step not reached.
pub const TRACK: Rgb = rgb(52, 46, 40);
/// Text on a coloured block or pill.
pub const INK: Rgb = rgb(0, 0, 0);

// ---- what each state is drawn in ----

/// The header frame.
pub const FRAME_HEAD: Rgb = AMBER;
/// The lower frame.
pub const FRAME_LOWER: Rgb = LAVENDER;
/// Both frames when something failed.
pub const FRAME_FAILED: Rgb = RED;
/// A step or a drive done.
pub const DONE: Rgb = BLUE;
/// A step or a drive at work.
pub const RUNNING: Rgb = AMBER;
/// A step not reached yet.
pub const PENDING: Rgb = TRACK;
/// A step or a drive that failed.
pub const FAILED: Rgb = RED;
/// A drive that wants something (not ready).
pub const WANTING: Rgb = PEACH;
/// A drive found and nothing else yet; a label.
pub const NEUTRAL: Rgb = GREY;
