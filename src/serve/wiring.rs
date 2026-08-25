//! Persistent export wiring — the mk-owned side of the export table.
//!
//! The engine's `ExportEntry` records *intent* (volume → protocol) and has no
//! room for the transport identity mk assigns. Recomputing that identity from
//! scratch on every boot is a data-integrity bug: LUN ids handed out from a
//! counter in `exports.json` iteration order can move between volumes across
//! a restart, so an initiator's disk silently points at a different volume
//! (issue #1). This table pins the assignment:
//!
//! * `lun` — the LUN id on the SHARED target, allocated from a monotonic
//!   counter that never reuses an id after an export is withdrawn.
//! * `portal_port` / `iqn` — the dedicated per-export target, so a consumer
//!   that cannot select a LUN (the RouterOS initiator) still reaches exactly
//!   one volume at LUN 0 (issue #2).
//! * `state` — the teardown ladder `pending → active → draining → withdrawn`
//!   (issue #7), so a LUN is not pulled out from under a live session.
//!
//! Written atomically (tmp + rename) to `<data_dir>/wiring.json`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const WIRING_FILE: &str = "wiring.json";

/// Which transport an export is wired for.
///
/// `Default` is **NVMe** — the profile's transport, and what an export with no
/// stated protocol gets. That is *not* the same question as what a persisted
/// row with no `protocol` field means: those were written before NVMe support
/// existed and are iSCSI, so the field deserializes through `legacy_proto`
/// rather than through `Default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireProto {
    Iscsi,
    Nvmeof,
}

impl Default for WireProto {
    fn default() -> Self {
        WireProto::Nvmeof
    }
}

/// The protocol of a wiring row that predates the `protocol` field. Every such
/// row was an iSCSI export; reading them as NVMe would hand their LUN to
/// nothing and point their initiator at a subsystem that never existed.
fn legacy_proto() -> WireProto {
    WireProto::Iscsi
}

