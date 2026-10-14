//! Who made what (#422, the owner's option 1 on stormbootx-test#2): the
//! caller that created a volume, a synonym or (through its `boothost/`
//! synonym) a host record, and the labels it gave.
//!
//! A storage-user may delete what it created itself and nothing else: a
//! Kubernetes bearer's delete, detach, synonym delete or host delete of an
//! object recorded as its own is reviewed as `delete`; of anything else, as
//! `destroy`, which only storage-admin holds (`mgmt::auth`). Objects with no
//! record (every one made before this) are never anyone's.
//!
//! Kept in `<data_dir>/creators.json`, not on the slab: an older engine
//! reads the slabs as before, and losing the file loses only ownership,
//! which makes a delete need storage-admin again.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The label a test run marks its objects with, to list them.
pub const TEST_RUN_LABEL: &str = "storm.io/test-run";

const FILE: &str = "creators.json";

/// Who made an object, and its labels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Creator {
    /// The reviewed caller: `kubernetes:system:serviceaccount:<ns>:<name>`,
    /// `node` (the node token), `admin`.
    pub who: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Seconds since the epoch.
    pub at: u64,
}

impl Creator {
    pub fn new(who: impl Into<String>, labels: BTreeMap<String, String>) -> Self {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Creator { who: who.into(), labels, at }
    }

    /// Whether a Kubernetes user is this object's creator.
    pub fn is(&self, username: &str) -> bool {
        self.who.strip_prefix("kubernetes:") == Some(username)
    }
}

pub fn volume(id: &uuid::Uuid) -> String {
    format!("volume:{id}")
}

pub fn synonym(namespace: &str, name: &str) -> String {
    format!("synonym:{namespace}/{name}")
}

/// A host record: made with its `boothost/<name>` synonym, and kept after
/// that synonym is deleted (the record outlives it once a claim made it).
pub fn host(name: &str) -> String {
    format!("host:{name}")
}

/// The record, loaded once and written on every change (small).
#[derive(Default)]
pub struct Creators {
    path: Option<PathBuf>,
    map: HashMap<String, Creator>,
}

impl Creators {
    pub fn load(data_dir: Option<&Path>) -> Self {
        let path = data_dir.map(|d| d.join(FILE));
        let map = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Creators { path, map }
    }

    pub fn get(&self, key: &str) -> Option<&Creator> {
        self.map.get(key)
    }

    pub fn set(&mut self, key: String, c: Creator) {
        self.map.insert(key, c);
        self.save();
    }

    pub fn remove(&mut self, key: &str) {
        if self.map.remove(key).is_some() {
            self.save();
        }
    }

    /// Keys whose labels carry `label=value`.
    pub fn labelled(&self, label: &str, value: &str) -> Vec<String> {
        let mut v: Vec<String> =
            self.map.iter().filter(|(_, c)| c.labels.get(label).map(String::as_str) == Some(value)).map(|(k, _)| k.clone()).collect();
        v.sort();
        v
    }

    fn save(&self) {
        let Some(p) = &self.path else { return };
        match serde_json::to_vec_pretty(&self.map) {
            Ok(b) => {
                if let Err(e) = crate::serve::wiring::write_atomic(p, &b) {
                    tracing::warn!("{}: {e}", p.display());
                }
            }
            Err(e) => tracing::warn!("{FILE}: {e}"),
        }
    }
}

/// Who a request is from, as recorded: the caller the auth layer named
/// (`mgmt::auth::Caller`), else the node token's holder.
pub fn who(caller: Option<&crate::mgmt::auth::Caller>) -> String {
    caller.map(|c| c.0.clone()).unwrap_or_else(|| "node".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_is_kept_and_found_by_label() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Creators::load(Some(dir.path()));
        let sa = "system:serviceaccount:stormbootx-test:runner";
        c.set(volume(&uuid::Uuid::nil()), Creator::new(format!("kubernetes:{sa}"), [(TEST_RUN_LABEL.into(), "r1".into())].into()));
        c.set(synonym("boothost", "mac-1"), Creator::new("node", Default::default()));
        let again = Creators::load(Some(dir.path()));
        let v = again.get(&volume(&uuid::Uuid::nil())).unwrap();
        assert!(v.is(sa));
        assert!(!v.is("system:serviceaccount:other:x"));
        assert!(!again.get(&synonym("boothost", "mac-1")).unwrap().is(sa), "the node's is no SA's");
        assert_eq!(again.labelled(TEST_RUN_LABEL, "r1"), vec![volume(&uuid::Uuid::nil())]);
    }
}

/// Run a create handler and, when it succeeds, record its caller as the
/// creator of what it made (#422): `key_of` finds the record's key in the
/// response body; `labels` come from the request body's `labels` object.
pub async fn recorded<F, Fut>(
    state: &std::sync::Arc<crate::mgmt::AppState>,
    caller: Option<&crate::mgmt::auth::Caller>,
    body: &[u8],
    key_of: impl Fn(&serde_json::Value) -> Option<String>,
    run: F,
) -> axum::response::Response
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = axum::response::Response>,
{
    let labels: BTreeMap<String, String> = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("labels").cloned())
        .and_then(|l| serde_json::from_value(l).ok())
        .unwrap_or_default();
    let resp = run().await;
    if !resp.status().is_success() {
        return resp;
    }
    let (parts, b) = resp.into_parts();
    let bytes = match axum::body::to_bytes(b, 64 << 20).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("creator record: reading the reply: {e}");
            return axum::response::Response::from_parts(parts, axum::body::Body::empty());
        }
    };
    if let Some(key) = serde_json::from_slice::<serde_json::Value>(&bytes).ok().as_ref().and_then(&key_of) {
        state.creators.lock().unwrap_or_else(|e| e.into_inner()).set(key, Creator::new(who(caller), labels));
    }
    axum::response::Response::from_parts(parts, axum::body::Body::from(bytes))
}

/// A volume's key from a reply that names it as `id`.
pub fn volume_in(v: &serde_json::Value) -> Option<String> {
    v.get("id").and_then(|i| i.as_str()).and_then(|s| s.parse::<uuid::Uuid>().ok()).map(|u| volume(&u))
}

/// The record of `key`, for a GET reply (`creator`, `labels`).
pub fn of(state: &crate::mgmt::AppState, key: &str) -> Option<Creator> {
    state.creators.lock().unwrap_or_else(|e| e.into_inner()).get(key).cloned()
}

/// Forget `key` (the object is gone).
pub fn forget(state: &crate::mgmt::AppState, key: &str) {
    state.creators.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
}
