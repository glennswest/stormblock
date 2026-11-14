//! Drives that come and go: the kernel's uevents (#593).
//!
//! Owner, #593: "You need to watch for hot plug even after boot." A drive
//! that arrives after the boot's storage inventory (a slow spin-up, a failed
//! drive swapped out) is told by a `NETLINK_KOBJECT_UEVENT` message; there is
//! no udev on stormcos, so this listens to the kernel's own multicast group.
//!
//! Only whole drives are told: a block disk (`SUBSYSTEM=block`,
//! `DEVTYPE=disk`, never a loop, ram, ublk, nbd, dm, md or optical device),
//! and the `sg` node of a disk-type SCSI device that has no block device -
//! a drive that answers with no readable format (NOT READY, #595), which sd
//! makes nothing of. Each is described from sysfs as it arrives: its serial,
//! WWID, controller (the PCI function closest to it) and bay (its enclosure
//! slot), so a remove, whose sysfs is gone, is said with what the add saw.
//!
//! `stormblock hotplug watch` runs this in the initramfs (one line per event,
//! [`DriveEvent::line`]); the engine runs it as a task and logs each event.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// One uevent as the kernel sent it: `ACTION@DEVPATH`, then `KEY=VALUE`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uevent {
    pub action: String,
    pub devpath: String,
    pub vars: HashMap<String, String>,
}

impl Uevent {
    pub fn var(&self, k: &str) -> Option<&str> {
        self.vars.get(k).map(String::as_str)
    }
}

/// A kernel uevent datagram, parsed. udev's own re-broadcasts (`libudev`
/// magic, on group 2) and anything without an `ACTION@path` header are not
/// the kernel's and give `None`.
pub fn parse(buf: &[u8]) -> Option<Uevent> {
    let mut parts = buf.split(|b| *b == 0).filter(|p| !p.is_empty());
    let head = std::str::from_utf8(parts.next()?).ok()?;
    let (action, devpath) = head.split_once('@')?;
    if action.is_empty() || !devpath.starts_with('/') {
        return None;
    }
    let mut vars = HashMap::new();
    for p in parts {
        let Ok(s) = std::str::from_utf8(p) else { continue };
        if let Some((k, v)) = s.split_once('=') {
            vars.insert(k.to_string(), v.to_string());
        }
    }
    Some(Uevent { action: action.to_string(), devpath: devpath.to_string(), vars })
}

/// What happened to a drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Add,
    Remove,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Add => "add",
            Action::Remove => "remove",
        }
    }
}

/// A whole drive that came or went, with what identifies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DriveEvent {
    pub action: Action,
    /// `/dev/sdb`, or `/dev/sg3` for a drive with no block device.
    pub device: String,
    pub serial: Option<String>,
    pub wwid: Option<String>,
    /// The PCI function closest to the drive, e.g. `0000:03:00.0`.
    pub controller: Option<String>,
    /// Its enclosure slot, as the enclosure names it.
    pub bay: Option<String>,
    pub bytes: u64,
    /// False: the drive answers with no readable format (0 bytes, or only
    /// an sg node): it needs a format before anything can use it.
    pub ready: bool,
    /// Seconds since the epoch, when it was seen.
    pub at: u64,
}

impl DriveEvent {
    /// One line, words `key=value` after the action and device; `-` for
    /// what is not known. `/init` reads it with `sed`/`awk`.
    pub fn line(&self) -> String {
        let w = |v: &Option<String>| v.as_deref().filter(|s| !s.is_empty()).map(clean).unwrap_or_else(|| "-".into());
        format!(
            "{} {} serial={} wwid={} controller={} bay={} bytes={} ready={} at={}",
            self.action.as_str(),
            self.device,
            w(&self.serial),
            w(&self.wwid),
            w(&self.controller),
            w(&self.bay),
            self.bytes,
            u8::from(self.ready),
            self.at,
        )
    }

