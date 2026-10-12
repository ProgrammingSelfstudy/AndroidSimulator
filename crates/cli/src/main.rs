use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use moniqi_vmm::{machine::Machine, VmConfig};

#[derive(Parser)]
#[command(name = "moniqi", about = "Lightweight Android emulator for macOS")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a VM directory with a default vm.json
    Create {
        name: String,
        #[arg(long, default_value_t = 2)]
        cpus: usize,
        #[arg(long, default_value_t = 2048)]
        memory_mib: u64,
        #[arg(long, default_value_t = 16)]
        disk_gib: u64,
    },
    /// Boot a VM headless and stream until it stops (Ctrl-C = clean shutdown)
    Start {
        name: String,
        /// Restore from saved state if present, and save on exit
        #[arg(long)]
        persist: bool,
        /// Extra host->guest vsock forwards as HOST:GUEST (adb 5555:5555 and control 15000:5000
        /// are always on; use --no-default-forwards to disable them)
        #[arg(long = "forward")]
        forwards: Vec<String>,
        #[arg(long)]
        no_default_forwards: bool,
        /// Adaptive balloon: shrink the guest's RAM allowance to what it uses. Off by default:
        /// on Intel/macOS 15 the host footprint did not follow the balloon (measured), so it
        /// only adds guest work. Worth re-testing on Apple silicon.
        #[arg(long)]
        governor: bool,
        /// Run headless: no window (adb and the forwards still work)
        #[arg(long)]
        no_window: bool,
        /// Also forward 127.0.0.1:15002 to the guest's root exec service (debugging only: any
        /// local process can run commands in the guest and container)
        #[arg(long)]
        debug_exec: bool,
    },
    /// Print the VM config and paths
    Info { name: String },
    /// Install APK(s) into the running VM (same as dropping them on the window)
    Install { apks: Vec<PathBuf> },
    /// Change a VM's resources (takes effect on the next start)
    Config {
        name: String,
        #[arg(long)]
        memory_mib: Option<u64>,
        #[arg(long)]
        cpus: Option<usize>,
        /// Disable ~24 unused system apps after boot (on|off)
        #[arg(long)]
        trim: Option<String>,
        /// Screen size as WxH, e.g. 540x960. Rendering is software, so fewer pixels = smoother.
        #[arg(long)]
        resolution: Option<String>,
        /// Screen density, e.g. 240
        #[arg(long)]
        dpi: Option<u32>,
        /// Display refresh rate in Hz (5-60). Software rendering costs CPU per frame.
        #[arg(long)]
        fps: Option<u32>,
    },
    /// Download the Android (redroid) image and attach it to the VM as a read-only disk
    PullAndroid {
        name: String,
        #[arg(long, default_value = "redroid/redroid")]
        repo: String,
        #[arg(long, default_value = "13.0.0-latest")]
        tag: String,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        #[arg(long)]
        arch: Option<String>,
    },
    /// Download the kernel/modules/rootfs and assemble the VM's boot images
    BuildImage {
        name: String,
        /// Download cache (default: ~/.moniqi/cache)
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        /// Extra kernel modules to include (dependencies are resolved)
        #[arg(long = "module")]
        modules: Vec<String>,
        /// Size of the sparse system image
        #[arg(long, default_value_t = 3072)]
        size_mib: u64,
        /// Static Linux binary (matching --arch) to install as /sbin/moniqi-init
        #[arg(long)]
        guest_init: Option<PathBuf>,
        /// Static Linux binary to install as /usr/local/bin/moniqi-agent
        #[arg(long)]
        guest_agent: Option<PathBuf>,
        /// Guest architecture: arm64 or x86_64 (default: this machine's, the only one VZ can run)
        #[arg(long)]
        arch: Option<String>,
    },
}

const ADB_SERIAL: &str = "127.0.0.1:5555";

