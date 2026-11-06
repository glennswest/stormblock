#!/usr/bin/env bash
# ci-ublk-qd-verify.sh — a ublk volume on a slow disk, served one request at a
# time and served at once (#264). Unprivileged on dev: the engine runs as root
# inside a QEMU guest, where ublk_drv and device-mapper are its own.
#
# The guest's disks go through dm-delay (8 ms per read, write and flush)
# so they behave like the 7200 rpm drive of an X9 blade rather than
# dev's storage. On each, the engine (the same binary) serves a volume over
# ublk, once with STORMBLOCK_UBLK_SERIAL=1 (the queue worker as it was: one
# request at a time) and once without, and the guest measures:
#
#   parallel reads   16 readers × 32 random 4 KiB O_DIRECT reads
#   reads under fsync  64 random 4 KiB reads while another process writes
#                    and fsyncs in a loop (a journal commit's FLUSH)
#   round trip       8 writers at distinct offsets, then everything read back
#                    and compared (correctness of concurrent service)
#
# And, with the engine serving at once (#267): a volume attached over ublk
# and given an ephemeral export, the export withdrawn (what the registry's reap
# of a kubelet's image clone does) — the volume must stay with its data, a
# DELETE be refused (409), and the volume go once the device is detached.
#
# And (#231): four volumes attached over ublk at once — each reply a block
# device that exists, all different, a listing answered meanwhile.
#
# And (#502): a detach and a delete while a mount of the device is being
# made (the slab suspended) are refused, and a detach answers only once the
# device is gone.
#
# And (#337): the slab's dm device suspended after a write, so an fsync's
# FLUSH is never answered — `/api/v1/health` must list it in `ublk_stuck`
# once it is 30 s old and the watchdog log it; after a resume the fsync
# returns and the entry goes.
#
# Pass: the round trips are exact in both modes, the #267 checks hold, and
# served at once is at least 3× faster on parallel reads than served serially.
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its modules
# (ublk_drv, dm-delay), a static busybox, dmsetup and curl (copied in with
# their libraries). Run on dev through sc-build:
#   sc-build 'sh ci-ublk-qd-verify.sh'
# NUMBERS=1 sc-build ... : only the timings, each mode three times, interleaved.
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
DMSETUP=$(command -v dmsetup || true)
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-ublk-qd.XXXXXX")
FAILS=0

say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
trap 'rm -rf "$W"' EXIT

for need in "$KERNEL" "$BUSYBOX" "$DMSETUP" "$CURL"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }

say "build"
cargo build --release --locked --bin stormblock --example ublk_take 2>&1 | tail -2
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/stormblock"
TAKE="${CARGO_TARGET_DIR:-$ROOT/target}/release/examples/ublk_take"
[ -x "$BIN" ] && [ -x "$TAKE" ] || { echo "FAIL: no binary"; exit 1; }

say "guest initramfs: busybox, stormblock, dmsetup, curl, ublk_drv, dm-delay"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount insmod ip sleep cat echo ls grep dd cmp poweroff dmesg head tail wc sed \
         awk cut tr kill seq mkdir rm timeout pidof ps touch od mv umount sync; do
    ln -sf busybox "$I/bin/$a"
done
for b in "$BIN" "$TAKE" "$DMSETUP" "$CURL"; do
    cp "$b" "$I/bin/$(basename "$b")"
    ldd "$b" | grep -o '/[^ ]*' | while read -r lib; do
        mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
    done
done
: > "$I/lib/mods/order"
for m in ublk_drv dm-delay ext4; do
    modprobe -S "$KVER" --show-depends "$m" 2>/dev/null | awk '$1=="insmod"{print $2}'
done | awk '!seen[$0]++' | while read -r ko; do
    base=$(basename "$ko" | sed 's/\.xz$//; s/\.zst$//')
    case "$ko" in
        *.xz) xz -dc "$ko" > "$I/lib/mods/$base" ;;
        *.zst) zstd -dcq "$ko" > "$I/lib/mods/$base" ;;
        *) cp "$ko" "$I/lib/mods/$base" ;;
    esac
    echo "$base" >> "$I/lib/mods/order"
done
cat > "$I/init" <<'EOF'
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs run /run; mount -t tmpfs tmp /tmp
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>&1 | sed "s/^/LOG insmod $m: /"; done
ip link set lo up
r() { echo "RESULT $1 $2"; }
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease)"
[ -e /dev/ublk-control ] || { r ublk_drv FAIL; poweroff -f; }
now() { cut -d' ' -f1 /proc/uptime; }
api() { curl -sf -m 120 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }
ms() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%d", (b-a)*1000}'; }

# One random 4 KiB block inside the first 64 MiB.
rnd() { echo $(( (RANDOM * 32768 + RANDOM) % 16384 )); }

