//! stormdist (#442): this engine's side of the release and backup ISO
//! (glennswest/stormdist, format 2).
//!
//! - **Export.** A sealed golden as stormdist's `make` takes it
//!   ([`export`]): its name, component, commit, size, the sha256 of its
//!   every byte and its allocated ranges, sorted and merged, from the extent
//!   map (nothing read). The bytes are the sealed volume's Range `content`;
//!   sealed, they cannot change under a reader, so every range read is of
//!   one state.
//! - **Forge inventory.** `<data-dir>/stormdist.json`: the retention roots
//!   (goldens forge keeps under retention) and the exact signed release
//!   catalogues (`RELEASE.toml` and its signature) this forge was handed. A
//!   backup ISO carries both as its `FORGE.toml`.
//! - **Restore.** [`Restore`] is `stormdist::media::Engine` over the volume
//!   manager. Each golden is written into an unsealed staging volume
//!   (`stormdist-staging-<txn>-<name>`); at commit each is flushed, sealed
//!   and digested, and its digest must equal the manifest's
//!   `device_sha256`; then every one is renamed to its golden's name by one
//!   persist ([`VolumeManager::publish_sealed`]), and only then are the
//!   catalogues and roots recorded. Any failure deletes the staging volumes.
//!   A golden already here, sealed, with the same digest, is kept and not
//!   written again, so a repeated restore is a no-op; one here with another
//!   digest refuses the restore.
//!
//! [`VolumeManager::publish_sealed`]: crate::volume::VolumeManager::publish_sealed

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, bail, ensure, Context};
use serde::{Deserialize, Serialize};
use stormdist::format::{Golden, Range, Release};
use stormdist::media::{ArchivedRelease, Engine, Forge, Media};

use crate::drive::BlockDevice;
use crate::mgmt::AppState;
use crate::volume::VolumeId;

pub const INVENTORY_FILE: &str = "stormdist.json";
/// What a staging volume's name starts with; one left by a restore cut short
/// is deleted when the next one begins.
pub const STAGING_PREFIX: &str = "stormdist-staging-";

/// A sealed golden as stormdist's recipe takes it (`media::Input` without
/// its `image`), with where its bytes are read.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GoldenExport {
    pub name: String,
    pub component: String,
    pub commit: String,
    /// Where `component` and `commit` came from: `record` (goldens.json's
    /// provenance) or `name` (`golden-<component>-<commit>`).
    pub provenance_from: String,
    pub size: u64,
    pub device_sha256: String,
    pub ranges: Vec<Range>,
    pub allocated_bytes: u64,
    pub extent_size_bytes: u64,
    pub volume_id: uuid::Uuid,
    /// `GET` it with a `Range` header: the sealed volume's bytes.
    pub content: String,
}

/// The forge's half of a backup: retention roots and signed catalogues.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Inventory {
    #[serde(default)]
    pub roots: BTreeSet<String>,
    #[serde(default)]
    pub releases: Vec<Catalogue>,
}

/// A signed release catalogue, exactly as signed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Catalogue {
    /// `RELEASE.toml`'s bytes.
    pub release: String,
    pub signature_hex: String,
}

impl From<&ArchivedRelease> for Catalogue {
    fn from(a: &ArchivedRelease) -> Self {
        Catalogue { release: a.release.clone(), signature_hex: a.signature_hex.clone() }
    }
}

impl Inventory {
    /// stormdist's `Forge` for a backup recipe: the roots that are still
    /// goldens here, and every catalogue.
    pub fn forge(&self) -> Forge {
        Forge {
            roots: self.roots.iter().cloned().collect(),
            releases: self
                .releases
                .iter()
                .map(|c| ArchivedRelease { release: c.release.clone(), signature_hex: c.signature_hex.clone() })
                .collect(),
        }
    }

    /// Add roots and catalogues; a catalogue already kept (the same bytes
    /// and signature) is not added twice. Answers whether anything changed.
    pub fn merge(&mut self, roots: impl IntoIterator<Item = String>, releases: impl IntoIterator<Item = Catalogue>) -> bool {
        let before = self.clone();
        self.roots.extend(roots);
        for c in releases {
            if !self.releases.contains(&c) {
                self.releases.push(c);
            }
        }
        *self != before
    }
}

fn inventory_path(state: &AppState) -> Option<PathBuf> {
    state.config.management.data_dir.as_ref().map(|d| PathBuf::from(d).join(INVENTORY_FILE))
}

