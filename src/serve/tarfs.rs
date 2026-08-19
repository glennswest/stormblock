//! Tar in and out of a volume's filesystem — the image path.
//!
//! The engine can put *files* into a volume it is holding (`fs::files`, served
//! at `/api/v1/volumes/{id}/files`), and it stops there on purpose. Its own
//! comment draws the line: "Writing *image* content — tar streams, whiteouts,
//! layer digests — stays with the consumer that owns the image." In this
//! profile that consumer is mk, acting for sbregistry, so this module is the
//! consumer's half of a seam upstream drew deliberately — not a fork of engine
//! policy.
//!
//! What it buys. Laying a container layer into a PVC previously meant: create
//! the volume, export it, have RouterOS attach it, mount it, untar over the
//! network, unmount, withdraw. Every one of those steps is a round trip
//! through an initiator that cannot be scripted reliably and cannot be done at
//! all while the volume is unexported. Here it is one HTTP request against a
//! volume nobody has attached: `fio-ext4` walks the ext2/3/4 structures in
//! userspace and keeps in step everything a check looks at — bitmaps, group
//! counts, superblock totals, every metadata checksum.
//!
//! Three properties matter to a caller:
//!
//! - **Nothing is held in memory.** The request body is an `AsyncRead` and the
//!   packed archive is a response stream. A 2 GB layer costs a 256 KiB pipe,
//!   which is the point on a router with a few hundred MB to spare.
//! - **Layers are layers.** With `whiteouts=true` a `.wh.<name>` entry
//!   *removes* `<name>` and `.wh..wh..opq` empties its directory, neither
//!   marker surviving into the filesystem. Unpack layers in order and the
//!   result is the image. Off by default, because in an ordinary tarball a
//!   name beginning `.wh.` is just a name.
//! - **A round trip is lossless.** Modes, ownership, timestamps, symlinks,
//!   hard links, device nodes and extended attributes (SELinux labels, ACLs)
//!   all survive out and back, and names are emitted sorted so one tree always
//!   produces one archive.

use std::sync::Arc;

use fio_ext4::archive::{self, Compression, PackOptions, UnpackOptions};
use fio_ext4::{UnpackReport, Volume};
use tokio::io::AsyncRead;

use crate::drive::BlockDevice;
use crate::fs::ext4::VolumeDevice;

/// Pipe between the packer and the response stream. One slot big enough that
/// the packer is not woken per tar block, small enough to be irrelevant to a
/// router's memory budget.
const PIPE_BYTES: usize = 256 * 1024;

/// Map the API's compression name onto the library's.
///
/// `auto` sniffs the magic bytes on the way in and means "no compression" on
/// the way out — there is nothing to sniff when writing.
pub fn parse_compression(name: Option<&str>) -> Result<Compression, String> {
    match name {
        None | Some("") | Some("auto") => Ok(Compression::Auto),
        Some("none") | Some("tar") => Ok(Compression::None),
        Some("gzip") | Some("gz") => Ok(Compression::Gzip),
        Some(other) => Err(format!(
            "unknown compression \"{other}\" — expected \"auto\", \"none\" or \"gzip\""
        )),
    }
}

/// The media type a packed archive is served as.
pub fn content_type(compression: Compression) -> &'static str {
    match compression {
        Compression::Gzip => "application/gzip",
        _ => "application/x-tar",
    }
}

/// Unpack an archive into `dev`'s filesystem, rooted at `into`.
///
/// Opened `opaque` rather than `thin`: a volume being unpacked into has
/// content — it was cloned from a template, or a previous layer was laid down
/// on it — so zeroing must write real zeros rather than discard. Getting that
/// backwards on a volume with old data underneath leaves the old bytes
/// showing through.
pub async fn unpack<R>(
    dev: &Arc<dyn BlockDevice>,
    src: R,
    into: &str,
    compression: Compression,
    whiteouts: bool,
) -> anyhow::Result<UnpackReport>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut vol = Volume::open(VolumeDevice::opaque(dev.clone()))
        .await
        .map_err(|e| anyhow::anyhow!("opening filesystem: {e}"))?;

    let options = UnpackOptions {
        into: into.to_string(),
        compression,
        whiteouts,
    };
    let report = archive::unpack_into(&mut vol, src, &options)
        .await
        .map_err(|e| anyhow::anyhow!("unpacking archive: {e}"))?;

    // `unpack_into` deliberately does not flush — the caller decides when the
    // image is finished with — and this is not optional bookkeeping. File data
    // and inodes are written as they change, but the bitmaps, group counts and
    // superblock totals are buffered, so a volume dropped without this leaves
    // names pointing at inodes the bitmap never marked used. `Volume::flush`
    // ends by flushing the device itself, so it covers the slab too.
    vol.flush()
        .await
        .map_err(|e| anyhow::anyhow!("flushing filesystem: {e}"))?;

    Ok(report)
}

