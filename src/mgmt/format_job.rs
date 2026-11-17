//! The runtime format (#621; owner, 2026-11-15: "the new 'format' needs to
//! be implemented in running system too. As we will need it for netapp
//! shelves as well").
//!
//! The install's card-path format (#595, #611, #614, #620) as a job on the
//! running node: one drive, or every drive of an enclosure, through its sg
//! node or the card. Each drive:
//! 1. skipped when it is already at the format asked (#614);
//! 2. MODE SELECT + FORMAT UNIT (IMMED), followed every 30 s: percent, ETA,
//!    elapsed, in the card's rows (`format_job`), on the console a line a
//!    minute;
//! 3. when it ends, the kernel is asked to look again (a rescan by SAS
//!    address) until Linux has the disk at its new size: hotplug (#593)
//!    then offers it as a replacement, spare or system leg, as it does any
//!    blank drive that arrives;
//! 4. the write test (#627: LBA 0 and the last LBA, with and without PI,
//!    read back); a drive whose firmware refuses writes at the format asked
//!    is formatted again at the next fallback format (4096 without PI, then
//!    512, as asked), said;
//! 5. `ready` at its format, or `failed` with why.
//!
//! A shelf whose drives carry NetApp firmware (the master, #621: forge1's
//! X378 took 4096 + PI and then left the link at its first write) has one
//! drive formatted and tested first, the **canary**; the rest start only
//! when it is ready, and at the format it ended at. Nothing formats by
//! itself: every job is asked for, confirmed by the drive's SAS address or
//! the enclosure's id. A drive this engine has a slab open on is refused.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;
use serde_json::Value;

use crate::drive::scsi::Ident;

/// A format: block size and PI type (0 none, 1 type 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Want {
    pub block_size: u32,
    pub pi_type: u8,
}

impl Want {
    pub fn said(&self) -> String {
        if self.pi_type == 1 {
            format!("{} + PI type 1", self.block_size)
        } else {
            format!("{} without PI", self.block_size)
        }
    }
    /// `4096+pi1`, `4096`, `512`.
    pub fn parse(s: &str) -> Result<Want, String> {
        let t = s.trim().to_ascii_lowercase().replace(' ', "");
        let (bs, pi) = match t.split_once('+') {
            Some((b, p)) if p == "pi1" || p == "pi" || p == "type1" => (b.to_string(), 1),
            Some((_, p)) => return Err(format!("a format '{s}': protection '{p}' is none or pi1")),
            None => (t.clone(), 0),
        };
        match bs.parse::<u32>() {
            Ok(b @ (512 | 4096)) => Ok(Want { block_size: b, pi_type: pi }),
            _ => Err(format!("a format '{s}': 512, 4096 or 4096+pi1")),
        }
    }
}

/// What a job is asked to do.
#[derive(Debug, Clone, Serialize)]
pub struct Opts {
    pub want: Want,
    /// Formats to fall back to, in order, when the drive refuses writes at
    /// the one before (the write test).
    pub fallback: Vec<Want>,
    /// The write test after each format (default on).
    pub test: bool,
    /// One drive first (a shelf; on by default when any drive is NetApp's).
    pub canary: bool,
}

/// One drive's job, as the rows and the API show it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Job {
    pub sas_address: String,
    pub enclosure: Option<String>,
    pub slot: Option<u64>,
    /// `queued`, `waiting` (behind the canary), `formatting`, `rescanning`,
    /// `testing`, `ready`, `failed`.
    pub state: String,
    pub canary: bool,
    /// The format being laid now, and the one asked first.
    pub format: String,
    pub asked: String,
    pub pct: Option<f32>,
    pub started: u64,
    pub elapsed_secs: u64,
    pub eta_secs: Option<u64>,
    pub ended: Option<u64>,
    /// Its own lines, oldest first (the console's).
    pub said: Vec<String>,
    pub revision: Option<String>,
}

