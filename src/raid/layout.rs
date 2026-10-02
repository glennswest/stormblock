//! Where a byte of an array lives on its members.
//!
//! Every member keeps its first `DATA_OFFSET` bytes for the superblock and the
//! write-intent bitmap; *member offsets* below are relative to that, so member
//! offset 0 is device byte `DATA_OFFSET`.
//!
//! - **RAID-1**: every member holds the whole array; logical offset = member
//!   offset. (Unchanged from the first on-disk format, which stormstorage's
//!   mirrored legs carry.)
//! - **RAID-5 / RAID-6**: stripe `s` is member offset `s * unit` on every
//!   member. Parity rotates left-symmetric: P is on member `n-1 - s mod n`,
//!   Q (RAID-6) on the member after it, and data strip `d` on the `d`-th
//!   member after the parity — so consecutive data units walk round the
//!   members and a sequential read touches each of them in turn.
//! - **RAID-10** (near-2): members `2k` and `2k+1` mirror each other; units
//!   are striped across the pairs, unit `u` on pair `u mod pairs` at member
//!   offset `(u / pairs) * unit`.

use super::RaidLevel;

/// The geometry of one array.
#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    pub level: RaidLevel,
    /// Member slots.
    pub n: usize,
    /// Strip / stripe-unit size in bytes.
    pub unit: u64,
}

/// One piece of a logical range that lies inside a single strip unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// The stripe (parity levels), or the row of units (RAID-10).
    pub stripe: u64,
    /// The data strip within the stripe (parity), or the pair (RAID-10).
    pub index: usize,
    /// Offset inside the unit.
    pub offset_in_unit: u64,
    pub len: u64,
    /// Where in the caller's buffer this piece starts.
    pub buf_offset: usize,
}

impl Geometry {
    /// Data strips per stripe (parity levels), pairs (RAID-10), 1 (RAID-1).
    pub fn data_count(&self) -> usize {
        match self.level {
            RaidLevel::Raid1 => 1,
            RaidLevel::Raid5 => self.n - 1,
            RaidLevel::Raid6 => self.n - 2,
            RaidLevel::Raid10 => self.n / 2,
        }
    }

    /// How many members may be lost with the data intact — at worst. A
    /// RAID-10 survives more when they are in different pairs.
    pub fn tolerated(&self) -> usize {
        match self.level {
            RaidLevel::Raid1 => self.n.saturating_sub(1),
            RaidLevel::Raid5 => 1,
            RaidLevel::Raid6 => 2,
            RaidLevel::Raid10 => 1,
        }
    }

    pub fn is_parity(&self) -> bool {
        matches!(self.level, RaidLevel::Raid5 | RaidLevel::Raid6)
    }

    /// The member holding P for a stripe.
    pub fn p_member(&self, stripe: u64) -> usize {
        let n = self.n as u64;
        ((n - 1) - (stripe % n)) as usize
    }

    /// The member holding Q (RAID-6 only).
    pub fn q_member(&self, stripe: u64) -> usize {
        (self.p_member(stripe) + 1) % self.n
    }

    /// The member holding data strip `d` of a stripe.
    pub fn data_member(&self, stripe: u64, d: usize) -> usize {
        let skip = if self.level == RaidLevel::Raid6 { 2 } else { 1 };
        (self.p_member(stripe) + skip + d) % self.n
    }

    /// What a member holds in a stripe: `Some(d)` for data strip d, `None`
    /// for P or Q.
    pub fn role_of(&self, stripe: u64, member: usize) -> StripRole {
        let p = self.p_member(stripe);
        if member == p {
            return StripRole::P;
        }
        if self.level == RaidLevel::Raid6 && member == self.q_member(stripe) {
            return StripRole::Q;
        }
        let skip = if self.level == RaidLevel::Raid6 { 2 } else { 1 };
        StripRole::Data((member + self.n - p - skip) % self.n)
    }

    /// The two members of a RAID-10 pair.
    pub fn pair_members(&self, pair: usize) -> [usize; 2] {
        [2 * pair, 2 * pair + 1]
    }

    /// The pair a RAID-10 member is in.
    pub fn pair_of(&self, member: usize) -> usize {
        member / 2
    }

