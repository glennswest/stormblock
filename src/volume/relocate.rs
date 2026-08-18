//! First-class volume move — re-home or shrink a volume without losing it (#20).
//!
//! Two different things get called "move", and only one of them existed.
//! [`crate::placement::migrate_extent`] relocates a volume's *extents* between
//! slabs and tiers with the volume online; that is physical placement and is
//! not this. This is the **volume-level** move: a whole volume relocated to a
//! different size or pool, ending with the consumer pointed at a new volume.
//!
//! # Why this cannot be a resize
//!
//! [`crate::volume::VolumeManager::resize_volume`] grows only, and for good
//! reason (#19): shrinking frees every extent past the new end, and **xfs
//! cannot shrink at all**. For a mounted `/var` that silently destroys live
//! data. The only safe form of "make this smaller" is to build a new, smaller
//! filesystem and copy the *contents* into it — which is why the copy here is
//! at the filesystem level and not the block level. A block-level clone would
//! faithfully reproduce the original size, which is the thing being escaped.
//!
//! # Offline, deliberately
//!
//! The volume is unmounted for the duration. That makes the source static, so
//! there is no delta to chase and correctness is obvious. A lower-downtime
//! variant — freeze, snapshot, copy the snapshot while the volume still
//! serves, then a brief final sync — is the classic live-migration shape and
//! belongs on top of this, not instead of it: chasing a delta on a live
//! filesystem is where this gets genuinely hard, and a shrink that is simple
//! and correct beats one that is fast and subtly wrong.
//!
//! The caller enforces "offline". Export registries live in the management
//! layer, so [`crate::mgmt::api`] refuses a move on an exported volume the same
//! way it refuses a resize.
//!
//! # Nothing destructive before verification
//!
//! The order is the whole point, because steps 4–6 are the ones people skip
//! when they script this by hand, and they are the ones that matter:
//!
//! 1. **snapshot the source** — a copy-on-write clone, so it costs metadata.
//!    This is both the static copy source and the rollback point, and it makes
//!    a shrink that goes wrong recoverable rather than a restore-from-backup.
//! 2. create the target volume at the new size
//! 3. make a filesystem on it, matching the source's profile
//! 4. copy the *contents* across, streaming — a 64 GiB volume holding 2 GiB
//!    moves 2 GiB
//! 5. **verify** — fsck the target, and check nothing went missing
//! 6. hand both volumes back and stop
//!
//! The source is still there at the end. [`commit`] deletes it, and only
//! after the caller has repointed whatever was using it. [`abort`] deletes the
//! target instead. A failure anywhere before [`commit`] costs the copy and
//! nothing else.
//!
//! # Not resumable mid-copy
//!
//! An interrupted move is *restartable*, not resumable: the target is
//! discarded and the copy runs again. Resuming a tar stream partway would mean
//! trusting a half-written filesystem to say truthfully what it already has,
//! and the source survives regardless, so a re-copy costs time rather than
//! data. Said plainly here because the issue asked for resumable and this is
//! not that.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::drive::BlockDevice;
use crate::fs::ext4;
use crate::volume::{VolumeId, VolumeManager};

/// How much of the archive stream is in flight between the two filesystems.
///
/// The copy pipes one volume's `pack_tar` into another's `unpack_tar` with no
/// scratch file and no whole-archive buffer, so this is the only memory the
/// move costs regardless of how much data it carries.
const PIPE_CHUNKS: usize = 8;

pub type Result<T> = std::result::Result<T, MoveError>;

#[derive(Debug)]
pub enum MoveError {
    NotFound(String),
    /// The move cannot start in this state (source busy, target name taken).
    Conflict(String),
    Invalid(String),
    /// The copy or the verification failed. The source is untouched.
    Failed(String),
}

