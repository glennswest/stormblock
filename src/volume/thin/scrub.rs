//! Scrub one volume (#160): read every leg of every extent and every
//! stripe's parity, and say what disagrees.
//!
//! There are no per-block checksums, so when copies disagree nothing can say
//! which is right. What repairs it, as decided on #160 (option (c) with (a)):
//! - **`mirror:3` and up:** the copy a strict majority of the legs agree on
//!   is written over the odd ones.
//! - **`mirror:2`** (or a wider mirror with no majority, or with legs failed
//!   down to two): reported and never guessed. An operator repairs it from a
//!   leg of their choosing ([`ThinVolumeHandle::repair_extent_from`]).
//! - **Parity:** reported, and rewritten from the data only when asked.
//!
//! Each extent (or stripe) is checked under its lock and its slots' fence,
//! as a write takes them, so a write in flight never reads as a mismatch.

use super::*;
use serde::Serialize;

/// The most findings one scrub keeps; the counts go on.
pub const MAX_FINDINGS: usize = 256;

/// How a scrub runs.
#[derive(Clone)]
pub struct ScrubOptions {
    /// Rewrite a stripe's parity that does not match its data.
    pub repair_parity: bool,
    /// Share of the drives' time the scrub takes: after a unit of work that
    /// took `t`, it waits `t × (100 − duty) / duty`. 100 = flat out.
    pub duty_percent: u8,
    /// Set to stop at the next extent.
    pub cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl Default for ScrubOptions {
    fn default() -> Self {
        ScrubOptions { repair_parity: false, duty_percent: 100, cancel: Default::default() }
    }
}

/// One thing a scrub found.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct ScrubFinding {
    /// `mirror`, `parity` or `unreadable`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub extent: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub stripe: Option<u64>,
    /// The legs, as `slab:slot`; for a mirror, grouped by content.
    pub legs: Vec<String>,
    /// `repaired`, `reported` or `rewritten`, and why.
    pub action: String,
}

/// What a scrub of one volume found.
#[derive(Debug, Clone, Default, PartialEq, Serialize, serde::Deserialize)]
pub struct ScrubReport {
    pub extents: u64,
    pub stripes: u64,
    pub bytes: u64,
    pub mirror_mismatches: u64,
    /// Mismatches a majority repaired (`mirror:3` and up).
    pub mirror_repaired: u64,
    /// Mismatches left for an operator: no majority to say which is right.
    pub mirror_reported: u64,
    pub parity_mismatches: u64,
    pub parity_rewritten: u64,
    /// Legs that would not read (marked failed for the volume, as a read
    /// marks them, so the rebuild queue takes them).
    pub unreadable: u64,
    pub findings: Vec<ScrubFinding>,
    pub cancelled: bool,
    pub seconds: f64,
}

impl ScrubReport {
    fn find(&mut self, f: ScrubFinding) {
        if self.findings.len() < MAX_FINDINGS {
            self.findings.push(f);
        }
    }
    pub fn clean(&self) -> bool {
        self.mirror_mismatches == 0 && self.parity_mismatches == 0 && self.unreadable == 0
    }
}

fn leg_name(l: &Leg) -> String {
    format!("{}:{}", l.slab_id.0, l.slot_idx)
}

