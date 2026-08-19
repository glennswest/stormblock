//! Serving — what it takes to hand a volume to something, whatever that is.
//!
//! Between the engine (how storage works) and a profile (how *this*
//! deployment is wired). It owns the parts every deployment needs and none
//! of the parts any deployment chooses: reclaiming space a filesystem has
//! freed, moving content in and out of a volume, and knowing whether an
//! initiator is still attached.
//!
//! It deliberately knows nothing about what attaches. A container root, a VM
//! disk and a micro-VM rootfs are the same thing here — a volume — and the
//! difference lives in the profile. If a name in this module says "image",
//! "container" or "pod", it is in the wrong layer.
//!
//! See `docs/layering.md`.

pub mod netstat;
pub mod tarfs;
pub mod trim;
