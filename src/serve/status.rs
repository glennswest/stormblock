//! Readiness and counters (issue #7).
//!
//! `/api/v1/health` only proves the process is alive. A stormblockmk that is
//! up but has not restored its volume metadata, or whose exports never got
//! wired, is worse than one that is down: consumers attach and find nothing.
//! Everything a supervisor or an operator needs to distinguish those states
//! is recorded here, published on `/mk/v1/ready` and `/mk/v1/status`, and
//! mirrored into the Prometheus registry the engine already exposes on
//! `/metrics`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct MkStatus {
    /// Slab backing file opened (or formatted) successfully.
    pub slab_open: AtomicBool,
    /// `VolumeManager::restore()` completed without error.
    pub volumes_restored: AtomicBool,
    /// Is the legacy iSCSI stack served at all (`STORMBLOCKMK_ENABLE_ISCSI`)?
    /// Readiness only asks about the iSCSI listener when it is.
    pub iscsi_enabled: AtomicBool,
    /// The shared iSCSI portal is bound and accepting.
    pub iscsi_listening: AtomicBool,
    /// The NVMe-oF/TCP portal is bound — the primary transport, so this one
    /// always gates readiness.
    pub nvmeof_listening: AtomicBool,
    /// The management server is bound.
    pub mgmt_listening: AtomicBool,
    /// At least one reconciler pass has completed.
    pub reconciled: AtomicBool,

    pub volumes: AtomicU64,
    pub bytes_virtual: AtomicU64,
    pub bytes_allocated: AtomicU64,
    pub exports_total: AtomicU64,
    pub exports_active: AtomicU64,
    pub exports_pending: AtomicU64,
    pub exports_draining: AtomicU64,
    /// Rows that cannot be wired because their transport is turned off (an
    /// iSCSI export while `STORMBLOCKMK_ENABLE_ISCSI` is unset). Counted apart
    /// from `exports_pending` on purpose: they are not waiting on anything mk
    /// is doing, so they must not hold readiness down forever.
    pub exports_blocked: AtomicU64,
    /// Pending rows naming a volume that does not currently exist.
    ///
    /// Diagnostic, not a gate: a row recorded a moment before its volume is
    /// created looks exactly like one whose volume is gone, and only time
    /// tells them apart. The reconciler withdraws the genuine orphans once
    /// `orphan_export_grace_secs` has passed, so a number that stays put here
    /// is the shape of #15 — and the number nobody could see while it was
    /// only a `debug!` line.
    pub exports_orphaned: AtomicU64,
    pub luns_wired: AtomicU64,
    pub portals: AtomicU64,
    pub subsystems: AtomicU64,
    pub reconciler_errors: AtomicU64,
    pub volumes_gc: AtomicU64,
}