run() {
    mode=$1; disk=$2; port=$3
    S=$(cat /sys/block/$disk/size) || { r $mode-disk "FAIL (no /dev/$disk)"; return; }
    dmsetup create --noudevsync slow$disk --table "0 $S delay /dev/$disk 0 8 /dev/$disk 0 8" \
        > /tmp/dm.out 2>&1 || { r $mode-dm "FAIL ($(cat /tmp/dm.out))"; return; }
    sed 's/^/LOG dmsetup: /' /tmp/dm.out
    # No udev here, so no /dev/mapper node: devtmpfs names it dm-N.
    slow=
    for d in /sys/block/dm-*; do
        [ "$(cat $d/dm/name)" = slow$disk ] && slow=/dev/${d##*/}
    done
    if [ ! -b "$slow" ]; then
        r $mode-dm "FAIL (no node for slow$disk)"
        echo "LOG sys/block: $(ls /sys/block)"; echo "LOG dev/mapper: $(ls /dev/mapper 2>&1)"
        echo "LOG modules: $(cut -d' ' -f1 /proc/modules | tr '\n' ' ')"
        dmsetup ls 2>&1 | sed 's/^/LOG dmsetup ls: /'; dmesg | tail -8 | sed 's/^/LOG /'
        return
    fi
    mkdir -p /run/sb$disk
    cat > /run/sb$disk.toml <<EOT
[management]
api_token = "t"
# Formatting a slab is destructive (#274): the same token is the admin one.
admin_token = "t"
listen_addr = "127.0.0.1:$port"
data_dir = "/run/sb$disk"
node_name = "ci-ublk"
discovery_disabled = true

[serve]
reconcile_secs = 1
EOT
    if [ "$mode" = serial ]; then export STORMBLOCK_UBLK_SERIAL=1; else unset STORMBLOCK_UBLK_SERIAL; fi
    # #505: the engine as before it: plain io_uring rings (no SINGLE_ISSUER +
    # DEFER_TASKRUN) and a command per request (no batch I/O), served at once.
    if [ "$mode" = plain ]; then export STORMBLOCK_URING_PLAIN=1 STORMBLOCK_UBLK_BATCH=0; else unset STORMBLOCK_URING_PLAIN STORMBLOCK_UBLK_BATCH; fi
    # #505 step 2: zero copy off in the engine as before it (`plain`) and in
    # `copy` (step 1: tuned rings and batch I/O, the data copied); `direct` is
    # zero copy with a command per request, `concurrent` with batch I/O.
    case "$mode" in plain|copy) export STORMBLOCK_UBLK_ZERO_COPY=0 ;; *) unset STORMBLOCK_UBLK_ZERO_COPY ;; esac
    [ "$mode" = direct ] && export STORMBLOCK_UBLK_BATCH=0
    # #505: no batch I/O, each queue's tags served by four threads (PER_IO_DAEMON).
    if [ "$mode" = split ]; then export STORMBLOCK_UBLK_BATCH=0 STORMBLOCK_UBLK_QUEUE_THREADS=4; else unset STORMBLOCK_UBLK_QUEUE_THREADS; fi
    RUST_LOG=stormblock=info stormblock --config /run/sb$disk.toml --data-dir /run/sb$disk \
        --no-iscsi > /run/engine-$mode.log 2>&1 &
    pid=$!
    for i in $(seq 1 100); do api http://127.0.0.1:$port/api/v1/health >/dev/null 2>&1 && break; sleep 0.2; done
    out=$(curl -s -m 120 -w ' HTTP%{http_code}' -H 'Authorization: Bearer t' -H 'Content-Type: application/json' \
        -X POST http://127.0.0.1:$port/api/v1/slabs \
        -d "{\"device_path\":\"$slow\",\"role\":\"data\"}")
    case "$out" in *HTTP2??) ;; *) r $mode-slab "FAIL ($out)"; sed 's/^/LOG /' /run/engine-$mode.log | tail -8; kill $pid; return ;; esac
    id=$(api -X POST http://127.0.0.1:$port/api/v1/volumes -d '{"name":"qd","size":"256M"}' \
        | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
    dev=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$id/attach -d '{"transport":"ublk"}' \
        | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
    [ -b "$dev" ] || { r $mode-attach "FAIL ($id '$dev')"; sed 's/^/LOG /' /run/engine-$mode.log | tail -8; kill $pid; return; }
    echo "GUEST $mode: volume $id on $dev over $slow (slow$disk)"

    step() { echo "STEP $mode $* at $(now)"; }
    step fill
    # Allocate the first 64 MiB so reads reach the disk.
    dd if=/dev/urandom of=/tmp/fill bs=1M count=64 2>/dev/null
    dd if=/tmp/fill of=$dev bs=1M count=64 oflag=direct 2>/dev/null
    echo 3 > /proc/sys/vm/drop_caches

    step parallel-reads
    t0=$(now)
    # `wait` with no pid would wait for the engine too: name the jobs.
    jobs=
    for j in $(seq 1 16); do
        ( for k in $(seq 1 32); do dd if=$dev of=/dev/null bs=4096 count=1 skip=$(rnd) iflag=direct 2>/dev/null; done ) &
        jobs="$jobs $!"
    done
    wait $jobs
    t1=$(now)
    echo "TIME $mode parallel-reads $(ms $t0 $t1)"

    step reads-under-fsync
    ( while [ ! -e /tmp/stop ]; do
        dd if=/dev/urandom of=$dev bs=4096 count=1 seek=$(rnd) oflag=direct conv=fsync 2>/dev/null
      done ) &
    w=$!
    sleep 1
    t0=$(now)
    for k in $(seq 1 64); do dd if=$dev of=/dev/null bs=4096 count=1 skip=$(rnd) iflag=direct 2>/dev/null; done
    t1=$(now)
    touch /tmp/stop; wait $w; rm -f /tmp/stop
    echo "TIME $mode reads-under-fsync $(ms $t0 $t1)"

    step round-trip
    jobs=
    dd if=/dev/urandom of=/tmp/pat bs=1M count=16 2>/dev/null
    for j in 0 1 2 3 4 5 6 7; do
        dd if=/tmp/pat of=$dev bs=64k skip=$((j * 32)) seek=$((2048 + j * 32)) count=32 oflag=direct 2>/dev/null &
        jobs="$jobs $!"
    done
    wait $jobs
    dd if=$dev of=/tmp/back bs=64k skip=2048 count=256 iflag=direct 2>/dev/null
    cmp -s /tmp/pat /tmp/back && r $mode-round-trip PASS || r $mode-round-trip FAIL

    if [ -e /etc/numbers ]; then
        fast_reads; fast_large
        kill -TERM $pid; wait $pid
        return
    fi
    [ "$mode" != serial ] && fast_reads
    [ "$mode" != serial ] && fast_large
    [ "$mode" != serial ] && features
    case "$mode" in concurrent|direct|split) in_place_check ;; esac
    [ "$mode" = serial ] || [ "$mode" = plain ] || in_place_clone
    [ "$mode" = concurrent ] && fd_cycles
    [ "$mode" = concurrent ] && taken
    [ "$mode" = concurrent ] && in_use
    [ "$mode" = concurrent ] && parallel_attach
    [ "$mode" = concurrent ] && attach_delete_race
    [ "$mode" = concurrent ] && mount_detach
    [ "$mode" = concurrent ] && fsck_snapshot
    [ "$mode" = concurrent ] && unanswered
    [ "$mode" = concurrent ] && in_place_kill

    step stop
    kill -TERM $pid; wait $pid
    grep -E "ERROR|panicked" /run/engine-$mode.log | head -5
}