/// The drive operations a job uses: the card's, or a test's.
pub trait Drives: Sync {
    fn ident(&self, sas: u64) -> Result<Ident, String>;
    fn format(&self, sas: u64, w: Want) -> Result<String, String>;
    fn progress(&self, sas: u64) -> Result<Option<f32>, String>;
    fn rescan(&self, sas: u64);
    /// Linux has the drive as a disk with a size.
    fn linux_ready(&self, sas: u64) -> bool;
    /// The write test: (passed, its lines).
    fn test(&self, sas: u64) -> Result<(bool, Vec<String>), String>;
    fn sleep(&self, secs: u64);
    fn now(&self) -> u64;
}

/// The storage policy's format (STORAGE-LAYOUT.md, #595): 4096 + PI type 1.
pub const POLICY: Want = Want { block_size: 4096, pi_type: 1 };

/// Seconds between progress reads; a line a minute.
pub const POLL_SECS: u64 = 30;
/// How long Linux is waited for after a format.
pub const LINUX_WAIT_SECS: u64 = 600;
/// Unanswered progress reads before the job says the format is lost.
pub const MISSES: u32 = 20;

static JOBS: Mutex<Option<HashMap<String, Job>>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut HashMap<String, Job>) -> R) -> R {
    let mut g = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(HashMap::new))
}

fn key(sas: u64) -> String {
    format!("0x{sas:016x}")
}

/// A drive's job, if it has one.
pub fn get(sas: &str) -> Option<Value> {
    with(|m| m.get(sas).and_then(|j| serde_json::to_value(j).ok()))
}

/// Every job, newest first.
pub fn all() -> Vec<Job> {
    let mut v: Vec<Job> = with(|m| m.values().cloned().collect());
    v.sort_by(|a, b| b.started.cmp(&a.started).then(a.sas_address.cmp(&b.sas_address)));
    v
}

/// Whether a drive has a job that has not ended.
pub fn running(sas: u64) -> bool {
    with(|m| m.get(&key(sas)).is_some_and(|j| j.ended.is_none()))
}

/// For health: how many run, and each one's state and percent, in brief.
pub fn summary() -> Vec<Value> {
    all()
        .into_iter()
        .filter(|j| j.ended.is_none() || j.state == "failed")
        .map(|j| serde_json::json!({"sas_address": j.sas_address, "state": j.state, "pct": j.pct, "eta_secs": j.eta_secs, "format": j.format}))
        .collect()
}

fn update(sas: u64, f: impl FnOnce(&mut Job)) -> Job {
    with(|m| {
        let j = m.get_mut(&key(sas)).expect("a job is registered before it runs");
        f(j);
        j.clone()
    })
}

fn say(sas: u64, line: String) {
    tracing::warn!("storage: drive {}: {line} (#621)", key(sas));
    update(sas, |j| j.said.push(line));
}

/// The ETA from the pace so far; none before 0.1 %.
pub fn eta(pct: f32, elapsed: u64) -> Option<u64> {
    (0.1..100.0).contains(&pct).then(|| (elapsed as f64 * f64::from(100.0 - pct) / f64::from(pct)) as u64)
}

