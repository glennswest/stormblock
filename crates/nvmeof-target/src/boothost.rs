//! The boothost HTTP surface a booting node asks before it attaches (#474):
//! what stormbootx and the initramfs's `boot-claim` call on forge, enough
//! for a node to claim and attach the one image a [`BootTarget`] serves.
//!
//! - `GET  /api/v1/health`: `version` must read above 19.3.0, or stormbootx
//!   makes no MAC-keyed default claims.
//! - `GET  /api/v1/synonyms/boothost/<name>/intent`: `{host, intent}`.
//! - `POST /api/v1/synonyms/boothost/<name>/claim`: a fresh overlay at a new
//!   NSID; `default` takes the machine's `mac` (a 400 without one), as forge
//!   does (#200).
//! - `POST /api/v1/synonyms/boothost/<name>/installed`: `{volume, stage}`;
//!   the intent goes back to `local` once the disk is laid, one-shot as on
//!   forge (#220).
//! - `GET  /api/v1/boothost`: the hosts and their claims, for the app.
//!
//! **Key order matters.** stormbootx reads a reply by finding the first
//! `"key"` anywhere in the body, not by parsing it: `attach` is the first key
//! (serde_json orders keys), nothing before it may carry `address`, `nqn`,
//! `port` or `nsid`, and `host` holds no nested object.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::server::BootTarget;

/// The API version this surface speaks: the engine's, whose claim reply it
/// reproduces. stormbootx compares it with 19.3.0.
pub const API_VERSION: &str = "21.2.1";

/// What a host is asked to do on its next boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    Install,
    Local,
    Auto,
}

impl Intent {
    pub fn as_str(self) -> &'static str {
        match self {
            Intent::Install => "install",
            Intent::Local => "local",
            Intent::Auto => "auto",
        }
    }

    pub fn parse(s: &str) -> Option<Intent> {
        match s.trim().to_ascii_lowercase().as_str() {
            "install" => Some(Intent::Install),
            "local" => Some(Intent::Local),
            "auto" => Some(Intent::Auto),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct HostRec {
    intent: Intent,
    mac: Option<String>,
    provisional: bool,
    /// The claims made, newest last: (volume id, nsid).
    claims: Vec<(String, u32)>,
    /// What the node said of its install.
    installed: Option<String>,
}

/// How hosts are let in.
#[derive(Debug, Clone)]
pub struct Policy {
    /// Hosts known by name, with their MAC when given (`name` or
    /// `name=aa:bb:cc:dd:ee:ff`).
    pub hosts: Vec<(String, Option<String>)>,
    /// Claims for any name are served (a laptop on a bench).
    pub any_host: bool,
    /// The intent every host starts with.
    pub intent: Intent,
    /// The NVMe/TCP port the target listens on.
    pub nvme_port: u16,
    /// The address handed to a node; else the one its own connection reached.
    pub advertise: Option<IpAddr>,
}

/// The surface's state.
pub struct Boothost {
    target: Arc<BootTarget>,
    policy: Policy,
    hosts: Mutex<BTreeMap<String, HostRec>>,
}

impl Boothost {
    pub fn new(target: Arc<BootTarget>, policy: Policy) -> Arc<Boothost> {
        let hosts = policy
            .hosts
            .iter()
            .map(|(n, mac)| {
                let rec = HostRec {
                    intent: policy.intent,
                    mac: mac.as_deref().map(normal_mac),
                    provisional: false,
                    claims: Vec::new(),
                    installed: None,
                };
                (n.clone(), rec)
            })
            .collect();
        Arc::new(Boothost { target, policy, hosts: Mutex::new(hosts) })
    }

    /// The routes. Serve with `into_make_service_with_connect_info::<SocketAddr>()`:
    /// a claim's address is the one the node's request reached.
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/api/v1/health", get(health))
            .route("/api/v1/boothost", get(list))
            .route("/api/v1/synonyms/boothost/{name}/intent", get(intent))
            .route("/api/v1/synonyms/boothost/{name}/claim", post(claim))
            .route("/api/v1/synonyms/boothost/{name}/installed", post(installed))
            .with_state(self.clone())
    }

    /// A host's intent as it stands.
    pub fn intent_of(&self, name: &str) -> Option<Intent> {
        self.hosts.lock().unwrap_or_else(|e| e.into_inner()).get(name).map(|h| h.intent)
    }

    /// The host a request names: by name, by a known host's MAC when the
    /// name is a MAC, or (any host) a new one.
    fn resolve(&self, name: &str, mac: Option<&str>) -> Result<String, (StatusCode, String)> {
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        let mac = mac.map(normal_mac).or_else(|| mac_of_key(name));
        if name != "default" && hosts.contains_key(name) {
            return Ok(name.to_string());
        }
        if let Some(m) = &mac {
            if let Some((n, _)) = hosts.iter().find(|(_, h)| h.mac.as_deref() == Some(m)) {
                return Ok(n.clone());
            }
        }
        if name == "default" && mac.is_none() {
            return Err((StatusCode::BAD_REQUEST, "a default claim needs the machine's mac".into()));
        }
        if !self.policy.any_host {
            return Err((StatusCode::NOT_FOUND, format!("no image for boothost/{name} on this server")));
        }
        let (n, provisional) = match (&mac, name) {
            (Some(m), "default") => (format!("mac-{}", m.replace(':', "")), true),
            (Some(m), n) if mac_of_key(n).is_some() => (format!("mac-{}", m.replace(':', "")), true),
            _ => (name.to_string(), false),
        };
        hosts.entry(n.clone()).or_insert(HostRec {
            intent: self.policy.intent,
            mac: mac.clone(),
            provisional,
            claims: Vec::new(),
            installed: None,
        });
        Ok(n)
    }
}

/// `aa:bb:…` from any spelling of a MAC.
fn normal_mac(m: &str) -> String {
    let hex: String = m.chars().filter(|c| c.is_ascii_hexdigit()).collect::<String>().to_ascii_lowercase();
    hex.as_bytes().chunks(2).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join(":")
}

/// A key that is a MAC as stormbootx sends it (12 hex digits), as `aa:bb:…`.
fn mac_of_key(key: &str) -> Option<String> {
    (key.len() == 12 && key.chars().all(|c| c.is_ascii_hexdigit())).then(|| normal_mac(key))
}

fn error(status: StatusCode, msg: String) -> Response {
    (status, Json(json!({"error": msg, "code": status.as_u16()}))).into_response()
}

async fn health(State(b): State<Arc<Boothost>>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "stormboot-serve",
        "version": API_VERSION,
        "crate_version": env!("CARGO_PKG_VERSION"),
        "auth": "none",
        "image": b.target.image().path().display().to_string(),
        "image_bytes": b.target.image().len(),
    }))
}

