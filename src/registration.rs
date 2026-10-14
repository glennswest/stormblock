//! Registration with stormstorage: announce this node's volumes (#170).
//!
//! A node posts its exported volumes to `<[registration] url>/api/v1/storage/
//! register` on a heartbeat, so stormstorage can build on them and sees a node
//! leave (`/deregister` on shutdown). The route is stormstorage's (stormstorage
//! #6, #77). It was called StormFS registration (`[stormfs] metadata_url`),
//! after the consumer it was first meant for; StormFS's metadata server does
//! not serve it. `[stormfs]` is still read, as the old spelling, with a
//! warning.

use std::sync::Arc;
use std::time::Duration;

use serde::{Serialize, Deserialize};

use crate::mgmt::AppState;

/// `[registration]`: where this node registers its volumes (stormstorage).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RegistrationConfig {
    /// Register this node's volumes.
    pub enabled: bool,
    /// stormstorage's URL (e.g. `http://stormstorage:8500`). `metadata_url`
    /// is the old spelling, still read.
    #[serde(alias = "metadata_url")]
    pub url: String,
    /// Registration heartbeat interval in seconds.
    pub heartbeat_secs: u64,
    /// This node's address, for stormstorage to reach back.
    pub advertise_addr: String,
    /// Sent as `Authorization: Bearer` on register and deregister (#214),
    /// so stormstorage can require a token on them (stormstorage#6). Absent:
    /// sent with none, as before. Never shown or serialized.
    #[serde(skip_serializing)]
    pub api_token: Option<crate::mgmt::config::Secret>,
    /// The token from a file instead (first line, trimmed); `api_token` wins.
    pub token_file: Option<String>,
}

impl RegistrationConfig {
    /// The token to present, from `api_token` or `token_file`. A file that
    /// cannot be read is an error, not "no token": a node configured to
    /// authenticate must not quietly register without.
    pub fn token(&self) -> anyhow::Result<Option<String>> {
        if let Some(t) = self.api_token.as_ref().filter(|t| !t.0.trim().is_empty()) {
            return Ok(Some(t.0.trim().to_string()));
        }
        match &self.token_file {
            None => Ok(None),
            Some(f) => {
                let s = std::fs::read_to_string(f).map_err(|e| anyhow::anyhow!("[registration] token_file {f}: {e}"))?;
                let t = s.lines().next().unwrap_or("").trim().to_string();
                if t.is_empty() {
                    anyhow::bail!("[registration] token_file {f} is empty");
                }
                Ok(Some(t))
            }
        }
    }
}

impl Default for RegistrationConfig {
    fn default() -> Self {
        RegistrationConfig {
            enabled: false,
            url: String::new(),
            heartbeat_secs: 30,
            advertise_addr: String::new(),
            api_token: None,
            token_file: None,
        }
    }
}

/// Volume announcement sent to stormstorage.
#[derive(Debug, Serialize)]
struct VolumeAnnouncement {
    node_addr: String,
    hostname: String,
    volumes: Vec<VolumeInfo>,
}

/// Per-volume info in a registration announcement.
#[derive(Debug, Serialize)]
struct VolumeInfo {
    id: String,
    name: String,
    capacity_bytes: u64,
    allocated_bytes: u64,
    protocols: Vec<String>,
}

/// Registration response from stormstorage.
#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    #[serde(default)]
    accepted: bool,
    #[serde(default)]
    message: String,
}

/// The registration client.
pub struct Registration {
    config: RegistrationConfig,
    client: crate::http::Client,
}

impl Registration {
    /// Create a registration client.
    pub fn new(config: RegistrationConfig) -> Self {
        Self::try_new(config).unwrap_or_else(|e| panic!("stormstorage registration: {e}"))
    }

    /// [`new`](Self::new), with the token's file read here (#214): every
    /// register and deregister presents it.
    pub fn try_new(config: RegistrationConfig) -> anyhow::Result<Self> {
        let token = config.token()?;
        let client = crate::http::Client::builder()
            .timeout(Duration::from_secs(10))
            .bearer(token)
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build HTTP client: {e}"))?;
        Ok(Registration { config, client })
    }

    /// Start the periodic registration loop.
    pub fn start(self, state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
        let interval = Duration::from_secs(self.config.heartbeat_secs);
        tokio::spawn(async move {
            loop {
                if let Err(e) = self.register(&state).await {
                    tracing::warn!("stormstorage registration failed: {e}");
                }
                tokio::time::sleep(interval).await;
            }
        })
    }

