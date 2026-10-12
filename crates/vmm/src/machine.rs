//! Build and drive a `VZVirtualMachine` from a [`VmConfig`].
//!
//! Everything here must run on the main thread: the VM is created with the main dispatch
//! queue, and completion handlers are delivered through the main run loop.
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_foundation::{NSArray, NSDate, NSError, NSRunLoop, NSString, NSURL};
use objc2_virtualization::*;

use crate::config::VmConfig;

pub struct Machine {
    vm: Retained<VZVirtualMachine>,
    balloon: Option<Retained<VZVirtioTraditionalMemoryBalloonDevice>>,
    socket: Option<Retained<VZVirtioSocketDevice>>,
    memory_bytes: u64,
    /// Why save/restore is unavailable for this configuration/host, if it is.
    save_restore_err: Option<String>,
}

/// A datagram socketpair: one end is the guest NIC, the other is served by `moniqi_netstack`.
fn user_net_attachment() -> Result<Retained<objc2_virtualization::VZFileHandleNetworkDeviceAttachment>> {
    use objc2_foundation::NSFileHandle;
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: fds has room for the two descriptors socketpair(2) writes.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) } != 0 {
        bail!("socketpair: {}", std::io::Error::last_os_error());
    }
    // Apple recommends a large send buffer and a ~4x larger receive buffer on a NIC socket.
    for fd in fds {
        for (opt, size) in [(libc::SO_SNDBUF, 1 << 20), (libc::SO_RCVBUF, 4 << 20)] {
            let v: libc::c_int = size;
            // SAFETY: valid fd, int option value.
            unsafe {
                libc::setsockopt(fd, libc::SOL_SOCKET, opt, (&v as *const libc::c_int).cast(), std::mem::size_of::<libc::c_int>() as u32)
            };
        }
    }
    // SAFETY: both descriptors are fresh and owned here.
    let host = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    moniqi_netstack::spawn(host).map_err(|e| anyhow!("start user-mode network: {e}"))?;
    unsafe {
        let fh = NSFileHandle::initWithFileDescriptor_closeOnDealloc(NSFileHandle::alloc(), fds[1].into_raw_fd(), true);
        let att = objc2_virtualization::VZFileHandleNetworkDeviceAttachment::initWithFileHandle(
            objc2_virtualization::VZFileHandleNetworkDeviceAttachment::alloc(),
            &fh,
        );
        att.setMaximumTransmissionUnit(1500);
        Ok(att)
    }
}

fn url(p: &std::path::Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy()))
}

fn ns_err(e: Retained<NSError>) -> anyhow::Error {
    anyhow!("{}", e.localizedDescription())
}

impl Machine {
    pub fn is_supported() -> bool {
        unsafe { VZVirtualMachine::isSupported() }
    }

    pub fn new(cfg: &VmConfig) -> Result<Self> {
        if !Self::is_supported() {
            bail!("Virtualization.framework is not supported on this machine");
        }
        unsafe {
            let c = VZVirtualMachineConfiguration::new();
            c.setCPUCount(cfg.cpus);
            let mem = cfg.memory_mib << 20;
            c.setMemorySize(mem);

            // Boot: direct kernel boot, no firmware / GRUB.
            let boot = VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &url(&cfg.kernel));
            boot.setCommandLine(&NSString::from_str(&cfg.cmdline));
            if let Some(rd) = &cfg.initrd {
                if rd.exists() {
                    boot.setInitialRamdiskURL(Some(&url(rd)));
                }
            }
            c.setBootLoader(Some(&boot));
            c.setPlatform(&VZGenericPlatformConfiguration::new());

            // Storage: virtio-blk. System disk read-only, data disk read-write.
            let mut disks: Vec<Retained<VZStorageDeviceConfiguration>> = Vec::new();
            let mut add_disk = |path: &std::path::Path, ro: bool| -> Result<()> {
                let att = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
                    VZDiskImageStorageDeviceAttachment::alloc(),
                    &url(path),
                    ro,
                )
                .map_err(ns_err)?;
                let dev = VZVirtioBlockDeviceConfiguration::initWithAttachment(
                    VZVirtioBlockDeviceConfiguration::alloc(),
                    &att,
                );
                disks.push(Retained::into_super(dev));
                Ok(())
            };
            if let Some(sys) = &cfg.system_disk {
                if sys.exists() {
                    add_disk(sys, true)?;
                }
            }
            add_disk(&cfg.data_disk, false)?;
            for d in &cfg.extra_disks {
                add_disk(&d.path, d.read_only)?;
            }
            c.setStorageDevices(&NSArray::from_retained_slice(&disks));

