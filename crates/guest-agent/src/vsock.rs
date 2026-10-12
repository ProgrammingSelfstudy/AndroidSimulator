//! AF_VSOCK listeners and the services behind them.
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::os::fd::FromRawFd;
use std::thread;
use std::time::Duration;

use std::process::{Command, Stdio};

use moniqi_proto::{
    parse_command, relay, ADB_PORT, CONTROL_PORT, ECHO_PORT, EXEC_PORT, SCRCPY_CONTROL_PORT, SCRCPY_VIDEO_PORT,
};

fn log(msg: &str) {
    println!("moniqi-agent: {msg}");
}

/// Listen on `port` for connections from the host (any CID) and hand each to `handler`.
fn listen(port: u32, handler: fn(File)) -> io::Result<()> {
    // SAFETY: plain socket(2).
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: sockaddr_vm is POD; zeroed then filled.
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_cid = u32::MAX; // VMADDR_CID_ANY
    addr.svm_port = port;
    // SAFETY: addr is a valid sockaddr_vm of the stated size.
    let r = unsafe {
        libc::bind(fd, &addr as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_vm>() as u32)
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: valid bound socket.
    if unsafe { libc::listen(fd, 16) } < 0 {
        return Err(io::Error::last_os_error());
    }
    thread::spawn(move || loop {
        // SAFETY: accept on a listening socket; null addr is allowed.
        let c = unsafe { libc::accept4(fd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
        if c < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                log(&format!("accept on port {port}: {e}"));
                thread::sleep(Duration::from_millis(200));
            }
            continue;
        }
        // SAFETY: c is a fresh fd we exclusively own.
        let f = unsafe { File::from_raw_fd(c) };
        thread::spawn(move || handler(f));
    });
    Ok(())
}

/// vsock <-> adbd on the guest's loopback.
fn adb(conn: File) {
    match TcpStream::connect(("127.0.0.1", ADB_PORT as u16)) {
        Ok(tcp) => {
            let _ = relay(conn, tcp);
        }
        Err(_) => {
            // adbd not up yet: closing makes the host's adb client retry, which is what it expects.
        }
    }
}

fn echo(mut conn: File) {
    let mut r = match conn.try_clone() {
        Ok(r) => r,
        Err(_) => return,
    };
    let _ = io::copy(&mut r, &mut conn);
}

/// PID (in the guest's namespace) of the Android container's init: the process that is PID 1 in
/// its own PID namespace, i.e. whose `NSpid:` line ends with `1`.
pub fn container_init_pid() -> Option<u32> {
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        if pid == 1 {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else { continue };
        let nspid = status.lines().find_map(|l| l.strip_prefix("NSpid:"));
        if let Some(ns) = nspid {
            let parts: Vec<&str> = ns.split_whitespace().collect();
            if parts.len() >= 2 && parts.last() == Some(&"1") {
                return Some(pid);
            }
        }
    }
    None
}

/// Run `cmd` with the container's shell (in its namespaces, with Android's environment) and
/// return stdout. For the agent's own housekeeping; the exec service is the debug surface.
pub fn run_in_container(cmd: &str) -> io::Result<String> {
    let pid = container_init_pid().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no running container"))?;
    let mut c = Command::new("/usr/bin/nsenter");
    c.args(["-t", &pid.to_string(), "-m", "-p", "-u", "-i", "--", "/system/bin/sh", "-c", cmd]);
    if let Some(env) = android_env() {
        c.env_clear().envs(env);
    }
    let out = c.stdin(Stdio::null()).output()?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The environment Android services run with (`BOOTCLASSPATH`, `ANDROID_ROOT`, ...). Android's
/// init only injects these into services it starts, so a process entered via `nsenter` has to
/// borrow them from a running one (zygote is the first that has `BOOTCLASSPATH`).
pub fn android_env() -> Option<Vec<(String, String)>> {
    let mut pids: Vec<u32> = std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse().ok())
        .collect();
    pids.sort_unstable();
    for pid in pids {
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/environ")) else { continue };
        let vars: Vec<(String, String)> = raw
            .split(|&b| b == 0)
            .filter_map(|kv| {
                let s = String::from_utf8_lossy(kv);
                let (k, v) = s.split_once('=')?;
                Some((k.to_string(), v.to_string()))
            })
            // Sockets are inherited descriptors of that process, meaningless elsewhere.
            .filter(|(k, _)| !k.starts_with("ANDROID_SOCKET_"))
            .collect();
        if vars.iter().any(|(k, _)| k == "BOOTCLASSPATH") {
            return Some(vars);
        }
    }
    None
}

fn exec(conn: File) {
    let Ok(read_half) = conn.try_clone() else { return };
    let mut out = conn;
    let mut line = String::new();
    if BufReader::new(read_half).read_line(&mut line).is_err() {
        return;
    }
    let (target, cmd) = parse_command(&line);
    let mut command = match target {
        "guest" => {
            let mut c = Command::new("/bin/sh");
            c.arg("-c").arg(cmd);
            c
        }
        "android" => {
            let Some(pid) = container_init_pid() else {
                let _ = writeln!(out, "error: no running container");
                return;
            };
            let mut c = Command::new("/usr/bin/nsenter");
            c.args(["-t", &pid.to_string(), "-m", "-p", "-u", "-i", "--", "/system/bin/sh", "-c", cmd]);
            if let Some(env) = android_env() {
                c.env_clear().envs(env);
            }
            c
        }
        _ => {
            let _ = writeln!(out, "error: usage: guest <cmd> | android <cmd>");
            return;
        }
    };
    command.stdin(Stdio::null());
    match command.output() {
        Ok(o) => {
            let _ = out.write_all(&o.stdout);
            let _ = out.write_all(&o.stderr);
        }
        Err(e) => {
            let _ = writeln!(out, "error: {e}");
        }
    }
}

fn control(conn: File) {
    let Ok(read_half) = conn.try_clone() else { return };
    let mut out = conn;
    for line in BufReader::new(read_half).lines() {
        let Ok(line) = line else { return };
        let (verb, _rest) = parse_command(&line);
        let reply = match verb {
            "ping" => "pong".to_string(),
            "uptime" => std::fs::read_to_string("/proc/uptime").unwrap_or_default().trim().to_string(),
            "meminfo" => std::fs::read_to_string("/proc/meminfo")
                .unwrap_or_default()
                .lines()
                .filter(|l| {
                    ["MemTotal:", "MemFree:", "MemAvailable:", "Cached:"].iter().any(|k| l.starts_with(k))
                })
                .collect::<Vec<_>>()
                .join(" "),
            // Give page cache back so the host can reclaim it through the balloon. Cheap to
            // rebuild (the Android image is read-only and cached by the host too).
            "drop_caches" => {
                // SAFETY: sync(2) has no preconditions.
                unsafe { libc::sync() };
                match std::fs::write("/proc/sys/vm/drop_caches", "1") {
                    Ok(()) => "ok".to_string(),
                    Err(e) => format!("error: {e}"),
                }
            }
            "" => continue,
            other => format!("error unknown command: {other}"),
        };
        if writeln!(out, "{reply}").is_err() {
            return;
        }
    }
}

pub fn run() -> ! {
    let services: [(u32, &str, fn(File)); 6] = [
        (ADB_PORT, "adb", adb),
        (CONTROL_PORT, "control", control),
        (ECHO_PORT, "echo", echo),
        (EXEC_PORT, "exec", exec),
        (SCRCPY_VIDEO_PORT, "scrcpy-video", crate::scrcpy::video),
        (SCRCPY_CONTROL_PORT, "scrcpy-control", crate::scrcpy::control),
    ];
    for (port, name, h) in services {
        match listen(port, h) {
            Ok(()) => log(&format!("{name} listening on vsock port {port}")),
            Err(e) => log(&format!("{name} on vsock port {port}: {e}")),
        }
    }
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}
