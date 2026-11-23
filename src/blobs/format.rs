//! The blob store's on-disk format (#732): the superblock and the journal.
//!
//! Everything lives in one thin volume, addressed in its slots (`S` bytes)
//! and 4 KiB blocks:
//!
//! - **slot 0**: two superblocks, block 0 and block 1. Each write takes the
//!   copy the next generation names (`generation % 2`), so a torn write
//!   leaves the other intact; open reads both and takes the valid one with
//!   the higher generation.
//! - **the journal**: a chain of *segments*, each `segment_slots` contiguous
//!   slots. The superblock names the first segment of the current epoch; a
//!   segment ends in a `Next` record naming the one after it. What is written
//!   is *batches*: whole 4 KiB blocks, a 64-byte header (magic, the store,
//!   the epoch, a sequence number, the payload's length and crc32c, the
//!   header's own crc32c) and records. A batch is written once and never
//!   rewritten, so a torn write can only lose the batch being written, which
//!   was not yet acknowledged. Replay stops at the first block that is not
//!   the next batch of this epoch: zeros, a torn batch, or one from an epoch
//!   before (a slot reused by the journal reads as whatever it held).
//! - a **checkpoint** writes the live set as a fresh chain under the next
//!   epoch, then the superblock naming it; the old chain is then free.

use uuid::Uuid;

/// Every journal and superblock unit, whatever the volume's LBA.
pub const BLOCK: u64 = 4096;

const SB_MAGIC: &[u8; 8] = b"SBBLOBS1";
const SB_VERSION: u32 = 1;
const BATCH_MAGIC: &[u8; 4] = b"SBJB";
/// The batch header's size; the payload follows it.
pub const BATCH_HEADER: usize = 64;

pub type Digest = [u8; 32];

/// `sha256:<hex>` for a digest.
pub fn digest_string(d: &Digest) -> String {
    format!("sha256:{}", hex::encode(d))
}

/// A digest as a caller spells it: `sha256:<64 hex>` (or the bare hex).
/// Upper-case hex is refused rather than folded: OCI digests are lower case,
/// and two spellings of one key would be two keys to everything above.
pub fn parse_digest(s: &str) -> Result<Digest, String> {
    let hexpart = s.strip_prefix("sha256:").unwrap_or(s);
    if hexpart.len() != 64 || !hexpart.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err(format!("{s:?} is not a digest: sha256:<64 lower-case hex digits>"));
    }
    let mut d = [0u8; 32];
    hex::decode_to_slice(hexpart, &mut d).map_err(|e| format!("{s:?}: {e}"))?;
    Ok(d)
}

// ---------------------------------------------------------------------------
// Superblock
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Superblock {
    pub generation: u64,
    pub epoch: u64,
    /// The first slot of the current epoch's first journal segment.
    pub head: u64,
    pub slot_size: u64,
    pub segment_slots: u64,
    pub store: Uuid,
}

