//! The export reconciler — declared intent in, live targets out.
//!
//! The engine's `/api/v1/exports` records intent and stops there; wiring it
//! into a target is this profile's job. What the reconciler guarantees:
//!
//! * **Stable identity** (issue #1). A LUN id is allocated once, persisted in
//!   `wiring.json`, honoured on every later boot, and never reused. An
//!   initiator's disk cannot silently change which volume it points at.
//! * **Reachable addressing** (issue #2). Every export gets its own dedicated
//!   target on its own port: for NVMe — the default transport — its own
//!   **subsystem NQN with the volume as namespace 1**; for a legacy iSCSI
//!   export, a target IQN with the volume at LUN 0. RouterOS attaches one
//!   namespace per `/disk add` and a host connecting to a shared subsystem
//!   would discover every namespace on it, so one target per volume is what
//!   makes "attach this volume" expressible in either transport.
//! * **No wiring for a transport that is not served.** An iSCSI row while
//!   `STORMBLOCKMK_ENABLE_ISCSI` is unset is reported blocked and left alone —
//!   never half-wired, never silently converted to NVMe (its initiator is
//!   looking for an IQN, not an NQN).
//! * **Ordered teardown** (issue #7). A withdrawn export goes
//!   `active → draining → withdrawn`: the LUN stays wired until the initiator
//!   has disconnected or the grace period expires, and only then is an
//!   ephemeral clone garbage-collected.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::mgmt::api::luns;
use crate::mgmt::{ExportEntry, ExportProtocol, ExportStatus, LunBacking};
use crate::target::iscsi::{IscsiConfig, IscsiTarget};
#[cfg(feature = "nvmeof")]
use crate::target::nvmeof::{NvmeofConfig, NvmeofTarget};
use crate::volume::VolumeId;

use super::ctx::{Portal, ServeContext};
#[cfg(feature = "nvmeof")]
use super::ctx::Subsystem;
use super::netstat;
use super::wiring::{WireProto, WireState, Wiring};

/// Run the reconciler until the process exits.
pub async fn run(ctx: Arc<ServeContext>) {
    let interval = Duration::from_secs(ctx.cfg.reconcile_secs);
    loop {
        if let Err(e) = pass(&ctx).await {
            ctx.status.bump(&ctx.status.reconciler_errors);
            tracing::warn!("reconciler pass failed: {e}");
        }
        ctx.status.set(&ctx.status.reconciled, true);
        tokio::time::sleep(interval).await;
    }
}

