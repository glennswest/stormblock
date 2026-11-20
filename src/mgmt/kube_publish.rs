//! The engine's `storage.storm.io/v1` objects on the cluster's apiserver
//! (#655; owner, 2026-11-19: "management = CRDs + controllers").
//!
//! The engine serves `Volume` and `Replication` on its own port (#80, #656);
//! `kubectl`/`sc` ask the apiserver. So, as stormdrive does with `Drive`,
//! the engine writes its objects there as CRDs (`deploy/crds.yaml`) and
//! takes back what is changed there. Every [`PERIOD`]:
//!
//! - **Volume**: one object a volume, named by its uuid, labelled
//!   `storm.io/node` with this node. Created when missing; its labels, spec
//!   and `status` merge-patched when they differ; deleted when the volume is
//!   gone. Deleting the object deletes nothing: it is written again (a
//!   volume is deleted through the engine).
//! - **A spec edited on the apiserver** (`sc patch volume <id> -p
//!   '{"spec":{"sizeBytes":N}}'`): the annotation `storm.io/engine-spec`
//!   holds the writable fields as the engine last wrote them. A field that
//!   differs from it is a change asked there; it goes through the same code
//!   as the engine's own PATCH ([`apply_volume_spec`]), then the engine's
//!   spec is written back. A refusal (smaller, sealed, exported) is said in
//!   `status.specError` and the spec is put back to what the volume is.
//!   `status.lastResize` says what the last grow did.
//! - **Replication**: this node's are published the same way. One made on
//!   the apiserver with this node's `storm.io/node` label runs here; one the
//!   engine had published and that is deleted there is deleted here (the
//!   names seen are kept in `<data_dir>/kube-published.json`, so a delete
//!   made while the engine was down is taken too). One with no
//!   `storm.io/node` label runs nowhere, and every engine says so once.
//!
//! - **NodeInstall** (#623): one object per node, named after it, whose
//!   status is the node's install record (`/api/v1/install/progress`, #616),
//!   as [`node_install_object`] renders it. It is written only when the node
//!   has an install record; its spec names the node and nothing else yet. Its errors (the
//!   CRD not on the cluster yet) are said once and never stop the Volume and
//!   Replication objects.
//!
//! The engine's bearer is the one it reviews callers with
//! (`[management.kubernetes]`, or the cluster's record on a worker, #557);
//! it must be allowed volumes, volumes/status, replications and
//! replications/status (and nodeinstalls, nodeinstalls/status) in `storage.storm.io` (`deploy/crds.yaml` has the
//! ClusterRole). Nothing is published when no apiserver is configured.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::mgmt::api::kube::{apply_volume_spec, volume_objects, VolumeSpecPatch};
use crate::mgmt::AppState;
use crate::volume::VolumeId;

/// How often the objects are compared and written.
pub const PERIOD: Duration = Duration::from_secs(15);

/// The annotation holding the writable spec as the engine last wrote it.
pub const ENGINE_SPEC: &str = "storm.io/engine-spec";
pub const NODE_LABEL: &str = "storm.io/node";
const GROUP_PATH: &str = "/apis/storage.storm.io/v1";
const SEEN_FILE: &str = "kube-published.json";

/// A Volume's spec fields that can be changed (the rest are what it is).
const VOLUME_WRITABLE: [&str; 6] = ["sizeBytes", "sealed", "access", "retention", "redundancy", "grow"];

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    pub created: usize,
    pub patched: usize,
    pub deleted: usize,
    /// Spec changes asked on the apiserver and done here.
    pub applied: usize,
    /// Spec changes asked on the apiserver and refused here.
    pub refused: usize,
}

/// What lasts between passes.
#[derive(Default)]
pub struct Publisher {
    /// Per volume uuid: `lastResize` and `specError`, added to its status.
    notes: BTreeMap<String, Map<String, Value>>,
    /// Per replication name: `specError`.
    rnotes: BTreeMap<String, Map<String, Value>>,
    /// Replications seen on the apiserver (persisted).
    seen: Option<BTreeSet<String>>,
    /// Said once each: an unlabelled Replication, a name another node holds.
    said: BTreeSet<String>,
    /// The NodeInstall's last error, said when it changes (#623).
    install_error: Option<String>,
    /// The last error, said when it changes.
    last_error: Option<String>,
}

