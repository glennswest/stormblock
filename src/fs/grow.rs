//! A mounted filesystem grown to its device, online (#655).
//!
//! A volume grown through its object (`spec.sizeBytes`) is a bigger device;
//! the filesystem on it is grown by whoever mounts it. When the engine can
//! see the mount (the device's mount point in the host's mount table), it
//! grows an ext4 itself, as `resize2fs` does on a mounted one: the kernel's
//! `EXT4_IOC_RESIZE_FS` with the new block count. Anything else is said, and
//! left to the service that mounts it (an XFS: `xfs_growfs`).

/// `_IOW('f', 16, __u64)`.
#[cfg(target_os = "linux")]
const EXT4_IOC_RESIZE_FS: libc::c_ulong = 0x4008_6610;

/// The new block count for `bytes` of device at `block` bytes a block.
pub fn blocks_for(bytes: u64, block: u64) -> u64 {
    bytes.checked_div(block).unwrap_or(0)
}

/// What the filesystem at `mountpoint` is (`ext4`, `xfs`, …), from the
/// mount table.
pub fn kind_at(mountinfo: &str, mountpoint: &str) -> Option<String> {
    mountinfo.lines().find_map(|l| {
        let f: Vec<&str> = l.split(' ').collect();
        let sep = f.iter().position(|x| *x == "-")?;
        if f.get(4) == Some(&mountpoint) {
            f.get(sep + 1).map(|s| s.to_string())
        } else {
            None
        }
    })
}

/// Grow the ext4 mounted at `mountpoint` to `device_bytes`. Said in words.
#[cfg(target_os = "linux")]
pub fn grow_mounted(mountpoint: &str, device_bytes: u64) -> Result<String, String> {
    use std::os::fd::AsRawFd;
    let info = ["/proc/1/mountinfo", "/proc/self/mountinfo"].iter().find_map(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    let kind = kind_at(&info, mountpoint).unwrap_or_else(|| "?".into());
    if kind != "ext4" {
        return Err(format!("{kind} at {mountpoint}: grown by the service that mounts it (an XFS: xfs_growfs {mountpoint})"));
    }
    let dir = std::fs::File::open(mountpoint).map_err(|e| format!("{mountpoint}: {e}"))?;
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
pub fn grow_mounted(mountpoint: &str, _device_bytes: u64) -> Result<String, String> {
    Err(format!("{mountpoint}: grown by the service that mounts it"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mount_table_read() {
        let t = "36 25 259:3 / /var/lib/registry rw,relatime shared:12 - ext4 /dev/ublkb7 rw\n37 25 259:4 / /data rw - xfs /dev/ublkb8 rw\n";
        assert_eq!(kind_at(t, "/var/lib/registry").as_deref(), Some("ext4"));
        assert_eq!(kind_at(t, "/data").as_deref(), Some("xfs"));
        assert_eq!(kind_at(t, "/nope"), None);
        assert_eq!(blocks_for(4 << 30, 4096), 1 << 20);
        assert_eq!(blocks_for(1, 0), 0);
    }
}
