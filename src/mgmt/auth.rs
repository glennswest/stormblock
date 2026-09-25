//! Who may call this node's management API, and how the node comes by a token.
//!
//! The mechanism was already written — `serve::api::require_token`, with a
//! read token, an admin token for destructive verbs and a public-path
//! exemption. What #107 found is that nothing ever wired it to the engine's
//! own router and nothing minted a token, so `management.api_token` was a
//! setting that guarded `/v1` and left `/api/v1` open on `0.0.0.0:9090`:
//! create, clone, seal, **delete** a volume, withdraw an export, re-point
//! `boothost/<tag>` and so choose what a machine boots at its next power
//! cycle — all of it, from anywhere that could reach the port, with no
//! credential.
//!
//! Three things live here:
//!
//! * **Resolution** — where a token comes from: the config, the environment,
//!   a token file, or minted on the spot into that file. A token cannot be
//!   baked into an image, because every node booting that image would carry
//!   the same one; it has to be made on the node, at boot, and written where
//!   something else on that machine can read it.
//! * **The middleware** the engine's whole router is wrapped in, so one
//!   answer covers `/api/v1`, `/v1`, `/serve/v1` and the kube surface.
//! * **The boot line.** A node that is open says so every time it starts.
//!   Silence is what let this last: nothing fails while it is wrong.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
    http::StatusCode,
    Json,
};
use serde_json::json;

use super::config::ManagementConfig;
use super::AppState;
pub use crate::serve::api::AuthConfig;

/// Where the token in force came from, for the one line the node logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// `management.api_token`.
    Config,
    /// `$STORMBLOCK_API_TOKEN`.
    Env,
    /// An existing token file, read at startup.
    File(PathBuf),
    /// Minted at this boot and written to a token file.
    Minted(PathBuf),
    /// Minted at this boot with nowhere to keep it: held in memory only. The
    /// API is closed, and nothing else can present the token — the node says
    /// so. Closed and unusable beats open (#107).
    Ephemeral,
    /// No token: the API is open.
    None,
    /// No token, and the config says that is deliberate.
    Disabled,
}

impl Source {
    /// Is a credential required?
    pub fn enforced(&self) -> bool {
        !matches!(self, Source::None | Source::Disabled)
    }
}

/// The outcome of resolution: what to enforce, and where it came from.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub auth: AuthConfig,
    pub source: Source,
    /// True when a distinct admin token guards destructive verbs.
    pub admin: bool,
}

/// A fresh token: 244 bits of randomness as 64 hex characters.
///
/// Two v4 uuids rather than a new dependency — `uuid`'s v4 is already here and
/// already reads the OS random source. Hex because this ends up in a shell
/// variable, an HTTP header and a TOML file, and anything that needs quoting
/// in one of those will eventually be pasted without them.
pub fn mint() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Where a minted token is kept: the configured path, else `<data_dir>`, else
/// the config directory a packaged node already has.
pub fn token_file(mgmt: &ManagementConfig) -> Option<PathBuf> {
    if let Some(p) = mgmt.token_file.as_deref() {
        return Some(PathBuf::from(p));
    }
    if let Some(d) = mgmt.data_dir.as_deref() {
        return Some(Path::new(d).join("api_token"));
    }
    let etc = Path::new("/etc/stormblock");
    etc.is_dir().then(|| etc.join("api_token"))
}

