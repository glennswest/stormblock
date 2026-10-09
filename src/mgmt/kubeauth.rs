//! A Kubernetes bearer for a destructive verb (#274, stormcos#250).
//!
//! The node token is mounted into every engine caller, so it cannot be what
//! decides whether a slab may be formatted or a golden deleted. A destructive
//! call needs the admin token (root-only on the node), or the caller's own
//! Kubernetes bearer, which the engine checks against the node's apiserver:
//!
//! 1. **TokenReview**: is the bearer valid, and who is it (user, groups)?
//! 2. **SubjectAccessReview** for that user: may it `<verb>` the
//!    `storage.storm.io` resource the request names (`volumes`, `slabs`,
//!    `arrays`, `forge`, …)? The release's `storage-admin` ClusterRole grants
//!    these; `storage-viewer` does not (stormcos `47-storage-rbac.yaml`).
//!
//! This is what stormconsole sends for a destructive request (stormconsole#82):
//! the signed-in user's own bearer, after its own SelfSubjectAccessReview.
//! The engine's own credential for the two reviews is `[management.kubernetes]
//! token_file` (allowed to create both, as `system:auth-delegator` is).
//!
//! An answer is cached for a minute per (bearer, resource, verb): a console
//! clicking through a list asks once, and a revoked role takes effect within
//! the minute.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::config::KubeAuthConfig;

/// How long an answer is kept.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// Who a bearer is, as TokenReview said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KubeUser {
    pub username: String,
    pub groups: Vec<String>,
}

/// The outcome of reviewing a bearer for a verb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Review {
    Allowed(KubeUser),
    /// Valid bearer, not allowed: who, and the apiserver's reason.
    Denied(KubeUser, String),
    /// Not a valid bearer.
    Unauthenticated(String),
    /// The apiserver could not be asked.
    Unavailable(String),
}

/// See the module documentation.
pub struct KubeAuth {
    api_url: String,
    token_file: Option<String>,
    client: crate::http::Client,
    /// Keyed by the bearer's digest, resource, verb and name: a review is for
    /// one name when the role names resources (#216).
    cache: Mutex<HashMap<(String, String, String, Option<String>), (Instant, Review)>>,
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn digest(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(token.as_bytes());
    d.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

impl KubeAuth {
    /// From `[management.kubernetes]` and the environment; `None` when no
    /// apiserver is named (Kubernetes bearers are then refused).
    pub fn from_config(cfg: Option<&KubeAuthConfig>) -> Option<KubeAuth> {
        let env = |k: &str| non_empty(std::env::var(k).ok());
        let api_url = env("STORMBLOCK_KUBE_API").or_else(|| cfg.map(|c| c.api_url.clone()).and_then(|u| non_empty(Some(u))))?;
        let ca = env("STORMBLOCK_KUBE_CA").or_else(|| cfg.and_then(|c| c.ca_file.clone()));
        let token_file = env("STORMBLOCK_KUBE_TOKEN_FILE").or_else(|| cfg.and_then(|c| c.token_file.clone()));
        let mut b = crate::http::Client::builder().timeout(Duration::from_secs(5));
        if let Some(ca) = ca {
            match std::fs::read(&ca) {
                Ok(pem) => b = b.add_root_certificate_pem(pem),
                Err(e) => {
                    tracing::error!("kubernetes auth: cannot read CA {ca}: {e} — Kubernetes bearers will be refused");
                    return None;
                }
            }
        }
        let client = match b.build() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("kubernetes auth: {e} — Kubernetes bearers will be refused");
                return None;
            }
        };
        Some(KubeAuth { api_url: api_url.trim_end_matches('/').to_string(), token_file, client, cache: Mutex::new(HashMap::new()) })
    }

    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    fn own_token(&self) -> Option<String> {
        let f = self.token_file.as_ref()?;
        non_empty(std::fs::read_to_string(f).ok())
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        let r = self
            .client
            .post(format!("{}{path}", self.api_url))
            .bearer(self.own_token().as_deref())
            .json(&body)
            // A TokenReview or a SubjectAccessReview changes nothing: safe to
            // ask again (#359), briefly, since an API call waits on it.
            .send_retried(crate::retry::Policy::QUICK)
            .await
            .map_err(|e| format!("{}{path}: {e}", self.api_url))?;
        let status = r.status();
        let v: Value = r.json().await.map_err(|e| format!("{path}: {e}"))?;
        if !status.is_success() {
            return Err(format!("{path} answered {status}: {}", v["message"].as_str().unwrap_or("")));
        }
        Ok(v)
    }

