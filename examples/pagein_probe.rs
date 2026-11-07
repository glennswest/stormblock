//! What a boot's page-in costs (#484), for `ci-boot-pagein-verify.sh`.
//!
//! ```text
//! pagein_probe exec <program> [args…]       run it once: wall ms, major faults
//! pagein_probe direct <device> <reads>      O_DIRECT 128 KiB reads: p50, p90, max
//! pagein_probe hammer <device> <seconds>    O_DIRECT 128 KiB reads, as a unit reading
//! ```

use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

const IO: usize = 128 * 1024;

struct Aligned(*mut u8);

fn aligned() -> Aligned {
    let mut p = std::ptr::null_mut();
    // SAFETY: a fresh allocation for this process's life.
    assert_eq!(unsafe { libc::posix_memalign(&mut p, 4096, IO) }, 0);
    Aligned(p.cast())
}

/// A deterministic walk: runs compare the same offsets, per process seed.
fn next(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

fn open_direct(path: &str) -> (std::fs::File, u64) {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .unwrap_or_else(|e| panic!("open {path}: {e}"));
    let mut size = 0u64;
    // SAFETY: BLKGETSIZE64 writes one u64.
    let r = unsafe { libc::ioctl(f.as_raw_fd(), 0x8008_1272u32 as _, &mut size) };
    if r != 0 {
        size = f.metadata().unwrap().len();
    }
    (f, size)
}

fn read_at(f: &std::fs::File, b: &Aligned, size: u64, x: &mut u64) {
    let span = size.saturating_sub(IO as u64).max(1);
    let off = (next(x) % span) & !(IO as u64 - 1);
    // SAFETY: the buffer is IO bytes, for this call only.
    let r = unsafe { libc::pread(f.as_raw_fd(), b.0.cast(), IO, off as i64) };
    assert!(r >= 0, "pread: {}", std::io::Error::last_os_error());
}

fn exec(args: &[String]) {
    let prog = CString::new(args[0].as_str()).unwrap();
    let cargs: Vec<CString> = args.iter().map(|a| CString::new(a.as_str()).unwrap()).collect();
    let mut argv: Vec<*const libc::c_char> = cargs.iter().map(|c| c.as_ptr()).collect();
    argv.push(std::ptr::null());
    let t0 = Instant::now();
    // SAFETY: the child only redirects its output and execs.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        // SAFETY: as above; a failed exec exits.
        unsafe {
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
            libc::dup2(null, 1);
            libc::dup2(null, 2);
            libc::execv(prog.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        }
    }
    let mut status = 0;
    // SAFETY: all-zero is a valid rusage.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: both out-pointers live across the call.
    assert_eq!(unsafe { libc::wait4(pid, &mut status, 0, &mut ru) }, pid);
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    let f = ru.ru_majflt;
    let per = if f > 0 { ms / f as f64 } else { 0.0 };
    println!("wall_ms {ms:.1} major_faults {f} ms_per_fault {per:.1} status {status}");
}

fn direct(path: &str, reads: usize) {
    let (f, size) = open_direct(path);
    let b = aligned();
    let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ std::process::id() as u64;
    let mut t: Vec<f64> = (0..reads)
        .map(|_| {
            let t0 = Instant::now();
            read_at(&f, &b, size, &mut x);
            t0.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    t.sort_by(f64::total_cmp);
    let q = |p: f64| t[((t.len() - 1) as f64 * p) as usize];
    println!("reads {reads} p50_ms {:.1} p90_ms {:.1} max_ms {:.1}", q(0.5), q(0.9), q(1.0));
}

fn hammer(path: &str, secs: u64) {
    let (f, size) = open_direct(path);
    let b = aligned();
    let mut x = 0xD1B5_4A32_D192_ED03u64 ^ std::process::id() as u64;
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        read_at(&f, &b, size, &mut x);
    }
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    match a.first().map(String::as_str) {
        Some("exec") if a.len() > 1 => exec(&a[1..]),
        Some("direct") if a.len() == 3 => direct(&a[1], a[2].parse().unwrap()),
        Some("hammer") if a.len() == 3 => hammer(&a[1], a[2].parse().unwrap()),
        _ => {
            eprintln!("usage: pagein_probe exec <program> [args…] | direct <device> <reads> | hammer <device> <seconds>");
            std::process::exit(2);
        }
    }
}