impl ThinVolumeHandle {
    /// Scrub this volume. `paused` is asked between units; while it says so
    /// the scrub waits (an install or a flow-over is running). `progress`
    /// gets (done, total) units.
    pub async fn scrub(
        &self,
        opts: &ScrubOptions,
        paused: impl Fn() -> bool,
        progress: impl Fn(u64, u64),
    ) -> ScrubReport {
        use std::sync::atomic::Ordering;
        let started = std::time::Instant::now();
        let mut report = ScrubReport::default();
        if let Err(e) = self.resident().await {
            report.unreadable += 1;
            report.find(ScrubFinding { kind: "unreadable".into(), extent: None, stripe: None, legs: vec![], action: format!("the extent map did not load: {e}") });
            return report;
        }
        let policy = self.redundancy();
        let units: Vec<u64> = {
            let gem = self.gem.read().await;
            match policy.scheme {
                Redundancy::Parity { .. } => {
                    let mut v: Vec<u64> = gem.get_volume_map(&self.id).map(|m| m.parity.keys().copied().collect()).unwrap_or_default();
                    v.sort_unstable();
                    v
                }
                Redundancy::Mirror { .. } => {
                    let mut v: Vec<u64> = gem.volume_extents(&self.id).map(|it| it.map(|(x, _)| x).collect()).unwrap_or_default();
                    v.sort_unstable();
                    v
                }
                Redundancy::None => Vec::new(),
            }
        };
        let total = units.len() as u64;
        let duty = u64::from(opts.duty_percent.clamp(1, 100));
        for (i, unit) in units.into_iter().enumerate() {
            while paused() && !opts.cancel.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            if opts.cancel.load(Ordering::Relaxed) {
                report.cancelled = true;
                break;
            }
            let t = std::time::Instant::now();
            match policy.scheme {
                Redundancy::Parity { data, parity } => {
                    self.scrub_stripe(unit, data as usize, parity, opts.repair_parity, &mut report).await
                }
                _ => self.scrub_mirror_extent(unit, &mut report).await,
            }
            progress(i as u64 + 1, total);
            if duty < 100 {
                let wait = t.elapsed() * ((100 - duty) as u32) / duty as u32;
                tokio::time::sleep(wait.min(std::time::Duration::from_secs(10))).await;
            }
        }
        report.seconds = started.elapsed().as_secs_f64();
        report
    }

    /// Make the bytes just written to `leg` durable.
    async fn flush_leg(&self, leg: Leg) -> DriveResult<()> {
        let (dev, _) = self.leg_io(leg, 0).await?;
        dev.flush().await
    }

    async fn scrub_mirror_extent(&self, vext: u64, report: &mut ScrubReport) {
        let _e = self.shard(vext).lock().await;
        let (loc, _held) = self.fenced_lookup(vext).await;
        let Some(loc) = loc else { return };
        let legs: Vec<Leg> = loc.legs().filter(|l| !self.is_failed(l.slab_id)).collect();
        report.extents += 1;
        if legs.len() < 2 {
            return;
        }
        let mut copies: Vec<(Leg, Vec<u8>)> = Vec::with_capacity(legs.len());
        for leg in legs {
            let mut buf = vec![0u8; self.slot_size as usize];
            match self.read_leg(leg, 0, &mut buf).await {
                Ok(()) => {
                    report.bytes += self.slot_size;
                    copies.push((leg, buf));
                }
                Err(e) => {
                    report.unreadable += 1;
                    self.mark_failed(leg.slab_id, &e);
                    report.find(ScrubFinding {
                        kind: "unreadable".into(),
                        extent: Some(vext),
                        stripe: None,
                        legs: vec![leg_name(&leg)],
                        action: format!("reported: {e}"),
                    });
                }
            }
        }
        if copies.len() < 2 {
            return;
        }
        // Group the legs by their bytes.
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for i in 0..copies.len() {
            match groups.iter_mut().find(|g| copies[g[0]].1 == copies[i].1) {
                Some(g) => g.push(i),
                None => groups.push(vec![i]),
            }
        }
        if groups.len() == 1 {
            return;
        }
        report.mirror_mismatches += 1;
        let n = copies.len();
        groups.sort_by_key(|g| std::cmp::Reverse(g.len()));
        let named = |g: &Vec<usize>| g.iter().map(|&i| leg_name(&copies[i].0)).collect::<Vec<_>>().join(" ");
        let legs: Vec<String> = groups.iter().map(named).collect();
        let majority = groups[0].len() * 2 > n;
        if n >= 3 && majority {
            let good = copies[groups[0][0]].1.clone();
            let mut wrote = 0;
            let mut errors = Vec::new();
            for g in &groups[1..] {
                for &i in g {
                    let leg = copies[i].0;
                    match self.write_leg(leg, 0, &good).await {
                        Ok(()) => match self.flush_leg(leg).await {
                            Ok(()) => wrote += 1,
                            Err(e) => errors.push(format!("{}: {e}", leg_name(&leg))),
                        },
                        Err(e) => errors.push(format!("{}: {e}", leg_name(&leg))),
                    }
                }
            }
            if errors.is_empty() {
                report.mirror_repaired += 1;
            } else {
                report.mirror_reported += 1;
            }
            tracing::warn!(volume = %self.id, vext, agree = groups[0].len(), legs = n, "scrub: mirror legs disagreed; the {} that agree were written over {wrote} other(s)", groups[0].len());
            report.find(ScrubFinding {
                kind: "mirror".into(),
                extent: Some(vext),
                stripe: None,
                legs,
                action: if errors.is_empty() {
                    format!("repaired: {} of {n} legs agreed and were written over the other {wrote}", groups[0].len())
                } else {
                    format!("reported: the repair failed: {}", errors.join("; "))
                },
            });
        } else {
            report.mirror_reported += 1;
            tracing::warn!(volume = %self.id, vext, legs = n, "scrub: mirror legs disagree and no majority says which is right; reported, nothing written");
            report.find(ScrubFinding {
                kind: "mirror".into(),
                extent: Some(vext),
                stripe: None,
                legs,
                action: format!(
                    "reported: {n} legs in {} versions, no majority; nothing can say which is right \
                     (repair from a chosen leg: POST …/scrub/repair {{extent, from}})",
                    groups.len()
                ),
            });
        }
    }

