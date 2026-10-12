//! The PID 1 boot sequence and supervision loop.
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::cmdline::{self, Options};
use crate::{modlist, net, sys};
use sys::log;

const MODULE_DIR: &str = "/usr/lib/moniqi/modules";
const MODULE_LOAD: &str = "/usr/lib/moniqi/modules.load";
const KEY_POWER: u16 = 116;

pub fn run() -> ! {
    sys::init_console();
    // SAFETY: getpid has no preconditions.
    if unsafe { libc::getpid() } != 1 {
        log("not PID 1; refusing to reconfigure this system");
        std::process::exit(1);
    }

    let mut cmdline_text = String::new();
    let _ = File::open("/proc/cmdline").and_then(|mut f| f.read_to_string(&mut cmdline_text));

    mount_basics();
    let opts = cmdline::parse(&cmdline_text);
    log(&format!("boot {}", fs::read_to_string("/proc/version").unwrap_or_default().trim()));

    load_modules();
    setup_binderfs();
    setup_zram(opts.zram_mib);
    let _ = sys::sethostname("moniqi");
    // DHCP can take many seconds (or fail); never hold up boot or the agent for it.
    let net_opts = opts.clone();
    std::thread::spawn(move || setup_net(&net_opts));

    std::thread::spawn(power_button_watcher);
    log("boot ok");
    supervise(&opts)
}

fn mount_basics() {
    let nsd = libc::MS_NOSUID | libc::MS_NODEV;
    sys::mount_lenient("proc", "/proc", "proc", nsd | libc::MS_NOEXEC);
    sys::mount_lenient("sysfs", "/sys", "sysfs", nsd | libc::MS_NOEXEC);
    sys::mount_lenient("devtmpfs", "/dev", "devtmpfs", libc::MS_NOSUID);
    sys::mount_lenient("devpts", "/dev/pts", "devpts", libc::MS_NOSUID | libc::MS_NOEXEC);
    sys::mount_lenient("tmpfs", "/dev/shm", "tmpfs", nsd);
    // The root filesystem is read-only: give services somewhere to write.
    for d in ["/run", "/tmp", "/var/tmp", "/var/log", "/var/lib"] {
        sys::mount_lenient("tmpfs", d, "tmpfs", nsd);
    }
}

fn load_modules() {
    let list = match fs::read_to_string(MODULE_LOAD) {
        Ok(t) => modlist::parse(&t),
        Err(e) => {
            log(&format!("{MODULE_LOAD}: {e}"));
            return;
        }
    };
    for m in list {
        let path = Path::new(MODULE_DIR).join(format!("{m}.ko"));
        match sys::finit_module(&path) {
            Ok(()) => log(&format!("module {m}")),
            Err(e) => log(&format!("module {m}: {e}")),
        }
    }
}

fn setup_binderfs() {
    const ROOT: &str = "/dev/binderfs";
    sys::mount_lenient("binder", ROOT, "binder", 0);
    let control = format!("{ROOT}/binder-control");
    for dev in ["binder", "hwbinder", "vndbinder"] {
        if let Err(e) = sys::binderfs_add(&control, dev) {
            // EEXIST: created on an earlier boot of a restored VM.
            if e.raw_os_error() != Some(libc::EEXIST) {
                log(&format!("binderfs {dev}: {e}"));
            }
        }
    }
}

fn setup_zram(mib: u64) {
    if mib == 0 || !Path::new("/sys/block/zram0").exists() {
        return;
    }
    let w = |f: &str, v: &str| fs::write(format!("/sys/block/zram0/{f}"), v);
    let _ = w("comp_algorithm", "zstd"); // optional; kernel keeps its default if unsupported
    if let Err(e) = w("disksize", &format!("{}M", mib)) {
        log(&format!("zram disksize: {e}"));
        return;
    }
    match Command::new("/usr/sbin/mkswap").arg("/dev/zram0").output() {
        Ok(o) if o.status.success() => {}
        other => {
            log(&format!("mkswap zram0 failed: {other:?}"));
            return;
        }
    }
    // SWAP_FLAG_PREFER | priority 100
    match sys::swapon("/dev/zram0", 0x8000 | 100) {
        Ok(()) => log(&format!("zram swap {mib} MiB")),
        Err(e) => log(&format!("swapon zram0: {e}")),
    }
}

