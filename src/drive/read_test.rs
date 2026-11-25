//! A drive's read speed, measured and compared with its siblings (#652).
//!
//! forge1: three HGST 10 TB drives at 12 Gbit read at 64–103 MB/s while the
//! HITACHI beside them (same class, 6 Gbit) read 238. Owner: "can we look at
//! the raid performance". [`run`] measures one drive, read only:
//!
//! - **sequential** reads of 1 MiB and 4 MiB from a quiet region (by default
//!   10 % into the drive, the same place on every drive so the zone is the
//!   same), each two ways: the kernel's block device with `O_DIRECT` (what
//!   the OS mirror and the set read through: sd's DIF path, the card
//!   checking and stripping the PI), and `READ(16)` with `RDPROTECT 0`
//!   through the sg node (the drive checks its own PI and sends none; the
//!   card does no EEDP). The two side by side say whether the DIF read path
//!   is the cost;
//! - **random** 4 KiB reads, one at a time (QD1), over the whole drive for a
//!   few seconds, through the block device;
//! - what the drive says of itself that bears on it: the mode pages 0x08
//!   caching, 0x0A control (QAM), 0x1A power condition and 0x1C
//!   informational exceptions, and log page 0x15's background scan (a
//!   medium scan running takes the heads).
//!
//! [`rank`] then sets each drive's `level` against the best of its class
//! (same rotation rate and size): `ok` at 80 % of it or more, `slow` below,
//! `very_slow` below 50 %.

use serde::Serialize;

use crate::drive::scsi::{self, ModePage, Transport};

/// What [`run`] reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Opts {
    /// Bytes per sequential pass.
    pub bytes: u64,
    /// Where the sequential passes start, percent into the drive.
    pub offset_pct: u8,
    /// Seconds of random reads.
    pub random_secs: u64,
    /// No pass runs longer than this (a very slow drive is said, not waited for).
    pub max_pass_secs: u64,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { bytes: 1 << 30, offset_pct: 10, random_secs: 10, max_pass_secs: 60 }
    }
}

/// Chunk sizes of the sequential passes.
pub const CHUNKS: [u64; 2] = [1 << 20, 4 << 20];
/// At or above this share of its class's best: `ok`.
pub const OK_SHARE: f64 = 0.8;
/// Below this share: `very_slow`.
pub const VERY_SLOW_SHARE: f64 = 0.5;

/// One sequential pass.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Pass {
    /// `block` (the kernel's device, O_DIRECT) or `sg` (READ(16), RDPROTECT 0).
    pub path: String,
    pub chunk_bytes: u64,
    pub bytes: u64,
    pub secs: f64,
    pub mb_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Random reads, one at a time.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Random {
    pub path: String,
    pub block_bytes: u64,
    pub ios: u64,
    pub secs: f64,
    pub iops: Option<f64>,
    /// Mean latency, ms.
    pub mean_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Log page 0x15, parameter 0 (SBC): the background scan.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct BackgroundScan {
    /// 0 none active, 1 a medium scan active, 2 a pre-scan active, 3..
    /// halted (why, by SBC's table), 8 waiting on the BMS timer.
    pub status: u8,
    pub said: String,
    pub power_on_minutes: u32,
    pub scans_performed: u16,
    /// Of the scan under way, percent.
    pub progress_pct: f32,
    pub medium_scans_performed: u16,
}

fn scan_said(s: u8) -> &'static str {
    match s {
        0 => "no background scan active",
        1 => "a background medium scan is active",
        2 => "a background pre-scan is active",
        3 => "halted: a fatal error",
        4 => "halted: a vendor-specific pattern of errors",
        5 => "halted: the PLIST is unusable",
        6 => "halted: vendor-specific",
        7 => "halted: the temperature",
        8 => "waiting for the BMS interval timer",
        _ => "a status SBC does not name",
    }
}