impl Publisher {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Start publishing, when an apiserver is configured.
pub fn start(state: &Arc<AppState>) {
    if state.kube_auth.is_none() {
        tracing::info!("kubernetes: no apiserver configured: Volume and Replication objects are served on this port only (#655)");
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        let mut p = Publisher::new();
        loop {
            match pass(&state, &mut p).await {
                Ok(s) => {
                    if p.last_error.take().is_some() {
                        tracing::info!("kubernetes: Volume and Replication objects published again (#655)");
                    }
                    if s != Summary::default() {
                        tracing::info!(
                            "kubernetes: objects published: {} created, {} patched, {} deleted; {} spec change(s) applied, {} refused (#655)",
                            s.created, s.patched, s.deleted, s.applied, s.refused
                        );
                    }
                }
                Err(e) => {
                    if p.last_error.as_deref() != Some(e.as_str()) {
                        tracing::warn!("kubernetes: publishing Volume and Replication objects: {e} (#655)");
                        p.last_error = Some(e);
                    }
                }
            }
            tokio::time::sleep(PERIOD).await;
        }
    });
}

/// `same` with nulls dropped from objects and numbers compared by value:
/// what went through the apiserver compared to what is rendered here.
pub fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: BTreeSet<&String> = x.iter().chain(y.iter()).filter(|(_, v)| !v.is_null()).map(|(k, _)| k).collect();
            keys.into_iter().all(|k| same(x.get(k).unwrap_or(&Value::Null), y.get(k).unwrap_or(&Value::Null)))
        }
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q)),
        (Value::Number(x), Value::Number(y)) => x == y || x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// The merge patch (RFC 7386) that makes `current` into `target`: target's
/// fields, nested objects patched in place, and a null for each field
/// `current` has that `target` has not.
pub fn patch_to(target: &Value, current: &Value) -> Value {
    match (target.as_object(), current.as_object()) {
        (Some(t), Some(c)) => {
            let mut out = Map::new();
            for (k, v) in t {
                match c.get(k) {
                    Some(cv) if same(v, cv) => {}
                    Some(cv) => {
                        out.insert(k.clone(), patch_to(v, cv));
                    }
                    None if v.is_null() => {}
                    None => {
                        out.insert(k.clone(), v.clone());
                    }
                }
            }
            for (k, cv) in c {
                if !t.contains_key(k) && !cv.is_null() {
                    out.insert(k.clone(), Value::Null);
                }
            }
            Value::Object(out)
        }
        _ => target.clone(),
    }
}

/// The writable fields of a Volume spec.
pub fn volume_writable(spec: &Value) -> Value {
    let mut m = Map::new();
    for k in VOLUME_WRITABLE {
        if let Some(v) = spec.get(k).filter(|v| !v.is_null()) {
            m.insert(k.to_string(), v.clone());
        }
    }
    Value::Object(m)
}

/// The fields asked on the apiserver: those of `asked` that differ from
/// what the engine last wrote (`applied`), plus `resync: true` (a verb).
pub fn volume_changes(asked: &Value, applied: &Value) -> Map<String, Value> {
    let mut m = Map::new();
    for k in VOLUME_WRITABLE {
        let a = asked.get(k).unwrap_or(&Value::Null);
        if !a.is_null() && !same(a, applied.get(k).unwrap_or(&Value::Null)) {
            m.insert(k.to_string(), a.clone());
        }
    }
    if asked.get("resync") == Some(&Value::Bool(true)) {
        m.insert("resync".into(), Value::Bool(true));
    }
    m
}

fn annotation_spec(obj: &Value) -> Option<Value> {
    obj["metadata"]["annotations"][ENGINE_SPEC].as_str().and_then(|s| serde_json::from_str(s).ok())
}

/// Seconds since the epoch, as the Replication's own times are.
fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

async fn call(state: &AppState, method: &str, path: &str, body: Option<&Value>) -> Result<Value, (u16, String)> {
    let kube = state.kube_auth.as_ref().ok_or((0, "no apiserver configured".to_string()))?;
    let (code, v) = kube.call(method, path, body).await.map_err(|e| (0, e))?;
    if (200..300).contains(&code) {
        Ok(v)
    } else {
        let msg = v["message"].as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
        Err((code, format!("{method} {path} answered {code}: {msg}")))
    }
}

