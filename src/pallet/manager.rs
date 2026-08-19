//! The pallet lifecycle: compose, publish, verify, activate, move, roll back.
//!
//! This is the component #52 asks for, and it lives here because everything a
//! pallet lifecycle needs is already the engine's: sealed extents, atomic
//! pointer changes, and drives to lay content on. A manager anywhere else
//! would either duplicate those or reach across a boundary for them.
//!
//! Two invariants shape every method below.
//!
//! **Nothing in use is ever rewritten.** Publishing an upgrade allocates a new
//! partition and writes new content; the running pallet's bytes are untouched.
//! Fallback therefore works by construction rather than by policy — the
//! previous pallet is still intact because nothing could have overwritten it.
//! Recomposing a pallet (adding or dropping a member) is the same story: it
//! publishes a *new version* rather than editing a sealed one.
//!
//! **Activation is an attribute write.** Selecting a different pallet changes
//! GPT attribute bits and nothing else, on both GPT copies. There is no
//! content to migrate and no window where the node has no bootable pallet.

use std::sync::Arc;

use uuid::Uuid;

use super::format::{Attributes, Member, MemberKind, MemberSpec, PalletBuilder, PalletKind};
use super::gpt::{Gpt, GptEntry, ALIGN_BYTES};
use super::store::{PalletLocation, PalletState, PalletStore};
use super::{
    format::PALLET_TYPE_GUID, MemberContent, Pallet, PalletError, PartitionView, Result,
};

/// Copy chunk for moving pallet bytes between drives.
const COPY_CHUNK: usize = 4 * 1024 * 1024;

/// Tries a freshly published pallet gets before a consumer gives up on it.
const DEFAULT_TRIES: u8 = 3;

/// The highest priority the 4-bit GPT field can hold — what activation sets.
const TOP_PRIORITY: u8 = 15;

// ------------------------------------------------------------------ content

/// A member of an existing pallet, used as the content of a new one.
///
/// This is what makes recompose and member-move possible without staging
/// anything: the bytes are read through the source pallet's extent map and
/// written into the new pallet directly. The source is never modified — it
/// could not be, it is sealed.
pub struct PalletMemberContent {
    pallet: Arc<Pallet>,
    view: PartitionView,
    member: Member,
}

impl PalletMemberContent {
    pub fn new(pallet: Arc<Pallet>, view: PartitionView, member: Member) -> Self {
        PalletMemberContent { pallet, view, member }
    }

    pub fn member(&self) -> &Member {
        &self.member
    }
}

#[async_trait::async_trait]
impl MemberContent for PalletMemberContent {
    fn byte_len(&self) -> u64 {
        self.member.byte_len
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.pallet.read_member(&self.member, &self.view, offset, buf).await
    }
}