# A hang must say where it is: the engine's log and every one of its threads'
# kernel stacks, then the guest powers off.
( sleep ${WATCHDOG:-1200}
  echo "WATCHDOG fired"
  for l in /run/engine-*.log; do sed "s|^|LOG ${l##*/}: |" $l | tail -15; done
  for p in $(pidof stormblock); do
    for t in /proc/$p/task/*; do
      echo "STACK $(cat $t/comm) $(cat $t/wchan)"; sed 's/^/STACK   /' $t/stack | head -8
    done
  done
  ps | grep -E ' (dd|stormblock|sh|sleep|timeout) ' | sed 's/^/PS /'
  poweroff -f ) &
# #267: an ephemeral export withdrawn while its volume is a ublk device on
# this node — what the registry's reap of an image clone does to the
# kubelet's mounted image. The volume must stay, with its data, and go once
# the device is detached.
in_use() {
    step in-use
    v=$(api -X POST http://127.0.0.1:$port/api/v1/volumes -d '{"name":"clone-image-1","size":"64M"}' \
        | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
    d=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$v/attach -d '{"transport":"ublk"}' \
        | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
    [ -b "$d" ] || { r in-use-attach "FAIL ($v '$d')"; return; }
    dd if=/dev/urandom of=/tmp/img bs=1M count=4 2>/dev/null
    dd if=/tmp/img of=$d bs=1M count=4 oflag=direct 2>/dev/null
    e=$(api -X POST http://127.0.0.1:$port/serve/v1/exports \
        -d "{\"volume_id\":\"$v\",\"protocol\":\"nvme-tcp\",\"ephemeral\":true}" \
        | sed -n 's/.*"export_id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
    [ -n "$e" ] || { r in-use-export FAIL; sed 's/^/LOG /' /run/engine-$mode.log | tail -5; return; }
    api -X DELETE http://127.0.0.1:$port/serve/v1/exports/$e >/dev/null || { r in-use-withdraw FAIL; return; }
    st=
    for i in $(seq 1 30); do
        st=$(api http://127.0.0.1:$port/serve/v1/exports/$e | sed -n 's/.*"state":"\([a-z]*\)".*/\1/p')
        [ "$st" = withdrawn ] && break
        sleep 1
    done
    sleep 3
    echo "GUEST in-use: export $e is ${st:-gone} after its delete"
    if api http://127.0.0.1:$port/api/v1/volumes/$v >/dev/null; then
        dd if=$d of=/tmp/imgback bs=1M count=4 iflag=direct 2>/dev/null
        cmp -s /tmp/img /tmp/imgback && r in-use-kept PASS || r in-use-kept "FAIL (data changed)"
    else
        r in-use-kept "FAIL (the volume was deleted under its device)"
    fi
    code=$(curl -s -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer t' \
        -X DELETE http://127.0.0.1:$port/api/v1/volumes/$v)
    [ "$code" = 409 ] && r in-use-delete-refused PASS || r in-use-delete-refused "FAIL ($code)"
    api -X DELETE http://127.0.0.1:$port/api/v1/volumes/$v/attach >/dev/null
    gone=
    for i in $(seq 1 20); do
        api http://127.0.0.1:$port/api/v1/volumes/$v >/dev/null 2>&1 || { gone=1; break; }
        sleep 1
    done
    [ -n "$gone" ] && r in-use-deleted-after-detach PASS || r in-use-deleted-after-detach FAIL
    grep -E "kept until it is detached|ephemeral volume .* deleted" /run/engine-$mode.log | sed 's/^/LOG /' | tail -3
}

# #231: four volumes attached at once. The wait for each device runs with
# no lock held, so they come up side by side, a listing answers meanwhile,
# and every reply is a block device that exists, each a different one.
parallel_attach() {
    step parallel-attach
    ids=
    for n in 1 2 3 4; do
        ids="$ids $(api -X POST http://127.0.0.1:$port/api/v1/volumes -d "{\"name\":\"pa$n\",\"size\":\"64M\"}" \
            | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)"
    done
    jobs=
    for v in $ids; do
        ( api -X POST http://127.0.0.1:$port/api/v1/volumes/$v/attach -d '{"transport":"ublk"}' \
            | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p' > /tmp/pa-$v ) &
        jobs="$jobs $!"
    done
    t0=$(now)
    api http://127.0.0.1:$port/api/v1/volumes > /dev/null && t1=$(now)
    wait $jobs
    devs=
    for v in $ids; do
        d=$(cat /tmp/pa-$v)
        [ -b "$d" ] || { r parallel-attach "FAIL ($v '$d')"; return; }
        devs="$devs $d"
    done
    # Distinct, in plain shell: the guest's busybox may lack sort.
    seen= dup=
    for d in $devs; do
        case " $seen " in *" $d "*) dup=$d ;; esac
        seen="$seen $d"
    done
    echo "GUEST parallel-attach:$devs; listing during them $(ms $t0 $t1) ms"
    [ -z "$dup" ] && r parallel-attach PASS || r parallel-attach "FAIL ($dup twice:$devs)"
    for v in $ids; do api -X DELETE http://127.0.0.1:$port/api/v1/volumes/$v/attach > /dev/null; done
}

# #493: the kubelet's root sweep deleted container roots while their ublk
# devices came up, and the devices then served volumes that were gone
# (EINVAL, ENXIO, EUCLEAN at the mount, or a 404 on the attach). Thirty
# fresh clones of an ext4 template, each attached and deleted at once:
# either the attach wins (the delete answers 409, the device has the
# clone's ext4 on it) or the delete does (the attach answers 404). Never a
# device whose volume is gone.
attach_delete_race() {
    step attach-delete-race
    g=$(api -X POST http://127.0.0.1:$port/api/v1/fstemplates -d '{"name":"race-root","size":"64M"}' \
        | sed -n 's/.*"sealed_volume_id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
    [ -n "$g" ] || { r attach-delete-race "FAIL (no template)"; sed 's/^/LOG /' /run/engine-$mode.log | tail -5; return; }
    won_attach=0 won_delete=0 bad=
    for n in $(seq 1 30); do
        c=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$g/clone -d "{\"name\":\"ctr-ct-$n\"}" \
            | grep -o '"\(volume_\)\{0,1\}id":"[0-9a-f-]*"' | head -1 | cut -d'"' -f4)
        [ -n "$c" ] || { bad="$bad clone$n"; continue; }
        ( curl -s -m 60 -o /tmp/ra-$n -w '%{http_code}' -H 'Authorization: Bearer t' -H 'Content-Type: application/json' \
            -X POST http://127.0.0.1:$port/api/v1/volumes/$c/attach -d '{"transport":"ublk"}' > /tmp/ra-$n.code ) &
        a=$!
        # The delete lands anywhere from before the attach to while its
        # device comes up.
        [ $((n % 3)) -eq 1 ] || sleep 0.0$((n % 10))
        dc=$(curl -s -m 60 -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer t' \
            -X DELETE http://127.0.0.1:$port/api/v1/volumes/$c)
        wait $a
        ac=$(cat /tmp/ra-$n.code)
        d=$(sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p' /tmp/ra-$n)
        case "$ac:$dc" in
        200:409|201:409)
            magic=$(dd if=$d bs=8 skip=135 count=1 2>/tmp/ra-$n.dd | od -An -tx1 | tr -d ' \n' | cut -c1-4)
            gc=$(curl -s -m 10 -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer t' http://127.0.0.1:$port/api/v1/volumes/$c)
            if [ "$magic" = 53ef ] && [ "$gc" = 200 ]; then
                won_attach=$((won_attach + 1))
            else
                bad="$bad $n:attached-but-magic[$magic]-get$gc"
                echo "LOG race $n: volume $c attach $ac delete $dc device '$d' get $gc dd: $(tr '\n' ' ' < /tmp/ra-$n.dd)"
                echo "LOG race $n reply: $(head -c 600 /tmp/ra-$n)"
                grep "$c" /run/engine-$mode.log | tail -12 | sed "s/^/LOG race $n engine: /"
            fi
            api -X DELETE http://127.0.0.1:$port/api/v1/volumes/$c/attach >/dev/null
            api -X DELETE http://127.0.0.1:$port/api/v1/volumes/$c >/dev/null ;;
        404:204|409:204)
            won_delete=$((won_delete + 1)) ;;
        *)
            bad="$bad $n:attach$ac/delete$dc${d:+($d)}" ;;
        esac
    done
    echo "GUEST attach-delete-race: the attach won $won_attach, the delete $won_delete"
    # No device is left on a volume that is gone.
    leftover=$(api http://127.0.0.1:$port/api/v1/volumes | grep -o '"name":"ctr-ct-[0-9]*"' | wc -l)
    [ -z "$bad" ] && [ "$leftover" = 0 ] && r attach-delete-race PASS \
        || r attach-delete-race "FAIL (${bad:-none bad}; $leftover left)"
}

# #502: server1's kubelet gave up on a container root while stormpump's
# mount of it was still being made (30 s on the SMR disk), detached it and
# deleted it; the device went on serving the mount, which read a deleted
# volume (EIO). Here the slab is suspended so a mount hangs reading its
# superblock, as there. While it is being made, a detach must answer 409
# (the device is claimed, though in no mount table yet) and a delete 409;
# resumed, the mount completes and reads the clone. Unmounted, a detach
# answers once the device is gone, so a delete right after it is 204.
mount_detach() {
    step mount-detach
    g=$(api -X POST http://127.0.0.1:$port/api/v1/fstemplates -d '{"name":"md-root","size":"64M"}' \
        | sed -n 's/.*"sealed_volume_id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
    c=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$g/clone -d '{"name":"md-ct"}' \
        | grep -o '"\(volume_\)\{0,1\}id":"[0-9a-f-]*"' | head -1 | cut -d'"' -f4)
    d=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$c/attach -d '{"transport":"ublk"}' \
        | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
    [ -b "$d" ] || { r mount-detach "FAIL (template '$g' clone '$c' device '$d')"; return; }
    mkdir -p /mnt/md
    del0=$(grep -c "has been deleted" /run/engine-$mode.log)
    dmsetup suspend --noflush slow$disk || { r mount-detach "FAIL (suspend)"; return; }
    ( mount -t ext4 "$d" /mnt/md; echo $? > /tmp/md.rc ) &
    m=$!
    sleep 3
    ac=$(curl -s -m 30 -o /tmp/md.det -w '%{http_code}' -H 'Authorization: Bearer t' \
        -X DELETE http://127.0.0.1:$port/api/v1/volumes/$c/attach)
    dc=$(curl -s -m 30 -o /tmp/md.del -w '%{http_code}' -H 'Authorization: Bearer t' \
        -X DELETE http://127.0.0.1:$port/api/v1/volumes/$c)
    dmsetup resume slow$disk
    wait $m
    rc=$(cat /tmp/md.rc 2>/dev/null)
    echo "GUEST mount-detach: detach during the mount $ac ($(head -c 200 /tmp/md.det)), delete $dc, mount rc ${rc:-?}"
    [ "$ac" = 409 ] && r mount-detach-refused PASS || r mount-detach-refused "FAIL ($ac)"
    [ "$dc" = 409 ] && [ "$rc" = 0 ] && r mount-detach-mounted PASS \
        || r mount-detach-mounted "FAIL (delete $dc, mount rc ${rc:-?})"
    del1=$(grep -c "has been deleted" /run/engine-$mode.log)
    [ "$del1" = "$del0" ] && r mount-detach-no-deleted-read PASS \
        || r mount-detach-no-deleted-read "FAIL ($((del1 - del0)) reads of a deleted volume)"
    umount /mnt/md 2>/dev/null
    ac=$(curl -s -m 30 -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer t' \
        -X DELETE http://127.0.0.1:$port/api/v1/volumes/$c/attach)
    dc=$(curl -s -m 30 -o /tmp/md.del -w '%{http_code}' -H 'Authorization: Bearer t' \
        -X DELETE http://127.0.0.1:$port/api/v1/volumes/$c)
    echo "GUEST mount-detach: after the unmount detach $ac, delete $dc ($(head -c 200 /tmp/md.del)), $d $( [ -b $d ] && echo still there || echo gone)"
    [ "$ac" = 200 ] && [ "$dc" = 204 ] && [ ! -b "$d" ] && r mount-detach-after PASS \
        || r mount-detach-after "FAIL (detach $ac delete $dc)"
}

# #496: the engine's fsck of a mounted volume. A mounted ext4 under directory
# churn (creates, renames, deletes; its journal busy) is checked again and
# again. Read live, block by block over time and its journal not replayed,
# #505: the rings with and without SINGLE_ISSUER + DEFER_TASKRUN, on a fast
# disk (the delay off): 16 readers, each 4000 sequential 4 KiB O_DIRECT reads
# in one process (no process start per I/O), wall time and the engine's CPU.
fast_reads() {
    step fast-reads
    t8=$(dmsetup table slow$disk)
    dmsetup suspend slow$disk && dmsetup reload slow$disk --table "$(echo "$t8" | awk '{$6=0; $9=0; print}')" \
        && dmsetup resume slow$disk || echo "LOG fast-reads: the delay stays on"
    # Five rounds: a build VM's timings move between runs; the median and
    # the spread say more than one.
    walls=; cpus=
    for round in 1 2 3 4 5; do
        cpu0=$(awk '{print $14 + $15}' /proc/$pid/stat)
        t0=$(now)
        jobs=
        for j in $(seq 0 15); do
            dd if=$dev of=/dev/null bs=4096 count=4000 skip=$((j * 1000)) iflag=direct 2>/dev/null &
            jobs="$jobs $!"
        done
        wait $jobs
        t1=$(now)
        cpu1=$(awk '{print $14 + $15}' /proc/$pid/stat)
        walls="$walls $(ms $t0 $t1)"; cpus="$cpus $(( (cpu1 - cpu0) * 10 ))"
    done
    # The guest's busybox has no sort: the median by awk.
    med() { echo $* | awk '{ for (i = 1; i <= NF; i++) a[i] = $i
        for (i = 2; i <= NF; i++) { v = a[i]; j = i - 1; while (j > 0 && a[j] > v) { a[j + 1] = a[j]; j-- } a[j + 1] = v }
        print a[int((NF + 1) / 2)] }'; }
    echo "TIME $mode fast-reads $(med $walls)"
    echo "TIME $mode fast-reads-cpu $(med $cpus)"
    echo "LOG $mode fast-reads rounds (ms):$walls; engine CPU (ms):$cpus"
    dmsetup suspend slow$disk && dmsetup reload slow$disk --table "$t8" && dmsetup resume slow$disk
}

# #505 step 2: what zero copy is for, large requests on a fast disk: 8 writers
# of 256 KiB × 64, twenty passes each, and 8 readers the same, over slots already written and
# flushed (in place), wall time and the engine's CPU, three rounds.
fast_large() {
    step fast-large
    t8=$(dmsetup table slow$disk)
    dmsetup suspend slow$disk && dmsetup reload slow$disk --table "$(echo "$t8" | awk '{$6=0; $9=0; print}')" \
        && dmsetup resume slow$disk || echo "LOG fast-large: the delay stays on"
    jobs=
    for j in 0 1 2 3 4 5 6 7; do
        dd if=/dev/zero of=$dev bs=256k count=64 seek=$((j * 64)) oflag=direct conv=fsync 2>/dev/null &
        jobs="$jobs $!"
    done
    wait $jobs
    for what in write read; do
        walls=; cpus=
        for round in 1 2 3; do
            cpu0=$(awk '{print $14 + $15}' /proc/$pid/stat)
            t0=$(now)
            jobs=
            for j in 0 1 2 3 4 5 6 7; do
                ( for pass in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
                    if [ $what = write ]; then
                        dd if=/dev/zero of=$dev bs=256k count=64 seek=$((j * 64)) oflag=direct 2>/dev/null
                    else
                        dd if=$dev of=/dev/null bs=256k count=64 skip=$((j * 64)) iflag=direct 2>/dev/null
                    fi
                done ) &
                jobs="$jobs $!"
            done
            wait $jobs
            t1=$(now)
            cpu1=$(awk '{print $14 + $15}' /proc/$pid/stat)
            walls="$walls $(ms $t0 $t1)"; cpus="$cpus $(( (cpu1 - cpu0) * 10 ))"
        done
        med() { echo $* | awk '{ for (i = 1; i <= NF; i++) a[i] = $i
            for (i = 2; i <= NF; i++) { v = a[i]; j = i - 1; while (j > 0 && a[j] > v) { a[j + 1] = a[j]; j-- } a[j + 1] = v }
            print a[int((NF + 1) / 2)] }'; }
        echo "TIME $mode large-$what $(med $walls)"
        echo "TIME $mode large-$what-cpu $(med $cpus)"
        echo "LOG $mode large-$what rounds (ms):$walls; engine CPU (ms):$cpus"
    done
    dmsetup suspend slow$disk && dmsetup reload slow$disk --table "$t8" && dmsetup resume slow$disk
}

counter() { curl -s -m 5 http://127.0.0.1:$port/api/v1/health | grep -o "\"$1\":[0-9]*" | head -1 | cut -d: -f2; }

# #505 step 2: a rewrite of slots written and flushed goes in place and reads
# back; a write across an extent and a first write beside it take the copy
# path on the same device.
in_place_check() {
    step in-place
    dd if=/tmp/pat of=$dev bs=64k seek=3008 count=256 oflag=direct conv=fsync 2>/dev/null
    p0=$(counter in_place); c0=$(counter copied)
    # Reversed in chunks, so the bytes differ from what is there.
    awk 'BEGIN { for (i = 0; i < 256; i++) print 255 - i }' > /tmp/order
    : > /tmp/pat2
    while read -r k; do dd if=/tmp/pat bs=64k skip=$k count=1 2>/dev/null >> /tmp/pat2; done < /tmp/order
    dd if=/tmp/pat2 of=$dev bs=64k seek=3008 count=256 oflag=direct conv=fsync 2>/dev/null
    p1=$(counter in_place); c1=$(counter copied)
    dd if=$dev of=/tmp/back2 bs=64k skip=3008 count=256 iflag=direct 2>/dev/null
    p2=$(counter in_place)
    cmp -s /tmp/pat2 /tmp/back2 && r $mode-in-place-data PASS || r $mode-in-place-data FAIL
    echo "LOG $mode in place: rewrite $((p1 - p0)) in place, $((c1 - c0)) copied; read back $((p2 - p1)) in place; retried $(counter in_place_retried)"
    [ $((p1 - p0)) -ge 128 ] && r $mode-in-place-writes PASS || r $mode-in-place-writes "FAIL ($((p1 - p0)) in place, $((c1 - c0)) copied)"
    [ $((p2 - p1)) -ge 128 ] && r $mode-in-place-reads PASS || r $mode-in-place-reads "FAIL ($((p2 - p1)))"
    # A write across two extents (1 MiB slots): the copy path, same device.
    c2=$(counter copied)
    # 1002 × 192 KiB ends 64 KiB past a 1 MiB boundary: one request, two extents.
    dd if=/tmp/pat of=$dev bs=192k seek=1002 count=1 oflag=direct conv=fsync 2>/dev/null
    dd if=$dev of=/tmp/back3 bs=192k skip=1002 count=1 iflag=direct 2>/dev/null
    dd if=/tmp/pat of=/tmp/want3 bs=192k count=1 2>/dev/null
    cmp -s /tmp/want3 /tmp/back3 && r $mode-across-extents PASS || r $mode-across-extents FAIL
    echo "LOG $mode across extents: $(( $(counter copied) - c2 )) copied"
    grep -iE 'ublk.*(data from|data to) the kernel' /run/engine-$mode.log | head -3 | sed 's/^/LOG /'
}

# #505 step 2: a clone shares its source's slots: its first write copies, the
# source keeps its bytes, and the clone's own slot then goes in place.
in_place_clone() {
    step in-place-clone
    s=$(api -X POST http://127.0.0.1:$port/api/v1/volumes -d '{"name":"zsrc","size":"16M"}' | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
    sd=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$s/attach -d '{"transport":"ublk"}' | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
    dd if=/tmp/pat of=$sd bs=64k count=128 oflag=direct conv=fsync 2>/dev/null
    c=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/snapshots -d "{\"name\":\"zclone\",\"source_volume_id\":\"$s\"}" | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
    cd=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$c/attach -d '{"transport":"ublk"}' | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
    [ -b "$sd" ] && [ -b "$cd" ] || { r $mode-clone "FAIL (no devices: '$sd' '$cd')"; return; }
    # Inside the first 64 KiB: what follows it stays the source's.
    dd if=/dev/urandom of=/tmp/cl bs=4096 count=8 2>/dev/null
    dd if=/tmp/cl of=$cd bs=4096 seek=2 count=8 oflag=direct conv=fsync 2>/dev/null
    dd if=/tmp/cl of=$cd bs=4096 seek=2 count=8 oflag=direct conv=fsync 2>/dev/null
    dd if=$cd of=/tmp/cl.b bs=4096 skip=2 count=8 iflag=direct 2>/dev/null
    cmp -s /tmp/cl /tmp/cl.b && a=ok || a=clone-data
    dd if=$sd of=/tmp/s.b bs=64k count=128 iflag=direct 2>/dev/null
    dd if=/tmp/pat of=/tmp/s.w bs=64k count=128 2>/dev/null
    cmp -s /tmp/s.w /tmp/s.b && b=ok || b=source-data
    dd if=$cd of=/tmp/c.b bs=64k skip=1 count=127 iflag=direct 2>/dev/null
    dd if=/tmp/pat of=/tmp/c.w bs=64k skip=1 count=127 2>/dev/null
    cmp -s /tmp/c.w /tmp/c.b && c2=ok || c2=clone-shared-data
    [ "$a$b$c2" = okokok ] && r $mode-clone PASS || r $mode-clone "FAIL ($a $b $c2)"
    for v in $c $s; do curl -s -m 30 -o /dev/null -H 'Authorization: Bearer t' -X DELETE http://127.0.0.1:$port/api/v1/volumes/$v/attach; done
}

# #505 step 2: what was written in place and fsync'd is there for an engine that
# never saw it end: killed, not stopped, and started again on the slab.
in_place_kill() {
    step in-place-kill
    dd if=/tmp/pat of=$dev bs=64k seek=3400 count=64 oflag=direct conv=fsync 2>/dev/null
    dd if=/tmp/pat of=/tmp/k.w bs=64k skip=64 count=64 2>/dev/null
    p0=$(counter in_place)
    dd if=/tmp/k.w of=$dev bs=64k seek=3400 count=64 oflag=direct conv=fsync 2>/dev/null
    p1=$(counter in_place)
    [ $((p1 - p0)) -ge 32 ] || r in-place-kill "FAIL (only $((p1 - p0)) in place)"
    kill -KILL $pid; wait $pid 2>/dev/null
    RUST_LOG=stormblock=info stormblock --config /run/sb$disk.toml --data-dir /run/sb$disk \
        --no-iscsi > /run/engine-$mode-2.log 2>&1 &
    pid=$!
    for i in $(seq 1 100); do api http://127.0.0.1:$port/api/v1/health >/dev/null 2>&1 && break; sleep 0.2; done
    # The slab was made through the API: opened again the same way, adopted.
    curl -s -m 60 -o /dev/null -H 'Authorization: Bearer t' -H 'Content-Type: application/json' \
        -X POST http://127.0.0.1:$port/api/v1/drives -d "{\"path\":\"$slow\"}"
    d2=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$id/attach -d '{"transport":"ublk"}' | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
    if [ -b "$d2" ]; then
        dd if=$d2 of=/tmp/k.b bs=64k skip=3400 count=64 iflag=direct 2>/dev/null
        cmp -s /tmp/k.w /tmp/k.b && r in-place-kill PASS || r in-place-kill "FAIL (data)"
    else
        r in-place-kill "FAIL (no device after the restart: '$d2')"
        sed 's/^/LOG /' /run/engine-$mode-2.log | tail -8
    fi
}

# #510: attach and detach one volume 40 times over ublk; the engine's open
# fds after are what they were before (pvetest1 ran out after ~805), and
# health reports them against the limit.
fd_cycles() {
    step fd-cycles
    v=$(api -X POST http://127.0.0.1:$port/api/v1/volumes -d '{"name":"fdc","size":"16M"}' \
        | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
    one() {
        d=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$v/attach -d '{"transport":"ublk"}' \
            | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
        [ -b "$d" ] && dd if=$d of=/dev/null bs=4096 count=1 iflag=direct 2>/dev/null
        curl -s -m 30 -o /dev/null -H 'Authorization: Bearer t' -X DELETE http://127.0.0.1:$port/api/v1/volumes/$v/attach
    }
    one; one
    f0=$(ls /proc/$pid/fd | wc -l)
    for i in $(seq 1 40); do one; done
    f1=$(ls /proc/$pid/fd | wc -l)
    echo "LOG fd-cycles: engine fds $f0 before 40 attach/detach cycles, $f1 after"
    [ $((f1 - f0)) -lt 10 ] && r fd-cycles PASS || r fd-cycles "FAIL ($f0 -> $f1)"
    h=$(curl -s -m 5 http://127.0.0.1:$port/api/v1/health)
    case "$h" in *'"fds":{"open":'*'"limit":'*) r fd-health PASS ;; *) r fd-health "FAIL ($h)" ;; esac
}

# #299: another process (a privileged pod) deletes a device the engine
# serves. The engine says so (ERROR, health `ublk_lost`, status degraded),
# does not tear down whatever gets that id next, and a new attach of the
# volume makes a new device that reads.
taken() {
    step taken
    v=$(api -X POST http://127.0.0.1:$port/api/v1/volumes -d '{"name":"taken","size":"16M"}' \
        | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
    d=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$v/attach -d '{"transport":"ublk"}' \
        | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
    n=${d#/dev/ublkb}
    echo "LOG taken: $d ($v)"
    timeout 30 ublk_take "$n" 2>&1 | sed 's/^/LOG ublk_take: /'
    sleep 2
    h=$(curl -s -m 5 http://127.0.0.1:$port/api/v1/health)
    echo "LOG taken health: $(echo "$h" | grep -o '"status":"[a-z]*"' | head -1) $(echo "$h" | grep -o '"ublk_lost":\[[^]]*\]')"
    case "$h" in
    '{"status":"degraded"'*'"ublk_lost":[{"ublk":"'"$d"'"'*'"how":"'[sd]*) r taken-reported PASS ;;
    *) r taken-reported FAIL ;;
    esac
    # Noticed at the stop a delete starts with: `stopped` or `deleted`.
    grep -qE "$d \(volume:.*\) was (stopped|deleted) by another process" /run/engine-$mode.log && r taken-logged PASS || r taken-logged FAIL
    d2=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$v/attach -d '{"transport":"ublk"}' \
        | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
    echo "LOG taken: attached again as $d2"
    sleep 3
    if [ -b "$d2" ] && dd if=$d2 of=/dev/null bs=4096 count=16 iflag=direct 2>/dev/null; then
        r taken-reattached PASS
    else
        r taken-reattached "FAIL ('$d2')"
    fi
    h=$(curl -s -m 5 http://127.0.0.1:$port/api/v1/health)
    case "$h" in *ublk_lost*) r taken-cleared "FAIL ($h)" ;; *) r taken-cleared PASS ;; esac
    curl -s -m 30 -o /dev/null -H 'Authorization: Bearer t' -X DELETE http://127.0.0.1:$port/api/v1/volumes/$v/attach
}

# #505: health says what the kernel offers, what the device uses, and how
# the rings were built (tuned unless asked plain).
features() {
    h=$(curl -s -m 5 http://127.0.0.1:$port/api/v1/health)
    echo "LOG $mode ublk_features: $(echo "$h" | grep -o '"ublk_features":{[^}]*}')"
    case "$mode:$h" in
    concurrent:*'"used":['*'AUTO_BUF_REG'*'BATCH_IO'*'"rings_tuned":'[1-9]*'"in_place":'[1-9]*) r features-tuned PASS ;;
    direct:*'"used":['*'BATCH_IO'*) r features-direct "FAIL (batch I/O used)" ;;
    direct:*'"used":['*'AUTO_BUF_REG'*'"in_place":'[1-9]*) r features-direct PASS ;;
    copy:*'"used":['*'AUTO_BUF_REG'*) r features-copy "FAIL (zero copy used)" ;;
    copy:*'"used":['*'BATCH_IO'*'"in_place":0'*) r features-copy PASS ;;
    plain:*'"used":['*'AUTO_BUF_REG'*) r features-plain "FAIL (zero copy used)" ;;
    plain:*'"used":['*'BATCH_IO'*) r features-plain "FAIL (batch I/O used)" ;;
    plain:*'"offered":['*'"rings_tuned":0,"rings_plain":'[1-9]*) r features-plain PASS ;;
    split:*'"used":['*'BATCH_IO'*) r features-split "FAIL (batch I/O used)" ;;
    split:*'"rings_tuned":'[4-9]*|split:*'"rings_tuned":'[1-9][0-9]*)
        grep -q "4 threads a queue" /run/engine-$mode.log && r features-split PASS \
            || r features-split "FAIL (no split queues in the log)" ;;
    *) r features-$mode "FAIL" ;;
    esac
}

