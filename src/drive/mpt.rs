//! The SAS card's own inventory: mpt3sas through `/dev/mpt3ctl` (#611, #595).
//!
//! Owner, #595: "I want you to use a direct api the the sas card"; #611: "we
//! need stormblock inventory to use the low level scan and then match to
//! linux devices". forge1's HBA330 had four drives and Linux made a disk of
//! one: the three NOT READY ones had no SCSI target, so nothing that reads
//! sysfs could see them. The card itself knows every attached device.
//!
//! So the rows come from the card, first: the `MPT3COMMAND` ioctl on the
//! driver's control device carries MPI2 config requests to the IOC, the same
//! pages `sas3ircu DISPLAY` reads:
//! - SAS IO Unit Page 0: every phy, its negotiated link, the attached handle;
//! - SAS Device Page 0, by handle (GET_NEXT_HANDLE): SAS address, enclosure
//!   handle and slot, device info (SSP/STP/SATA target, SEP, expander),
//!   access status, flags;
//! - Enclosure Page 0: the enclosure's logical id and its slots.
//!
//! Then each row is matched to Linux by SAS address (the SAS transport
//! class's `sas_address`): its SCSI device (H:C:T:L) and sd / sg nodes, or
//! none. A device with no SCSI device is asked INQUIRY and TEST UNIT READY
//! through the card's own SCSI pass-through (`MPI2_FUNCTION_SCSI_IO_REQUEST`
//! to its handle), so a row the kernel never made a target for still says
//! what the drive answers.
//!
//! The frame building and page parsing are plain functions over bytes (tested
//! here); only [`Ctl`] touches the device.

use std::path::Path;

// ---- MPI2 constants (mpi2.h, mpi2_cnfg.h, mpi2_init.h) ----

const MPI2_FUNCTION_SCSI_IO_REQUEST: u8 = 0x00;
const MPI2_FUNCTION_CONFIG: u8 = 0x04;
const MPI2_CONFIG_ACTION_PAGE_HEADER: u8 = 0x00;
const MPI2_CONFIG_ACTION_PAGE_READ_CURRENT: u8 = 0x01;
const MPI2_CONFIG_PAGETYPE_EXTENDED: u8 = 0x0F;
pub const EXTPAGE_SAS_IO_UNIT: u8 = 0x10;
pub const EXTPAGE_SAS_DEVICE: u8 = 0x12;
pub const EXTPAGE_ENCLOSURE: u8 = 0x15;
const IOCSTATUS_MASK: u16 = 0x7FFF;
const IOCSTATUS_SUCCESS: u16 = 0x0000;
const IOCSTATUS_CONFIG_INVALID_PAGE: u16 = 0x0022;
const IOCSTATUS_SCSI_DATA_UNDERRUN: u16 = 0x0045;
/// The drive answered and the card recovered something on the way: still
/// the drive's answer.
const IOCSTATUS_SCSI_RECOVERED_ERROR: u16 = 0x0040;
const SCSIIO_CONTROL_READ: u32 = 0x0200_0000;
const SCSIIO_CONTROL_WRITE: u32 = 0x0100_0000;
const SCSI_STATE_AUTOSENSE_VALID: u8 = 0x01;

/// The config request's SGL offset (`PageBufferSGE`, 0x1C), in dwords.
pub const CONFIG_SGE_DWORDS: u32 = 0x1C / 4;
/// The SCSI IO request's SGL offset (`SGL`, 0x60), in dwords.
pub const SCSIIO_SGE_DWORDS: u32 = 0x60 / 4;

fn le16(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).unwrap_or(0)
}
fn le32(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).unwrap_or(0)
}
fn le64(b: &[u8], o: usize) -> u64 {
    ((le32(b, o + 4) as u64) << 32) | (le32(b, o) as u64)
}

/// A config request frame (`MPI2_CONFIG_REQUEST`, 0x1C bytes before its SGE)
/// for an extended page. `version` and `ext_len` are 0 for the header action
/// and what the header said for a read.
pub fn config_request(action: u8, ext_type: u8, page: u8, address: u32, version: u8, ext_len: u16) -> Vec<u8> {
    let mut f = vec![0u8; (CONFIG_SGE_DWORDS * 4) as usize];
    f[0x00] = action;
    f[0x03] = MPI2_FUNCTION_CONFIG;
    f[0x04..0x06].copy_from_slice(&ext_len.to_le_bytes());
    f[0x06] = ext_type;
    // Header: PageVersion, PageLength (0 for extended), PageNumber, PageType.
    f[0x14] = version;
    f[0x16] = page;
    f[0x17] = MPI2_CONFIG_PAGETYPE_EXTENDED;
    f[0x18..0x1C].copy_from_slice(&address.to_le_bytes());
    f
}

/// A config reply's IOCStatus, extended page length (dwords) and page version.
pub fn config_reply(r: &[u8]) -> (u16, u16, u8) {
    (le16(r, 0x0E) & IOCSTATUS_MASK, le16(r, 0x04), r.get(0x14).copied().unwrap_or(0))
}

/// A SCSI IO request frame (`MPI2_SCSI_IO_REQUEST`, 0x60 bytes before its
/// SGL) to a device handle, LUN 0, reading `len` bytes (0: no data).
pub fn scsi_io_request(handle: u16, cdb: &[u8], len: u32) -> Vec<u8> {
    scsi_io_request_dir(handle, cdb, len, false)
}

/// The same, writing `len` bytes to the drive when `out`.
pub fn scsi_io_request_dir(handle: u16, cdb: &[u8], len: u32, out: bool) -> Vec<u8> {
    let mut f = vec![0u8; (SCSIIO_SGE_DWORDS * 4) as usize];
    f[0x00..0x02].copy_from_slice(&handle.to_le_bytes());
    f[0x03] = MPI2_FUNCTION_SCSI_IO_REQUEST;
    f[0x14] = SCSIIO_SGE_DWORDS as u8; // SGLOffset0
    f[0x1C..0x20].copy_from_slice(&len.to_le_bytes());
    f[0x24..0x26].copy_from_slice(&(cdb.len() as u16).to_le_bytes()); // IoFlags: CDB length
    let control = match (len > 0, out) {
        (false, _) => 0,
        (true, false) => SCSIIO_CONTROL_READ,
        (true, true) => SCSIIO_CONTROL_WRITE,
    };
    f[0x3C..0x40].copy_from_slice(&control.to_le_bytes());
    f[0x40..0x40 + cdb.len().min(32)].copy_from_slice(&cdb[..cdb.len().min(32)]);
    f
}

