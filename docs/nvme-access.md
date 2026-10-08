# Who may connect over NVMe/TCP (#210)

Until #210 a node served every volume it exported as a namespace of **one**
subsystem that admitted **any** host. pve connected to forge's `:4420` with no
arrangement and saw 71 namespaces: every sealed golden, every OS release and
every machine's boot clone, read-write. Now:

- **A volume is served to a named host**, from a subsystem of that host's own,
  whose only allowed host is that host's NQN. A host sees exactly what was
  attached to it.
- **The shared subsystem** (`--nvmeof-nqn`) **admits no host** unless the
  configuration says otherwise.
- **A sealed golden is never on the shared subsystem**, and is served
  write-protected to the one host that reads it.
- **DH-HMAC-CHAP**: a host can be given a secret it must prove it holds, so
  presenting another host's NQN is not enough.

## What a host sees

One listener (`:4420`) carries several subsystems:

| subsystem | admits | holds |
|---|---|---|
| `<nqn>` (the shared one, `--nvmeof-nqn`) | nobody; or `[nvmeof] allowed_hosts`; or anyone with `allow_any_host = true` | raw drives (`export_drives`), and volumes attached without a host |
| `<nqn>:host:<16 hex>` | the one host NQN it was made for (hex = SHA-256 of that NQN) | what was attached, exported or claimed for that host |
| `<nqn>:host:<name>` | a boot host's NQNs: `nqn.2026-09.lo.storm:host-<name>` and the same for each alias and the tag it claimed as | that machine's boot clone |

- **Connect** to a subsystem the host is not allowed on is refused with
  *Connect Invalid Host* (status type 1, code 0x84, Do Not Retry); to a
  subsystem the node does not have, *Connect Invalid Parameters*.
- **Discovery** is open to every host, and lists only the subsystems the
  asking host may connect to — a host given nothing is told of nothing.
- A host subsystem goes when its last namespace does (a boot host's stays, with
  its hosts and secret, for the next claim).
- **One volume is never two namespaces of one subsystem.** The namespace's
  NGUID is the volume id; two NSIDs with one NGUID is what the kernel reports
  as `duplicate IDs in subsystem`. Attaching a volume that is already served
  there answers with the NSID it has.

## Attaching for a host

Every door that serves a volume over NVMe/TCP takes the host:

```bash
# /api/v1 — any engine volume
curl -X POST http://node:9090/api/v1/volumes/<uuid>/attach \
  -d '{"transport":"nvme-tcp","host_nqn":"nqn.2014-08.org.nvmexpress:uuid:…","dhchap":true}'
# → {"transport":"nvme_tcp","nqn":"nqn.…:host:1f2e…",
#    "addresses":[{"traddr":"10.0.0.5","trsvcid":4420}],"nsid":1,
#    "host_nqn":"nqn.2014-08…","dhchap_secret":"DHHC-1:01:…:"}

# /v1 (the CSI contract): the same two optional fields on attach
curl -X POST http://node:9090/v1/volumes/<id>/attach \
  -d '{"node":"n1","mode":"read_write","transport":"nvme_tcp","host_nqn":"nqn.…"}'

# exports: nqn and port are in the reply now
curl -X POST http://node:9090/api/v1/exports \
  -d '{"volume_id":"<uuid>","protocol":"nvmeof","host_nqn":"nqn.…"}'

# a claim other than a boot claim
curl -X POST http://node:9090/api/v1/synonyms/<ns>/<name>/claim -d '{"host_nqn":"nqn.…"}'
```

- `host_nqn` must start `nqn.` and fit in 223 bytes.
- **Without `host_nqn`** the volume goes on the shared subsystem — refused
  (400, naming `host_nqn`) when the shared subsystem admits no host, and
  always refused for a sealed golden.
- `mode: "ro"`, and any sealed volume, is a **write-protected namespace**:
  Identify Namespace sets NSATTR bit 0, which makes the host's block device
  read-only, and writes are refused with *Namespace is Write Protected*.
- `GET /api/v1/volumes/{id}/attach` lists the host subsystems serving the
  volume (`hosts`); `DELETE …/attach?host_nqn=…` withdraws it from one host,
  plain `DELETE` from every host.
- A volume served to a host is busy: deleting it is refused while it is
  (`what_is_serving` names the subsystem).
