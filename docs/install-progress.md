# The install's page and progress record (#616)

Owner, 2026-11-15: "any way we can have a web page with status, instead of
primitive logging?", and then "Im asking on host": at a customer site there
is no forge and no stormcentral, and the install-config comes from our own
installer. So the page and its record are served **by the host being
installed**, from network-up, during the install and after it.

## Where

| when | who serves it | port |
|---|---|---|
| the initramfs, from the command line's parse until switch_root | `stormblock install-status --listen 0.0.0.0:9090 --run /run/stormblock` (started by `/init`) | 9090 |
| the running node | the node's engine (`adopt-ublk --api 0.0.0.0:9090`) | 9090 |

Routes, the same on both, **open and read only** (GET; no token, as
`/api/v1/health`, which already carries `storage_layout`):

- `GET /` and `GET /install`: the page. With the old web UI (feature `ui`)
  the engine keeps `/` as its redirect and the page is at `/install`.
- `GET /api/v1/install/progress`: the record below.

`rd.stormblock.status=off` on the kernel command line runs no server in the
initramfs; `rd.stormblock.status=<addr:port>` moves it. Between switch_root
and the engine's start (seconds), nothing answers; the page says so and
tries again.

## Made from what `/init` already writes

The record is built on each read from files under `/run/stormblock` (a
tmpfs that survives switch_root), so nothing keeps it in step by hand:

- `local-disk.json`: the inventory (kernel drives) and `card_rows` (#611:
  every drive the mpt card lists, with its INQUIRY and TEST UNIT READY
  through the card when Linux has no device for it).
- `storage-format/<name>.log|.rc|.fmt`: each drive's format lines and
  outcome. `<name>` is the kernel name (sg path) or the SAS address (a drive
  only the card lists, formatted through it).
- `storage-layout.json`: the drives laid, their format, the system legs and
  the set (#573).
- `init.log`: the console; its last 60 lines are in the record.
- `install-progress.events`: one tab-separated line per step, stage or drive
  event (`<epoch> <kind> <name> <state> <detail>`), appended by `/init`'s
  `progress` and by the engine (`laid`, `failed`, `booted`).

## The record

```json
{"version": 1, "state": "formatting", "at": 1794790000, "started": 1794785800,
 "eta_s": 45600, "eta": 1794835600,
 "stage": {"stage": "formatting", "state": "running", "at": 1794786120, "detail": "through the card: 0x5000cca26801b649 …"},
 "drives": [
  {"sas": "0x5000cca26801b649", "dev": null, "found": "card", "slot": "0x52cea7f04e8aa500:1",
   "model": "HGST H7210A521SUN010T B3Y1", "state": "formatting", "mode": "full",
   "percent": 1.2, "elapsed_s": 540, "eta_s": 45600,
   "card": {"sas_address": "0x5000cca26801b649", "state": "no target", "card_test_unit_ready": "NOT READY asc/ascq 0x31/0x01", "…": "…"},
   "lines": ["…the drive's last five format lines…"]},
  {"sas": "0x5000cca2680ab8d9", "dev": "sda", "found": "linux", "state": "already", "percent": 100, "…": "…"}],
 "layout": [{"step": "inventory", "state": "done", "at": 1794785900, "detail": "1 drive(s) with a Linux device, 3 more on the card only"},
            {"step": "format", "state": "running", "…": "…"},
            {"step": "lay", "state": "pending"}, {"step": "set", "state": "pending"},
            {"step": "system legs", "state": "pending"}, {"step": "flow-over", "state": "pending"},
            {"step": "laid", "state": "pending"}, {"step": "booted", "state": "pending"}],
 "stages": [{"stage": "starting", "…": "…"}, {"stage": "formatting", "…": "…"}],
 "set": null, "system_legs": null,
 "console": ["…the console's last 60 lines…"],
 "health": null}
```

- `state`: `starting`, then the current stage (`formatting`, `laying`,
  `flow-over`, …), then `laid` (the disk boots on its own), `booted` (the
  first boot off it, a new boot's record), or `failed` (a step failed, or
  `/init` dropped to its rescue shell: the stage's `detail` is the last
  `FATAL:` or `STOP:` line).
- A drive's `state`: `found`, `not ready`, `formatting`, `formatted`,
  `already` (#614's skip), `laid`, `failed` (with `said`, its words).
  `found`: `linux` or `card` (only the card lists it). `percent`, `eta_s`
  and `elapsed_s` come from its last format line (the sg path's
  `formatting (full) 57.6%, about 5h 17m left, 7h 12m so far`, or the card
  path's), moved on by the time since that line was written.
- `eta_s` / `eta`: the latest drive's, while drives format.
- `health`: in-band IPMI readings (inlet, exhaust, CPU, fans, watts) land
  here with stormipmi#111.

The console's lines stay as they are; the record is for the page (and for
stormcentral's panel, stormcentral#1433, which reads the node's API). The
graphical install screen is #618, a client of the same record.