/// One phy of the card (SAS IO Unit Page 0's `PhyData`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phy {
    pub num: u8,
    pub port: u8,
    /// The logical negotiated rate (high nibble of `NegotiatedLinkRate`).
    pub rate: u8,
    pub attached: u16,
}

impl Phy {
    pub fn linked(&self) -> bool {
        (0x8..=0xC).contains(&self.rate)
    }
    pub fn rate_name(&self) -> &'static str {
        match self.rate {
            0x0 => "Unknown",
            0x1 => "Disabled",
            0x2 => "negotiation failed",
            0x3 => "SATA OOB complete",
            0x4 => "port selector",
            0x5 => "SMP reset in progress",
            0x6 => "unsupported phy",
            0x8 => "1.5 Gbit",
            0x9 => "3.0 Gbit",
            0xA => "6.0 Gbit",
            0xB => "12.0 Gbit",
            0xC => "22.5 Gbit",
            _ => "?",
        }
    }
}

/// SAS IO Unit Page 0 -> its phys.
pub fn parse_sas_io_unit0(p: &[u8]) -> Vec<Phy> {
    let n = p.get(0x0C).copied().unwrap_or(0) as usize;
    (0..n)
        .filter_map(|i| {
            let o = 0x10 + i * 0x14;
            p.get(o..o + 0x14).map(|d| Phy { num: i as u8, port: d[0], rate: d[3] >> 4, attached: le16(d, 0x08) })
        })
        .collect()
}

/// One device the card lists (SAS Device Page 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub handle: u16,
    pub sas_address: u64,
    pub enclosure: u16,
    pub slot: u16,
    pub parent: u16,
    pub phy: u8,
    pub access_status: u8,
    pub info: u32,
    pub flags: u16,
    pub device_name: u64,
}

const DI_TYPE_MASK: u32 = 0x7;
const DI_END_DEVICE: u32 = 0x1;
const DI_SATA_DEVICE: u32 = 0x80;
const DI_STP_TARGET: u32 = 0x200;
const DI_SSP_TARGET: u32 = 0x400;
const DI_SEP: u32 = 0x4000;
const DEV_FLAGS_PRESENT: u16 = 0x0001;

impl Device {
    /// An end device that is a drive: an SSP, STP or SATA target, not the
    /// enclosure's own processor (SEP) and not an expander.
    pub fn is_drive(&self) -> bool {
        self.info & DI_TYPE_MASK == DI_END_DEVICE
            && self.info & DI_SEP == 0
            && self.info & (DI_SSP_TARGET | DI_STP_TARGET | DI_SATA_DEVICE) != 0
    }
    pub fn is_expander(&self) -> bool {
        matches!(self.info & DI_TYPE_MASK, 2 | 3)
    }
    pub fn present(&self) -> bool {
        self.flags & DEV_FLAGS_PRESENT != 0
    }
    pub fn kind(&self) -> &'static str {
        if self.is_expander() {
            "expander"
        } else if self.info & DI_SEP != 0 {
            "enclosure processor (SEP)"
        } else if self.info & DI_SSP_TARGET != 0 {
            "SSP target"
        } else if self.info & (DI_SATA_DEVICE | DI_STP_TARGET) != 0 {
            "SATA target"
        } else if self.info & DI_TYPE_MASK == DI_END_DEVICE {
            "end device"
        } else {
            "?"
        }
    }
    /// `AccessStatus` in the card's words (mpi2_cnfg.h), None when 0.
    pub fn access(&self) -> Option<String> {
        Some(
            match self.access_status {
                0x00 => return None,
                0x01 => "SATA needs initialization",
                0x02 => "SATA capability failed",
                0x03 => "SATA affiliation conflict",
                0x04 => "route not addressable",
                0x05 => "SMP error, not addressable",
                0x06 => "device blocked",
                0x10..=0x1F => "SATA initialization failed",
                _ => return Some(format!("access status 0x{:02x}", self.access_status)),
            }
            .to_string(),
        )
    }
}

/// SAS Device Page 0 -> the device.
pub fn parse_sas_device0(p: &[u8]) -> Device {
    Device {
        slot: le16(p, 0x08),
        enclosure: le16(p, 0x0A),
        sas_address: le64(p, 0x0C),
        parent: le16(p, 0x14),
        phy: p.get(0x16).copied().unwrap_or(0),
        access_status: p.get(0x17).copied().unwrap_or(0),
        handle: le16(p, 0x18),
        info: le32(p, 0x1C),
        flags: le16(p, 0x20),
        device_name: le64(p, 0x24),
    }
}

/// An enclosure the card knows (Enclosure Page 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enclosure {
    pub handle: u16,
    pub logical_id: u64,
    pub num_slots: u16,
    pub start_slot: u16,
}

pub fn parse_enclosure0(p: &[u8]) -> Enclosure {
    Enclosure {
        logical_id: le64(p, 0x0C),
        handle: le16(p, 0x16),
        num_slots: le16(p, 0x18),
        start_slot: le16(p, 0x1A),
    }
}

/// A SCSI answer through the card: SCSI status and sense key / ASC / ASCQ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub ioc_status: u16,
    pub scsi_status: u8,
    pub sense: Option<(u8, u8, u8)>,
    pub data: Vec<u8>,
}

/// Fixed (0x70/0x71) or descriptor (0x72/0x73) sense -> key, ASC, ASCQ.
pub fn parse_sense(s: &[u8]) -> Option<(u8, u8, u8)> {
    match s.first()? & 0x7F {
        0x70 | 0x71 if s.len() >= 14 => Some((s[2] & 0x0F, s[12], s[13])),
        0x72 | 0x73 if s.len() >= 4 => Some((s[1] & 0x0F, s[2], s[3])),
        _ => None,
    }
}

pub fn sense_key_name(k: u8) -> &'static str {
    match k {
        0x0 => "NO SENSE",
        0x1 => "RECOVERED ERROR",
        0x2 => "NOT READY",
        0x3 => "MEDIUM ERROR",
        0x4 => "HARDWARE ERROR",
        0x5 => "ILLEGAL REQUEST",
        0x6 => "UNIT ATTENTION",
        0x7 => "DATA PROTECT",
        0xB => "ABORTED COMMAND",
        _ => "sense",
    }
}