/// One drive, to its end: `Some(the format it is ready at)`, or `None`.
pub fn run_one(d: &dyn Drives, sas: u64, opts: &Opts) -> Option<Want> {
    let started = d.now();
    update(sas, |j| {
        j.started = started;
        j.state = "starting".into();
    });
    let id = d.ident(sas).unwrap_or_default();
    update(sas, |j| j.revision = id.revision.clone());
    let chain: Vec<Want> = std::iter::once(opts.want).chain(opts.fallback.iter().copied()).collect();
    for (i, w) in chain.iter().enumerate() {
        update(sas, |j| {
            j.format = w.said();
            j.pct = None;
            j.eta_secs = None;
        });
        let now_id = if i == 0 { id.clone() } else { d.ident(sas).unwrap_or_default() };
        if now_id.at(w.block_size, w.pi_type) {
            say(sas, format!("already at {}: not formatted again (#614)", w.said()));
        } else {
            update(sas, |j| j.state = "formatting".into());
            match d.format(sas, *w) {
                Ok(s) => say(sas, s),
                Err(e) => {
                    say(sas, format!("the format to {} was refused: {e}", w.said()));
                    continue;
                }
            }
            let t0 = d.now();
            let mut misses = 0u32;
            let mut last_line = 0u64;
            let ended = loop {
                d.sleep(POLL_SECS);
                match d.progress(sas) {
                    Ok(None) => break true,
                    Ok(Some(p)) => {
                        misses = 0;
                        let el = d.now().saturating_sub(t0);
                        update(sas, |j| {
                            j.pct = Some(p);
                            j.elapsed_secs = d.now().saturating_sub(started);
                            j.eta_secs = eta(p, el);
                        });
                        if el.saturating_sub(last_line) >= 60 || last_line == 0 {
                            last_line = el.max(1);
                            tracing::info!("{} (#621)", crate::drive::scsi::card_format_line(sas, p, el));
                        }
                    }
                    Err(e) => {
                        misses += 1;
                        if misses >= MISSES {
                            say(sas, format!("the format's progress unread {} times in a row: {e}", misses));
                            break false;
                        }
                    }
                }
            };
            if !ended {
                break;
            }
            say(sas, format!("formatted to {} in {}", w.said(), crate::drive::scsi::span(d.now().saturating_sub(t0))));
        }
        // Linux finds it at its new format, by SAS address (#611).
        update(sas, |j| {
            j.state = "rescanning".into();
            j.pct = Some(100.0);
            j.eta_secs = Some(0);
        });
        let t1 = d.now();
        d.rescan(sas);
        while !d.linux_ready(sas) {
            if d.now().saturating_sub(t1) >= LINUX_WAIT_SECS {
                say(sas, format!("Linux has no disk for it after {}: tested through the card", crate::drive::scsi::span(LINUX_WAIT_SECS)));
                break;
            }
            d.sleep(10);
            if d.now().saturating_sub(t1) % 60 < 10 {
                d.rescan(sas);
            }
        }
        if !opts.test {
            return finish(d, sas, Some(*w));
        }
        update(sas, |j| j.state = "testing".into());
        match d.test(sas) {
            Ok((true, _)) => {
                say(sas, format!("the write test passed at {}", w.said()));
                return finish(d, sas, Some(*w));
            }
            Ok((false, lines)) => {
                let why = lines.iter().find(|l| !l.contains("good")).cloned().unwrap_or_else(|| "a step was not good".into());
                match chain.get(i + 1) {
                    Some(n) => say(sas, format!("its firmware refuses writes at {} ({why}): formatting it to {} instead", w.said(), n.said())),
                    None => say(sas, format!("its firmware refuses writes at {} ({why}), and no fallback format is left", w.said())),
                }
            }
            Err(e) => {
                say(sas, format!("the write test did not run: {e}"));
                break;
            }
        }
    }
    finish(d, sas, None)
}

fn finish(d: &dyn Drives, sas: u64, at: Option<Want>) -> Option<Want> {
    let now = d.now();
    let rev = d.ident(sas).ok().and_then(|i| i.revision);
    update(sas, |j| {
        j.state = if at.is_some() { "ready" } else { "failed" }.into();
        j.ended = Some(now);
        j.elapsed_secs = now.saturating_sub(j.started);
        j.eta_secs = None;
        if rev.is_some() {
            j.revision = rev;
        }
        if let Some(w) = at {
            j.format = w.said();
        }
    });
    super::hba::poke();
    at
}

/// Register the jobs (refusing a drive with one running), then run them:
/// the canary first when asked, the rest at once, each on its own thread.
/// Answers the drives registered.
pub fn start(d: &'static dyn Drives, drives: Vec<(u64, Option<String>, Option<u64>)>, opts: Opts) -> Result<Vec<String>, String> {
    if drives.is_empty() {
        return Err("no drive to format".into());
    }
    let now = d.now();
    with(|m| {
        if let Some((a, _, _)) = drives.iter().find(|(a, _, _)| m.get(&key(*a)).is_some_and(|j| j.ended.is_none())) {
            return Err(format!("{} is being formatted already", key(*a)));
        }
        for (i, (a, enc, slot)) in drives.iter().enumerate() {
            let canary = opts.canary && i == 0 && drives.len() > 1;
            m.insert(
                key(*a),
                Job {
                    sas_address: key(*a),
                    enclosure: enc.clone(),
                    slot: *slot,
                    state: if opts.canary && i > 0 { "waiting" } else { "queued" }.into(),
                    canary,
                    format: opts.want.said(),
                    asked: opts.want.said(),
                    pct: None,
                    started: now,
                    elapsed_secs: 0,
                    eta_secs: None,
                    ended: None,
                    said: if opts.canary && i > 0 { vec!["waits for the first drive (the canary) to format and pass its write test".into()] } else { vec![] },
                    revision: None,
                },
            );
        }
        Ok(())
    })?;
    let names: Vec<String> = drives.iter().map(|(a, _, _)| key(*a)).collect();
    std::thread::Builder::new()
        .name("format-job".into())
        .spawn(move || run_all(d, drives, opts))
        .map_err(|e| e.to_string())?;
    Ok(names)
}

