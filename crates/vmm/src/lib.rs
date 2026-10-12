//! moniqi-vmm: thin wrapper over Apple Virtualization.framework.
pub mod config;
#[cfg(target_os = "macos")]
pub mod forward;
#[cfg(target_os = "macos")]
pub mod machine;

pub use config::VmConfig;
