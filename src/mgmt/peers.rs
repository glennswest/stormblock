//! A volume on another node, attached here as a local device (#328,
//! stormvm#46).
//!
//! stormvm and the kubelet attach and clone through their *own* node's
//! engine, by volume id, and open whatever `device_hint` comes back. Every
//! volume in this engine's manager is backed here and answers ublk; a volume
//! that lives on another node used to be a 404. With `[fabric] peers` set
//! (the master's decision on #328: 1a peer list, 2a node certificate,
//! 3b this engine's own initiator served through ublk):
//!
//! 1. the volume is looked for on each peer in turn (`GET
//!    /api/v1/volumes/{id}`), presenting this node's certificate, which a
//!    peer's `tls_client_ca` admits at the node-token tier (#203);
//! 2. a clone of a volume a peer holds is made *there* and its answer
//!    relayed, so "a VM on B off a clone of A's golden" is two calls to B;
//! 3. an attach asks the peer to serve the volume over NVMe/TCP to this
//!    node's own host NQN (its per-host subsystem, #210, with the
//!    DH-HMAC-CHAP secret it hands out), connects with the engine's
//!    initiator (the same `nvme-tcp://` device a drive or a flow-over
//!    source is) and serves that as a ublk device: `device_hint` is
//!    `/dev/ublkbN`, with the same detach, mounted check and hold rules as a
//!    local volume;
//! 4. a detach takes the ublk device down and releases the peer's
//!    namespace (`DELETE …/attach?host_nqn=&holder=`).
//!
//! What is attached is kept in `<data_dir>/peer_attaches.json`. A ublk
//! export does not outlive the engine that serves it, so at start every
//! record left there is released at its peer and dropped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::drive::BlockDevice;
use crate::mgmt::AppState;

const RECORD: &str = "peer_attaches.json";

/// One volume of a peer's, attached here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerAttach {
    pub volume: Uuid,
    /// The peer's management API it was attached from.
    pub peer: String,
    /// What this node connected as.
    pub host_nqn: String,
    /// What the peer's attach was held as (#276), so the detach releases
    /// this node's hold and nobody else's.
    pub holder: String,
    /// The namespace connected (no secret: a URI is logged and persisted).
    pub uri: String,
    /// The ublk device it is served as.
    pub device_hint: String,
}

/// Every peer volume attached here, by volume id.
#[derive(Debug, Default)]
pub struct PeerAttaches {
    path: Option<PathBuf>,
    map: HashMap<Uuid, PeerAttach>,
}

impl PeerAttaches {
    pub fn load(data_dir: Option<&Path>) -> Self {
        let path = data_dir.map(|d| d.join(RECORD));
        let map = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<Vec<PeerAttach>>(&b).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|a| (a.volume, a))
            .collect();
        PeerAttaches { path, map }
    }

    pub fn get(&self, volume: &Uuid) -> Option<&PeerAttach> {
        self.map.get(volume)
    }

    pub fn all(&self) -> Vec<PeerAttach> {
        self.map.values().cloned().collect()
    }

    fn save(&self) {
        let Some(p) = &self.path else { return };
        let mut v: Vec<&PeerAttach> = self.map.values().collect();
        v.sort_by_key(|a| a.volume);
        match serde_json::to_vec_pretty(&v) {
            Ok(b) => {
                if let Err(e) = crate::serve::wiring::write_atomic(p, &b) {
                    tracing::warn!("peer attaches: writing {}: {e}", p.display());
                }
            }
            Err(e) => tracing::warn!("peer attaches: {e}"),
        }
    }

    fn insert(&mut self, a: PeerAttach) {
        self.map.insert(a.volume, a);
        self.save();
    }

    fn remove(&mut self, volume: &Uuid) -> Option<PeerAttach> {
        let a = self.map.remove(volume);
        if a.is_some() {
            self.save();
        }
        a
    }

    fn take_all(&mut self) -> Vec<PeerAttach> {
        let all: Vec<PeerAttach> = self.map.drain().map(|(_, a)| a).collect();
        if !all.is_empty() {
            self.save();
        }
        all
    }
}

