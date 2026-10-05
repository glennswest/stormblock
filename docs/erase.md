# Secure delete (#286)

When a volume is deleted, every slot it was the last user of is
**overwritten before it goes back to the free pool**. One pass of zeros is the
default. A delete may ask for more passes, but never fewer than the node's
default.

Before #286 a freed slot kept its bytes until a later tenant wrote over them.
No consumer could *read* them back: a slot's first write zero-fills the whole
slot (#171), and discard was best effort. But the bytes stayed on the media,
where anyone holding the drive could read them.

## What happens to a freed slot

1. **It is marked `Erasing`.** When a slot's last reference goes (a volume or
   clone deleted, a copy-on-write's old slot, a discard, a GC'd orphan, a
   drain's source) and its slab has an erase level, the slot is marked
   `Erasing` (state 3 in the slot table) instead of free.
   - Its entry is written at once. It keeps the volume the slot was allocated
     for, and stores the erase level in its share-count field.
   - It is not put in the free bitmap. Nothing can allocate it, share it,
     free it again, or map it.
   - GC, restore and `rebuild_from_slabs` all count it as nobody's.
2. **The eraser overwrites it.** The eraser is a background task, started by
   the daemon and by `adopt-ublk`. It takes the slot in a batch of up to 16 and,
   with no lock held:
   - writes each pass, with a device flush after each one;
   - for the DoD levels, reads the last pass back and compares it;
   - discards the range afterwards, except on a spinning disk.
3. **It is freed the ordinary way.** Its free entry is written, and the slot
   becomes reusable once that entry is durable (#171).

**After a crash or a stop.** Opening a slab puts every `Erasing` entry back in
the queue, so an interrupted erase is finished, not skipped. An erased slot
whose free never became durable is erased again, which does no harm.

**Foreground I/O comes first.** When any volume was read or written since the
last slot, the eraser waits for as long as that slot took, up to 0.5 s. This is
the same rule the flow-over follows (#269).

**Space.** An `Erasing` slot counts as allocated until it is erased:
`free_slots` does not include it, and `/api/v1/slabs` reports `erasing_slots`.
A full slab only refills as fast as its erases finish.

**Discards cost writes.** A consumer's discard (`fstrim`, ublk `DISCARD`) that
frees whole slots now writes over them. That is a 1 MiB write per freed slot
at `once`, and seven at `dod7`.

## Levels

| level | passes | read back |
|---|---|---|
| `none` | none; freed and discarded as before #286 | — |
| `once` (default) | zeros | no |
| `dod3` | 0x00, 0xFF, random (DoD 5220.22-M E) | the last pass |
| `dod7` | 0x00, 0xFF, random, random, 0x00, 0xFF, random (DoD 5220.22-M ECE) | the last pass |

Where a level is set:

- **For the node:** `[erase] default` in the config file, `once` when unset.
  It applies to every local slab. A slab reached over a fabric is not erased
  (`nvme-tcp://`, `iscsi://`), because it is another engine's volume, such as
  the appliance's per-boot clone during a flow-over. Its own engine erases what
  it frees.
- **For one delete:** `DELETE /api/v1/volumes/{id}?erase=dod3`. The higher of
  this and the node's default applies to every slot the delete frees.
  - Slots the volume still shares with another volume are not freed, so they
    are not erased. A clone's own copy-on-write slots are freed; its golden's
    slots are not.
  - Library callers use `VolumeManager::delete_volume_erasing`.
- **Not yet:** a level stored on the volume itself, or one per class. Today the
  level comes from the node and from the delete call.

## The audit record

Each volume whose freed slots were all erased gets one record. It is logged at
info, served by `GET /api/v1/erasures`, and kept in `<data_dir>/erasures.json`
(the last 1000 records). A record looks like this:

```json
{"volume":"…","level":"dod3","passes":3,"slots":3,"bytes":3145728,
 "verified":true,"discarded":true,"started":1759700000,"finished":1759700001,
 "duration_ms":412,"retries":0}
```

- `volume` is the volume the slots were **allocated for**. For a shared slot
  freed when its last sharer went, that is the volume that wrote it first.
- `retries` counts slots whose erase failed and went back in the queue. A slot
  that keeps failing stays `Erasing`: it is never handed out.

`GET /api/v1/erasures` also returns the node's `default` level,
`pending_slots`, and the volumes being erased right now (`running`: slots done
and slots left).

**Metrics:**

- `stormblock_erase_pending_slots`
- `stormblock_erased_slots_total{level}`
- `stormblock_erased_bytes_total`

## What an overwrite does not do on flash

On an SSD or NVMe drive, an overwrite goes to fresh cells, chosen by the flash
translation layer. The old cells keep their charge until the drive
garbage-collects them, and over-provisioned and retired blocks are never
addressable at all. Overwrite plus discard makes the data unreachable through
the drive's interface and makes recovery much harder. **It is not a complete
erase.**

Only two things are complete on flash:

- **The drive's own sanitize** (NVMe Sanitize / Format with Secure Erase, SAS
  SANITIZE). It works on a whole drive, so it is for retiring one.
  stormdrive's sanitize is that path.
- **A crypto-erase** (below).

On a spinning disk, one overwrite of an addressable sector is enough for any
practical recovery. Reallocated sectors are the exception, and only a sanitize
reaches those.

## Crypto-erase (designed, not built)

Crypto-erase covers a single volume on flash.

- Every volume gets a data key, generated when the volume is created. Its slots
  are encrypted with it: AES-XTS, with the key and tweak taken from the slot's
  physical address and generation.
- The key is wrapped by the node's key and kept in the volume record.
- Deleting the volume destroys its key, after which its slots are noise
  wherever their cells are. The overwrite then only tidies up.

Before it can be built:

- **Clones and snapshots.** These share slots across volumes, so a key cannot
  belong to one volume. It must belong to the lineage: a golden and every clone
  of it share its key, and a clone's own copy-on-write slots use the clone's
  key. A slot then records which key it was written with: a key id in the slot
  entry's reserved bytes, 40..60.
- **Destroying a key** is possible only when no live volume still holds a slot
  written with it. It needs the share counts the slot table already keeps,
  grouped by key.
- **The node's key** must live somewhere that does not travel with the drives:
  a TPM, or the forge's key service. A copy on the same drive defeats the
  purpose.
- **The cost.** It costs CPU on every I/O (AES-NI: about 1–2 GB/s per core). It
  also blocks deduplicated golden sharing across lineages, and that sharing
  does not exist today.

This is follow-up work.

## Where it is not on

- **The initramfs engine** (`boot-local`, `boot-claim`). It does not start the
  eraser and frees as before. Its frees during the few seconds of a boot are not
  erased.
- **An engine older than #286** reads an `Erasing` entry as free. Rolling an
  engine back gives those slots back without overwriting them, which is no worse
  than before.
