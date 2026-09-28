//! Per-host NVMe/TCP subsystems (#210).
//!
//! A node used to serve every volume it exported as a namespace of one
//! subsystem that admitted any host, so anything that could reach `:4420`
//! saw every golden, every release and every machine's boot clone. Now a
//! volume attached *for a host* goes into that host's own subsystem, whose
//! only allowed host is that host's NQN — optionally with a DH-HMAC-CHAP
//! secret it must prove it holds — and a host sees only what was attached
//! to it. The shared subsystem is closed unless the configuration opens it.
//!
//! The table here is the record: which subsystem, which hosts (and their
//! secrets), which volumes at which NSID. It is persisted to
//! `<data_dir>/nvme_hosts.json` (mode 0600: it holds secrets) and put back
//! on the target at startup, because an NQN and an NSID are an address a
//! machine has written down.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::mgmt::AppState;
use crate::target::nvmeof::auth::DhchapKey;
use crate::target::nvmeof::HostAccess;
use crate::volume::VolumeId;

/// Default host-NQN template for boot hosts: what stormbootx presents
/// (`nqn.2026-09.lo.storm:host-<name>`), `{name}` being the host's name as
/// the claim reply gives it.
pub const DEFAULT_BOOTHOST_HOST_NQN: &str = "nqn.2026-09.lo.storm:host-{name}";

/// One host allowed on a subsystem.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostEntry {
    pub nqn: String,
    /// `DHHC-1:…` when this host must authenticate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dhchap_secret: Option<String>,
}

/// One volume served in a subsystem.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NsRecord {
    pub volume: Uuid,
    pub nsid: u32,
    #[serde(default)]
    pub read_only: bool,
}

/// A subsystem of this node's own, and who may connect to it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostSubsystem {
    pub nqn: String,
    pub hosts: Vec<HostEntry>,
    #[serde(default)]
    pub namespaces: Vec<NsRecord>,
    /// The boot host this subsystem serves, when a claim made it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boothost: Option<String>,
}

impl HostSubsystem {
    fn access(&self) -> HostAccess {
        let mut m = HashMap::new();
        for h in &self.hosts {
            let key = h.dhchap_secret.as_deref().and_then(|s| match DhchapKey::parse(s) {
                Ok(k) => Some(k),
                Err(e) => {
                    // A stored secret that does not parse must not open the
                    // door: the host stays listed, and will fail to
                    // authenticate against a key it cannot know.
                    tracing::error!("nvme host {} on {}: stored secret unusable ({e})", h.nqn, self.nqn);
                    Some(DhchapKey::generate())
                }
            });
            m.insert(h.nqn.clone(), key);
        }
        HostAccess::Hosts(m)
    }

    fn host_mut(&mut self, nqn: &str) -> &mut HostEntry {
        if let Some(i) = self.hosts.iter().position(|h| h.nqn == nqn) {
            return &mut self.hosts[i];
        }
        self.hosts.push(HostEntry { nqn: nqn.to_string(), dhchap_secret: None });
        self.hosts.last_mut().unwrap()
    }
}

/// Every host subsystem this node keeps, by NQN.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NvmeHosts {
    #[serde(default)]
    pub subsystems: BTreeMap<String, HostSubsystem>,
}

impl NvmeHosts {
    /// Where the volume is served, as `(subsystem, nsid)`.
    pub fn serving(&self, volume: Uuid) -> Vec<(String, u32)> {
        self.subsystems
            .values()
            .flat_map(|s| {
                s.namespaces
                    .iter()
                    .filter(move |n| n.volume == volume)
                    .map(move |n| (s.nqn.clone(), n.nsid))
            })
            .collect()
    }
}

/// The node's NVMe access policy, from `[nvmeof]`.
#[derive(Debug, Clone)]
pub struct Policy {
    /// The shared subsystem admits any host (the old behaviour).
    pub allow_any_host: bool,
    /// Hosts the shared subsystem admits when it is not open to all.
    pub allowed_hosts: Vec<String>,
    /// Every host subsystem's hosts get a secret, asked for or not.
    pub require_dhchap: bool,
    /// Template for the host NQN a boot host presents.
    pub boothost_host_nqn: String,
}

