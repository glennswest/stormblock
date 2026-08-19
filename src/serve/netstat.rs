//! Counting live initiator connections per portal port.
//!
//! Ordered teardown (issue #7) needs to know whether anybody is still
//! attached before a LUN is withdrawn. `IscsiTarget` keeps a `SessionRegistry`
//! but exposes no accessor, and mk consumes the engine unmodified — so the
//! authority here is the kernel: every iSCSI session is a TCP connection to
//! the export's dedicated portal port, and `/proc/net/tcp{,6}` lists them.
//!
//! This is exact for the per-export portals (one target, one volume, so every
//! connection on that port belongs to that export) and is the reason each
//! export gets its own portal in the first place.

/// TCP state 01 = ESTABLISHED in /proc/net/tcp.
const TCP_ESTABLISHED: &str = "01";

/// Count established inbound connections whose LOCAL port is `port`.
///
/// Returns `None` when /proc is unreadable — callers must treat that as
/// "unknown", never as "nobody is attached".
pub fn established_on_port(port: u16) -> Option<usize> {
    let mut total = 0usize;
    let mut saw_any_table = false;
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(raw) = std::fs::read_to_string(path) else { continue };
        saw_any_table = true;
        total += count_in_table(&raw, port);
    }
    saw_any_table.then_some(total)
}

/// Parse a /proc/net/tcp table: `sl local_address rem_address st ...` where
/// addresses are `HEX_ADDR:HEX_PORT` and `st` is the connection state.
fn count_in_table(table: &str, port: u16) -> usize {
    table
        .lines()
        .skip(1) // header
        .filter(|line| {
            let mut f = line.split_whitespace();
            let (Some(_sl), Some(local), Some(_rem), Some(st)) =
                (f.next(), f.next(), f.next(), f.next())
            else {
                return false;
            };
            if st != TCP_ESTABLISHED {
                return false;
            }
            local
                .rsplit_once(':')
                .and_then(|(_, p)| u16::from_str_radix(p, 16).ok())
                .map(|p| p == port)
                .unwrap_or(false)
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:0CBD 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0
   1: C8A8C0C0:0CBD 15A8C0C0:C350 01 00000000:00000000 00:00000000 00000000     0        0 2 1 0
   2: C8A8C0C0:0CBD 16A8C0C0:C351 01 00000000:00000000 00:00000000 00000000     0        0 3 1 0
   3: C8A8C0C0:0CBE 16A8C0C0:C352 01 00000000:00000000 00:00000000 00000000     0        0 4 1 0
   4: C8A8C0C0:0CBD 17A8C0C0:C353 06 00000000:00000000 00:00000000 00000000     0        0 5 1 0
";

    #[test]
    fn counts_only_established_on_the_asked_port() {
        // 0x0CBD = 3261: two ESTABLISHED, one LISTEN (0A), one TIME_WAIT (06).
        assert_eq!(count_in_table(SAMPLE, 3261), 2);
        // 0x0CBE = 3262
        assert_eq!(count_in_table(SAMPLE, 3262), 1);
        assert_eq!(count_in_table(SAMPLE, 3260), 0);
    }

    #[test]
    fn tolerates_garbage() {
        assert_eq!(count_in_table("header\nnonsense\n", 3261), 0);
        assert_eq!(count_in_table("", 3261), 0);
    }
}
