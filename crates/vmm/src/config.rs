use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Persistent description of one moniqi VM. Stored as `vm.json` in the VM directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConfig {
    pub name: String,
    pub cpus: usize,
    pub memory_mib: u64,
    /// Uncompressed arm64 Linux `Image` (VZLinuxBootLoader needs a raw Image).
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
    pub cmdline: String,
    /// Read-only system image (Alpine rootfs + redroid), optional.
    pub system_disk: Option<PathBuf>,
    /// Writable sparse data disk.
    pub data_disk: PathBuf,
    pub data_disk_gib: u64,
    /// Guest serial output (hvc0) is appended here.
    pub console_log: PathBuf,
    pub balloon: bool,
    /// Additional virtio-blk disks, attached after system and data (so `/dev/vdc`, ...).
    #[serde(default)]
    pub extra_disks: Vec<DiskSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskSpec {
    pub path: PathBuf,
    pub read_only: bool,
}

impl VmConfig {
    pub fn new(name: &str, dir: &Path) -> Self {
        Self {
            name: name.to_string(),
            cpus: 2,
            memory_mib: 2048,
            kernel: dir.join("kernel/Image"),
            initrd: Some(dir.join("kernel/initrd")),
            cmdline: "console=hvc0 root=/dev/vda rootfstype=ext4 ro init=/sbin/moniqi-init init_on_alloc=0 init_on_free=0 quiet".to_string(),
            system_disk: Some(dir.join("system.img")),
            data_disk: dir.join("data.img"),
            data_disk_gib: 16,
            console_log: dir.join("console.log"),
            balloon: true,
            extra_disks: Vec::new(),
        }
    }

    /// Kernel command-line options moniqi depends on. Applied to configs written by older
    /// versions on `build-image`, so existing VMs pick them up.
    pub fn ensure_cmdline_options(&mut self) {
        for opt in ["init_on_alloc=0", "init_on_free=0"] {
            if !self.cmdline.split_whitespace().any(|t| t == opt) {
                self.cmdline.push(' ');
                self.cmdline.push_str(opt);
            }
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_str(&s).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Create the sparse data disk if missing (APFS keeps holes, so it costs ~0 until written).
    pub fn ensure_data_disk(&self) -> Result<()> {
        if self.data_disk.exists() {
            return Ok(());
        }
        let f = std::fs::File::create(&self.data_disk)?;
        f.set_len(self.data_disk_gib << 30)?;
        Ok(())
    }
}
