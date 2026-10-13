//! A block device's flow, measured from inside the machine using it (#409).
//!
//! Run against a ublk device in a guest by `ci-ublk-flow-verify.sh`, with no
//! dependency beyond libc, so the binary can be copied into a busybox guest.
//!
//! ```text
//! flow_bench rand  <dev> <threads> <secs> read|write   random 4 KiB O_DIRECT I/O in the first 64 MiB
//! flow_bench commit <dev> <threads> <secs> fsync|dsync  each thread rewrites its own 4 KiB record
//!                                                       and makes it durable: write + fdatasync, or
//!                                                       an O_DSYNC write (a FUA write to the device)
//! ```
//!
//! One line out: `BENCH <mode> <kind> threads=<n> ops=<n> per_s=<n> p50_us=<n> p99_us=<n>`.

use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

const BLOCK: usize = 4096;
const SPAN: u64 = 64 << 20;

/// A page-aligned 4 KiB buffer, as O_DIRECT needs.
struct Aligned(*mut u8);

impl Aligned {
    fn new(fill: u8) -> Aligned {
        let mut p: *mut libc::c_void = std::ptr::null_mut();
        let r = unsafe { libc::posix_memalign(&mut p, BLOCK, BLOCK) };
        assert_eq!(r, 0, "posix_memalign");
        unsafe { std::ptr::write_bytes(p as *mut u8, fill, BLOCK) };
        Aligned(p as *mut u8)
    }
}

impl Drop for Aligned {
    fn drop(&mut self) {
        unsafe { libc::free(self.0 as *mut libc::c_void) };
    }
}

fn open(dev: &str, dsync: bool) -> std::fs::File {
    let mut flags = libc::O_DIRECT;
    if dsync {
        flags |= libc::O_DSYNC;
    }
    std::fs::OpenOptions::new().read(true).write(true).custom_flags(flags).open(dev).expect("open the device")
}

/// A small xorshift: no crate needed in the guest.
fn next(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

fn worker(dev: String, mode: String, kind: String, thread: usize, secs: u64) -> Vec<u32> {
    let dsync = mode == "commit" && kind == "dsync";
    let f = open(&dev, dsync);
    let fd = f.as_raw_fd();
    let buf = Aligned::new(thread as u8 + 1);
    let mut lat = Vec::new();
    let mut seed = 0x9E37_79B9_7F4A_7C15u64 ^ (thread as u64 + 1);
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        let t = Instant::now();
        let r = match (mode.as_str(), kind.as_str()) {
            ("rand", "read") => {
                let off = (next(&mut seed) % (SPAN / BLOCK as u64)) * BLOCK as u64;
                unsafe { libc::pread(fd, buf.0 as *mut libc::c_void, BLOCK, off as libc::off_t) }
            }
            ("rand", _) => {
                let off = (next(&mut seed) % (SPAN / BLOCK as u64)) * BLOCK as u64;
                unsafe { libc::pwrite(fd, buf.0 as *const libc::c_void, BLOCK, off as libc::off_t) }
            }
            _ => {
                // Each writer its own record, a megabyte apart: unrelated
                // commits, as fastetcd's would be (fastetcd#142).
                let off = thread as u64 * (1 << 20);
                let n = unsafe { libc::pwrite(fd, buf.0 as *const libc::c_void, BLOCK, off as libc::off_t) };
                if n >= 0 && kind == "fsync" && unsafe { libc::fdatasync(fd) } != 0 {
                    -1
                } else {
                    n
                }
            }
        };
        if r < 0 {
            eprintln!("thread {thread}: {}", std::io::Error::last_os_error());
            break;
        }
        lat.push(t.elapsed().as_micros().min(u32::MAX as u128) as u32);
    }
    lat
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 6 {
        eprintln!("usage: flow_bench rand|commit <dev> <threads> <secs> read|write|fsync|dsync");
        std::process::exit(2);
    }
    let (mode, dev, threads, secs, kind) =
        (a[1].clone(), a[2].clone(), a[3].parse::<usize>().unwrap(), a[4].parse::<u64>().unwrap(), a[5].clone());
    let started = Instant::now();
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let (dev, mode, kind) = (dev.clone(), mode.clone(), kind.clone());
            std::thread::spawn(move || worker(dev, mode, kind, t, secs))
        })
        .collect();
    let mut lat: Vec<u32> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
    let took = started.elapsed().as_secs_f64();
    lat.sort_unstable();
    let pct = |p: f64| if lat.is_empty() { 0 } else { lat[((lat.len() - 1) as f64 * p) as usize] };
    println!(
        "BENCH {mode} {kind} threads={threads} ops={} per_s={:.0} p50_us={} p99_us={}",
        lat.len(),
        lat.len() as f64 / took,
        pct(0.5),
        pct(0.99)
    );
}
