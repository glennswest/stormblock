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
    d.errors = e;
    d
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

/// A format's minute line through the card (#595), the shape the sg path
/// prints: `0x…: formatting (full) 12.3% through the card, about 7h 10m
/// left, 58m so far`. The ETA is the pace so far; none before 0.1 %.
pub fn card_format_line(addr: u64, pct: f32, elapsed_secs: u64) -> String {
    let left = if pct >= 0.1 && pct < 100.0 {
        let rest = elapsed_secs as f64 * f64::from(100.0 - pct) / f64::from(pct);
        format!("about {} left", span(rest as u64))
    } else if pct >= 100.0 {
        "about 0m left".to_string()
    } else {
        "no ETA yet".to_string()
    };
    format!("0x{addr:016x}: formatting (full) {pct:.1}% through the card, {left}, {} so far", span(elapsed_secs))
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
            "0x5000cca268077301: formatting (full) 10.0% through the card, about 9h 0m left, 1h 0m so far"
        );
        assert_eq!(card_format_line(1, 0.0, 30), "0x0000000000000001: formatting (full) 0.0% through the card, no ETA yet, 0m so far");
        assert_eq!(card_format_line(1, 100.0, 120), "0x0000000000000001: formatting (full) 100.0% through the card, about 0m left, 2m so far");
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
