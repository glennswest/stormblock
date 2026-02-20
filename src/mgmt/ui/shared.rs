//! Shared UI utilities — template rendering, HTMX detection, toast, custom filters.

use askama::Template;
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Response};

/// Check if the request is an HTMX partial request.
pub fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

/// Render an askama template into an axum HTML response.
pub fn render<T: Template>(tmpl: &T) -> Response {
    match tmpl.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("template render error: {e}");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Html("<p>Template render error</p>".to_string()),
            )
                .into_response()
        }
    }
}

/// Toast notification template (returned as OOB swap).
#[derive(Template)]
#[template(path = "_toast.html")]
pub struct ToastTemplate {
    pub message: String,
    pub level: String,
}

/// Render a toast OOB fragment to append alongside a primary response.
pub fn toast_oob(message: &str, level: &str) -> String {
    format!(
        r#"<div id="toast" hx-swap-oob="innerHTML:#toast-container"><div class="toast toast-{level}" hx-get="data:text/html," hx-trigger="load delay:3s" hx-swap="outerHTML" hx-target="this">{message}</div></div>"#,
        level = level,
        message = message,
    )
}

/// Askama custom filter: truncate a UUID to first 8 chars.
pub mod filters {
    use uuid::Uuid;

    pub fn truncate_uuid(uuid: &Uuid) -> ::askama::Result<String> {
        let s = uuid.to_string();
        Ok(s[..8].to_string())
    }
}