# the check sees blocks from different moments: checksums and counts no
# mount would see. Checked the default way (a point-in-time snapshot, its
# journal replayed) it must be clean every time, and after the unmount the
# volume itself must be clean. No check's snapshot is left behind.
fsck_snapshot() {
    step fsck-snapshot
    # Not a timing test: the slab's 8 ms per I/O off for this phase (a check
    # reads the filesystem's metadata), back on after.
    t8=$(dmsetup table slow$disk)
    dmsetup suspend slow$disk && dmsetup reload slow$disk --table "$(echo "$t8" | awk '{$6=0; $9=0; print}')" \
        && dmsetup resume slow$disk || echo "LOG fsck-snapshot: the delay stays on"
    g=$(api -X POST http://127.0.0.1:$port/api/v1/fstemplates -d '{"name":"fsck-root","size":"64M"}' \
        | sed -n 's/.*"sealed_volume_id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
    c=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$g/clone -d '{"name":"fsck-state"}' \
        | grep -o '"\(volume_\)\{0,1\}id":"[0-9a-f-]*"' | head -1 | cut -d'"' -f4)
    d=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$c/attach -d '{"transport":"ublk"}' \
        | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
    mkdir -p /mnt/f
    if [ -z "$c" ] || [ -z "$d" ] || ! mount -t ext4 "$d" /mnt/f; then
        r fsck-snapshot "FAIL (no mounted clone: template '$g' clone '$c' device '$d')"; return
    fi
    ( i=0; while [ ! -e /tmp/churn-stop ]; do
        i=$((i + 1)); mkdir -p /mnt/f/d$((i % 23))/s$((i % 7))
        echo $i > /mnt/f/d$((i % 23))/s$((i % 7))/f$i
        mv /mnt/f/d$((i % 23))/s$((i % 7))/f$i /mnt/f/d$(((i + 5) % 23))/m$i 2>/dev/null
        rm -f /mnt/f/d$(((i + 11) % 23))/m$((i - 40)) 2>/dev/null
        [ $((i % 97)) -eq 0 ] && rm -rf /mnt/f/d$(((i + 3) % 23))
        [ $((i % 50)) -eq 0 ] && sync
      done ) &
    churn=$!
    sleep 3
    live_bad=0 live_csum=0 snap_bad=0 snap_note=
    rounds=${FSCK_ROUNDS:-10}
    for n in $(seq 1 $rounds); do
        api -X POST "http://127.0.0.1:$port/api/v1/volumes/$c/fsck?live=true" > /tmp/fl 2>/dev/null
        grep -q '"clean":true' /tmp/fl || live_bad=$((live_bad + 1))
        live_csum=$((live_csum + $(grep -o '"code":"[a-z-]*csum[a-z-]*"' /tmp/fl | wc -l)))
        api -X POST "http://127.0.0.1:$port/api/v1/volumes/$c/fsck" > /tmp/fs 2>/dev/null
        if ! grep -q '"view":"snapshot"' /tmp/fs || ! grep -q '"clean":true' /tmp/fs; then
            snap_bad=$((snap_bad + 1))
            [ -n "$snap_note" ] || snap_note=$(head -c 700 /tmp/fs)
        fi
        sleep 0.$((n % 7))
    done
    touch /tmp/churn-stop; wait $churn
    echo "GUEST fsck-snapshot: live reads not clean $live_bad/$rounds (checksum findings $live_csum); snapshots not clean $snap_bad/$rounds"
    [ -n "$snap_note" ] && echo "LOG fsck-snapshot first bad snapshot check: $snap_note"
    umount /mnt/f
    api -X DELETE http://127.0.0.1:$port/api/v1/volumes/$c/attach >/dev/null
    api -X POST "http://127.0.0.1:$port/api/v1/volumes/$c/fsck?live=true" > /tmp/fu 2>/dev/null
    grep -q '"clean":true' /tmp/fu && r fsck-unmounted-clean PASS \
        || r fsck-unmounted-clean "FAIL ($(head -c 500 /tmp/fu))"
    left=$(api http://127.0.0.1:$port/api/v1/volumes | grep -o '"name":"fsck-snapshot-[^"]*"' | wc -l)
    [ "$snap_bad" = 0 ] && r fsck-snapshot PASS || r fsck-snapshot "FAIL ($snap_bad/$rounds snapshot checks not clean)"
    [ "$left" = 0 ] && r fsck-snapshot-deleted PASS || r fsck-snapshot-deleted "FAIL ($left left)"
    api -X DELETE http://127.0.0.1:$port/api/v1/volumes/$c >/dev/null
    dmsetup suspend slow$disk && dmsetup reload slow$disk --table "$t8" && dmsetup resume slow$disk
}

# #337: a flush the disk below never answers. The slab's dm device is
# suspended (I/O queued, not failed) after a write, so the fsync's FLUSH
# waits in the engine. Health must name it once it is 30 s old, and the
# watchdog log it; after a resume the fsync returns and the entry goes.
unanswered() {
    step unanswered
    dd if=/dev/urandom of=$dev bs=4096 count=1 seek=100 oflag=direct 2>/dev/null
    dmsetup suspend --noflush slow$disk || { r unanswered-suspend FAIL; return; }
    dd if=/dev/null of=$dev bs=4096 count=0 conv=fsync,notrunc 2>/dev/null &
    f=$!
    sleep 36
    h=$(curl -s -m 5 http://127.0.0.1:$port/api/v1/health)
    echo "LOG health while suspended: $h"
    case "$h" in *'"ublk_stuck"'*'"op":"flush"'*)
        r unanswered-named PASS ;;
        *) r unanswered-named "FAIL (no flush in ublk_stuck)" ;; esac
    grep -q "unanswered for 30s or more: flush" /run/engine-$mode.log && r unanswered-logged PASS \
        || r unanswered-logged FAIL
    dmsetup resume slow$disk
    wait $f && r unanswered-fsync-returned PASS || r unanswered-fsync-returned FAIL
    sleep 3
    h=$(curl -s -m 5 http://127.0.0.1:$port/api/v1/health)
    case "$h" in *ublk_stuck*) r unanswered-cleared "FAIL ($h)" ;; *) r unanswered-cleared PASS ;; esac
}

