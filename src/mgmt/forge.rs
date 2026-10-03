//! Forge mode, turned on per node and kept by the engine (#206, #272).
//!
//! A stormcos node can also be its site's forge (a bastion, stormcos#187):
//! it exports goldens and host clones over NVMe/TCP and answers boot claims
//! with something to attach. Every node runs the same image with the same
//! engine argv, so the role cannot come from the command line. It is the
//! node's own setting instead: `PUT /api/v1/forge` with the `[nvmeof]`
//! settings starts the shared target live and keeps them in
//! `<data_dir>/forge.json`, which goes wherever the engine's data directory
//! goes (on stormcos, the `stormblock-state` volume). Every later start
//! serves it again. `DELETE` turns it off.
//!
//! A target the command line or `--config` set up (the daemon's flags, an
//! `[nvmeof]` section) is the configuration's, not this API's: the API
//! reports it and refuses to change it.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};

use crate::mgmt::config::{ManagementConfig, NvmeofExportConfig};
use crate::mgmt::AppState;
use crate::target::nvmeof::{NvmeofConfig, NvmeofTarget};
use crate::target::reactor::{ReactorConfig, ReactorPool};

/// Where the node's forge settings are kept, in its data directory.
pub const FORGE_FILE: &str = "forge.json";

/// Who set up the shared target this engine serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The command line or `--config`.
    Config,
    /// `PUT /api/v1/forge`, kept in `forge.json`.
    Api,
}

/// The forge state an engine keeps.
#[derive(Default)]
pub struct Forge {
    pub source: Option<Source>,
    /// A reactor for a target started when the node has none of its own to
    /// lend (no serving layer).
    reactor: Option<Arc<ReactorPool>>,
}

#[derive(Debug)]
pub enum ForgeError {
    /// The settings cannot be served (a listen address that is not one, a
    /// port already taken).
    Invalid(String),
    /// The target belongs to the configuration, not to this API.
    Configured,
    /// The setting could not be kept.
    Io(String),
}

impl std::fmt::Display for ForgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForgeError::Invalid(e) => write!(f, "{e}"),
            ForgeError::Configured => write!(
                f,
                "this engine's NVMe/TCP target is set by its command line or --config; \
                 change it there"
            ),
            ForgeError::Io(e) => write!(f, "keeping the forge settings: {e}"),
        }
    }
}

/// The shared target `settings` describe. No raw drive namespaces: an engine
/// that owns a slab serves volumes, never the slab's drive.
pub fn target_from(
    settings: &NvmeofExportConfig,
    management: &ManagementConfig,
) -> Result<NvmeofTarget, String> {
    let listen_addr: std::net::SocketAddr = settings
        .listen_addr
        .parse()
        .map_err(|e| format!("[nvmeof] listen_addr {:?}: {e}", settings.listen_addr))?;
    if settings.nqn.trim().is_empty() {
        return Err("[nvmeof] nqn is empty".into());
    }
    if settings.export_drives {
        tracing::info!(
            "NVMe-oF: export_drives is ignored here — this engine serves volumes, never its slab's drive"
        );
    }
    let advertised_addr = management
        .advertised_host()
        .and_then(|h| format!("{h}:{}", listen_addr.port()).parse().ok());
    Ok(NvmeofTarget::new(NvmeofConfig {
        listen_addr,
        nqn: settings.nqn.clone(),
        advertised_addr,
        ..Default::default()
    }))
}

/// Put a shared target in service: the daemon's, and a node's forge.
///
/// Bound here, so a port that is taken is an answer rather than a log line.
/// Then stored in the AppState so the export API can add namespaces at
/// runtime (#26) and a boothost claim can answer with an attach; then who may
/// connect, before anything is served (#210); then the exports and host
/// subsystems made in an earlier run, because an export is an address
/// something out there has written down.
pub async fn serve(
    state: &Arc<AppState>,
    reactor: &Arc<ReactorPool>,
    target: Arc<NvmeofTarget>,
) -> std::io::Result<()> {
    let addr = target.listen_addr();
    // A target just told to stop releases its port as its accept loop ends:
    // a moment, not a failure.
    let mut tries = 0;
    let listener = loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => break l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && tries < 40 => {
                tries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e),
        }
    };
    *state.nvmeof_target.write().await = Some(target.clone());
    crate::mgmt::nvme_hosts::apply_shared_policy(state, &target);
    crate::mgmt::nvme_hosts::restore(state).await;
    crate::mgmt::api::exports::restore_exports(state).await;
    crate::mgmt::api::v1::restore_nvme_nsids(state).await;
    tracing::info!("NVMe-oF/TCP target listening on {addr}");
    let reactor = reactor.clone();
    tokio::spawn(async move {
        if let Err(e) = target.run_with_listener(listener, &reactor).await {
            tracing::error!("NVMe-oF target error: {e}");
        }
    });
    Ok(())
}

/// Record that the configuration set up the target this engine serves.
pub async fn mark_configured(state: &AppState) {
    state.forge.lock().await.source = Some(Source::Config);
}

fn path(state: &AppState) -> Option<PathBuf> {
    state.config.management.data_dir.as_ref().map(|d| PathBuf::from(d).join(FORGE_FILE))
}

