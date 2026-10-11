# The storage network: data path and control channel (#85)

A node may sit on two networks. A **storage VLAN** carries the data path:
NVMe/TCP (:4420, `/serve/v1`'s :3261), iSCSI (:3260 and the per-export
portals), the ring's remote legs and replica traffic. The ordinary **LAN**
carries the control channel: the management API (:9090), the discovery
beacon, health, `/v1` fencing and epochs. A storage VLAN here is a different
interface — a VLAN sub-interface (`eth1.100`) or a second port — with its own
subnet, MTU (9000 is the usual) and routing, usually with no gateway.

Setting up the interface is node configuration (stormcos, stormpump, the
bridge and tap work). The engine is **told which interface**; it never infers
it from the routing table, which is the answer that works on a one-network
test box and fails, intermittently, on the first real deployment.

## Configuration

```toml
[storage]
interface = "eth1.100"   # or $STORMBLOCK_STORAGE_INTERFACE
# address = "10.99.0.5"  # which of its addresses, when it has several
# advertised_addr = "…"  # what initiators dial, when it is not the address
mtu = 9000               # what this network's MTU is; compared, never set
# wait_secs = 30         # how long to wait at start for the address

[management]
interface = "eth0"       # the control side: the beacon goes out and is heard here
# listen_addr = "192.168.1.5:9090"  # to keep the API off the storage network
```

Unset, there is one network and everything is as before.

## What follows from it

| | with `[storage] interface` | without |
|---|---|---|
| NVMe/TCP and iSCSI targets (shared, forge, `/serve/v1`) bind | the storage address when their listen address is a wildcard (`0.0.0.0:4420` → `10.99.0.5:4420`); a concrete listen address is kept | as configured |
| attach replies (`/v1`, `/api/v1/volumes/{id}/attach`), boot claims, discovery log pages, `/serve/v1` | `[storage] advertised_addr`, else the storage address | `management.advertised_addr`, the listen host, `[management] interface`'s address, then the default route's |
| the beacon's `mgmt_addr`, forge's trust URL | the control chain (`management.advertised_addr`, the listen host, `[management] interface`, the default route) | the same |
| the beacon is sent and heard on | `[management] interface` when set (`IP_MULTICAST_IF`, the group joined there) | whatever the kernel picks |

`[serve] advertise_addr` still overrides `/serve/v1`'s answer. The management
API's own `listen_addr` is never narrowed: callers on the node reach it on
loopback. Bind it to the LAN address to keep it off the storage network.

## Failure: a node whose data path is down is not a dead node

- **Health** (`GET /api/v1/health`, the control channel) reports
  `data_path {interface, state, address, mtu, expected_mtu, mtu_mismatch}` and,
  when named, `control {…}`. `state` is `up`, `down` (no link), `no_address`
  (link, but not the address the targets were bound to) or `missing`.
  `status` stays `ok`: the engine answers; its volumes are what cannot be
  reached.
- **Beacons** carry `data_addr` and `data_path`. A peer whose beacon arrives
  with `data_path` not `up` stays in `GET /api/v1/discovery/nodes` (not stale:
  its control channel answers) and leaves placement (`cluster_peers`), since
  it cannot hold a replica. A beacon without the fields (one network, or an
  older engine) counts as up.
- **No address at start.** The engine waits `wait_secs` for the interface's
  address. If it never comes, the data targets bind loopback only — never the
  LAN — and attach replies name loopback, an ERROR says why, health says
  `no_address`, and the control channel works. The targets stay there until
  the engine restarts.
- **MTU.** `[storage] mtu` is what the network is; the interface's own is
  compared at start (WARN) and in health (`mtu_mismatch`). A data path on the
  wrong MTU fails as stalls on large transfers, the hardest failure there is
  to diagnose, so it is said rather than left to be found.

## Checked

- unit tests: `netif` (addresses on `lo`, a fake sysfs for a link that is
  down), the config's per-role hosts, the beacon and the peer table;
- `ci-storage-net-verify.sh`: a kernel in QEMU with a second interface
  (`storage0`, 10.99.0.1/24, MTU 9000): the engine's NVMe/TCP target listens
  on 10.99.0.1 only, an attach reply names 10.99.0.1, health reports the data
  path up at MTU 9000; with the link down, health (still answering) says
  `down`.
