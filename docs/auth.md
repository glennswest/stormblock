# Who may call a node's API

A stormblock node's management API is not a status page. Anything that can
reach it can create, clone, seal and **delete** volumes, add and withdraw
exports, publish and unpublish releases, and re-point a synonym — and
re-pointing `boothost/<tag>` decides which image a machine boots at its next
power cycle. It listens on `0.0.0.0:9090` by default.

This page is what guards it (issue #107).

## The short version

**Closed by default** (v17.0.0). A node mints a token at boot, writes it to
`<data_dir>/api_token` (or `management.token_file`) mode `0600`, keeps it
across restarts, and requires it on every request:

```bash
curl -H "Authorization: Bearer $(cat /var/lib/stormblock/api_token)" \
     http://node:9090/api/v1/volumes
```

Anything else gets a 401 — with two kinds of exception: the health and
readiness probes, and **one write**, a machine claiming its own boot image
(`POST /api/v1/synonyms/boothost/<tag>/claim`), which is safe to leave open
because of what it cannot do (§ below). A node that must be open says
`require_auth = false`, and says so on every boot.

## What the settings mean

| setting | effect |
|---|---|
| `management.api_token` | The token, named in the config. Accepted for everything. |
| `management.admin_token` | When set, destructive verbs need **this** one and `api_token` is not enough. |
| `management.token_file` | Where a minted token is kept. Defaults to `<data_dir>/api_token`, then `/etc/stormblock/api_token` if `/etc/stormblock` is a directory; with neither, the token is kept in memory only. |
| `management.require_auth` | Unset or `true` — required, minting one if there is none. `false` — deliberately open. Unset with nowhere to keep a minted token: closed anyway, with a token held in memory only, and a warning; `true` in that case fails startup. |

`$STORMBLOCK_API_TOKEN` and `$STORMBLOCK_ADMIN_TOKEN` are read when the config
names neither.

`require_auth = true` with nowhere to keep a token — no `token_file`, no
`data_dir`, no `api_token` — **fails startup**. Left unset, the same node
closes with an in-memory token nothing else can present, rather than stop a
node booting over its own config. Falling back to open is the one thing
neither may do.

`$STORMBLOCK_TOKEN_FILE` tells the CLI where a local token is; the CLI
(`image build --engine http://127.0.0.1:…`) reads it, then
`/etc/stormblock/api_token`, then `/var/lib/stormblock/api_token`, and only
ever presents a minted token to an engine on its own machine.

### Destructive verbs (#274, stormcos#250)

On a stormcos node the node token is mounted read-only into every engine
caller, so it cannot be what decides whether a drive is formatted. Since #274
(owner's choice **B**), the node token covers the **ordinary** verbs, and a
**destructive** one needs the **admin token** or a **Kubernetes bearer** that
a SubjectAccessReview allows. `serve::api::classify` decides which is which.

**Ordinary**, on the node token:
- reads;
- creating, cloning and attaching volumes;
- deleting a volume that is unsealed and is no template;
- detach-like DELETEs: a volume's attach, an export, a LUN, a rebuild, a
  drain, a scrub, a StormFS pin, and `/v1`'s volumes, snapshots and group
  snapshots, which a CSI controller deletes as Kubernetes objects go.

**Destructive:**
- slabs: `POST /api/v1/slabs` (format), `DELETE`, and a GC that is not a dry
  run;
- arrays: create, delete, and members added, failed or replaced;
- spares;
- forge on and off (`PUT`/`DELETE /api/v1/forge`);
- the pallet verbs that write a partition table (`gpt`, `convert`, `prune`,
  `adopt`);
- an emulated drive's fault (`/emulate`);
- deleting a sealed volume (a golden, a blank, a snapshot) or a template;
- sealing or unsealing;
- writing files or a tar into a filesystem;
- `?repair=true` on an fsck and `?apply` on a trim;
- a boot intent;
- every other `DELETE`.

**The admin token** is never under `/run/stormblock`, which every service
mounts. It comes from `management.admin_token` or `$STORMBLOCK_ADMIN_TOKEN`.
Without either, it is read from, or minted at boot into,
`management.admin_token_file` (default `/run/stormblock-admin/admin_token`,
mode 0600, in a directory of mode 0700). With nowhere to write it, it is
held in memory, and the node says so.

**A Kubernetes bearer** (what stormconsole sends for a destructive request,
stormconsole#82) is checked against the apiserver in `[management.kubernetes]`
(`api_url`, `ca_file`, and `token_file` for the engine's own credential, which
may create `tokenreviews` and `subjectaccessreviews`, as
`system:auth-delegator` may):

1. **TokenReview:** who is it?
2. **SubjectAccessReview** for that user. The group is `storage.storm.io`;
   the resource is the path's first segment after `api/v1/` (`volumes`,
   `slabs`, `arrays`, `forge`, …); the verb is `delete` for DELETE, `create`
   for a POST to a collection, `update` otherwise; and the name is the next
   segment.

The release's `storage-admin` ClusterRole allows these; `storage-viewer`
does not. The outcomes:
- a valid bearer that is not allowed: **403** with the apiserver's reason;
- an invalid bearer: **401**;
- the apiserver unreachable: **503**;
- no apiserver configured: **401**.

Answers are cached for a minute per bearer, resource and verb. A Kubernetes
bearer is not an ordinary credential: reads stay the node token's.

**`admin_gate = "audit"`** (or `$STORMBLOCK_ADMIN_GATE=audit`) is for rolling
this out. The node token still gets through destructive verbs, and each such
call is logged as one `enforce` (the default) would refuse.

**Audit.** Every destructive call, refusals included, is one JSON line in
`management.audit_log` (default `<data_dir>/audit.log`) and in the log:

- `who`: `admin-token`, `node-token`, `kubernetes:<user>`, `unknown-bearer`
  or `none`;
- `method`, `path`, `resource`, `verb`, `target`;
- `decision`: `allowed`, `allowed-audit-only` or `refused`, with the reason;
- `status`: the response's status code.

### What stays open

* `GET /api/v1/health` — the question "is the thing at this address an
  appliance at all". An initramfs asks it of every candidate address DHCP gave
  it, before it has any credential; a 401 there is indistinguishable from "not
  an appliance" and drops a booting node to a shell. It answers name,
  version and whether a token is required, plus two counts read without a
  lock (the worst RAID set's state, #252; the extents a flow-over still has
  to move, #260): what a supervisor holding no token needs to tell a settled
  node from one still busy.
* `/serve/v1/health`, `/serve/v1/ready` — supervisor probes, and the same two
  under the deprecated `/mk/v1` prefix.
* `GET /debug/stalls`, `/debug/tasks`, `/debug/threads`, `/debug/locks` —
  what the engine is doing when its API stops answering (#269). Read-only,
  no volume data (function names, lock states, kernel stacks); a node has no
  ssh and the supervisor asking holds no node token.
* `POST /api/v1/synonyms/boothost/<tag>/claim` — the boot claim, matched
  exactly (one method, that namespace, one path segment). The re-point beside
  it, `PUT /api/v1/synonyms/boothost/<tag>`, is what decides what a machine
  boots, and it is guarded like everything else.
* `GET /api/v1/synonyms/boothost/<tag>/intent` and
  `POST /api/v1/synonyms/boothost/<tag>/installed` — the boot intent's read
  and the install report (#148), matched the same way. The read answers one
  word about the tag asked; the report can only lower `install` to `local`,
  and only for the clone a claim handed out under that `install`. Setting the
  intent, `PUT …/intent`, needs the token — the admin token when one is
  configured, since `install` takes the machine's disk whatever it carries.

Everything else needs the token, `/metrics` included: a scrape names this
node's volumes and says how full it is, which is a read of its state rather
than a question about whether it is alive. Prometheus presents a bearer token
like any other client:

```yaml
scrape_configs:
  - job_name: stormblock
    authorization:
      credentials_file: /var/lib/stormblock/api_token
```

## Why a token is minted rather than baked in

A token in an image is shared by every node that boots that image, and a node
image is exactly what this fleet ships. So the node makes its own, at boot, and
writes it where something else on that machine can read it — which is the whole
requirement for the case that matters: a registry talking to the engine on the
same box.

That has a consequence worth stating. **A minted token is local.** It
identifies a caller to *this* node, and presenting it to a peer authenticates
nothing, because the peer minted its own. A cluster therefore shares one
`management.api_token` across its nodes, which is how a cluster is configured
anyway, and only that shared token is presented outward — cluster replication
and migration handoffs, `image build` reading a golden off an appliance, and
`boot-claim`, which also takes `--token`.

## The boot claim: the one open write (#107)

A machine claims its boot image before it has any credential — stormbootx is
firmware on a USB stick, and the initramfs a stage later is no better placed —
so that one verb has to be open. The owner's decision (2026-09-25) was to make
it safe by what it **cannot** do rather than by who calls it:

1. **Each host has its own sealed golden.** A host is known by its DNS name;
   its SMBIOS serial and MACs are aliases, and a claim by an alias is a claim
   of that host (#199, `/api/v1/boothost`). `boothost/<name>` is the host's
   *assignment* — the release stormcentral points it at. The claim keeps
   `hostgolden/<name>`: a sealed copy-on-write clone of that release, owned by
   the host (metadata only; it costs nothing until the assignment changes).
   Below, *tag* is the host's name, whichever alias it claimed as.
2. **A tag seen for the first time** takes whatever `boothost/default` names,
   and is pinned to it: `boothost/<tag>` is created then, so moving the default
   later does not move a machine that already has an image (stormbootx#15).
   **Universal boot (#200):** a machine with no name claims `boothost/default`
   itself and carries its first NIC's MAC (`?mac=` or `{"mac": …}` — the only
   thing a boot claim reads). The MAC is the host it is an alias of, or else
   the provisional host `mac-<12 hex>`, made on its first claim with the MAC
   as its alias; from there it is an ordinary tag, with its own golden. A
   claim of `boothost/default` without a MAC is a 400: tag `default` would be
   one boot clone for every machine, each claim releasing the clone the last
   machine is running on. Naming it is the #199 rename; the golden is kept.
3. **Every boot is a fresh clone** of the host's golden (`boothost-<tag>`), and
   **every earlier clone of that tag is released** (#127) — each one that is
   not inside the grace protecting the firmware → initramfs double claim (#97,
   `STORMBLOCK_CLAIM_GRACE_SECS`, 600 s), not named by a synonym, not sealed,
   and a clone of something the tag has booted. Its export goes first. So a tag
   holds at most this boot's clones; the claim response lists what it
   `released` and what a guard `kept`. Nothing written to the image survives a
   reboot; a machine's state lives in its data volumes.
4. **The claim takes no options** (apart from `mac` on a default claim). Whatever the body says — a name to bind, a
   namespace, a size, `unsealed_ok` — is ignored in the `boothost` namespace.
   It can only hand tag X a fresh clone of X's own golden, and it refuses an
   assignment that is not sealed.
5. **Re-imaging X** is an authenticated re-point of `boothost/<tag>`. The next
   claim makes X a new golden; the old one is deleted once nothing is cloned
   from it (its last boot clone goes at the next boot).

So the worst a caller that is not machine X can do by claiming as X is get
X's image. Until a claim is bound to the host itself — a host key recorded on
first use, a TPM, or mutual boot auth (stormcos#35) — the tag is the binding.
Also still to come: attaching the boot clone read-only with a writable
overlay, so the image is not modified even within a boot.

The claim answers with `host` (`name`, `claimed_as`, `aliases`),
`host_golden` (`volume`, `minted`, `collected`) and `claimed_from.release`
beside the usual `volume` and `attach`.

The claim also answers `intent` (below).

## Boot intent (#148, stormbootx#11)

Beside `boothost/<name>`, each host carries an intent that its boot agent
reads *before* it claims:

| intent | stormbootx does | then |
|---|---|---|
| `auto` (never set) | claims and boots the image | |
| `install` | claims and boots; the initramfs takes the local disk with force (`--local-disk-force`) | once the flow-over is done and the disk boots on its own, the node reports it and the intent becomes `local` |
| `local` | boots the local disk at once: no claim, no clone | |

```
GET  /api/v1/synonyms/boothost/<name>/intent      → {host, intent, updated_at}   open
PUT  /api/v1/synonyms/boothost/<name>/intent      {"intent": "install"|"local"|"auto"}  admin token
POST /api/v1/synonyms/boothost/<name>/installed   {"volume": <boot clone id>}  open
```

`<name>` resolves like a claim: the host's name or any alias — a machine that
booted the default reads under its MAC's 12 hex digits, which still reaches it
after a rename. A machine nobody has heard of is a 404; stormbootx reads any
doubt (404, error, unknown word) as `auto`, so the intent can never keep a
machine from booting. The intent lives on the host record in `synonyms.json`
and moves with a rename.

**One request, one install.** A claim served while the intent is `install`
records its boot clone (the later of firmware's and the initramfs's claims
wins) and answers `intent: install`. `boot-claim` then writes
`/run/stormblock/install.json`; the initramfs survey takes the local disk with
force (an explicit `rd.stormblock.assimilate=off` still says no); `boot-local`
carries the ticket in the handover record; and the adopting engine, once the
flow-over has moved every extent and `local-boot` has laid the ESP and boot
pallets and judged the disk bootable, posts `…/installed` with that clone's id,
retrying for an hour. That is still the installer's session: nothing has booted
from the disk yet. The owner's requirement is that the report come from the
first boot off the local disk (#220). Only that clone's
report resets the intent — any other is a 409 and changes nothing — and setting
the intent clears it, so an install that began before a request never answers
for it. Until the report lands the intent stays `install`, and the next power
cycle installs again.

**Aliases do not widen the claim.** An alias only lets a machine reach the host
it has been *told* it is; nothing becomes an alias by itself, two hosts never
share one (a conflict is refused, naming both), and setting aliases or renaming
a host needs the token. A rename keeps the host's assignment, golden, history
and clones — the old name stays an alias unless `keep_alias: false`, and
clones and goldens made under it are still collected.

**Callers that must now present a token.** An audit on 2026-09-25 found most
of the engine's outside clients sending none; each has an issue: stormcentral
#30, stormcos #89 (also: where a node keeps its token), stormconsole #30,
stormdrive #14, stormvm #44, stormcos_qa #19, rustkube-node #66,
stormblock-csi #20 (manifests), stormblock-registry #40 and stormstorage #12
(token paths), vmcloud-image-operator #7. Inside this repo, cluster heartbeat,
join and Raft present the cluster's shared token, and the `ci-*.sh` scripts
give their engines one.

## The data path: who may connect over NVMe/TCP

This document is about the management API. Who may *connect* to a volume over
NVMe/TCP is a separate door with its own answer: a volume is served to the host
an attach names, from a subsystem of that host's own, optionally behind a
DH-HMAC-CHAP secret; the shared subsystem admits no host by default; a golden
is only ever served write-protected to a named host. A boot claim's clone is
served to that machine's NQNs alone. See [nvme-access.md](nvme-access.md)
(#210).

## TLS

The token is a bearer credential: on plain HTTP it is readable by anything on
the path, and replayable. `management.tls_cert` and `management.tls_key` turn
the same listener into HTTPS (rustls), and a node that is reachable from
anywhere but its own machine wants both.

## Where it lives in the code

* `src/mgmt/auth.rs` — resolution (config → environment → token file → mint),
  the middleware the whole router is wrapped in, and the boot line.
* `src/serve/api.rs` — `decide`, `is_public`, `is_destructive`: the check
  itself, in one place, so `/api/v1`, `/v1`, `/serve/v1` and the kube surface
  cannot answer differently.

The check was written long before #107 and guarded `/v1` alone. That is the
shape of hole worth remembering: the mechanism existed, the setting existed,
and nothing connected them — so a node whose config named a token read as
closed and answered `POST /api/v1/fstemplates` from anywhere.
