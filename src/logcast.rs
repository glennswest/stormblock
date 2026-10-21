//! The install path's log over the network (#477).
//!
//! A BMC's serial-over-LAN that dies mid-install (server8, #476) took the
//! installer's output with it: stormpump and stormd put their lines on the
//! stormcast group, but nothing in the initramfs did. `stormblock logcast`
//! runs from /init's first lines and follows what the initramfs prints:
//! /init's own lines (`init.log`, written by the console fan-out) and the
//! engine's (`engine.log`, every line, not the console's collapsed view).
//!
//! - **When**: every line is kept from the start. Nothing is sent until a
//!   destination is reachable (a route to it) and the node has a name, or
//!   [`NAME_WAIT`] passed with an address and none; then the backlog goes
//!   first, in order, each line with the time it was read. At most
//!   [`KEEP_LINES`] lines and [`KEEP_BYTES`] bytes wait; past that the oldest
//!   go, and the destination is told how many.
//! - **Where**: the stormcast group ([`stormcast::DEFAULT_GROUP`]) always,
//!   and a unicast collector when one is named, because multicast does not
//!   leave the subnet: `stormpump.syslog=<host[:port]>` on the kernel command
//!   line (the key stormpump reads), else a top-level `syslog:` in the
//!   install-config the boot media or forge handed down (as stormbootx reads
//!   it, stormbootx#162). Port 514 when bare. `off` sends nothing at all.
//!   Each destination has its own place in the backlog, so a collector named
//!   late (forge's install-config arrives with the claim) still gets the boot
//!   from its first line.
//! - **HOSTNAME** is the kernel's hostname when the line is sent (/init sets
//!   it from DHCP, DNS or `storm-<mac>`, #238); TIMESTAMP the line's own
//!   time, or `-` while the clock reads before 2020 (stormcast's rule).
//!
//! Best effort by construction: nothing here waits on a collector, fails a
//! boot, or writes to the console (its own few lines go to a status file,
//! and over the network).

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Lines kept while nothing can be sent.
pub const KEEP_LINES: usize = 100_000;
/// Bytes kept while nothing can be sent.
pub const KEEP_BYTES: usize = 16 << 20;
/// How long a reachable destination waits for the node's name.
pub const NAME_WAIT: Duration = Duration::from_secs(15);
/// Frames sent before a short pause, so a backlog does not overrun the
/// socket's buffer (the emitter never blocks; a full buffer drops).
const BATCH: usize = 64;
/// The APP-NAME of logcast's own lines.
pub const SELF_TAG: &str = "stormblock-logcast";

/// Where the network log goes, as configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `off`: nothing goes out, not even to the group.
    Off,
    /// Nothing named: the group alone.
    None,
    /// A collector, `host:port` (port 514 added when bare).
    At(String),
}

/// A target as written: `off`, empty (none), or `host[:port]`.
pub fn parse_target(v: &str) -> Target {
    let v = v.trim();
    if v.is_empty() {
        return Target::None;
    }
    if v.eq_ignore_ascii_case("off") {
        return Target::Off;
    }
    // `host:port`, `[v6]:port` or a bare host; a bare v6 address is many
    // colons and no brackets.
    let has_port = if v.starts_with('[') {
        v.contains("]:")
    } else {
        v.matches(':').count() == 1
    };
    if has_port {
        Target::At(v.to_string())
    } else if v.contains(':') {
        Target::At(format!("[{v}]:514"))
    } else {
        Target::At(format!("{v}:514"))
    }
}

/// `stormpump.syslog=` on a kernel command line.
pub fn cmdline_target(cmdline: &str) -> Option<Target> {
    cmdline.split_whitespace().find_map(|t| t.strip_prefix("stormpump.syslog=")).map(parse_target)
}

