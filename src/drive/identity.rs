//! Who a block device is: serial, model and world-wide name, from sysfs.
//!
//! The identity stormdrive knows a drive by — and the one that survives a
//! drive moving to another bay, slot or host — is its serial and WWN, not its
//! device name. A `FileDevice` opened on `/dev/sda` used to report itself as
//! `file`, so every slab on a real disk carried `domain: drive=file+<offset>`:
//! nothing could join it to the drive, and two slabs on one spindle counted as
//! two failure domains (#136).
//!
//! A partition (`/dev/sda2`, `/dev/nvme0n1p3`) is resolved to the disk it is
//! on: a partition has no identity of its own worth having.

use std::path::{Path, PathBuf};

/// What sysfs says about a disk. Empty strings for what it does not say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub serial: String,
    pub model: String,
    pub wwn: String,
    /// The disk's kernel name (`sda`, `nvme0n1`) — for a partition, its disk.
    pub disk: String,
}

impl Identity {
    pub fn is_empty(&self) -> bool {
        self.serial.is_empty() && self.wwn.is_empty()
    }
}

/// Identity of the block device at `path`, from the live `/sys`.
pub fn of(path: &str) -> Option<Identity> {
    let dev = std::fs::canonicalize(path).ok()?;
    let name = dev.file_name()?.to_str()?.to_string();
    of_in(Path::new("/sys"), &name)
}

/// Identity of block device `name` under a sysfs root — split out so it can
/// be tested against a tree built in a temp directory.
pub fn of_in(sys: &Path, name: &str) -> Option<Identity> {
    let class = sys.join("class/block").join(name);
    let node = std::fs::canonicalize(&class).unwrap_or(class);
    if !node.exists() {
        return None;
    }
    // A partition's sysfs directory sits inside its disk's, and has a
    // `partition` file saying which one it is.
    let disk = if node.join("partition").exists() {
        node.parent().map(PathBuf::from)?
    } else {
        node
    };
    let read = |rel: &str| -> String {
        std::fs::read_to_string(disk.join(rel)).map(|s| s.trim().to_string()).unwrap_or_default()
    };

    // NVMe puts `wwid` on the namespace; SCSI on the device.
    let mut wwn = read("wwid");
    if wwn.is_empty() {
        wwn = read("device/wwid");
    }
    // NVMe controllers and some SCSI drivers expose `serial`; otherwise the
    // unit serial number is VPD page 0x80.
    let mut serial = read("device/serial");
    if serial.is_empty() {
        serial = std::fs::read(disk.join("device/vpd_pg80")).map(|b| vpd_serial(&b)).unwrap_or_default();
    }
    let model = read("device/model");
    let id = Identity {
        serial,
        model,
        wwn,
        disk: disk.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string(),
    };
    (!id.is_empty()).then_some(id)
}

/// How a disk records (#282): conventional, or shingled (SMR). A shingled
/// disk takes sustained writes into a cache region and slows to seconds per
/// write, and per flush, once that is full: a flow-over is exactly that load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Recording {
    /// Nothing says otherwise.
    Conventional,
    /// The kernel sees zones the host must write in order (`queue/zoned`
    /// `host-managed`).
    HostManaged,
    /// Zoned but takes random writes (`queue/zoned` `host-aware`).
    HostAware,
    /// Reports itself conventional (`queue/zoned` `none`), but its model is on
    /// the makers' published lists of drive-managed SMR disks.
    DriveManagedSmr,
}

/// Models the makers have said are drive-managed SMR (Seagate's and WD's
/// published lists, 2020). Matched as a word in sysfs's `device/model`,
/// which carries a firmware suffix (`ST2000DM008-2FR102`, `WDC WD40EFAX-68JH4N0`).
const DM_SMR_MODELS: &[&str] = &[
    // Seagate
    "ST500LM030", "ST1000LM048", "ST1000LM049", "ST2000LM015", "ST3000LM024", "ST4000LM024", "ST5000LM000",
    "ST2000DM005", "ST2000DM008", "ST3000DM007", "ST4000DM004", "ST6000DM003", "ST8000DM004", "ST8000AS0002",
    "ST8000AS0003",
    // WD
    "WD20EFAX", "WD30EFAX", "WD40EFAX", "WD60EFAX", "WD20EZAZ", "WD60EZAZ", "WD10SPZX", "WD20SPZX", "WD30SPZX",
    "WD40NPZZ", "WD50NPZZ",
    // Toshiba
    "MQ04ABF100", "MQ04ABD200", "DT02ABA400", "DT02ABA600",
];

/// Whether `model` is on [`DM_SMR_MODELS`].
pub fn drive_managed_smr(model: &str) -> bool {
    model
        .split(|c: char| c.is_whitespace() || c == '-')
        .any(|w| DM_SMR_MODELS.iter().any(|m| w.eq_ignore_ascii_case(m)))
}

