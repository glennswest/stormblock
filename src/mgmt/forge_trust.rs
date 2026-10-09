//! Forge's trust, handed to every node that boots from it (#381, stormcos#486).
//!
//! A node enrols with forge (stormcert's `forge-enroll`, stormcert#79) by
//! filing a `storm.io/forge-node` CSR on forge's apiserver, and it must
//! send nothing to an apiserver it cannot verify. So the forge a node boots
//! from leaves three things in `/run/stormblock/forge/`:
//!
//! - `ca.crt`: forge's CA (PEM; both CAs during a rotation), 0644;
//! - `url`: forge's apiserver, `https://<forge>:6443`, 0644;
//! - `bootstrap.token`: a narrow credential that may only create
//!   `storm.io/forge-node` CSRs, 0600;
//! - `from`: `claim <forge>` or `self`, so a reader knows which.
//!
//! Where it comes from: forge's own stormcert makes the CA and mints the
//! token (stormcert#78), and gives them to forge's engine with
//! `PUT /api/v1/forge/trust` (admin: the admin token, or a Kubernetes bearer
//! a SubjectAccessReview allows `update` on `storm.io` `forge`). The engine
//! keeps them in `<data_dir>/forge-trust.json` (0600, carried by the state
//! volume). Every boot claim's answer carries them (`forge_trust`), and
//! `boot-claim` in the initramfs writes the directory, which `/run` carries
//! into the booted system. **The first node** — forge mode on, booted from
//! its own disk, no claim — writes its own, with `https://127.0.0.1:6443`,
//! when its trust is set and at every start. No forge and no claim: the
//! directory is absent, and the client says so and waits.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::mgmt::AppState;

/// Kept in the engine's data directory.
pub const TRUST_FILE: &str = "forge-trust.json";
/// Where a node finds it. `STORMBLOCK_FORGE_TRUST_DIR` moves it (tests).
pub const NODE_DIR: &str = "/run/stormblock/forge";
/// Forge's apiserver port.
pub const APISERVER_PORT: u16 = 6443;

pub fn node_dir() -> PathBuf {
    std::env::var_os("STORMBLOCK_FORGE_TRUST_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(NODE_DIR))
}

/// What forge's stormcert gives its engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForgeTrust {
    /// Forge's CA, PEM (both during a rotation).
    pub ca: String,
    /// Forge's apiserver as other nodes reach it. Unset: `https://<this
    /// node's advertised address>:6443`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// May only create `storm.io/forge-node` CSRs.
    pub bootstrap_token: String,
    #[serde(default)]
    pub updated_at: u64,
}

impl ForgeTrust {
    pub fn validate(&self) -> Result<(), String> {
        if !self.ca.contains("-----BEGIN CERTIFICATE-----") || !self.ca.contains("-----END CERTIFICATE-----") {
            return Err("ca must be PEM certificate(s)".into());
        }
        let t = self.bootstrap_token.trim();
        if t.is_empty() || t.len() > 512 || t.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("bootstrap_token must be one non-empty token, no whitespace".into());
        }
        if let Some(u) = &self.url {
            if !u.starts_with("https://") || u.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err("url must be https://…".into());
            }
        }
        Ok(())
    }
}

fn kept_path(state: &AppState) -> Option<PathBuf> {
    state.config.management.data_dir.as_ref().map(|d| Path::new(d).join(TRUST_FILE))
}

/// The trust this node keeps as a forge, if any.
pub fn load(state: &AppState) -> Option<ForgeTrust> {
    let p = kept_path(state)?;
    serde_json::from_slice(&std::fs::read(&p).ok()?).ok()
}

/// Keep it (0600).
pub fn keep(state: &AppState, t: &ForgeTrust) -> std::io::Result<()> {
    let p = kept_path(state).ok_or_else(|| std::io::Error::other("this node keeps no data directory"))?;
    write_file(&p, &serde_json::to_vec_pretty(t).map_err(|e| std::io::Error::other(e.to_string()))?, 0o600)
}