/// Every object of `plural` on the apiserver, by name.
async fn listed(state: &AppState, plural: &str) -> Result<BTreeMap<String, Value>, String> {
    let v = call(state, "GET", &format!("{GROUP_PATH}/{plural}"), None).await.map_err(|(_, e)| e)?;
    let mut out = BTreeMap::new();
    for it in v["items"].as_array().into_iter().flatten() {
        if let Some(n) = it["metadata"]["name"].as_str() {
            out.insert(n.to_string(), it.clone());
        }
    }
    Ok(out)
}

/// Write one object: create it when missing, else merge-patch its labels,
/// annotation and spec where they differ; then its status where it differs.
/// `(created, patched)`.
async fn write(state: &AppState, plural: &str, kind: &str, want: &Value, have: Option<&Value>) -> Result<(bool, bool), String> {
    let name = want["metadata"]["name"].as_str().unwrap_or_default();
    let path = format!("{GROUP_PATH}/{plural}/{name}");
    let mut created = false;
    let mut patched = false;
    let have = match have {
        Some(h) => h.clone(),
        None => {
            let body = json!({
                "apiVersion": "storage.storm.io/v1", "kind": kind,
                "metadata": { "name": name, "labels": want["metadata"]["labels"], "annotations": want["metadata"]["annotations"] },
                "spec": want["spec"],
            });
            created = true;
            match call(state, "POST", &format!("{GROUP_PATH}/{plural}"), Some(&body)).await {
                Ok(v) => v,
                // Made meanwhile (or by another pass): patched below.
                Err((409, _)) => call(state, "GET", &path, None).await.map_err(|(_, e)| e)?,
                Err((_, e)) => return Err(e),
            }
        }
    };
    // Labels the engine sets are `storm.io/…`; others (a user's) are left.
    let mut labels = Map::new();
    let want_labels = want["metadata"]["labels"].as_object().cloned().unwrap_or_default();
    for (k, v) in &want_labels {
        if have["metadata"]["labels"].get(k) != Some(v) {
            labels.insert(k.clone(), v.clone());
        }
    }
    for (k, _) in have["metadata"]["labels"].as_object().into_iter().flatten() {
        if k.starts_with("storm.io/") && !want_labels.contains_key(k) {
            labels.insert(k.clone(), Value::Null);
        }
    }
    let mut main = Map::new();
    if !labels.is_empty() {
        main.insert("labels".into(), Value::Object(labels));
    }
    let want_ann = &want["metadata"]["annotations"][ENGINE_SPEC];
    if !want_ann.is_null() && &have["metadata"]["annotations"][ENGINE_SPEC] != want_ann {
        main.insert("annotations".into(), json!({ ENGINE_SPEC: want_ann }));
    }
    let mut body = Map::new();
    if !main.is_empty() {
        body.insert("metadata".into(), Value::Object(main));
    }
    if !same(&want["spec"], &have["spec"]) {
        body.insert("spec".into(), patch_to(&want["spec"], &have["spec"]));
    }
    if !body.is_empty() {
        call(state, "PATCH", &path, Some(&Value::Object(body))).await.map_err(|(_, e)| e)?;
        patched = true;
    }
    if !same(&want["status"], &have["status"]) {
        let st = json!({ "status": patch_to(&want["status"], &have["status"]) });
        call(state, "PATCH", &format!("{path}/status"), Some(&st)).await.map_err(|(_, e)| e)?;
        patched = true;
    }
    Ok((created, patched && !created))
}

fn add_notes(obj: &mut Value, notes: Option<&Map<String, Value>>) {
    if let (Some(n), Some(st)) = (notes, obj["status"].as_object_mut()) {
        for (k, v) in n {
            st.insert(k.clone(), v.clone());
        }
    }
}

fn spec_error(asked: &Value, code: u16, message: &str) -> Value {
    json!({ "asked": asked, "code": code, "message": message, "at": now() })
}

