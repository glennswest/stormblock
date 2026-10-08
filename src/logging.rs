//! Where the engine's log goes (#243).
//!
//! Everything the engine writes to stderr reaches every console of a node:
//! in the initramfs through `/init`'s follower, and on the running node
//! through stormpump's echo. At INFO that is one line per volume per step,
//! about 55 volumes × 2–3 lines at a shutdown. That scrolls the boot's stages
//! and its errors off a VGA screen, and on a 115200-baud serial console it
//! slows the boot itself. The owner, watching server8: "the info messages we
//! need to turn off to the console. Overload".
//!
//! So on a node (`boot-local`, `adopt-ublk`, `boot-iscsi`) the log is two
//! streams:
//!
//! - **the record**, the whole log, in [`DEFAULT_RECORD`]. `/init` moves
//!   `/run` into the real root, so the initramfs engine and the node's one
//!   append to one file for the boot. Its level is `RUST_LOG`, else
//!   `stormblock.log=` on the kernel command line, else `info`;
//! - **the console** (stderr): WARN and above. `STORMBLOCK_CONSOLE_LOG`, else
//!   `stormblock.console_log=` on the kernel command line, raises it, so a
//!   machine can be booted verbose without a rebuild.
//!
//! The stage lines a boot shows (`Flow-over: …`, `Boot volume: …`) are
//! `println!`, on stdout, and are not filtered.
//!
//! A record that cannot be opened (no `/run`, a read-only one) leaves stderr
//! carrying the whole log, as before, and says so. Every other command (the
//! daemon, the CLI tools) logs to stderr at `RUST_LOG`, else `info`, as it
//! always has.

use std::path::PathBuf;

/// The record's default path on a node.
pub const DEFAULT_RECORD: &str = "/run/stormblock/stormblock.log";

/// What [`plan`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The record's filter (`EnvFilter` syntax).
    pub record: String,
    /// The record's file, on a node. `None`: everything to stderr.
    pub file: Option<PathBuf>,
    /// The console's (stderr's) filter, when the record has a file.
    pub console: String,
}