    /// Send one registration announcement to stormstorage.
    async fn register(&self, state: &Arc<AppState>) -> anyhow::Result<()> {
        let hostname = gethostname()
            .unwrap_or_else(|| "unknown".to_string());

        // Collect volume info
        let vm = state.volume_manager.lock().await;
        let vol_list = vm.list_volumes().await;
        let volumes: Vec<VolumeInfo> = vol_list.iter().map(|(id, name, capacity, allocated)| {
            #[allow(clippy::vec_init_then_push)]
            let protocols = {
                let mut p = Vec::new();
                #[cfg(feature = "iscsi")]
                p.push("iscsi".to_string());
                p.push("nvmeof".to_string());
                p
            };
            VolumeInfo {
                id: id.to_string(),
                name: name.clone(),
                capacity_bytes: *capacity,
                allocated_bytes: *allocated,
                protocols,
            }
        }).collect();
        drop(vm);

        let announcement = VolumeAnnouncement {
            node_addr: self.config.advertise_addr.clone(),
            hostname,
            volumes,
        };

        let url = format!("{}/api/v1/storage/register", self.config.url.trim_end_matches('/'));
        // An announcement of what this node holds: the same one twice is
        // the same registration (#359).
        let resp = self.client
            .post(&url)
            .json(&announcement)
            .send_retried(crate::retry::Policy::NETWORK)
            .await
            .map_err(|e| anyhow::anyhow!("stormstorage unreachable: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("stormstorage registration rejected ({status}): {body}");
        }

        let reg_resp: RegistrationResponse = resp.json().await
            .unwrap_or(RegistrationResponse { accepted: true, message: String::new() });

        if reg_resp.accepted {
            tracing::debug!("stormstorage registration accepted");
        } else {
            tracing::warn!("stormstorage registration not accepted: {}", reg_resp.message);
        }

        Ok(())
    }

    /// Send a deregistration to stormstorage on shutdown.
    pub async fn deregister(&self) -> anyhow::Result<()> {
        let url = format!(
            "{}/api/v1/storage/deregister",
            self.config.url.trim_end_matches('/')
        );
        let _ = self.client
            .post(&url)
            .json(&serde_json::json!({
                "node_addr": self.config.advertise_addr,
            }))
            // At shutdown, briefly: deregistering twice is deregistering.
            .send_retried(crate::retry::Policy::QUICK)
            .await;
        tracing::info!("stormstorage deregistration sent");
        Ok(())
    }
}

/// Get the system hostname.
fn gethostname() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buf = vec![0u8; 256];
        let ret = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if ret == 0 {
            let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            return String::from_utf8(buf[..nul].to_vec()).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #214: with a token set, register and deregister carry it; without,
    /// neither carries one; the file form works and an unreadable file is an
    /// error, never a silent "no token".
    #[tokio::test]
    async fn register_and_deregister_present_the_token_when_one_is_set() {
        use axum::{routing::post, Router};
        use std::sync::Mutex;
        let seen: Arc<Mutex<Vec<(String, Option<String>)>>> = Arc::new(Mutex::new(Vec::new()));
        let rec = seen.clone();
        let app = Router::new().route(
            "/api/v1/storage/{what}",
            post(move |axum::extract::Path(what): axum::extract::Path<String>, headers: axum::http::HeaderMap| {
                let rec = rec.clone();
                async move {
                    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string);
                    rec.lock().unwrap().push((what, auth));
                    axum::Json(serde_json::json!({"accepted": true}))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

        let vm = crate::volume::VolumeManager::new(crate::volume::DEFAULT_EXTENT_SIZE);
        let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
        let state = Arc::new(AppState::new(crate::mgmt::config::StormBlockConfig::default(), vm, reg, gem));
        let cfg = |token: Option<&str>, file: Option<String>| RegistrationConfig {
            enabled: true,
            url: url.clone(),
            advertise_addr: "10.0.0.5:9090".into(),
            api_token: token.map(|t| crate::mgmt::config::Secret(t.into())),
            token_file: file,
            ..RegistrationConfig::default()
        };

        let with = Registration::new(cfg(Some("s3cret"), None));
        with.register(&state).await.unwrap();
        with.deregister().await.unwrap();
        let without = Registration::new(cfg(None, None));
        without.register(&state).await.unwrap();
        without.deregister().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("token");
        std::fs::write(&f, "from-file\n").unwrap();
        let filed = Registration::new(cfg(None, Some(f.to_string_lossy().into())));
        filed.register(&state).await.unwrap();

        let got = seen.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                ("register".into(), Some("Bearer s3cret".into())),
                ("deregister".into(), Some("Bearer s3cret".into())),
                ("register".into(), None),
                ("deregister".into(), None),
                ("register".into(), Some("Bearer from-file".into())),
            ]
        );
        let missing = cfg(None, Some(dir.path().join("nope").to_string_lossy().into()));
        assert!(Registration::try_new(missing).is_err(), "an unreadable token file is an error");
        assert!(!format!("{:?}", cfg(Some("s3cret"), None)).contains("s3cret"), "never shown");
    }

    #[test]
    fn default_config_disabled() {
        let cfg = RegistrationConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.url.is_empty());
        assert_eq!(cfg.heartbeat_secs, 30);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = RegistrationConfig {
            enabled: true,
            url: "http://stormfs:8500".to_string(),
            heartbeat_secs: 60,
            advertise_addr: "10.0.0.1:9090".to_string(),
            ..RegistrationConfig::default()
        };
        let toml_str = toml::to_string(&cfg).unwrap();
        let parsed: RegistrationConfig = toml::from_str(&toml_str).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.url, "http://stormfs:8500");
        assert_eq!(parsed.heartbeat_secs, 60);
    }
}
