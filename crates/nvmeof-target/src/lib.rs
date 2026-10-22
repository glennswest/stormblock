//! NVMe/TCP for the stormblock engine and for a laptop that boots nodes (#474).
//!
//! Two halves:
//!
//! - **The wire half**, shared: PDUs ([`pdu`]), fabric commands ([`fabric`]),
//!   the discovery log page ([`discovery`]), DH-HMAC-CHAP ([`auth`]) and the
//!   Identify data ([`identify`]). The engine's target (`src/target/nvmeof`)
//!   re-exports these, so the engine and the laptop speak one copy and the two
//!   cannot drift.
//! - **A portable boot target**: [`server::BootTarget`] serves one image file
//!   read-only to named hosts, each through its own copy-on-write
//!   [`overlay::Overlay`], and [`boothost`] is the HTTP surface stormbootx
//!   asks before it attaches (intent, claim, installed). Tokio and plain std
//!   file I/O only, so it builds for macOS, Windows and Linux. The
//!   `stormboot-serve` command line puts the two together.
//!
//! Why an overlay and not a read-only namespace: the boot path writes to the
//! disk it was handed. stormuefi counts a boot attempt in the pallet's GPT
//! attributes, a diskless node keeps its data there, and an install discards
//! what it has moved. Each host's writes land in its own scratch file; the
//! image itself is never opened for writing.

pub mod auth;
pub mod boothost;
pub mod command;
pub mod discovery;
pub mod fabric;
pub mod identify;
pub mod image;
pub mod overlay;
pub mod pdu;
pub mod server;

/// The most a target may be told to take in one command: NLB is 16 bits, so
/// 65536 blocks of 512 bytes is the most a command can name on a 512-byte
/// namespace.
pub const MAX_TRANSFER_CEILING: usize = 32 << 20;