impl std::fmt::Display for MoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MoveError::NotFound(m) => write!(f, "{m}"),
            MoveError::Conflict(m) => write!(f, "{m}"),
            MoveError::Invalid(m) => write!(f, "{m}"),
            MoveError::Failed(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for MoveError {}

/// What to move, and to where.
#[derive(Debug, Clone)]
pub struct MoveSpec {
    pub source: VolumeId,
    /// Name for the new volume. Must not already be taken.
    pub target_name: String,
    /// Size of the new volume. Smaller than the source is the whole point;
    /// larger is allowed but a plain resize is cheaper and stays online (#19).
    pub target_size_bytes: u64,
    /// Check the target's filesystem before handing it back. On by default:
    /// the point of this operation is that nothing destructive happens until
    /// the copy is proven good, and an unverified copy proves nothing.
    pub verify: bool,
}

impl MoveSpec {
    pub fn new(source: VolumeId, target_name: impl Into<String>, target_size_bytes: u64) -> Self {
        MoveSpec {
            source,
            target_name: target_name.into(),
            target_size_bytes,
            verify: true,
        }
    }
}

/// Where a move has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveState {
    /// Copied and verified. The source is intact and both volumes exist —
    /// repoint the consumer, then [`commit`].
    ReadyToCommit,
    /// The source is gone. The move is done.
    Committed,
    /// The target is gone. The source never moved.
    Aborted,
}

/// A move in progress or finished, persisted so it survives a restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeMove {
    pub id: Uuid,
    pub source: Uuid,
    pub target: Uuid,
    /// The copy source, and the way back. Kept until commit.
    pub rollback_snapshot: Uuid,
    pub source_size_bytes: u64,
    pub target_size_bytes: u64,
    pub state: MoveState,
    pub files_copied: u64,
    pub bytes_copied: u64,
    pub verified: bool,
}

impl VolumeMove {
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "source_volume_id": self.source,
            "target_volume_id": self.target,
            "rollback_snapshot_id": self.rollback_snapshot,
            "source_size_bytes": self.source_size_bytes,
            "target_size_bytes": self.target_size_bytes,
            "state": self.state,
            "files_copied": self.files_copied,
            "bytes_copied": self.bytes_copied,
            "verified": self.verified,
        })
    }
}

/// Perform the copy half of a move: everything up to, and not including,
/// anything destructive.
///
/// On return the source volume is untouched, a rollback snapshot of it exists,
/// and the target holds a verified copy of its contents at the new size. The
/// caller repoints its consumer and then calls [`commit`].
pub async fn start(
    vm: &tokio::sync::Mutex<VolumeManager>,
    spec: &MoveSpec,
) -> Result<VolumeMove> {
    if spec.target_name.trim().is_empty() {
        return Err(MoveError::Invalid("target name must not be empty".to_string()));
    }
    if spec.target_size_bytes == 0 {
        return Err(MoveError::Invalid("target size must be > 0".to_string()));
    }

    let (source_dev, source_size) = {
        let m = vm.lock().await;
        let dev = m
            .get_volume(&spec.source)
            .ok_or_else(|| MoveError::NotFound(format!("volume {} not found", spec.source)))?;
        let size = dev.capacity_bytes();
        (dev, size)
    };

    // Read the source's filesystem before touching anything. A volume with no
    // readable filesystem cannot be moved this way — the copy is at the
    // filesystem level, and there is nothing to copy from.
    let layout = ext4::read_layout(&source_dev).await.map_err(|e| {
        MoveError::Invalid(format!(
            "volume {} has no readable filesystem to move ({e}) — a volume-level move copies \
             file contents, which is what lets it shrink",
            spec.source
        ))
    })?;
    if !layout.clean {
        return Err(MoveError::Conflict(format!(
            "the filesystem on {} is not cleanly unmounted — moving it now would copy a \
             filesystem that still has recovery pending",
            spec.source
        )));
    }
    drop(source_dev);

    // 1. Snapshot: the static copy source, and the way back.
    let rollback = {
        let mut m = vm.lock().await;
        m.create_snapshot(spec.source, &format!("premove-{}", spec.target_name))
            .await
            .map_err(|e| MoveError::Failed(format!("snapshotting the source: {e}")))?
    };

    // From here, anything that fails takes the volumes it made with it.
    let outcome = copy_into_new_volume(vm, spec, rollback, &layout).await;
    let (target, files, bytes, verified) = match outcome {
        Ok(v) => v,
        Err(e) => {
            let mut m = vm.lock().await;
            if let Err(e) = m.delete_volume(rollback).await {
                tracing::warn!("move rollback: snapshot {rollback} not deleted: {e}");
            }
            return Err(e);
        }
    };

    let mv = VolumeMove {
        id: Uuid::new_v4(),
        source: spec.source.0,
        target: target.0,
        rollback_snapshot: rollback.0,
        source_size_bytes: source_size,
        target_size_bytes: spec.target_size_bytes,
        state: MoveState::ReadyToCommit,
        files_copied: files,
        bytes_copied: bytes,
        verified,
    };
    tracing::info!(
        source = %spec.source,
        target = %target,
        files,
        bytes,
        "volume move copied and verified — source intact until commit"
    );
    Ok(mv)
}