/// One full reconcile. Safe to call repeatedly; every step is idempotent.
pub async fn pass(ctx: &Arc<ServeContext>) -> anyhow::Result<()> {
    let mut dirty = false;
    let mut newly_active: Vec<Uuid> = Vec::new();

    // ── 1. Reconcile the row set against the export table ─────────────────
    // The export table is read *inside* the wiring lock, and every writer
    // that adds an export takes that same lock across both mutations. Read
    // outside it and a pass that snapshotted the table a moment before an
    // export was declared would see the fresh row as an orphan and start
    // draining a LUN that had only just been created.
    {
        let mut w = ctx.wiring.lock().await;
        let entries: Vec<ExportEntry> = ctx.state.exports.read().await.clone();
        let live: HashSet<Uuid> = entries.iter().map(|e| e.id).collect();

        // 1a. Exports we have not seen before get their identity pinned.
        for e in entries.iter() {
            if w.get(&e.id).is_some() {
                continue;
            }
            let proto = match e.protocol {
                ExportProtocol::Nvmeof => WireProto::Nvmeof,
                ExportProtocol::Iscsi => WireProto::Iscsi,
            };
            match w.insert(
                e.id,
                e.volume_id,
                proto,
                e.lun_id,
                &ctx.cfg.iqn_prefix,
                &ctx.cfg.nqn_prefix,
                ctx.cfg.portal_base,
                ctx.cfg.portal_span,
                false,
            ) {
                Ok(row) => {
                    dirty = true;
                    match row.protocol {
                        WireProto::Nvmeof => tracing::info!(
                            "export {} volume {} -> nvme subsystem {} on port {}",
                            row.export_id,
                            row.volume_id,
                            row.nqn.as_deref().unwrap_or("?"),
                            row.portal_port
                        ),
                        WireProto::Iscsi => tracing::info!(
                            "export {} volume {} -> LUN {:?} (portal {}, iqn {})",
                            row.export_id, row.volume_id, row.lun, row.portal_port, row.iqn
                        ),
                    }
                }
                Err(err) => {
                    ctx.status.bump(&ctx.status.reconciler_errors);
                    tracing::error!("export {}: cannot allocate wiring: {err}", e.id);
                }
            }
        }

        // 1b. Exports that disappeared start draining.
        let grace = Duration::from_secs(ctx.cfg.drain_grace_secs);
        let mut deadlines = ctx.drain_deadlines.lock().await;
        for row in w.exports.iter_mut() {
            let gone = !live.contains(&row.export_id);
            match row.state {
                WireState::Active | WireState::Pending if gone => {
                    row.state = WireState::Draining;
                    deadlines.insert(row.export_id, Instant::now() + grace);
                    dirty = true;
                    tracing::info!(
                        "export {} withdrawn — draining {} (grace {}s)",
                        row.export_id,
                        match row.protocol {
                            WireProto::Nvmeof => format!("nqn {}", row.nqn.as_deref().unwrap_or("?")),
                            WireProto::Iscsi => format!("LUN {:?}", row.lun),
                        },
                        ctx.cfg.drain_grace_secs
                    );
                }
                // Restarted mid-drain: restart the clock rather than pull the
                // LUN out from under whoever is still attached.
                WireState::Draining => {
                    deadlines.entry(row.export_id).or_insert_with(|| Instant::now() + grace);
                }
                _ => {}
            }
        }
    }

    // ── 2. Wire pending rows ──────────────────────────────────────────────
    let orphan_grace = Duration::from_secs(ctx.cfg.orphan_export_grace_secs);
    // Snapshot the rows to wire so the volume-manager lock is never taken
    // while the wiring lock is held.
    let pending: Vec<Wiring> = {
        let w = ctx.wiring.lock().await;
        w.exports.iter().filter(|r| r.state == WireState::Pending).cloned().collect()
    };
    for row in pending {
        // A transport that is not being served can never wire this row. Say so
        // once — on every pass would be 30 identical lines a minute — and move
        // on; `refresh_counters` keeps it out of `exports_pending` so it does
        // not wedge readiness for the exports that ARE wireable.
        if ctx.is_blocked(&row) {
            if ctx.blocked_reported.lock().await.insert(row.export_id) {
                // Which transport is missing decides what the operator can do
                // about it, so the two cases do not share a sentence.
                match row.protocol {
                    WireProto::Iscsi => tracing::warn!(
                        "export {} (volume {}) is iSCSI, but iSCSI is not served — left unwired. \
                         Set STORMBLOCKMK_ENABLE_ISCSI=1 to bring the legacy stack up, or \
                         withdraw the export and recreate it with protocol \"nvme-tcp\"",
                        row.export_id,
                        row.volume_id
                    ),
                    WireProto::Nvmeof => tracing::warn!(
                        "export {} (volume {}) is NVMe-TCP, but this build has no NVMe-oF \
                         support — left unwired. Withdraw the export and recreate it with \
                         protocol \"iscsi\", or run a build with the nvmeof feature",
                        row.export_id,
                        row.volume_id
                    ),
                }
            }
            continue;
        }

        let dev = { ctx.state.volume_manager.lock().await.get_volume(&VolumeId(row.volume_id)) };
        let Some(dev) = dev else {
            // A volume that has not been created yet is normal right after a
            // POST. A volume that never appears is not: the row can never be
            // wired, nothing else removes it, and `Pending` gates readiness —
            // so mk retried it forever and reported `ready=false` for good
            // (#15, found with 26 such rows on rose1).
            //
            // Time it rather than acting on the first miss, because the
            // legitimate case looks identical for a moment. Past the grace,
            // withdraw the export entry: that hands the row to the ordered
            // teardown in step 1b instead of deleting it here, which keeps
            // one path responsible for pulling wiring apart.
            if orphan_grace.is_zero() {
                tracing::debug!(
                    "export {}: volume {} not present yet",
                    row.export_id,
                    row.volume_id
                );
                continue;
            }
            let first = {
                let mut seen = ctx.export_orphan_first_seen.lock().await;
                *seen.entry(row.export_id).or_insert_with(Instant::now)
            };
            if first.elapsed() < orphan_grace {
                tracing::debug!(
                    "export {}: volume {} not present yet",
                    row.export_id,
                    row.volume_id
                );
                continue;
            }
            let withdrawn = ctx.withdraw_exports_for_volume(row.volume_id).await;
            if withdrawn.is_empty() {
                // No entry to withdraw, so the row is already detached from
                // the export table and step 1b will drain it on its own.
                tracing::debug!(
                    "export {}: volume {} absent and no export entry to withdraw",
                    row.export_id,
                    row.volume_id
                );
            } else {
                if let Err(e) = ctx.persist_exports().await {
                    ctx.status.bump(&ctx.status.reconciler_errors);
                    tracing::error!("persisting export table after withdrawing {withdrawn:?}: {e}");
                    continue;
                }
                tracing::warn!(
                    "export {} names volume {}, which has not existed for {}s — withdrawing {:?}.                      A volume was deleted without its export; the row would otherwise stay pending                      forever and hold readiness down (#15)",
                    row.export_id,
                    row.volume_id,
                    orphan_grace.as_secs(),
                    withdrawn
                );
            }
            ctx.export_orphan_first_seen.lock().await.remove(&row.export_id);
            continue;
        };
        // It wired, so it was never an orphan — forget any timing for it.
        ctx.export_orphan_first_seen.lock().await.remove(&row.export_id);

        // NVMe rows get a dedicated subsystem and nothing else: there is no
        // shared-target LUN to pin, and the volume is namespace 1 of its own
        // subsystem.
        #[cfg(feature = "nvmeof")]
        if row.protocol == WireProto::Nvmeof {
            match start_subsystem(ctx, &row, dev).await {
                Ok(()) => {
                    let mut w = ctx.wiring.lock().await;
                    if let Some(r) = w.get_mut(&row.export_id) {
                        r.state = WireState::Active;
                    }
                    drop(w);
                    // Record the NSID where a consumer can see it, the way
                    // `ensure_lun` records the LUN id. It is always 1: the
                    // volume is the only namespace of its own subsystem, and
                    // an entry restored with anything else was written by the
                    // old shared-subsystem scheme.
                    {
                        let mut ex = ctx.state.exports.write().await;
                        if let Some(e) = ex.iter_mut().find(|e| e.id == row.export_id) {
                            if e.nsid != Some(1) {
                                e.nsid = Some(1);
                            }
                        }
                    }
                    dirty = true;
                    newly_active.push(row.export_id);
                    tracing::info!(
                        "export {} volume {} -> nvme ACTIVE ({}:{} nqn {} nsid 1)",
                        row.export_id,
                        row.volume_id,
                        ctx.cfg.advertise_addr,
                        row.portal_port,
                        row.nqn.as_deref().unwrap_or("?"),
                    );
                }
                Err(e) => {
                    ctx.status.bump(&ctx.status.reconciler_errors);
                    tracing::error!(
                        "export {}: nvme subsystem on port {} failed: {e}",
                        row.export_id, row.portal_port
                    );
                }
            }
            continue;
        }

        // Shared multi-LUN target, at the volume's pinned LUN id. This goes
        // through the engine's LUN table rather than straight to
        // `add_lun_dynamic`, so the table, `luns.json` and the target all
        // agree — the engine restores from that file at boot and allocates
        // fresh ids out of it, and a LUN it does not know about would be
        // handed to somebody else.
        if let Err(e) = ensure_lun(ctx, &row).await {
            ctx.status.bump(&ctx.status.reconciler_errors);
            tracing::error!("export {}: attaching LUN {:?}: {e}", row.export_id, row.lun);
            continue;
        }

        // Dedicated single-volume target.
        match start_portal(ctx, &row, dev).await {
            Ok(()) => {
                let mut w = ctx.wiring.lock().await;
                if let Some(r) = w.get_mut(&row.export_id) {
                    r.state = WireState::Active;
                }
                drop(w);
                dirty = true;
                newly_active.push(row.export_id);
                tracing::info!(
                    "export {} volume {} -> LUN {:?} ACTIVE (portal {}:{} iqn {})",
                    row.export_id,
                    row.volume_id,
                    row.lun,
                    ctx.cfg.advertise_addr,
                    row.portal_port,
                    row.iqn
                );
            }
            Err(e) => {
                if let Some(lun) = row.lun {
                    luns::detach_lun(&ctx.state, lun).await;
                }
                ctx.status.bump(&ctx.status.reconciler_errors);
                tracing::error!("export {}: portal {} failed: {e}", row.export_id, row.portal_port);
            }
        }
    }

    // ── 3. Finish draining once nobody is attached (or grace expires) ─────
    let draining: Vec<Wiring> = {
        let w = ctx.wiring.lock().await;
        w.exports.iter().filter(|r| r.state == WireState::Draining).cloned().collect()
    };
    for row in draining {
        let expired = {
            let deadlines = ctx.drain_deadlines.lock().await;
            deadlines.get(&row.export_id).map(|d| Instant::now() >= *d).unwrap_or(true)
        };
        // The dedicated portal serves exactly this one volume, so an
        // established connection to its port is this export's initiator.
        let conns = netstat::established_on_port(row.portal_port);
        let withdraw = match conns {
            Some(0) => true,
            Some(n) if expired => {
                tracing::warn!(
                    "export {}: grace expired with {n} session(s) still attached on port {} — withdrawing anyway",
                    row.export_id,
                    row.portal_port
                );
                true
            }
            Some(n) => {
                tracing::debug!("export {}: {n} session(s) still attached, waiting", row.export_id);
                false
            }
            // /proc unreadable: unknown is not "idle", so only the timeout
            // may release it.
            None => expired,
        };
        if !withdraw {
            continue;
        }

        if row.protocol == WireProto::Nvmeof {
            #[cfg(feature = "nvmeof")]
            stop_subsystem(ctx, &row.export_id).await;
        } else {
            stop_portal(ctx, &row.export_id).await;
            if let Some(lun) = row.lun {
                luns::detach_lun(&ctx.state, lun).await;
            }
        }
        let mut w = ctx.wiring.lock().await;
        if let Some(r) = w.get_mut(&row.export_id) {
            r.state = WireState::Withdrawn;
        }
        drop(w);
        ctx.drain_deadlines.lock().await.remove(&row.export_id);
        dirty = true;
        tracing::info!(
            "export {} withdrawn — {} released, port {} closed",
            row.export_id,
            match row.protocol {
                WireProto::Nvmeof => format!("nqn {}", row.nqn.as_deref().unwrap_or("?")),
                WireProto::Iscsi => format!("LUN {:?}", row.lun),
            },
            row.portal_port
        );
    }

    // ── 4. GC: drop withdrawn rows, delete the volumes marked ephemeral ───
    let withdrawn: Vec<Wiring> = {
        let w = ctx.wiring.lock().await;
        w.exports.iter().filter(|r| r.state == WireState::Withdrawn).cloned().collect()
    };
    for row in withdrawn {
        if row.ephemeral {
            let mut vm = ctx.state.volume_manager.lock().await;
            match vm.delete_volume(VolumeId(row.volume_id)).await {
                Ok(()) => {
                    ctx.status.bump(&ctx.status.volumes_gc);
                    tracing::info!("gc: ephemeral volume {} deleted", row.volume_id);
                }
                Err(e) => tracing::warn!("gc: deleting volume {}: {e}", row.volume_id),
            }
        }
        ctx.wiring.lock().await.remove(&row.export_id);
        ctx.blocked_reported.lock().await.remove(&row.export_id);
        dirty = true;
    }

    // ── 5. Reflect wiring back into the engine's export table ─────────────
    if !newly_active.is_empty() {
        let mut ex = ctx.state.exports.write().await;
        for e in ex.iter_mut() {
            if newly_active.contains(&e.id) {
                e.status = ExportStatus::Active;
            }
        }
    }

    // ── 6. Persist and publish ────────────────────────────────────────────
    if dirty {
        ctx.wiring.lock().await.persist()?;
        ctx.persist_exports().await?;
    }
    refresh_counters(ctx).await;
    Ok(())
}

