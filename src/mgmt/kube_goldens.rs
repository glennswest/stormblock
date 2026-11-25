//! `Golden` objects on the cluster's apiserver (#743): a golden made from a
//! volume, and files read out of it, asked through the apiserver.
//!
//! stormfn (stormfn#62) builds a function into a volume and needs it made a
//! golden, and the dependency list read out of it. Management goes through
//! the apiserver (owner, 2026-11-19: "management = CRDs + controllers";
//! master, 2026-11-25, on #743): a component never calls a node's own port
//! with its cluster bearer. So the caller creates a `Golden`
//! (`storage.storm.io/v1`, cluster-scoped, `deploy/crds.yaml`) and the
//! engine of the node its `storm.io/node` label names makes it:
//!
//! ```yaml
//! apiVersion: storage.storm.io/v1
//! kind: Golden
//! metadata: { name: fn-shop-checkout-r1, labels: { storm.io/node: server8 } }
//! spec:
//!   volume: pvc-shop-checkout-build   # the source, by name or uuid, on that node
//!   name: fn-shop-checkout            # the golden is golden-<name>-<sha12>
//!   namespace: shop                   # optional: the golden's namespace (#408)
//!   provenance: { component: stormfn, commit: 0123abc }
//!   force: false                      # make it although something serves the volume
//!   files: [/dependencies.txt]        # read out of the golden once made
//! ```
//!
//! The engine runs [`goldens::make`] (the same verb and checks as
//! `POST /api/v1/goldens`, #143: a volume in use is a 409, the name is
//! checked, the same content again answers the golden already there), then
//! reads each of `spec.files` out of **the golden**, never the source, and
//! writes `status`:
//!
//! - `phase`: `Making`, `Ready` or `Failed`;
//! - `observedSpec`: the spec it acted on;
//! - `golden`: `{name, volumeId, sha256, sizeBytes, existing}`;
//! - `files`: one entry a path asked, in order: `{path, size, sha256,
//!   contentsBase64}`, or `{path, error}`;
//! - `error`: `{code, message}` when it failed.
//!
//! A spec is acted on once: again only when it changes (`observedSpec`
//! differs) or a `Making` was cut short by a restart. Files are bounded
//! ([`MAX_FILES`], [`MAX_FILE_BYTES`] a file, [`MAX_TOTAL_BYTES`] in all, so
//! the object stays far under etcd's limit); a path is absolute, with no
//! `.` or `..` part. A file over its bound is an entry's error, never a
//! failed golden. A secret volume is refused. Deleting the object deletes
//! no golden. One with no `storm.io/node` label is made nowhere, and every
//! engine says so once.
//!
//! Who may ask is the apiserver's RBAC: `create` on `goldens` (what the
//! engine's own verb reviews a bearer for, #274) and `get` to read the
//! answer. The engine writes the status with its own token (`goldens/status`
//! in its ClusterRole).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::mgmt::goldens::{self, MakeRequest};
use crate::mgmt::kube_publish::{call, listed, same, Summary, NODE_LABEL};
use crate::mgmt::AppState;
use crate::volume::VolumeId;

pub const PLURAL: &str = "goldens";
/// Files asked of one Golden, at most.
pub const MAX_FILES: usize = 16;
/// One file's bytes, at most.
pub const MAX_FILE_BYTES: u64 = 64 * 1024;
/// Every file's bytes together, at most (base64 makes it a third more).
pub const MAX_TOTAL_BYTES: u64 = 256 * 1024;
const MAX_PATH: usize = 1024;

/// What a Golden asks.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoldenSpec {
    pub volume: String,
    pub name: String,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub provenance: BTreeMap<String, String>,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub files: Vec<String>,
}

/// Golden objects being made on this node, and what was said once.
#[derive(Default)]
pub struct Goldens {
    running: Arc<Mutex<BTreeSet<String>>>,
    said: BTreeSet<String>,
}

/// The spec read and checked: `Err((422, why))` when it cannot be done.
pub fn parse_spec(spec: &Value) -> Result<GoldenSpec, (u16, String)> {
    let s: GoldenSpec = serde_json::from_value(spec.clone()).map_err(|e| (422, format!("spec: {e}")))?;
    if s.volume.is_empty() {
        return Err((422, "spec.volume: the source volume, by name or uuid, is required".into()));
    }
    if !goldens::valid_name(&s.name) {
        return Err((422, format!(
            "spec.name '{}': lower-case letters, digits and dashes, at most 48, not starting with a dash",
            s.name
        )));
    }
    if s.files.len() > MAX_FILES {
        return Err((422, format!("spec.files: {} paths, at most {MAX_FILES}", s.files.len())));
    }
    for p in &s.files {
        if let Some(why) = bad_path(p) {
            return Err((422, format!("spec.files: {p:?}: {why}")));
        }
    }
    Ok(s)
}

