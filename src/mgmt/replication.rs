//! `Replication` (#656): sync this engine's store from a peer's.
//!
//! Owner, 2026-11-18: "We should have a sync/replicate!" A site's second
//! store (forge1) takes what its first (forge) holds: goldens, media, blanks
//! (and, from stage 2, writable volumes and the registry's references),
//! once or following it. Managed through the object, the owner's rule:
//! `storage.storm.io/v1` `replications`, kept in `<data_dir>/replications.json`.
//!
//! **How a volume moves** (thin, lineage kept, nothing new needed on the
//! peer): the peer's listing (`GET /api/v1/volumes?kind=…`) says each
//! volume's name, namespace, size, parent, seal, role, filesystem and owner;
//! its `GET /api/v1/volumes/{id}/legs` lists the extents it maps and where
//! each lives; `GET /api/v1/volumes/{id}/content` with a `Range` gives a
//! sealed volume's bytes. A volume with no parent here gets every extent it
//! maps; a clone whose parent is already here is a copy-on-write clone of
//! that, given only the extents it holds of its own (a location that is not
//! its parent's at the same index), and zeros where its parent maps an
//! extent it does not. 87 GiB moves, not the 6 TB the sizes add to. Parents
//! go first. A volume already here under the same name and namespace,
//! sealed, of the same size, is taken as the same and not copied again; one
//! that differs is an error, never overwritten. Nothing here is ever
//! deleted because the peer deleted it: the copy is the point.
//!
//! **Stage 1** (this): sealed goldens, media and blanks. A writable volume is
//! reported, not copied: the peer serves no content of a volume that takes
//! writes. The registry's references (`images`) are stage 2.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::drive::BlockDevice;
use crate::mgmt::AppState;
use crate::volume::VolumeId;

const FILE: &str = "replications.json";
/// The most bytes one content request asks for.
const MAX_RANGE: u64 = 8 << 20;

/// Where from.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Peer {
    /// The peer engine's management API (`http://host:9090`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// The peer registry (stage 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// A credential for a closed peer (stage 2: an open peer only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_secret: Option<String>,
}

/// What is taken; empty lists take everything.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Select {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
    /// Name patterns, `*` any run of characters (`golden-*`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Spec {
    pub from: Peer,
    /// `goldens`, `media`, `blanks`, `volumes`, `images`. Empty: the
    /// engine's sealed kinds (goldens, media, blanks).
    #[serde(default)]
    pub what: Vec<String>,
    #[serde(default)]
    pub select: Select,
    /// `once` or `mirror` (follow the peer every `every` seconds).
    #[serde(default = "once")]
    pub mode: String,
    #[serde(default = "every")]
    pub every: u64,
    /// MiB/s; 0 unlimited.
    #[serde(default)]
    pub bandwidth_mi_bps: u64,
}

