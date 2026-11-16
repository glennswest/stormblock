//! A drive's own answers, through whatever reaches it (#611).
//!
//! Owner, #611: "also allow us to get more low level details from drives".
//! A row of the card's inventory ([`crate::drive::mpt`]) is asked through
//! its `sg` node when the kernel made one (`SG_IO`), else through the card's
//! own SCSI pass-through to its device handle: a drive with no sd still
//! answers. [`details`] reads identity (INQUIRY, VPD 0x00/0x80/0x83/0xB0/
//! 0xB1/0xB2), format state (READ CAPACITY 16, REQUEST SENSE progress) and
//! health (LOG SENSE temperature, informational exceptions, self-test,
//! start-stop, read/write/non-medium errors; the grown defect count; the
//! caching mode page). [`Action`]s are the row's: spin up or down, a
//! self-test, and FORMAT UNIT to a block size and PI (MODE SELECT with the
//! drive's own descriptor, then FMTDATA + IMMED + FMTPINFO, as stormdrive
//! does; progress read with REQUEST SENSE).
//!
//! The CDBs and the parsers are plain functions over bytes, tested here.

use serde::Serialize;

/// Which way the data goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    None,
    In,
    Out,
}

/// What a command came back with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reply {
    /// The SCSI status (0 good, 2 CHECK CONDITION).
    pub status: u8,
    /// The sense data, raw.
    pub sense: Vec<u8>,
    /// The bytes that came in.
    pub len: usize,
    /// A transport word, when the command did not reach the drive cleanly
    /// (the card's IOCStatus, sg's host/driver status).
    pub transport: Option<String>,
}

impl Reply {
    pub fn key(&self) -> Option<(u8, u8, u8)> {
        crate::drive::mpt::parse_sense(&self.sense)
    }
    pub fn good(&self) -> bool {
        self.status == 0 && self.transport.is_none()
    }
    /// A FORMAT / sanitize progress indication (0..=65535 of 65536), from the
    /// sense-key-specific bytes when SKSV is set.
    pub fn progress(&self) -> Option<u16> {
        progress_of(&self.sense)
    }
    pub fn said(&self) -> String {
        if let Some(t) = &self.transport {
            return t.clone();
        }
        match self.key() {
            Some((k, a, q)) => format!("{} asc/ascq 0x{a:02x}/0x{q:02x}", crate::drive::mpt::sense_key_name(k)),
            None if self.status == 0 => "good".into(),
            None => format!("SCSI status 0x{:02x}", self.status),
        }
    }
}

/// Sense -> progress indication (fixed: SKSV byte 15 bit 7, bytes 16-17;
/// descriptor: descriptor type 2, sense-key specific).
pub fn progress_of(s: &[u8]) -> Option<u16> {
    match s.first()? & 0x7F {
        0x70 | 0x71 if s.len() >= 18 && s[15] & 0x80 != 0 => Some(u16::from_be_bytes([s[16], s[17]])),
        0x72 | 0x73 if s.len() >= 8 => {
            let add = s[7] as usize;
            let mut o = 8;
            while o + 1 < s.len().min(8 + add) {
                let (t, l) = (s[o], s[o + 1] as usize);
                if t == 0x02 && l >= 6 && o + 8 <= s.len() && s[o + 4] & 0x80 != 0 {
                    return Some(u16::from_be_bytes([s[o + 5], s[o + 6]]));
                }
                o += 2 + l;
            }
            None
        }
        _ => None,
    }
}

/// Something that carries a CDB to a drive.
pub trait Transport {
    fn run(&self, cdb: &[u8], dir: Dir, data: &mut [u8], timeout_s: u32) -> std::io::Result<Reply>;
    /// What it is, for the console (`sg3`, `the card, handle 0x000a`).
    fn name(&self) -> String;
}

// ---- CDBs ----

pub mod cdb {
    pub fn inquiry(vpd: Option<u8>, alloc: u16) -> [u8; 6] {
        let [hi, lo] = alloc.to_be_bytes();
        [0x12, u8::from(vpd.is_some()), vpd.unwrap_or(0), hi, lo, 0]
    }
    pub fn read_capacity16(alloc: u32) -> [u8; 16] {
        let a = alloc.to_be_bytes();
        [0x9E, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, a[0], a[1], a[2], a[3], 0, 0]
    }
    pub fn test_unit_ready() -> [u8; 6] {
        [0; 6]
    }
    pub fn request_sense(alloc: u8) -> [u8; 6] {
        [0x03, 0, 0, 0, alloc, 0]
    }
    /// Cumulative values (PC 01b).
    pub fn log_sense(page: u8, sub: u8, alloc: u16) -> [u8; 10] {
        let [hi, lo] = alloc.to_be_bytes();
        [0x4D, 0, 0x40 | (page & 0x3F), sub, 0, 0, 0, hi, lo, 0]
    }
    /// Current values; `llbaa` asks for a long block descriptor.
    pub fn mode_sense10(page: u8, llbaa: bool, dbd: bool, alloc: u16) -> [u8; 10] {
        let [hi, lo] = alloc.to_be_bytes();
        [0x5A, (if llbaa { 0x10 } else { 0 }) | (if dbd { 0x08 } else { 0 }), page & 0x3F, 0, 0, 0, 0, hi, lo, 0]
    }
    /// PF=1, SP=0.
    pub fn mode_select10(len: u16) -> [u8; 10] {
        let [hi, lo] = len.to_be_bytes();
        [0x55, 0x10, 0, 0, 0, 0, 0, hi, lo, 0]
    }
    /// The grown list (GLIST), address descriptor format 101b; only the
    /// header (its length) is asked for.
    pub fn read_defect_data12(alloc: u32) -> [u8; 12] {
        let a = alloc.to_be_bytes();
        [0xB7, 0x08 | 0x05, 0, 0, 0, 0, a[0], a[1], a[2], a[3], 0, 0]
    }
    /// IMMED; START or not.
    pub fn start_stop(start: bool) -> [u8; 6] {
        [0x1B, 0x01, 0, 0, u8::from(start), 0]
    }
    /// SEND DIAGNOSTIC self-test code: 001b short, 010b extended (background).
    pub fn self_test(extended: bool) -> [u8; 6] {
        [0x1D, if extended { 0x40 } else { 0x20 }, 0, 0, 0, 0]
    }
    /// FMTDATA=1, FMTPINFO (00b none, 10b type 1 with PFU 0, 11b types 2/3).
    pub fn format_unit(fmtpinfo: u8) -> [u8; 6] {
        [0x04, ((fmtpinfo & 0x03) << 6) | 0x10, 0, 0, 0, 0]
    }
    /// The short parameter list header: FOV=0, IMMED.
    pub fn format_param_immed() -> [u8; 4] {
        [0, 0x02, 0, 0]
    }
    /// READ(16) of `blocks` at `lba`, RDPROTECT 0: the drive checks its own
    /// protection information and sends none (#625).
    pub fn read16(lba: u64, blocks: u32) -> [u8; 16] {
        let l = lba.to_be_bytes();
        let n = blocks.to_be_bytes();
        [0x88, 0, l[0], l[1], l[2], l[3], l[4], l[5], l[6], l[7], n[0], n[1], n[2], n[3], 0, 0]
    }
    /// WRITE(16) of `blocks` at `lba`, WRPROTECT 0: no protection
    /// information is sent; a drive formatted with PI makes its own (#625).
    pub fn write16(lba: u64, blocks: u32) -> [u8; 16] {
        let l = lba.to_be_bytes();
        let n = blocks.to_be_bytes();
        [0x8A, 0, l[0], l[1], l[2], l[3], l[4], l[5], l[6], l[7], n[0], n[1], n[2], n[3], 0, 0]
    }
}

// ---- parsers ----

fn be16(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2).map(|s| u16::from_be_bytes([s[0], s[1]])).unwrap_or(0)
}
fn be32(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]])).unwrap_or(0)
}
fn be64(b: &[u8], o: usize) -> u64 {
    ((be32(b, o) as u64) << 32) | be32(b, o + 4) as u64
}
fn be_var(b: &[u8]) -> u64 {
    b.iter().take(8).fold(0u64, |a, x| (a << 8) | *x as u64)
}
fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim_matches(|c: char| c.is_whitespace() || c == '\0').to_string()
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// READ CAPACITY 16.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Capacity {
    pub blocks: u64,
    pub block_size: u32,
    pub prot_en: bool,
    /// P_TYPE + 1 when PROT_EN (1 = type 1), 0 without.
    pub pi_type: u8,
    pub lbpme: bool,
}

pub fn parse_capacity16(d: &[u8]) -> Option<Capacity> {
    if d.len() < 32 {
        return None;
    }
    let prot_en = d[12] & 0x01 != 0;
    Some(Capacity {
        blocks: be64(d, 0).wrapping_add(1),
        block_size: be32(d, 8),
        prot_en,
        pi_type: if prot_en { ((d[12] >> 1) & 0x07) + 1 } else { 0 },
        lbpme: d[14] & 0x80 != 0,
    })
}