fn inventory_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// The kept inventory; empty when there is none. One that does not parse is
/// an error, never an empty inventory: a backup must not quietly lose it.
pub fn load_inventory(state: &AppState) -> anyhow::Result<Inventory> {
    let Some(p) = inventory_path(state) else { return Ok(Inventory::default()) };
    match std::fs::read(&p) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| format!("{} does not parse", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Inventory::default()),
        Err(e) => Err(anyhow!("reading {}: {e}", p.display())),
    }
}

/// Merge into the kept inventory and write it (atomically).
pub fn record_inventory(
    state: &AppState,
    roots: impl IntoIterator<Item = String>,
    releases: impl IntoIterator<Item = Catalogue>,
) -> anyhow::Result<Inventory> {
    let _g = inventory_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut inv = load_inventory(state)?;
    if inv.merge(roots, releases) {
        let p = inventory_path(state).context("this node keeps no data directory")?;
        crate::serve::wiring::write_atomic(&p, &serde_json::to_vec_pretty(&inv)?)?;
    }
    Ok(inv)
}

/// Replace the retention roots.
pub fn set_roots(state: &AppState, roots: BTreeSet<String>) -> anyhow::Result<Inventory> {
    let _g = inventory_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut inv = load_inventory(state)?;
    inv.roots = roots;
    let p = inventory_path(state).context("this node keeps no data directory")?;
    crate::serve::wiring::write_atomic(&p, &serde_json::to_vec_pretty(&inv)?)?;
    Ok(inv)
}

/// A catalogue is kept only if it parses as a stormdist release and its
/// signature is 64 bytes of hex; with a key, the signature is checked too.
pub fn check_catalogue(c: &Catalogue, key: Option<&ed25519_dalek::VerifyingKey>) -> anyhow::Result<Release> {
    let sig = hex::decode(&c.signature_hex).context("signature_hex is not hex")?;
    ensure!(sig.len() == 64, "an Ed25519 signature is 64 bytes, not {}", sig.len());
    let release: Release = toml_release(&c.release)?;
    release.validate()?;
    if let Some(k) = key {
        stormdist::format::verify_signature(c.release.as_bytes(), &sig, k).context("the signature does not verify with the given key")?;
    }
    Ok(release)
}

fn toml_release(text: &str) -> anyhow::Result<Release> {
    toml::from_str(text).map_err(|e| anyhow!("not a stormdist RELEASE.toml: {e}"))
}

/// `golden-<component>-<commit>`: the commit a hex run of 7 or more.
fn from_name(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix("golden-")?;
    let (component, commit) = rest.rsplit_once('-')?;
    (!component.is_empty() && commit.len() >= 7 && commit.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| (component.to_string(), commit.to_string()))
}

/// Export a sealed golden (by name or id). `Err((status, why))`.
pub async fn export(state: &Arc<AppState>, id: VolumeId) -> Result<GoldenExport, (u16, String)> {
    let (h, dev) = {
        let vm = state.volume_manager.lock().await;
        match (vm.get_volume_handle(&id), vm.get_volume(&id)) {
            (Some(h), Some(d)) => (h, d),
            _ => return Err((404, format!("no volume {}", id.0))),
        }
    };
    if !h.is_sealed() {
        return Err((409, format!("volume {} is not sealed: its ranges and bytes could change under a reader", id.0)));
    }
    if state.volume_manager.lock().await.is_secret(&id) {
        return Err((409, format!("volume {} is a secret volume: it is never copied off this engine", id.0)));
    }
    let name = h.name().await;
    let record = crate::mgmt::goldens::load(state).into_iter().find(|r| r.volume_id == id.0);
    let recorded = record.as_ref().and_then(|r| {
        let commit = r.provenance.get("commit")?.clone();
        let component = r.provenance.get("component").cloned().or_else(|| from_name(&name).map(|(c, _)| c))?;
        Some((component, commit))
    });
    let (component, commit, provenance_from) = match (recorded, from_name(&name)) {
        (Some((c, m)), _) => (c, m, "record"),
        (None, Some((c, m))) => (c, m, "name"),
        (None, None) => {
            return Err((
                409,
                format!("{name}: no commit is recorded for it (goldens.json provenance `commit`) and its name is not golden-<component>-<commit>"),
            ))
        }
    };
    let size = dev.capacity_bytes();
    let device_sha256 = match h.cached_digest() {
        Some((sha, bytes)) if bytes == size => sha,
        _ => {
            let epoch = h.seal_epoch();
            let sha = crate::mgmt::goldens::digest(dev.clone()).await.map_err(|e| (500, format!("reading {name}: {e}")))?;
            h.cache_digest(sha.clone(), size, epoch);
            sha
        }
    };
    let (extent, vsize, changed) =
        state.volume_manager.lock().await.changes(id, None).await.map_err(|e| (409, e.to_string()))?;
    let ranges: Vec<Range> = crate::volume::snapshot::changed_ranges(&changed, extent, vsize)
        .into_iter()
        .map(|r| Range { offset: r.offset, length: r.length })
        .collect();
    Ok(GoldenExport {
        allocated_bytes: ranges.iter().map(|r| r.length).sum(),
        name,
        component,
        commit,
        provenance_from: provenance_from.into(),
        size,
        device_sha256,
        ranges,
        extent_size_bytes: extent,
        volume_id: id.0,
        content: format!("/api/v1/volumes/{}/content", id.0),
    })
}