impl Superblock {
    /// Which of the two copies this generation is written to.
    pub fn copy(&self) -> u64 {
        self.generation % 2
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; BLOCK as usize];
        b[0..8].copy_from_slice(SB_MAGIC);
        b[8..12].copy_from_slice(&SB_VERSION.to_le_bytes());
        b[16..24].copy_from_slice(&self.generation.to_le_bytes());
        b[24..32].copy_from_slice(&self.epoch.to_le_bytes());
        b[32..40].copy_from_slice(&self.head.to_le_bytes());
        b[40..48].copy_from_slice(&self.slot_size.to_le_bytes());
        b[48..64].copy_from_slice(self.store.as_bytes());
        b[64..72].copy_from_slice(&self.segment_slots.to_le_bytes());
        let crc = crc32c::crc32c(&b[0..72]);
        b[72..76].copy_from_slice(&crc.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<Superblock> {
        if b.len() < 76 || &b[0..8] != SB_MAGIC {
            return None;
        }
        let crc = u32::from_le_bytes(b[72..76].try_into().ok()?);
        if crc32c::crc32c(&b[0..72]) != crc {
            return None;
        }
        if u32::from_le_bytes(b[8..12].try_into().ok()?) != SB_VERSION {
            return None;
        }
        let u = |r: std::ops::Range<usize>| u64::from_le_bytes(b[r].try_into().unwrap());
        Some(Superblock {
            generation: u(16..24),
            epoch: u(24..32),
            head: u(32..40),
            slot_size: u(40..48),
            store: Uuid::from_bytes(b[48..64].try_into().ok()?),
            segment_slots: u(64..72),
        })
    }

    /// Whether a block looks like a superblock of this format at all, valid
    /// or not: what tells a damaged store from a volume that never was one.
    pub fn has_magic(b: &[u8]) -> bool {
        b.len() >= 8 && &b[0..8] == SB_MAGIC
    }

    /// The newer valid copy of the two.
    pub fn pick(a: Option<Superblock>, b: Option<Superblock>) -> Option<Superblock> {
        match (a, b) {
            (Some(a), Some(b)) => Some(if b.generation > a.generation { b } else { a }),
            (a, b) => a.or(b),
        }
    }
}

// ---------------------------------------------------------------------------
// Journal records
// ---------------------------------------------------------------------------

/// Where a blob's bytes are: `slots`, whole, in order, then `tail` (fewer
/// than a slot's bytes) at a byte offset inside a pack slot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Layout {
    pub size: u64,
    pub slots: Vec<u64>,
    pub tail: Option<(u64, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// A blob is held (or its layout replaced, a repack).
    Put { digest: Digest, layout: Layout },
    Delete { digest: Digest },
    SessionOpen { id: Uuid, at: u64 },
    /// A session grew to `len`, `slots` added to its own.
    SessionAppend { id: Uuid, len: u64, at: u64, slots: Vec<u64> },
    /// Committed or aborted: the session is gone.
    SessionEnd { id: Uuid },
    /// The journal continues at this slot.
    Next { slot: u64 },
}

const T_PUT: u8 = 1;
const T_DELETE: u8 = 2;
const T_SOPEN: u8 = 3;
const T_SAPPEND: u8 = 4;
const T_SEND: u8 = 5;
const T_NEXT: u8 = 6;

/// Slots as runs of consecutive slots: a streamed blob's are mostly one.
fn runs(slots: &[u64]) -> Vec<(u64, u32)> {
    let mut out: Vec<(u64, u32)> = Vec::new();
    for &s in slots {
        match out.last_mut() {
            Some((start, n)) if *start + *n as u64 == s && *n < u32::MAX => *n += 1,
            _ => out.push((s, 1)),
        }
    }
    out
}

fn put_runs(out: &mut Vec<u8>, slots: &[u64]) {
    let r = runs(slots);
    out.extend_from_slice(&(r.len() as u32).to_le_bytes());
    for (s, n) in r {
        out.extend_from_slice(&s.to_le_bytes());
        out.extend_from_slice(&n.to_le_bytes());
    }
}

impl Record {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Record::Put { digest, layout } => {
                out.push(T_PUT);
                out.extend_from_slice(digest);
                out.extend_from_slice(&layout.size.to_le_bytes());
                put_runs(out, &layout.slots);
                match layout.tail {
                    Some((off, len)) => {
                        out.push(1);
                        out.extend_from_slice(&off.to_le_bytes());
                        out.extend_from_slice(&len.to_le_bytes());
                    }
                    None => out.push(0),
                }
            }
            Record::Delete { digest } => {
                out.push(T_DELETE);
                out.extend_from_slice(digest);
            }
            Record::SessionOpen { id, at } => {
                out.push(T_SOPEN);
                out.extend_from_slice(id.as_bytes());
                out.extend_from_slice(&at.to_le_bytes());
            }
            Record::SessionAppend { id, len, at, slots } => {
                out.push(T_SAPPEND);
                out.extend_from_slice(id.as_bytes());
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(&at.to_le_bytes());
                put_runs(out, slots);
            }
            Record::SessionEnd { id } => {
                out.push(T_SEND);
                out.extend_from_slice(id.as_bytes());
            }
            Record::Next { slot } => {
                out.push(T_NEXT);
                out.extend_from_slice(&slot.to_le_bytes());
            }
        }
    }

    /// Every record in a payload, or `None` if any is malformed (a payload
    /// passed its crc, so that is a writer's bug, and nothing of it is
    /// applied).
    pub fn decode_all(mut b: &[u8]) -> Option<Vec<Record>> {
        let mut out = Vec::new();
        while !b.is_empty() {
            let mut r = Reader(b);
            let rec = match r.u8()? {
                T_PUT => {
                    let digest = r.digest()?;
                    let size = r.u64()?;
                    let slots = r.runs()?;
                    let tail = match r.u8()? {
                        0 => None,
                        1 => Some((r.u64()?, r.u32()?)),
                        _ => return None,
                    };
                    Record::Put { digest, layout: Layout { size, slots, tail } }
                }
                T_DELETE => Record::Delete { digest: r.digest()? },
                T_SOPEN => Record::SessionOpen { id: r.uuid()?, at: r.u64()? },
                T_SAPPEND => {
                    let id = r.uuid()?;
                    let len = r.u64()?;
                    let at = r.u64()?;
                    Record::SessionAppend { id, len, at, slots: r.runs()? }
                }
                T_SEND => Record::SessionEnd { id: r.uuid()? },
                T_NEXT => Record::Next { slot: r.u64()? },
                _ => return None,
            };
            out.push(rec);
            b = r.0;
        }
        Some(out)
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn digest(&mut self) -> Option<Digest> {
        self.take(32)?.try_into().ok()
    }
    fn uuid(&mut self) -> Option<Uuid> {
        Some(Uuid::from_bytes(self.take(16)?.try_into().ok()?))
    }
    fn runs(&mut self) -> Option<Vec<u64>> {
        let n = self.u32()? as usize;
        let mut slots = Vec::new();
        for _ in 0..n {
            let s = self.u64()?;
            let c = self.u32()? as u64;
            // A run longer than any volume is a malformed record, not a
            // reason to allocate without bound.
            if c > (1 << 32) || slots.len() as u64 + c > (1 << 32) {
                return None;
            }
            slots.extend(s..s + c);
        }
        Some(slots)
    }
}