/// Why a peer could not give what was asked: an HTTP status and what to say.
#[derive(Debug)]
pub struct PeerError {
    pub status: u16,
    pub message: String,
}

impl PeerError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        PeerError { status, message: message.into() }
    }
}

/// Whether any peer is configured.
pub fn configured(state: &AppState) -> bool {
    !state.config.fabric.peers.is_empty()
}

/// The host NQN this node connects to a peer as: `[fabric] host_nqn`, else
/// `/etc/nvme/hostnqn`, else `nqn.2026-09.lo.storm:host-<node>` (the name
/// stormbootx gives a boot host, #210).
pub async fn host_nqn(state: &AppState) -> String {
    if let Some(n) = state.config.fabric.host_nqn.as_deref().filter(|n| !n.trim().is_empty()) {
        return n.trim().to_string();
    }
    if let Ok(n) = std::fs::read_to_string("/etc/nvme/hostnqn") {
        let n = n.trim();
        if !n.is_empty() {
            return n.to_string();
        }
    }
    let node = state.v1.lock().await.local_node.clone();
    format!("nqn.2026-09.lo.storm:host-{node}")
}

/// The client a peer is asked with: this node's certificate when it has
/// one, and the node CA to check the peer's.
fn client(state: &AppState) -> Result<crate::http::Client, PeerError> {
    let (f, m) = (&state.config.fabric, &state.config.management);
    let mut b = crate::http::Client::builder().timeout(std::time::Duration::from_secs(30));
    if let Some(ca) = f.ca(m) {
        let pem = std::fs::read(ca).map_err(|e| PeerError::new(500, format!("fabric CA {ca}: {e}")))?;
        b = b.add_root_certificate_pem(pem);
    }
    match (f.client_cert(m), f.client_key(m)) {
        (Some(c), Some(k)) => {
            let cert = std::fs::read(c).map_err(|e| PeerError::new(500, format!("fabric certificate {c}: {e}")))?;
            let key = std::fs::read(k).map_err(|e| PeerError::new(500, format!("fabric key {k}: {e}")))?;
            b = b.identity_pem(cert, key);
        }
        _ => tracing::debug!("fabric: no client certificate; a peer sees no credential"),
    }
    b.build().map_err(|e| PeerError::new(500, format!("fabric client: {e}")))
}

fn base(peer: &str) -> &str {
    peer.trim_end_matches('/')
}

/// The peer holding `key` (a volume id or name) and what it says of it, or
/// `None` when no peer does. A peer that does not answer is skipped, said.
pub async fn find(state: &AppState, key: &str) -> Result<Option<(String, serde_json::Value)>, PeerError> {
    if !configured(state) {
        return Ok(None);
    }
    let c = client(state)?;
    let mut refused = Vec::new();
    for peer in &state.config.fabric.peers {
        let url = format!("{}/api/v1/volumes/{key}", base(peer));
        match c.get(&url).send().await {
            Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
                Ok(v) => return Ok(Some((peer.clone(), v))),
                Err(e) => tracing::warn!("fabric: {url}: {e}"),
            },
            Ok(r) if r.status().as_u16() == 404 => {}
            Ok(r) => {
                let code = r.status().as_u16();
                let body = r.text().await.unwrap_or_default();
                tracing::warn!("fabric: {url}: {code} {body}");
                refused.push(format!("{peer}: {code}"));
            }
            Err(e) => tracing::warn!("fabric: {url}: not reachable: {e}"),
        }
    }
    if !refused.is_empty() {
        tracing::warn!(
            "fabric: volume {key} not found on any peer that answered; refused by {} \
             (does its tls_client_ca take this node's certificate?)",
            refused.join(", ")
        );
    }
    Ok(None)
}

/// A clone of a peer's volume, made on that peer: its status and body, or
/// `None` when no peer holds `key`.
pub async fn clone(state: &AppState, key: &str, body: &[u8]) -> Result<Option<(u16, serde_json::Value)>, PeerError> {
    let Some((peer, vol)) = find(state, key).await? else { return Ok(None) };
    let id = vol["id"].as_str().unwrap_or(key).to_string();
    let c = client(state)?;
    let url = format!("{}/api/v1/volumes/{id}/clone", base(&peer));
    let r = c
        .post(&url)
        .body(body.to_vec())
        .send()
        .await
        .map_err(|e| PeerError::new(502, format!("{url}: {e}")))?;
    let code = r.status().as_u16();
    let mut v: serde_json::Value = r.json().await.unwrap_or(serde_json::Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("peer".into(), serde_json::Value::String(peer));
    }
    Ok(Some((code, v)))
}

