//! A plain listener for the boot verbs alone (#524, stormcos#374).
//!
//! The management API goes TLS; pre-boot firmware cannot do TLS (stormbootx is
//! HTTP/1.1 over smoltcp). So `[management] boot_listen` serves what a machine
//! asks before it has a system, with no credential, and nothing else: the
//! whole API behind one filter, so that the auth rules on these verbs and the
//! handlers are the same ones `:9090` has. A verb added to `is_boot_claim` is
//! open here too, which is what that list means.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;

use crate::mgmt::AppState;

/// What this port answers.
pub fn allowed(method: &Method, path: &str) -> bool {
    (*method == Method::GET && path == "/api/v1/health") || crate::serve::api::is_boot_claim(method, path)
}

async fn only_boot_verbs(req: Request, next: Next) -> Response {
    if allowed(req.method(), req.uri().path()) {
        return next.run(req).await;
    }
    (StatusCode::NOT_FOUND, "not on the boot port: the management API is on its own listener\n").into_response()
}

/// The management router, as this port serves it.
pub fn router(full: Router) -> Router {
    full.layer(axum::middleware::from_fn(only_boot_verbs))
}

/// How long between tries at a port something else holds: a predecessor
/// letting go at a handover, usually. The API goes on meanwhile: storage must
/// not stop over the port machines boot through, and health says it is down.
const BIND_RETRY: Duration = Duration::from_secs(5);

/// Serve `router` on `addr` for as long as the engine runs.
pub fn spawn(state: Arc<AppState>, addr: String, full: Router) {
    let app = router(full);
    tokio::spawn(async move {
        let mut said = false;
        loop {
            match tokio::net::TcpListener::bind(&addr).await {
                Ok(listener) => {
                    tracing::warn!(
                        "Boot verbs on {addr} (plain, no credential, #524): what a claim answers crosses in the clear"
                    );
                    said = false;
                    state.boot_listening.store(true, Ordering::Relaxed);
                    let service = app.clone().into_make_service_with_connect_info::<std::net::SocketAddr>();
                    if let Err(e) = axum::serve(listener, service).await {
                        tracing::error!("boot listener on {addr}: {e}");
                    }
                    state.boot_listening.store(false, Ordering::Relaxed);
                }
                Err(e) if !said => {
                    tracing::error!("boot_listen {addr}: {e}; machines that boot through it cannot until it binds. Trying again every {}s", BIND_RETRY.as_secs());
                    said = true;
                }
                Err(_) => {}
            }
            tokio::time::sleep(BIND_RETRY).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_port_answers_the_boot_verbs_and_nothing_else() {
        let b = "/api/v1/synonyms/boothost";
        for (m, p) in [
            (Method::GET, "/api/v1/health".to_string()),
            (Method::GET, format!("{b}/server8/intent")),
            (Method::POST, format!("{b}/server8/claim")),
            (Method::POST, format!("{b}/default/claim")),
            (Method::GET, format!("{b}/server8/command")),
            (Method::POST, format!("{b}/server8/command/done")),
            (Method::POST, format!("{b}/server8/installed")),
        ] {
            assert!(allowed(&m, &p), "{m} {p}");
        }
        for (m, p) in [
            (Method::POST, "/api/v1/health".to_string()),
            (Method::GET, "/api/v1/version".to_string()),
            (Method::GET, "/api/v1/volumes".to_string()),
            (Method::GET, "/metrics".to_string()),
            (Method::GET, "/debug/stalls".to_string()),
            (Method::PUT, format!("{b}/server8")),
            (Method::GET, format!("{b}/server8")),
            (Method::PUT, format!("{b}/server8/intent")),
            (Method::PUT, format!("{b}/server8/command")),
            (Method::GET, format!("{b}/server8/install-config")),
            (Method::POST, "/api/v1/synonyms/other/server8/claim".to_string()),
            (Method::POST, format!("{b}/a/b/claim")),
        ] {
            assert!(!allowed(&m, &p), "{m} {p}");
        }
    }
}