/// Locate the adb client: PATH first, then the usual install locations.
fn find_adb() -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        if let Some(p) = std::env::split_paths(&paths).map(|d| d.join("adb")).find(|p| p.is_file()) {
            return Some(p);
        }
    }
    let home = dirs::home_dir()?;
    [
        PathBuf::from("/usr/local/bin/adb"),
        PathBuf::from("/opt/homebrew/bin/adb"),
        home.join("Library/Android/sdk/platform-tools/adb"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

/// Keep trying `adb connect` until the guest's adbd answers, so `adb devices` just works.
/// adb only auto-discovers emulators that expose a console port; ours is a plain TCP forward.
fn spawn_adb_autoconnect(stop: Arc<AtomicBool>) {
    let Some(adb) = find_adb() else { return };
    std::thread::spawn(move || {
        use std::process::Command;
        while !stop.load(Ordering::SeqCst) {
            let out = Command::new(&adb).args(["connect", ADB_SERIAL]).output();
            let connected = out
                .as_ref()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains("connected to"))
                .unwrap_or(false);
            if connected {
                println!("adb: connected to {ADB_SERIAL}");
                return;
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    });
}

/// Install one APK through adb over the forwarded port. Returns the adb summary line.
fn adb_install(apk: &Path) -> Result<String> {
    use std::process::Command;
    let adb = find_adb().context("adb not found (install it with `brew install --cask android-platform-tools`)")?;
    if !apk.is_file() {
        bail!("{} is not a file", apk.display());
    }
    // adb wants an `.apk` name; a renamed download (`app.apk.1`) is staged under a proper one.
    let staged;
    let apk = if apk.extension().is_some_and(|e| e.eq_ignore_ascii_case("apk")) {
        apk
    } else {
        let dir = std::env::temp_dir().join(format!("moniqi-install-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        staged = dir.join("app.apk");
        let _ = std::fs::remove_file(&staged);
        std::os::unix::fs::symlink(std::fs::canonicalize(apk)?, &staged)?;
        &staged
    };
    // Idempotent: a no-op when already connected.
    let _ = Command::new(&adb).args(["connect", ADB_SERIAL]).output();
    let out = Command::new(&adb)
        .args(["-s", ADB_SERIAL, "install", "-r", "-t"])
        .arg(apk)
        .output()
        .context("run adb")?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let summary = text
        .lines()
        .rev()
        .find(|l| l.starts_with("Success") || l.starts_with("Failure") || l.starts_with("adb:") || l.contains("error"))
        .unwrap_or_else(|| text.lines().last().unwrap_or("no output"))
        .trim()
        .to_string();
    if out.status.success() && summary.starts_with("Success") {
        Ok(summary)
    } else {
        bail!("{summary}")
    }
}

/// `kill -USR1 <pid>` toggles "minimised" without a window: the same pause/shrink path a
/// minimised window triggers, usable headless and for testing.
static FORCE_MINIMIZE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_usr1(_: libc::c_int) {
    FORCE_MINIMIZE.fetch_xor(true, Ordering::SeqCst);
}

/// One request/response on the guest control channel (host forward 15000 -> vsock 5000).
///
/// Must not run on the VM thread: the forward that carries this very connection is serviced by
/// that thread's loop, so blocking there waits on ourselves until the timeout.
fn guest_query_blocking(verb: &str) -> Option<String> {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = "127.0.0.1:15000".parse().ok()?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    s.set_write_timeout(Some(Duration::from_millis(500))).ok()?;
    writeln!(s, "{verb}").ok()?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).ok()?;
    Some(line.trim().to_string())
}

/// Background worker that performs control-channel requests and hands back the replies.
struct GuestCtl {
    tx: std::sync::mpsc::Sender<(String, std::sync::mpsc::Sender<Option<String>>)>,
}

impl GuestCtl {
    fn spawn() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<(String, std::sync::mpsc::Sender<Option<String>>)>();
        std::thread::spawn(move || {
            for (verb, reply) in rx {
                let _ = reply.send(guest_query_blocking(&verb));
            }
        });
        GuestCtl { tx }
    }

    /// Queue a request; poll the returned receiver with `try_recv` from the main loop.
    fn ask(&self, verb: &str) -> std::sync::mpsc::Receiver<Option<String>> {
        let (rtx, rrx) = std::sync::mpsc::channel();
        let _ = self.tx.send((verb.to_string(), rtx));
        rrx
    }
}

/// Minimise sequence: drop caches -> read memory -> shrink balloon -> let the guest inflate
/// it -> pause. (The balloon only moves while the guest runs.)
enum Shrink {
    DropCaches(std::sync::mpsc::Receiver<Option<String>>),
    Meminfo(std::sync::mpsc::Receiver<Option<String>>),
    Settle(std::time::Instant),
}

fn vm_dir(name: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(home.join(".moniqi").join(name))
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Create { name, cpus, memory_mib, disk_gib } => {
            let dir = vm_dir(&name)?;
            if dir.join("vm.json").exists() {
                bail!("{} already exists", dir.display());
            }
            std::fs::create_dir_all(dir.join("kernel"))?;
            let mut cfg = VmConfig::new(&name, &dir);
            cfg.cpus = cpus;
            cfg.memory_mib = memory_mib;
            cfg.data_disk_gib = disk_gib;
            cfg.save(&dir.join("vm.json"))?;
            cfg.ensure_data_disk()?;
            println!("created {}\nnext: moniqi build-image {name} --guest-init <moniqi-init>", dir.display());
        }
        Cmd::Install { apks } => {
            if apks.is_empty() {
                bail!("give at least one .apk");
            }
            let mut failed = 0;
            for apk in &apks {
                match adb_install(apk) {
                    Ok(s) => println!("{}: {s}", apk.display()),
                    Err(e) => {
                        failed += 1;
                        eprintln!("{}: {e}", apk.display());
                    }
                }
            }
            if failed > 0 {
                bail!("{failed} of {} installs failed", apks.len());
            }
        }
        Cmd::Config { name, memory_mib, cpus, trim, resolution, dpi, fps } => {
            let path = vm_dir(&name)?.join("vm.json");
            let mut cfg = VmConfig::load(&path)?;
            if let Some(m) = memory_mib {
                if m < 512 {
                    bail!("memory_mib {m} is below 512; Android does not run usefully there");
                }
                cfg.memory_mib = m;
            }
            if let Some(c) = cpus {
                cfg.cpus = c.max(1);
            }
            let set_flag = |cmdline: &mut String, key: &str, val: String| {
                let mut toks: Vec<String> = cmdline
                    .split_whitespace()
                    .filter(|t| !t.starts_with(&format!("{key}=")))
                    .map(String::from)
                    .collect();
                toks.push(format!("{key}={val}"));
                *cmdline = toks.join(" ");
            };
            if let Some(r) = resolution {
                let (w, h) = r.split_once('x').context("--resolution expects WxH, e.g. 540x960")?;
                let (w, h): (u32, u32) = (w.parse().context("width")?, h.parse().context("height")?);
                if !(240..=2160).contains(&w) || !(320..=3840).contains(&h) {
                    bail!("resolution {w}x{h} is outside 240x320..2160x3840");
                }
                set_flag(&mut cfg.cmdline, "moniqi.width", w.to_string());
                set_flag(&mut cfg.cmdline, "moniqi.height", h.to_string());
            }
            if let Some(d) = dpi {
                set_flag(&mut cfg.cmdline, "moniqi.dpi", d.to_string());
            }
            if let Some(f) = fps {
                if !(5..=60).contains(&f) {
                    bail!("--fps must be between 5 and 60");
                }
                set_flag(&mut cfg.cmdline, "moniqi.fps", f.to_string());
            }
            if let Some(t) = trim {
                let v = match t.as_str() {
                    "on" | "1" | "true" => "1",
                    "off" | "0" | "false" => "0",
                    other => bail!("--trim expects on|off, got `{other}`"),
                };
                let mut toks: Vec<String> =
                    cfg.cmdline.split_whitespace().filter(|t| !t.starts_with("moniqi.trim=")).map(String::from).collect();
                toks.push(format!("moniqi.trim={v}"));
                cfg.cmdline = toks.join(" ");
            }
            cfg.save(&path)?;
            println!("{}: {} MiB, {} vCPU\n  cmdline: {}", name, cfg.memory_mib, cfg.cpus, cfg.cmdline);
        }
        Cmd::Info { name } => {
            let dir = vm_dir(&name)?;
            let cfg = VmConfig::load(&dir.join("vm.json"))?;
            println!("{}", serde_json::to_string_pretty(&cfg)?);
        }
        Cmd::Start { name, persist, forwards, no_default_forwards, governor, no_window, debug_exec } => {
            let mut forwards = forwards;
            if debug_exec {
                forwards.push("15002:5002".into());
            }
            start(&name, persist, forwards, no_default_forwards, !no_window, governor)?
        }
        Cmd::PullAndroid { name, repo, tag, cache_dir, arch } => {
            let dir = vm_dir(&name)?;
            let cfg_path = dir.join("vm.json");
            let mut cfg = VmConfig::load(&cfg_path)?;
            let cache_dir = match cache_dir {
                Some(d) => d,
                None => dirs::home_dir().context("no home dir")?.join(".moniqi").join("cache"),
            };
            let arch = match arch {
                Some(a) => moniqi_image::Arch::parse(&a)?,
                None => moniqi_image::Arch::host(),
            };
            let out_img = moniqi_image::android::default_image_path(&dir);
            let run = moniqi_image::android::build_android(&moniqi_image::android::AndroidOptions {
                cache_dir,
                out_img: out_img.clone(),
                repo,
                tag,
                arch,
            })?;
            cfg.extra_disks.retain(|d| d.path != out_img);
            cfg.extra_disks.push(moniqi_vmm::config::DiskSpec { path: out_img.clone(), read_only: true });
            cfg.save(&cfg_path)?;
            println!("android image: {}\nentrypoint {:?} cmd {:?}", out_img.display(), run.entrypoint, run.cmd);
        }
        Cmd::BuildImage { name, cache_dir, modules, size_mib, guest_init, guest_agent, arch } => {
            let dir = vm_dir(&name)?;
            let cfg_path = dir.join("vm.json");
            let mut cfg = VmConfig::load(&cfg_path)?;
            let cache_dir = match cache_dir {
                Some(d) => d,
                None => dirs::home_dir().context("no home dir")?.join(".moniqi").join("cache"),
            };
            let built = moniqi_image::build(&moniqi_image::BuildOptions {
                cache_dir,
                vm_dir: dir.clone(),
                arch: match arch {
                    Some(a) => moniqi_image::Arch::parse(&a)?,
                    None => moniqi_image::Arch::host(),
                },
                extra_modules: modules,
                guest_init,
                guest_agent,
                system_size_mib: size_mib,
            })?;
            cfg.kernel = built.kernel;
            cfg.initrd = Some(built.initrd);
            cfg.system_disk = Some(built.system_img);
            cfg.ensure_cmdline_options();
            cfg.save(&cfg_path)?;
            println!("image ready for {} (kernel {})", name, built.info.release);
        }
    }
    Ok(())
}

fn start(name: &str, persist: bool, extra: Vec<String>, no_defaults: bool, window: bool, governor: bool) -> Result<()> {
    let dir = vm_dir(name)?;
    let cfg = VmConfig::load(&dir.join("vm.json"))?;
    if !cfg.kernel.exists() {
        bail!("kernel not found: {}", cfg.kernel.display());
    }
    cfg.ensure_data_disk()?;
    let state_file = dir.join("state.vzvmsave");

    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    ctrlc::set_handler(move || s2.store(true, Ordering::SeqCst))?;
    // SAFETY: the handler only flips an atomic, which is async-signal-safe.
    unsafe { libc::signal(libc::SIGUSR1, on_usr1 as extern "C" fn(libc::c_int) as libc::sighandler_t) };

    let mut specs: Vec<String> = Vec::new();
    if !no_defaults {
        // 5000 is taken by macOS AirPlay Receiver, so control uses 15000 on the host.
        specs.push("5555:5555".into());
        specs.push("15000:5000".into());
        specs.push("15003:5003".into()); // scrcpy video
        specs.push("15004:5004".into()); // scrcpy control
    }
    specs.extend(extra);
    let mut forwards = Vec::new();
    for spec in &specs {
        let (host, guest) = moniqi_vmm::forward::parse_spec(spec)?;
        forwards.push(moniqi_vmm::forward::Forward::listen(host, guest)?);
        println!("forward 127.0.0.1:{host} -> guest vsock {guest}");
    }

    if forwards.iter().any(|f| f.host_port == 5555) {
        spawn_adb_autoconnect(stop.clone());
    }

    let mut m = Machine::new(&cfg)?;
    // Suspend-to-disk isn't available on every host/config; saving anyway aborts the process.
    let persist = persist
        && match m.save_restore_supported() {
            Ok(()) => true,
            Err(e) => {
                eprintln!("--persist ignored: this VM can't be saved on this Mac ({e}); it will shut down normally");
                false
            }
        };
    if persist && state_file.exists() {
        println!("restoring {}", state_file.display());
        m.restore_state(&state_file)?;
        m.resume()?;
        std::fs::remove_file(&state_file).ok();
    } else {
        m.start()?;
    }
    println!("running; console -> {}", cfg.console_log.display());

    // The window needs the video forwards, which exist unless the user turned defaults off.
    let mut display = if window && !no_defaults {
        Some(moniqi_display::Display::new(moniqi_display::DisplayConfig {
            video_addr: "127.0.0.1:15003".into(),
            control_addr: "127.0.0.1:15004".into(),
            title: format!("moniqi — {name}"),
        })?)
    } else {
        None
    };
    let debug = std::env::var_os("MONIQI_DEBUG").is_some();
    // Memory governor: hand free guest pages back to the host through the balloon.
    let configured_bytes = cfg.memory_mib << 20;
    let policy = moniqi_governor::Policy::default();
    let mut balloon_now = configured_bytes;
    let vm_started = std::time::Instant::now();
    let mut last_gov = std::time::Instant::now();
    let govern = governor && cfg.balloon && !no_defaults;
    let ctl = GuestCtl::spawn();
    let mut pending_meminfo: Option<std::sync::mpsc::Receiver<Option<String>>> = None;
    let mut shrink: Option<Shrink> = None;
    let mut last_stats = std::time::Instant::now();
    let (install_tx, install_rx) = std::sync::mpsc::channel::<(String, Result<String, String>)>();
    let mut title_reset_at: Option<std::time::Instant> = None;
    // Installs in flight: label -> start time. A repeated drop of the same file is ignored, and
    // the title shows elapsed seconds so a long install doesn't look hung.
    let mut installing: std::collections::HashMap<String, std::time::Instant> = Default::default();
    let mut install_tick: u64 = u64::MAX;
    let base_title = format!("moniqi — {name}");
    // The window stays black until Android has booted and the first frame arrives (~15 s):
    // say so in the title bar, with a running count, instead of looking hung.
    let mut booting_secs: Option<u64> = Some(u64::MAX);
    let mut minimized_since: Option<std::time::Instant> = None;
    let mut paused_for_minimize = false;

    let mut asked_at: Option<std::time::Instant> = None;
    while m.state_name() == "running" || m.state_name() == "paused" {
        match display.as_mut() {
            Some(d) => d.pump(Duration::from_millis(20)),
            None => m.run_for(Duration::from_millis(20)),
        }
        m.pump_forwards(&forwards);
        if m.state_name() != "running" && m.state_name() != "paused" {
            break; // guest finished powering off while we were waiting
        }
        // Android's first minute is allocation-heavy; shrinking then just triggers reclaim.
        let settled = vm_started.elapsed() > Duration::from_secs(60);
        if govern && settled && shrink.is_none() && !paused_for_minimize && m.state_name() == "running" {
            if pending_meminfo.is_none() && last_gov.elapsed() > Duration::from_secs(5) {
                last_gov = std::time::Instant::now();
                pending_meminfo = Some(ctl.ask("meminfo"));
            }
            if let Some(rx) = &pending_meminfo {
                if let Ok(reply) = rx.try_recv() {
                    pending_meminfo = None;
                    if let Some(info) = reply.and_then(|r| moniqi_governor::parse_meminfo(&r)) {
                        let held = configured_bytes.saturating_sub(balloon_now);
                        let want = moniqi_governor::target(&info, held, configured_bytes, &policy);
                        if let Some(t) = moniqi_governor::next_target(balloon_now, want, &policy) {
                            if m.set_balloon_target(t).is_ok() {
                                if debug {
                                    println!("balloon: guest keeps {} MiB (was {} MiB)", t >> 20, balloon_now >> 20);
                                }
                                balloon_now = t;
                            }
                        }
                    }
                }
            }
        }
        if let Some(d) = display.as_ref() {
            if debug && last_stats.elapsed() > Duration::from_secs(5) {
                last_stats = std::time::Instant::now();
                println!("display: {}", d.stats());
            }
            if d.is_closed() {
                stop.store(true, Ordering::SeqCst); // closing the window shuts the VM down
            }
            if booting_secs.is_some() {
                if d.is_connected() {
                    booting_secs = None;
                    d.set_title(&base_title);
                } else {
                    let secs = vm_started.elapsed().as_secs();
                    if booting_secs != Some(secs) {
                        booting_secs = Some(secs);
                        d.set_title(&format!("{base_title} — starting Android… {secs}s"));
                    }
                }
            }
        }
        // APKs dropped on the window: install on a worker thread, report in the title bar.
        if let Some(d) = display.as_ref() {
            for apk in d.take_dropped() {
                let label = apk.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if installing.contains_key(&label) {
                    d.set_title(&format!("{base_title} — already installing {label}"));
                    title_reset_at = Some(std::time::Instant::now() + Duration::from_secs(3));
                    continue;
                }
                installing.insert(label.clone(), std::time::Instant::now());
                d.set_title(&format!("{base_title} — installing {label}…"));
                let tx = install_tx.clone();
                std::thread::spawn(move || {
                    let r = adb_install(&apk).map_err(|e| e.to_string());
                    let _ = tx.send((label, r));
                });
            }
            if let Some(note) = d.take_notice() {
                eprintln!("window: {note}");
                d.set_title(&format!("{base_title} — {note}"));
                title_reset_at = Some(std::time::Instant::now() + Duration::from_secs(6));
            }
            while let Ok((label, r)) = install_rx.try_recv() {
                installing.remove(&label);
                match r {
                    Ok(_) => {
                        println!("installed {label}");
                        d.set_title(&format!("{base_title} — installed {label} ✓"));
                    }
                    Err(e) => {
                        eprintln!("install {label} failed: {e}");
                        d.set_title(&format!("{base_title} — install failed: {e}"));
                    }
                }
                title_reset_at = Some(std::time::Instant::now() + Duration::from_secs(6));
            }
            if let Some((label, since)) = installing.iter().next() {
                let secs = since.elapsed().as_secs();
                if secs != install_tick {
                    install_tick = secs;
                    d.set_title(&format!("{base_title} — installing {label}… {secs}s"));
                    title_reset_at = None;
                }
            } else if title_reset_at.is_some_and(|t| std::time::Instant::now() >= t) {
                d.set_title(&base_title);
                title_reset_at = None;
            }
        }
        // A minimised window shows nothing, so stop spending CPU on the guest.
        let minimized = display.as_ref().is_some_and(|d| d.is_minimized()) || FORCE_MINIMIZE.load(Ordering::SeqCst);
        if minimized {
            let since = *minimized_since.get_or_insert_with(std::time::Instant::now);
            if !paused_for_minimize && shrink.is_none() && since.elapsed() > Duration::from_secs(2) && m.state_name() == "running" {
                if govern {
                    shrink = Some(Shrink::DropCaches(ctl.ask("drop_caches")));
                } else {
                    m.pause()?;
                    paused_for_minimize = true;
                    println!("minimised: VM paused");
                }
            }
        } else {
            minimized_since = None;
            shrink = None; // restored mid-sequence: abandon it
            if paused_for_minimize && m.state_name() == "paused" {
                m.resume()?;
                paused_for_minimize = false;
                println!("restored: VM resumed");
            }
        }
        // Advance the minimise sequence without ever blocking the loop.
        shrink = match shrink.take() {
            Some(Shrink::DropCaches(rx)) => match rx.try_recv() {
                Ok(_) => Some(Shrink::Meminfo(ctl.ask("meminfo"))),
                Err(std::sync::mpsc::TryRecvError::Empty) => Some(Shrink::DropCaches(rx)),
                Err(_) => Some(Shrink::Settle(std::time::Instant::now())),
            },
            Some(Shrink::Meminfo(rx)) => match rx.try_recv() {
                Ok(reply) => {
                    let tight = moniqi_governor::Policy { headroom: 256 << 20, ..policy };
                    if let Some(info) = reply.and_then(|r| moniqi_governor::parse_meminfo(&r)) {
                        let held = configured_bytes.saturating_sub(balloon_now);
                        let t = moniqi_governor::target(&info, held, configured_bytes, &tight);
                        if m.set_balloon_target(t).is_ok() {
                            balloon_now = t;
                        }
                    }
                    Some(Shrink::Settle(std::time::Instant::now()))
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => Some(Shrink::Meminfo(rx)),
                Err(_) => Some(Shrink::Settle(std::time::Instant::now())),
            },
            Some(Shrink::Settle(t0)) => {
                if t0.elapsed() > Duration::from_secs(3) {
                    m.pause()?;
                    paused_for_minimize = true;
                    println!("minimised: VM paused (guest keeps {} MiB)", balloon_now >> 20);
                    None
                } else {
                    Some(Shrink::Settle(t0))
                }
            }
            None => None,
        };
        if !stop.load(Ordering::SeqCst) {
            continue;
        }
        if persist {
            m.pause()?;
            m.save_state(&state_file)?;
            println!("state saved");
            return Ok(());
        }
        match asked_at {
            None => {
                asked_at = Some(std::time::Instant::now());
                println!("requesting guest shutdown...");
                m.request_stop()?;
            }
            // Guest ignored the power button (or isn't ours): pull the plug.
            Some(t) if t.elapsed() > Duration::from_secs(30) => {
                println!("guest did not shut down in 30s; forcing stop");
                m.stop()?;
            }
            Some(_) => {}
        }
    }
    if let Some(adb) = find_adb() {
        let _ = std::process::Command::new(adb).args(["disconnect", ADB_SERIAL]).output();
    }
    println!("vm {}", m.state_name());
    Ok(())
}
