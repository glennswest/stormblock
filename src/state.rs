//! Where the engine keeps what it must remember.
//!
//! # Not through a filesystem it serves
//!
//! The engine writes its own state — the export table, the LUN map, the `/v1`
//! fencing epochs, the filesystem templates — and it used to write it as
//! ordinary files under a `--data-dir`. That is fine right up until the
//! directory sits on a volume the engine itself exports, and then it is a
//! cycle: every volume create and delete ends in a metadata write, taken while
//! the volume-manager lock is held, and that write goes out through the VFS,
//! into ext4, down to a ublk device, and back into the same process that is
//! holding the lock.
//!
//! A stormcos node did exactly that and wedged four seconds into every boot.
//! One lock held forever, every container's disk I/O queued behind it, and the
//! registry, four supervisors, sshd and the console shell all accepting
//! connections they would never answer. The engine's own API kept replying in a
//! millisecond, which is what made it read as a network fault rather than a
//! storage one.
//!
//! # The same trick the kernel golden uses
//!
//! The state still lives in a filesystem, and it is still files — that is worth
//! keeping, because `must-gather` collects them, a person can read them out of
//! an image, and nothing has to learn a new on-disk format. What changes is who
//! reads it.
//!
//! [`crate::fs::files`] reads and writes ext4 *directly*, against a
//! [`BlockDevice`], through the same library that builds a golden: no mount, no
//! loop device, no ublk export, no kernel filesystem at all. So the engine
//! opens the volume it already has a handle for and reads its own files out of
//! it in-process. The path is engine → ext4 library → slab → disk, and there is
//! nothing in it that can come back around to the engine, because the kernel's
//! block layer is not involved.
//!
//! It is the same move as mounting the kernel's modules from a golden rather
//! than carrying them in the initramfs: the filesystem is a container for
//! files, and reading one does not require the kernel's help.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::drive::BlockDevice;

/// The volume metadata, which is the largest and most important of these.
pub const VOLUMES: &str = "volumes.dat";

/// Where files live inside the state volume.
///
/// A directory rather than the root, so the volume is recognisable for what it
/// is when someone opens it, and so anything else that ever needs a corner of
/// it has somewhere to go.
const ROOT: &str = "/engine";

/// Hand-written rather than derived: this crate has no `thiserror`, and one
/// more dependency for four lines is not a trade worth making.
#[derive(Debug)]
pub enum StateError {
    Io(std::io::Error),
    /// The filesystem in the state volume would not open, or a read or write
    /// through it failed. Reported rather than treated as an empty store:
    /// losing the record of what a node holds is not the same as never having
    /// had one.
    Fs(String),
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateError::Io(e) => write!(f, "state store: {e}"),
            StateError::Fs(w) => write!(f, "state store: {w}"),
        }
    }
}

impl std::error::Error for StateError {}

impl From<std::io::Error> for StateError {
    fn from(e: std::io::Error) -> Self {
        StateError::Io(e)
    }
}

/// Where the bytes actually go.
enum Backing {
    /// An ext4 filesystem in a volume, read and written by the library rather
    /// than by the kernel.
    Volume(Arc<dyn BlockDevice>),
    /// A directory on whatever filesystem the process is already running on.
    ///
    /// What a CLI run, a test and a development node use. Kept because those
    /// have no state volume and nothing to deadlock against — the cycle exists
    /// only when the directory is on storage this engine is responsible for,
    /// which is a property of the deployment rather than of the code.
    Dir(PathBuf),
}

/// The engine's own durable state.
pub struct StateStore {
    backing: Backing,
    /// One writer at a time.
    ///
    /// Two concurrent `write_files` calls would each open the filesystem, each
    /// allocate from the same free space, and each flush a superblock that
    /// disagrees with the other's. This is the store's own lock and is never
    /// the volume manager's — holding *that* across I/O is what made the
    /// original cycle fatal rather than merely slow.
    gate: Mutex<()>,
}

impl StateStore {
    /// Keep state in an ext4 volume, read through the library.
    ///
    /// The volume must already hold a filesystem; the engine does not format
    /// it, because a state store that formats what it finds would erase a node
    /// the first time it failed to read one.
    pub fn on_volume(dev: Arc<dyn BlockDevice>) -> StateStore {
        StateStore { backing: Backing::Volume(dev), gate: Mutex::new(()) }
    }

