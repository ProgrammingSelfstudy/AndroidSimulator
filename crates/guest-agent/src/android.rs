//! Start the Android container (redroid) without a container runtime.
//!
//! Layout inside the guest:
//!   /dev/vdb  writable ext4 (formatted on first boot)  -> /run/moniqi/data
//!   /dev/vdc  read-only Android rootfs (from OCI image) -> /run/moniqi/ro
//!   overlayfs(lower=ro, upper=data/upper)               -> /run/moniqi/root   (pivot_root target)
//!   data/userdata                                       -> bind-mounted at /data in the container
//!
//! The container shares the guest's network namespace, so adbd's :5555 is directly reachable by
//! the vsock forwarder on the guest loopback.
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use moniqi_proto::{RunConfig, RUN_CONFIG_PATH};

const DATA_DEV: &str = "/dev/vdb";
const ANDROID_DEV: &str = "/dev/vdc";
const BASE: &str = "/run/moniqi";

fn log(msg: &str) {
    println!("moniqi-agent: android: {msg}");
}

fn cstr(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interior NUL"))
}

fn check(r: libc::c_int, what: &str) -> io::Result<()> {
    if r < 0 {
        let e = io::Error::last_os_error();
        Err(io::Error::new(e.kind(), format!("{what}: {e}")))
    } else {
        Ok(())
    }
}

fn mount(src: &str, dst: &str, fstype: &str, flags: libc::c_ulong, data: &str) -> io::Result<()> {
    let (s, d, f, o) = (cstr(src)?, cstr(dst)?, cstr(fstype)?, cstr(data)?);
    let data_ptr = if data.is_empty() { std::ptr::null() } else { o.as_ptr().cast() };
    // SAFETY: all pointers are valid NUL-terminated strings for the duration of the call.
    check(
        unsafe { libc::mount(s.as_ptr(), d.as_ptr(), f.as_ptr(), flags, data_ptr) },
        &format!("mount {fstype} {src} -> {dst}"),
    )
}

fn has_ext4_magic(dev: &str) -> io::Result<bool> {
    let mut f = File::open(dev)?;
    f.seek(SeekFrom::Start(1024 + 56))?; // superblock s_magic
    let mut m = [0u8; 2];
    f.read_exact(&mut m)?;
    Ok(u16::from_le_bytes(m) == 0xEF53)
}