/// Create the target, format it, copy into it, and check it.
async fn copy_into_new_volume(
    vm: &tokio::sync::Mutex<VolumeManager>,
    spec: &MoveSpec,
    from: VolumeId,
    layout: &ext4::Ext4Layout,
) -> Result<(VolumeId, u64, u64, bool)> {
    let target = {
        let mut m = vm.lock().await;
        m.create_volume_any(&spec.target_name, spec.target_size_bytes)
            .await
            .map_err(|e| MoveError::Failed(format!("creating the target volume: {e}")))?
    };

    let result = fill_target(vm, spec, from, target, layout).await;
    if result.is_err() {
        let mut m = vm.lock().await;
        if let Err(e) = m.delete_volume(target).await {
            tracing::warn!("move rollback: target {target} not deleted: {e}");
        }
    }
    result.map(|(files, bytes, verified)| (target, files, bytes, verified))
}

async fn fill_target(
    vm: &tokio::sync::Mutex<VolumeManager>,
    spec: &MoveSpec,
    from: VolumeId,
    target: VolumeId,
    layout: &ext4::Ext4Layout,
) -> Result<(u64, u64, bool)> {
    let (src_dev, dst_dev) = {
        let m = vm.lock().await;
        let s = m
            .get_volume(&from)
            .ok_or_else(|| MoveError::Failed("the move snapshot vanished".to_string()))?;
        let d = m
            .get_volume(&target)
            .ok_or_else(|| MoveError::Failed("the target volume vanished".to_string()))?;
        (s, d)
    };

    // The target gets the source's shape, not the defaults: journal, feature
    // set and label carry over, so what comes back is the same kind of
    // filesystem at a different size. Its UUID is *not* carried over — two
    // filesystems with one UUID collide on mount-by-UUID the moment both are
    // attached, and during the retention window both of these exist.
    let params = ext4::Ext4Params {
        profile: match layout.profile_name() {
            "ext2" => ext4::FsProfile::Ext2,
            "ext3" => ext4::FsProfile::Ext3,
            _ => ext4::FsProfile::Ext4,
        },
        label: layout.label.clone(),
        uuid: Uuid::new_v4(),
        journal: Some(layout.has_journal),
        block_size: Some(layout.block_size as u32),
        assume_blank: true,
        ..Default::default()
    };
    // No lock held across the format: it is I/O against one volume, and a move
    // must not stall every other volume operation for its duration.
    ext4::format(&dst_dev, &params)
        .await
        .map_err(|e| MoveError::Failed(format!("making a filesystem on the target: {e}")))?;

    let (files, bytes) = copy_contents(&src_dev, &dst_dev).await?;

    // Verify before the caller is told anything is ready. This is the step
    // that makes "never destructive before verification" mean something.
    let verified = if spec.verify {
        let report = ext4::check(&dst_dev)
            .await
            .map_err(|e| MoveError::Failed(format!("checking the copy: {e}")))?;
        if !report.is_clean() {
            let why = report
                .problems
                .iter()
                .map(|p| format!("{}: {}", p.code, p.message))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(MoveError::Failed(format!(
                "the copy did not check out and was discarded: {why}"
            )));
        }
        true
    } else {
        false
    };

    Ok((files, bytes, verified))
}

