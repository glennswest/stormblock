//! Whether a drive is in use, before anything destroys what is on it (#701).
//!
//! A runtime format (#621), a card action's format (#611) and a firmware
//! download refuse a drive that something on this node depends on. The check
//! used to compare the engine's slab paths with `/dev/sdX`. A slab on one of
//! the engine's RAID arrays has the path `raid:<id>`, so none of the array's
//! member drives matched, and a live member could be formatted under the
//! running engine (finding 661ff070abe6). Nor did it see the node-data set,
//! which stormraid serves from the drives' data regions as `/dev/ublkb65535`.
//!
//! A drive is in use when:
//! - **the engine has a slab open on it** (by the slab's own path, or the
//!   drive under it: a partition's slab is on its disk);
//! - **it is a member of one of the engine's arrays**, attached or rebuilding;
//! - **this node's storage layout names it** (`storage-layout.json`: a system
//!   leg and a member of the node-data set, #573);
//! - **a partition of it carries a stormraid superblock** (`STORMRD1`): a
//!   member of some set, whoever serves it;
//! - **the kernel holds it** (a `holders` entry on it or a partition: md, dm)
//!   **or it is mounted** (it or a partition is in `/proc/mounts`).
//!
//! Each reason is said, so a refusal names what would be destroyed.

use std::path::Path;

use serde_json::Value;

use crate::mgmt::storage_layout::on_drive;
use crate::mgmt::AppState;

/// Every reason `dev` (`/dev/sdX`) is in use on this node, empty when none.
pub async fn reasons(state: &AppState, dev: &str) -> Vec<String> {
    let mut out = engine_reasons(state, dev).await;
    out.extend(layout_reasons(Path::new(crate::install_progress::RUN), dev));
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let name = dev.trim_start_matches("/dev/").to_string();
    let found = tokio::task::spawn_blocking(move || kernel_reasons(Path::new("/sys"), Path::new("/dev"), &mounts, &name))
        .await
        .unwrap_or_default();
    out.extend(found);
    out
}

/// The engine's own: a slab open on the drive, or one of its arrays with a
/// member there.
pub async fn engine_reasons(state: &AppState, dev: &str) -> Vec<String> {
    let mut out = Vec::new();
    {
        let reg = state.slab_registry.read().await;
        for (id, s) in reg.iter() {
            let d = s.device();
            if on_drive(&d.id().path, dev) || on_drive(&d.drive_id().path, dev) {
                out.push(format!("it holds slab {} this engine has open", id.0));
            }
        }
    }
    let arrays = state.arrays.read().await;
    for (id, info) in arrays.iter() {
        let a = &info.array;
        let mut paths: Vec<String> = a.drive_ids().into_iter().map(|d| d.path).collect();
        paths.extend(a.members_view().into_iter().filter_map(|m| m.drive.map(|d| d.path)));
        if paths.iter().any(|p| on_drive(p, dev)) {
            out.push(format!("it is a member of array {} ({}), which this engine runs", a.name(), id.0));
        }
    }
    out
}

