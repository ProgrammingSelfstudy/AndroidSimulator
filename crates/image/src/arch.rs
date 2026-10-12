use anyhow::{bail, Result};

/// Guest architecture. Virtualization.framework only runs guests matching the host CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Arm64,
    X86_64,
}

impl Arch {
    pub fn host() -> Self {
        if cfg!(target_arch = "aarch64") {
            Arch::Arm64
        } else {
            Arch::X86_64
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "arm64" | "aarch64" => Ok(Arch::Arm64),
            "x86_64" | "amd64" => Ok(Arch::X86_64),
            other => bail!("unknown arch `{other}` (expected arm64 or x86_64)"),
        }
    }

    /// Debian/Ubuntu architecture name.
    pub fn deb(self) -> &'static str {
        match self {
            Arch::Arm64 => "arm64",
            Arch::X86_64 => "amd64",
        }
    }

    pub fn rust_target(self) -> &'static str {
        match self {
            Arch::Arm64 => "aarch64-unknown-linux-musl",
            Arch::X86_64 => "x86_64-unknown-linux-musl",
        }
    }

    /// Pool directory holding kernel packages for this architecture.
    pub fn pool_url(self) -> &'static str {
        match self {
            Arch::Arm64 => "https://ports.ubuntu.com/ubuntu-ports/pool/main/l/linux",
            Arch::X86_64 => "https://archive.ubuntu.com/ubuntu/pool/main/l/linux",
        }
    }

    /// ubuntu-base 24.04.5 tarball URL and its published SHA256.
    pub fn base_tarball(self) -> (String, &'static str) {
        let url = format!(
            "https://cdimage.ubuntu.com/ubuntu-base/releases/24.04/release/ubuntu-base-24.04.5-base-{}.tar.gz",
            self.deb()
        );
        let sha = match self {
            Arch::Arm64 => "a91d5a93010193712d346d761372b7c9db6dfcf093893161c64ca107f05914f2",
            Arch::X86_64 => "e77b6f10c2590cef872b33ee9f635a0e3fd1f57fb074c0e52b5c7f56147a0c86",
        };
        (url, sha)
    }
}