async fn list(State(b): State<Arc<Boothost>>) -> Json<Value> {
    let hosts = b.hosts.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let rows: Vec<Value> = hosts
        .iter()
        .map(|(n, h)| {
            let claims: Vec<Value> = h
                .claims
                .iter()
                .map(|(v, nsid)| {
                    let o = b.target.overlay(n, *nsid);
                    json!({
                        "volume": v, "nsid": nsid,
                        "written_blocks": o.as_ref().map(|o| o.written_blocks()),
                        "scratch_bytes": o.as_ref().map(|o| o.scratch_bytes()),
                    })
                })
                .collect();
            json!({"name": n, "intent": h.intent.as_str(), "mac": h.mac, "provisional": h.provisional,
                   "installed": h.installed, "claims": claims})
        })
        .collect();
    Json(json!({"hosts": rows}))
}

async fn intent(State(b): State<Arc<Boothost>>, Path(name): Path<String>) -> Response {
    let host = {
        let hosts = b.hosts.lock().unwrap_or_else(|e| e.into_inner());
        let mac = mac_of_key(&name);
        hosts
            .iter()
            .find(|(n, h)| **n == name || (mac.is_some() && h.mac == mac))
            .map(|(n, h)| (n.clone(), h.intent))
    };
    match host {
        Some((n, i)) => Json(json!({"host": n, "intent": i.as_str()})).into_response(),
        None if b.policy.any_host => Json(json!({"host": name, "intent": b.policy.intent.as_str()})).into_response(),
        None => error(StatusCode::NOT_FOUND, format!("no host {name} on this server")),
    }
}

#[derive(Deserialize, Default)]
struct ClaimBody {
    #[serde(default)]
    mac: Option<String>,
    #[serde(default)]
    serial: Option<String>,
    #[serde(default)]
    agent: Option<Value>,
}