fn read_token_file(path: &Path) -> Option<String> {
    let t = std::fs::read_to_string(path).ok()?;
    let t = t.trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// Write a token where only this node's own processes can read it.
///
/// The mode is set before the token is written, not after: a file that is
/// world-readable for the width of one `write` is world-readable, and the
/// window is exactly when something is watching the directory it appeared in.
fn write_token_file(path: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    // An existing file keeps its old mode through `create`, so say it again.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    writeln!(f, "{token}")?;
    f.flush()
}

/// Decide what this node enforces.
///
/// Order for the ordinary token: `management.api_token`, `$STORMBLOCK_API_TOKEN`,
/// an existing token file. With `require_auth = true` and none of those, one is
/// minted into the token file — and if there is nowhere to write it, startup
/// fails rather than quietly falling back to open, which is the failure mode
/// this whole module exists to end.
pub fn resolve(mgmt: &ManagementConfig) -> anyhow::Result<Resolved> {
    let admin_token = mgmt
        .admin_token
        .clone()
        .or_else(|| non_empty(std::env::var("STORMBLOCK_ADMIN_TOKEN").ok()));

    if mgmt.require_auth == Some(false) {
        return Ok(Resolved {
            auth: AuthConfig { api_token: None, admin_token: None },
            source: Source::Disabled,
            admin: false,
        });
    }

    let path = token_file(mgmt);
    let (token, source) = if let Some(t) = non_empty(mgmt.api_token.clone()) {
        (Some(t), Source::Config)
    } else if let Some(t) = non_empty(std::env::var("STORMBLOCK_API_TOKEN").ok()) {
        (Some(t), Source::Env)
    } else if let Some(t) = path.as_deref().and_then(read_token_file) {
        (Some(t), Source::File(path.clone().unwrap()))
    } else if mgmt.require_auth == Some(true) {
        let path = path.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "management.require_auth is set but there is nowhere to keep a token: \
                 set management.token_file, or management.data_dir, or management.api_token"
            )
        })?;
        let token = mint();
        write_token_file(&path, &token)
            .map_err(|e| anyhow::anyhow!("cannot write token file {}: {e}", path.display()))?;
        (Some(token), Source::Minted(path))
    } else {
        // **Unset means required** (#107). The default used to be open, on
        // the reasoning that a machine claims its boot image before it has a
        // credential; that one verb is now open by itself
        // (`serve::api::is_boot_claim`), so nothing is left that needs the
        // rest open. Mint where the node can keep it; where it cannot, close
        // anyway with a token only this process holds, and say so. Failing
        // startup would stop a node booting over its own config, and falling
        // back to open is the thing this default exists to end.
        let token = mint();
        match path.as_deref().map(|p| (p, write_token_file(p, &token))) {
            Some((p, Ok(()))) => (Some(token), Source::Minted(p.to_path_buf())),
            Some((p, Err(e))) => {
                tracing::warn!("cannot write token file {}: {e}", p.display());
                (Some(token), Source::Ephemeral)
            }
            None => (Some(token), Source::Ephemeral),
        }
    };

    Ok(Resolved {
        admin: token.is_some() && admin_token.is_some(),
        auth: AuthConfig {
            api_token: token,
            // An admin token without an ordinary one guards nothing: with
            // `api_token` unset the middleware lets everything through.
            admin_token: admin_token.filter(|_| source.enforced()),
        },
        source,
    })
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// The credential this node presents when it calls **another** node's API:
/// cluster replication, a migration handoff.
///
/// Deliberately only a *shared* token — one named in the config or the
/// environment. A minted token identifies this node to things on this node;
/// presenting it to a peer would authenticate nothing, because the peer minted
/// its own. A cluster therefore shares one `management.api_token`, which is
/// how a cluster is configured anyway, and a standalone node mints.
static FLEET_TOKEN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Record what to present to peers. Called once, at startup.
pub fn set_fleet_token(token: Option<String>) {
    let _ = FLEET_TOKEN.set(token);
}

/// What to present to peers, if anything.
pub fn fleet_token() -> Option<String> {
    FLEET_TOKEN
        .get()
        .cloned()
        .flatten()
        .or_else(|| non_empty(std::env::var("STORMBLOCK_API_TOKEN").ok()))
}

/// The credential for calling an engine at `url`: the shared token when
/// there is one, else — when the engine is on this machine — the token this
/// node minted, read from where it keeps it.
///
/// For the CLI and scripts running on the node itself (`image build
/// --engine http://127.0.0.1:9090` on an appliance). A minted token means
/// nothing to a peer, so it is never presented to anything that is not local.
pub fn token_for(url: &str) -> Option<String> {
    if let Some(t) = fleet_token() {
        return Some(t);
    }
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', '?'])
        .next()
        .unwrap_or("");
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host).trim_matches(['[', ']']);
    let local = matches!(host, "localhost" | "127.0.0.1" | "::1" | "");
    if !local {
        return None;
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = non_empty(std::env::var("STORMBLOCK_TOKEN_FILE").ok()) {
        candidates.push(PathBuf::from(p));
    }
    candidates.push(PathBuf::from("/etc/stormblock/api_token"));
    candidates.push(PathBuf::from("/var/lib/stormblock/api_token"));
    candidates.iter().find_map(|p| read_token_file(p))
}

