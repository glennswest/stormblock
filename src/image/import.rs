//! Import a disk image into a sealed golden — the way a cloud image, a VM
//! export or an ISO becomes something the node can clone.
//!
//! `POST /api/v1/volumes/import` starts a job; `GET …/import/{id}` follows
//! it. The source is a local file or a URL (streamed to
//! `<data_dir>/imports/`, never held in memory), the format is detected by
//! magic (`raw`, `qcow2`, `vmdk`, `ova`, `iso` is raw), only the clusters
//! the image actually carries are written, and the result is sealed with
//! its disk shape recorded (`gpt`/`mbr`/`iso9660`/`ext4`) so every clone can
//! be given its own identity. Thin from the first byte: a 2 GB cloud image
//! with 600 MB used costs 600 MB.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::drive::BlockDevice;
use crate::image::decode::{self, SourceFormat};
use crate::mgmt::AppState;
use crate::volume::{CreateOptions, RedundancyPolicy, VolumeId};

#[derive(Debug, Clone, Deserialize)]
pub struct ImportSpec {
    /// Name of the golden volume.
    pub name: String,
    /// A local file…
    #[serde(default)]
    pub file: Option<String>,
    /// …or a URL to stream first.
    #[serde(default)]
    pub url: Option<String>,
    /// `raw`, `qcow2`, `vmdk`, `ova`; absent = detect by magic.
    #[serde(default)]
    pub format: Option<String>,
    /// Redundancy for the golden (and so for its clones).
    #[serde(default)]
    pub redundancy: Option<String>,
    /// Grow the volume beyond the image's size (never shrink).
    #[serde(default)]
    pub size: Option<String>,
    /// Which half of the node's storage the golden lands in: `system` (the
    /// goldens an image build replaces) or `data` (identity and state).
    ///
    /// Absent means the node decides — system where it has a system slab,
    /// otherwise data. Without this a registry box, whose drives are all
    /// `role=data`, asked for a system slab that does not exist and failed
    /// the import on its first write (#93).
    #[serde(default)]
    pub role: Option<String>,
    /// Seal when done (default true). `false` leaves it writable.
    #[serde(default = "yes")]
    pub seal: bool,
    /// Keep a downloaded file after the import.
    #[serde(default)]
    pub keep_download: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportState {
    Downloading,
    Writing,
    Sealing,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportStatus {
    pub id: Uuid,
    pub name: String,
    pub state: ImportState,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    pub virtual_size: u64,
    /// Bytes of the image walked so far (of `virtual_size`).
    pub progress_bytes: u64,
    /// Bytes actually written to the volume (what the golden costs).
    pub written_bytes: u64,
    pub downloaded_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fs: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Default)]
pub struct Imports {
    jobs: HashMap<Uuid, Arc<RwLock<ImportStatus>>>,
}

impl Imports {
    pub async fn status(&self, id: &Uuid) -> Option<ImportStatus> {
        match self.jobs.get(id) {
            Some(j) => Some(j.read().await.clone()),
            None => None,
        }
    }

    pub async fn all(&self) -> Vec<ImportStatus> {
        let mut out = Vec::new();
        for j in self.jobs.values() {
            out.push(j.read().await.clone());
        }
        out.sort_by_key(|s| s.started_at);
        out
    }