    /// The same, said to a person: `drive added: /dev/sdb (serial X, WWID
    /// Y) on 0000:03:00.0, bay 3, 10000 GB`.
    pub fn said(&self) -> String {
        let mut s = format!(
            "drive {}: {}",
            match self.action {
                Action::Add => "added",
                Action::Remove => "removed",
            },
            self.device
        );
        let mut id = Vec::new();
        if let Some(v) = self.serial.as_deref().filter(|v| !v.is_empty()) {
            id.push(format!("serial {v}"));
        }
        if let Some(v) = self.wwid.as_deref().filter(|v| !v.is_empty()) {
            id.push(format!("WWID {v}"));
        }
        if !id.is_empty() {
            s.push_str(&format!(" ({})", id.join(", ")));
        }
        if let Some(c) = &self.controller {
            s.push_str(&format!(" on {c}"));
        }
        if let Some(b) = &self.bay {
            s.push_str(&format!(", bay {b}"));
        }
        if self.action == Action::Add {
            if self.ready {
                s.push_str(&format!(", {} GB", self.bytes / 1_000_000_000));
            } else {
                s.push_str(", NOT READY (no readable format)");
            }
        }
        s
    }
}

/// Spaces and controls out of a sysfs string, so a line stays one word a
/// field.
fn clean(s: &str) -> String {
    s.trim().chars().map(|c| if c.is_whitespace() || c.is_control() { '_' } else { c }).collect()
}

/// The block devices that are never drives.
pub fn is_drive_name(name: &str) -> bool {
    const NOT: [&str; 10] = ["loop", "ram", "zram", "ublkb", "nbd", "dm-", "md", "sr", "fd", "ublkc"];
    !name.is_empty() && !NOT.iter().any(|p| name.starts_with(p))
}

/// The last PCI function (`dddd:bb:dd.f`) on a sysfs path.
pub fn controller_of(path: &str) -> Option<String> {
    path.split('/').rfind(|c| is_pci_address(c)).map(str::to_string)
}

fn is_pci_address(c: &str) -> bool {
    let b = c.as_bytes();
    b.len() == 12
        && b[4] == b':'
        && b[7] == b':'
        && b[10] == b'.'
        && b.iter().enumerate().all(|(i, x)| matches!(i, 4 | 7 | 10) || x.is_ascii_hexdigit())
}