fn wait_for(path: &str, secs: u64) -> bool {
    for _ in 0..secs * 10 {
        if Path::new(path).exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// System apps that are useless in a headless-ish emulator and cost resident memory. Disabled
/// per-user (persisted by PackageManager; idempotent), never uninstalled. Deliberately
/// excludes anything the framework or app-install flow depends on.
const TRIM_PACKAGES: &[&str] = &[
    "com.android.traceur",
    "com.android.printspooler",
    "com.android.bips",
    "com.android.dynsystem",
    "com.android.managedprovisioning",
    "com.android.onetimeinitializer",
    "com.android.localtransport",
    "com.android.deskclock",
    "com.android.calendar",
    "com.android.providers.calendar",
    "com.android.camera2",
    "com.android.quicksearchbox",
    "com.android.smspush",
    "com.android.se",
    "com.android.statementservice",
    "com.android.cellbroadcastreceiver",
    "com.android.bluetoothmidiservice",
    // No Bluetooth HAL in the guest: the stack aborts ("start the HAL before starting
    // Bluetooth") and is restarted in a crash loop on every boot, each leaving a tombstone.
    "com.android.bluetooth",
    "com.android.musicfx",
    "com.android.email",
    "com.android.soundrecorder",
    "com.android.wallpaper.livepicker",
    "com.android.stk",
    "com.android.emergency",
    "com.android.simappdialog",
];

pub fn cmdline_flag(key: &str) -> Option<String> {
    let text = fs::read_to_string("/proc/cmdline").ok()?;
    text.split_whitespace().find_map(|t| t.strip_prefix(&format!("{key}=")).map(str::to_string))
}

/// After Android reports boot completed (and the BOOT_COMPLETED broadcast storm has passed),
/// disable the trim list. Enabled with `moniqi.trim=1` on the kernel command line.
fn trim_when_booted() {
    if cmdline_flag("moniqi.trim").as_deref() != Some("1") {
        return;
    }
    for _ in 0..240 {
        thread::sleep(Duration::from_secs(1));
        let done = crate::vsock::run_in_container("getprop sys.boot_completed").unwrap_or_default();
        if done.trim() == "1" {
            break;
        }
    }
    thread::sleep(Duration::from_secs(20));
    let mut script = String::new();
    for p in TRIM_PACKAGES {
        script.push_str(&format!("pm disable-user --user 0 {p} >/dev/null 2>&1; "));
    }
    match crate::vsock::run_in_container(&script) {
        Ok(_) => log(&format!("trimmed {} packages", TRIM_PACKAGES.len())),
        Err(e) => log(&format!("trim failed: {e}")),
    }
}

/// Flutter apps that opt out of Impeller render through Skia's GL backend, which cannot start
/// under the ARM translator here (ANGLE extension entry points are missing: "Failed to set up
/// Skia Gr context", then a blank window). Impeller (Vulkan) works, and Flutter honours
/// `--ez enable-impeller true` on the launch intent, so when that error shows up relaunch the
/// app once with it. A launcher tap carries no such extra, hence the watcher.
const FLUTTER_FALLBACK: &str = r#"
logcat -v brief -T 1 'flutter:E' '*:S' | while read -r line; do
  case "$line" in
    *"Failed to set up Skia Gr context"*)
      pid=$(echo "$line" | sed -n 's/^[A-Z]\/[^(]*( *\([0-9]*\)).*/\1/p')
      pkg=$(tr '\0' ' ' < /proc/$pid/cmdline 2>/dev/null | cut -d' ' -f1)
      [ -n "$pkg" ] || continue
      stamp=/data/local/tmp/.moniqi-impeller-$pkg
      now=$(date +%s)
      last=$(cat $stamp 2>/dev/null || echo 0)
      [ $((now - last)) -ge 20 ] || continue
      echo $now > $stamp
      comp=$(cmd package resolve-activity --brief -c android.intent.category.LAUNCHER $pkg | tail -1)
      am force-stop $pkg
      am start -n "$comp" --ez enable-impeller true >/dev/null 2>&1
      ;;
  esac
done
"#;

fn flutter_gl_fallback() {
    for _ in 0..240 {
        thread::sleep(Duration::from_secs(1));
        let done = crate::vsock::run_in_container("getprop sys.boot_completed").unwrap_or_default();
        if done.trim() == "1" {
            break;
        }
    }
    log("flutter GL fallback watcher started");
    loop {
        match crate::vsock::spawn_in_container(FLUTTER_FALLBACK) {
            Ok(mut child) => {
                let _ = child.wait();
            }
            Err(e) => log(&format!("flutter fallback: {e}")),
        }
        thread::sleep(Duration::from_secs(5));
    }
}

/// Agent side: prepare storage, then keep the container alive (no restart loop yet: a crashing
/// Android should be visible, not hidden behind retries).
pub fn start() {
    thread::spawn(|| {
        if let Err(e) = run() {
            log(&format!("failed: {e}"));
        }
    });
    thread::spawn(trim_when_booted);
    thread::spawn(flutter_gl_fallback);
}

fn run() -> io::Result<()> {
    if !wait_for(ANDROID_DEV, 5) {
        log(&format!("no {ANDROID_DEV}; not starting Android (attach one with `moniqi pull-android`)"));
        return Ok(());
    }
    if !wait_for(DATA_DEV, 5) {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("{DATA_DEV} missing")));
    }

    if !has_ext4_magic(DATA_DEV)? {
        log(&format!("formatting {DATA_DEV} (first boot)"));
        let st = Command::new("/usr/sbin/mke2fs")
            .args(["-q", "-t", "ext4", "-m", "0", "-L", "moniqi-data", DATA_DEV])
            .status()?;
        if !st.success() {
            return Err(io::Error::other(format!("mke2fs {DATA_DEV}: {st}")));
        }
    }

    let (data, ro, root) = (format!("{BASE}/data"), format!("{BASE}/ro"), format!("{BASE}/root"));
    for d in [&data, &ro, &root] {
        fs::create_dir_all(d)?;
    }
    mount(DATA_DEV, &data, "ext4", libc::MS_NOATIME, "")?;
    for d in ["upper", "work", "userdata"] {
        fs::create_dir_all(format!("{data}/{d}"))?;
    }
    mount(ANDROID_DEV, &ro, "ext4", libc::MS_RDONLY, "")?;
    mount(
        "overlay",
        &root,
        "overlay",
        0,
        &format!("lowerdir={ro},upperdir={data}/upper,workdir={data}/work"),
    )?;

    install_overrides(&root)?;

    // Android's Ethernet service reads the interface's address once at start-up, so the guest
    // must have it configured first (init writes this flag; bounded so a missing NIC can't hang).
    let net_wait = Instant::now();
    while !Path::new("/run/moniqi/net-ready").exists() && net_wait.elapsed() < Duration::from_secs(25) {
        thread::sleep(Duration::from_millis(100));
    }
    log(&format!("network ready={} after {} ms", Path::new("/run/moniqi/net-ready").exists(), net_wait.elapsed().as_millis()));

    log("starting container");
    let me = std::env::current_exe()?;
    let mut child = Command::new(me).arg("container").arg(&root).arg(format!("{data}/userdata")).spawn()?;
    let status = child.wait()?;
    log(&format!("container exited: {status}"));
    Ok(())
}