/// How the block device at `path` records, from the live `/sys`.
pub fn recording_of(path: &str) -> Option<(Recording, Identity)> {
    let dev = std::fs::canonicalize(path).ok()?;
    let name = dev.file_name()?.to_str()?.to_string();
    recording_of_in(Path::new("/sys"), &name)
}

/// [`recording_of`] under a sysfs root (tests).
pub fn recording_of_in(sys: &Path, name: &str) -> Option<(Recording, Identity)> {
    let id = of_in(sys, name).unwrap_or_default();
    let class = sys.join("class/block").join(name);
    let node = std::fs::canonicalize(&class).unwrap_or(class);
    if !node.exists() {
        return None;
    }
    let disk = if node.join("partition").exists() { node.parent().map(PathBuf::from)? } else { node };
    let zoned = std::fs::read_to_string(disk.join("queue/zoned")).map(|s| s.trim().to_string()).unwrap_or_default();
    let model = if id.model.is_empty() {
        std::fs::read_to_string(disk.join("device/model")).map(|s| s.trim().to_string()).unwrap_or_default()
    } else {
        id.model.clone()
    };
    let r = match zoned.as_str() {
        "host-managed" => Recording::HostManaged,
        "host-aware" => Recording::HostAware,
        _ if drive_managed_smr(&model) => Recording::DriveManagedSmr,
        _ => Recording::Conventional,
    };
    let disk_name = disk.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
    Some((r, Identity { model, disk: disk_name, ..id }))
}

/// A disk this engine opened that is not conventional, for `/debug/stalls`
/// and the flow-over's log (#282).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Shingled {
    pub path: String,
    pub disk: String,
    pub model: String,
    pub recording: Recording,
}

fn shingled() -> &'static std::sync::Mutex<Vec<Shingled>> {
    static S: std::sync::OnceLock<std::sync::Mutex<Vec<Shingled>>> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

/// Look at the disk under `path` once it is opened: a shingled one is said
/// (WARN) and remembered. Returns what it is.
pub fn note_recording(path: &str) -> Recording {
    let Some((r, id)) = recording_of(path) else {
        return Recording::Conventional;
    };
    if r != Recording::Conventional {
        let mut all = shingled().lock().unwrap_or_else(|e| e.into_inner());
        if !all.iter().any(|s| s.disk == id.disk) {
            tracing::warn!(
                "{path}: disk {} ({}) is {} — sustained writes (a flow-over) fill its cache and its flushes \
                 take seconds; the flow-over paces itself to its measured flush time",
                id.disk,
                id.model,
                match r {
                    Recording::HostManaged => "zoned, host-managed",
                    Recording::HostAware => "zoned, host-aware",
                    Recording::DriveManagedSmr => "drive-managed SMR (a known model)",
                    Recording::Conventional => "conventional",
                }
            );
            all.push(Shingled { path: path.to_string(), disk: id.disk, model: id.model, recording: r });
        }
    }
    r
}

