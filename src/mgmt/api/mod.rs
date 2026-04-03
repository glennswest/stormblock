//! REST API routes — /api/v1/{drives,arrays,volumes,exports}.

pub mod drives;
pub mod arrays;
pub mod volumes;
pub mod exports;
pub mod slabs;
#[cfg(feature = "iscsi")]
pub mod luns;
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

/// Build the complete API router.
pub fn router(state: Arc<AppState>) -> Router {
    let r = Router::new()
        .nest("/api/v1/drives", drives::router(state.clone()))
        .nest("/api/v1/arrays", arrays::router(state.clone()))
        .nest("/api/v1/volumes", volumes::router(state.clone()))
        .nest("/api/v1/exports", exports::router(state.clone()))
        .nest("/api/v1/slabs", slabs::router(state.clone()));

    #[cfg(feature = "iscsi")]
    let r = r.nest("/api/v1/luns", luns::router(state.clone()));

    #[cfg(feature = "cluster")]
    let r = r.merge(cluster::router(state.clone()));

    #[cfg(feature = "cluster")]
    let r = if let Some(ref cluster_mgr) = state.cluster {
        r.merge(cluster_mgr.rpc_router())
    } else {
        r
    };

    r
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

/// Standard list response wrapper.
#[derive(Debug, Serialize)]
pub struct ListResponse<T: Serialize> {
    pub items: Vec<T>,
    pub count: usize,
}