/// Init scripts dropped into the (overlay) vendor partition. Written on every start, so they
/// follow the agent version rather than lingering in the persistent upper layer.
const OVERRIDES_RC: &str = "\
# generated by moniqi-agent
on init
    # No Bluetooth hardware: skip the framework service and the HAL. Left alone, the Bluetooth
    # app crash-loops for the first minute of every boot, burning CPU and memory.
    setprop config.disable_bluetooth true
    stop vendor.bluetooth-1-1
";

fn install_overrides(root: &str) -> io::Result<()> {
    let dir = format!("{root}/vendor/etc/init");
    fs::create_dir_all(&dir)?;
    fs::write(format!("{dir}/moniqi.rc"), OVERRIDES_RC)
}

// ---------------------------------------------------------------------------------------------
// Container side: runs as a fresh single-threaded process (`moniqi-agent container ROOT DATA`).
// ---------------------------------------------------------------------------------------------

/// Block devices Android expects in /dev: (name, major, minor).
const DEV_NODES: &[(&str, u32, u32)] = &[
    ("null", 1, 3),
    ("zero", 1, 5),
    ("full", 1, 7),
    ("random", 1, 8),
    ("urandom", 1, 9),
    ("kmsg", 1, 11),
    ("tty", 5, 0),
    ("console", 5, 1),
    ("fuse", 10, 229),
    ("uinput", 10, 223),
];

fn mknod_char(path: &str, major: u32, minor: u32) -> io::Result<()> {
    let p = cstr(path)?;
    // SAFETY: valid path; makedev builds the dev_t.
    check(
        unsafe { libc::mknod(p.as_ptr(), libc::S_IFCHR | 0o666, libc::makedev(major, minor)) },
        &format!("mknod {path}"),
    )
}

