# Engine `/v1` wire-contract fixtures

**Vendored from [stormblock-csi](https://github.com/glennswest/stormblock-csi/tree/main/contract)** — these files are one pin held on two sides (#34, stormblock-csi#8). `tests/contract_v1_wire.rs` round-trips every one of them through the serde types in `src/mgmt/api/v1.rs`, so a field rename, a tag change or a dropped `skip_serializing_if` fails `cargo test` here. Changing the wire means changing the fixture in **both** repos; a change landed in only one fails that repo's build, which is the point.

The rest of this file is the upstream README, kept as copied.

---

# Engine `/v1` wire-contract fixtures

Golden JSON for every wire type the CSI driver exchanges with the
StormBlock engine (issue #8). `cargo test -p stormblock-client` asserts
this repo's serde types round-trip every fixture byte-for-byte (as JSON
values), so a field rename or tag change fails the build instead of a
cluster.

The engine repo (`glennswest/stormblock`) should mirror these exact files
against its own serializers — one change updates both pins. Normative
prose contract: `docs/stormblock-api.md`.

| Fixture | Type |
|---|---|
| `volume.json` | `Volume` (embeds `Replica`, `SyncState` in_sync + resyncing) |
| `sync-state-detached.json` | `SyncState::Detached` |
| `attach-info-nvme-tcp.json` | `AttachInfo::NvmeTcp` (shared subsystem NQN + nsid) |
| `attach-info-ublk.json` | `AttachInfo::Ublk` |
| `volume-source-snapshot.json` / `volume-source-volume.json` | `VolumeSource` |
| `dual-attach-window.json` | `DualAttachWindow` |
| `create-volume-request.json` | `CreateVolumeRequest` (every field set) |
| `snapshot.json` | `Snapshot` |
| `group-snapshot.json` | `GroupSnapshot` |
| `node-capacity.json` | `NodeCapacity` |
| `error-envelope-stale-epoch.json` | error envelope, 412 fencing shape |
| `error-envelope-already-exists.json` | error envelope, 409 idempotency shape |
