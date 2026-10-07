# Moving a disk between nodes: ANA and epochs (#83)

A VM's disk moves from node A to node B while the VM runs. Three things make
that safe and invisible to the guest. The engine does all three on its own
side. What copies the bytes is not here: under the owner's decision on #179
(b), that is stormstorage's RAID heads, over NVMe/TCP legs from the engines.

| | what | where |
|---|---|---|
| 1 | a host with native NVMe multipath follows the move, with no unmount | ANA on the NVMe/TCP target |
| 2 | a host that missed the move cannot write after the fence | the epoch, enforced at the target |
| 3 | an export survives an engine restart on either side | the durable export table (2603646) |

## 1. ANA: which node a host should use

A volume served from two nodes is **one multipath namespace** to a Linux host
when both serve it:

- on the **same subsystem NQN**. The per-volume serve subsystems are named by
  the volume (`<prefix>:vol-<uuid>`), so they qualify. So does a shared
  subsystem that both nodes name the same (`--nvmeof-nqn`). Per-host
  subsystems (`<node nqn>:host:<hash>`) name the node, so they do not;
- at the **same NSID**, which a per-volume subsystem always is (1);
- with the **same NGUID**, which is the volume's id. A volume created on the
  second node takes the first's id: `POST /api/v1/volumes {"id": "<uuid>"}`.
  A re-headed mirror adopted from its slab keeps its id anyway;
- with **controller IDs that do not collide**. The host refuses a second
  controller of one subsystem with an ID it already has, so each node gets
  its own range: `[management] nvme_cntlid_range = [1000, 1999]`. Unset is
  1..=65519, the same on every node.

Each node then reports, per volume, an ANA state:

| state | I/O on this path | group |
|---|---|---|
| `optimized` (default) | served | 1 |
| `non_optimized` | served | 2 |
| `inaccessible` | refused: SCT 3 / SC 0x02 (the host retries on another path) | 3 |
| `persistent_loss` | refused: SCT 3 / SC 0x01 | 4 |
| `change` | refused: SCT 3 / SC 0x03 (the host waits, ANATT 10 s) | 5 |

```
PUT /api/v1/volumes/{id}/ana   {"state": "inaccessible"}    # node A
PUT /api/v1/volumes/{id}/ana   {"state": "optimized"}       # node B
GET /api/v1/volumes/{id}/ana   → {"state", "group", "change_count", "served": [{"nqn","nsid"}]}
```

A change reaches every connected host of every subsystem that serves the
volume on that node, as an ANA change notice (AEN type Notice, info 0x03, log
page 0x0C). The host re-reads the ANA log page and moves its I/O. The state
is kept in `<data_dir>/ana.json`, written before it is applied, so a node
told `inaccessible` does not come back from a restart saying `optimized`. The
node token sets it: like attach, it is an ordinary verb.

On the wire (NVMe 1.4 §8.20):
- Identify Controller: CMIC bits 0, 1, 3 (several ports, several controllers,
  ANA); OAES bit 11; ANATT 10; ANACAP 0x1F (every state; bit 6 clear, so a
  namespace changes group when its state changes); ANAGRPMAX = NANAGRPID = 5;
  NN = MNAN = 1024 (Linux sizes its ANA log buffer from MNAN). A discovery
  controller reports none of it.
- Identify Namespace: NMIC bit 0 (shared), ANAGRPID = the state's group.
- Log page 0x0C: all five groups, every one with its state, NSIDs ascending.
  `RGO` (LSP bit 0) is honoured.

Groups are by state rather than per namespace so that the log page has a
fixed size whatever NSIDs a subsystem uses. Linux follows a namespace from
group to group by the NSID lists.

## 2. The epoch at the target

The `/v1` contract fences with an epoch CAS (`POST /v1/volumes/{id}/fence
{expected_epoch}`). Until #83 that was bookkeeping. A host attached before
the fence kept writing. This is the contract stormstorage#33 set for #6:

1. **An attach carries the epoch.** `POST /v1/volumes/{id}/attach {…,
   "epoch": N}` is the epoch the caller last saw, i.e. what its last fence
   returned.
2. **A stale attach is refused.** If `epoch` is not the volume's, the answer
   is `412 {code: "stale_epoch", current_epoch}` and nothing is attached.
   Leaving `epoch` out once the volume has been fenced (epoch > 1) is refused
   the same way. Leaving it out at epoch 1 is accepted, as before.
3. **A fence revokes.** On success, and before it answers, a fence takes away
   every attachment made below the new epoch. Its namespace leaves that
   host's subsystem, or the shared one, or its ublk device goes. The answer
   says how many: `{"epoch": 2, "revoked": 1}`.
4. **Per host.** Fencing means something only when each head reaches the
   volume through a subsystem that admits that head alone (`host_nqn`, #210).
5. **Persisted.** Every attachment is recorded on the volume with its epoch
   (`GET /v1/volumes/{id}` → `attachments: [{node, host_nqn, epoch,
   transport}]`) and kept with the rest of `/v1`'s state.

What makes the fence's answer mean "no more writes" is the namespace removal:
- A command resolves its namespace once, then may wait for its R2T data
  before it reaches the device.
- Each device operation is counted in flight and checks a `revoked` flag
  inside the count. A removal sets the flag, then waits for the count to
  reach zero (both sides SeqCst).
- So a write whose data arrives after the removal is refused (Invalid
  Namespace, DNR), and one already at the device finishes before the removal
  returns.
- A removal that is still waiting after 30 s says so in the log. Device
  operations are local, so this has not happened.

An attach that was on its way while a fence ran checks the epoch again once
its data path is set up. If the fence moved it, the attach takes the path
away again and answers 412. A dual-attach `commit` revokes the old master's
attachments and keeps the migration target's, which becomes the master's at
the new epoch. `abort` takes the target's away.

Not here:
- **Per-I/O epochs and self-demotion.** A head cut off from its legs fails
  its own writes (stormstorage#33).
- **A host that reattaches as the same host NQN under the new epoch** gets
  the same subsystem. The fence only guarantees that whatever it attached
  before is gone.

## Checked

- `tests/it/integration_ana_epoch.rs`, over HTTP and real NVMe/TCP:
  - Identify and log page fields.
  - The ANA change notice.
  - `0x604` on a write to an inaccessible path.
  - `ana.json` reloaded.
  - A fence that refuses the fenced head's next write and its reconnect.
  - 412 on stale attaches.
  - The new head's data intact.
  - Epochs after a restart.
- `ci-ana-verify.sh`, on dev, with the host's own kernel as a QEMU guest:
  - Two engines serve one volume.
  - The kernel builds one multipath head with two paths and reads the ANA
    states.
  - After `PUT …/ana` on both engines it moves its reads to the other
    engine's data (the engines hold different bytes on purpose) and back,
    with no reconnect.
  - A `/v1` leg attached for the guest stops taking writes when it is fenced.
