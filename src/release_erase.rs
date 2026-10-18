//! Release erase (#291, stormcluster#14/#21): one call that erases everything
//! a node holds for a cluster, once, and brings it back as a new SNO.
//!
//! **When it runs** (master, for the owner, 2026-10-17: 1B). A running node's
//! own service data (`fastetcd-data`, `stormcert-data`, the kubelet's) are
//! ublk devices mounted at boot, and the engine never deletes a volume that
//! something serves (#267). So the live call only records a *ticket* in the
//! engine's state (captured to `stormblock-state` before it answers), and the
//! node reboots. The initramfs engine (`boot-local`, from the local disk
//! alone) reads the ticket and runs the release **before anything is
//! resolved, exported or mounted**, then boots on.
//!
//! **What is kept and what goes** ([`plan`]):
//! - kept: every sealed golden of origin `release` (#349): the release's
//!   system images, and with them every PVC blank; and `system-data`, the
//!   machine record (stormcluster#43: "always kept");
//! - laid again: every unsealed volume of origin `release` cloned from a kept
//!   golden: deleted (its slots erased), then a fresh clone of that golden
//!   under the same name with a new filesystem identity, as an install lays
//!   it (with no image copy). `stormcos-state` gets the node's own
//!   declaration back ([`Declaration`]): `[node]` and `[network]` of
//!   `stormcos.toml`, `install-node.toml`, `install-config.yaml` and the ssh
//!   keys; the role (`[cluster]`) is not carried;
//! - erased: everything else: claims, VM disks, the node's own goldens
//!   (customer images), unmarked volumes (an unmarked golden cannot be shown
//!   to be a system image).
//!
//! Every freed slot is overwritten at least at the ticket's level (#286):
//! deleted here, it is `Erasing` in its slab's table and overwritten by the
//! successor's eraser, which re-queues it when it opens the slab
//! (`/api/v1/erasures`). Slots a kept golden still shares are not freed, so
//! not erased (#286's rule).
//!
//! **A cut part-way repeats the run.** The ticket, with the declaration in
//! it, is in `stormblock-state`, and that volume is laid again last, through
//! a temporary name: until the end the old one (and its ticket) is there.
//! What was already laid again is laid again (still fresh, still erased).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::drive::erase::EraseLevel;
use crate::volume::metadata::Origin;
use crate::volume::{VolumeId, VolumeManager};

/// The ticket, in the engine's data directory and so in `stormblock-state`.
pub const TICKET: &str = "release-ticket.json";
/// What the run did, written into the fresh `stormblock-state`.
pub const REPORT: &str = "release-report.json";
/// The engine's state volume (`cli::STATE_VOLUME`).
pub const STATE_VOLUME: &str = "stormblock-state";
/// The node's declaration (`/state`).
pub const NODE_STATE_VOLUME: &str = "stormcos-state";
/// The machine record (#355).
pub const SYSTEM_DATA: &str = "system-data";
/// The fresh `stormblock-state` while the old one is still there.
const STATE_NEW: &str = "stormblock-state.release-new";

/// A release asked for and not yet run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Ticket {
    pub id: String,
    /// Freed slots are overwritten at least at this level.
    pub erase: EraseLevel,
    /// Seconds since the epoch.
    pub asked_at: u64,
    pub asked_by: String,
    /// Asked with `force`: remote hosts attached were not a refusal.
    #[serde(default)]
    pub force: bool,
    /// `pending`, or `cancelled` (the state store keeps files, so a cancel
    /// is written, never a delete).
    pub state: String,
    /// The node's declaration, saved before anything is deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declaration: Option<Declaration>,
}

impl Ticket {
    pub fn pending(&self) -> bool {
        self.state == "pending"
    }
}

/// What `stormcos-state` keeps across a release: the node's own declaration,
/// so the new SNO comes up on the same network with the same name.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Declaration {
    /// Path inside `stormcos-state` → contents (UTF-8: every one of them is
    /// text).
    pub files: std::collections::BTreeMap<String, String>,
}

