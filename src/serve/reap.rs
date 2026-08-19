//! Reclaiming filesystem-template volumes nobody references any more.
//!
//! # Why this exists
//!
//! Creating a template makes **two** volumes: `fstemplate-<name>-raw`, which
//! gets formatted, and `fstemplate-<fs>-<name>`, the sealed snapshot clones
//! descend from. Neither is removed when a create fails part-way, and the raw
//! one outlives a create that succeeds. Every retry therefore costs two more
//! volumes, permanently.
//!
//! Measured on rose1 2026-08-18: **186 of 187 volumes** were template debris —
//! 22.85 GB of a 22.87 GB total, against a single real consumer volume. One
//! name, `fstemplate-pvc-ext4j-5120m`, appeared **64 times** against 17
//! registered templates. Raised upstream as stormblock#47; this is mk's
//! mitigation while that is open, in the same spirit as the slab collector mk
//! carried in v0.5.0 until the engine grew a better one.
//!
//! # Why the engine's extent collector cannot do this
//!
//! `volume::gc` reclaims extents whose owning **volume is gone**. These
//! volumes are not gone — they are live and referenced by the volume manager,
//! so the collector correctly reports `orphans: 0` while 22 GB sits in them.
//! The leak is one level up, at the volume, and only something that knows the
//! *template store* can see it: a template volume is garbage precisely when no
//! store entry names it.
//!
//! # The safety rules, in order
//!
//! 1. **Name must carry the engine's template prefix.** A consumer volume is
//!    never a candidate, whatever else is true of it. Clones are named by the
//!    consumer, so they never match.
//! 2. **Never delete a volume the store names**, as either `raw_volume_id` or
//!    `sealed_volume_id`. This is what protects every live template, including
//!    the one live copy among 64 identically-named dead ones — the match is by
//!    id, never by name.
//! 3. **Never delete an exported volume.** Same rule ordered teardown exists
//!    for: a disk must not vanish under a consumer.
//! 4. **Never delete a young orphan.** `create` makes the raw volume *before*
//!    it writes the store entry, so for a moment a perfectly healthy
//!    in-progress template looks exactly like debris. A volume must have been
//!    continuously orphaned across two passes and for `min_age` before it is
//!    touched. This is the rule that makes an automatic sweep safe; without it
//!    the sweep would race template creation and delete a filesystem mid-format.
//! 5. **Bounded per pass.** A cap means a mistake costs a bounded number of
//!    volumes and shows up in the log before it costs the rest.
//!
//! Deleting a sealed volume that still has clones is safe by construction: the
//! GEM ref-counts extents, so a clone's own references keep every shared slot
//! alive, and release has been best-effort per slot since stormblock v8.0.0
//! (#37, raised from here). Rule 2 keeps the *registered* sealed volumes
//! regardless.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::volume::VolumeId;

use super::ctx::ServeContext;

/// Both volumes the engine makes for a template start with this —
/// `fstemplate-<name>-raw` at create, `fstemplate-<fs>-<name>` at seal.
pub const TEMPLATE_PREFIX: &str = "fstemplate-";

/// One volume the sweep would remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: Uuid,
    pub name: String,
    pub virtual_bytes: u64,
    pub allocated_bytes: u64,
}

/// What one classification pass found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    pub orphans: Vec<Candidate>,
    /// Template volumes a store entry still names.
    pub kept_referenced: u64,
    /// Template volumes with an export — never touched at any force level.
    pub kept_exported: u64,
    /// Everything that is not a template volume at all.
    pub kept_consumer: u64,
}

impl Scan {
    pub fn reclaimable_bytes(&self) -> u64 {
        self.orphans.iter().map(|c| c.allocated_bytes).sum()
    }
}

/// Classify every volume. Pure, so the rules can be tested without a slab.
///
/// `volumes` is the volume manager's own `(id, name, virtual, allocated)`
/// listing; `referenced` is every `raw_volume_id` and `sealed_volume_id` in
/// the template store; `exported` is every volume with a wiring row.
pub fn scan(
    volumes: &[(Uuid, String, u64, u64)],
    referenced: &HashSet<Uuid>,
    exported: &HashSet<Uuid>,
) -> Scan {
    let mut out = Scan::default();
    for (id, name, virtual_bytes, allocated_bytes) in volumes {
        // Rule 1 — a consumer volume is never a candidate.
        if !name.starts_with(TEMPLATE_PREFIX) {
            out.kept_consumer += 1;
            continue;
        }
        // Rule 2 — by id, never by name. Sixty-four volumes can share a name;
        // only one of them is the one the store means.
        if referenced.contains(id) {
            out.kept_referenced += 1;
            continue;
        }
        // Rule 3 — never pull a disk out from under a consumer.
        if exported.contains(id) {
            out.kept_exported += 1;
            continue;
        }
        out.orphans.push(Candidate {
            id: *id,
            name: name.clone(),
            virtual_bytes: *virtual_bytes,
            allocated_bytes: *allocated_bytes,
        });
    }
    out
}

