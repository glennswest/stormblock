//! A named golden from a stopped VM's disk (#143).
//!
//! `POST /api/v1/goldens {volume, name, provenance}` takes a VM's root volume
//! once the guest has powered itself off, and makes it a golden:
//!
//! 1. refuses it while anything serves it (the VM is still attached), unless
//!    `force`;
//! 2. snapshots it (copy-on-write, so the VM's own volume is left as stormvm
//!    knows it) and seals the snapshot;
//! 3. digests the snapshot's whole content (sha256 over every byte of its
//!    virtual size, so the same content always has the same name, here or on
//!    forge after an import);
//! 4. names it `golden-<name>-<sha12>`, never over another volume: the same
//!    content made again answers the golden that is already there;
//! 5. records it in `<data_dir>/goldens.json`: name, digest, size, the VM
//!    volume it came from, the golden that volume was cloned from (parent),
//!    the caller's provenance (commit, playbook, builder) and who made it.
//!
//! **Off the node** (owner, #143: 1A): the verb is destructive, so a builder
//! calls it with a Kubernetes bearer its SubjectAccessReview allows
//! (`storage.storm.io` `goldens` `create`, #274); no engine token leaves the
//! node. **To forge** (2A): the reply carries an expiring **ticket** URL,
//! `GET /api/v1/goldens/{name}/content?ticket=…` (Range-capable, sealed
//! goldens only), which the builder hands to forge's own import with the
//! digest and provenance; forge pulls, checks the digest and records it. No
//! node holds a forge credential (#247).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::drive::BlockDevice;
use crate::mgmt::AppState;
use crate::volume::VolumeId;

pub const GOLDENS_FILE: &str = "goldens.json";
/// A ticket lives an hour unless asked otherwise, and never more than a day.
pub const TICKET_SECS: u64 = 3600;
pub const TICKET_MAX_SECS: u64 = 86_400;

/// A volume, by id and by the name it had.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolRef {
    pub id: Uuid,
    pub name: String,
}

/// What this node knows about a golden it made or imported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoldenRecord {
    /// `golden-<name>-<sha12>`: the volume's name.
    pub name: String,
    pub volume_id: Uuid,
    /// sha256 of every byte of the volume's virtual size.
    pub sha256: String,
    pub size_bytes: u64,
    pub made_at: u64,
    /// `made` (from a VM's disk here) or `imported` (pulled from a node).
    pub how: String,
    /// The VM volume it was made from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<VolRef>,
    /// The golden the source was cloned from, by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// What the builder says about it: commit, playbook, builder, VM, node.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provenance: BTreeMap<String, String>,
    /// Who asked (the audit log's caller).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub made_by: Option<String>,
    /// Where an import pulled it from (no ticket).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

fn file(state: &AppState) -> Option<PathBuf> {
    state.config.management.data_dir.as_ref().map(|d| PathBuf::from(d).join(GOLDENS_FILE))
}

pub fn load(state: &AppState) -> Vec<GoldenRecord> {
    file(state)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn store_lock() -> &'static tokio::sync::Mutex<()> {
    static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Keep `r`, replacing a record of the same name.
pub async fn record(state: &AppState, r: GoldenRecord) -> Result<(), String> {
    let _g = store_lock().lock().await;
    let mut all = load(state);
    all.retain(|x| x.name != r.name);
    all.push(r);
    all.sort_by(|a, b| a.name.cmp(&b.name));
    let p = file(state).ok_or("this node keeps no data directory")?;
    let bytes = serde_json::to_vec_pretty(&all).map_err(|e| e.to_string())?;
    crate::serve::wiring::write_atomic(&p, &bytes).map_err(|e| e.to_string())
}

/// The golden of that name, when its volume is still here.
pub async fn find(state: &AppState, name: &str) -> Option<GoldenRecord> {
    let r = load(state).into_iter().find(|r| r.name == name)?;
    state.volume_manager.lock().await.get_volume_handle(&VolumeId(r.volume_id))?;
    Some(r)
}

/// A builder's name: lower-case letters, digits and dashes, 1–48, starting
/// with a letter or digit. It ends up in a volume name and a URL.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 48
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && s.as_bytes()[0] != b'-'
}