/// A VPD 0x83 designator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Designator {
    /// `lun`, `port` or `target`.
    pub of: &'static str,
    /// `naa`, `eui64`, `t10`, `vendor`, `scsi_name`, `relative_port`, …
    pub kind: &'static str,
    pub value: String,
}

pub fn parse_vpd83(d: &[u8]) -> Vec<Designator> {
    let mut v = Vec::new();
    let end = (4 + be16(d, 2) as usize).min(d.len());
    let mut o = 4;
    while o + 4 <= end {
        let code_set = d[o] & 0x0F;
        let assoc = (d[o + 1] >> 4) & 0x03;
        let kind = d[o + 1] & 0x0F;
        let len = d[o + 3] as usize;
        let Some(val) = d.get(o + 4..o + 4 + len) else { break };
        let of = match assoc {
            0 => "lun",
            1 => "port",
            2 => "target",
            _ => "?",
        };
        let (kind, value) = match kind {
            0 => ("vendor", if code_set == 2 || code_set == 3 { text(val) } else { hex(val) }),
            1 => ("t10", text(val)),
            2 => ("eui64", hex(val)),
            3 => ("naa", hex(val)),
            4 => ("relative_port", format!("{}", be16(val, 2))),
            5 => ("target_port_group", format!("{}", be16(val, 2))),
            8 => ("scsi_name", text(val)),
            _ => ("other", hex(val)),
        };
        v.push(Designator { of, kind, value });
        o += 4 + len;
    }
    v
}

/// The LUN's NAA name, as the kernel spells a WWID (`naa.5000cca2…`).
pub fn wwid_of(ds: &[Designator]) -> Option<String> {
    ds.iter().find(|d| d.of == "lun" && d.kind == "naa").map(|d| format!("naa.{}", d.value))
}

/// Log page parameters: (code, value bytes).
pub fn log_params(d: &[u8]) -> Vec<(u16, Vec<u8>)> {
    let mut v = Vec::new();
    let end = (4 + be16(d, 2) as usize).min(d.len());
    let mut o = 4;
    while o + 4 <= end {
        let code = be16(d, o);
        let len = d[o + 3] as usize;
        let Some(val) = d.get(o + 4..o + 4 + len) else { break };
        v.push((code, val.to_vec()));
        o += 4 + len;
    }
    v
}

fn param(ps: &[(u16, Vec<u8>)], code: u16) -> Option<&[u8]> {
    ps.iter().find(|(c, _)| *c == code).map(|(_, v)| v.as_slice())
}

/// A self-test log entry (page 0x10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SelfTest {
    pub code: u8,
    /// 0 completed with no error, 1 aborted by command, 2 aborted, 3 an
    /// unknown error, 4..=7 failed (segment), 15 in progress.
    pub result: u8,
    pub said: &'static str,
    pub hours: u16,
}

fn self_test_said(r: u8) -> &'static str {
    match r {
        0 => "completed, no error",
        1 => "aborted (SEND DIAGNOSTIC)",
        2 => "aborted (reset)",
        3 => "an unknown error",
        4..=7 => "failed",
        15 => "in progress",
        _ => "?",
    }
}

/// The caching mode page (0x08): write cache on, read cache off.
pub fn parse_caching(d: &[u8]) -> Option<(bool, bool)> {
    let bdl = be16(d, 6) as usize;
    let p = d.get(8 + bdl..)?;
    if p.len() < 3 || p[0] & 0x3F != 0x08 {
        return None;
    }
    Some((p[2] & 0x04 != 0, p[2] & 0x01 != 0))
}

/// MODE SENSE(10)'s header and its first block descriptor (page 0x01, for
/// a block size change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeHeader {
    pub medium_type: u8,
    pub long_lba: bool,
    pub descriptor: Vec<u8>,
}

pub fn parse_mode_header(d: &[u8]) -> Option<ModeHeader> {
    if d.len() < 8 {
        return None;
    }
    let bdl = be16(d, 6) as usize;
    Some(ModeHeader { medium_type: d[2], long_lba: d[4] & 1 != 0, descriptor: d.get(8..8 + bdl).map(<[u8]>::to_vec).unwrap_or_default() })
}

/// The MODE SELECT(10) list that sets a new block length (stormdrive's
/// `mode_select10_list`, sg_format's way): the drive's own descriptor with
/// NUMBER OF LOGICAL BLOCKS 0 (the most there are at the new length) and the
/// new LOGICAL BLOCK LENGTH; long when the drive gave a long one.
pub fn mode_select_list(h: &ModeHeader, block_len: u32) -> Vec<u8> {
    let long = h.long_lba && h.descriptor.len() >= 16;
    let bdl = if long { 16 } else { 8 };
    let mut v = vec![0u8; 8 + bdl];
    v[2] = h.medium_type;
    v[4] = u8::from(long);
    v[7] = bdl as u8;
    if h.descriptor.len() == bdl {
        v[8..].copy_from_slice(&h.descriptor);
    }
    let bd = &mut v[8..];
    if long {
        bd[0..8].fill(0);
        bd[12..16].copy_from_slice(&block_len.to_be_bytes());
    } else {
        bd[0..4].fill(0);
        bd[5..8].copy_from_slice(&block_len.to_be_bytes()[1..]);
    }
    v
}

// ---- details ----

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// What a drive says of itself (#611), each part `None` when it would not.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Details {
    pub via: String,
    pub read_at: u64,
    pub ready: Option<String>,
    /// A format or sanitize under way: percent done.
    pub progress_pct: Option<f32>,
    pub vendor: Option<String>,
    pub product: Option<String>,
    pub revision: Option<String>,
    pub serial: Option<String>,
    pub wwid: Option<String>,
    pub designators: Vec<Designator>,
    pub vpd_pages: Vec<String>,
    /// VPD 0xB1: 1 = non-rotating (SSD), else rpm.
    pub rotation_rate: Option<u16>,
    pub form_factor: Option<String>,
    /// VPD 0xB0, in blocks.
    pub max_transfer_blocks: Option<u32>,
    pub optimal_transfer_blocks: Option<u32>,
    /// VPD 0xB2: UNMAP / WRITE SAME(16) UNMAP supported.
    pub lbpu: Option<bool>,
    pub lbpws: Option<bool>,
    pub capacity: Option<Capacity>,
    pub temperature_c: Option<u8>,
    pub reference_temperature_c: Option<u8>,
    /// Informational exceptions: ASC/ASCQ (0/0 none; 5D/xx a SMART trip).
    pub smart_asc_ascq: Option<String>,
    pub smart_trip: Option<bool>,
    pub self_tests: Vec<SelfTest>,
    pub start_stop_cycles: Option<u32>,
    pub load_unload_cycles: Option<u32>,
    pub read_uncorrected: Option<u64>,
    pub write_uncorrected: Option<u64>,
    pub non_medium_errors: Option<u64>,
    pub grown_defects: Option<u32>,
    pub write_cache: Option<bool>,
    pub read_cache_disabled: Option<bool>,
    /// Mode pages 0x03/0x08/0x0A/0x19/0x1A/0x1C, raw and said (#627: the
    /// drives compared side by side).
    pub mode_pages: Vec<ModePage>,
    /// What did not answer, and how.
    pub errors: Vec<String>,
}

fn ask(t: &dyn Transport, cdb: &[u8], len: usize, what: &str, errs: &mut Vec<String>) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    match t.run(cdb, Dir::In, &mut buf, 30) {
        Ok(r) if r.good() => {
            buf.truncate(r.len.min(len));
            Some(buf)
        }
        Ok(r) => {
            errs.push(format!("{what}: {}", r.said()));
            None
        }
        Err(e) => {
            errs.push(format!("{what}: {e}"));
            None
        }
    }
}