impl MkStatus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ready means: a consumer that attaches right now gets what it expects.
    ///
    /// Deliberately includes "every persisted export is wired": a pod whose
    /// rootfs LUN has not come back yet must not be told the storage backend
    /// is ready.
    pub fn ready(&self) -> bool {
        let iscsi_ok = !self.iscsi_enabled.load(Ordering::Relaxed)
            || self.iscsi_listening.load(Ordering::Relaxed);
        self.slab_open.load(Ordering::Relaxed)
            && self.volumes_restored.load(Ordering::Relaxed)
            && self.nvmeof_listening.load(Ordering::Relaxed)
            && iscsi_ok
            && self.mgmt_listening.load(Ordering::Relaxed)
            && self.reconciled.load(Ordering::Relaxed)
            && self.exports_pending.load(Ordering::Relaxed) == 0
    }

    /// Human-readable list of what is holding readiness back.
    pub fn blockers(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.slab_open.load(Ordering::Relaxed) {
            out.push("slab not open");
        }
        if !self.volumes_restored.load(Ordering::Relaxed) {
            out.push("volume metadata not restored");
        }
        if !self.nvmeof_listening.load(Ordering::Relaxed) {
            out.push("nvme-tcp portal not listening");
        }
        if self.iscsi_enabled.load(Ordering::Relaxed)
            && !self.iscsi_listening.load(Ordering::Relaxed)
        {
            out.push("iscsi portal not listening");
        }
        if !self.mgmt_listening.load(Ordering::Relaxed) {
            out.push("management API not listening");
        }
        if !self.reconciled.load(Ordering::Relaxed) {
            out.push("no reconciler pass yet");
        }
        if self.exports_pending.load(Ordering::Relaxed) > 0 {
            out.push("exports still pending wiring");
        }
        out
    }

    pub fn json(&self) -> serde_json::Value {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let b = |a: &AtomicBool| a.load(Ordering::Relaxed);
        serde_json::json!({
            "ready": self.ready(),
            "blockers": self.blockers(),
            "slab_open": b(&self.slab_open),
            "volumes_restored": b(&self.volumes_restored),
            "iscsi_enabled": b(&self.iscsi_enabled),
            "iscsi_listening": b(&self.iscsi_listening),
            "nvmeof_listening": b(&self.nvmeof_listening),
            "mgmt_listening": b(&self.mgmt_listening),
            "reconciled": b(&self.reconciled),
            "volumes": g(&self.volumes),
            "bytes_virtual": g(&self.bytes_virtual),
            "bytes_allocated": g(&self.bytes_allocated),
            "exports_total": g(&self.exports_total),
            "exports_active": g(&self.exports_active),
            "exports_pending": g(&self.exports_pending),
            "exports_draining": g(&self.exports_draining),
            "exports_blocked": g(&self.exports_blocked),
            "exports_orphaned": g(&self.exports_orphaned),
            "luns_wired": g(&self.luns_wired),
            "portals": g(&self.portals),
            "subsystems": g(&self.subsystems),
            "reconciler_errors": g(&self.reconciler_errors),
            "volumes_gc": g(&self.volumes_gc),
        })
    }

    /// Mirror the counters into the engine's Prometheus registry so a stuck
    /// export is visible on `/metrics` without reading logs.
    pub fn publish_metrics(&self) {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed) as f64;
        metrics::gauge!("stormblockmk_ready").set(if self.ready() { 1.0 } else { 0.0 });
        metrics::gauge!("stormblockmk_volumes").set(g(&self.volumes));
        metrics::gauge!("stormblockmk_bytes_virtual").set(g(&self.bytes_virtual));
        metrics::gauge!("stormblockmk_bytes_allocated").set(g(&self.bytes_allocated));
        metrics::gauge!("stormblockmk_exports_total").set(g(&self.exports_total));
        metrics::gauge!("stormblockmk_exports_active").set(g(&self.exports_active));
        metrics::gauge!("stormblockmk_exports_pending").set(g(&self.exports_pending));
        metrics::gauge!("stormblockmk_exports_draining").set(g(&self.exports_draining));
        metrics::gauge!("stormblockmk_exports_blocked").set(g(&self.exports_blocked));
        metrics::gauge!("stormblockmk_exports_orphaned").set(g(&self.exports_orphaned));
        metrics::gauge!("stormblockmk_luns_wired").set(g(&self.luns_wired));
        metrics::gauge!("stormblockmk_portals").set(g(&self.portals));
        metrics::gauge!("stormblockmk_subsystems").set(g(&self.subsystems));
        metrics::gauge!("stormblockmk_reconciler_errors_total").set(g(&self.reconciler_errors));
        metrics::gauge!("stormblockmk_volumes_gc_total").set(g(&self.volumes_gc));
    }

    pub fn set(&self, field: &AtomicBool, v: bool) {
        field.store(v, Ordering::Relaxed);
    }

    pub fn store(&self, field: &AtomicU64, v: u64) {
        field.store(v, Ordering::Relaxed);
    }

    pub fn bump(&self, field: &AtomicU64) {
        field.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up() -> MkStatus {
        let s = MkStatus::new();
        s.set(&s.slab_open, true);
        s.set(&s.volumes_restored, true);
        s.set(&s.nvmeof_listening, true);
        s.set(&s.mgmt_listening, true);
        s.set(&s.reconciled, true);
        s
    }

    #[test]
    fn readiness_ignores_iscsi_when_it_is_not_served() {
        let s = up();
        assert!(s.ready(), "{:?}", s.blockers());
        assert!(!s.blockers().iter().any(|b| b.contains("iscsi")));
    }

    #[test]
    fn readiness_requires_iscsi_once_it_is_enabled() {
        let s = up();
        s.set(&s.iscsi_enabled, true);
        assert!(!s.ready());
        assert!(s.blockers().contains(&"iscsi portal not listening"));
        s.set(&s.iscsi_listening, true);
        assert!(s.ready());
    }

    /// NVMe is the primary transport: no readiness without it, ever.
    #[test]
    fn readiness_always_requires_nvme() {
        let s = up();
        s.set(&s.nvmeof_listening, false);
        assert!(!s.ready());
        assert!(s.blockers().contains(&"nvme-tcp portal not listening"));
    }

    /// A row whose transport is turned off is not "pending" — it is blocked,
    /// and must not wedge readiness for the exports that ARE wireable.
    #[test]
    fn blocked_rows_do_not_hold_readiness_down() {
        let s = up();
        s.store(&s.exports_blocked, 3);
        assert!(s.ready());
        s.store(&s.exports_pending, 1);
        assert!(!s.ready());
    }

    /// `exports_orphaned` reports, it does not gate.
    ///
    /// A row naming a volume that does not exist yet is indistinguishable
    /// from one naming a volume that is gone, so it still counts as pending
    /// and still holds readiness — the reconciler withdraws the real orphans
    /// once the grace expires, and readiness comes back on its own. What must
    /// not happen is the counter itself deciding anything (#15).
    #[test]
    fn orphaned_rows_are_reported_but_do_not_gate_readiness() {
        let s = up();
        s.store(&s.exports_orphaned, 26);
        assert!(s.ready(), "the orphan count alone must not hold readiness down");
        assert!(s.blockers().is_empty());

        // It is the pending count that gates, exactly as before.
        s.store(&s.exports_pending, 26);
        assert!(!s.ready());
        assert!(s.blockers().contains(&"exports still pending wiring"));

        // And once the reconciler has withdrawn them, readiness returns
        // without a restart — the thing rose1 could not do.
        s.store(&s.exports_pending, 0);
        s.store(&s.exports_orphaned, 0);
        assert!(s.ready());
    }
}
