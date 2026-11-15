//! What a local path is stored on, for the one moment it matters (#190):
//! `adopt-ublk` reads after the incumbent has stood down, and from then
//! until it serves the devices again nothing ublk-backed answers. A read of
//! a file on the root filesystem (the node's root is a ublk device) does not
//! fail there — it waits for the server this process is about to become.
//!
//! So everything the restore will read is checked before the stand-down,
//! while refusing still leaves the incumbent serving.

use std::path::{Path, PathBuf};

/// Where a path's bytes live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backing {
    /// Memory or the kernel (`tmpfs`, `devtmpfs`, `proc`, …), or a block
    /// device that is not ublk. Readable with no ublk server.
    Independent(String),
    /// A ublk device (`ublkb0`, or a device-mapper stack over one).
    Ublk(String),
    /// A filesystem with no block device of its own that is not memory
    /// (`overlay`, `fuse`, `nfs`): on a node, an overlay root is ublk
    /// underneath, so it is treated as such.
    Unknown(String),
}

impl Backing {
    pub fn is_safe(&self) -> bool {
        matches!(self, Backing::Independent(_))
    }
}

impl std::fmt::Display for Backing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backing::Independent(s) => write!(f, "{s}"),
            Backing::Ublk(s) => write!(f, "ublk device {s}"),
            Backing::Unknown(s) => write!(f, "a {s} filesystem, which may be on a ublk device"),
        }
    }
}

/// Filesystems that live in memory or in the kernel.
const MEMORY_FS: &[&str] =
    &["tmpfs", "devtmpfs", "ramfs", "proc", "sysfs", "devpts", "cgroup2", "efivarfs", "rootfs"];

/// One mount: its point, `major:minor`, and filesystem type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub point: PathBuf,
    pub dev: (u32, u32),
    pub fstype: String,
}

/// mountinfo escapes space, tab, newline and backslash as octal.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The mount `path` (absolute, resolved) is on, from `/proc/self/mountinfo`
/// text: the longest mount point that contains it, the last one listed when
/// one point is mounted over another.
pub fn mount_of(mountinfo: &str, path: &Path) -> Option<Mount> {
    let mut best: Option<Mount> = None;
    for line in mountinfo.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        let Some(sep) = f.iter().position(|x| *x == "-") else { continue };
        if f.len() < 5 || sep + 1 >= f.len() {
            continue;
        }
        let Some((maj, min)) = f[2].split_once(':') else { continue };
        let (Ok(maj), Ok(min)) = (maj.parse(), min.parse()) else { continue };
        let point = PathBuf::from(unescape(f[4]));
        if !path.starts_with(&point) {
            continue;
        }
        let longer = best
            .as_ref()
            .is_none_or(|b| point.components().count() >= b.point.components().count());
        if longer {
            best = Some(Mount { point, dev: (maj, min), fstype: f[sep + 1].to_string() });
        }
    }
    best
}