/// The peer's attach answer, connected: the device and its record (no ublk
/// yet). The peer's namespace is released again when the connect fails.
pub async fn connect(
    state: &AppState,
    key: &str,
    read_only: bool,
) -> Result<Option<(Arc<dyn BlockDevice>, PeerAttach)>, PeerError> {
    let Some((peer, vol)) = find(state, key).await? else { return Ok(None) };
    let volume: Uuid = vol["id"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| PeerError::new(502, format!("{peer} answered volume {key} with no id")))?;
    let host_nqn = host_nqn(state).await;
    let node = state.v1.lock().await.local_node.clone();
    let holder = format!("peer:{node}");
    let c = client(state)?;
    let url = format!("{}/api/v1/volumes/{volume}/attach", base(&peer));
    let r = c
        .post(&url)
        .json(&serde_json::json!({
            "transport": "nvme_tcp",
            "host_nqn": host_nqn,
            "holder": holder,
            "mode": if read_only { "ro" } else { "rw" },
        }))
        .send()
        .await
        .map_err(|e| PeerError::new(502, format!("{url}: {e}")))?;
    let code = r.status().as_u16();
    if !r.status().is_success() {
        let body = r.text().await.unwrap_or_default();
        // The peer's own refusal (sealed, read-only) is the caller's answer.
        return Err(PeerError::new(if code == 409 || code == 400 { code } else { 502 }, format!("{peer}: {body}")));
    }
    let info: crate::mgmt::api::v1::AttachInfo =
        r.json().await.map_err(|e| PeerError::new(502, format!("{url}: {e}")))?;
    let crate::mgmt::api::v1::AttachInfo::NvmeTcp { nqn, addresses, nsid, dhchap_secret, .. } = info else {
        release(state, &peer, volume, &host_nqn, &holder).await;
        return Err(PeerError::new(502, format!("{peer} answered the attach with no NVMe/TCP namespace")));
    };
    let addr = addresses.iter().map(|a| format_addr(&a.traddr, a.trsvcid)).collect::<Vec<_>>().join(",");
    let mut spec = crate::drive::nvmeof_dev::NvmeTcpSpec {
        addr,
        nqn,
        nsid: nsid.unwrap_or(1),
        host_nqn: Some(host_nqn.clone()),
        ..Default::default()
    };
    if let Some(s) = dhchap_secret.as_deref() {
        match crate::target::nvmeof::auth::DhchapKey::parse(s) {
            Ok(k) => spec.dhchap = Some(k),
            Err(e) => {
                release(state, &peer, volume, &host_nqn, &holder).await;
                return Err(PeerError::new(502, format!("{peer}'s DH-HMAC-CHAP secret: {e}")));
            }
        }
    }
    let uri = spec.uri();
    let dev = match crate::drive::nvmeof_dev::NvmeofDevice::connect(&spec).await {
        Ok(d) => Arc::new(d) as Arc<dyn BlockDevice>,
        Err(e) => {
            release(state, &peer, volume, &host_nqn, &holder).await;
            return Err(PeerError::new(502, format!("connecting {uri}: {e}")));
        }
    };
    tracing::info!("fabric: volume {volume} on {peer} connected as {host_nqn}: {uri}");
    Ok(Some((dev, PeerAttach { volume, peer, host_nqn, holder, uri, device_hint: String::new() })))
}

