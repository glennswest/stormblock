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
//! **On by default on a node (#287).** A single-node cluster is its own
//! forge, and nothing off the node holds the admin token to turn it on. So
//! `adopt-ublk` serves the target with [`default_settings`] whenever
//! `forge.json` is missing, and `forge.json` says what the node was *told*:
//! the settings a `PUT` gave (on), or `{"enabled": false}` from a `DELETE`
//! (off, kept: stormcluster turns a node that joins as a plain worker off).
//! The default is never written down, so a later engine with another default
//! applies it. The daemon (an appliance) keeps #272's rule: off unless kept.
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
    /// Nothing kept: a node's default (#287).
    Default,
}

/// What `forge.json` says (#287).
#[derive(Debug, Clone)]
pub enum Persisted {
    /// Serve these settings.
    On(NvmeofExportConfig),
    /// Told off, and kept off.
    Off,
}

/// The forge state an engine keeps.
#[derive(Default)]
pub struct Forge {
    pub source: Option<Source>,
    /// Off because `forge.json` says so (#287).
    pub off: bool,
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

/// What this node was told about forge mode, if anything. A file that does
/// not read is taken as off, loudly: someone wrote it, and serving defaults
/// over it would be guessing what they meant.
pub fn load(state: &AppState) -> Option<Persisted> {
    let p = path(state)?;
    let raw = std::fs::read_to_string(&p).ok()?;
    let parsed = serde_json::from_str::<Value>(&raw).and_then(|v| {
        if v.get("enabled") == Some(&Value::Bool(false)) {
            Ok(Persisted::Off)
        } else {
            serde_json::from_value(v).map(Persisted::On)
        }
    });
    match parsed {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::error!("{}: {e} — forge mode stays off", p.display());
            Some(Persisted::Off)
        }
    }
}

/// A node's forge when nothing is kept (#287): every address on 4420, an NQN
/// of the node's name, and #210's closed policy (the shared subsystem admits
/// no host; boot claims and attaches get subsystems of their own for the host
/// they name).
pub fn default_settings(state: &AppState) -> NvmeofExportConfig {
    NvmeofExportConfig {
        listen_addr: "0.0.0.0:4420".into(),
        nqn: format!("nqn.2026-08.lo.storm:{}", nqn_word(&state.local_node_name())),
        export_drives: false,
        allow_any_host: false,
        allowed_hosts: Vec::new(),
        require_dhchap: false,
        boothost_host_nqn: None,
    }
}

/// A node name as it may appear in an NQN: lower case, `[a-z0-9.-]`.
fn nqn_word(name: &str) -> String {
    let w: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '-' })
        .collect();
    let w = w.trim_matches('-').to_string();
    if w.is_empty() { "node".into() } else { w }
}

fn write(state: &AppState, body: &[u8]) -> Result<(), ForgeError> {
    use std::io::Write;
    let Some(p) = path(state) else {
        return Err(ForgeError::Io("this engine has no data directory to keep them in".into()));
    };
    let tmp = p.with_extension("json.tmp");
    let mut f = std::fs::File::create(&tmp).map_err(|e| ForgeError::Io(e.to_string()))?;
    f.write_all(body).map_err(|e| ForgeError::Io(e.to_string()))?;
    f.sync_all().map_err(|e| ForgeError::Io(e.to_string()))?;
    std::fs::rename(&tmp, &p).map_err(|e| ForgeError::Io(e.to_string()))
}

fn keep(state: &AppState, settings: &NvmeofExportConfig) -> Result<(), ForgeError> {
    let body = serde_json::to_vec_pretty(settings).map_err(|e| ForgeError::Io(e.to_string()))?;
    write(state, &body)
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
    start_as(state, settings, keep_it, Source::Api).await
}

async fn start_as(
    state: &Arc<AppState>,
    settings: NvmeofExportConfig,
    keep_it: bool,
    source: Source,
) -> Result<(), ForgeError> {
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
    forge.source = Some(source);
    if keep_it {
        keep(state, &settings)?;
    }
    forge.off = false;
    tracing::info!("forge mode: NVMe/TCP target on {} ({})", settings.listen_addr, settings.nqn);
    Ok(())
}

/// Turn forge mode off, and keep it off (#287): `forge.json` says so, so a
/// node's default does not turn it back on at the next start. No new
/// connections; returns how many are still being served (they finish on
/// their own).
pub async fn stop(state: &Arc<AppState>) -> Result<usize, ForgeError> {
    let mut forge = state.forge.lock().await;
    if forge.source == Some(Source::Config) {
        return Err(ForgeError::Configured);
    }
    // Written first: an off that is not kept is not the off asked for.
    // Nothing running and nowhere to keep it (an appliance's daemon with no
    // data directory) is off already.
    if path(state).is_some() || forge.source.is_some() {
        write(state, b"{\"enabled\": false}\n")?;
    }
    forge.off = true;
    if forge.source.is_none() {
        return Ok(0);
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
    tracing::info!("forge mode off ({live} connection(s) left to finish)");
    Ok(live)
}

/// At start, when nothing configured a target of its own: serve the forge
/// this node keeps; with nothing kept, serve `default` (a node's, #287;
/// `None` for the daemon, which stays off).
pub async fn restore(state: &Arc<AppState>, default: Option<NvmeofExportConfig>) {
    let (settings, source) = match load(state) {
        Some(Persisted::On(s)) => (s, Source::Api),
        Some(Persisted::Off) => {
            state.forge.lock().await.off = true;
            println!("  forge mode: off ({FORGE_FILE} says so)");
            tracing::info!("forge mode: off ({FORGE_FILE} says so)");
            return;
        }
        None => match default {
            Some(d) => (d, Source::Default),
            None => return,
        },
    };
    let what = match source {
        Source::Default => "this node's default",
        _ => FORGE_FILE,
    };
    let (addr, nqn) = (settings.listen_addr.clone(), settings.nqn.clone());
    match start_as(state, settings, false, source).await {
        Ok(()) => println!("  forge mode: NVMe/TCP target on {addr} ({nqn}), from {what}"),
        Err(e) => {
            println!("  forge mode: not serving {what}: {e}");
            tracing::error!("forge mode: not serving {what}: {e}");
        }
    }
}

/// What `GET /api/v1/forge` says.
pub async fn status(state: &AppState) -> Value {
    let (source, off) = {
        let f = state.forge.lock().await;
        (f.source, f.off)
    };
    let target = state.nvmeof_target.read().await.clone();
    let settings = state.nvmeof_settings();
    // Where the state came from: the configuration, what this node was told
    // and kept, or its default (on for a node, off for an appliance's daemon).
    let from = match source {
        Some(Source::Config) => "config",
        Some(Source::Api) => "persisted",
        Some(Source::Default) => "default",
        None if off => "persisted",
        None => "default",
    };
    json!({
        "state": if target.is_some() { "on" } else { "off" },
        "from": from,
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
