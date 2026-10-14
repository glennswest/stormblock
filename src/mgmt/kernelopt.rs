//! An optional kernel image from forge on day 2 (#383, stormcos#276).
//!
//! A server node takes the kernel set's optional module images (`gpu`,
//! `misc`) only when asked: the node manifest's `[kernel] optional = ["gpu"]`,
//! read by stormcos, which calls `POST /api/v1/kernel/optional`. The image is
//! per kernel (`golden-gpu-<sha>` is built with that kernel's `modules`), so
//! the engine:
//!
//! 1. reads its release's manifest from forge, `GET
//!    /api/v1/releases/{release}/manifest`, and finds the entry marked
//!    `optional` for that image and the kernel it runs (`uname -r`). The
//!    manifest travels with the release onto any forge, so this needs no
//!    stormcentral (the owner's decision on #383, 1B);
//! 2. pulls the golden's bytes from forge, `GET
//!    /api/v1/volumes/{golden}/content`, presenting the node's enrolled
//!    certificate (stormcert#78/#79: `/data/stormcert/forge-node.*.pem`),
//!    which forge lets read only that and manifests (decision 2). It is an
//!    import: streamed with resume, only what is not zeros written, its
//!    sha256 checked against the manifest's, sealed, and deleted if anything
//!    fails, so nothing is left half-made;
//! 3. clones it under the image's name (`gpu`), the volume stormcos mounts.
//!
//! Asked again with both there: nothing happens. Recorded in
//! `<data_dir>/kernel-optional.json`, which an A/B switch reads to pull the
//! same images for the next release's kernel (stormupdate calls this with
//! `release` and `kernel` once it has staged it).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::mgmt::AppState;

/// What is asked for.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    /// `gpu`, `misc`.
    pub images: Vec<String>,
    /// The release whose manifest names them; the node's current generation
    /// when absent (#122).
    #[serde(default)]
    pub release: Option<String>,
    /// The kernel, as `uname -r` says it; the running one when absent.
    #[serde(default)]
    pub kernel: Option<String>,
    /// Forge's engine, `https://forge:9090`; `[kernel] forge_url`, else the
    /// host of the forge this node booted from (`/run/stormblock/forge/url`)
    /// on port 9090, when absent.
    #[serde(default)]
    pub forge: Option<String>,
}

/// Where one image stands.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Status {
    pub image: String,
    pub kernel: String,
    pub release: String,
    /// `present` (nothing to do), `pulling`, `cloning`, `ready`, `failed`.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub golden: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone: Option<String>,
    /// The import pulling it, for its progress (`GET /api/v1/volumes/import/{id}`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import: Option<uuid::Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What this node holds, kept across restarts: what an A/B switch pulls again.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Installed {
    pub image: String,
    pub kernel: String,
    pub release: String,
    pub golden: String,
    pub clone: String,
}

const RECORD: &str = "kernel-optional.json";

fn jobs() -> &'static Mutex<HashMap<String, Status>> {
    static J: std::sync::OnceLock<Mutex<HashMap<String, Status>>> = std::sync::OnceLock::new();
    J.get_or_init(Default::default)
}

/// Every image asked for in this process, and its state.
pub fn statuses() -> Vec<Status> {
    let mut v: Vec<Status> = jobs().lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
    v.sort_by(|a, b| a.image.cmp(&b.image));
    v
}

pub fn status(image: &str) -> Option<Status> {
    jobs().lock().unwrap_or_else(|e| e.into_inner()).get(image).cloned()
}

fn set(s: Status) {
    jobs().lock().unwrap_or_else(|e| e.into_inner()).insert(s.image.clone(), s);
}

pub fn installed(data_dir: &Path) -> Vec<Installed> {
    std::fs::read(data_dir.join(RECORD)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn record(data_dir: &Path, i: Installed) {
    let mut all = installed(data_dir);
    all.retain(|x| !(x.image == i.image && x.kernel == i.kernel));
    all.push(i);
    match serde_json::to_vec_pretty(&all) {
        Ok(b) => {
            if let Err(e) = crate::serve::wiring::write_atomic(&data_dir.join(RECORD), &b) {
                tracing::warn!("{RECORD}: {e}");
            }
        }
        Err(e) => tracing::warn!("{RECORD}: {e}"),
    }
}

/// The kernel this node runs.
pub fn running_kernel() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/osrelease").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Whether a manifest's kernel names the one asked for: equal, or one is the
/// other with its architecture (`6.17.1-300.fc43` and `6.17.1-300.fc43.x86_64`).
pub fn kernel_matches(entry: &str, asked: &str) -> bool {
    entry == asked
        || asked.strip_prefix(entry).is_some_and(|rest| rest.starts_with('.'))
        || entry.strip_prefix(asked).is_some_and(|rest| rest.starts_with('.'))
}

/// The manifest entry for `image` built for `kernel`.
pub fn find<'a>(
    manifest: &'a [crate::mgmt::api::releases::ManifestEntry],
    image: &str,
    kernel: &str,
) -> Option<&'a crate::mgmt::api::releases::ManifestEntry> {
    manifest.iter().find(|e| {
        e.optional
            && e.image.as_deref() == Some(image)
            && e.kernel.as_deref().is_some_and(|k| kernel_matches(k, kernel))
    })
}

