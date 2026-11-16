//! What the boot and install screen shows (#618), drawn from #616's record
//! (`install_progress::record`): the same data as the web page, so the two
//! never disagree. A pure function of the record and a few facts about the
//! machine, so it is tested as pictures.
//!
//! The look: a starship bridge status display in spirit (owner, #618: "an
//! enterprise boot screen (As in trek)"), our own design: a black field,
//! rounded elbow frames and pill bars in a small palette, condensed capitals,
//! large numeric readouts. Nothing copied, nothing named after anyone's.

use serde_json::Value;

use super::canvas::{rgb, Align, Canvas, Corners, Rgb};

pub const BG: Rgb = rgb(0, 0, 0);
pub const AMBER: Rgb = rgb(255, 153, 0);
pub const LAVENDER: Rgb = rgb(204, 153, 204);
pub const BLUE: Rgb = rgb(153, 204, 255);
pub const PEACH: Rgb = rgb(255, 204, 153);
pub const GREY: Rgb = rgb(170, 160, 150);
pub const RED: Rgb = rgb(221, 68, 68);
pub const TRACK: Rgb = rgb(52, 46, 40);
pub const INK: Rgb = rgb(0, 0, 0);

/// What the screen knows of the machine, beside the record.
#[derive(Debug, Clone, Default)]
pub struct Info {
    /// An install boot (the ticket is there), else an ordinary boot.
    pub install: bool,
    pub product: String,
    pub release: String,
    pub node: String,
    pub address: String,
    pub model: String,
    pub service_tag: String,
    pub now: u64,
}

/// `7H 10M`, `58M`, `45S`.
pub fn span(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}H {m:02}M")
    } else if m > 0 {
        format!("{m}M")
    } else {
        format!("{s}S")
    }
}

