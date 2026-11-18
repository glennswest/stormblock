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
    /// MODE SENSE(10) with the page-control field (#636): 0 current values,
    /// 1 changeable (a mask of what MODE SELECT may set), 2 default, 3 saved.
    pub fn mode_sense10_pc(page: u8, pc: u8, dbd: bool, alloc: u16) -> [u8; 10] {
        let mut c = mode_sense10(page, false, dbd, alloc);
        c[2] = ((pc & 0x03) << 6) | (page & 0x3F);
        c
    }
    pub fn mode_sense10(page: u8, llbaa: bool, dbd: bool, alloc: u16) -> [u8; 10] {
        let [hi, lo] = alloc.to_be_bytes();
        [0x5A, (if llbaa { 0x10 } else { 0 }) | (if dbd { 0x08 } else { 0 }), page & 0x3F, 0, 0, 0, 0, hi, lo, 0]
    }
    /// MODE SENSE(10) of a subpage (#646: 0x19/1, phy control and
    /// discover), no block descriptors, `pc` as above.
    pub fn mode_sense10_sub(page: u8, sub: u8, pc: u8, alloc: u16) -> [u8; 10] {
        let mut c = mode_sense10_pc(page, pc, true, alloc);
        c[3] = sub;
        c
    }
    /// PF=1, SP as asked (#646: SP=1 saves the page, so it holds across a
    /// power cycle).
    pub fn mode_select10_sp(len: u16, sp: bool) -> [u8; 10] {
        let mut c = mode_select10(len);
        c[1] |= u8::from(sp);
        c
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
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
    /// Every page `vpd_pages` lists, read whole (#636).
    pub vpd: Vec<VpdPage>,
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
    let list: Vec<u8> = if pages.len() >= 4 { pages[4..(4 + pages[3] as usize).min(pages.len())].to_vec() } else { vec![] };
    d.vpd = vpd_all(t, &list);
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
pub fn kernel_lines(name: &str, extra: &[String], max: usize) -> Vec<String> {
    kernel_lines_of(&kmsg_records(), name, extra, max)
}

/// [`kernel_lines`] over records already read (`pri,seq,usec,flags;text`).
/// `extra` are more words a line may carry instead (#625: the drive's SAS
/// address, which the link's lines name it by).
pub fn kernel_lines_of(records: &[String], name: &str, extra: &[String], max: usize) -> Vec<String> {
    let mut marks = vec![format!("[{name}]"), format!("dev {name},"), format!("{name}:"), format!(" {name} ")];
    marks.extend(extra.iter().filter(|e| !e.is_empty()).cloned());
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

// ---- after a format: the drive brought back (#620) ----

/// One round of asking a drive that has just been formatted whether it is
/// ready (#620: forge1's three drives, 850 min of FORMAT UNIT through the
/// card, then 9 s of waiting before the install gave up on them).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadyRound {
    pub tur: String,
    /// START UNIT was sent: the drive said it needs one (NOT READY, 04/02).
    pub started: bool,
    pub capacity: Option<Capacity>,
    pub ready: bool,
}

/// TEST UNIT READY; START UNIT when the drive asks for one; READ CAPACITY 16.
pub fn ready_round(t: &dyn Transport) -> ReadyRound {
    let mut r = ReadyRound::default();
    match t.run(&cdb::test_unit_ready(), Dir::None, &mut [], 30) {
        Ok(rep) => {
            r.tur = rep.said();
            r.ready = rep.good();
            // "not ready, initializing command required": spun down, or
            // stopped after the format.
            if rep.key() == Some((2, 0x04, 0x02)) {
                r.started = t.run(&cdb::start_stop(true), Dir::None, &mut [], 60).is_ok_and(|x| x.good());
            }
        }
        Err(e) => r.tur = format!("not sent: {e}"),
    }
    let mut c = vec![0u8; 32];
    if let Ok(rep) = t.run(&cdb::read_capacity16(32), Dir::In, &mut c, 30) {
        if rep.good() {
            r.capacity = parse_capacity16(&c[..rep.len.min(32)]);
        }
    }
    r
}

impl ReadyRound {
    pub fn said(&self) -> String {
        format!(
            "TEST UNIT READY {}{}; READ CAPACITY 16 {}",
            self.tur,
            if self.started { ", START UNIT sent" } else { "" },
            self.capacity
                .as_ref()
                .map(|c| format!("{} blocks of {}{}", c.blocks, c.block_size, if c.prot_en { format!(" + PI type {}", c.pi_type) } else { ", no PI".into() }))
                .unwrap_or_else(|| "no answer".into())
        )
    }
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
    /// The current values.
    pub hex: String,
    /// The changeable values (#636): the bits MODE SELECT may set.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub changeable: String,
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
            "control: DPICZ {} D_SENSE {} QAM {} QERR {} SWP {} ATO {} TAS {}",
            (b(2) >> 2) & 1, (b(2) >> 3) & 1, b(3) >> 4, (b(3) >> 1) & 3, (b(4) >> 3) & 1, b(5) >> 7, (b(5) >> 6) & 1
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

// ---- the SAS link's counters (#642) ----

/// One phy's link counters, from the drive's log page 0x18 or the HBA's
/// `sas_phy` (SPL: SAS phy log descriptor). Lifetime totals, which saturate
/// at 2^32 - 1 on an old drive: the rate between samples is what says now.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct PhyLink {
    pub phy: u8,
    /// Negotiated logical link rate in Mbit/s (0 when not up).
    pub rate_mbps: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attached_sas: Option<String>,
    pub invalid_dwords: u32,
    pub disparity_errors: u32,
    pub loss_of_sync: u32,
    pub phy_resets: u32,
}

/// A SAS negotiated-link-rate code as Mbit/s (SPL: 8 = 1.5, 9 = 3, 0xA = 6,
/// 0xB = 12, 0xC = 22.5 Gbit/s).
pub fn sas_rate_mbps(code: u8) -> u32 {
    match code & 0x0F {
        0x8 => 1500,
        0x9 => 3000,
        0xA => 6000,
        0xB => 12000,
        0xC => 22500,
        _ => 0,
    }
}

/// Log page 0x18 (protocol-specific port, SAS): each phy's counters.
pub fn parse_sas_port_log(d: &[u8]) -> Vec<PhyLink> {
    let mut out = Vec::new();
    for (_, p) in log_params(d) {
        // A SAS port parameter: protocol (bits 3-0 of byte 0) 6 = SAS, the
        // number of phys at byte 3, then a descriptor each.
        if p.len() < 4 || p[0] & 0x0F != 6 {
            continue;
        }
        let mut o = 4;
        for _ in 0..p[3] {
            let Some(desc) = p.get(o..) else { break };
            if desc.len() < 48 {
                break;
            }
            let len = 4 + desc[3] as usize;
            out.push(PhyLink {
                phy: desc[1],
                rate_mbps: sas_rate_mbps(desc[5]),
                attached_sas: Some(format!("0x{:016x}", be64(desc, 16))).filter(|a| a != "0x0000000000000000"),
                invalid_dwords: be32(desc, 32),
                disparity_errors: be32(desc, 36),
                loss_of_sync: be32(desc, 40),
                phy_resets: be32(desc, 44),
            });
            o += len.max(48);
        }
    }
    out
}

/// The drive's own phys, read now (LOG SENSE 0x18, cumulative values).
pub fn sas_phys(t: &dyn Transport) -> Result<Vec<PhyLink>, String> {
    let mut e = Vec::new();
    match ask(t, &cdb::log_sense(0x18, 0, 1024), 1024, "LOG SENSE 0x18", &mut e) {
        Some(d) => Ok(parse_sas_port_log(&d)),
        None => Err(e.join("; ")),
    }
}

/// What moved between two samples of the same phys, in words (#642: the
/// write test and the lay say `link: loss of sync +N` instead of guessing
/// at the PI path). Empty when nothing moved.
pub fn link_moved(before: &[PhyLink], after: &[PhyLink]) -> Vec<String> {
    let mut out = Vec::new();
    for a in after {
        let Some(b) = before.iter().find(|b| b.phy == a.phy) else { continue };
        let d = |x: u32, y: u32| y.wrapping_sub(x);
        let mut parts = Vec::new();
        for (name, x, y) in [
            ("loss of sync", b.loss_of_sync, a.loss_of_sync),
            ("invalid dwords", b.invalid_dwords, a.invalid_dwords),
            ("disparity errors", b.disparity_errors, a.disparity_errors),
            ("phy resets", b.phy_resets, a.phy_resets),
        ] {
            // A saturated counter (2^32 - 1) says nothing now.
            if x != u32::MAX && y > x {
                parts.push(format!("{name} +{}", d(x, y)));
            }
        }
        if !parts.is_empty() {
            out.push(format!("phy {} at {} Mbit/s: {}", a.phy, a.rate_mbps, parts.join(", ")));
        }
    }
    out
}

// ---- the drive's own link rates, mode page 0x19/1 (#646) ----

/// One phy's rates from the drive's phy control and discover subpage
/// (SPL: mode page 0x19 subpage 1): what it negotiated, the minimum and
/// maximum its firmware programs (MODE SELECT may change them) and what its
/// hardware can. forge1 bay 3's EMC-firmware HITACHI: hardware 12 Gbit,
/// programmed 6, so it negotiated 6 on a 12 Gbit HBA phy.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct PhyRates {
    pub phy: u8,
    pub negotiated_mbps: u32,
    pub programmed_min_mbps: u32,
    pub programmed_max_mbps: u32,
    pub hardware_min_mbps: u32,
    pub hardware_max_mbps: u32,
}