/// What one sweep did.
#[derive(Debug, Clone, Default)]
pub struct SweepReport {
    pub scan: Scan,
    /// Orphans that also cleared the age gate.
    pub eligible: Vec<Candidate>,
    pub deleted: Vec<Candidate>,
    pub failed: Vec<(Candidate, String)>,
    /// Orphans held back because they have not been orphaned long enough.
    pub held_young: u64,
    /// Eligible orphans left for the next pass by the per-pass cap.
    pub held_capped: u64,
    pub applied: bool,
}

impl SweepReport {
    pub fn freed_bytes(&self) -> u64 {
        self.deleted.iter().map(|c| c.allocated_bytes).sum()
    }
}

/// One sweep: classify, apply the age gate, and — only when `apply` — delete.
///
/// Locks are taken in the order `ctx.rs` documents (wiring → engine) and each
/// is released before the next, so a long sweep never holds the volume manager
/// against the reconciler. Deletion re-takes the manager **per volume** rather
/// than once for the batch: 186 deletions under one guard would stall every
/// attach for the duration.
pub async fn sweep(
    ctx: &Arc<ServeContext>,
    apply: bool,
    min_age: Duration,
    max: usize,
) -> SweepReport {
    let exported: HashSet<Uuid> = {
        let w = ctx.wiring.lock().await;
        w.exports.iter().map(|r| r.volume_id).collect()
    };
    let referenced: HashSet<Uuid> = {
        let store = ctx.state.fstemplates.lock().await;
        // Both are `Option` as of stormblock v9.0.0: the engine clears
        // `raw_volume_id` once a template is sealed, because it now deletes the
        // scratch volume rather than leaving it (stormblock#47, raised from
        // here). A template mid-create still has one, and it must stay
        // protected — which is exactly what including it here does.
        store
            .templates
            .iter()
            .flat_map(|t| t.raw_volume_id.into_iter().chain(t.sealed_volume_id))
            .collect()
    };
    let volumes: Vec<(Uuid, String, u64, u64)> = {
        let vm = ctx.state.volume_manager.lock().await;
        vm.list_volumes()
            .await
            .into_iter()
            .map(|(id, name, virtual_bytes, allocated)| (id.0, name, virtual_bytes, allocated))
            .collect()
    };

    let found = scan(&volumes, &referenced, &exported);
    let mut report = SweepReport { applied: false, ..Default::default() };

    // Rule 4 — the age gate. A volume must have been *continuously* orphaned
    // since a previous pass: `or_insert(now)` gives a newly-seen orphan an age
    // of zero, so nothing can be deleted on the pass that first sees it. Any
    // id that stops being an orphan is forgotten, so a template that is
    // created, deleted and recreated starts its clock again.
    let now = Instant::now();
    let current: HashSet<Uuid> = found.orphans.iter().map(|c| c.id).collect();
    {
        let mut seen = ctx.orphan_first_seen.lock().await;
        seen.retain(|id, _| current.contains(id));
        for c in &found.orphans {
            let first = *seen.entry(c.id).or_insert(now);
            if now.duration_since(first) >= min_age {
                report.eligible.push(c.clone());
            } else {
                report.held_young += 1;
            }
        }
    }

    report.held_capped = report.eligible.len().saturating_sub(max) as u64;
    report.scan = found;

    if !apply {
        return report;
    }
    report.applied = true;

    for c in report.eligible.iter().take(max) {
        let outcome = {
            let mut vm = ctx.state.volume_manager.lock().await;
            vm.delete_volume(VolumeId(c.id)).await.map(|_| ())
        };
        match outcome {
            Ok(()) => {
                tracing::info!(
                    "reaped orphaned template volume {} ({}) — {} bytes allocated",
                    c.name,
                    c.id,
                    c.allocated_bytes
                );
                report.deleted.push(c.clone());
            }
            Err(e) => {
                tracing::warn!("reaping template volume {} ({}): {e}", c.name, c.id);
                report.failed.push((c.clone(), e.to_string()));
            }
        }
    }
    report
}

