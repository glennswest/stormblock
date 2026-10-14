//! A drive that appears is reported and offered, never formatted on its own
//! (#154, the owner's decision 3 of `docs/multi-drive.md` §4: "on auto
//! format, no, since we may be doing a test, and a power off could destroy
//! a drive").
//!
//! When a drive is opened on a running node, what it holds is read (its
//! first and last MiB, and whether a slab of ours opens on it):
//! - **ours:** it carries our slab format, so it is adopted at once with
//!   what is on it. That destroys nothing.
//! - **blank:** nothing there; offered for a person to format.
//! - **foreign:** someone's data (a partition table, a filesystem, a RAID
//!   member): offered, with what it holds named.
//!
//! Only a person formats an offered drive, one at a time, naming the drive
//! (`POST /api/v1/drives/{id}/format` with `confirm` = its serial), and the
//! refusal without it names the risk.

use std::sync::Arc;

use serde::Serialize;

use crate::drive::BlockDevice;

const MIB: u64 = 1 << 20;

/// What an opened drive holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Offer {
    /// Our slab format: adopted, with `slabs` slab(s) on it.
    Ours { slabs: usize, volumes: usize },
    /// Nothing there: may be formatted, by a person.
    Blank,
    /// Someone's data: formatting destroys `holds`.
    Foreign { holds: String },
}

impl Offer {
    /// What formatting it would destroy, said to the person asked to confirm.
    pub fn destroys(&self) -> String {
        match self {
            Offer::Ours { slabs, volumes } => format!("{slabs} slab(s) of ours with {volumes} volume(s)"),
            Offer::Blank => "nothing found (its first and last MiB are zeros)".into(),
            Offer::Foreign { holds } => holds.clone(),
        }
    }
}

/// What a drive's first and last MiB say it holds, when it carries no slab
/// of ours.
pub async fn survey(dev: &Arc<dyn BlockDevice>) -> Offer {
    let cap = dev.capacity_bytes();
    let head_len = cap.min(MIB) as usize;
    let mut head = vec![0u8; head_len];
    let mut tail = vec![0u8; head_len];
    let read_head = dev.read(0, &mut head).await;
    let read_tail = dev.read(cap - head_len as u64, &mut tail).await;
    if read_head.is_err() || read_tail.is_err() {
        return Offer::Foreign { holds: "content that could not be read (treated as someone's)".into() };
    }
    if head.iter().all(|b| *b == 0) && tail.iter().all(|b| *b == 0) {
        return Offer::Blank;
    }
    Offer::Foreign { holds: describe(&head) }
}

/// The signatures the first MiB carries, named.
fn describe(head: &[u8]) -> String {
    let at = |off: usize, magic: &[u8]| head.len() >= off + magic.len() && &head[off..off + magic.len()] == magic;
    let mut found = Vec::new();
    if at(512, b"EFI PART") || at(4096, b"EFI PART") {
        found.push("a GPT partition table".to_string());
    } else if head.len() >= 512 && head[510] == 0x55 && head[511] == 0xAA {
        found.push("an MBR (a partition table or a boot sector)".to_string());
    }
    if at(0, b"STORMPAL") {
        found.push("a stormblock pallet".to_string());
    }
    if head.windows(8).any(|w| w == b"STORMRD1") {
        found.push("a stormraid member".to_string());
    }
    if at(1024 + 56, &[0x53, 0xEF]) {
        found.push("an ext2/3/4 filesystem".to_string());
    }
    if at(0, b"XFSB") {
        found.push("an XFS filesystem".to_string());
    }
    if at(512, b"LABELONE") {
        found.push("an LVM physical volume".to_string());
    }
    if at(0x8001, b"CD001") {
        found.push("an ISO 9660 image".to_string());
    }
    if found.is_empty() {
        "data that is not ours (no signature this engine knows)".into()
    } else {
        found.join(", ")
    }
}

/// What the person asked to confirm is told: the drive, what formatting it
/// destroys, and the risk of a power cut.
pub fn risk(path: &str, model: &str, serial: &str, capacity: u64, offer: &Offer) -> String {
    format!(
        "formatting {path} ({}{}, {} bytes) as a slab erases what it holds: {}. A power cut \
         while it is formatted leaves the drive unusable until it is formatted again. Nothing \
         formats a drive on its own (#154): to go ahead, send \"confirm\": \"{}\"",
        if model.is_empty() { "no model" } else { model },
        if serial.is_empty() { String::new() } else { format!(", serial {serial}") },
        capacity,
        offer.destroys(),
        confirmation_of(path, serial),
    )
}

/// What `confirm` must say: the drive's serial, or its path when it has none.
pub fn confirmation_of(path: &str, serial: &str) -> String {
    if serial.trim().is_empty() {
        path.to_string()
    } else {
        serial.trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_are_named() {
        let mut h = vec![0u8; MIB as usize];
        assert_eq!(describe(&h), "data that is not ours (no signature this engine knows)");
        h[512..520].copy_from_slice(b"EFI PART");
        h[1024 + 56] = 0x53;
        h[1024 + 57] = 0xEF;
        assert_eq!(describe(&h), "a GPT partition table, an ext2/3/4 filesystem");
        let mut r = vec![0u8; MIB as usize];
        r[8192..8200].copy_from_slice(b"STORMRD1");
        assert_eq!(describe(&r), "a stormraid member");
    }

    #[test]
    fn the_risk_names_the_drive_what_it_destroys_and_the_word_to_send() {
        let r = risk("/dev/sdq", "ST2000DM008", "ZFL1ABC", 2_000_398_934_016, &Offer::Foreign { holds: "an XFS filesystem".into() });
        assert!(r.contains("/dev/sdq (ST2000DM008, serial ZFL1ABC"), "{r}");
        assert!(r.contains("an XFS filesystem"), "{r}");
        assert!(r.contains("power cut"), "{r}");
        assert!(r.contains("\"confirm\": \"ZFL1ABC\""), "{r}");
        assert_eq!(confirmation_of("/tmp/d.img", ""), "/tmp/d.img");
    }
}
