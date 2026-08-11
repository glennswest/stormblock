# Connection and protocol overhead: iSCSI vs NVMe-oF/TCP

Measured 2026-08-11 against stormblock 7.1.0 serving both targets from one
node, with real kernel initiators (`iscsi_tcp`, `nvme_tcp`).

**Topology.** Target `sb-node1` (192.168.8.181), initiator `sb-node2`
(192.168.8.182), both Proxmox VMs on pve.g8.lo, 2 vCPU / 2 GB, over the
192.168.8.0/24 network. Target exports the same 4 KiB-block volume on
`:3260` (iqn.2024.io.stormblock:sb-node1) and `:4420`
(nqn.2024.io.stormblock:sb-node1). 10 iterations per measurement.

Harnesses: `attach-bench.sh`, `hotadd-bench.sh`.

---

## Headline: it is not seconds versus milliseconds

Both protocols attach in **tens of milliseconds**. The gap is real and
consistent — iSCSI costs about 2.6× more — but it is 91 ms versus 35 ms, not
seconds versus milliseconds.

| | min | p50 | p95 | max |
|---|---|---|---|---|
| **NVMe-oF attach (total)** | 30.7 | **35.0** | 39.1 | 39.9 |
| **iSCSI attach, cold** | 79.5 | **91.2** | 102.5 | 104.1 |
| **iSCSI attach, warm** | 72.5 | **75.5** | 86.1 | 90.2 |

*(ms; "cold" includes SendTargets discovery, "warm" reuses a cached node
record.)*