/// The value of a top-level `syslog:` key in install-config.yaml, quotes
/// taken off and a trailing comment dropped (stormbootx's `yaml_syslog`).
/// Only a key at column 0: a `syslog:` under a host is not the install's.
pub fn yaml_syslog(text: &str) -> Option<&str> {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("syslog:") else { continue };
        let mut v = rest.trim();
        if let Some(i) = v.find(" #") {
            v = v[..i].trim_end();
        }
        let quoted = |q: char| v.len() >= 2 && v.starts_with(q) && v.ends_with(q);
        if quoted('"') || quoted('\'') {
            v = &v[1..v.len() - 1];
        }
        return (!v.is_empty()).then_some(v);
    }
    None
}

/// A file followed from its start, a line at a time.
struct Follow {
    path: PathBuf,
    tag: String,
    file: Option<std::fs::File>,
    offset: u64,
    partial: Vec<u8>,
}

impl Follow {
    fn new(path: PathBuf, tag: String) -> Follow {
        Follow { path, tag, file: None, offset: 0, partial: Vec::new() }
    }

    /// Whatever whole lines were added since the last read. A file that was
    /// truncated (/init empties the engine's log before each start) is read
    /// again from its start. `last`: the partial line at the end counts too.
    fn read(&mut self, last: bool) -> Vec<String> {
        if self.file.is_none() {
            // Opened once: after switch_root the path is gone, the open file
            // is not.
            self.file = std::fs::File::open(&self.path).ok();
            self.offset = 0;
        }
        let mut out = Vec::new();
        let Some(f) = self.file.as_mut() else { return out };
        if let Ok(m) = f.metadata() {
            if m.len() < self.offset {
                self.offset = 0;
                self.partial.clear();
            }
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return out;
        }
        let mut buf = Vec::new();
        if let Ok(n) = f.take(4 << 20).read_to_end(&mut buf) {
            self.offset += n as u64;
        }
        self.partial.extend_from_slice(&buf);
        while let Some(i) = self.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=i).collect();
            out.push(String::from_utf8_lossy(&line[..line.len() - 1]).trim_end_matches('\r').to_string());
        }
        if last && !self.partial.is_empty() {
            out.push(String::from_utf8_lossy(&std::mem::take(&mut self.partial)).to_string());
        }
        out
    }
}

/// A line waiting to be sent.
#[derive(Debug, Clone)]
struct Line {
    ts: String,
    tag: String,
    msg: String,
}

/// One place lines go.
struct Dest {
    /// What the status says: `group 239.255.42.1:5514`, `stormcentral:5514`.
    label: String,
    /// The name to look up, for a collector given by name.
    name: Option<String>,
    addr: Option<SocketAddr>,
    looked_up: Option<Instant>,
    emitter: Option<(String, stormcast::Emitter)>,
    /// The next line's sequence number.
    next: u64,
    started: bool,
    sent: u64,
}

impl Dest {
    fn new(spec: &str, label: String) -> Dest {
        let addr = spec.parse::<SocketAddr>().ok();
        let name = addr.is_none().then(|| spec.to_string());
        Dest { label, name, addr, looked_up: None, emitter: None, next: 0, started: false, sent: 0 }
    }

    /// Look a named collector up, at most every 5 s until it answers.
    fn resolve(&mut self) {
        if self.addr.is_some() {
            return;
        }
        let Some(name) = self.name.as_ref() else { return };
        if self.looked_up.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        self.looked_up = Some(Instant::now());
        self.addr = name.to_socket_addrs().ok().and_then(|it| {
            let all: Vec<SocketAddr> = it.collect();
            all.iter().find(|a| a.is_ipv4()).or(all.first()).copied()
        });
    }
}

/// Whether this machine has a route to `addr` (a UDP connect sends nothing).
pub fn routes(addr: SocketAddr) -> bool {
    let bind: SocketAddr = if addr.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
    let Ok(sock) = UdpSocket::bind(bind) else { return false };
    if sock.connect(addr).is_err() {
        return false;
    }
    match sock.local_addr().map(|a| a.ip()) {
        Ok(ip) if ip.is_unspecified() => false,
        Ok(ip) if ip.is_loopback() => addr.ip().is_loopback(),
        Ok(_) => true,
        Err(_) => false,
    }
}