/// Make sure the volume is present at `row.lun` on the shared target, and
/// that the export entry records the id.
///
/// Idempotent by design: at boot the engine's `restore_luns` has usually
/// re-attached the LUN from `luns.json` already, and an export created
/// through `POST /api/v1/exports` was wired by the engine on the way in. Only
/// the gaps — an export mk declared itself, or one whose LUN went missing —
/// need an attach.
async fn ensure_lun(ctx: &Arc<ServeContext>, row: &Wiring) -> anyhow::Result<()> {
    let Some(lun) = row.lun else {
        return Err(anyhow::anyhow!("iscsi wiring row has no LUN id"));
    };
    let already = ctx.state.lun_entries.read().await.contains_key(&lun);
    if !already {
        luns::attach_lun(
            &ctx.state,
            LunBacking::Volume { volume_id: row.volume_id },
            Some(lun),
            false,
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    // Record the id where a consumer can see it. The engine fills this in for
    // exports created through its own API; mk-declared ones start as None.
    let mut ex = ctx.state.exports.write().await;
    if let Some(e) = ex.iter_mut().find(|e| e.id == row.export_id) {
        if e.lun_id != Some(lun) {
            e.lun_id = Some(lun);
        }
    }
    Ok(())
}

/// Bind and run a dedicated single-volume iSCSI target for one export.
///
/// The listener is bound here rather than inside `IscsiTarget::run` so a port
/// clash surfaces as an error on this export instead of a task that dies
/// silently.
async fn start_portal(
    ctx: &Arc<ServeContext>,
    row: &Wiring,
    dev: Arc<dyn crate::drive::BlockDevice>,
) -> anyhow::Result<()> {
    let mut portals = ctx.portals.lock().await;
    if portals.contains_key(&row.export_id) {
        return Ok(());
    }

    let addr = SocketAddr::new(ctx.portal_bind_ip(), row.portal_port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("binding {addr}: {e}"))?;

    let target = Arc::new(IscsiTarget::new(IscsiConfig {
        listen_addr: addr,
        target_name: row.iqn.clone(),
        chap: None,
        max_sessions: 8,
        // Anything added to IscsiConfig later takes its default rather
        // than breaking the build here.
        ..Default::default()
    }));
    // LUN 0: the only LUN this target will ever have.
    target.add_lun_dynamic(0, dev, false).await;

    let runner = target.clone();
    let iqn = row.iqn.clone();
    let reactor = ctx.reactor.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = runner.run_with_listener(listener, &reactor).await {
            tracing::error!("portal {addr} ({iqn}) stopped: {e}");
        }
    });

    portals.insert(
        row.export_id,
        Portal { target, task, port: row.portal_port, iqn: row.iqn.clone() },
    );
    Ok(())
}