/// The forge settings this node keeps, if it is a forge.
pub fn load(state: &AppState) -> Option<NvmeofExportConfig> {
    let p = path(state)?;
    let raw = std::fs::read_to_string(&p).ok()?;
    match serde_json::from_str(&raw) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!("{}: {e} — not serving it", p.display());
            None
        }
    }
}

fn keep(state: &AppState, settings: &NvmeofExportConfig) -> Result<(), ForgeError> {
    use std::io::Write;
    let Some(p) = path(state) else {
        return Err(ForgeError::Io("this engine has no data directory to keep them in".into()));
    };
    let tmp = p.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(settings).map_err(|e| ForgeError::Io(e.to_string()))?;
    let mut f = std::fs::File::create(&tmp).map_err(|e| ForgeError::Io(e.to_string()))?;
    f.write_all(&body).map_err(|e| ForgeError::Io(e.to_string()))?;
    f.sync_all().map_err(|e| ForgeError::Io(e.to_string()))?;
    std::fs::rename(&tmp, &p).map_err(|e| ForgeError::Io(e.to_string()))
}

async fn reactor(state: &AppState, forge: &mut Forge) -> Arc<ReactorPool> {
    if let Some(pv) = state.per_volume.read().await.as_ref() {
        return pv.reactor.clone();
    }
    forge
        .reactor
        .get_or_insert_with(|| Arc::new(ReactorPool::new(&ReactorConfig { core_count: 1, pin_cores: false })))
        .clone()
}

/// Serve `settings` as this node's forge, and keep them (`keep`: not when
/// restoring them from the file they came from).
pub async fn start(state: &Arc<AppState>, settings: NvmeofExportConfig, keep_it: bool) -> Result<(), ForgeError> {
    let mut forge = state.forge.lock().await;
    if forge.source == Some(Source::Config)
        || (forge.source.is_none() && state.nvmeof_target.read().await.is_some())
    {
        return Err(ForgeError::Configured);
    }
    let target = target_from(&settings, &state.config.management).map_err(ForgeError::Invalid)?;
    let reactor = reactor(state, &mut forge).await;
    // Replacing one forge with another: the old one stops taking new
    // connections; the ones it has finish.
    if let Some(old) = state.nvmeof_target.write().await.take() {
        old.stop_accepting();
    }
    let before = state.nvmeof_settings.write().unwrap().replace(settings.clone());
    if let Err(e) = serve(state, &reactor, Arc::new(target)).await {
        *state.nvmeof_settings.write().unwrap() = before;
        forge.source = None;
        return Err(ForgeError::Invalid(format!("serving on {}: {e}", settings.listen_addr)));
    }
    forge.source = Some(Source::Api);
    if keep_it {
        keep(state, &settings)?;
    }
    tracing::info!("forge mode: NVMe/TCP target on {} ({})", settings.listen_addr, settings.nqn);
    Ok(())
}

/// Turn forge mode off: no new connections, the settings forgotten. Returns
/// how many connections are still being served (they finish on their own).
pub async fn stop(state: &Arc<AppState>) -> Result<usize, ForgeError> {
    let mut forge = state.forge.lock().await;
    match forge.source {
        Some(Source::Config) => return Err(ForgeError::Configured),
        None => return Ok(0),
        Some(Source::Api) => {}
    }
    let live = match state.nvmeof_target.write().await.take() {
        Some(t) => {
            t.stop_accepting();
            t.live_connections()
        }
        None => 0,
    };
    *state.nvmeof_settings.write().unwrap() = state.config.nvmeof.clone();
    forge.source = None;
    if let Some(p) = path(state) {
        if let Err(e) = std::fs::remove_file(&p) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(ForgeError::Io(e.to_string()));
            }
        }
    }
    tracing::info!("forge mode off ({live} connection(s) left to finish)");
    Ok(live)
}

/// At start: serve the forge this node keeps, when nothing configured a
/// target of its own.
pub async fn restore(state: &Arc<AppState>) {
    let Some(settings) = load(state) else { return };
    match start(state, settings, false).await {
        Ok(()) => println!("  forge mode: serving the NVMe/TCP target this node keeps ({FORGE_FILE})"),
        Err(e) => {
            println!("  forge mode: not serving {FORGE_FILE}: {e}");
            tracing::error!("forge mode: not serving {FORGE_FILE}: {e}");
        }
    }
}

/// What `GET /api/v1/forge` says.
pub async fn status(state: &AppState) -> Value {
    let source = state.forge.lock().await.source;
    let target = state.nvmeof_target.read().await.clone();
    let settings = state.nvmeof_settings();
    json!({
        "enabled": target.is_some(),
        "source": source,
        "listen_addr": settings.as_ref().map(|s| s.listen_addr.clone()),
        "nqn": settings.as_ref().map(|s| s.nqn.clone()),
        "allow_any_host": settings.as_ref().map(|s| s.allow_any_host),
        "allowed_hosts": settings.as_ref().map(|s| s.allowed_hosts.clone()),
        "require_dhchap": settings.as_ref().map(|s| s.require_dhchap),
        "boothost_host_nqn": settings.as_ref().and_then(|s| s.boothost_host_nqn.clone()),
        "live_connections": target.map(|t| t.live_connections()),
    })
}