impl Answer {
    pub fn said(&self) -> String {
        if let Some((k, a, q)) = self.sense {
            return format!("{} asc/ascq 0x{a:02x}/0x{q:02x}", sense_key_name(k));
        }
        match (self.ioc_status, self.scsi_status) {
            (IOCSTATUS_SUCCESS | IOCSTATUS_SCSI_DATA_UNDERRUN, 0) => "good".to_string(),
            (IOCSTATUS_SUCCESS | IOCSTATUS_SCSI_DATA_UNDERRUN, s) => format!("SCSI status 0x{s:02x}"),
            (i, _) => format!("IOCStatus 0x{i:04x}"),
        }
    }
}

/// INQUIRY data -> "vendor product revision".
pub fn inquiry_name(d: &[u8]) -> Option<String> {
    if d.len() < 36 {
        return None;
    }
    let t = |r: std::ops::Range<usize>| String::from_utf8_lossy(&d[r]).trim().to_string();
    Some(format!("{} {} {}", t(8..16), t(16..32), t(32..36)).split_whitespace().collect::<Vec<_>>().join(" "))
}

/// What Linux made of a row (#611: the second column, never the source of
/// presence), found by SAS address under the SAS transport class.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Linux {
    /// The end device (`end_device-H:N`), when the transport class has it.
    pub end_device: Option<String>,
    /// `H:C:T:L`, when a SCSI device was made.
    pub scsi: Option<String>,
    pub block: Option<String>,
    pub sg: Option<String>,
    /// The block device's size in 512-byte sectors.
    pub sectors: Option<u64>,
}

impl Linux {
    /// The row's state (#611): `ready` (a disk with capacity), `not ready`
    /// (a SCSI device and no readable format: no disk, or one of 0 bytes),
    /// `no target` (the kernel made no SCSI device yet).
    pub fn state(&self) -> &'static str {
        match (&self.scsi, &self.block, self.sectors) {
            (None, _, _) => "no target",
            (Some(_), Some(_), Some(n)) if n > 0 => "ready",
            _ => "not ready",
        }
    }
    pub fn said(&self) -> String {
        match &self.scsi {
            None => match &self.end_device {
                Some(e) => format!("{e}, no SCSI target yet"),
                None => "no SCSI target yet (not in the SAS transport class)".to_string(),
            },
            Some(s) => {
                let mut v = vec![s.clone()];
                v.extend(self.block.clone());
                v.extend(self.sg.clone());
                if self.block.is_none() {
                    v.push("(no block device)".to_string());
                }
                v.join(" ")
            }
        }
    }
}

fn first_entry(dir: &Path, prefix: &str) -> Option<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with(prefix))
        .collect();
    v.sort();
    v.into_iter().next()
}

/// Finds the row's Linux devices by SAS address under `sys` (`/sys`).
pub fn match_linux(sys: &Path, sas_address: u64) -> Linux {
    let mut l = Linux::default();
    let Ok(rd) = std::fs::read_dir(sys.join("class/sas_device")) else { return l };
    for e in rd.flatten() {
        let p = e.path();
        let a = std::fs::read_to_string(p.join("sas_address")).unwrap_or_default();
        let a = a.trim().trim_start_matches("0x");
        if u64::from_str_radix(a, 16).ok() != Some(sas_address) {
            continue;
        }
        // class/sas_device/X -> .../end_device-H:N/sas_device/end_device-H:N
        let Ok(real) = std::fs::canonicalize(&p) else { continue };
        let Some(dev) = real.parent().and_then(Path::parent) else { continue };
        l.end_device = dev.file_name().map(|n| n.to_string_lossy().into_owned());
        let Some(t) = first_entry(dev, "target") else { return l };
        let tdir = dev.join(&t);
        let Some(s) = std::fs::read_dir(&tdir).ok().and_then(|rd| {
            let mut v: Vec<String> = rd
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.split(':').count() == 4)
                .collect();
            v.sort();
            v.into_iter().next()
        }) else {
            return l;
        };
        let sdir = tdir.join(&s);
        l.scsi = Some(s);
        l.block = first_entry(&sdir.join("block"), "");
        l.sg = first_entry(&sdir.join("scsi_generic"), "sg");
        if let Some(b) = &l.block {
            l.sectors = std::fs::read_to_string(sdir.join("block").join(b).join("size"))
                .ok()
                .and_then(|s| s.trim().parse().ok());
        }
        return l;
    }
    l
}

/// One row of the card's inventory: what the card lists, then Linux.
#[derive(Debug, Clone)]
pub struct Row {
    pub device: Device,
    pub enclosure: Option<Enclosure>,
    pub linux: Linux,
    /// INQUIRY and TEST UNIT READY through the card, when Linux has no
    /// SCSI device for it.
    pub probe: Option<(Option<String>, String)>,
}

/// A card's whole view.
#[derive(Debug, Clone)]
pub struct Card {
    pub ioc: u32,
    pub ctl: String,
    pub pci: String,
    pub host: String,
    pub phys: Vec<Phy>,
    pub devices: Vec<Device>,
    pub enclosures: Vec<Enclosure>,
    pub rows: Vec<Row>,
}

fn hex_addr(a: u64) -> String {
    format!("0x{a:016x}")
}

impl Card {
    /// The drive rows: end devices that are drives.
    pub fn drives(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter().filter(|r| r.device.is_drive())
    }

