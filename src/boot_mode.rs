//! The boot's mode (#629): what /init read from the install-config's
//! top-level `mode:` and handed to the node as `/run/stormblock/mode`.
//!
//! `diagnostic` (owner, 2026-11-16: "boot just on nvme over tcp but not do
//! flow over ... drive diagnostics without install/flow over"): the node runs
//! from the claimed image and touches no local drive. /init left every drive
//! alone (no layout, no node-data set). Here the engine refuses a flow-over,
//! health and the screen say `diagnostic`, and the runtime actions on drives
//! (details, self-test, format, write test) stay available: they are asked
//! for, one drive at a time. Cleared by a boot whose install-config says
//! otherwise.

use std::path::Path;
use std::sync::OnceLock;

/// Where /init leaves the mode.
pub const FILE: &str = "/run/stormblock/mode";

/// The mode in a run directory's `mode` file, if any.
pub fn read_in(run: &Path) -> Option<String> {
    let v = std::fs::read_to_string(run.join("mode")).ok()?;
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// This boot's mode, read once.
pub fn mode() -> Option<&'static str> {
    static MODE: OnceLock<Option<String>> = OnceLock::new();
    MODE.get_or_init(|| read_in(Path::new(FILE).parent().unwrap_or(Path::new("/run/stormblock"))))
        .as_deref()
}

/// A diagnostic boot: no local drive is laid or flowed over.
pub fn diagnostic() -> bool {
    mode() == Some("diagnostic")
}

/// Why a lay or a flow-over is refused on this boot, when it is.
pub fn refuses(what: &str) -> Option<String> {
    diagnostic().then(|| {
        format!("{what} refused: this is a diagnostic boot (mode: diagnostic in the install-config, #629); clear the mode and boot again")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mode_file() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(read_in(d.path()), None);
        std::fs::write(d.path().join("mode"), "diagnostic\n").unwrap();
        assert_eq!(read_in(d.path()).as_deref(), Some("diagnostic"));
        std::fs::write(d.path().join("mode"), "\n").unwrap();
        assert_eq!(read_in(d.path()), None);
    }
}