/// Forge's engine: asked, configured, or the host the node booted from.
fn forge_base(state: &AppState, asked: Option<&str>) -> Option<String> {
    if let Some(f) = asked.or(state.config.kernel.forge_url.as_deref()) {
        return Some(f.trim_end_matches('/').to_string());
    }
    let url = std::fs::read_to_string(Path::new(crate::mgmt::forge_trust::NODE_DIR).join("url")).ok()?;
    let host = url.trim().strip_prefix("https://")?.split('/').next()?.rsplit_once(':').map(|(h, _)| h.to_string())?;
    Some(format!("https://{host}:9090"))
}

/// The node's enrolled identity and forge's CA (#383 decision 2).
fn identity(state: &AppState) -> Result<crate::image::import::ClientTls, String> {
    let k = &state.config.kernel;
    let read = |p: &PathBuf, what: &str| std::fs::read(p).map_err(|e| format!("{what} {}: {e}", p.display()));
    let cert = read(&k.client_cert(), "the node's enrolled certificate")?;
    let key = read(&k.client_key(), "its key")?;
    let ca = std::fs::read(k.forge_ca()).ok();
    Ok(crate::image::import::ClientTls { ca, cert, key })
}

/// Start pulling `req.images`. Each answers at once: `present` when the node
/// already holds it, `pulling` when started (its progress in [`status`]), or
/// an error that made nothing (no forge, no certificate, no such image for
/// this kernel). `Err` for what is wrong with the request as a whole.
pub async fn start(state: &Arc<AppState>, req: Request) -> Result<Vec<Status>, (u16, String)> {
    if req.images.is_empty() {
        return Err((400, "name the images: \"images\": [\"gpu\"]".into()));
    }
    let release = match req.release.clone().or_else(|| {
        state
            .data_dir
            .as_deref()
            .and_then(|d| crate::image::stage::Generations::load(d).current.map(|g| g.version))
    }) {
        Some(r) => r,
        None => return Err((400, "no release named, and this node records none it runs (release-generations.json)".into())),
    };
    let Some(kernel) = req.kernel.clone().or_else(running_kernel) else {
        return Err((400, "no kernel named, and the running one is unknown".into()));
    };
    let Some(forge) = forge_base(state, req.forge.as_deref()) else {
        return Err((409, "no forge to pull from: none named, no [kernel] forge_url, and no forge trust in /run/stormblock/forge".into()));
    };
    let tls = identity(state).map_err(|e| (409, format!("{e}: this node holds no enrolled certificate (stormcos#486)")))?;
    // The release's manifest, from forge, with the node's certificate.
    let client = {
        let mut b = crate::http::Client::builder().timeout(std::time::Duration::from_secs(30));
        if let Some(ca) = &tls.ca {
            b = b.add_root_certificate_pem(ca.clone());
        }
        b.identity_pem(tls.cert.clone(), tls.key.clone()).build().map_err(|e| (500, e.to_string()))?
    };
    let url = format!("{forge}/api/v1/releases/{release}/manifest");
    #[derive(Deserialize)]
    struct Manifest {
        items: Vec<crate::mgmt::api::releases::ManifestEntry>,
    }
    let manifest: Vec<crate::mgmt::api::releases::ManifestEntry> = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => {
            r.json::<Manifest>().await.map_err(|e| (502, format!("{url}: {e}")))?.items
        }
        Ok(r) => return Err((502, format!("{url}: forge answered {}", r.status()))),
        Err(e) => return Err((502, format!("forge is not reachable ({url}): {e}; nothing was made"))),
    };
    let mut out = Vec::new();
    for image in req.images {
        let mut s = Status {
            image: image.clone(),
            kernel: kernel.clone(),
            release: release.clone(),
            state: "failed".into(),
            golden: None,
            clone: None,
            import: None,
            bytes: None,
            error: None,
        };
        let Some(entry) = find(&manifest, &image, &kernel).cloned() else {
            s.error = Some(format!("release {release}'s manifest has no optional {image} for kernel {kernel}"));
            set(s.clone());
            out.push(s);
            continue;
        };
        s.golden = Some(entry.name.clone());
        s.bytes = entry.size_bytes;
        let (golden, clone) = {
            let vm = state.volume_manager.lock().await;
            let golden = vm.find_volume(&entry.name).await.filter(|id| vm.is_sealed(id));
            let clone = vm.find_volume(&image).await;
            (golden, clone)
        };
        if golden.is_some() && clone.is_some() {
            s.state = "present".into();
            s.clone = Some(image.clone());
            set(s.clone());
            out.push(s);
            continue;
        }
        if let Some(g) = golden {
            // The golden is here and its clone is not: only the clone.
            s.state = "cloning".into();
            set(s.clone());
            let st = state.clone();
            let mut s2 = s.clone();
            tokio::spawn(async move {
                finish(&st, &mut s2, g).await;
            });
            out.push(s);
            continue;
        }
        let spec = crate::image::import::ImportSpec {
            name: entry.name.clone(),
            file: None,
            url: Some(format!("{forge}/api/v1/volumes/{}/content", entry.name)),
            format: Some("raw".into()),
            redundancy: None,
            size: entry.size_bytes.map(|b| b.to_string()),
            role: None,
            seal: true,
            sha256: Some(entry.digest.trim_start_matches("sha256:").to_string()),
            parent: None,
            provenance: [("from".to_string(), format!("forge release {release}, kernel {kernel} (#383)"))].into(),
            keep_download: false,
            verify: true,
            tls: Some(tls.clone()),
            lba: None,
        };
        let job = match state.imports.write().await.start(state.clone(), spec) {
            Ok(j) => j,
            Err(e) => {
                s.error = Some(e);
                set(s.clone());
                out.push(s);
                continue;
            }
        };
        s.import = Some(job.read().await.id);
        s.state = "pulling".into();
        set(s.clone());
        let st = state.clone();
        let mut s2 = s.clone();
        tokio::spawn(async move {
            use crate::image::import::ImportState;
            loop {
                let (state_now, vol, err) = {
                    let j = job.read().await;
                    (j.state, j.volume_id, j.error.clone())
                };
                match state_now {
                    ImportState::Done => {
                        let Some(v) = vol else {
                            s2.state = "failed".into();
                            s2.error = Some("the pull finished with no volume".into());
                            set(s2);
                            return;
                        };
                        finish(&st, &mut s2, crate::volume::VolumeId(v)).await;
                        return;
                    }
                    ImportState::Failed => {
                        s2.state = "failed".into();
                        s2.error = err.or(Some("the pull failed".into()));
                        tracing::warn!("kernel optional {}: {}", s2.image, s2.error.as_deref().unwrap_or(""));
                        set(s2);
                        return;
                    }
                    _ => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
                }
            }
        });
        out.push(s);
    }
    Ok(out)
}