/// The whole run (blocking).
pub fn run_all(d: &dyn Drives, drives: Vec<(u64, Option<String>, Option<u64>)>, opts: Opts) {
    let mut rest: &[(u64, Option<String>, Option<u64>)] = &drives;
    let mut opts = opts;
    if opts.canary && drives.len() > 1 {
        let (first, others) = drives.split_first().expect("not empty");
        rest = others;
        match run_one(d, first.0, &opts) {
            Some(w) => {
                // The rest at the format the canary ended at.
                if w != opts.want {
                    for (a, _, _) in rest {
                        say(*a, format!("the canary {} ended at {}: formatting to that", key(first.0), w.said()));
                    }
                }
                let pos = std::iter::once(opts.want).chain(opts.fallback.iter().copied()).position(|x| x == w).unwrap_or(0);
                let chain: Vec<Want> = std::iter::once(opts.want).chain(opts.fallback.iter().copied()).skip(pos).collect();
                opts.want = chain[0];
                opts.fallback = chain[1..].to_vec();
            }
            None => {
                let now = d.now();
                for (a, _, _) in rest {
                    update(*a, |j| {
                        j.state = "failed".into();
                        j.ended = Some(now);
                        j.said.push(format!("not formatted: the canary {} failed", key(first.0)));
                    });
                }
                tracing::warn!("storage: the canary {} failed: the other {} drive(s) are not formatted (#621)", key(first.0), rest.len());
                return;
            }
        }
    }
    std::thread::scope(|s| {
        for (a, _, _) in rest {
            let o = &opts;
            s.spawn(move || run_one(d, *a, o));
        }
    });
}

