//! A machine's install-config.yaml, kept by forge and handed to it at its
//! boot claim (#459, stormcos#604).
//!
//! The boot media's install-config (#275) reaches a node only from the ESP
//! storminstall wrote, through stormbootx's EFI variables. Test machines
//! boot one shared stormbootx ISO and take their release from forge's
//! `boothost/<name>`, so nothing per machine reached them, and stormcentral
//! mints a stormupdate token per machine at each install (stormcentral#710).
//! So forge keeps one per host:
//!
//! ```text
//! PUT    /api/v1/synonyms/boothost/<name>/install-config   the YAML (admin)
//! GET    /api/v1/synonyms/boothost/<name>/install-config   the YAML (admin)
//! DELETE /api/v1/synonyms/boothost/<name>/install-config   (admin)
//! ```
//!
//! It holds secrets (a pull secret, API tokens, the update token), so it is
//! kept apart from `synonyms.json`, which every boothost GET serializes: one
//! file per host in `<data_dir>/install-configs/`, 0600, never listed and
//! never logged. Every verb is the admin's, the read included. The claim of
//! that host (the same trust as its intent and image) carries it as
//! `install_config`; nothing else does.

use std::path::{Path, PathBuf};

use crate::mgmt::AppState;

const DIR: &str = "install-configs";

/// The largest install-config kept: what storminstall writes is a few KiB.
pub const MAX_BYTES: usize = 64 * 1024;

fn dir(state: &AppState) -> Option<PathBuf> {
    state.data_dir.as_deref().map(|d| d.join(DIR))
}

/// A host's file. Host names are DNS labels, aliases and MAC spellings;
/// anything that is not one is never a path.
fn file(dir: &Path, host: &str) -> Option<PathBuf> {
    let ok = !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('.')
        && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
    ok.then(|| dir.join(format!("{host}.yaml")))
}

/// What a PUT is refused for.
pub fn check(bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() {
        return Err("an install-config is the YAML storminstall writes; this one is empty".into());
    }
    if bytes.len() > MAX_BYTES {
        return Err(format!("an install-config of {} bytes; at most {MAX_BYTES}", bytes.len()));
    }
    std::str::from_utf8(bytes).map_err(|_| "an install-config is YAML text, and this is not UTF-8".to_string())?;
    Ok(())
}

/// Keep `host`'s install-config, replacing the one it had.
pub fn put(state: &AppState, host: &str, bytes: &[u8]) -> Result<(), String> {
    check(bytes)?;
    let d = dir(state).ok_or("this engine keeps no data directory, so it keeps no install-config")?;
    let f = file(&d, host).ok_or_else(|| format!("{host:?} is not a host name"))?;
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
    crate::mgmt::forge_trust::write_file(&f, bytes, 0o600).map_err(|e| format!("{}: {e}", f.display()))
}

/// `host`'s install-config, if forge keeps one.
pub fn get(state: &AppState, host: &str) -> Option<Vec<u8>> {
    std::fs::read(file(&dir(state)?, host)?).ok()
}

/// Forget it. Whether there was one.
pub fn delete(state: &AppState, host: &str) -> bool {
    match dir(state).and_then(|d| file(&d, host)) {
        Some(f) => std::fs::remove_file(f).is_ok(),
        None => false,
    }
}

/// A host renamed: its install-config goes with it.
pub fn rename(state: &AppState, from: &str, to: &str) {
    let Some(d) = dir(state) else { return };
    let (Some(a), Some(b)) = (file(&d, from), file(&d, to)) else { return };
    if a.exists() {
        if let Err(e) = std::fs::rename(&a, &b) {
            tracing::warn!("install-config of {from} not moved to {to}: {e}");
        }
    }
}

/// What a claim of `host` carries: the YAML and its sha256, or nothing.
pub fn for_claim(state: &AppState, host: &str) -> Option<(String, String)> {
    let bytes = get(state, host)?;
    let text = String::from_utf8(bytes).ok()?;
    use sha2::Digest;
    Some((hex::encode(sha2::Sha256::digest(text.as_bytes())), text))
}

/// Where `boot-claim` leaves forge's copy for `/init` (0600, in RAM; `/init`
/// removes it once /state has it or not).
pub const STAGED: &str = "/run/stormblock/install-config.forge.yaml";

/// From a claim's answer (`boot-claim`): forge's install-config staged, or a
/// stale one removed. Returns what it did, for the console; never the bytes.
pub fn from_claim(path: &Path, reply: &serde_json::Value) -> Result<String, String> {
    match reply.get("install_config").and_then(|v| v.as_str()) {
        Some(text) => {
            check(text.as_bytes())?;
            crate::mgmt::forge_trust::write_file(path, text.as_bytes(), 0o600).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(format!("{} bytes staged", text.len()))
        }
        None => {
            let _ = std::fs::remove_file(path);
            Ok("none".into())
        }
    }
}

/// A claim's answer with the install-config's bytes taken out, for anything
/// that prints it.
pub fn redacted(body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(mut v) => {
            if let Some(o) = v.as_object_mut() {
                if o.contains_key("install_config") {
                    o.insert("install_config".into(), serde_json::Value::String("(redacted)".into()));
                }
            }
            v.to_string()
        }
        Err(_) if body.contains("install_config") => "(a reply carrying an install-config, not printed)".into(),
        Err(_) => body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_name_is_a_file_and_nothing_else_is() {
        let d = Path::new("/x");
        assert_eq!(file(d, "pvetest1").unwrap(), Path::new("/x/pvetest1.yaml"));
        assert!(file(d, "../etc").is_none());
        assert!(file(d, "a/b").is_none());
        assert!(file(d, "").is_none());
        assert!(file(d, ".hidden").is_none());
    }

    #[test]
    fn a_claim_stages_it_privately_and_a_claim_without_removes_it() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::TempDir::new().unwrap();
        let p = t.path().join("install-config.forge.yaml");
        let reply = serde_json::json!({"install_config": "updateToken: SECRET\n"});
        assert_eq!(from_claim(&p, &reply).unwrap(), "20 bytes staged");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "updateToken: SECRET\n");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(from_claim(&p, &serde_json::json!({})).unwrap(), "none");
        assert!(!p.exists(), "a stale one is not left for /init");
        assert!(!redacted(&reply.to_string()).contains("SECRET"));
        assert!(check(b"").is_err() && check(&vec![b'a'; MAX_BYTES + 1]).is_err() && check(&[0xff, 0xfe]).is_err());
    }
}
