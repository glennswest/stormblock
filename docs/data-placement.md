# Where a claim lives, and how it follows a workload

Design, 2026-08-28. Not implemented. Records a decision about the default
durability shape and what has to be true before it is safe.

## The default: RAID 1, one leg local, one leg on an appliance

A data pallet's volumes are mirrored between the **node's own drive** and a
**storage appliance**. Both legs are real copies; neither is a cache.

That split is doing two different jobs, which is why it is the default rather
than one of several equal options:

- **The local leg is for speed.** Reads are served from the drive in the
  machine, so the common path touches no network.
- **The appliance leg is the anchor.** It is the copy that does not belong to
  any host, and therefore the copy a workload can be reunited with somewhere
  else.

## Moving a workload is adding a leg and dropping one

Because the appliance leg is host-independent, a move never copies host to
host:

```
   before          during                         after
  ┌────────┐     ┌────────┐   ┌────────┐        ┌────────┐
  │ host A │     │ host A │   │ host B │        │ host B │
  │  local │     │  local │   │  local │        │  local │
  └───┬────┘     └───┬────┘   └───┬────┘        └───┬────┘
      │ mirror       │            │ rebuild         │ mirror
  ┌───┴────┐     ┌───┴────────────┴───┐        ┌────┴───┐
  │appliance│    │     appliance      │        │appliance│
  └────────┘     └────────────────────┘        └────────┘
                  three legs, briefly
```

1. Host B adds its local drive as a member, rebuilding from the appliance.
2. When that leg is genuinely in sync, host A's local leg is removed.
3. The appliance leg never moved and never stopped serving.

**Draining a node is therefore quiesce and move**, not evacuate-and-copy: the
data does not travel between hosts at all, so a drain costs one rebuild per
workload against an appliance that was already holding a copy.

The mechanism exists. `RaidArray::add_member` / `remove_member` are
implemented for RAID 1, and `migrate.rs` already orchestrates exactly this
shape for remote → local.

## What must be fixed first, and it is not optional

**`migrate_to_local` treats a failed rebuild as a completed one**
(stormblock#69). The wait loop is:

```rust
let all_active = states.iter().all(|(_, s)| s == "Active");
if all_active { break; }                    // genuine success
let rebuilding = states.filter(|s| s == "Rebuilding");
if rebuilding.is_empty() { break; }         // ← anything else also breaks
...
tracing::info!("Migration: rebuild complete");   // claimed unconditionally
// remove the remote member
```

Any member state that is neither `Active` nor `Rebuilding` — `Failed`
included — leaves the loop, logs completion, and **removes the remote
member**. In this design the remote member is the appliance leg: the only copy
that is not on the host being drained. A failed rebuild would therefore delete
the surviving copy after an incomplete write.

The companion faults make it worse rather than unlikely: nothing ever marks a
member `Failed` in production (`set_member_state` is test-only), and a rebuild
that errors returns silently leaving the member `Rebuilding` forever — so the
same loop's other branch hangs a drain with no timeout.

**Nothing in this document is safe to build until #69 is closed.** The
requirements are small and specific:

- a member that fails is *marked* failed;
- the wait ends on an explicit outcome — synced, failed, or timed out — never
  on the absence of a state;
- the old leg is removed only after a positive statement that the new one
  holds every extent;
- a timeout that ends the drain rather than the day.

## The balancer

Placement across data pallets is not a ladder. `select()` answers "which one
pallet wins", which is correct for boot, kernel and system, where exactly one
does. Data pallets are a **pool**: a claim is placed *into* one, and later
moved when the pool is out of balance.

Two jobs, deliberately separate:

- **Placement** — where a new claim goes. Synchronous, on the claim path, and
  must be cheap.
- **Rebalancing** — moving existing claims when the pool has drifted.
  Background, interruptible, and never urgent enough to disturb a workload
  that is meeting its needs.

Inputs, in the order they are worth having:

1. **Free capacity per data pallet.** Enough on its own to start, and the only
   input that exists today.
2. **Which drive an extent is on.** `SlabRegistry` indexes by tier only and
   cannot answer this (stormblock#70) — so nothing today can tell whether two
   legs of a mirror are on the same physical drive, which is the one thing a
   mirror must never be.
3. **Failure domain** — bay, shelf, controller, node, site (#71, #72).
   stormdrive already collects the physical half (SES enclosure/bay, PCIe
   slot, SAS address) and stormblock has none of it. This is an integration,
   not an invention.
4. **Wear and health**, also from stormdrive: prefer a young drive for a new
   claim, and drain a drive whose projected life is short before it is urgent.

**The rule the balancer exists to keep** is that the two legs of a mirror are
never in the same failure domain — and today the system cannot even express
that they are on the same drive. That is why #70 is the first dependency and
not a nicety.

## What this does not give

A data pallet is a **hard allocation boundary**, and that is all it is. It is
not durability: a slab has no redundancy of its own, and a dead drive takes
its extents with it unless the volume is mirrored or replicated. The mirror
above is what makes a claim survive a drive; the pallet is what stops a golden
and a claim from competing for the same space.