/// One golden of a restore.
struct Staged {
    golden: Golden,
    /// `None`: the golden is already here, sealed, with this digest.
    vol: Option<(VolumeId, Arc<dyn BlockDevice>)>,
}

/// `stormdist::media::Engine` over this engine's volume manager. Called from
/// a blocking thread (stormdist's reader is synchronous); each call runs its
/// async half on `rt`.
pub struct Restore {
    state: Arc<AppState>,
    rt: tokio::runtime::Handle,
    txn: String,
    /// The signed catalogue of the media being restored, kept at commit.
    catalogue: Option<Catalogue>,
    /// Where the media came from, for the golden records.
    from: String,
    forge: Option<Forge>,
    version: String,
    staged: Vec<Staged>,
    /// Golden names in order of `create`.
    index: HashMap<String, usize>,
    /// What the restore did, golden by golden.
    pub report: Arc<Mutex<Report>>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub written: Vec<String>,
    pub kept: Vec<String>,
    pub bytes_written: u64,
    pub steps: Vec<String>,
}

impl Restore {
    pub fn new(state: Arc<AppState>, rt: tokio::runtime::Handle, catalogue: Option<Catalogue>, from: String) -> Self {
        Restore {
            state,
            rt,
            txn: uuid::Uuid::new_v4().simple().to_string()[..8].to_string(),
            catalogue,
            from,
            forge: None,
            version: String::new(),
            staged: Vec::new(),
            index: HashMap::new(),
            report: Arc::new(Mutex::new(Report::default())),
        }
    }

    fn staging_name(&self, golden: &str) -> String {
        format!("{STAGING_PREFIX}{}-{golden}", self.txn)
    }

    async fn delete_staged(state: &AppState, ids: Vec<VolumeId>) {
        let mut vm = state.volume_manager.lock().await;
        for id in ids {
            if let Err(e) = vm.delete_volume(id).await {
                tracing::error!("stormdist restore: deleting staging volume {}: {e}", id.0);
            }
        }
    }

    /// Staging volumes a restore cut short left behind.
    async fn leftovers(state: &AppState) -> Vec<VolumeId> {
        let vm = state.volume_manager.lock().await;
        let mut out = Vec::new();
        for id in vm.volume_ids() {
            if let Some(h) = vm.get_volume_handle(&id) {
                if h.name().await.starts_with(STAGING_PREFIX) {
                    out.push(id);
                }
            }
        }
        out
    }

    async fn write_at(dev: &Arc<dyn BlockDevice>, off: u64, bytes: &[u8]) -> anyhow::Result<()> {
        let bs = dev.block_size().max(1) as u64;
        let end = off.checked_add(bytes.len() as u64).context("write overflow")?;
        ensure!(end <= dev.capacity_bytes(), "a write to {off}..{end} is past the volume's end");
        let (start, stop) = (off / bs * bs, end.div_ceil(bs) * bs);
        let mut buf = crate::drive::dma::DmaBuf::zeroed((stop - start) as usize);
        if start != off || stop != end {
            dev.read(start, &mut buf).await.map_err(|e| anyhow!("reading {start} to merge an unaligned write: {e}"))?;
        }
        buf[(off - start) as usize..(end - start) as usize].copy_from_slice(bytes);
        dev.write(start, &buf).await.map_err(|e| anyhow!("writing at {start}: {e}"))?;
        Ok(())
    }