/// The SAS rate code of a rate in Mbit/s (the inverse of [`sas_rate_mbps`]).
pub fn sas_rate_code(mbps: u32) -> Option<u8> {
    (0x8..=0xC).find(|c| sas_rate_mbps(*c) == mbps)
}

/// Mbit/s as Gbit/s in words: `6`, `12`, `1.5`, `22.5`.
pub fn gbit(mbps: u32) -> String {
    if mbps % 1000 == 0 {
        (mbps / 1000).to_string()
    } else {
        format!("{:.1}", mbps as f64 / 1000.0)
    }
}

/// A subpage out of a MODE SENSE(10) answer: header, block descriptors,
/// then the page with its 2-byte length (SPF = 1).
pub fn mode_subpage_of(d: &[u8]) -> Option<&[u8]> {
    if d.len() < 8 {
        return None;
    }
    let total = (be16(d, 0) as usize + 2).min(d.len());
    let bdl = be16(d, 6) as usize;
    let p = d.get(8 + bdl..total)?;
    if p.len() < 4 || p[0] & 0x40 == 0 {
        return None;
    }
    let len = 4 + be16(p, 2) as usize;
    p.get(..len.min(p.len()))
}

/// Offset of the first phy descriptor in subpage 0x19/1, and each one's size.
const PHY_DESC_AT: usize = 8;
const PHY_DESC_LEN: usize = 48;

/// Each phy's rates from subpage 0x19/1 (the page from its first byte).
pub fn parse_phy_control(p: &[u8]) -> Vec<PhyRates> {
    if p.len() < PHY_DESC_AT || p[0] & 0x3F != 0x19 || p[1] != 1 {
        return Vec::new();
    }
    (0..p[7] as usize)
        .map_while(|i| p.get(PHY_DESC_AT + i * PHY_DESC_LEN..PHY_DESC_AT + (i + 1) * PHY_DESC_LEN))
        .map(|d| PhyRates {
            phy: d[1],
            negotiated_mbps: sas_rate_mbps(d[5]),
            programmed_min_mbps: sas_rate_mbps(d[32] >> 4),
            hardware_min_mbps: sas_rate_mbps(d[32]),
            programmed_max_mbps: sas_rate_mbps(d[33] >> 4),
            hardware_max_mbps: sas_rate_mbps(d[33]),
        })
        .collect()
}

/// The MODE SELECT(10) parameter list that programs every phy's maximum
/// to `want` Mbit/s (each phy's hardware maximum when `None`, or when
/// `want` is above it): an 8-byte header with no block descriptor, then
/// the page as the drive gave it (PS cleared), only the programmed maximum
/// changed. Every other field stays the current value, as SPC asks.
pub fn phy_control_select(page: &[u8], want: Option<u32>) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    let mut p = page.to_vec();
    p[0] &= 0x7F;
    for i in 0..p.get(7).copied().unwrap_or(0) as usize {
        let at = PHY_DESC_AT + i * PHY_DESC_LEN + 33;
        let Some(b) = p.get(at).copied() else { break };
        let hw = sas_rate_mbps(b);
        let to = want.map_or(hw, |w| w.min(hw));
        if let Some(code) = sas_rate_code(to) {
            p[at] = (code << 4) | (b & 0x0F);
        }
    }
    v.extend(p);
    v
}

/// The drive's phys' rates, read now (MODE SENSE 0x19/1, current values).
pub fn phy_rates(t: &dyn Transport) -> Result<Vec<PhyRates>, String> {
    let mut e = Vec::new();
    let d = ask(t, &cdb::mode_sense10_sub(0x19, 1, 0, 1024), 1024, "MODE SENSE 0x19/1", &mut e).ok_or_else(|| e.join("; "))?;
    let p = mode_subpage_of(&d).ok_or("MODE SENSE 0x19/1: an answer with no subpage in it")?;
    Ok(parse_phy_control(p))
}

/// What the drive programs, in words, when it holds a phy below what its
/// hardware can (`the drive programs 6 Gbit (hardware 12)`); `None` when
/// every phy is programmed to its hardware maximum.
pub fn programmed_below(r: &[PhyRates]) -> Option<String> {
    let p = r.iter().find(|p| p.programmed_max_mbps > 0 && p.programmed_max_mbps < p.hardware_max_mbps)?;
    Some(format!("the drive programs {} Gbit (hardware {})", gbit(p.programmed_max_mbps), gbit(p.hardware_max_mbps)))
}

/// Program the drive's maximum link rate (#646): raised to `want` Mbit/s
/// (capped at what its hardware can) when it programs less, or set back to
/// the hardware maximum (`None`). A drive that already programs at least
/// that is left alone (the HBA's maximum holds a link lower). MODE SELECT
/// with SP = 1 when the page is savable, so it holds across a power cycle;
/// read back. Answers what was done; a refusal with the drive's sense.
pub fn set_programmed_max(t: &dyn Transport, want: Option<u32>) -> Result<String, String> {
    let mut e = Vec::new();
    let cur = ask(t, &cdb::mode_sense10_sub(0x19, 1, 0, 1024), 1024, "MODE SENSE 0x19/1", &mut e).ok_or_else(|| e.join("; "))?;
    let page = mode_subpage_of(&cur).ok_or("MODE SENSE 0x19/1: an answer with no subpage in it")?.to_vec();
    let before = parse_phy_control(&page);
    if before.is_empty() {
        return Err("mode page 0x19/1 lists no phy".into());
    }
    let said = |r: &[PhyRates]| {
        let p = &r[0];
        format!("drive programmed {} (hardware {})", gbit(p.programmed_max_mbps), gbit(p.hardware_max_mbps))
    };
    let target = |p: &PhyRates| want.map_or(p.hardware_max_mbps, |w| w.min(p.hardware_max_mbps));
    let needed = before.iter().any(|p| match want {
        Some(_) => p.programmed_max_mbps < target(p),
        None => p.programmed_max_mbps != p.hardware_max_mbps,
    });
    if !needed {
        return Ok(format!("{}, left as it is", said(&before[..])));
    }
    let chg = ask(t, &cdb::mode_sense10_sub(0x19, 1, 1, 1024), 1024, "MODE SENSE 0x19/1 (changeable)", &mut e)
        .and_then(|c| mode_subpage_of(&c).map(<[u8]>::to_vec))
        .ok_or_else(|| e.join("; "))?;
    for (i, p) in before.iter().enumerate() {
        let m = chg.get(PHY_DESC_AT + i * PHY_DESC_LEN + 33).copied().unwrap_or(0);
        if m & 0xF0 != 0xF0 {
            return Err(format!("phy {}: the programmed maximum is not changeable (changeable mask 0x{m:02x})", p.phy));
        }
    }
    let mut list = phy_control_select(&page, want);
    let savable = page[0] & 0x80 != 0;
    let r = t.run(&cdb::mode_select10_sp(list.len() as u16, savable), Dir::Out, &mut list, 30).map_err(|e| format!("MODE SELECT 0x19/1: not sent: {e}"))?;
    if !r.good() {
        return Err(format!("MODE SELECT 0x19/1 refused: {}", r.said()));
    }
    let after = phy_rates(t)?;
    let was = gbit(before[0].programmed_max_mbps);
    if after.iter().zip(&before).any(|(a, b)| a.programmed_max_mbps != target(b)) {
        return Err(format!("MODE SELECT 0x19/1 taken, but the drive still programs {} (asked {})", gbit(after[0].programmed_max_mbps), gbit(target(&before[0]))));
    }
    Ok(format!("{}, was {was}{}", said(&after[..]), if savable { ", saved" } else { "; the page is not savable: set again on every boot" }))
}