    async fn scrub_stripe(&self, stripe: u64, width: usize, parity: u8, repair: bool, report: &mut ScrubReport) {
        let _s = self.shard(stripe).lock().await;
        let _f = self.fenced_stripe(stripe, width).await;
        let group = { self.gem.read().await.lookup_parity(self.id, stripe).cloned() };
        let Some(g) = group else { return };
        report.stripes += 1;
        let members = match self.assemble_stripe(stripe, width).await {
            Ok(m) => m,
            Err(e) => {
                report.unreadable += 1;
                report.find(ScrubFinding { kind: "unreadable".into(), extent: None, stripe: Some(stripe), legs: vec![], action: format!("reported: {e}") });
                return;
            }
        };
        report.bytes += self.slot_size * width as u64;
        let refs: Vec<Option<&[u8]>> = members.iter().map(|m| Some(m.as_slice())).collect();
        let want = stripe::compute_parity(&refs, self.slot_size as usize, parity);
        for (i, leg) in g.legs.iter().enumerate() {
            if self.is_failed(leg.slab_id) || i >= want.len() {
                continue;
            }
            let mut have = vec![0u8; self.slot_size as usize];
            if let Err(e) = self.read_leg(*leg, 0, &mut have).await {
                report.unreadable += 1;
                self.mark_failed(leg.slab_id, &e);
                report.find(ScrubFinding { kind: "unreadable".into(), extent: None, stripe: Some(stripe), legs: vec![leg_name(leg)], action: format!("reported: {e}") });
                continue;
            }
            report.bytes += self.slot_size;
            if have == want[i] {
                continue;
            }
            report.parity_mismatches += 1;
            let which = if i == 0 { "P" } else { "Q" };
            let action = if repair {
                match self.write_leg(*leg, 0, &want[i]).await {
                    Ok(()) => match self.flush_leg(*leg).await {
                        Ok(()) => {
                            report.parity_rewritten += 1;
                            format!("rewritten: {which} recomputed from the data (asked)")
                        }
                        Err(e) => format!("reported: the rewrite did not flush: {e}"),
                    },
                    Err(e) => format!("reported: the rewrite failed: {e}"),
                }
            } else {
                format!("reported: {which} does not match the data; rewritten only when asked (repair_parity)")
            };
            tracing::warn!(volume = %self.id, stripe, which, "scrub: parity does not match its data; {action}");
            report.find(ScrubFinding { kind: "parity".into(), extent: None, stripe: Some(stripe), legs: vec![leg_name(leg)], action });
        }
    }

