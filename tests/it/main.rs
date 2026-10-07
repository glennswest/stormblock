//! The in-process integration tests, as one test binary (stormblock#209).
//!
//! Each file used to be its own binary, and every one linked the whole engine:
//! 33 links per `cargo test`. They are modules of this one now. Run them with
//! cargo-nextest (`cargo nextest run`), which gives every test its own process,
//! so modules that each bound ports or set globals as separate binaries still
//! cannot interfere.
//!
//! Tests that need the real binary, a kernel device or privileges are runtime
//! tests and live in `tests-runtime/`, outside the routine build.

mod common;

mod boot_iscsi;
mod contract_v1_wire;
mod crash_recovery;
mod integration_array_pin;
mod integration_auth;
mod integration_compose_disk;
mod integration_destructive;
mod integration_emulated;
mod integration_erase;
mod integration_forge;
mod integration_fstemplates;
mod integration_handover_order;
mod integration_iscsi;
mod integration_metadata_v2;
mod integration_mgmt_api;
mod integration_moves;
mod integration_multidrive;
mod integration_ana_epoch;
mod integration_nvme_hosts;
mod integration_nvmeof;
mod integration_pallet;
mod integration_placement;
mod integration_power_cut;
mod integration_raid_degraded;
mod integration_raid_sets;
mod integration_releases;
mod integration_serve_in_use;
mod integration_serve_own_exports;
mod integration_serve_mounted;
mod integration_stormfs;
mod integration_synonyms;
mod integration_v1_api;
mod integration_volume;
mod integration_volume_kinds;
mod small_volumes;
