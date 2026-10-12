use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;

use crate::arch::Arch;

#[derive(Debug, Clone, PartialEq)]
pub struct KernelInfo {
    /// e.g. `6.8.0-142-generic` (matches `/lib/modules/<release>`).
    pub release: String,
    /// Ubuntu package version, e.g. `6.8.0-142.142` (names the module debs).
    pub pkg_version: String,
}

const ARM64_MAGIC: &[u8; 4] = b"ARMd"; // at offset 56 of an arm64 Image

/// Turn Ubuntu's `vmlinuz` into what VZ's direct boot accepts for `arch`:
/// arm64 wants the raw `Image` (so gunzip it); x86_64 takes the bzImage as shipped.
pub fn extract_image(vmlinuz: &Path, out: &Path, arch: Arch) -> Result<KernelInfo> {
    let raw = std::fs::read(vmlinuz).with_context(|| format!("read {}", vmlinuz.display()))?;
    match arch {
        Arch::Arm64 => {
            let image = if raw.starts_with(&[0x1f, 0x8b]) {
                let mut v = Vec::with_capacity(raw.len() * 3);
                GzDecoder::new(&raw[..]).read_to_end(&mut v).context("gunzip kernel")?;
                v
            } else if raw.starts_with(b"MZ") {
                bail!("{} is an EFI/zboot PE image; VZLinuxBootLoader needs a raw arm64 Image", vmlinuz.display());
            } else {
                raw
            };
            if image.len() < 64 || &image[56..60] != ARM64_MAGIC {
                bail!("not an arm64 Linux Image (missing ARM\\x64 magic)");
            }
            let info = parse_version(&image).context("could not find `Linux version` banner in kernel")?;
            std::fs::write(out, &image)?;
            Ok(info)
        }
        Arch::X86_64 => {
            let info = parse_bzimage_version(&raw).context("not an x86 bzImage (missing HdrS header)")?;
            std::fs::write(out, &raw)?;
            Ok(info)
        }
    }
}

/// An x86 bzImage's banner is compressed, but the boot protocol header points at a plain
/// version string: `kernel_version` at 0x20e, relative to 0x200.
/// Ubuntu's package version is `<abi>.<build>` where build comes from `#<build>-Ubuntu`.
pub fn parse_bzimage_version(img: &[u8]) -> Option<KernelInfo> {
    if img.get(0x202..0x206)? != b"HdrS" {
        return None;
    }
    let ptr = u16::from_le_bytes([img[0x20e], img[0x20f]]) as usize + 0x200;
    let tail = img.get(ptr..)?;
    let end = tail.iter().take(256).position(|&b| b == 0)?;
    let s = std::str::from_utf8(&tail[..end]).ok()?;
    let release = s.split_whitespace().next()?.to_string();
    let flavour = release.rsplit('-').next()?;
    let abi = release.strip_suffix(&format!("-{flavour}"))?.to_string();
    let build = s.split('#').nth(1)?.split('-').next()?;
    if build.is_empty() || !build.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(KernelInfo { release, pkg_version: format!("{abi}.{build}") })
}

/// Parse the banner: `Linux version 6.8.0-142-generic (...) (Ubuntu 6.8.0-142.142-generic 6.8.12)`.
pub fn parse_version(image: &[u8]) -> Option<KernelInfo> {
    let needle = b"Linux version ";
    let at = image.windows(needle.len()).position(|w| w == needle)?;
    let end = image[at..].iter().position(|&b| b == b'\n' || b == 0).map(|e| at + e)?;
    let banner = std::str::from_utf8(&image[at..end]).ok()?;
    let release = banner[needle.len()..].split_whitespace().next()?.to_string();
    let flavour = release.rsplit('-').next()?; // "generic"
    let pkg_version = banner
        .rsplit("(Ubuntu ") // the gcc version also has "(Ubuntu ...)": take the last one
        .next()?
        .split_whitespace()
        .next()?
        .trim_end_matches(&format!("-{flavour}"))
        .to_string();
    Some(KernelInfo { release, pkg_version })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ubuntu_banner() {
        // Real banner from the noble arm64 kernel (gcc's own "(Ubuntu ...)" comes first).
        let b = b"xx\0Linux version 6.8.0-142-generic (buildd@bos03-arm64-111) (aarch64-linux-gnu-gcc-13 (Ubuntu 13.3.0-6ubuntu2~24.04.1) 13.3.0, GNU ld (GNU Binutils for Ubuntu) 2.42) #142-Ubuntu SMP PREEMPT_DYNAMIC  (Ubuntu 6.8.0-142.142-generic 6.8.12)\n";
        let k = parse_version(b).unwrap();
        assert_eq!(k.release, "6.8.0-142-generic");
        assert_eq!(k.pkg_version, "6.8.0-142.142");
    }

    #[test]
    fn parses_bzimage_header() {
        let mut img = vec![0u8; 0x400];
        img[0x202..0x206].copy_from_slice(b"HdrS");
        let off: u16 = 0x1a0; // string lives at 0x200 + off
        img[0x20e..0x210].copy_from_slice(&off.to_le_bytes());
        let s = b"6.8.0-53-generic (buildd@x) #55-Ubuntu SMP PREEMPT_DYNAMIC Wed";
        img[0x200 + off as usize..0x200 + off as usize + s.len()].copy_from_slice(s);
        let k = parse_bzimage_version(&img).unwrap();
        assert_eq!(k.release, "6.8.0-53-generic");
        assert_eq!(k.pkg_version, "6.8.0-53.55");
        assert!(parse_bzimage_version(&vec![0u8; 0x400]).is_none());
    }

    #[test]
    fn rejects_non_arm64() {
        let d = std::env::temp_dir().join("moniqi-kernel-test");
        std::fs::write(&d, vec![0u8; 128]).unwrap();
        assert!(extract_image(&d, &d.with_extension("out"), Arch::Arm64).is_err());
    }
}
