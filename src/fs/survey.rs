//! What filesystems a volume carries, found and read (#147).
//!
//! An imported cloud image is usually a whole disk: a GPT with an ESP, a
//! `/boot`, and a root. For RHEL, Rocky and Alma that root is XFS. The engine
//! stores the disk as bytes and records it as `gpt`, and until #147 it never
//! looked inside. This finds every XFS and ext2/3/4 filesystem on the volume,
//! either the whole volume or each GPT partition. It opens each one with the
//! userspace reader for its kind (`fio-xfs`, `fio-ext4`), reads
//! `/etc/os-release` where there is one, and walks the tree end to end, which
//! reads every directory and inode and checks every v5 XFS CRC on the way. A
//! filesystem that is recognised and does not read is an image nothing will
//! boot, and an import says so rather than sealing it.
//!
//! Read-only throughout: nothing here writes to the volume.

use std::sync::Arc;

use serde::Serialize;
use uuid::Uuid;

use crate::drive::partition::PartitionDevice;
use crate::drive::BlockDevice;

/// One filesystem found on a volume.
#[derive(Debug, Clone, Serialize)]
pub struct FoundFs {
    /// 1-based GPT partition number; absent when the whole volume is the
    /// filesystem.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition: Option<u32>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub partition_name: String,
    /// Byte offset and length on the volume.
    pub offset: u64,
    pub bytes: u64,
    /// `xfs`, or `ext2`/`ext3`/`ext4` (reported as `ext4`: one reader).
    pub kind: String,
    pub uuid: Uuid,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// `PRETTY_NAME` from `/etc/os-release`, when this is a root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// What walking it found, when it was walked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub walked: Option<Walked>,
    /// Its log or journal (#198): `clean`; for XFS `dirty`, `external` or
    /// `unreadable`; for ext4 `needs_recovery`. Anything but `clean` is not
    /// what a mount would show: Linux replays it first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    /// Why it could not be read, if it could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Walked {
    pub entries: u64,
    pub directories: u64,
    pub files: u64,
}

/// `PRETTY_NAME` (else `NAME`) from an os-release file.
fn pretty_name(os_release: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(os_release);
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
            .map(|v| v.trim().trim_matches('"').to_string())
    };
    field("PRETTY_NAME").or_else(|| field("NAME"))
}

async fn read_xfs(dev: &Arc<dyn BlockDevice>, walk: bool, f: &mut FoundFs) {
    use super::xfs;
    // The log first (#198): a filesystem not cleanly unmounted reads, as it
    // stands on disk, as something older than what a mount shows.
    match xfs::log_state(dev).await {
        Ok(state) => {
            f.log = Some(xfs::log_name(&state).to_string());
            if let Some(why) = xfs::log_problem(&state) {
                f.error = Some(match state {
                    // fio.xfs.rs#16: on rare occasions a clean log reads dirty.
                    fio_xfs::LogState::Dirty { .. } => format!(
                        "{why} (mount it once, or unmount it cleanly, and import again; on rare \
                         occasions a clean log is read as dirty, fio.xfs.rs#16)"
                    ),
                    _ => why,
                });
                return;
            }
        }
        Err(e) => {
            f.error = Some(format!("reading the log: {e}"));
            return;
        }
    }
    match xfs::read_file(dev, "/etc/os-release").await {
        Ok(Some(b)) => f.os = pretty_name(&b),
        Ok(None) => {}
        Err(e) => {
            f.error = Some(format!("reading /etc/os-release: {e}"));
            return;
        }
    }
    if walk {
        match xfs::check(dev).await {
            Ok(c) => f.walked = Some(Walked { entries: c.entries, directories: c.directories, files: c.files }),
            Err(e) => f.error = Some(format!("walking the tree: {e}")),
        }
    }
}

async fn read_ext4(dev: &Arc<dyn BlockDevice>, walk: bool, f: &mut FoundFs) {
    let vol = match fio_ext4::Volume::open(super::ext4::VolumeDevice::opaque(dev.clone())).await {
        Ok(v) => v,
        Err(e) => {
            f.error = Some(format!("opening: {e}"));
            return;
        }
    };
    match vol.exists("/etc/os-release").await {
        Ok(true) => match vol.read("/etc/os-release").await {
            Ok(b) => f.os = pretty_name(&b),
            Err(e) => {
                f.error = Some(format!("reading /etc/os-release: {e}"));
                return;
            }
        },
        Ok(false) => {}
        Err(e) => {
            f.error = Some(format!("looking up /etc/os-release: {e}"));
            return;
        }
    }
    if !walk {
        return;
    }
    // `fio-ext4` lists a directory at a time; walk it breadth first.
    let mut w = Walked::default();
    let mut dirs = vec!["/".to_string()];
    while let Some(dir) = dirs.pop() {
        let entries = match vol.read_dir(&dir).await {
            Ok(e) => e,
            Err(e) => {
                f.error = Some(format!("walking {dir}: {e}"));
                return;
            }
        };
        for e in entries {
            if e.name == "." || e.name == ".." {
                continue;
            }
            w.entries += 1;
            if e.is_dir {
                w.directories += 1;
                let path = if dir == "/" { format!("/{}", e.name) } else { format!("{dir}/{}", e.name) };
                // `lost+found` is a directory like any other; nothing to skip.
                dirs.push(path);
            } else {
                w.files += 1;
            }
        }
    }
    f.walked = Some(w);
}