pub fn policy(state: &AppState) -> Policy {
    match state.config.nvmeof.as_ref() {
        Some(n) => Policy {
            allow_any_host: n.allow_any_host,
            allowed_hosts: n.allowed_hosts.clone(),
            require_dhchap: n.require_dhchap,
            boothost_host_nqn: n
                .boothost_host_nqn
                .clone()
                .unwrap_or_else(|| DEFAULT_BOOTHOST_HOST_NQN.to_string()),
        },
        None => Policy {
            allow_any_host: false,
            allowed_hosts: Vec::new(),
            require_dhchap: false,
            boothost_host_nqn: DEFAULT_BOOTHOST_HOST_NQN.to_string(),
        },
    }
}

impl Policy {
    /// Who the shared subsystem admits.
    pub fn shared_access(&self) -> HostAccess {
        if self.allow_any_host {
            HostAccess::Any
        } else {
            HostAccess::Hosts(self.allowed_hosts.iter().map(|h| (h.clone(), None)).collect())
        }
    }

    /// Whether anything can reach the shared subsystem at all.
    pub fn shared_reachable(&self) -> bool {
        self.allow_any_host || !self.allowed_hosts.is_empty()
    }

    /// The host NQNs a boot host `name` (and its aliases) may present.
    pub fn boothost_nqns(&self, name: &str, aliases: &[String]) -> Vec<String> {
        let mut v = vec![self.boothost_host_nqn.replace("{name}", name)];
        for a in aliases {
            let n = self.boothost_host_nqn.replace("{name}", a);
            if !v.contains(&n) {
                v.push(n);
            }
        }
        v
    }
}

/// Keep what an NQN may carry and nothing that could be read as structure.
fn nqn_safe(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') { c } else { '-' })
        .take(96)
        .collect()
}

/// The subsystem a host NQN is served from: stable, and saying nothing
/// about the host beyond "one host".
pub fn host_subsystem_nqn(base: &str, host_nqn: &str) -> String {
    use sha2::Digest;
    let h = sha2::Sha256::digest(host_nqn.as_bytes());
    format!("{base}:host:{}", hex::encode(&h[..8]))
}

/// The subsystem a boot host is served from.
pub fn boothost_subsystem_nqn(base: &str, name: &str) -> String {
    format!("{base}:host:{}", nqn_safe(name))
}

fn store_path(state: &AppState) -> Option<PathBuf> {
    state.config.management.data_dir.as_ref().map(|d| PathBuf::from(d).join("nvme_hosts.json"))
}

fn persist(state: &AppState, hosts: &NvmeHosts) {
    let Some(path) = store_path(state) else { return };
    let bytes = match serde_json::to_vec_pretty(hosts) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("nvme hosts: cannot serialize: {e}");
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    let written = {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&tmp).and_then(|mut f| f.write_all(&bytes).and_then(|_| f.sync_all()))
    };
    if let Err(e) = written.and_then(|_| std::fs::rename(&tmp, &path)) {
        tracing::warn!("nvme hosts: cannot write {}: {e}", path.display());
    }
}

/// What an attach for a host answers with.
#[derive(Debug, Clone)]
pub struct HostAttach {
    pub nqn: String,
    pub nsid: u32,
    pub host_nqn: String,
    pub dhchap_secret: Option<String>,
}

async fn target(state: &AppState) -> Result<Arc<crate::target::nvmeof::NvmeofTarget>, String> {
    state
        .nvmeof_target
        .read()
        .await
        .as_ref()
        .cloned()
        .ok_or_else(|| "this node serves no NVMe-oF target".to_string())
}

/// Serve `volume` to `host_nqn` from that host's own subsystem.
///
/// Idempotent: an attach replay answers the same NQN and NSID. `dhchap` (or
/// the node's `require_dhchap`) gives the host a secret if it has none; a
/// host that has one keeps it — dropping it would be a downgrade anyone
/// holding a token could ask for.
pub async fn attach_for_host(
    state: &AppState,
    volume: Uuid,
    host_nqn: &str,
    read_only: bool,
    dhchap: bool,
) -> Result<HostAttach, String> {
    let host_nqn = host_nqn.trim();
    if host_nqn.is_empty() || host_nqn.len() > 223 || !host_nqn.starts_with("nqn.") {
        return Err(format!("{host_nqn:?} is not a host NQN (nqn.…, at most 223 bytes)"));
    }
    let target = target(state).await?;
    let nqn = host_subsystem_nqn(target.default_subsystem().nqn(), host_nqn);
    let pol = policy(state);
    attach_into(state, &target, &nqn, &[host_nqn.to_string()], None, volume, read_only, dhchap || pol.require_dhchap)
        .await
        .map(|(nsid, secrets)| HostAttach {
            nqn,
            nsid,
            host_nqn: host_nqn.to_string(),
            dhchap_secret: secrets.into_iter().next().flatten(),
        })
}