// ------------------------------------------------------------------ verdicts

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemberVerdict {
    pub name: String,
    pub role: String,
    pub kind: String,
    pub byte_len: u64,
    pub digest: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The result of checking a pallet and everything it claims.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VerifyReport {
    pub id: Uuid,
    pub name: String,
    pub version: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub members: Vec<MemberVerdict>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FailedPallet {
    pub location: PalletLocation,
    pub reason: String,
}

/// What the node has: what is selected, what it could fall back to, and what
/// it will not use.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PalletStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<PalletLocation>,
    pub available: Vec<PalletLocation>,
    pub failed: Vec<FailedPallet>,
}

// -------------------------------------------------------------------- specs

/// What to publish.
pub struct PublishSpec {
    pub name: String,
    pub kind: PalletKind,
    /// Monotonic version. `None` takes one past the highest existing pallet of
    /// the same name, which is what makes selection deterministic.
    pub version: Option<u64>,
    pub version_label: String,
    pub members: Vec<MemberSpec>,
    /// Which drive to land on. `None` takes the first with room — several
    /// pallets per drive and several drives per node are both normal.
    pub drive: Option<usize>,
    /// Partition size. `None` fits the content. Sparse is free to a consumer
    /// that only does block reads, so sizing for headroom costs nothing but
    /// address space.
    pub size_bytes: Option<u64>,
    pub block_size: Option<u32>,
    /// Members are immutable and must never be attached writably. True unless
    /// the caller has a specific reason otherwise.
    pub read_only: bool,
    pub sealed: bool,
    pub tries: u8,
    pub priority: Option<u8>,
    /// Verify and make this the selected pallet in one step.
    pub activate: bool,
}

impl PublishSpec {
    pub fn new(name: impl Into<String>, kind: PalletKind) -> Self {
        PublishSpec {
            name: name.into(),
            kind,
            version: None,
            version_label: String::new(),
            members: Vec::new(),
            drive: None,
            size_bytes: None,
            block_size: None,
            read_only: true,
            sealed: true,
            tries: DEFAULT_TRIES,
            priority: None,
            activate: false,
        }
    }

    pub fn member(mut self, m: MemberSpec) -> Self {
        self.members.push(m);
        self
    }
}

/// Changes to make while republishing a pallet as a new version.
#[derive(Default)]
pub struct RecomposeSpec {
    pub add: Vec<MemberSpec>,
    pub remove: Vec<String>,
    pub version: Option<u64>,
    pub version_label: Option<String>,
    pub kind: Option<PalletKind>,
    pub name: Option<String>,
    /// Land the new version on a different drive. This is how a pallet's
    /// contents move across drives while changing what is in them.
    pub drive: Option<usize>,
    pub size_bytes: Option<u64>,
    pub activate: bool,
}

// ------------------------------------------------------------------ manager

/// Owns pallet lifecycle over a set of drives.
pub struct PalletManager {
    store: PalletStore,
}

fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

impl PalletManager {
    pub fn new(store: PalletStore) -> Self {
        PalletManager { store }
    }

    pub fn store(&self) -> &PalletStore {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut PalletStore {
        &mut self.store
    }

    /// Write a fresh, empty GPT. Refuses a drive that already has one, since
    /// that would strand every partition on it.
    pub async fn init_gpt(&self, drive_index: usize, force: bool) -> Result<()> {
        let d = self.store.drive(drive_index)?.clone();
        if !force {
            if let Ok(existing) = Gpt::read(&d.device).await {
                let used = existing.partitions().count();
                return Err(PalletError::Refused(format!(
                    "{} already has a GPT with {used} partition(s)",
                    d.path
                )));
            }
            // A whole-drive pallet keeps its superblock at byte zero, which is
            // where the protective MBR and the GPT header have to go. Writing a
            // table here does not subdivide the drive, it destroys the pallet
            // on it.
            if let Some(p) = self.store.whole_drive_pallet(drive_index).await {
                return Err(PalletError::Refused(format!(
                    "{} is a whole-drive pallet ('{}' v{}). Adopt it onto another drive first —                      subdividing is a copy, because the table wants the bytes the superblock is in",
                    d.path, p.name, p.version
                )));
            }
        }
        Gpt::create(&d.device).write(&d.device).await
    }

    pub async fn list(&self) -> Vec<PalletLocation> {
        self.store.scan().await
    }

    pub async fn get(&self, id: Uuid) -> Result<PalletLocation> {
        self.store.find(id).await
    }

    /// Active, available and failed, per kind.
    ///
    /// "Active" is the selection a consumer would make right now: the
    /// highest-priority candidate of that kind. Nothing records it separately,
    /// because a record could disagree with the GPT and then one of them would
    /// be lying.
    pub async fn status(&self, kind: Option<PalletKind>) -> PalletStatus {
        let all = self.store.scan().await;
        let mut failed = Vec::new();
        let mut usable = Vec::new();
        for p in all {
            if kind.is_some_and(|k| p.kind != k) && p.is_readable() {
                continue;
            }
            match &p.state {
                PalletState::Unreadable { reason } => {
                    let reason = reason.clone();
                    failed.push(FailedPallet { location: p, reason });
                }
                PalletState::Readable => {
                    if p.attributes.is_candidate() {
                        usable.push(p);
                    } else {
                        failed.push(FailedPallet {
                            location: p,
                            reason: "not a candidate: priority 0, or out of tries and never \
                                     confirmed good"
                                .to_string(),
                        });
                    }
                }
            }
        }
        usable.sort_by_key(|p| std::cmp::Reverse(p.order_key()));
        let active = usable.first().cloned();
        PalletStatus { active, available: usable, failed }
    }

    /// The pallet a consumer of this kind would select.
    pub async fn active(&self, kind: Option<PalletKind>) -> Option<PalletLocation> {
        self.status(kind).await.active
    }

    // ------------------------------------------------------------- publish

    /// Compose and publish: lay new sealed content down beside whatever is
    /// running, and never touch what is in use.
    pub async fn publish(&self, spec: PublishSpec) -> Result<PalletLocation> {
        let existing = self.store.scan().await;
        let version = match spec.version {
            Some(v) => v,
            None => existing
                .iter()
                .filter(|p| p.name == spec.name)
                .map(|p| p.version)
                .max()
                .unwrap_or(0)
                + 1,
        };

        let attrs = Attributes {
            priority: spec.priority.unwrap_or(1),
            tries_left: spec.tries,
            successful: false,
            sealed: spec.sealed,
            read_only: spec.read_only,
            required: true,
        };

        let mut builder = PalletBuilder::new(spec.name.clone(), version)
            .kind(spec.kind)
            .version_label(spec.version_label.clone())
            .attributes(attrs);
        for m in spec.members {
            builder = builder.member(m);
        }

        // Pick the drive before sizing, so the pallet's block size matches the
        // device it will live on rather than a default that may be smaller.
        let drive_index = spec.drive.unwrap_or(0);
        let drive = self.store.drive(drive_index)?.clone();
        let bs = spec.block_size.unwrap_or_else(|| drive.device.block_size().max(4096));
        builder = builder.block_size(bs);

        let built = builder.build().await?;
        let want = align_up(spec.size_bytes.unwrap_or(built.total_bytes).max(built.total_bytes), ALIGN_BYTES);

        // Allocate the partition first: it is the claim on the range, and two
        // publishes racing for the same free run would otherwise both win.
        let mut gpt = Gpt::read(&drive.device).await?;
        let slot = gpt.allocate(&spec.name, PALLET_TYPE_GUID, want, attrs.to_u64())?;
        gpt.write(&drive.device).await?;

        let view = gpt.view(&drive.device, slot)?;
        builder.write(&built, &view).await?;

        let id = gpt.entries[slot].uuid();
        let loc = self.store.find(id).await?;

        // Check it where it landed, not where it was built — the two are only
        // the same claim until a cable disagrees.
        let report = self.verify(id).await?;
        if !report.ok {
            return Err(PalletError::Refused(format!(
                "published pallet did not verify: {}",
                report.reason.unwrap_or_else(|| "unknown".into())
            )));
        }

        if spec.activate {
            self.activate(id).await?;
            return self.store.find(id).await;
        }
        Ok(loc)
    }

    /// Republish a pallet as a new version with members added or dropped.
    ///
    /// The source is sealed, so this is never an edit: kept members are copied
    /// through the source's extent map into a new pallet, and the old version
    /// stays exactly where it was until someone prunes it.
    pub async fn recompose(&self, id: Uuid, spec: RecomposeSpec) -> Result<PalletLocation> {
        let src = self.store.find(id).await?;
        let view = self.store.view(&src)?;
        let pallet = Arc::new(self.store.open(&src).await?);

        let mut members: Vec<MemberSpec> = Vec::new();
        for m in pallet.members() {
            if spec.remove.contains(&m.name) {
                continue;
            }
            let content = Arc::new(PalletMemberContent::new(
                pallet.clone(),
                view.clone(),
                m.clone(),
            ));
            members.push(
                MemberSpec::new(m.name.clone(), m.role.clone(), m.kind, content)
                    .with_flags(m.flags),
            );
        }
        let dropped = pallet.member_count() - members.len();
        if dropped < spec.remove.len() {
            let missing: Vec<&String> = spec
                .remove
                .iter()
                .filter(|r| pallet.find(r).is_err())
                .collect();
            return Err(PalletError::NotFound(format!(
                "member(s) {missing:?} are not in pallet '{}'",
                src.name
            )));
        }
        members.extend(spec.add);

        let mut publish = PublishSpec::new(
            spec.name.unwrap_or_else(|| src.name.clone()),
            spec.kind.unwrap_or(src.kind),
        );
        publish.version = spec.version;
        publish.version_label = spec
            .version_label
            .unwrap_or_else(|| src.version_label.clone());
        publish.members = members;
        publish.drive = Some(spec.drive.unwrap_or(src.drive_index));
        publish.size_bytes = spec.size_bytes;
        publish.read_only = src.attributes.read_only;
        publish.sealed = src.attributes.sealed;
        publish.activate = spec.activate;
        self.publish(publish).await
    }

    // -------------------------------------------------------------- verify

    /// Check a pallet and every member it claims — the same check the
    /// pre-kernel reader performs, run where failing is cheap.
    ///
    /// Members are checked against the **manifest's** digest. Checking against
    /// anything else would pass for whoever rewrote that other index.
    pub async fn verify(&self, id: Uuid) -> Result<VerifyReport> {
        let loc = self.store.find(id).await?;
        let view = self.store.view(&loc)?;
        let pallet = match self.store.open(&loc).await {
            Ok(p) => p,
            Err(e) => {
                return Ok(VerifyReport {
                    id,
                    name: loc.name,
                    version: loc.version,
                    ok: false,
                    reason: Some(e.to_string()),
                    members: Vec::new(),
                })
            }
        };

        let mut report = VerifyReport {
            id,
            name: pallet.name().to_string(),
            version: pallet.version(),
            ok: true,
            reason: None,
            members: Vec::new(),
        };

        if let Err(e) = pallet.verify_manifest() {
            report.ok = false;
            report.reason = Some(e.to_string());
            // A pallet that fails at the manifest has no member a consumer may
            // use, so there is nothing worth reporting per member.
            return Ok(report);
        }

        for m in pallet.members() {
            let (ok, reason) = if m.has_digest() {
                match pallet.verify_member(&m, &view).await {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(e.to_string())),
                }
            } else {
                (true, Some("no digest recorded; content not checked".to_string()))
            };
            if !ok {
                report.ok = false;
                report.reason.get_or_insert_with(|| {
                    format!("member '{}' failed verification", m.name)
                });
            }
            report.members.push(MemberVerdict {
                name: m.name.clone(),
                role: m.role.clone(),
                kind: m.kind.to_string(),
                byte_len: m.byte_len,
                digest: m.digest_hex(),
                ok,
                reason,
            });
        }
        Ok(report)
    }

    // ------------------------------------------------------------ attributes

    /// Make this the pallet its consumers select.
    ///
    /// One attribute write per affected drive: the target takes the top
    /// priority and its competitors are renumbered below it, preserving their
    /// order. The 4-bit priority field cannot be incremented forever, so
    /// activation renumbers rather than climbs.
    ///
    /// Renumbering is **per kind**. A kube pallet does not outrank a boot
    /// pallet by carrying a bigger number — they are not in the same race, and
    /// a consumer must filter by kind before comparing priority.
    pub async fn activate(&self, id: Uuid) -> Result<PalletLocation> {
        let target = self.store.find(id).await?;
        if !target.is_readable() {
            return Err(PalletError::Refused(format!(
                "pallet {id} does not parse; activating it would strand the node"
            )));
        }

        let mut peers: Vec<PalletLocation> = self
            .store
            .scan()
            .await
            .into_iter()
            // A whole-drive pallet has no entry to renumber; leaving it out of
            // the ladder is the only honest thing, and it is why adoption
            // exists.
            .filter(|p| !p.is_whole_drive())
            .filter(|p| p.kind == target.kind && p.id != id && p.attributes.priority > 0)
            .collect();
        peers.sort_by_key(|p| std::cmp::Reverse(p.order_key()));

        let mut changes = Vec::new();
        let mut attrs = target.attributes;
        attrs.priority = TOP_PRIORITY;
        if !attrs.successful && attrs.tries_left == 0 {
            attrs.tries_left = DEFAULT_TRIES;
        }
        changes.push((target.drive_index, target.entry_index, attrs));

        for (i, p) in peers.iter().enumerate() {
            let want = (TOP_PRIORITY - 1).saturating_sub(i as u8).max(1);
            if p.attributes.priority != want {
                let mut a = p.attributes;
                a.priority = want;
                changes.push((p.drive_index, p.entry_index, a));
            }
        }
        self.apply_attributes(&changes).await?;
        self.store.find(id).await
    }

    /// Confirm a pallet booted and is good. Until this happens, a consumer
    /// spends a try each attempt and falls back when they run out.
    pub async fn mark_successful(&self, id: Uuid) -> Result<PalletLocation> {
        let loc = self.store.find(id).await?;
        let mut a = loc.attributes;
        a.successful = true;
        a.tries_left = 0;
        self.apply_attributes(&[(loc.drive_index, loc.entry_index, a)]).await?;
        self.store.find(id).await
    }

    /// Select the pallet below the active one, and take the active one out of
    /// the running.
    ///
    /// This restores nothing: the fallback's content was never overwritten, so
    /// rolling back is choosing it again.
    pub async fn rollback(&self, kind: Option<PalletKind>) -> Result<PalletLocation> {
        let status = self.status(kind).await;
        let active = status
            .active
            .clone()
            .ok_or_else(|| PalletError::Refused("no active pallet to roll back from".into()))?;
        let fallback = status
            .available
            .iter()
            .find(|p| p.id != active.id)
            .cloned()
            .ok_or_else(|| {
                PalletError::Refused(format!(
                    "nothing to fall back to: '{}' is the only candidate. Retention has to keep \
                     N-1 or fallback has nothing to fall back to",
                    active.name
                ))
            })?;

        let mut dead = active.attributes;
        dead.priority = 0;
        self.apply_attributes(&[(active.drive_index, active.entry_index, dead)]).await?;
        self.activate(fallback.id).await
    }

    /// Set the read-only bit, in the GPT attribute **and** in the superblock
    /// mirror, so a consumer that never sees the GPT is told the same thing.
    ///
    /// The mirror lives in the superblock's `flags`, which the manifest digest
    /// does not cover — so this is not a change to signed content.
    pub async fn set_read_only(&self, id: Uuid, read_only: bool, force: bool) -> Result<PalletLocation> {
        let loc = self.store.find(id).await?;
        if !read_only && loc.attributes.sealed && !force {
            return Err(PalletError::Refused(format!(
                "pallet '{}' is sealed: its members may be referenced by something that expects \
                 them never to change. Clear sealed first, or pass force",
                loc.name
            )));
        }
        let mut a = loc.attributes;
        a.read_only = read_only;
        if !loc.is_whole_drive() {
            self.apply_attributes(&[(loc.drive_index, loc.entry_index, a)]).await?;
        }
        self.write_superblock_flags(&loc, a).await?;
        self.store.find(id).await
    }

    /// Set the sealed bit — "never relocate, reuse or GC these extents".
    pub async fn set_sealed(&self, id: Uuid, sealed: bool) -> Result<PalletLocation> {
        let loc = self.store.find(id).await?;
        let mut a = loc.attributes;
        a.sealed = sealed;
        if !loc.is_whole_drive() {
            self.apply_attributes(&[(loc.drive_index, loc.entry_index, a)]).await?;
        }
        self.write_superblock_flags(&loc, a).await?;
        self.store.find(id).await
    }

    async fn write_superblock_flags(&self, loc: &PalletLocation, a: Attributes) -> Result<()> {
        if !loc.is_readable() {
            return Ok(());
        }
        let view = self.store.view(loc)?;
        let mut sb = vec![0u8; super::format::SUPERBLOCK_LEN];
        view.read_at(0, &mut sb).await?;
        sb[136..144].copy_from_slice(&a.to_superblock_flags().to_le_bytes());
        let crc = {
            let mut c = super::crc32(&sb[..132]);
            c = super::crc32_continue(c, &[0, 0, 0, 0]);
            super::crc32_continue(c, &sb[136..])
        };
        sb[132..136].copy_from_slice(&crc.to_le_bytes());
        view.write_at(0, &sb).await?;
        view.flush().await
    }

    async fn apply_attributes(&self, changes: &[(usize, usize, Attributes)]) -> Result<()> {
        if let Some((d, _, _)) = changes
            .iter()
            .find(|(_, e, _)| *e == super::store::WHOLE_DRIVE)
        {
            return Err(PalletError::Refused(format!(
                "{} holds a whole-drive pallet: there is no GPT entry to carry priority, tries or                  the successful bit. Adopt it onto a partitioned drive first",
                self.store.drive(*d)?.path
            )));
        }
        let mut drives: Vec<usize> = changes.iter().map(|(d, _, _)| *d).collect();
        drives.sort_unstable();
        drives.dedup();
        for d in drives {
            let dev = self.store.drive(d)?.device.clone();
            let mut gpt = Gpt::read(&dev).await?;
            for (di, ei, a) in changes.iter().filter(|(di, _, _)| *di == d) {
                let _ = di;
                let e = gpt
                    .entries
                    .get_mut(*ei)
                    .ok_or_else(|| PalletError::NotFound(format!("partition {ei}")))?;
                e.attributes = a.to_u64();
            }
            gpt.write(&dev).await?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- moves

    /// Move a pre-subdivision whole-drive pallet onto a partitioned drive.
    ///
    /// This is the migration from "the drive is the pallet" to "the drive holds
    /// pallets". It has to be a copy: the GPT needs the first bytes of the
    /// device, and those are the pallet's superblock. Afterwards the source
    /// drive can be given a table of its own with `init_gpt(.., force)` and
    /// start carrying several pallets like any other.
    pub async fn adopt_whole_drive(
        &self,
        src_drive: usize,
        dest_drive: usize,
    ) -> Result<PalletLocation> {
        if src_drive == dest_drive {
            return Err(PalletError::Refused(
                "a whole-drive pallet cannot be adopted onto its own drive: the partition table                  would land on the superblock it is trying to preserve"
                    .into(),
            ));
        }
        let src = self
            .store
            .whole_drive_pallet(src_drive)
            .await
            .ok_or_else(|| {
                PalletError::NotFound(format!(
                    "no whole-drive pallet on {}",
                    self.store.drives()[src_drive].path
                ))
            })?;
        self.copy_pallet(src.id, dest_drive).await
    }

    /// Copy a pallet onto another drive, byte for byte.
    ///
    /// Nothing inside a pallet is absolute, so this is a copy and not a
    /// rewrite — the manifest, the extents and the signature all travel
    /// unchanged. The copy is verified at the destination before it counts.
    pub async fn copy_pallet(&self, id: Uuid, dest_drive: usize) -> Result<PalletLocation> {
        let src = self.store.find(id).await?;
        if !src.is_readable() {
            return Err(PalletError::Refused(format!(
                "pallet {id} does not parse; copying it would only spread the damage"
            )));
        }
        let dest = self.store.drive(dest_drive)?.clone();
        let src_view = self.store.view(&src)?;

        let mut gpt = Gpt::read(&dest.device).await?;
        // Size the destination for what the pallet *uses*, not for the space it
        // happened to sit in — a whole-drive pallet "occupies" its entire disk
        // and would otherwise never fit anywhere. Headroom is preserved when
        // the source was a partition and the room is there.
        let used = align_up(src.used_bytes.max(ALIGN_BYTES), ALIGN_BYTES);
        let want = if !src.is_whole_drive() && src.size_bytes <= gpt.largest_free_bytes() {
            align_up(src.size_bytes, ALIGN_BYTES).max(used)
        } else {
            used
        };
        let part_name = if src.partition_name.is_empty() {
            src.name.clone()
        } else {
            src.partition_name.clone()
        };
        let slot = gpt.allocate(&part_name, PALLET_TYPE_GUID, want, src.attributes.to_u64())?;
        gpt.write(&dest.device).await?;
        let dest_view = gpt.view(&dest.device, slot)?;

        // Only the bytes the pallet uses — a pallet is usually laid into a
        // partition with headroom, and headroom is not worth copying.
        let bs = dest.device.block_size() as u64;
        let len = align_up(src.used_bytes.min(src.size_bytes), bs).min(dest_view.len());
        let mut buf = vec![0u8; COPY_CHUNK];
        let mut off = 0u64;
        while off < len {
            let take = ((len - off) as usize).min(COPY_CHUNK);
            let chunk = &mut buf[..take];
            src_view.read_at(off, chunk).await?;
            dest_view.write_at(off, chunk).await?;
            off += take as u64;
        }
        dest_view.flush().await?;

        let new_id = gpt.entries[slot].uuid();
        let report = self.verify(new_id).await?;
        if !report.ok {
            // Take the bad copy back out rather than leaving a partition that
            // looks like a pallet and is not one.
            let mut g = Gpt::read(&dest.device).await?;
            if let Some((i, _)) = g.find_by_uuid(new_id) {
                g.remove(i)?;
                g.write(&dest.device).await?;
            }
            return Err(PalletError::Refused(format!(
                "copy did not verify at the destination: {}",
                report.reason.unwrap_or_else(|| "unknown".into())
            )));
        }
        self.store.find(new_id).await
    }

    /// Move a pallet to another drive: copy, verify, adopt the source's
    /// identity, then drop the source.
    ///
    /// The source entry goes before the destination takes its GUID, so no
    /// interruption can leave two disks claiming to be the same pallet. An
    /// interruption between them leaves the pallet present under a new
    /// identity — recoverable, and the content is already verified.
    pub async fn move_pallet(&self, id: Uuid, dest_drive: usize) -> Result<PalletLocation> {
        let src = self.store.find(id).await?;
        if src.drive_index == dest_drive {
            return Err(PalletError::Refused(
                "source and destination are the same drive".into(),
            ));
        }
        let copy = self.copy_pallet(id, dest_drive).await?;

        let src_dev = self.store.drive(src.drive_index)?.device.clone();
        let mut sg = Gpt::read(&src_dev).await?;
        sg.remove(src.entry_index)?;
        sg.write(&src_dev).await?;

        let dest_dev = self.store.drive(dest_drive)?.device.clone();
        let mut dg = Gpt::read(&dest_dev).await?;
        let (slot, _) = dg
            .find_by_uuid(copy.id)
            .ok_or_else(|| PalletError::NotFound(format!("pallet {} after copy", copy.id)))?;
        dg.entries[slot].unique_guid = id.to_bytes_le();
        dg.write(&dest_dev).await?;

        self.store.find(id).await
    }

    /// Copy one member — a container, a kernel, whatever it is — into another
    /// pallet, which republishes that pallet as a new version.
    ///
    /// A member cannot be added to a sealed pallet in place; that is the whole
    /// point of sealing. So this is a recompose of the destination, and the
    /// destination's previous version is still there afterwards.
    pub async fn copy_member(
        &self,
        from: Uuid,
        member: &str,
        into: Uuid,
        activate: bool,
    ) -> Result<PalletLocation> {
        let spec = self.member_spec(from, member).await?;
        self.recompose(
            into,
            RecomposeSpec { add: vec![spec], activate, ..Default::default() },
        )
        .await
    }

    /// Move a member between pallets: it appears in a new version of the
    /// destination and is absent from a new version of the source.
    ///
    /// Returns `(destination, source)`. Both are new versions; neither old one
    /// is touched, so nothing is lost if only one of the two lands.
    pub async fn move_member(
        &self,
        from: Uuid,
        member: &str,
        into: Uuid,
        activate: bool,
    ) -> Result<(PalletLocation, PalletLocation)> {
        let dest = self.copy_member(from, member, into, activate).await?;
        let src = self
            .recompose(
                from,
                RecomposeSpec {
                    remove: vec![member.to_string()],
                    activate,
                    ..Default::default()
                },
            )
            .await?;
        Ok((dest, src))
    }

    /// A member of an existing pallet, as content for a new one.
    pub async fn member_spec(&self, id: Uuid, member: &str) -> Result<MemberSpec> {
        let loc = self.store.find(id).await?;
        let view = self.store.view(&loc)?;
        let pallet = Arc::new(self.store.open(&loc).await?);
        let m = pallet.find(member)?;
        let content = Arc::new(PalletMemberContent::new(pallet, view, m.clone()));
        Ok(MemberSpec::new(m.name.clone(), m.role.clone(), m.kind, content).with_flags(m.flags))
    }

    // -------------------------------------------------------------- removal

    /// Remove a pallet's GPT entry. The bytes stay until something else claims
    /// the range; only the reference goes.
    pub async fn delete(&self, id: Uuid, force: bool) -> Result<PalletLocation> {
        let loc = self.store.find(id).await?;
        if !force {
            let status = self.status(Some(loc.kind)).await;
            if status.active.as_ref().is_some_and(|a| a.id == id) {
                return Err(PalletError::Refused(format!(
                    "'{}' is the active {} pallet",
                    loc.name, loc.kind
                )));
            }
            let candidates = status.available.len();
            if loc.attributes.is_candidate() && candidates <= 2 {
                return Err(PalletError::Refused(format!(
                    "'{}' is the only fallback left; retention must keep N-1 or a failed upgrade \
                     strands the node",
                    loc.name
                )));
            }
        }
        if loc.is_whole_drive() {
            return Err(PalletError::Refused(format!(
                "'{}' is a whole-drive pallet: there is no GPT entry to remove. Reformat the                  drive if that is really what you want",
                loc.name
            )));
        }
        let dev = self.store.drive(loc.drive_index)?.device.clone();
        let mut gpt = Gpt::read(&dev).await?;
        gpt.remove(loc.entry_index)?;
        gpt.write(&dev).await?;
        Ok(loc)
    }

    /// Keep the newest `keep` versions of a name and drop the rest.
    ///
    /// `keep` is at least 2 whatever the caller asks for: the active pallet and
    /// the one fallback depends on. Refcounting is pallet-aware in exactly this
    /// sense — an older pallet still pins its members.
    pub async fn prune(&self, name: &str, keep: usize) -> Result<Vec<PalletLocation>> {
        let keep = keep.max(2);
        let mut versions = self.store.find_by_name(name).await;
        versions.sort_by_key(|p| std::cmp::Reverse(p.order_key()));
        let active: Vec<Uuid> = self
            .status(None)
            .await
            .active
            .into_iter()
            .map(|a| a.id)
            .collect();

        let mut removed = Vec::new();
        for loc in versions.into_iter().skip(keep) {
            if active.contains(&loc.id) || loc.is_whole_drive() {
                continue;
            }
            let dev = self.store.drive(loc.drive_index)?.device.clone();
            let mut gpt = Gpt::read(&dev).await?;
            gpt.remove(loc.entry_index)?;
            gpt.write(&dev).await?;
            removed.push(loc);
        }
        Ok(removed)
    }
}

/// Convenience: a member whose content is a file on the host.
pub async fn file_member(
    name: impl Into<String>,
    role: impl Into<String>,
    kind: MemberKind,
    path: impl Into<std::path::PathBuf>,
) -> Result<MemberSpec> {
    let content = Arc::new(super::FileContent::open(path).await?);
    Ok(MemberSpec::new(name, role, kind, content))
}

/// Convenience: a member whose content is a volume — the golden a pallet ships
/// is a sealed clone, and it is published by being read out of the engine.
pub fn volume_member(
    name: impl Into<String>,
    role: impl Into<String>,
    kind: MemberKind,
    device: Arc<dyn crate::drive::BlockDevice>,
    byte_len: u64,
) -> MemberSpec {
    let content = Arc::new(super::DeviceContent::new(device, byte_len));
    MemberSpec::new(name, role, kind, content)
}

/// The GPT entry a pallet lives in, for callers that need the raw view.
pub fn entry_of(gpt: &Gpt, id: Uuid) -> Option<GptEntry> {
    gpt.find_by_uuid(id).map(|(_, e)| e.clone())
}