/// The address a node should dial: the advertised one, else the source this
/// machine routes toward the node from (the address its request reached).
fn address_for(b: &Boothost, peer: SocketAddr) -> Option<IpAddr> {
    if let Some(a) = b.policy.advertise {
        return Some(a);
    }
    let bind = if peer.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let s = std::net::UdpSocket::bind(bind).ok()?;
    s.connect(SocketAddr::new(peer.ip(), 9)).ok()?;
    s.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_unspecified())
}

async fn claim(
    State(b): State<Arc<Boothost>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(name): Path<String>,
    body: Option<Json<ClaimBody>>,
) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let host = match b.resolve(&name, body.mac.as_deref()) {
        Ok(h) => h,
        Err((status, msg)) => return error(status, msg),
    };
    let Some(address) = address_for(&b, peer) else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "no address to hand the node; start with --advertise".into());
    };
    let c = match b.target.claim(&host) {
        Ok(c) => c,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("making {host}'s overlay: {e}")),
    };
    let agent = body.agent.as_ref().and_then(|a| a.get("name")).and_then(|n| n.as_str()).unwrap_or("?").to_string();
    let volume = format!("stormboot-{host}-{}", c.nsid);
    let (intent, provisional, mac) = {
        let mut hosts = b.hosts.lock().unwrap_or_else(|e| e.into_inner());
        let rec = hosts.get_mut(&host).expect("resolved");
        if rec.mac.is_none() {
            rec.mac = body.mac.as_deref().map(normal_mac);
        }
        rec.claims.push((volume.clone(), c.nsid));
        let keep = rec.claims.len().saturating_sub(2);
        rec.claims.drain(..keep);
        (rec.intent, rec.provisional, rec.mac.clone())
    };
    tracing::info!(
        "claim of boothost/{name} by {peer} ({agent}{}): {host}, nsid {}, intent {}",
        body.serial.as_deref().map(|s| format!(", serial {s}")).unwrap_or_default(),
        c.nsid,
        intent.as_str()
    );
    let port = b.policy.nvme_port;
    let uri = format!("nvme-tcp://{address}:{port}/{}?nsid={}", c.subsystem, c.nsid);
    let reply = json!({
        "attach": {
            "protocol": "nvme-tcp",
            "address": address.to_string(),
            "port": port,
            "nqn": c.subsystem,
            "nsid": c.nsid,
            "uri": uri,
            "host_nqns": [c.host_nqn],
        },
        "host": {"name": host, "claimed_as": name, "provisional": provisional, "mac": mac},
        "intent": intent.as_str(),
        "volume": {"id": volume, "name": format!("boothost-{host}"), "size_bytes": b.target.image().blocks()
            * b.target.image().block_size() as u64, "sealed": false, "access": "rw"},
    });
    (StatusCode::CREATED, Json(reply)).into_response()
}

#[derive(Deserialize)]
struct InstalledBody {
    volume: String,
    #[serde(default)]
    stage: Option<String>,
}

async fn installed(State(b): State<Arc<Boothost>>, Path(name): Path<String>, Json(body): Json<InstalledBody>) -> Response {
    let mut hosts = b.hosts.lock().unwrap_or_else(|e| e.into_inner());
    let Some(rec) = hosts.get_mut(&name) else {
        return error(StatusCode::NOT_FOUND, format!("no host {name} on this server"));
    };
    if !rec.claims.iter().any(|(v, _)| *v == body.volume) {
        return error(StatusCode::CONFLICT, format!("{} is not a claim of {name}", body.volume));
    }
    let stage = body.stage.unwrap_or_else(|| "laid".into());
    if stage == "laid" && rec.intent == Intent::Install {
        rec.intent = Intent::Local;
    }
    rec.installed = Some(stage.clone());
    tracing::info!("{name}: install reported {stage}; intent {}", rec.intent.as_str());
    Json(json!({"host": name, "stage": stage, "intent": rec.intent.as_str()})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs_are_one_spelling() {
        assert_eq!(normal_mac("AC1F6B8AA396"), "ac:1f:6b:8a:a3:96");
        assert_eq!(normal_mac("ac-1f-6b-8a-a3-96"), "ac:1f:6b:8a:a3:96");
        assert_eq!(mac_of_key("ac1f6b8aa396").as_deref(), Some("ac:1f:6b:8a:a3:96"));
        assert_eq!(mac_of_key("server8"), None);
    }
}