If you are seeing seconds, it is almost certainly **not** the steady-state
protocol path — see [Where seconds actually come from](#where-seconds-actually-come-from).

---

## Where the time goes

Broken down by phase, the interesting result is that **the protocol handshakes
cost almost exactly the same**:

| phase | NVMe-oF | iSCSI |
|---|---|---|
| discovery | — (not required) | 13.5 |
| login / connect | **21.0** | **20.6** |
| device usable after login | **14.0** | **55.7** |
| teardown | 46.3 | 37.7 (logout) |

*(ms, p50.)*

iSCSI login (20.6 ms) and NVMe-oF connect (21.0 ms) are within noise of each
other. Wire volume agrees — capturing a full setup+teardown cycle:

| | packets | data-bearing | TCP connections |
|---|---|---|---|
| NVMe-oF | 218 | 143 | 3 (admin + 2 I/O queues) |
| iSCSI | 255 | 179 | 2 (discovery session + normal session) |

So the difference is **not** the number of round trips on the wire. It is:

1. **Device materialisation — 55.7 ms vs 14.0 ms.** After iSCSI login the
   kernel runs a SCSI bus scan and the `sd` probe sequence (INQUIRY, VPD pages,
   READ CAPACITY, MODE SENSE...), then udev processes the new device. NVMe-oF
   identifies namespaces directly from the admin queue with no SCSI layer to
   emulate. Four times the cost, and it is host-side work, not network.
2. **Discovery is a separate session.** iSCSI opens a whole TCP connection,
   logs in, runs SendTargets, and logs out again *before* the real session
   starts — 13.5 ms and a second connection that NVMe-oF simply does not need.

---

## The part that actually matters: cost per volume

The numbers above are for one volume on a fresh connection. The workload that
drives the registry design is *N volumes*, one CoW clone per container. There
the two models diverge structurally.

NVMe-oF connects **once** and hot-adds a namespace per volume, with no
reconnect and no rescan (issue #26). Measured on an already-connected
controller:

| | min | p50 | max |
|---|---|---|---|
| create (control plane) | 20.0 | 28.9 | 33.0 |
| attach call | 16.8 | 17.8 | 23.4 |
| **per volume, until usable** | 20.0 | **21.7** | 26.9 |
| **per volume, detach** | 28.0 | **32.7** | 43.5 |

Eleven namespaces ended up on one controller over three TCP connections.

Extrapolating to 1000 containers on a node:

| | NVMe-oF (hot-add) | iSCSI (session per volume) |
|---|---|---|
| setup | 28 ms once + 1000 × 21.7 ms ≈ **21.7 s** | 1000 × 75.5 ms ≈ **75.5 s** |
| TCP connections | **3** | ~2000 |
| kernel sessions | 1 controller | 1000 sessions |

That is the argument for NVMe-oF, and it is about the *shape* of the model —
one controller with many namespaces versus one session per LUN — far more than
about per-operation microseconds. The alternative iSCSI shape (one session,
many LUNs, rescan per addition) trades session count for a SCSI rescan on every
add, which is the 55.7 ms path above and gets worse as the LUN count grows.

---

## Why NVMe for current systems, beyond connect time

Connection cost is the smaller half of the argument. The I/O path is the rest:

- **Queueing.** A SCSI/iSCSI session carries a single command window: every
  command takes a CmdSN, and the target advertises `MaxCmdSN`/`ExpCmdSN` as one
  sliding window shared by the whole session. Parallelism has to come from
  MC/S (rarely deployed well — stormblock's own gap is #31) or from multiple
  sessions plus dm-multipath. NVMe defines up to 64K queues of up to 64K
  entries, conventionally one queue pair per CPU core, with no cross-queue
  ordering and no shared sequence number. This node negotiated 2 I/O queues for
  2 vCPUs; stormblock grants `min(requested, max_io_queues)` (#27).
- **No translation layer.** iSCSI carries SCSI CDBs, so the target emulates
  INQUIRY, VPD pages, MODE SENSE, REPORT LUNS and the rest — and the initiator
  pays a SCSI probe per device. NVMe commands map to the operations a flash
  device actually performs.
- **Thin provisioning is native.** NVMe DSM/deallocate is one command; the iSCSI
  equivalent needs VPD 0xB2 to advertise LBPU/LBPME/LBPRZ before Linux will
  even issue UNMAP — the omission of which was the root cause of #25's
  monotonic growth.
- **Multipath.** ANA is part of the protocol; iSCSI needs dm-multipath and
  ALUA glued on top, with its own settle time.
- **Hot-add.** Namespaces can appear and disappear on a live controller via
  AEN + Changed Namespace List. iSCSI's equivalent is a LUN rescan.

---

## Where seconds actually come from

This study did **not** reproduce second-scale attach times on either protocol,
so if the production observation is seconds, the cause is somewhere the steady
state does not exercise. The realistic candidates:

- **iSCSI timeout and retry paths.** `node.conn[0].timeo.login_timeout`
  defaults to 15 s and `node.session.timeo.replacement_timeout` to 120 s. One
  dropped login or a target that stalls mid-negotiation turns a 20 ms operation
  into a multi-second one. This is the single most likely source.
- **Serialisation through `iscsid`.** Attaches funnel through one daemon;
  concurrent container starts queue behind each other rather than overlapping.
- **udev and multipath settle under load.** The 55.7 ms device-ready figure is
  for an idle host with one device. udev's queue is shared, and `multipathd`
  adds its own settle before a usable path exists.
- **Discovery against a portal advertising many targets.** SendTargets returns
  every target; a login per returned target multiplies the 91 ms figure.
- **Layers above the transport** — CSI, kubelet volume manager, and their own
  retry backoffs, which are typically measured in seconds by design.

Distinguishing these needs a trace of the slow case rather than a
steady-state benchmark: `iscsiadm -m session -P 3`, `journalctl -u iscsid`, and
timestamps around the CSI NodeStage/NodePublish calls would localise it
quickly.

---

## Caveats

- 2 vCPU / 2 GB VMs with file-backed drives. Absolute numbers are a floor;
  the *ratios* are the useful part, and the device-materialisation gap is
  host-side work that would persist on faster hardware.
- Same-subnet 1 GbE-class virtual networking, sub-millisecond RTT. On a link
  with real latency, the round-trip counts above (218 vs 255 packets) matter
  more than they do here.
- Single volume per attach, idle host. Contention effects — the ones most
  likely to produce the reported seconds — are explicitly *not* covered.
- stormblock 7.1.0. The v8.x extent-GC and refcount work does not touch either
  target's connect path.

## Reproducing

```bash
# target: run stormblock with both targets enabled
stormblock --iscsi-addr 0.0.0.0:3260 --iscsi-target-name iqn.2024.io.stormblock:sb-node1 \
           --nvmeof-addr 0.0.0.0:4420 --nvmeof-nqn nqn.2024.io.stormblock:sb-node1 ...

# initiator
sudo dnf install -y iscsi-initiator-utils nvme-cli tcpdump
sudo modprobe iscsi_tcp nvme-tcp && sudo systemctl start iscsid
ITER=10 ./attach-bench.sh     # full attach, both protocols
ITER=10 ./hotadd-bench.sh     # per-volume cost on a live controller
```

Note that both harnesses probe device readiness with a 4 KiB O_DIRECT read:
these volumes present 4 KiB logical blocks, and a 512-byte direct read fails
with EINVAL on both transports.
