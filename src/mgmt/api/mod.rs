//! REST API routes — /api/v1/{drives,arrays,volumes,exports}.

pub mod drives;
pub mod arrays;
pub mod volumes;
pub mod exports;
pub mod fstemplates;
pub mod images;
pub mod pallets;
pub mod slabs;
#[cfg(feature = "stormfs-data")]
pub mod stormfs;
pub mod discovery;
pub mod v1;
pub mod kube;
pub mod moves;
pub mod synonyms;
pub mod releases;
#[cfg(feature = "iscsi")]
pub mod luns;
#[cfg(feature = "iscsi")]
pub mod sessions;
#[cfg(feature = "cluster")]
pub mod cluster;

use std::sync::Arc;

use axum::{
    Router,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use super::AppState;

/// `GET /api/v1/health` — is there an engine here, and which one.
///
/// The one endpoint whose answer is the question. Everything else needs a
/// noun: a volume, a slab, a release. A machine that has just come up on a
/// network it was told nothing about has no noun to ask for yet — it is
/// trying to establish whether the thing at this address is an appliance at
/// all, and it has to do that before it can ask for anything.
///
/// Its absence cost five boots. The initramfs probes every candidate address
/// it can find with this path, and it 404'd on the appliance exactly as it
/// did on the machines that were not one, so a node that had been handed the
/// right address by DHCP reported "no engine" about it and dropped to a
/// shell.
///
/// Cheap on purpose: no locks, no I/O, no counting. A discovery probe runs
/// against several addresses on every boot of every node, and one that reads
/// state is one that answers slowly when the appliance is busy — which is
/// when a node most wants an answer.
/// It reports `auth` for the reason #107 exists: a fleet has to be able to ask
/// which of its nodes are open without trying to break into each one. Saying
/// "a token is required here" tells an attacker nothing they do not learn from
/// the first 401.
async fn health(axum::extract::State(state): axum::extract::State<Arc<AppState>>) -> Response {
    #[derive(Serialize)]
    struct Health {
        status: &'static str,
        service: &'static str,
        version: &'static str,
        auth: &'static str,
    }
    Json(Health {
        status: "ok",
        service: "stormblock",
        version: env!("CARGO_PKG_VERSION"),
        auth: if state.auth_enforced() { "required" } else { "none" },
    })
    .into_response()
}

/// Build the complete API router.
pub fn router(state: Arc<AppState>) -> Router {
    let r = Router::new()
        .route("/api/v1/health", axum::routing::get(health).with_state(state.clone()))
        .nest("/api/v1/drives", drives::router(state.clone()))
        .nest("/api/v1/arrays", arrays::router(state.clone()))
        .nest("/api/v1/volumes", volumes::router(state.clone()))
        .nest("/api/v1/exports", exports::router(state.clone()))
        .nest("/api/v1/slabs", slabs::router(state.clone()))
        .nest("/api/v1/pallets", pallets::router(state.clone()))
        .nest("/api/v1/images", images::router(state.clone()))
        .nest("/api/v1/fstemplates", fstemplates::router(state.clone()))
        .nest("/api/v1/moves", moves::router(state.clone()))
        .nest("/api/v1/synonyms", synonyms::router(state.clone()))
        .nest("/api/v1/releases", releases::router(state.clone()))
        .nest("/api/v1/discovery", discovery::router(state.clone()))
        // CSI/wander-operator contract surface (stormblock-csi docs/stormblock-api.md)
        .nest("/v1", v1::router(state.clone()))
        // Kubernetes-shaped resources, served by the engine itself (#80).
        .merge(kube::router(state.clone()));

    // The StormFS data path (#49, #50). Out of the `mikrotik` profile: a
    // RouterOS node with 256 MB serves container volumes over NVMe-TCP; it is
    // not a StormFS data node, and a surface that is there invites being
    // called.
    #[cfg(feature = "stormfs-data")]
    let r = r.nest("/api/v1/stormfs", stormfs::router(state.clone()));

    // The serving surface (#60). Layer 2 in `docs/layering.md` — "what it
    // takes to serve volumes to something", which is the job rather than a
    // choice a deployment makes differently. Mounted here rather than by each
    // profile so that a consumer calling it can rely on it being there:
    // a surface only some profiles serve is a convention, not a guarantee.
    let r = match state.serve.get() {
        Some(serve) => r.merge(crate::serve::api::router(serve.clone())),
        None => r,
    };

    #[cfg(feature = "iscsi")]
    let r = r.nest("/api/v1/luns", luns::router(state.clone()));

    #[cfg(feature = "iscsi")]
    let r = r.nest("/api/v1/sessions", sessions::router(state.clone()));

    #[cfg(feature = "cluster")]
    let r = r.merge(cluster::router(state.clone()));

    #[cfg(feature = "cluster")]
    let r = if let Some(ref cluster_mgr) = state.cluster {
        r.merge(cluster_mgr.rpc_router())
    } else {
        r
    };

    // One credential check, over every surface this router serves (#107).
    //
    // It goes on last so it wraps `/api/v1`, `/v1`, `/serve/v1` and the kube
    // resources alike — and unmatched paths too, which is right: a 404 is
    // still an answer about what this node is. `/v1` used to check a token by
    // itself and everything beside it was open, which is the shape of hole
    // that survives review: the setting existed, so the surface read as
    // guarded.
    //
    // What it enforces is whatever the node resolved at startup
    // (`mgmt::auth::resolve`). A router built in-process with no resolution
    // enforces the config's own token, or nothing.
    r.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        crate::mgmt::auth::require_token,
    ))
}

