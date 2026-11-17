//! The power button, while the initramfs runs (#635).
//!
//! forge1 was cut hard seven times on 2026-11-16: each install that failed
//! dropped to the rescue shell, stormcentral pressed the button (the BMC's
//! soft off, an ACPI power-button event), nothing in the initramfs was
//! listening, and after 120 s the power was cut, with SAS drives mid-I/O.
//!
//! `stormblock power-button` watches the input device the kernel names
//! "Power Button" (the ACPI button: what a BMC's soft off, a person's press
//! and QEMU's `system_powerdown` all are). On a press it says so on the
//! console, stops the storage engine and the screen (SIGTERM, then waits for
//! them), syncs and powers off. `/init` starts it early and stops it at
//! switch_root, where the node's own init takes the button.

use std::path::{Path, PathBuf};

/// `EV_KEY` and `KEY_POWER` from `<linux/input-event-codes.h>`.
pub const EV_KEY: u16 = 1;
pub const KEY_POWER: u16 = 116;

/// One `struct input_event` on a 64-bit kernel: a timeval (16), type, code
/// (u16 each) and value (i32): 24 bytes.
pub const EVENT_SIZE: usize = 24;

/// (type, code, value) of one event.
pub fn parse_event(b: &[u8]) -> Option<(u16, u16, i32)> {
    if b.len() < EVENT_SIZE {
        return None;
    }
    let t = u16::from_ne_bytes([b[16], b[17]]);
    let c = u16::from_ne_bytes([b[18], b[19]]);
    let v = i32::from_ne_bytes([b[20], b[21], b[22], b[23]]);
    Some((t, c, v))
}

/// A press of the power button (a key down, not its release or repeat).
pub fn is_press(e: (u16, u16, i32)) -> bool {
    e.0 == EV_KEY && e.1 == KEY_POWER && e.2 == 1
}

