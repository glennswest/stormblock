//! The shelf test (#390): two RAID-6 sets and their spares, laid out on a
//! shelf through an engine's API, and the owner's test plan run against them.
//! The same program runs on the Dell's NetApp DS224C and, in
//! `ci-shelf-raid6-verify.sh`, on scsi_debug drives in QEMU.
//!
//! ```text
//! shelf_raid6_test run --api http://127.0.0.1:9090 --token T --state s.json \
//!     --drives /dev/sdb,/dev/sdc,…  [--extra /dev/sdz,…] [--fill-pct 50] [--allow-no-pi]
//! shelf_raid6_test writeloop --api … --token … --state s.json     # until killed
//! shelf_raid6_test verify    --api … --token … --state s.json [--wait-clean SECS]
//! ```
//!
//! `run`: (1) the drives registered (each 4096 + T10 PI type 1 unless
//! `--allow-no-pi`; a drive that is not is refused, as the owner asked), the
//! shelf laid out (`POST /api/v1/shelves`: 2 sets, 2 spares); (2) a volume
//! pinned to each set filled with a pattern to `--fill-pct` and read back;
//! (3) one member of each set failed: reads stay right, the spare rebuilds,
//! timed; (4) two members of set A failed (spares used up): reads right;
//! (5) a third refused (409); the `--extra` drives added as spares, both
//! rebuilt; (6) a block flipped on a data member of set B (behind the engine:
//! PI is regenerated, so only parity can tell), found by a scrub, put right by
//! a repairing one, the data read back; (7) a full scrub timed; (9) both sets
//! written at once, then B written while A rebuilds. `RESULT name PASS|FAIL`
//! and `NUM name value unit` lines; the state (arrays, volumes, sizes) for
//! `verify` and `writeloop` (step 8, the power cuts, is the caller's:
//! `writeloop`, kill the engine, restart it, `verify`).

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const MIB: u64 = 1024 * 1024;
const CHUNK: usize = MIB as usize;

struct Api {
    base: String,
    token: String,
    c: reqwest::Client,
}

impl Api {
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut r = self.c.request(method, format!("{}{}", self.base, path)).bearer_auth(&self.token);
        if let Some(b) = body {
            r = r.json(&b);
        }
        match r.send().await {
            Ok(resp) => {
                let st = resp.status().as_u16();
                let text = resp.text().await.unwrap_or_default();
                (st, serde_json::from_str(&text).unwrap_or(Value::String(text)))
            }
            Err(e) => (0, Value::String(e.to_string())),
        }
    }
    async fn get(&self, path: &str) -> Value {
        self.call(reqwest::Method::GET, path, None).await.1
    }
    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }
}

fn result(name: &str, ok: bool, why: &str) {
    if ok {
        println!("RESULT {name} PASS");
    } else {
        println!("RESULT {name} FAIL ({why})");
    }
}

fn num(name: &str, v: f64, unit: &str) {
    println!("NUM {name} {v:.2} {unit}");
}

/// The pattern at `off` (a whole chunk), different for each volume.
fn pattern(vol: u64, off: u64, buf: &mut [u8]) {
    let mut x = vol.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ off.wrapping_add(1).wrapping_mul(0xD1B5_4A32_D192_ED03);
    for c in buf.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        c.copy_from_slice(&x.to_le_bytes()[..c.len()]);
    }
}

/// A buffer aligned for O_DIRECT.
fn aligned(len: usize) -> Box<[u8]> {
    let layout = std::alloc::Layout::from_size_align(len, 4096).unwrap();
    unsafe {
        let p = std::alloc::alloc_zeroed(layout);
        Box::from_raw(std::ptr::slice_from_raw_parts_mut(p, len))
    }
}

fn open_direct(dev: &str, write: bool) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().read(true).write(write).custom_flags(libc::O_DIRECT).open(dev)
}