// ---- error and PI-check counters (#648) ----

/// One of the error counter log pages (0x02 write, 0x03 read, 0x05 verify),
/// lifetime totals as the drive keeps them.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ErrorPage {
    /// Parameter 0: corrected without substantial delay (ECC on the fly).
    pub corrected_fast: Option<u64>,
    /// Parameter 1: corrected with possible delays (a retry).
    pub corrected_delayed: Option<u64>,
    /// Parameter 2: total rewrites (write) or rereads (read, verify).
    pub retries: Option<u64>,
    /// Parameter 3: total errors corrected.
    pub corrected: Option<u64>,
    /// Parameter 6: total uncorrected errors.
    pub uncorrected: Option<u64>,
}

pub fn parse_error_page(d: &[u8]) -> ErrorPage {
    let ps = log_params(d);
    let v = |c| param(&ps, c).map(be_var);
    ErrorPage { corrected_fast: v(0), corrected_delayed: v(1), retries: v(2), corrected: v(3), uncorrected: v(6) }
}

/// The drive's own error counters, read now (#648): every one `None` when
/// the drive keeps no such page.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ErrorCounters {
    pub write: Option<ErrorPage>,
    pub read: Option<ErrorPage>,
    pub verify: Option<ErrorPage>,
    /// Log page 0x06, parameter 0: non-medium errors (the link, the
    /// controller, a timeout: anything that is not the platter).
    pub non_medium: Option<u64>,
    /// Log page 0x2F, parameter 0: the last informational exception, ASC/ASCQ
    /// (`0x00/0x00` none; `0x5d/..` a SMART trip). The page holds the last
    /// one, not a count.
    pub exception: Option<String>,
    /// READ DEFECT DATA (12), the grown list's length.
    pub grown_defects: Option<u32>,
    /// A SATA drive's SMART raw values (ATA PASS-THROUGH 16): 5 reallocated
    /// sectors, 197 pending, 198 offline uncorrectable. A SAS drive keeps
    /// none of these; its grown list is what grows instead.
    pub reallocated: Option<u64>,
    pub pending: Option<u64>,
    pub offline_uncorrectable: Option<u64>,
    /// What did not answer.
    pub errors: Vec<String>,
}

/// Log page 0x00: the pages the drive keeps.
pub fn parse_supported_logs(d: &[u8]) -> Vec<u8> {
    let n = be16(d, 2) as usize;
    d.get(4..(4 + n).min(d.len())).map(|s| s.iter().map(|p| p & 0x3F).collect()).unwrap_or_default()
}

/// ATA PASS-THROUGH (16), SMART READ DATA: PIO data-in, one 512-byte block.
pub fn cdb_smart_read_data() -> [u8; 16] {
    // protocol 4 (PIO data-in) << 1; T_DIR in, BYT_BLOK, T_LENGTH in the
    // sector count; FEATURES 0xD0, COUNT 1, LBA mid/high 0x4F/0xC2, 0xB0.
    [0x85, 4 << 1, 0x0E, 0, 0xD0, 0, 1, 0, 0, 0, 0x4F, 0, 0xC2, 0, 0xB0, 0]
}

/// The SMART attribute table: raw values by id (30 entries of 12 bytes from
/// offset 2, the raw value 6 bytes little-endian at 5).
pub fn parse_smart_attrs(d: &[u8]) -> Vec<(u8, u64)> {
    (0..30)
        .filter_map(|i| {
            let e = d.get(2 + i * 12..14 + i * 12)?;
            (e[0] != 0).then(|| (e[0], e[5..11].iter().rev().fold(0u64, |a, b| (a << 8) | *b as u64)))
        })
        .collect()
}

/// Every error counter the drive keeps, read now (#648). `ata` asks SMART
/// too (a SATA drive: INQUIRY's vendor is `ATA`).
pub fn error_counters(t: &dyn Transport, ata: bool) -> ErrorCounters {
    let mut c = ErrorCounters::default();
    let mut e = Vec::new();
    let pages = ask(t, &cdb::log_sense(0, 0, 255), 255, "LOG SENSE 0x00", &mut e).map(|d| parse_supported_logs(&d));
    // A drive that lists no pages is asked the standard ones anyway.
    let keeps = |p: u8| pages.as_ref().is_none_or(|l| l.contains(&p));
    let log = |page: u8, e: &mut Vec<String>| ask(t, &cdb::log_sense(page, 0, 512), 512, &format!("LOG SENSE 0x{page:02x}"), e);
    if keeps(0x02) {
        c.write = log(0x02, &mut e).map(|d| parse_error_page(&d));
    }
    if keeps(0x03) {
        c.read = log(0x03, &mut e).map(|d| parse_error_page(&d));
    }
    if keeps(0x05) {
        c.verify = log(0x05, &mut e).map(|d| parse_error_page(&d));
    }
    if keeps(0x06) {
        c.non_medium = log(0x06, &mut e).and_then(|d| param(&log_params(&d), 0).map(be_var));
    }
    if keeps(0x2F) {
        c.exception = log(0x2F, &mut e).and_then(|d| {
            let ps = log_params(&d);
            param(&ps, 0).filter(|v| v.len() >= 2).map(|v| format!("0x{:02x}/0x{:02x}", v[0], v[1]))
        });
    }
    if let Some(h) = ask(t, &cdb::read_defect_data12(8), 8, "READ DEFECT DATA 12", &mut e) {
        c.grown_defects = (h.len() >= 8).then(|| be32(&h, 4) / 8);
    }
    if ata {
        if let Some(d) = ask(t, &cdb_smart_read_data(), 512, "SMART READ DATA", &mut e) {
            let a = parse_smart_attrs(&d);
            let raw = |id| a.iter().find(|(i, _)| *i == id).map(|(_, v)| *v);
            c.reallocated = raw(5);
            c.pending = raw(197);
            c.offline_uncorrectable = raw(198);
        }
    }
    c.errors = e;
    c
}

/// What the kernel said of a drive's failed commands, counted (#648): the
/// sense lines sd prints (`Sense Key : …`, `Add. Sense: …`), by device.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct SenseCounts {
    /// ASC 0x10, ASCQ 1/2/3 the drive raised (ABORTED COMMAND or MEDIUM
    /// ERROR): its own PI check failed (bit rot, a torn write, a misdirected
    /// one).
    pub pi_guard: u64,
    pub pi_app_tag: u64,
    pub pi_ref_tag: u64,
    /// The same ASC raised by the card (mpt3sas makes ILLEGAL REQUEST of an
    /// IOC EEDP error, `_scsih_eedp_error_handling`): the PI the card
    /// inserted or stripped did not check.
    pub eedp_guard: u64,
    pub eedp_app_tag: u64,
    pub eedp_ref_tag: u64,
    pub medium_error: u64,
    pub recovered_error: u64,
    pub hardware_error: u64,
}

fn sense_key_word(s: &str) -> Option<u8> {
    let s = s.trim().to_ascii_lowercase();
    const NAMES: [(&str, u8); 7] = [
        ("recovered error", 0x1),
        ("not ready", 0x2),
        ("medium error", 0x3),
        ("hardware error", 0x4),
        ("illegal request", 0x5),
        ("unit attention", 0x6),
        ("aborted command", 0xB),
    ];
    if let Some((_, k)) = NAMES.iter().find(|(n, _)| s.starts_with(n)) {
        return Some(*k);
    }
    u8::from_str_radix(s.strip_prefix("0x")?.split_whitespace().next()?, 16).ok()
}

