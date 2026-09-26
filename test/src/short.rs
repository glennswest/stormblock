//! `short` (< 2 min): the node's engine is up, and the engine of this commit
//! does its main job — create, clone, attach, write, read, delete — leaving
//! nothing behind. The gate every OS release passes on every test machine.

use crate::engine::{read_check, Engine, MIB};
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
        let e = Engine::start(env, "short", 512 * MIB).await?;
        let h = e.ok("GET", "/health", None).await?;
        engine = Some(e);
        Ok(format!("version {}", h["version"]))
    })
    .await;
    let Some(e) = engine else { return Ok(()) };

    let base_vols = volume_count(&e).await?;
    let base_slots = allocated_slots(&e).await?;
    let mut made: Vec<String> = Vec::new();
    let mut blank: Option<String> = None;

    r.run("volume-create-attach-write", async {
        let id = create_volume(&e, "t-plain", "64M", None).await?;
        made.push(id.clone());
        let dev = e.attach(&id).await?;
        crate::engine::write_check(&dev, 0, 1, MIB as usize).await?;
        crate::engine::write_check(&dev, 40 * MIB, 2, MIB as usize).await?;
        read_check(&dev, 0, 1, MIB as usize).await?;
        drop(dev);
        e.detach(&id).await?;
        Ok(format!("volume {id}: 2 MiB written and read back over NVMe/TCP"))
    })
    .await;

    r.run("blank-claim-clone", async {
        let t = make_blank(&e, "t-blank", "64M").await?;
        blank = Some(t.clone());
        let id = claim(&e, &t).await?;
        made.push(id.clone());
        let dev = e.attach(&id).await?;
        ensure(is_ext4(&dev).await?, "the claim carries no ext4 superblock")?;
        crate::engine::write_check(&dev, 32 * MIB, 3, MIB as usize).await?;
        drop(dev);
        e.detach(&id).await?;
        Ok(format!("claim {id} of blank {t}: ext4, writable"))
    })
    .await;

    r.run("delete-leaves-nothing", async {
        for id in made.drain(..) {
            delete_volume(&e, &id).await?;
        }
        if let Some(t) = blank.take() {
            e.ok("DELETE", &format!("/fstemplates/{t}"), None).await?;
        }
        let vols = volume_count(&e).await?;
        let slots = allocated_slots(&e).await?;
        ensure(vols == base_vols, format!("{vols} volume(s) left, {base_vols} before"))?;
        ensure(slots == base_slots, format!("{slots} slot(s) allocated, {base_slots} before"))?;
        Ok::<_, Why>(format!("{vols} volume(s), {slots} slot(s): as before"))
    })
    .await;

    e.remove().await;
    Ok(())
}