# NUMBERS=1: only the timings, each mode three times, interleaved, so a build
# VM's drift lands on none of them.
if [ -e /etc/numbers ]; then
    port=9100
    for d in vda vdb vdc vdd vde vdf vdg vdh vdi vdj vdk vdl; do
        case $port in 9100|9104|9108) m=direct ;; 9101|9105|9109) m=concurrent ;; 9102|9106|9110) m=copy ;; *) m=plain ;; esac
        run $m $d $port
        port=$((port + 1))
    done
    echo "GUEST done"
    poweroff -f
fi
run serial vda 9091
run concurrent vdb 9092
run plain vdc 9093
run split vdd 9094
run copy vde 9095
run direct vdf 9096
echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
[ -n "${NUMBERS:-}" ] && touch "$I/etc/numbers"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"
truncate -s 1G "$W/a.img" "$W/b.img" "$W/c.img" "$W/d.img" "$W/e.img" "$W/f.img" \
    "$W/g.img" "$W/h.img" "$W/i.img" "$W/j.img" "$W/k.img" "$W/l.img"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 1500 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 2048 -smp 4 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" \
    -drive file="$W/a.img",if=virtio,format=raw \
    -drive file="$W/b.img",if=virtio,format=raw \
    -drive file="$W/c.img",if=virtio,format=raw \
    -drive file="$W/d.img",if=virtio,format=raw \
    -drive file="$W/e.img",if=virtio,format=raw \
    -drive file="$W/f.img",if=virtio,format=raw \
    -drive file="$W/g.img",if=virtio,format=raw -drive file="$W/h.img",if=virtio,format=raw \
    -drive file="$W/i.img",if=virtio,format=raw -drive file="$W/j.img",if=virtio,format=raw \
    -drive file="$W/k.img",if=virtio,format=raw -drive file="$W/l.img",if=virtio,format=raw > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|TIME|LOG|STEP|STACK|PS|WATCHDOG)|ERROR|panick'

