//! Thin, checked wrappers over the raw syscalls PID 1 needs.
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Mutex;

static CONSOLE: Mutex<Option<File>> = Mutex::new(None);

/// Route stdio and our log to the virtio console (hvc0); falls back to whatever stderr is.
pub fn init_console() {
    if let Ok(f) = OpenOptions::new().read(true).write(true).open("/dev/hvc0") {
        let fd = f.as_raw_fd();
        // SAFETY: fd is a valid open descriptor; dup2 onto stdio.
        unsafe {
            libc::dup2(fd, 0);
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
        }
        *CONSOLE.lock().unwrap() = Some(f);
    }
}

pub fn log(msg: &str) {
    let line = format!("moniqi-init: {msg}\n");
    if let Some(f) = CONSOLE.lock().unwrap().as_mut() {
        let _ = f.write_all(line.as_bytes());
    } else {
        eprint!("{line}");
    }
}

fn c(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interior NUL"))
}

fn check(r: libc::c_int) -> io::Result<()> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn mount(source: &str, target: &str, fstype: &str, flags: libc::c_ulong) -> io::Result<()> {
    let (s, t, f) = (c(source)?, c(target)?, c(fstype)?);
    // SAFETY: all pointers are valid NUL-terminated strings for the call duration.
    check(unsafe { libc::mount(s.as_ptr(), t.as_ptr(), f.as_ptr(), flags, std::ptr::null()) })
}

/// Mount, treating "already mounted" (EBUSY) as success; log anything else.
pub fn mount_lenient(source: &str, target: &str, fstype: &str, flags: libc::c_ulong) {
    let _ = std::fs::create_dir_all(target);
    if let Err(e) = mount(source, target, fstype, flags) {
        if e.raw_os_error() != Some(libc::EBUSY) {
            log(&format!("mount {fstype} on {target}: {e}"));
        }
    }
}

/// Load a kernel module from an open file (`finit_module`). Already-loaded is not an error.
pub fn finit_module(path: &Path) -> io::Result<()> {
    let f = File::open(path)?;
    let params = c("")?;
    // SAFETY: raw syscall with a valid fd and NUL-terminated params string.
    let r = unsafe { libc::syscall(libc::SYS_finit_module, f.as_raw_fd(), params.as_ptr(), 0) };
    if r < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EEXIST) {
            return Ok(());
        }
        return Err(e);
    }
    Ok(())
}

pub fn sethostname(name: &str) -> io::Result<()> {
    // SAFETY: pointer/len describe the live `name` slice.
    check(unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) })
}

pub fn swapon(path: &str, flags: libc::c_int) -> io::Result<()> {
    let p = c(path)?;
    // SAFETY: valid NUL-terminated path.
    check(unsafe { libc::swapon(p.as_ptr(), flags) })
}

pub fn sync_and_poweroff() -> ! {
    // SAFETY: plain syscalls; reboot(2) with POWER_OFF does not return on success.
    unsafe {
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
    // If reboot failed there is nothing sensible left to do as PID 1.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// `_IOWR('b', 1, struct binderfs_device)`; the struct is 256 + 4 + 4 bytes.
const BINDER_CTL_ADD: libc::c_ulong = 0xC108_6201;

#[repr(C)]
struct BinderfsDevice {
    name: [u8; 256],
    major: u32,
    minor: u32,
}

/// Create a device node inside a mounted binderfs via its control file.
pub fn binderfs_add(control: &str, name: &str) -> io::Result<()> {
    let f = OpenOptions::new().read(true).write(true).open(control)?;
    let mut dev = BinderfsDevice { name: [0; 256], major: 0, minor: 0 };
    if name.len() >= dev.name.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "binder device name too long"));
    }
    dev.name[..name.len()].copy_from_slice(name.as_bytes());
    // SAFETY: `dev` is a properly sized, initialised repr(C) struct that outlives the call.
    check(unsafe { libc::ioctl(f.as_raw_fd(), BINDER_CTL_ADD as _, &mut dev) })
}
