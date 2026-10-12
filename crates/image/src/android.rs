//! Build the read-only Android root filesystem image from an OCI image (redroid).
use std::path::{Path, PathBuf};

use anyhow::Result;
use moniqi_proto::RUN_CONFIG_PATH;

use crate::arch::Arch;
use crate::fetch::Fetcher;
use crate::{oci, rootfs};

pub struct AndroidOptions {
    pub cache_dir: PathBuf,
    pub out_img: PathBuf,
    pub repo: String,
    pub tag: String,
    pub arch: Arch,
}

pub fn build_android(opts: &AndroidOptions) -> Result<oci::RunConfig> {
    let mut f = Fetcher::new(&opts.cache_dir)?;
    eprintln!("[1/3] pull {}:{} ({})", opts.repo, opts.tag, opts.arch.deb());
    let img = oci::pull(&mut f, &opts.repo, &opts.tag, opts.arch)?;

    eprintln!("[2/3] flatten {} layer(s)", img.layers.len());
    let tar = opts.out_img.with_extension("tar");
    let extra = vec![(RUN_CONFIG_PATH.to_string(), img.config.to_text().into_bytes(), 0o644)];
    oci::flatten(&img.layers, &extra, &tar)?;

    let size_mib = (std::fs::metadata(&tar)?.len() >> 20) * 115 / 100 + 256;
    eprintln!("[3/3] ext4 image ({size_mib} MiB)");
    let r = rootfs::make_ext4(&tar, &opts.out_img, size_mib);
    std::fs::remove_file(&tar).ok();
    r?;
    Ok(img.config)
}

pub fn default_image_path(vm_dir: &Path) -> PathBuf {
    vm_dir.join("android.img")
}
