//! moniqi-init: PID 1 of the moniqi guest.
//!
//! ubuntu-base ships neither kmod nor iproute2 nor a DHCP client, so everything the guest needs
//! at boot (module loading, binderfs, zram, link/IP configuration, power button) is done here
//! with raw syscalls. The pure parsing/encoding logic lives in modules that build and test on
//! any host; the syscall layer is Linux-only.
pub mod cmdline;
pub mod dhcp;
pub mod modlist;

#[cfg(target_os = "linux")]
pub mod boot;
#[cfg(target_os = "linux")]
pub mod net;
#[cfg(target_os = "linux")]
pub mod sys;