fn setup_dev(root: &str) -> io::Result<()> {
    let dev = format!("{root}/dev");
    fs::create_dir_all(&dev)?;
    mount("tmpfs", &dev, "tmpfs", libc::MS_NOSUID | libc::MS_STRICTATIME, "mode=755,size=64m")?;
    for (name, maj, min) in DEV_NODES {
        if let Err(e) = mknod_char(&format!("{dev}/{name}"), *maj, *min) {
            log(&format!("{e}"));
        }
    }
    // DMA-BUF heaps (Codec2 and gralloc allocate from them). The kernel exposes them in sysfs;
    // Android's ueventd would normally create the nodes, but not before the first consumer asks.
    if let Ok(rd) = fs::read_dir("/sys/class/dma_heap") {
        let _ = fs::create_dir_all(format!("{dev}/dma_heap"));
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(devnum) = fs::read_to_string(e.path().join("dev")) else { continue };
            if let Some((maj, min)) = devnum.trim().split_once(':') {
                if let (Ok(maj), Ok(min)) = (maj.parse(), min.parse()) {
                    if let Err(e) = mknod_char(&format!("{dev}/dma_heap/{name}"), maj, min) {
                        log(&format!("{e}"));
                    }
                }
            }
        }
    }
    fs::create_dir_all(format!("{dev}/pts"))?;
    mount("devpts", &format!("{dev}/pts"), "devpts", libc::MS_NOSUID | libc::MS_NOEXEC, "newinstance,ptmxmode=0666,mode=0620")?;
    let _ = symlink("pts/ptmx", format!("{dev}/ptmx"));
    fs::create_dir_all(format!("{dev}/shm"))?;
    mount("tmpfs", &format!("{dev}/shm"), "tmpfs", libc::MS_NOSUID | libc::MS_NODEV, "mode=1777")?;
    // Android's init mounts its own binderfs here.
    fs::create_dir_all(format!("{dev}/binderfs"))?;
    Ok(())
}

