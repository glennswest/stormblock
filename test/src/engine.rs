//! The engine under test: the stormblock binary of the same commit, run as a
//! child of the test on sparse files in the results directory, and driven
//! through its API with the token it mints. Unprivileged: no ublk, no block
//! devices, NVMe/TCP on loopback, so it runs the same on every machine.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use stormblock::drive::BlockDevice;
use stormblock::http::Client;
use tokio::process::{Child, Command};

use crate::env::Env;

pub const MIB: u64 = 1 << 20;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(0)
}

pub struct Engine {
    pub dir: PathBuf,
    pub api: String,
    bin: PathBuf,
    api_port: u16,
    nvme_port: u16,
    nqn: String,
    /// This engine's node name in `/v1`.
    pub node: String,
    token: String,
    child: Option<Child>,
    http: Client,
}

impl Engine {
    /// Lay out a fresh engine under `<results>/work/<run>/<name>` with two
    /// drives of `drive_bytes` (sparse), start it, and give each drive a data
    /// slab. A restart adopts those slabs with their volumes — the way a node
    /// comes back — where `--raid` would build a new array and format a new
    /// slab on every start.
    pub async fn start(env: &Env, name: &str, drive_bytes: u64) -> Result<Engine, String> {
        let dir = env.results.join("work").join(&env.run_id).join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data")).map_err(|e| format!("{}: {e}", dir.display()))?;
        for d in ["d1.img", "d2.img"] {
            sparse(&dir.join(d), drive_bytes)?;
        }
        let (api_port, nvme_port) = (free_port(), free_port());
        let nqn = format!("nqn.2026-09.io.storm:test-{}", name);
        let config = format!(
            "[management]\nlisten_addr = \"127.0.0.1:{api_port}\"\ndata_dir = \"{data}\"\nnode_name = \"test-{name}\"\n\
             ublk_transport = false\n\n[nvmeof]\nlisten_addr = \"127.0.0.1:{nvme_port}\"\nnqn = \"{nqn}\"\n\
             export_drives = false\n",
            data = dir.join("data").display()
        );
        std::fs::write(dir.join("stormblock.toml"), config).map_err(|e| e.to_string())?;
        let mut e = Engine {
            api: format!("http://127.0.0.1:{api_port}/api/v1"),
            dir,
            bin: env.bin.clone(),
            api_port,
            nvme_port,
            nqn,
            node: format!("test-{name}"),
            token: String::new(),
            child: None,
            http: Client::new(),
        };
        e.spawn().await?;
        for d in ["d1.img", "d2.img"] {
            let path = e.dir.join(d).display().to_string();
            e.ok("POST", "/slabs", Some(json!({ "device_path": path, "role": "data" }))).await?;
        }
        Ok(e)
    }

    async fn spawn(&mut self) -> Result<(), String> {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("engine.log"))
            .map_err(|e| e.to_string())?;
        let d = |f: &str| self.dir.join(f).display().to_string();
        let child = Command::new(&self.bin)
            .args(["--config", &d("stormblock.toml")])
            .args(["--device", &d("d1.img"), "--device", &d("d2.img")])
            .args(["--data-dir", &d("data"), "--no-iscsi"])
            .args(["--nvmeof-addr", &format!("127.0.0.1:{}", self.nvme_port), "--nvmeof-nqn", &self.nqn])
            .env("RUST_LOG", "stormblock=info")
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", self.bin.display()))?;
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(120);
        let health = format!("http://127.0.0.1:{}/api/v1/health", self.api_port);
        loop {
            if let Ok(r) = self.http.get(&health).timeout(Duration::from_secs(2)).send().await {
                if r.status().is_success() {
                    break;
                }
            }
            if let Some(Ok(Some(st))) = self.child.as_mut().map(|c| c.try_wait()) {
                return Err(format!("the engine exited ({st}); see {}", d("engine.log")));
            }
            if Instant::now() > deadline {
                return Err(format!("the engine did not answer in 120 s; see {}", d("engine.log")));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.token = std::fs::read_to_string(self.dir.join("data/api_token"))
            .map(|t| t.trim().to_string())
            .map_err(|e| format!("no minted token: {e}"))?;
        self.http = Client::builder()
            .timeout(Duration::from_secs(120))
            .bearer(Some(self.token.clone()))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.id())
    }