/// Identify and read one filesystem at `dev`, if there is one.
async fn one(dev: Arc<dyn BlockDevice>, walk: bool, partition: Option<(u32, String)>, offset: u64) -> Option<FoundFs> {
    let bytes = dev.capacity_bytes();
    let (part, name) = partition.map(|(p, n)| (Some(p), n)).unwrap_or((None, String::new()));
    if super::xfs::looks_like_xfs(&dev).await {
        let mut f = FoundFs {
            partition: part,
            partition_name: name,
            offset,
            bytes,
            kind: "xfs".into(),
            uuid: Uuid::nil(),
            label: String::new(),
            os: None,
            walked: None,
            log: None,
            error: None,
        };
        match super::xfs::read_layout(&dev).await {
            Ok(l) => {
                f.uuid = l.uuid;
                f.label = l.label;
                read_xfs(&dev, walk, &mut f).await;
            }
            Err(e) => f.error = Some(e.to_string()),
        }
        return Some(f);
    }
    if let Ok(l) = super::ext4::read_layout(&dev).await {
        let mut f = FoundFs {
            partition: part,
            partition_name: name,
            offset,
            bytes,
            kind: "ext4".into(),
            uuid: l.uuid,
            label: l.label.clone(),
            os: None,
            walked: None,
            log: Some(if l.needs_recovery { "needs_recovery" } else { "clean" }.into()),
            error: None,
        };
        // A pending journal replay (#198): what is on disk may be stale
        // until a mount replays it.
        if l.needs_recovery {
            f.error = Some(
                "the journal needs recovery (RECOVER is set): the filesystem was not cleanly \
                 unmounted, and what is on disk may be stale until a mount replays the journal"
                    .into(),
            );
            return Some(f);
        }
        read_ext4(&dev, walk, &mut f).await;
        return Some(f);
    }
    None
}