/// Why `p` is no path a Golden may read, if it is not.
pub fn bad_path(p: &str) -> Option<&'static str> {
    if !p.starts_with('/') {
        return Some("not absolute");
    }
    if p.len() > MAX_PATH {
        return Some("longer than 1024 bytes");
    }
    if p.contains('\0') {
        return Some("holds a NUL");
    }
    if p.split('/').any(|c| c == "." || c == "..") {
        return Some("has a . or .. part");
    }
    None
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Whether this node acts on the object now: its node, not being made here,
/// and its spec not yet acted on (or a `Making` cut short).
fn wanted(obj: &Value, node: &str, running: &BTreeSet<String>) -> bool {
    let name = obj["metadata"]["name"].as_str().unwrap_or_default();
    if obj["metadata"]["labels"][NODE_LABEL].as_str() != Some(node) || running.contains(name) {
        return false;
    }
    let st = &obj["status"];
    let done = matches!(st["phase"].as_str(), Some("Ready") | Some("Failed"));
    !(done && same(&st["observedSpec"], &obj["spec"]))
}

/// One pass: every Golden of this node's not acted on is started.
pub async fn pass(state: &Arc<AppState>, g: &mut Goldens, s: &mut Summary) -> Result<(), String> {
    let node = state.local_node_name();
    let there = listed(state, PLURAL).await?;
    let running = g.running.lock().unwrap_or_else(|p| p.into_inner()).clone();
    for (name, obj) in &there {
        if obj["metadata"]["labels"][NODE_LABEL].is_null() && g.said.insert(name.clone()) {
            tracing::warn!("kubernetes: Golden {name} has no {NODE_LABEL} label: no engine makes it (label it with the node that holds spec.volume) (#743)");
        }
        if !wanted(obj, &node, &running) {
            continue;
        }
        let spec = obj["spec"].clone();
        let path = format!("/apis/storage.storm.io/v1/{PLURAL}/{name}/status");
        let req = match parse_spec(&spec) {
            Ok(r) => r,
            Err((code, why)) => {
                tracing::warn!("golden object {name}: refused: {why} (#743)");
                let st = json!({ "status": {
                    "phase": "Failed", "observedSpec": spec, "golden": null, "files": null,
                    "error": { "code": code, "message": why }, "finishedAt": now(),
                }});
                call(state, "PATCH", &path, Some(&st)).await.map_err(|(_, e)| e)?;
                s.refused += 1;
                continue;
            }
        };
        let st = json!({ "status": {
            "phase": "Making", "observedSpec": spec, "golden": null, "files": null, "error": null,
            "startedAt": now(), "finishedAt": null,
        }});
        call(state, "PATCH", &path, Some(&st)).await.map_err(|(_, e)| e)?;
        g.running.lock().unwrap_or_else(|p| p.into_inner()).insert(name.clone());
        tracing::info!("golden object {name}: making {} from {} (#743)", req.name, req.volume);
        let (state, running, name) = (state.clone(), g.running.clone(), name.clone());
        tokio::spawn(async move {
            let status = make(&state, &name, &req).await;
            let failed = status["phase"] == json!("Failed");
            match call(&state, "PATCH", &path, Some(&json!({ "status": status.clone() }))).await {
                Ok(_) if failed => tracing::warn!("golden object {name}: failed: {} (#743)", status["error"]["message"]),
                Ok(_) => tracing::info!("golden object {name}: {} ready (#743)", status["golden"]["name"]),
                Err((_, e)) => tracing::warn!("golden object {name}: its status not written: {e} (#743)"),
            }
            running.lock().unwrap_or_else(|p| p.into_inner()).remove(&name);
        });
        s.applied += 1;
    }
    Ok(())
}

/// Make the golden and read its files: the status to write.
pub async fn make(state: &Arc<AppState>, object: &str, spec: &GoldenSpec) -> Value {
    let fail = |code: u16, message: String| {
        json!({ "phase": "Failed", "error": { "code": code, "message": message }, "finishedAt": now() })
    };
    let source = match crate::mgmt::api::volumes::volume_key_text(state, &spec.volume, None).await {
        Ok(v) => v,
        Err((code, why)) => return fail(code, why),
    };
    if let Err(e) = state.volume_manager.lock().await.refuse_secret(&source, "made a golden through the apiserver") {
        return fail(409, e.to_string());
    }
    let req = MakeRequest {
        volume: spec.volume.clone(),
        name: spec.name.clone(),
        provenance: spec.provenance.clone(),
        force: spec.force,
        ticket_secs: None,
        namespace: spec.namespace.clone(),
    };
    let made_by = Some(format!("kubernetes:Golden/{object}"));
    let (r, existing) = match goldens::make(state, source, &req, made_by).await {
        Ok(x) => x,
        Err(goldens::MakeError(code, why)) => return fail(code, why),
    };
    let files = read_files(state, VolumeId(r.volume_id), &spec.files).await;
    json!({
        "phase": "Ready",
        "golden": {
            "name": r.name, "volumeId": r.volume_id, "sha256": r.sha256,
            "sizeBytes": r.size_bytes, "namespace": r.namespace, "existing": existing,
        },
        "files": files,
        "error": null,
        "finishedAt": now(),
    })
}