/// Stop an export's dedicated target and release its port.
async fn stop_portal(ctx: &Arc<ServeContext>, export_id: &Uuid) {
    if let Some(p) = ctx.portals.lock().await.remove(export_id) {
        p.target.remove_lun(0).await;
        // Aborting the accept loop drops the listener, which frees the port.
        p.task.abort();
        tracing::debug!("portal {} ({}) stopped", p.port, p.iqn);
    }
}

/// Bind and run a dedicated single-volume NVMe-oF subsystem for one export.
#[cfg(feature = "nvmeof")]
///
/// One subsystem NQN per volume, the volume as namespace 1, on its own port.
/// The discovery log page advertises the routable address rather than the
/// wildcard we bind, so a remote `nvme connect` works unchanged.
async fn start_subsystem(
    ctx: &Arc<ServeContext>,
    row: &Wiring,
    dev: Arc<dyn crate::drive::BlockDevice>,
) -> anyhow::Result<()> {
    let mut subs = ctx.subsystems.lock().await;
    if subs.contains_key(&row.export_id) {
        return Ok(());
    }
    let nqn = row
        .nqn
        .clone()
        .ok_or_else(|| anyhow::anyhow!("nvme wiring row has no NQN"))?;

    let addr = SocketAddr::new(ctx.portal_bind_ip(), row.portal_port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("binding {addr}: {e}"))?;

    let target = Arc::new(NvmeofTarget::new(NvmeofConfig {
        listen_addr: addr,
        nqn: nqn.clone(),
        advertised_addr: format!("{}:{}", ctx.cfg.advertise_addr, row.portal_port).parse().ok(),
        ..Default::default()
    }));
    // Namespace 1: the only namespace this subsystem will ever have.
    target.add_namespace_dynamic(1, dev).await;

    let runner = target.clone();
    let reactor = ctx.reactor.clone();
    let nqn_log = nqn.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = runner.run_with_listener(listener, &reactor).await {
            tracing::error!("nvme subsystem {addr} ({nqn_log}) stopped: {e}");
        }
    });

    subs.insert(row.export_id, Subsystem { target, task, port: row.portal_port, nqn });
    Ok(())
}