/// `golden-<name>-<sha12>`.
pub fn golden_name(name: &str, sha256: &str) -> String {
    format!("golden-{name}-{}", &sha256[..12.min(sha256.len())])
}

/// sha256 of every byte of `dev`, read in 4 MiB steps; the hashing runs on
/// the blocking pool, never on a runtime worker.
pub async fn digest(dev: Arc<dyn BlockDevice>) -> Result<String, String> {
    const STEP: u64 = 4 * 1024 * 1024;
    let size = dev.capacity_bytes();
    let bs = (dev.block_size().max(1)) as u64;
    let step = STEP.div_ceil(bs) * bs;
    let mut h = Sha256::new();
    let mut off = 0u64;
    while off < size {
        let want = step.min(size - off);
        let span = want.div_ceil(bs) * bs;
        let mut buf = crate::drive::dma::DmaBuf::zeroed(span as usize);
        dev.read(off, &mut buf).await.map_err(|e| format!("read at {off}: {e}"))?;
        h = tokio::task::spawn_blocking(move || {
            h.update(&buf[..want as usize]);
            h
        })
        .await
        .map_err(|e| e.to_string())?;
        off += want;
    }
    Ok(hex::encode(h.finalize()))
}

/// The request.
#[derive(Debug, Clone, Deserialize)]
pub struct MakeRequest {
    /// The VM's root volume: `<namespace>.<vm>-root`, or its id.
    pub volume: String,
    /// The golden's name; the volume is `golden-<name>-<sha12>`.
    pub name: String,
    #[serde(default)]
    pub provenance: BTreeMap<String, String>,
    /// Make it although something still serves the volume (a VM that is
    /// still attached: its filesystem may be mid-write).
    #[serde(default)]
    pub force: bool,
    /// How long the reply's ticket lives (default an hour, at most a day).
    #[serde(default)]
    pub ticket_secs: Option<u64>,
}

/// Why a make failed, as an HTTP status and a sentence.
#[derive(Debug)]
pub struct MakeError(pub u16, pub String);

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Make the golden; see the module doc. `Ok((record, existing))`: `existing`
/// is true when the same content was already a golden here.
pub async fn make(
    state: &AppState,
    source: VolumeId,
    req: &MakeRequest,
    made_by: Option<String>,
) -> Result<(GoldenRecord, bool), MakeError> {
    if !valid_name(&req.name) {
        return Err(MakeError(400, format!(
            "name '{}': lower-case letters, digits and dashes, at most 48, not starting with a dash",
            req.name
        )));
    }
    let serving = crate::mgmt::api::what_is_serving(state, source.0).await;
    if !serving.is_empty() && !req.force {
        return Err(MakeError(409, format!(
            "volume {} is in use ({}): power the VM off and detach it first, or pass force",
            req.volume,
            serving.join(", ")
        )));
    }
    let (source_name, parent) = {
        let vm = state.volume_manager.lock().await;
        let Some(h) = vm.get_volume_handle(&source) else {
            return Err(MakeError(404, format!("no volume {}", req.volume)));
        };
        let parent = match vm.parent(&source) {
            Some(p) => match vm.get_volume_handle(&p) {
                Some(ph) => Some(ph.name().await),
                None => Some(p.0.to_string()),
            },
            None => None,
        };
        (h.name().await, parent)
    };

    // A point-in-time copy, so the VM's own volume is left alone.
    let pending = format!("golden-{}-pending-{}", req.name, &Uuid::new_v4().simple().to_string()[..8]);
    let snap = state
        .volume_manager
        .lock()
        .await
        .create_snapshot(source, &pending)
        .await
        .map_err(|e| MakeError(500, format!("snapshot: {e}")))?;
    let pending_name: &str = &pending;
    let undo = |why: MakeError| async move {
        if let Err(e) = state.volume_manager.lock().await.delete_volume(snap).await {
            tracing::warn!("golden: the pending snapshot {pending_name} could not be removed: {e}");
        }
        why
    };
    let Some(dev) = state.volume_manager.lock().await.get_volume(&snap) else {
        return Err(undo(MakeError(500, "the snapshot vanished".into())).await);
    };
    let fs = crate::fs::disk::probe(&dev).await;
    if let Err(e) = state.volume_manager.lock().await.seal_volume(snap, fs).await {
        return Err(undo(MakeError(500, format!("seal: {e}"))).await);
    }
    let size = dev.capacity_bytes();
    let sha = match digest(dev).await {
        Ok(s) => s,
        Err(e) => return Err(undo(MakeError(500, format!("digest: {e}"))).await),
    };
    let name = golden_name(&req.name, &sha);

    // Never over another volume; the same content again is the golden here.
    let taken = state.volume_manager.lock().await.find_volume(&name).await;
    if let Some(other) = taken {
        let same = load(state).into_iter().find(|r| r.name == name && r.volume_id == other.0 && r.sha256 == sha);
        let _ = undo(MakeError(0, String::new())).await;
        return match same {
            Some(r) => Ok((r, true)),
            None => Err(MakeError(409, format!(
                "a volume named {name} is already here and is not this content's golden; nothing was made"
            ))),
        };
    }
    if let Err(e) = state.volume_manager.lock().await.rename_volume(snap, &name).await {
        return Err(undo(MakeError(500, format!("rename to {name}: {e}"))).await);
    }
    let r = GoldenRecord {
        name,
        volume_id: snap.0,
        sha256: sha,
        size_bytes: size,
        made_at: now(),
        how: "made".into(),
        source: Some(VolRef { id: source.0, name: source_name }),
        parent,
        provenance: req.provenance.clone(),
        made_by,
        from: None,
    };
    if let Err(e) = record(state, r.clone()).await {
        tracing::warn!("golden {}: made, but not recorded: {e}", r.name);
    }
    tracing::info!("golden {} made from {} (sha256 {})", r.name, req.volume, r.sha256);
    Ok((r, false))
}

