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
//! | writing a handful of known files into one | yes — [`files`], in userspace, no mount | |
//! | writing image *content* into a filesystem | | the consumer that owns the content (tar, whiteouts, image config) |
//!
//! See [`ext4`] for the on-disk format and [`template`] for the lifecycle.

pub mod ext4;
/// Reading the blocks a filesystem is *not* using.
///
/// Lived in the RouterOS profile because "mk must not patch the engine", which
/// is exactly the rule this layering removes: it is a read-only ext2/3/4
/// layout reader, and where free space is on a volume is engine knowledge.
pub mod ext4_free;
pub mod files;
pub mod template;

pub use ext4::{Ext4Layout, Ext4Params, Ext4Report, FsProfile, SealBlocker};
pub use files::SeedFile;
pub use template::{
    claim, clone_template, ensure_standing, ensure_standing_all, ClaimSpec, CloneResult, CloneSpec,
    FsKind, FsTemplate, StandingClone, TemplateError, TemplateSpec, TemplateState, TemplateStore,
};