/// Write the pattern over `[from, to)` of `dev`; MB/s.
fn fill(dev: &str, vol: u64, from: u64, to: u64) -> std::io::Result<f64> {
    let mut f = open_direct(dev, true)?;
    let mut buf = aligned(CHUNK);
    let t = Instant::now();
    let mut off = from;
    f.seek(SeekFrom::Start(off))?;
    while off < to {
        pattern(vol, off, &mut buf);
        f.write_all(&buf)?;
        off += CHUNK as u64;
    }
    f.sync_all()?;
    Ok((to - from) as f64 / MIB as f64 / t.elapsed().as_secs_f64())
}

/// Read `[0, to)` back against the pattern: `Ok(MB/s)` or where it differs.
fn check(dev: &str, vol: u64, to: u64) -> Result<f64, String> {
    let mut f = open_direct(dev, false).map_err(|e| format!("open {dev}: {e}"))?;
    let (mut got, mut want) = (aligned(CHUNK), vec![0u8; CHUNK]);
    let t = Instant::now();
    let mut off = 0;
    while off < to {
        f.read_exact(&mut got).map_err(|e| format!("read {dev} at {off}: {e}"))?;
        pattern(vol, off, &mut want);
        if got[..] != want[..] {
            let i = got.iter().zip(&want).position(|(a, b)| a != b).unwrap_or(0);
            return Err(format!("{dev}: differs at byte {}", off + i as u64));
        }
        off += CHUNK as u64;
    }
    Ok(to as f64 / MIB as f64 / t.elapsed().as_secs_f64())
}