/// Serve a boot host's clone from the host's own subsystem, admitting the
/// host NQNs it may present, and take the volumes it no longer boots
/// (`superseded`) out of it.
pub async fn attach_for_boothost(
    state: &AppState,
    name: &str,
    aliases: &[String],
    volume: Uuid,
) -> Result<(String, u32, Vec<String>), String> {
    let target = target(state).await?;
    let pol = policy(state);
    let nqn = boothost_subsystem_nqn(target.default_subsystem().nqn(), name);
    let hosts = pol.boothost_nqns(name, aliases);
    // No secret, even under `require_dhchap`: the claim is unauthenticated
    // and firmware has nowhere to be handed one, so a secret here would
    // stop every machine booting. The NQN binding is what a boot has until
    // a claim can carry a host key (stormcos#35).
    let (nsid, _) = attach_into(state, &target, &nqn, &hosts, Some(name), volume, false, false).await?;
    Ok((nqn, nsid, hosts))
}

#[allow(clippy::too_many_arguments)]
async fn attach_into(
    state: &AppState,
    target: &crate::target::nvmeof::NvmeofTarget,
    nqn: &str,
    host_nqns: &[String],
    boothost: Option<&str>,
    volume: Uuid,
    read_only: bool,
    dhchap: bool,
) -> Result<(u32, Vec<Option<String>>), String> {
    let device = state
        .volume_manager
        .lock()
        .await
        .get_volume(&VolumeId(volume))
        .ok_or_else(|| format!("volume {volume} is not on this node"))?;

    let mut hosts = state.nvme_hosts.lock().await;
    let rec = hosts.subsystems.entry(nqn.to_string()).or_insert_with(|| HostSubsystem {
        nqn: nqn.to_string(),
        ..Default::default()
    });
    if let Some(b) = boothost {
        rec.boothost = Some(b.to_string());
    }
    let mut secrets = Vec::new();
    for h in host_nqns {
        let entry = rec.host_mut(h);
        if dhchap && entry.dhchap_secret.is_none() {
            entry.dhchap_secret = Some(DhchapKey::generate().to_secret());
        }
        secrets.push(entry.dhchap_secret.clone());
    }
    let sub = target.ensure_subsystem(nqn, rec.access());
    let nsid = match rec.namespaces.iter().find(|n| n.volume == volume) {
        Some(n) => {
            // Recorded; make sure the target has it (a restart, a replay).
            if sub.nsid_of(volume).await.is_none() && !sub.add_namespace_at(n.nsid, device.clone(), n.read_only).await {
                return Err(format!("{nqn}: NSID {} is taken by another volume", n.nsid));
            }
            n.nsid
        }
        None => {
            let nsid = sub.add_namespace_next(device, read_only).await;
            rec.namespaces.push(NsRecord { volume, nsid, read_only });
            nsid
        }
    };
    let snapshot = hosts.clone();
    drop(hosts);
    persist(state, &snapshot);
    tracing::info!(%volume, "served as NVMe namespace {nsid} of {nqn} to {}", host_nqns.join(", "));
    Ok((nsid, secrets))
}

/// Stop serving `volume` to `host_nqn` (or, with `None`, to every host).
/// A host subsystem left with no namespaces is taken off the listener.
/// Returns how many namespaces went.
pub async fn detach(state: &AppState, volume: Uuid, host_nqn: Option<&str>) -> usize {
    let target = state.nvmeof_target.read().await.as_ref().cloned();
    let mut hosts = state.nvme_hosts.lock().await;
    let mut removed = 0;
    let mut empty = Vec::new();
    for (nqn, rec) in hosts.subsystems.iter_mut() {
        if let Some(h) = host_nqn {
            if !rec.hosts.iter().any(|e| e.nqn == h) {
                continue;
            }
        }
        let before = rec.namespaces.len();
        let gone: Vec<NsRecord> = rec.namespaces.iter().filter(|n| n.volume == volume).cloned().collect();
        rec.namespaces.retain(|n| n.volume != volume);
        if let Some(t) = target.as_ref() {
            if let Some(sub) = t.subsystem(nqn) {
                for n in &gone {
                    sub.remove_namespace(n.nsid).await;
                }
            }
        }
        removed += before - rec.namespaces.len();
        // A boot host's subsystem stays (its secret and hosts with it): the
        // next claim puts the next clone there.
        if rec.namespaces.is_empty() && rec.boothost.is_none() {
            empty.push(nqn.clone());
        }
    }
    for nqn in &empty {
        hosts.subsystems.remove(nqn);
        if let Some(t) = target.as_ref() {
            t.remove_subsystem(nqn);
        }
    }
    if removed > 0 {
        let snapshot = hosts.clone();
        drop(hosts);
        persist(state, &snapshot);
        tracing::info!(%volume, "withdrawn from {removed} host NVMe namespace(s)");
    }
    removed
}