// ── Tickets ───────────────────────────────────────────────────────────────

struct Ticket {
    golden: String,
    expires: u64,
}

fn tickets() -> &'static Mutex<HashMap<String, Ticket>> {
    static T: OnceLock<Mutex<HashMap<String, Ticket>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(ticket: &str) -> String {
    hex::encode(Sha256::digest(ticket.as_bytes()))
}

/// A ticket to read `golden`'s content for `secs` (capped at a day). Kept in
/// memory, by its hash: a restart forgets every ticket, and a new one is a
/// `POST /api/v1/goldens/{name}/ticket` away.
pub fn mint_ticket(golden: &str, secs: Option<u64>) -> (String, u64) {
    let secs = secs.unwrap_or(TICKET_SECS).clamp(1, TICKET_MAX_SECS);
    let t = crate::mgmt::auth::mint();
    let expires = now() + secs;
    let mut m = tickets().lock().unwrap_or_else(|p| p.into_inner());
    let n = now();
    m.retain(|_, v| v.expires > n);
    m.insert(key(&t), Ticket { golden: golden.to_string(), expires });
    (t, expires)
}

/// Whether `ticket` reads `golden` now. A ticket is good for one golden,
/// many times (Range resumes), until it expires.
pub fn ticket_reads(ticket: &str, golden: &str) -> bool {
    let m = tickets().lock().unwrap_or_else(|p| p.into_inner());
    m.get(&key(ticket)).is_some_and(|t| t.golden == golden && t.expires > now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_tickets() {
        assert!(valid_name("buildbox2") && valid_name("fedora-43"));
        assert!(!valid_name("") && !valid_name("-x") && !valid_name("Fedora") && !valid_name("a/b") && !valid_name(&"a".repeat(49)));
        assert_eq!(golden_name("bb", "0123456789abcdef"), "golden-bb-0123456789ab");
        let (t, _) = mint_ticket("golden-a-1", Some(60));
        assert!(ticket_reads(&t, "golden-a-1"));
        assert!(!ticket_reads(&t, "golden-b-1"), "a ticket reads one golden");
        assert!(!ticket_reads("nope", "golden-a-1"));
    }
}