/// This node's storage layout (#573) names the drive.
pub fn layout_reasons(run: &Path, dev: &str) -> Vec<String> {
    let layout: Value = std::fs::read(run.join("storage-layout.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let named = layout["drives"].as_array().into_iter().flatten().any(|d| d["device"].as_str() == Some(dev));
    if named {
        vec!["this node's storage layout lays it (a system leg and the node-data set's member, #573)".into()]
    } else {
        Vec::new()
    }
}

/// What the kernel and the drive's partitions say: holders, mounts, a
/// stormraid superblock. `sys` is `/sys`, `devdir` `/dev` (tests point both
/// elsewhere); `name` the disk's kernel name.
pub fn kernel_reasons(sys: &Path, devdir: &Path, mounts: &str, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let disk = sys.join("block").join(name);
    let mut parts: Vec<String> = std::fs::read_dir(&disk)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().join("partition").exists())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    parts.sort();
    let holders = |dir: &Path| -> Vec<String> {
        std::fs::read_dir(dir.join("holders"))
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default()
    };
    for h in holders(&disk) {
        out.push(format!("the kernel holds it ({h})"));
    }
    for p in &parts {
        for h in holders(&disk.join(p)) {
            out.push(format!("the kernel holds its partition {p} ({h})"));
        }
    }
    let dev = devdir.join(name).to_string_lossy().into_owned();
    for line in mounts.lines() {
        let mut f = line.split_whitespace();
        let (Some(src), Some(at)) = (f.next(), f.next()) else { continue };
        if on_drive(src, &dev) || on_drive(src, &format!("/dev/{name}")) {
            out.push(format!("{src} is mounted at {at}"));
        }
    }
    for p in &parts {
        if carries_stormraid(&devdir.join(p)) {
            out.push(format!("its partition {p} is a stormraid set's member"));
        }
    }
    out
}

/// A stormraid superblock (`STORMRD1`) in the first MiB of `path`.
fn carries_stormraid(path: &Path) -> bool {
    use std::io::Read;
    let Ok(f) = std::fs::File::open(path) else { return false };
    let mut head = Vec::with_capacity(1 << 20);
    if f.take(1 << 20).read_to_end(&mut head).is_err() {
        return false;
    }
    head.windows(8).any(|w| w == b"STORMRD1")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #701: the kernel's holders, a mount, and a partition that is a
    /// stormraid member are each a reason; a quiet drive has none.
    #[test]
    fn the_kernel_and_the_partitions_say_a_drive_is_in_use() {
        let w = tempfile::tempdir().unwrap();
        let (sys, devdir) = (w.path().join("sys"), w.path().join("dev"));
        let mk = |disk: &str, parts: &[&str]| {
            std::fs::create_dir_all(sys.join("block").join(disk).join("holders")).unwrap();
            for p in parts {
                let d = sys.join("block").join(disk).join(p);
                std::fs::create_dir_all(d.join("holders")).unwrap();
                std::fs::write(d.join("partition"), "1\n").unwrap();
            }
        };
        std::fs::create_dir_all(&devdir).unwrap();
        mk("sda", &["sda1"]);
        assert!(kernel_reasons(&sys, &devdir, "", "sda").is_empty(), "a quiet drive");

        mk("sdb", &["sdb1", "sdb4"]);
        std::fs::create_dir_all(sys.join("block/sdb/sdb1/holders/md0")).unwrap();
        let mut member = vec![0u8; 8192];
        member[4096..4104].copy_from_slice(b"STORMRD1");
        std::fs::write(devdir.join("sdb4"), &member).unwrap();
        std::fs::write(devdir.join("sdb1"), vec![0u8; 4096]).unwrap();
        let mounts = format!("{}/sdb1 /boot vfat rw 0 0\n/dev/sda1x /x ext4 rw 0 0\n", devdir.display());
        let r = kernel_reasons(&sys, &devdir, &mounts, "sdb");
        assert!(r.iter().any(|s| s.contains("partition sdb1 (md0)")), "{r:?}");
        assert!(r.iter().any(|s| s.contains("mounted at /boot")), "{r:?}");
        assert!(r.iter().any(|s| s.contains("sdb4 is a stormraid set's member")), "{r:?}");
        assert_eq!(r.len(), 3, "{r:?}");
    }

    /// #701: a drive the node's layout lays (forge1's sda..sdd: a system leg
    /// and the node-data set's member) is in use; another is not.
    #[test]
    fn the_layout_names_its_drives_in_use() {
        let w = tempfile::tempdir().unwrap();
        std::fs::write(
            w.path().join("storage-layout.json"),
            r#"{"drives": [{"device": "/dev/sda"}, {"device": "/dev/sdb"}]}"#,
        )
        .unwrap();
        assert_eq!(layout_reasons(w.path(), "/dev/sdb").len(), 1);
        assert!(layout_reasons(w.path(), "/dev/sdc").is_empty());
        assert!(layout_reasons(&w.path().join("none"), "/dev/sda").is_empty());
    }
}