- **Holders** (#276). One volume attached to one host is one namespace (one
  NSID per volume, the NGUID is the volume's id). When several users of that
  host attach it — two builds on one build box reading the same input golden
  — each names itself as `holder` on the attach
  (`{"transport":"nvme-tcp","host_nqn":"…","holder":"job-1234"}`) and on the
  detach (`DELETE …/attach?host_nqn=…&holder=job-1234`); the namespace stays
  until the last holder releases it, and the detach's reply says whether it
  went (`namespace_removed`). An export holds its namespace as
  `export:<id>`. An attach that names no holder is the anonymous holder, so
  callers that send none behave as before — but then one detach takes the
  namespace from every one of them. A plain `DELETE …/attach` (every host),
  a volume being deleted and a released boot clone take it from every
  holder.
- **No lease, no idle timeout.** An export, and the connection a host makes
  to it, last until they are deleted or the host disconnects; the target
  answers keep-alives and enforces no keep-alive timeout of its own. A
  connection that ends is logged with its reason (`host_closed`, `reset`,
  `protocol_error`, `io_error`, …), its host, controller, queue, how long it
  lived, how many commands it served and its last command (#276).

## Boot claims

A boot claim (`POST /api/v1/synonyms/boothost/<tag>/claim`) takes no options,
and needs none: stormbootx presents `nqn.2026-09.lo.storm:host-<name>`, with
`<name>` the host name the claim reply gives. The engine serves the clone from
`<nqn>:host:<name>`, admitting that NQN, the same for each of the host's
aliases and for the tag it claimed as. The reply's `attach` carries the
subsystem, port, NSID and `host_nqns`. The next boot's clone lands in the same
subsystem and the released one leaves it. No firmware change is needed. The
template is `[nvmeof] boothost_host_nqn` (`{name}` is replaced).

On a node without the shared listener (no NVMe-oF target at startup), a claim
falls back to the volume's own subsystem on a `[serve]` portal, with the same
allowed hosts.

## DH-HMAC-CHAP

`"dhchap": true` on an attach (or `[nvmeof] require_dhchap = true` for every
host) gives the host a secret, returned once as `dhchap_secret` in the form
`nvme connect --dhchap-secret` takes (`DHHC-1:01:<base64>:`, a 32-byte key
transformed with SHA-256). From then on every queue that host opens must
authenticate before anything else runs:

```bash
nvme connect -t tcp -a 10.0.0.5 -s 4420 -n nqn.…:host:1f2e… \
  --hostnqn nqn.2014-08.org.nvmexpress:uuid:… --dhchap-secret 'DHHC-1:01:…:'
```

- The Connect response sets **ATR** (bit 17); until the exchange succeeds, any
  command but Authentication Send/Receive is answered *Authentication
  Required* (status type 1, code 0x91).
- **NULL DH group, SHA-256/384/512, unidirectional.** Linux offers the NULL
  group whenever it is not asked for secure concatenation. A host that asks
  the controller to authenticate too (`--dhchap-ctrl-secret`) is refused, as
  Linux's own target refuses it without a controller key.
- A wrong secret, or none, ends the queue after *Failure1*.
- A host keeps its secret: a later attach without `dhchap` does not remove it.
- The engine's own initiator (`nvme-tcp://` drives, `image build`'s golden
  reader) answers with the secret its spec carries, or
  `$STORMBLOCK_DHCHAP_SECRET`. A secret never goes in a URI.
- A drive's own secret (#213) goes beside its path:
  - over HTTP, `POST /api/v1/drives {path, dhchap_secret}`;
  - in config, `[[drives]] dhchap_secret` or `dhchap_secret_file`.

  It is kept with the open drive (reconnects use it) and never echoed or
  written down; `GET /api/v1/drives` says `dhchap: true`. This is how a RAID
  head attaches a leg whose host was given a secret by the leg's engine
  (`dhchap: true` on its attach).
- Wire format and every HMAC input are Linux's
  (`drivers/nvme/{common,host,target}/auth.c`); the HMAC is pinned by RFC 4231
  vectors.

## Checked with the Linux kernel as initiator

`ci-nvme-hosts-verify.sh` (run on dev: `sc-build 'bash ci-nvme-hosts-verify.sh'`)
needs no root. It starts the engine on dev with volumes attached for named
hosts, then boots dev's own kernel in QEMU from an initramfs holding
nvme-cli and the nvme-tcp/nvme-auth modules, and runs what an operator would:
`nvme discover` as a host given nothing (no subsystem listed) and as H1 (its
own only); `nvme connect` to the shared subsystem and to another host's
subsystem (the kernel logs *Connect for subsystem … is not allowed*); H1's
clone read/written and its golden a read-only block device that refuses a
write; H2 without its secret (`no key`), with another host's (`authentication
failed`), and with its own (`authenticated with hash hmac(sha256) dhgroup
null`).

## Configuration

`[nvmeof]`:

| key | default | |
|---|---|---|
| `allow_any_host` | `false` | the shared subsystem admits any host (the old behaviour; said on every boot) |
| `allowed_hosts` | `[]` | host NQNs the shared subsystem admits otherwise |
| `require_dhchap` | `false` | every host of a host subsystem gets a secret — except boot hosts: a claim is unauthenticated and firmware cannot be handed a secret, so a boot subsystem is bound by NQN only |
| `boothost_host_nqn` | `nqn.2026-09.lo.storm:host-{name}` | what a boot host presents |

## Files

`<data_dir>/nvme_hosts.json` (mode 0600 — it holds the secrets): every host
subsystem, its hosts and their secrets, and its namespaces with their NSIDs.
Put back on the target at startup, before anything is served, because an NQN
and an NSID are an address a machine has written down. A volume that is gone,
or an NSID that is taken, is dropped from the record with a warning.

Attach records on the shared subsystem (`/v1` and `/api/v1` attaches without a
host) are restored at startup too. They were persisted and never put back, so
after a restart a record named an NSID that nothing served — until the next
attach of *another* volume took it, and the first volume's consumer read that
one. A record for a sealed golden is dropped; so is an export of one (left
recorded, `pending_restart`, to be re-exported for a host).

## Rollout

The shared subsystem closing is a change for every caller that attaches without
naming a host. On a node whose clients do not send `host_nqn` yet, set
`allow_any_host = true` (it is logged loudly) until they do. Boot claims need
nothing from the firmware. Callers: stormcentral (`sc-build-out`),
stormstorage (RAID legs), rustkube-node, stormblock-csi, stormvm.

**`/serve/v1` serves its own exports only (#217).** Its reconciler wires the
exports `/serve/v1` made (`ExportEntry.serve`; an entry from before the mark
is recognised by serve's per-volume name, `<nqn_prefix>:vol-<volume>`, and no
host binding). An export made through `/api/v1/exports` stays the engine's:
- a host-bound one only on its host's subsystem, at its own NSID;
- a shared one only on the shared subsystem, at its own NSID;
- a wiring row an earlier engine made for one drains, closing its portal.

The engine, for its part, does not restore serve's exports (NSID 1 of their
own subsystem) onto its shared subsystem at start.

Not covered here: `/serve/v1`'s own per-volume subsystems still admit any
host (#212).