/// Every volume id the manager currently knows. Take this *before* a clone, so
/// [`sweep_failed_clone`] can tell what the clone made.
pub async fn volume_ids(ctx: &Arc<ServeContext>) -> HashSet<Uuid> {
    let vm = ctx.state.volume_manager.lock().await;
    vm.list_volumes().await.into_iter().map(|(id, _, _, _)| id.0).collect()
}

/// Remove whatever a **failed clone** left behind.
///
/// # Why this cannot be left to the engine
///
/// `clone_template` verifies a clone with `fsck` before handing it out and
/// discards one that does not check out. It discards it like this:
///
/// ```text
/// let _ = vm.lock().await.delete_volume(id).await;
/// return Err(... "did not check out and was discarded: {why}"));
/// ```
///
/// The delete's error is thrown away, so when the delete *itself* fails the
/// volume survives while the caller is told in as many words that it was
/// discarded. That is not hypothetical: `delete_volume` failing on a stale GEM
/// entry is exactly stormblock#37, raised from here. Raised again for this
/// specific swallow.
///
/// # Why a name-prefix reaper cannot cover it either
///
/// A clone is created under the **caller's own name** — `pvc-web-1`, not
/// `fstemplate-…`. A leaked failed clone is therefore indistinguishable by
/// name from a live consumer volume, which is why [`sweep`] deliberately never
/// considers one, and why "delete unexported volumes older than N" would be a
/// data-loss bug rather than a cleanup.
///
/// # What makes this precise
///
/// mk asked for the clone, so mk knows two things nothing else does: the exact
/// set of volumes that existed a moment before, and the name it asked for.
/// A volume is removed only when it is **new since `before`**, carries
/// **exactly the requested name**, and has **no export**. Requiring the name
/// as well as the novelty is what makes a concurrent create by another caller
/// safe — its volume is new too, but it is not this one.
pub async fn sweep_failed_clone(
    ctx: &Arc<ServeContext>,
    before: &HashSet<Uuid>,
    name: &str,
) -> Vec<Candidate> {
    let exported: HashSet<Uuid> = {
        let w = ctx.wiring.lock().await;
        w.exports.iter().map(|r| r.volume_id).collect()
    };
    let after: Vec<(Uuid, String, u64, u64)> = {
        let vm = ctx.state.volume_manager.lock().await;
        vm.list_volumes()
            .await
            .into_iter()
            .map(|(id, n, v, a)| (id.0, n, v, a))
            .collect()
    };

    let mut removed = Vec::new();
    for (id, vol_name, virtual_bytes, allocated_bytes) in after {
        if before.contains(&id) || vol_name != name || exported.contains(&id) {
            continue;
        }
        let c = Candidate { id, name: vol_name, virtual_bytes, allocated_bytes };
        let outcome = {
            let mut vm = ctx.state.volume_manager.lock().await;
            vm.delete_volume(VolumeId(id)).await.map(|_| ())
        };
        match outcome {
            Ok(()) => {
                tracing::info!(
                    "removed the volume left behind by a failed clone: {} ({})",
                    c.name,
                    c.id
                );
                removed.push(c);
            }
            // Now it is genuinely stuck — say so loudly rather than swallowing
            // it a second time. The background sweep will not reach it, because
            // it carries a consumer name.
            Err(e) => tracing::error!(
                "a failed clone left volume {} ({}) behind and it could not be deleted: {e} \
                 — this volume is leaked and must be removed by hand",
                c.name,
                c.id
            ),
        }
    }
    removed
}