/// One pass over this node's objects.
pub async fn pass(state: &Arc<AppState>, p: &mut Publisher) -> Result<Summary, String> {
    let mut s = Summary::default();
    volumes(state, p, &mut s).await?;
    replications(state, p, &mut s).await?;
    match node_install(state, &mut s).await {
        Ok(()) => {
            if p.install_error.take().is_some() {
                tracing::info!("kubernetes: the NodeInstall object is published again (#623)");
            }
        }
        Err(e) => {
            if p.install_error.as_deref() != Some(e.as_str()) {
                tracing::warn!("kubernetes: publishing the NodeInstall object: {e} (#623)");
                p.install_error = Some(e);
            }
        }
    }
    Ok(s)
}

/// The fields of a drive in the install record that its NodeInstall status
/// carries: what it is and where its install is, not the card's whole row.
const INSTALL_DRIVE_FIELDS: [&str; 11] =
    ["sas", "dev", "slot", "model", "bytes", "state", "percent", "eta_s", "elapsed_s", "format", "said"];

/// This node's NodeInstall object, from its install record (#623); `None`
/// when the record holds nothing (no stage, no drive, no step reported): a
/// node with no install record has no object, rather than one that says
/// `starting` for ever. The record's own clock fields (`at`, `eta`) are left
/// out, so an unchanged install writes nothing.
pub fn node_install_object(node: &str, record: &Value) -> Option<Value> {
    let stages = record["stages"].as_array().map_or(0, Vec::len);
    let drives = record["drives"].as_array().cloned().unwrap_or_default();
    let reported = record["layout"].as_array().into_iter().flatten().any(|s| s["state"] != json!("pending"));
    if stages == 0 && drives.is_empty() && !reported {
        return None;
    }
    let drives: Vec<Value> = drives
        .iter()
        .map(|d| {
            let mut o = Map::new();
            for k in INSTALL_DRIVE_FIELDS {
                if !d[k].is_null() {
                    o.insert(k.into(), d[k].clone());
                }
            }
            // The last format line only: the record keeps five.
            if let Some(l) = d["lines"].as_array().and_then(|l| l.last()) {
                o.insert("line".into(), l.clone());
            }
            Value::Object(o)
        })
        .collect();
    let formatting = drives.iter().filter(|d| d["state"] == json!("formatting")).count();
    Some(json!({
        "metadata": { "name": node, "labels": { NODE_LABEL: node } },
        // An empty spec reads back as none, which would differ every pass.
        "spec": { "node": node },
        "status": {
            "state": record["state"],
            "stage": record["stage"],
            "started": record["started"],
            "etaSeconds": record["eta_s"],
            "steps": record["layout"],
            "drivesTotal": drives.len(),
            "drivesFormatting": formatting,
            "drives": drives,
            "set": record["set"],
            "systemLegs": record["system_legs"],
        },
    }))
}

/// This node's NodeInstall written, when it has an install record (#623).
async fn node_install(state: &Arc<AppState>, s: &mut Summary) -> Result<(), String> {
    let node = state.local_node_name();
    let run = std::path::PathBuf::from(crate::install_progress::RUN);
    let record = tokio::task::spawn_blocking(move || crate::install_progress::record(&run, crate::install_progress::now()))
        .await
        .map_err(|e| e.to_string())?;
    let Some(want) = node_install_object(&node, &record) else {
        return Ok(());
    };
    let have = match call(state, "GET", &format!("{GROUP_PATH}/nodeinstalls/{node}"), None).await {
        Ok(v) => Some(v),
        Err((404, _)) => None,
        Err((_, e)) => return Err(e),
    };
    let (c, pa) = write(state, "nodeinstalls", "NodeInstall", &want, have.as_ref()).await?;
    s.created += usize::from(c);
    s.patched += usize::from(pa);
    Ok(())
}

fn mine(obj: &Value, node: &str) -> bool {
    obj["metadata"]["labels"][NODE_LABEL].as_str() == Some(node)
}

