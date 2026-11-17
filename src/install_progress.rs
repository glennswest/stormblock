//! The install's progress as one structured record (#616; owner: "a web
//! page with status, instead of primitive logging", and "on host": no forge
//! and no stormcentral needed for it).
//!
//! `/init` already writes everything the record needs under `/run/stormblock`
//! (it survives switch_root): the inventory (`local-disk.json`, with the
//! card's rows of #611), each drive's format lines and outcome
//! (`storage-format/<name>.log|.rc|.fmt`, a kernel name or a SAS address for
//! a drive only the card lists), the layout (`storage-layout.json`), its own
//! console (`init.log`), and the steps and stages it passes
//! (`install-progress.events`, one tab-separated line each, appended by
//! `stormblock install-progress event`). The record is built from those on
//! each read, so nothing has to keep it in step: the initramfs's
//! `stormblock install-status` serves it while the install runs, and the
//! node's engine serves the same at `/api/v1/install/progress` after it.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Where `/init` keeps what the record is made from.
pub const RUN: &str = "/run/stormblock";
/// The event log's name under [`RUN`].
pub const EVENTS: &str = "install-progress.events";

/// The steps an install goes through, in order: the page shows each, ticked
/// off as `/init` and the engine report it. A step not reported is pending.
pub const STEPS: &[&str] = &["inventory", "format", "lay", "set", "system legs", "flow-over", "laid", "booted"];

/// Whether `storage-format/<name>.log` is a drive's: a SAS address (a drive
/// only the card lists, `0x` and 16 hex digits) or a kernel disk name.
pub fn is_format_log(name: &str) -> bool {
    if let Some(h) = name.strip_prefix("0x") {
        return h.len() == 16 && h.chars().all(|c| c.is_ascii_hexdigit());
    }
    let letters = |rest: &str| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_lowercase());
    let digits = |rest: &str| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    for p in ["sd", "vd", "xvd", "hd"] {
        if let Some(r) = name.strip_prefix(p) {
            if letters(r) {
                return true;
            }
        }
    }
    if let Some(r) = name.strip_prefix("nvme") {
        if let Some((c, n)) = r.split_once('n') {
            return digits(c) && digits(n);
        }
    }
    name.strip_prefix("mmcblk").is_some_and(digits) || name.strip_prefix("sg").is_some_and(digits)
}

/// One line of the event log.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub at: u64,
    /// `stage` (what the install is doing), `step` (a layout step's state)
    /// or `drive` (a drive's state outside its format lines: found again,
    /// laid, failed).
    pub kind: String,
    pub name: String,
    pub state: String,
    pub detail: String,
}

fn clean(s: &str) -> String {
    s.chars().map(|c| if c == '\t' || c == '\n' || c == '\r' { ' ' } else { c }).filter(|c| !c.is_control()).collect()
}

/// Append one event (never fatal to the caller: the record is a view).
/// Once the engine has [`settle`]d, the kept copy follows.
pub fn append(run: &Path, kind: &str, name: &str, state: &str, detail: &str, at: u64) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(run)?;
    {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(run.join(EVENTS))?;
        writeln!(f, "{at}\t{}\t{}\t{}\t{}", clean(kind), clean(name), clean(state), clean(detail))?;
    }
    if let Some(dir) = KEEP_DIR.get() {
        keep(run, dir)?;
    }
    Ok(())
}

/// The install's record, kept in the engine's data directory, which the
/// state volume carries across reboots (#616: pvetest2's record said
/// `flow-over running` an hour after its install passed: an ordinary boot
/// after it had started a fresh one in `/run`, a tmpfs, and the install's own
/// `laid` and `booted` were gone with the reboot).
pub const KEPT: &str = "install-progress.kept";

static KEEP_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

fn keep(run: &Path, dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{KEPT}"));
    std::fs::copy(run.join(EVENTS), &tmp)?;
    std::fs::rename(tmp, dir.join(KEPT))
}