pub fn parse_background_scan(d: &[u8]) -> Option<BackgroundScan> {
    let ps = scsi::log_params(d);
    let v = ps.iter().find(|(c, _)| *c == 0).map(|(_, v)| v.as_slice())?;
    if v.len() < 12 {
        return None;
    }
    let be16 = |o: usize| u16::from_be_bytes([v[o], v[o + 1]]);
    Some(BackgroundScan {
        status: v[5],
        said: scan_said(v[5]).to_string(),
        power_on_minutes: u32::from_be_bytes([v[0], v[1], v[2], v[3]]),
        scans_performed: be16(6),
        progress_pct: be16(8) as f32 * 100.0 / 65536.0,
        medium_scans_performed: be16(10),
    })
}

/// A drive's read test.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ReadTest {
    pub sas_address: String,
    pub at: u64,
    /// The kernel's block device (`sdb`), when it has one.
    pub block: Option<String>,
    /// What the drive was asked through (`sg3`, `the card, handle …`).
    pub via: String,
    pub vendor: Option<String>,
    pub product: Option<String>,
    pub revision: Option<String>,
    pub capacity_bytes: Option<u64>,
    pub block_size: Option<u32>,
    pub pi_type: Option<u8>,
    /// VPD 0xB1: 1 non-rotating, else rpm.
    pub rotation_rate: Option<u16>,
    /// The card's link to it, when known (`12.0 Gbit`).
    pub link: Option<String>,
    pub offset_bytes: u64,
    pub passes: Vec<Pass>,
    pub random: Vec<Random>,
    /// Mode pages 0x03 0x08 0x0A 0x19 0x1A 0x1C, said.
    pub pages: Vec<ModePage>,
    pub background_scan: Option<BackgroundScan>,
    /// Same rotation rate and size, rounded to the TB: what it is compared with.
    pub class: String,
    /// Its best sequential rate (any path, any chunk), MB/s.
    pub best_mb_s: Option<f64>,
    /// The best of its class, and its share of it.
    pub class_best_mb_s: Option<f64>,
    pub share: Option<f64>,
    /// `ok`, `slow`, `very_slow`, or `unknown` (no rate, or alone in its class
    /// counts as `ok`).
    pub level: String,
    pub errors: Vec<String>,
}

/// Something a drive is read through.
pub trait Reader {
    /// Reads `buf.len()` bytes at `off`.
    fn read_at(&mut self, buf: &mut [u8], off: u64) -> Result<(), String>;
}

/// A buffer aligned for O_DIRECT.
pub struct Aligned {
    v: Vec<u8>,
    start: usize,
    len: usize,
}

impl Aligned {
    pub fn new(len: usize) -> Aligned {
        let v = vec![0u8; len + 4096];
        let start = v.as_ptr().align_offset(4096);
        Aligned { v, start, len }
    }
    pub fn buf(&mut self) -> &mut [u8] {
        &mut self.v[self.start..self.start + self.len]
    }
}

/// READ(16) with RDPROTECT 0 through a transport.
pub struct SgReader<'a> {
    pub t: &'a dyn Transport,
    pub block_size: u32,
}

impl Reader for SgReader<'_> {
    fn read_at(&mut self, buf: &mut [u8], off: u64) -> Result<(), String> {
        let bs = self.block_size.max(1) as u64;
        let cdb = scsi::cdb::read16(off / bs, (buf.len() as u64 / bs) as u32);
        match self.t.run(&cdb, scsi::Dir::In, buf, 60) {
            Ok(r) if r.good() => Ok(()),
            Ok(r) => Err(format!("READ(16): {}", r.said())),
            Err(e) => Err(format!("READ(16): {e}")),
        }
    }
}

/// The kernel's block device, O_DIRECT.
#[cfg(target_os = "linux")]
pub struct BlockReader {
    pub file: std::fs::File,
}

#[cfg(target_os = "linux")]
impl BlockReader {
    pub fn open(path: &std::path::Path) -> std::io::Result<BlockReader> {
        use std::os::unix::fs::OpenOptionsExt;
        Ok(BlockReader { file: std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECT).open(path)? })
    }
}

#[cfg(target_os = "linux")]
impl Reader for BlockReader {
    fn read_at(&mut self, buf: &mut [u8], off: u64) -> Result<(), String> {
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(buf, off).map_err(|e| format!("pread: {e}"))
    }
}