    /// Start an import. Validation that can fail fast happens here; the
    /// long part runs behind the returned status.
    pub fn start(&mut self, state: Arc<AppState>, spec: ImportSpec) -> Result<Arc<RwLock<ImportStatus>>, String> {
        if spec.name.trim().is_empty() {
            return Err("name must not be empty".into());
        }
        let source = match (&spec.file, &spec.url) {
            (Some(f), None) => f.clone(),
            (None, Some(u)) => u.clone(),
            (Some(_), Some(_)) => return Err("give file or url, not both".into()),
            (None, None) => return Err("give file or url".into()),
        };
        if let Some(r) = &spec.redundancy {
            RedundancyPolicy::parse(r).map_err(|e| format!("redundancy: {e}"))?;
        }
        if let Some(r) = &spec.role {
            crate::drive::slab::SlabRole::parse(r)
                .ok_or_else(|| format!("invalid role '{r}' (use system or data)"))?;
        }
        if let Some(f) = &spec.format {
            match f.to_ascii_lowercase().as_str() {
                "raw" | "qcow2" | "vmdk" | "ova" | "iso" => {}
                other => return Err(format!("format {other:?}: raw, qcow2, vmdk, ova or iso")),
            }
        }
        let id = Uuid::new_v4();
        let status = Arc::new(RwLock::new(ImportStatus {
            id,
            name: spec.name.clone(),
            state: if spec.url.is_some() { ImportState::Downloading } else { ImportState::Writing },
            source,
            format: spec.format.clone(),
            virtual_size: 0,
            progress_bytes: 0,
            written_bytes: 0,
            downloaded_bytes: 0,
            volume_id: None,
            fs: None,
            error: None,
            started_at: now(),
            finished_at: None,
        }));
        let st = status.clone();
        tokio::spawn(async move {
            let outcome = run(&state, &spec, &st).await;
            let mut s = st.write().await;
            s.finished_at = Some(now());
            match outcome {
                Ok(()) => s.state = ImportState::Done,
                Err(e) => {
                    s.state = ImportState::Failed;
                    s.error = Some(e);
                }
            }
        });
        self.jobs.insert(id, status.clone());
        Ok(status)
    }
}

/// A readable image of any supported shape.
pub enum Source {
    Raw { file: tokio::fs::File, len: u64 },
    Qcow2(decode::qcow2::Qcow2),
    Vmdk(decode::vmdk::Vmdk),
}

impl Source {
    pub fn virtual_size(&self) -> u64 {
        match self {
            Source::Raw { len, .. } => *len,
            Source::Qcow2(q) => q.virtual_size(),
            Source::Vmdk(v) => v.virtual_size(),
        }
    }

    /// The natural copy unit: the format's own allocation unit, so a hole
    /// next to data is never written as zeros.
    pub fn chunk(&self) -> u64 {
        match self {
            Source::Raw { .. } => 1 << 20,
            Source::Qcow2(q) => q.cluster_size,
            Source::Vmdk(v) => v.chunk,
        }
    }