if [ -n "${NUMBERS:-}" ]; then
    echo "== numbers: done"; exit 0
fi
t() { tr -d '\r' < "$W/guest.log" | awk -v m="$1" -v w="$2" '$1=="TIME" && $2==m && $3==w {print $4}'; }
SP=$(t serial parallel-reads); CP=$(t concurrent parallel-reads)
SF=$(t serial reads-under-fsync); CF=$(t concurrent reads-under-fsync)
echo "parallel reads:    serial ${SP:-?} ms, concurrent ${CP:-?} ms"
echo "reads under fsync: serial ${SF:-?} ms, concurrent ${CF:-?} ms"
echo "fast reads (#505, median of 5): tuned rings + batch I/O $(t concurrent fast-reads) ms, engine CPU $(t concurrent fast-reads-cpu) ms; as before (plain, no batch) $(t plain fast-reads) ms, CPU $(t plain fast-reads-cpu) ms; no batch, 4 threads a queue $(t split fast-reads) ms, CPU $(t split fast-reads-cpu) ms"
for w in fast-reads large-write large-read; do
    echo "$w (#505 step 2, ms, engine CPU ms): as before $(t plain $w) / $(t plain $w-cpu); step 1, copied $(t copy $w) / $(t copy $w-cpu); zero copy + batch $(t concurrent $w) / $(t concurrent $w-cpu); zero copy $(t direct $w) / $(t direct $w-cpu); zero copy, 4 threads a queue $(t split $w) / $(t split $w-cpu)"