/// `10.0 TB`, `512 GB`.
pub fn size(bytes: u64) -> String {
    let tb = bytes as f64 / 1e12;
    if tb >= 1.0 {
        format!("{tb:.1} TB")
    } else {
        format!("{:.0} GB", bytes as f64 / 1e9)
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn u(v: &Value, k: &str) -> Option<u64> {
    v.get(k).and_then(Value::as_u64)
}

/// A drive's state in words and its colour.
pub fn drive_state(d: &Value) -> (String, Rgb) {
    let st = s(d, "state");
    let fmt = d.get("format");
    let fmt_said = fmt
        .and_then(|f| {
            let bs = f.get("block_size")?.as_u64()?;
            let pi = f.get("pi").and_then(Value::as_str).unwrap_or("none");
            Some(if pi == "none" || pi.is_empty() { format!("{bs}") } else { format!("{bs} + PI") })
        })
        .unwrap_or_default();
    match st {
        "formatting" => ("FORMATTING".into(), AMBER),
        "formatted" => ("FORMATTED".into(), BLUE),
        "already" if !fmt_said.is_empty() => (format!("ALREADY {fmt_said}"), BLUE),
        "already" => ("ALREADY FORMATTED".into(), BLUE),
        "laid" => ("LAID".into(), BLUE),
        "laying" => ("LAYING".into(), AMBER),
        "failed" => ("FAILED".into(), RED),
        "not ready" => ("NOT READY".into(), PEACH),
        "" | "found" => ("FOUND".into(), GREY),
        other => (other.to_uppercase(), GREY),
    }
}

/// A step's colour by its state.
fn step_colour(state: &str) -> Rgb {
    match state {
        "done" => BLUE,
        "running" => AMBER,
        "failed" => RED,
        "skipped" => TRACK,
        _ => TRACK,
    }
}

/// The step names as the screen says them.
fn step_name(step: &str) -> String {
    match step {
        "flow-over" => "HAND-OVER".into(),
        "set" => "DATA SET".into(),
        "lay" => "LAYOUT".into(),
        "format" => "FORMATS".into(),
        "laid" => "LAID".into(),
        "booted" => "OWN DISK".into(),
        other => other.to_uppercase(),
    }
}

/// Words wrapped to `max` wide at `px`.
fn wrap(c: &Canvas, text: &str, px: f32, max: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for w in text.split_whitespace() {
        let next = if cur.is_empty() { w.to_string() } else { format!("{cur} {w}") };
        if c.text_width(&next, px) > max && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            cur = w.to_string();
        } else {
            cur = next;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// The whole screen.
pub fn render(c: &mut Canvas, rec: &Value, info: &Info) {
    c.clear(BG);
    // Laid out on 1920 × 1080 and stretched to the display, across and down
    // apart (a BMC's 4:3 or 5:4 console is filled, not letterboxed); type
    // and corners take the smaller scale, so nothing is distorted.
    let (kx, ky) = (c.w as f32 / 1920.0, c.h as f32 / 1080.0);
    let k = kx.min(ky);
    let x = |v: f32| (v * kx) as i64;
    let y = |v: f32| (v * ky) as i64;
    let xf = |v: f32| v * kx;
    let yf = |v: f32| v * ky;
    // Lengths across (lw), down (lh), and radii (l).
    let lw = |v: f32| (v * kx) as i64;
    let lh = |v: f32| (v * ky) as i64;
    let l = |v: f32| (v * k) as i64;
    let p = |v: f32| v * k;

    let state = s(rec, "state");
    let failed = state == "failed";
    let finished = info.install && (state == "laid" || state == "booted");
    let kind = if info.install { "INSTALL" } else { "BOOT" };

    // ---- the header frame ----
    let head = if failed { RED } else { AMBER };
    c.elbow(x(24.0), y(24.0), lw(1872.0), lh(300.0), lw(200.0), lh(44.0), l(70.0), l(28.0), true, head);
    // The title, cut into the bar.
    let title = format!("{}  {kind}", if info.product.is_empty() { "STORMCOS" } else { info.product.as_str() });
    let tw = c.text_width(&title, p(54.0));
    c.fill_rect(x(1896.0) - tw as i64 - lw(48.0), y(24.0), tw as i64 + lw(24.0), lh(44.0), BG);
    c.text(xf(1896.0 - 36.0), yf(66.0), p(54.0), head, Align::Right, &title);
    // The end cap of the bar.
    c.round_rect(x(1896.0) - lw(16.0), y(24.0), lw(16.0), lh(44.0), l(8.0), Corners::RIGHT, head);
    // Side blocks of the header's vertical bar.
    c.fill_rect(x(24.0), y(196.0), lw(200.0), lh(6.0), BG);
    c.fill_rect(x(24.0), y(202.0), lw(200.0), lh(122.0), GREY);
    c.text(xf(214.0), yf(190.0), p(30.0), INK, Align::Right, kind);
    c.text(xf(214.0), yf(316.0), p(30.0), INK, Align::Right, &format!("{}", info.release.to_uppercase()));

    // Who and where.
    let node = if info.node.is_empty() { "—".to_string() } else { info.node.to_uppercase() };
    c.text(xf(260.0), yf(150.0), p(72.0), PEACH, Align::Left, &c.fit(&node, p(72.0), p(900.0)));
    let addr = if info.address.is_empty() { "NO ADDRESS YET".to_string() } else { info.address.clone() };
    c.text(xf(260.0), yf(202.0), p(40.0), BLUE, Align::Left, &addr);
    let machine = [info.model.as_str(), info.service_tag.as_str()]
        .iter()
        .filter(|v| !v.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("  ·  ");
    c.text(xf(260.0), yf(250.0), p(32.0), GREY, Align::Left, &c.fit(&machine.to_uppercase(), p(32.0), p(900.0)));
    c.text(xf(260.0), yf(296.0), p(28.0), LAVENDER, Align::Left, &format!("STARTED {}", u(rec, "started").map(|t| format!("{} AGO", span(info.now.saturating_sub(t)))).unwrap_or_else(|| "—".into())));

    // The big readout: percent while drives format, else the state.
    let drives: Vec<Value> = rec.get("drives").and_then(Value::as_array).cloned().unwrap_or_default();
    let formatting: Vec<&Value> = drives.iter().filter(|d| s(d, "state") == "formatting").collect();
    let big = if failed {
        "STOPPED".to_string()
    } else if finished {
        "COMPLETE".to_string()
    } else if !formatting.is_empty() {
        let pct: f64 = formatting.iter().filter_map(|d| d.get("percent").and_then(Value::as_f64)).sum::<f64>() / formatting.len() as f64;
        format!("{pct:.0}%")
    } else {
        let st = rec.get("stage").map(|v| s(v, "stage")).unwrap_or("");
        step_name(if st.is_empty() { state } else { st })
    };
    c.text(xf(1880.0), yf(206.0), p(124.0), if failed { RED } else { AMBER }, Align::Right, &big);
    let eta = u(rec, "eta_s").map(|e| format!("ETA {}", span(e))).unwrap_or_else(|| if formatting.is_empty() { String::new() } else { "ETA —".into() });
    if !eta.is_empty() && !failed && !finished {
        c.text(xf(1880.0), yf(262.0), p(44.0), BLUE, Align::Right, &eta);
    }
    let now_said = rec.get("stage").map(|v| s(v, "detail").to_string()).unwrap_or_default();
    if !now_said.is_empty() && !failed && !finished {
        c.text(xf(1880.0), yf(304.0), p(26.0), GREY, Align::Right, &c.fit(&now_said.to_uppercase(), p(26.0), p(760.0)));
    }

    // ---- the lower frame ----
    let frame = if failed { RED } else { LAVENDER };
    c.elbow(x(24.0), y(348.0), lw(1872.0), lh(708.0), lw(200.0), lh(36.0), l(70.0), l(28.0), false, frame);
    c.round_rect(x(1896.0) - lw(16.0), y(1020.0), lw(16.0), lh(36.0), l(8.0), Corners::RIGHT, frame);
    // Side blocks: DRIVES, STEPS, HEALTH.
    let blocks: [(f32, f32, Rgb, &str); 3] = [(348.0, 328.0, BLUE, "DRIVES"), (682.0, 186.0, AMBER, "STEPS"), (874.0, 108.0, PEACH, "HEALTH")];
    for (top, h, col, label) in blocks {
        c.fill_rect(x(24.0), y(top), lw(200.0), lh(h), col);
        c.fill_rect(x(24.0), y(top + h), lw(200.0), lh(6.0), BG);
        c.text(xf(214.0), yf(top + h - 12.0), p(30.0), INK, Align::Right, label);
    }

    // ---- the drives ----
    let cols = [260.0, 340.0, 700.0, 830.0, 930.0, 1180.0, 1640.0, 1760.0, 1880.0];
    for (i, h) in ["SLOT", "DRIVE", "SIZE", "SEEN BY", "STATE", "PROGRESS", "", "ELAPSED", "ETA"].iter().enumerate() {
        let al = if i >= 7 { Align::Right } else { Align::Left };
        c.text(xf(cols[i]), yf(380.0), p(24.0), GREY, al, h);
    }
    let rows = drives.len().min(5);
    for (i, d) in drives.iter().take(rows).enumerate() {
        let ry = 396.0 + i as f32 * 56.0;
        let base = ry + 40.0;
        let slot = d.get("slot").map(|v| match v {
            Value::String(t) => t.clone(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        }).filter(|t| !t.is_empty()).unwrap_or_else(|| "—".into());
        c.text(xf(cols[0]), yf(base), p(34.0), PEACH, Align::Left, &slot);
        let dev = s(d, "dev");
        let model = s(d, "model");
        let name = match (dev.is_empty(), model.is_empty()) {
            (false, false) => format!("{} {}", dev.to_uppercase(), model),
            (false, true) => dev.to_uppercase(),
            (true, false) => model.to_string(),
            (true, true) => s(d, "sas").to_string(),
        };
        c.text(xf(cols[1]), yf(base), p(34.0), BLUE, Align::Left, &c.fit(&name, p(34.0), p(350.0)));
        c.text(xf(cols[2]), yf(base), p(30.0), GREY, Align::Left, &u(d, "bytes").filter(|b| *b > 0).map(size).unwrap_or_else(|| "—".into()));
        c.text(xf(cols[3]), yf(base), p(30.0), GREY, Align::Left, &s(d, "found").to_uppercase());
        let (said, col) = drive_state(d);
        let sw = c.text_width(&said, p(28.0)) + p(36.0);
        c.pill(x(cols[4]), y(ry + 12.0), sw as i64, lh(36.0), col);
        c.text(xf(cols[4]) + p(18.0), yf(ry + 12.0) + (lh(36.0) as f32 + p(28.0) * 0.72) / 2.0, p(28.0), INK, Align::Left, &said);
        let pct = d.get("percent").and_then(Value::as_f64);
        let frac = match s(d, "state") {
            "formatted" | "already" | "laid" => 1.0,
            _ => pct.unwrap_or(0.0) as f32 / 100.0,
        };
        c.bar(x(cols[5]), y(ry + 20.0), lw(420.0), lh(22.0), frac, TRACK, col);
        if let Some(pc) = pct {
            c.text(xf(cols[6]), yf(base), p(34.0), col, Align::Left, &format!("{pc:.1}%"));
        }
        c.text(xf(cols[7]), yf(base), p(30.0), GREY, Align::Right, &u(d, "elapsed_s").map(span).unwrap_or_else(|| "—".into()));
        c.text(xf(cols[8]), yf(base), p(30.0), GREY, Align::Right, &u(d, "eta_s").map(span).unwrap_or_else(|| "—".into()));
    }
    if drives.is_empty() {
        c.text(xf(260.0), yf(440.0), p(34.0), GREY, Align::Left, "NO DRIVE LISTED YET");
    } else if drives.len() > rows {
        c.text(xf(260.0), yf(672.0), p(26.0), GREY, Align::Left, &format!("+ {} MORE DRIVE(S): F2 FOR THE LOG", drives.len() - rows));
    }

    // ---- the end panels, else the steps and the health strip ----
    if failed || finished {
        let col = if failed { RED } else { BLUE };
        c.round_rect(x(260.0), y(700.0), lw(1636.0), lh(296.0), l(36.0), Corners::ALL, col);
        let head = if failed { format!("{kind} STOPPED") } else { "INSTALL COMPLETE".to_string() };
        c.text(xf(300.0), yf(776.0), p(64.0), INK, Align::Left, &head);
        let words = if failed {
            let st = rec.get("stage").map(|v| s(v, "detail").to_string()).unwrap_or_default();
            let step = rec
                .get("layout")
                .and_then(Value::as_array)
                .and_then(|a| a.iter().find(|x| s(x, "state") == "failed").map(|x| s(x, "detail").to_string()))
                .unwrap_or_default();
            if !st.is_empty() { st } else if !step.is_empty() { step } else { "SEE THE LOG (F2)".into() }
        } else {
            format!(
                "{} AT {} BOOTS FROM ITS OWN DISK.   ITS INSTALL RECORD:  http://{}:9090/install",
                if info.node.is_empty() { "THIS NODE".into() } else { info.node.to_uppercase() },
                if info.address.is_empty() { "ITS ADDRESS".into() } else { info.address.clone() },
                if info.address.is_empty() { "<node>".into() } else { info.address.clone() },
            )
        };
        for (i, line) in wrap(c, &words, p(32.0), p(1560.0)).iter().take(5).enumerate() {
            c.text(xf(300.0), yf(830.0 + i as f32 * 40.0), p(32.0), INK, Align::Left, line);
        }
    } else {
        let steps: Vec<Value> = rec
            .get("layout")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|st| info.install || s(st, "state") != "pending")
            .collect();
        let started = u(rec, "started");
        let n = steps.len().max(1) as f32;
        let w = ((1636.0 - (n - 1.0) * 14.0) / n).min(260.0);
        for (i, st) in steps.iter().enumerate() {
            let sx = 260.0 + i as f32 * (w + 14.0);
            let state = s(st, "state");
            let col = step_colour(state);
            c.pill(x(sx), y(712.0), lw(w), lh(56.0), col);
            let ink = if col == TRACK { GREY } else { INK };
            c.text(xf(sx + w / 2.0), yf(752.0), p(30.0), ink, Align::Center, &c.fit(&step_name(s(st, "step")), p(30.0), p(w - 20.0)));
            let when = match (state, u(st, "at"), started) {
                ("done", Some(a), Some(s0)) => format!("+{}", span(a.saturating_sub(s0))),
                ("running", Some(a), _) => format!("{} SO FAR", span(info.now.saturating_sub(a))),
                ("failed", _, _) => "FAILED".into(),
                ("skipped", _, _) => "SKIPPED".into(),
                _ => String::new(),
            };
            c.text(xf(sx + w / 2.0), yf(806.0), p(26.0), if state == "running" { AMBER } else { GREY }, Align::Center, &when);
        }
        // The health strip (stormipmi#111): values when the record has them.
        let hv = rec.get("health").cloned().unwrap_or(Value::Null);
        let read = |k: &str, unit: &str| -> String {
            match hv.get(k) {
                Some(Value::Number(n)) => format!("{n}{unit}"),
                Some(Value::String(t)) => t.clone(),
                _ => "—".into(),
            }
        };
        let cells = [
            ("INLET", read("inlet_c", "°C")),
            ("EXHAUST", read("exhaust_c", "°C")),
            ("FANS", read("fans", "")),
            ("POWER", read("power_w", " W")),
        ];
        for (i, (label, val)) in cells.iter().enumerate() {
            let cx = 260.0 + i as f32 * 300.0;
            c.text(xf(cx), yf(904.0), p(24.0), GREY, Align::Left, label);
            c.text(xf(cx), yf(966.0), p(58.0), if val == "—" { TRACK } else { PEACH }, Align::Left, val);
        }
        if hv.is_null() {
            c.text(xf(1880.0), yf(966.0), p(24.0), GREY, Align::Right, "SENSORS NOT READ YET");
        }
    }
    // The key line along the bottom bar.
    let keys = "F2  LOG    F1  THIS SCREEN";
    let kw = c.text_width(keys, p(26.0));
    c.fill_rect(x(1896.0) - kw as i64 - lw(60.0), y(1020.0), kw as i64 + lw(24.0), lh(36.0), BG);
    c.text(xf(1896.0 - 48.0), yf(1050.0), p(26.0), frame, Align::Right, keys);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// forge1, #618's "done when": four drives, three formatting, sda
    /// already at 4096 + PI, the steps ticking.
    pub fn forge1() -> (Value, Info) {
        let d = |slot: u64, dev: &str, state: &str, pct: Option<f64>, eta: Option<u64>| {
            json!({"slot": slot, "dev": dev, "model": "H7210A521SUN010T", "bytes": 9_931_038_130_176u64, "found": "linux",
                   "state": state, "percent": pct, "elapsed_s": pct.map(|_| 3480), "eta_s": eta,
                   "format": {"block_size": 4096, "pi": "type1"}})
        };
        let rec = json!({
            "state": "formatting", "started": 1000, "eta_s": 25800,
            "stage": {"stage": "formatting", "state": "running", "detail": "sdb sdc sdd through the card"},
            "drives": [d(0, "sda", "already", None, None), d(1, "sdb", "formatting", Some(12.3), Some(25800)),
                       d(2, "sdc", "formatting", Some(11.8), Some(25100)), d(3, "sdd", "formatting", Some(12.9), Some(24700))],
            "layout": [{"step": "inventory", "state": "done", "at": 1040}, {"step": "format", "state": "running", "at": 1100},
                       {"step": "lay", "state": "pending"}, {"step": "set", "state": "pending"}, {"step": "system legs", "state": "pending"},
                       {"step": "flow-over", "state": "pending"}, {"step": "laid", "state": "pending"}, {"step": "booted", "state": "pending"}],
            "health": null,
        });
        let info = Info {
            install: true, product: "STORMCOS".into(), release: "12.91".into(), node: "forge1".into(),
            address: "192.168.31.203".into(), model: "Dell PowerEdge R7525".into(), service_tag: "C2NR0Q2".into(), now: 4600,
        };
        (rec, info)
    }

    #[test]
    fn words_and_spans() {
        assert_eq!(span(25800), "7H 10M");
        assert_eq!(span(3480), "58M");
        assert_eq!(span(45), "45S");
        assert_eq!(size(9_931_038_130_176), "9.9 TB");
        let (rec, _) = forge1();
        assert_eq!(drive_state(&rec["drives"][0]), ("ALREADY 4096 + PI".into(), BLUE));
        assert_eq!(drive_state(&rec["drives"][1]).0, "FORMATTING");
    }

    /// The screen drawn: the drive rows' bars at their percent, the colours
    /// of each state, and the end panels. Each is also written as a PNG
    /// when `STORMBLOCK_SCREEN_SNAPSHOTS` names a directory.
    #[test]
    fn the_screen_as_pictures() {
        let out = std::env::var_os("STORMBLOCK_SCREEN_SNAPSHOTS").map(std::path::PathBuf::from);
        let snap = |name: &str, c: &Canvas| {
            if let Some(d) = &out {
                std::fs::create_dir_all(d).unwrap();
                std::fs::write(d.join(name), c.png()).unwrap();
            }
        };
        let (rec, info) = forge1();
        let mut c = Canvas::new(1920, 1080, 0);
        render(&mut c, &rec, &info);
        snap("forge1-formatting.png", &c);
        // sdb's bar: its first 12 % amber, the rest the track.
        let bar_y = 396 + 56 + 31;
        assert_eq!(c.get(1180 + 20, bar_y), AMBER);
        assert_eq!(c.get(1180 + 300, bar_y), TRACK);
        // sda (already): its bar full, blue.
        assert_eq!(c.get(1180 + 300, 396 + 31), BLUE);
        // The current step amber, a pending one the track.
        assert_eq!(c.get(260 + 100, 716), BLUE, "inventory done");

        let mut done = rec.clone();
        done["state"] = json!("laid");
        let mut c2 = Canvas::new(1280, 720, 0);
        render(&mut c2, &done, &info);
        snap("forge1-complete-1280.png", &c2);
        assert!(c2.px.iter().filter(|p| **p == BLUE).count() > 50_000, "the finished panel");

        let mut fail = rec.clone();
        fail["state"] = json!("failed");
        fail["stage"] = json!({"stage": "failed", "state": "failed", "detail": "FATAL: laying /dev/sdb failed: clearing the front of the drive: I/O error (so: the drive left the SAS link on the write: the kernel removed /dev/sdb (SAS 0x5000cca26a819e69)) (#573)"});
        let mut c3 = Canvas::new(1920, 1080, 0);
        render(&mut c3, &fail, &info);
        snap("forge1-failed.png", &c3);
        assert_eq!(c3.get(30, 30 + 100), RED, "the header frame turns red");

        // An ordinary boot: only the steps it has.
        let boot = json!({"state": "flow-over", "started": 1000,
            "stage": {"stage": "flow-over", "state": "running", "detail": ""},
            "drives": [{"dev": "sda", "model": "QEMU HARDDISK", "bytes": 103079215104u64, "found": "linux", "state": "found"}],
            "layout": [{"step": "inventory", "state": "done", "at": 1002}, {"step": "format", "state": "pending"}, {"step": "flow-over", "state": "running", "at": 1005}]});
        let mut c4 = Canvas::new(1024, 768, 0);
        render(&mut c4, &boot, &Info { install: false, node: "pvetest2".into(), address: "192.168.31.173".into(), now: 1010, ..Default::default() });
        snap("pvetest2-boot-1024.png", &c4);
    }
}
