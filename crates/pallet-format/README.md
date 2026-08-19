# stormblock-pallet-format

Read-only `no_std` reader for the stormblock pallet on-disk format v1.

> A **pallet** is a GPT partition containing a named, versioned, self-contained
> set of sealed member images and the manifest that describes them.

**There is no write path in this crate and there must never be one.** Firmware
links it — it runs before the kernel, before Secure Boot hands off, and before
anything can be debugged with a shell — so it stays small enough to read in one
sitting, and structurally incapable of writing.

## Why it is a crate

One on-disk format with two consumers — the engine that writes pallets and the
firmware that boots them — is exactly the shape that produces two
hand-maintained readers which must stay bit-compatible forever, and whose drift
fails as *the node does not boot*. So the decode side lives here, once:

- **stormblock** wraps it in the async I/O layer that publishes, verifies,
  activates and moves pallets. Its writer lays bytes down at the offsets
  `layout` defines, so emission has one implementation and every field has one
  definition.
- **stormuefi** compiles it for `x86_64-unknown-uefi` and reads pallets with no
  allocator, no runtime and no `async`.

## Using it

```toml
stormblock-pallet-format = { git = "https://github.com/glennswest/stormblock", default-features = false, features = ["verify"] }
```

```rust
use stormblock_pallet_format::{BlockReader, Pallet};

let pallet = Pallet::parse(&head)?;      // superblock + member and extent tables
pallet.verify_manifest()?;               // the quantity a signature covers
let kernel = pallet.find_role("kernel")?;
pallet.verify_member(&kernel, &reader, &mut scratch)?;   // content, through the extent map
let m = pallet.map(&kernel, offset)?;    // partition-relative: block, offset, run length
```

`BlockReader` is all the caller supplies. It reads **partition-relative** blocks
in the pallet's own `block_size`, which stormblock writes equal to the media's
sector size — so on a real device the two coincide and there is no unit to
convert.

## Features

| Feature | Default | What it adds |
|---|---|---|
| `verify` | yes | SHA-256: manifest recomputation, member content digests. Without it the crate can navigate a pallet but not prove anything about it. |
| `serde` | no | Derives on the small value types, for a host putting them in an API response. Firmware has no use for it. |

## What it will not do

Publish, allocate, activate, renumber priority, migrate, copy, move, convert,
retention, refcounting — all of that lives in stormblock, where the state, the
devices and the async runtime are.

## The specification

[`docs/pallets.md`](../../docs/pallets.md) in this repository. The tests here
work from **hand-built bytes** rather than from stormblock's writer: a decoder
tested only against its own encoder proves nothing about either.