    async fn commit_async(&mut self) -> anyhow::Result<()> {
        let state = self.state.clone();
        // 1. Flush, seal and digest each staged golden, under its staging name.
        let mut publish = Vec::new();
        for s in &self.staged {
            let Some((id, dev)) = &s.vol else { continue };
            dev.flush().await.map_err(|e| anyhow!("{}: flush: {e}", s.golden.name))?;
            state.volume_manager.lock().await.seal_volume(*id, None).await.map_err(|e| anyhow!("{}: seal: {e}", s.golden.name))?;
            let sha = crate::mgmt::goldens::digest(dev.clone()).await.map_err(|e| anyhow!("{}: digest: {e}", s.golden.name))?;
            ensure!(
                sha == s.golden.device_sha256,
                "{}: the engine's copy hashes to {sha}, the manifest says {}: not published",
                s.golden.name,
                s.golden.device_sha256
            );
            if let Some(h) = state.volume_manager.lock().await.get_volume_handle(id) {
                let epoch = h.seal_epoch();
                h.cache_digest(sha, dev.capacity_bytes(), epoch);
            }
            publish.push((*id, s.golden.name.clone()));
        }
        // 2. Every golden under its own name, by one persist.
        if !publish.is_empty() {
            state.volume_manager.lock().await.publish_sealed(&publish).await.map_err(|e| anyhow!("publishing: {e}"))?;
        }
        // 3. Their records, then the forge's roots and catalogues.
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        for s in &self.staged {
            let Some((id, _)) = &s.vol else { continue };
            let mut provenance = BTreeMap::new();
            provenance.insert("commit".to_string(), s.golden.commit.clone());
            provenance.insert("component".to_string(), s.golden.component.clone());
            provenance.insert("release".to_string(), self.version.clone());
            let r = crate::mgmt::goldens::GoldenRecord {
                name: s.golden.name.clone(),
                volume_id: id.0,
                sha256: s.golden.device_sha256.clone(),
                size_bytes: s.golden.size,
                made_at: now,
                how: "restored".into(),
                source: None,
                parent: None,
                provenance,
                made_by: None,
                from: Some(self.from.clone()),
                namespace: None,
            };
            if let Err(e) = crate::mgmt::goldens::record(&state, r).await {
                // The golden is published and its digest checked; a record
                // missing only loses the export's `record` provenance.
                tracing::warn!("stormdist restore: {} published, its golden record not kept: {e}", s.golden.name);
            }
        }
        let (roots, mut releases): (Vec<String>, Vec<Catalogue>) = match &self.forge {
            Some(f) => (f.roots.clone(), f.releases.iter().map(Catalogue::from).collect()),
            None => (Vec::new(), Vec::new()),
        };
        releases.extend(self.catalogue.clone());
        if state.config.management.data_dir.is_some() {
            record_inventory(&state, roots, releases)?;
        }
        Ok(())
    }
}

impl Engine for Restore {
    fn begin(&mut self, release: &Release, forge: Option<&Forge>) -> anyhow::Result<()> {
        self.version = release.version.clone();
        self.forge = forge.cloned();
        let state = self.state.clone();
        let left = self.rt.block_on(Self::leftovers(&state));
        if !left.is_empty() {
            tracing::warn!("stormdist restore: {} staging volumes a restore cut short left; deleted", left.len());
            self.rt.block_on(Self::delete_staged(&state, left));
        }
        tracing::info!("stormdist restore {}: release {} from {}", self.txn, release.version, self.from);
        Ok(())
    }

