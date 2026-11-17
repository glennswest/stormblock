//! The spec's own checks (#637).

extern crate std;
use std::vec;

use crate::font;
use crate::frame::{self, Frame};
use crate::grid::{frame as m, Grid};
use crate::palette::{self, rgb};
use crate::surface::{Align, Corners, PixelFormat, Surface};

/// The font in the crate is the pinned file (fonts/README.md).
#[test]
fn the_font_is_the_pinned_one() {
    use sha2::Digest;
    let got: std::string::String = sha2::Sha256::digest(font::FONT).iter().map(|b| std::format!("{b:02x}")).collect();
    assert_eq!(got, font::SHA256);
    assert_eq!(font::FONT.len(), 55416);
    assert!(font::text_width("STORMCOS", 54.0) > 100.0);
}

/// XRGB8888 (DRM, the GOP's BGRx) keeps a colour as it is; XBGR8888 (the
/// GOP's RGBx) swaps red and blue, and back.
#[test]
fn pixel_formats() {
    let c = rgb(0x11, 0x22, 0x33);
    assert_eq!(PixelFormat::Xrgb8888.pixel(c), 0x0011_2233);
    assert_eq!(PixelFormat::Xbgr8888.pixel(c), 0x0033_2211);
    assert_eq!(PixelFormat::Xbgr8888.colour(0x0033_2211), c);
    // In memory, XBGR8888 is R, G, B, X: what the GOP's RGBx is.
    assert_eq!(PixelFormat::Xbgr8888.pixel(c).to_le_bytes(), [0x11, 0x22, 0x33, 0]);
    assert_eq!(PixelFormat::Xrgb8888.pixel(c).to_le_bytes(), [0x33, 0x22, 0x11, 0]);
}

/// The shapes, on a surface whose rows are longer than its width (a GOP's
/// PixelsPerScanLine), in either format: the padding is never written.
#[test]
fn shapes_land_where_asked_in_either_format() {
    for format in [PixelFormat::Xrgb8888, PixelFormat::Xbgr8888] {
        let (w, h, stride) = (200u32, 100u32, 256u32);
        let mut px = vec![0xDEAD_BEEFu32; (stride * h) as usize];
        {
        let mut s = Surface::new(&mut px, w, h, stride, format);
        s.clear(palette::BG);
        s.fill_rect(10, 10, 20, 20, palette::RED);
        assert_eq!(s.get(15, 15), palette::RED);
        assert_eq!(s.get(31, 15), palette::BG);
        s.round_rect(100, 10, 40, 40, 15, Corners::ALL, palette::BLUE);
        assert_eq!(s.get(100, 10), palette::BG, "a rounded corner");
        assert_eq!(s.get(120, 30), palette::BLUE);
        s.bar(10, 60, 100, 10, 0.5, palette::TRACK, palette::AMBER);
        assert_eq!(s.get(30, 65), palette::AMBER);
        assert_eq!(s.get(90, 65), palette::TRACK);
        s.fill_rect(150, 0, 500, 5, palette::PEACH);
        }
        assert_eq!(px[(stride * 2 + w + 3) as usize], 0xDEAD_BEEF, "past the width is the stride's padding, untouched");
    }
}

#[test]
fn an_elbow_turns_and_text_draws() {
    let mut px = vec![0u32; 300 * 200];
    let mut s = Surface::new(&mut px, 300, 200, 300, PixelFormat::Xrgb8888);
    s.elbow(0, 0, 300, 200, 60, 20, 40, 15, true, palette::AMBER);
    assert_eq!(s.get(0, 0), 0, "the outer corner is round");
    assert_eq!(s.get(30, 150), palette::AMBER, "the vertical bar");
    assert_eq!(s.get(250, 10), palette::AMBER, "the horizontal bar");
    assert_eq!(s.get(250, 100), 0, "inside the frame");
    assert_eq!(s.get(62, 22), palette::AMBER, "the inner fillet");
    let wd = s.text(80.0, 120.0, 40.0, palette::PEACH, Align::Left, "FORMATTING");
    assert!(wd > 50.0 && wd < 220.0);
    let cut = s.text_fit(80.0, 180.0, 40.0, palette::PEACH, Align::Left, "A VERY LONG MODEL NAME INDEED", 120.0);
    assert!(cut <= 121.0, "{cut}");
}

/// The frame at the design grid's places: what stormbootx's last frame and
/// the screen's first have in common.
#[test]
fn the_frame() {
    let mut px = vec![0u32; 1920 * 1080];
    let g = Grid::new(1920, 1080);
    let mut s = Surface::new(&mut px, 1920, 1080, 1920, PixelFormat::Xrgb8888);
    frame::draw(&mut s, &g, &Frame { title: "STORMCOS  BOOT", kind: "BOOT", release: "RELEASE 12.92", ..Default::default() });
    assert_eq!(s.get(100, 100), palette::FRAME_HEAD, "the header's vertical bar");
    assert_eq!(s.get(800, 40), palette::FRAME_HEAD, "the header's bar");
    assert_eq!(s.get(30, (m::HEAD_BLOCK_TOP + 20.0) as u32), palette::GREY, "the header's lower block");
    assert_eq!(s.get(30, 400), palette::BLUE, "DRIVES");
    assert_eq!(s.get(30, 700), palette::AMBER, "STEPS");
    assert_eq!(s.get(30, 900), palette::PEACH, "HEALTH");
    assert_eq!(s.get(800, 1040), palette::FRAME_LOWER, "the lower bar");
    assert_eq!(s.get(800, 600), palette::BG, "inside");
    frame::draw(&mut s, &g, &Frame { failed: true, ..Default::default() });
    assert_eq!(s.get(800, 40), palette::FRAME_FAILED);
    // On a 4:3 console the frame still reaches the corners.
    let mut small = vec![0u32; 1024 * 768];
    let g2 = Grid::new(1024, 768);
    let mut s2 = Surface::new(&mut small, 1024, 768, 1024, PixelFormat::Xbgr8888);
    frame::draw(&mut s2, &g2, &Frame::default());
    assert_eq!(s2.get(1000, 25), palette::FRAME_HEAD);
    assert_eq!(s2.get(500, 742), palette::FRAME_LOWER);
}

/// No allocator, no std: the crate builds for firmware (checked here by
/// nothing but the crate's own `#![no_std]`; the build job builds it for
/// x86_64-unknown-uefi too).
#[test]
fn grid_scales() {
    let g = Grid::new(1024, 768);
    assert_eq!(g.x(1920.0), 1024);
    assert_eq!(g.y(1080.0), 768);
    assert!((g.k - 1024.0 / 1920.0).abs() < 1e-6);
}
