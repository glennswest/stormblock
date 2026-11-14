#!/bin/sh
# The boot's storage inventory (#345), against a fake sysfs.
#
# Owner: "a check during the boot, that we see if controller and drive is
# there". Pinned here:
#   * every PCI mass-storage controller is listed with its id, class and
#     driver; one with no driver bound is a WARNING naming its PCI id;
#   * every drive is listed under its controller: model, serial (sysfs, else
#     VPD page 0x80), size, transport, SES bay;
#   * a bound SAS/RAID/NVMe controller with no drive yet is waited for
#     (bounded), and the console says whether its drives came;
#   * a SAS HBA's drives that arrive one by one are all waited for: the
#     count must hold still for STORM_STORAGE_SETTLE, a drive the SAS layer
#     found but sd has not made a disk of yet is waited for past the plain
#     bound, and a machine with no SAS/RAID controller does not wait (#592);
#   * the inventory is written to local-disk.json as `controllers` and
#     `drives`, as JSON the engine reads (#344's verdict file).
#
# Runs the real code: the block is extracted from the init script this repo
# generates, between its marker comments, so the test cannot drift.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN shelves/,/# --- END shelves/p' "$GEN" > "$WORK/inv.sh"
sed -n '/# --- BEGIN storage inventory/,/# --- END storage inventory/p' "$GEN" >> "$WORK/inv.sh"
[ -s "$WORK/inv.sh" ] || { echo "FAIL: could not extract the storage inventory block"; exit 1; }

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: expected '$2', got '$3'"
        fail=1
    fi
}
contains() { # name needle haystack
    case "$3" in
    *"$2"*) echo "  ok    $1" ;;
    *) echo "  FAIL  $1: '$2' not in: $3"; fail=1 ;;
    esac
}

# The Dell: an mpt3sas HBA with the WD disk in bay 4 of its own SES
# backplane; the onboard AHCI with no driver; an NVMe controller whose
# namespace comes late (LATE=1) or never (LATE=never).
make_tree() {
    T="$WORK/t"; rm -rf "$T"; mkdir -p "$T/pci" "$T/block" "$T/drivers/mpt3sas" "$T/drivers/nvme"
    dev="$T/devices/pci0000:00"
    # mpt3sas, 0000:01:00.0
    mkdir -p "$T/pci/0000:01:00.0"
    echo 0x010700 > "$T/pci/0000:01:00.0/class"; echo 0x1000 > "$T/pci/0000:01:00.0/vendor"; echo 0x0097 > "$T/pci/0000:01:00.0/device"
    ln -s "$T/drivers/mpt3sas" "$T/pci/0000:01:00.0/driver"
    sd="$dev/0000:00:01.0/0000:01:00.0/host0/port-0:0/end_device-0:0/target0:0:0/0:0:0:0"
    mkdir -p "$sd/block/sda"
    # Its shelf (#347): the R230's front backplane, an SES enclosure.
    ses="$dev/0000:00:01.0/0000:01:00.0/host0/port-0:1/end_device-0:1/target0:0:1/0:0:1:0/enclosure/0:0:1:0"
    mkdir -p "$ses/Slot 04" "$ses/device"
    echo 500056b3a1b2c3d4 > "$ses/id"; echo DP > "$ses/device/vendor"; echo "BP13G+" > "$ses/device/model"
    ln -s "$ses/Slot 04" "$sd/enclosure_device:Slot 04"
    echo "WDC WD20EFAX-68F" > "$sd/model"
    printf '\000\200\000\020WD-WX11D28JFS6T' > "$sd/vpd_pg80"
    ln -s "$sd" "$sd/block/sda/device"
    echo 3907029168 > "$sd/block/sda/size"
    ln -s "$sd/block/sda" "$T/block/sda"
    # AHCI, 0000:00:1f.2, no driver
    mkdir -p "$T/pci/0000:00:1f.2"
    echo 0x010601 > "$T/pci/0000:00:1f.2/class"; echo 0x8086 > "$T/pci/0000:00:1f.2/vendor"; echo 0xa102 > "$T/pci/0000:00:1f.2/device"
    # NVMe, 0000:02:00.0, its namespace not there yet
    mkdir -p "$T/pci/0000:02:00.0"
    echo 0x010802 > "$T/pci/0000:02:00.0/class"; echo 0x144d > "$T/pci/0000:02:00.0/vendor"; echo 0xa808 > "$T/pci/0000:02:00.0/device"
    ln -s "$T/drivers/nvme" "$T/pci/0000:02:00.0/driver"
    nv="$dev/0000:00:02.0/0000:02:00.0/nvme/nvme0"
    mkdir -p "$nv"
    echo "Samsung SSD 970" > "$nv/model"; echo "S4EWNX0N" > "$nv/serial"
    # A network card: not storage, never listed
    mkdir -p "$T/pci/0000:03:00.0"; echo 0x020000 > "$T/pci/0000:03:00.0/class"
    # A loop device: not a drive
    mkdir -p "$T/devices/virtual/block/loop0"; ln -s "$T/devices/virtual/block/loop0" "$T/block/loop0"
}
nvme_appears() {
    mkdir -p "$nv/nvme0n1"
    ln -s "$nv" "$nv/nvme0n1/device"
    echo 1953525168 > "$nv/nvme0n1/size"
    ln -s "$nv/nvme0n1" "$T/block/nvme0n1"
}