    /// Logical bytes in one stripe (parity) or one row of units (RAID-10).
    pub fn stripe_bytes(&self) -> u64 {
        self.unit * self.data_count() as u64
    }

    /// Usable bytes for a given per-member data size.
    pub fn capacity(&self, data_size: u64) -> u64 {
        match self.level {
            RaidLevel::Raid1 => data_size,
            _ => (data_size / self.unit) * self.stripe_bytes(),
        }
    }

    /// Split a logical range into pieces that each lie in one unit.
    /// Parity levels and RAID-10 only.
    pub fn segments(&self, offset: u64, len: u64) -> Vec<Segment> {
        let sb = self.stripe_bytes();
        let mut out = Vec::new();
        let mut pos = offset;
        let end = offset + len;
        while pos < end {
            let stripe = pos / sb;
            let within = pos % sb;
            let index = (within / self.unit) as usize;
            let offset_in_unit = within % self.unit;
            let take = (self.unit - offset_in_unit).min(end - pos);
            out.push(Segment {
                stripe,
                index,
                offset_in_unit,
                len: take,
                buf_offset: (pos - offset) as usize,
            });
            pos += take;
        }
        out
    }
}

/// What one member's strip is in a given stripe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripRole {
    Data(usize),
    P,
    Q,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raid5_every_member_is_used_once_per_stripe() {
        let g = Geometry { level: RaidLevel::Raid5, n: 5, unit: 4096 };
        for s in 0..20 {
            let mut seen = vec![false; 5];
            seen[g.p_member(s)] = true;
            for d in 0..g.data_count() {
                let m = g.data_member(s, d);
                assert!(!seen[m], "stripe {s}: member {m} twice");
                seen[m] = true;
                assert_eq!(g.role_of(s, m), StripRole::Data(d));
            }
            assert!(seen.iter().all(|x| *x));
            assert_eq!(g.role_of(s, g.p_member(s)), StripRole::P);
        }
        // Left-symmetric: stripe 0 has P on the last member, data from 0.
        assert_eq!(g.p_member(0), 4);
        assert_eq!(g.data_member(0, 0), 0);
        assert_eq!(g.p_member(1), 3);
        assert_eq!(g.data_member(1, 0), 4);
    }

    #[test]
    fn raid6_p_q_and_data_are_distinct() {
        let g = Geometry { level: RaidLevel::Raid6, n: 11, unit: 65536 };
        assert_eq!(g.data_count(), 9);
        for s in 0..40 {
            let mut seen = vec![false; 11];
            for m in [g.p_member(s), g.q_member(s)] {
                assert!(!seen[m]);
                seen[m] = true;
            }
            assert_eq!(g.role_of(s, g.q_member(s)), StripRole::Q);
            for d in 0..9 {
                let m = g.data_member(s, d);
                assert!(!seen[m]);
                seen[m] = true;
                assert_eq!(g.role_of(s, m), StripRole::Data(d));
            }
        }
    }

    #[test]
    fn segments_cover_the_range_exactly() {
        let g = Geometry { level: RaidLevel::Raid6, n: 6, unit: 4096 };
        let segs = g.segments(1000, 4 * 4096 * 3 + 77);
        let total: u64 = segs.iter().map(|s| s.len).sum();
        assert_eq!(total, 4 * 4096 * 3 + 77);
        assert_eq!(segs[0], Segment { stripe: 0, index: 0, offset_in_unit: 1000, len: 3096, buf_offset: 0 });
        assert_eq!(segs[1].index, 1);
        assert_eq!(segs[4].stripe, 1);
        assert_eq!(segs[4].index, 0);
        let mut at = 0usize;
        for s in &segs {
            assert_eq!(s.buf_offset, at);
            at += s.len as usize;
        }
    }

    #[test]
    fn capacity_by_level() {
        let unit = 65536;
        let data = 100 * unit + 123;
        assert_eq!(Geometry { level: RaidLevel::Raid1, n: 2, unit }.capacity(data), data);
        assert_eq!(Geometry { level: RaidLevel::Raid5, n: 4, unit }.capacity(data), 300 * unit);
        assert_eq!(Geometry { level: RaidLevel::Raid6, n: 11, unit }.capacity(data), 900 * unit);
        assert_eq!(Geometry { level: RaidLevel::Raid10, n: 6, unit }.capacity(data), 300 * unit);
    }
}