    fn create(&mut self, golden: &Golden) -> anyhow::Result<()> {
        golden.validate()?;
        ensure!(!self.index.contains_key(&golden.name), "{} named twice", golden.name);
        let state = self.state.clone();
        let staging = self.staging_name(&golden.name);
        let vol = self.rt.block_on(async {
            let existing = state.volume_manager.lock().await.find_volume(&golden.name).await;
            if let Some(id) = existing {
                let (h, dev) = {
                    let vm = state.volume_manager.lock().await;
                    (vm.get_volume_handle(&id).context("vanished")?, vm.get_volume(&id).context("vanished")?)
                };
                ensure!(h.is_sealed(), "{} is here and not sealed: not restored over", golden.name);
                let size = dev.capacity_bytes();
                let sha = match h.cached_digest() {
                    Some((sha, b)) if b == size => sha,
                    _ => crate::mgmt::goldens::digest(dev).await.map_err(|e| anyhow!("{}: {e}", golden.name))?,
                };
                ensure!(
                    size == golden.size && sha == golden.device_sha256,
                    "{} is here with another content ({size} bytes, {sha}) than the media's ({} bytes, {}): not restored over",
                    golden.name,
                    golden.size,
                    golden.device_sha256
                );
                return Ok::<_, anyhow::Error>(None);
            }
            let mut vm = state.volume_manager.lock().await;
            let id = vm.create_volume_any(&staging, golden.size).await.map_err(|e| anyhow!("{}: create: {e}", golden.name))?;
            let dev = vm.get_volume(&id).context("the staging volume vanished")?;
            if dev.capacity_bytes() != golden.size {
                let cap = dev.capacity_bytes();
                drop(vm);
                Self::delete_staged(&state, vec![id]).await;
                bail!("{}: the engine made {cap} bytes for {}: a golden's size must be whole blocks", golden.name, golden.size);
            }
            Ok(Some((id, dev)))
        })?;
        {
            let mut r = self.report.lock().unwrap_or_else(|e| e.into_inner());
            if vol.is_some() { r.written.push(golden.name.clone()) } else { r.kept.push(golden.name.clone()) }
        }
        self.index.insert(golden.name.clone(), self.staged.len());
        self.staged.push(Staged { golden: golden.clone(), vol });
        Ok(())
    }

    fn write(&mut self, golden: &str, offset: u64, bytes: &[u8]) -> anyhow::Result<()> {
        let i = *self.index.get(golden).with_context(|| format!("{golden} was not created"))?;
        let Some((_, dev)) = &self.staged[i].vol else { return Ok(()) };
        self.rt.block_on(Self::write_at(dev, offset, bytes)).with_context(|| format!("{golden} at {offset}"))?;
        self.report.lock().unwrap_or_else(|e| e.into_inner()).bytes_written += bytes.len() as u64;
        Ok(())
    }

    fn record(&mut self, release: &Release) -> anyhow::Result<()> {
        self.report.lock().unwrap_or_else(|e| e.into_inner()).steps.push(release.version.clone());
        Ok(())
    }

    fn commit(&mut self) -> anyhow::Result<()> {
        let rt = self.rt.clone();
        rt.block_on(self.commit_async())
    }

    fn abort(&mut self) {
        let ids: Vec<VolumeId> = self.staged.iter().filter_map(|s| s.vol.as_ref().map(|(id, _)| *id)).collect();
        tracing::error!("stormdist restore {}: aborted; {} staging volumes deleted", self.txn, ids.len());
        let state = self.state.clone();
        self.rt.block_on(Self::delete_staged(&state, ids));
        self.staged.clear();
        self.index.clear();
    }
}

/// What a restore job is asked.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RestoreRequest {
    /// The ISO on this node: a file or a block device (`/dev/sr0`).
    pub iso: String,
    /// For a diff ISO: its bases, oldest (the full ISO) first.
    #[serde(default)]
    pub base: Vec<String>,
    /// The pinned Ed25519 public key, 64 hex digits. Never one from the media.
    pub key: String,
    /// `restore` (every golden and the forge inventory; a backup),
    /// `install` (a full release ISO's `flavour`), `upgrade` (diffs from
    /// `current`, the installed release's identity, to `flavour`).
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub flavour: Option<String>,
    #[serde(default)]
    pub current: Option<String>,
}

fn default_mode() -> String {
    "restore".into()
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: String,
    pub request: RestoreRequest,
    /// `running`, `done`, `failed`.
    pub state: String,
    pub started_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    pub report: Report,
}

fn jobs() -> &'static Mutex<BTreeMap<String, Job>> {
    static J: OnceLock<Mutex<BTreeMap<String, Job>>> = OnceLock::new();
    J.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// One restore at a time on an engine: a second waits for the first.
fn restore_gate() -> &'static Mutex<()> {
    static G: OnceLock<Mutex<()>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(()))
}

pub fn parse_key(hex_key: &str) -> anyhow::Result<ed25519_dalek::VerifyingKey> {
    let b = hex::decode(hex_key.trim()).context("key is not hex")?;
    let b: [u8; 32] = b.try_into().map_err(|_| anyhow!("an Ed25519 public key is 32 bytes"))?;
    Ok(ed25519_dalek::VerifyingKey::from_bytes(&b)?)
}

