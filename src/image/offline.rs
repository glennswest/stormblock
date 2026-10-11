//! Goldens in and out of a slab with no engine running (#87).
//!
//! What the boot path and the API already do — a sealed golden in a slab, a
//! copy-on-write clone of it handed over as a device — for a build tool that
//! holds a slab image and no daemon (tinyanvil): `stormblock clone` imports a
//! golden image once and clones it, `stormblock attach` mounts the clone, and
//! `stormblock golden --from-volume` writes the result back out as a golden
//! image. The same volume manager, the same clone (`fs::clone_volume`: a fresh
//! filesystem UUID, checked), persisted to the slab's own records.

use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::drive::BlockDevice;
use crate::fs::template::VmLock;
use crate::volume::{CreateOptions, VolumeId};

/// How many hex digits of the content's sha256 name an imported golden.
pub const DIGEST_IN_NAME: usize = 16;

/// What [`import_golden`] found or made.
#[derive(Debug, Clone)]
pub struct Imported {
    pub id: VolumeId,
    pub name: String,
    /// sha256 of the image's content (its virtual disk, whatever the file's
    /// format), hex.
    pub sha256: String,
    /// False when the slab already held it: nothing was written.
    pub imported: bool,
    /// Bytes written into the slab (non-zero chunks only).
    pub written: u64,
}

/// The name an image of this file and content is kept under in a slab:
/// `golden-<stem>-<first 16 hex of its sha256>`.
pub fn golden_name(file: &Path, sha256: &str) -> String {
    let mut stem = file
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".into());
    for ext in [".gz", ".xz", ".img", ".raw", ".qcow2", ".vmdk", ".ova", ".iso"] {
        if let Some(s) = stem.strip_suffix(ext) {
            stem = s.to_string();
        }
    }
    let stem: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' })
        .collect();
    let stem = stem.trim_matches('-');
    let stem = if stem.is_empty() { "image" } else { stem };
    format!("golden-{stem}-{}", &sha256[..DIGEST_IN_NAME.min(sha256.len())])
}

/// sha256 of an image's content: every byte of its virtual disk.
pub async fn content_sha256(path: &Path) -> Result<String, String> {
    let (mut src, _) = crate::image::import::open_source(path, None).await?;
    let size = src.virtual_size();
    let chunk = src.chunk().max(4096);
    let mut h = Sha256::new();
    let mut buf = vec![0u8; chunk as usize];
    let zeros = vec![0u8; chunk as usize];
    let mut off = 0u64;
    while off < size {
        let n = chunk.min(size - off) as usize;
        if src.may_have_data(off, chunk).await? {
            src.read_at(off, &mut buf[..n]).await?;
            h.update(&buf[..n]);
        } else {
            h.update(&zeros[..n]);
        }
        off += n as u64;
    }
    Ok(hex::encode(h.finalize()))
}

/// The sealed golden in this slab with that content, if there is one.
pub async fn find_golden(vm: &Arc<VmLock>, sha256: &str, at_least: u64) -> Option<(VolumeId, String)> {
    let tail = format!("-{}", &sha256[..DIGEST_IN_NAME.min(sha256.len())]);
    let m = vm.lock().await;
    for (id, name, size, _) in m.list_volumes().await {
        if name.starts_with("golden-") && name.ends_with(&tail) && m.is_sealed(&id) && size >= at_least {
            return Some((id, name));
        }
    }
    None
}