/// The card's drives, for a job on the node.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub struct Card;

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
impl Drives for Card {
    fn ident(&self, sas: u64) -> Result<Ident, String> {
        crate::drive::mpt::with_drive(std::path::Path::new("/dev"), std::path::Path::new("/sys"), sas, crate::drive::scsi::ident)
    }
    fn format(&self, sas: u64, w: Want) -> Result<String, String> {
        crate::drive::mpt::with_drive(std::path::Path::new("/dev"), std::path::Path::new("/sys"), sas, |t| {
            crate::drive::scsi::act(t, crate::drive::scsi::Action::Format { block_size: w.block_size, pi_type: w.pi_type }).map(|s| format!("{s} through {}", t.name()))
        })?
    }
    fn progress(&self, sas: u64) -> Result<Option<f32>, String> {
        crate::drive::mpt::with_drive(std::path::Path::new("/dev"), std::path::Path::new("/sys"), sas, crate::drive::scsi::format_progress)?
    }
    fn rescan(&self, sas: u64) {
        crate::drive::mpt::rescan_for(std::path::Path::new("/sys"), sas);
        super::hba::poke();
    }
    fn linux_ready(&self, sas: u64) -> bool {
        let want = key(sas);
        let (rows, _) = super::hba::read();
        rows.iter().any(|r| r["sas_address"] == want.as_str() && r["state"] == "ready")
    }
    fn test(&self, sas: u64) -> Result<(bool, Vec<String>), String> {
        let v = super::hba::write_test(sas)?;
        let lines = v["lines"].as_array().map(|a| a.iter().filter_map(|l| l.as_str().map(str::to_string)).collect()).unwrap_or_default();
        Ok((v["passed"].as_bool().unwrap_or(false), lines))
    }
    fn sleep(&self, secs: u64) {
        std::thread::sleep(std::time::Duration::from_secs(secs));
    }
    fn now(&self) -> u64 {
        crate::install_progress::now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::scsi::Capacity;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A shelf in a test: each drive at a format, formats taking 90 s,
    /// some drives' firmware refusing writes with PI.
    struct Fake {
        clock: AtomicU64,
        at: Mutex<HashMap<u64, Want>>,
        started: Mutex<HashMap<u64, u64>>,
        refuse_pi: Vec<u64>,
        refuse_format: Vec<u64>,
        log: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(drives: &[(u64, Want)], refuse_pi: &[u64]) -> Fake {
            Fake {
                clock: AtomicU64::new(1000),
                at: Mutex::new(drives.iter().copied().collect()),
                started: Mutex::new(HashMap::new()),
                refuse_pi: refuse_pi.to_vec(),
                refuse_format: vec![],
                log: Mutex::new(vec![]),
            }
        }
        fn log(&self, s: String) {
            self.log.lock().unwrap().push(s);
        }
    }

    impl Drives for Fake {
        fn ident(&self, sas: u64) -> Result<Ident, String> {
            let w = self.at.lock().unwrap().get(&sas).copied();
            Ok(Ident {
                vendor: Some("NETAPP".into()),
                revision: Some("NA02".into()),
                capacity: w.map(|w| Capacity { blocks: 1000, block_size: w.block_size, prot_en: w.pi_type > 0, pi_type: w.pi_type, lbpme: false }),
                ..Default::default()
            })
        }
        fn format(&self, sas: u64, w: Want) -> Result<String, String> {
            if self.refuse_format.contains(&sas) {
                return Err("ILLEGAL REQUEST".into());
            }
            self.log(format!("format {sas:x} {}", w.said()));
            self.at.lock().unwrap().insert(sas, w);
            self.started.lock().unwrap().insert(sas, self.now());
            Ok(format!("FORMAT UNIT to {} started (IMMED)", w.said()))
        }
        fn progress(&self, sas: u64) -> Result<Option<f32>, String> {
            let s = self.started.lock().unwrap()[&sas];
            let el = self.now() - s;
            Ok((el < 90).then_some(el as f32 * 100.0 / 90.0))
        }
        fn rescan(&self, _sas: u64) {}
        fn linux_ready(&self, _sas: u64) -> bool {
            true
        }
        fn test(&self, sas: u64) -> Result<(bool, Vec<String>), String> {
            let w = self.at.lock().unwrap()[&sas];
            self.log(format!("test {sas:x} {}", w.said()));
            let refused = w.pi_type == 1 && self.refuse_pi.contains(&sas);
            Ok((!refused, vec![if refused { "WRPROTECT 0 LBA 0: write ABORTED COMMAND".into() } else { "good".into() }]))
        }
        fn sleep(&self, secs: u64) {
            self.clock.fetch_add(secs, Ordering::SeqCst);
        }
        fn now(&self) -> u64 {
            self.clock.load(Ordering::SeqCst)
        }
    }

    const PI: Want = Want { block_size: 4096, pi_type: 1 };
    const PLAIN: Want = Want { block_size: 4096, pi_type: 0 };
    const B512: Want = Want { block_size: 512, pi_type: 0 };

    fn opts(canary: bool) -> Opts {
        Opts { want: PI, fallback: vec![PLAIN], test: true, canary }
    }

    fn register(drives: &[u64], canary: bool) {
        with(|m| {
            for (i, a) in drives.iter().enumerate() {
                m.insert(key(*a), Job {
                    sas_address: key(*a), enclosure: None, slot: None,
                    state: if canary && i > 0 { "waiting" } else { "queued" }.into(), canary: canary && i == 0,
                    format: PI.said(), asked: PI.said(), pct: None, started: 0, elapsed_secs: 0, eta_secs: None, ended: None, said: vec![], revision: None,
                });
            }
        });
    }

    fn job(a: u64) -> Job {
        with(|m| m[&key(a)].clone())
    }

    #[test]
    fn formats_parse() {
        assert_eq!(Want::parse("4096+pi1").unwrap(), PI);
        assert_eq!(Want::parse("4096").unwrap(), PLAIN);
        assert_eq!(Want::parse("512").unwrap(), B512);
        assert!(Want::parse("520").is_err() && Want::parse("4096+pi2").is_err());
        assert_eq!(eta(25.0, 600), Some(1800));
        assert_eq!(eta(0.0, 600), None);
    }

    /// One 512-byte drive to 4096 + PI: formatted, its progress followed,
    /// tested, ready; a drive already there is not formatted again (#614).
    #[test]
    fn one_drive_to_its_end() {
        let f = Fake::new(&[(0xA1, B512), (0xA2, PI)], &[]);
        register(&[0xA1, 0xA2], false);
        assert_eq!(run_one(&f, 0xA1, &opts(false)), Some(PI));
        let j = job(0xA1);
        assert_eq!((j.state.as_str(), j.pct, j.revision.as_deref()), ("ready", Some(100.0), Some("NA02")));
        assert!(j.said.iter().any(|l| l.starts_with("formatted to 4096 + PI type 1 in")), "{:?}", j.said);
        assert!(j.ended.is_some() && j.elapsed_secs >= 90);
        assert_eq!(run_one(&f, 0xA2, &opts(false)), Some(PI));
        assert!(job(0xA2).said[0].starts_with("already at 4096 + PI type 1"));
        assert!(!f.log.lock().unwrap().iter().any(|l| l.starts_with("format a2")), "not formatted again");
    }

    /// forge1's bay 3: the firmware takes the PI format and refuses its
    /// writes; it is formatted again without PI, tested, ready there.
    #[test]
    fn a_drive_refusing_pi_writes_falls_back() {
        let f = Fake::new(&[(0xB1, B512)], &[0xB1]);
        register(&[0xB1], false);
        assert_eq!(run_one(&f, 0xB1, &opts(false)), Some(PLAIN));
        let j = job(0xB1);
        assert_eq!((j.state.as_str(), j.format.as_str()), ("ready", "4096 without PI"));
        assert!(j.said.iter().any(|l| l.contains("refuses writes at 4096 + PI type 1") && l.contains("formatting it to 4096 without PI instead")), "{:?}", j.said);
        // With no fallback left: failed, said.
        let f = Fake::new(&[(0xB2, B512)], &[0xB2]);
        register(&[0xB2], false);
        assert_eq!(run_one(&f, 0xB2, &Opts { fallback: vec![], ..opts(false) }), None);
        assert_eq!(job(0xB2).state, "failed");
        assert!(job(0xB2).said.last().unwrap().contains("no fallback format is left"));
    }

    /// A NetApp shelf: the canary first, the rest only after it, and at the
    /// format it ended at; a failed canary formats nothing else.
    #[test]
    fn a_shelf_canary_first() {
        let f = Fake::new(&[(0xC1, B512), (0xC2, B512), (0xC3, B512)], &[0xC1, 0xC2, 0xC3]);
        let shelf = vec![(0xC1, None, Some(0)), (0xC2, None, Some(1)), (0xC3, None, Some(2))];
        register(&[0xC1, 0xC2, 0xC3], true);
        run_all(&f, shelf.clone(), opts(true));
        let log = f.log.lock().unwrap().clone();
        let first_other = log.iter().position(|l| l.starts_with("format c2") || l.starts_with("format c3")).unwrap();
        let canary_done = log.iter().position(|l| l == "test c1 4096 without PI").unwrap();
        assert!(canary_done < first_other, "the others wait for the canary: {log:?}");
        assert!(!log.iter().any(|l| l == "format c2 4096 + PI type 1"), "the others start at the canary's format: {log:?}");
        for a in [0xC1, 0xC2, 0xC3] {
            assert_eq!((job(a).state.as_str(), job(a).format.as_str()), ("ready", "4096 without PI"));
        }
        // A canary that fails: the rest are not formatted.
        let mut f = Fake::new(&[(0xD1, B512), (0xD2, B512)], &[]);
        f.refuse_format = vec![0xD1];
        register(&[0xD1, 0xD2], true);
        run_all(&f, vec![(0xD1, None, None), (0xD2, None, None)], opts(true));
        assert_eq!(job(0xD1).state, "failed");
        assert_eq!(job(0xD2).state, "failed");
        assert!(job(0xD2).said.last().unwrap().contains("the canary 0x00000000000000d1 failed"));
        assert!(!f.log.lock().unwrap().iter().any(|l| l.starts_with("format d2")));
    }
}