/// A sequential pass of `bytes` from `off` in `chunk`s, stopping at
/// `max_secs`.
pub fn seq_pass(r: &mut dyn Reader, path: &str, off: u64, bytes: u64, chunk: u64, max_secs: u64) -> Pass {
    let mut a = Aligned::new(chunk as usize);
    let start = std::time::Instant::now();
    let mut done = 0u64;
    let mut error = None;
    while done < bytes {
        if let Err(e) = r.read_at(a.buf(), off + done) {
            error = Some(e);
            break;
        }
        done += chunk;
        if start.elapsed().as_secs() >= max_secs {
            break;
        }
    }
    let secs = start.elapsed().as_secs_f64();
    Pass {
        path: path.into(),
        chunk_bytes: chunk,
        bytes: done,
        secs,
        mb_s: (done > 0 && secs > 0.0).then(|| done as f64 / secs / 1e6),
        error,
    }
}

/// A small, fixed-seed generator: the same offsets on every drive.
fn next(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

/// Random reads of `block` bytes over `[0, span)`, one at a time, for `secs`.
pub fn random_pass(r: &mut dyn Reader, path: &str, span: u64, block: u64, secs: u64) -> Random {
    let mut a = Aligned::new(block as usize);
    let slots = (span / block).max(1);
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let start = std::time::Instant::now();
    let mut ios = 0u64;
    let mut error = None;
    while start.elapsed().as_secs_f64() < secs as f64 {
        let off = (next(&mut seed) % slots) * block;
        if let Err(e) = r.read_at(a.buf(), off) {
            error = Some(e);
            break;
        }
        ios += 1;
    }
    let el = start.elapsed().as_secs_f64();
    Random {
        path: path.into(),
        block_bytes: block,
        ios,
        secs: el,
        iops: (ios > 0 && el > 0.0).then(|| ios as f64 / el),
        mean_ms: (ios > 0).then(|| el * 1000.0 / ios as f64),
        error,
    }
}

// ---- the sustained-write pass (#661) ----

/// Something a drive is written through, for [`write_cliff`].
pub trait Writer {
    fn write_at(&mut self, buf: &[u8], off: u64) -> Result<(), String>;
    /// What was written is on the medium (fdatasync).
    fn sync(&mut self) -> Result<(), String>;
}

/// The kernel's block device, O_DIRECT, for writing.
#[cfg(target_os = "linux")]
pub struct BlockWriter {
    pub file: std::fs::File,
}

#[cfg(target_os = "linux")]
impl BlockWriter {
    pub fn open(path: &std::path::Path) -> std::io::Result<BlockWriter> {
        use std::os::unix::fs::OpenOptionsExt;
        Ok(BlockWriter { file: std::fs::OpenOptions::new().write(true).custom_flags(libc::O_DIRECT).open(path)? })
    }
}

#[cfg(target_os = "linux")]
impl Writer for BlockWriter {
    fn write_at(&mut self, buf: &[u8], off: u64) -> Result<(), String> {
        use std::os::unix::fs::FileExt;
        self.file.write_all_at(buf, off).map_err(|e| format!("pwrite: {e}"))
    }
    fn sync(&mut self) -> Result<(), String> {
        self.file.sync_data().map_err(|e| format!("fdatasync: {e}"))
    }
}

/// One step of a sustained write: `bytes` from `at` (bytes into the pass),
/// synced, at `mb_s`.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct WriteStep {
    pub at: u64,
    pub bytes: u64,
    pub secs: f64,
    pub mb_s: f64,
}

/// A sustained sequential write (#661): what a DRAM-less SSD writes once its
/// SLC cache is full, and where that starts.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct WriteCliff {
    pub device: String,
    pub offset_bytes: u64,
    pub chunk_bytes: u64,
    pub steps: Vec<WriteStep>,
    /// The best step's rate and the slowest step's, MB/s.
    pub peak_mb_s: Option<f64>,
    pub floor_mb_s: Option<f64>,
    /// Bytes into the pass where a step first fell below half the peak of
    /// the steps before it: the cliff. `None` when none did.
    pub cliff_at: Option<u64>,
    pub error: Option<String>,
}