/// Stop an export's dedicated subsystem and release its port.
#[cfg(feature = "nvmeof")]
async fn stop_subsystem(ctx: &Arc<ServeContext>, export_id: &Uuid) {
    if let Some(s) = ctx.subsystems.lock().await.remove(export_id) {
        s.target.remove_namespace(1).await;
        s.task.abort();
        tracing::debug!("nvme subsystem {} ({}) stopped", s.port, s.nqn);
    }
}

/// Refresh the readiness counters and the Prometheus gauges.
pub async fn refresh_counters(ctx: &Arc<ServeContext>) {
    let (volumes, virt, alloc, ids) = {
        let vm = ctx.state.volume_manager.lock().await;
        let list = vm.list_volumes().await;
        let virt: u64 = list.iter().map(|(_, _, v, _)| *v).sum();
        let alloc: u64 = list.iter().map(|(_, _, _, a)| *a).sum();
        let ids: HashSet<Uuid> = list.iter().map(|(id, _, _, _)| id.0).collect();
        (list.len() as u64, virt, alloc, ids)
    };
    let w = ctx.wiring.lock().await;
    let s = &ctx.status;
    // A row whose transport is not served is blocked, not pending: it is not
    // waiting on anything mk is doing, so it must not sit in the counter that
    // gates readiness.
    // A row naming a volume that does not exist is counted as well as
    // pending, not instead of it: for the first few moments that is simply a
    // row recorded ahead of its volume, and only the grace period tells the
    // two apart. Reporting it is what makes #15 visible while it happens.
    let (pending, blocked, orphaned) =
        w.exports.iter().filter(|r| r.state == WireState::Pending).fold(
            (0u64, 0u64, 0u64),
            |(p, b, o), r| {
                if ctx.is_blocked(r) {
                    (p, b + 1, o)
                } else if ids.contains(&r.volume_id) {
                    (p + 1, b, o)
                } else {
                    (p + 1, b, o + 1)
                }
            },
        );
    s.store(&s.volumes, volumes);
    s.store(&s.bytes_virtual, virt);
    s.store(&s.bytes_allocated, alloc);
    s.store(&s.exports_total, w.exports.len() as u64);
    s.store(&s.exports_active, w.count(WireState::Active) as u64);
    s.store(&s.exports_pending, pending);
    s.store(&s.exports_blocked, blocked);
    s.store(&s.exports_orphaned, orphaned);
    s.store(&s.exports_draining, w.count(WireState::Draining) as u64);
    let luns = match &ctx.shared_iscsi {
        Some(t) => t.lun_count().await as u64,
        None => 0,
    };
    s.store(&s.luns_wired, luns);
    drop(w);
    s.store(&s.portals, ctx.portals.lock().await.len() as u64);
    #[cfg(feature = "nvmeof")]
    s.store(&s.subsystems, ctx.subsystems.lock().await.len() as u64);
    s.publish_metrics();
}