// ---------------------------------------------------------------------------
// Batches
// ---------------------------------------------------------------------------

/// A batch: its records, as whole blocks with the header in front.
pub fn encode_batch(store: &Uuid, epoch: u64, seq: u64, records: &[Record]) -> Vec<u8> {
    let mut payload = Vec::new();
    for r in records {
        r.encode(&mut payload);
    }
    let total = BATCH_HEADER + payload.len();
    let nblocks = total.div_ceil(BLOCK as usize).max(1);
    let mut b = vec![0u8; nblocks * BLOCK as usize];
    b[0..4].copy_from_slice(BATCH_MAGIC);
    b[4..8].copy_from_slice(&(nblocks as u32).to_le_bytes());
    b[8..16].copy_from_slice(&epoch.to_le_bytes());
    b[16..24].copy_from_slice(&seq.to_le_bytes());
    b[24..28].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    b[28..32].copy_from_slice(&crc32c::crc32c(&payload).to_le_bytes());
    b[32..48].copy_from_slice(store.as_bytes());
    let hcrc = crc32c::crc32c(&b[0..48]);
    b[48..52].copy_from_slice(&hcrc.to_le_bytes());
    b[BATCH_HEADER..BATCH_HEADER + payload.len()].copy_from_slice(&payload);
    b
}

/// What a batch's first block says, if it is the batch `seq` of `epoch` of
/// this store: how many blocks it takes and the payload's length and crc.
pub struct BatchHeader {
    pub nblocks: u32,
    pub payload_len: u32,
    pub payload_crc: u32,
}