    /// May `bearer` `verb` the `storage.storm.io` resource `resource` (named
    /// `name`, when there is one)?
    pub async fn review(&self, bearer: &str, resource: &str, verb: &str, name: Option<&str>) -> Review {
        let key = (digest(bearer), resource.to_string(), verb.to_string(), name.map(str::to_string));
        if let Some((at, r)) = self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            if at.elapsed() < CACHE_TTL && !matches!(r, Review::Unavailable(_)) {
                return r.clone();
            }
        }
        let r = self.review_uncached(bearer, resource, verb, name).await;
        if !matches!(r, Review::Unavailable(_)) {
            let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            c.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
            c.insert(key, (Instant::now(), r.clone()));
        }
        r
    }

    async fn review_uncached(&self, bearer: &str, resource: &str, verb: &str, name: Option<&str>) -> Review {
        let tr = match self
            .post(
                "/apis/authentication.k8s.io/v1/tokenreviews",
                json!({ "apiVersion": "authentication.k8s.io/v1", "kind": "TokenReview", "spec": { "token": bearer } }),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => return Review::Unavailable(e),
        };
        if tr["status"]["authenticated"] != json!(true) {
            return Review::Unauthenticated(tr["status"]["error"].as_str().unwrap_or("not authenticated").to_string());
        }
        let user = KubeUser {
            username: tr["status"]["user"]["username"].as_str().unwrap_or("").to_string(),
            groups: tr["status"]["user"]["groups"]
                .as_array()
                .map(|g| g.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
        };
        let mut attrs = json!({ "group": "storage.storm.io", "resource": resource, "verb": verb });
        if let Some(n) = name {
            attrs["name"] = json!(n);
        }
        let sar = match self
            .post(
                "/apis/authorization.k8s.io/v1/subjectaccessreviews",
                json!({
                    "apiVersion": "authorization.k8s.io/v1",
                    "kind": "SubjectAccessReview",
                    "spec": {
                        "user": user.username,
                        "groups": user.groups,
                        "uid": tr["status"]["user"]["uid"],
                        "resourceAttributes": attrs,
                    },
                }),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => return Review::Unavailable(e),
        };
        if sar["status"]["allowed"] == json!(true) {
            Review::Allowed(user)
        } else {
            let why = sar["status"]["reason"].as_str().unwrap_or("not allowed").to_string();
            Review::Denied(user, why)
        }
    }
}

/// One destructive call, as the audit log keeps it (#274).
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditRecord {
    /// Unix seconds.
    pub at: u64,
    /// `admin-token`, `node-token` (audit mode), `kubernetes:<user>`, or
    /// `none` / `unknown-bearer` for a refusal.
    pub who: String,
    pub method: String,
    pub path: String,
    /// The `storage.storm.io` resource and verb, and the name when the path
    /// has one: which slab, volume, array.
    pub resource: String,
    pub verb: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// `allowed`, `allowed-audit-only` (would be refused under enforce), or
    /// `refused`.
    pub decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The response's status, for an allowed call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

/// Append one record to the audit log, and say it in the log as well.
pub fn audit(path: Option<&std::path::Path>, rec: &AuditRecord) {
    tracing::info!(
        target: "stormblock::audit",
        who = %rec.who, method = %rec.method, path = %rec.path, decision = %rec.decision,
        status = rec.status.unwrap_or(0), "destructive call"
    );
    let Some(p) = path else { return };
    let line = match serde_json::to_string(rec) {
        Ok(l) => l,
        Err(_) => return,
    };
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o640);
    }
    match opts.open(p) {
        Ok(mut f) => {
            let _ = writeln!(f, "{line}");
        }
        Err(e) => tracing::warn!("audit log {}: {e}", p.display()),
    }
}
