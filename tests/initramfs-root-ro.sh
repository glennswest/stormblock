#!/bin/sh
# A read-only root (stormcos#470, #380), against stubs and a fake root.
#
# A stormcos node's root clone must never be written, so it stays equal to its
# golden. Pinned here:
#   * `ro` / `rw` on the command line, the last one winning: `ro` mounts the
#     root read-only (ext4 `-o ro`), neither leaves the default;
#   * the hostname and the resolver are written to /run, which becomes the
#     root's, never through /sysroot: a golden whose /etc/hostname and
#     /etc/resolv.conf link to /run reads what DHCP gave, and nothing under the
#     root changes (its digest before and after);
#   * a read-write root whose files are not links gets /etc as before.
#
# Runs the real code: the blocks are extracted from the init script this repo
# generates, between their marker comments, so the test cannot drift.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN root mode/,/# --- END root mode/p' "$GEN" > "$WORK/mode.sh"
sed -n '/# --- BEGIN network handoff/,/# --- END network handoff/p' "$GEN" > "$WORK/handoff.sh"
[ -s "$WORK/mode.sh" ] && [ -s "$WORK/handoff.sh" ] || { echo "FAIL: could not extract the blocks"; exit 1; }

fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then echo "  ok    $1"; else echo "  FAIL  $1: expected '$2', got '$3'"; fail=1; fi
}

echo "root mode:"
ro_for() { # cmdline -> ROOT_RO
    printf '%s\n' "$1" > "$WORK/cmdline"
    ( set +e; STORM_CMDLINE="$WORK/cmdline"; . "$WORK/mode.sh" >/dev/null 2>&1; echo "${ROOT_RO:-}" )
}
check "ro" "1" "$(ro_for "BOOT_IMAGE=vmlinuz ro console=ttyS0")"
check "rw" "" "$(ro_for "rw quiet")"
check "neither: the default" "" "$(ro_for "quiet console=tty0")"
check "ro then rw: the last wins" "" "$(ro_for "ro foo rw")"
check "rw then ro: the last wins" "1" "$(ro_for "rw ro")"
check "a word containing ro is not ro" "" "$(ro_for "rd.stormblock.root=x root=/dev/ublkb0 ro_x=1")"

# mount_root's arguments: a stub mount that refuses erofs and records the rest.
MSTUB="$WORK/mount"
cat > "$MSTUB" <<'STUBEOF'
#!/bin/sh
case "$*" in *erofs*) exit 1 ;; esac
echo "$*" >> "$MOUNT_LOG"
exit 0
STUBEOF
chmod +x "$MSTUB"
mounted_with() { # cmdline -> what mount_root ran for ext4
    printf '%s\n' "$1" > "$WORK/cmdline"
    : > "$WORK/mount.log"
    ( set +e; STORM_CMDLINE="$WORK/cmdline"; STORM_MOUNT="$MSTUB"; MOUNT_LOG="$WORK/mount.log"; export MOUNT_LOG
      . "$WORK/mode.sh" >/dev/null 2>&1; mount_root /dev/ublkb0 /sysroot )
    cat "$WORK/mount.log"
}
check "ro: ext4 mounted read-only" "-t ext4 -o ro /dev/ublkb0 /sysroot" "$(mounted_with "ro")"
check "rw: ext4 as before" "-t ext4 /dev/ublkb0 /sysroot" "$(mounted_with "rw")"

echo "network handoff:"
make_root() { # dir links|files
    rm -rf "$1"; mkdir -p "$1/etc" "$1/run" "$1/usr/bin"
    echo "golden" > "$1/usr/bin/tool"
    if [ "$2" = links ]; then
        ln -s /run/hostname "$1/etc/hostname"
        ln -s /run/resolv.conf "$1/etc/resolv.conf"
    else
        echo "old-name" > "$1/etc/hostname"
        echo "nameserver 0.0.0.0" > "$1/etc/resolv.conf"
    fi
}
digest() { # dir -> a digest of every path, link target and file content
    ( cd "$1" && find . | LC_ALL=C sort | while read -r p; do
        if [ -L "$p" ]; then echo "L $p $(readlink "$p")"
        elif [ -f "$p" ]; then echo "F $p $(cksum < "$p")"
        else echo "D $p"; fi
      done ) | cksum
}
printf 'nameserver 192.168.8.252\nsearch g8.lo\n' > "$WORK/resolv.conf"
handoff() { # root run ro
    ( set +e; STORM_SYSROOT="$1"; STORM_RUN="$2"; STORM_RESOLV="$WORK/resolv.conf"
      NODE_NAME="stormblock1"; ROOT_RO="$3"
      . "$WORK/handoff.sh" >/dev/null 2>&1 )
}

# stormcos: a read-only root whose /etc links to /run.
make_root "$WORK/root" links
before=$(digest "$WORK/root")
rm -rf "$WORK/run"; mkdir -p "$WORK/run"
chmod -R a-w "$WORK/root" 2>/dev/null || true
handoff "$WORK/root" "$WORK/run" 1
chmod -R u+w "$WORK/root" 2>/dev/null || true
check "read-only root: nothing under it changed" "$before" "$(digest "$WORK/root")"
check "/run/hostname holds the name" "stormblock1" "$(cat "$WORK/run/hostname" 2>/dev/null)"
check "/run/resolv.conf holds what DHCP gave" "$(cat "$WORK/resolv.conf")" "$(cat "$WORK/run/resolv.conf" 2>/dev/null)"
# What the booted root reads, once /run is its: the links resolve.
check "the root's /etc/hostname link names /run/hostname" "/run/hostname" "$(readlink "$WORK/root/etc/hostname")"

# A read-write root with links: still nothing written through them.
make_root "$WORK/root" links
before=$(digest "$WORK/root")
rm -rf "$WORK/run"; mkdir -p "$WORK/run"
handoff "$WORK/root" "$WORK/run" ""
check "read-write root with links: unchanged" "$before" "$(digest "$WORK/root")"
check "and /run has both" "stormblock1" "$(cat "$WORK/run/hostname")"

# An older read-write root (files, not links): /etc as before, and /run too.
make_root "$WORK/root" files
rm -rf "$WORK/run"; mkdir -p "$WORK/run"
handoff "$WORK/root" "$WORK/run" ""
check "older read-write root: /etc/hostname written" "stormblock1" "$(cat "$WORK/root/etc/hostname")"
check "older read-write root: /etc/resolv.conf written" "$(cat "$WORK/resolv.conf")" "$(cat "$WORK/root/etc/resolv.conf")"
check "and /run/resolv.conf too" "$(cat "$WORK/resolv.conf")" "$(cat "$WORK/run/resolv.conf")"

# An older root mounted ro: /etc not touched.
make_root "$WORK/root" files
before=$(digest "$WORK/root")
handoff "$WORK/root" "$WORK/run" 1
check "older root, ro: /etc not written" "$before" "$(digest "$WORK/root")"

[ "$fail" -eq 0 ] && echo "ALL PASS" || { echo "FAILED"; exit 1; }