/// The kernel's hostname, when /init has set one.
fn kernel_hostname(path: &Path) -> Option<String> {
    let h = std::fs::read_to_string(path).ok()?;
    let h = h.trim();
    (!h.is_empty() && h != "(none)" && h != "localhost").then(|| h.to_string())
}

/// What `stormblock logcast` is told.
#[derive(Debug, Clone)]
pub struct Options {
    /// Files to follow, each with the APP-NAME its lines carry.
    pub follow: Vec<(PathBuf, String)>,
    /// The kernel command line, read once (gone after switch_root).
    pub cmdline: PathBuf,
    /// install-config files to look in for `syslog:`, in order, until one
    /// names a target.
    pub install_configs: Vec<PathBuf>,
    /// The group (`off`: none; tests).
    pub group: String,
    /// The kernel's hostname.
    pub hostname: PathBuf,
    /// Where logcast's own few lines go besides the network.
    pub status: Option<PathBuf>,
    /// How long a stop waits for a destination to become reachable.
    pub drain: Duration,
    /// How often the files are read.
    pub tick: Duration,
    /// The APP-NAME of a file whose lines the console also shows, and so
    /// /init's own log holds a copy of (the engine follower writes through
    /// /init's fan-out): those copies, and the follower's `... and N more
    /// line(s) like that` notes, are not sent again.
    pub console_copy: Option<String>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            follow: Vec::new(),
            cmdline: "/proc/cmdline".into(),
            install_configs: vec![
                "/run/stormblock/install-config.yaml".into(),
                "/run/stormblock/install-config.forge.yaml".into(),
            ],
            group: stormcast::DEFAULT_GROUP.to_string(),
            hostname: "/proc/sys/kernel/hostname".into(),
            status: Some("/run/stormblock/logcast.status".into()),
            drain: Duration::from_secs(3),
            tick: Duration::from_millis(250),
            console_copy: None,
        }
    }
}

/// The follower: lines in, frames out.
pub struct Logcast {
    opts: Options,
    files: Vec<Follow>,
    lines: VecDeque<Line>,
    bytes: usize,
    /// Sequence number of `lines[0]`.
    base: u64,
    dests: Vec<Dest>,
    /// The collector, once settled (cmdline, an install-config, or none).
    target: Option<Target>,
    cmdline: String,
    host: Option<String>,
    reachable_since: Option<Instant>,
    /// Lines of the `console_copy` file whose copy in another file is still
    /// to come (oldest first, at most [`COPIES_KEPT`]), and how many of each.
    copies: VecDeque<String>,
    copies_by: std::collections::HashMap<String, usize>,
    /// Stand-in for [`routes`] (tests).
    pub route: fn(SocketAddr) -> bool,
}

/// Copies waited for at most: the console shows a flood as three lines and
/// a count, so most of a flood's lines never come back.
const COPIES_KEPT: usize = 4096;

/// The engine follower's note for a collapsed run (the console's, not the
/// engine's).
fn follower_note(line: &str) -> bool {
    line.starts_with("  ... and ") && line.contains(" more line(s) like that")
}

impl Logcast {
    pub fn new(opts: Options) -> Logcast {
        let cmdline = std::fs::read_to_string(&opts.cmdline).unwrap_or_default();
        let files = opts.follow.iter().map(|(p, t)| Follow::new(p.clone(), t.clone())).collect();
        let mut lc = Logcast {
            opts,
            files,
            lines: VecDeque::new(),
            bytes: 0,
            base: 0,
            dests: Vec::new(),
            target: None,
            cmdline,
            host: None,
            reachable_since: None,
            copies: VecDeque::new(),
            copies_by: std::collections::HashMap::new(),
            route: routes,
        };
        let off = matches!(cmdline_target(&lc.cmdline), Some(Target::Off));
        if !off && !lc.opts.group.eq_ignore_ascii_case("off") {
            let g = lc.opts.group.clone();
            lc.dests.push(Dest::new(&g, format!("group {g}")));
        }
        lc
    }