inventory() ( # -> console in inv.out, JSON in report.json
    set +e
    STORM_PCI_SYS="$T/pci"; STORM_INV_BLOCK="$T/block"; STORM_STORAGE_WAIT=${WAIT:-3}
    STORM_INV_SCSI="$T/scsi"; STORM_INV_SAS_END="$T/sas_end_device"; STORM_INV_SG="$T/sg"
    STORM_STORAGE_SETTLE=${SETTLE:-1}; STORM_STORAGE_PENDING_WAIT=${PWAIT:-3}
    STORM_LOCAL_DISK_REPORT="$WORK/report.json"
    rm -f "$WORK/report.json"
    SLEPT=0
    sleep() {
        SLEPT=$((SLEPT + 1))
        [ "${LATE:-}" = 1 ] && [ ! -e "$T/block/nvme0n1" ] && nvme_appears
        [ -n "${ON_SLEEP:-}" ] && $ON_SLEEP
        :
    }
    . "$WORK/inv.sh" > "$WORK/inv.out" 2>&1
    echo "$SLEPT" > "$WORK/slept"
)

# forge1 (#592): an R6415's HBA330 (mpt3sas) with four SAS drives, sda there
# at once; the others found by the SAS layer and made disks later.
make_forge() {
    T="$WORK/t"; rm -rf "$T"; mkdir -p "$T/pci" "$T/block" "$T/scsi" "$T/sas_end_device" "$T/drivers/mpt3sas"
    hba="$T/devices/pci0000:c0/0000:c0:03.1/0000:c1:00.0"
    mkdir -p "$T/pci/0000:c1:00.0"
    echo 0x010700 > "$T/pci/0000:c1:00.0/class"; echo 0x1000 > "$T/pci/0000:c1:00.0/vendor"; echo 0x0097 > "$T/pci/0000:c1:00.0/device"
    ln -s "$T/drivers/mpt3sas" "$T/pci/0000:c1:00.0/driver"
    forge_end 0
    forge_scsi 0
    forge_disk 0
}
forge_end() { # n -> the SAS layer finds end device n (no SCSI target yet)
    _e="$hba/host0/port-0:$1/end_device-0:$1"
    mkdir -p "$_e/sas_end_device/end_device-0:$1"
    ln -s "$_e/sas_end_device/end_device-0:$1" "$T/sas_end_device/end_device-0:$1"
}
forge_scsi() { # n -> its SCSI disk (type 0), no block device yet
    _s="$hba/host0/port-0:$1/end_device-0:$1/target0:0:$1/0:0:$1:0"
    mkdir -p "$_s"; echo 0 > "$_s/type"; echo "H7210A520SUN010T" > "$_s/model"
    ln -s "$_s" "$T/scsi/0:0:$1:0"
}
forge_disk() { # n -> sd made it a disk
    _s="$hba/host0/port-0:$1/end_device-0:$1/target0:0:$1/0:0:$1:0"
    _n=sd$(printf '%s' "$1" | tr 0123 abcd)
    mkdir -p "$_s/block/$_n"; ln -s "$_s" "$_s/block/$_n/device"
    echo 19134414848 > "$_s/block/$_n/size"
    ln -s "$_s/block/$_n" "$T/block/$_n"
}