/// Files carried whole.
const CARRIED: &[&str] = &["/config/install-node.toml", "/config/install-config.yaml", "/config/ssh/authorized_keys"];
/// Carried with only its `[node]` and `[network]` tables.
const NODE_TOML: &str = "/config/stormcos.toml";

/// What happens to one volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum Action {
    Keep { why: &'static str },
    /// Deleted, then cloned again from `from`.
    Relay { from: String },
    Erase { why: &'static str },
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub origin: &'static str,
    pub sealed: bool,
    #[serde(flatten)]
    pub action: Action,
}

impl Item {
    fn vid(&self) -> VolumeId {
        VolumeId(self.id.parse().expect("an id from the manager"))
    }
}

/// What a release does to each volume on this node, in name order.
pub async fn plan(m: &VolumeManager) -> Vec<Item> {
    let mut items = Vec::new();
    for id in m.volume_ids() {
        let Some(h) = m.get_volume_handle(&id) else { continue };
        let name = h.name().await;
        let origin = m.origin(&id);
        let sealed = m.is_sealed(&id);
        let kept_golden = |v: &VolumeId| m.is_sealed(v) && m.origin(v) == Origin::Release;
        let action = if name == SYSTEM_DATA {
            Action::Keep { why: "the machine record" }
        } else if name == STATE_NEW {
            Action::Erase { why: "a release cut short" }
        } else if sealed && origin == Origin::Release {
            Action::Keep { why: "a release golden" }
        } else if !sealed && origin == Origin::Release {
            match m.parent(&id).filter(|p| m.get_volume_handle(p).is_some() && kept_golden(p)) {
                Some(p) => Action::Relay { from: p.0.to_string() },
                None => Action::Erase { why: "the release's, with no golden here to lay it from" },
            }
        } else if sealed {
            Action::Erase { why: if origin == Origin::Node { "the node's golden" } else { "an unmarked golden" } }
        } else {
            Action::Erase { why: if origin == Origin::Node { "the node's" } else { "unmarked" } }
        };
        items.push(Item { id: id.0.to_string(), name, origin: origin.as_str(), sealed, action });
    }
    items.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    items
}

/// Only the `[node]` and `[network]` tables (and their sub-tables) of a
/// `stormcos.toml`; `None` when nothing is left or it does not parse after.
pub fn declaration_toml(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut keep = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with('[') {
            let head = t.trim_start_matches('[').split([']', '#']).next().unwrap_or("").trim().trim_matches('"');
            let first = head.split('.').next().unwrap_or("").trim().trim_matches('"');
            keep = first == "node" || first == "network";
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    if out.trim().is_empty() || toml::from_str::<serde_json::Value>(&out).is_err() {
        return None;
    }
    Some(out)
}

/// The declaration in a `stormcos-state` volume.
pub async fn read_declaration(dev: &Arc<dyn crate::drive::BlockDevice>) -> Declaration {
    let mut d = Declaration::default();
    for path in CARRIED {
        if let Ok(true) = crate::fs::files::exists(dev, path).await {
            if let Ok(b) = crate::fs::files::read_file(dev, path).await {
                if let Ok(s) = String::from_utf8(b) {
                    d.files.insert(path.to_string(), s);
                }
            }
        }
    }
    if let Ok(true) = crate::fs::files::exists(dev, NODE_TOML).await {
        if let Ok(b) = crate::fs::files::read_file(dev, NODE_TOML).await {
            if let Some(kept) = String::from_utf8(b).ok().as_deref().and_then(declaration_toml) {
                d.files.insert(NODE_TOML.to_string(), kept);
            }
        }
    }
    d
}

/// What a run did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub ticket: String,
    pub erase: EraseLevel,
    /// Seconds since the epoch.
    pub ran_at: u64,
    pub kept: Vec<String>,
    pub relaid: Vec<String>,
    pub erased: Vec<String>,
    /// Slots and bytes queued for overwrite.
    pub slots: u64,
    pub bytes: u64,
    /// The declaration's files written into the fresh `stormcos-state`.
    pub carried: Vec<String>,
    /// What did not go as planned, each said on the console too.
    pub failed: Vec<String>,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The ticket in `stormblock-state`, when there is one and it is pending.
pub async fn pending_ticket(m: &VolumeManager) -> Option<(Arc<crate::state::StateStore>, Ticket)> {
    let id = m.find_volume(STATE_VOLUME).await?;
    let dev = m.get_volume(&id)?;
    let store = Arc::new(crate::state::StateStore::open_volume(dev).await);
    let bytes = store.get(TICKET).await?;
    let t: Ticket = serde_json::from_slice(&bytes).ok()?;
    t.pending().then_some((store, t))
}

/// Run a ticket on a node whose volumes nothing serves yet: `boot-local`,
/// before the root is resolved.
pub async fn run(vm: &Arc<crate::fs::template::VmLock>, store: &crate::state::StateStore, mut t: Ticket) -> anyhow::Result<Report> {
    println!("RELEASE: ticket {} ({} erase, asked by {}): erasing what this node holds for its cluster", t.id, t.erase, t.asked_by);
    // The declaration first, kept in the ticket: a cut after stormcos-state
    // is gone still has it.
    if t.declaration.is_none() {
        let dev = {
            let m = vm.lock().await;
            match m.find_volume(NODE_STATE_VOLUME).await {
                Some(id) => m.get_volume(&id),
                None => None,
            }
        };
        let d = match dev {
            Some(dev) => read_declaration(&dev).await,
            None => Declaration::default(),
        };
        t.declaration = Some(d);
        store
            .put(TICKET, serde_json::to_vec_pretty(&t)?)
            .await
            .map_err(|e| anyhow::anyhow!("saving the declaration in the ticket: {e}"))?;
    }
    let decl = t.declaration.clone().unwrap_or_default();

    let items = plan(&*vm.lock().await).await;
    let mut r = Report { ticket: t.id.clone(), erase: t.erase, ran_at: now(), ..Default::default() };
    let level = Some(t.erase.max(EraseLevel::Once));
    // Deepest first, so a clone goes before what it was cloned from; the
    // state volume after everything else.
    let mut order: Vec<&Item> = items.iter().filter(|i| !matches!(i.action, Action::Keep { .. })).collect();
    {
        let m = vm.lock().await;
        order.sort_by_key(|i| (i.name == STATE_VOLUME, std::cmp::Reverse(m.lineage(&i.vid()).len())));
    }
    for i in &items {
        if let Action::Keep { why } = i.action {
            r.kept.push(format!("{} ({why})", i.name));
        }
    }
    let mut relay = Vec::new();
    let mut state_from = None;
    for i in order.iter().filter(|i| i.name != STATE_VOLUME) {
        let mut m = vm.lock().await;
        match m.delete_volume_erasing(i.vid(), level).await {
            Ok(s) => {
                r.slots += s.slots;
                r.bytes += s.bytes;
                if let Action::Relay { from } = &i.action {
                    relay.push((i.name.clone(), from.clone()));
                } else {
                    r.erased.push(i.name.clone());
                }
            }
            Err(e) => {
                let why = format!("{}: not deleted: {e}", i.name);
                eprintln!("WARNING: release: {why}");
                r.failed.push(why);
            }
        }
    }
    for i in order.iter().filter(|i| i.name == STATE_VOLUME) {
        if let Action::Relay { from } = &i.action {
            state_from = Some(from.clone());
        }
    }
    vm.lock().await.persist().await;

    for (name, from) in &relay {
        match lay(vm, name, from).await {
            Ok(id) => {
                r.relaid.push(name.clone());
                if name == NODE_STATE_VOLUME && !decl.files.is_empty() {
                    let dev = vm.lock().await.get_volume(&id);
                    if let Some(dev) = dev {
                        let files: Vec<_> =
                            decl.files.iter().map(|(p, c)| crate::fs::files::SeedFile::new(p.clone(), c.clone().into_bytes())).collect();
                        match crate::fs::files::write_files(&dev, &files).await {
                            Ok(()) => r.carried = decl.files.keys().cloned().collect(),
                            Err(e) => r.failed.push(format!("{NODE_STATE_VOLUME}: the declaration not written: {e}")),
                        }
                    }
                }
            }
            Err(e) => {
                let why = format!("{name}: not laid again: {e}");
                eprintln!("WARNING: release: {why}");
                r.failed.push(why);
            }
        }
    }
    vm.lock().await.persist().await;

    // The state volume last, through a temporary name: until the rename the
    // old one, with its ticket, is the one the next boot reads.
    let old_state = items.iter().find(|i| i.name == STATE_VOLUME).map(|i| i.vid());
    if let (Some(old), Some(from)) = (old_state, state_from) {
        match lay(vm, STATE_NEW, &from).await {
            Ok(new) => {
                r.relaid.push(STATE_VOLUME.to_string());
                let mut done = r.clone();
                done.ran_at = now();
                let dev = vm.lock().await.get_volume(&new);
                if let Some(dev) = dev {
                    let fresh = crate::state::StateStore::open_volume(dev).await;
                    if let Err(e) = fresh.put(REPORT, serde_json::to_vec_pretty(&done)?).await {
                        r.failed.push(format!("the report not written: {e}"));
                    }
                }
                let mut m = vm.lock().await;
                match m.delete_volume_erasing(old, level).await {
                    Ok(s) => {
                        r.slots += s.slots;
                        r.bytes += s.bytes;
                        if let Err(e) = m.rename_volume(new, STATE_VOLUME).await {
                            r.failed.push(format!("{STATE_NEW}: not renamed: {e}"));
                        }
                    }
                    Err(e) => r.failed.push(format!("{STATE_VOLUME}: not deleted: {e}")),
                }
                m.persist().await;
            }
            Err(e) => r.failed.push(format!("{STATE_VOLUME}: not laid again: {e}")),
        }
    } else if let Some(old) = old_state {
        // No golden to lay it from: erased, and the ticket with it.
        let mut m = vm.lock().await;
        if let Ok(s) = m.delete_volume_erasing(old, level).await {
            r.slots += s.slots;
            r.bytes += s.bytes;
            r.erased.push(STATE_VOLUME.to_string());
        }
        m.persist().await;
    }
    println!(
        "RELEASE: done: {} kept, {} laid again, {} erased, {} slot(s) to overwrite ({}){}",
        r.kept.len(),
        r.relaid.len(),
        r.erased.len(),
        r.slots,
        t.erase.max(EraseLevel::Once),
        if r.failed.is_empty() { String::new() } else { format!(", {} not as planned", r.failed.len()) }
    );
    Ok(r)
}

/// A fresh clone of `from` named `name`, the release's.
async fn lay(vm: &Arc<crate::fs::template::VmLock>, name: &str, from: &str) -> anyhow::Result<VolumeId> {
    let src = VolumeId(from.parse()?);
    let c = crate::fs::template::clone_volume(vm, src, &crate::fs::template::CloneSpec::new(name))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut m = vm.lock().await;
    m.set_origin(c.volume_id, Origin::Release);
    Ok(c.volume_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_declaration_keeps_node_and_network_and_drops_the_role() {
        let text = "\
# the node's own
[node]
hostname = \"stormblock1\"

[cluster]
role = \"master\"
join = \"https://x\"

[network]
mode = \"static\"
[[network.addresses]]
address = \"192.168.8.10/24\"
[network.dns]
servers = [\"192.168.8.252\"]

[kube]
token = \"secret\"
";
        let kept = declaration_toml(text).unwrap();
        let t: serde_json::Value = toml::from_str(&kept).unwrap();
        assert_eq!(t["node"]["hostname"].as_str(), Some("stormblock1"));
        assert_eq!(t["network"]["mode"].as_str(), Some("static"));
        assert!(t["network"]["addresses"].as_array().is_some_and(|a| a.len() == 1));
        assert!(t["network"]["dns"].is_object());
        assert!(t.get("cluster").is_none() && t.get("kube").is_none(), "{kept}");
        assert!(!kept.contains("master") && !kept.contains("secret"));
        assert_eq!(declaration_toml("[cluster]\nrole = \"sno\"\n"), None, "nothing of the node's");
    }
}
