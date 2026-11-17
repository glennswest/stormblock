//! The boot and install screen's look, as one spec both of its drawers build
//! from (stormblock#637): stormblock's `screen` (#618, on DRM after the
//! kernel has the display) and stormbootx's boot panel (stormbootx#220, on
//! the UEFI GOP before it), so the machine looks like one product from power
//! on and the two never drift.
//!
//! `no_std`, `core` only, no allocator: firmware builds it for
//! `x86_64-unknown-uefi` and `aarch64-unknown-uefi`.
//!
//! - [`palette`]: the colours, named by what each means.
//! - [`font`]: the typeface (Antonio Bold, OFL 1.1, pinned) and its sizes.
//! - [`grid`]: the 1920 × 1080 design grid and how it scales to a display.
//! - [`surface`]: the shapes (rectangles, rounded rectangles, elbows, pills,
//!   bars) and text over a plain `&mut [u32]` with a stride and a pixel
//!   format (the GOP's BGRx or RGBx, DRM's XRGB8888).
//! - [`frame`]: the frame the screen draws first, which stormbootx's last
//!   frame matches so the hand-over shows no change of frame.
//!
//! The look: a starship bridge status display in spirit (owner: "an
//! enterprise boot screen (As in trek)"), our own design. Nothing copied,
//! nothing named after anyone else's.

#![no_std]

pub mod font;
pub mod frame;
pub mod grid;
pub mod palette;
pub mod surface;

pub use grid::Grid;
pub use palette::Rgb;
pub use surface::{Align, Corners, PixelFormat, Surface};

#[cfg(test)]
mod tests;
