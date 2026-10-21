#!/bin/sh
# The system-data block of /init (#355): the node's kept record of itself.
#
# boot-local names the `system-data` volume's ublk device in system-data.dev;
# /init mounts it and writes config/ (the release's mount list),
# history/boots/ (one record per boot) and history/installs/ (when the boot
# installed), keeping the newest N boot records. Pinned here with `mount`
# stubbed: nothing happens without a device; the records are JSON with what
# the boot knew; pruning keeps N; a mount that fails is a warning, never a
# failed boot.
#
# Runs the real code: the block is extracted from the init script this repo
# generates, between its marker comments, so the test cannot drift.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
sed -n '/# --- BEGIN system data/,/# --- END system data/p' "$GEN" > "$WORK/sd.sh"
[ -s "$WORK/sd.sh" ] || { echo "FAIL: could not extract the system data block"; exit 1; }

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then echo "  ok    $1"; else echo "  FAIL  $1: expected '$2', got '$3'"; fail=1; fi
}
contains() { # name needle haystack
    case "$3" in *"$2"*) echo "  ok    $1" ;; *) echo "  FAIL  $1: '$2' not in: $3"; fail=1 ;; esac
}

run() ( # -> console in sd.out
    set +e
    R="$WORK/run"; mkdir -p "$R" "$WORK/root/etc"
    echo 'VERSION_ID="11.97"' > "$WORK/root/etc/os-release"
    echo "console=ttyS0 stormblock.volume=stormpump rd.stormblock.boothost=http://forge:9090" > "$WORK/cmdline"
    STORM_SYSTEM_DATA_DEV="$R/system-data.dev"; STORM_SYSTEM_DATA_DIR="$WORK/sd"
    STORM_SYSTEM_DATA_ROOT="$WORK/root"; STORM_SYSTEM_DATA_RUN="$R"; STORM_CMDLINE="$WORK/cmdline"
    STORM_SYSTEM_DATA_WAIT=0; STORM_SYSTEM_DATA_KEEP="${KEEP:-500}"
    MOUNTS="cilium:/p/cilium,kubelet-data:/var/lib/kubelet"; MOUNTS_FROM="/etc/stormblock/mounts in stormpump"
    BOOTTAG=C2NR0Q2
    LOCAL_DISK="${LD:-}"; LOCAL_DISK_ID="${LDID:-}"; CHOSEN_BY="${CB:-}"; SYSTEM_DISK=""; SYSTEM_DISK_ID=""
    mount() { [ "${MOUNT_FAILS:-}" = 1 ] && { echo "mount: wrong fs type" >&2; return 1; }; return 0; }
    sync() { :; }
    . "$WORK/sd.sh" > "$WORK/sd.out" 2>&1
)

echo "system data:"
rm -rf "$WORK/run" "$WORK/sd"
run
check "no device named: nothing is mounted or written" "no" "$([ -e "$WORK/sd" ] && echo yes || echo no)"

mkdir -p "$WORK/run"
echo /dev/ublkb7 > "$WORK/run/system-data.dev"
echo '{"state": "taken", "drive": "/dev/sda", "controllers": [], "drives": []}' > "$WORK/run/local-disk.json"
echo '{"slabs": ["/dev/sda"], "installed": {"version": "11.97", "previous": "11.95", "carried": ["img-1"]}}' > "$WORK/run/handover.json"
run
out=$(cat "$WORK/sd.out")
contains "the mount and the boot record are said" "system-data: /dev/ublkb7 on $WORK/sd, boot recorded" "$out"
contains "an install is recorded too" "system-data: install recorded" "$out"
check "one boot record" 1 "$(ls "$WORK/sd/history/boots" | wc -l)"
check "one install record" 1 "$(ls "$WORK/sd/history/installs" | wc -l)"
check "the release's mount list in config/" "cilium:/p/cilium
kubelet-data:/var/lib/kubelet" "$(cat "$WORK/sd/config/mounts.release")"
if command -v python3 >/dev/null 2>&1; then
    r=$(python3 -c '
import json, sys, glob
b = json.load(open(glob.glob(sys.argv[1] + "/history/boots/*.json")[0]))
print(b["release"], b["tag"], "stormblock.volume=stormpump" in b["cmdline"], b["local_disk"]["state"], b["handover"]["installed"]["version"], b["install_config"])
' "$WORK/sd" 2>&1)
    # No install-config block in this run: its source reads none (#459).
    check "the boot record is JSON with what the boot knew" "11.97 C2NR0Q2 True taken 11.97 none" "$r"
fi

# #478: the drive an install chose, recorded as stormdrive's designation,
# once: stormdrive's own file is never rewritten.
check "no drive identity: no designation written" "no" "$([ -e "$WORK/sd/stormdrive/system-drives.json" ] && echo yes || echo no)"
LD=/dev/sdb LDID="wwn=naa.5002538e40a1b2c3;serial=TF512A;model=TEAM T253X2512G;size=512110190592" CB=system_drive run
contains "an install records its drive for stormdrive" "/dev/sdb recorded as the system drive for stormdrive (#478)" "$(cat "$WORK/sd.out")"
if command -v python3 >/dev/null 2>&1; then
    r=$(python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))["drives"][0]
print(d["wwn"], d["serial"], d["model"], d["path"], d["chosen_by"])
' "$WORK/sd/stormdrive/system-drives.json" 2>&1)
    check "  in stormdrive's format (#402's reader takes wwn, serial, model)" "naa.5002538e40a1b2c3 TF512A TEAM T253X2512G /dev/sdb system_drive" "$r"
fi
echo '{"version": 1, "by": "stormdrive", "drives": []}' > "$WORK/sd/stormdrive/system-drives.json"
sleep 1
LD=/dev/sda LDID="wwn=naa.1;serial=X;model=Y;size=1" CB=first run
check "  stormdrive's own file is never rewritten" '{"version": 1, "by": "stormdrive", "drives": []}' "$(cat "$WORK/sd/stormdrive/system-drives.json")"

# A boot that did not install: a boot record, no install record.
echo '{"slabs": ["/dev/sda"]}' > "$WORK/run/handover.json"
sleep 1
run
# (Three installing boots above: three boot records, three install records.)
check "a boot that did not install adds a boot record" 4 "$(ls "$WORK/sd/history/boots" | wc -l)"
check "and no install record" 3 "$(ls "$WORK/sd/history/installs" | wc -l)"

# Pruning: the newest N.
for t in 20200101T000000Z 20200102T000000Z 20200103T000000Z; do echo '{}' > "$WORK/sd/history/boots/$t.json"; done
KEEP=3 run
check "the newest 3 boot records are kept" 3 "$(ls "$WORK/sd/history/boots" | wc -l)"
check "the oldest went first" "" "$(ls "$WORK/sd/history/boots" | grep 2020010[12] || true)"

# A mount that fails: said, and the boot goes on.
MOUNT_FAILS=1 run
contains "a failed mount is a WARNING" "WARNING: system-data: /dev/ublkb7 would not mount" "$(cat "$WORK/sd.out")"

[ "$fail" = 0 ] && echo "all system data checks passed" || { echo "FAILURES"; exit 1; }