/// Every detail the drive will give, read now.
pub fn details(t: &dyn Transport) -> Details {
    let mut d = Details { via: t.name(), read_at: now_secs(), ..Default::default() };
    let mut e = Vec::new();
    match t.run(&cdb::test_unit_ready(), Dir::None, &mut [], 30) {
        Ok(r) => d.ready = Some(r.said()),
        Err(x) => e.push(format!("TEST UNIT READY: {x}")),
    }
    if let Some(s) = ask(t, &cdb::request_sense(96), 96, "REQUEST SENSE", &mut e) {
        d.progress_pct = progress_of(&s).map(|p| p as f32 * 100.0 / 65536.0);
    }
    if let Some(i) = ask(t, &cdb::inquiry(None, 96), 96, "INQUIRY", &mut e) {
        if i.len() >= 36 {
            d.vendor = Some(text(&i[8..16]));
            d.product = Some(text(&i[16..32]));
            d.revision = Some(text(&i[32..36]));
        }
    }
    let pages = ask(t, &cdb::inquiry(Some(0), 252), 252, "VPD 0x00", &mut e).unwrap_or_default();
    let has = |p: u8| pages.len() >= 4 && pages[4..(4 + pages[3] as usize).min(pages.len())].contains(&p);
    d.vpd_pages = if pages.len() >= 4 { pages[4..(4 + pages[3] as usize).min(pages.len())].iter().map(|p| format!("0x{p:02x}")).collect() } else { vec![] };
    if has(0x80) {
        if let Some(v) = ask(t, &cdb::inquiry(Some(0x80), 252), 252, "VPD 0x80", &mut e) {
            d.serial = v.get(4..(4 + v.get(3).copied().unwrap_or(0) as usize).min(v.len())).map(text);
        }
    }
    if has(0x83) {
        if let Some(v) = ask(t, &cdb::inquiry(Some(0x83), 1024), 1024, "VPD 0x83", &mut e) {
            d.designators = parse_vpd83(&v);
            d.wwid = wwid_of(&d.designators);
        }
    }
    if has(0xB0) {
        if let Some(v) = ask(t, &cdb::inquiry(Some(0xB0), 64), 64, "VPD 0xB0", &mut e) {
            d.max_transfer_blocks = Some(be32(&v, 8)).filter(|x| *x != 0);
            d.optimal_transfer_blocks = Some(be32(&v, 12)).filter(|x| *x != 0);
        }
    }
    if has(0xB1) {
        if let Some(v) = ask(t, &cdb::inquiry(Some(0xB1), 64), 64, "VPD 0xB1", &mut e) {
            d.rotation_rate = Some(be16(&v, 4)).filter(|x| *x != 0);
            d.form_factor = v.get(7).map(|f| match f & 0x0F {
                1 => "5.25in".to_string(),
                2 => "3.5in".to_string(),
                3 => "2.5in".to_string(),
                4 => "1.8in".to_string(),
                5 => "<1.8in".to_string(),
                0 => "not reported".to_string(),
                x => format!("code {x}"),
            });
        }
    }
    if has(0xB2) {
        if let Some(v) = ask(t, &cdb::inquiry(Some(0xB2), 64), 64, "VPD 0xB2", &mut e) {
            if let Some(b) = v.get(5) {
                d.lbpu = Some(b & 0x80 != 0);
                d.lbpws = Some(b & 0x40 != 0);
            }
        }
    }
    d.capacity = ask(t, &cdb::read_capacity16(32), 32, "READ CAPACITY 16", &mut e).and_then(|c| parse_capacity16(&c));
    let log = |page: u8, e: &mut Vec<String>| {
        ask(t, &cdb::log_sense(page, 0, 1024), 1024, &format!("LOG SENSE 0x{page:02x}"), e).map(|p| log_params(&p))
    };
    if let Some(ps) = log(0x0D, &mut e) {
        d.temperature_c = param(&ps, 0).and_then(|v| v.get(1).copied()).filter(|t| *t != 0xFF);
        d.reference_temperature_c = param(&ps, 1).and_then(|v| v.get(1).copied()).filter(|t| *t != 0xFF);
    }
    if let Some(ps) = log(0x2F, &mut e) {
        if let Some(v) = param(&ps, 0) {
            if v.len() >= 2 {
                d.smart_asc_ascq = Some(format!("0x{:02x}/0x{:02x}", v[0], v[1]));
                d.smart_trip = Some(v[0] == 0x5D);
            }
            if d.temperature_c.is_none() {
                d.temperature_c = v.get(2).copied().filter(|t| *t != 0 && *t != 0xFF);
            }
        }
    }
    if let Some(ps) = log(0x10, &mut e) {
        d.self_tests = ps
            .iter()
            .filter(|(c, v)| (1..=20).contains(c) && v.len() >= 4 && v.iter().any(|b| *b != 0))
            .map(|(_, v)| SelfTest { code: v[0] >> 5, result: v[0] & 0x0F, said: self_test_said(v[0] & 0x0F), hours: be16(v, 2) })
            .collect();
    }
    if let Some(ps) = log(0x0E, &mut e) {
        d.start_stop_cycles = param(&ps, 4).map(|v| be32(v, 0));
        d.load_unload_cycles = param(&ps, 6).map(|v| be32(v, 0));
    }
    if let Some(ps) = log(0x03, &mut e) {
        d.read_uncorrected = param(&ps, 6).map(be_var);
    }
    if let Some(ps) = log(0x02, &mut e) {
        d.write_uncorrected = param(&ps, 6).map(be_var);
    }
    if let Some(ps) = log(0x06, &mut e) {
        d.non_medium_errors = param(&ps, 0).map(be_var);
    }
    if let Some(h) = ask(t, &cdb::read_defect_data12(8), 8, "READ DEFECT DATA 12", &mut e) {
        d.grown_defects = (h.len() >= 8).then(|| be32(&h, 4) / 8);
    }
    if let Some(m) = ask(t, &cdb::mode_sense10(0x08, false, true, 64), 64, "MODE SENSE 0x08", &mut e) {
        if let Some((wce, rcd)) = parse_caching(&m) {
            d.write_cache = Some(wce);
            d.read_cache_disabled = Some(rcd);
        }
    }
    d.mode_pages = mode_pages(t);
    d.errors = e;
    d
}

// ---- a write that failed (#625) ----

/// MODE SENSE(10)'s header: the device-specific parameter's WP bit.
pub fn parse_write_protect(d: &[u8]) -> Option<bool> {
    d.get(3).map(|b| b & 0x80 != 0)
}

/// What a drive says when a write to it failed (#625): its pending sense,
/// whether it is write protected, and how a block at LBA 0 reads and
/// writes with no protection information sent (the drive makes its own).
#[derive(Debug, Clone, Default, Serialize)]
pub struct WriteProbe {
    pub via: String,
    pub ready: Option<String>,
    /// REQUEST SENSE: what the drive has pending, `good` when nothing.
    pub pending: Option<String>,
    pub progress_pct: Option<f32>,
    pub write_protect: Option<bool>,
    pub capacity: Option<Capacity>,
    /// READ(16) at LBA 0, one block, RDPROTECT 0.
    pub read_lba0: Option<String>,
    /// WRITE(16) at LBA 0, one block of zeros, WRPROTECT 0.
    pub write_lba0: Option<String>,
    /// (key, asc, ascq) of each answer above, for [`WriteProbe::hint`].
    #[serde(skip)]
    pub senses: Vec<(u8, u8, u8)>,
    #[serde(skip)]
    pub write_good: Option<bool>,
}

fn said_of(r: std::io::Result<Reply>, senses: &mut Vec<(u8, u8, u8)>) -> (String, Option<bool>) {
    match r {
        Ok(r) => {
            if let Some(k) = r.key() {
                senses.push(k);
            }
            (r.said(), Some(r.good()))
        }
        Err(e) => (format!("not sent: {e}"), None),
    }
}

/// Ask the drive why a write failed. **Writes one block of zeros at LBA 0**:
/// only for a drive being laid, whose front is cleared anyway.
pub fn write_probe(t: &dyn Transport, block_size: u32) -> WriteProbe {
    let mut p = WriteProbe { via: t.name(), ..Default::default() };
    let mut senses = Vec::new();
    p.ready = Some(said_of(t.run(&cdb::test_unit_ready(), Dir::None, &mut [], 30), &mut senses).0);
    let mut s = vec![0u8; 96];
    if let Ok(r) = t.run(&cdb::request_sense(96), Dir::In, &mut s, 30) {
        if r.good() {
            let s = &s[..r.len.min(96)];
            p.progress_pct = progress_of(s).map(|x| x as f32 * 100.0 / 65536.0);
            let k = crate::drive::mpt::parse_sense(s);
            p.pending = Some(match k {
                Some((0, 0, 0)) | None => "good".into(),
                Some((k, a, q)) => {
                    senses.push((k, a, q));
                    format!("{} asc/ascq 0x{a:02x}/0x{q:02x}", crate::drive::mpt::sense_key_name(k))
                }
            });
        } else {
            p.pending = Some(r.said());
        }
    }
    let mut m = vec![0u8; 64];
    if let Ok(r) = t.run(&cdb::mode_sense10(0x3F, false, true, 64), Dir::In, &mut m, 30) {
        if r.good() {
            p.write_protect = parse_write_protect(&m[..r.len.min(64)]);
        }
    }
    let mut c = vec![0u8; 32];
    if let Ok(r) = t.run(&cdb::read_capacity16(32), Dir::In, &mut c, 30) {
        if r.good() {
            p.capacity = parse_capacity16(&c[..r.len.min(32)]);
        }
    }
    let bs = p.capacity.as_ref().map(|c| c.block_size).filter(|b| *b >= 512).unwrap_or(block_size.max(512)) as usize;
    let mut blk = vec![0u8; bs];
    p.read_lba0 = Some(said_of(t.run(&cdb::read16(0, 1), Dir::In, &mut blk, 30), &mut senses).0);
    let mut zeros = vec![0u8; bs];
    let (w, good) = said_of(t.run(&cdb::write16(0, 1), Dir::Out, &mut zeros, 60), &mut senses);
    p.write_lba0 = Some(w);
    p.write_good = good;
    p.senses = senses;
    p
}

