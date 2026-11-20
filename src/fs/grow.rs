//! A mounted filesystem grown to its device, online (#655).
//!
//! A volume grown through its object (`spec.sizeBytes`) is a bigger device;
//! the filesystem on it is grown by whoever mounts it. When the engine can
//! see the mount (in any mount namespace, reached through `/proc/<pid>/root`,
//! #690), it grows an ext4 itself, as `resize2fs` does on a mounted one: the kernel's
//! `EXT4_IOC_RESIZE_FS` with the new block count. Anything else is said, and
//! left to the service that mounts it (an XFS: `xfs_growfs`).

/// `_IOW('f', 16, __u64)`.
#[cfg(target_os = "linux")]
const EXT4_IOC_RESIZE_FS: libc::c_ulong = 0x4008_6610;

/// The new block count for `bytes` of device at `block` bytes a block.
pub fn blocks_for(bytes: u64, block: u64) -> u64 {
    bytes.checked_div(block).unwrap_or(0)
}

/// What a filesystem's `f_type` says it is (`statfs(2)`'s magic numbers).
pub fn kind_of_magic(magic: u64) -> &'static str {
    match magic {
        0xEF53 => "ext4",
        0x5846_5342 => "xfs",
        0x9123_683E => "btrfs",
        0x0102_1994 => "tmpfs",
        _ => "another filesystem",
    }
}

/// Grow the ext4 mounted at `mountpoint` (as its namespace shows it) to
/// `device_bytes`, opened through `reach` (#690: `/proc/<pid>/root` and the
/// mount point, so a mount in a service's own namespace is the one grown,
/// never a path that means something else here). Said in words.
#[cfg(target_os = "linux")]
pub fn grow_mounted(reach: &str, mountpoint: &str, device_bytes: u64) -> Result<String, String> {
    use std::os::fd::AsRawFd;
    let dir = std::fs::File::open(reach).map_err(|e| format!("{mountpoint}: {e}"))?;
    // SAFETY: statfs fills the struct it is given.
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(dir.as_raw_fd(), &mut fs) } != 0 {
        return Err(format!("{mountpoint}: statfs: {}", std::io::Error::last_os_error()));
    }
    let kind = kind_of_magic(fs.f_type as u64);
    if kind != "ext4" {
        return Err(format!("{kind} at {mountpoint}: grown by the service that mounts it (an XFS: xfs_growfs {mountpoint})"));
    }
    // SAFETY: statvfs fills the struct it is given.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatvfs(dir.as_raw_fd(), &mut st) } != 0 {
        return Err(format!("{mountpoint}: statvfs: {}", std::io::Error::last_os_error()));
    }
    let block = st.f_bsize as u64;
    let before = st.f_blocks as u64;
    let want = blocks_for(device_bytes, block);
    if want <= before {
        return Ok(format!("ext4 at {mountpoint} already {before} blocks of {block}"));
    }
    let n: u64 = want;
    // SAFETY: the ioctl reads one u64.
    if unsafe { libc::ioctl(dir.as_raw_fd(), EXT4_IOC_RESIZE_FS as _, &n) } != 0 {
        return Err(format!("ext4 at {mountpoint}: EXT4_IOC_RESIZE_FS to {want} blocks: {}", std::io::Error::last_os_error()));
    }
    Ok(format!("ext4 at {mountpoint} grown online from {before} to {want} blocks of {block}"))
}

#[cfg(not(target_os = "linux"))]
pub fn grow_mounted(_reach: &str, mountpoint: &str, _device_bytes: u64) -> Result<String, String> {
    Err(format!("{mountpoint}: grown by the service that mounts it"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filesystem_told_by_its_magic() {
        assert_eq!(kind_of_magic(0xEF53), "ext4");
        assert_eq!(kind_of_magic(0x5846_5342), "xfs");
        assert_eq!(kind_of_magic(7), "another filesystem");
        assert_eq!(blocks_for(4 << 30, 4096), 1 << 20);
        assert_eq!(blocks_for(1, 0), 0);
    }
}