/// Every shingled disk this engine has opened.
pub fn shingled_disks() -> Vec<Shingled> {
    shingled().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The serial in a VPD page 0x80 (Unit Serial Number): a 4-byte header whose
/// bytes 2..4 are the length, then that many ASCII bytes, space-padded.
fn vpd_serial(page: &[u8]) -> String {
    if page.len() < 4 || page[1] != 0x80 {
        return String::new();
    }
    let len = u16::from_be_bytes([page[2], page[3]]) as usize;
    let body = &page[4..page.len().min(4 + len)];
    String::from_utf8_lossy(body).trim().trim_matches('\0').to_string()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A sysfs tree the way the kernel lays it out: the class entry is a
    /// symlink into /sys/devices, and a partition's directory is inside its
    /// disk's.
    fn tree(root: &Path) {
        let disk = root.join("devices/pci0/host0/block/sda");
        std::fs::create_dir_all(disk.join("sda2")).unwrap();
        std::fs::create_dir_all(disk.join("device")).unwrap();
        std::fs::write(disk.join("sda2/partition"), "2\n").unwrap();
        std::fs::write(disk.join("device/model"), "WDC WD20EFRX-68E\n").unwrap();
        std::fs::write(disk.join("device/wwid"), "naa.50014ee2b5d3a1f0\n").unwrap();
        let mut pg80 = vec![0x00, 0x80, 0x00, 0x14];
        pg80.extend_from_slice(b"     WD-WX11D28JFS6T");
        std::fs::write(disk.join("device/vpd_pg80"), pg80).unwrap();

        let nvme = root.join("devices/pci1/nvme/nvme0/nvme0n1");
        std::fs::create_dir_all(nvme.join("nvme0n1p1")).unwrap();
        std::fs::write(nvme.join("nvme0n1p1/partition"), "1\n").unwrap();
        std::fs::create_dir_all(nvme.join("device")).unwrap();
        std::fs::write(nvme.join("wwid"), "eui.0025385b71b0c2d4\n").unwrap();
        std::fs::write(nvme.join("device/serial"), "S4EWNX0R123456  \n").unwrap();
        std::fs::write(nvme.join("device/model"), "Samsung SSD 970\n").unwrap();

        let class = root.join("class/block");
        std::fs::create_dir_all(&class).unwrap();
        for (name, target) in [
            ("sda", disk.clone()),
            ("sda2", disk.join("sda2")),
            ("nvme0n1", nvme.clone()),
            ("nvme0n1p1", nvme.join("nvme0n1p1")),
        ] {
            std::os::unix::fs::symlink(target, class.join(name)).unwrap();
        }
    }

    /// #282: a disk's recording from `queue/zoned`, else its model.
    #[test]
    fn a_shingled_disk_is_named_from_sysfs_or_its_model() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        let disk = dir.path().join("devices/pci0/host0/block/sda");
        // WD20EFRX is CMR: conventional, through the partition too.
        assert_eq!(recording_of_in(dir.path(), "sda2").unwrap().0, Recording::Conventional);
        // server3's disk: drive-managed SMR, reports itself not zoned.
        std::fs::write(disk.join("device/model"), "ST2000DM008-2FR102\n").unwrap();
        std::fs::create_dir_all(disk.join("queue")).unwrap();
        std::fs::write(disk.join("queue/zoned"), "none\n").unwrap();
        let (r, id) = recording_of_in(dir.path(), "sda2").unwrap();
        assert_eq!(r, Recording::DriveManagedSmr);
        assert_eq!(id.disk, "sda");
        // Zoned, whatever its model.
        std::fs::write(disk.join("queue/zoned"), "host-managed\n").unwrap();
        assert_eq!(recording_of_in(dir.path(), "sda").unwrap().0, Recording::HostManaged);
        std::fs::write(disk.join("queue/zoned"), "host-aware\n").unwrap();
        assert_eq!(recording_of_in(dir.path(), "sda").unwrap().0, Recording::HostAware);
        // An NVMe drive is conventional.
        assert_eq!(recording_of_in(dir.path(), "nvme0n1p1").unwrap().0, Recording::Conventional);
        assert!(drive_managed_smr("WDC WD40EFAX-68JH4N0"));
        assert!(!drive_managed_smr("WDC WD40EFRX-68N32N0"));
        assert!(!drive_managed_smr("ST2000DM0089"), "a word, not a prefix");
    }

    #[test]
    fn a_scsi_disk_and_its_partition_are_the_same_drive() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        let disk = of_in(dir.path(), "sda").unwrap();
        assert_eq!(disk.serial, "WD-WX11D28JFS6T", "the serial is VPD page 0x80");
        assert_eq!(disk.wwn, "naa.50014ee2b5d3a1f0");
        assert_eq!(disk.model, "WDC WD20EFRX-68E");
        assert_eq!(disk.disk, "sda");
        assert_eq!(of_in(dir.path(), "sda2").unwrap(), disk, "a partition is its disk");
    }

    #[test]
    fn an_nvme_namespace_and_its_partition_are_the_same_drive() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        let ns = of_in(dir.path(), "nvme0n1").unwrap();
        assert_eq!(ns.serial, "S4EWNX0R123456");
        assert_eq!(ns.wwn, "eui.0025385b71b0c2d4");
        assert_eq!(of_in(dir.path(), "nvme0n1p1").unwrap(), ns);
    }

    #[test]
    fn something_sysfs_does_not_know_has_no_identity() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        assert_eq!(of_in(dir.path(), "loop7"), None);
    }
    /// Against the real `/sys` of whatever runs the tests: every disk the
    /// kernel gives a serial or WWN is identified, and every partition on it
    /// is the same drive. Nothing to check on a machine with no such disk.
    #[test]
    fn the_live_sysfs_resolves_partitions_to_their_disks() {
        let sys = Path::new("/sys");
        let Ok(entries) = std::fs::read_dir(sys.join("class/block")) else { return };
        let mut checked = 0;
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let node = std::fs::canonicalize(e.path()).unwrap();
            if node.join("partition").exists() {
                continue;
            }
            let exposes = ["device/serial", "device/vpd_pg80", "device/wwid", "wwid"]
                .iter()
                .any(|f| std::fs::read(node.join(f)).map(|b| !b.iter().all(|c| c.is_ascii_whitespace() || *c == 0)).unwrap_or(false));
            let id = of_in(sys, &name);
            if !exposes {
                continue;
            }
            let id = id.unwrap_or_else(|| panic!("{name} exposes an identity and none was read"));
            println!("{name}: serial={:?} wwn={:?} model={:?}", id.serial, id.wwn, id.model);
            for p in std::fs::read_dir(&node).unwrap().flatten() {
                if p.path().join("partition").exists() {
                    let pname = p.file_name().to_string_lossy().to_string();
                    assert_eq!(of_in(sys, &pname).as_ref(), Some(&id), "{pname} is not {name}");
                    println!("  {pname}: same drive");
                }
            }
            checked += 1;
        }
        println!("{checked} disk(s) with an identity checked");
    }
}