/// Stream one filesystem's contents into another.
///
/// Piped rather than spooled: the source's `pack_tar` writes into a bounded
/// channel that the target's `unpack_tar` reads from, with both halves driven
/// by the same `join!`. No scratch file, no whole-archive buffer, and the
/// memory cost is [`PIPE_CHUNKS`] chunks whether the volume holds 2 GiB or 200.
///
/// Going through tar rather than walking the tree by hand is what preserves the
/// things a hand-rolled copy loses: modes, ownership, timestamps, symlinks,
/// hard links, device nodes and extended attributes — SELinux labels among
/// them, which a rootfs stops booting without.
async fn copy_contents(
    src: &Arc<dyn BlockDevice>,
    dst: &Arc<dyn BlockDevice>,
) -> Result<(u64, u64)> {
    use fio_ext4::Volume;

    let source = Volume::open(ext4::VolumeDevice::opaque(src.clone()))
        .await
        .map_err(|e| MoveError::Failed(format!("opening the source filesystem: {e}")))?;
    let mut sink_vol = Volume::open(ext4::VolumeDevice::opaque(dst.clone()))
        .await
        .map_err(|e| MoveError::Failed(format!("opening the target filesystem: {e}")))?;

    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(PIPE_CHUNKS);

    let pack = async {
        let report = source.pack_tar_to(ChannelSink { tx }, "/").await;
        report.map_err(|e| MoveError::Failed(format!("reading the source filesystem: {e}")))
    };
    let unpack = async {
        let report = sink_vol.unpack_tar_from(ChannelSource { rx, rest: Vec::new(), pos: 0 }).await;
        match report {
            Ok(r) => {
                sink_vol
                    .flush()
                    .await
                    .map_err(|e| MoveError::Failed(format!("flushing the target: {e}")))?;
                Ok(r)
            }
            Err(e) => Err(MoveError::Failed(format!("writing the target filesystem: {e}"))),
        }
    };

    // Both halves on one task: the channel only makes progress because `join!`
    // polls the reader whenever the writer is blocked on a full buffer, and
    // vice versa.
    let (packed, unpacked) = tokio::join!(pack, unpack);
    let packed = packed?;
    let unpacked = unpacked?;

    // The two ends must agree about everything that crossed. The counts are
    // arrived at independently — one walks the source, one writes the target —
    // so a mismatch in any of them is a copy that lost something, which is
    // exactly the failure this whole operation exists not to do silently.
    // Comparing every category rather than a total is what catches the
    // interesting losses: xattrs are where SELinux labels live, and hard links
    // silently becoming separate files changes what the filesystem holds
    // without changing how many names are in it.
    let mismatches: Vec<String> = [
        ("files", packed.files, unpacked.files),
        ("directories", packed.directories, unpacked.directories),
        ("symlinks", packed.symlinks, unpacked.symlinks),
        ("hard links", packed.hard_links, unpacked.hard_links),
        ("devices", packed.devices, unpacked.devices),
        ("xattrs", packed.xattrs, unpacked.xattrs),
        ("bytes", packed.bytes, unpacked.bytes),
    ]
    .into_iter()
    .filter(|(_, from, to)| from != to)
    .map(|(what, from, to)| format!("{what}: {from} read, {to} written"))
    .collect();
    if !mismatches.is_empty() {
        return Err(MoveError::Failed(format!(
            "the copy did not carry everything across — {}",
            mismatches.join("; ")
        )));
    }

    let entries = unpacked.files
        + unpacked.directories
        + unpacked.symlinks
        + unpacked.hard_links
        + unpacked.devices;
    Ok((entries, unpacked.bytes))
}

