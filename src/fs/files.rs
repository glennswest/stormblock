//! Files inside a volume, without mounting it.
//!
//! [`fio_ext4`] walks and edits an ext2/3/4 filesystem in userspace, so the
//! engine can put content into a volume it is already holding: no kernel
//! mount, no loop device, no attach, and nothing that requires the volume to
//! be exported first. Every write keeps in step the things a check looks at —
//! bitmaps, group counts, superblock totals and every metadata checksum.
//!
//! What this is for:
//!
//! - **Templates that ship content.** A skeleton `/etc`, a kernel cmdline, the
//!   `boot.toml` the initramfs reads — written once into the template, then
//!   inherited by every clone for free.
//! - **Repairing or inspecting a volume nobody can mount.** RouterOS cannot
//!   mount a volume read-write to fix one file; the engine can.
//!
//! Writing *image* content — tar streams, whiteouts, layer digests — stays
//! with the consumer that owns the image. This is for the handful of files a
//! storage layer legitimately knows about.

use std::sync::Arc;

use fio_ext4::Volume;

use crate::drive::BlockDevice;

use super::ext4::VolumeDevice;

/// One file to place into a filesystem.
#[derive(Debug, Clone)]
pub struct SeedFile {
    /// Absolute path inside the filesystem. Parent directories are created.
    pub path: String,
    pub contents: Vec<u8>,
}

impl SeedFile {
    pub fn new(path: impl Into<String>, contents: impl Into<Vec<u8>>) -> Self {
        SeedFile { path: path.into(), contents: contents.into() }
    }
}

fn parent_of(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => None,
        Some(i) => Some(&trimmed[..i]),
    }
}

/// Write files into a volume's filesystem.
///
/// Opens the filesystem once for the whole batch — a template with twenty
/// files costs one open and one flush, not twenty.
pub async fn write_files(dev: &Arc<dyn BlockDevice>, files: &[SeedFile]) -> anyhow::Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let mut vol = Volume::open(VolumeDevice::opaque(dev.clone()))
        .await
        .map_err(|e| anyhow::anyhow!("opening the filesystem: {e}"))?;

    for f in files {
        if !f.path.starts_with('/') {
            anyhow::bail!("path {:?} must be absolute", f.path);
        }
        if let Some(parent) = parent_of(&f.path) {
            vol.mkdir_all(parent)
                .await
                .map_err(|e| anyhow::anyhow!("creating {parent}: {e}"))?;
        }
        vol.write(&f.path, &f.contents)
            .await
            .map_err(|e| anyhow::anyhow!("writing {}: {e}", f.path))?;
    }

    vol.flush()
        .await
        .map_err(|e| anyhow::anyhow!("flushing the filesystem: {e}"))?;
    drop(vol);
    dev.flush()
        .await
        .map_err(|e| anyhow::anyhow!("flushing the volume: {e}"))?;
    Ok(())
}

/// Read one file out of a volume's filesystem.
pub async fn read_file(dev: &Arc<dyn BlockDevice>, path: &str) -> anyhow::Result<Vec<u8>> {
    let vol = Volume::open(VolumeDevice::opaque(dev.clone()))
        .await
        .map_err(|e| anyhow::anyhow!("opening the filesystem: {e}"))?;
    vol.read(path)
        .await
        .map_err(|e| anyhow::anyhow!("reading {path}: {e}"))
}

/// One entry in a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// List a directory inside a volume's filesystem.
pub async fn list_dir(dev: &Arc<dyn BlockDevice>, path: &str) -> anyhow::Result<Vec<DirEntry>> {
    let vol = Volume::open(VolumeDevice::opaque(dev.clone()))
        .await
        .map_err(|e| anyhow::anyhow!("opening the filesystem: {e}"))?;
    let entries = vol
        .read_dir(path)
        .await
        .map_err(|e| anyhow::anyhow!("listing {path}: {e}"))?;

    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        if e.name == "." || e.name == ".." {
            continue;
        }
        let full = if path.ends_with('/') {
            format!("{path}{}", e.name)
        } else {
            format!("{path}/{}", e.name)
        };
        let stat = vol
            .stat(&full)
            .await
            .map_err(|err| anyhow::anyhow!("stat {full}: {err}"))?;
        out.push(DirEntry {
            name: e.name,
            is_dir: stat.is_dir(),
            size: stat.size,
        });
    }
    Ok(out)
}