/// What [`settle`] made of this boot's record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settled {
    /// An install boot: `/init`'s events are the record, and are kept.
    Install,
    /// Another boot: the last install's kept record is put back.
    Kept,
    /// Another boot, with no install record kept (installed before #616's
    /// keeping): this boot's own events are not an install's, and go.
    None,
}

/// At the engine's start, after the state is restored into `data_dir`: which
/// record this boot shows. An install boot's `/init` events become the kept
/// record; on any other boot the kept record replaces what that boot's
/// `/init` wrote (a boot is no install, and its `flow-over running` would
/// never end). From here every [`append`] keeps the copy too.
pub fn settle(run: &Path, data_dir: &Path, install_boot: bool) -> std::io::Result<Settled> {
    let out = settle_into(run, data_dir, install_boot)?;
    let _ = KEEP_DIR.set(data_dir.to_path_buf());
    Ok(out)
}

/// [`settle`] without making every later [`append`] keep the copy.
fn settle_into(run: &Path, data_dir: &Path, install_boot: bool) -> std::io::Result<Settled> {
    let live = run.join(EVENTS);
    let kept = data_dir.join(KEPT);
    let out = if install_boot {
        if live.exists() {
            keep(run, data_dir)?;
        }
        Settled::Install
    } else if kept.exists() {
        std::fs::create_dir_all(run)?;
        std::fs::copy(&kept, &live)?;
        Settled::Kept
    } else {
        let _ = std::fs::remove_file(&live);
        Settled::None
    };
    Ok(out)
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn events(run: &Path) -> Vec<Event> {
    let Ok(text) = std::fs::read_to_string(run.join(EVENTS)) else { return vec![] };
    text.lines()
        .filter_map(|l| {
            let mut f = l.splitn(5, '\t');
            Some(Event {
                at: f.next()?.parse().ok()?,
                kind: f.next()?.to_string(),
                name: f.next()?.to_string(),
                state: f.next()?.to_string(),
                detail: f.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// `5h 17m`, `1m 00s`, `12m`, `45s` → seconds.
pub fn parse_span(s: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut any = false;
    for w in s.split_whitespace() {
        let (n, unit) = w.split_at(w.find(|c: char| !c.is_ascii_digit())?);
        let n: u64 = n.parse().ok()?;
        total += n * match unit {
            "d" => 86_400,
            "h" => 3600,
            "m" => 60,
            "s" => 1,
            _ => return None,
        };
        any = true;
    }
    any.then_some(total)
}

/// What one format line says of its drive.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Line {
    pub state: String,
    pub mode: Option<String>,
    pub percent: Option<f64>,
    pub eta_s: Option<u64>,
    pub elapsed_s: Option<u64>,
}

/// A format line from stormdrive (the sg path) or `stormblock hba act`
/// (the card path), its drive's name already taken off or not:
/// `formatting (full) 57.6%, about 5h 17m left, 7h 12m so far`,
/// `formatting (full) 1.2%, about 12h 40m left, 9m so far`,
/// `formatted (fast) in 1m 00s: …`, `already 512 with no PI: …`,
/// `refused 4096 with PI type 1; …`, `full format (FORMAT UNIT) to …`.
/// `(full) rest` → (`full`, `rest`).
fn mode_of(s: &str) -> Option<(String, &str)> {
    let (m, rest) = s.strip_prefix('(')?.split_once(')')?;
    Some((m.to_string(), rest.trim_start()))
}

pub fn parse_line(raw: &str) -> Option<Line> {
    let l = raw.trim();
    // `sda: …`, `/dev/sda: …`, `0x5000…: …`: the drive's own name first.
    let l = match l.split_once(": ") {
        Some((head, rest)) if !head.contains(' ') => rest,
        _ => l,
    };
    if let Some(rest) = l.strip_prefix("formatting ") {
        let (mode, rest) = match mode_of(rest) {
            Some((m, r)) => (Some(m), r),
            None => (None, rest),
        };
        let pct = rest.split('%').next().and_then(|p| p.trim().parse::<f64>().ok());
        let eta_s = rest.split("about ").nth(1).and_then(|r| r.split(" left").next()).and_then(parse_span);
        let elapsed_s = rest
            .rsplit(", ")
            .find(|p| p.ends_with(" so far"))
            .and_then(|p| p.strip_suffix(" so far"))
            .and_then(parse_span);
        return Some(Line { state: "formatting".into(), mode, percent: pct, eta_s, elapsed_s });
    }
    if let Some(rest) = l.strip_prefix("formatted ") {
        let (mode, rest) = match mode_of(rest) {
            Some((m, r)) => (Some(m), r),
            None => (None, rest),
        };
        // `in 1m 00s: …` right after the mode, or `through the card in 14h 20m; …`.
        let padded = format!(" {rest}");
        let elapsed_s = padded.split(" in ").nth(1).map(|r| r.split([':', ';']).next().unwrap_or(r)).and_then(parse_span);
        return Some(Line { state: "formatted".into(), mode, percent: Some(100.0), eta_s: Some(0), elapsed_s });
    }
    if l.starts_with("already ") {
        return Some(Line { state: "already".into(), percent: Some(100.0), ..Default::default() });
    }
    if l.starts_with("refused ") || l.starts_with("failed") {
        return Some(Line { state: "failed".into(), ..Default::default() });
    }
    if l.starts_with("full format") || l.starts_with("fast format") || l.starts_with("FORMAT UNIT to ") {
        let mode = if l.starts_with("fast") { "fast" } else { "full" };
        return Some(Line { state: "formatting".into(), mode: Some(mode.into()), percent: Some(0.0), ..Default::default() });
    }
    None
}

fn read_json(p: &Path) -> Value {
    std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)
}

fn tail(p: &Path, n: usize) -> Vec<String> {
    let Ok(b) = std::fs::read(p) else { return vec![] };
    // The last 256 KiB is plenty for the last lines of a long console.
    let from = b.len().saturating_sub(256 << 10);
    let text = String::from_utf8_lossy(&b[from..]);
    let lines: Vec<String> = text
        .split(['\n', '\r'])
        .map(strip_ansi)
        .filter(|l| !l.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// A console line without its colour codes and other escapes.
fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// One drive of the record, joined from the inventory, the card's rows, its
/// format files and the layout.
#[derive(Debug, Default)]
struct Drive {
    sas: Option<String>,
    dev: Option<String>,
    v: serde_json::Map<String, Value>,
}

fn key_of(name: &str) -> (Option<String>, Option<String>) {
    if name.starts_with("0x") {
        (Some(name.to_ascii_lowercase()), None)
    } else {
        (None, Some(name.trim_start_matches("/dev/").to_string()))
    }
}

fn find<'a>(drives: &'a mut Vec<Drive>, sas: Option<&str>, dev: Option<&str>) -> &'a mut Drive {
    let i = drives.iter().position(|d| {
        (sas.is_some() && d.sas.as_deref() == sas) || (dev.is_some() && d.dev.as_deref() == dev)
    });
    match i {
        Some(i) => {
            let d = &mut drives[i];
            if d.sas.is_none() {
                d.sas = sas.map(str::to_string);
            }
            if d.dev.is_none() {
                d.dev = dev.map(str::to_string);
            }
            d
        }
        None => {
            drives.push(Drive { sas: sas.map(str::to_string), dev: dev.map(str::to_string), v: Default::default() });
            drives.last_mut().expect("pushed")
        }
    }
}

/// The record, as `GET /api/v1/install/progress` answers it.
pub fn record(run: &Path, now: u64) -> Value {
    let inv = read_json(&run.join("local-disk.json"));
    let layout = read_json(&run.join("storage-layout.json"));
    let fmt_dir = run.join("storage-format");
    let evs = events(run);
    let mut drives: Vec<Drive> = vec![];

    // The card's rows first (#611: the card is the source of presence).
    for r in inv["card_rows"].as_array().into_iter().flatten() {
        let sas = r["sas_address"].as_str().map(str::to_ascii_lowercase);
        let dev = r["block"].as_str().map(str::to_string);
        let d = find(&mut drives, sas.as_deref(), dev.as_deref());
        d.v.insert("found".into(), json!(if dev.is_some() { "linux" } else { "card" }));
        let slot = match (r["enclosure"].as_str(), r["slot"].as_u64()) {
            (Some(e), Some(s)) => Some(format!("{e}:{s}")),
            (None, Some(s)) => Some(s.to_string()),
            _ => None,
        };
        d.v.insert("slot".into(), json!(slot));
        d.v.insert("card".into(), r.clone());
        if let Some(b) = r["bytes"].as_u64() {
            d.v.insert("bytes".into(), json!(b));
        }
        if let Some(q) = r["card_inquiry"].as_str() {
            d.v.entry("model").or_insert(json!(q));
        }
        if r["state"].as_str() == Some("no target") || r["state"].as_str() == Some("not ready") {
            d.v.insert("state".into(), json!("not ready"));
        }
    }
    // Then the kernel's drives.
    for r in inv["drives"].as_array().into_iter().flatten() {
        let Some(name) = r["name"].as_str() else { continue };
        let (_, dev) = key_of(name);
        let d = find(&mut drives, None, dev.as_deref());
        d.v.entry("found").or_insert(json!("linux"));
        for (k, from) in [("model", "model"), ("serial", "serial"), ("wwid", "wwid"), ("bay", "bay"), ("transport", "transport")] {
            if !r[from].is_null() {
                d.v.insert(k.into(), r[from].clone());
            }
        }
        if let Some(b) = r["size_bytes"].as_u64().filter(|b| *b > 0) {
            d.v.insert("bytes".into(), json!(b));
        }
        if r["ready"] == json!(false) {
            d.v.entry("state").or_insert(json!("not ready"));
        }
    }
    // Each drive's format, from its own lines.
    let mut started: Option<u64> = None;
    if let Ok(rd) = std::fs::read_dir(&fmt_dir) {
        let mut names: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".log")).map(str::to_string))
            // A drive's own log only: the lay's `lay.log` and
            // `raid-create.log` live here too, and were drive rows on
            // forge1's page (12.91).
            .filter(|n| is_format_log(n))
            .collect();
        names.sort();
        for name in names {
            let log = fmt_dir.join(format!("{name}.log"));
            let lines: Vec<String> = std::fs::read_to_string(&log).unwrap_or_default().lines().map(str::to_string).collect();
            let rc = std::fs::read_to_string(fmt_dir.join(format!("{name}.rc"))).ok().map(|s| s.trim().to_string());
            let fmtf = std::fs::read_to_string(fmt_dir.join(format!("{name}.fmt"))).ok().map(|s| s.trim().to_string());
            let mtime = std::fs::metadata(&log).and_then(|m| m.modified()).ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs());
            let (sas, dev) = key_of(&name);
            let d = find(&mut drives, sas.as_deref(), dev.as_deref());
            let last = lines.iter().rev().find_map(|l| parse_line(l));
            let mut state = last.as_ref().map(|l| l.state.clone());
            if let Some(l) = &last {
                if let Some(m) = &l.mode {
                    d.v.insert("mode".into(), json!(m));
                }
                d.v.insert("percent".into(), json!(l.percent));
                // The line's numbers are as of when it was written: the time
                // since then is added, so a page between lines still moves.
                let since = match (mtime, l.state.as_str()) {
                    (Some(t), "formatting") => now.saturating_sub(t),
                    _ => 0,
                };
                d.v.insert("elapsed_s".into(), json!(l.elapsed_s.map(|e| e + since)));
                d.v.insert("eta_s".into(), json!(l.eta_s.map(|e| e.saturating_sub(since))));
                if let (Some(e), Some(t)) = (l.elapsed_s, mtime) {
                    let s = t.saturating_sub(e);
                    started = Some(started.map_or(s, |x| x.min(s)));
                }
            }
            match fmtf.as_deref() {
                Some(f) if f.starts_with("already") => state = Some("already".into()),
                Some(f) if f.starts_with("formatted") => state = Some("formatted".into()),
                Some(f) if f.starts_with("refused") || f.starts_with("failed") => {
                    state = Some("failed".into());
                    d.v.insert("said".into(), json!(f));
                }
                _ => {}
            }
            match rc.as_deref() {
                Some("0") | Some("4") if state.as_deref() == Some("formatting") || state.is_none() => state = Some("formatted".into()),
                Some(c) if c != "0" && c != "4" => {
                    state = Some("failed".into());
                    d.v.entry("said").or_insert(json!(lines.iter().rev().take(3).rev().cloned().collect::<Vec<_>>().join(" ")));
                }
                _ => {}
            }
            if let Some(s) = state {
                d.v.insert("state".into(), json!(s));
            }
            d.v.insert("lines".into(), json!(lines[lines.len().saturating_sub(5)..].to_vec()));
        }
    }
    // Drive events (`drive <name> <state> <detail>`): found again, failed.
    for e in evs.iter().filter(|e| e.kind == "drive") {
        let (sas, dev) = key_of(&e.name);
        let d = find(&mut drives, sas.as_deref(), dev.as_deref());
        d.v.insert("state".into(), json!(e.state));
        if !e.detail.is_empty() {
            d.v.insert("said".into(), json!(e.detail));
        }
    }
    // Laid: the drives the layout names.
    let asked = &layout["asked"];
    for r in layout["drives"].as_array().into_iter().flatten() {
        let Some(dev) = r["device"].as_str() else { continue };
        let (_, dev) = key_of(dev);
        let d = find(&mut drives, None, dev.as_deref());
        d.v.insert("state".into(), json!("laid"));
        d.v.insert("format".into(), json!({
            "block_size": asked["block_size"], "pi": r["protection"], "method": r["method"], "was": r["format"],
        }));
    }

    // Steps and stages.
    let mut steps: Vec<Value> = STEPS.iter().map(|s| json!({"step": s, "state": "pending", "at": null, "detail": ""})).collect();
    let mut stages: Vec<Value> = vec![];
    let mut first = evs.first().map(|e| e.at);
    for e in &evs {
        match e.kind.as_str() {
            "step" => {
                let v = json!({"step": e.name, "state": e.state, "at": e.at, "detail": e.detail});
                match steps.iter_mut().find(|s| s["step"] == json!(e.name)) {
                    Some(s) => *s = v,
                    None => steps.push(v),
                }
            }
            "stage" => stages.push(json!({"stage": e.name, "state": e.state, "at": e.at, "detail": e.detail})),
            _ => {}
        }
    }
    if let Some(s) = started {
        first = Some(first.map_or(s, |f| f.min(s)));
    }
    let last_stage = stages.last().cloned().unwrap_or(json!({"stage": "starting", "state": "running", "at": null, "detail": ""}));
    let failed = steps.iter().any(|s| s["state"] == json!("failed")) || last_stage["state"] == json!("failed");
    let drives_v: Vec<Value> = drives
        .into_iter()
        .map(|d| {
            let mut v = d.v;
            v.insert("sas".into(), json!(d.sas));
            v.insert("dev".into(), json!(d.dev));
            v.entry("state").or_insert(json!("found"));
            v.entry("percent").or_insert(Value::Null);
            v.entry("eta_s").or_insert(Value::Null);
            v.entry("elapsed_s").or_insert(Value::Null);
            Value::Object(v)
        })
        .collect();
    let formatting = drives_v.iter().filter(|d| d["state"] == json!("formatting")).count();
    let eta_s = drives_v.iter().filter(|d| d["state"] == json!("formatting")).filter_map(|d| d["eta_s"].as_u64()).max();
    let state = if failed {
        "failed".to_string()
    } else if steps.iter().any(|s| s["step"] == json!("booted") && s["state"] == json!("done")) {
        "booted".into()
    } else if steps.iter().any(|s| s["step"] == json!("laid") && s["state"] == json!("done")) {
        "laid".into()
    } else if formatting > 0 {
        "formatting".into()
    } else {
        last_stage["stage"].as_str().unwrap_or("starting").to_string()
    };
    json!({
        "version": 1,
        "state": state,
        "stage": last_stage,
        "at": now,
        "started": first,
        "eta_s": eta_s,
        "eta": eta_s.map(|e| now + e),
        "drives": drives_v,
        "layout": steps,
        "stages": stages,
        "set": layout["set"],
        "system_legs": layout["system_legs"],
        "console": tail(&run.join("init.log"), 60),
        // In-band IPMI readings land here (stormipmi#111).
        "health": Value::Null,
    })
}

