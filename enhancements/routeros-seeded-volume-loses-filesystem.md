# RouterOS-written volumes lose their filesystem — engine exonerated

**Filed by:** mkube (2026-08-10), from the CoW image-catalog work on rose1.
**Applies to:** stormblockmk 0.3.0 (stormblock 7.1.0), RouterOS 7.22.2,
file-backed slab, iSCSI loopback.

**Status: NOT a stormblock bug.** An earlier revision of this file blamed
partial 4 MB slab-slot durability. A direct data-path test disproved that —
kept here because the measurements are useful and the theory was wrong.

## The data-path test (all legs passed)

Known pattern, each 4 KB block stamped with its own LBA, written at LBA 8192
through our own initiator (`iscsi-pvc`, SCSI WRITE(10) + SYNCHRONIZE CACHE):

| Leg | Result |
|---|---|
| verify in a fresh iSCSI session | 64/64 good |
| withdraw export, re-export, verify | 64/64 good |
| write into an fstemplate's raw volume, verify | 64/64 good |
| **seal → clone `from_template` → verify** | **64/64 good** |

So: writes are durable, partial slots are durable, snapshots capture them,
and clones reproduce them byte for byte. Thin CoW cloning is sound.

## What actually fails

Only writes made by **RouterOS itself** — a container `add` whose `root-dir`
sits on a mounted stormblock volume:

- allocation climbs to the full image size (60 MB for `nats:edge`), so bytes
  do reach the engine;
- afterwards the volume **will not mount** (`fs=-` on the disk row);
- a clone of the sealed template mounts as valid ext4 and is **empty**
  (`ls -la` in a container shows only `.` and `..`).

Ruled out by measurement: 45 s writeback settle; restoring the ext4 clean
flag; `/disk/eject` (hardware disks only); `/disk/unmount` (absent on
7.22.2); `/disk/disable` (accepted, no effect); SCSI SYNCHRONIZE CACHE from
our own initiator (succeeds, no effect); duplicate filesystem identity
between golden and clone (fixed independently — clones now get their own
UUID/label/state).

## Where that points

RouterOS does not appear to produce a coherent ext4 on a *network* disk when
extracting a container image onto it. Note it also never mounts an NVMe-TCP
disk at all (`fs=-` even after a verified-good format), which suggests its
handling of network-attached block devices is generally shallower than for
hardware disks. Candidate factors worth checking on the RouterOS side:
4096-byte logical blocks, and whether container extraction bypasses or
mishandles the writeback path for these devices.

## Relevance to stormblock-registry

Good news: the registry's model is unaffected. sbregistry writes layers into
the volume **through stormblock**, which this test shows is durable end to
end, including through seal and clone. There is no partial-slot hazard to
design around. The unusable path is specifically "let RouterOS write the
filesystem", which sbregistry does not do.