impl WireProto {
    pub fn as_str(&self) -> &'static str {
        match self {
            WireProto::Iscsi => "iscsi",
            WireProto::Nvmeof => "nvmeof",
        }
    }

    /// The name used on the API surface — requests and responses say
    /// `nvme-tcp` (the transport a consumer actually dials), while wiring.json
    /// keeps persisting `nvmeof` so existing rows keep their meaning.
    pub fn api_name(&self) -> &'static str {
        match self {
            WireProto::Iscsi => "iscsi",
            WireProto::Nvmeof => "nvme-tcp",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireState {
    /// Recorded, not yet wired into a target (volume may not exist yet).
    Pending,
    /// LUN present on both the shared and the dedicated target.
    Active,
    /// The export entry is gone; waiting for initiators to disconnect.
    Draining,
    /// LUNs pulled, dedicated target stopped. Row is about to be dropped.
    Withdrawn,
}

impl WireState {
    pub fn as_str(&self) -> &'static str {
        match self {
            WireState::Pending => "pending",
            WireState::Active => "active",
            WireState::Draining => "draining",
            WireState::Withdrawn => "withdrawn",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wiring {
    pub export_id: Uuid,
    pub volume_id: Uuid,
    /// Which transport this row serves.
    #[serde(default = "legacy_proto")]
    pub protocol: WireProto,
    /// Stable LUN id on the shared iSCSI target. Never reused. `None` for
    /// NVMe rows: an NVMe consumer reaches the volume as namespace 1 of its
    /// own subsystem, so there is no shared-target LUN to pin.
    #[serde(default)]
    pub lun: Option<u64>,
    /// Dedicated port for this export's own target (iSCSI portal or NVMe
    /// subsystem — both are just TCP ports out of one allocator).
    pub portal_port: u16,
    /// Dedicated target IQN (iSCSI rows only).
    #[serde(default)]
    pub iqn: String,
    /// Dedicated subsystem NQN (NVMe rows only). One subsystem per volume is
    /// what lets a host `nvme connect` to exactly one volume instead of
    /// discovering every namespace on a shared subsystem.
    #[serde(default)]
    pub nqn: Option<String>,
    pub state: WireState,
    /// Delete the backing volume once the export is fully withdrawn.
    /// Set for clones handed out by `/mk/v1/volumes` with `ephemeral: true`
    /// so dropped clones are actually garbage-collected (issue #7).
    #[serde(default)]
    pub ephemeral: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WiringTable {
    pub version: u32,
    /// Monotonic LUN allocator. Never decreases, never reuses.
    pub next_lun: u64,
    /// Where to start looking for the next per-export portal port.
    ///
    /// Cycles the range so a port is not handed out again the instant its
    /// previous target releases it — see `insert`. Persisted, so a restart
    /// does not immediately reuse the port it was last serving on.
    #[serde(default)]
    pub next_portal: u16,
    pub exports: Vec<Wiring>,
    #[serde(skip)]
    path: PathBuf,
}

impl WiringTable {
    /// Load the table, or start an empty one. A corrupt file is moved aside
    /// rather than deleted — losing the LUN map silently is the bug this
    /// whole module exists to prevent, so it must stay recoverable.
    pub fn load(data_dir: &str) -> Self {
        let path = Path::new(data_dir).join(WIRING_FILE);
        match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<WiringTable>(&raw) {
                Ok(mut t) => {
                    t.path = path;
                    t
                }
                Err(e) => {
                    let bak = path.with_extension("json.corrupt");
                    tracing::error!(
                        "corrupt {} ({e}) — preserved as {}; LUN ids will be reallocated",
                        path.display(),
                        bak.display()
                    );
                    let _ = std::fs::rename(&path, &bak);
                    WiringTable::empty(path)
                }
            },
            Err(_) => WiringTable::empty(path),
        }
    }

    fn empty(path: PathBuf) -> Self {
        WiringTable { version: 1, next_lun: 0, next_portal: 0, exports: Vec::new(), path }
    }

    /// Demote wiring that only existed in the previous process.
    ///
    /// `state` records two different things: the durable half (which LUN,
    /// which portal, is it on its way out) and the live half (is the LUN
    /// actually in a target right now). A restart destroys the live half —
    /// nothing is wired until the reconciler wires it — so an `active` row
    /// read off disk must go back to `pending` or it would be skipped
    /// forever and the initiator would find nothing at its LUN.
    ///
    /// `draining` and `withdrawn` are left alone: they describe an export
    /// that is on its way out, and that is still true after a restart.
    pub fn reset_runtime_state(&mut self) -> usize {
        let mut demoted = 0;
        for row in self.exports.iter_mut() {
            if row.state == WireState::Active {
                row.state = WireState::Pending;
                demoted += 1;
            }
        }
        demoted
    }

    pub fn get(&self, export_id: &Uuid) -> Option<&Wiring> {
        self.exports.iter().find(|w| &w.export_id == export_id)
    }

    pub fn get_mut(&mut self, export_id: &Uuid) -> Option<&mut Wiring> {
        self.exports.iter_mut().find(|w| &w.export_id == export_id)
    }

    /// Record a new export, pinning its LUN id and portal for good.
    ///
    /// `preferred_lun` is the id the engine already assigned (v6.3.0+ wires
    /// the LUN itself on `POST /api/v1/exports` and records it in
    /// `ExportEntry::lun_id`). When it is set we adopt it rather than invent a
    /// competing number, and push the local counter past it so a later
    /// mk-side allocation cannot collide. When it is `None` — an export mk
    /// declared itself, or one restored from an `exports.json` written before
    /// the engine tracked LUNs — we allocate.
    ///
    /// Ports ARE reused (they are only transport endpoints, and the engine
    /// validates `TargetName` at login, so a stale initiator hitting a reused
    /// port is rejected rather than misrouted). LUN ids are NOT.
    #[allow(clippy::too_many_arguments)]
    pub fn insert(
        &mut self,
        export_id: Uuid,
        volume_id: Uuid,
        protocol: WireProto,
        preferred_lun: Option<u64>,
        iqn_prefix: &str,
        nqn_prefix: &str,
        portal_base: u16,
        portal_span: u16,
        ephemeral: bool,
    ) -> anyhow::Result<Wiring> {
        // Round-robin through the range rather than always taking the lowest
        // free port.
        //
        // A withdrawn export frees its port the moment its row goes, but the
        // target that was serving it is still shutting its listener down. The
        // next export then binds the same port, and for a moment two targets
        // exist on it: an initiator can land on the outgoing one while the
        // reconciler asks the incoming one how many connections it has. The
        // answer is honestly zero, so the fresh export is drained and its
        // volume deleted seconds after it was created — which is precisely
        // what was happening, every export, on port 3261.
        //
        // Cycling means a port is only revisited after the whole span has been
        // used, by which time nothing is left holding it. It costs nothing:
        // the range exists to be spread across.
        let used: HashSet<u16> = self.exports.iter().map(|w| w.portal_port).collect();
        let span = portal_span.max(1) as u32;
        let from = self.next_portal.max(portal_base);
        let offset = u32::from(from.saturating_sub(portal_base)) % span;
        let portal_port = (0..span)
            .map(|i| portal_base.saturating_add(((offset + i) % span) as u16))
            .find(|p| !used.contains(p))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "per-export portal range {}..{} is exhausted ({} live) — raise STORMBLOCKMK_PORTAL_SPAN",
                    portal_base,
                    portal_base.saturating_add(portal_span),
                    used.len()
                )
            })?;

        // Next time, start after this one.
        self.next_portal = portal_base
            .saturating_add((((u32::from(portal_port.saturating_sub(portal_base)) + 1) % span) as u16));

        // Only iSCSI rows consume a LUN id; burning one for an NVMe export
        // would imply a shared-target LUN that is never created.
        let lun = match protocol {
            WireProto::Nvmeof => None,
            WireProto::Iscsi => Some(match preferred_lun {
                Some(l) => {
                    self.next_lun = self.next_lun.max(l + 1);
                    l
                }
                None => {
                    let l = self.next_lun;
                    self.next_lun += 1;
                    l
                }
            }),
        };
        let w = Wiring {
            export_id,
            volume_id,
            protocol,
            lun,
            portal_port,
            iqn: match protocol {
                WireProto::Iscsi => format!("{iqn_prefix}:vol-{volume_id}"),
                WireProto::Nvmeof => String::new(),
            },
            nqn: match protocol {
                WireProto::Nvmeof => Some(format!("{nqn_prefix}:vol-{volume_id}")),
                WireProto::Iscsi => None,
            },
            state: WireState::Pending,
            ephemeral,
        };
        self.exports.push(w.clone());
        Ok(w)
    }

    pub fn remove(&mut self, export_id: &Uuid) {
        self.exports.retain(|w| &w.export_id != export_id);
    }

    pub fn count(&self, state: WireState) -> usize {
        self.exports.iter().filter(|w| w.state == state).count()
    }

    /// Atomic persist: write a sibling tmp file, fsync, rename over.
    pub fn persist(&self) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        write_atomic(&self.path, json.as_bytes())
    }
}

