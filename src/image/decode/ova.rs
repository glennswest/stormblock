//! An OVA is a tar. The engine wants the VMDK inside it and nothing else:
//! walk the ustar headers, find the first `.vmdk` member, and hand back a
//! window over its bytes. No extraction, no temp file the size of the disk.

use std::path::Path;

use super::vmdk::Window;

/// Where the first VMDK member sits in the archive: `(offset, size)`.
pub async fn find_vmdk(path: &Path) -> std::io::Result<Option<(u64, u64, String)>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = tokio::fs::File::open(path).await?;
    let total = f.metadata().await?.len();
    let mut off = 0u64;
    let mut hdr = [0u8; 512];
    while off + 512 <= total {
        f.seek(std::io::SeekFrom::Start(off)).await?;
        f.read_exact(&mut hdr).await?;
        if hdr.iter().all(|&b| b == 0) {
            break;
        }
        let name = String::from_utf8_lossy(&hdr[..100]).trim_end_matches('\0').to_string();
        let size_str = String::from_utf8_lossy(&hdr[124..136]).trim_end_matches('\0').trim().to_string();
        let size = u64::from_str_radix(size_str.trim_start_matches('0'), 8).unwrap_or(0);
        let typeflag = hdr[156];
        let data = off + 512;
        if (typeflag == b'0' || typeflag == 0) && name.to_ascii_lowercase().ends_with(".vmdk") {
            return Ok(Some((data, size, name)));
        }
        off = data + size.div_ceil(512) * 512;
    }
    Ok(None)
}

/// A window over the OVA's VMDK member.
pub async fn vmdk_window(path: &Path) -> std::io::Result<Option<(Window, String)>> {
    let Some((off, size, name)) = find_vmdk(path).await? else { return Ok(None) };
    let file = tokio::fs::File::open(path).await?;
    Ok(Some((Window::inside(file, off, size), name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_entry(name: &str, data: &[u8]) -> Vec<u8> {
        let mut h = vec![0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..108].copy_from_slice(b"0000644\0");
        let size = format!("{:011o}\0", data.len());
        h[124..136].copy_from_slice(size.as_bytes());
        h[156] = b'0';
        h[257..263].copy_from_slice(b"ustar\0");
        let sum: u32 = h.iter().map(|&b| b as u32).sum::<u32>() + 8 * 32;
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        let mut out = h;
        out.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        out.extend(std::iter::repeat_n(0u8, pad));
        out
    }

    #[tokio::test]
    async fn finds_the_vmdk_past_the_ovf() {
        let mut tar = tar_entry("box.ovf", b"<Envelope/>");
        let disk = b"KDMV....not really a disk but the bytes we want";
        tar.extend(tar_entry("box-disk1.vmdk", disk));
        tar.extend(tar_entry("box.mf", b"SHA256(box.ovf)= 00"));
        tar.extend(std::iter::repeat_n(0u8, 1024));
        let p = std::env::temp_dir().join(format!("stormblock-ova-{}.ova", uuid::Uuid::new_v4().simple()));
        tokio::fs::write(&p, &tar).await.unwrap();
        let (mut w, name) = vmdk_window(&p).await.unwrap().expect("a vmdk member");
        assert_eq!(name, "box-disk1.vmdk");
        assert_eq!(w.len(), disk.len() as u64);
        let mut buf = vec![0u8; disk.len()];
        assert_eq!(w.read_at(0, &mut buf).await.unwrap(), disk.len());
        assert_eq!(&buf, disk);
        let _ = std::fs::remove_file(p);
    }
}