    /// Lines dropped from the front so far.
    fn note(&mut self, msg: String) {
        if let Some(p) = self.opts.status.as_ref() {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{msg}");
            }
        }
        self.push(SELF_TAG.to_string(), msg);
    }

    fn push(&mut self, tag: String, msg: String) {
        self.bytes += msg.len() + tag.len();
        self.lines.push_back(Line { ts: stormcast::now_rfc3339(), tag, msg });
        while self.lines.len() > KEEP_LINES || self.bytes > KEEP_BYTES {
            if let Some(l) = self.lines.pop_front() {
                self.bytes -= l.msg.len() + l.tag.len();
                self.base += 1;
            } else {
                break;
            }
        }
    }

    /// Settle the collector: the command line's, else the first
    /// install-config naming one (looked at again each pass until one does;
    /// forge's arrives with the claim).
    fn settle_target(&mut self) {
        if self.target.is_some() {
            return;
        }
        let mut found = cmdline_target(&self.cmdline).map(|t| (t, "the kernel command line".to_string()));
        if found.is_none() {
            for p in &self.opts.install_configs {
                // Its content is never printed: an install-config carries
                // secrets (#275). Only the one key is read.
                if let Ok(text) = std::fs::read_to_string(p) {
                    if let Some(v) = yaml_syslog(&text) {
                        found = Some((parse_target(v), format!("syslog in {}", p.display())));
                        break;
                    }
                }
            }
        }
        let Some((t, from)) = found else { return };
        match &t {
            Target::Off => {
                self.dests.clear();
                self.note(format!("logcast: nothing sent over the network (syslog off, {from})"));
            }
            Target::None => {}
            Target::At(spec) => {
                let group = self.opts.group.clone();
                let same = spec.parse::<SocketAddr>().ok().zip(group.parse::<SocketAddr>().ok()).is_some_and(|(a, b)| a == b);
                if !same {
                    self.dests.push(Dest::new(spec, format!("{spec} ({from})")));
                }
            }
        }
        self.target = Some(t);
    }

    /// Read what the files added; `last`: the end of each counts as a line.
    pub fn read(&mut self, last: bool) {
        let copied = self.opts.console_copy.clone();
        // The copied file first: its lines are written before their copy.
        let mut order: Vec<usize> = (0..self.files.len()).collect();
        order.sort_by_key(|i| Some(&self.files[*i].tag) != copied.as_ref());
        let mut new = Vec::new();
        for i in order {
            let f = &mut self.files[i];
            for l in f.read(last) {
                new.push((f.tag.clone(), l));
            }
        }
        for (tag, l) in new {
            if let Some(c) = copied.as_ref() {
                if tag == *c {
                    self.copies.push_back(l.clone());
                    *self.copies_by.entry(l.clone()).or_default() += 1;
                    while self.copies.len() > COPIES_KEPT {
                        if let Some(old) = self.copies.pop_front() {
                            self.forget_copy(&old);
                        }
                    }
                } else if follower_note(&l) {
                    continue;
                } else if self.copies_by.get(&l).is_some_and(|n| *n > 0) {
                    // The console's copy of a line already kept.
                    self.forget_copy(&l);
                    if let Some(i) = self.copies.iter().position(|c| *c == l) {
                        self.copies.remove(i);
                    }
                    continue;
                }
            }
            self.push(tag, l);
        }
    }

    fn forget_copy(&mut self, l: &str) {
        if let Some(n) = self.copies_by.get_mut(l) {
            *n -= 1;
            if *n == 0 {
                self.copies_by.remove(l);
            }
        }
    }

    /// Send what each reachable destination has not had.
    pub fn send(&mut self) {
        self.settle_target();
        if let Some(h) = kernel_hostname(&self.opts.hostname) {
            self.host = Some(h);
        }
        let route = self.route;
        let mut any = false;
        for d in &mut self.dests {
            d.resolve();
            if d.addr.is_some_and(route) {
                any = true;
            }
        }
        if !any {
            return;
        }
        let since = *self.reachable_since.get_or_insert_with(Instant::now);
        let host = match self.host.clone() {
            Some(h) => h,
            None if since.elapsed() >= NAME_WAIT => "-".to_string(),
            None => return,
        };
        let mut notes = Vec::new();
        for d in &mut self.dests {
            let Some(addr) = d.addr else { continue };
            if !route(addr) {
                continue;
            }
            if d.emitter.as_ref().is_none_or(|(h, _)| *h != host) {
                d.emitter = stormcast::Emitter::new(addr, host.clone()).map(|e| (host.clone(), e));
            }
            let Some((_, e)) = d.emitter.as_ref() else { continue };
            if !d.started {
                d.started = true;
                notes.push(format!("logcast: the boot's log to {} (APP-NAMEs {})", d.label, self.files.iter().map(|f| f.tag.as_str()).collect::<Vec<_>>().join(", ")));
            }
            if d.next < self.base {
                let lost = self.base - d.next;
                e.send(SELF_TAG, stormcast::Severity::Warning, &format!("logcast: {lost} line(s) dropped before {} could be reached (kept at most {KEEP_LINES} lines, {} MiB)", d.label, KEEP_BYTES >> 20));
                d.next = self.base;
            }
            let start = (d.next - self.base) as usize;
            for (i, l) in self.lines.iter().skip(start).enumerate() {
                e.send_at(&l.ts, &l.tag, stormcast::Severity::of(&l.msg), &l.msg);
                d.sent += 1;
                if i % BATCH == BATCH - 1 {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            d.next = self.base + self.lines.len() as u64;
        }
        for n in notes {
            self.note(n);
        }
    }

    /// Whether every destination that could be reached has every line.
    fn drained(&self) -> bool {
        let end = self.base + self.lines.len() as u64;
        self.dests.iter().all(|d| d.next >= end)
    }

    /// Frames sent to each destination, by label (tests, the status).
    pub fn sent(&self) -> Vec<(String, u64)> {
        self.dests.iter().map(|d| (d.label.clone(), d.sent)).collect()
    }

    /// Run until `stop` is set, then read to the end and send, waiting at
    /// most [`Options::drain`] for a destination to become reachable.
    pub fn run(&mut self, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            self.read(false);
            self.send();
            std::thread::sleep(self.opts.tick);
        }
        self.read(true);
        let until = Instant::now() + self.opts.drain;
        loop {
            self.send();
            if self.drained() || Instant::now() >= until {
                break;
            }
            std::thread::sleep(self.opts.tick.min(Duration::from_millis(200)));
        }
        let sent = self.sent().into_iter().map(|(l, n)| format!("{n} to {l}")).collect::<Vec<_>>().join(", ");
        if let Some(p) = self.opts.status.as_ref() {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "logcast: stopped ({})", if sent.is_empty() { "nothing sent".to_string() } else { sent });
            }
        }
    }
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// `stormblock logcast`: follow, buffer, send, until SIGTERM or SIGINT.
pub fn main(opts: Options) {
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, on_term as extern "C" fn(libc::c_int) as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as extern "C" fn(libc::c_int) as libc::sighandler_t);
        // Its stdout and stderr may be a console that goes away.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    Logcast::new(opts).run(&STOP);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_read_as_stormpump_and_stormbootx_read_them() {
        assert_eq!(parse_target("off"), Target::Off);
        assert_eq!(parse_target(""), Target::None);
        assert_eq!(parse_target("stormcentral.g8.lo"), Target::At("stormcentral.g8.lo:514".into()));
        assert_eq!(parse_target("192.168.8.10:5514"), Target::At("192.168.8.10:5514".into()));
        assert_eq!(parse_target("fd00::1"), Target::At("[fd00::1]:514".into()));
        assert_eq!(parse_target("[fd00::1]:5514"), Target::At("[fd00::1]:5514".into()));
        assert_eq!(cmdline_target("quiet stormpump.syslog=10.0.0.1:5514 x"), Some(Target::At("10.0.0.1:5514".into())));
        assert_eq!(cmdline_target("quiet"), None);
        assert_eq!(yaml_syslog("apiVersion: v1\nsyslog: \"stormcentral.g8.lo:5514\" # the collector\n"), Some("stormcentral.g8.lo:5514"));
        assert_eq!(yaml_syslog("hosts:\n  syslog: nested\n"), None);
        assert_eq!(yaml_syslog("syslog: ''\n"), None);
    }

    /// A UDP listener on loopback, standing in for a collector.
    fn collector() -> (UdpSocket, SocketAddr) {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let a = s.local_addr().unwrap();
        (s, a)
    }

    fn frames(s: &UdpSocket) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 16 << 10];
        while let Ok(n) = s.recv(&mut buf) {
            out.push(String::from_utf8_lossy(&buf[..n]).to_string());
        }
        out
    }

    static NET: AtomicBool = AtomicBool::new(false);
    fn net(_: SocketAddr) -> bool {
        NET.load(Ordering::Relaxed)
    }

    /// #477: every line /init and the engine print is kept from the first,
    /// sent in order once the collector can be reached and the node has a
    /// name, with its APP-NAME and the node's name; a truncated engine log is
    /// read from its start; a stop sends the last unterminated line.
    #[test]
    fn the_boot_is_kept_until_the_network_is_up_then_sent_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let init = dir.path().join("init.log");
        let engine = dir.path().join("engine.log");
        let host = dir.path().join("hostname");
        let cmdline = dir.path().join("cmdline");
        let (col, at) = collector();
        std::fs::write(&cmdline, format!("console=ttyS0 stormpump.syslog={at}\n")).unwrap();
        std::fs::write(&init, "early line one\nERROR: early two\n").unwrap();
        let mut lc = Logcast::new(Options {
            follow: vec![(init.clone(), "stormblock-init".into()), (engine.clone(), "stormblock".into())],
            cmdline,
            install_configs: vec![],
            group: "off".into(),
            hostname: host.clone(),
            status: Some(dir.path().join("status")),
            drain: Duration::from_millis(300),
            tick: Duration::from_millis(10),
            console_copy: None,
        });
        lc.route = net;
        NET.store(false, Ordering::Relaxed);
        lc.read(false);
        lc.send();
        // No network: nothing out.
        assert!(frames(&col).is_empty());
        std::fs::write(&engine, "Flow-over: from the engine\n").unwrap();
        lc.read(false);
        // The network, but no name yet: still nothing.
        NET.store(true, Ordering::Relaxed);
        lc.send();
        assert!(frames(&col).is_empty(), "waits for the node's name");
        std::fs::write(&host, "server8\n").unwrap();
        lc.send();
        let got = frames(&col);
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(got[0].starts_with("<134>1 "), "an ordinary line is info: {}", got[0]);
        assert!(got[0].contains(" server8 stormblock-init - - - early line one"), "{}", got[0]);
        assert!(got[1].starts_with("<131>1 "), "an ERROR line is severity 3: {}", got[1]);
        assert!(got[2].contains(" server8 stormblock - - - Flow-over: from the engine"), "{}", got[2]);
        // The engine's log emptied for a restart, then a partial last line.
        std::fs::write(&engine, "").unwrap();
        lc.read(false);
        std::fs::write(&engine, "restarted\nno newline").unwrap();
        let stop = AtomicBool::new(true);
        lc.run(&stop);
        let got = frames(&col);
        let msgs: Vec<String> = got.iter().map(|f| f.split(" - - - ").nth(1).unwrap().to_string()).collect();
        assert!(msgs[0].starts_with("logcast: the boot's log to 127.0.0.1:"), "{msgs:?}");
        assert!(got[0].contains(" server8 stormblock-logcast "), "{}", got[0]);
        assert_eq!(&msgs[1..], &["restarted", "no newline"]);
        let status = std::fs::read_to_string(dir.path().join("status")).unwrap();
        assert!(status.contains("logcast: stopped (6 to 127.0.0.1:"), "{status}");
    }

    /// #477: a collector named late (forge's install-config comes with the
    /// claim) gets the boot from its first line; `off` sends nothing.
    #[test]
    fn a_collector_named_late_gets_the_whole_boot_and_off_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let init = dir.path().join("init.log");
        let host = dir.path().join("hostname");
        let cfg = dir.path().join("install-config.forge.yaml");
        std::fs::write(&host, "blade7").unwrap();
        std::fs::write(&init, "a\nb\nc\n").unwrap();
        let (col, at) = collector();
        let opts = Options {
            follow: vec![(init.clone(), "stormblock-init".into())],
            cmdline: dir.path().join("none"),
            install_configs: vec![dir.path().join("install-config.yaml"), cfg.clone()],
            group: "off".into(),
            hostname: host,
            status: None,
            drain: Duration::from_millis(100),
            tick: Duration::from_millis(10),
            console_copy: None,
        };
        let mut lc = Logcast::new(opts.clone());
        lc.route = routes;
        lc.read(false);
        lc.send();
        assert!(frames(&col).is_empty(), "nothing named yet");
        std::fs::write(&cfg, format!("pullSecret: secret\nsyslog: {at}\n")).unwrap();
        lc.send();
        let got = frames(&col);
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(got[0].ends_with(" a") && got[2].ends_with(" c"));
        assert!(!got.iter().any(|f| f.contains("pullSecret")), "the install-config's content is never sent");

        std::fs::write(&cfg, "syslog: off\n").unwrap();
        let mut lc = Logcast::new(Options { group: stormcast::DEFAULT_GROUP.into(), ..opts });
        lc.route = routes;
        lc.read(false);
        lc.send();
        assert!(lc.sent().is_empty(), "off: no destination at all, the group included");
    }

    /// #477: the engine's lines reach /init's log too (the follower writes
    /// through the fan-out), collapsed: they are sent once, from the
    /// engine's log, whole; /init's own lines all go.
    #[test]
    fn the_consoles_copy_of_the_engines_lines_is_not_sent_again() {
        let dir = tempfile::tempdir().unwrap();
        let init = dir.path().join("init.log");
        let engine = dir.path().join("engine.log");
        let host = dir.path().join("hostname");
        std::fs::write(&host, "blade3").unwrap();
        let (col, at) = collector();
        let cmdline = dir.path().join("cmdline");
        std::fs::write(&cmdline, format!("stormpump.syslog={at}")).unwrap();
        // The engine floods; the console showed three and a count. /init
        // printed one line of its own, the same text as none of them.
        let flood: Vec<String> = (1..=10).map(|i| format!("slab {i} not attached")).collect();
        std::fs::write(&engine, flood.join("\n") + "\nengine: done\n").unwrap();
        std::fs::write(
            &init,
            "INSTALL: over sda\nslab 1 not attached\nslab 2 not attached\nslab 3 not attached\n  ... and 7 more line(s) like that (all of them in /run/stormblock/engine.log)\nengine: done\nslab 1 not attached\n",
        )
        .unwrap();
        let mut lc = Logcast::new(Options {
            follow: vec![(init, "stormblock-init".into()), (engine, "stormblock".into())],
            cmdline,
            install_configs: vec![],
            group: "off".into(),
            hostname: host,
            status: None,
            drain: Duration::from_millis(100),
            tick: Duration::from_millis(10),
            console_copy: Some("stormblock".into()),
        });
        lc.route = routes;
        lc.read(false);
        lc.send();
        let got: Vec<(String, String)> = frames(&col)
            .iter()
            .map(|f| {
                let mut it = f.splitn(8, ' ');
                let tag = it.nth(3).unwrap().to_string();
                (tag, f.split(" - - - ").nth(1).unwrap().to_string())
            })
            .collect();
        let engine_lines: Vec<&str> = got.iter().filter(|(t, _)| t == "stormblock").map(|(_, m)| m.as_str()).collect();
        let init_lines: Vec<&str> = got.iter().filter(|(t, _)| t == "stormblock-init").map(|(_, m)| m.as_str()).collect();
        assert_eq!(engine_lines.len(), 11, "the engine's log whole: {engine_lines:?}");
        // A second "slab 1 not attached" in /init's log has no engine line
        // left to be a copy of: it is /init's.
        assert_eq!(init_lines, vec!["INSTALL: over sda", "slab 1 not attached"]);
    }
}