/// The power buttons under `sys_input` (`/sys/class/input`): each
/// `eventN` whose device is named like one, as `/dev/input/eventN` and
/// its name.
pub fn buttons(sys_input: &Path, dev_input: &Path) -> Vec<(PathBuf, String)> {
    let mut v: Vec<(PathBuf, String)> = std::fs::read_dir(sys_input)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_str()?.to_string();
                    if !n.starts_with("event") {
                        return None;
                    }
                    let name = std::fs::read_to_string(e.path().join("device/name")).ok()?.trim().to_string();
                    name.to_ascii_lowercase().contains("power button").then(|| (dev_input.join(&n), name))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// The processes to stop before the power goes: the storage engine
/// (`boot-local`, `adopt-ublk`) and the screen, by an argument of theirs;
/// never this process.
pub fn to_stop(proc_dir: &Path, me: u32) -> Vec<u32> {
    let mut v: Vec<u32> = std::fs::read_dir(proc_dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let pid: u32 = e.file_name().to_str()?.parse().ok()?;
                    if pid == me {
                        return None;
                    }
                    let cmd = std::fs::read(e.path().join("cmdline")).ok()?;
                    cmd.split(|b| *b == 0)
                        .any(|a| a == b"boot-local" || a == b"adopt-ublk" || a == b"screen")
                        .then_some(pid)
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// A line on the console (and stdout, the log).
fn say(line: &str) {
    use std::io::Write;
    println!("{line}");
    if let Ok(mut c) = std::fs::OpenOptions::new().write(true).open("/dev/console") {
        let _ = writeln!(c, "{line}\r");
    }
}

/// What is under way that a power-off loses, for the console (a format
/// through sg or the card leaves its drive to be formatted again).
fn under_way(run: &Path) -> Option<String> {
    let dir = run.join("storage-format");
    let rd = std::fs::read_dir(&dir).ok()?;
    let mut running = Vec::new();
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if let Some(d) = n.strip_suffix(".log") {
            if !dir.join(format!("{d}.rc")).exists() {
                running.push(d.to_string());
            }
        }
    }
    running.sort();
    (!running.is_empty()).then(|| running.join(" "))
}

/// Watch until a press, then power off. Never returns on a press.
#[cfg(target_os = "linux")]
pub fn run(run_dir: &Path) -> anyhow::Result<()> {
    use std::io::Read;
    // The button can come up a moment after /init starts us.
    let mut found = buttons(Path::new("/sys/class/input"), Path::new("/dev/input"));
    for _ in 0..30 {
        if !found.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        found = buttons(Path::new("/sys/class/input"), Path::new("/dev/input"));
    }
    if found.is_empty() {
        println!("power button: none found under /sys/class/input: a soft off cannot be acted on here (#635)");
        return Ok(());
    }
    let mut files = Vec::new();
    for (p, name) in &found {
        match std::fs::File::open(p) {
            Ok(f) => files.push(f),
            Err(e) => println!("power button: {} ({name}): {e}", p.display()),
        }
    }
    println!(
        "power button: watching {} (a press powers off cleanly) (#635)",
        found.iter().map(|(p, n)| format!("{} ({n})", p.display())).collect::<Vec<_>>().join(", ")
    );
    loop {
        use std::os::fd::AsRawFd;
        let mut fds: Vec<libc::pollfd> = files.iter().map(|f| libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
        // SAFETY: an array of pollfds this function owns, for its length.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if n < 0 {
            continue;
        }
        let mut pressed = false;
        for (i, p) in fds.iter().enumerate() {
            if p.revents & libc::POLLIN == 0 {
                continue;
            }
            let mut buf = [0u8; EVENT_SIZE * 16];
            if let Ok(k) = files[i].read(&mut buf) {
                pressed |= buf[..k].chunks(EVENT_SIZE).filter_map(parse_event).any(is_press);
            }
        }
        if pressed {
            power_off(run_dir);
        }
    }
}

/// Stop the engine and the screen, sync, power off.
#[cfg(target_os = "linux")]
fn power_off(run_dir: &Path) -> ! {
    say("power button: pressed - stopping the storage engine, then powering off (#635)");
    if let Some(f) = under_way(run_dir) {
        say(&format!("power button: a format is running on {f}: it stops here, and the drive is formatted again by the next install"));
    }
    let me = std::process::id();
    let pids = to_stop(Path::new("/proc"), me);
    for p in &pids {
        // SAFETY: a signal to a process of this boot.
        unsafe { libc::kill(*p as libc::pid_t, libc::SIGTERM) };
    }
    let start = std::time::Instant::now();
    while pids.iter().any(|p| Path::new(&format!("/proc/{p}")).exists()) && start.elapsed() < std::time::Duration::from_secs(20) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let left: Vec<u32> = pids.iter().copied().filter(|p| Path::new(&format!("/proc/{p}")).exists()).collect();
    if left.is_empty() {
        say(&format!("power button: {} process(es) stopped in {:.1}s; syncing and powering off", pids.len(), start.elapsed().as_secs_f32()));
    } else {
        say(&format!("power button: {left:?} did not stop within 20s; syncing and powering off anyway"));
    }
    // SAFETY: sync and reboot(2) as root, at the end of this boot.
    unsafe {
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
    // reboot only returns on failure.
    say(&format!("power button: power off failed: {}", std::io::Error::last_os_error()));
    std::process::exit(1)
}

#[cfg(not(target_os = "linux"))]
pub fn run(_run_dir: &Path) -> anyhow::Result<()> {
    anyhow::bail!("power-button: Linux only")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(t: u16, c: u16, v: i32) -> Vec<u8> {
        let mut b = vec![0u8; 16];
        b.extend_from_slice(&t.to_ne_bytes());
        b.extend_from_slice(&c.to_ne_bytes());
        b.extend_from_slice(&v.to_ne_bytes());
        b
    }

    #[test]
    fn a_press_is_a_key_down_of_key_power() {
        assert!(is_press(parse_event(&event(EV_KEY, KEY_POWER, 1)).unwrap()));
        assert!(!is_press(parse_event(&event(EV_KEY, KEY_POWER, 0)).unwrap()), "its release");
        assert!(!is_press(parse_event(&event(0, 0, 0)).unwrap()), "a SYN_REPORT");
        assert!(!is_press(parse_event(&event(EV_KEY, 30, 1)).unwrap()), "another key");
        assert_eq!(parse_event(&[0u8; 10]), None);
    }

    #[test]
    fn buttons_engines_and_formats_found() {
        let w = tempfile::tempdir().unwrap();
        let sys = w.path().join("sys");
        for (ev, name) in [("event0", "Power Button"), ("event1", "AT Translated Set 2 keyboard"), ("event2", "Power Button")] {
            std::fs::create_dir_all(sys.join(ev).join("device")).unwrap();
            std::fs::write(sys.join(ev).join("device/name"), format!("{name}\n")).unwrap();
        }
        std::fs::create_dir_all(sys.join("input3")).unwrap();
        let b = buttons(&sys, Path::new("/dev/input"));
        assert_eq!(b, vec![(PathBuf::from("/dev/input/event0"), "Power Button".into()), (PathBuf::from("/dev/input/event2"), "Power Button".into())]);

        let proc_ = w.path().join("proc");
        for (pid, cmd) in [("1", "/init"), ("50", "/usr/sbin/stormblock\0boot-local\0--slab\0x"), ("60", "/usr/sbin/stormblock\0screen\0--run"), ("70", "/usr/sbin/stormblock\0power-button"), ("80", "sh")] {
            std::fs::create_dir_all(proc_.join(pid)).unwrap();
            std::fs::write(proc_.join(pid).join("cmdline"), cmd).unwrap();
        }
        assert_eq!(to_stop(&proc_, 70), vec![50, 60]);

        let run = w.path().join("run");
        std::fs::create_dir_all(run.join("storage-format")).unwrap();
        assert_eq!(under_way(&run), None);
        std::fs::write(run.join("storage-format/sdb.log"), "x").unwrap();
        std::fs::write(run.join("storage-format/sda.log"), "x").unwrap();
        std::fs::write(run.join("storage-format/sda.rc"), "0").unwrap();
        assert_eq!(under_way(&run).as_deref(), Some("sdb"));
    }
}