/// The background sweep.
///
/// Off with `STORMBLOCKMK_REAP_SECS=0`; report-only with
/// `STORMBLOCKMK_REAP_REPORT_ONLY=1`, which logs what it would remove and
/// removes nothing.
pub async fn run(ctx: Arc<ServeContext>) {
    if ctx.cfg.reap_secs == 0 {
        tracing::info!("template reaper disabled (STORMBLOCKMK_REAP_SECS=0)");
        return;
    }
    tracing::info!(
        "template reaper every {}s ({}, min_age {}s, max {} per pass)",
        ctx.cfg.reap_secs,
        if ctx.cfg.reap_apply { "deleting" } else { "report only" },
        ctx.cfg.reap_min_age_secs,
        ctx.cfg.reap_max_per_pass,
    );
    let mut tick = tokio::time::interval(Duration::from_secs(ctx.cfg.reap_secs));
    // The first tick fires immediately, which only records first-seen times —
    // nothing can be deleted until min_age has passed, by construction.
    loop {
        tick.tick().await;
        let r = sweep(
            &ctx,
            ctx.cfg.reap_apply,
            Duration::from_secs(ctx.cfg.reap_min_age_secs),
            ctx.cfg.reap_max_per_pass,
        )
        .await;

        metrics::gauge!("stormblockmk_template_orphans").set(r.scan.orphans.len() as f64);
        metrics::gauge!("stormblockmk_template_orphan_bytes")
            .set(r.scan.reclaimable_bytes() as f64);

        if !r.deleted.is_empty() || !r.failed.is_empty() {
            tracing::info!(
                "template reaper: {} deleted ({} bytes), {} failed, {} held young, {} held by cap",
                r.deleted.len(),
                r.freed_bytes(),
                r.failed.len(),
                r.held_young,
                r.held_capped,
            );
        } else if !r.scan.orphans.is_empty() {
            tracing::debug!(
                "template reaper: {} orphan(s), {} bytes reclaimable, {} held young",
                r.scan.orphans.len(),
                r.scan.reclaimable_bytes(),
                r.held_young,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn vols() -> Vec<(Uuid, String, u64, u64)> {
        vec![
            // The live template pair — both named by the store.
            (id(1), "fstemplate-base-raw".into(), 1000, 100),
            (id(2), "fstemplate-ext4-base".into(), 1000, 100),
            // Debris sharing those exact names, from failed retries.
            (id(3), "fstemplate-base-raw".into(), 1000, 100),
            (id(4), "fstemplate-ext4-base".into(), 1000, 200),
            // A consumer volume cloned from the template.
            (id(5), "pvc-web-1".into(), 5000, 50),
            // A template volume that is somehow exported.
            (id(6), "fstemplate-ext4-old".into(), 1000, 300),
        ]
    }

    #[test]
    fn identically_named_debris_is_told_apart_from_the_live_pair() {
        // The store names ids 1 and 2. Ids 3 and 4 carry the *same names* and
        // are debris — which is the whole point: on rose1 one name appeared 64
        // times and exactly one of them was live.
        let referenced = HashSet::from([id(1), id(2)]);
        let exported = HashSet::from([id(6)]);
        let s = scan(&vols(), &referenced, &exported);

        let got: Vec<Uuid> = s.orphans.iter().map(|c| c.id).collect();
        assert_eq!(got, vec![id(3), id(4)]);
        assert_eq!(s.kept_referenced, 2);
        assert_eq!(s.kept_exported, 1);
        assert_eq!(s.kept_consumer, 1);
        assert_eq!(s.reclaimable_bytes(), 300);
    }

    #[test]
    fn a_consumer_volume_is_never_a_candidate() {
        // Not referenced, not exported, and still kept — the name decides.
        let s = scan(
            &[(id(9), "pvc-writetest-2".into(), 100, 21)],
            &HashSet::new(),
            &HashSet::new(),
        );
        assert!(s.orphans.is_empty());
        assert_eq!(s.kept_consumer, 1);
    }

    #[test]
    fn an_exported_template_volume_is_never_a_candidate() {
        let s = scan(
            &[(id(6), "fstemplate-ext4-old".into(), 1000, 300)],
            &HashSet::new(),
            &HashSet::from([id(6)]),
        );
        assert!(s.orphans.is_empty());
        assert_eq!(s.kept_exported, 1);
    }

    /// The dangerous case: a create has made the raw volume and has not yet
    /// written its store entry. Classification alone calls that an orphan —
    /// which is correct, and is exactly why the caller must also apply the
    /// minimum-age gate before deleting anything.
    #[test]
    fn an_in_progress_create_classifies_as_orphan_and_needs_the_age_gate() {
        let s = scan(
            &[(id(7), "fstemplate-brand-new-raw".into(), 1000, 4)],
            &HashSet::new(),
            &HashSet::new(),
        );
        assert_eq!(s.orphans.len(), 1, "the age gate is not optional");
    }

    #[test]
    fn an_empty_store_does_not_make_every_template_volume_garbage_by_name_alone() {
        // Still orphans by the rules — but the caller sweeps at most `max` per
        // pass and logs each one, so a store that failed to load costs a
        // bounded, visible number rather than everything at once.
        let s = scan(&vols(), &HashSet::new(), &HashSet::new());
        assert_eq!(s.orphans.len(), 5);
        assert_eq!(s.kept_consumer, 1);
    }
}