/// The page, a client of the record (`/install` and `/` on the node).
pub const PAGE: &str = include_str!("install_progress.html");

/// `stormblock install-status`: the record and the page on `listen`, read
/// only and open, until killed (`/init` runs it from network-up until
/// switch_root; the engine serves both after).
pub async fn serve(listen: &str, run: PathBuf) -> anyhow::Result<()> {
    let l = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(l, router::<()>(run, true)).await?;
    Ok(())
}

/// The page at `/install` (and at `/` when `root`) and the record. The
/// engine passes `root = false` with the old web UI, whose `/` it is.
pub fn router<S: Clone + Send + Sync + 'static>(run: PathBuf, root: bool) -> axum::Router<S> {
    use axum::routing::get;
    let page = || async {
        (
            [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"), (axum::http::header::CACHE_CONTROL, "no-store")],
            PAGE,
        )
    };
    let r = axum::Router::new().route("/install", get(page));
    let r = if root { r.route("/", get(page)) } else { r };
    r.route(
            "/api/v1/install/progress",
            get(move || {
                let run = run.clone();
                async move {
                    let v = tokio::task::spawn_blocking(move || record(&run, now())).await.unwrap_or(Value::Null);
                    ([(axum::http::header::CACHE_CONTROL, "no-store")], axum::Json(v))
                }
            }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// forge1 12.91: `lay` and `raid-create` were drive rows.
    #[test]
    fn only_a_drives_log_is_a_drive() {
        for n in ["sda", "sdab", "vdc", "nvme0n1", "nvme12n3", "mmcblk0", "sg3", "0x5000cca26801b649"] {
            assert!(is_format_log(n), "{n}");
        }
        for n in ["lay", "raid-create", "plan", "beat", "sd", "sda1", "nvme0", "0x5000", "0xZZ00cca26801b649"] {
            assert!(!is_format_log(n), "{n}");
        }
    }

    /// #616: an install boot's record is kept; a later boot shows it, not
    /// its own `flow-over running`; an engine's append keeps the copy.
    #[test]
    fn the_install_record_outlives_a_reboot() {
        let w = tempfile::tempdir().unwrap();
        let (run, dir) = (w.path().join("run"), w.path().join("engine"));
        // The install boot: /init's events, then the engine settles.
        append(&run, "stage", "flow-over", "running", "", 10).unwrap();
        assert_eq!(settle_into(&run, &dir, true).unwrap(), Settled::Install);
        assert!(std::fs::read_to_string(dir.join(KEPT)).unwrap().contains("flow-over"));
        // The engine's own append after settling is kept too.
        keep_after(&run, &dir, "step", "laid", "done", 20);
        assert!(std::fs::read_to_string(dir.join(KEPT)).unwrap().contains("laid"));
        // A reboot: /run is new, and that boot's /init wrote its own lines.
        std::fs::remove_dir_all(&run).unwrap();
        append(&run, "stage", "starting", "running", "", 30).unwrap();
        append(&run, "stage", "flow-over", "running", "", 31).unwrap();
        assert_eq!(settle_into(&run, &dir, false).unwrap(), Settled::Kept);
        let r = record(&run, 40);
        assert_eq!(r["started"], 10, "{r}");
        let laid = r["layout"].as_array().unwrap().iter().find(|s| s["step"] == "laid").unwrap().clone();
        assert_eq!(laid["state"], "done");
        assert_eq!(r["state"], "laid", "{r}");
        // What the proof reads (keys in order): `"started":10,"state":"laid"`.
        assert!(r.to_string().contains("\"started\":10,\"state\":\"laid\""), "{r}");
        // No record kept (a node installed before this): the boot's own
        // events are not an install's.
        let run2 = w.path().join("run2");
        append(&run2, "stage", "flow-over", "running", "", 50).unwrap();
        assert_eq!(settle_into(&run2, &w.path().join("none"), false).unwrap(), Settled::None);
        assert!(!run2.join(EVENTS).exists());
    }

    /// [`append`] with the kept copy, without the process-wide directory.
    fn keep_after(run: &Path, dir: &Path, kind: &str, name: &str, state: &str, at: u64) {
        append(run, kind, name, state, "", at).unwrap();
        keep(run, dir).unwrap();
    }

    #[test]
    fn lines() {
        let l = parse_line("sda: formatting (full) 57.6%, about 5h 17m left, 7h 12m so far").unwrap();
        assert_eq!(l.state, "formatting");
        assert_eq!(l.mode.as_deref(), Some("full"));
        assert_eq!(l.percent, Some(57.6));
        assert_eq!(l.eta_s, Some(5 * 3600 + 17 * 60));
        assert_eq!(l.elapsed_s, Some(7 * 3600 + 12 * 60));
        let c = parse_line("0x5000cca26801b649: formatting (full) 1.2%, about 12h 40m left, 9m so far").unwrap();
        assert_eq!((c.percent, c.eta_s, c.elapsed_s), (Some(1.2), Some(12 * 3600 + 40 * 60), Some(540)));
        let f = parse_line("sda: formatted (fast) in 1m 00s: 512-byte blocks, no PI").unwrap();
        assert_eq!((f.state.as_str(), f.elapsed_s), ("formatted", Some(60)));
        let g = parse_line("0x5000cca26801b649: formatted (full) through the card in 14h 20m; the kernel asked to look again").unwrap();
        assert_eq!(g.elapsed_s, Some(14 * 3600 + 20 * 60));
        assert_eq!(parse_line("vda: already 512 with no PI: 512-byte blocks").unwrap().state, "already");
        assert_eq!(parse_line("sda: refused 4096 with PI type 1; nothing was formatted").unwrap().state, "failed");
        assert_eq!(parse_line("sda: full format (FORMAT UNIT) to 4096 with PI type 1").unwrap().mode.as_deref(), Some("full"));
        assert_eq!(parse_line("the fast format was refused: a full FORMAT UNIT to the same format instead"), None);
        assert_eq!(parse_span("1m 00s"), Some(60));
        assert_eq!(parse_span("about"), None);
    }

    #[test]
    fn forge1_record() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path();
        std::fs::write(
            run.join("local-disk.json"),
            r#"{"drives": [{"name": "sda", "model": "H7210A521SUN010T", "serial": "E4G5WTTN", "wwid": null, "size_bytes": 9796000000000, "ready": true, "transport": "sas", "controller": "0000:c1:00.0", "shelf": null, "bay": null}],
               "card_rows": [
                 {"sas_address": "0x5000cca2680ab8d9", "enclosure": "0x52cea7f04e8aa500", "slot": 0, "state": "ready", "block": "sda", "sg": "sg0"},
                 {"sas_address": "0x5000cca26801b649", "enclosure": "0x52cea7f04e8aa500", "slot": 1, "state": "no target", "block": null,
                  "card_inquiry": "HGST H7210A521SUN010T B3Y1", "card_test_unit_ready": "NOT READY asc/ascq 0x31/0x01"}]}"#,
        )
        .unwrap();
        let f = run.join("storage-format");
        std::fs::create_dir_all(&f).unwrap();
        std::fs::write(f.join("sda.log"), "").unwrap();
        std::fs::write(f.join("sda.rc"), "0\n").unwrap();
        std::fs::write(f.join("sda.fmt"), "already type1 none\n").unwrap();
        std::fs::write(
            f.join("0x5000cca26801b649.log"),
            "0x5000cca26801b649: FORMAT UNIT to 4096 with PI type 1 started (IMMED) through the card\n0x5000cca26801b649: formatting (full) 1.2%, about 12h 40m left, 9m so far\n",
        )
        .unwrap();
        append(run, "step", "inventory", "done", "4 drives on the card, 1 with a Linux device", 100).unwrap();
        append(run, "step", "format", "running", "1 drive through the card", 110).unwrap();
        append(run, "stage", "formatting", "running", "", 110).unwrap();
        std::fs::write(run.join("init.log"), "\u{1b}[32m INFO\u{1b}[0m hello\r\nFORMAT: 9 min so far\n").unwrap();

        let r = record(run, 2000);
        assert_eq!(r["state"], json!("formatting"));
        let ds = r["drives"].as_array().unwrap();
        assert_eq!(ds.len(), 2, "{ds:?}");
        let sda = ds.iter().find(|d| d["dev"] == json!("sda")).unwrap();
        assert_eq!(sda["sas"], json!("0x5000cca2680ab8d9"));
        assert_eq!(sda["state"], json!("already"));
        assert_eq!(sda["found"], json!("linux"));
        assert_eq!(sda["slot"], json!("0x52cea7f04e8aa500:0"));
        let c = ds.iter().find(|d| d["sas"] == json!("0x5000cca26801b649")).unwrap();
        assert_eq!(c["found"], json!("card"));
        assert_eq!(c["dev"], Value::Null);
        assert_eq!(c["state"], json!("formatting"));
        assert_eq!(c["percent"], json!(1.2));
        assert_eq!(c["card"]["card_test_unit_ready"], json!("NOT READY asc/ascq 0x31/0x01"));
        assert!(c["eta_s"].as_u64().unwrap() <= 12 * 3600 + 40 * 60);
        assert_eq!(r["layout"][0]["state"], json!("done"));
        assert_eq!(r["layout"][1]["state"], json!("running"));
        assert_eq!(r["layout"][2]["state"], json!("pending"));
        assert_eq!(r["console"], json!([" INFO hello", "FORMAT: 9 min so far"]));
        assert!(r["eta"].as_u64().is_some());

        // Laid, then the engine's report.
        std::fs::write(
            run.join("storage-layout.json"),
            r#"{"from": "install", "drives": [{"device": "/dev/sda", "format": "already", "protection": "type1", "method": "none"}], "asked": {"block_size": 4096}, "system_legs": ["/dev/sda"], "set": null}"#,
        )
        .unwrap();
        std::fs::write(f.join("0x5000cca26801b649.rc"), "1\n").unwrap();
        append(run, "step", "laid", "done", "", 3000).unwrap();
        let r = record(run, 3001);
        assert_eq!(r["state"], json!("laid"));
        let sda = r["drives"].as_array().unwrap().iter().find(|d| d["dev"] == json!("sda")).cloned().unwrap();
        assert_eq!(sda["state"], json!("laid"));
        assert_eq!(sda["format"]["block_size"], json!(4096));
        let c = r["drives"].as_array().unwrap().iter().find(|d| d["sas"] == json!("0x5000cca26801b649")).cloned().unwrap();
        assert_eq!(c["state"], json!("failed"));

        append(run, "step", "lay", "failed", "STOP: storage: …", 3002).unwrap();
        assert_eq!(record(run, 3003)["state"], json!("failed"));
    }

    #[test]
    fn empty_run_dir() {
        let dir = tempfile::tempdir().unwrap();
        let r = record(dir.path(), 5);
        assert_eq!(r["state"], json!("starting"));
        assert_eq!(r["drives"], json!([]));
        assert_eq!(r["layout"].as_array().unwrap().len(), STEPS.len());
    }
}