/// Standard error response.
#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
    pub code: u16,
}

impl ApiError {
    pub fn not_found(msg: impl Into<String>) -> Response {
        let body = ApiError {
            error: msg.into(),
            code: 404,
        };
        (StatusCode::NOT_FOUND, Json(body)).into_response()
    }

    pub fn bad_request(msg: impl Into<String>) -> Response {
        let body = ApiError {
            error: msg.into(),
            code: 400,
        };
        (StatusCode::BAD_REQUEST, Json(body)).into_response()
    }

    pub fn conflict(msg: impl Into<String>) -> Response {
        let body = ApiError {
            error: msg.into(),
            code: 409,
        };
        (StatusCode::CONFLICT, Json(body)).into_response()
    }

    pub fn internal(msg: impl Into<String>) -> Response {
        let body = ApiError {
            error: msg.into(),
            code: 500,
        };
        (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
    }
}

/// Everything on this node that is currently serving `volume_id`.
///
/// One answer, shared: a volume-level move must not copy a filesystem
/// something is still writing to (#20), and the template orphan sweep must not
/// delete a volume something has attached (#48). Both questions are the same
/// question, and having two implementations of it means one of them is
/// eventually wrong.
/// Lock order note: the StormFS state is taken **first** here, because two of
/// its own handlers hold it while reaching for the volume manager (#49, #50).
/// One order, stated once.
pub async fn what_is_serving(state: &AppState, volume_id: uuid::Uuid) -> Vec<String> {
    let mut busy = Vec::new();

    // A pin's snapshot is being read by a StormFS reader that has no other
    // way of saying so — deleting it out from under them is the exact thing
    // the pin exists to prevent.
    #[cfg(feature = "stormfs-data")]
    {
        let st = state.stormfs.lock().await;
        if st.pins.is_pinned_snapshot(crate::volume::VolumeId(volume_id)) {
            busy.push("a StormFS version pin".to_string());
        }
    }

    for e in state.exports.read().await.iter() {
        if e.volume_id == volume_id {
            busy.push(format!("export {} ({})", e.id, e.protocol));
        }
    }
    #[cfg(feature = "iscsi")]
    for (lun_id, entry) in state.lun_entries.read().await.iter() {
        if matches!(entry.backing, crate::mgmt::LunBacking::Volume { volume_id: v } if v == volume_id)
        {
            busy.push(format!("iSCSI LUN {lun_id}"));
        }
    }
    if state.ublk_exports.lock().await.is_exported(&volume_id.to_string()) {
        busy.push("a ublk device".to_string());
    }
    busy
}

/// Every volume this node is currently serving.
///
/// The set form, for callers deciding what is safe to delete in bulk.
pub async fn volumes_in_use(state: &AppState) -> std::collections::HashSet<uuid::Uuid> {
    let mut in_use = std::collections::HashSet::new();
    // StormFS lock first, as above — this one goes on to take the volume
    // manager, so the order is not optional.
    #[cfg(feature = "stormfs-data")]
    for pin in state.stormfs.lock().await.pins.list() {
        in_use.insert(pin.snapshot.0);
    }
    for e in state.exports.read().await.iter() {
        in_use.insert(e.volume_id);
    }
    #[cfg(feature = "iscsi")]
    for entry in state.lun_entries.read().await.values() {
        if let crate::mgmt::LunBacking::Volume { volume_id } = entry.backing {
            in_use.insert(volume_id);
        }
    }
    // ublk exports are keyed by the /v1 volume id rather than the engine's, so
    // they are matched by string against the engine ids we already have.
    let ublk = state.ublk_exports.lock().await;
    let volumes: Vec<uuid::Uuid> = state
        .volume_manager
        .lock()
        .await
        .list_volumes()
        .await
        .into_iter()
        .map(|(id, _, _, _)| id.0)
        .collect();
    for id in volumes {
        if ublk.is_exported(&id.to_string()) {
            in_use.insert(id);
        }
    }
    in_use
}

/// Standard list response wrapper.
#[derive(Debug, Serialize)]
pub struct ListResponse<T: Serialize> {
    pub items: Vec<T>,
    pub count: usize,
}