/// Pack a subtree and hand back a reader over the archive.
///
/// Read-only: the volume is opened, walked and left alone.
///
/// The walk runs in its own task writing into a bounded pipe, so the response
/// starts flowing before the tree has been read and no part of the archive is
/// ever fully resident. The filesystem is opened *before* the task is spawned:
/// a volume with no filesystem on it must be a 4xx, not a 200 whose body turns
/// out to be empty.
///
/// A failure part-way through the walk can only truncate the stream — there is
/// no status code left to send by then. It is logged with the volume id, and
/// the client sees a tar that ends without its trailer, which every tar reader
/// treats as an error rather than as a complete archive.
pub async fn pack_stream(
    dev: &Arc<dyn BlockDevice>,
    volume_id: uuid::Uuid,
    from: &str,
    compression: Compression,
) -> anyhow::Result<tokio::io::DuplexStream> {
    // Fail before a byte of response has been committed.
    let vol = Volume::open(VolumeDevice::opaque(dev.clone()))
        .await
        .map_err(|e| anyhow::anyhow!("opening filesystem: {e}"))?;

    let options = PackOptions {
        from: from.to_string(),
        compression,
    };
    let (writer, reader) = tokio::io::duplex(PIPE_BYTES);
    tokio::spawn(async move {
        match archive::pack_from(&vol, writer, &options).await {
            Ok(report) => tracing::info!(
                "pack volume {volume_id} from {}: {} file(s), {} dir(s), {} byte(s)",
                options.from,
                report.files,
                report.directories,
                report.bytes
            ),
            // The writer half drops with this task, closing the pipe, so the
            // client's archive ends mid-stream.
            Err(e) => tracing::warn!("pack volume {volume_id} from {}: {e}", options.from),
        }
    });
    Ok(reader)
}

/// An unpack report as the API reports it.
pub fn unpack_json(r: &UnpackReport) -> serde_json::Value {
    serde_json::json!({
        "files": r.files,
        "directories": r.directories,
        "symlinks": r.symlinks,
        "hard_links": r.hard_links,
        "devices": r.devices,
        "xattrs": r.xattrs,
        "bytes": r.bytes,
        "removed": r.removed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compression_names_parse() {
        // Absent, empty and "auto" all sniff.
        assert_eq!(parse_compression(None).unwrap(), Compression::Auto);
        assert_eq!(parse_compression(Some("")).unwrap(), Compression::Auto);
        assert_eq!(parse_compression(Some("auto")).unwrap(), Compression::Auto);
        assert_eq!(parse_compression(Some("none")).unwrap(), Compression::None);
        assert_eq!(parse_compression(Some("tar")).unwrap(), Compression::None);
        assert_eq!(parse_compression(Some("gzip")).unwrap(), Compression::Gzip);
        assert_eq!(parse_compression(Some("gz")).unwrap(), Compression::Gzip);
        // Anything else is the caller's mistake, not a silent fallback: a
        // gzipped layer sent as "zstd" would otherwise be unpacked as tar and
        // fail deep inside the walk with a header error.
        assert!(parse_compression(Some("zstd")).is_err());
        assert!(parse_compression(Some("bzip2")).is_err());
    }

    #[test]
    fn media_type_follows_compression() {
        assert_eq!(content_type(Compression::Gzip), "application/gzip");
        assert_eq!(content_type(Compression::None), "application/x-tar");
        // Auto means "no compression" when writing, so it must not claim gzip.
        assert_eq!(content_type(Compression::Auto), "application/x-tar");
    }
}