/// The ublk id in a kernel block name: `ublkb65535` and its partition
/// `ublkb65535p1` are both 65535 (#607).
pub fn ublk_id(name: &str) -> Option<u32> {
    let digits: String = name.strip_prefix("ublkb")?.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

impl Backing {
    /// The ublk ids a `Ublk` backing names (`ublkb3`, or `ublkb3, ublkb7`
    /// for a stack over two), empty for anything else.
    pub fn ublk_ids(&self) -> Vec<u32> {
        match self {
            Backing::Ublk(s) => s.split(", ").filter_map(ublk_id).collect(),
            _ => Vec::new(),
        }
    }
}

/// The kernel's name for a block device number, and whether it is ublk —
/// directly, as a partition of one, or under device-mapper/md (`slaves`).
/// Every ublk device below is named (#607: whose server each one is
/// decides whether it answers through a handover).
#[cfg(target_os = "linux")]
fn block_backing(dev: (u32, u32)) -> Backing {
    fn ublk_below(sys: &Path, depth: u32, out: &mut Vec<String>) {
        let Ok(real) = std::fs::canonicalize(sys) else { return };
        let Some(name) = real.file_name().map(|n| n.to_string_lossy().into_owned()) else { return };
        if name.starts_with("ublkb") {
            if !out.contains(&name) {
                out.push(name);
            }
            return;
        }
        // A partition's parent directory is its disk.
        if let Some(parent) = real.parent().and_then(|p| p.file_name()) {
            let p = parent.to_string_lossy();
            if p.starts_with("ublkb") {
                if !out.contains(&name) {
                    out.push(name);
                }
                return;
            }
        }
        if depth > 8 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(real.join("slaves")) else { return };
        for e in rd.flatten() {
            ublk_below(&e.path(), depth + 1, out);
        }
    }
    let sys = PathBuf::from(format!("/sys/dev/block/{}:{}", dev.0, dev.1));
    let mut found = Vec::new();
    ublk_below(&sys, 0, &mut found);
    if !found.is_empty() {
        return Backing::Ublk(found.join(", "));
    }
    let name = std::fs::canonicalize(&sys)
        .ok()
        .and_then(|r| r.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| format!("{}:{}", dev.0, dev.1));
    Backing::Independent(format!("block device {name}"))
}

/// Whether ublk-backed storage answers through a handover that stands down
/// the devices `stood_down` (#607). It does when every ublk device below it
/// is none of those and is served by a live process other than `incumbents`
/// (the servers being stood down) and this one: stormraid's node-data set at
/// `/dev/ublkb65535` is served by stormraid, which keeps serving while the
/// engine changes hands. `server` is the kernel's server pid for an id
/// (`None`: no such device). Returns why not, or `None` when it answers.
pub fn ublk_unsafe_why(
    ids: &[u32],
    stood_down: &[u32],
    incumbents: &[i32],
    me: i32,
    server: impl Fn(u32) -> Option<i32>,
) -> Option<String> {
    if ids.is_empty() {
        return Some("no ublk device could be named".into());
    }
    for &id in ids {
        if stood_down.contains(&id) {
            return Some(format!("/dev/ublkb{id} is one of the devices this handover stands down"));
        }
        match server(id) {
            None => return Some(format!("/dev/ublkb{id} is not a live ublk device")),
            Some(pid) if pid <= 0 => {
                return Some(format!("/dev/ublkb{id} has no server now (pid {pid}): it answers only once one takes it"))
            }
            Some(pid) if pid == me => return Some(format!("/dev/ublkb{id} is served by this process")),
            Some(pid) if incumbents.contains(&pid) => {
                return Some(format!("/dev/ublkb{id} is served by process {pid}, the server being stood down"))
            }
            Some(_) => {}
        }
    }
    None
}

/// What `path` is stored on. A path that does not exist yet is judged by
/// the nearest directory above it that does (where it would be created).
#[cfg(target_os = "linux")]
pub fn backing(path: &Path) -> std::io::Result<Backing> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let abs = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let mut at = abs.as_path();
    let meta = loop {
        match std::fs::metadata(at) {
            Ok(m) => break m,
            Err(_) => match at.parent() {
                Some(p) => at = p,
                None => return Err(std::io::Error::other(format!("no part of {} exists", path.display()))),
            },
        }
    };
    // A block device node is its own storage, whatever /dev is.
    if meta.file_type().is_block_device() {
        let rdev = meta.rdev();
        let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
        let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
        return Ok(block_backing((major as u32, minor as u32)));
    }
    let real = std::fs::canonicalize(at)?;
    let info = std::fs::read_to_string("/proc/self/mountinfo")?;
    let Some(m) = mount_of(&info, &real) else {
        return Ok(Backing::Unknown("unlisted".into()));
    };
    if MEMORY_FS.contains(&m.fstype.as_str()) {
        return Ok(Backing::Independent(m.fstype));
    }
    if m.dev.0 == 0 {
        return Ok(Backing::Unknown(m.fstype));
    }
    Ok(block_backing(m.dev))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = "\
22 1 0:21 / / rw,relatime - overlay overlay rw,lowerdir=/l,upperdir=/u
23 22 0:5 / /dev rw,nosuid - devtmpfs devtmpfs rw
24 22 0:24 / /run rw,nosuid - tmpfs tmpfs rw
25 22 259:3 / /var/lib\\040data rw - ext4 /dev/ublkb3 rw
26 24 0:30 / /run rw - tmpfs tmpfs rw
";

    #[test]
    fn the_longest_mount_point_wins() {
        let m = mount_of(INFO, Path::new("/run/stormblock/engine")).unwrap();
        assert_eq!(m.point, Path::new("/run"));
        assert_eq!(m.dev, (0, 30), "the later of two mounts on one point");
        assert_eq!(m.fstype, "tmpfs");
        let m = mount_of(INFO, Path::new("/etc/stormblock")).unwrap();
        assert_eq!(m.fstype, "overlay");
        let m = mount_of(INFO, Path::new("/var/lib data/meta")).unwrap();
        assert_eq!((m.dev, m.fstype.as_str()), ((259, 3), "ext4"), "escaped point");
        assert_eq!(mount_of(INFO, Path::new("/runner")).unwrap().point, Path::new("/"), "a prefix of a name is not a parent");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn memory_is_safe_and_an_unlisted_overlay_is_not() {
        assert!(backing(Path::new("/proc/self")).unwrap().is_safe());
        assert!(!Backing::Unknown("overlay".into()).is_safe());
        assert!(!Backing::Ublk("ublkb0".into()).is_safe());
    }

    #[test]
    fn ublk_ids_from_names() {
        assert_eq!(ublk_id("ublkb65535"), Some(65535));
        assert_eq!(ublk_id("ublkb3p1"), Some(3));
        assert_eq!(ublk_id("sda"), None);
        assert_eq!(Backing::Ublk("ublkb3, ublkb65535p2".into()).ublk_ids(), vec![3, 65535]);
        assert!(Backing::Independent("tmpfs".into()).ublk_ids().is_empty());
    }

    /// #607: stormraid's set (65535, pid 2121) answers through the engine's
    /// handover (devices 0..3 served by 900); the engine's own do not.
    #[test]
    fn another_servers_ublk_answers_through_the_handover() {
        let server = |id: u32| match id {
            0..=2 => Some(900),
            65535 => Some(2121),
            70 => Some(-1),
            71 => Some(4242),
            _ => None,
        };
        let stood = [0, 1, 2];
        assert_eq!(ublk_unsafe_why(&[65535], &stood, &[900], 4242, server), None);
        assert!(ublk_unsafe_why(&[1], &stood, &[900], 4242, server).unwrap().contains("stands down"));
        assert!(ublk_unsafe_why(&[65535, 2], &stood, &[900], 4242, server).is_some(), "a stack over one of ours");
        assert!(ublk_unsafe_why(&[70], &stood, &[900], 4242, server).unwrap().contains("no server"));
        assert!(ublk_unsafe_why(&[71], &stood, &[900], 4242, server).unwrap().contains("this process"));
        assert!(ublk_unsafe_why(&[99], &stood, &[900], 4242, server).unwrap().contains("not a live"));
        assert!(ublk_unsafe_why(&[], &stood, &[900], 4242, server).is_some());
        // A device the incumbent serves but was not named (also_served_by
        // refuses that first) is still not safe here.
        let by_incumbent = |_id: u32| Some(900);
        assert!(ublk_unsafe_why(&[40], &stood, &[900], 4242, by_incumbent).unwrap().contains("being stood down"));
    }
}
