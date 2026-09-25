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
            error: None,
        };
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
}