    /// The console lines (#595 step 2), each to be indented by the caller.
    pub fn lines(&self) -> Vec<String> {
        let linked = self.phys.iter().filter(|p| p.linked()).count();
        let ndrives = self.drives().count();
        let nexp = self.devices.iter().filter(|d| d.is_expander()).count();
        let mut out = vec![format!(
            "card: ioc {} ({}) {} {}: {} phys, {} with a link, {} drive(s), {} expander(s), {} enclosure(s) (the card's own pages)",
            self.ioc, self.ctl, self.pci, self.host, self.phys.len(), linked, ndrives, nexp, self.enclosures.len()
        )];
        for e in &self.enclosures {
            out.push(format!(
                "card: enclosure {} handle 0x{:04x}: {} slot(s) from {}",
                hex_addr(e.logical_id), e.handle, e.num_slots, e.start_slot
            ));
        }
        for p in self.phys.iter().filter(|p| p.linked()) {
            let what = match self.devices.iter().find(|d| d.handle == p.attached) {
                Some(d) => format!("device 0x{:04x} sas {} ({})", d.handle, hex_addr(d.sas_address), d.kind()),
                None if p.attached == 0 => "nothing attached".to_string(),
                None => format!("handle 0x{:04x}, no SAS Device page", p.attached),
            };
            out.push(format!("card: phy {}: {}, port {}, {}", p.num, p.rate_name(), p.port, what));
        }
        let mut down: Vec<(&str, Vec<String>)> = Vec::new();
        for p in self.phys.iter().filter(|p| !p.linked()) {
            match down.iter_mut().find(|(n, _)| *n == p.rate_name()) {
                Some((_, v)) => v.push(p.num.to_string()),
                None => down.push((p.rate_name(), vec![p.num.to_string()])),
            }
        }
        for (n, v) in down {
            out.push(format!("card: no link, {n}: phy {}", v.join(" ")));
        }
        for r in self.drives() {
            let d = &r.device;
            let at = match &r.enclosure {
                Some(e) => format!("enclosure {} slot {}", hex_addr(e.logical_id), d.slot),
                None => format!("slot {}", d.slot),
            };
            let mut s = format!(
                "card: drive sas {} {}, {}, phy {}, {}: {} - {}",
                hex_addr(d.sas_address), at, d.kind(), d.phy,
                if d.present() { "present" } else { "NOT present" },
                r.linux.state(), r.linux.said()
            );
            if let Some(a) = d.access() {
                s.push_str(&format!("; the card: {a}"));
            }
            if let Some((inq, tur)) = &r.probe {
                s.push_str(&format!(
                    "; through the card: INQUIRY {}, TEST UNIT READY {}",
                    inq.as_deref().unwrap_or("no answer"), tur
                ));
            }
            out.push(s);
        }
        out
    }

    /// The drive rows as JSON (#611: `card_rows` in local-disk.json, the
    /// engine's `GET /api/v1/storage/hba`).
    pub fn rows_json(&self) -> Vec<serde_json::Value> {
        self.drives()
            .map(|r| {
                let d = &r.device;
                let phy = self.phys.iter().find(|p| p.attached == d.handle);
                serde_json::json!({
                    "pci": self.pci,
                    "host": self.host,
                    "ioc": self.ioc,
                    "sas_address": hex_addr(d.sas_address),
                    "handle": d.handle,
                    "enclosure": r.enclosure.as_ref().map(|e| hex_addr(e.logical_id)),
                    "slot": d.slot,
                    "phy": d.phy,
                    "link": phy.map(Phy::rate_name),
                    "kind": d.kind(),
                    "present": d.present(),
                    "access": d.access(),
                    "state": r.linux.state(),
                    "end_device": r.linux.end_device,
                    "scsi": r.linux.scsi,
                    "block": r.linux.block,
                    "sg": r.linux.sg,
                    "bytes": r.linux.sectors.map(|n| n * 512),
                    "card_inquiry": r.probe.as_ref().and_then(|p| p.0.clone()),
                    "card_test_unit_ready": r.probe.as_ref().map(|p| p.1.clone()),
                })
            })
            .collect()
    }
}

/// A SAS address as given on the command line or in a path
/// (`0x5000cca2…`, `5000cca2…`).
pub fn parse_addr(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim().trim_start_matches("sas-").trim_start_matches("0x"), 16).ok()
}

/// Runs `f` on the drive with SAS address `sas`: through its sg node when
/// the kernel made one, else through the card to its handle.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub fn with_drive<R>(dev: &Path, sys: &Path, sas: u64, f: impl FnOnce(&dyn crate::drive::scsi::Transport) -> R) -> Result<R, String> {
    let ctls = Ctl::open(dev).map_err(|e| format!("the mpt control device: {e}"))?;
    for (ioc, pci, host) in mpt_hosts(sys) {
        for c in &ctls {
            let Ok(card) = c.card(ioc, &pci, &host, sys, false) else { continue };
            let Some(r) = card.drives().find(|r| r.device.sas_address == sas) else { continue };
            if let Some(sg) = &r.linux.sg {
                if let Ok(t) = crate::drive::scsi::Sg::open(&dev.join(sg)) {
                    return Ok(f(&t));
                }
            }
            return Ok(f(&CardDrive { ctl: c, ioc, handle: r.device.handle }));
        }
    }
    Err(format!("no mpt card lists a drive with SAS address {}", hex_addr(sas)))
}

/// A link reset on the phy the card says the drive is on (#620): written to
/// `/sys/class/sas_phy/phy-<host>:<phy>/link_reset`. Answers the phy's name.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub fn link_reset_for(dev: &Path, sys: &Path, sas: u64) -> Option<String> {
    let (cards, _) = cards(dev, sys, false);
    for c in &cards {
        let Some(r) = c.drives().find(|r| r.device.sas_address == sas) else { continue };
        let n = c.host.trim_start_matches("host");
        let name = format!("phy-{n}:{}", r.device.phy);
        let p = sys.join("class/sas_phy").join(&name).join("link_reset");
        if std::fs::write(&p, "1").is_ok() {
            return Some(name);
        }
    }
    None
}

/// After a format (#611: "linux needs to find them without a reboot"):
/// the kernel is asked to look at the drive again. Its SCSI device is
/// rescanned when it has one (and deleted first when its disk reads 0
/// bytes, so sd makes it anew); every mpt host is scanned otherwise.
pub fn rescan_for(sys: &Path, sas: u64) {
    let l = match_linux(sys, sas);
    if let Some(scsi) = &l.scsi {
        let dev = sys.join("bus/scsi/devices").join(scsi);
        if l.block.is_some() && l.sectors.unwrap_or(0) > 0 {
            let _ = std::fs::write(dev.join("rescan"), "1");
            return;
        }
        let _ = std::fs::write(dev.join("delete"), "1");
    }
    for (_, _, host) in mpt_hosts(sys) {
        let _ = std::fs::write(sys.join("class/scsi_host").join(host).join("scan"), "- - -");
    }
}

// ---- the device ----

