//! StormBlock — Pure Rust Enterprise Block Storage Engine.
//!
//! The binary is a wrapper: the command line lives in the library
//! (`stormblock::cli`), where it is compiled and tested once (#209).

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    stormblock::cli::run().await
}
