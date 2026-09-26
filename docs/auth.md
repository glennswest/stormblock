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
| `management.token_file` | Where a minted token is kept. Defaults to `<data_dir>/api_token`, then `/etc/stormblock/api_token`. |
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

### Destructive verbs

`admin_token` splits the surface in two. What counts as destructive is decided
in `serve::api::is_destructive`: every `DELETE`, any path ending in `/seal`
(a volume's or a template's, whatever the method), writing
files or a tar into a volume's filesystem, `?repair=true` on an fsck,
`?apply` on a trim, and a slab GC that is not a dry run. Everything else takes
either token.

### What stays open

* `GET /api/v1/health` — the question "is the thing at this address an
  appliance at all". An initramfs asks it of every candidate address DHCP gave
  it, before it has any credential; a 401 there is indistinguishable from "not
  an appliance" and drops a booting node to a shell. It answers a constant:
  name, version, and whether a token is required.
* `/serve/v1/health`, `/serve/v1/ready` — supervisor probes, and the same two
  under the deprecated `/mk/v1` prefix.
* `POST /api/v1/synonyms/boothost/<tag>/claim` — the boot claim, matched
  exactly (one method, that namespace, one path segment). The re-point beside
  it, `PUT /api/v1/synonyms/boothost/<tag>`, is what decides what a machine
  boots, and it is guarded like everything else.

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

1. **Each host has its own sealed golden.** `boothost/<tag>` is the host's
   *assignment* — the release stormcentral points it at. The claim keeps
   `hostgolden/<tag>`: a sealed copy-on-write clone of that release, owned by
   the tag (metadata only; it costs nothing until the assignment changes).
2. **A tag seen for the first time** takes whatever `boothost/default` names,
   and is pinned to it: `boothost/<tag>` is created then, so moving the default
   later does not move a machine that already has an image (stormbootx#15).
3. **Every boot is a fresh clone** of the host's golden (`boothost-<tag>`), and
   the previous boot's clone is released — after the grace that protects the
   firmware → initramfs double claim (#97). Nothing written to the image
   survives a reboot; a machine's state lives in its data volumes.
4. **The claim takes no options.** Whatever the body says — a name to bind, a
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

The claim answers with `host_golden` (`volume`, `minted`, `collected`) and
`claimed_from.release` beside the usual `volume` and `attach`.

**Callers that must now present a token.** An audit on 2026-09-25 found most
of the engine's outside clients sending none; each has an issue: stormcentral
#30, stormcos #89 (also: where a node keeps its token), stormconsole #30,
stormdrive #14, stormvm #44, stormcos_qa #19, rustkube-node #66,
stormblock-csi #20 (manifests), stormblock-registry #40 and stormstorage #12
(token paths), vmcloud-image-operator #7. Inside this repo, cluster heartbeat,
join and Raft present the cluster's shared token, and the `ci-*.sh` scripts
give their engines one.

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
