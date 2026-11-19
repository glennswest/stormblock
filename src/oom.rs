//! The node engine is the last process the OOM killer may take (#685).
//!
//! On a stormcos node the engine serves every ublk device, its own root
//! (`/pallets/stormblock`) and the log volumes among them. Killed, it cannot
//! be started again: the restart faults on pages of a binary nothing serves,
//! and every process that touches a volume waits for ever. 12.96's pvetest2
//! and pvetest3 were left that way, `:9090` and `:6443` refused and every other
//! port accepting and never answering. The engine also locks every page it
//! allocates (`mlockall(MCL_FUTURE)`, for the handover), which makes it the
//! largest thing the kernel cannot reclaim: the OOM killer's likeliest pick.
//!
//! So the node engine sets its `oom_score_adj` to [`EXEMPT`]: under memory
//! pressure the kernel kills a pod or a service, which its supervisor can
//! start again, never the storage under all of them.

use std::path::Path;

/// "Never this one" to the OOM killer.
pub const EXEMPT: i32 = -1000;

const SELF: &str = "/proc/self/oom_score_adj";

/// Write `value` to an `oom_score_adj` file and read it back: `Ok` with what
/// the kernel holds, `Err` with why it could not be set.
pub fn set_adj(path: &Path, value: i32) -> Result<i32, String> {
    std::fs::write(path, format!("{value}\n")).map_err(|e| format!("writing {}: {e}", path.display()))?;
    let got = std::fs::read_to_string(path).map_err(|e| format!("reading {} back: {e}", path.display()))?;
    let got: i32 = got.trim().parse().map_err(|e| format!("{} reads {:?}: {e}", path.display(), got.trim()))?;
    if got == value {
        Ok(got)
    } else {
        Err(format!("{} reads {got} after writing {value}", path.display()))
    }
}

/// The node engine's exemption, said either way: a node whose engine can be
/// killed under memory pressure is one an OOM wedges.
pub fn exempt_node_engine() {
    match set_adj(Path::new(SELF), EXEMPT) {
        Ok(v) => tracing::info!(
            "oom: the engine's oom_score_adj is {v}: under memory pressure the kernel takes another process, never the storage every volume needs (#685)"
        ),
        Err(e) => tracing::warn!(
            "oom: the engine could not be exempted from the OOM killer ({e}): one kill under memory pressure leaves every volume on this node without its server (#685)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_value_is_written_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("oom_score_adj");
        std::fs::write(&f, "0\n").unwrap();
        assert_eq!(set_adj(&f, EXEMPT), Ok(-1000));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "-1000\n");
    }

    #[test]
    fn a_file_that_cannot_be_written_is_said() {
        let e = set_adj(Path::new("/nonexistent-685/oom_score_adj"), EXEMPT).unwrap_err();
        assert!(e.contains("writing /nonexistent-685/oom_score_adj"), "{e}");
    }

    /// The kernel clamps or refuses; a value that does not stick is an error.
    #[test]
    fn a_value_that_does_not_stick_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("adj");
        std::fs::create_dir(&f).unwrap();
        assert!(set_adj(&f, EXEMPT).is_err());
    }
}
