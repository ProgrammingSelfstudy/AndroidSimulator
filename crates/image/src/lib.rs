//! moniqi-image: fetch upstream artifacts and assemble a bootable guest
//! (arm64 `Image` + modules + ext4 rootfs) for `VZLinuxBootLoader`.
//!
//! Why not Alpine's kernel: it ships with `CONFIG_ANDROID_BINDER_IPC` disabled and as an EFI
//! zboot image, so neither redroid nor VZ's direct boot can use it. Ubuntu's arm64 kernel
//! decompresses to a raw `Image`, and `binder_linux` is available as a module package.
pub mod android;
pub mod arch;
pub mod build;
pub mod fetch;
pub mod kernel;
pub mod modules;
pub mod oci;
pub mod rootfs;

pub use arch::Arch;
pub use build::{build, BuildOptions, BuiltImage};
