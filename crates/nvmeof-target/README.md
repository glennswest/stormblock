# stormblock-nvmeof

NVMe/TCP for the stormblock engine, and for a laptop that boots nodes
(stormblock#474, storminstall#20, stormbootx#164).

## Two halves

**The wire half**, shared with the engine. It covers PDU framing and digests
(`pdu`), fabric commands and controller properties (`fabric`), the discovery
log page (`discovery`), DH-HMAC-CHAP (`auth`), and the Identify data and admin
opcodes (`identify`). The engine's target (`src/target/nvmeof`) re-exports
these and builds its own Identify answers on them, adding only what the
engine alone has (ANA, #83). So the engine and a laptop speak one copy, and
the two cannot drift. `ci-nvmeof-identify-same.sh` (repo root) checks that
every Identify answer the engine gives is byte for byte what it gave before
the move.

**A portable boot target.** It serves one image file to named hosts, over
tokio and plain std file I/O only. There is no io-uring, nix, libc, CPU
pinning or kernel module, so it builds for macOS, Windows and Linux
(`ci-bootserve-cross.sh`).

- `image::Image`: the file, opened read-only and never written. Its length
  need not be whole blocks (an ISO): the namespace rounds up, and the tail
  reads as zeros.
- `overlay::Overlay`: one host's copy-on-write overlay. Every write the host
  makes lands in a scratch file of its own, holding only the blocks written
  (append-only, so it is small on any filesystem, NTFS included). Write Zeroes
  and Deallocate take no room. The file goes when the overlay is dropped.
  The boot path writes to the disk it was handed: stormuefi counts boot
  attempts in the GPT, a diskless node keeps its data there, and an install
  discards what it moved. A read-only namespace would fail all of these.
- `server::BootTarget`: each host gets a subsystem of its own,
  `<base>:host:<name>`, that admits only `nqn.2026-09.lo.storm:host-<name>`
  (what stormbootx and the initramfs present, as forge does since #210).
  Discovery lists to a host only its own subsystem. A claim adds a namespace
  over a fresh overlay, and the newest two claims are kept (stormbootx's and
  the initramfs's, one boot). The target serves Identify, log pages,
  features, keep-alive, Read, Write (with R2T), Flush, Write Zeroes and
  Dataset Management. Left out on purpose: DH-HMAC-CHAP (a boot host is
  handed no secret), ANA, asynchronous events and the engine's reactor.
- `boothost::Boothost`: the HTTP surface stormbootx and the initramfs's
  `boot-claim` call before attaching, as an axum router:
  - `GET /api/v1/health`
  - `GET /api/v1/synonyms/boothost/<name>/intent`
  - `POST /api/v1/synonyms/boothost/<name>/claim`. `default` and a MAC key
    resolve as on forge, and the reply has `attach` as its first key.
  - `POST /api/v1/synonyms/boothost/<name>/installed`. A laid install moves
    the host to `local`.
  - `GET /api/v1/boothost`: the hosts, their claims and each overlay's size.

## `stormboot-serve`

```
stormboot-serve --image release.img --host server8 [--host server7=0c:c4:7a:06:f9:6d]
                [--any-host] [--intent install|local|auto]
                [--listen 0.0.0.0] [--nvme-port 4420] [--http-port 9090]
                [--advertise <address>] [--scratch <dir>] [--block-size 4096|512]
```

It serves the image until Ctrl-C, then removes every overlay. A node's
stormbootx media names this machine as its boothost. stormbootx#164 has the
app make that media, with this machine's address and the node's
install-config.

Library users (storminstall) take it with `default-features = false`, which
leaves out the CLI (clap and the log subscriber).

## Tests

- `cargo test -p stormblock-nvmeof`: the wire half's own tests, the overlay,
  Identify and MAC spellings.
- `tests/it/integration_bootserve.rs`: the engine's NVMe/TCP initiator against
  the target, over the boothost API.
- `ci-bootserve-verify.sh`: the Linux kernel's nvme-tcp in QEMU as the node.
- `ci-bootserve-cross.sh`: macOS (x86_64, aarch64) and Windows builds.