pub fn forget(state: &AppState) -> std::io::Result<()> {
    match kept_path(state) {
        Some(p) => match std::fs::remove_file(p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
        None => Ok(()),
    }
}

/// The apiserver URL other nodes are told.
pub fn remote_url(state: &AppState, t: &ForgeTrust) -> String {
    t.url.clone().unwrap_or_else(|| {
        let host = state.config.management.resolve_advertised_host("0.0.0.0");
        format!("https://{host}:{APISERVER_PORT}")
    })
}

/// What a boot claim answers (`forge_trust`), or null when this node holds
/// no trust to hand out.
pub fn claim_answer(state: &AppState) -> Value {
    match load(state) {
        Some(t) => json!({ "ca": t.ca, "url": remote_url(state, &t), "bootstrap_token": t.bootstrap_token }),
        None => Value::Null,
    }
}

/// What `GET /api/v1/forge/trust` says: never the token.
pub fn status(state: &AppState) -> Value {
    match load(state) {
        Some(t) => json!({
            "set": true,
            "ca": t.ca,
            "url": remote_url(state, &t),
            "bootstrap_token_set": !t.bootstrap_token.is_empty(),
            "updated_at": t.updated_at,
            "node_dir_from": read_from(&node_dir()),
        }),
        None => json!({ "set": false, "node_dir_from": read_from(&node_dir()) }),
    }
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
    f.write_all(bytes)?;
    drop(f);
    std::fs::rename(&tmp, path)
}

/// Write the node's directory: `ca.crt`, `url`, `bootstrap.token` (0600),
/// and `from` last, so a reader that finds `from` finds the rest.
pub fn write_dir(dir: &Path, ca: &str, url: &str, token: &str, from: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let ca = if ca.ends_with('\n') { ca.to_string() } else { format!("{ca}\n") };
    write_file(&dir.join("ca.crt"), ca.as_bytes(), 0o644)?;
    write_file(&dir.join("url"), format!("{url}\n").as_bytes(), 0o644)?;
    write_file(&dir.join("bootstrap.token"), format!("{}\n", token.trim()).as_bytes(), 0o600)?;
    write_file(&dir.join("from"), format!("{from}\n").as_bytes(), 0o644)
}

/// Who wrote the node's directory: `claim <forge>`, `self`, or none.
pub fn read_from(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join("from")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Remove the node's directory (a claim that carried no trust).
pub fn remove_dir(dir: &Path) {
    for f in ["from", "ca.crt", "url", "bootstrap.token"] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    let _ = std::fs::remove_dir(dir);
}

/// From a claim's answer (`boot-claim`): the directory written from
/// `forge_trust`, or removed when the answer carries none. Returns what it
/// did, for the console.
pub fn from_claim(dir: &Path, forge: &str, reply: &Value) -> Result<&'static str, String> {
    let t = reply.get("forge_trust").filter(|v| v.is_object());
    let Some(t) = t else {
        if read_from(dir).is_some_and(|f| f.starts_with("claim")) {
            remove_dir(dir);
        }
        return Ok("none");
    };
    let get = |k: &str| t.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let trust = ForgeTrust {
        ca: get("ca").unwrap_or_default(),
        url: get("url"),
        bootstrap_token: get("bootstrap_token").unwrap_or_default(),
        updated_at: 0,
    };
    trust.validate()?;
    let url = trust.url.clone().ok_or("forge_trust without a url")?;
    write_dir(dir, &trust.ca, &url, &trust.bootstrap_token, &format!("claim {forge}")).map_err(|e| e.to_string())?;
    Ok("written")
}

/// The first node writes its own (#381): forge mode on, trust kept, and the
/// node's directory not already written by a claim this boot. Its own
/// apiserver, on loopback. Returns whether it wrote.
pub async fn write_self(state: &AppState) -> bool {
    if state.nvmeof_target.read().await.is_none() {
        return false;
    }
    let Some(t) = load(state) else { return false };
    let dir = node_dir();
    if read_from(&dir).is_some_and(|f| f != "self") {
        return false;
    }
    let url = format!("https://127.0.0.1:{APISERVER_PORT}");
    match write_dir(&dir, &t.ca, &url, &t.bootstrap_token, "self") {
        Ok(()) => {
            tracing::info!("forge trust: this node's own written to {}", dir.display());
            true
        }
        Err(e) => {
            tracing::warn!("forge trust: {}: {e}", dir.display());
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const CA: &str = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_claims_trust_is_written_with_its_modes_and_none_removes_it() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("forge");
        let reply = json!({"forge_trust": {"ca": CA, "url": "https://10.0.0.5:6443", "bootstrap_token": "abc.def"}});
        assert_eq!(from_claim(&dir, "http://10.0.0.5:9090", &reply), Ok("written"));
        assert_eq!(std::fs::read_to_string(dir.join("ca.crt")).unwrap(), CA);
        assert_eq!(std::fs::read_to_string(dir.join("url")).unwrap(), "https://10.0.0.5:6443\n");
        assert_eq!(std::fs::read_to_string(dir.join("bootstrap.token")).unwrap(), "abc.def\n");
        assert_eq!((mode(&dir.join("ca.crt")), mode(&dir.join("url")), mode(&dir.join("bootstrap.token"))), (0o644, 0o644, 0o600));
        assert_eq!(read_from(&dir).as_deref(), Some("claim http://10.0.0.5:9090"));
        // Nothing left behind but the four.
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into()).collect();
        names.sort();
        assert_eq!(names, ["bootstrap.token", "ca.crt", "from", "url"]);
        // A forge that hands out none: gone.
        assert_eq!(from_claim(&dir, "http://10.0.0.5:9090", &json!({"forge_trust": null})), Ok("none"));
        assert!(!dir.exists());
        // A malformed one: refused, nothing written.
        let bad = json!({"forge_trust": {"ca": "not pem", "url": "https://x:6443", "bootstrap_token": "t"}});
        assert!(from_claim(&dir, "f", &bad).is_err());
        assert!(!dir.exists());
    }

    #[test]
    fn trust_is_validated() {
        let ok = ForgeTrust { ca: CA.into(), url: None, bootstrap_token: "abc.def".into(), updated_at: 0 };
        assert!(ok.validate().is_ok());
        assert!(ForgeTrust { bootstrap_token: "a b".into(), ..ok.clone() }.validate().is_err());
        assert!(ForgeTrust { bootstrap_token: "".into(), ..ok.clone() }.validate().is_err());
        assert!(ForgeTrust { url: Some("http://x:6443".into()), ..ok.clone() }.validate().is_err());
        assert!(ForgeTrust { ca: "x".into(), ..ok }.validate().is_err());
    }
}