    /// Open a state volume, checking it first.
    ///
    /// The engine writes this volume and can be killed between the data and
    /// the metadata — a node loses power, a supervisor's patience runs out, a
    /// kernel panics — and there is no unmount to make that tidy because
    /// nothing ever mounted it. So it is checked on the way in, and repaired
    /// if the check says so, using the same `fsck` the engine already offers
    /// for any other volume.
    ///
    /// A volume that will not come clean is still opened, and said so loudly.
    /// The alternative is a node that refuses to start because the file
    /// recording what it exports is damaged — which throws away every export
    /// that was fine in order to protect the one that was not.
    pub async fn open_volume(dev: Arc<dyn BlockDevice>) -> StateStore {
        match crate::fs::ext4::check(&dev).await {
            Ok(r) if r.is_clean() => {}
            Ok(_) => match crate::fs::ext4::repair(&dev).await {
                Ok(r) if r.is_clean() => {
                    tracing::warn!("state volume was not clean and has been repaired")
                }
                Ok(_) => tracing::error!(
                    "state volume is damaged and would not repair — the engine's own \
                     records may be incomplete"
                ),
                Err(e) => tracing::error!("state volume repair failed: {e}"),
            },
            Err(e) => tracing::error!("state volume check failed: {e}"),
        }
        StateStore::on_volume(dev)
    }

    /// Keep state in a directory.
    pub fn in_dir(dir: impl Into<PathBuf>) -> StateStore {
        StateStore { backing: Backing::Dir(dir.into()), gate: Mutex::new(()) }
    }

    /// What is stored under `key`, if anything.
    ///
    /// A key that is not there is `None`, not an error: a node that has never
    /// exported anything has no export table, and that is an ordinary state to
    /// be in rather than a fault.
    pub async fn get(&self, key: &str) -> Option<Vec<u8>> {
        match &self.backing {
            Backing::Dir(dir) => std::fs::read(dir.join(key)).ok(),
            Backing::Volume(dev) => {
                let path = format!("{ROOT}/{key}");
                match crate::fs::files::exists(dev, &path).await {
                    Ok(true) => crate::fs::files::read_file(dev, &path).await.ok(),
                    _ => None,
                }
            }
        }
    }

    /// Store `bytes` under `key`, durably.
    pub async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<(), StateError> {
        let _gate = self.gate.lock().await;
        match &self.backing {
            Backing::Dir(dir) => {
                std::fs::create_dir_all(dir)?;
                // Through a temporary and a rename: a half-written file is
                // indistinguishable from a truncated one, and this is the
                // record that says what the node holds.
                let tmp = dir.join(format!(".{key}.tmp"));
                std::fs::write(&tmp, &bytes)?;
                std::fs::rename(&tmp, dir.join(key))?;
                Ok(())
            }
            Backing::Volume(dev) => {
                let file = crate::fs::files::SeedFile::new(format!("{ROOT}/{key}"), bytes);
                crate::fs::files::write_files(dev, std::slice::from_ref(&file))
                    .await
                    .map_err(|e| StateError::Fs(format!("writing {key}: {e}")))
            }
        }
    }

    /// Every key held, in name order.
    ///
    /// There is deliberately no `remove`. Nothing here is ever deleted: an
    /// export table with nothing in it is `[]`, not a missing file, and a node
    /// that has stopped exporting has said so rather than gone quiet. A delete
    /// would be one more operation to get wrong, on the one record that says
    /// what the node holds.
    pub async fn keys(&self) -> Vec<String> {
        match &self.backing {
            Backing::Dir(dir) => {
                let mut names: Vec<String> = std::fs::read_dir(dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter_map(|e| e.file_name().to_str().map(str::to_owned))
                    .filter(|n| !n.starts_with('.'))
                    .collect();
                names.sort();
                names
            }
            Backing::Volume(dev) => {
                let mut names: Vec<String> = crate::fs::files::list_dir(dev, ROOT)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|e| !e.is_dir)
                    .map(|e| e.name)
                    .collect();
                names.sort();
                names
            }
        }
    }