fn setup_net(opts: &Options) {
    if let Err(e) = net::link_up("lo") {
        log(&format!("lo up: {e}"));
    }
    if !opts.net {
        return;
    }
    let Some(ifname) = net::find_ethernet() else {
        let all: Vec<String> = fs::read_dir("/sys/class/net")
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
            .unwrap_or_default();
        log(&format!("no ethernet interface found (have: {all:?})"));
        return;
    };
    // redroid's Ethernet service only manages `eth\d`; udev's predictable name (enp0s1) is
    // invisible to it. Rename before anything configures the link.
    let ifname = if ifname.starts_with("eth") {
        ifname
    } else {
        let _ = net::link_down(&ifname);
        match net::rename(&ifname, "eth0") {
            Ok(()) => "eth0".to_string(),
            Err(e) => {
                log(&format!("rename {ifname} -> eth0: {e}"));
                ifname
            }
        }
    };
    let ready = || {
        let _ = fs::create_dir_all("/run/moniqi");
        let _ = fs::write("/run/moniqi/net-ready", "1");
    };
    match net::configure_dhcp(&ifname) {
        Ok(l) => {
            log(&format!("{ifname} {} gw {:?} dns {:?}", l.yiaddr, l.router, l.dns));
            ready();
        }
        Err(e) => {
            log(&format!("{ifname} dhcp: {e} ({})", net::dhcp_debug()));
            // `moniqi.netdebug=1`: dump every frame seen for a few seconds after a DISCOVER.
            let dbg = fs::read_to_string("/proc/cmdline").unwrap_or_default().contains("moniqi.netdebug=1");
            if dbg {
                for line in net::sniff_after_discover(&ifname, 8) {
                    log(&format!("netdebug: {line}"));
                }
            }
            // The host's NAT answers on the wire even when its DHCP server stays silent.
            match net::mac(&ifname).and_then(|hw| net::static_fallback(&ifname, hw, Duration::from_secs(6))) {
                Ok((ip, _, gw, _)) => log(&format!("{ifname} static {ip} gw {gw}")),
                Err(e) => log(&format!("{ifname} static fallback: {e}")),
            }
            ready();
            let rd = |f: &str| {
                fs::read_to_string(format!("/sys/class/net/{ifname}/{f}"))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|e| format!("<{e}>"))
            };
            log(&format!(
                "{ifname}: operstate={} carrier={} flags={} rx_packets={} tx_packets={} rx_dropped={}",
                rd("operstate"), rd("carrier"), rd("flags"),
                rd("statistics/rx_packets"), rd("statistics/tx_packets"), rd("statistics/rx_dropped"),
            ));
        }
    }
}

/// VZ's `requestStop` presents as a KEY_POWER event on an input device; without a handler
/// (no systemd-logind/acpid here) nothing would ever shut the guest down.
fn power_button_watcher() {
    let mut open: Vec<(String, File)> = Vec::new();
    loop {
        if let Ok(rd) = fs::read_dir("/dev/input") {
            for e in rd.flatten() {
                let p = e.path().to_string_lossy().to_string();
                if p.contains("/event") && !open.iter().any(|(n, _)| *n == p) {
                    if let Ok(f) = File::open(&p) {
                        open.push((p, f));
                    }
                }
            }
        }
        let mut fds: Vec<libc::pollfd> = open
            .iter()
            .map(|(_, f)| libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 })
            .collect();
        if fds.is_empty() {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        // SAFETY: fds is a valid slice of pollfd for the given length.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 2000) };
        if n <= 0 {
            continue;
        }
        for (i, pfd) in fds.iter().enumerate() {
            if pfd.revents & libc::POLLIN == 0 {
                continue;
            }
            // struct input_event on 64-bit: timeval(16) + type(2) + code(2) + value(4)
            let mut ev = [0u8; 24];
            if (&open[i].1).read_exact(&mut ev).is_ok() {
                let ty = u16::from_ne_bytes([ev[16], ev[17]]);
                let code = u16::from_ne_bytes([ev[18], ev[19]]);
                let val = i32::from_ne_bytes([ev[20], ev[21], ev[22], ev[23]]);
                if ty == 1 && code == KEY_POWER && val == 1 {
                    log("power button: shutting down");
                    sys::sync_and_poweroff();
                }
            }
        }
    }
}

/// Reap every child (we are PID 1, so orphans reparent to us) and keep the agent alive.
fn supervise(opts: &Options) -> ! {
    let mut agent = spawn_agent(&opts.agent);
    loop {
        let mut status = 0;
        // SAFETY: blocking wait for any child; status is a valid out pointer.
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid < 0 {
            // No children at all (no agent installed): nothing to do but idle.
            std::thread::sleep(Duration::from_secs(3600));
            continue;
        }
        if Some(pid as u32) == agent {
            log(&format!("agent exited (status {status}); restarting"));
            std::thread::sleep(Duration::from_secs(1));
            agent = spawn_agent(&opts.agent);
        }
    }
}

fn spawn_agent(path: &str) -> Option<u32> {
    if !Path::new(path).exists() {
        log(&format!("no agent at {path}; idling"));
        return None;
    }
    match Command::new(path).spawn() {
        Ok(child) => {
            let pid = child.id();
            log(&format!("agent started (pid {pid})"));
            // Reaped by the waitpid(-1) loop; dropping Child does not kill it.
            Some(pid)
        }
        Err(e) => {
            log(&format!("agent {path}: {e}"));
            None
        }
    }
}