/// Import `file` (raw, qcow2, vmdk) into the slab as a sealed golden, unless
/// one with its content is already there. Only non-zero chunks are written;
/// a filesystem in it that does not read is refused (nothing is kept).
pub async fn import_golden(vm: &Arc<VmLock>, file: &Path) -> Result<Imported, String> {
    let sha256 = content_sha256(file).await.map_err(|e| format!("{}: {e}", file.display()))?;
    let (mut src, format) = crate::image::import::open_source(file, None)
        .await
        .map_err(|e| format!("{}: {e}", file.display()))?;
    let size = src.virtual_size();
    if size == 0 {
        return Err(format!("{}: the image is empty", file.display()));
    }
    if let Some((id, name)) = find_golden(vm, &sha256, size).await {
        return Ok(Imported { id, name, sha256, imported: false, written: 0 });
    }
    let name = golden_name(file, &sha256);
    let id = {
        let mut m = vm.lock().await;
        if m.find_volume(&name).await.is_some() {
            return Err(format!("{name} is in the slab and is not a sealed golden of this content"));
        }
        m.create_volume_with(&name, size, CreateOptions::default())
            .await
            .map_err(|e| format!("create {name}: {e}"))?
    };
    let undo = |e: String| async move {
        let _ = vm.lock().await.delete_volume(id).await;
        Err::<Imported, String>(e)
    };
    let Some(dev) = vm.lock().await.get_volume(&id) else {
        return undo("the new volume has no handle".into()).await;
    };
    tracing::info!("importing {} ({format}, {size} bytes) as {name}", file.display());
    let chunk = src.chunk().max(4096);
    let mut buf = vec![0u8; chunk as usize];
    let mut off = 0u64;
    let mut written = 0u64;
    while off < size {
        let n = chunk.min(size - off) as usize;
        let has = match src.may_have_data(off, chunk).await {
            Ok(h) => h,
            Err(e) => return undo(format!("{}: {e}", file.display())).await,
        };
        if has {
            if let Err(e) = src.read_at(off, &mut buf[..n]).await {
                return undo(format!("{}: {e}", file.display())).await;
            }
            if buf[..n].iter().any(|&b| b != 0) {
                if let Err(e) = dev.write(off, &buf[..n]).await {
                    return undo(format!("write {name} at {off}: {e}")).await;
                }
                written += n as u64;
            }
        }
        off += n as u64;
    }
    if let Err(e) = dev.flush().await {
        return undo(format!("flush {name}: {e}")).await;
    }
    let fs = crate::fs::disk::probe(&dev).await;
    let bad: Vec<String> = crate::fs::survey::survey(&dev, true)
        .await
        .into_iter()
        .filter_map(|f| f.error.map(|e| format!("{}: {e}", f.kind)))
        .collect();
    drop(dev);
    if !bad.is_empty() {
        return undo(format!("{}: the image does not read cleanly ({})", file.display(), bad.join("; "))).await;
    }
    if let Err(e) = vm.lock().await.seal_volume(id, fs).await {
        return undo(format!("seal {name}: {e}")).await;
    }
    Ok(Imported { id, name, sha256, imported: true, written })
}

/// Write a volume out as an image file: its whole capacity, sparse (zero
/// chunks are holes). Returns the bytes written.
pub async fn export_volume(dev: &Arc<dyn BlockDevice>, out: &Path) -> Result<u64, String> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};
    const STEP: u64 = 1 << 20;
    let size = dev.capacity_bytes();
    let bs = dev.block_size().max(1) as u64;
    let _ = tokio::fs::remove_file(out).await;
    let mut f = tokio::fs::File::create(out).await.map_err(|e| format!("{}: {e}", out.display()))?;
    let mut off = 0u64;
    let mut written = 0u64;
    while off < size {
        let want = STEP.min(size - off);
        let span = want.div_ceil(bs) * bs;
        let mut buf = crate::drive::dma::DmaBuf::zeroed(span as usize);
        dev.read(off, &mut buf).await.map_err(|e| format!("read at {off}: {e}"))?;
        let data = &buf[..want as usize];
        if data.iter().any(|&b| b != 0) {
            f.seek(std::io::SeekFrom::Start(off)).await.map_err(|e| e.to_string())?;
            f.write_all(data).await.map_err(|e| format!("{}: {e}", out.display()))?;
            written += want;
        }
        off += want;
    }
    f.set_len(size).await.map_err(|e| format!("{}: {e}", out.display()))?;
    f.sync_all().await.map_err(|e| format!("{}: {e}", out.display()))?;
    Ok(written)
}

/// Whether the clone `name` may be made: refused when the slab has one.
pub async fn clone_name_free(vm: &Arc<VmLock>, name: &str) -> Result<(), String> {
    if vm.lock().await.find_volume(name).await.is_some() {
        return Err(format!("the slab already holds a volume named {name}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_golden_is_named_by_its_file_and_content() {
        let sha = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(golden_name(Path::new("/x/fedora.img"), sha), "golden-fedora-0123456789abcdef");
        assert_eq!(golden_name(Path::new("base layer.qcow2"), sha), "golden-base-layer-0123456789abcdef");
        assert_eq!(golden_name(Path::new("/x/"), sha), "golden-x-0123456789abcdef");
    }
}