/// The PI check an `Add. Sense:` text names: 1 guard, 2 application tag,
/// 3 reference tag (also `ASC=0x10 ASCQ=0x1` from a kernel without the
/// names).
fn pi_check_of(add: &str) -> Option<u8> {
    let a = add.trim().to_ascii_lowercase();
    if a.starts_with("logical block guard check failed") {
        return Some(1);
    }
    if a.starts_with("logical block application tag check failed") {
        return Some(2);
    }
    if a.starts_with("logical block reference tag check failed") {
        return Some(3);
    }
    let (asc, ascq) = a.strip_prefix("asc=")?.split_once(" ascq=")?;
    let n = |s: &str| u8::from_str_radix(s.trim().trim_start_matches("0x").split_whitespace().next().unwrap_or(""), 16).ok();
    match (n(asc)?, n(ascq)?) {
        (0x10, q @ 1..=3) => Some(q),
        _ => None,
    }
}

/// Counts the kernel's sense lines (#648), kept across reads of `/dev/kmsg`:
/// by device (`0:0:1:0`, sd's prefix), only records after the last one
/// seen.
#[derive(Debug, Default)]
pub struct SenseTally {
    pub last_seq: Option<u64>,
    pub by_device: std::collections::HashMap<String, SenseCounts>,
    /// The sense key of a command whose `Add. Sense` line has not come yet,
    /// by device and tag.
    keys: std::collections::HashMap<(String, String), u8>,
}

impl SenseTally {
    /// Takes records as `/dev/kmsg` gives them (`pri,seq,usec,flags;text`).
    pub fn take(&mut self, records: &[String]) {
        for r in records {
            let Some(first) = r.lines().next() else { continue };
            let Some((head, text)) = first.split_once(';') else { continue };
            let seq: Option<u64> = head.split(',').nth(1).and_then(|s| s.parse().ok());
            if let (Some(s), Some(l)) = (seq, self.last_seq) {
                if s <= l {
                    continue;
                }
            }
            if seq.is_some() {
                self.last_seq = seq;
            }
            self.line(text);
        }
    }

    /// One line: `sd 0:0:1:0: [sdb] tag#12 Sense Key : Medium Error [current]`.
    pub fn line(&mut self, text: &str) {
        let Some(rest) = text.strip_prefix("sd ") else { return };
        let Some((dev, rest)) = rest.split_once(": ") else { return };
        let mut rest = rest.trim_start();
        if rest.starts_with('[') {
            rest = rest.split_once("] ").map(|(_, r)| r).unwrap_or("");
        }
        let mut tag = String::new();
        if let Some(t) = rest.strip_prefix("tag#") {
            let (n, r) = t.split_once(' ').unwrap_or((t, ""));
            tag = n.to_string();
            rest = r;
        }
        let rest = rest.trim_start();
        let k = (dev.to_string(), tag);
        if let Some(s) = rest.strip_prefix("Sense Key : ").or_else(|| rest.strip_prefix("Sense Key: ")) {
            let Some(key) = sense_key_word(s) else { return };
            self.keys.insert(k.clone(), key);
            let c = self.by_device.entry(k.0).or_default();
            match key {
                0x1 => c.recovered_error += 1,
                0x3 => c.medium_error += 1,
                0x4 => c.hardware_error += 1,
                _ => {}
            }
        } else if let Some(add) = rest.strip_prefix("Add. Sense: ") {
            let key = self.keys.remove(&k);
            let Some(q) = pi_check_of(add) else { return };
            let c = self.by_device.entry(k.0).or_default();
            let card = key == Some(0x5);
            match (card, q) {
                (false, 1) => c.pi_guard += 1,
                (false, 2) => c.pi_app_tag += 1,
                (false, _) => c.pi_ref_tag += 1,
                (true, 1) => c.eedp_guard += 1,
                (true, 2) => c.eedp_app_tag += 1,
                (true, _) => c.eedp_ref_tag += 1,
            }
        }
        // Bound what is waiting: a key whose Add. Sense never came.
        if self.keys.len() > 4096 {
            self.keys.clear();
        }
    }
}

/// `/dev/kmsg`'s records now (non-blocking, from the oldest the buffer
/// holds).
#[cfg(target_os = "linux")]
pub fn kmsg_records() -> Vec<String> {
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
            Err(e) if e.raw_os_error() == Some(libc::EPIPE) => continue,
            Err(_) => break,
        }
    }
    all
}

// ---- every VPD page (#636) ----

/// A VPD page as the drive answered it: raw always, what it says when the
/// layout is a standard one, and the printable strings in it (a vendor
/// page's layout is the vendor's; its bytes are shown, never trusted).
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct VpdPage {
    pub page: String,
    pub hex: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub strings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub said: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Runs of four or more printable ASCII characters (a vendor page's
/// firmware names, part numbers).
pub fn ascii_strings(b: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for &c in b {
        if (0x20..0x7F).contains(&c) {
            cur.push(c as char);
        } else {
            if cur.trim().len() >= 4 {
                out.push(cur.trim().to_string());
            }
            cur.clear();
        }
    }
    if cur.trim().len() >= 4 {
        out.push(cur.trim().to_string());
    }
    out
}

/// What a standard VPD page says (SPC-5 / SBC-4 layouts).
pub fn vpd_said(d: &[u8]) -> Option<String> {
    let b = |i: usize| d.get(i).copied().unwrap_or(0);
    let body = || d.get(4..(4 + be16(d, 2) as usize).min(d.len())).unwrap_or(&[]);
    Some(match b(1) {
        0x00 => format!("pages: {}", body().iter().map(|p| format!("0x{p:02x}")).collect::<Vec<_>>().join(" ")),
        0x80 => format!("serial: {}", text(body())),
        0x83 => {
            let ds = parse_vpd83(d);
            format!("{} designator(s){}", ds.len(), wwid_of(&ds).map(|w| format!(", {w}")).unwrap_or_default())
        }
        0x86 => format!(
            "extended INQUIRY: ACTIVATE_MICROCODE {} SPT {} GRD_CHK {} APP_CHK {} REF_CHK {} UASK_SUP {} GROUP_SUP {} PRIOR_SUP {} HEADSUP {} ORDSUP {} SIMPSUP {} WU_SUP {} CRD_SUP {} NV_SUP {} V_SUP {}",
            b(4) >> 6, (b(4) >> 3) & 7, (b(4) >> 2) & 1, (b(4) >> 1) & 1, b(4) & 1,
            (b(5) >> 5) & 1, (b(5) >> 4) & 1, (b(5) >> 3) & 1, (b(5) >> 2) & 1, (b(5) >> 1) & 1, b(5) & 1,
            (b(6) >> 3) & 1, (b(6) >> 2) & 1, (b(6) >> 1) & 1, b(6) & 1
        ),
        0x8F => "third-party copy (raw)".into(),
        0x90 => "protocol-specific logical unit information (raw)".into(),
        0x91 => "protocol-specific port information (raw)".into(),
        0xB0 => format!("block limits: maximum transfer {} blocks, optimal {} blocks", be32(d, 8), be32(d, 12)),
        0xB1 => format!("block device characteristics: rotation {} (1 = non-rotating), form factor code {}", be16(d, 4), b(7) & 0x0F),
        0xB2 => format!("logical block provisioning: LBPU {} LBPWS {} LBPWS10 {}", b(5) >> 7, (b(5) >> 6) & 1, (b(5) >> 5) & 1),
        0xB7 => "block limits extension (raw)".into(),
        _ => return None,
    })
}

/// Every VPD page `list` names, each read whole (up to 4 KiB).
pub fn vpd_all(t: &dyn Transport, list: &[u8]) -> Vec<VpdPage> {
    list.iter()
        .map(|&pg| {
            let page = format!("0x{pg:02x}");
            let mut buf = vec![0u8; 255];
            let first = t.run(&cdb::inquiry(Some(pg), 255), Dir::In, &mut buf, 30);
            let got = match first {
                Ok(r) if r.good() => {
                    buf.truncate(r.len.min(255));
                    let want = 4 + be16(&buf, 2) as usize;
                    if want > buf.len() && want <= 4096 {
                        let mut big = vec![0u8; want];
                        match t.run(&cdb::inquiry(Some(pg), want as u16), Dir::In, &mut big, 30) {
                            Ok(r) if r.good() => {
                                big.truncate(r.len.min(want));
                                Ok(big)
                            }
                            _ => Ok(buf),
                        }
                    } else {
                        Ok(buf)
                    }
                }
                Ok(r) => Err(r.said()),
                Err(e) => Err(format!("not sent: {e}")),
            };
            match got {
                Ok(d) => VpdPage { page, hex: hex(&d), strings: if pg >= 0xC0 { ascii_strings(&d) } else { vec![] }, said: vpd_said(&d), error: None },
                Err(e) => VpdPage { page, error: Some(e), ..Default::default() },
            }
        })
        .collect()
}

fn unhex(h: &str) -> Vec<u8> {
    (0..h.len() / 2).filter_map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).ok()).collect()
}

