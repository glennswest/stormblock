//! What serving needs to know, separated from where the values come from.
//!
//! The engine owns what these *mean* — how long a withdrawn export drains
//! before its LUN is pulled, which ports per-export targets are allocated
//! from, what address a consumer is told to attach to. A profile owns where
//! the values come from: environment variables, a config file, a fabric
//! controller. Those are different jobs, and putting them in one struct is
//! what made a RouterOS crate the only place this logic could live.
//!
//! Every field here is a serving parameter, not a deployment detail. A
//! profile that sets none of them gets a working default; a profile that sets
//! all of them has said nothing about *how* it is deployed, only about how it
//! serves.

/// Serving parameters. See the module docs for the division of labour.
#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// Where durable serving state is kept — the export table, the wiring
    /// table. A directory the profile guarantees survives a restart.
    pub data_dir: String,

    /// Address a consumer is told to attach to. Not necessarily an address
    /// this process binds: a node behind a fabric advertises where it can be
    /// *reached*, which is not always where it listens.
    pub advertise_addr: String,

    /// Serve the legacy iSCSI stack at all. Off by default — NVMe-TCP is the
    /// transport, and an iSCSI export while this is false is *blocked*
    /// rather than pending, because nothing will ever wire it.
    pub iscsi_enabled: bool,
    /// Where the shared multi-LUN iSCSI target listens, when it is served.
    pub iscsi_bind: String,
    /// Where the shared NVMe-oF portal listens. Also decides which interface
    /// the per-export targets bind to, so one firewall rule covers the range.
    pub nvmeof_bind: String,
    /// The shared target's IQN.
    pub iqn: String,
    /// Prefix for per-export IQNs.
    pub iqn_prefix: String,
    /// The shared subsystem's NQN.
    pub nqn: String,
    /// Prefix for per-export NQNs.
    pub nqn_prefix: String,

    /// First port of the per-export range, and how many ports it holds.
    ///
    /// Each export gets a target of its own on its own port, so an initiator
    /// that cannot select a LUN still reaches exactly one volume. The range
    /// bounds how many exports can be served at once.
    pub portal_base: u16,
    pub portal_span: u16,

    /// How long a withdrawn export drains before its LUN is pulled anyway.
    /// Long enough for an initiator to notice and let go; not so long that a
    /// consumer that already left holds a port.
    pub drain_grace_secs: u64,
    /// Reconciler tick.
    pub reconcile_secs: u64,
    /// How long a pending export may name a volume that does not exist before
    /// it is withdrawn. `Pending` legitimately means "the volume may not
    /// exist yet", so this cannot be zero; past it the volume is not late, it
    /// is gone. **0 disables the sweep.**
    pub orphan_export_grace_secs: u64,

    /// How often the template reaper runs. **0 disables it.**
    pub reap_secs: u64,
    /// Whether the reaper deletes, or only reports what it would delete.
    pub reap_apply: bool,
    /// How long debris must have looked like debris before it is reaped —
    /// the difference between reaping a leak and deleting a template someone
    /// is still formatting.
    pub reap_min_age_secs: u64,
    /// A ceiling per pass, so a bad classification cannot empty an array in
    /// one sweep.
    pub reap_max_per_pass: usize,
}

impl Default for ServeConfig {
    fn default() -> Self {
        ServeConfig {
            data_dir: "/data/meta".to_string(),
            advertise_addr: String::new(),
            iscsi_enabled: false,
            iscsi_bind: "0.0.0.0:3260".to_string(),
            nvmeof_bind: "0.0.0.0:4420".to_string(),
            iqn: "iqn.2026-08.lo.storm:shared".to_string(),
            iqn_prefix: "iqn.2026-08.lo.storm".to_string(),
            nqn: "nqn.2026-08.lo.storm:shared".to_string(),
            nqn_prefix: "nqn.2026-08.lo.storm".to_string(),
            portal_base: 3261,
            portal_span: 128,
            drain_grace_secs: 120,
            reconcile_secs: 2,
            orphan_export_grace_secs: 300,
            reap_secs: 600,
            reap_apply: true,
            reap_min_age_secs: 900,
            reap_max_per_pass: 64,
        }
    }
}