            // Network: NAT.
            let net = VZVirtioNetworkDeviceConfiguration::new();
            match cfg.net {
                crate::config::NetMode::Nat => net.setAttachment(Some(&VZNATNetworkDeviceAttachment::new())),
                crate::config::NetMode::User => {
                    let att = user_net_attachment()?;
                    net.setAttachment(Some(&att))
                }
            }
            c.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(net)]));

            // Serial console -> file.
            let att = VZFileSerialPortAttachment::initWithURL_append_error(
                VZFileSerialPortAttachment::alloc(),
                &url(&cfg.console_log),
                true,
            )
            .map_err(ns_err)?;
            let ser = VZVirtioConsoleDeviceSerialPortConfiguration::new();
            ser.setAttachment(Some(&att));
            c.setSerialPorts(&NSArray::from_retained_slice(&[Retained::into_super(ser)]));

            // vsock (adb / scrcpy forwarding, guest agent) and entropy.
            c.setSocketDevices(&NSArray::from_retained_slice(&[Retained::into_super(
                VZVirtioSocketDeviceConfiguration::new(),
            )]));
            c.setEntropyDevices(&NSArray::from_retained_slice(&[Retained::into_super(
                VZVirtioEntropyDeviceConfiguration::new(),
            )]));
            if cfg.balloon {
                c.setMemoryBalloonDevices(&NSArray::from_retained_slice(&[Retained::into_super(
                    VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new(),
                )]));
            }

            c.validateWithError().map_err(ns_err)?;
            let vm = VZVirtualMachine::initWithConfiguration(VZVirtualMachine::alloc(), &c);
            // Virtualization.framework only implements suspend-to-disk on Apple silicon; on Intel
            // even asking raises an Objective-C exception (aborts the process), so don't ask.
            let save_restore_err = if cfg!(target_arch = "aarch64") {
                c.validateSaveRestoreSupportWithError().err().map(|e| e.localizedDescription().to_string())
            } else {
                Some("Virtualization.framework supports saving VM state only on Apple silicon".to_string())
            };
            Ok(Self { vm, balloon: None, socket: None, memory_bytes: mem, save_restore_err })
        }
    }

    pub fn state(&self) -> VZVirtualMachineState {
        unsafe { self.vm.state() }
    }

    pub fn state_name(&self) -> &'static str {
        match self.state() {
            VZVirtualMachineState::Stopped => "stopped",
            VZVirtualMachineState::Running => "running",
            VZVirtualMachineState::Paused => "paused",
            VZVirtualMachineState::Error => "error",
            VZVirtualMachineState::Starting => "starting",
            VZVirtualMachineState::Pausing => "pausing",
            VZVirtualMachineState::Resuming => "resuming",
            VZVirtualMachineState::Stopping => "stopping",
            VZVirtualMachineState::Saving => "saving",
            VZVirtualMachineState::Restoring => "restoring",
            _ => "unknown",
        }
    }

    /// Run `f` with a completion block, spin the main run loop until it fires.
    fn wait(&self, what: &str, f: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<()> {
        let slot: Rc<RefCell<Option<Result<()>>>> = Rc::new(RefCell::new(None));
        let s2 = slot.clone();
        let block = RcBlock::new(move |err: *mut NSError| {
            let r = if err.is_null() {
                Ok(())
            } else {
                // SAFETY: non-null NSError delivered by the framework.
                Err(anyhow!("{}", unsafe { &*err }.localizedDescription()))
            };
            *s2.borrow_mut() = Some(r);
        });
        f(&block);
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(r) = slot.borrow_mut().take() {
                return r.map_err(|e| anyhow!("{what}: {e}"));
            }
            if Instant::now() > deadline {
                bail!("{what}: timed out");
            }
            let until = NSDate::dateWithTimeIntervalSinceNow(0.05);
            NSRunLoop::currentRunLoop().runUntilDate(&until);
        }
    }

    pub fn start(&mut self) -> Result<()> {
        let vm = self.vm.clone();
        self.wait("start", |b| unsafe { vm.startWithCompletionHandler(b) })?;
        unsafe {
            let devs = self.vm.memoryBalloonDevices();
            self.balloon = devs
                .iter()
                .next()
                .and_then(|d| d.downcast::<VZVirtioTraditionalMemoryBalloonDevice>().ok());
            self.socket = self
                .vm
                .socketDevices()
                .iter()
                .next()
                .and_then(|d| d.downcast::<VZVirtioSocketDevice>().ok());
        }
        Ok(())
    }

    pub fn pause(&self) -> Result<()> {
        let vm = self.vm.clone();
        self.wait("pause", |b| unsafe { vm.pauseWithCompletionHandler(b) })
    }

    pub fn resume(&self) -> Result<()> {
        let vm = self.vm.clone();
        self.wait("resume", |b| unsafe { vm.resumeWithCompletionHandler(b) })
    }

    pub fn stop(&self) -> Result<()> {
        let vm = self.vm.clone();
        self.wait("stop", |b| unsafe { vm.stopWithCompletionHandler(b) })
    }

    /// Ask the guest to shut down cleanly (ACPI-style power button).
    pub fn request_stop(&self) -> Result<()> {
        unsafe { self.vm.requestStopWithError().map_err(ns_err) }
    }

    /// `Ok` when suspend-to-disk works here; otherwise the framework's reason.
    pub fn save_restore_supported(&self) -> Result<()> {
        match &self.save_restore_err {
            None => Ok(()),
            Some(e) => bail!("{e}"),
        }
    }

    pub fn save_state(&self, path: &std::path::Path) -> Result<()> {
        let vm = self.vm.clone();
        let u = url(path);
        self.wait("save", |b| unsafe { vm.saveMachineStateToURL_completionHandler(&u, b) })
    }

    pub fn restore_state(&self, path: &std::path::Path) -> Result<()> {
        let vm = self.vm.clone();
        let u = url(path);
        self.wait("restore", |b| unsafe { vm.restoreMachineStateFromURL_completionHandler(&u, b) })
    }

    /// Shrink (or restore) the amount of guest RAM the host keeps resident.
    pub fn set_balloon_target(&self, bytes: u64) -> Result<()> {
        let b = self.balloon.as_ref().ok_or_else(|| anyhow!("no balloon device"))?;
        let bytes = bytes.clamp(512 << 20, self.memory_bytes);
        unsafe { b.setTargetVirtualMachineMemorySize(bytes) };
        Ok(())
    }

    /// Open a vsock connection to `port` in the guest. `done` runs on the main thread with a
    /// plain `File` owning a dup of the connection's descriptor.
    pub fn connect_vsock(&self, port: u32, done: impl FnOnce(Result<std::fs::File>) + 'static) {
        use std::os::fd::FromRawFd;
        let Some(dev) = self.socket.as_ref() else {
            done(Err(anyhow!("no vsock device (VM not started?)")));
            return;
        };
        let done = RefCell::new(Some(done));
        let block = RcBlock::new(move |conn: *mut VZVirtioSocketConnection, err: *mut NSError| {
            let Some(cb) = done.borrow_mut().take() else { return };
            if conn.is_null() {
                let msg = if err.is_null() {
                    "unknown error".to_string()
                } else {
                    // SAFETY: non-null NSError delivered by the framework.
                    unsafe { &*err }.localizedDescription().to_string()
                };
                cb(Err(anyhow!("vsock connect: {msg}")));
                return;
            }
            // SAFETY: conn is a live connection object for the duration of this handler.
            let fd = unsafe { (*conn).fileDescriptor() };
            // SAFETY: dup so the descriptor outlives the connection object.
            let dup = unsafe { libc::dup(fd) };
            if dup < 0 {
                cb(Err(anyhow!("dup vsock fd: {}", std::io::Error::last_os_error())));
            } else {
                // SAFETY: dup is a fresh descriptor we exclusively own.
                cb(Ok(unsafe { std::fs::File::from_raw_fd(dup) }));
            }
        });
        unsafe { dev.connectToPort_completionHandler(port, &block) };
    }

    /// Service pending host-side forward connections: each accepted TCP connection gets a
    /// vsock connection to its guest port and the two are relayed on a worker thread.
    pub fn pump_forwards(&self, forwards: &[crate::forward::Forward]) {
        for fw in forwards {
            while let Some(tcp) = fw.try_accept() {
                let guest_port = fw.guest_port;
                // The callback outlives this iteration, so it carries its own handle to the gate.
                let gate = fw.failure_gate();
                self.connect_vsock(guest_port, move |res| match res {
                    Ok(file) => {
                        gate.store(false, std::sync::atomic::Ordering::Relaxed);
                        std::thread::spawn(move || {
                            let _ = moniqi_proto::relay(tcp, file);
                        });
                    }
                    // Dropping `tcp` resets the client, which adb treats as "device offline".
                    Err(e) => {
                        if !gate.swap(true, std::sync::atomic::Ordering::Relaxed) {
                            eprintln!("forward -> guest port {guest_port}: {e} (repeats are silent until it works)");
                        }
                    }
                });
            }
        }
    }

    /// Pump the run loop for `d`; used by the CLI while the guest runs.
    pub fn run_for(&self, d: Duration) {
        let until = NSDate::dateWithTimeIntervalSinceNow(d.as_secs_f64());
        NSRunLoop::currentRunLoop().runUntilDate(&until);
    }
}