fn format_addr(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Attach a peer's volume here: its ublk device, or `None` when no peer
/// holds `key`. A repeat attach answers the device it already has.
pub async fn attach(state: &Arc<AppState>, key: &str, read_only: bool) -> Result<Option<String>, PeerError> {
    if let Ok(u) = key.parse::<Uuid>() {
        let have = state.peer_attaches.lock().await.get(&u).map(|a| a.device_hint.clone());
        if let Some(path) = have {
            if state.ublk_exports.lock().await.device_path(&u.to_string()).as_deref() == Some(path.as_str()) {
                return Ok(Some(path));
            }
        }
    }
    let Some((dev, mut rec)) = connect(state, key, read_only).await? else { return Ok(None) };
    let id = rec.volume.to_string();
    match crate::mgmt::ublk_export::attach(state, &id, dev).await {
        Some(path) => {
            rec.device_hint = path.clone();
            state.peer_attaches.lock().await.insert(rec);
            Ok(Some(path))
        }
        None => {
            release(state, &rec.peer, rec.volume, &rec.host_nqn, &rec.holder).await;
            Err(PeerError::new(
                409,
                format!(
                    "volume {} is on {}: this node serves a peer's volume as a ublk device, \
                     and ublk is not available here (kernel ublk_drv, or ublk_transport is off)",
                    rec.volume, rec.peer
                ),
            ))
        }
    }
}

/// Take a peer's volume off this node: the ublk device down, the peer's
/// namespace released. `None` when it is not one attached here. The caller
/// checks the device is not mounted first.
pub async fn detach(state: &Arc<AppState>, volume: Uuid) -> Option<PeerAttach> {
    let rec = state.peer_attaches.lock().await.get(&volume).cloned()?;
    let stopping = state.ublk_exports.lock().await.remove(&volume.to_string());
    stopping.settle(crate::mgmt::ublk_export::DETACH_WAIT).await;
    release(state, &rec.peer, rec.volume, &rec.host_nqn, &rec.holder).await;
    state.peer_attaches.lock().await.remove(&volume);
    Some(rec)
}

/// The peer's namespace let go of (this node's hold only).
async fn release(state: &AppState, peer: &str, volume: Uuid, host_nqn: &str, holder: &str) {
    let c = match client(state) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("fabric: releasing {volume} on {peer}: {}", e.message);
            return;
        }
    };
    let url = format!(
        "{}/api/v1/volumes/{volume}/attach?host_nqn={}&holder={}",
        base(peer),
        urlencode(host_nqn),
        urlencode(holder)
    );
    match c.delete(&url).send().await {
        Ok(r) if r.status().is_success() || r.status().as_u16() == 404 => {}
        Ok(r) => tracing::warn!("fabric: releasing {volume} on {peer}: {}", r.status().as_u16()),
        Err(e) => tracing::warn!("fabric: releasing {volume} on {peer}: {e}"),
    }
}

fn urlencode(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

/// At start: what an earlier run attached is gone with it (a ublk export
/// does not outlive its engine), so each peer's namespace is let go of and
/// the record dropped.
pub fn start(state: &Arc<AppState>) {
    let state = state.clone();
    tokio::spawn(async move {
        let stale = state.peer_attaches.lock().await.take_all();
        for a in stale {
            tracing::info!("fabric: volume {} on {} was attached by an earlier run; releasing it there", a.volume, a.peer);
            release(&state, &a.peer, a.volume, &a.host_nqn, &a.holder).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_survive_a_reload() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut a = PeerAttaches::load(Some(dir.path()));
        let v = Uuid::new_v4();
        a.insert(PeerAttach {
            volume: v,
            peer: "https://a:9090".into(),
            host_nqn: "nqn.x:host-b".into(),
            holder: "peer:b".into(),
            uri: "nvme-tcp://10.0.0.1:4420/nqn.y?nsid=1".into(),
            device_hint: "/dev/ublkb3".into(),
        });
        let b = PeerAttaches::load(Some(dir.path()));
        assert_eq!(b.get(&v).unwrap().device_hint, "/dev/ublkb3");
        let mut b = b;
        assert_eq!(b.take_all().len(), 1);
        assert!(PeerAttaches::load(Some(dir.path())).all().is_empty());
    }

    #[test]
    fn query_values_are_encoded() {
        assert_eq!(urlencode("nqn.2026-09.lo.storm:host-b"), "nqn.2026-09.lo.storm%3Ahost-b");
        assert_eq!(format_addr("fd00::1", 4420), "[fd00::1]:4420");
        assert_eq!(format_addr("10.0.0.1", 4420), "10.0.0.1:4420");
    }
}