impl WriteProbe {
    /// What the answers mean, in a line, when they say something.
    pub fn hint(&self) -> Option<String> {
        if self.write_protect == Some(true) {
            return Some("the drive is write protected (WP in its mode header)".into());
        }
        for &(k, a, q) in &self.senses {
            let h = match (k, a, q) {
                (_, 0x31, _) => "the drive's format is corrupt (did not complete): it needs a FORMAT UNIT again (#595, #621)",
                (2, 0x04, 0x04) => "a format is still running on the drive",
                (2, 0x04, _) => "the drive is not ready",
                (7, _, _) | (_, 0x27, _) => "the drive refuses writes (DATA PROTECT)",
                (_, 0x10, 0x01) => "the drive rejects the protection information it was sent: guard check failed",
                (_, 0x10, 0x02) => "the drive rejects the protection information it was sent: application tag check failed",
                (_, 0x10, 0x03) => "the drive rejects the protection information it was sent: reference tag check failed",
                (_, 0x10, _) => "the drive rejects the protection information it was sent",
                (3, _, _) => "a medium error at the front of the drive",
                (4, _, _) => "the drive reports a hardware error",
                _ => continue,
            };
            return Some(h.into());
        }
        match self.write_good {
            Some(true) => Some(
                "a write with no protection information succeeds through SG_IO: the failure is in the path the \
                 kernel's writes take (the PI the HBA adds, or the HBA itself)"
                    .into(),
            ),
            Some(false) => Some("the drive refuses a plain write too".into()),
            None => None,
        }
    }

    /// The block for the console, a line each.
    pub fn lines(&self) -> Vec<String> {
        let o = |v: &Option<String>| v.clone().unwrap_or_else(|| "no answer".into());
        let mut v = vec![
            format!("asked through {}: TEST UNIT READY {}; REQUEST SENSE {}{}", self.via, o(&self.ready), o(&self.pending),
                self.progress_pct.map(|p| format!(" ({p:.1}% of a format)")).unwrap_or_default()),
            format!(
                "format: {}; write protect: {}",
                self.capacity.as_ref().map(|c| format!("{} blocks of {}{}", c.blocks, c.block_size,
                    if c.prot_en { format!(" + PI type {}", c.pi_type) } else { " no PI".into() }))
                    .unwrap_or_else(|| "READ CAPACITY 16 did not answer".into()),
                match self.write_protect { Some(true) => "on", Some(false) => "off", None => "not read" }
            ),
            format!("READ(16) LBA 0, no PI: {}; WRITE(16) LBA 0, no PI: {}", o(&self.read_lba0), o(&self.write_lba0)),
        ];
        if let Some(h) = self.hint() {
            v.push(format!("so: {h}"));
        }
        v
    }
}

/// The kernel's last lines about `name` (`sdb`): its sense, its I/O
/// errors, and the card's lines beside them (#625: the console carried
/// none). Read from `/dev/kmsg`, at most `max`.
#[cfg(target_os = "linux")]
pub fn kernel_lines(name: &str, max: usize) -> Vec<String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(mut f) = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open("/dev/kmsg") else {
        return vec![];
    };
    let mut all = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => all.push(String::from_utf8_lossy(&buf[..n]).into_owned()),
            // EPIPE: a record was overwritten while read; the next one follows.
            Err(e) if e.raw_os_error() == Some(libc::EPIPE) => continue,
            Err(_) => break,
        }
    }
    kernel_lines_of(&all, name, max)
}

/// [`kernel_lines`] over records already read (`pri,seq,usec,flags;text`).
pub fn kernel_lines_of(records: &[String], name: &str, max: usize) -> Vec<String> {
    let marks = [format!("[{name}]"), format!("dev {name},"), format!("{name}:"), format!(" {name} ")];
    let mut v: Vec<String> = records
        .iter()
        .filter_map(|r| {
            let first = r.lines().next()?;
            let text = first.split_once(';').map(|(_, t)| t).unwrap_or(first);
            let t = format!(" {text} ");
            (marks.iter().any(|m| t.contains(m.as_str())) || text.starts_with("mpt3sas") && text.contains("log_info"))
                .then(|| text.trim().to_string())
        })
        .collect();
    let n = v.len();
    v.drain(..n.saturating_sub(max));
    v
}

// ---- the write-test matrix (#627) ----

/// CRC-16 T10-DIF (polynomial 0x8BB7, no reflection, init 0): a block's
/// protection-information guard.
pub fn crc_t10dif(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8BB7 } else { crc << 1 };
        }
    }
    crc
}

/// A block's 8 bytes of type-1 protection information: guard, application
/// tag 0, reference tag the LBA's low 32 bits.
pub fn pi_type1(data: &[u8], lba: u64) -> [u8; 8] {
    let g = crc_t10dif(data).to_be_bytes();
    let r = (lba as u32).to_be_bytes();
    [g[0], g[1], 0, 0, r[0], r[1], r[2], r[3]]
}

/// READ(16) / WRITE(16) with RDPROTECT / WRPROTECT (bits 7-5 of byte 1).
pub fn cdb_rw16_protect(write: bool, protect: u8, lba: u64, blocks: u32) -> [u8; 16] {
    let mut c = if write { cdb::write16(lba, blocks) } else { cdb::read16(lba, blocks) };
    c[1] = (protect & 0x07) << 5;
    c
}

/// A mode page, read: its code, raw bytes (from the page code on), and
/// the bits #627 asks about, when it is one of those pages.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ModePage {
    pub page: String,
    pub hex: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub said: Option<String>,
}

/// The page out of a MODE SENSE(10) answer (header, block descriptors).
pub fn mode_page_of(d: &[u8]) -> Option<&[u8]> {
    if d.len() < 8 {
        return None;
    }
    let total = (be16(d, 0) as usize + 2).min(d.len());
    let bdl = be16(d, 6) as usize;
    let p = d.get(8 + bdl..total)?;
    let len = 2 + *p.get(1)? as usize;
    p.get(..len.min(p.len()))
}

/// What a page's bits say (#627: caching WCE/RCD, control DPICZ/ATO/QAM,
/// power-condition timers, informational exceptions).
pub fn mode_page_said(p: &[u8]) -> Option<String> {
    let b = |i: usize| p.get(i).copied().unwrap_or(0);
    Some(match b(0) & 0x3F {
        0x08 => format!("caching: WCE {} RCD {}", (b(2) >> 2) & 1, b(2) & 1),
        0x0A => format!(
            "control: DPICZ {} D_SENSE {} QAM {} SWP {} ATO {} TAS {}",
            (b(2) >> 2) & 1, (b(2) >> 3) & 1, b(3) >> 4, (b(4) >> 3) & 1, b(5) >> 7, (b(5) >> 6) & 1
        ),
        0x1A => format!(
            "power condition: IDLE_A {} STANDBY_Z {} idle_a {} standby_z {} (100 ms units)",
            (b(3) >> 1) & 1, b(3) & 1, be32(p, 4), be32(p, 8)
        ),
        0x1C => format!("informational exceptions: DEXCPT {} EWASC {} MRIE {}", (b(2) >> 3) & 1, (b(2) >> 4) & 1, b(3) & 0x0F),
        0x19 => format!("protocol-specific port: protocol {}", b(2) & 0x0F),
        0x03 => format!("format device: {} bytes a sector", be16(p, 12)),
        _ => return None,
    })
}

/// The pages #627 reads before and after each step.
pub const MATRIX_MODE_PAGES: &[u8] = &[0x03, 0x08, 0x0A, 0x19, 0x1A, 0x1C];
pub const MATRIX_LOG_PAGES: &[u8] = &[0x0E, 0x15];

/// The mode pages, read now; a page the drive refuses is said so.
pub fn mode_pages(t: &dyn Transport) -> Vec<ModePage> {
    MATRIX_MODE_PAGES
        .iter()
        .map(|&pg| {
            let mut m = vec![0u8; 252];
            let page = format!("0x{pg:02x}");
            match t.run(&cdb::mode_sense10(pg, false, true, 252), Dir::In, &mut m, 30) {
                Ok(r) if r.good() => match mode_page_of(&m[..r.len.min(252)]) {
                    Some(p) => ModePage { page, hex: hex(p), said: mode_page_said(p) },
                    None => ModePage { page, hex: String::new(), said: Some("an answer with no page in it".into()) },
                },
                Ok(r) => ModePage { page, hex: String::new(), said: Some(format!("refused: {}", r.said())) },
                Err(e) => ModePage { page, hex: String::new(), said: Some(format!("not sent: {e}")) },
            }
        })
        .collect()
}