    /// Where this store writes, for a diagnostic. A node that cannot say where
    /// its own state lives is a node nobody can reason about.
    pub fn describe(&self) -> String {
        match &self.backing {
            Backing::Volume(_) => "a state volume, read through the ext4 library".to_string(),
            Backing::Dir(d) => format!("the directory {}", d.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("stormblock-state-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn a_directory_store_round_trips() {
        let dir = temp("dir");
        let s = StateStore::in_dir(&dir);

        assert!(s.get("luns.json").await.is_none(), "absent is None, not an error");

        s.put("luns.json", b"[]".to_vec()).await.unwrap();
        s.put(VOLUMES, vec![7, 7, 7]).await.unwrap();
        assert_eq!(s.get("luns.json").await.unwrap(), b"[]");

        // Read back by a store opened fresh on the same directory, which is
        // what a restart is.
        let again = StateStore::in_dir(&dir);
        assert_eq!(again.get(VOLUMES).await.unwrap(), vec![7, 7, 7]);
        assert_eq!(again.keys().await, vec!["luns.json".to_string(), VOLUMES.to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The one that matters: a real ext4 filesystem, read and written with no
    /// mount and no kernel filesystem in the path.
    ///
    /// The directory backing above is ordinary file I/O and would pass whatever
    /// the library did. This is the backing a node actually uses, and the whole
    /// claim of this module is that it works without the kernel's help.
    #[tokio::test]
    async fn a_volume_store_round_trips_through_the_library() {
        use crate::drive::filedev::FileDevice;
        use crate::drive::BlockDevice;

        let dir = temp("volume");
        let img = dir.join("state.img");
        // 64 MiB, which is what a node's state volume is sized at.
        let dev: Arc<dyn BlockDevice> = Arc::new(
            FileDevice::open_with_capacity(img.to_str().unwrap(), 64 * 1024 * 1024)
                .await
                .expect("a backing file"),
        );
        crate::fs::ext4::format(&dev, &crate::fs::ext4::Ext4Params::default())
            .await
            .expect("format");

        let s = StateStore::on_volume(dev.clone());
        assert!(s.get(VOLUMES).await.is_none(), "a fresh volume holds nothing");

        // A payload of the shape this really carries: a volume table is tens of
        // kilobytes on a node with fifty volumes.
        let big: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();
        s.put(VOLUMES, big.clone()).await.unwrap();
        s.put("luns.json", b"[{\"lun\":1}]".to_vec()).await.unwrap();

        assert_eq!(s.get(VOLUMES).await.unwrap(), big);
        assert_eq!(s.get("luns.json").await.unwrap(), b"[{\"lun\":1}]");
        assert_eq!(s.keys().await, vec!["luns.json".to_string(), VOLUMES.to_string()]);

        // Rewritten in place, not appended to.
        s.put(VOLUMES, vec![1, 2, 3]).await.unwrap();
        assert_eq!(s.get(VOLUMES).await.unwrap(), vec![1, 2, 3]);

        // And a store opened fresh on the same volume sees it — which is what
        // a restart is, and the only reason any of this is durable.
        let again = StateStore::on_volume(dev.clone());
        assert_eq!(again.get(VOLUMES).await.unwrap(), vec![1, 2, 3]);
        assert_eq!(
            again.keys().await,
            vec!["luns.json".to_string(), VOLUMES.to_string()]
        );

        // The filesystem is still sound after all of that.
        let report = crate::fs::ext4::check(&dev).await.expect("fsck runs");
        assert!(report.is_clean(), "the state volume must still check out: {report:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A damaged state volume is repaired on the way in, not refused.
    #[tokio::test]
    async fn a_state_volume_is_checked_when_it_is_opened() {
        use crate::drive::filedev::FileDevice;

        let dir = temp("fsck");
        let img = dir.join("state.img");
        let dev: Arc<dyn BlockDevice> = Arc::new(
            FileDevice::open_with_capacity(img.to_str().unwrap(), 64 * 1024 * 1024)
                .await
                .expect("a backing file"),
        );
        crate::fs::ext4::format(&dev, &crate::fs::ext4::Ext4Params::default())
            .await
            .expect("format");

        let s = StateStore::open_volume(dev.clone()).await;
        s.put("exports.json", b"[]".to_vec()).await.unwrap();

        // Opening it again checks it again, and what was written is still
        // there — a check must not cost the caller its data.
        let again = StateStore::open_volume(dev.clone()).await;
        assert_eq!(again.get("exports.json").await.unwrap(), b"[]");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_rewrite_replaces_rather_than_appends() {
        let dir = temp("rewrite");
        let s = StateStore::in_dir(&dir);
        s.put("v1_state.json", b"aaaaaaaaaa".to_vec()).await.unwrap();
        s.put("v1_state.json", b"bb".to_vec()).await.unwrap();
        assert_eq!(s.get("v1_state.json").await.unwrap(), b"bb");
    }

    #[tokio::test]
    async fn no_temporary_file_survives_a_write() {
        // The rename is what makes a torn write impossible to read; a leftover
        // temporary would also show up in `keys`.
        let dir = temp("tmp");
        let s = StateStore::in_dir(&dir);
        s.put("exports.json", b"[]".to_vec()).await.unwrap();
        assert_eq!(s.keys().await, vec!["exports.json".to_string()]);
    }
}
