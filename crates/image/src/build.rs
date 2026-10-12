use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::arch::Arch;
use crate::fetch::Fetcher;
use crate::kernel::{self, KernelInfo};
use crate::{modules, rootfs};

const UBUNTU_UNPACKED: &str = "https://cloud-images.ubuntu.com/releases/noble/release/unpacked";

/// scrcpy server (matches `moniqi_scrcpy::SERVER_VERSION`); sha256 of the release asset.
const SCRCPY_SERVER_URL: &str =
    "https://github.com/Genymobile/scrcpy/releases/download/v5.0.1/scrcpy-server-v5.0.1";
const SCRCPY_SERVER_SHA256: &str = "764eb6f79811d5211fe9df341120882ba9994c7a61b897d7bf3fb662e53bc536";

pub struct BuildOptions {
    /// Shared download cache (e.g. `~/.moniqi/cache`).
    pub cache_dir: PathBuf,
    /// VM directory; receives `kernel/Image`, `kernel/initrd`, `system.img`.
    pub vm_dir: PathBuf,
    /// Guest architecture; must match the host CPU to boot under Virtualization.framework.
    pub arch: Arch,
    pub extra_modules: Vec<String>,
    /// Static aarch64 Linux binary to install as PID 1 (`/sbin/moniqi-init`). Without it a shell
    /// fallback is installed that cannot load modules (ubuntu-base has no kmod).
    pub guest_init: Option<PathBuf>,
    /// Static binary installed as `/usr/local/bin/moniqi-agent` (started by the guest init).
    pub guest_agent: Option<PathBuf>,
    /// Size of the (sparse) system image.
    pub system_size_mib: u64,
}

#[derive(Debug)]
pub struct BuiltImage {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub system_img: PathBuf,
    pub info: KernelInfo,
    pub modules: Vec<String>,
}

pub fn build(opts: &BuildOptions) -> Result<BuiltImage> {
    let arch = opts.arch;
    let deb = arch.deb();
    let mut f = Fetcher::new(&opts.cache_dir)?;
    let kdir = opts.vm_dir.join("kernel");
    std::fs::create_dir_all(&kdir)?;

    eprintln!("[1/4] kernel");
    let vmlinuz = f.fetch(
        &format!("{UBUNTU_UNPACKED}/ubuntu-24.04-server-cloudimg-{deb}-vmlinuz-generic"),
        &format!("ubuntu-noble-{deb}-vmlinuz-generic"),
        None,
    )?;
    let initrd_src = f.fetch(
        &format!("{UBUNTU_UNPACKED}/ubuntu-24.04-server-cloudimg-{deb}-initrd-generic"),
        &format!("ubuntu-noble-{deb}-initrd-generic"),
        None,
    )?;
    let kernel_path = kdir.join("Image");
    let info = kernel::extract_image(&vmlinuz, &kernel_path, arch)?;
    let initrd = kdir.join("initrd");
    std::fs::copy(&initrd_src, &initrd)?;
    eprintln!("      kernel {} (package {})", info.release, info.pkg_version);

    eprintln!("[2/4] kernel modules");
    let mut debs = Vec::new();
    for pkg in ["linux-modules", "linux-modules-extra"] {
        let file = format!("{pkg}-{}_{}_{deb}.deb", info.release, info.pkg_version);
        let p = f
            .fetch(&format!("{}/{file}", arch.pool_url()), &file, None)
            .with_context(|| format!("fetch {file} (kernel may have been superseded in the pool)"))?;
        debs.push(p);
    }
    let deb_refs: Vec<&Path> = debs.iter().map(|p| p.as_path()).collect();
    let mut wanted: Vec<&str> = modules::DEFAULT_MODULES.to_vec();
    wanted.extend(opts.extra_modules.iter().map(String::as_str));
    let mods = modules::collect(&deb_refs, &wanted, modules::DEFAULT_MODULE_DIRS)?;
    let mod_names: Vec<String> = mods.iter().map(|m| m.name.clone()).collect();
    eprintln!("      {} modules (first: {})", mods.len(), mod_names.iter().take(8).cloned().collect::<Vec<_>>().join(" "));

    eprintln!("[3/4] rootfs");
    let (base_url, base_sha) = arch.base_tarball();
    let base = f.fetch(&base_url, &format!("ubuntu-base-24.04.5-base-{deb}.tar.gz"), Some(base_sha))?;
    let tar_path = opts.vm_dir.join("system.tar");
    let guest_init = match &opts.guest_init {
        Some(p) => Some(std::fs::read(p).with_context(|| format!("read {}", p.display()))?),
        None => {
            eprintln!("      warning: no guest init binary; using shell fallback (modules won't load)");
            None
        }
    };
    let guest_agent = match &opts.guest_agent {
        Some(p) => Some(std::fs::read(p).with_context(|| format!("read {}", p.display()))?),
        None => None,
    };
    let scrcpy = f.fetch(SCRCPY_SERVER_URL, "scrcpy-server-v5.0.1.jar", Some(SCRCPY_SERVER_SHA256))?;
    let scrcpy_jar = std::fs::read(&scrcpy)?;
    rootfs::overlay_tar(
        &base,
        &mods,
        guest_init.as_deref(),
        guest_agent.as_deref(),
        Some(&scrcpy_jar),
        &tar_path,
    )?;

    eprintln!("[4/4] ext4 image");
    let img = opts.vm_dir.join("system.img");
    // Keep the tar if mke2fs fails so the failure can be reproduced by hand.
    rootfs::make_ext4(&tar_path, &img, opts.system_size_mib)?;
    std::fs::remove_file(&tar_path).ok();

    Ok(BuiltImage { kernel: kernel_path, initrd, system_img: img, info, modules: mod_names })
}