pub fn decode_batch_header(b: &[u8], store: &Uuid, epoch: u64, seq: u64) -> Option<BatchHeader> {
    if b.len() < BATCH_HEADER || &b[0..4] != BATCH_MAGIC {
        return None;
    }
    if crc32c::crc32c(&b[0..48]) != u32::from_le_bytes(b[48..52].try_into().ok()?) {
        return None;
    }
    if &b[32..48] != store.as_bytes()
        || u64::from_le_bytes(b[8..16].try_into().ok()?) != epoch
        || u64::from_le_bytes(b[16..24].try_into().ok()?) != seq
    {
        return None;
    }
    let h = BatchHeader {
        nblocks: u32::from_le_bytes(b[4..8].try_into().ok()?),
        payload_len: u32::from_le_bytes(b[24..28].try_into().ok()?),
        payload_crc: u32::from_le_bytes(b[28..32].try_into().ok()?),
    };
    if h.nblocks == 0 || BATCH_HEADER as u64 + h.payload_len as u64 > h.nblocks as u64 * BLOCK {
        return None;
    }
    Some(h)
}

/// The records of a whole batch whose header passed, if its payload does.
pub fn decode_batch_payload(b: &[u8], h: &BatchHeader) -> Option<Vec<Record>> {
    let end = BATCH_HEADER + h.payload_len as usize;
    if b.len() < end {
        return None;
    }
    let payload = &b[BATCH_HEADER..end];
    if crc32c::crc32c(payload) != h.payload_crc {
        return None;
    }
    Record::decode_all(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_through_a_batch() {
        let store = Uuid::new_v4();
        let recs = vec![
            Record::Put { digest: [7; 32], layout: Layout { size: 9 << 20, slots: vec![4, 5, 6, 9], tail: Some((40960, 1234)) } },
            Record::Put { digest: [8; 32], layout: Layout { size: 0, slots: vec![], tail: None } },
            Record::Delete { digest: [7; 32] },
            Record::SessionOpen { id: Uuid::new_v4(), at: 17 },
            Record::SessionAppend { id: Uuid::nil(), len: 99, at: 18, slots: vec![1, 2, 3] },
            Record::SessionEnd { id: Uuid::nil() },
            Record::Next { slot: 77 },
        ];
        let b = encode_batch(&store, 3, 11, &recs);
        assert_eq!(b.len() % BLOCK as usize, 0);
        let h = decode_batch_header(&b, &store, 3, 11).unwrap();
        assert_eq!(decode_batch_payload(&b, &h).unwrap(), recs);
        // Another epoch, sequence or store is not this batch.
        assert!(decode_batch_header(&b, &store, 2, 11).is_none());
        assert!(decode_batch_header(&b, &store, 3, 12).is_none());
        assert!(decode_batch_header(&b, &Uuid::new_v4(), 3, 11).is_none());
        // A flipped payload bit fails the payload's crc.
        let mut bad = b.clone();
        bad[BATCH_HEADER + 3] ^= 1;
        let h = decode_batch_header(&bad, &store, 3, 11).unwrap();
        assert!(decode_batch_payload(&bad, &h).is_none());
    }

    #[test]
    fn the_newer_valid_superblock_wins() {
        let s = Superblock { generation: 4, epoch: 2, head: 9, slot_size: 1 << 20, segment_slots: 4, store: Uuid::new_v4() };
        let t = Superblock { generation: 5, ..s };
        assert_eq!(Superblock::decode(&s.encode()), Some(s));
        assert_eq!(Superblock::pick(Some(s), Some(t)), Some(t));
        let mut torn = t.encode();
        torn[30] ^= 0xff;
        assert_eq!(Superblock::pick(Superblock::decode(&s.encode()), Superblock::decode(&torn)), Some(s));
        assert!(Superblock::has_magic(&torn));
    }

    #[test]
    fn digests_are_spelt_one_way() {
        let h = "a".repeat(64);
        assert!(parse_digest(&format!("sha256:{h}")).is_ok());
        assert!(parse_digest(&h).is_ok());
        assert!(parse_digest(&format!("sha256:{}", "A".repeat(64))).is_err());
        assert!(parse_digest("sha256:abc").is_err());
        assert!(parse_digest(&format!("sha512:{h}")).is_err());
        assert_eq!(digest_string(&parse_digest(&h).unwrap()), format!("sha256:{h}"));
    }
}