fn read_trim(p: &Path) -> Option<String> {
    let s = std::fs::read(p).ok()?;
    let s = String::from_utf8_lossy(&s[..s.len().min(256)]).trim().trim_matches('\0').trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// A SCSI device's serial: `serial` (NVMe, some HBAs), else VPD page 0x80.
fn serial_of(dev: &Path) -> Option<String> {
    if let Some(s) = read_trim(&dev.join("serial")) {
        return Some(s);
    }
    let pg = std::fs::read(dev.join("vpd_pg80")).ok()?;
    let s = String::from_utf8_lossy(pg.get(4..)?).trim().trim_matches('\0').trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Its enclosure slot: the `enclosure_device:<slot>` link under the device.
fn bay_of(dev: &Path) -> Option<String> {
    std::fs::read_dir(dev).ok()?.flatten().find_map(|e| {
        e.file_name().to_str().and_then(|n| n.strip_prefix("enclosure_device:")).map(str::to_string)
    })
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Where sysfs is (tests point it at a tree of their own).
#[derive(Debug, Clone)]
pub struct Sys {
    pub root: PathBuf,
}

impl Default for Sys {
    fn default() -> Self {
        Sys { root: PathBuf::from("/sys") }
    }
}

impl Sys {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Sys { root: root.into() }
    }

    /// A block disk, described as it is now; `None` when it is not a drive
    /// (or is gone).
    pub fn block_disk(&self, name: &str, action: Action) -> Option<DriveEvent> {
        if !is_drive_name(name) {
            return None;
        }
        let b = self.root.join("block").join(name);
        let dev = b.join("device");
        if !dev.exists() {
            return None;
        }
        let real = std::fs::canonicalize(&b).ok().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        if real.contains("/usb") {
            return None;
        }
        let bytes = read_trim(&b.join("size")).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0) * 512;
        let removable = read_trim(&b.join("removable")).as_deref() == Some("1");
        if removable {
            return None;
        }
        Some(DriveEvent {
            action,
            device: format!("/dev/{name}"),
            serial: serial_of(&dev),
            wwid: read_trim(&dev.join("wwid")).or_else(|| read_trim(&b.join("wwid"))),
            controller: controller_of(&real),
            bay: bay_of(&dev),
            bytes,
            ready: bytes > 0,
            at: now(),
        })
    }

    /// An sg node whose SCSI device is a disk with no block device: a drive
    /// that answers with no readable format (#595).
    pub fn unready_sg(&self, sg: &str, action: Action) -> Option<DriveEvent> {
        let dev = self.root.join("class/scsi_generic").join(sg).join("device");
        if read_trim(&dev.join("type")).as_deref() != Some("0") {
            return None;
        }
        if std::fs::read_dir(dev.join("block")).ok().is_some_and(|mut d| d.next().is_some()) {
            return None;
        }
        let real = std::fs::canonicalize(&dev).ok().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        if real.contains("/usb") {
            return None;
        }
        Some(DriveEvent {
            action,
            device: format!("/dev/{sg}"),
            serial: serial_of(&dev),
            wwid: read_trim(&dev.join("wwid")),
            controller: controller_of(&real),
            bay: bay_of(&dev),
            bytes: 0,
            ready: false,
            at: now(),
        })
    }

    /// Every drive there is now: the block disks, then the not-ready ones.
    pub fn present(&self) -> Vec<DriveEvent> {
        let mut out = Vec::new();
        let names = |d: PathBuf| -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(d)
                .map(|r| r.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
                .unwrap_or_default();
            v.sort();
            v
        };
        for n in names(self.root.join("block")) {
            out.extend(self.block_disk(&n, Action::Add));
        }
        for n in names(self.root.join("class/scsi_generic")) {
            out.extend(self.unready_sg(&n, Action::Add));
        }
        out
    }
}

/// Uevents turned into drive events, with what each drive was when it came
/// (its sysfs is gone by its remove).
#[derive(Debug, Default)]
pub struct Tracker {
    pub sys: Sys,
    known: HashMap<String, DriveEvent>,
}

impl Tracker {
    pub fn new(sys: Sys) -> Self {
        Tracker { sys, known: HashMap::new() }
    }

    /// The drives there now, remembered (so their removes are said whole).
    pub fn prime(&mut self) -> Vec<DriveEvent> {
        let now = self.sys.present();
        for d in &now {
            self.known.insert(d.device.clone(), d.clone());
        }
        now
    }

    /// What changed since the drives were last known, read from sysfs: for
    /// when uevents were lost (the socket's buffer overran, ENOBUFS).
    pub fn rescan(&mut self) -> Vec<DriveEvent> {
        let there = self.sys.present();
        let mut out = Vec::new();
        let gone: Vec<String> =
            self.known.keys().filter(|k| !there.iter().any(|d| &d.device == *k)).cloned().collect();
        for g in gone {
            if let Some(mut d) = self.known.remove(&g) {
                d.action = Action::Remove;
                d.at = now();
                out.push(d);
            }
        }
        for d in there {
            let was = self.known.get(&d.device);
            if was.is_none() || (was.is_some_and(|w| !w.ready) && d.ready) {
                out.push(d.clone());
            }
            self.known.insert(d.device.clone(), d);
        }
        out
    }

    /// A uevent, as a drive event when it is one. A drive's `change` (a
    /// format that gave it a size, a capacity rescan) is said as an add when
    /// it was not ready before and is now.
    pub fn on(&mut self, ev: &Uevent) -> Option<DriveEvent> {
        let subsystem = ev.var("SUBSYSTEM")?;
        let name = ev.var("DEVNAME").map(|n| n.trim_start_matches("/dev/").to_string()).or_else(|| {
            ev.devpath.rsplit('/').next().map(str::to_string)
        })?;
        let action = ev.action.as_str();
        let found = match (subsystem, action) {
            ("block", "add" | "change") if ev.var("DEVTYPE") == Some("disk") => {
                let d = self.sys.block_disk(&name, Action::Add)?;
                if action == "change" {
                    let was_ready = self.known.get(&d.device).map(|k| k.ready);
                    if was_ready == Some(true) || !d.ready {
                        self.known.insert(d.device.clone(), d);
                        return None;
                    }
                }
                Some(d)
            }
            ("scsi_generic", "add") => self.sys.unready_sg(&name, Action::Add),
            ("block", "remove") if ev.var("DEVTYPE") == Some("disk") => {
                let dev = format!("/dev/{name}");
                if !is_drive_name(&name) {
                    return None;
                }
                let mut d = self.known.remove(&dev).unwrap_or(DriveEvent {
                    action: Action::Remove,
                    device: dev,
                    serial: None,
                    wwid: None,
                    controller: controller_of(&ev.devpath),
                    bay: None,
                    bytes: 0,
                    ready: false,
                    at: 0,
                });
                d.action = Action::Remove;
                d.at = now();
                return Some(d);
            }
            ("scsi_generic", "remove") => {
                let mut d = self.known.remove(&format!("/dev/{name}"))?;
                d.action = Action::Remove;
                d.at = now();
                return Some(d);
            }
            _ => None,
        }?;
        self.known.insert(found.device.clone(), found.clone());
        Some(found)
    }
}

/// The kernel's uevent multicast, opened.
#[cfg(target_os = "linux")]
pub struct Listener {
    fd: std::os::fd::OwnedFd,
}

#[cfg(target_os = "linux")]
impl Listener {
    /// Bound to the kernel's group (1); a 4 MiB receive buffer so a burst
    /// (a shelf of drives at once) is not dropped.
    pub fn open() -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;
        // SAFETY: plain socket(2); the fd is owned below.
        let raw = unsafe {
            libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, libc::NETLINK_KOBJECT_UEVENT)
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: raw is a new, valid fd that nothing else owns.
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        let size: libc::c_int = 4 << 20;
        // SAFETY: a c_int option of its size on our own fd. A smaller buffer
        // than asked is no failure (SO_RCVBUFFORCE needs privilege).
        unsafe {
            use std::os::fd::AsRawFd;
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &size as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        // SAFETY: sockaddr_nl is plain data; zeroed is valid.
        let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        sa.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        sa.nl_groups = 1;
        // SAFETY: bind with a sockaddr_nl of its size on our own fd.
        let rc = unsafe {
            use std::os::fd::AsRawFd;
            libc::bind(
                fd.as_raw_fd(),
                &sa as *const libc::sockaddr_nl as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Listener { fd })
    }

    /// The next uevent, or `None` after `timeout` with none. Datagrams that
    /// are not the kernel's are skipped.
    pub fn next(&self, timeout: std::time::Duration) -> std::io::Result<Option<Uevent>> {
        use std::os::fd::AsRawFd;
        let mut buf = vec![0u8; 16 * 1024];
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let mut pfd = libc::pollfd { fd: self.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let ms = left.as_millis().min(i32::MAX as u128) as libc::c_int;
            // SAFETY: one pollfd we own.
            let n = unsafe { libc::poll(&mut pfd, 1, ms) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                return Ok(None);
            }
            // SAFETY: recv into a buffer of its length on our own fd.
            let got = unsafe {
                libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len(), libc::MSG_DONTWAIT)
            };
            if got < 0 {
                let e = std::io::Error::last_os_error();
                // ENOBUFS: the buffer overran and events were lost; the
                // caller rescans.
                if matches!(e.raw_os_error(), Some(libc::EAGAIN) | Some(libc::EINTR)) {
                    continue;
                }
                return Err(e);
            }
            if let Some(ev) = parse(&buf[..got as usize]) {
                return Ok(Some(ev));
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(parts: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        for p in parts {
            v.extend_from_slice(p.as_bytes());
            v.push(0);
        }
        v
    }

    const SDB: &str = "/devices/pci0000:00/0000:00:01.0/0000:03:00.0/host0/port-0:1/end_device-0:1/target0:0:1/0:0:1:0/block/sdb";

    #[test]
    fn a_kernel_uevent_parses() {
        let b = msg(&[&format!("add@{SDB}"), "ACTION=add", &format!("DEVPATH={SDB}"), "SUBSYSTEM=block", "DEVNAME=sdb", "DEVTYPE=disk", "SEQNUM=4242"]);
        let e = parse(&b).unwrap();
        assert_eq!(e.action, "add");
        assert_eq!(e.devpath, SDB);
        assert_eq!(e.var("DEVTYPE"), Some("disk"));
        assert_eq!(e.var("SEQNUM"), Some("4242"));
    }

    #[test]
    fn udev_rebroadcasts_and_junk_are_not_kernel_uevents() {
        assert!(parse(b"libudev\0\xfe\xed\xca\xfe").is_none());
        assert!(parse(b"").is_none());
        assert!(parse(b"add@\0").is_none());
    }

    #[test]
    fn the_controller_is_the_pci_function_closest_to_the_drive() {
        assert_eq!(controller_of(SDB).as_deref(), Some("0000:03:00.0"));
        assert_eq!(controller_of("/devices/virtual/block/loop0"), None);
    }

    #[test]
    fn only_drives_are_drives() {
        for n in ["sda", "nvme0n1", "vdb", "sdaa"] {
            assert!(is_drive_name(n), "{n}");
        }
        for n in ["loop0", "ram1", "zram0", "ublkb64", "nbd0", "dm-0", "md127", "sr0", "fd0"] {
            assert!(!is_drive_name(n), "{n}");
        }
    }

    /// A sysfs tree: /sys/block/<name> -> devices/.../block/<name>, its
    /// device dir with serial, wwid, type and an enclosure link.
    fn disk(root: &Path, name: &str, sectors: u64, bay: Option<&str>) {
        let real = root.join(format!("devices/pci0000:00/0000:00:01.0/0000:03:00.0/host0/0:0:{name}/block/{name}"));
        std::fs::create_dir_all(&real).unwrap();
        let dev = real.parent().unwrap().parent().unwrap().to_path_buf();
        std::os::unix::fs::symlink(&dev, real.join("device")).unwrap();
        std::fs::write(real.join("size"), format!("{sectors}\n")).unwrap();
        std::fs::write(real.join("removable"), "0\n").unwrap();
        std::fs::write(dev.join("wwid"), format!("naa.5000c500{name}\n")).unwrap();
        std::fs::write(dev.join("type"), "0\n").unwrap();
        let mut pg = vec![0u8, 0x80, 0, 8];
        pg.extend_from_slice(format!("  SER{name} ").as_bytes());
        std::fs::write(dev.join("vpd_pg80"), pg).unwrap();
        if let Some(b) = bay {
            std::os::unix::fs::symlink(root, dev.join(format!("enclosure_device:{b}"))).unwrap();
        }
        std::fs::create_dir_all(root.join("block")).unwrap();
        std::os::unix::fs::symlink(&real, root.join("block").join(name)).unwrap();
    }

    fn block_ev(action: &str, name: &str) -> Uevent {
        parse(&msg(&[&format!("{action}@/devices/x/block/{name}"), "SUBSYSTEM=block", &format!("DEVNAME={name}"), "DEVTYPE=disk"])).unwrap()
    }

    #[test]
    fn a_drive_that_arrives_is_said_with_its_serial_controller_and_bay() {
        let t = tempfile::tempdir().unwrap();
        disk(t.path(), "sda", 1000, Some("Slot 01"));
        let mut tr = Tracker::new(Sys::new(t.path()));
        assert_eq!(tr.prime().len(), 1);
        disk(t.path(), "sdb", 19532873728, Some("Slot 02"));
        let d = tr.on(&block_ev("add", "sdb")).unwrap();
        assert_eq!(d.action, Action::Add);
        assert_eq!(d.device, "/dev/sdb");
        assert_eq!(d.serial.as_deref(), Some("SERsdb"));
        assert_eq!(d.wwid.as_deref(), Some("naa.5000c500sdb"));
        assert_eq!(d.controller.as_deref(), Some("0000:03:00.0"));
        assert_eq!(d.bay.as_deref(), Some("Slot 02"));
        assert!(d.ready);
        let l = d.line();
        assert!(l.starts_with("add /dev/sdb serial=SERsdb wwid=naa.5000c500sdb controller=0000:03:00.0 bay=Slot_02 bytes=10000831348736 ready=1 at="), "{l}");
        assert!(d.said().starts_with("drive added: /dev/sdb (serial SERsdb, WWID naa.5000c500sdb) on 0000:03:00.0, bay Slot 02, 10000 GB"), "{}", d.said());
    }

    #[test]
    fn a_removed_drive_is_said_with_what_its_add_saw() {
        let t = tempfile::tempdir().unwrap();
        disk(t.path(), "sdc", 1000, Some("4"));
        let mut tr = Tracker::new(Sys::new(t.path()));
        tr.prime();
        std::fs::remove_file(t.path().join("block/sdc")).unwrap();
        let d = tr.on(&block_ev("remove", "sdc")).unwrap();
        assert_eq!(d.action, Action::Remove);
        assert_eq!(d.serial.as_deref(), Some("SERsdc"));
        assert_eq!(d.bay.as_deref(), Some("4"));
        assert!(d.said().starts_with("drive removed: /dev/sdc (serial SERsdc"), "{}", d.said());
        // One never seen still says its device and controller.
        let d = tr.on(&parse(&msg(&[&format!("remove@{SDB}"), "SUBSYSTEM=block", "DEVNAME=sdb", "DEVTYPE=disk"])).unwrap()).unwrap();
        assert_eq!(d.device, "/dev/sdb");
        assert_eq!(d.controller.as_deref(), Some("0000:03:00.0"));
    }

    #[test]
    fn a_rescan_says_what_came_and_went() {
        let t = tempfile::tempdir().unwrap();
        disk(t.path(), "sda", 1000, None);
        let mut tr = Tracker::new(Sys::new(t.path()));
        tr.prime();
        std::fs::remove_file(t.path().join("block/sda")).unwrap();
        disk(t.path(), "sdb", 1000, None);
        let mut got: Vec<String> = tr.rescan().iter().map(|d| format!("{} {}", d.action.as_str(), d.device)).collect();
        got.sort();
        assert_eq!(got, ["add /dev/sdb", "remove /dev/sda"]);
        assert!(tr.rescan().is_empty());
    }

    #[test]
    fn partitions_and_virtual_devices_are_not_drive_events() {
        let t = tempfile::tempdir().unwrap();
        let mut tr = Tracker::new(Sys::new(t.path()));
        let part = parse(&msg(&["add@/devices/x/block/sda/sda1", "SUBSYSTEM=block", "DEVNAME=sda1", "DEVTYPE=partition"])).unwrap();
        assert!(tr.on(&part).is_none());
        assert!(tr.on(&block_ev("add", "ublkb64")).is_none());
        assert!(tr.on(&block_ev("remove", "loop3")).is_none());
    }

    #[test]
    fn a_not_ready_drive_becoming_ready_is_an_add() {
        let t = tempfile::tempdir().unwrap();
        disk(t.path(), "sdd", 0, None);
        let mut tr = Tracker::new(Sys::new(t.path()));
        let now = tr.prime();
        assert!(!now[0].ready);
        // A change while still 0 bytes says nothing.
        assert!(tr.on(&block_ev("change", "sdd")).is_none());
        std::fs::write(t.path().join("block/sdd/size"), "2000\n").unwrap();
        let d = tr.on(&block_ev("change", "sdd")).unwrap();
        assert!(d.ready);
        // And a change of a ready drive (a rescan) says nothing again.
        assert!(tr.on(&block_ev("change", "sdd")).is_none());
    }

    #[test]
    fn an_sg_node_of_a_disk_with_no_block_device_is_a_not_ready_drive() {
        let t = tempfile::tempdir().unwrap();
        let dev = t.path().join("devices/pci0000:00/0000:00:01.0/0000:04:00.0/host1/1:0:2:0");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("type"), "0\n").unwrap();
        std::fs::write(dev.join("wwid"), "naa.5000cca0aa\n").unwrap();
        let sg = t.path().join("class/scsi_generic/sg2");
        std::fs::create_dir_all(&sg).unwrap();
        std::os::unix::fs::symlink(&dev, sg.join("device")).unwrap();
        let mut tr = Tracker::new(Sys::new(t.path()));
        let ev = parse(&msg(&["add@/devices/x/scsi_generic/sg2", "SUBSYSTEM=scsi_generic", "DEVNAME=sg2"])).unwrap();
        let d = tr.on(&ev).unwrap();
        assert_eq!(d.device, "/dev/sg2");
        assert!(!d.ready);
        assert_eq!(d.controller.as_deref(), Some("0000:04:00.0"));
        assert!(d.said().ends_with("NOT READY (no readable format)"));
        // An sg node of a disk sd made a block device for is not one.
        std::fs::create_dir_all(dev.join("block/sde")).unwrap();
        assert!(tr.sys.unready_sg("sg2", Action::Add).is_none());
    }
}
