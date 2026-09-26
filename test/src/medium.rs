//! `medium` (< 30 min): the engine's features and failure paths, end to end,
//! on the engine of this commit in the pod.

use serde_json::json;

use crate::engine::{read_check, write_check, Engine, MIB};
use crate::env::Env;
use crate::flows::*;
use crate::node;
use crate::report::{ensure, Report, Why};

pub async fn run(env: &Env, r: &mut Report) -> Result<(), String> {
    r.run("node-health", node::health(env)).await;
    r.run("node-inventory", node::inventory(env)).await;
    if !env.bin.exists() {
        return Err(format!("no stormblock binary at {}", env.bin.display()));
    }
    let mut engine: Option<Engine> = None;
    r.run("engine-up", async {
        engine = Some(Engine::start(env, "medium", 1024 * MIB).await?);
        Ok("up".to_string())
    })
    .await;
    let Some(mut e) = engine else { return Ok(()) };

    r.run("api-closed-without-token", async {
        let s = e.call_anonymous("GET", "/volumes").await?;
        ensure(s == 401, format!("GET /volumes without a token answered {s}"))?;
        let s = e.call_anonymous("POST", "/volumes").await?;
        ensure(s == 401, format!("POST /volumes without a token answered {s}"))?;
        let s = e.call_anonymous("GET", "/health").await?;
        ensure(s == 200, format!("health answered {s}"))?;
        Ok("401 without the token; health open".to_string())
    })
    .await;

    r.run("restart-keeps-flushed-data", async {
        let id = create_volume(&e, "t-restart", "64M", None).await?;
        attach_write(&e, &id, 8 * MIB, 4, 11).await?;
        e.restart().await?;
        let dev = e.attach(&id).await?;
        read_check(&dev, 8 * MIB, 11, (4 * MIB) as usize).await?;
        drop(dev);
        e.detach(&id).await?;
        delete_volume(&e, &id).await?;
        Ok("4 MiB read back after SIGTERM and restart".to_string())
    })
    .await;

    r.run("crash-keeps-flushed-data", async {
        let id = create_volume(&e, "t-crash", "64M", None).await?;
        let blank = make_blank(&e, "t-crash-blank", "64M").await?;
        let clone = claim(&e, &blank).await?;
        attach_write(&e, &id, 0, 2, 21).await?;
        attach_write(&e, &clone, 16 * MIB, 2, 22).await?;
        e.crash_restart().await?;
        for (vid, off, seed) in [(&id, 0, 21), (&clone, 16 * MIB, 22)] {
            let dev = e.attach(vid).await?;
            read_check(&dev, off, seed, (2 * MIB) as usize).await?;
            drop(dev);
            e.detach(vid).await?;
        }
        let dev = e.attach(&clone).await?;
        ensure(is_ext4(&dev).await?, "the claim lost its filesystem across the crash")?;
        drop(dev);
        e.detach(&clone).await?;
        delete_volume(&e, &id).await?;
        delete_volume(&e, &clone).await?;
        e.ok("DELETE", &format!("/fstemplates/{blank}"), None).await?;
        Ok("a volume and a claim read back after SIGKILL and restart".to_string())
    })
    .await;

    r.run("group-snapshot-and-restore", async {
        let a = create_volume(&e, "t-vm-root", "32M", None).await?;
        let b = create_volume(&e, "t-vm-data", "32M", None).await?;
        attach_write(&e, &a, 0, 1, 31).await?;
        attach_write(&e, &b, 0, 1, 32).await?;
        let g = e
            .ok("POST", &format!("{}/group-snapshots", e.v1()), Some(json!({ "name": "t-vm-snap", "volume_ids": [a, b] })))
            .await?;
        ensure(g["ready"] == true, format!("group snapshot not ready: {g}"))?;
        // The guest keeps writing after the snapshot.
        attach_write(&e, &a, 0, 1, 33).await?;
        let snap = g["snapshots"][0]["id"].as_str().ok_or("no member snapshot")?.to_string();
        let v = e
            .ok(
                "POST",
                &format!("{}/volumes", e.v1()),
                Some(json!({
                    "name": "t-vm-root-restored", "size_bytes": 32 * MIB,
                    "replica_tier": { "slaves": 0 },
                    "source": { "kind": "snapshot", "id": snap },
                })),
            )
            .await?;
        let rid = v["id"].as_str().ok_or("no restored volume id")?.to_string();
        let dev = e.attach_v1(&rid).await?;
        read_check(&dev, 0, 31, MIB as usize).await?;
        drop(dev);
        e.ok("POST", &format!("{}/volumes/{rid}/detach", e.v1()), Some(json!({ "node": e.node }))).await?;
        e.ok("DELETE", &format!("{}/volumes/{rid}", e.v1()), None).await?;
        e.ok("DELETE", &format!("{}/group-snapshots/{}", e.v1(), g["id"].as_str().unwrap_or("")), None).await?;
        delete_volume(&e, &a).await?;
        delete_volume(&e, &b).await?;
        Ok("restored the snapshot's point in time, not what came after".to_string())
    })
    .await;

    r.run("mirror-survives-a-failed-drive", async {
        // Three drives in three shelves beside the seed array.
        let mut drives = Vec::new();
        for (i, shelf) in ["a", "b", "c"].iter().enumerate() {
            let path = e.dir.join(format!("m{i}.img"));
            crate::engine::sparse(&path, 128 * MIB)?;
            let p = path.display().to_string();
            let d = e
                .ok("POST", "/drives", Some(json!({ "path": p, "size_bytes": 128 * MIB, "labels": { "shelf": shelf } })))
                .await?;
            e.ok("POST", "/slabs", Some(json!({ "device_path": p, "slot_size": MIB, "metadata_bytes": 0 }))).await?;
            drives.push((p, d["uuid"].as_str().unwrap_or("").to_string()));
        }
        let id = create_volume(&e, "t-mirror", "16M", Some("mirror:2@shelf")).await?;
        attach_write(&e, &id, 0, 16, 41).await?;
        // A drive that holds one of its legs.
        let v = e.ok("GET", &format!("/volumes/{id}"), None).await?;
        let victim = items(&v["placement"]["slabs"])
            .iter()
            .filter(|s| s["legs"].as_u64().unwrap_or(0) > 0)
            .find_map(|s| drives.iter().find(|(p, _)| s["drive"]["path"].as_str() == Some(p.as_str())).cloned())
            .ok_or_else(|| format!("no leg on the test drives: {}", v["placement"]))?;
        let h = e
            .ok("POST", &format!("/drives/{}/health", victim.1), Some(json!({ "state": "failed", "reason": "test" })))
            .await?;
        let job = h["rebuild"].as_u64().ok_or_else(|| format!("no rebuild started: {h}"))?;
        let mut state = serde_json::Value::Null;
        for _ in 0..1200 {
            state = e.ok("GET", &format!("/rebuilds/{job}"), None).await?;
            if state["state"] != "running" && state["state"] != "queued" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        ensure(state["state"] == "done", format!("rebuild ended {state}"))?;
        let dev = e.attach(&id).await?;
        read_check(&dev, 0, 41, (16 * MIB) as usize).await?;
        drop(dev);
        e.detach(&id).await?;
        let health = e.ok("GET", &format!("/volumes/{id}/health"), None).await?;
        delete_volume(&e, &id).await?;
        Ok(format!("drive {} failed, rebuilt, 16 MiB intact; health {}", victim.1, health["state"]))
    })
    .await;

    r.run("sealed-refuses-read-write-attach", async {
        let t = make_blank(&e, "t-sealed", "64M").await?;
        let tv = e.ok("GET", &format!("/fstemplates/{t}"), None).await?;
        let sealed = tv["sealed_volume_id"]
            .as_str()
            .or(tv["template"]["sealed_volume_id"].as_str())
            .ok_or_else(|| format!("no sealed volume: {tv}"))?
            .to_string();
        let (s, body) = e
            .call("POST", &format!("/volumes/{sealed}/attach"), Some(json!({ "transport": "nvme-tcp" })))
            .await?;
        ensure((400..500).contains(&s), format!("a read-write attach of a golden answered {s}: {body}"))?;
        e.ok("DELETE", &format!("/fstemplates/{t}"), None).await?;
        Ok(format!("refused with {s}"))
    })
    .await;

    r.run("discard-gives-space-back", async {
        let id = create_volume(&e, "t-discard", "64M", None).await?;
        let dev = e.attach(&id).await?;
        write_check(&dev, 0, 51, (32 * MIB) as usize).await?;
        let before = e.ok("GET", &format!("/volumes/{id}"), None).await?["allocated_bytes"].as_u64().unwrap_or(0);
        dev.discard(0, 32 * MIB).await.map_err(|e| format!("discard: {e}"))?;
        dev.flush().await.map_err(|e| format!("flush: {e}"))?;
        drop(dev);
        e.detach(&id).await?;
        let after = e.ok("GET", &format!("/volumes/{id}"), None).await?["allocated_bytes"].as_u64().unwrap_or(0);
        delete_volume(&e, &id).await?;
        ensure(before >= 32 * MIB && after < before / 4, format!("allocated {before} → {after} after discarding 32 MiB"))?;
        Ok::<_, Why>(format!("allocated {before} → {after}"))
    })
    .await;

    e.remove().await;
    Ok(())
}