/// The log pages, as (page, parameter count or the refusal).
pub fn log_pages(t: &dyn Transport) -> Vec<(String, String)> {
    MATRIX_LOG_PAGES
        .iter()
        .map(|&pg| {
            let mut e = Vec::new();
            let v = ask(t, &cdb::log_sense(pg, 0, 1024), 1024, &format!("LOG SENSE 0x{pg:02x}"), &mut e);
            let said = match v {
                Some(d) => {
                    let ps = log_params(&d);
                    let vals: Vec<String> = ps.iter().take(8).map(|(c, v)| format!("{c:04x}={}", hex(v))).collect();
                    format!("{} parameter(s): {}", ps.len(), vals.join(" "))
                }
                None => e.join("; "),
            };
            (format!("0x{pg:02x}"), said)
        })
        .collect()
}

/// One step of the matrix.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MatrixStep {
    pub lba: u64,
    pub wrprotect: u8,
    pub write: String,
    /// RDPROTECT 0: the data as written?
    pub read0: String,
    /// RDPROTECT 1: the data and its PI (guard, reference tag) as expected?
    pub read1: String,
    pub ready_after: String,
    /// The card's word for the drive's link after the step, when asked.
    pub link_after: Option<String>,
}

/// The whole run: what the drive is, its pages before and after, the steps.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Matrix {
    pub via: String,
    pub vendor: Option<String>,
    pub product: Option<String>,
    pub revision: Option<String>,
    pub capacity: Option<Capacity>,
    pub mode_before: Vec<ModePage>,
    pub log_before: Vec<(String, String)>,
    pub steps: Vec<MatrixStep>,
    pub mode_after: Vec<ModePage>,
    pub log_after: Vec<(String, String)>,
    pub capacity_after: Option<Capacity>,
    /// Why the run stopped short, when it did.
    pub stopped: Option<String>,
}

fn said_reply(r: std::io::Result<Reply>) -> (String, bool) {
    match r {
        Ok(r) => (r.said(), r.good()),
        Err(e) => (format!("not sent: {e}"), false),
    }
}

/// The write-test matrix (#627) at the format the drive has: one block at
/// LBA 0 and at the last LBA, WRPROTECT 0 (the drive makes the PI), then
/// WRPROTECT 1 (the host's PI: guard and reference tag), each read back
/// with RDPROTECT 0 and 1. **Overwrites those two blocks.** `link` asks the
/// card for the drive's link after each step. Stops when the drive no
/// longer answers TEST UNIT READY by a transport (the link gone).
pub fn write_matrix(t: &dyn Transport, link: &dyn Fn() -> Option<String>) -> Matrix {
    let mut m = Matrix { via: t.name(), ..Default::default() };
    let mut e = Vec::new();
    if let Some(i) = ask(t, &cdb::inquiry(None, 96), 96, "INQUIRY", &mut e) {
        if i.len() >= 36 {
            m.vendor = Some(text(&i[8..16]));
            m.product = Some(text(&i[16..32]));
            m.revision = Some(text(&i[32..36]));
        }
    }
    m.capacity = ask(t, &cdb::read_capacity16(32), 32, "READ CAPACITY 16", &mut e).and_then(|c| parse_capacity16(&c));
    m.mode_before = mode_pages(t);
    m.log_before = log_pages(t);
    let Some(cap) = m.capacity.clone() else {
        m.stopped = Some(format!("READ CAPACITY 16 did not answer: {}", e.join("; ")));
        return m;
    };
    let bs = cap.block_size as usize;
    let last = cap.blocks.saturating_sub(1);
    'steps: for wrprotect in [0u8, 1] {
        for lba in [0, last] {
            if wrprotect == 1 && !cap.prot_en {
                break 'steps;
            }
            let mut st = MatrixStep { lba, wrprotect, ..Default::default() };
            // A pattern that names the step, so a read of the other step's
            // block cannot pass for this one.
            let data: Vec<u8> = (0..bs).map(|i| (i as u64 ^ lba ^ (wrprotect as u64 * 0x5A)) as u8).collect();
            let pi = pi_type1(&data, lba);
            let mut out = data.clone();
            if wrprotect == 1 {
                out.extend_from_slice(&pi);
            }
            let (w, _) = said_reply(t.run(&cdb_rw16_protect(true, wrprotect, lba, 1), Dir::Out, &mut out, 60));
            st.write = w;
            let mut back = vec![0u8; bs];
            st.read0 = match t.run(&cdb_rw16_protect(false, 0, lba, 1), Dir::In, &mut back, 60) {
                Ok(r) if r.good() => if back == data { "good, the data as written".into() } else { "good, but NOT the data written".into() },
                Ok(r) => r.said(),
                Err(x) => format!("not sent: {x}"),
            };
            if cap.prot_en {
                let mut back = vec![0u8; bs + 8];
                st.read1 = match t.run(&cdb_rw16_protect(false, 1, lba, 1), Dir::In, &mut back, 60) {
                    Ok(r) if r.good() => {
                        let (d, p) = back.split_at(bs);
                        let want = pi_type1(&data, lba);
                        match (d == data.as_slice(), p == want.as_slice()) {
                            (true, true) => "good, data and PI as expected".into(),
                            (true, false) => format!("good data, PI {} (expected {})", hex(p), hex(&want)),
                            (false, _) => format!("NOT the data written; PI {}", hex(p)),
                        }
                    }
                    Ok(r) => r.said(),
                    Err(x) => format!("not sent: {x}"),
                };
            } else {
                st.read1 = "no PI on this format".into();
            }
            let tur = t.run(&cdb::test_unit_ready(), Dir::None, &mut [], 30);
            let gone = match &tur {
                Ok(r) => r.transport.is_some(),
                Err(_) => true,
            };
            st.ready_after = said_reply(tur).0;
            st.link_after = link();
            m.steps.push(st);
            if gone {
                m.stopped = Some(format!(
                    "the drive stopped answering after WRPROTECT {wrprotect} at LBA {lba}: TEST UNIT READY {}",
                    m.steps.last().map(|s| s.ready_after.clone()).unwrap_or_default()
                ));
                break 'steps;
            }
        }
    }
    m.mode_after = mode_pages(t);
    m.log_after = log_pages(t);
    let mut e2 = Vec::new();
    m.capacity_after = ask(t, &cdb::read_capacity16(32), 32, "READ CAPACITY 16", &mut e2).and_then(|c| parse_capacity16(&c));
    m
}

impl Matrix {
    /// The run as console lines (and the issue's table rows).
    pub fn lines(&self) -> Vec<String> {
        let mut v = vec![format!(
            "drive {} {} {} through {}; {}",
            self.vendor.as_deref().unwrap_or("?"),
            self.product.as_deref().unwrap_or("?"),
            self.revision.as_deref().unwrap_or("?"),
            self.via,
            self.capacity
                .as_ref()
                .map(|c| format!("{} blocks of {}{}", c.blocks, c.block_size, if c.prot_en { format!(" + PI type {}", c.pi_type) } else { ", no PI".into() }))
                .unwrap_or_else(|| "no READ CAPACITY 16".into())
        )];
        for p in &self.mode_before {
            v.push(format!("mode page {} before: {}", p.page, p.said.clone().unwrap_or_else(|| p.hex.clone())));
        }
        for (pg, s) in &self.log_before {
            v.push(format!("log page {pg} before: {s}"));
        }
        for s in &self.steps {
            v.push(format!(
                "WRPROTECT {} LBA {}: write {}; read RDPROTECT 0 {}; RDPROTECT 1 {}; then TUR {}{}",
                s.wrprotect,
                s.lba,
                s.write,
                s.read0,
                s.read1,
                s.ready_after,
                s.link_after.as_ref().map(|l| format!(", link {l}")).unwrap_or_default()
            ));
        }
        for (b, a) in self.mode_before.iter().zip(&self.mode_after) {
            if b.hex != a.hex {
                v.push(format!("mode page {} changed: {} -> {}", a.page, b.hex, a.hex));
            }
        }
        for ((pg, b), (_, a)) in self.log_before.iter().zip(&self.log_after) {
            if b != a {
                v.push(format!("log page {pg} after: {a}"));
            }
        }
        if self.capacity_after != self.capacity {
            v.push(format!("READ CAPACITY 16 after: {:?}", self.capacity_after));
        }
        if let Some(s) = &self.stopped {
            v.push(format!("STOPPED: {s}"));
        }
        v
    }
}

// ---- actions ----

/// What can be done to a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    SpinUp,
    SpinDown,
    SelfTestShort,
    SelfTestExtended,
    /// Block size and PI type (0 none, 1 type 1).
    Format { block_size: u32, pi_type: u8 },
}

