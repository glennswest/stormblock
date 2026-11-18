#!/bin/sh
# Every drive a boot's layout does not take is said, with what it is and why
# (#672): server8's 2 TB SATA HDD sat `out` with no word. Runs the real block,
# extracted from the init script between its marker comments, over a fake
# /sys/block shaped as server8's: the Patriot SSD taken, the Seagate 512e HDD,
# the BMC's USB virtual disk, and a USB stick.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN drives not taken/,/# --- END drives not taken/p' "$GEN" > "$WORK/block.sh"
grep -q 'END drives not taken' "$WORK/block.sh" || { echo "FAIL: could not extract the drives-not-taken block"; exit 1; }

fail=0
has() { # name file pattern
    if grep -q -- "$3" "$2" 2>/dev/null; then
        echo "  ok    $1"
    else
        echo "  FAIL  $1: no '$3' in $(basename "$2")"
        sed 's/^/  | /' "$2"
        fail=1
    fi
}
hasnt() {
    if grep -q -- "$3" "$2" 2>/dev/null; then
        echo "  FAIL  $1: '$3' in $(basename "$2")"
        sed 's/^/  | /' "$2"
        fail=1
    else
        echo "  ok    $1"
    fi
}

# A drive under a fake device path (the path decides the transport: /ata, /usb).
SYS="$WORK/sys/block"
mkdir -p "$SYS"
disk() { # name path model sectors rotational logical physical removable
    mkdir -p "$WORK/devices/$2/$1/device" "$WORK/devices/$2/$1/queue"
    printf '%s\n' "$3" > "$WORK/devices/$2/$1/device/model"
    echo "$4" > "$WORK/devices/$2/$1/size"
    echo "$5" > "$WORK/devices/$2/$1/queue/rotational"
    echo none > "$WORK/devices/$2/$1/queue/zoned"
    echo "$6" > "$WORK/devices/$2/$1/queue/logical_block_size"
    echo "$7" > "$WORK/devices/$2/$1/queue/physical_block_size"
    echo "$8" > "$WORK/devices/$2/$1/removable"
    ln -s "$WORK/devices/$2/$1" "$SYS/$1"
}
disk sda pci0/ata1 "Patriot P210 512GB      " 1000215216 0 512 512 0
disk sdb pci0/ata2 "ST2000DM008-2UB1" 3907029168 1 512 4096 0
disk sdc pci0/usb1 "Virtual Disk" 0 0 512 512 1
disk sdd pci0/usb2 "USB" 60620800 0 512 512 1

run() { # vars...
    {
        echo "SURVEY_SYS='$SYS'"
        echo 'disk_of() { case "$1" in /dev/sd*[0-9]) echo "${1%%[0-9]*}" ;; *) echo "$1" ;; esac; }'
        for v in "$@"; do echo "$v"; done
        cat "$WORK/block.sh"
    } > "$WORK/run.sh"
    sh "$WORK/run.sh" > "$WORK/out" 2>&1 || echo "EXIT $?" >> "$WORK/out"
}

echo "server8: the node on the SSD, the HDD and the USB drives said"
run LOCAL_DISK=/dev/sda4 SYSTEM_DISK= PAIR_DISK=
hasnt "the drive taken is not said" "$WORK/out" "not taken: sda "
has "the HDD, what it is" "$WORK/out" "not taken: sdb (ST2000DM008-2UB1, 2000 GB, SATA HDD, 512e)"
has "the HDD, why" "$WORK/out" "the install lays the node on /dev/sda; a data drive beyond them is enrolled by stormdrive's DrivePolicy after the boot"
has "the virtual disk" "$WORK/out" "not taken: sdc (Virtual Disk, 0 GB, USB): a USB drive: an install never takes one (#672)"
has "the stick" "$WORK/out" "not taken: sdd (USB, 31 GB, USB): a USB drive"
hasnt "no error" "$WORK/out" "EXIT"

echo "server8 as the owner laid it (#672): the node on the HDD, the SSD left for the fast tier"
run LOCAL_DISK=/dev/sdb2 SYSTEM_DISK= PAIR_DISK=
hasnt "the HDD is not said" "$WORK/out" "not taken: sdb "
has "the SSD, what it is" "$WORK/out" "not taken: sda (Patriot P210 512GB, 512 GB, SATA SSD, 512-byte sectors)"
has "the SSD, why" "$WORK/out" "left for the fast tier: the node is on the rotating /dev/sdb; stormdrive's DrivePolicy enrolls this drive as a \`fast\` data slab"
hasnt "no error" "$WORK/out" "EXIT"

echo "a system drive and storage: drives are taken too"
run LOCAL_DISK=/dev/sda4 SYSTEM_DISK=/dev/sdb PAIR_DISK=
hasnt "the system drive is not said" "$WORK/out" "not taken: sdb "
run LOCAL_DISK=/dev/ublkb65535 "STORAGE_DRIVES='/dev/sda /dev/sdb'"
hasnt "storage:'s drives are not said" "$WORK/out" "not taken: sd[ab] "

echo "a zoned drive and a removable one"
echo host-managed > "$WORK/devices/pci0/ata2/sdb/queue/zoned"
echo 4096 > "$WORK/devices/pci0/ata2/sdb/queue/logical_block_size"
run LOCAL_DISK=/dev/sda4
has "zoned and 4Kn said" "$WORK/out" "not taken: sdb (ST2000DM008-2UB1, 2000 GB, SATA HDD, zoned host-managed, 4Kn)"
echo 1 > "$WORK/devices/pci0/ata2/sdb/removable"
run LOCAL_DISK=/dev/sda4
has "removable" "$WORK/out" "not taken: sdb (.*): removable: an install never takes one"

echo "no local disk (a diskless or appliance boot): nothing said"
run LOCAL_DISK=
hasnt "silent" "$WORK/out" "not taken"

exit "$fail"