/// Each path read out of the golden, bounded: one entry a path, in order.
pub async fn read_files(state: &AppState, golden: VolumeId, paths: &[String]) -> Vec<Value> {
    use base64::Engine;
    use crate::fs::files::{read_file_bounded, Bounded};
    if paths.is_empty() {
        return Vec::new();
    }
    let dev = state.volume_manager.lock().await.get_volume(&golden);
    let Some(dev) = dev else {
        return paths.iter().map(|p| json!({ "path": p, "error": "the golden is not on this node any more" })).collect();
    };
    let mut left = MAX_TOTAL_BYTES;
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let e = match read_file_bounded(&dev, p, MAX_FILE_BYTES.min(left)).await {
            Ok(Bounded::Bytes(b)) => {
                left -= b.len() as u64;
                json!({
                    "path": p, "size": b.len(), "sha256": hex::encode(Sha256::digest(&b)),
                    "contentsBase64": base64::engine::general_purpose::STANDARD.encode(&b),
                })
            }
            Ok(Bounded::TooBig(n)) if n <= MAX_FILE_BYTES => json!({ "path": p, "size": n,
                "error": format!("{n} bytes: over the {MAX_TOTAL_BYTES} bytes the files of one Golden may hold together ({left} left)") }),
            Ok(Bounded::TooBig(n)) => json!({ "path": p, "size": n,
                "error": format!("{n} bytes: over the {MAX_FILE_BYTES} a file may hold") }),
            Ok(Bounded::Dir) => json!({ "path": p, "error": "a directory, not a file" }),
            Err(e) => json!({ "path": p, "error": e.to_string() }),
        };
        out.push(e);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_is_checked_before_anything_is_made() {
        let ok = parse_spec(&json!({ "volume": "pvc-shop-b", "name": "fn-shop-checkout", "files": ["/dependencies.txt"] })).unwrap();
        assert_eq!(ok.files, vec!["/dependencies.txt".to_string()]);
        assert!(!ok.force && ok.namespace.is_none());
        let bad = |v: Value| parse_spec(&v).unwrap_err().1;
        assert!(bad(json!({ "volume": "", "name": "x" })).contains("spec.volume"));
        assert!(bad(json!({ "volume": "v", "name": "Fn" })).contains("spec.name"));
        assert!(bad(json!({ "volume": "v", "name": "x", "files": ["etc/passwd"] })).contains("not absolute"));
        assert!(bad(json!({ "volume": "v", "name": "x", "files": ["/a/../etc/shadow"] })).contains(". or .."));
        assert!(bad(json!({ "volume": "v", "name": "x", "files": ["/a/./b"] })).contains(". or .."));
        let many: Vec<String> = (0..17).map(|i| format!("/f{i}")).collect();
        assert!(bad(json!({ "volume": "v", "name": "x", "files": many })).contains("at most 16"));
        assert!(bad(json!({ "volume": "v", "name": "x", "sizeBytes": 1 })).contains("unknown field"), "a typo is refused, not ignored");
    }

    #[test]
    fn acted_on_once_a_spec() {
        let mut o = json!({ "metadata": { "name": "g", "labels": { NODE_LABEL: "n1" } }, "spec": { "volume": "v", "name": "x" } });
        let none = BTreeSet::new();
        assert!(wanted(&o, "n1", &none), "new");
        assert!(!wanted(&o, "n2", &none), "another node's");
        assert!(!wanted(&o, "n1", &BTreeSet::from(["g".to_string()])), "being made here");
        o["status"] = json!({ "phase": "Making", "observedSpec": o["spec"].clone() });
        assert!(wanted(&o, "n1", &none), "a Making cut short");
        o["status"]["phase"] = json!("Ready");
        assert!(!wanted(&o, "n1", &none), "done");
        o["status"]["phase"] = json!("Failed");
        assert!(!wanted(&o, "n1", &none), "failed as it stands");
        o["spec"]["force"] = json!(true);
        assert!(wanted(&o, "n1", &none), "changed since");
    }
}