/// Load the table and put every subsystem and namespace back on the target.
/// A volume no longer here is dropped from the record, loudly.
pub async fn restore(state: &AppState) -> usize {
    let Some(path) = store_path(state) else { return 0 };
    let Ok(bytes) = std::fs::read(&path) else { return 0 };
    let mut loaded: NvmeHosts = match serde_json::from_slice(&bytes) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("nvme hosts: {} does not parse ({e}); host subsystems not restored", path.display());
            return 0;
        }
    };
    let Some(target) = state.nvmeof_target.read().await.as_ref().cloned() else {
        *state.nvme_hosts.lock().await = loaded;
        return 0;
    };
    let mut restored = 0;
    for rec in loaded.subsystems.values_mut() {
        let sub = target.ensure_subsystem(&rec.nqn, rec.access());
        let mut keep = Vec::new();
        for n in rec.namespaces.drain(..) {
            let dev = state.volume_manager.lock().await.get_volume(&VolumeId(n.volume));
            match dev {
                Some(dev) if sub.add_namespace_at(n.nsid, dev.clone(), n.read_only).await => {
                    restored += 1;
                    keep.push(n);
                }
                Some(_) => tracing::warn!(
                    "nvme hosts: {} NSID {} for volume {} conflicts; dropped",
                    rec.nqn, n.nsid, n.volume
                ),
                None => tracing::warn!(
                    "nvme hosts: volume {} is gone; {} NSID {} dropped",
                    n.volume, rec.nqn, n.nsid
                ),
            }
        }
        rec.namespaces = keep;
    }
    let snapshot = loaded.clone();
    *state.nvme_hosts.lock().await = loaded;
    persist(state, &snapshot);
    if restored > 0 {
        tracing::info!("restored {restored} host NVMe namespace(s) from {}", path.display());
    }
    restored
}

/// Close (or open) the shared subsystem per the policy, and say which.
pub fn apply_shared_policy(state: &AppState, target: &crate::target::nvmeof::NvmeofTarget) {
    let pol = policy(state);
    let shared = target.default_subsystem();
    shared.set_access(pol.shared_access());
    if pol.allow_any_host {
        tracing::warn!(
            "NVMe-oF: {} admits ANY host ([nvmeof] allow_any_host = true): everything \
             attached without a host_nqn is readable and writable by whatever reaches {} (#210)",
            shared.nqn(),
            target.advertised()
        );
    } else if pol.allowed_hosts.is_empty() {
        tracing::info!(
            "NVMe-oF: {} admits no host; volumes are served to the host named on attach (host_nqn)",
            shared.nqn()
        );
    } else {
        tracing::info!("NVMe-oF: {} admits {}", shared.nqn(), pol.allowed_hosts.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsystem_names() {
        let a = host_subsystem_nqn("nqn.x:s", "nqn.2014-08.org.nvmexpress:uuid:1");
        let b = host_subsystem_nqn("nqn.x:s", "nqn.2014-08.org.nvmexpress:uuid:2");
        assert!(a.starts_with("nqn.x:s:host:") && a.len() == "nqn.x:s:host:".len() + 16);
        assert_ne!(a, b);
        assert_eq!(a, host_subsystem_nqn("nqn.x:s", "nqn.2014-08.org.nvmexpress:uuid:1"));
        assert_eq!(boothost_subsystem_nqn("nqn.x:s", "server 1/a"), "nqn.x:s:host:server-1-a");
    }

    #[test]
    fn boothost_nqns_follow_the_template() {
        let p = Policy {
            allow_any_host: false,
            allowed_hosts: vec![],
            require_dhchap: false,
            boothost_host_nqn: DEFAULT_BOOTHOST_HOST_NQN.into(),
        };
        assert_eq!(
            p.boothost_nqns("stormblock1", &["C2NR0Q2".into(), "stormblock1".into()]),
            vec!["nqn.2026-09.lo.storm:host-stormblock1", "nqn.2026-09.lo.storm:host-C2NR0Q2"]
        );
        assert!(!p.shared_reachable());
        assert!(matches!(p.shared_access(), HostAccess::Hosts(h) if h.is_empty()));
    }
}