/// The value of `key=` on a kernel command line, if it is there (the last
/// one wins, as the kernel's own parameters do).
pub fn cmdline_value(cmdline: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    cmdline
        .split_whitespace()
        .filter_map(|w| w.strip_prefix(&prefix))
        .last()
        .map(|v| v.trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

/// Decide the streams from what the process was given. `env` looks up an
/// environment variable; `cmdline` is `/proc/cmdline` (empty off Linux).
pub fn plan(node: bool, env: impl Fn(&str) -> Option<String>, cmdline: &str) -> Plan {
    let given = |k: &str| env(k).filter(|v| !v.trim().is_empty());
    let record = given("RUST_LOG")
        .or_else(|| cmdline_value(cmdline, "stormblock.log"))
        .unwrap_or_else(|| "info".to_string());
    if !node {
        return Plan { record, file: None, console: String::new() };
    }
    let file = Some(PathBuf::from(given("STORMBLOCK_LOG_FILE").unwrap_or_else(|| DEFAULT_RECORD.to_string())));
    let console = given("STORMBLOCK_CONSOLE_LOG")
        .or_else(|| cmdline_value(cmdline, "stormblock.console_log"))
        .unwrap_or_else(|| "warn".to_string());
    Plan { record, file, console }
}

fn filter(spec: &str) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_new(spec).unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
}

/// Set up the log for this process. `node`: one of the commands a node
/// boots with.
pub fn init(node: bool) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;

    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let p = plan(node, |k| std::env::var(k).ok(), &cmdline);
    let opened = p.file.as_ref().map(|path| {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::OpenOptions::new().create(true).append(true).open(path).map_err(|e| (path.clone(), e))
    });
    match opened {
        Some(Ok(file)) => {
            tracing_subscriber::registry()
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(std::sync::Mutex::new(file))
                        .with_ansi(false)
                        .with_filter(filter(&p.record)),
                )
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(std::io::stderr)
                        .with_filter(filter(&p.console)),
                )
                .init();
            if let Some(path) = &p.file {
                // On the record only: the console has heard enough.
                tracing::info!(
                    "log: everything at '{}' in {}; the console gets '{}' (stormblock.console_log= to change)",
                    p.record,
                    path.display(),
                    p.console
                );
            }
        }
        other => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(filter(&p.record))
                .init();
            if let Some(Err((path, e))) = other {
                tracing::warn!("log: cannot write {} ({e}); the whole log goes to the console", path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn a_node_writes_the_record_to_a_file_and_warn_to_the_console() {
        let p = plan(true, env(&[]), "BOOT_IMAGE=/vmlinuz console=ttyS0,115200n8 console=tty0");
        assert_eq!(p.record, "info");
        assert_eq!(p.file.as_deref(), Some(std::path::Path::new(DEFAULT_RECORD)));
        assert_eq!(p.console, "warn");
    }

    #[test]
    fn the_kernel_line_raises_either_stream() {
        let p = plan(true, env(&[]), "quiet stormblock.console_log=info stormblock.log=stormblock=debug");
        assert_eq!((p.record.as_str(), p.console.as_str()), ("stormblock=debug", "info"));
    }

    #[test]
    fn the_environment_wins_over_the_kernel_line() {
        let p = plan(
            true,
            env(&[("RUST_LOG", "debug"), ("STORMBLOCK_CONSOLE_LOG", "error"), ("STORMBLOCK_LOG_FILE", "/tmp/x.log")]),
            "stormblock.console_log=info stormblock.log=warn",
        );
        assert_eq!(p.record, "debug");
        assert_eq!(p.console, "error");
        assert_eq!(p.file.as_deref(), Some(std::path::Path::new("/tmp/x.log")));
    }

    #[test]
    fn other_commands_log_to_stderr_as_before() {
        let p = plan(false, env(&[]), "stormblock.console_log=info");
        assert_eq!((p.record.as_str(), p.file.clone()), ("info", None));
        let p = plan(false, env(&[("RUST_LOG", "trace")]), "");
        assert_eq!(p.record, "trace");
    }

    #[test]
    fn cmdline_values_are_read_whole_and_the_last_wins() {
        assert_eq!(cmdline_value("a=1 stormblock.log=x b", "stormblock.log").as_deref(), Some("x"));
        assert_eq!(cmdline_value("stormblock.logs=x", "stormblock.log"), None);
        assert_eq!(cmdline_value("stormblock.log=a stormblock.log=b", "stormblock.log").as_deref(), Some("b"));
        assert_eq!(cmdline_value("stormblock.log=", "stormblock.log"), None);
        assert_eq!(cmdline_value("stormblock.log=\"debug\"", "stormblock.log").as_deref(), Some("debug"));
    }
}

/// A panic anywhere in a node's engine ends the process (#368).
///
/// On the Dell (11.98) a tokio worker panicked inside the scheduler
/// (`state.rs:120 next.is_notified()`). The process stayed up with that
/// worker gone: the API stopped answering, the watchdog (blocked in a task
/// dump that waited on the dead worker) stopped logging, and nothing
/// restarted it. Alive and silent is the worst outcome a supervisor can be
/// handed. So the panic is said, with a backtrace captured here (whatever
/// `RUST_BACKTRACE` is), on stderr (every console, and stormpump's log) and
/// in the record, and the process aborts. stormpump restarts the engine, and
/// `adopt-ublk` takes the devices back as from any incumbent.
pub fn abort_on_panic(mode: &'static str) {
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let bt = std::backtrace::Backtrace::force_capture();
        let msg = format!(
            "FATAL: {mode}: panic on thread '{thread}': {info}; aborting so the supervisor restarts the engine (#368)\n{bt}"
        );
        eprintln!("{msg}");
        // The record, from a thread of its own and bounded: a panic taken
        // while the log's own lock was held must not keep the abort waiting.
        let (tx, rx) = std::sync::mpsc::channel();
        let m = msg.clone();
        let _ = std::thread::Builder::new().name("panic-log".into()).spawn(move || {
            tracing::error!("{m}");
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
        std::process::abort();
    }));
}
