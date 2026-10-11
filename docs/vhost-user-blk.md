# vhost-user-blk: a VM's disk served by the engine (#82)

A VMM (qemu, cloud-hypervisor, firecracker) can hand a guest's virtqueues to
the engine over a Unix socket and shared memory. The engine then serves the
guest's virtio-blk requests itself, with no kernel block device on the host.
With ublk the VMM opens `/dev/ublkbN` as a host device: every guest I/O is a
kernel block-layer round trip plus the ublk command hop. With vhost-user-blk
it is neither.

## Attaching

```http
POST /api/v1/volumes/{id}/attach
{ "transport": "vhost-user-blk", "mode": "rw" }

200 { "transport": "vhost_user_blk",
      "socket": "<data_dir>/vhost/<volume id>.sock",
      "read_only": false, "connected": false }
```

- **Asked for by name only** (`vhost-user-blk`, `vhost_user_blk`,
  `vhost-user` or `vhost`). Only a VMM can use the socket, so the engine
  never chooses it on its own.
- **This node only.** An attach for another `node` is `409`. The socket is
  `<data_dir>/vhost/<volume id>.sock`, or `/run/stormblock/vhost/` with no
  data dir.
- **`mode: ro`** offers `VIRTIO_BLK_F_RO`, and every write, discard and
  write-zeroes fails at the device, whatever the guest does. A sealed volume
  takes only `ro`, as on every transport.
- **Idempotent.** Attaching again in the same mode returns the same socket.
  The other mode is `409`: the guest that may be connected negotiated the
  first. Detach first.
- **Held**: the volume cannot be deleted while it is attached (`409 … a
  vhost-user-blk socket`, #267).
- **`GET …/attach`** reports the socket, `connected` (a VMM is on it now)
  and `requests` served. **`DELETE …/attach`** disconnects the VMM and
  removes the socket. A daemon stop does the same for every socket.
- **`/v1`** (the CSI contract) refuses the transport (`400`): it is the
  volume API's.

QEMU, for example (guest RAM must be shared memory):

```sh
qemu-system-x86_64 … \
  -object memory-backend-memfd,id=mem,size=4G,share=on -numa node,memdev=mem \
  -chardev socket,id=disk0,path=<socket> \
  -device vhost-user-blk-pci,chardev=disk0,num-queues=4
```

## What the device is

- virtio-blk with `VERSION_1`, `SEG_MAX` (128), `BLK_SIZE` (the volume's LBA:
  512 or 4096, #228), `FLUSH`, `DISCARD` and `WRITE_ZEROES` (up to 1 GiB
  each, one segment), `MQ` (up to 8 queues), and `RO` when read-only.
  Capacity is the volume's size in 512-byte sectors. `GET_ID` answers the
  volume's id.
- Requests: IN, OUT, FLUSH (the volume's flush: durable when answered),
  GET_ID, DISCARD, WRITE_ZEROES. Anything else is `UNSUPP`. A request past
  the end of the disk, or with a buffer outside the guest's memory, fails
  that request only.
- Protocol features: `MQ`, `REPLY_ACK`, `CONFIG` (QEMU's vhost-user-blk
  needs it).
- One VMM at a time. A second VMM that connects while one is connected is
  closed at once, because one guest owns a disk. A VMM that disconnects and
  connects again (QEMU `reconnect=`) negotiates afresh and is served.
- Each queue is served by a task on the engine's runtime, woken by its kick
  eventfd. The requests in an available batch run at once. A new memory
  table (memory hotplug) restarts the queues on it.

## Not offered

Stated so these are decisions, not omissions:

- **The dirty log** (`VHOST_F_LOG_ALL`, `LOG_SHMFD`). QEMU therefore blocks
  a live migration of a guest that uses this device rather than migrating
  it unsafely. Live migration from the disk side is its own issue.
- **Inflight tracking** (`INFLIGHT_SHMFD`): a backend restart while a guest
  runs fails the guest's in-flight requests instead of replaying them.
- **Indirect descriptors and event index**, and `CONFIGURE_MEM_SLOTS`.
- **Zero copy**: a request's data is copied between guest memory and the
  volume once.

## Where it is

`src/drive/vhost_user_blk.rs` (the backend; its tests are an in-process
frontend: memfd guest memory, a split ring, eventfds) and
`src/mgmt/vhost_export.rs` (one socket per volume). `ci-vhost-user-blk-verify.sh`
runs a real guest: the host's kernel in QEMU with `vhost-user-blk-pci`, two
queues, read-write and then read-only.