/// Open the media: each base in order, then the ISO on the last.
pub fn open_media(req: &RestoreRequest) -> anyhow::Result<Media> {
    let key = parse_key(&req.key)?;
    let mut base: Option<Media> = None;
    for p in &req.base {
        base = Some(Media::open(Path::new(p), &key, base).with_context(|| format!("base {p}"))?);
    }
    Media::open(Path::new(&req.iso), &key, base).with_context(|| format!("media {}", req.iso))
}

/// Run a restore to its end on this (blocking) thread.
pub fn run_restore(state: Arc<AppState>, rt: tokio::runtime::Handle, req: &RestoreRequest, report: Option<Arc<Mutex<Report>>>) -> anyhow::Result<String> {
    let _g = restore_gate().lock().unwrap_or_else(|e| e.into_inner());
    let media = open_media(req)?;
    let (rel, sig) = media.signed_catalogue()?;
    let catalogue = Catalogue { release: String::from_utf8(rel.to_vec())?, signature_hex: hex::encode(sig) };
    let mut engine = Restore::new(state, rt, Some(catalogue), req.iso.clone());
    if let Some(r) = report {
        engine.report = r;
    }
    match req.mode.as_str() {
        "restore" => stormdist::media::restore(&media, &mut engine)?,
        "install" => stormdist::media::install(&media, req.flavour.as_deref().context("install needs a flavour")?, &mut engine)?,
        "upgrade" => stormdist::media::upgrade(
            &media,
            req.current.as_deref().context("upgrade needs current, the installed release's identity")?,
            req.flavour.as_deref().context("upgrade needs a flavour")?,
            &mut engine,
        )?,
        m => bail!("mode {m:?}: restore, install or upgrade"),
    }
    Ok(media.release.version.clone())
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Start a restore job in the background.
pub fn start(state: Arc<AppState>, req: RestoreRequest) -> anyhow::Result<Job> {
    parse_key(&req.key)?;
    ensure!(matches!(req.mode.as_str(), "restore" | "install" | "upgrade"), "mode {:?}: restore, install or upgrade", req.mode);
    let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
    let job = Job {
        id: id.clone(),
        request: req.clone(),
        state: "running".into(),
        started_at: now(),
        finished_at: None,
        error: None,
        release: None,
        report: Report::default(),
    };
    jobs().lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), job.clone());
    let report = Arc::new(Mutex::new(Report::default()));
    let rt = tokio::runtime::Handle::current();
    let r2 = report.clone();
    tokio::task::spawn_blocking(move || {
        let out = run_restore(state, rt, &req, Some(r2));
        let mut all = jobs().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(j) = all.get_mut(&id) {
            j.finished_at = Some(now());
            j.report = report.lock().unwrap_or_else(|e| e.into_inner()).clone();
            match out {
                Ok(v) => {
                    tracing::info!("stormdist restore {id}: release {v} done: {:?}", j.report);
                    j.state = "done".into();
                    j.release = Some(v);
                }
                Err(e) => {
                    tracing::error!("stormdist restore {id} failed: {e:#}");
                    j.state = "failed".into();
                    j.error = Some(format!("{e:#}"));
                }
            }
        }
    });
    Ok(job)
}

pub fn job(id: &str) -> Option<Job> {
    jobs().lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned()
}

pub fn list_jobs() -> Vec<Job> {
    jobs().lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_golden_name_gives_component_and_commit() {
        assert_eq!(from_name("golden-stormd-0123456789ab"), Some(("stormd".into(), "0123456789ab".into())));
        assert_eq!(from_name("golden-rustkube-api-abcdef1"), Some(("rustkube-api".into(), "abcdef1".into())));
        assert_eq!(from_name("golden-x-12"), None, "too short a commit");
        assert_eq!(from_name("golden-x-nothex1"), None);
        assert_eq!(from_name("pvc-1"), None);
    }

    #[test]
    fn the_inventory_merges_without_duplicates() {
        let c = Catalogue { release: "a".into(), signature_hex: "00".into() };
        let mut inv = Inventory::default();
        assert!(inv.merge(["g1".to_string()], [c.clone()]));
        assert!(!inv.merge(["g1".to_string()], [c.clone()]), "the same again changes nothing");
        assert!(inv.merge([], [Catalogue { release: "b".into(), signature_hex: "00".into() }]));
        assert_eq!(inv.releases.len(), 2);
        assert_eq!(inv.forge().roots, vec!["g1".to_string()]);
    }
}
