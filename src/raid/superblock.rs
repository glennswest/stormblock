//! The on-disk superblock: the first 4 KiB of every member and every spare.
//!
//! Each member carries the whole array's description — level, geometry, the
//! slot table with every slot's member uuid and state — so any surviving
//! member says what the array is, and `events` (bumped on every change of
//! state) says which copy is newest. That is what reassembly reads (#168).
//!
//! Layout (little-endian):
//!
//! | bytes | field |
//! |---|---|
//! | 0..8 | magic `STRMBLK\0` |
//! | 8..12 | version (2) |
//! | 12..28 | array uuid (all zero on a spare) |
//! | 28..44 | this member's uuid |
//! | 44..48 | this member's slot (`u32::MAX` on a spare) |
//! | 48 | level (1, 5, 6, 10; 0 = spare) |
//! | 52..56 | slot count |
//! | 56..64 | stripe unit |
//! | 64..72 | data offset (1 MiB) |
//! | 72..80 | data bytes per member |
//! | 80..88 | created (unix s) |
//! | 88..96 | updated (unix s) |
//! | 96..104 | events |
//! | 104..112 | bitmap offset on the member |
//! | 112..120 | bitmap bytes |
//! | 120..128 | bytes of member data per bitmap bit |
//! | 128..192 | set name, UTF-8, NUL-padded |
//! | 192..256 | spare pool, UTF-8, NUL-padded (empty = global) |
//! | 256.. | slot table, 32 bytes a slot: member uuid, state, 7 pad, rebuilt-to |
//! | 4092..4096 | CRC32C of bytes 0..4092 |
//!
//! Version 1 (the first format, no slot table, never reassembled) is not
//! read: a drive carrying one is treated as carrying no array.

use uuid::Uuid;

use super::{RaidError, RaidLevel, RaidMemberState};

pub const SUPERBLOCK_MAGIC: [u8; 8] = *b"STRMBLK\0";
pub const SUPERBLOCK_VERSION: u32 = 2;
pub const SUPERBLOCK_BYTES: usize = 4096;
/// Slots one superblock describes.
pub const MAX_SLOTS: usize = 64;
const SLOT_TABLE: usize = 256;
const SLOT_BYTES: usize = 32;
const CRC_AT: usize = SUPERBLOCK_BYTES - 4;
const NAME_LEN: usize = 64;

/// One slot of the array as the superblock records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotRecord {
    pub member_uuid: Uuid,
    pub state: RaidMemberState,
    /// For a `Rebuilding` slot: member data bytes already rebuilt.
    pub rebuilt_to: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    pub array_uuid: Uuid,
    pub member_uuid: Uuid,
    /// `None` on a spare.
    pub slot: Option<u32>,
    /// `None` on a spare.
    pub level: Option<RaidLevel>,
    pub stripe_size: u64,
    pub data_offset: u64,
    pub data_size: u64,
    pub create_time: u64,
    pub update_time: u64,
    pub events: u64,
    pub bitmap_offset: u64,
    pub bitmap_bytes: u64,
    pub bitmap_chunk: u64,
    pub name: String,
    /// The spare pool: on a spare, the pool it is in; on a member, the pool
    /// the array takes spares from. Empty = the global pool.
    pub pool: String,
    pub slots: Vec<SlotRecord>,
}

fn level_byte(l: Option<RaidLevel>) -> u8 {
    match l {
        None => 0,
        Some(RaidLevel::Raid1) => 1,
        Some(RaidLevel::Raid5) => 5,
        Some(RaidLevel::Raid6) => 6,
        Some(RaidLevel::Raid10) => 10,
    }
}

fn state_byte(s: RaidMemberState) -> u8 {
    match s {
        RaidMemberState::Active => 1,
        RaidMemberState::Degraded => 2,
        RaidMemberState::Spare => 3,
        RaidMemberState::Failed => 4,
        RaidMemberState::Rebuilding => 5,
    }
}

