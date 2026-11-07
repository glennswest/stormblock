#!/bin/sh
# /init reads what the node needs into the page cache before any unit starts
# (#484). Runs the real block, extracted from the generator, with `cat` stubbed
# to log what it was asked to read.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
GEN="$HERE/../scripts/build-stormblock-initramfs.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

sed -n '/# --- BEGIN boot read-ahead/,/# --- END boot read-ahead/p' "$GEN" > "$WORK/block.sh"
[ -s "$WORK/block.sh" ] || { echo "FAIL: could not extract the block"; exit 1; }

fail=0
ok() { if [ "$2" = 0 ]; then echo "  ok    $1"; else echo "  FAIL  $1"; fail=1; fi; }
has() { grep -q "$2" "$1" && echo 0 || echo 1; }
hasnt() { grep -q "$2" "$1" && echo 1 || echo 0; }

cat > "$WORK/stubcat" <<'S'
#!/bin/sh
echo "${1#"$STORM_SYSROOT"}" >> "$RA_LOG"
[ -z "${RA_SLEEP:-}" ] || sleep "$RA_SLEEP"
S
chmod +x "$WORK/stubcat"

root() { # a fresh tree: name
    R="$WORK/$1"
    mkdir -p "$R/p/stormblock/sub" "$R/p/rustkube-node" "$R/p/other" "$R/sbin" "$R/etc/stormblock"
    head -c 2048 /dev/zero > "$R/p/stormblock/stormblock"
    head -c 1024 /dev/zero > "$R/p/stormblock/sub/lib"
    head -c 1048576 /dev/zero > "$R/p/rustkube-node/kubelet"
    head -c 10 /dev/zero > "$R/p/other/rdp"
    head -c 10 /dev/zero > "$R/sbin/init"
    echo "$R"
}
: > "$WORK/cmdline"
run() { # root name [env…]: the log in $WORK/log.<name>, output in $WORK/out.<name>
    R=$1; N=$2; shift 2
    : > "$WORK/log.$N"
    env STORM_SYSROOT="$R" STORM_READAHEAD_CAT="$WORK/stubcat" STORM_CMDLINE="$WORK/cmdline" \
        RA_LOG="$WORK/log.$N" "$@" sh "$WORK/block.sh" > "$WORK/out.$N" 2>&1
}

echo "the default list"
R=$(root d); run "$R" d
ok "the engine's volume is read" "$(has "$WORK/log.d" '^/p/stormblock/stormblock$')"
ok "under a directory too" "$(has "$WORK/log.d" '^/p/stormblock/sub/lib$')"
ok "the kubelet" "$(has "$WORK/log.d" '^/p/rustkube-node/kubelet$')"
ok "the root's init" "$(has "$WORK/log.d" '^/sbin/init$')"
ok "not a unit Ready does not need" "$(hasnt "$WORK/log.d" 'other/rdp')"
[ "$(head -1 "$WORK/log.d" | cut -d/ -f3)" = stormblock ]; ok "the engine first" $?
ok "said, with the list" "$(has "$WORK/out.d" 'Boot read-ahead: 4 file(s), 1 MiB in .* s (list: default)')"

echo "a volume this node does not carry is no failure"
R=$(root m); rm -rf "$R/p/rustkube-node"; run "$R" m
ok "the rest are read" "$(has "$WORK/log.m" '^/sbin/init$')"

echo "the release's list"
R=$(root l); printf '# control plane\n/p/other/rdp  # a comment\n/missing\n' > "$R/etc/stormblock/boot-readahead"; run "$R" l
ok "only what it names" "$(has "$WORK/log.l" '^/p/other/rdp$')"
ok "nothing else" "$(hasnt "$WORK/log.l" 'stormblock/stormblock')"
ok "named as the source" "$(has "$WORK/out.l" 'list: /etc/stormblock/boot-readahead')"
R=$(root e); : > "$R/etc/stormblock/boot-readahead"; run "$R" e
[ ! -s "$WORK/log.e" ]; ok "an empty list reads nothing" $?

echo "off"
R=$(root o); echo "quiet rd.stormblock.readahead=off" > "$WORK/cmdline"; run "$R" o; : > "$WORK/cmdline"
[ ! -s "$WORK/log.o" ]; ok "reads nothing" $?
ok "says so" "$(has "$WORK/out.o" 'off (rd.stormblock.readahead=off)')"

echo "the bounds"
R=$(root b); run "$R" b STORM_READAHEAD_MIB=0
[ ! -s "$WORK/log.b" ]; ok "past the size bound nothing is read" $?
ok "and said" "$(has "$WORK/out.b" 'stopped at its memory bound')"
printf 'MemAvailable:  1024 kB\n' > "$WORK/meminfo"; R=$(root a); run "$R" a STORM_MEMINFO="$WORK/meminfo"
ok "a quarter of the memory available: under it" "$(has "$WORK/log.a" '^/p/stormblock/stormblock$')"
ok "a quarter of the memory available: the kubelet is past it" "$(hasnt "$WORK/log.a" 'kubelet')"
R=$(root t); t0=$(cut -d' ' -f1 /proc/uptime); run "$R" t RA_SLEEP=3 STORM_READAHEAD_SECS=1
t1=$(cut -d' ' -f1 /proc/uptime)
awk -v a="$t0" -v b="$t1" 'BEGIN { exit !(b - a < 2.5) }'; ok "a slow read is cut at the time bound" $?
ok "and said" "$(has "$WORK/out.t" 'stopped at its 1 s')"

[ "$fail" = 0 ] && echo "PASS" || { echo "FAILED"; exit 1; }