fn once() -> String {
    "once".into()
}
fn every() -> u64 {
    600
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Items {
    pub total: u64,
    pub done: u64,
    pub failed: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Bytes {
    pub total: u64,
    pub done: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ItemError {
    pub reference: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    pub reason: String,
    pub message: String,
    pub last_transition_time: u64,
}

/// Field order is the wire's (no sorted map): `phase` first, `items` as
/// `total, done, failed`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// `Pending`, `Listing`, `Syncing`, `Synced`, `Failed` (some items did
    /// not sync), `Waiting` (mirror, between passes).
    pub phase: String,
    pub items: Items,
    pub bytes: Bytes,
    pub rate_mi_bps: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced: Option<u64>,
    /// What is being copied now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    pub errors: Vec<ItemError>,
    /// What the spec asked that this engine does not do yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub conditions: Vec<Condition>,
    pub observed_generation: u64,
}

/// One Replication as kept.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    pub name: String,
    pub uid: String,
    pub generation: u64,
    pub created: u64,
    pub spec: Spec,
    pub status: Status,
    /// Peer volume id -> the volume here.
    #[serde(default)]
    pub map: BTreeMap<String, String>,
}

/// The object as served: typed, so its fields keep their order.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Object<'a> {
    pub api_version: &'static str,
    pub kind: &'static str,
    pub metadata: Meta<'a>,
    pub spec: &'a Spec,
    pub status: &'a Status,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta<'a> {
    pub name: &'a str,
    pub uid: &'a str,
    pub generation: u64,
    pub creation_timestamp: u64,
}

impl Record {
    pub fn object(&self) -> Object<'_> {
        Object {
            api_version: "storage.storm.io/v1",
            kind: "Replication",
            metadata: Meta { name: &self.name, uid: &self.uid, generation: self.generation, creation_timestamp: self.created },
            spec: &self.spec,
            status: &self.status,
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ------------------------------------------------------------------ store

/// Every replication by data dir (tests run several engines in a process).
static STORE: Mutex<Option<HashMap<String, BTreeMap<String, Record>>>> = Mutex::new(None);
static FILE_LOCK: Mutex<()> = Mutex::new(());

fn dir_key(state: &AppState) -> String {
    state.config.management.data_dir.clone().unwrap_or_default()
}

fn load_file(state: &AppState) -> BTreeMap<String, Record> {
    let Some(d) = state.config.management.data_dir.as_ref() else { return BTreeMap::new() };
    std::fs::read(std::path::Path::new(d).join(FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn with<R>(state: &AppState, f: impl FnOnce(&mut BTreeMap<String, Record>) -> R) -> R {
    let mut g = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let all = g.get_or_insert_with(HashMap::new);
    let k = dir_key(state);
    if !all.contains_key(&k) {
        all.insert(k.clone(), load_file(state));
    }
    f(all.get_mut(&k).expect("inserted"))
}

/// Write the records out (after a change).
fn save(state: &AppState) {
    let Some(d) = state.config.management.data_dir.clone() else { return };
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let body = with(state, |m| serde_json::to_vec_pretty(m).unwrap_or_default());
    let p = std::path::Path::new(&d).join(FILE);
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, &p)).is_err() {
        tracing::warn!("replication: {} not written", p.display());
    }
}

pub fn get(state: &AppState, name: &str) -> Option<Record> {
    with(state, |m| m.get(name).cloned())
}

pub fn list(state: &AppState) -> Vec<Record> {
    with(state, |m| m.values().cloned().collect())
}

/// A Replication's name: a DNS-1123 label.
pub fn valid_name(s: &str) -> bool {
    crate::volume::metadata::valid_namespace(s)
}

/// Check a spec (#656).
pub fn check(spec: &Spec) -> Result<(), String> {
    let e = spec.from.engine.as_deref().unwrap_or("");
    let engine_kinds = spec.what.is_empty() || spec.what.iter().any(|w| ENGINE_KINDS.iter().any(|(k, _)| k == w));
    if engine_kinds && !(e.starts_with("http://") || e.starts_with("https://")) {
        return Err("spec.from.engine: the peer engine's URL (http:// or https://)".into());
    }
    for w in &spec.what {
        if !ENGINE_KINDS.iter().any(|(k, _)| k == w) && w != "images" {
            return Err(format!("spec.what: {w}: goldens, media, blanks, volumes or images"));
        }
    }
    if !matches!(spec.mode.as_str(), "once" | "mirror") {
        return Err(format!("spec.mode: {}: once or mirror", spec.mode));
    }
    if spec.mode == "mirror" && spec.every < 30 {
        return Err("spec.every: at least 30 seconds".into());
    }
    Ok(())
}

/// Create or replace one (a new spec is a new generation; the run follows it).
pub fn put(state: &Arc<AppState>, name: &str, spec: Spec) -> Result<Record, String> {
    if !valid_name(name) {
        return Err(format!("{name}: a Replication's name is a DNS-1123 label"));
    }
    check(&spec)?;
    let r = with(state, |m| {
        let r = match m.get(name) {
            Some(old) if old.spec == spec => old.clone(),
            Some(old) => Record { spec, generation: old.generation + 1, status: Status { phase: "Pending".into(), ..old.status.clone() }, ..old.clone() },
            None => Record {
                name: name.into(),
                uid: uuid::Uuid::new_v4().to_string(),
                generation: 1,
                created: now(),
                spec,
                status: Status { phase: "Pending".into(), ..Default::default() },
                map: BTreeMap::new(),
            },
        };
        m.insert(name.into(), r.clone());
        r
    });
    save(state);
    start(state.clone(), name.to_string(), r.generation);
    Ok(r)
}

/// Forget one: its run stops; nothing it copied is deleted.
pub fn delete(state: &AppState, name: &str) -> bool {
    let gone = with(state, |m| m.remove(name).is_some());
    if gone {
        save(state);
    }
    gone
}

/// Whether the run of `generation` is still the one asked for.
fn current(state: &AppState, name: &str, generation: u64) -> bool {
    with(state, |m| m.get(name).is_some_and(|r| r.generation == generation))
}

fn set_status(state: &AppState, name: &str, generation: u64, f: impl FnOnce(&mut Status, &mut BTreeMap<String, String>)) -> bool {
    with(state, |m| match m.get_mut(name) {
        Some(r) if r.generation == generation => {
            f(&mut r.status, &mut r.map);
            r.status.observed_generation = generation;
            true
        }
        _ => false,
    })
}

// ------------------------------------------------------------------- run

/// The engine kinds and the listing's `kind` for each.
pub const ENGINE_KINDS: [(&str, &str); 4] = [("goldens", "golden"), ("media", "media"), ("blanks", "blank"), ("volumes", "volume")];

/// A `*` pattern.
pub fn glob(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let mut rest = s;
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            let Some(r) = rest.strip_prefix(p) else { return false };
            rest = r;
        } else if i == parts.len() - 1 {
            return rest.ends_with(p);
        } else {
            match rest.find(p) {
                Some(at) => rest = &rest[at + p.len()..],
                None => return false,
            }
        }
    }
    true
}

/// A peer volume, from its listing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PeerVol {
    pub id: String,
    pub name: String,
    pub namespace: String,
    pub size: u64,
    pub allocated: u64,
    pub sealed: bool,
    pub parent: Option<String>,
    pub kind: String,
    pub lba: u32,
    pub role: Option<String>,
    pub fs: Option<crate::volume::metadata::FsInfo>,
    pub owner: Option<crate::volume::metadata::Owner>,
}

fn fs_of(v: &Value) -> Option<crate::volume::metadata::FsInfo> {
    let k = v.get("kind")?.as_str()?;
    Some(crate::volume::metadata::FsInfo {
        kind: k.to_string(),
        journal: v["journal"].as_bool().unwrap_or(false),
        features: v["features"].as_str().map(str::to_string),
        sixty_four_bit: v["64bit"].as_bool().unwrap_or(false),
        metadata_csum: v["metadata_csum"].as_bool().unwrap_or(false),
        csum_seed: v["metadata_csum_seed"].as_bool().unwrap_or(false),
        label: v["label"].as_str().unwrap_or("").to_string(),
        uuid: v["uuid"].as_str().and_then(|u| u.parse().ok()),
    })
}

/// The peer's listing, as volumes.
pub fn parse_listing(v: &Value) -> Vec<PeerVol> {
    v["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|i| {
                    Some(PeerVol {
                        id: i["id"].as_str()?.to_string(),
                        name: i["name"].as_str()?.to_string(),
                        namespace: i["namespace"].as_str().unwrap_or(crate::volume::metadata::DEFAULT_NAMESPACE).to_string(),
                        size: i["virtual_size_bytes"].as_u64()?,
                        allocated: i["allocated_bytes"].as_u64().unwrap_or(0),
                        sealed: i["sealed"].as_bool().unwrap_or(false),
                        parent: i["parent"].as_str().map(str::to_string),
                        kind: i["kind"].as_str().unwrap_or("volume").to_string(),
                        lba: i["lba"].as_u64().unwrap_or(4096) as u32,
                        role: i["role"].as_str().map(str::to_string),
                        fs: i.get("fs").filter(|f| f.is_object()).and_then(fs_of),
                        owner: i.get("owner").filter(|o| o.is_object()).and_then(|o| serde_json::from_value(o.clone()).ok()),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What a spec selects of a listing, parents ahead of their clones.
pub fn selected(spec: &Spec, all: &[PeerVol]) -> Vec<PeerVol> {
    let kinds: Vec<&str> = if spec.what.is_empty() {
        vec!["golden", "media", "blank"]
    } else {
        ENGINE_KINDS.iter().filter(|(w, _)| spec.what.iter().any(|x| x == w)).map(|(_, k)| *k).collect()
    };
    let s = &spec.select;
    let mut chosen: Vec<PeerVol> = all
        .iter()
        .filter(|v| kinds.contains(&v.kind.as_str()))
        .filter(|v| s.namespaces.is_empty() || s.namespaces.contains(&v.namespace))
        .filter(|v| s.names.is_empty() || s.names.iter().any(|p| glob(p, &v.name)))
        .cloned()
        .collect();
    // Parents first: by depth among what is chosen.
    let ids: HashMap<String, Option<String>> = chosen.iter().map(|v| (v.id.clone(), v.parent.clone())).collect();
    let depth = |v: &PeerVol| {
        let mut d = 0u32;
        let mut p = v.parent.clone();
        let mut seen = HashSet::new();
        while let Some(x) = p {
            if !seen.insert(x.clone()) {
                break;
            }
            match ids.get(&x) {
                Some(next) => {
                    d += 1;
                    p = next.clone();
                }
                None => break,
            }
        }
        d
    };
    chosen.sort_by_key(|v| (depth(v), v.name.clone()));
    chosen
}

/// A peer volume's mapped extents: index -> its first leg's (slab, slot).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extents {
    pub extent_bytes: u64,
    pub at: BTreeMap<u64, (String, u64)>,
}

/// One page of `/legs`, into `e`; the next start, if any.
pub fn take_legs_page(v: &Value, e: &mut Extents) -> Option<u64> {
    if let Some(b) = v["extent_bytes"].as_u64() {
        e.extent_bytes = b;
    }
    for x in v["extents"].as_array().into_iter().flatten() {
        let Some(i) = x["extent"].as_u64() else { continue };
        let leg = &x["legs"][0];
        e.at.insert(i, (leg["slab"].as_str().unwrap_or("").to_string(), leg["slot"].as_u64().unwrap_or(u64::MAX)));
    }
    v["next"].as_u64()
}

/// What a clone must be given over a copy-on-write clone of its parent: the
/// extents it holds of its own, and the ones its parent maps and it does not
/// (zeros). With no parent: everything it maps.
pub fn plan(own: &Extents, parent: Option<&Extents>) -> (Vec<u64>, Vec<u64>) {
    match parent {
        None => (own.at.keys().copied().collect(), vec![]),
        Some(p) => {
            let copy = own.at.iter().filter(|(i, loc)| p.at.get(i) != Some(loc)).map(|(i, _)| *i).collect();
            let zero = p.at.keys().filter(|i| !own.at.contains_key(i)).copied().collect();
            (copy, zero)
        }
    }
}

/// Contiguous extents as byte ranges, none longer than `max`, clipped to `size`.
pub fn ranges(extents: &[u64], extent_bytes: u64, size: u64, max: u64) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &i in extents {
        let from = i * extent_bytes;
        if from >= size {
            continue;
        }
        let to = (from + extent_bytes).min(size);
        match out.last_mut() {
            Some((a, b)) if *b == from && to - *a <= max => *b = to,
            _ => out.push((from, to)),
        }
    }
    out
}

fn client() -> Result<crate::http::Client, String> {
    crate::http::Client::builder().timeout(std::time::Duration::from_secs(120)).build().map_err(|e| e.to_string())
}

async fn get_json(c: &crate::http::Client, url: &str) -> Result<Value, String> {
    let r = c.get(url).send().await.map_err(|e| format!("{url}: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("{url}: HTTP {}", r.status().as_u16()));
    }
    r.json().await.map_err(|e| format!("{url}: {e}"))
}

async fn legs(c: &crate::http::Client, base: &str, id: &str) -> Result<Extents, String> {
    let mut e = Extents::default();
    let mut start = 0u64;
    loop {
        let v = get_json(c, &format!("{base}/api/v1/volumes/{id}/legs?start={start}&limit=65536")).await?;
        match take_legs_page(&v, &mut e) {
            Some(n) if n > start => start = n,
            _ => break,
        }
    }
    if e.extent_bytes == 0 {
        return Err(format!("{base}: volume {id}'s legs name no extent size"));
    }
    Ok(e)
}

/// Every volume here by (namespace, name): (id, sealed, size). Read once a
/// pass, not once a volume (forge holds 2,466).
async fn local_index(state: &AppState) -> HashMap<(String, String), (VolumeId, bool, u64)> {
    let vm = state.volume_manager.lock().await;
    let mut out = HashMap::new();
    for id in vm.volume_ids() {
        if let Some(h) = vm.get_volume_handle(&id) {
            out.insert((vm.namespace(&id).to_string(), h.name().await), (id, h.is_sealed(), h.capacity_bytes()));
        }
    }
    out
}

struct Pace {
    start: std::time::Instant,
    bytes: u64,
    limit: u64,
}

impl Pace {
    async fn took(&mut self, n: u64) {
        self.bytes += n;
        if self.limit == 0 {
            return;
        }
        let want = self.bytes as f64 / (self.limit as f64 * 1048576.0);
        let el = self.start.elapsed().as_secs_f64();
        if want > el {
            tokio::time::sleep(std::time::Duration::from_secs_f64(want - el)).await;
        }
    }
}

/// Copy one peer volume here. The id here, and the bytes moved.
async fn copy_one(
    state: &Arc<AppState>,
    c: &crate::http::Client,
    base: &str,
    v: &PeerVol,
    parent_here: Option<(VolumeId, Extents)>,
    pace: &mut Pace,
    moved: &(dyn Fn(u64) + Sync),
) -> Result<(VolumeId, u64, Extents), String> {
    let mine = legs(c, base, &v.id).await?;
    let (copy, zero) = plan(&mine, parent_here.as_ref().map(|p| &p.1));
    let id = {
        let mut vm = state.volume_manager.lock().await;
        match &parent_here {
            Some((p, _)) => {
                let id = vm.create_snapshot(*p, &v.name).await.map_err(|e| format!("a clone of its parent here: {e}"))?;
                if let Some(h) = vm.get_volume_handle(&id) {
                    if v.size > h.capacity_bytes() {
                        vm.resize_volume(id, v.size).await.map_err(|e| format!("grown to {}: {e}", v.size))?;
                    }
                }
                id
            }
            None => {
                let role = v.role.as_deref().and_then(crate::drive::slab::SlabRole::parse);
                let made = vm.create_volume_with(&v.name, v.size, crate::volume::CreateOptions::default().in_role_opt(role)).await;
                let id = match made {
                    Ok(id) => id,
                    // No slab of that role here: wherever the node has room.
                    Err(_) if role.is_some() => vm.create_volume_with(&v.name, v.size, crate::volume::CreateOptions::default()).await.map_err(|e| e.to_string())?,
                    Err(e) => return Err(e.to_string()),
                };
                if v.lba != 4096 {
                    let _ = vm.set_lba(id, v.lba).await;
                }
                id
            }
        }
    };
    // A copy that did not finish is not left half made.
    let fail = |e: String| async move {
        let _ = state.volume_manager.lock().await.delete_volume(id).await;
        Err::<(VolumeId, u64, Extents), String>(e)
    };
    let dev = match state.volume_manager.lock().await.get_volume(&id) {
        Some(d) => d,
        None => return fail("the new volume is gone".into()).await,
    };
    let mut bytes = 0u64;
    for (a, b) in ranges(&copy, mine.extent_bytes, v.size, MAX_RANGE) {
        let url = format!("{base}/api/v1/volumes/{}/content", v.id);
        let r = match c.get(&url).range(a, b - 1).send().await {
            Ok(r) if r.status().is_success() => r.bytes(),
            Ok(r) => return fail(format!("{url} bytes {a}-{}: HTTP {}", b - 1, r.status().as_u16())).await,
            Err(e) => return fail(format!("{url} bytes {a}-{}: {e}", b - 1)).await,
        };
        if r.len() as u64 != b - a {
            return fail(format!("{url} bytes {a}-{}: {} bytes came (the peer ignored the range?)", b - 1, r.len())).await;
        }
        if let Err(e) = dev.write(a, &r).await {
            return fail(format!("writing {} bytes at {a}: {e}", r.len())).await;
        }
        bytes += r.len() as u64;
        moved(r.len() as u64);
        pace.took(r.len() as u64).await;
    }
    for (a, b) in ranges(&zero, mine.extent_bytes, v.size, MAX_RANGE) {
        if let Err(e) = dev.write(a, &vec![0u8; (b - a) as usize]).await {
            return fail(format!("zeroing {} bytes at {a}: {e}", b - a)).await;
        }
    }
    if let Err(e) = dev.flush().await {
        return fail(format!("flush: {e}")).await;
    }
    if let Err(r) = crate::mgmt::namespaces::settle(state, id, Some(&v.namespace)).await {
        // `settle` has deleted it already.
        return Err(format!("namespace {}: {}", v.namespace, r.message));
    }
    let mut vm = state.volume_manager.lock().await;
    if v.owner.is_some() {
        let _ = vm.set_owner(id, v.owner.clone()).await;
    }
    if v.kind == "blank" {
        vm.mark_template(id);
    }
    if v.sealed {
        if let Err(e) = vm.seal_volume(id, v.fs.clone()).await {
            drop(vm);
            return fail(format!("seal: {e}")).await;
        }
    } else {
        vm.persist().await;
    }
    Ok((id, bytes, mine))
}

/// One pass over the peer.
async fn pass(state: &Arc<AppState>, name: &str, generation: u64, spec: &Spec) -> Result<(), String> {
    let base = spec.from.engine.clone().unwrap_or_default().trim_end_matches('/').to_string();
    let c = client()?;
    set_status(state, name, generation, |s, _| {
        s.phase = "Listing".into();
        s.started = Some(now());
        s.errors.clear();
        s.notes.clear();
        if spec.what.iter().any(|w| w == "images") {
            s.notes.push("images: the registry's references are stage 2 of #656, not synced yet".into());
        }
        if spec.from.credential_secret.is_some() || spec.from.ca_secret.is_some() {
            s.notes.push("credentialSecret / caSecret: stage 2 of #656; the peer is reached as an open one".into());
        }
    });
    let mut kinds: Vec<&str> = ENGINE_KINDS.iter().filter(|(w, _)| spec.what.iter().any(|x| x == w)).map(|(_, k)| *k).collect();
    if spec.what.is_empty() {
        kinds = vec!["golden", "media", "blank"];
    }
    if kinds.is_empty() {
        set_status(state, name, generation, |s, _| s.phase = "Synced".into());
        return Ok(());
    }
    let listing = get_json(&c, &format!("{base}/api/v1/volumes?kind={}", kinds.join(","))).await?;
    let all = parse_listing(&listing);
    let chosen = selected(spec, &all);
    let total_bytes: u64 = chosen.iter().map(|v| v.allocated).sum();
    tracing::info!("replication {name}: {} volume(s) of {} at {base} selected, {} allocated (#656)", chosen.len(), all.len(), crate::mgmt::config::human_size(total_bytes));
    set_status(state, name, generation, |s, _| {
        s.phase = "Syncing".into();
        s.items = Items { total: chosen.len() as u64, ..Default::default() };
        s.bytes = Bytes { total: total_bytes, done: 0 };
    });
    let started = std::time::Instant::now();
    let mut pace = Pace { start: started, bytes: 0, limit: spec.bandwidth_mi_bps };
    let done_bytes = std::sync::atomic::AtomicU64::new(0);
    // Each peer volume's extents, kept for its clones.
    let mut extents: HashMap<String, Extents> = HashMap::new();
    let mut index = local_index(state).await;
    for v in &chosen {
        if !current(state, name, generation) {
            return Ok(());
        }
        let reference = format!("{}/{}", v.namespace, v.name);
        let here = with(state, |m| m.get(name).and_then(|r| r.map.get(&v.id).cloned()));
        let here_ok = match &here {
            Some(l) => {
                let id = l.parse().ok().map(VolumeId);
                match id {
                    Some(id) => state.volume_manager.lock().await.get_volume_handle(&id).is_some(),
                    None => false,
                }
            }
            None => false,
        };
        let mut outcome: Result<(), String> = Ok(());
        if !here_ok {
            set_status(state, name, generation, |s, _| s.current = Some(reference.clone()));
            match index.get(&(v.namespace.clone(), v.name.clone())).copied() {
                Some((id, true, size)) if size == v.size => {
                    set_status(state, name, generation, |_, m| {
                        m.insert(v.id.clone(), id.0.to_string());
                    });
                }
                Some((id, _, _)) => outcome = Err(format!("a different volume of this name is here ({}): not overwritten", id.0)),
                None if !v.sealed => outcome = Err("takes writes on the peer: the peer serves no content of a writable volume (stage 2 of #656)".into()),
                None => {
                    // Its parent here, when the parent was replicated.
                    let parent_here = match &v.parent {
                        Some(p) => {
                            let local = with(state, |m| m.get(name).and_then(|r| r.map.get(p).cloned())).and_then(|l| l.parse().ok()).map(VolumeId);
                            match local {
                                Some(l) => {
                                    let ex = match extents.get(p) {
                                        Some(e) => Some(e.clone()),
                                        None => legs(&c, &base, p).await.ok(),
                                    };
                                    ex.map(|e| (l, e))
                                }
                                None => None,
                            }
                        }
                        None => None,
                    };
                    let moved = |n: u64| {
                        done_bytes.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                        metrics::counter!("storm_replication_bytes_total", "replication" => name.to_string()).increment(n);
                    };
                    match copy_one(state, &c, &base, v, parent_here, &mut pace, &moved).await {
                        Ok((id, _, mine)) => {
                            extents.insert(v.id.clone(), mine);
                            index.insert((v.namespace.clone(), v.name.clone()), (id, v.sealed, v.size));
                            set_status(state, name, generation, |_, m| {
                                m.insert(v.id.clone(), id.0.to_string());
                            });
                        }
                        Err(e) => outcome = Err(e),
                    }
                }
            }
        }
        let db = done_bytes.load(std::sync::atomic::Ordering::Relaxed);
        let el = started.elapsed().as_secs_f64().max(0.001);
        let result = if outcome.is_ok() { "done" } else { "failed" };
        metrics::counter!("storm_replication_items_total", "replication" => name.to_string(), "result" => result).increment(1);
        set_status(state, name, generation, |s, _| {
            match &outcome {
                Ok(()) => s.items.done += 1,
                Err(e) => {
                    s.items.failed += 1;
                    s.errors.push(ItemError { reference: reference.clone(), reason: e.clone() });
                }
            }
            s.bytes.done = db;
            s.rate_mi_bps = db as f64 / el / 1048576.0;
            let left = s.bytes.total.saturating_sub(db);
            s.eta_secs = (db > 0).then(|| (left as f64 / (db as f64 / el)) as u64);
        });
        if let Err(e) = &outcome {
            tracing::warn!("replication {name}: {reference}: {e} (#656)");
        }
        // Kept as it goes: a restart does not copy again what arrived.
        save(state);
    }
    let t = now();
    set_status(state, name, generation, |s, _| {
        s.current = None;
        let ok = s.items.failed == 0;
        s.phase = if ok { "Synced" } else { "Failed" }.into();
        if ok {
            s.last_synced = Some(t);
            s.eta_secs = Some(0);
        }
        let status = if ok { "True" } else { "False" };
        let changed = s.conditions.first().is_none_or(|c| c.status != status);
        let since = if changed { t } else { s.conditions[0].last_transition_time };
        s.conditions = vec![Condition {
            kind: "Synced".into(),
            status: status.into(),
            reason: if ok { "AllItemsSynced" } else { "ItemsFailed" }.into(),
            message: format!("{} of {} item(s) here, {} failed", s.items.done, s.items.total, s.items.failed),
            last_transition_time: since,
        }];
    });
    save(state);
    let (done, total, failed) = with(state, |m| m.get(name).map(|r| (r.status.items.done, r.status.items.total, r.status.items.failed)).unwrap_or_default());
    tracing::info!("replication {name}: a pass ended: {done} of {total} here, {failed} failed (#656)");
    Ok(())
}

/// Run a replication of `generation` until it is done (once) or replaced or
/// deleted (mirror).
pub fn start(state: Arc<AppState>, name: String, generation: u64) {
    tokio::spawn(async move {
        loop {
            let Some(spec) = with(&state, |m| m.get(&name).filter(|r| r.generation == generation).map(|r| r.spec.clone())) else { return };
            if let Err(e) = pass(&state, &name, generation, &spec).await {
                tracing::warn!("replication {name}: {e} (#656)");
                set_status(&state, &name, generation, |s, _| {
                    s.phase = "Failed".into();
                    s.errors.push(ItemError { reference: spec.from.engine.clone().unwrap_or_default(), reason: e.clone() });
                });
                save(&state);
            }
            if spec.mode != "mirror" {
                return;
            }
            let lag_from = with(&state, |m| m.get(&name).and_then(|r| r.status.last_synced));
            let _ = set_status(&state, &name, generation, |s, _| {
                if s.phase == "Synced" {
                    s.phase = "Waiting".into();
                }
            });
            let mut waited = 0;
            while waited < spec.every {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                waited += 10;
                if !current(&state, &name, generation) {
                    return;
                }
                if let Some(l) = lag_from {
                    metrics::gauge!("storm_replication_lag_seconds", "replication" => name.clone()).set(now().saturating_sub(l) as f64);
                }
            }
        }
    });
}

/// At the engine's start: every replication not finished is run again
/// (what arrived is not copied again), and every mirror resumes.
pub fn resume(state: Arc<AppState>) {
    for r in list(&state) {
        if r.spec.mode == "mirror" || r.status.phase != "Synced" {
            tracing::info!("replication {}: resumed ({}, generation {}) (#656)", r.name, r.spec.mode, r.generation);
            start(state.clone(), r.name.clone(), r.generation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn globs() {
        assert!(glob("golden-*", "golden-stormpanel-25da1f262ed5"));
        assert!(!glob("golden-*", "media-x"));
        assert!(glob("*-x86_64", "fedora-43-x86_64"));
        assert!(glob("a*b*c", "aXXbYYc") && !glob("a*b*c", "aXXcYYb"));
        assert!(glob("exact", "exact") && !glob("exact", "exactly"));
        assert!(glob("*", "anything"));
    }

    fn pv(id: &str, name: &str, kind: &str, parent: Option<&str>) -> PeerVol {
        PeerVol { id: id.into(), name: name.into(), namespace: "stormcos".into(), kind: kind.into(), parent: parent.map(str::to_string), sealed: true, size: 1 << 20, ..Default::default() }
    }

    #[test]
    fn selection_and_parents_first() {
        let all = vec![pv("c2", "golden-b-2", "golden", Some("c1")), pv("c1", "golden-b-1", "golden", Some("p")), pv("p", "golden-a", "golden", None), pv("v", "data", "volume", None), pv("m", "iso", "media", None)];
        let spec = Spec { from: Peer::default(), what: vec![], select: Select::default(), mode: "once".into(), every: 600, bandwidth_mi_bps: 0 };
        let ids: Vec<String> = selected(&spec, &all).into_iter().map(|v| v.id).collect();
        assert_eq!(ids, vec!["p", "m", "c1", "c2"], "the sealed kinds by default, parents before clones, by name at a depth");
        let spec = Spec { what: vec!["volumes".into()], ..spec.clone() };
        assert_eq!(selected(&spec, &all).len(), 1);
        let spec = Spec { what: vec![], select: Select { names: vec!["golden-b-*".into()], ..Default::default() }, ..spec };
        let ids: Vec<String> = selected(&spec, &all).into_iter().map(|v| v.id).collect();
        assert_eq!(ids, vec!["c1", "c2"]);
    }

    /// forge's listing and legs, as 24.2.2 answers them.
    #[test]
    fn listing_and_legs_read() {
        let l = json!({"items": [{"id": "7c0ca6cb-49ae-4678-9f5b-984654e60dfb", "name": "golden-stormpanel-25da1f262ed5", "namespace": "stormcos",
            "virtual_size_bytes": 16777216, "allocated_bytes": 5242880, "sealed": true, "kind": "golden", "lba": 4096, "role": "system",
            "fs": {"kind": "ext4", "journal": false, "features": null, "64bit": true, "metadata_csum": true, "metadata_csum_seed": true, "label": "golden", "uuid": "9cf7a70a-d02e-4095-b0a2-cf5e35b4ab87"},
            "owner": {"kind": "Pod", "namespace": "ns", "name": "p", "uid": null}}]});
        let v = &parse_listing(&l)[0];
        assert_eq!((v.size, v.allocated, v.sealed, v.lba), (16777216, 5242880, true, 4096));
        let fs = v.fs.as_ref().unwrap();
        assert!(fs.kind == "ext4" && fs.sixty_four_bit && fs.csum_seed && fs.uuid.is_some());
        assert_eq!(v.owner.as_ref().unwrap().kind, "Pod");
        let page = json!({"volume": "x", "extent_bytes": 1048576, "extents": [
            {"extent": 0, "offset": 0, "shared": false, "legs": [{"slab": "s1", "slot": 28290}]},
            {"extent": 1, "offset": 1048576, "shared": false, "legs": [{"slab": "s1", "slot": 28291}]}], "next": 2});
        let mut e = Extents::default();
        assert_eq!(take_legs_page(&page, &mut e), Some(2));
        assert_eq!(e.extent_bytes, 1048576);
        assert_eq!(e.at[&1], ("s1".to_string(), 28291));
    }

    #[test]
    fn a_clone_moves_only_what_it_holds() {
        let ex = |v: &[(u64, &str, u64)]| Extents { extent_bytes: 1 << 20, at: v.iter().map(|(i, s, n)| (*i, (s.to_string(), *n))).collect() };
        let parent = ex(&[(0, "a", 1), (1, "a", 2), (2, "a", 3)]);
        let clone = ex(&[(0, "a", 1), (1, "b", 9), (5, "b", 10)]);
        assert_eq!(plan(&clone, Some(&parent)), (vec![1, 5], vec![2]), "own: 1 rewritten, 5 new; 2 unmapped in the clone");
        assert_eq!(plan(&clone, None), (vec![0, 1, 5], vec![]));
        assert_eq!(ranges(&[0, 1, 2, 5, 6], 1 << 20, 100 << 20, 2 << 20), vec![(0, 2 << 20), (2 << 20, 3 << 20), (5 << 20, 7 << 20)]);
        assert_eq!(ranges(&[3], 1 << 20, (3 << 20) + 4096, 8 << 20), vec![(3 << 20, (3 << 20) + 4096)], "clipped to the size");
    }

    /// The proof's shape: `"phase":"Synced"` ahead of `"items":{"total":N,"done":N`.
    #[test]
    fn the_object_keeps_its_order() {
        let r = Record {
            name: "forge".into(),
            uid: "u".into(),
            generation: 1,
            created: 1,
            spec: Spec { from: Peer { engine: Some("http://forge.g16.lo:9090".into()), ..Default::default() }, what: vec![], select: Select::default(), mode: "once".into(), every: 600, bandwidth_mi_bps: 0 },
            status: Status { phase: "Synced".into(), items: Items { total: 3, done: 3, failed: 0 }, ..Default::default() },
            map: BTreeMap::new(),
        };
        let s = serde_json::to_string(&r.object()).unwrap();
        let re = regex_lite(&s);
        assert!(re, "{s}");
        assert!(s.contains(r#""from":{"engine":"http://forge.g16.lo:9090"}"#), "{s}");
        assert!(check(&r.spec).is_ok());
        assert!(check(&Spec { mode: "sometimes".into(), ..r.spec.clone() }).is_err());
        assert!(check(&Spec { from: Peer::default(), ..r.spec.clone() }).is_err(), "an engine kind needs the engine");
        assert!(check(&Spec { from: Peer::default(), what: vec!["images".into()], ..r.spec.clone() }).is_ok());
    }

    fn regex_lite(s: &str) -> bool {
        let Some(p) = s.find(r#""phase":"Synced""#) else { return false };
        let rest = &s[p..];
        let Some(i) = rest.find(r#""items":{"total":"#) else { return false };
        let after = &rest[i + r#""items":{"total":"#.len()..];
        let n: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        after[n.len()..].starts_with(&format!(r#","done":{n}"#))
    }
}