fn container_exec(root: &str, userdata: &str) -> io::Result<()> {
    let cfg = fs::read_to_string(format!("{root}/{RUN_CONFIG_PATH}"))
        .map(|t| RunConfig::parse(&t))
        .unwrap_or_default();
    let mut argv = cfg.argv();
    if argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("{RUN_CONFIG_PATH} has no entrypoint/cmd")));
    }
    // Display geometry comes from the kernel command line (`moniqi.width=` etc., set by
    // `moniqi config --resolution`); software rendering cost scales with pixel count.
    let w = cmdline_flag("moniqi.width").unwrap_or_else(|| "720".into());
    let h = cmdline_flag("moniqi.height").unwrap_or_else(|| "1280".into());
    let dpi = cmdline_flag("moniqi.dpi").unwrap_or_else(|| "320".into());
    // redroid's software-rendering default is a 15 Hz display, which is what makes the UI feel
    // laggy (every frame waits ~67 ms for the next vsync).
    let fps = cmdline_flag("moniqi.fps").unwrap_or_else(|| "30".into());
    // Resolve through the address init configured (the host NAT's DNS proxy, or the DHCP
    // lease's servers): redroid otherwise falls back to a hard-coded 8.8.8.8, which is
    // unreachable behind some host VPN/TUN setups.
    if let Some(dns) = fs::read_to_string("/run/moniqi/resolv.conf")
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("nameserver ").map(|v| v.trim().to_string())))
    {
        if !argv.iter().any(|a| a.starts_with("androidboot.redroid_net_dns1=")) {
            argv.push("androidboot.redroid_net_ndns=1".to_string());
            argv.push(format!("androidboot.redroid_net_dns1={dns}"));
        }
    }
    // Defaults only where the image doesn't already set them.
    for (k, v) in [
        ("androidboot.redroid_width", w.as_str()),
        ("androidboot.redroid_height", h.as_str()),
        ("androidboot.redroid_dpi", dpi.as_str()),
        ("androidboot.redroid_fps", fps.as_str()),
        // Software Codec2 encoders (what scrcpy's H.264 stream uses). redroid defaults to the
        // legacy OMX path, whose software encoder fails here.
        ("androidboot.use_redroid_c2", "1"),
        ("androidboot.use_dmabufheaps", "1"),
    ] {
        if !argv.iter().any(|a| a.starts_with(&format!("{k}="))) {
            argv.push(format!("{k}={v}"));
        }
    }
    log(&format!("exec {argv:?}"));

    check(unsafe { libc::mount(std::ptr::null(), c"/".as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) }, "make / private")?;

    fs::create_dir_all(format!("{root}/data"))?;
    mount(userdata, &format!("{root}/data"), "", libc::MS_BIND, "")?;
    fs::create_dir_all(format!("{root}/proc"))?;
    mount("proc", &format!("{root}/proc"), "proc", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, "")?;
    fs::create_dir_all(format!("{root}/sys"))?;
    mount("sysfs", &format!("{root}/sys"), "sysfs", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, "")?;
    setup_dev(root)?;

    // pivot_root(".", ".") then detach the old root, so nothing of the guest is visible.
    let r = cstr(root)?;
    check(unsafe { libc::chdir(r.as_ptr()) }, "chdir root")?;
    // SAFETY: raw syscall with valid NUL-terminated paths.
    check(unsafe { libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) as libc::c_int }, "pivot_root")?;
    check(unsafe { libc::umount2(c".".as_ptr(), libc::MNT_DETACH) }, "detach old root")?;
    check(unsafe { libc::chdir(c"/".as_ptr()) }, "chdir /")?;

    let c_args: Vec<CString> = argv.iter().map(|a| cstr(a)).collect::<io::Result<_>>()?;
    let mut c_env: Vec<CString> = cfg.env.iter().map(|e| cstr(e)).collect::<io::Result<_>>()?;
    if !cfg.env.iter().any(|e| e.starts_with("PATH=")) {
        c_env.push(cstr("PATH=/sbin:/system/sbin:/system/bin:/system/xbin:/vendor/bin:/vendor/xbin")?);
    }
    let mut a: Vec<*const libc::c_char> = c_args.iter().map(|s| s.as_ptr()).collect();
    a.push(std::ptr::null());
    let mut e: Vec<*const libc::c_char> = c_env.iter().map(|s| s.as_ptr()).collect();
    e.push(std::ptr::null());
    // SAFETY: argv/envp are NUL-terminated arrays of valid C strings that outlive the call.
    unsafe { libc::execve(a[0], a.as_ptr(), e.as_ptr()) };
    Err(io::Error::new(io::Error::last_os_error().kind(), format!("execve {}: {}", argv[0], io::Error::last_os_error())))
}

/// Entry for the `container` subcommand. Never returns.
pub fn container_main(root: &str, userdata: &str) -> ! {
    // New mount/pid/uts/ipc/cgroup namespaces. The network namespace is deliberately shared.
    let flags = libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC | libc::CLONE_NEWCGROUP;
    // SAFETY: plain syscall.
    if unsafe { libc::unshare(flags) } < 0 {
        log(&format!("unshare: {}", io::Error::last_os_error()));
        std::process::exit(1);
    }
    // SAFETY: we are single-threaded here, so fork is safe.
    match unsafe { libc::fork() } {
        -1 => {
            log(&format!("fork: {}", io::Error::last_os_error()));
            std::process::exit(1);
        }
        0 => {
            // Child is PID 1 of the new PID namespace.
            if let Err(e) = container_exec(root, userdata) {
                log(&format!("{e}"));
            }
            std::process::exit(127);
        }
        pid => {
            let mut st = 0;
            // SAFETY: waiting on our own child.
            unsafe { libc::waitpid(pid, &mut st, 0) };
            let code = if libc::WIFEXITED(st) { libc::WEXITSTATUS(st) } else { 128 + libc::WTERMSIG(st) };
            std::process::exit(code);
        }
    }
}
