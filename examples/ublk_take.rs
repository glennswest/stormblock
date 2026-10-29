//! Play the privileged pod of #299: STOP_DEV and DEL_DEV for a ublk device
//! this process does not serve, as any process with CAP_SYS_ADMIN and
//! `/dev/ublk-control` may. For `ci-ublk-qd-verify.sh`'s `taken` phase.
//!
//! ```text
//! ublk_take <dev_id>
//! ```

fn main() -> anyhow::Result<()> {
    let id: u32 = std::env::args().nth(1).and_then(|a| a.parse().ok()).ok_or_else(|| anyhow::anyhow!("usage: ublk_take <dev_id>"))?;
    stormblock::drive::ublk::remove(id).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("/dev/ublkb{id}: stopped and deleted");
    Ok(())
}
