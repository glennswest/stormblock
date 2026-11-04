//! What became of the mount list at this boot (#532), from the file `/init`
//! leaves in `/run`, which survives the switch_root: a volume the kernel
//! refused leaves its mount point on the read-only image, and every service
//! on it fails far from the cause. Open health says so.

use serde::Serialize;

/// Where `/init` writes it.
pub const REPORT_PATH: &str = "/run/stormblock/boot-mounts";

/// One volume of the mount list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BootMount {
    pub device: String,
    pub mount: String,
    pub why: String,
}

/// Health's `boot_mounts`: absent when everything simply mounted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BootMounts {
    /// Not mounted: its services will not work.
    pub failed: Vec<BootMount>,
    /// Refused, repaired, then mounted.
    pub repaired: Vec<BootMount>,
}

fn parse(text: &str) -> BootMounts {
    let mut out = BootMounts::default();
    for line in text.lines() {
        let mut part = line.splitn(4, ' ');
        let (Some(what), Some(device), Some(mount)) = (part.next(), part.next(), part.next()) else {
            continue;
        };
        let entry = BootMount { device: device.into(), mount: mount.into(), why: part.next().unwrap_or("").into() };
        match what {
            "failed" => out.failed.push(entry),
            "repaired" => out.repaired.push(entry),
            _ => {}
        }
    }
    out
}

/// The report, when there is anything in it.
pub fn read() -> Option<BootMounts> {
    let path = std::env::var_os("STORMBLOCK_BOOT_MOUNTS_REPORT").unwrap_or_else(|| REPORT_PATH.into());
    let report = parse(&std::fs::read_to_string(path).ok()?);
    (!report.failed.is_empty() || !report.repaired.is_empty()).then_some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_init_writes_is_read_back() {
        let r = parse(
            "failed /dev/ublkb15 /d/stormblock the kernel refused it and it could not be repaired: x y\n\
             repaired /dev/ublkb16 /d/registry see the boot's console\n\
             nonsense\n",
        );
        assert_eq!(r.failed.len(), 1);
        assert_eq!(r.failed[0].mount, "/d/stormblock");
        assert_eq!(r.failed[0].why, "the kernel refused it and it could not be repaired: x y");
        assert_eq!(r.repaired[0].device, "/dev/ublkb16");
        assert_eq!(parse(""), BootMounts::default());
    }
}