/// `struct mpt3_ioctl_command` (mpt3sas_ctl.h) on a 64-bit kernel, with room
/// for the message frame the driver copies from `mf` (data_sge_offset dwords).
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
#[repr(C)]
#[allow(dead_code)] // read by the driver, not by us
struct Mpt3Command {
    ioc_number: u32,
    port_number: u32,
    max_data_size: u32,
    timeout: u32,
    reply_frame_buf_ptr: u64,
    data_in_buf_ptr: u64,
    data_out_buf_ptr: u64,
    sense_data_ptr: u64,
    max_reply_bytes: u32,
    data_in_size: u32,
    data_out_size: u32,
    max_sense_bytes: u32,
    data_sge_offset: u32,
    mf: [u8; 128],
}

/// `_IOWR('L', 20, struct mpt3_ioctl_command)`: the driver checks the size
/// encoded here against its own struct (72 bytes, `mf[1]` padded).
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
const MPT3COMMAND: u64 = (3 << 30) | (72 << 16) | ((b'L' as u64) << 8) | 20;

/// The driver's control device, open.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub struct Ctl {
    file: std::fs::File,
    pub name: String,
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
impl Ctl {
    /// `/dev/mpt3ctl` (SAS3 cards), else `/dev/mpt2ctl` (SAS2).
    pub fn open(dev: &Path) -> std::io::Result<Vec<Ctl>> {
        let mut v = Vec::new();
        let mut last = None;
        for n in ["mpt3ctl", "mpt2ctl"] {
            match std::fs::OpenOptions::new().read(true).write(true).open(dev.join(n)) {
                Ok(file) => v.push(Ctl { file, name: n.to_string() }),
                Err(e) => last = Some(e),
            }
        }
        if v.is_empty() {
            return Err(last.unwrap_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound)));
        }
        Ok(v)
    }

    /// One request to IOC `ioc`: the frame, its SGE offset, a buffer to read
    /// into. -> the reply frame and the sense data.
    pub fn command(&self, ioc: u32, frame: &[u8], sge_dwords: u32, data_in: &mut [u8], timeout: u32) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
        self.command_io(ioc, frame, sge_dwords, data_in, &[], timeout)
    }

    /// The same, with data to the drive as well.
    pub fn command_io(&self, ioc: u32, frame: &[u8], sge_dwords: u32, data_in: &mut [u8], data_out: &[u8], timeout: u32) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
        use std::os::fd::AsRawFd;
        let mut reply = vec![0u8; 128];
        let mut sense = vec![0u8; 96];
        let mut c = Mpt3Command {
            ioc_number: ioc,
            port_number: 0,
            max_data_size: 0,
            timeout,
            reply_frame_buf_ptr: reply.as_mut_ptr() as u64,
            data_in_buf_ptr: if data_in.is_empty() { 0 } else { data_in.as_mut_ptr() as u64 },
            data_out_buf_ptr: if data_out.is_empty() { 0 } else { data_out.as_ptr() as u64 },
            sense_data_ptr: sense.as_mut_ptr() as u64,
            max_reply_bytes: reply.len() as u32,
            data_in_size: data_in.len() as u32,
            data_out_size: data_out.len() as u32,
            max_sense_bytes: sense.len() as u32,
            data_sge_offset: sge_dwords,
            mf: [0u8; 128],
        };
        let n = frame.len().min(c.mf.len());
        c.mf[..n].copy_from_slice(&frame[..n]);
        // SAFETY: `c` is the driver's struct with the frame after it; every
        // pointer in it names a live buffer of the size given beside it.
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), MPT3COMMAND as libc::Ioctl, &mut c) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((reply, sense))
    }

    /// An extended config page, or None when the card says there is no such
    /// page (the end of a GET_NEXT_HANDLE walk).
    pub fn ext_page(&self, ioc: u32, ext_type: u8, page: u8, address: u32) -> std::io::Result<Option<Vec<u8>>> {
        let f = config_request(MPI2_CONFIG_ACTION_PAGE_HEADER, ext_type, page, address, 0, 0);
        let (r, _) = self.command(ioc, &f, CONFIG_SGE_DWORDS, &mut [], 10)?;
        let (st, len, ver) = config_reply(&r);
        if st == IOCSTATUS_CONFIG_INVALID_PAGE {
            return Ok(None);
        }
        if st != IOCSTATUS_SUCCESS || len == 0 {
            return Err(std::io::Error::other(format!("config page 0x{ext_type:02x}/{page} header: IOCStatus 0x{st:04x}")));
        }
        let mut buf = vec![0u8; len as usize * 4];
        let f = config_request(MPI2_CONFIG_ACTION_PAGE_READ_CURRENT, ext_type, page, address, ver, len);
        let (r, _) = self.command(ioc, &f, CONFIG_SGE_DWORDS, &mut buf, 10)?;
        let (st, _, _) = config_reply(&r);
        if st == IOCSTATUS_CONFIG_INVALID_PAGE {
            return Ok(None);
        }
        if st != IOCSTATUS_SUCCESS {
            return Err(std::io::Error::other(format!("config page 0x{ext_type:02x}/{page} read: IOCStatus 0x{st:04x}")));
        }
        Ok(Some(buf))
    }

    /// Every page of a GET_NEXT_HANDLE walk (SAS Device 0, Enclosure 0),
    /// with the handle offset in each page.
    fn walk(&self, ioc: u32, ext_type: u8, handle_at: usize) -> std::io::Result<Vec<Vec<u8>>> {
        let mut v = Vec::new();
        let mut h: u32 = 0xFFFF;
        for _ in 0..1024 {
            let Some(p) = self.ext_page(ioc, ext_type, 0, h)? else { break };
            let next = le16(&p, handle_at) as u32;
            v.push(p);
            if next == h || next == 0xFFFF {
                break;
            }
            h = next;
        }
        Ok(v)
    }

    /// A SCSI command with data in, through the card to a device handle.
    pub fn scsi(&self, ioc: u32, handle: u16, cdb: &[u8], len: usize) -> std::io::Result<Answer> {
        let mut data = vec![0u8; len];
        let f = scsi_io_request(handle, cdb, len as u32);
        let (r, sense) = self.command(ioc, &f, SCSIIO_SGE_DWORDS, &mut data, 30)?;
        let state = r.get(0x0D).copied().unwrap_or(0);
        Ok(Answer {
            ioc_status: le16(&r, 0x0E) & IOCSTATUS_MASK,
            scsi_status: r.get(0x0C).copied().unwrap_or(0),
            sense: if state & SCSI_STATE_AUTOSENSE_VALID != 0 { parse_sense(&sense) } else { None },
            data,
        })
    }

    /// The card's view: its phys, devices and enclosures, each drive matched
    /// to Linux under `sys`; one with no SCSI device probed through the card
    /// when `probe`.
    pub fn card(&self, ioc: u32, pci: &str, host: &str, sys: &Path, probe: bool) -> std::io::Result<Card> {
        let io0 = self
            .ext_page(ioc, EXTPAGE_SAS_IO_UNIT, 0, 0)?
            .ok_or_else(|| std::io::Error::other("no SAS IO Unit Page 0"))?;
        let phys = parse_sas_io_unit0(&io0);
        let devices: Vec<Device> = self.walk(ioc, EXTPAGE_SAS_DEVICE, 0x18)?.iter().map(Vec::as_slice).map(parse_sas_device0).collect();
        let enclosures: Vec<Enclosure> = self.walk(ioc, EXTPAGE_ENCLOSURE, 0x16)?.iter().map(Vec::as_slice).map(parse_enclosure0).collect();
        let rows = devices
            .iter()
            .filter(|d| d.is_drive())
            .map(|d| {
                let linux = match_linux(sys, d.sas_address);
                let probe = (probe && linux.scsi.is_none()).then(|| {
                    let inq = self.scsi(ioc, d.handle, &[0x12, 0, 0, 0, 96, 0], 96).ok().and_then(|a| inquiry_name(&a.data));
                    let tur = match self.scsi(ioc, d.handle, &[0, 0, 0, 0, 0, 0], 0) {
                        Ok(a) => a.said(),
                        Err(e) => format!("failed: {e}"),
                    };
                    (inq, tur)
                });
                Row {
                    device: d.clone(),
                    enclosure: enclosures.iter().find(|e| e.handle == d.enclosure).cloned(),
                    linux,
                    probe,
                }
            })
            .collect();
        Ok(Card { ioc, ctl: self.name.clone(), pci: pci.to_string(), host: host.to_string(), phys, devices, enclosures, rows })
    }
}

