//! Namespaces (#408, stormcentral#642): forge holds more than stormcos —
//! nana builds goldens there too — so a volume belongs to a family's
//! namespace, a Kubernetes namespace on forge's apiserver.
//!
//! The namespace is on the volume (`VolumeManager::namespace`, the v2 header's
//! `N` block). Here: what a namespace holds and may hold.
//!
//! - **Names.** A name is unique within a namespace other than the default
//!   (`stormcos`): a create, clone or import that would make a second
//!   `nana/golden-x` is undone and answered 409 naming the first. The default
//!   keeps the engine's old rule (names repeat; `{id}` routes refuse an
//!   ambiguous one, #112), since nodes today hold many volumes of one name.
//! - **Quotas.** `quotas.json` in the data directory: per namespace, at most
//!   so many volumes and so many bytes. A create, clone or import past one is
//!   undone and answered 403 `quota_exceeded` with the numbers.
//! - **Usage.** A sealed volume counts what it has allocated (it is what
//!   clones share); an unsealed one what it alone holds (allocated less what
//!   it shares), so a clone of a golden costs only what it wrote.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::mgmt::AppState;
use crate::volume::catalog::VolumeView;
use crate::volume::metadata::{valid_namespace, DEFAULT_NAMESPACE};
use crate::volume::VolumeId;

const FILE: &str = "quotas.json";

/// What a namespace may hold. `None`: no limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quota {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_volumes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

/// What a namespace holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub volumes: u64,
    /// Sealed volumes (goldens, blanks, media).
    pub goldens: u64,
    pub bytes: u64,
}

/// The quotas, loaded once and written on every change (small).
#[derive(Default)]
pub struct Quotas {
    path: Option<PathBuf>,
    map: BTreeMap<String, Quota>,
}

impl Quotas {
    pub fn load(data_dir: Option<&Path>) -> Self {
        let path = data_dir.map(|d| d.join(FILE));
        let map = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Quotas { path, map }
    }

    pub fn get(&self, ns: &str) -> Option<Quota> {
        self.map.get(ns).copied()
    }

    pub fn all(&self) -> &BTreeMap<String, Quota> {
        &self.map
    }

    pub fn set(&mut self, ns: &str, q: Quota) -> anyhow::Result<()> {
        let before = self.map.insert(ns.to_string(), q);
        if let Err(e) = self.save() {
            match before {
                Some(b) => self.map.insert(ns.to_string(), b),
                None => self.map.remove(ns),
            };
            return Err(e);
        }
        Ok(())
    }

    pub fn remove(&mut self, ns: &str) -> anyhow::Result<bool> {
        let Some(before) = self.map.remove(ns) else { return Ok(false) };
        if let Err(e) = self.save() {
            self.map.insert(ns.to_string(), before);
            return Err(e);
        }
        Ok(true)
    }

    fn save(&self) -> anyhow::Result<()> {
        let Some(p) = &self.path else { return Ok(()) };
        crate::serve::wiring::write_atomic(p, &serde_json::to_vec_pretty(&self.map)?)
    }
}

/// Every namespace's usage, from a view of the volumes.
pub async fn usage(vm: &impl VolumeView) -> BTreeMap<String, Usage> {
    let mut out: BTreeMap<String, Usage> = BTreeMap::new();
    for id in vm.volume_ids() {
        let Some(h) = vm.get_volume_handle(&id) else { continue };
        let u = out.entry(vm.namespace(&id).to_string()).or_default();
        u.volumes += 1;
        let allocated = h.allocated().await;
        if h.is_sealed() {
            u.goldens += 1;
            u.bytes += allocated;
        } else {
            u.bytes += allocated.saturating_sub(h.shared().await);
        }
    }
    out
}

/// A view of the volumes as they are now: the published catalog when it is
/// current, else the manager publishes one (held only for that).
pub async fn current_view(state: &AppState) -> Option<Arc<crate::volume::catalog::Catalog>> {
    if let Some(c) = state.volume_catalog.latest() {
        if crate::volume::catalog::is_current(&c, &state.volume_presence) {
            return Some(c);
        }
    }
    state.volume_manager.lock().await.publish_catalog();
    state.volume_catalog.latest()
}

/// Why a namespace may not take one more volume, if it may not.
pub fn over(ns: &str, q: Option<Quota>, u: Usage) -> Option<String> {
    let q = q?;
    if let Some(m) = q.max_volumes {
        if u.volumes > m {
            return Some(format!("namespace '{ns}' holds {} volumes, its quota is {m}", u.volumes));
        }
    }
    if let Some(m) = q.max_bytes {
        if u.bytes > m {
            return Some(format!("namespace '{ns}' holds {} bytes, its quota is {m}", u.bytes));
        }
    }
    None
}

fn refused(status: axum::http::StatusCode, code: &str, message: String, extra: serde_json::Value) -> Response {
    let mut body = serde_json::json!({ "code": code, "error": message });
    if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            b.insert(k.clone(), v.clone());
        }
    }
    (status, Json(body)).into_response()
}