    /// Stop it the way a unit does (SIGTERM), and wait.
    pub async fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            if let Some(pid) = c.id() {
                // SAFETY: a signal to our own child.
                unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            }
            if tokio::time::timeout(Duration::from_secs(30), c.wait()).await.is_err() {
                let _ = c.kill().await;
            }
        }
    }

    /// The process dies without warning (SIGKILL).
    pub async fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill().await;
        }
    }

    pub async fn restart(&mut self) -> Result<(), String> {
        self.stop().await;
        self.spawn().await
    }

    /// Killed, then started again: what survives is what was flushed.
    pub async fn crash_restart(&mut self) -> Result<(), String> {
        self.kill().await;
        self.spawn().await
    }

    /// The `/v1` contract's base URL.
    pub fn v1(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.api_port)
    }

    /// An API call with the engine's token. Answers the status and the body.
    pub async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<(u16, Value), String> {
        self.call_with(&self.http, method, path, body).await
    }

    /// The same call with no token at all.
    pub async fn call_anonymous(&self, method: &str, path: &str) -> Result<u16, String> {
        let c = Client::new();
        Ok(self.call_with(&c, method, path, None).await?.0)
    }

    async fn call_with(&self, c: &Client, method: &str, path: &str, body: Option<Value>) -> Result<(u16, Value), String> {
        let url = if path.starts_with("http") { path.to_string() } else { format!("{}{path}", self.api) };
        let rb = match method {
            "GET" => c.get(&url),
            "POST" => c.post(&url),
            "PUT" => c.put(&url),
            "DELETE" => c.delete(&url),
            m => return Err(format!("method {m}")),
        };
        let rb = match body {
            Some(b) => rb.json(&b),
            None => rb,
        };
        let r = rb.send().await.map_err(|e| format!("{method} {path}: {e}"))?;
        let status = r.status().as_u16();
        let text = r.text().await.unwrap_or_default();
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::String(text))))
    }

    /// A call that must answer 2xx.
    pub async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        let (s, v) = self.call(method, path, body).await?;
        if (200..300).contains(&s) {
            Ok(v)
        } else {
            Err(format!("{method} {path} answered {s}: {v}"))
        }
    }

    /// Attach a volume over NVMe/TCP and open it with the userspace
    /// initiator: a block device the test reads and writes as a consumer
    /// would.
    pub async fn attach(&self, id: &str) -> Result<Arc<dyn BlockDevice>, String> {
        self.attach_at(&format!("/volumes/{id}/attach"), json!({ "transport": "nvme-tcp" })).await
    }

    /// A `/v1` volume, attached for this node over NVMe/TCP.
    pub async fn attach_v1(&self, id: &str) -> Result<Arc<dyn BlockDevice>, String> {
        let me = self.node.clone();
        self.attach_at(&format!("{}/volumes/{id}/attach", self.v1()), json!({ "node": me, "mode": "read_write", "transport": "nvme_tcp" }))
            .await
    }

    async fn attach_at(&self, path: &str, body: Value) -> Result<Arc<dyn BlockDevice>, String> {
        let a = self.ok("POST", path, Some(body)).await?;
        if a["transport"] != "nvme_tcp" {
            return Err(format!("attach answered {a}, not nvme_tcp"));
        }
        let addr = &a["addresses"][0];
        let uri = format!(
            "nvme-tcp://{}:{}/{}?nsid={}",
            addr["traddr"].as_str().unwrap_or("127.0.0.1"),
            addr["trsvcid"].as_u64().unwrap_or(self.nvme_port as u64),
            a["nqn"].as_str().unwrap_or(&self.nqn),
            a["nsid"].as_u64().unwrap_or(1)
        );
        stormblock::drive::open_path(&uri, false)
            .await
            .map_err(|e| format!("connect {uri}: {e}"))
    }

    pub async fn detach(&self, id: &str) -> Result<(), String> {
        self.ok("DELETE", &format!("/volumes/{id}/attach"), None).await.map(|_| ())
    }

    /// The engine's own resource use: resident KiB and open fds.
    pub fn footprint(&self) -> (u64, u64) {
        let Some(pid) = self.pid() else { return (0, 0) };
        let rss = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
            })
            .unwrap_or(0);
        let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).map(|d| d.count() as u64).unwrap_or(0);
        (rss, fds)
    }

    /// Stop the engine and delete its files — what a run leaves behind is
    /// only its results.
    pub async fn remove(mut self) {
        self.stop().await;
        // STORM_KEEP_WORK=1 keeps the engine's files (log, drives) for a look.
        if std::env::var("STORM_KEEP_WORK").ok().as_deref() != Some("1") {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

pub fn sparse(path: &std::path::Path, bytes: u64) -> Result<(), String> {
    let f = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    f.set_len(bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// A recognisable pattern for `(seed, block)`: a read from the wrong place or
/// a stale one does not match by accident.
pub fn pattern(seed: u64, len: usize) -> Vec<u8> {
    let mut b = vec![0u8; len];
    for (i, c) in b.chunks_mut(8).enumerate() {
        let v = seed.wrapping_mul(K) ^ i as u64;
        c.copy_from_slice(&v.to_le_bytes()[..c.len()]);
    }
    b
}

pub async fn write_check(dev: &Arc<dyn BlockDevice>, off: u64, seed: u64, len: usize) -> Result<(), String> {
    let data = pattern(seed, len);
    dev.write(off, &data).await.map_err(|e| format!("write @{off}: {e}"))?;
    dev.flush().await.map_err(|e| format!("flush: {e}"))?;
    read_check(dev, off, seed, len).await
}

pub async fn read_check(dev: &Arc<dyn BlockDevice>, off: u64, seed: u64, len: usize) -> Result<(), String> {
    let mut got = vec![0u8; len];
    dev.read(off, &mut got).await.map_err(|e| format!("read @{off}: {e}"))?;
    let want = pattern(seed, len);
    if got != want {
        let first = got.iter().zip(&want).position(|(a, b)| a != b).unwrap_or(0);
        let block = first / 4096 * 4096;
        let what = describe(&got[block..(block + 4096).min(len)], block);
        return Err(format!(
            "read @{off} does not hold what was written (seed {seed}): first difference at +{first}, \
             that block holds {what}"
        ));
    }
    Ok(())
}

const K: u64 = 0x9E37_79B9_7F4A_7C15;

/// K's inverse mod 2^64 (K is odd), so a block of `pattern` names its seed.
fn k_inverse() -> u64 {
    let mut x: u64 = 1;
    for _ in 0..6 {
        x = x.wrapping_mul(2u64.wrapping_sub(K.wrapping_mul(x)));
    }
    x
}

/// Whose data a block of a pattern buffer is: zeros, a seed's (and whether at
/// the right place), or neither.
fn describe(block: &[u8], at: usize) -> String {
    if block.iter().all(|&b| b == 0) {
        return "zeros".into();
    }
    let v0 = u64::from_le_bytes(block[..8].try_into().unwrap());
    let chunk = (at / 8) as u64;
    let seed = (v0 ^ chunk).wrapping_mul(k_inverse());
    let full = pattern(seed, at + block.len());
    if full[at..] == *block {
        return format!("seed {seed}'s data");
    }
    format!("other bytes ({})", block[..16].iter().map(|b| format!("{b:02x}")).collect::<String>())
}