async fn volumes(state: &Arc<AppState>, p: &mut Publisher, s: &mut Summary) -> Result<(), String> {
    let node = state.local_node_name();
    let there = listed(state, "volumes").await?;
    let mut here = volume_objects(state, None).await;
    // Spec changes asked on the apiserver first; the objects after them.
    let mut changed: Vec<String> = Vec::new();
    for v in &here {
        let name = v["metadata"]["name"].as_str().unwrap_or_default().to_string();
        let Some(have) = there.get(&name).filter(|h| mine(h, &node)) else { continue };
        let Some(applied) = annotation_spec(have) else { continue };
        let asked = volume_changes(&have["spec"], &applied);
        if asked.is_empty() {
            continue;
        }
        let Ok(uuid) = uuid::Uuid::parse_str(&name) else { continue };
        let asked = Value::Object(asked);
        let patch: VolumeSpecPatch = match serde_json::from_value(asked.clone()) {
            Ok(x) => x,
            Err(e) => {
                p.notes.entry(name.clone()).or_default().insert("specError".into(), spec_error(&asked, 422, &e.to_string()));
                s.refused += 1;
                changed.push(name);
                continue;
            }
        };
        let notes = p.notes.entry(name.clone()).or_default();
        match apply_volume_spec(state, VolumeId(uuid), patch).await {
            Ok(resized) => {
                tracing::warn!("volume {name}: spec change asked on the apiserver applied: {asked} (#655)");
                notes.remove("specError");
                if let Some(r) = resized {
                    notes.insert("lastResize".into(), r);
                }
                s.applied += 1;
            }
            Err(r) => {
                tracing::warn!("volume {name}: spec change asked on the apiserver refused: {asked}: {} (#655)", r.message);
                notes.insert("specError".into(), spec_error(&asked, r.code.as_u16(), &r.message));
                s.refused += 1;
            }
        }
        changed.push(name);
    }
    if !changed.is_empty() {
        here = volume_objects(state, None).await;
    }
    let names: BTreeSet<String> = here.iter().filter_map(|v| v["metadata"]["name"].as_str().map(str::to_string)).collect();
    p.notes.retain(|k, _| names.contains(k));
    for mut v in here {
        let name = v["metadata"]["name"].as_str().unwrap_or_default().to_string();
        let have = there.get(&name);
        if let Some(h) = have.filter(|h| !mine(h, &node)) {
            if p.said.insert(format!("volume/{name}")) {
                tracing::warn!(
                    "kubernetes: Volume {name} on the apiserver is node {:?}'s, not this node's ({node}): not written (#655)",
                    h["metadata"]["labels"][NODE_LABEL]
                );
            }
            continue;
        }
        let ann = volume_writable(&v["spec"]).to_string();
        v["metadata"]["annotations"] = json!({ ENGINE_SPEC: ann });
        add_notes(&mut v, p.notes.get(&name));
        let (c, pa) = write(state, "volumes", "Volume", &v, have).await?;
        s.created += usize::from(c);
        s.patched += usize::from(pa);
    }
    for (name, h) in &there {
        if mine(h, &node) && !names.contains(name) {
            match call(state, "DELETE", &format!("{GROUP_PATH}/volumes/{name}"), None).await {
                Ok(_) | Err((404, _)) => s.deleted += 1,
                Err((_, e)) => return Err(e),
            }
        }
    }
    Ok(())
}

fn seen_path(state: &AppState) -> Option<std::path::PathBuf> {
    state.config.management.data_dir.as_ref().map(|d| std::path::Path::new(d).join(SEEN_FILE))
}

fn seen(state: &AppState, p: &mut Publisher) -> BTreeSet<String> {
    if p.seen.is_none() {
        let v = seen_path(state)
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|b| serde_json::from_slice::<BTreeSet<String>>(&b).ok())
            .unwrap_or_default();
        p.seen = Some(v);
    }
    p.seen.clone().unwrap_or_default()
}

fn keep_seen(state: &AppState, p: &mut Publisher, now: BTreeSet<String>) {
    if p.seen.as_ref() == Some(&now) {
        return;
    }
    if let Some(f) = seen_path(state) {
        let tmp = f.with_extension("json.tmp");
        let body = serde_json::to_vec(&now).unwrap_or_default();
        if let Err(e) = std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, &f)) {
            tracing::warn!("kubernetes: {} not written: {e} (#655)", f.display());
        }
    }
    p.seen = Some(now);
}

