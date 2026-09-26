//! Steps the suites share.

use std::sync::Arc;

use serde_json::{json, Value};
use stormblock::drive::BlockDevice;

use crate::engine::{write_check, Engine, MIB};

pub fn items(v: &Value) -> Vec<Value> {
    v["items"].as_array().or(v.as_array()).cloned().unwrap_or_default()
}

pub async fn volume_count(e: &Engine) -> Result<usize, String> {
    Ok(items(&e.ok("GET", "/volumes", None).await?).len())
}

/// Slots allocated across every slab.
pub async fn allocated_slots(e: &Engine) -> Result<u64, String> {
    Ok(items(&e.ok("GET", "/slabs", None).await?)
        .iter()
        .map(|s| s["allocated_slots"].as_u64().unwrap_or(0))
        .sum())
}

pub async fn create_volume(e: &Engine, name: &str, size: &str, redundancy: Option<&str>) -> Result<String, String> {
    let mut body = json!({ "name": name, "size": size });
    if let Some(r) = redundancy {
        body["redundancy"] = json!(r);
    }
    let v = e.ok("POST", "/volumes", Some(body)).await?;
    v["id"].as_str().map(str::to_string).ok_or_else(|| format!("create {name} answered {v}"))
}

pub async fn delete_volume(e: &Engine, id: &str) -> Result<(), String> {
    e.ok("DELETE", &format!("/volumes/{id}"), None).await.map(|_| ())
}

/// A sealed ext4 blank, the way a size class is made. Answers the template id.
pub async fn make_blank(e: &Engine, name: &str, size: &str) -> Result<String, String> {
    let v = e
        .ok("POST", "/fstemplates", Some(json!({ "name": name, "size": size, "fs": "ext4" })))
        .await?;
    v["template"]["id"].as_str().map(str::to_string).ok_or_else(|| format!("template create answered {v}"))
}

/// A claim of a blank: a clone with its own filesystem identity.
pub async fn claim(e: &Engine, template: &str) -> Result<String, String> {
    let v = e.ok("POST", &format!("/fstemplates/{template}/claim"), Some(json!({}))).await?;
    v["volume_id"].as_str().map(str::to_string).ok_or_else(|| format!("claim answered {v}"))
}

/// The ext4 superblock magic (0xEF53) where it belongs.
pub async fn is_ext4(dev: &Arc<dyn BlockDevice>) -> Result<bool, String> {
    let mut sb = vec![0u8; 4096];
    dev.read(0, &mut sb).await.map_err(|e| format!("read superblock: {e}"))?;
    Ok(sb[1024 + 56..1024 + 58] == [0x53, 0xEF])
}

/// Attach, write and read back `mib` MiB at `at`, detach.
pub async fn attach_write(e: &Engine, id: &str, at: u64, mib: u64, seed: u64) -> Result<(), String> {
    let dev = e.attach(id).await?;
    let r = write_check(&dev, at, seed, (mib * MIB) as usize).await;
    drop(dev);
    e.detach(id).await?;
    r
}
