//! scrcpy server lifecycle and the vsock <-> abstract-socket bridges.
//!
//! The server runs inside the container (`app_process`), listening on an abstract unix socket
//! (`tunnel_forward=true`). The container shares our network namespace, but abstract sockets
//! live in the *network* namespace, so we can connect to it directly from the guest.
use std::fs::File;
use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use moniqi_proto::relay;

const JAR_IN_GUEST: &str = "/usr/lib/moniqi/scrcpy-server.jar";
const JAR_IN_CONTAINER: &str = "/data/local/tmp/scrcpy-server.jar";
const SCID: &str = "00004d51"; // fixed: one display session at a time
const VERSION: &str = "5.0.1";

/// The running server and the session that started it. A session ending must only stop *its own*
/// server: a reconnect starts a new server before the old session's thread has unwound.
static SERVER: Mutex<Option<(u64, Child)>> = Mutex::new(None);
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

fn log(msg: &str) {
    println!("moniqi-agent: scrcpy: {msg}");
}

fn connect_abstract(deadline: Instant) -> io::Result<UnixStream> {
    let name = format!("scrcpy_{SCID}");
    let addr = SocketAddr::from_abstract_name(name.as_bytes())?;
    loop {
        match UnixStream::connect_addr(&addr) {
            Ok(s) => return Ok(s),
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

/// `nsenter -p` forks, so killing only its pid would orphan `app_process`. The server is
/// started in its own process group and the whole group is killed.
///
/// `only` limits the kill to the server started by that session; `None` kills whatever runs.
fn kill_server(only: Option<u64>) {
    let mut slot = SERVER.lock().unwrap();
    if only.is_some_and(|id| slot.as_ref().map(|(s, _)| *s) != Some(id)) {
        return;
    }
    if let Some((_, mut c)) = slot.take() {
        // SAFETY: killpg on the process group we created for this child.
        unsafe { libc::killpg(c.id() as libc::pid_t, libc::SIGKILL) };
        let _ = c.wait();
    }
}

fn start_server(session: u64) -> io::Result<()> {
    kill_server(None);
    let pid = crate::vsock::container_init_pid()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no running container"))?;
    // Make the jar visible inside the container through its mount namespace.
    let dst = format!("/proc/{pid}/root{JAR_IN_CONTAINER}");
    std::fs::copy(JAR_IN_GUEST, &dst).map_err(|e| io::Error::new(e.kind(), format!("copy jar to {dst}: {e}")))?;

    // Never ask the encoder for more frames than the display produces.
    let fps = crate::android::cmdline_flag("moniqi.fps")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(30)
        .clamp(5, 60);
    let script = format!(
        "CLASSPATH={JAR_IN_CONTAINER} exec app_process / com.genymobile.scrcpy.Server {VERSION} \
         scid={SCID} tunnel_forward=true audio=false control=true cleanup=false \
         max_size=1280 max_fps={fps} video_bit_rate=4000000 log_level=info"
    );
    let env = crate::vsock::android_env()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Android environment not available yet (zygote not up)"))?;
    let child = Command::new("/usr/bin/nsenter")
        .args(["-t", &pid.to_string(), "-m", "-p", "-u", "-i", "--", "/system/bin/sh", "-c", &script])
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .process_group(0)
        .spawn()?;
    log(&format!("server started (nsenter pid {})", child.id()));
    *SERVER.lock().unwrap() = Some((session, child));
    Ok(())
}

fn owns_server(session: u64) -> bool {
    SERVER.lock().unwrap().as_ref().is_some_and(|(s, _)| *s == session)
}

/// First connection: (re)start the server and bridge the video socket.
pub fn video(conn: File) {
    let session = NEXT_SESSION.fetch_add(1, Ordering::SeqCst);
    if let Err(e) = start_server(session) {
        log(&format!("start failed: {e}"));
        return;
    }
    match connect_abstract(Instant::now() + Duration::from_secs(20)) {
        // A newer session may have replaced our server while we were waiting to connect.
        Ok(_) if !owns_server(session) => log("video session superseded"),
        Ok(s) => {
            let _ = relay(conn, s);
        }
        Err(e) => log(&format!("video connect failed: {e}")),
    }
    log("video session ended");
    kill_server(Some(session));
}

/// Second connection: bridge the control socket (the server accepts video first, then control).
pub fn control(conn: File) {
    match connect_abstract(Instant::now() + Duration::from_secs(20)) {
        Ok(s) => {
            let _ = relay(conn, s);
        }
        Err(e) => log(&format!("control connect failed: {e}")),
    }
}