/// Where the rate first fell below half the best step before it.
pub fn cliff_of(steps: &[WriteStep]) -> Option<u64> {
    let mut best = 0f64;
    for s in steps {
        if best > 0.0 && s.mb_s < best / 2.0 {
            return Some(s.at);
        }
        best = best.max(s.mb_s);
    }
    None
}

/// `bytes` written sequentially from `off` in `chunk`s, a step every
/// `step` bytes, each step synced and timed with its sync, so a drive's
/// cache cannot hide its medium. **Overwrites that range.** Stops at the
/// first error, said.
pub fn write_cliff(w: &mut dyn Writer, device: &str, off: u64, bytes: u64, chunk: u64, step: u64) -> WriteCliff {
    let mut a = Aligned::new(chunk as usize);
    // Not zeros: a drive that compresses or dedups zeros would not write.
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    for c in a.buf().chunks_mut(8) {
        let v = next(&mut x).to_le_bytes();
        c.copy_from_slice(&v[..c.len()]);
    }
    let step = step.max(chunk) / chunk * chunk;
    let mut out = WriteCliff { device: device.into(), offset_bytes: off, chunk_bytes: chunk, ..Default::default() };
    let mut done = 0u64;
    'steps: while done < bytes {
        let start = std::time::Instant::now();
        let from = done;
        let end = (done + step).min(bytes);
        while done < end {
            if let Err(e) = w.write_at(a.buf(), off + done) {
                out.error = Some(e);
                break 'steps;
            }
            done += chunk;
        }
        if let Err(e) = w.sync() {
            out.error = Some(e);
            break;
        }
        let secs = start.elapsed().as_secs_f64();
        let n = done - from;
        out.steps.push(WriteStep { at: from, bytes: n, secs, mb_s: if secs > 0.0 { n as f64 / secs / 1e6 } else { 0.0 } });
    }
    out.peak_mb_s = out.steps.iter().map(|s| s.mb_s).reduce(f64::max);
    out.floor_mb_s = out.steps.iter().map(|s| s.mb_s).reduce(f64::min);
    out.cliff_at = cliff_of(&out.steps);
    out
}

/// What a sustained write says, a line each, starting `write-cliff`.
pub fn cliff_lines(c: &WriteCliff) -> Vec<String> {
    let who = format!("write-cliff {}", c.device);
    let gib = |b: u64| format!("{:.2} GiB", b as f64 / (1u64 << 30) as f64);
    let total: u64 = c.steps.iter().map(|s| s.bytes).sum();
    let mut v = Vec::new();
    let rates: Vec<String> = c.steps.iter().map(|s| format!("{:.0}", s.mb_s)).collect();
    v.push(format!(
        "{who}: {} written from {} in {} MiB steps, each synced: {} MB/s",
        gib(total),
        gib(c.offset_bytes),
        c.steps.first().map(|s| s.bytes >> 20).unwrap_or(0),
        rates.join(", ")
    ));
    match (c.peak_mb_s, c.floor_mb_s, c.cliff_at) {
        (Some(p), Some(f), Some(at)) => v.push(format!("{who}: a cliff at {}: from {p:.0} MB/s down to {f:.0}", gib(at))),
        // No cliff in the range written says the cache is bigger than it,
        // not that there is none: a fresh drive's SLC cache can be tens of GiB.
        (Some(p), Some(f), None) => v.push(format!("{who}: no cliff in {}: {f:.0}-{p:.0} MB/s throughout", gib(total))),
        _ => {}
    }
    if let Some(e) = &c.error {
        v.push(format!("{who}: stopped: {e}"));
    }
    v
}

/// VPD 0xB1's rotation rate.
fn rotation(t: &dyn Transport) -> Option<u16> {
    let mut b = vec![0u8; 64];
    match t.run(&scsi::cdb::inquiry(Some(0xB1), 64), scsi::Dir::In, &mut b, 30) {
        Ok(r) if r.good() && b.len() >= 6 => Some(u16::from_be_bytes([b[4], b[5]])).filter(|x| *x != 0),
        _ => None,
    }
}