/// A drive reached through the card's pass-through to its device handle
/// (#611): one the kernel made no target for still answers.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub struct CardDrive<'a> {
    pub ctl: &'a Ctl,
    pub ioc: u32,
    pub handle: u16,
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
impl crate::drive::scsi::Transport for CardDrive<'_> {
    fn run(&self, cdb: &[u8], dir: crate::drive::scsi::Dir, data: &mut [u8], timeout_s: u32) -> std::io::Result<crate::drive::scsi::Reply> {
        use crate::drive::scsi::Dir;
        let out = dir == Dir::Out;
        let len = if dir == Dir::None { 0 } else { data.len() };
        let f = scsi_io_request_dir(self.handle, cdb, len as u32, out);
        let (r, sense) = if out {
            self.ctl.command_io(self.ioc, &f, SCSIIO_SGE_DWORDS, &mut [], data, timeout_s.max(10))?
        } else if len > 0 {
            self.ctl.command(self.ioc, &f, SCSIIO_SGE_DWORDS, data, timeout_s.max(10))?
        } else {
            self.ctl.command(self.ioc, &f, SCSIIO_SGE_DWORDS, &mut [], timeout_s.max(10))?
        };
        let ioc_status = le16(&r, 0x0E) & IOCSTATUS_MASK;
        let state = r.get(0x0D).copied().unwrap_or(0);
        let transfer = le32(&r, 0x14) as usize;
        // A context reply (no reply frame) is a success with every byte moved.
        let replied = r.iter().any(|b| *b != 0);
        let mut sense = if state & SCSI_STATE_AUTOSENSE_VALID != 0 { sense } else { Vec::new() };
        let sc = le32(&r, 0x18) as usize;
        if sc > 0 && sc < sense.len() {
            sense.truncate(sc);
        }
        Ok(crate::drive::scsi::Reply {
            status: r.get(0x0C).copied().unwrap_or(0),
            sense,
            len: if replied && !out { transfer.min(len) } else { len },
            transport: (!matches!(ioc_status, IOCSTATUS_SUCCESS | IOCSTATUS_SCSI_DATA_UNDERRUN | IOCSTATUS_SCSI_RECOVERED_ERROR))
                .then(|| format!("the card: IOCStatus 0x{ioc_status:04x}")),
        })
    }
    fn name(&self) -> String {
        format!("the card ({} ioc {}), handle 0x{:04x}", self.ctl.name, self.ioc, self.handle)
    }
}

/// The mpt2sas/mpt3sas SCSI hosts under `sys`: (ioc number, PCI function,
/// hostN). The driver sets each host's `unique_id` to its IOC number.
pub fn mpt_hosts(sys: &Path) -> Vec<(u32, String, String)> {
    let mut v = Vec::new();
    let Ok(rd) = std::fs::read_dir(sys.join("class/scsi_host")) else { return v };
    for e in rd.flatten() {
        let p = e.path();
        let proc_name = std::fs::read_to_string(p.join("proc_name")).unwrap_or_default();
        if !matches!(proc_name.trim(), "mpt3sas" | "mpt2sas") {
            continue;
        }
        let Some(id) = std::fs::read_to_string(p.join("unique_id")).ok().and_then(|s| s.trim().parse().ok()) else { continue };
        let pci = std::fs::canonicalize(&p).ok().and_then(|r| pci_of(&r)).unwrap_or_else(|| "?".to_string());
        v.push((id, pci, e.file_name().to_string_lossy().into_owned()));
    }
    v.sort();
    v
}

/// The PCI function closest to a sysfs path (`DDDD:BB:DD.F`).
pub fn pci_of(p: &Path) -> Option<String> {
    p.ancestors()
        .filter_map(|a| a.file_name()?.to_str())
        .find(|n| {
            let b = n.as_bytes();
            b.len() == 12 && b[4] == b':' && b[7] == b':' && b[10] == b'.'
        })
        .map(str::to_string)
}