async fn replications(state: &Arc<AppState>, p: &mut Publisher, s: &mut Summary) -> Result<(), String> {
    use crate::mgmt::replication as repl;
    let node = state.local_node_name();
    let there = listed(state, "replications").await?;
    let mut seen_now = seen(state, p);
    for (name, h) in &there {
        if h["metadata"]["labels"][NODE_LABEL].is_null() && p.said.insert(format!("replication/{name}")) {
            tracing::warn!("kubernetes: Replication {name} has no {NODE_LABEL} label: no engine runs it (label it with the node that pulls) (#655)");
        }
    }
    // What the apiserver asks first: made there, edited there, deleted there.
    for (name, h) in there.iter().filter(|(_, h)| mine(h, &node)) {
        let local = repl::get(state, name);
        let applied = annotation_spec(h);
        let asked = &h["spec"];
        let ask = match (&local, &applied) {
            // Made on the apiserver.
            (None, None) => true,
            // Edited there since the engine wrote it.
            (Some(_), Some(a)) => !same(asked, a),
            // The engine had it; deleted here: the object goes below.
            (None, Some(_)) => false,
            // Ours and never annotated: the engine's spec is written.
            (Some(_), None) => false,
        };
        // Refused before as it stands: said already, not asked again.
        if !ask || (local.is_none() && same(&h["status"]["specError"]["asked"], asked)) {
            continue;
        }
        let r = serde_json::from_value::<repl::Spec>(asked.clone()).map_err(|e| format!("spec: {e}")).and_then(|spec| repl::put(state, name, spec));
        match r {
            Ok(_) => {
                tracing::warn!("replication {name}: spec asked on the apiserver taken (#655)");
                p.rnotes.remove(name);
                s.applied += 1;
            }
            Err(e) => {
                tracing::warn!("replication {name}: spec asked on the apiserver refused: {e} (#655)");
                p.rnotes.entry(name.clone()).or_default().insert("specError".into(), spec_error(asked, 422, &e));
                s.refused += 1;
                if local.is_none() {
                    // Nothing here to publish: the refusal goes on its status.
                    let st = json!({ "status": { "phase": "Failed", "specError": spec_error(asked, 422, &e) } });
                    call(state, "PATCH", &format!("{GROUP_PATH}/replications/{name}/status"), Some(&st)).await.map_err(|(_, e)| e)?;
                }
            }
        }
    }
    // Deleted on the apiserver after the engine had seen it there.
    for r in repl::list(state) {
        if !there.contains_key(&r.name) && seen_now.contains(&r.name) {
            tracing::warn!("replication {}: its object was deleted on the apiserver: deleted here (#655)", r.name);
            repl::delete(state, &r.name);
            seen_now.remove(&r.name);
            p.rnotes.remove(&r.name);
            s.deleted += 1;
        }
    }
    let local = repl::list(state);
    let names: BTreeSet<String> = local.iter().map(|r| r.name.clone()).collect();
    for r in &local {
        let have = there.get(&r.name);
        if let Some(h) = have.filter(|h| !mine(h, &node)) {
            if p.said.insert(format!("replication-owned/{}", r.name)) {
                tracing::warn!(
                    "kubernetes: Replication {} on the apiserver is {:?}'s, not this node's ({node}): this node's is not published (#655)",
                    r.name,
                    h["metadata"]["labels"][NODE_LABEL]
                );
            }
            continue;
        }
        let mut v = serde_json::to_value(r.object()).map_err(|e| e.to_string())?;
        let spec = v["spec"].clone();
        v["metadata"] = json!({
            "name": r.name,
            "labels": { NODE_LABEL: node },
            "annotations": { ENGINE_SPEC: spec.to_string() },
        });
        add_notes(&mut v, p.rnotes.get(&r.name));
        let (c, pa) = write(state, "replications", "Replication", &v, have).await?;
        s.created += usize::from(c);
        s.patched += usize::from(pa);
        seen_now.insert(r.name.clone());
    }
    // The engine's, gone here: the object goes.
    for (name, h) in &there {
        if mine(h, &node) && !names.contains(name) && annotation_spec(h).is_some() {
            match call(state, "DELETE", &format!("{GROUP_PATH}/replications/{name}"), None).await {
                Ok(_) | Err((404, _)) => s.deleted += 1,
                Err((_, e)) => return Err(e),
            }
            seen_now.remove(name);
        }
    }
    seen_now.retain(|n| names.contains(n));
    p.rnotes.retain(|k, _| names.contains(k));
    keep_seen(state, p, seen_now);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #623: the install record as a NodeInstall: none with no record, the
    /// drives cut to what they are and where they are, no clock fields.
    #[test]
    fn a_node_install_is_the_install_record() {
        let empty = crate::install_progress::record(std::path::Path::new("/nonexistent/stormblock-623"), 100);
        assert!(node_install_object("n1", &empty).is_none(), "no record, no object");

        let w = tempfile::tempdir().unwrap();
        crate::install_progress::append(w.path(), "stage", "formatting", "running", "2 drives", 10).unwrap();
        crate::install_progress::append(w.path(), "step", "format", "running", "", 11).unwrap();
        let rec = crate::install_progress::record(w.path(), 200);
        let o = node_install_object("n1", &rec).expect("a record makes an object");
        assert_eq!(o["metadata"]["name"], json!("n1"));
        assert_eq!(o["metadata"]["labels"][NODE_LABEL], json!("n1"));
        let st = &o["status"];
        assert_eq!(st["stage"]["stage"], json!("formatting"));
        assert!(st["steps"].as_array().unwrap().iter().any(|s| s["step"] == json!("format") && s["state"] == json!("running")));
        assert!(st.get("at").is_none() && st.get("console").is_none(), "{st}");
        // Read again later: the same object, so nothing is written.
        let again = node_install_object("n1", &crate::install_progress::record(w.path(), 900)).unwrap();
        assert!(same(&o, &again), "the clock alone changes nothing");

        let rec = json!({
            "state": "formatting", "stage": {"stage": "formatting"}, "started": 10, "eta_s": 600,
            "stages": [], "layout": [],
            "drives": [{"sas": "0x5000cca26801b649", "dev": "sda", "state": "formatting", "percent": 40,
                        "eta_s": 600, "card": {"phys": 8}, "lines": ["a", "b", "FORMAT: 40%"]},
                       {"sas": null, "dev": "sdb", "state": "laid"}],
        });
        let st = node_install_object("n2", &rec).unwrap()["status"].clone();
        assert_eq!(st["drivesTotal"], json!(2));
        assert_eq!(st["drivesFormatting"], json!(1));
        assert_eq!(st["etaSeconds"], json!(600));
        let d0 = &st["drives"][0];
        assert_eq!(d0["line"], json!("FORMAT: 40%"));
        assert!(d0.get("card").is_none() && d0.get("lines").is_none(), "{d0}");
        assert!(st["drives"][1].get("sas").is_none(), "nulls left out");
    }

    #[test]
    fn same_ignores_nulls_and_number_spelling() {
        assert!(same(&json!({ "a": 1, "b": null }), &json!({ "a": 1.0 })));
        assert!(!same(&json!({ "a": 1 }), &json!({ "a": 2 })));
        assert!(same(&json!([{ "x": null }]), &json!([{}])));
        assert!(!same(&json!([1]), &json!([1, 2])));
    }

    #[test]
    fn patch_to_nulls_what_is_gone_and_keeps_what_is_same() {
        let p = patch_to(&json!({ "a": 1, "n": { "x": 1, "y": 2 } }), &json!({ "a": 1, "b": 3, "n": { "x": 1, "z": 9 } }));
        assert_eq!(p, json!({ "b": null, "n": { "y": 2, "z": null } }));
    }

    #[test]
    fn a_change_is_what_differs_from_what_the_engine_wrote() {
        let applied = json!({ "sizeBytes": 4294967296u64, "sealed": false, "access": "rw" });
        let asked = json!({ "sizeBytes": 1099511627776u64, "sealed": false, "access": "rw", "name": "registry-data" });
        let c = volume_changes(&asked, &applied);
        assert_eq!(Value::Object(c), json!({ "sizeBytes": 1099511627776u64 }));
        assert!(volume_changes(&applied, &applied).is_empty());
        let c = volume_changes(&json!({ "resync": true }), &applied);
        assert_eq!(Value::Object(c), json!({ "resync": true }));
    }

    #[test]
    fn writable_fields_only() {
        let w = volume_writable(&json!({ "name": "x", "sizeBytes": 1, "role": "data", "access": "rw", "fs": null }));
        assert_eq!(w, json!({ "sizeBytes": 1, "access": "rw" }));
    }
}