/// A [`fio_ext4::tar::Sink`] that hands chunks to the reader half.
struct ChannelSink {
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

impl fio_ext4::tar::Sink for ChannelSink {
    async fn write_all(&mut self, buf: &[u8]) -> fio_ext4::Result<()> {
        self.tx
            .send(buf.to_vec())
            .await
            .map_err(|_| {
                fio_ext4::Error::Io(std::io::Error::other(
                    "the move copy target stopped reading",
                ))
            })
    }
}

/// A [`fio_ext4::tar::Source`] over the writer half.
struct ChannelSource {
    rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    rest: Vec<u8>,
    pos: usize,
}

impl fio_ext4::tar::Source for ChannelSource {
    async fn read(&mut self, buf: &mut [u8]) -> fio_ext4::Result<usize> {
        if self.pos >= self.rest.len() {
            match self.rx.recv().await {
                // Closed means the packer is done, which is end of stream —
                // and must keep meaning that on every later call.
                None => return Ok(0),
                Some(chunk) => {
                    self.rest = chunk;
                    self.pos = 0;
                }
            }
        }
        let n = buf.len().min(self.rest.len() - self.pos);
        buf[..n].copy_from_slice(&self.rest[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Finish a move: delete the source, and the rollback snapshot with it.
///
/// Only the caller knows whether whatever was using the source has been
/// repointed, so this is never automatic. Until it is called the source is
/// intact and the move can still be undone by pointing back at it.
pub async fn commit(
    vm: &tokio::sync::Mutex<VolumeManager>,
    mv: &mut VolumeMove,
) -> Result<()> {
    if mv.state != MoveState::ReadyToCommit {
        return Err(MoveError::Conflict(format!(
            "move {} is {:?}, not ready to commit",
            mv.id, mv.state
        )));
    }
    let mut m = vm.lock().await;
    // The target has to still be there — committing deletes the only other
    // copy of the data.
    if m.get_volume(&VolumeId(mv.target)).is_none() {
        return Err(MoveError::Conflict(format!(
            "the target volume {} is gone — refusing to delete the source",
            mv.target
        )));
    }
    for (what, id) in [("source", mv.source), ("rollback snapshot", mv.rollback_snapshot)] {
        if let Err(e) = m.delete_volume(VolumeId(id)).await {
            tracing::warn!("committing move {}: {what} {id} not deleted: {e}", mv.id);
        }
    }
    mv.state = MoveState::Committed;
    tracing::info!(move_id = %mv.id, source = %mv.source, target = %mv.target, "volume move committed");
    Ok(())
}

/// Give up on a move: delete the target and the rollback snapshot, keep the
/// source exactly as it was.
pub async fn abort(
    vm: &tokio::sync::Mutex<VolumeManager>,
    mv: &mut VolumeMove,
) -> Result<()> {
    if mv.state != MoveState::ReadyToCommit {
        return Err(MoveError::Conflict(format!(
            "move {} is {:?}, not abortable",
            mv.id, mv.state
        )));
    }
    let mut m = vm.lock().await;
    for (what, id) in [("target", mv.target), ("rollback snapshot", mv.rollback_snapshot)] {
        if let Err(e) = m.delete_volume(VolumeId(id)).await {
            tracing::warn!("aborting move {}: {what} {id} not deleted: {e}", mv.id);
        }
    }
    mv.state = MoveState::Aborted;
    tracing::info!(move_id = %mv.id, source = %mv.source, "volume move aborted — the source never moved");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::fs::files::SeedFile;
    use crate::raid::RaidArrayId;

    type VmLock = tokio::sync::Mutex<VolumeManager>;

    async fn node(bytes: u64) -> (VmLock, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("move.slab");
        let dev = FileDevice::open_with_capacity(path.to_str().unwrap(), bytes).await.unwrap();
        let mut vm = VolumeManager::new(1024 * 1024);
        vm.add_backing_device(RaidArrayId(Uuid::new_v4()), Arc::new(dev)).await;
        (tokio::sync::Mutex::new(vm), dir)
    }

    /// A formatted volume carrying a small tree, with content worth checking.
    async fn seeded(vm: &VmLock, name: &str, size: u64) -> VolumeId {
        let id = vm.lock().await.create_volume_any(name, size).await.unwrap();
        let dev = vm.lock().await.get_volume(&id).unwrap();
        ext4::format(&dev, &ext4::Ext4Params { label: "moveme".into(), ..Default::default() })
            .await
            .unwrap();
        crate::fs::files::write_files(
            &dev,
            &[
                SeedFile::new("/etc/hostname", b"storm-node".to_vec()),
                SeedFile::new("/etc/deep/nested/config.toml", b"key = 1\n".to_vec()),
                // Big enough to span blocks, so this is not just a metadata copy.
                SeedFile::new("/var/blob.bin", vec![0x7Eu8; 300 * 1024]),
            ],
        )
        .await
        .unwrap();
        id
    }

    async fn read_file(vm: &VmLock, id: VolumeId, path: &str) -> Vec<u8> {
        use fio_ext4::Volume;
        let dev = vm.lock().await.get_volume(&id).unwrap();
        let v = Volume::open(ext4::VolumeDevice::opaque(dev)).await.unwrap();
        v.read(path).await.unwrap()
    }

    /// The operation this issue is about: a volume ends up on a **smaller**
    /// one, with its contents intact — which resize can never do.
    #[tokio::test]
    async fn a_volume_moves_onto_a_smaller_one_with_its_contents() {
        let (vm, _d) = node(1024 * 1024 * 1024).await;
        let source = seeded(&vm, "var", 128 * 1024 * 1024).await;

        let spec = MoveSpec::new(source, "var-small", 48 * 1024 * 1024);
        let mut mv = start(&vm, &spec).await.expect("move");

        assert_eq!(mv.state, MoveState::ReadyToCommit);
        assert!(mv.verified, "the copy was checked before being offered");
        assert!(mv.files_copied > 0, "{mv:?}");
        assert!(
            mv.target_size_bytes < mv.source_size_bytes,
            "the point of the exercise: {} -> {}",
            mv.source_size_bytes,
            mv.target_size_bytes
        );

        // Contents crossed, byte for byte, including the multi-block file.
        let target = VolumeId(mv.target);
        assert_eq!(read_file(&vm, target, "/etc/hostname").await, b"storm-node");
        assert_eq!(read_file(&vm, target, "/etc/deep/nested/config.toml").await, b"key = 1\n");
        assert_eq!(read_file(&vm, target, "/var/blob.bin").await, vec![0x7Eu8; 300 * 1024]);

        // The filesystem is the same kind, at the new size, with its own identity.
        let dev = vm.lock().await.get_volume(&target).unwrap();
        let layout = ext4::read_layout(&dev).await.unwrap();
        assert_eq!(layout.label, "moveme", "the label carried over");
        assert!(layout.size_bytes() <= 48 * 1024 * 1024);
        let src_dev = vm.lock().await.get_volume(&source).unwrap();
        let src_layout = ext4::read_layout(&src_dev).await.unwrap();
        assert_ne!(layout.uuid, src_layout.uuid, "two filesystems must not share a UUID");

        // Nothing destructive has happened: the source is untouched and so is
        // the way back.
        assert!(vm.lock().await.get_volume(&source).is_some());
        assert_eq!(read_file(&vm, source, "/etc/hostname").await, b"storm-node");
        assert!(vm.lock().await.get_volume(&VolumeId(mv.rollback_snapshot)).is_some());

        // Commit is what finally removes it.
        commit(&vm, &mut mv).await.unwrap();
        assert_eq!(mv.state, MoveState::Committed);
        assert!(vm.lock().await.get_volume(&source).is_none());
        assert!(vm.lock().await.get_volume(&VolumeId(mv.rollback_snapshot)).is_none());
        assert!(vm.lock().await.get_volume(&target).is_some(), "the data survived");
    }

    /// Abort is the way back, and it costs the source nothing.
    #[tokio::test]
    async fn aborting_leaves_the_source_exactly_where_it_was() {
        let (vm, _d) = node(1024 * 1024 * 1024).await;
        let source = seeded(&vm, "var", 128 * 1024 * 1024).await;

        let mut mv = start(&vm, &MoveSpec::new(source, "var-small", 48 * 1024 * 1024))
            .await
            .unwrap();
        let target = VolumeId(mv.target);

        abort(&vm, &mut mv).await.unwrap();
        assert_eq!(mv.state, MoveState::Aborted);
        assert!(vm.lock().await.get_volume(&target).is_none(), "the target went");
        assert!(vm.lock().await.get_volume(&VolumeId(mv.rollback_snapshot)).is_none());

        // The source never moved, and still reads.
        assert!(vm.lock().await.get_volume(&source).is_some());
        assert_eq!(read_file(&vm, source, "/etc/hostname").await, b"storm-node");

        // And an aborted move cannot then be committed.
        assert!(matches!(commit(&vm, &mut mv).await, Err(MoveError::Conflict(_))));
    }

    /// Committing while the target is missing would delete the only copy.
    #[tokio::test]
    async fn commit_refuses_when_the_target_is_gone() {
        let (vm, _d) = node(1024 * 1024 * 1024).await;
        let source = seeded(&vm, "var", 128 * 1024 * 1024).await;
        let mut mv = start(&vm, &MoveSpec::new(source, "var-small", 48 * 1024 * 1024))
            .await
            .unwrap();

        vm.lock().await.delete_volume(VolumeId(mv.target)).await.unwrap();

        assert!(matches!(commit(&vm, &mut mv).await, Err(MoveError::Conflict(_))));
        assert!(vm.lock().await.get_volume(&source).is_some(), "the source survived the refusal");
        assert_eq!(mv.state, MoveState::ReadyToCommit);
    }

    /// A volume with no filesystem cannot be moved this way, and says why
    /// rather than producing an empty target.
    #[tokio::test]
    async fn a_volume_without_a_filesystem_is_refused_cleanly() {
        let (vm, _d) = node(512 * 1024 * 1024).await;
        let raw = vm.lock().await.create_volume_any("raw", 32 * 1024 * 1024).await.unwrap();

        let before = vm.lock().await.list_volumes().await.len();
        let err = start(&vm, &MoveSpec::new(raw, "raw-small", 16 * 1024 * 1024))
            .await
            .unwrap_err();
        assert!(matches!(err, MoveError::Invalid(_)), "{err}");
        assert!(err.to_string().contains("no readable filesystem"), "{err}");

        // Refused before anything was created: no orphan snapshot, no orphan
        // target.
        assert_eq!(vm.lock().await.list_volumes().await.len(), before);
    }

    /// A target too small for the contents fails, and takes its own volumes
    /// with it rather than leaving debris behind.
    #[tokio::test]
    async fn a_target_that_cannot_hold_the_contents_leaves_nothing_behind() {
        let (vm, _d) = node(1024 * 1024 * 1024).await;
        let source = seeded(&vm, "var", 128 * 1024 * 1024).await;
        let before: Vec<String> = vm
            .lock()
            .await
            .list_volumes()
            .await
            .into_iter()
            .map(|(_, n, _, _)| n)
            .collect();

        // Put more in the source than the target could ever hold.
        {
            let dev = vm.lock().await.get_volume(&source).unwrap();
            crate::fs::files::write_files(
                &dev,
                &[SeedFile::new("/var/big.bin", vec![0x33u8; 24 * 1024 * 1024])],
            )
            .await
            .unwrap();
        }

        let err = start(&vm, &MoveSpec::new(source, "far-too-small", 8 * 1024 * 1024))
            .await
            .unwrap_err();
        assert!(matches!(err, MoveError::Failed(_)), "{err}");

        let after: Vec<String> = vm
            .lock()
            .await
            .list_volumes()
            .await
            .into_iter()
            .map(|(_, n, _, _)| n)
            .collect();
        assert_eq!(after.len(), before.len(), "a failed move left volumes behind: {after:?}");
        assert!(vm.lock().await.get_volume(&source).is_some());
        assert_eq!(read_file(&vm, source, "/etc/hostname").await, b"storm-node");
    }

    /// Re-homing at the same size is a legitimate move too — the size is not
    /// what makes it one.
    #[tokio::test]
    async fn a_same_size_move_is_a_rehome() {
        let (vm, _d) = node(1024 * 1024 * 1024).await;
        let source = seeded(&vm, "var", 64 * 1024 * 1024).await;
        let mut mv = start(&vm, &MoveSpec::new(source, "var-elsewhere", 64 * 1024 * 1024))
            .await
            .unwrap();
        assert_eq!(mv.source_size_bytes, mv.target_size_bytes);
        assert_eq!(
            read_file(&vm, VolumeId(mv.target), "/var/blob.bin").await,
            vec![0x7Eu8; 300 * 1024]
        );
        commit(&vm, &mut mv).await.unwrap();
    }
}