    /// An operator's repair of a mirror extent (#160, (a)): `from`'s copy
    /// is written over every other leg. Answers how many legs were written.
    pub async fn repair_extent_from(&self, vext: u64, from: SlabId) -> Result<usize, String> {
        if !matches!(self.redundancy().scheme, Redundancy::Mirror { .. }) {
            return Err("only a mirrored volume's extent is repaired from a leg".into());
        }
        self.resident().await.map_err(|e| e.to_string())?;
        let _e = self.shard(vext).lock().await;
        let (loc, _held) = self.fenced_lookup(vext).await;
        let loc = loc.ok_or_else(|| format!("extent {vext} is not mapped"))?;
        let legs: Vec<Leg> = loc.legs().collect();
        let source = *legs
            .iter()
            .find(|l| l.slab_id == from)
            .ok_or_else(|| format!("extent {vext} has no leg on slab {}", from.0))?;
        let mut good = vec![0u8; self.slot_size as usize];
        self.read_leg(source, 0, &mut good).await.map_err(|e| format!("reading the chosen leg: {e}"))?;
        let mut wrote = 0;
        for leg in legs.into_iter().filter(|l| *l != source && !self.is_failed(l.slab_id)) {
            self.write_leg(leg, 0, &good).await.map_err(|e| format!("{}: {e}", leg_name(&leg)))?;
            self.flush_leg(leg).await.map_err(|e| format!("{}: {e}", leg_name(&leg)))?;
            wrote += 1;
        }
        tracing::warn!(volume = %self.id, vext, from = %from.0, wrote, "scrub repair: an operator's leg written over the others");
        Ok(wrote)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::drive::slab::Slab;
    use crate::placement::topology::StorageTier;

    type Shared<T> = Arc<crate::lockwatch::TrackedRwLock<T>>;
    const SLOT: u64 = 16 * 1024;

    async fn slabs(n: usize, dir: &tempfile::TempDir) -> (Shared<GlobalExtentMap>, Shared<SlabRegistry>) {
        let mut registry = SlabRegistry::new();
        for i in 0..n {
            let path = dir.path().join(format!("slab-{i}.bin")).to_string_lossy().to_string();
            let dev = FileDevice::open_with_capacity(&path, 8 * 1024 * 1024).await.unwrap();
            registry.add(Slab::format(Arc::new(dev), SLOT, StorageTier::Hot).await.unwrap());
        }
        (
            Arc::new(crate::lockwatch::TrackedRwLock::new(GlobalExtentMap::new())),
            Arc::new(crate::lockwatch::TrackedRwLock::new(registry)),
        )
    }

    fn volume(gem: &Shared<GlobalExtentMap>, reg: &Shared<SlabRegistry>, policy: &str) -> Arc<ThinVolumeHandle> {
        Arc::new(ThinVolumeHandle::with_redundancy(
            ThinVolume::new("v".into(), 64 * 1024 * 1024, SLOT),
            gem.clone(),
            reg.clone(),
            PlacementPolicy::default(),
            RedundancyPolicy::parse(policy).unwrap(),
        ))
    }

    fn pattern(seed: u8) -> Vec<u8> {
        (0..SLOT as usize).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    async fn leg_bytes(reg: &Shared<SlabRegistry>, leg: Leg) -> Vec<u8> {
        let mut b = vec![0u8; SLOT as usize];
        reg.read().await.get(&leg.slab_id).unwrap().read_slot(leg.slot_idx, 0, &mut b).await.unwrap();
        b
    }

    async fn spoil(reg: &Shared<SlabRegistry>, leg: Leg, seed: u8) {
        let reg = reg.read().await;
        let s = reg.get(&leg.slab_id).unwrap();
        s.write_slot(leg.slot_idx, 4096, &vec![seed; 512]).await.unwrap();
        s.device().flush().await.unwrap();
    }

    async fn legs_of(gem: &Shared<GlobalExtentMap>, v: &ThinVolumeHandle, vext: u64) -> Vec<Leg> {
        gem.read().await.lookup(v.volume_id(), vext).unwrap().legs().collect()
    }

    async fn scrub(v: &ThinVolumeHandle, repair_parity: bool) -> ScrubReport {
        v.scrub(&ScrubOptions { repair_parity, ..Default::default() }, || false, |_, _| {}).await
    }

    /// #160 (c): three copies, one spoiled — the two that agree are written
    /// over it, and the next pass is clean.
    #[tokio::test]
    async fn at_mirror_3_the_copies_that_agree_repair_the_odd_one() {
        let dir = tempfile::tempdir().unwrap();
        let (gem, reg) = slabs(3, &dir).await;
        let v = volume(&gem, &reg, "mirror:3");
        let data = pattern(7);
        v.write(0, &data).await.unwrap();
        v.write(SLOT * 2, &pattern(9)).await.unwrap();
        let legs = legs_of(&gem, &v, 0).await;
        assert_eq!(legs.len(), 3);
        spoil(&reg, legs[1], 0xEE).await;

        let r = scrub(&v, false).await;
        assert_eq!((r.extents, r.mirror_mismatches, r.mirror_repaired, r.mirror_reported), (2, 1, 1, 0), "{r:?}");
        assert_eq!(r.findings[0].extent, Some(0));
        assert!(r.findings[0].action.starts_with("repaired: 2 of 3"), "{:?}", r.findings);
        for leg in &legs {
            assert_eq!(leg_bytes(&reg, *leg).await, data, "every leg is the majority's copy");
        }
        assert!(scrub(&v, false).await.clean(), "the next pass is clean");
    }

    /// #160 (a): two copies that disagree are reported and nothing is
    /// written; an operator's repair from a chosen leg settles it.
    #[tokio::test]
    async fn at_mirror_2_a_mismatch_is_reported_and_repaired_only_from_a_chosen_leg() {
        let dir = tempfile::tempdir().unwrap();
        let (gem, reg) = slabs(2, &dir).await;
        let v = volume(&gem, &reg, "mirror:2");
        let data = pattern(3);
        v.write(0, &data).await.unwrap();
        let legs = legs_of(&gem, &v, 0).await;
        spoil(&reg, legs[1], 0x55).await;
        let spoiled = leg_bytes(&reg, legs[1]).await;

        let r = scrub(&v, false).await;
        assert_eq!((r.mirror_mismatches, r.mirror_repaired, r.mirror_reported), (1, 0, 1), "{r:?}");
        assert!(r.findings[0].action.starts_with("reported"), "{:?}", r.findings);
        assert_eq!(leg_bytes(&reg, legs[0]).await, data, "nothing written on a guess");
        assert_eq!(leg_bytes(&reg, legs[1]).await, spoiled, "nothing written on a guess");

        assert_eq!(v.repair_extent_from(0, legs[0].slab_id).await.unwrap(), 1);
        assert_eq!(leg_bytes(&reg, legs[1]).await, data, "the chosen leg's copy");
        assert!(scrub(&v, false).await.clean());
        assert!(v.repair_extent_from(0, SlabId(uuid::Uuid::new_v4())).await.is_err(), "a slab the extent has no leg on");
    }

    /// Three copies in three versions: no majority, so reported.
    #[tokio::test]
    async fn three_copies_with_no_majority_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (gem, reg) = slabs(3, &dir).await;
        let v = volume(&gem, &reg, "mirror:3");
        v.write(0, &pattern(1)).await.unwrap();
        let legs = legs_of(&gem, &v, 0).await;
        spoil(&reg, legs[1], 0x11).await;
        spoil(&reg, legs[2], 0x22).await;
        let r = scrub(&v, false).await;
        assert_eq!((r.mirror_mismatches, r.mirror_repaired, r.mirror_reported), (1, 0, 1), "{r:?}");
        assert_eq!(r.findings[0].legs.len(), 3, "three versions, one leg each");
    }

    /// #160: parity is reported, and rewritten from the data only when asked.
    #[tokio::test]
    async fn parity_is_reported_and_rewritten_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let (gem, reg) = slabs(3, &dir).await;
        let v = volume(&gem, &reg, "raid5:2+1");
        let (a, b) = (pattern(5), pattern(6));
        v.write(0, &a).await.unwrap();
        v.write(SLOT, &b).await.unwrap();
        let p = gem.read().await.lookup_parity(v.volume_id(), 0).unwrap().legs[0];
        spoil(&reg, p, 0x99).await;
        let spoiled = leg_bytes(&reg, p).await;

        let r = scrub(&v, false).await;
        assert_eq!((r.stripes, r.parity_mismatches, r.parity_rewritten), (1, 1, 0), "{r:?}");
        assert_eq!(leg_bytes(&reg, p).await, spoiled, "reported, not rewritten");

        let r = scrub(&v, true).await;
        assert_eq!((r.parity_mismatches, r.parity_rewritten), (1, 1), "{r:?}");
        let want: Vec<u8> = a.iter().zip(&b).map(|(x, y)| x ^ y).collect();
        assert_eq!(leg_bytes(&reg, p).await, want, "P is the data's XOR again");
        assert!(scrub(&v, false).await.clean());
        // The data reads as written throughout.
        let mut back = vec![0u8; SLOT as usize];
        v.read(SLOT, &mut back).await.unwrap();
        assert_eq!(back, b);
    }

    /// A scrub waits while paused and stops when cancelled.
    #[tokio::test]
    async fn a_scrub_stops_when_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let (gem, reg) = slabs(2, &dir).await;
        let v = volume(&gem, &reg, "mirror:2");
        v.write(0, &pattern(1)).await.unwrap();
        let opts = ScrubOptions::default();
        opts.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let r = v.scrub(&opts, || true, |_, _| {}).await;
        assert!(r.cancelled);
        assert_eq!(r.extents, 0);
    }
}