/// Why a volume just made may not stay: the status, a code, the message
/// and what to add to the body.
pub struct Refusal {
    pub status: axum::http::StatusCode,
    pub code: &'static str,
    pub message: String,
    pub extra: serde_json::Value,
}

impl Refusal {
    pub fn response(self) -> Response {
        refused(self.status, self.code, self.message, self.extra)
    }
}

/// Settle a volume just made (create, clone, import) in its namespace:
/// `asked` when the request named one (else the volume keeps what it has,
/// its source's for a clone), its name unique there, and the quota kept.
/// On a refusal the volume is deleted and the answer returned.
pub async fn settle_new_volume(state: &AppState, id: VolumeId, asked: Option<&str>) -> Option<Response> {
    settle(state, id, asked).await.err().map(Refusal::response)
}

/// [`settle_new_volume`], the refusal as it is.
pub async fn settle(state: &AppState, id: VolumeId, asked: Option<&str>) -> Result<(), Refusal> {
    let (ns, clash) = {
        let mut vm = state.volume_manager.lock().await;
        if let Some(ns) = asked {
            if let Err(e) = vm.set_namespace(id, ns) {
                drop(vm);
                undo(state, id).await;
                return Err(Refusal {
                    status: axum::http::StatusCode::BAD_REQUEST,
                    code: "bad_namespace",
                    message: e.to_string(),
                    extra: serde_json::json!({}),
                });
            }
        }
        let ns = vm.namespace(&id).to_string();
        let name = match vm.get_volume_handle(&id) {
            Some(h) => h.name().await,
            None => return Ok(()),
        };
        let clash = if ns == DEFAULT_NAMESPACE { Vec::new() } else { vm.named_in(&ns, &name, Some(id)).await };
        (ns, clash.first().map(|c| (name, *c)))
    };
    if let Some((name, other)) = clash {
        undo(state, id).await;
        return Err(Refusal {
            status: axum::http::StatusCode::CONFLICT,
            code: "name_taken",
            message: format!("namespace '{ns}' already has a volume named '{name}' ({})", other.0),
            extra: serde_json::json!({ "namespace": ns, "volume": other.0 }),
        });
    }
    let quota = state.quotas.lock().unwrap_or_else(|e| e.into_inner()).get(&ns);
    if quota.is_some() {
        if let Some(view) = current_view(state).await {
            let u = usage(&*view).await.get(&ns).copied().unwrap_or_default();
            if let Some(why) = over(&ns, quota, u) {
                undo(state, id).await;
                return Err(Refusal {
                    status: axum::http::StatusCode::FORBIDDEN,
                    code: "quota_exceeded",
                    message: why,
                    extra: serde_json::json!({ "namespace": ns, "quota": quota, "usage": u }),
                });
            }
        }
    }
    Ok(())
}

async fn undo(state: &AppState, id: VolumeId) {
    let mut vm = state.volume_manager.lock().await;
    if let Err(e) = vm.delete_volume(id).await {
        tracing::warn!("namespace refusal: undoing volume {}: {e}", id.0);
    }
    vm.publish_catalog();
}

/// The namespace a request names in `?namespace=` or a JSON body's
/// `namespace`, validated.
pub fn asked(ns: Option<&str>) -> Result<Option<&str>, Response> {
    match ns {
        None => Ok(None),
        Some(n) if valid_namespace(n) => Ok(Some(n)),
        Some(n) => Err(refused(
            axum::http::StatusCode::BAD_REQUEST,
            "bad_namespace",
            format!("'{n}' is not a namespace name (a DNS-1123 label)"),
            serde_json::json!({}),
        )),
    }
}

/// The namespace a Kubernetes bearer's request on `/api/v1/volumes` is
/// reviewed in (#408): `?namespace=`; else the volume the path names (by id,
/// or by a name one volume has); else a create's or import's JSON body's
/// `namespace`; else the default. `None` for anything that is not a volume
/// (slabs, drives, …: node-wide, reviewed cluster-wide) and for the list,
/// which the auth layer reviews per namespace.
pub async fn for_review(state: &AppState, path: &str, query: Option<&str>, body: Option<&[u8]>) -> Option<String> {
    let rest = path.trim_end_matches('/').strip_prefix("/api/v1/volumes")?;
    if let Some(n) = query.and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("namespace="))).filter(|n| !n.is_empty()) {
        return Some(n.to_string());
    }
    let key = rest.trim_start_matches('/').split('/').next().unwrap_or("");
    let collection = matches!(key, "" | "import" | "snapshots" | "compose");
    if !collection {
        if let Some(view) = current_view(state).await {
            let id = match key.parse::<uuid::Uuid>() {
                Ok(u) => Some(VolumeId(u)),
                Err(_) => {
                    let mut hit = None;
                    for id in view.volume_ids() {
                        if let Some(h) = view.get_volume_handle(&id) {
                            if h.name().await == key {
                                if hit.is_some() {
                                    hit = None;
                                    break;
                                }
                                hit = Some(id);
                            }
                        }
                    }
                    hit
                }
            };
            if let Some(id) = id.filter(|id| view.get_volume_handle(id).is_some()) {
                return Some(view.namespace(&id).to_string());
            }
        }
    }
    if let Some(n) = body
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
        .and_then(|v| v.get("namespace").and_then(|n| n.as_str()).map(str::to_string))
    {
        return Some(n);
    }
    (!(key.is_empty() && collection && body.is_none())).then(|| DEFAULT_NAMESPACE.to_string())
}