impl Action {
    pub fn parse(word: &str, block_size: Option<u32>, pi: Option<&str>) -> Result<Action, String> {
        Ok(match word {
            "spin_up" => Action::SpinUp,
            "spin_down" => Action::SpinDown,
            "self_test" | "self_test_short" => Action::SelfTestShort,
            "self_test_extended" => Action::SelfTestExtended,
            "format" => {
                let bs = block_size.ok_or("format: a block size (512 or 4096)")?;
                if bs != 512 && bs != 4096 {
                    return Err(format!("format: block size {bs}: 512 or 4096"));
                }
                let pi_type = match pi.unwrap_or("none") {
                    "none" | "0" => 0,
                    "type1" | "1" => 1,
                    p => return Err(format!("format: protection {p}: none or type1")),
                };
                Action::Format { block_size: bs, pi_type }
            }
            w => return Err(format!("{w}: spin_up, spin_down, self_test, self_test_extended or format")),
        })
    }
}

fn must(r: std::io::Result<Reply>, what: &str) -> Result<(), String> {
    match r {
        Ok(r) if r.good() => Ok(()),
        Ok(r) => Err(format!("{what}: {}", r.said())),
        Err(e) => Err(format!("{what}: {e}")),
    }
}

/// Starts an action. A format is started (IMMED) and left running: follow
/// it with [`format_progress`].
pub fn act(t: &dyn Transport, a: Action) -> Result<String, String> {
    match a {
        Action::SpinUp => must(t.run(&cdb::start_stop(true), Dir::None, &mut [], 60), "START STOP UNIT (start)").map(|_| "spinning up".into()),
        Action::SpinDown => must(t.run(&cdb::start_stop(false), Dir::None, &mut [], 60), "START STOP UNIT (stop)").map(|_| "spinning down".into()),
        Action::SelfTestShort => must(t.run(&cdb::self_test(false), Dir::None, &mut [], 60), "SEND DIAGNOSTIC (short)").map(|_| "short self-test started".into()),
        Action::SelfTestExtended => must(t.run(&cdb::self_test(true), Dir::None, &mut [], 60), "SEND DIAGNOSTIC (extended)").map(|_| "extended self-test started".into()),
        Action::Format { block_size, pi_type } => {
            let mut m = vec![0u8; 64];
            let h = match t.run(&cdb::mode_sense10(0x01, true, false, 64), Dir::In, &mut m, 30) {
                Ok(r) if r.good() => parse_mode_header(&m[..r.len.min(64)]),
                _ => None,
            }
            .unwrap_or(ModeHeader { medium_type: 0, long_lba: false, descriptor: vec![] });
            let mut list = mode_select_list(&h, block_size);
            must(t.run(&cdb::mode_select10(list.len() as u16), Dir::Out, &mut list, 60), &format!("MODE SELECT (block length {block_size})"))?;
            let fmtpinfo = if pi_type == 1 { 0b10 } else { 0 };
            let mut p = cdb::format_param_immed();
            must(t.run(&cdb::format_unit(fmtpinfo), Dir::Out, &mut p, 120), "FORMAT UNIT (IMMED)")?;
            Ok(format!("FORMAT UNIT to {block_size}{} started (IMMED)", if pi_type == 1 { " with PI type 1" } else { "" }))
        }
    }
}

/// A format's state: `Some(pct)` while it runs, `None` when the drive
/// answers ready (the format ended), an error for any other answer.
pub fn format_progress(t: &dyn Transport) -> Result<Option<f32>, String> {
    let mut s = vec![0u8; 96];
    if let Ok(r) = t.run(&cdb::request_sense(96), Dir::In, &mut s, 30) {
        if r.good() {
            if let Some(p) = progress_of(&s[..r.len.min(96)]) {
                return Ok(Some(p as f32 * 100.0 / 65536.0));
            }
        }
    }
    let r = t.run(&cdb::test_unit_ready(), Dir::None, &mut [], 30).map_err(|e| format!("TEST UNIT READY: {e}"))?;
    if r.good() {
        return Ok(None);
    }
    match r.key() {
        Some((2, 0x04, 0x04)) => Ok(Some(r.progress().map(|p| p as f32 * 100.0 / 65536.0).unwrap_or(0.0))),
        Some((6, _, _)) => Ok(Some(100.0)), // a unit attention first: asked again
        _ => Err(format!("TEST UNIT READY: {}", r.said())),
    }
}

/// A span of seconds as the console says it: `7h 10m`, `58m`, `0m`.
pub fn span(secs: u64) -> String {
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    if h > 0 { format!("{h}h {m}m") } else { format!("{m}m") }
}

/// A format's minute line through the card (#595, #620), in the sg path's
/// own wording, named by SAS address: `0x…: formatting (full) 12.3%, about
/// 7h 10m left, 58m so far`. The ETA is the pace so far; none before 0.1 %.
pub fn card_format_line(addr: u64, pct: f32, elapsed_secs: u64) -> String {
    let left = if (0.1..100.0).contains(&pct) {
        let rest = elapsed_secs as f64 * f64::from(100.0 - pct) / f64::from(pct);
        format!("about {} left", span(rest as u64))
    } else if pct >= 100.0 {
        "about 0m left".to_string()
    } else {
        "no ETA yet".to_string()
    };
    format!("0x{addr:016x}: formatting (full) {pct:.1}%, {left}, {} so far", span(elapsed_secs))
}

// ---- the transports ----

/// `SG_IO` on an sg (or sd) node.
#[cfg(target_os = "linux")]
pub struct Sg {
    file: std::fs::File,
    name: String,
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[allow(dead_code)] // the kernel's struct: some fields are only its to fill
struct SgIoHdr {
    interface_id: libc::c_int,
    dxfer_direction: libc::c_int,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut libc::c_void,
    cmdp: *const u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: libc::c_int,
    usr_ptr: *mut libc::c_void,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: libc::c_int,
    duration: u32,
    info: u32,
}

#[cfg(target_os = "linux")]
impl Sg {
    pub fn open(dev: &std::path::Path) -> std::io::Result<Sg> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(dev)?;
        Ok(Sg { file, name: dev.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() })
    }
}