/// Where a volume's last eighth starts, MiB-aligned: `writeloop` writes from
/// here, `verify` checks up to here.
fn tail_start(size: u64) -> u64 {
    ((size - size / 8) / MIB) * MIB
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn list(s: Option<String>) -> Vec<String> {
    s.map(|s| s.split(',').filter(|x| !x.is_empty()).map(str::to_string).collect()).unwrap_or_default()
}

/// A drive's PI profile and logical block from sysfs.
fn drive_format(dev: &str) -> (String, u32) {
    let name = std::path::Path::new(dev).file_name().and_then(|n| n.to_str()).unwrap_or("");
    let rd = |p: &str| std::fs::read_to_string(format!("/sys/block/{name}/{p}")).unwrap_or_default().trim().to_string();
    let f = rd("integrity/format");
    (if f.is_empty() { "none".into() } else { f }, rd("queue/logical_block_size").parse().unwrap_or(0))
}

async fn array(api: &Api, id: &str) -> Value {
    api.get(&format!("/api/v1/arrays/{id}")).await
}

fn state_of(a: &Value) -> String {
    a["status"]["state"].as_str().unwrap_or("?").to_string()
}

/// Wait until the set is clean; the seconds it took, or None at `limit`.
async fn wait_clean(api: &Api, id: &str, limit: Duration) -> Option<f64> {
    let t = Instant::now();
    while t.elapsed() < limit {
        let a = array(api, id).await;
        if state_of(&a) == "clean" && a["status"]["rebuild"].is_null() {
            return Some(t.elapsed().as_secs_f64());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    None
}

/// The first active member's slot (skipping `skip`).
fn active_slot(a: &Value, skip: &[u64]) -> Option<u64> {
    a["members"].as_array()?.iter().find_map(|m| {
        let slot = m["index"].as_u64()?;
        (m["state"].as_str()?.eq_ignore_ascii_case("active") && !skip.contains(&slot)).then_some(slot)
    })
}

/// Attach a volume over ublk: its device.
async fn attach(api: &Api, vol: &str) -> Option<String> {
    let (_, v) = api.post(&format!("/api/v1/volumes/{vol}/attach"), json!({"transport": "ublk"})).await;
    v["device_hint"].as_str().map(str::to_string)
}

async fn scrub(api: &Api, id: &str, repair: bool) -> (Value, f64) {
    let t = Instant::now();
    api.post(&format!("/api/v1/arrays/{id}/scrub"), json!({"repair": repair})).await;
    loop {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let a = array(api, id).await;
        let s = &a["status"]["scrub"];
        if s.is_null() || !s["running"].as_bool().unwrap_or(false) {
            return (s.clone(), t.elapsed().as_secs_f64());
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).cloned().unwrap_or_default();
    let api = Api {
        base: arg(&args, "--api").unwrap_or("http://127.0.0.1:9090".into()),
        token: arg(&args, "--token").unwrap_or_default(),
        c: reqwest::Client::new(),
    };
    let state_path = arg(&args, "--state").unwrap_or("shelf-test.json".into());
    match mode.as_str() {
        "run" => run(&api, &args, &state_path).await,
        "verify" => verify(&api, &args, &state_path).await,
        "writeloop" => writeloop(&api, &state_path).await,
        _ => anyhow::bail!("usage: shelf_raid6_test run|verify|writeloop --api URL --token T --state FILE …"),
    }
}

async fn run(api: &Api, args: &[String], state_path: &str) -> anyhow::Result<()> {
    let drives = list(arg(args, "--drives"));
    let extra = list(arg(args, "--extra"));
    let fill_pct: u64 = arg(args, "--fill-pct").and_then(|v| v.parse().ok()).unwrap_or(50);
    let allow_no_pi = args.iter().any(|a| a == "--allow-no-pi");
    let shelf = arg(args, "--shelf").unwrap_or("ds1".into());

    // 1. The drives, 4K + PI, and the shelf laid out.
    let mut uuid_of = std::collections::HashMap::new();
    let listed = api.get("/api/v1/drives").await;
    for d in drives.iter().chain(&extra) {
        let (fmt, lbs) = drive_format(d);
        if !allow_no_pi && (fmt != "T10-DIF-TYPE1-CRC" || lbs != 4096) {
            result("drives-4k-pi", false, &format!("{d}: {fmt}, {lbs}-byte blocks"));
            anyhow::bail!("{d} is not 4096 + PI type 1");
        }
        let known = listed["items"].as_array().or(listed.as_array()).and_then(|items| {
            items.iter().find(|i| i["path"].as_str() == Some(d)).and_then(|i| i["uuid"].as_str().map(str::to_string))
        });
        let uuid = match known {
            Some(u) => u,
            None => {
                let (st, v) = api.post("/api/v1/drives", json!({"path": d})).await;
                v["uuid"].as_str().map(str::to_string).ok_or_else(|| anyhow::anyhow!("open {d}: {st} {v}"))?
            }
        };
        uuid_of.insert(d.clone(), uuid);
    }
    result("drives-4k-pi", true, "");
    let bay: Vec<&String> = drives.iter().map(|d| &uuid_of[d]).collect();
    let (st, sh) = api
        .post("/api/v1/shelves", json!({"name": shelf, "drive_uuids": bay, "level": "raid6", "sets": 2, "spares": 2}))
        .await;
    let sets: Vec<Value> = sh["sets"].as_array().cloned().unwrap_or_default();
    let ok = st / 100 == 2 && sets.len() == 2 && sh["spares"].as_array().map(|s| s.len()) == Some(2)
        && sets.iter().all(|s| state_of(s) == "clean");
    result("shelf-laid-out", ok, &format!("{st} {sh}"));
    if !ok {
        anyhow::bail!("the shelf did not lay out");
    }
    num("usable", sh["usable_bytes"].as_f64().unwrap_or(0.0) / MIB as f64, "MiB");
    for s in &sets {
        println!("SET {} {} members, {} bytes", s["name"], s["member_count"], s["capacity_bytes"]);
    }

    // 2. A volume on each set, filled and read back.
    let mut vols = Vec::new();
    for (i, s) in sets.iter().enumerate() {
        let cap = s["capacity_bytes"].as_u64().unwrap_or(0);
        let size = (cap * fill_pct / 100 / MIB) * MIB;
        let (st, v) = api
            .post("/api/v1/volumes", json!({"name": format!("{shelf}-fill-{i}"), "size": format!("{}M", size / MIB), "array_id": s["id"]}))
            .await;
        let vid = v["id"].as_str().map(str::to_string).ok_or_else(|| anyhow::anyhow!("volume on set {i}: {st} {v}"))?;
        let dev = attach(api, &vid).await.ok_or_else(|| anyhow::anyhow!("attach {vid}"))?;
        let w = fill(&dev, i as u64, 0, size)?;
        num(&format!("fill-set{i}"), w, "MB/s");
        let r = check(&dev, i as u64, size);
        result(&format!("fill-read-set{i}"), r.is_ok(), r.as_ref().err().map(String::as_str).unwrap_or(""));
        if let Ok(r) = r {
            num(&format!("read-set{i}"), r, "MB/s");
        }
        vols.push(json!({"array": s["id"], "volume": vid, "device": dev, "size": size}));
    }
    let state = json!({"shelf": shelf, "volumes": vols});
    std::fs::write(state_path, serde_json::to_vec_pretty(&state)?)?;
    let ids: Vec<String> = sets.iter().map(|s| s["id"].as_str().unwrap_or("").to_string()).collect();
    let devs: Vec<String> = vols.iter().map(|v| v["device"].as_str().unwrap_or("").to_string()).collect();
    let sizes: Vec<u64> = vols.iter().map(|v| v["size"].as_u64().unwrap_or(0)).collect();
    let limit = Duration::from_secs(arg(args, "--rebuild-limit").and_then(|v| v.parse().ok()).unwrap_or(1800));

    // 3. One member of each set: reads right while degraded, the spare rebuilds.
    for (i, id) in ids.iter().enumerate() {
        let a = array(api, id).await;
        let slot = active_slot(&a, &[]).unwrap_or(0);
        let (st, _) = api.post(&format!("/api/v1/arrays/{id}/members/{slot}/fail"), json!({})).await;
        let r = check(&devs[i], i as u64, sizes[i]);
        result(&format!("one-lost-reads-set{i}"), st / 100 == 2 && r.is_ok(), &format!("{st} {:?}", r.err()));
        match wait_clean(api, id, limit).await {
            Some(secs) => {
                num(&format!("rebuild-one-set{i}"), secs, "s");
                let r = check(&devs[i], i as u64, sizes[i]);
                result(&format!("one-lost-rebuilt-set{i}"), r.is_ok(), r.err().as_deref().unwrap_or(""));
            }
            None => result(&format!("one-lost-rebuilt-set{i}"), false, "not clean in time"),
        }
    }

    // 4 and 5. Set A: two lost (no spare left), readable; a third refused.
    let a_id = &ids[0];
    let mut gone = Vec::new();
    for _ in 0..2 {
        let a = array(api, a_id).await;
        let slot = active_slot(&a, &gone).unwrap_or(0);
        let (st, _) = api.post(&format!("/api/v1/arrays/{a_id}/members/{slot}/fail"), json!({})).await;
        gone.push(slot);
        if st / 100 != 2 {
            result("two-lost", false, &format!("fail slot {slot}: {st}"));
        }
    }
    let r = check(&devs[0], 0, sizes[0]);
    result("two-lost-reads", r.is_ok(), r.err().as_deref().unwrap_or(""));
    let a = array(api, a_id).await;
    let third = active_slot(&a, &gone).unwrap_or(0);
    let (st, v) = api.post(&format!("/api/v1/arrays/{a_id}/members/{third}/fail"), json!({})).await;
    result("third-refused", st == 409, &format!("{st} {v}"));
    let r = check(&devs[0], 0, sizes[0]);
    result("third-refused-reads", r.is_ok(), r.err().as_deref().unwrap_or(""));
    if extra.len() >= 2 {
        for d in extra.iter().take(2) {
            api.post("/api/v1/spares", json!({"drive_uuid": uuid_of[d], "pool": shelf})).await;
        }
        match wait_clean(api, a_id, limit).await {
            Some(secs) => {
                num("rebuild-two-setA", secs, "s");
                let r = check(&devs[0], 0, sizes[0]);
                result("two-lost-rebuilt", r.is_ok(), r.err().as_deref().unwrap_or(""));
            }
            None => result("two-lost-rebuilt", false, "not clean in time"),
        }
    }

    // 6. A flipped block on a data member of set B, behind the engine.
    let b_id = &ids[1];
    let b = array(api, b_id).await;
    let member_dev = b["members"].as_array().and_then(|m| {
        m.iter().find(|x| x["state"].as_str().is_some_and(|s| s.eq_ignore_ascii_case("active"))).and_then(|x| x["device_path"].as_str().map(str::to_string))
    });
    if let Some(md) = member_dev {
        // Inside the filled part of every member: a quarter of the way in.
        let size = std::fs::metadata(&md).ok().map(|m| m.len()).filter(|l| *l > 0).unwrap_or_else(|| {
            std::fs::File::open(&md).and_then(|mut f| f.seek(SeekFrom::End(0))).unwrap_or(0)
        });
        let at = (size / 4 / 4096) * 4096;
        {
            let mut f = open_direct(&md, true)?;
            let mut blk = aligned(4096);
            f.seek(SeekFrom::Start(at))?;
            f.read_exact(&mut blk)?;
            for x in blk.iter_mut().take(64) {
                *x ^= 0xFF;
            }
            f.seek(SeekFrom::Start(at))?;
            f.write_all(&blk)?;
            f.sync_all()?;
        }
        let (s, secs) = scrub(api, b_id, false).await;
        let found = s["mismatches"].as_u64().unwrap_or(0);
        result("corruption-found", found >= 1, &format!("{s}"));
        let total = s["total_bytes"].as_f64().unwrap_or(0.0);
        num("scrub-setB", secs, "s");
        num("scrub-setB-rate", total / MIB as f64 / secs.max(0.001), "MB/s (member data)");
        let (s, _) = scrub(api, b_id, true).await;
        result("corruption-repaired", s["repaired"].as_u64().unwrap_or(0) >= 1, &format!("{s}"));
        let (s, _) = scrub(api, b_id, false).await;
        result("corruption-clean-after", s["mismatches"].as_u64() == Some(0), &format!("{s}"));
        let r = check(&devs[1], 1, sizes[1]);
        result("corruption-data-right", r.is_ok(), r.err().as_deref().unwrap_or(""));
    } else {
        result("corruption-found", false, "no active member with a device path");
    }

    // 7. A full scrub of set A, timed.
    let (s, secs) = scrub(api, a_id, false).await;
    num("scrub-setA", secs, "s");
    num("scrub-setA-rate", s["total_bytes"].as_f64().unwrap_or(0.0) / MIB as f64 / secs.max(0.001), "MB/s (member data)");
    result("scrub-setA-clean", s["mismatches"].as_u64() == Some(0), &format!("{s}"));

    // 9. Both sets written at once; then B while A rebuilds.
    let both = |label: String, which: Vec<usize>| {
        let (devs, sizes) = (devs.clone(), sizes.clone());
        async move {
            let hs: Vec<_> = which
                .into_iter()
                .map(|i| {
                    let (d, sz) = (devs[i].clone(), sizes[i]);
                    std::thread::spawn(move || (i, fill(&d, i as u64, 0, sz)))
                })
                .collect();
            for h in hs {
                if let Ok((i, Ok(mbs))) = h.join() {
                    num(&format!("{label}-set{i}"), mbs, "MB/s");
                }
            }
        }
    };
    both("both-busy".into(), vec![0, 1]).await;
    // A third extra drive is the spare A rebuilds onto meanwhile.
    if let (Some(d), Some(slot)) = (extra.get(2), active_slot(&array(api, a_id).await, &[])) {
        api.post("/api/v1/spares", json!({"drive_uuid": uuid_of[d], "pool": shelf})).await;
        api.post(&format!("/api/v1/arrays/{a_id}/members/{slot}/fail"), json!({})).await;
        both("setB-while-setA-rebuilds".into(), vec![1]).await;
        match wait_clean(api, a_id, limit).await {
            Some(secs) => num("rebuild-under-load-setA", secs, "s"),
            None => result("rebuild-under-load-setA", false, "not clean in time"),
        }
    }
    for (i, d) in devs.iter().enumerate() {
        let r = check(d, i as u64, sizes[i]);
        result(&format!("end-reads-set{i}"), r.is_ok(), r.err().as_deref().unwrap_or(""));
    }
    Ok(())
}

/// Write past each volume's checked range until killed (the power cut's load).
async fn writeloop(api: &Api, state_path: &str) -> anyhow::Result<()> {
    let st: Value = serde_json::from_slice(&std::fs::read(state_path)?)?;
    let _ = api;
    let vols = st["volumes"].as_array().cloned().unwrap_or_default();
    let mut f: Vec<(std::fs::File, u64)> = Vec::new();
    for v in &vols {
        let dev = v["device"].as_str().unwrap_or("");
        f.push((open_direct(dev, true)?, v["size"].as_u64().unwrap_or(0)));
    }
    let mut buf = aligned(CHUNK);
    let mut n = 0u64;
    loop {
        for (i, (file, size)) in f.iter_mut().enumerate() {
            // The last eighth of each volume: never what `verify` checks.
            let from = tail_start(*size);
            let span = ((*size - from) / CHUNK as u64).max(1) * CHUNK as u64;
            let off = from + (n * CHUNK as u64) % span;
            pattern(1000 + i as u64, off, &mut buf);
            file.seek(SeekFrom::Start(off))?;
            file.write_all(&buf)?;
        }
        n += 1;
    }
}

/// After a cut: the sets back, clean (or rebuilding to clean), the data right.
async fn verify(api: &Api, args: &[String], state_path: &str) -> anyhow::Result<()> {
    let st: Value = serde_json::from_slice(&std::fs::read(state_path)?)?;
    let limit = Duration::from_secs(arg(args, "--wait-clean").and_then(|v| v.parse().ok()).unwrap_or(600));
    // A restarted engine has forgotten drives opened through the API: open
    // them again, and have the sets assembled from their superblocks.
    for d in list(arg(args, "--drives")) {
        api.post("/api/v1/drives", json!({"path": d})).await;
    }
    let (st_a, v) = api.call(reqwest::Method::POST, "/api/v1/arrays/assemble", None).await;
    println!("ASSEMBLE {st_a} {v}");
    for (i, v) in st["volumes"].as_array().cloned().unwrap_or_default().iter().enumerate() {
        let id = v["array"].as_str().unwrap_or("");
        let a = array(api, id).await;
        println!("SET {i} after the cut: {} ({})", state_of(&a), a["status"]);
        match wait_clean(api, id, limit).await {
            Some(secs) => num(&format!("after-cut-clean-set{i}"), secs, "s"),
            None => result(&format!("after-cut-clean-set{i}"), false, &state_of(&array(api, id).await)),
        }
        let (s, _) = scrub(api, id, false).await;
        result(&format!("after-cut-consistent-set{i}"), s["mismatches"].as_u64() == Some(0), &format!("{s}"));
        let vol = v["volume"].as_str().unwrap_or("");
        let size = v["size"].as_u64().unwrap_or(0);
        let checked = tail_start(size);
        match attach(api, vol).await {
            Some(dev) => {
                let r = check(&dev, i as u64, checked);
                result(&format!("after-cut-data-set{i}"), r.is_ok(), r.err().as_deref().unwrap_or(""));
            }
            None => result(&format!("after-cut-data-set{i}"), false, "no device"),
        }
    }
    Ok(())
}