done
for m in concurrent-in-place-data concurrent-in-place-writes concurrent-in-place-reads concurrent-across-extents \
         direct-in-place-data direct-in-place-writes direct-in-place-reads direct-across-extents \
         split-in-place-data split-in-place-writes split-in-place-reads split-across-extents \
         concurrent-clone direct-clone split-clone copy-clone copy-round-trip direct-round-trip features-copy features-direct in-place-kill \
         serial-round-trip concurrent-round-trip in-use-kept in-use-delete-refused in-use-deleted-after-detach \
         fsck-snapshot fsck-snapshot-deleted fsck-unmounted-clean \
         unanswered-named unanswered-logged unanswered-fsync-returned unanswered-cleared \
         parallel-attach attach-delete-race plain-round-trip split-round-trip features-tuned features-plain features-split fd-cycles fd-health taken-reported taken-logged taken-reattached taken-cleared \
         mount-detach-refused mount-detach-mounted mount-detach-no-deleted-read mount-detach-after; do
    # Not `grep -q`: it stops at the match, tr dies of SIGPIPE, and pipefail
    # calls that a failure (taken-logged, once the log outgrew the pipe).
    tr -d '\r' < "$W/guest.log" | grep "^RESULT $m PASS" > /dev/null || fail "$m"
done
if [ -n "$SP" ] && [ -n "$CP" ] && [ "$CP" -gt 0 ]; then
    [ $((SP)) -ge $((CP * 3)) ] || fail "served at once is not 3x faster on parallel reads ($SP vs $CP ms)"
else
    fail "no timings"
fi
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