/// Remove one file from a volume's filesystem.
///
/// A path that is not there is success: the caller wanted it gone, and it is.
/// Anything else — a path that is a directory, a filesystem that will not open
/// — is an error, because those are not the same thing as "already removed".
pub async fn remove_file(dev: &Arc<dyn BlockDevice>, path: &str) -> anyhow::Result<()> {
    if !exists(dev, path).await? {
        return Ok(());
    }
    let mut vol = Volume::open(VolumeDevice::opaque(dev.clone()))
        .await
        .map_err(|e| anyhow::anyhow!("opening the filesystem: {e}"))?;
    vol.unlink(path)
        .await
        .map_err(|e| anyhow::anyhow!("removing {path}: {e}"))?;
    vol.flush()
        .await
        .map_err(|e| anyhow::anyhow!("flushing the filesystem: {e}"))?;
    drop(vol);
    dev.flush()
        .await
        .map_err(|e| anyhow::anyhow!("flushing the volume: {e}"))?;
    Ok(())
}

/// Whether a path exists inside a volume's filesystem.
pub async fn exists(dev: &Arc<dyn BlockDevice>, path: &str) -> anyhow::Result<bool> {
    let vol = Volume::open(VolumeDevice::opaque(dev.clone()))
        .await
        .map_err(|e| anyhow::anyhow!("opening the filesystem: {e}"))?;
    vol.exists(path)
        .await
        .map_err(|e| anyhow::anyhow!("looking up {path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    use crate::drive::filedev::FileDevice;
    use crate::fs::ext4;

    async fn formatted(size: u64) -> (Arc<dyn BlockDevice>, String) {
        let dir = std::env::temp_dir().join("stormblock-fsfiles-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.img", Uuid::new_v4().simple()));
        let p = path.to_str().unwrap().to_string();
        let dev: Arc<dyn BlockDevice> =
            Arc::new(FileDevice::open_with_capacity(&p, size).await.unwrap());
        ext4::format(&dev, &ext4::Ext4Params::default()).await.unwrap();
        (dev, p)
    }

    #[tokio::test]
    async fn files_go_in_and_come_back_out() {
        let (dev, path) = formatted(128 * 1024 * 1024).await;

        write_files(
            &dev,
            &[
                SeedFile::new("/etc/hostname", "router\n"),
                SeedFile::new("/etc/conf.d/net", "dhcp\n"),
                SeedFile::new("/boot.toml", "[boot]\nvolume = \"root\"\n"),
            ],
        )
        .await
        .unwrap();

        assert_eq!(read_file(&dev, "/etc/hostname").await.unwrap(), b"router\n");
        assert_eq!(read_file(&dev, "/etc/conf.d/net").await.unwrap(), b"dhcp\n");
        assert!(exists(&dev, "/boot.toml").await.unwrap());
        assert!(!exists(&dev, "/nope").await.unwrap());

        let etc = list_dir(&dev, "/etc").await.unwrap();
        assert!(etc.iter().any(|e| e.name == "hostname" && !e.is_dir && e.size == 7));
        assert!(etc.iter().any(|e| e.name == "conf.d" && e.is_dir));
        // The listing never includes the dots — a caller enumerating a seeded
        // directory should not have to filter them.
        assert!(!etc.iter().any(|e| e.name == "." || e.name == ".."));

        // Writing content must leave the filesystem checkable — bitmaps, group
        // counts and every checksum in step.
        let report = ext4::check(&dev).await.unwrap();
        assert!(report.is_clean(), "seeded filesystem: {:?}", report.problems);

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn a_file_larger_than_one_block_survives_intact() {
        let (dev, path) = formatted(128 * 1024 * 1024).await;
        let payload: Vec<u8> = (0..300_000).map(|i| (i % 251) as u8).collect();
        write_files(&dev, &[SeedFile::new("/big.bin", payload.clone())])
            .await
            .unwrap();
        assert_eq!(read_file(&dev, "/big.bin").await.unwrap(), payload);
        assert!(ext4::check(&dev).await.unwrap().is_clean());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn relative_paths_and_missing_files_are_refused() {
        let (dev, path) = formatted(64 * 1024 * 1024).await;
        assert!(write_files(&dev, &[SeedFile::new("etc/hostname", "x")]).await.is_err());
        assert!(read_file(&dev, "/absent").await.is_err());
        // The refusal must not have left the filesystem damaged.
        assert!(ext4::check(&dev).await.unwrap().is_clean());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn parents_are_derived_the_way_a_shell_would() {
        assert_eq!(parent_of("/etc/conf.d/net"), Some("/etc/conf.d"));
        assert_eq!(parent_of("/etc/hostname"), Some("/etc"));
        assert_eq!(parent_of("/boot.toml"), None);
        assert_eq!(parent_of("/"), None);
    }
}