/// tmp + fsync + rename. Used for every mk-owned durable file, so a power cut
/// mid-write can never leave a truncated table behind.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)
            .map_err(|e| anyhow::anyhow!("creating {}: {e}", tmp.display()))?;
        f.write_all(bytes).map_err(|e| anyhow::anyhow!("writing {}: {e}", tmp.display()))?;
        f.sync_all().map_err(|e| anyhow::anyhow!("fsync {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .map_err(|e| anyhow::anyhow!("rename {} -> {}: {e}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The transport a fresh export gets when nobody says otherwise.
    #[test]
    fn default_transport_is_nvme() {
        assert_eq!(WireProto::default(), WireProto::Nvmeof);
        assert_eq!(WireProto::default().api_name(), "nvme-tcp");
    }

    /// A row persisted before the `protocol` field existed is iSCSI, and must
    /// stay iSCSI now that the *default* transport has moved to NVMe — reading
    /// it as NVMe would strand its LUN and its initiator.
    #[test]
    fn wiring_rows_without_a_protocol_are_iscsi() {
        let legacy = r#"{
            "export_id": "00000000-0000-0000-0000-000000000001",
            "volume_id": "00000000-0000-0000-0000-000000000002",
            "lun": 0,
            "portal_port": 3261,
            "iqn": "iqn.2026-08.lo.gt:vol-x",
            "state": "active"
        }"#;
        let row: Wiring = serde_json::from_str(legacy).unwrap();
        assert_eq!(row.protocol, WireProto::Iscsi);
        assert_eq!(row.lun, Some(0));
    }

    /// Only iSCSI rows consume a LUN id; an NVMe volume is namespace 1 of its
    /// own subsystem, so burning one would imply a shared-target LUN that is
    /// never created.
    #[test]
    fn nvme_rows_take_a_port_but_no_lun() {
        let mut t = WiringTable::empty(PathBuf::from("/nonexistent/wiring.json"));
        let nvme = t
            .insert(
                Uuid::new_v4(),
                Uuid::new_v4(),
                WireProto::Nvmeof,
                None,
                "iqn.test",
                "nqn.test",
                3261,
                4,
                false,
            )
            .unwrap();
        assert_eq!(nvme.lun, None);
        assert_eq!(nvme.portal_port, 3261);
        assert!(nvme.nqn.unwrap().starts_with("nqn.test:vol-"));
        assert_eq!(t.next_lun, 0);

        let iscsi = t
            .insert(
                Uuid::new_v4(),
                Uuid::new_v4(),
                WireProto::Iscsi,
                None,
                "iqn.test",
                "nqn.test",
                3261,
                4,
                false,
            )
            .unwrap();
        assert_eq!(iscsi.lun, Some(0));
        assert_eq!(iscsi.portal_port, 3262);
    }
}