/// Every XFS and ext filesystem on `dev`: the volume itself, or each of its
/// GPT partitions. `walk` also reads each tree end to end.
pub async fn survey(dev: &Arc<dyn BlockDevice>, walk: bool) -> Vec<FoundFs> {
    if let Some(f) = one(dev.clone(), walk, None, 0).await {
        return vec![f];
    }
    let Ok(gpt) = crate::pallet::gpt::Gpt::read(dev).await else {
        return Vec::new();
    };
    let lba = gpt.block_size as u64;
    let mut out = Vec::new();
    for (i, e) in gpt.entries.iter().enumerate() {
        if e.type_guid == [0u8; 16] || e.last_lba < e.first_lba {
            continue;
        }
        let start = e.first_lba * lba;
        let len = (e.last_lba - e.first_lba + 1) * lba;
        // Reading only, and a thin volume takes any offset: the window
        // presents the table's own sector size, since a cloud image's
        // partitions need not sit on the volume's 4 KiB blocks.
        let Ok(part) = PartitionDevice::with_block_size(dev.clone(), start, len, lba as u32) else {
            continue;
        };
        if let Some(f) = one(Arc::new(part), walk, Some((i as u32 + 1, e.name.clone())), start).await {
            out.push(f);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_names() {
        let r = b"NAME=\"Rocky Linux\"\nVERSION=\"9.4 (Blue Onyx)\"\nPRETTY_NAME=\"Rocky Linux 9.4 (Blue Onyx)\"\n";
        assert_eq!(pretty_name(r).as_deref(), Some("Rocky Linux 9.4 (Blue Onyx)"));
        assert_eq!(pretty_name(b"NAME=Alpine\n").as_deref(), Some("Alpine"));
        assert_eq!(pretty_name(b"ID=x\n"), None);
    }

    async fn scratch(name: &str, bytes: u64) -> (Arc<dyn BlockDevice>, String) {
        let path = std::env::temp_dir()
            .join(format!("stormblock-survey-198-{name}-{}.bin", uuid::Uuid::new_v4().simple()));
        let path = path.to_str().unwrap().to_string();
        let dev = crate::drive::filedev::FileDevice::open_with_capacity(&path, bytes).await.unwrap();
        (Arc::new(dev), path)
    }

    fn import_spec(verify: bool) -> crate::image::import::ImportSpec {
        serde_json::from_value(serde_json::json!({"name": "img", "file": "/x", "verify": verify})).unwrap()
    }

    /// Leave an XFS log as a system that was not cleanly unmounted does: an
    /// unmount record, then transactions after it (fio-xfs's own fixture).
    async fn dirty_the_xfs_log(dev: &Arc<dyn BlockDevice>) {
        let mut sb = vec![0u8; 512];
        dev.read(0, &mut sb).await.unwrap();
        let be32 = |o: usize| u32::from_be_bytes(sb[o..o + 4].try_into().unwrap()) as u64;
        let (block, ag_blocks, log_blocks) = (be32(4), be32(84), be32(96));
        let log_start = u64::from_be_bytes(sb[48..56].try_into().unwrap());
        let ag_log = sb[124] as u64;
        let at = ((log_start >> ag_log) * ag_blocks + (log_start & ((1 << ag_log) - 1))) * block;
        let mut log = vec![0u8; (log_blocks * block) as usize];
        let bb = 512usize;
        let mut record = |log: &mut Vec<u8>, at: usize, data: usize, unmount: bool, tail: u64| {
            let h = at * bb;
            log[h..h + 4].copy_from_slice(&0xFEED_BABEu32.to_be_bytes());
            log[h + 4..h + 8].copy_from_slice(&1u32.to_be_bytes());
            log[h + 8..h + 12].copy_from_slice(&2u32.to_be_bytes());
            log[h + 12..h + 16].copy_from_slice(&((data * bb) as u32).to_be_bytes());
            log[h + 16..h + 24].copy_from_slice(&(1u64 << 32 | at as u64).to_be_bytes());
            log[h + 24..h + 32].copy_from_slice(&(1u64 << 32 | tail).to_be_bytes());
            log[h + 40..h + 44].copy_from_slice(&(if unmount { 1u32 } else { 3 }).to_be_bytes());
            log[h + 320..h + 324].copy_from_slice(&32768u32.to_be_bytes());
            for i in 1..=data {
                let d = (at + i) * bb;
                log[d..d + 4].copy_from_slice(&1u32.to_be_bytes());
                if i == 1 && unmount {
                    log[d + 9] = 0x20;
                }
            }
        };
        record(&mut log, 0, 1, true, 0);
        let mut at_bb = 2;
        while at_bb < 50 {
            let data = (50 - at_bb).min(8) - 1;
            record(&mut log, at_bb, data, false, 2);
            at_bb += data + 1;
        }
        dev.write(at, &log).await.unwrap();
        dev.flush().await.unwrap();
    }

    /// #198: an engine-made XFS reads clean and seals; the same filesystem
    /// with a dirty log is reported `dirty`, fails the import's verification
    /// (passes with `verify: false`) and is refused a seal.
    #[tokio::test]
    async fn an_xfs_with_a_dirty_log_fails_verification_and_the_seal() {
        let (dev, path) = scratch("xfs", 320 * 1024 * 1024).await;
        crate::fs::xfs::format(&dev, &crate::fs::xfs::XfsParams::default()).await.unwrap();
        let found = survey(&dev, true).await;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].log.as_deref(), Some("clean"), "{:?}", found[0]);
        assert!(found[0].error.is_none(), "{:?}", found[0]);
        assert!(crate::image::import::verdict(&import_spec(true), &found).is_ok());
        assert!(crate::fs::xfs::seal_blockers(&dev).await.unwrap().is_empty(), "a fresh XFS seals");

        dirty_the_xfs_log(&dev).await;
        assert!(matches!(
            crate::fs::xfs::log_state(&dev).await.unwrap(),
            fio_xfs::LogState::Dirty { .. }
        ));
        let found = survey(&dev, true).await;
        assert_eq!(found[0].log.as_deref(), Some("dirty"));
        let err = found[0].error.clone().unwrap();
        assert!(err.contains("the log is dirty") && err.contains("fio.xfs.rs#16"), "{err}");
        let v = crate::image::import::verdict(&import_spec(true), &found).unwrap_err();
        assert!(v.contains("the log is dirty") && v.contains("\"verify\": false"), "{v}");
        assert!(crate::image::import::verdict(&import_spec(false), &found).is_ok(), "verify false takes it as it is");
        let blockers = crate::fs::xfs::seal_blockers(&dev).await.unwrap();
        assert!(blockers.iter().any(|b| b.contains("the log is dirty")), "{blockers:?}");
        let _ = std::fs::remove_file(path);
    }

    /// #198, ext4: a pending journal replay (RECOVER) fails verification.
    #[tokio::test]
    async fn an_ext4_needing_recovery_fails_verification() {
        let (dev, path) = scratch("ext4", 128 * 1024 * 1024).await;
        crate::fs::ext4::format(&dev, &crate::fs::ext4::Ext4Params::default()).await.unwrap();
        let found = survey(&dev, true).await;
        assert_eq!(found[0].log.as_deref(), Some("clean"));
        assert!(found[0].error.is_none(), "{:?}", found[0]);
        {
            use mkfs_ext4::features::IncompatFeatures;
            let target = crate::fs::ext4::VolumeDevice::opaque(dev.clone());
            let mut fs = mkfs_ext4::fs::Filesystem::open(target).await.unwrap();
            fs.superblock_mut().feature_incompat |= IncompatFeatures::RECOVER;
            fs.flush_superblock().await.unwrap();
        }
        let found = survey(&dev, true).await;
        assert_eq!(found[0].log.as_deref(), Some("needs_recovery"));
        let v = crate::image::import::verdict(&import_spec(true), &found).unwrap_err();
        assert!(v.contains("the journal needs recovery"), "{v}");
        assert!(crate::image::import::verdict(&import_spec(false), &found).is_ok());
        let _ = std::fs::remove_file(path);
    }
}
