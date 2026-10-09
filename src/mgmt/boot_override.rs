//! A machine's boot override, read from stormipmi when forge answers its
//! claim (#354, stormipmi#70).
//!
//! The override is stormipmi's (owner, 2026-10-08: it is part of BMC/IPMI
//! management, "Forge would use that"); forge reads it and acts:
//!
//! - `install` (with `release`): the next boot installs that release whatever
//!   the disk holds — `boothost/<tag>` is pointed at the release, the intent
//!   is `install`, and the ordinary install runs (system half laid again,
//!   data half kept, #311). Reported `ok` when the installer's successor
//!   reports the disk laid (#220); until then every claim installs again.
//! - `local`: boot the disk, no installer. `GET …/intent` answers `local`,
//!   the claim answers `local`, and the initramfs boots the disk.
//! - `recovery`: boot the claimed clone without touching any local drive.
//!   The claim answers `recovery`; the initramfs leaves every drive alone.
//! - `hold`: the claim is refused (423) while the override stands. No result
//!   is reported: that would clear a one-shot hold and let the next retry
//!   boot. It ends when stormipmi's override is removed.
//!
//! `local` and `recovery` are reported `ok` by the claim the initramfs makes
//! (agent `stormblock-initramfs`), the last of a boot's claims: the
//! firmware's claim comes first and must see the override too.
//!
//! The contract (stormipmi#70): `GET <base>/api/v1/machines/<tag>/override`
//! (no token) → `{actor, override}`; forge acts when `actor == "forge"`.
//! `POST <base>/api/v1/machines/<tag>/override/result {id, result, message,
//! by: "forge"}` with stormipmi's admin token or the machine's secret; a
//! repeat returns the record already written.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::mgmt::AppState;

/// The agent name `boot-claim` sends: the last claim of a boot.
pub const INITRAMFS_AGENT: &str = "stormblock-initramfs";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Override {
    pub id: String,
    /// `install`, `local`, `hold` or `recovery`.
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    #[serde(default)]
    pub persistent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn base(state: &AppState) -> Option<String> {
    state
        .config
        .management
        .boot_override_url
        .clone()
        .or_else(|| std::env::var("STORMBLOCK_BOOT_OVERRIDE_URL").ok())
        .map(|b| b.trim().trim_end_matches('/').to_string())
        .filter(|b| !b.is_empty())
}

fn token(state: &AppState) -> Option<String> {
    let path = state
        .config
        .management
        .boot_override_token_file
        .clone()
        .or_else(|| std::env::var("STORMBLOCK_BOOT_OVERRIDE_TOKEN_FILE").ok())?;
    std::fs::read_to_string(path).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// Whether this forge reads overrides at all.
pub fn configured(state: &AppState) -> bool {
    base(state).is_some()
}

/// The override forge is to act on for `tag`, if any. An error (stormipmi
/// unreachable, an answer that does not read) is said by the caller and the
/// claim goes on as it would have: an override nobody can read is no reason
/// to keep a machine from booting.
pub async fn fetch(state: &AppState, tag: &str) -> Result<Option<Override>, String> {
    let Some(base) = base(state) else { return Ok(None) };
    let url = format!("{base}/api/v1/machines/{tag}/override");
    let client = crate::http::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(&url).send().await.map_err(|e| format!("{url}: {e}"))?;
    let status = resp.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!("{url}: {status}"));
    }
    let body = resp.text().await.map_err(|e| format!("{url}: {e}"))?;
    parse(&body).map_err(|e| format!("{url}: {e}"))
}

/// What stormipmi's answer says forge is to do (`None`: nothing).
pub fn parse(body: &str) -> Result<Option<Override>, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    if v.get("actor").and_then(|a| a.as_str()) != Some("forge") {
        return Ok(None);
    }
    let o = match v.get("override") {
        None | Some(Value::Null) => return Ok(None),
        Some(o) => o,
    };
    let o: Override = serde_json::from_value(o.clone()).map_err(|e| e.to_string())?;
    match o.action.as_str() {
        "install" if o.release.as_deref().is_none_or(|r| r.trim().is_empty()) => {
            Err(format!("override {}: install names no release", o.id))
        }
        "install" | "local" | "hold" | "recovery" => Ok(Some(o)),
        other => Err(format!("override {}: unknown action {other:?}", o.id)),
    }
}

/// Report what came of an override, in the background, retried: a one-shot
/// override is cleared by it. Never fails the caller.
pub fn report(state: &AppState, tag: &str, id: &str, ok: bool, message: String) {
    let Some(base) = base(state) else { return };
    let token = token(state);
    let url = format!("{base}/api/v1/machines/{tag}/override/result");
    let body = json!({ "id": id, "result": if ok { "ok" } else { "failed" }, "message": message, "by": "forge" });
    tokio::spawn(async move {
        let Ok(client) = crate::http::Client::builder().timeout(std::time::Duration::from_secs(10)).build() else {
            return;
        };
        for attempt in 1..=6u32 {
            match client.post(&url).bearer(token.as_deref()).json(&body).send().await {
                Ok(r) if r.status().is_success() => {
                    tracing::info!("boot override: result reported to {url}");
                    return;
                }
                Ok(r) if (400..500).contains(&r.status().as_u16()) => {
                    tracing::warn!("boot override: {url} refused the result: {}", r.status());
                    return;
                }
                Ok(r) => tracing::warn!("boot override: {url}: {} (attempt {attempt})", r.status()),
                Err(e) => tracing::warn!("boot override: {url}: {e} (attempt {attempt})"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
        tracing::error!("boot override: result not reported to {url}; the override stands");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_forges_overrides_are_acted_on() {
        let b = |actor: &str, o: Value| json!({"machine": "m", "host": "server1", "actor": actor, "override": o}).to_string();
        let ins = json!({"id": "o1", "action": "install", "release": "11.98", "persistent": false, "by": "op", "reason": "redo"});
        assert_eq!(parse(&b("forge", ins.clone())).unwrap().unwrap().release.as_deref(), Some("11.98"));
        assert_eq!(parse(&b("stormipmi", json!({"id": "o2", "action": "boot-once"}))).unwrap(), None);
        assert_eq!(parse(&b("forge", Value::Null)).unwrap(), None);
        for a in ["local", "hold", "recovery"] {
            assert_eq!(parse(&b("forge", json!({"id": "x", "action": a}))).unwrap().unwrap().action, a);
        }
        assert!(parse(&b("forge", json!({"id": "x", "action": "install"}))).is_err(), "install names a release");
        assert!(parse(&b("forge", json!({"id": "x", "action": "explode"}))).is_err());
        assert!(parse("not json").is_err());
    }
}