/// Every mpt card's view (`/dev`, `/sys`). A card one control device does
/// not take (an SAS2 IOC on mpt3ctl) is tried on the other.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub fn cards(dev: &Path, sys: &Path, probe: bool) -> (Vec<Card>, Vec<String>) {
    let mut cards = Vec::new();
    let mut errs = Vec::new();
    let ctls = match Ctl::open(dev) {
        Ok(c) => c,
        Err(e) => {
            if !mpt_hosts(sys).is_empty() {
                errs.push(format!("card: the mpt control device cannot be opened: {e}"));
            }
            return (cards, errs);
        }
    };
    for (ioc, pci, host) in mpt_hosts(sys) {
        let mut last = None;
        let mut got = false;
        for c in &ctls {
            match c.card(ioc, &pci, &host, sys, probe) {
                Ok(card) => {
                    cards.push(card);
                    got = true;
                    break;
                }
                Err(e) => last = Some(format!("{}: {e}", c.name)),
            }
        }
        if !got {
            errs.push(format!("card: ioc {ioc} {pci} {host}: its pages cannot be read ({})", last.unwrap_or_default()));
        }
    }
    (cards, errs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn config_request_frame() {
        let f = config_request(MPI2_CONFIG_ACTION_PAGE_READ_CURRENT, EXTPAGE_SAS_DEVICE, 0, 0xFFFF, 9, 0x11);
        assert_eq!(f.len(), 28);
        assert_eq!(f[0], 1);
        assert_eq!(f[3], 0x04);
        assert_eq!(le16(&f, 4), 0x11);
        assert_eq!(f[6], 0x12);
        assert_eq!((f[0x14], f[0x16], f[0x17]), (9, 0, 0x0F));
        assert_eq!(le32(&f, 0x18), 0xFFFF);
    }

    #[test]
    fn config_reply_fields() {
        let mut r = vec![0u8; 32];
        r[0x04] = 0x11;
        r[0x0E] = 0x22;
        r[0x0F] = 0x80; // the log-info-available bit is masked off
        r[0x14] = 9;
        assert_eq!(config_reply(&r), (0x0022, 0x11, 9));
    }

    #[test]
    fn scsi_io_frame() {
        let f = scsi_io_request(0x000a, &[0x12, 0, 0, 0, 96, 0], 96);
        assert_eq!(f.len(), 0x60);
        assert_eq!(le16(&f, 0), 0x000a);
        assert_eq!(f[3], 0x00);
        assert_eq!(f[0x14], 24);
        assert_eq!(le32(&f, 0x1C), 96);
        assert_eq!(le16(&f, 0x24), 6);
        assert_eq!(le32(&f, 0x3C), SCSIIO_CONTROL_READ);
        assert_eq!(&f[0x40..0x46], &[0x12, 0, 0, 0, 96, 0]);
        let t = scsi_io_request(0x000a, &[0; 6], 0);
        assert_eq!(le32(&t, 0x3C), 0);
    }

    /// forge1's HBA330 as the owner's iDRAC lists it: 8 phys, 4 linked.
    fn forge1_io_unit0() -> Vec<u8> {
        let mut p = vec![0u8; 0x10 + 8 * 0x14];
        p[0x0C] = 8;
        for i in 0..8 {
            let o = 0x10 + i * 0x14;
            if i < 4 {
                p[o] = i as u8;
                p[o + 3] = 0xBB; // 12.0 Gbit logical and physical
                p[o + 8..o + 10].copy_from_slice(&(0x09u16 + i as u16).to_le_bytes());
            }
        }
        p
    }

    fn dev0(handle: u16, addr: u64, slot: u16, info: u32) -> Vec<u8> {
        let mut p = vec![0u8; 0x44];
        p[0x08..0x0A].copy_from_slice(&slot.to_le_bytes());
        p[0x0A..0x0C].copy_from_slice(&1u16.to_le_bytes());
        p[0x0C..0x14].copy_from_slice(&addr.to_le_bytes());
        p[0x16] = slot as u8;
        p[0x18..0x1A].copy_from_slice(&handle.to_le_bytes());
        p[0x1C..0x20].copy_from_slice(&info.to_le_bytes());
        p[0x20..0x22].copy_from_slice(&1u16.to_le_bytes());
        p
    }

    #[test]
    fn io_unit0_phys() {
        let phys = parse_sas_io_unit0(&forge1_io_unit0());
        assert_eq!(phys.len(), 8);
        assert!(phys[0].linked());
        assert_eq!(phys[0].rate_name(), "12.0 Gbit");
        assert_eq!(phys[3].attached, 0x0c);
        assert!(!phys[4].linked());
        assert_eq!(phys[4].rate_name(), "Unknown");
    }

    #[test]
    fn device0_fields() {
        let d = parse_sas_device0(&dev0(0x0a, 0x5000cca26801b649, 1, 0x0000_0401 | 0x40));
        assert_eq!(d.handle, 0x0a);
        assert_eq!(d.sas_address, 0x5000cca26801b649);
        assert_eq!((d.enclosure, d.slot, d.phy), (1, 1, 1));
        assert!(d.is_drive() && d.present());
        assert_eq!(d.kind(), "SSP target");
        assert_eq!(d.access(), None);
        // The backplane's SEP is no drive; nor an expander.
        assert!(!parse_sas_device0(&dev0(0x10, 1, 0, 0x4401)).is_drive());
        assert!(parse_sas_device0(&dev0(0x11, 2, 0, 0x2)).is_expander());
        let mut b = dev0(0x0b, 3, 2, 0x401);
        b[0x17] = 0x06;
        assert_eq!(parse_sas_device0(&b).access().as_deref(), Some("device blocked"));
    }

    #[test]
    fn enclosure0_fields() {
        let mut p = vec![0u8; 0x28];
        p[0x0C..0x14].copy_from_slice(&0x500605b00a1b2c3du64.to_le_bytes());
        p[0x16] = 1;
        p[0x18] = 8;
        let e = parse_enclosure0(&p);
        assert_eq!((e.handle, e.logical_id, e.num_slots, e.start_slot), (1, 0x500605b00a1b2c3d, 8, 0));
    }

    #[test]
    fn sense_and_answers() {
        let mut fixed = vec![0u8; 18];
        fixed[0] = 0x70;
        fixed[2] = 0x02;
        fixed[12] = 0x31;
        assert_eq!(parse_sense(&fixed), Some((2, 0x31, 0)));
        assert_eq!(parse_sense(&[0x72, 0x02, 0x04, 0x00]), Some((2, 4, 0)));
        let a = Answer { ioc_status: 0x0045, scsi_status: 2, sense: Some((2, 0x31, 0)), data: vec![] };
        assert_eq!(a.said(), "NOT READY asc/ascq 0x31/0x00");
        let g = Answer { ioc_status: 0, scsi_status: 0, sense: None, data: vec![] };
        assert_eq!(g.said(), "good");
        let mut inq = vec![0u8; 36];
        inq[8..16].copy_from_slice(b"HGST    ");
        inq[16..32].copy_from_slice(b"H7210A520SUN010T");
        inq[32..36].copy_from_slice(b"A3B0");
        assert_eq!(inquiry_name(&inq).as_deref(), Some("HGST H7210A520SUN010T A3B0"));
    }

    fn tmp() -> PathBuf {
        let base = std::env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let d = base.join(format!("mpt-test-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    /// forge1's sysfs: sda on end_device-0:0; end_device-0:1 with a SCSI
    /// device and only sg1; end_device-0:2 with no target; 0:3 not there.
    fn forge1_sys() -> PathBuf {
        let s = tmp();
        let host = s.join("devices/pci0000:c0/0000:c0:03.1/0000:c1:00.0/host0");
        std::fs::create_dir_all(s.join("class/sas_device")).unwrap();
        std::fs::create_dir_all(s.join("class/scsi_host")).unwrap();
        let sh = host.join("scsi_host/host0");
        std::fs::create_dir_all(&sh).unwrap();
        std::fs::write(sh.join("proc_name"), "mpt3sas\n").unwrap();
        std::fs::write(sh.join("unique_id"), "0\n").unwrap();
        std::os::unix::fs::symlink(&sh, s.join("class/scsi_host/host0")).unwrap();
        for (n, addr) in [(0, "0x5000cca2680ab8d9"), (1, "0x5000cca26801b649"), (2, "0x5000cca26a819e69")] {
            let ed = host.join(format!("port-0:{n}/end_device-0:{n}"));
            let sd = ed.join(format!("sas_device/end_device-0:{n}"));
            std::fs::create_dir_all(&sd).unwrap();
            std::fs::write(sd.join("sas_address"), format!("{addr}\n")).unwrap();
            std::os::unix::fs::symlink(&sd, s.join(format!("class/sas_device/end_device-0:{n}"))).unwrap();
            let scsi = ed.join(format!("target0:0:{n}/0:0:{n}:0"));
            match n {
                0 => {
                    std::fs::create_dir_all(scsi.join("block/sda")).unwrap();
                    std::fs::write(scsi.join("block/sda/size"), "19134414848\n").unwrap();
                    std::fs::create_dir_all(scsi.join("scsi_generic/sg0")).unwrap();
                }
                1 => std::fs::create_dir_all(scsi.join("scsi_generic/sg1")).unwrap(),
                _ => {}
            }
        }
        s
    }

    #[test]
    fn rows_matched_to_linux() {
        let s = forge1_sys();
        let a = match_linux(&s, 0x5000cca2680ab8d9);
        assert_eq!(a.state(), "ready");
        assert_eq!(a.said(), "0:0:0:0 sda sg0");
        let b = match_linux(&s, 0x5000cca26801b649);
        assert_eq!(b.state(), "not ready");
        assert_eq!(b.said(), "0:0:1:0 sg1 (no block device)");
        let c = match_linux(&s, 0x5000cca26a819e69);
        assert_eq!(c.state(), "no target");
        assert_eq!(c.said(), "end_device-0:2, no SCSI target yet");
        let d = match_linux(&s, 0x5000cca268077301);
        assert_eq!(d.state(), "no target");
        assert_eq!(d.said(), "no SCSI target yet (not in the SAS transport class)");
        assert_eq!(mpt_hosts(&s), vec![(0, "0000:c1:00.0".to_string(), "host0".to_string())]);
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn card_lines() {
        let s = forge1_sys();
        let phys = parse_sas_io_unit0(&forge1_io_unit0());
        let addrs = [0x5000cca2680ab8d9u64, 0x5000cca26801b649, 0x5000cca26a819e69, 0x5000cca268077301];
        let devices: Vec<Device> =
            addrs.iter().enumerate().map(|(i, a)| parse_sas_device0(&dev0(0x09 + i as u16, *a, i as u16, 0x441))).collect();
        let enc = Enclosure { handle: 1, logical_id: 0x500605b00a1b2c3d, num_slots: 8, start_slot: 0 };
        let rows = devices
            .iter()
            .map(|d| Row {
                device: d.clone(),
                enclosure: Some(enc.clone()),
                linux: match_linux(&s, d.sas_address),
                probe: match_linux(&s, d.sas_address)
                    .scsi
                    .is_none()
                    .then(|| (Some("HGST H7210A520SUN010T A3B0".to_string()), "NOT READY asc/ascq 0x31/0x00".to_string())),
            })
            .collect();
        let card = Card {
            ioc: 0,
            ctl: "mpt3ctl".into(),
            pci: "0000:c1:00.0".into(),
            host: "host0".into(),
            phys,
            devices,
            enclosures: vec![enc],
            rows,
        };
        let l = card.lines();
        assert_eq!(
            l[0],
            "card: ioc 0 (mpt3ctl) 0000:c1:00.0 host0: 8 phys, 4 with a link, 4 drive(s), 0 expander(s), 1 enclosure(s) (the card's own pages)"
        );
        assert!(l.contains(&"card: phy 1: 12.0 Gbit, port 1, device 0x000a sas 0x5000cca26801b649 (SSP target)".to_string()), "{l:#?}");
        assert!(l.contains(&"card: no link, Unknown: phy 4 5 6 7".to_string()), "{l:#?}");
        assert!(l.contains(&"card: drive sas 0x5000cca2680ab8d9 enclosure 0x500605b00a1b2c3d slot 0, SSP target, phy 0, present: ready - 0:0:0:0 sda sg0".to_string()), "{l:#?}");
        assert!(l.contains(&"card: drive sas 0x5000cca26a819e69 enclosure 0x500605b00a1b2c3d slot 2, SSP target, phy 2, present: no target - end_device-0:2, no SCSI target yet; through the card: INQUIRY HGST H7210A520SUN010T A3B0, TEST UNIT READY NOT READY asc/ascq 0x31/0x00".to_string()), "{l:#?}");
        assert_eq!(card.drives().filter(|r| r.linux.state() != "ready").count(), 3);
        let _ = std::fs::remove_dir_all(&s);
    }
}