fn state_of(b: u8) -> Result<RaidMemberState, RaidError> {
    Ok(match b {
        1 => RaidMemberState::Active,
        2 => RaidMemberState::Degraded,
        3 => RaidMemberState::Spare,
        4 => RaidMemberState::Failed,
        5 => RaidMemberState::Rebuilding,
        x => return Err(RaidError::SuperblockMismatch(format!("slot state {x}"))),
    })
}

fn put_str(buf: &mut [u8], s: &str) {
    let b = s.as_bytes();
    let n = b.len().min(buf.len());
    buf[..n].copy_from_slice(&b[..n]);
}

fn get_str(buf: &[u8]) -> String {
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

impl Superblock {
    pub fn is_spare(&self) -> bool {
        self.level.is_none()
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        assert!(self.slots.len() <= MAX_SLOTS, "more slots than a superblock holds");
        let mut b = vec![0u8; SUPERBLOCK_BYTES];
        b[0..8].copy_from_slice(&SUPERBLOCK_MAGIC);
        b[8..12].copy_from_slice(&SUPERBLOCK_VERSION.to_le_bytes());
        b[12..28].copy_from_slice(self.array_uuid.as_bytes());
        b[28..44].copy_from_slice(self.member_uuid.as_bytes());
        b[44..48].copy_from_slice(&self.slot.unwrap_or(u32::MAX).to_le_bytes());
        b[48] = level_byte(self.level);
        b[52..56].copy_from_slice(&(self.slots.len() as u32).to_le_bytes());
        b[56..64].copy_from_slice(&self.stripe_size.to_le_bytes());
        b[64..72].copy_from_slice(&self.data_offset.to_le_bytes());
        b[72..80].copy_from_slice(&self.data_size.to_le_bytes());
        b[80..88].copy_from_slice(&self.create_time.to_le_bytes());
        b[88..96].copy_from_slice(&self.update_time.to_le_bytes());
        b[96..104].copy_from_slice(&self.events.to_le_bytes());
        b[104..112].copy_from_slice(&self.bitmap_offset.to_le_bytes());
        b[112..120].copy_from_slice(&self.bitmap_bytes.to_le_bytes());
        b[120..128].copy_from_slice(&self.bitmap_chunk.to_le_bytes());
        put_str(&mut b[128..128 + NAME_LEN], &self.name);
        put_str(&mut b[192..192 + NAME_LEN], &self.pool);
        for (i, s) in self.slots.iter().enumerate() {
            let at = SLOT_TABLE + i * SLOT_BYTES;
            b[at..at + 16].copy_from_slice(s.member_uuid.as_bytes());
            b[at + 16] = state_byte(s.state);
            b[at + 24..at + 32].copy_from_slice(&s.rebuilt_to.to_le_bytes());
        }
        let crc = crc32c::crc32c(&b[..CRC_AT]);
        b[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// `Ok(None)` when there is no superblock of this format here at all —
    /// a blank drive, a drive holding something else, or a version-1
    /// superblock. `Err` when there is one and it is damaged.
    pub fn from_bytes(b: &[u8]) -> Result<Option<Self>, RaidError> {
        if b.len() < SUPERBLOCK_BYTES || b[0..8] != SUPERBLOCK_MAGIC {
            return Ok(None);
        }
        if u32_at(b, 8) != SUPERBLOCK_VERSION {
            return Ok(None);
        }
        if crc32c::crc32c(&b[..CRC_AT]) != u32_at(b, CRC_AT) {
            return Err(RaidError::ChecksumError);
        }
        let level = match b[48] {
            0 => None,
            1 => Some(RaidLevel::Raid1),
            5 => Some(RaidLevel::Raid5),
            6 => Some(RaidLevel::Raid6),
            10 => Some(RaidLevel::Raid10),
            x => return Err(RaidError::SuperblockMismatch(format!("level {x}"))),
        };
        let count = u32_at(b, 52) as usize;
        if count > MAX_SLOTS {
            return Err(RaidError::SuperblockMismatch(format!("{count} slots")));
        }
        let mut slots = Vec::with_capacity(count);
        for i in 0..count {
            let at = SLOT_TABLE + i * SLOT_BYTES;
            slots.push(SlotRecord {
                member_uuid: Uuid::from_slice(&b[at..at + 16]).unwrap(),
                state: state_of(b[at + 16])?,
                rebuilt_to: u64_at(b, at + 24),
            });
        }
        let slot = u32_at(b, 44);
        Ok(Some(Superblock {
            array_uuid: Uuid::from_slice(&b[12..28]).unwrap(),
            member_uuid: Uuid::from_slice(&b[28..44]).unwrap(),
            slot: (slot != u32::MAX).then_some(slot),
            level,
            stripe_size: u64_at(b, 56),
            data_offset: u64_at(b, 64),
            data_size: u64_at(b, 72),
            create_time: u64_at(b, 80),
            update_time: u64_at(b, 88),
            events: u64_at(b, 96),
            bitmap_offset: u64_at(b, 104),
            bitmap_bytes: u64_at(b, 112),
            bitmap_chunk: u64_at(b, 120),
            name: get_str(&b[128..128 + NAME_LEN]),
            pool: get_str(&b[192..192 + NAME_LEN]),
            slots,
        }))
    }

    /// The superblock of a hot spare in `pool` ("" = global).
    pub fn spare(member_uuid: Uuid, pool: &str) -> Self {
        let now = super::now_secs();
        Superblock {
            array_uuid: Uuid::nil(),
            member_uuid,
            slot: None,
            level: None,
            stripe_size: 0,
            data_offset: 0,
            data_size: 0,
            create_time: now,
            update_time: now,
            events: 0,
            bitmap_offset: 0,
            bitmap_bytes: 0,
            bitmap_chunk: 0,
            name: String::new(),
            pool: pool.to_string(),
            slots: Vec::new(),
        }
    }
}

/// A name or pool is at most this many bytes on disk.
pub fn check_name(what: &str, s: &str) -> Result<(), RaidError> {
    if s.len() > NAME_LEN {
        return Err(RaidError::InvalidStripe(format!("{what} '{s}' is longer than {NAME_LEN} bytes")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Superblock {
        Superblock {
            array_uuid: Uuid::new_v4(),
            member_uuid: Uuid::new_v4(),
            slot: Some(3),
            level: Some(RaidLevel::Raid6),
            stripe_size: 65536,
            data_offset: 1 << 20,
            data_size: 1 << 40,
            create_time: 1,
            update_time: 2,
            events: 42,
            bitmap_offset: 65536,
            bitmap_bytes: 4096,
            bitmap_chunk: 64 << 20,
            name: "shelf1-a".into(),
            pool: "shelf1".into(),
            slots: (0..11)
                .map(|i| SlotRecord {
                    member_uuid: Uuid::new_v4(),
                    state: if i == 4 { RaidMemberState::Rebuilding } else { RaidMemberState::Active },
                    rebuilt_to: if i == 4 { 12345 } else { 0 },
                })
                .collect(),
        }
    }

    #[test]
    fn roundtrip() {
        let sb = sample();
        let back = Superblock::from_bytes(&sb.to_bytes()).unwrap().unwrap();
        assert_eq!(back, sb);
        let spare = Superblock::spare(Uuid::new_v4(), "shelf1");
        let back = Superblock::from_bytes(&spare.to_bytes()).unwrap().unwrap();
        assert!(back.is_spare());
        assert_eq!(back.pool, "shelf1");
    }

    #[test]
    fn damage_is_an_error_and_blank_is_nothing() {
        let mut b = sample().to_bytes();
        b[100] ^= 1;
        assert!(matches!(Superblock::from_bytes(&b), Err(RaidError::ChecksumError)));
        assert!(Superblock::from_bytes(&vec![0u8; 4096]).unwrap().is_none());
        // A version-1 superblock is not this format.
        let mut v1 = vec![0u8; 4096];
        v1[0..8].copy_from_slice(&SUPERBLOCK_MAGIC);
        v1[8..12].copy_from_slice(&1u32.to_le_bytes());
        assert!(Superblock::from_bytes(&v1).unwrap().is_none());
    }
}
