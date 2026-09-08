# Who may call a node's API

A stormblock node's management API is not a status page. Anything that can
reach it can create, clone, seal and **delete** volumes, add and withdraw
exports, publish and unpublish releases, and re-point a synonym — and
re-pointing `boothost/<tag>` decides which image a machine boots at its next
power cycle. It listens on `0.0.0.0:9090` by default.

This page is what guards it (issue #107).

## The short version

```toml
[management]
require_auth = true
data_dir = "/var/lib/stormblock"
```

The node mints a token at boot, writes it to `<data_dir>/api_token` mode
`0600`, and requires it on every request:

```bash
curl -H "Authorization: Bearer $(cat /var/lib/stormblock/api_token)" \
     http://node:9090/api/v1/volumes
```

Anything else gets a 401. Nothing else has to be configured, and the token
survives restarts because the file does.

## What the settings mean

| setting | effect |
|---|---|
| `management.api_token` | The token, named in the config. Accepted for everything. |
| `management.admin_token` | When set, destructive verbs need **this** one and `api_token` is not enough. |
| `management.token_file` | Where a minted token is kept. Defaults to `<data_dir>/api_token`, then `/etc/stormblock/api_token`. |
| `management.require_auth` | `true` — required, minting one if there is none. `false` — deliberately open. Unset — enforced if a token is configured or a token file exists, open otherwise. |

`$STORMBLOCK_API_TOKEN` and `$STORMBLOCK_ADMIN_TOKEN` are read when the config
names neither.

`require_auth = true` with nowhere to keep a token — no `token_file`, no
`data_dir`, no `api_token` — **fails startup**. Falling back to open is the
one thing it must not do.

### Destructive verbs

`admin_token` splits the surface in two. What counts as destructive is decided
in `serve::api::is_destructive`: every `DELETE`, sealing a template, writing
files or a tar into a volume's filesystem, `?repair=true` on an fsck,
`?apply` on a trim, and a slab GC that is not a dry run. Everything else takes
either token.

### What stays open

* `GET /api/v1/health` — the question "is the thing at this address an
  appliance at all". An initramfs asks it of every candidate address DHCP gave
  it, before it has any credential; a 401 there is indistinguishable from "not
  an appliance" and drops a booting node to a shell. It answers a constant:
  name, version, and whether a token is required.
* `/serve/v1/health`, `/serve/v1/ready` — supervisor probes.

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

## Why the default is still open

Because a machine claims its boot image before it has any credential.
`boot-claim` — and the firmware one stage earlier — asks an appliance for the
volume this machine boots from, and closing the fleet from inside the engine
would stop machines booting with no way to hand them the token first. That is a
migration, and the order it runs in is: distribute the token, then set
`require_auth = true` on the appliance.

What is *not* deferred is the silence. A node with no token says so on every
boot, naming what is exposed:

```
WARN SECURITY: the management API on 0.0.0.0:9090 is UNAUTHENTICATED
WARN SECURITY: anyone who can reach that address can create, clone, seal and
     DELETE volumes, add and withdraw exports, re-point synonyms and publish
     releases — which includes choosing what a machine boots at its next power
     cycle
WARN SECURITY: set management.require_auth = true to require a bearer token;
     the node will mint one into /var/lib/stormblock/api_token and keep it
     across restarts
```

and `GET /api/v1/health` reports `"auth": "none"`, so a fleet can be asked
which of its nodes are open without trying each one:

```bash
for n in $(cat nodes); do
  printf '%s %s\n' "$n" "$(curl -s "http://$n:9090/api/v1/health" | jq -r .auth)"
done
```

An insecure default survives because nothing fails while it is wrong. These two
are what make it fail loudly instead of silently.

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