/// Several drives' pages side by side (#636): for each page any of them
/// has, `=` and what it says when it is the same on every drive, else each
/// drive's bytes in rows of 16 with the bytes that differ from the first
/// drive's in brackets, and what each says.
pub fn compare_pages(drives: &[(String, Vec<(String, String, Option<String>)>)]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for (_, pages) in drives {
        for (k, _, _) in pages {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }
    let mut out = Vec::new();
    for k in keys {
        let got: Vec<(&String, Option<&(String, String, Option<String>)>)> =
            drives.iter().map(|(d, ps)| (d, ps.iter().find(|(pk, _, _)| *pk == k))).collect();
        let hexes: Vec<Option<&str>> = got.iter().map(|(_, p)| p.map(|p| p.1.as_str())).collect();
        if hexes.iter().all(|h| *h == hexes[0]) && hexes[0].is_some() {
            let said = got[0].1.and_then(|p| p.2.clone()).unwrap_or_default();
            out.push(format!("{k}  = on every drive{}", if said.is_empty() { String::new() } else { format!(": {said}") }));
            continue;
        }
        out.push(format!("{k}  DIFFERS"));
        let first: Vec<u8> = hexes.iter().flatten().next().map(|h| unhex(h)).unwrap_or_default();
        for (d, p) in &got {
            let Some((_, h, said)) = p else {
                out.push(format!("  {d}: not on this drive"));
                continue;
            };
            if let Some(s) = said {
                out.push(format!("  {d}: {s}"));
            }
            let bytes = unhex(h);
            for (row, chunk) in bytes.chunks(16).enumerate() {
                let cells: Vec<String> = chunk
                    .iter()
                    .enumerate()
                    .map(|(i, b)| {
                        let at = row * 16 + i;
                        if first.get(at) == Some(b) { format!("{b:02x}") } else { format!("[{b:02x}]") }
                    })
                    .collect();
                out.push(format!("  {d} {:04x}: {}", row * 16, cells.join(" ")));
            }
        }
    }
    out
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
            let mut mp = match t.run(&cdb::mode_sense10(pg, false, true, 252), Dir::In, &mut m, 30) {
                Ok(r) if r.good() => match mode_page_of(&m[..r.len.min(252)]) {
                    Some(p) => ModePage { page, hex: hex(p), said: mode_page_said(p), ..Default::default() },
                    None => ModePage { page, said: Some("an answer with no page in it".into()), ..Default::default() },
                },
                Ok(r) => ModePage { page, said: Some(format!("refused: {}", r.said())), ..Default::default() },
                Err(e) => ModePage { page, said: Some(format!("not sent: {e}")), ..Default::default() },
            };
            // What may be changed (#636), for a page the drive has.
            if !mp.hex.is_empty() {
                let mut c = vec![0u8; 252];
                if let Ok(r) = t.run(&cdb::mode_sense10_pc(pg, 1, true, 252), Dir::In, &mut c, 30) {
                    if r.good() {
                        mp.changeable = mode_page_of(&c[..r.len.min(252)]).map(hex).unwrap_or_default();
                    }
                }
            }
            mp
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
    /// The drive's own link counters (log page 0x18) before and after
    /// (#642: forge1 bay 3's failures were the link, not the PI path).
    pub link_before: Vec<PhyLink>,
    pub link_after: Vec<PhyLink>,
    /// The drive's own phys' rates (#646, mode page 0x19/1): what its
    /// firmware programs against what its hardware can.
    pub link_rates: Vec<PhyRates>,
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
    m.link_before = sas_phys(t).unwrap_or_default();
    m.link_rates = phy_rates(t).unwrap_or_default();
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
    m.link_after = sas_phys(t).unwrap_or_default();
    let mut e2 = Vec::new();
    m.capacity_after = ask(t, &cdb::read_capacity16(32), 32, "READ CAPACITY 16", &mut e2).and_then(|c| parse_capacity16(&c));
    m
}

impl Matrix {
    /// Every write and read good, nothing stopped: the drive takes writes at
    /// its format (#621: the canary's verdict, a fallback's reason).
    pub fn passed(&self) -> bool {
        self.stopped.is_none()
            && !self.steps.is_empty()
            && self.steps.iter().all(|s| {
                s.write == "good" && s.read0.starts_with("good, the data as written") && (s.read1.starts_with("good, data and PI") || s.read1 == "no PI on this format")
            })
    }

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
        if let Some(p) = programmed_below(&self.link_rates) {
            v.push(format!("link: {p}: storage.drives[].linkRate or hba linkrate raises it (#646)"));
        }
        if !self.link_before.is_empty() {
            let moved = link_moved(&self.link_before, &self.link_after);
            if moved.is_empty() {
                v.push("link (the drive's side): nothing moved during the test".into());
            }
            for l in moved {
                v.push(format!("link (the drive's side): {l} during the test"));
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

// ---- what a drive is, cheaply (#621) ----

/// A drive's identity and format, from INQUIRY and READ CAPACITY 16: what
/// the card's rows carry (#621: "show the firmware revision in the row").
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Ident {
    pub vendor: Option<String>,
    pub product: Option<String>,
    pub revision: Option<String>,
    pub capacity: Option<Capacity>,
}

impl Ident {
    /// Whether the drive is at `block_size` with PI `pi_type` (0 none).
    pub fn at(&self, block_size: u32, pi_type: u8) -> bool {
        self.capacity.as_ref().is_some_and(|c| c.block_size == block_size && (if pi_type == 0 { !c.prot_en } else { c.prot_en && c.pi_type == pi_type }))
    }
    /// NetApp firmware (#621, #627): built for 520/4160-byte sectors and its
    /// own checksums; a shelf of it is formatted one drive first.
    pub fn netapp(&self) -> bool {
        self.vendor.as_deref().is_some_and(|v| v.trim().eq_ignore_ascii_case("NETAPP"))
    }
}

/// INQUIRY and READ CAPACITY 16, now (a drive not ready answers no capacity).
pub fn ident(t: &dyn Transport) -> Ident {
    let mut e = Vec::new();
    let mut i = Ident::default();
    if let Some(d) = ask(t, &cdb::inquiry(None, 96), 96, "INQUIRY", &mut e) {
        if d.len() >= 36 {
            i.vendor = Some(text(&d[8..16]));
            i.product = Some(text(&d[16..32]));
            i.revision = Some(text(&d[32..36]));
        }
    }
    i.capacity = ask(t, &cdb::read_capacity16(32), 32, "READ CAPACITY 16", &mut e).and_then(|c| parse_capacity16(&c));
    i
}

// ---- drive firmware (#621: "can we update the drive firmware") ----

/// READ BUFFER (10), mode 0x03: the descriptor of buffer `id`.
pub fn cdb_read_buffer_descriptor(id: u8) -> [u8; 10] {
    [0x3C, 0x03, id, 0, 0, 0, 0, 0, 4, 0]
}

/// WRITE BUFFER (10): `mode` (0x07 download with offsets, save and activate;
/// 0x0E with offsets, save, defer activate; 0x0F activate deferred), the
/// buffer offset and the parameter list length.
pub fn cdb_write_buffer(mode: u8, id: u8, offset: u32, len: u32) -> [u8; 10] {
    let o = offset.to_be_bytes();
    let l = len.to_be_bytes();
    [0x3B, mode & 0x1F, id, o[1], o[2], o[3], l[1], l[2], l[3], 0]
}

/// The chunks a microcode image goes down in: at most `capacity` and 64 KiB
/// (what the card's pass-through takes), a multiple of the drive's offset
/// boundary (2^`boundary`; 0xFF: none asked).
pub fn microcode_chunk(capacity: u32, boundary: u8) -> u32 {
    let cap = if capacity == 0 { 64 * 1024 } else { capacity.min(64 * 1024) };
    if boundary == 0xFF || boundary >= 31 {
        return cap;
    }
    let b = 1u32 << boundary;
    (cap / b * b).max(b)
}

/// Download microcode (#621): the vendor's image, through sg or the card,
/// in offset chunks (mode 0x0E, then 0x0F to activate; or 0x07, which
/// activates when the last chunk is in). The revision is read before and
/// after (INQUIRY). Never another vendor's image: the drive refuses one it
/// does not take, and that refusal is said.
pub fn download_microcode(t: &dyn Transport, image: &[u8], mode: u8) -> Result<Vec<String>, String> {
    if image.is_empty() {
        return Err("the firmware image is empty".into());
    }
    if image.len() > 0x00FF_FFFF {
        return Err(format!("the firmware image is {} bytes: WRITE BUFFER (10) offsets reach 16 MiB", image.len()));
    }
    if mode != 0x07 && mode != 0x0E {
        return Err(format!("WRITE BUFFER mode 0x{mode:02x}: 0x07 or 0x0e"));
    }
    let before = ident(t);
    let mut out = vec![format!(
        "firmware: {} {} revision {} before, through {}",
        before.vendor.as_deref().unwrap_or("?"), before.product.as_deref().unwrap_or("?"),
        before.revision.as_deref().unwrap_or("?"), t.name()
    )];
    let mut d = [0u8; 4];
    let (boundary, capacity) = match t.run(&cdb_read_buffer_descriptor(0), Dir::In, &mut d, 30) {
        Ok(r) if r.good() => (d[0], u32::from_be_bytes([0, d[1], d[2], d[3]])),
        Ok(r) => {
            out.push(format!("firmware: READ BUFFER descriptor: {} (64 KiB chunks)", r.said()));
            (0xFF, 0)
        }
        Err(e) => {
            out.push(format!("firmware: READ BUFFER descriptor not sent: {e} (64 KiB chunks)"));
            (0xFF, 0)
        }
    };
    let chunk = microcode_chunk(capacity, boundary) as usize;
    out.push(format!("firmware: {} bytes in {} chunk(s) of {} (mode 0x{mode:02x})", image.len(), image.len().div_ceil(chunk), chunk));
    let mut off = 0usize;
    while off < image.len() {
        let n = chunk.min(image.len() - off);
        let mut part = image[off..off + n].to_vec();
        match t.run(&cdb_write_buffer(mode, 0, off as u32, n as u32), Dir::Out, &mut part, 300) {
            Ok(r) if r.good() => {}
            Ok(r) => return Err(format!("{}; WRITE BUFFER at offset {off}: {}", out.join("; "), r.said())),
            Err(e) => return Err(format!("{}; WRITE BUFFER at offset {off}: not sent: {e}", out.join("; "))),
        }
        off += n;
    }
    if mode == 0x0E {
        must(t.run(&cdb_write_buffer(0x0F, 0, 0, 0), Dir::None, &mut [], 300), "WRITE BUFFER (activate deferred microcode)")
            .map_err(|e| format!("{}; {e}", out.join("; ")))?;
    }
    let after = ident(t);
    out.push(format!(
        "firmware: revision {} after{}",
        after.revision.as_deref().unwrap_or("? (the drive did not answer INQUIRY yet)"),
        if after.revision.is_some() && after.revision == before.revision { " - the same: the drive may activate it at its next power cycle" } else { "" }
    ));
    Ok(out)
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

    /// #620: a drive that needs START UNIT after its format gets one, and
    /// says its new geometry.
    struct Stopped {
        started: std::cell::Cell<bool>,
    }
    impl Transport for Stopped {
        fn run(&self, cdb: &[u8], _dir: Dir, data: &mut [u8], _t: u32) -> std::io::Result<Reply> {
            let sense = |k: u8, a: u8, q: u8| vec![0x70, 0, k, 0, 0, 0, 0, 10, 0, 0, 0, 0, a, q];
            match cdb[0] {
                0x00 if self.started.get() => Ok(Reply::default()),
                0x00 => Ok(Reply { status: 2, sense: sense(2, 0x04, 0x02), len: 0, transport: None }),
                0x1B => {
                    self.started.set(true);
                    Ok(Reply::default())
                }
                0x9E => {
                    data[..32].fill(0);
                    data[..8].copy_from_slice(&0x917_FFFFFu64.to_be_bytes());
                    data[8..12].copy_from_slice(&4096u32.to_be_bytes());
                    data[12] = 1;
                    Ok(Reply { len: 32, ..Default::default() })
                }
                _ => Ok(Reply { status: 2, sense: sense(5, 0x20, 0), len: 0, transport: None }),
            }
        }
        fn name(&self) -> String {
            "card".into()
        }
    }

    #[test]
    fn a_formatted_drive_brought_back() {
        let d = Stopped { started: Default::default() };
        let r = ready_round(&d);
        assert!(!r.ready && r.started, "{r:?}");
        assert_eq!(r.said(), "TEST UNIT READY NOT READY asc/ascq 0x04/0x02, START UNIT sent; READ CAPACITY 16 2441084928 blocks of 4096 + PI type 1");
        let r = ready_round(&d);
        assert!(r.ready && !r.started);
        assert!(r.said().starts_with("TEST UNIT READY good; "));
    }

    #[test]
    fn microcode_chunks_and_cdbs() {
        assert_eq!(microcode_chunk(0, 0xFF), 65536);
        assert_eq!(microcode_chunk(32768, 9), 32768);
        assert_eq!(microcode_chunk(1_000_000, 9), 65536);
        assert_eq!(microcode_chunk(1000, 9), 512);
        assert_eq!(cdb_write_buffer(0x0E, 0, 0x012345, 0x8000), [0x3B, 0x0E, 0, 0x01, 0x23, 0x45, 0, 0x80, 0, 0]);
        assert_eq!(cdb_read_buffer_descriptor(0), [0x3C, 0x03, 0, 0, 0, 0, 0, 0, 4, 0]);
        let i = Ident { vendor: Some("NETAPP".into()), capacity: Some(Capacity { block_size: 4096, prot_en: true, pi_type: 1, ..Default::default() }), ..Default::default() };
        assert!(i.netapp() && i.at(4096, 1) && !i.at(4096, 0) && !i.at(512, 0));
    }

    /// A log page 0x18 with one SAS port parameter and two phys (SPL), the
    /// first at 12 Gbit with forge1 bay 3's counters.
    pub fn sas_port_page(sync0: u32, sync1: u32) -> Vec<u8> {
        let desc = |phy: u8, rate: u8, inv: u32, disp: u32, sync: u32, reset: u32| {
            let mut v = vec![0u8; 48];
            v[1] = phy;
            v[3] = 44;
            v[5] = rate;
            v[8..16].copy_from_slice(&0x5000cca26a819e69u64.to_be_bytes());
            v[16..24].copy_from_slice(&0x52cea7f04e8aa500u64.to_be_bytes());
            v[32..36].copy_from_slice(&inv.to_be_bytes());
            v[36..40].copy_from_slice(&disp.to_be_bytes());
            v[40..44].copy_from_slice(&sync.to_be_bytes());
            v[44..48].copy_from_slice(&reset.to_be_bytes());
            v
        };
        let mut param = vec![0x06, 0, 0, 2];
        param.extend(desc(0, 0xB, u32::MAX, u32::MAX, sync0, 0));
        param.extend(desc(1, 0x0, 0, 0, sync1, 0));
        let mut p = vec![0x18, 0, 0, 0];
        p.extend_from_slice(&[0, 1, 0x03, param.len() as u8]);
        p.extend(param);
        let n = (p.len() - 4) as u16;
        p[2..4].copy_from_slice(&n.to_be_bytes());
        p
    }

    /// Subpage 0x19/1 as forge1 bay 3's HITACHI (EMC firmware) answers it:
    /// two phys, phy 0 at 6 Gbit, byte 32 0x88, byte 33 0xab (programmed
    /// max 6, hardware 12); changeable f0 ff. Savable (PS) as asked.
    fn phy_control_page(max: u8, ps: bool) -> Vec<u8> {
        let mut p = vec![0x40 | 0x19 | if ps { 0x80 } else { 0 }, 1, 0, 0, 0, 6, 0, 2];
        for phy in 0..2u8 {
            let mut d = vec![0u8; 48];
            d[1] = phy;
            d[5] = if phy == 0 { 0x0A } else { 0 };
            d[8..16].copy_from_slice(&0x5000cca27ea19055u64.to_be_bytes());
            d[32] = 0x88;
            d[33] = max;
            p.extend(d);
        }
        let n = (p.len() - 4) as u16;
        p[2..4].copy_from_slice(&n.to_be_bytes());
        p
    }

    /// A drive with subpage 0x19/1 that takes MODE SELECT as a drive does.
    struct Bay3 {
        max: std::cell::Cell<u8>,
        ps: bool,
        changeable: u8,
        selects: std::cell::RefCell<Vec<(u8, Vec<u8>)>>,
    }
    impl Transport for Bay3 {
        fn run(&self, cdb: &[u8], _dir: Dir, data: &mut [u8], _t: u32) -> std::io::Result<Reply> {
            match cdb[0] {
                0x5A if cdb[2] & 0x3F == 0x19 && cdb[3] == 1 => {
                    let mut page = phy_control_page(self.max.get(), self.ps);
                    if cdb[2] >> 6 == 1 {
                        for i in 0..2 {
                            let d = &mut page[8 + i * 48..8 + (i + 1) * 48];
                            d.fill(0);
                            d[32] = 0xf0;
                            d[33] = self.changeable;
                        }
                    }
                    let mut v = vec![0u8; 8];
                    v.extend(page);
                    let n = (v.len() - 2) as u16;
                    v[0..2].copy_from_slice(&n.to_be_bytes());
                    let k = v.len().min(data.len());
                    data[..k].copy_from_slice(&v[..k]);
                    Ok(Reply { len: k, ..Default::default() })
                }
                0x55 => {
                    let p = &data[8..];
                    self.max.set(p[8 + 33]);
                    self.selects.borrow_mut().push((cdb[1], data.to_vec()));
                    Ok(Reply::default())
                }
                _ => Ok(Reply { status: 2, sense: vec![0x70, 0, 5, 0, 0, 0, 0, 10, 0, 0, 0, 0, 0x20, 0], len: 0, transport: None }),
            }
        }
        fn name(&self) -> String {
            "bay3".into()
        }
    }

    #[test]
    fn the_drives_programmed_link_rate() {
        let r = parse_phy_control(&phy_control_page(0xab, true));
        assert_eq!(r.len(), 2);
        assert_eq!(
            r[0],
            PhyRates { phy: 0, negotiated_mbps: 6000, programmed_min_mbps: 1500, hardware_min_mbps: 1500, programmed_max_mbps: 6000, hardware_max_mbps: 12000 }
        );
        assert_eq!(programmed_below(&r).as_deref(), Some("the drive programs 6 Gbit (hardware 12)"));
        assert_eq!(programmed_below(&parse_phy_control(&phy_control_page(0xbb, true))), None);
        assert_eq!((gbit(1500), gbit(12000)), ("1.5".to_string(), "12".to_string()));
        assert_eq!(sas_rate_code(12000), Some(0xB));
        // linkRate 12: raised, saved (SP = 1), only byte 33's high nibble changed.
        let d = Bay3 { max: std::cell::Cell::new(0xab), ps: true, changeable: 0xff, selects: Default::default() };
        assert_eq!(phy_rates(&d).unwrap()[0].programmed_max_mbps, 6000);
        assert_eq!(set_programmed_max(&d, Some(12000)).unwrap(), "drive programmed 12 (hardware 12), was 6, saved");
        let sel = d.selects.borrow();
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].0, 0x11, "PF and SP");
        let mut want = vec![0u8; 8];
        let mut page = phy_control_page(0xbb, false);
        page[0] &= 0x7F;
        want.extend(page);
        assert_eq!(sel[0].1, want, "the page as given, PS cleared, programmed max 12 on both phys");
        drop(sel);
        // Already there: nothing sent.
        assert_eq!(set_programmed_max(&d, Some(12000)).unwrap(), "drive programmed 12 (hardware 12), left as it is");
        assert_eq!(d.selects.borrow().len(), 1);
        // A lower ask is the HBA's to hold: the drive is left alone.
        assert!(set_programmed_max(&d, Some(6000)).unwrap().ends_with("left as it is"));
        // Above the hardware: capped at it. Not savable: SP = 0, said.
        let d = Bay3 { max: std::cell::Cell::new(0xab), ps: false, changeable: 0xff, selects: Default::default() };
        assert_eq!(set_programmed_max(&d, Some(22500)).unwrap(), "drive programmed 12 (hardware 12), was 6; the page is not savable: set again on every boot");
        assert_eq!(d.selects.borrow()[0].0, 0x10);
        // hw from a programmed 3: the hardware maximum.
        let d = Bay3 { max: std::cell::Cell::new(0x9b), ps: true, changeable: 0xff, selects: Default::default() };
        assert!(set_programmed_max(&d, None).unwrap().starts_with("drive programmed 12 (hardware 12), was 3"));
        // Not changeable: refused, nothing sent.
        let d = Bay3 { max: std::cell::Cell::new(0xab), ps: true, changeable: 0x0f, selects: Default::default() };
        assert!(set_programmed_max(&d, Some(12000)).unwrap_err().contains("not changeable (changeable mask 0x0f)"));
        assert!(d.selects.borrow().is_empty());
    }

    #[test]
    fn the_sas_link_counters() {
        let v = parse_sas_port_log(&sas_port_page(73_506_628, 0));
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].rate_mbps, 12000);
        assert_eq!(v[0].attached_sas.as_deref(), Some("0x52cea7f04e8aa500"));
        assert_eq!(v[0].invalid_dwords, u32::MAX);
        assert_eq!(v[0].loss_of_sync, 73_506_628);
        assert_eq!(v[1].rate_mbps, 0);
        let after = parse_sas_port_log(&sas_port_page(73_506_928, 0));
        // The saturated counters say nothing; loss of sync moved by 300.
        assert_eq!(link_moved(&v, &after), vec!["phy 0 at 12000 Mbit/s: loss of sync +300".to_string()]);
        assert!(link_moved(&v, &v).is_empty());
        assert_eq!(sas_rate_mbps(0xA), 6000);
    }

    /// #636: a page's words and its bytes, side by side across drives.
    #[test]
    fn vpd_pages_said_and_compared() {
        // 0x86: ACTIVATE_MICROCODE 1, SPT 1, GRD_CHK 1, APP_CHK 0, REF_CHK 1.
        let ext = [0x00, 0x86, 0x00, 0x3C, 0x4D, 0x07, 0x01, 0x00];
        let s = vpd_said(&ext).unwrap();
        assert!(s.contains("ACTIVATE_MICROCODE 1 SPT 1 GRD_CHK 1 APP_CHK 0 REF_CHK 1"), "{s}");
        assert!(vpd_said(&[0x00, 0x00, 0x00, 0x03, 0x00, 0x86, 0xD1]).unwrap().ends_with("0x00 0x86 0xd1"));
        assert_eq!(vpd_said(&[0, 0xD1, 0, 0]), None, "a vendor page has no said");
        assert_eq!(ascii_strings(b"\x00\x01NA02\x00X378_HLBRE10TA07\x05ab"), vec!["NA02", "X378_HLBRE10TA07"]);
        let cmp = compare_pages(&[
            ("bay0".into(), vec![("vpd 0x86".into(), "00863c4d".into(), Some("GRD_CHK 1".into())), ("mode 0x0a".into(), "0a0a".into(), None)]),
            ("bay3".into(), vec![("vpd 0x86".into(), "00863c4c".into(), Some("GRD_CHK 0".into())), ("mode 0x0a".into(), "0a0a".into(), None), ("vpd 0xd3".into(), "00d3".into(), None)]),
        ]);
        let all = cmp.join("\n");
        assert!(all.contains("vpd 0x86  DIFFERS"), "{all}");
        assert!(all.contains("bay3 0000: 00 86 3c [4c]"), "{all}");
        assert!(all.contains("mode 0x0a  = on every drive"), "{all}");
        assert!(all.contains("bay0: not on this drive"), "{all}");
        assert_eq!(cdb::mode_sense10_pc(0x0A, 1, true, 252)[2], 0x4A);
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
        assert_eq!(mode_page_said(p).unwrap(), "control: DPICZ 1 D_SENSE 0 QAM 1 QERR 0 SWP 1 ATO 1 TAS 0");
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
                    let mut i = [0u8; 36];
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
        let v = kernel_lines_of(&recs, "sdb", &[], 10);
        assert_eq!(v.len(), 4, "{v:?}");
        assert!(v[2].starts_with("I/O error, dev sdb"));
        assert!(v[3].starts_with("mpt3sas_cm0: log_info"));
        assert_eq!(kernel_lines_of(&recs, "sdb", &[], 2).len(), 2);
        // #625: the link's lines name the drive by its SAS address only.
        let gone: Vec<String> = [
            "6,200,1,-;mpt3sas_cm0: handle(0xc) sas_address(0x5000cca2680ab8d9) port_type(0x1)",
            "6,201,1,-;mpt3sas_cm0: removing handle(0x000b), sas_addr(0x5000cca26a819e69)",
            "6,202,1,-;mpt3sas_cm0: enclosure logical id(0x52cea7f04e8aa500), slot(3)",
            "6,203,1,-;sd 1:0:1:0: [sda] Synchronizing SCSI cache",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let v = kernel_lines_of(&gone, "sda", &["5000cca26a819e69".into()], 10);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v[0].contains("removing handle(0x000b)"));
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

    /// A log page from (code, value) parameters.
    fn log_page(page: u8, ps: &[(u16, &[u8])]) -> Vec<u8> {
        let mut p = vec![page, 0, 0, 0];
        for (c, v) in ps {
            p.extend_from_slice(&c.to_be_bytes());
            p.extend_from_slice(&[0x02, v.len() as u8]);
            p.extend_from_slice(v);
        }
        let n = (p.len() - 4) as u16;
        p[2..4].copy_from_slice(&n.to_be_bytes());
        p
    }

    /// #648: a HITACHI's read error page, the pages it keeps, a SATA drive's
    /// SMART table.
    #[test]
    fn error_pages_read() {
        let d = log_page(0x03, &[(0, &[0, 0, 0x01, 0x00]), (1, &[0, 0, 0, 2]), (2, &[0, 0, 0, 3]), (3, &[0, 0, 1, 2]), (4, &[0, 0, 0, 9]), (6, &[0, 0, 0, 0, 0, 0, 0, 1])]);
        assert_eq!(
            parse_error_page(&d),
            ErrorPage { corrected_fast: Some(256), corrected_delayed: Some(2), retries: Some(3), corrected: Some(258), uncorrected: Some(1) }
        );
        assert_eq!(parse_supported_logs(&[0, 0, 0, 4, 0x00, 0x02, 0x03, 0x2F]), vec![0, 2, 3, 0x2F]);
        let mut smart = vec![0u8; 512];
        for (i, (id, raw)) in [(5u8, 8u64), (197, 3), (198, 1)].iter().enumerate() {
            let e = 2 + i * 12;
            smart[e] = *id;
            smart[e + 5..e + 11].copy_from_slice(&raw.to_le_bytes()[..6]);
        }
        assert_eq!(parse_smart_attrs(&smart), vec![(5, 8), (197, 3), (198, 1)]);
        assert_eq!(cdb_smart_read_data()[..3], [0x85, 0x08, 0x0E]);
        assert_eq!(cdb_smart_read_data()[14], 0xB0);
    }

    /// #648: the counters through a transport: only the pages listed, the
    /// grown list's length, SMART for an ATA drive.
    #[test]
    fn error_counters_asked() {
        struct D;
        impl Transport for D {
            fn run(&self, cdb: &[u8], _dir: Dir, data: &mut [u8], _t: u32) -> std::io::Result<Reply> {
                let b: Vec<u8> = match (cdb[0], cdb.get(2).map(|p| p & 0x3F)) {
                    (0x4D, Some(0x00)) => vec![0, 0, 0, 4, 0x00, 0x02, 0x03, 0x06],
                    (0x4D, Some(0x02)) => log_page(0x02, &[(6, &[0, 0, 0, 0])]),
                    (0x4D, Some(0x03)) => log_page(0x03, &[(1, &[0, 0, 0, 7]), (6, &[0, 0, 0, 2])]),
                    (0x4D, Some(0x06)) => log_page(0x06, &[(0, &[0, 0, 0, 5])]),
                    (0xB7, _) => vec![0, 0x0D, 0, 0, 0, 0, 0, 24],
                    (0x85, _) => {
                        let mut s = vec![0u8; 512];
                        s[2] = 197;
                        s[7] = 4;
                        s
                    }
                    (0x4D, Some(p)) => panic!("asked a page the drive does not list: 0x{p:02x}"),
                    _ => return Ok(Reply { status: 2, sense: vec![0x70, 0, 5, 0, 0, 0, 0, 10, 0, 0, 0, 0, 0x20, 0], len: 0, transport: None }),
                };
                let n = b.len().min(data.len());
                data[..n].copy_from_slice(&b[..n]);
                Ok(Reply { len: n, ..Default::default() })
            }
            fn name(&self) -> String {
                "d".into()
            }
        }
        let c = error_counters(&D, true);
        assert_eq!(c.read.as_ref().and_then(|p| p.uncorrected), Some(2));
        assert_eq!(c.read.as_ref().and_then(|p| p.corrected_delayed), Some(7));
        assert_eq!(c.write.as_ref().and_then(|p| p.uncorrected), Some(0));
        assert_eq!((c.verify.as_ref(), c.exception.as_ref()), (None, None), "not listed, not asked");
        assert_eq!(c.non_medium, Some(5));
        assert_eq!(c.grown_defects, Some(3));
        assert_eq!((c.pending, c.reallocated), (Some(4), None));
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(error_counters(&D, false).pending, None, "SMART only for an ATA drive");
    }

    /// #648: the kernel's sense lines counted by device: the drive's PI
    /// checks, the card's (ILLEGAL REQUEST), medium errors; a record seen
    /// once counted once.
    #[test]
    fn sense_lines_counted() {
        let rec = |seq: u64, t: &str| format!("3,{seq},100,-;{t}\n");
        let lines = [
            rec(10, "sd 0:0:1:0: [sdb] tag#7 FAILED Result: hostbyte=DID_OK driverbyte=DRIVER_OK cmd_age=0s"),
            rec(11, "sd 0:0:1:0: [sdb] tag#7 Sense Key : Aborted Command [current] [descriptor] "),
            rec(12, "sd 0:0:1:0: [sdb] tag#7 Add. Sense: Logical block guard check failed"),
            rec(13, "sd 0:0:1:0: [sdb] tag#9 Sense Key : Illegal Request [current] "),
            rec(14, "sd 0:0:1:0: [sdb] tag#9 Add. Sense: Logical block reference tag check failed"),
            rec(15, "sd 0:0:2:0: [sdc] tag#1 Sense Key : Medium Error [current] "),
            rec(16, "sd 0:0:2:0: [sdc] tag#1 Add. Sense: ASC=0x10 ASCQ=0x2"),
            rec(17, "sd 0:0:2:0: [sdc] tag#2 Sense Key : Medium Error [current] "),
            rec(18, "sd 0:0:2:0: [sdc] tag#2 Add. Sense: Unrecovered read error"),
            rec(19, "mpt3sas_cm0: log_info(0x31110d00)"),
        ];
        let mut t = SenseTally::default();
        t.take(&lines);
        let b = t.by_device["0:0:1:0"];
        assert_eq!((b.pi_guard, b.eedp_ref_tag, b.pi_ref_tag, b.medium_error), (1, 1, 0, 0));
        let c = t.by_device["0:0:2:0"];
        assert_eq!((c.pi_app_tag, c.medium_error), (1, 2));
        assert_eq!(t.last_seq, Some(19));
        t.take(&lines);
        assert_eq!(t.by_device["0:0:1:0"].pi_guard, 1, "records already seen are not counted again");
        t.take(&[rec(20, "sd 0:0:1:0: Sense Key : Recovered Error [current] ")]);
        assert_eq!(t.by_device["0:0:1:0"].recovered_error, 1, "a line with no name or tag");
    }
}