/// The class a drive is compared within: its rotation rate and its size.
pub fn class_of(rotation: Option<u16>, capacity: Option<u64>) -> String {
    let r = match rotation {
        Some(1) => "ssd".to_string(),
        Some(n) => format!("{n}rpm"),
        None => "?rpm".into(),
    };
    let tb = capacity.map(|c| format!("{}TB", (c as f64 / 1e12).round() as u64)).unwrap_or_else(|| "?TB".into());
    format!("{r} {tb}")
}

/// One drive's test, through `t` and (when `block` is given) the kernel's
/// device. Read only.
pub fn run(t: &dyn Transport, block: Option<&std::path::Path>, o: &Opts) -> ReadTest {
    let id = scsi::ident(t);
    let cap = id.capacity.clone();
    let mut rt = ReadTest {
        at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        block: block.and_then(|b| b.file_name()).map(|n| n.to_string_lossy().into_owned()),
        via: t.name(),
        vendor: id.vendor,
        product: id.product,
        revision: id.revision,
        capacity_bytes: cap.as_ref().map(|c| c.blocks * c.block_size as u64),
        block_size: cap.as_ref().map(|c| c.block_size),
        pi_type: cap.as_ref().map(|c| if c.prot_en { c.pi_type } else { 0 }),
        rotation_rate: rotation(t),
        level: "unknown".into(),
        ..Default::default()
    };
    rt.class = class_of(rt.rotation_rate, rt.capacity_bytes);
    rt.pages = scsi::mode_pages(t);
    let mut e = Vec::new();
    let mut lp = vec![0u8; 512];
    match t.run(&scsi::cdb::log_sense(0x15, 0, 512), scsi::Dir::In, &mut lp, 30) {
        Ok(r) if r.good() => rt.background_scan = parse_background_scan(&lp[..r.len.min(512)]),
        Ok(r) => e.push(format!("LOG SENSE 0x15: {}", r.said())),
        Err(x) => e.push(format!("LOG SENSE 0x15: {x}")),
    }
    let Some(capb) = rt.capacity_bytes.filter(|c| *c > 0) else {
        e.push("no capacity (READ CAPACITY 16 not answered): nothing read".into());
        rt.errors = e;
        return rt;
    };
    let bs = rt.block_size.unwrap_or(512).max(512) as u64;
    let mib = 1u64 << 20;
    let room = capb.saturating_sub(o.bytes + 8 * mib);
    rt.offset_bytes = ((capb / 100 * o.offset_pct.min(90) as u64).min(room) / mib) * mib;
    #[cfg(target_os = "linux")]
    let mut blk = block.map(BlockReader::open);
    for chunk in CHUNKS {
        #[cfg(target_os = "linux")]
        match blk.as_mut() {
            Some(Ok(b)) => rt.passes.push(seq_pass(b, "block", rt.offset_bytes, o.bytes, chunk, o.max_pass_secs)),
            Some(Err(x)) => e.push(format!("{}: O_DIRECT open: {x}", rt.block.as_deref().unwrap_or("?"))),
            None => {}
        }
        let mut sg = SgReader { t, block_size: bs as u32 };
        rt.passes.push(seq_pass(&mut sg, "sg", rt.offset_bytes, o.bytes, chunk, o.max_pass_secs));
    }
    #[cfg(target_os = "linux")]
    if let Some(Ok(b)) = blk.as_mut() {
        rt.random.push(random_pass(b, "block", capb.saturating_sub(8 * mib), 4096u64.max(bs), o.random_secs));
    }
    if rt.random.is_empty() {
        let mut sg = SgReader { t, block_size: bs as u32 };
        rt.random.push(random_pass(&mut sg, "sg", capb.saturating_sub(8 * mib), 4096u64.max(bs), o.random_secs));
    }
    rt.best_mb_s = rt.passes.iter().filter_map(|p| p.mb_s).reduce(f64::max);
    rt.errors = e;
    rt
}