// --- the API -------------------------------------------------------------

/// `GET /api/v1/namespaces`: every namespace that holds a volume or has a
/// quota, with its usage and quota.
pub async fn list(axum::extract::State(state): axum::extract::State<Arc<AppState>>) -> Response {
    let usage = match current_view(&state).await {
        Some(v) => usage(&*v).await,
        None => BTreeMap::new(),
    };
    let quotas = state.quotas.lock().unwrap_or_else(|e| e.into_inner()).all().clone();
    let mut names: Vec<&String> = usage.keys().chain(quotas.keys()).collect();
    names.sort();
    names.dedup();
    let rows: Vec<_> = names
        .into_iter()
        .map(|n| serde_json::json!({ "name": n, "usage": usage.get(n).copied().unwrap_or_default(), "quota": quotas.get(n) }))
        .collect();
    Json(serde_json::json!({ "namespaces": rows, "default": DEFAULT_NAMESPACE })).into_response()
}

/// `GET /api/v1/namespaces/{ns}`.
pub async fn get(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(ns): axum::extract::Path<String>,
) -> Response {
    if let Err(r) = asked(Some(&ns)) {
        return r;
    }
    let u = match current_view(&state).await {
        Some(v) => usage(&*v).await.get(&ns).copied().unwrap_or_default(),
        None => Usage::default(),
    };
    let q = state.quotas.lock().unwrap_or_else(|e| e.into_inner()).get(&ns);
    Json(serde_json::json!({ "name": ns, "usage": u, "quota": q })).into_response()
}

/// `PUT /api/v1/namespaces/{ns}/quota {max_volumes?, max_bytes?}`
/// (destructive): what the namespace may hold from now on. What it holds
/// already is kept; nothing more is made past it.
pub async fn put_quota(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(ns): axum::extract::Path<String>,
    Json(q): Json<Quota>,
) -> Response {
    if let Err(r) = asked(Some(&ns)) {
        return r;
    }
    if let Err(e) = state.quotas.lock().unwrap_or_else(|e| e.into_inner()).set(&ns, q) {
        return crate::mgmt::api::ApiError::internal(format!("{FILE}: {e}"));
    }
    tracing::info!("namespace '{ns}': quota {q:?}");
    Json(serde_json::json!({ "name": ns, "quota": q })).into_response()
}

/// `DELETE /api/v1/namespaces/{ns}/quota` (destructive): no limit.
pub async fn delete_quota(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(ns): axum::extract::Path<String>,
) -> Response {
    match state.quotas.lock().unwrap_or_else(|e| e.into_inner()).remove(&ns) {
        Ok(true) => axum::http::StatusCode::NO_CONTENT.into_response(),
        Ok(false) => crate::mgmt::api::ApiError::not_found(format!("namespace '{ns}' has no quota")),
        Err(e) => crate::mgmt::api::ApiError::internal(format!("{FILE}: {e}")),
    }
}

/// Nested at `/api/v1/namespaces`.
pub fn router(state: Arc<AppState>) -> axum::Router {
    use axum::routing::{get as g, put};
    axum::Router::new()
        .route("/", g(list))
        .route("/{ns}", g(get))
        .route("/{ns}/quota", put(put_quota).delete(delete_quota))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quota_is_kept_and_a_namespace_over_it_is_said() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = Quotas::load(Some(dir.path()));
        q.set("nana", Quota { max_volumes: Some(2), max_bytes: None }).unwrap();
        let again = Quotas::load(Some(dir.path()));
        let nana = again.get("nana");
        assert_eq!(nana, Some(Quota { max_volumes: Some(2), max_bytes: None }));
        assert!(over("nana", nana, Usage { volumes: 2, goldens: 0, bytes: 0 }).is_none());
        assert!(over("nana", nana, Usage { volumes: 3, goldens: 0, bytes: 0 }).unwrap().contains("quota is 2"));
        assert!(over("stormcos", again.get("stormcos"), Usage { volumes: 900, goldens: 9, bytes: 1 << 40 }).is_none());
        let mut q = again;
        assert!(q.remove("nana").unwrap());
        assert!(!q.remove("nana").unwrap());
        assert_eq!(Quotas::load(Some(dir.path())).get("nana"), None);
    }
}