    /// Whether `[off, off+chunk)` may carry data. `false` is definite.
    pub async fn may_have_data(&mut self, off: u64, chunk: u64) -> Result<bool, String> {
        match self {
            Source::Raw { .. } => Ok(true),
            Source::Qcow2(q) => {
                let first = off / q.cluster_size;
                let last = (off + chunk - 1) / q.cluster_size;
                for c in first..=last {
                    if q.cluster_state(c).await.map_err(|e| e.to_string())? == decode::qcow2::ClusterState::Data {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Source::Vmdk(v) => {
                let mut o = off;
                while o < off + chunk {
                    if v.chunk_state(o).await.map_err(|e| e.to_string())? == decode::vmdk::GrainState::Data {
                        return Ok(true);
                    }
                    o += v.chunk;
                }
                Ok(false)
            }
        }
    }

    pub async fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), String> {
        match self {
            Source::Raw { file, len } => {
                use tokio::io::{AsyncReadExt, AsyncSeekExt};
                if off >= *len {
                    buf.fill(0);
                    return Ok(());
                }
                file.seek(std::io::SeekFrom::Start(off)).await.map_err(|e| e.to_string())?;
                let take = ((*len - off) as usize).min(buf.len());
                let mut got = 0usize;
                while got < take {
                    let n = file.read(&mut buf[got..take]).await.map_err(|e| e.to_string())?;
                    if n == 0 {
                        break;
                    }
                    got += n;
                }
                buf[got..].fill(0);
                Ok(())
            }
            Source::Qcow2(q) => q.read_at(off, buf).await.map_err(|e| e.to_string()),
            Source::Vmdk(v) => v.read_at(off, buf).await.map_err(|e| e.to_string()),
        }
    }
}

/// Open a file as whatever it is.
pub async fn open_source(path: &Path, forced: Option<&str>) -> Result<(Source, SourceFormat), String> {
    let detected = decode::detect_file(path).await.map_err(|e| format!("{}: {e}", path.display()))?;
    let format = match forced.map(|f| f.to_ascii_lowercase()) {
        Some(f) => match f.as_str() {
            "raw" | "iso" => SourceFormat::Raw,
            "qcow2" => SourceFormat::Qcow2,
            "vmdk" => SourceFormat::Vmdk,
            "ova" => SourceFormat::Ova,
            _ => detected,
        },
        None => detected,
    };
    let src = match format {
        SourceFormat::Raw => {
            let file = tokio::fs::File::open(path).await.map_err(|e| e.to_string())?;
            let len = file.metadata().await.map_err(|e| e.to_string())?.len();
            Source::Raw { file, len }
        }
        SourceFormat::Qcow2 => Source::Qcow2(decode::qcow2::Qcow2::open(path).await.map_err(|e| e.to_string())?),
        SourceFormat::Vmdk => Source::Vmdk(decode::vmdk::Vmdk::open(path).await.map_err(|e| e.to_string())?),
        SourceFormat::Ova => {
            let (win, name) = decode::ova::vmdk_window(path)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "no .vmdk member in the OVA".to_string())?;
            tracing::info!(member = %name, "importing the VMDK inside the OVA");
            Source::Vmdk(decode::vmdk::Vmdk::from_window(win).await.map_err(|e| e.to_string())?)
        }
        SourceFormat::Unsupported(n) => return Err(format!("{n} images are not supported; convert with qemu-img convert -O qcow2")),
    };
    Ok((src, format))
}

async fn run(state: &Arc<AppState>, spec: &ImportSpec, st: &Arc<RwLock<ImportStatus>>) -> Result<(), String> {
    // A raw image over HTTP is written as it arrives, with nothing staged.
    //
    // Raw is sequential, so there is no reason to spool it — and spooling
    // means needing room for the image's *whole virtual size* on a node about
    // to store only the parts that are used. A 32 GB image with 9 GB in it
    // failed with ENOSPC while the volume it was headed for had room three
    // times over. qcow2 and VMDK still stage: their decoders seek.
    if let (Some(url), Some("raw" | "iso" | "iso9660")) = (&spec.url, spec.format.as_deref()) {
        return stream_raw(state, spec, st, url).await;
    }

    // 1. Fetch, if a URL.
    let mut downloaded: Option<PathBuf> = None;
    let path: PathBuf = match (&spec.file, &spec.url) {
        (Some(f), _) => PathBuf::from(f),
        (None, Some(url)) => {
            let dir = state
                .data_dir
                .clone()
                .unwrap_or_else(std::env::temp_dir)
                .join("imports");
            tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
            let target = dir.join(format!("{}.img", st.read().await.id.simple()));
            let client = crate::http::Client::builder().timeout(std::time::Duration::from_secs(6 * 3600)).build().map_err(|e| e.to_string())?;
            let n = client.get_to_file(url, &target).await.map_err(|e| e.to_string())?;
            {
                let mut s = st.write().await;
                s.downloaded_bytes = n;
                s.state = ImportState::Writing;
            }
            downloaded = Some(target.clone());
            target
        }
        _ => unreachable!("validated at start"),
    };

    let result = write_and_seal(state, spec, st, &path).await;
    if let Some(p) = downloaded {
        if !spec.keep_download {
            let _ = tokio::fs::remove_file(&p).await;
        }
    }
    result
}

/// Import a raw image straight off the wire.
///
/// The volume has to exist before the last byte arrives, so its size comes
/// from the request or from `Content-Length`. Only non-zero chunks are
/// written, exactly as the staged path does — a sparse image costs what it
/// uses, not what it spans.
async fn stream_raw(
    state: &Arc<AppState>,
    spec: &ImportSpec,
    st: &Arc<RwLock<ImportStatus>>,
    url: &str,
) -> Result<(), String> {
    let client = crate::http::Client::builder()
        .timeout(std::time::Duration::from_secs(6 * 3600))
        .build()
        .map_err(|e| e.to_string())?;
    let declared = match &spec.size {
        Some(s) => Some(crate::mgmt::config::parse_size(s).map_err(|e| format!("size: {e}"))?),
        None => None,
    };
    let length = client.content_length(url).await.map_err(|e| e.to_string())?;
    let size = match (declared, length) {
        (Some(d), Some(l)) => d.max(l),
        (Some(d), None) => d,
        (None, Some(l)) => l,
        // Neither: a volume cannot be created without a size, and guessing one
        // would silently truncate the image.
        (None, None) => {
            return Err(format!(
                "{url} does not say how big it is (no Content-Length); give `size`"
            ))
        }
    };
    {
        let mut s = st.write().await;
        s.format = Some("raw".into());
        s.virtual_size = size;
        s.state = ImportState::Writing;
    }

    let policy = match &spec.redundancy {
        Some(r) => RedundancyPolicy::parse(r)?,
        None => RedundancyPolicy::none(),
    };
    let role = spec.role.as_deref().and_then(crate::drive::slab::SlabRole::parse);
    let vol_id: VolumeId = {
        let mut vm = state.volume_manager.lock().await;
        vm.create_volume_with(&spec.name, size, CreateOptions::redundant(policy).in_role_opt(role))
            .await
            .map_err(|e| format!("create volume: {e}"))?
    };
    st.write().await.volume_id = Some(vol_id.0);
    let dev: Arc<dyn BlockDevice> = state
        .volume_manager
        .lock()
        .await
        .get_volume(&vol_id)
        .ok_or_else(|| "volume vanished after create".to_string())?;

    // The body arrives in frames of whatever size the server chose, and a
    // volume write wants a decent-sized piece — so frames are gathered into a
    // 4 MiB window and each window is written only if it holds something.
    const WINDOW: usize = 4 << 20;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(8);
    let fetch = {
        let url = url.to_string();
        tokio::spawn(async move { client.get_to_channel(&url, tx).await })
    };

    let mut window: Vec<u8> = Vec::with_capacity(WINDOW);
    let mut off = 0u64;
    let mut written = 0u64;
    let outcome: Result<(), String> = async {
        while let Some(data) = rx.recv().await {
            let mut rest = &data[..];
            while !rest.is_empty() {
                let take = (WINDOW - window.len()).min(rest.len());
                window.extend_from_slice(&rest[..take]);
                rest = &rest[take..];
                if window.len() == WINDOW {
                    if window.iter().any(|&b| b != 0) {
                        dev.write(off, &window)
                            .await
                            .map_err(|e| format!("write at {off}: {e}"))?;
                        written += window.len() as u64;
                    }
                    off += window.len() as u64;
                    window.clear();
                    let mut s = st.write().await;
                    s.downloaded_bytes = off;
                    s.progress_bytes = off;
                    s.written_bytes = written;
                }
            }
        }
        if !window.is_empty() {
            if window.iter().any(|&b| b != 0) {
                dev.write(off, &window).await.map_err(|e| format!("write at {off}: {e}"))?;
                written += window.len() as u64;
            }
            off += window.len() as u64;
        }
        // The fetch's own error is the interesting one when both fail: a write
        // that stopped because the body did should not be reported as a disk
        // problem.
        match fetch.await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(e.to_string()),
            Err(e) => return Err(format!("fetch task: {e}")),
        }
        dev.flush().await.map_err(|e| e.to_string())?;
        Ok(())
    }
    .await;
    if let Err(e) = outcome {
        // Nothing half-imported survives as a volume.
        drop(dev);
        let _ = state.volume_manager.lock().await.delete_volume(vol_id).await;
        st.write().await.volume_id = None;
        return Err(e);
    }
    {
        let mut s = st.write().await;
        s.downloaded_bytes = off;
        s.progress_bytes = off;
        s.written_bytes = written;
        s.state = ImportState::Sealing;
    }

