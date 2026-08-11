//! Filesystem knowledge the storage engine needs to own.
//!
//! The engine already lays out filesystems for bootable drives —
//! `boot_iscsi.rs` orchestrates a complete partitioned Linux disk (ESP, boot,
//! root, swap, home) as independent thin volumes. A module that can *create*
//! an empty one is not a new kind of knowledge here, and it buys the capability
//! every consumer wants: **mkfs once, clone forever**.
//!
//! Scope is deliberately narrow — only what a *storage* layer must do:
//!
//! | | Here | Elsewhere |
//! |---|---|---|
//! | blank filesystem creation | yes — content-free, local, fast, pure Rust is enough | |
//! | clone-time UUID stamping | yes — only the engine has the clone in hand | |
//! | superblock inspection / seal guard | yes — a storage operation | |
//! | writing image *content* into a filesystem | | the consumer that owns the content (tar, whiteouts, image config) |
//!
//! See [`ext4`] for the on-disk format and [`template`] for the lifecycle.

pub mod ext4;
pub mod template;

pub use ext4::{Ext4Layout, Ext4Params, Ext4Report, SealBlocker};
pub use template::{
    clone_template, CloneResult, CloneSpec, FsKind, FsTemplate, TemplateError, TemplateSpec,
    TemplateState, TemplateStore,
};
