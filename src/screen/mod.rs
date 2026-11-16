//! The boot and install screen on the machine's own display (#618).
//!
//! Owner, 2026-11-15: "I really want a install screen/graphical on the
//! install screen showing progress. Not a log. Log can be another function
//! key away. Make it look professional", and "an enterprise boot screen (As
//! in trek)". `stormblock screen` is a process of its own that `/init`
//! starts as soon as the kernel has a display. It draws #616's record
//! (`install_progress::record`, built from what `/init` and the engine write
//! under `/run/stormblock`: the same data the web page shows) every second
//! on the firmware's display through DRM, and answers F2 (the log, on tty2)
//! and F1 (back). With no display it does nothing and blocks nothing.
//!
//! - [`canvas`]: the shapes and the bundled typeface.
//! - [`draw`]: the screen as a function of the record.
//! - `drm` and `vt` (Linux): the display and the terminals.

pub mod canvas;
pub mod draw;
#[cfg(target_os = "linux")]
pub mod drm;
#[cfg(target_os = "linux")]
pub mod vt;

use std::path::{Path, PathBuf};

/// What `stormblock screen` is asked.
#[derive(Debug, Clone)]
pub struct Opts {
    pub run: PathBuf,
    pub card: Option<PathBuf>,
    /// Draw once into this PNG and stop (tests, CI, the docs).
    pub snapshot: Option<PathBuf>,
    pub width: u32,
    pub height: u32,
}

fn read_trim(p: &str) -> String {
    std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// The first IPv4 address of an interface that is up and not loopback.
#[cfg(target_os = "linux")]
fn address() -> String {
    let mut out = String::new();
    let mut ifa: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills the list, freed below; every pointer read is
    // one of its entries.
    unsafe {
        if libc::getifaddrs(&mut ifa) != 0 {
            return out;
        }
        let mut p = ifa;
        while !p.is_null() {
            let e = &*p;
            if !e.ifa_addr.is_null()
                && (*e.ifa_addr).sa_family as i32 == libc::AF_INET
                && e.ifa_flags & libc::IFF_LOOPBACK as u32 == 0
                && e.ifa_flags & libc::IFF_UP as u32 != 0
            {
                let sin = &*(e.ifa_addr as *const libc::sockaddr_in);
                out = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)).to_string();
                break;
            }
            p = e.ifa_next;
        }
        libc::freeifaddrs(ifa);
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn address() -> String {
    String::new()
}

/// What the screen says of the machine, read now.
pub fn info(run: &Path) -> draw::Info {
    let release = ["/sysroot/etc/os-release", "/etc/os-release"]
        .iter()
        .find_map(|p| {
            std::fs::read_to_string(p).ok().and_then(|t| {
                t.lines().find_map(|l| l.strip_prefix("VERSION_ID=").map(|v| v.trim_matches('"').to_string()))
            })
        })
        .filter(|v| !v.is_empty())
        .map(|v| format!("RELEASE {v}"))
        .unwrap_or_else(|| format!("STORMBLOCK {}", crate::version::VERSION));
    let node = read_trim("/proc/sys/kernel/hostname");
    let node = if node.is_empty() || node == "(none)" || node == "localhost" { String::new() } else { node };
    let vendor = read_trim("/sys/class/dmi/id/sys_vendor");
    let product = read_trim("/sys/class/dmi/id/product_name");
    let model = [vendor, product].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" ");
    draw::Info {
        install: run.join("install.json").exists(),
        product: "STORMCOS".into(),
        release,
        node,
        address: address(),
        model,
        service_tag: read_trim("/sys/class/dmi/id/product_serial"),
        now: crate::install_progress::now(),
    }
}

/// Draw once into a PNG.
pub fn snapshot(o: &Opts, out: &Path) -> anyhow::Result<()> {
    let rec = crate::install_progress::record(&o.run, crate::install_progress::now());
    let mut c = canvas::Canvas::new(o.width, o.height, draw::BG);
    draw::render(&mut c, &rec, &info(&o.run));
    std::fs::write(out, c.png())?;
    println!("screen: {}x{} written to {}", o.width, o.height, out.display());
    Ok(())
}

#[cfg(target_os = "linux")]
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(target_os = "linux")]
extern "C" fn on_term(_: libc::c_int) {
    STOP.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// The screen, until SIGTERM. No display: said once, and nothing else.
#[cfg(target_os = "linux")]
pub fn run(o: &Opts) -> anyhow::Result<()> {
    use std::sync::atomic::Ordering;
    if let Some(out) = &o.snapshot {
        return snapshot(o, out);
    }
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
    }
    let display = match drm::Display::open(o.card.as_deref()) {
        Ok(Some(d)) => d,
        Ok(None) => {
            println!("screen: no display connected: nothing drawn (#618)");
            return Ok(());
        }
        Err(e) => {
            println!("screen: no display ({e}): nothing drawn (#618)");
            return Ok(());
        }
    };
    println!("screen: drawing on {} (#618)", display.said);
    let mut tty1 = vt::Term::open(1).ok();
    let mut tty2 = vt::Term::open(2).ok();
    if let Some(t) = &tty1 {
        t.activate(1);
        t.graphics(true);
    }
    let _ = display.acquire();
    let mut c = canvas::Canvas::new(display.width, display.height, draw::BG);
    let mut log = vt::Follow::default();
    let mut showing_log = false;
    let mut last_draw = std::time::Instant::now() - std::time::Duration::from_secs(10);
    let mut inf = info(&o.run);
    let mut last_info = std::time::Instant::now();
    let init_log = o.run.join("init.log");
    while !STOP.load(Ordering::SeqCst) {
        let mut keys = Vec::new();
        for t in [&mut tty1, &mut tty2].into_iter().flatten() {
            keys.extend(t.keys());
        }
        for k in keys {
            match k {
                vt::KEY_F2 if !showing_log => {
                    showing_log = true;
                    display.release();
                    if let Some(t) = &tty1 {
                        t.graphics(false);
                    }
                    if let Some(t) = &tty2 {
                        t.activate(2);
                    }
                }
                vt::KEY_F1 if showing_log => {
                    showing_log = false;
                    if let Some(t) = &tty1 {
                        t.activate(1);
                        t.graphics(true);
                    }
                    let _ = display.acquire();
                    last_draw = std::time::Instant::now() - std::time::Duration::from_secs(10);
                }
                _ => {}
            }
        }
        // The log on tty2, as it grows.
        let new = log.new_bytes(&init_log);
        if !new.is_empty() {
            if let Some(t) = &mut tty2 {
                t.write(&new);
            }
        }
        if !showing_log && last_draw.elapsed() >= std::time::Duration::from_secs(1) {
            if last_info.elapsed() >= std::time::Duration::from_secs(5) {
                inf = info(&o.run);
                last_info = std::time::Instant::now();
            }
            inf.now = crate::install_progress::now();
            let rec = crate::install_progress::record(&o.run, inf.now);
            draw::render(&mut c, &rec, &inf);
            display.present(&c.px);
            last_draw = std::time::Instant::now();
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // Let the display go: the console's own framebuffer comes back.
    display.release();
    drop(tty2);
    drop(tty1);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn run(o: &Opts) -> anyhow::Result<()> {
    match &o.snapshot {
        Some(out) => snapshot(o, out),
        None => anyhow::bail!("screen: a display needs Linux (DRM)"),
    }
}