#[cfg(target_os = "linux")]
impl Transport for Sg {
    fn run(&self, cdb: &[u8], dir: Dir, data: &mut [u8], timeout_s: u32) -> std::io::Result<Reply> {
        use std::os::fd::AsRawFd;
        const SG_IO: libc::c_ulong = 0x2285;
        let mut sense = vec![0u8; 96];
        let mut h = SgIoHdr {
            interface_id: b'S' as libc::c_int,
            dxfer_direction: match dir {
                Dir::None => -1,
                Dir::Out => -2,
                Dir::In => -3,
            },
            cmd_len: cdb.len() as u8,
            mx_sb_len: sense.len() as u8,
            iovec_count: 0,
            dxfer_len: data.len() as u32,
            dxferp: if data.is_empty() { std::ptr::null_mut() } else { data.as_mut_ptr().cast() },
            cmdp: cdb.as_ptr(),
            sbp: sense.as_mut_ptr(),
            timeout: timeout_s.saturating_mul(1000),
            flags: 0,
            pack_id: 0,
            usr_ptr: std::ptr::null_mut(),
            status: 0,
            masked_status: 0,
            msg_status: 0,
            sb_len_wr: 0,
            host_status: 0,
            driver_status: 0,
            resid: 0,
            duration: 0,
            info: 0,
        };
        // SAFETY: `h` is the kernel's sg_io_hdr; every pointer names a live
        // buffer of the length beside it.
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), SG_IO as _, &mut h) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        sense.truncate(h.sb_len_wr as usize);
        let transport = (h.host_status != 0 || (h.driver_status & 0x0F != 0 && h.driver_status & 0x08 == 0))
            .then(|| format!("host status 0x{:x}, driver status 0x{:x}", h.host_status, h.driver_status));
        Ok(Reply { status: h.status, sense, len: data.len().saturating_sub(h.resid.max(0) as usize), transport })
    }
    fn name(&self) -> String {
        self.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_format_lines() {
        // forge1's 10 TB drive: 14 h 25 m for the whole format.
        assert_eq!(
            card_format_line(0x5000cca268077301, 10.0, 3600),
            "0x5000cca268077301: formatting (full) 10.0%, about 9h 0m left, 1h 0m so far"
        );
        assert_eq!(card_format_line(1, 0.0, 30), "0x0000000000000001: formatting (full) 0.0%, no ETA yet, 0m so far");
        assert_eq!(card_format_line(1, 100.0, 120), "0x0000000000000001: formatting (full) 100.0%, about 0m left, 2m so far");
        assert_eq!(span(59), "0m");
        assert_eq!(span(3 * 3600 + 7 * 60), "3h 7m");
    }

    #[test]
    fn cdbs() {
        assert_eq!(cdb::inquiry(Some(0x83), 1024), [0x12, 1, 0x83, 4, 0, 0]);
        assert_eq!(cdb::inquiry(None, 96), [0x12, 0, 0, 0, 96, 0]);
        assert_eq!(cdb::log_sense(0x0D, 0, 1024), [0x4D, 0, 0x4D, 0, 0, 0, 0, 4, 0, 0]);
        assert_eq!(cdb::format_unit(0b10), [0x04, 0x90, 0, 0, 0, 0]);
        assert_eq!(cdb::format_param_immed(), [0, 2, 0, 0]);
        assert_eq!(cdb::read_capacity16(32)[..2], [0x9E, 0x10]);
        assert_eq!(cdb::read_defect_data12(8)[1], 0x0D);
        assert_eq!(cdb::start_stop(true), [0x1B, 1, 0, 0, 1, 0]);
        assert_eq!(cdb::self_test(true)[1], 0x40);
    }

    #[test]
    fn capacity() {
        let mut d = vec![0u8; 32];
        d[0..8].copy_from_slice(&(2_391_801_855u64).to_be_bytes());
        d[8..12].copy_from_slice(&4096u32.to_be_bytes());
        d[12] = 0x01; // PROT_EN, P_TYPE 0 = type 1
        d[14] = 0x80;
        let c = parse_capacity16(&d).unwrap();
        assert_eq!((c.blocks, c.block_size, c.prot_en, c.pi_type, c.lbpme), (2_391_801_856, 4096, true, 1, true));
    }

    #[test]
    fn vpd83() {
        // NAA LUN name, then a target port NAA.
        let mut d = vec![0x00, 0x83, 0, 0];
        d.extend_from_slice(&[0x01, 0x03, 0, 8, 0x50, 0x00, 0xcc, 0xa2, 0x68, 0x01, 0xb6, 0x48]);
        d.extend_from_slice(&[0x61, 0x93, 0, 8, 0x50, 0x00, 0xcc, 0xa2, 0x68, 0x01, 0xb6, 0x49]);
        d[3] = (d.len() - 4) as u8;
        let ds = parse_vpd83(&d);
        assert_eq!(ds.len(), 2);
        assert_eq!(wwid_of(&ds).as_deref(), Some("naa.5000cca26801b648"));
        assert_eq!((ds[1].of, ds[1].kind, ds[1].value.as_str()), ("port", "naa", "5000cca26801b649"));
    }

    #[test]
    fn log_pages() {
        // Temperature: param 0 = 38 C, param 1 = 65 C.
        let d = [0x0D, 0, 0, 12, 0, 0, 3, 2, 0, 38, 0, 1, 3, 2, 0, 65];
        let ps = log_params(&d);
        assert_eq!(param(&ps, 0), Some(&[0u8, 38][..]));
        assert_eq!(param(&ps, 1).and_then(|v| v.get(1).copied()), Some(65));
        assert_eq!(be_var(&[0, 0, 1, 2]), 0x102);
    }

    #[test]
    fn progress_and_caching() {
        let mut s = vec![0u8; 18];
        s[0] = 0x70;
        s[2] = 0x02;
        s[12] = 0x04;
        s[13] = 0x04;
        s[15] = 0x80;
        s[16] = 0x80;
        assert_eq!(progress_of(&s), Some(0x8000));
        s[15] = 0;
        assert_eq!(progress_of(&s), None);
        let r = Reply { status: 2, sense: { let mut x = s.clone(); x[15] = 0x80; x }, len: 0, transport: None };
        assert_eq!(r.said(), "NOT READY asc/ascq 0x04/0x04");
        assert_eq!(r.progress(), Some(0x8000));
        // MODE SENSE 10, DBD: page 0x08 with WCE.
        let m = [0, 0x12, 0, 0, 0, 0, 0, 0, 0x08, 0x12, 0x04, 0];
        assert_eq!(parse_caching(&m), Some((true, false)));
    }

    #[test]
    fn mode_select_keeps_the_drives_descriptor() {
        let h = ModeHeader { medium_type: 0, long_lba: false, descriptor: vec![0xff, 0xff, 0xff, 0xff, 0, 0, 2, 0] };
        assert_eq!(mode_select_list(&h, 4096), vec![0, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0x10, 0]);
        let l = ModeHeader { medium_type: 0, long_lba: true, descriptor: vec![0; 16] };
        let v = mode_select_list(&l, 4096);
        assert_eq!((v[4], v[7], v.len()), (1, 16, 24));
        assert_eq!(&v[20..24], &4096u32.to_be_bytes());
    }

    #[test]
    fn actions_parse() {
        assert_eq!(Action::parse("format", Some(4096), Some("type1")), Ok(Action::Format { block_size: 4096, pi_type: 1 }));
        assert!(Action::parse("format", Some(520), None).is_err());
        assert!(Action::parse("format", None, None).is_err());
        assert_eq!(Action::parse("spin_up", None, None), Ok(Action::SpinUp));
        assert!(Action::parse("sanitize", None, None).is_err());
    }

    /// A drive that answers from a script: (CDB opcode, page) -> data or sense.
    struct Fake;
    impl Transport for Fake {
        fn run(&self, cdb: &[u8], _dir: Dir, data: &mut [u8], _t: u32) -> std::io::Result<Reply> {
            let mut put = |b: &[u8]| {
                let n = b.len().min(data.len());
                data[..n].copy_from_slice(&b[..n]);
                Ok(Reply { len: n, ..Default::default() })
            };
            match (cdb[0], cdb.get(2).copied().unwrap_or(0)) {
                (0x00, _) => {
                    let mut s = vec![0u8; 18];
                    s[0] = 0x70;
                    s[2] = 2;
                    s[12] = 0x31;
                    Ok(Reply { status: 2, sense: s, len: 0, transport: None })
                }
                (0x12, 0) if cdb[1] == 0 => {
                    let mut i = vec![0u8; 36];
                    i[8..16].copy_from_slice(b"HGST    ");
                    i[16..32].copy_from_slice(b"H7210A520SUN010T");
                    i[32..36].copy_from_slice(b"A3B0");
                    put(&i)
                }
                (0x12, 0) => put(&[0, 0, 0, 3, 0x00, 0x80, 0x83]),
                (0x12, 0x80) => put(&[0, 0x80, 0, 8, b'0', b'0', b'1', b'7', b'5', b'1', b'6', b'5']),
                (0x12, 0x83) => put(&[0, 0x83, 0, 12, 0x01, 0x03, 0, 8, 0x50, 0x00, 0xcc, 0xa2, 0x68, 0x01, 0xb6, 0x48]),
                (0x4D, 0x4D) => put(&[0x0D, 0, 0, 6, 0, 0, 3, 2, 0, 38]),
                _ => Ok(Reply { status: 2, sense: vec![0x70, 0, 5, 0, 0, 0, 0, 10, 0, 0, 0, 0, 0x20, 0], len: 0, transport: None }),
            }
        }
        fn name(&self) -> String {
            "fake".into()
        }
    }

    /// forge1 sdb, #625: formatted with PI, the format left corrupt.
    struct Corrupt;
    impl Transport for Corrupt {
        fn run(&self, cdb: &[u8], _dir: Dir, data: &mut [u8], _t: u32) -> std::io::Result<Reply> {
            let sense = |k: u8, a: u8, q: u8| {
                let mut s = vec![0u8; 18];
                s[0] = 0x70;
                s[2] = k;
                s[12] = a;
                s[13] = q;
                s
            };
            match cdb[0] {
                0x00 => Ok(Reply::default()),
                0x03 => Ok(Reply { len: 18, ..Default::default() }),
                0x5A => {
                    data[..8].copy_from_slice(&[0, 6, 0, 0, 0, 0, 0, 0]);
                    Ok(Reply { len: 8, ..Default::default() })
                }
                0x9E => {
                    data[..32].fill(0);
                    data[..8].copy_from_slice(&0x917_FFFFFu64.to_be_bytes());
                    data[8..12].copy_from_slice(&4096u32.to_be_bytes());
                    data[12] = 0x01;
                    Ok(Reply { len: 32, ..Default::default() })
                }
                0x88 => Ok(Reply::default()),
                0x8A => Ok(Reply { status: 2, sense: sense(3, 0x31, 0), len: 0, transport: None }),
                _ => Ok(Reply { status: 2, sense: sense(5, 0x20, 0), len: 0, transport: None }),
            }
        }
        fn name(&self) -> String {
            "sdb".into()
        }
    }

    #[test]
    fn a_write_that_fails_is_explained() {
        assert_eq!(cdb::write16(0, 1), [0x8A, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0]);
        assert_eq!(cdb::read16(0x0102, 2)[8..14], [0x01, 0x02, 0, 0, 0, 2]);
        let p = write_probe(&Corrupt, 4096);
        assert_eq!(p.pending.as_deref(), Some("good"));
        assert_eq!(p.write_protect, Some(false));
        assert_eq!(p.capacity.as_ref().map(|c| (c.block_size, c.pi_type)), Some((4096, 1)));
        assert_eq!(p.read_lba0.as_deref(), Some("good"));
        assert_eq!(p.write_lba0.as_deref(), Some("MEDIUM ERROR asc/ascq 0x31/0x00"));
        assert!(p.hint().unwrap().contains("FORMAT UNIT again"));
        let l = p.lines().join("\n");
        assert!(l.contains("+ PI type 1") && l.contains("write protect: off") && l.contains("so: "), "{l}");
        // A write that goes through: the path, not the drive.
        let ok = WriteProbe { write_good: Some(true), ..Default::default() };
        assert!(ok.hint().unwrap().contains("the path"));
        let wp = WriteProbe { write_protect: Some(true), ..Default::default() };
        assert!(wp.hint().unwrap().contains("write protected"));
        let pi = WriteProbe { senses: vec![(0xB, 0x10, 0x03)], ..Default::default() };
        assert!(pi.hint().unwrap().contains("reference tag"));
    }

    #[test]
    fn crc_t10dif_and_pi() {
        // The standard check value of CRC-16/T10-DIF.
        assert_eq!(crc_t10dif(b"123456789"), 0xD0DB);
        assert_eq!(crc_t10dif(&[0u8; 4096]), 0);
        let pi = pi_type1(b"123456789", 0x1_0000_0007);
        assert_eq!(pi, [0xD0, 0xDB, 0, 0, 0, 0, 0, 7]);
        let c = cdb_rw16_protect(true, 1, 5, 1);
        assert_eq!((c[0], c[1], c[9], c[13]), (0x8A, 0x20, 5, 1));
        assert_eq!(cdb_rw16_protect(false, 1, 0, 1)[..2], [0x88, 0x20]);
    }

    #[test]
    fn mode_pages_said() {
        // MODE SENSE(10) header (no block descriptors), then the control page.
        let mut d = vec![0, 0, 0, 0, 0, 0, 0, 0];
        d.extend_from_slice(&[0x0A, 0x0A, 0x04, 0x10, 0x08, 0x80, 0, 0, 0, 0, 0, 0]);
        let len = (d.len() - 2) as u16;
        d[..2].copy_from_slice(&len.to_be_bytes());
        let p = mode_page_of(&d).unwrap();
        assert_eq!(p.len(), 12);
        assert_eq!(mode_page_said(p).unwrap(), "control: DPICZ 1 D_SENSE 0 QAM 1 SWP 1 ATO 1 TAS 0");
        assert_eq!(mode_page_said(&[0x08, 0x12, 0x05]).unwrap(), "caching: WCE 1 RCD 1");
    }

    /// A drive at 4096 + PI type 1 that keeps what it is sent: the host's PI
    /// as given, its own (correct) PI for a WRPROTECT 0 write; and one that
    /// drops its link on the first write (forge1 bay 3, #625/#627).
    struct PiDrive {
        blocks: std::cell::RefCell<std::collections::HashMap<u64, Vec<u8>>>,
        drop_link: bool,
        gone: std::cell::Cell<bool>,
    }
    impl Transport for PiDrive {
        fn run(&self, cdb: &[u8], _dir: Dir, data: &mut [u8], _t: u32) -> std::io::Result<Reply> {
            if self.gone.get() {
                return Ok(Reply { transport: Some("host status 0x1, driver status 0x0".into()), ..Default::default() });
            }
            let lba = u64::from_be_bytes(cdb.get(2..10).map(|s| s.try_into().unwrap()).unwrap_or([0; 8]));
            let prot = cdb.get(1).copied().unwrap_or(0) >> 5;
            match cdb[0] {
                0x9E => {
                    data[..32].fill(0);
                    data[..8].copy_from_slice(&999u64.to_be_bytes());
                    data[8..12].copy_from_slice(&4096u32.to_be_bytes());
                    data[12] = 1;
                    Ok(Reply { len: 32, ..Default::default() })
                }
                0x12 => {
                    let mut i = vec![0u8; 36];
                    i[8..16].copy_from_slice(b"NETAPP  ");
                    i[16..32].copy_from_slice(b"X378_HLBRE10TA07");
                    i[32..36].copy_from_slice(b"NA02");
                    let n = data.len().min(36);
                    data[..n].copy_from_slice(&i[..n]);
                    Ok(Reply { len: n, ..Default::default() })
                }
                0x8A => {
                    if self.drop_link {
                        self.gone.set(true);
                        return Ok(Reply { transport: Some("host status 0x1, driver status 0x0".into()), ..Default::default() });
                    }
                    let mut b = data[..4096].to_vec();
                    b.extend_from_slice(&if prot == 1 { data[4096..4104].try_into().unwrap() } else { pi_type1(&data[..4096], lba) });
                    self.blocks.borrow_mut().insert(lba, b);
                    Ok(Reply::default())
                }
                0x88 => {
                    let b = self.blocks.borrow().get(&lba).cloned().unwrap_or_else(|| vec![0; 4104]);
                    let n = if prot == 0 { 4096 } else { 4104 };
                    data[..n].copy_from_slice(&b[..n]);
                    Ok(Reply { len: n, ..Default::default() })
                }
                0x00 => Ok(Reply::default()),
                _ => Ok(Reply { status: 2, sense: vec![0x70, 0, 5, 0, 0, 0, 0, 10, 0, 0, 0, 0, 0x24, 0], len: 0, transport: None }),
            }
        }
        fn name(&self) -> String {
            "sg9".into()
        }
    }

    #[test]
    fn the_write_matrix() {
        let d = PiDrive { blocks: Default::default(), drop_link: false, gone: Default::default() };
        let m = write_matrix(&d, &|| Some("12.0 Gbit".into()));
        assert_eq!(m.vendor.as_deref(), Some("NETAPP"));
        assert_eq!(m.revision.as_deref(), Some("NA02"));
        assert_eq!(m.steps.len(), 4, "{:?}", m.steps);
        assert_eq!((m.steps[0].wrprotect, m.steps[0].lba), (0, 0));
        assert_eq!((m.steps[3].wrprotect, m.steps[3].lba), (1, 999));
        for s in &m.steps {
            assert_eq!(s.write, "good");
            assert_eq!(s.read0, "good, the data as written");
            assert_eq!(s.read1, "good, data and PI as expected");
            assert_eq!(s.link_after.as_deref(), Some("12.0 Gbit"));
        }
        assert!(m.stopped.is_none());
        assert!(m.mode_before.iter().all(|p| p.said.as_deref().is_some_and(|s| s.starts_with("refused"))));
        let l = m.lines().join("\n");
        assert!(l.contains("drive NETAPP X378_HLBRE10TA07 NA02 through sg9; 1000 blocks of 4096 + PI type 1"), "{l}");
        assert!(l.contains("WRPROTECT 1 LBA 999: write good"), "{l}");

        // The link dropped on the first write: said, and the run stops there.
        let d = PiDrive { blocks: Default::default(), drop_link: true, gone: Default::default() };
        let m = write_matrix(&d, &|| Some("down".into()));
        assert_eq!(m.steps.len(), 1);
        assert!(m.steps[0].write.starts_with("host status"), "{:?}", m.steps[0]);
        assert_eq!(m.steps[0].link_after.as_deref(), Some("down"));
        assert!(m.stopped.as_deref().unwrap().contains("after WRPROTECT 0 at LBA 0"));
    }

    #[test]
    fn the_kernels_lines_for_a_drive() {
        let recs: Vec<String> = [
            "6,100,1,-;sd 0:0:3:0: [sdb] tag#9 Sense Key : Medium Error [current]",
            "6,101,1,-;sd 0:0:3:0: [sdb] tag#9 Add. Sense: Medium format corrupted",
            "3,102,1,-;I/O error, dev sdb, sector 0 op 0x1:(WRITE) flags 0x8800 phys_seg 1 prio class 0",
            "6,103,1,-;sd 0:0:2:0: [sdc] 2441609216 4096-byte logical blocks",
            "4,104,1,-;mpt3sas_cm0: log_info(0x31110610): originator(PL), code(0x11), sub_code(0x0610)",
            "6,105,1,-;sdba: sdba1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let v = kernel_lines_of(&recs, "sdb", 10);
        assert_eq!(v.len(), 4, "{v:?}");
        assert!(v[2].starts_with("I/O error, dev sdb"));
        assert!(v[3].starts_with("mpt3sas_cm0: log_info"));
        assert_eq!(kernel_lines_of(&recs, "sdb", 2).len(), 2);
    }

    #[test]
    fn details_of_a_not_ready_drive() {
        let d = details(&Fake);
        assert_eq!(d.ready.as_deref(), Some("NOT READY asc/ascq 0x31/0x00"));
        assert_eq!(d.product.as_deref(), Some("H7210A520SUN010T"));
        assert_eq!(d.serial.as_deref(), Some("00175165"));
        assert_eq!(d.wwid.as_deref(), Some("naa.5000cca26801b648"));
        assert_eq!(d.temperature_c, Some(38));
        assert!(d.capacity.is_none());
        assert!(d.errors.iter().any(|e| e.starts_with("READ CAPACITY 16: ILLEGAL REQUEST")), "{:?}", d.errors);
    }
}