/// The golden is here: its clone under the image's name, and the record.
async fn finish(state: &Arc<AppState>, s: &mut Status, golden: crate::volume::VolumeId) {
    s.state = "cloning".into();
    set(s.clone());
    let spec = crate::fs::template::CloneSpec::new(s.image.clone());
    // The clone owes its persist, made once it has let go of the volume
    // manager (#364, #415).
    let vm = state.volume_manager.clone();
    let cloned = crate::lockwatch::spawn_owing(
        {
            let vm = vm.clone();
            async move { crate::fs::template::clone_volume(&vm, golden, &spec).await }
        },
        move || async move {
            if let Err(e) = crate::volume::VolumeManager::persist_detached_checked(&vm).await {
                tracing::error!("kernel optional: the clone was not written to the volume metadata: {e}");
            }
        },
    )
    .await
    .map_err(|e| format!("the clone task: {e}"))
    .and_then(|r| r.map_err(|e| e.to_string()));
    match cloned {
        Ok(_) => {
            s.state = "ready".into();
            s.clone = Some(s.image.clone());
            if let Some(d) = state.data_dir.as_deref() {
                record(
                    d,
                    Installed {
                        image: s.image.clone(),
                        kernel: s.kernel.clone(),
                        release: s.release.clone(),
                        golden: s.golden.clone().unwrap_or_default(),
                        clone: s.image.clone(),
                    },
                );
            }
            let line = format!(
                "kernel optional {}: {} for kernel {} pulled from forge and cloned as {}",
                s.image,
                s.golden.as_deref().unwrap_or("?"),
                s.kernel,
                s.image
            );
            println!("{line}");
            tracing::info!("{line}");
        }
        Err(e) => {
            s.state = "failed".into();
            s.error = Some(format!("clone: {e}"));
            tracing::warn!("kernel optional {}: clone: {e}", s.image);
        }
    }
    set(s.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kernel_with_or_without_its_architecture_matches() {
        assert!(kernel_matches("6.17.1-300.fc43", "6.17.1-300.fc43.x86_64"));
        assert!(kernel_matches("6.17.1-300.fc43.x86_64", "6.17.1-300.fc43"));
        assert!(kernel_matches("6.17.1-300.fc43.x86_64", "6.17.1-300.fc43.x86_64"));
        assert!(!kernel_matches("6.17.1-300.fc43", "6.17.10-300.fc43"));
        assert!(!kernel_matches("6.17.1-300.fc43", "6.17.1-301.fc43.x86_64"));
    }
}
