//! Disk-image formats a golden can come from — the *input* side.
//! (`super::formats` is the output side: the finished image converted to
//! qcow2/vhd/vmdk for a consumer.)
//!
//! A cloud image is a qcow2; a VM export is a raw disk or a qcow2; a
//! filesystem built by `mke2fs` is raw. The engine stores goldens as raw
//! volumes — the copy-on-write is the slab's, not the file's — so the only
//! thing a format decoder has to do is say what the raw bytes would be and
//! which clusters are worth writing. Detection is by magic, never by
//! extension: a cloud image called `.img` is a qcow2 more often than not.

pub mod ova;
pub mod qcow2;
pub mod vmdk;

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    Raw,
    Qcow2,
    /// A VMDK sparse extent (monolithicSparse or streamOptimized) or a
    /// text descriptor naming its extents.
    Vmdk,
    /// An OVA: a tar carrying a VMDK (and an OVF the engine does not need).
    Ova,
    /// Recognised, not supported: `vhd`, `vhdx`.
    Unsupported(&'static str),
}

impl std::fmt::Display for SourceFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceFormat::Raw => f.write_str("raw"),
            SourceFormat::Qcow2 => f.write_str("qcow2"),
            SourceFormat::Vmdk => f.write_str("vmdk"),
            SourceFormat::Ova => f.write_str("ova"),
            SourceFormat::Unsupported(n) => write!(f, "{n} (unsupported)"),
        }
    }
}

/// What the first bytes say the file is.
pub fn detect(head: &[u8]) -> SourceFormat {
    if head.len() >= 4 && &head[..4] == b"QFI\xfb" {
        return SourceFormat::Qcow2;
    }
    if head.len() >= 4 && &head[..4] == b"KDMV" {
        return SourceFormat::Vmdk;
    }
    if head.starts_with(b"# Disk DescriptorFile") {
        return SourceFormat::Vmdk;
    }
    // ustar: magic at 257. A tar whose first entry is an OVF or a VMDK is an
    // OVA as far as we are concerned.
    if head.len() >= 263 && &head[257..262] == b"ustar" {
        return SourceFormat::Ova;
    }
    if head.len() >= 8 && &head[..8] == b"vhdxfile" {
        return SourceFormat::Unsupported("vhdx");
    }
    if head.len() >= 8 && &head[..8] == b"conectix" {
        return SourceFormat::Unsupported("vhd");
    }
    SourceFormat::Raw
}

/// Read enough of a file to detect its format.
pub async fn detect_file(path: &Path) -> std::io::Result<SourceFormat> {
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(path).await?;
    let mut head = [0u8; 512];
    let n = f.read(&mut head).await?;
    Ok(detect(&head[..n]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_decides_not_extension() {
        assert_eq!(detect(b"QFI\xfb\0\0\0\x03"), SourceFormat::Qcow2);
        assert_eq!(detect(b"KDMV...."), SourceFormat::Vmdk);
        assert_eq!(detect(b"# Disk DescriptorFile\nversion=1"), SourceFormat::Vmdk);
        let mut tar = vec![0u8; 512];
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(detect(&tar), SourceFormat::Ova);
        assert_eq!(detect(b"conectix"), SourceFormat::Unsupported("vhd"));
        assert_eq!(detect(b"vhdxfile"), SourceFormat::Unsupported("vhdx"));
        assert_eq!(detect(&[0u8; 512]), SourceFormat::Raw);
        assert_eq!(detect(b"\xebc\x90"), SourceFormat::Raw);
    }
}