    let fs = crate::fs::disk::probe(&dev).await;
    drop(dev);
    st.write().await.fs = fs.as_ref().map(|f| f.json());
    let mut vm = state.volume_manager.lock().await;
    if spec.seal {
        vm.seal_volume(vol_id, fs).await.map_err(|e| format!("seal: {e}"))?;
    } else if fs.is_some() {
        vm.set_fs_info(vol_id, fs).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn write_and_seal(state: &Arc<AppState>, spec: &ImportSpec, st: &Arc<RwLock<ImportStatus>>, path: &Path) -> Result<(), String> {
    let (mut src, format) = open_source(path, spec.format.as_deref()).await?;
    let vsize = src.virtual_size();
    if vsize == 0 {
        return Err("image is empty".into());
    }
    let size = match &spec.size {
        Some(s) => crate::mgmt::config::parse_size(s).map_err(|e| format!("size: {e}"))?.max(vsize),
        None => vsize,
    };
    {
        let mut s = st.write().await;
        s.format = Some(format.to_string());
        s.virtual_size = vsize;
    }

    // 2. The volume.
    let policy = match &spec.redundancy {
        Some(r) => RedundancyPolicy::parse(r)?,
        None => RedundancyPolicy::none(),
    };
    let role = spec.role.as_deref().and_then(crate::drive::slab::SlabRole::parse);
    let vol_id: VolumeId = {
        let mut vm = state.volume_manager.lock().await;
        vm.create_volume_with(&spec.name, size, CreateOptions::redundant(policy).in_role_opt(role))
            .await
            .map_err(|e| format!("create volume: {e}"))?
    };
    st.write().await.volume_id = Some(vol_id.0);
    let dev: Arc<dyn BlockDevice> = state
        .volume_manager
        .lock()
        .await
        .get_volume(&vol_id)
        .ok_or_else(|| "volume vanished after create".to_string())?;

    // 3. Only what the image carries, and of that only what is not zero.
    let chunk = src.chunk();
    let mut buf = vec![0u8; chunk as usize];
    let mut off = 0u64;
    let mut written = 0u64;
    let outcome: Result<(), String> = async {
        while off < vsize {
            let take = ((vsize - off).min(chunk)) as usize;
            if src.may_have_data(off, chunk).await? {
                src.read_at(off, &mut buf[..take]).await?;
                if buf[..take].iter().any(|&b| b != 0) {
                    dev.write(off, &buf[..take]).await.map_err(|e| format!("write at {off}: {e}"))?;
                    written += take as u64;
                }
            }
            off += take as u64;
            if off % (64 << 20) < chunk {
                let mut s = st.write().await;
                s.progress_bytes = off;
                s.written_bytes = written;
            }
        }
        dev.flush().await.map_err(|e| e.to_string())?;
        Ok(())
    }
    .await;
    if let Err(e) = outcome {
        // Nothing half-imported survives as a volume.
        let _ = state.volume_manager.lock().await.delete_volume(vol_id).await;
        st.write().await.volume_id = None;
        return Err(e);
    }
    {
        let mut s = st.write().await;
        s.progress_bytes = vsize;
        s.written_bytes = written;
        s.state = ImportState::Sealing;
    }

    // 4. Say what it is, and seal.
    let fs = crate::fs::disk::probe(&dev).await;
    drop(dev);
    st.write().await.fs = fs.as_ref().map(|f| f.json());
    let mut vm = state.volume_manager.lock().await;
    if spec.seal {
        vm.seal_volume(vol_id, fs).await.map_err(|e| format!("seal: {e}"))?;
    } else if fs.is_some() {
        vm.set_fs_info(vol_id, fs).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}