echo "storage inventory:"
make_tree
LATE=1 inventory
out=$(cat "$WORK/inv.out")
contains "a bound NVMe controller with no drive is waited for" "waiting up to 3s for drives on 0000:02:00.0" "$out"
contains "  and its drive came" "drives appeared after 1s" "$out"
contains "the HBA, its driver, its drive" "0000:01:00.0 [1000:0097] class 0x010700: mpt3sas, 1 drive(s)" "$out"
contains "the drive: model, serial from VPD 0x80, size, transport, controller, bay" \
    "sda: WDC WD20EFAX-68F serial WD-WX11D28JFS6T, 2000 GB, sas on 0000:01:00.0, front shelf 500056b3a1b2c3d4, bay Slot 04" "$out"
contains "the NVMe namespace, serial from sysfs, in no enclosure: internal" "nvme0n1: Samsung SSD 970 serial S4EWNX0N, 1000 GB, nvme on 0000:02:00.0, internal shelf -" "$out"
contains "an unbound controller is a WARNING naming its PCI id" \
    "WARNING: storage controller 0000:00:1f.2 [8086:a102] (class 0x010601) has no driver bound" "$out"
case "$out" in *0000:03:00.0*|*loop0*) check "no network card, no loop device" no yes ;; *) check "no network card, no loop device" yes yes ;; esac

if command -v python3 >/dev/null 2>&1; then
    r=$(python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
c = {x["pci"]: x for x in d["controllers"]}
dr = {x["name"]: x for x in d["drives"]}
print(d["state"], len(c), c["0000:01:00.0"]["driver"], c["0000:00:1f.2"]["driver"], ",".join(c["0000:01:00.0"]["drives"]),
      dr["sda"]["serial"], dr["sda"]["bay"], dr["sda"]["size_bytes"], dr["nvme0n1"]["transport"],
      dr["sda"]["shelf"]["position"], dr["sda"]["shelf"]["id"], dr["sda"]["shelf"]["identity"], dr["nvme0n1"]["shelf"]["position"])
' "$WORK/report.json" 2>&1)
    check "local-disk.json carries the inventory, as JSON" \
        "unknown 3 mpt3sas None sda WD-WX11D28JFS6T Slot 04 2000398934016 nvme front 500056b3a1b2c3d4 DP BP13G+ internal" "$r"
else
    echo "  skip  local-disk.json as JSON (no python3)"
fi

make_tree
LATE=never inventory
out=$(cat "$WORK/inv.out")
contains "a controller whose drives never come: waited for, bounded, and said" \
    "after 3s still no drive on 0000:02:00.0" "$out"

echo "SAS drives that arrive one by one (#592):"
# One more drive found and made a disk every other second: the count never
# holds 3 s until the fourth.
forge_step() {
    case "$SLEPT" in
    2) forge_end 1; forge_scsi 1; forge_disk 1 ;;
    4) forge_end 2; forge_scsi 2; forge_disk 2 ;;
    6) forge_end 3; forge_scsi 3; forge_disk 3 ;;
    esac
}
make_forge
WAIT=30 SETTLE=3 PWAIT=3 ON_SLEEP=forge_step inventory
out=$(cat "$WORK/inv.out")
contains "all four drives seen, not the first alone" "0000:c1:00.0 [1000:0097] class 0x010700: mpt3sas, 4 drive(s)" "$out"
contains "  and said, with when the last one came" "storage: 1 -> 4 drives on 0000:c1:00.0 after 6s" "$out"
check "  the wait ends once the count held for the settle time" 9 "$(cat "$WORK/slept")"