/// Each drive's level against the best of its class (#652).
pub fn rank(all: &mut [ReadTest]) {
    let best: std::collections::HashMap<String, f64> = all.iter().fold(Default::default(), |mut m, r| {
        if let Some(b) = r.best_mb_s {
            let e = m.entry(r.class.clone()).or_insert(b);
            *e = e.max(b);
        }
        m
    });
    for r in all.iter_mut() {
        let (Some(b), Some(top)) = (r.best_mb_s, best.get(&r.class).copied()) else {
            r.level = "unknown".into();
            continue;
        };
        let share = if top > 0.0 { b / top } else { 1.0 };
        r.class_best_mb_s = Some(top);
        r.share = Some(share);
        r.level = if share >= OK_SHARE {
            "ok"
        } else if share >= VERY_SLOW_SHARE {
            "slow"
        } else {
            "very_slow"
        }
        .into();
    }
}

fn rate(p: Option<&Pass>) -> String {
    match p {
        Some(Pass { mb_s: Some(m), .. }) => format!("{m:.0} MB/s"),
        Some(Pass { error: Some(e), .. }) => format!("failed ({e})"),
        Some(_) => "no rate".into(),
        None => "not run".into(),
    }
}

/// What a test says, a line a step, every line starting `read-test`.
pub fn lines(r: &ReadTest) -> Vec<String> {
    let who = format!(
        "read-test {} ({}{} {} {}, {}{})",
        r.sas_address,
        r.block.as_deref().map(|b| format!("{b}, ")).unwrap_or_default(),
        r.vendor.as_deref().unwrap_or("?"),
        r.product.as_deref().unwrap_or("?"),
        r.revision.as_deref().unwrap_or("?"),
        r.class,
        r.link.as_deref().map(|l| format!(", link {l}")).unwrap_or_default()
    );
    let p = |path: &str, chunk: u64| r.passes.iter().find(|p| p.path == path && p.chunk_bytes == chunk);
    let mut v = Vec::new();
    for path in ["block", "sg"] {
        if r.passes.iter().any(|x| x.path == path) {
            let how = if path == "block" { "the block device, O_DIRECT (the kernel's DIF path)" } else { "sg READ(16) RDPROTECT 0 (the drive checks its own PI)" };
            v.push(format!("{who}: sequential at {} GiB: 1 MiB {}, 4 MiB {} through {how}", r.offset_bytes >> 30, rate(p(path, 1 << 20)), rate(p(path, 4 << 20))));
        }
    }
    for x in &r.random {
        match (x.iops, &x.error) {
            (Some(i), _) => v.push(format!("{who}: random 4 KiB QD1 through {}: {i:.0} IOPS, {:.1} ms each", x.path, x.mean_ms.unwrap_or(0.0))),
            (None, Some(e)) => v.push(format!("{who}: random 4 KiB through {}: failed ({e})", x.path)),
            _ => {}
        }
    }
    let said: Vec<String> = r.pages.iter().filter(|m| ["0x08", "0x0a", "0x1a", "0x1c"].contains(&m.page.as_str())).filter_map(|m| m.said.clone()).collect();
    if !said.is_empty() {
        v.push(format!("{who}: {}", said.join("; ")));
    }
    if let Some(b) = &r.background_scan {
        v.push(format!("{who}: background scan: {} ({} scans, {:.0} % of this one)", b.said, b.scans_performed, b.progress_pct));
    }
    match (r.share, r.class_best_mb_s) {
        (Some(s), Some(top)) => v.push(format!("{who}: {}: {:.0} MB/s is {:.0} % of the best of its class ({top:.0} MB/s)", r.level, r.best_mb_s.unwrap_or(0.0), s * 100.0)),
        _ => v.push(format!("{who}: {}", r.level)),
    }
    for e in &r.errors {
        v.push(format!("{who}: {e}"));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A drive in memory: reads cost nothing, a read past the end fails.
    struct Mem(u64, u64);
    impl Reader for Mem {
        fn read_at(&mut self, buf: &mut [u8], off: u64) -> Result<(), String> {
            if off + buf.len() as u64 > self.0 {
                return Err("past the end".into());
            }
            self.1 += 1;
            buf.fill(0xAB);
            Ok(())
        }
    }

    struct Cache {
        /// Bytes taken at full speed before each later chunk costs `slow`.
        fast: u64,
        taken: u64,
        written: Vec<(u64, usize)>,
        slow: std::time::Duration,
        syncs: u32,
    }
    impl Writer for Cache {
        fn write_at(&mut self, buf: &[u8], off: u64) -> Result<(), String> {
            if self.taken >= self.fast {
                std::thread::sleep(self.slow);
            }
            self.taken += buf.len() as u64;
            self.written.push((off, buf.len()));
            Ok(())
        }
        fn sync(&mut self) -> Result<(), String> {
            self.syncs += 1;
            Ok(())
        }
    }

    /// #661: a DRAM-less SSD's cliff is found where its cache runs out; the
    /// range written is exactly what was asked, each step synced.
    #[test]
    fn a_sustained_write_finds_the_cliff() {
        let mib = 1u64 << 20;
        let mut w = Cache { fast: 8 * mib, taken: 0, written: Vec::new(), slow: std::time::Duration::from_millis(20), syncs: 0 };
        let c = write_cliff(&mut w, "sdb", 100 * mib, 16 * mib, mib, 4 * mib);
        assert_eq!(c.steps.len(), 4);
        assert_eq!(w.syncs, 4);
        assert_eq!(w.written.first(), Some(&(100 * mib, mib as usize)));
        assert_eq!(w.written.last(), Some(&(115 * mib, mib as usize)));
        assert_eq!(c.cliff_at, Some(8 * mib), "{:?}", c.steps);
        let l = cliff_lines(&c);
        assert!(l[0].starts_with("write-cliff sdb: 0.02 GiB written from 0.10 GiB in 4 MiB steps, each synced: "), "{l:?}");
        assert!(l[1].contains("a cliff at 0.01 GiB"), "{l:?}");
    }

    #[test]
    fn no_cliff_when_the_rate_holds() {
        let s = |at: u64, mb_s: f64| WriteStep { at, bytes: 1, secs: 1.0, mb_s };
        assert_eq!(cliff_of(&[s(0, 500.0), s(1, 480.0), s(2, 300.0)]), None);
        assert_eq!(cliff_of(&[s(0, 500.0), s(1, 510.0), s(2, 240.0)]), Some(2));
        assert_eq!(cliff_of(&[]), None);
        let c = WriteCliff { device: "sdb".into(), steps: vec![s(0, 500.0), s(1, 480.0)], peak_mb_s: Some(500.0), floor_mb_s: Some(480.0), ..Default::default() };
        let l = cliff_lines(&c);
        assert!(l[1].starts_with("write-cliff sdb: no cliff in 0.00 GiB: 480-500 MB/s"), "{l:?}");
    }

    #[test]
    fn passes_read_what_they_say() {
        let mut m = Mem(64 << 20, 0);
        let p = seq_pass(&mut m, "block", 0, 16 << 20, 1 << 20, 60);
        assert_eq!((p.bytes, m.1), (16 << 20, 16));
        assert!(p.mb_s.is_some() && p.error.is_none());
        let mut m = Mem(4 << 20, 0);
        let p = seq_pass(&mut m, "sg", 0, 16 << 20, 1 << 20, 60);
        assert_eq!(p.bytes, 4 << 20, "stops at the first failed read");
        assert_eq!(p.error.as_deref(), Some("past the end"));
        let mut m = Mem(64 << 20, 0);
        let r = random_pass(&mut m, "block", 64 << 20, 4096, 0);
        assert_eq!(r.ios, 0, "no time, no reads");
        let mut m = Mem(64 << 20, 0);
        let r = random_pass(&mut m, "block", 64 << 20, 4096, 1);
        assert!(r.ios > 0 && r.error.is_none() && r.iops.is_some(), "every offset inside the span: {r:?}");
        let mut a = Aligned::new(8192);
        assert_eq!(a.buf().as_ptr() as usize % 4096, 0);
        assert_eq!(a.buf().len(), 8192);
    }

    /// forge1: the three HGSTs against the HITACHI, all 7200 rpm 10 TB.
    #[test]
    fn levels_against_the_class() {
        let t = |sas: &str, mb: f64, class: &str| ReadTest { sas_address: sas.into(), best_mb_s: Some(mb), class: class.into(), ..Default::default() };
        let mut all = vec![t("sda", 103.0, "7200rpm 10TB"), t("sdb", 238.0, "7200rpm 10TB"), t("sdc", 200.0, "7200rpm 10TB"), t("ssd", 500.0, "ssd 1TB"), ReadTest { class: "7200rpm 10TB".into(), ..Default::default() }];
        rank(&mut all);
        let lv: Vec<&str> = all.iter().map(|r| r.level.as_str()).collect();
        assert_eq!(lv, vec!["very_slow", "ok", "ok", "ok", "unknown"]);
        assert!((all[0].share.unwrap() - 103.0 / 238.0).abs() < 1e-9);
        all[0].best_mb_s = Some(150.0);
        rank(&mut all);
        assert_eq!(all[0].level, "slow");
        assert_eq!(class_of(Some(7200), Some(10_000_831_348_736)), "7200rpm 10TB");
        assert_eq!(class_of(Some(1), None), "ssd ?TB");
    }

    #[test]
    fn background_scan_read() {
        // Parameter 0: 1000 minutes, status 1 (medium scan), 7 scans, half
        // way, 3 medium scans.
        let v = [0, 0, 3, 0xE8, 0, 1, 0, 7, 0x80, 0, 0, 3];
        let mut d = vec![0x15, 0, 0, 0, 0, 0, 0x03, v.len() as u8];
        d.extend_from_slice(&v);
        let n = (d.len() - 4) as u16;
        d[2..4].copy_from_slice(&n.to_be_bytes());
        let b = parse_background_scan(&d).unwrap();
        assert_eq!((b.status, b.power_on_minutes, b.scans_performed, b.medium_scans_performed), (1, 1000, 7, 3));
        assert_eq!(b.progress_pct, 50.0);
        assert_eq!(b.said, "a background medium scan is active");
    }

    #[test]
    fn lines_say_it() {
        let r = ReadTest {
            sas_address: "0x5000cca26801b649".into(),
            block: Some("sda".into()),
            vendor: Some("HGST".into()),
            product: Some("H7210A521SUN010T".into()),
            revision: Some("A3B0".into()),
            class: "7200rpm 10TB".into(),
            link: Some("12.0 Gbit".into()),
            offset_bytes: 1000 << 30,
            passes: vec![
                Pass { path: "block".into(), chunk_bytes: 1 << 20, mb_s: Some(64.0), ..Default::default() },
                Pass { path: "block".into(), chunk_bytes: 4 << 20, mb_s: Some(103.0), ..Default::default() },
                Pass { path: "sg".into(), chunk_bytes: 1 << 20, mb_s: Some(200.0), ..Default::default() },
            ],
            best_mb_s: Some(200.0),
            share: Some(0.84),
            class_best_mb_s: Some(238.0),
            level: "ok".into(),
            ..Default::default()
        };
        let l = lines(&r);
        assert_eq!(l[0], "read-test 0x5000cca26801b649 (sda, HGST H7210A521SUN010T A3B0, 7200rpm 10TB, link 12.0 Gbit): sequential at 1000 GiB: 1 MiB 64 MB/s, 4 MiB 103 MB/s through the block device, O_DIRECT (the kernel's DIF path)");
        assert!(l[1].ends_with("1 MiB 200 MB/s, 4 MiB not run through sg READ(16) RDPROTECT 0 (the drive checks its own PI)"), "{}", l[1]);
        assert!(l.last().unwrap().ends_with("ok: 200 MB/s is 84 % of the best of its class (238 MB/s)"), "{l:?}");
    }
}