/// Say, on every boot, whether this node's API is open. See the module note:
/// an insecure default survives because nothing fails while it is wrong.
pub fn log_mode(r: &Resolved, listen_addr: &str, mgmt: &ManagementConfig) {
    match &r.source {
        Source::Config => tracing::info!(
            "Management API on {listen_addr} requires a bearer token (management.api_token){}",
            admin_note(r)
        ),
        Source::Env => tracing::info!(
            "Management API on {listen_addr} requires a bearer token ($STORMBLOCK_API_TOKEN){}",
            admin_note(r)
        ),
        Source::File(p) => tracing::info!(
            "Management API on {listen_addr} requires a bearer token, read from {}{}",
            p.display(),
            admin_note(r)
        ),
        Source::Minted(p) => tracing::info!(
            "Management API on {listen_addr} requires a bearer token, minted this boot into {} (mode 0600){}",
            p.display(),
            admin_note(r)
        ),
        Source::Ephemeral => {
            tracing::warn!(
                "SECURITY: the management API on {listen_addr} is closed with a token minted this \
                 boot and kept in memory only — there was nowhere to write it, so nothing else \
                 on this node can call the API"
            );
            tracing::warn!(
                "SECURITY: set management.data_dir or management.token_file so the token can be \
                 kept and read ({}), or management.api_token to name one",
                token_file(mgmt)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "no token file configured".to_string())
            );
        }
        Source::Disabled | Source::None => {
            let chosen = r.source == Source::Disabled;
            tracing::warn!(
                "SECURITY: the management API on {listen_addr} is UNAUTHENTICATED{}",
                if chosen { " (management.require_auth = false)" } else { "" }
            );
            tracing::warn!(
                "SECURITY: anyone who can reach that address can create, clone, seal and DELETE \
                 volumes, add and withdraw exports, re-point synonyms and publish releases — \
                 which includes choosing what a machine boots at its next power cycle"
            );
            if !chosen {
                tracing::warn!(
                    "SECURITY: remove management.require_auth = false to require a bearer token; \
                     the node will mint one into {} and keep it across restarts",
                    token_file(mgmt)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "management.token_file (unset — set it, or management.data_dir)".to_string())
                );
            }
        }
    }
}

fn admin_note(r: &Resolved) -> &'static str {
    if r.admin {
        "; destructive verbs require the admin token"
    } else {
        ""
    }
}

/// The engine-wide middleware. Reads whatever the node resolved at startup, so
/// a router built in a test with no resolution stays open and the served one
/// never is.
pub async fn require_token(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let auth = state.auth();
    let path = req.uri().path().to_string();
    let presented = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string());

    match crate::serve::api::decide(
        &auth,
        req.method(),
        &path,
        req.uri().query(),
        presented.as_deref(),
    ) {
        Ok(()) => next.run(req).await,
        Err(msg) => {
            tracing::warn!("unauthorized {} {}", req.method(), path);
            unauthorized(&path, msg)
        }
    }
}