# A drive the SAS layer found whose disk sd makes only at 8 s: past the 3 s
# plain bound, inside the pending bound.
forge_late() {
    [ "$SLEPT" = 1 ] && { forge_end 1; forge_scsi 1; }
    [ "$SLEPT" = 8 ] && forge_disk 1
    :
}
make_forge
SETTLE=1 PWAIT=20 ON_SLEEP=forge_late inventory
out=$(cat "$WORK/inv.out")
contains "a drive still spinning up is said" "waiting up to 20s for drives still coming up: 0000:c1:00.0:0:0:1:0" "$out"
contains "  and waited for past the plain bound" "mpt3sas, 2 drive(s)" "$out"
contains "  and its arrival said" "storage: 1 -> 2 drives on 0000:c1:00.0 after 8s" "$out"

# An end device whose target never comes: bounded, and a WARNING.
forge_never() { [ "$SLEPT" = 1 ] && forge_end 2; :; }
make_forge
SETTLE=1 PWAIT=6 ON_SLEEP=forge_never inventory
out=$(cat "$WORK/inv.out")
contains "a found drive that never comes up is a WARNING, bounded" \
    "WARNING: storage: after 6s these drives the SAS layer found still have no block device: 0000:c1:00.0:end_device-0:2" "$out"
contains "  the inventory goes on with what is there" "mpt3sas, 1 drive(s)" "$out"

# forge1 as it is (#595): sda a disk; three drives left unformatted answer
# with no readable format - two with no block device (only an sg node),
# one a disk of 0 bytes. Each listed, NOT READY, with its WWID.
forge_sg() { # n -> sd gave it no disk; sg did
    _s="$hba/host0/port-0:$1/end_device-0:$1/target0:0:$1/0:0:$1:0"
    mkdir -p "$_s/scsi_generic/sg$1" "$T/sg/sg$1"; ln -s "$_s" "$T/sg/sg$1/device"
}
make_forge
for n in 1 2 3; do forge_end "$n"; forge_scsi "$n"; done
echo naa.5000cca2680ab8d8 > "$hba/host0/port-0:0/end_device-0:0/target0:0:0/0:0:0:0/wwid"
echo naa.5000cca26801b648 > "$hba/host0/port-0:1/end_device-0:1/target0:0:1/0:0:1:0/wwid"
echo naa.5000cca26a819e68 > "$hba/host0/port-0:2/end_device-0:2/target0:0:2/0:0:2:0/wwid"
echo naa.5000cca268077300 > "$hba/host0/port-0:3/end_device-0:3/target0:0:3/0:0:3:0/wwid"
forge_sg 1; forge_sg 2; forge_disk 3
echo 0 > "$T/block/sdd/size"
SETTLE=1 PWAIT=2 inventory
out=$(cat "$WORK/inv.out")
contains "the readable drive with its WWID" "sda: H7210A520SUN010T serial ? naa.5000cca2680ab8d8, 9796 GB" "$out"
contains "a drive with only an sg node is listed, NOT READY" \
    "sg1 (0:0:1:0, no block device): H7210A520SUN010T serial ? naa.5000cca26801b648, NOT READY: no readable format, on 0000:c1:00.0 (#595)" "$out"
contains "  the other" "sg2 (0:0:2:0, no block device): H7210A520SUN010T serial ? naa.5000cca26a819e68, NOT READY" "$out"
contains "a disk of 0 bytes is NOT READY" "sdd: H7210A520SUN010T serial ? naa.5000cca268077300, NOT READY: no readable format (0 bytes)" "$out"
contains "  in the JSON, not ready" '"name": "sg1", "model": "H7210A520SUN010T", "serial": null, "wwid": "naa.5000cca26801b648", "size_bytes": 0, "ready": false' "$(cat "$WORK/report.json")"
contains "  a ready one ready" '"wwid": "naa.5000cca2680ab8d8", "size_bytes": 9796820402176, "ready": true' "$(cat "$WORK/report.json")"
if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$WORK/report.json" 2>/dev/null \
        && echo "  ok    the report is JSON" || { echo "  FAIL  the report is not JSON"; fail=1; }
fi

# No SAS/RAID controller bound (NVMe there at once): no settle wait at all.
make_tree
rm "$T/pci/0000:01:00.0/driver"
nvme_appears
SETTLE=5 inventory
check "no SAS/RAID controller: the boot does not wait" 0 "$(cat "$WORK/slept")"

[ "$fail" = 0 ] && echo "all storage inventory checks passed" || { echo "FAILURES"; exit 1; }