/// A 401 in the envelope the surface being called uses. `/v1` is a contract
/// with `{code, message}` and CSI reads `code`; everything else answers
/// `{error, code}`.
fn unauthorized(path: &str, msg: &str) -> Response {
    let v1 = path == "/v1" || path.starts_with("/v1/");
    let body = if v1 {
        json!({ "code": "unauthorized", "message": msg })
    } else {
        json!({ "error": msg, "code": 401 })
    };
    (StatusCode::UNAUTHORIZED, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ManagementConfig {
        ManagementConfig::default()
    }

    #[test]
    fn minted_tokens_are_long_and_unique() {
        let a = mint();
        let b = mint();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Unset means required (#107): a node with somewhere to keep a token
    /// mints one, and keeps it.
    #[test]
    fn closed_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = cfg();
        m.data_dir = Some(dir.path().to_string_lossy().to_string());
        assert_eq!(m.require_auth, None);
        let r = resolve(&m).unwrap();
        assert_eq!(r.source, Source::Minted(dir.path().join("api_token")));
        assert!(r.source.enforced());
        assert!(r.auth.api_token.is_some());
        assert_eq!(resolve(&m).unwrap().auth.api_token, r.auth.api_token, "kept, not re-minted");
    }

    /// With nowhere to keep a token, the default still closes — with one only
    /// this process holds — rather than failing startup or falling open.
    #[test]
    fn closed_by_default_even_with_nowhere_to_keep_a_token() {
        let mut m = cfg();
        m.data_dir = None;
        m.token_file = Some("/proc/stormblock/api_token".into());
        let r = resolve(&m).unwrap();
        assert_eq!(r.source, Source::Ephemeral);
        assert!(r.source.enforced());
        assert!(r.auth.api_token.is_some());
    }

    #[test]
    fn configured_token_is_enforced() {
        let mut m = cfg();
        m.api_token = Some("sekrit".into());
        let r = resolve(&m).unwrap();
        assert_eq!(r.source, Source::Config);
        assert_eq!(r.auth.api_token.as_deref(), Some("sekrit"));
    }

    #[test]
    fn require_auth_mints_and_keeps_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = cfg();
        m.data_dir = Some(dir.path().to_string_lossy().to_string());
        m.require_auth = Some(true);

        let first = resolve(&m).unwrap();
        let path = dir.path().join("api_token");
        assert_eq!(first.source, Source::Minted(path.clone()));
        let token = first.auth.api_token.clone().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "token file must not be readable by anyone else");
        }

        // A restart reads it back rather than minting a second one: a token
        // that changes every boot is one nothing else on the node can hold.
        let second = resolve(&m).unwrap();
        assert_eq!(second.source, Source::File(path));
        assert_eq!(second.auth.api_token, Some(token));
    }

    #[test]
    fn require_auth_with_nowhere_to_write_fails_startup() {
        let mut m = cfg();
        m.data_dir = None;
        m.token_file = Some("/proc/stormblock/api_token".into());
        m.require_auth = Some(true);
        assert!(resolve(&m).is_err(), "must not fall back to open");
    }

    #[test]
    fn explicitly_disabled_drops_even_a_configured_token() {
        let mut m = cfg();
        m.api_token = Some("sekrit".into());
        m.admin_token = Some("root".into());
        m.require_auth = Some(false);
        let r = resolve(&m).unwrap();
        assert_eq!(r.source, Source::Disabled);
        assert!(r.auth.api_token.is_none());
        assert!(r.auth.admin_token.is_none());
    }

    /// An admin token with no ordinary one no longer leaves the node open:
    /// the default mints the ordinary token beside it, and the admin token
    /// then guards the destructive verbs as it would anywhere (#107).
    #[test]
    fn an_admin_token_alone_sits_beside_a_minted_one() {
        let mut m = cfg();
        m.data_dir = None;
        m.token_file = Some("/proc/stormblock/api_token".into());
        m.admin_token = Some("root".into());
        let r = resolve(&m).unwrap();
        assert_eq!(r.source, Source::Ephemeral);
        assert!(r.auth.api_token.is_some());
        assert_eq!(r.auth.admin_token.as_deref(), Some("root"));
        assert!(r.admin);
    }

    /// Only an explicit `require_auth = false` drops an admin token, because
    /// only then is nothing enforced for it to sit beside.
    #[test]
    fn an_admin_token_on_an_open_node_guards_nothing_and_is_dropped() {
        let mut m = cfg();
        m.require_auth = Some(false);
        m.admin_token = Some("root".into());
        let r = resolve(&m).unwrap();
        assert_eq!(r.source, Source::Disabled);
        assert!(r.auth.admin_token.is_none());
        assert!(!r.admin);
    }

    #[test]
    fn admin_token_covers_destructive_verbs_only() {
        let mut m = cfg();
        m.api_token = Some("read".into());
        m.admin_token = Some("root".into());
        let r = resolve(&m).unwrap();
        assert!(r.admin);
        let get = axum::http::Method::GET;
        let del = axum::http::Method::DELETE;
        let d = |method: &axum::http::Method, tok: Option<&str>| {
            crate::serve::api::decide(&r.auth, method, "/api/v1/volumes/x", None, tok)
        };
        assert!(d(&get, Some("read")).is_ok());
        assert!(d(&del, Some("read")).is_err());
        assert!(d(&del, Some("root")).is_ok());
    }
    #[test]
    fn a_local_token_is_only_for_a_local_engine() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("api_token");
        std::fs::write(&f, "localsecret\n").unwrap();
        std::env::set_var("STORMBLOCK_TOKEN_FILE", &f);
        if fleet_token().is_none() {
            assert_eq!(token_for("http://127.0.0.1:9090").as_deref(), Some("localsecret"));
            assert_eq!(token_for("localhost:9090").as_deref(), Some("localsecret"));
            assert_eq!(token_for("http://[::1]:9090/api").as_deref(), Some("localsecret"));
            assert_eq!(token_for("http://forge.g8.lo:9090"), None, "never sent to a peer");
        }
        std::env::remove_var("STORMBLOCK_TOKEN_FILE");
    }
}

