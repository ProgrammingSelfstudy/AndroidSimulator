//! Wire-level pieces shared by the host (`moniqi`) and the guest (`moniqi-agent`).
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::thread;

/// vsock port of the guest's adbd forwarder. Also the TCP port adbd/the host forward use.
pub const ADB_PORT: u32 = 5555;
/// Line-oriented control channel (`ping`, `status`, ...).
pub const CONTROL_PORT: u32 = 5000;
/// Diagnostic echo service.
pub const ECHO_PORT: u32 = 5001;
/// One-shot command execution: `android <cmd>` runs inside the container, `guest <cmd>` in the
/// guest itself. Output streams back, then the connection closes. Debug aid: root access.
pub const EXEC_PORT: u32 = 5002;
/// scrcpy video stream (first connection) and control channel (second); see `moniqi-scrcpy`.
pub const SCRCPY_VIDEO_PORT: u32 = 5003;
pub const SCRCPY_CONTROL_PORT: u32 = 5004;

/// A byte stream that can be cloned for the other direction and half-closed.
pub trait Duplex: Read + Write + Send + Sized + 'static {
    fn dup(&self) -> io::Result<Self>;
    fn shutdown_write(&self);
}

impl Duplex for TcpStream {
    fn dup(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

impl Duplex for std::os::unix::net::UnixStream {
    fn dup(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

/// A socket fd wrapped in a `File` (how vsock connections arrive on both sides).
impl Duplex for File {
    fn dup(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn shutdown_write(&self) {
        // SAFETY: valid fd owned by `self`; shutdown on a non-socket just fails harmlessly.
        unsafe { libc::shutdown(self.as_raw_fd(), libc::SHUT_WR) };
    }
}

/// Copy `a <-> b` until both directions hit EOF, propagating half-close so request/response
/// protocols (adb) terminate cleanly. Blocks the calling thread.
pub fn relay<A: Duplex, B: Duplex>(a: A, b: B) -> io::Result<()> {
    let mut a_read = a.dup()?;
    let mut b_write = b.dup()?;
    let t = thread::spawn(move || {
        let _ = io::copy(&mut a_read, &mut b_write);
        b_write.shutdown_write();
    });
    let (mut b_read, mut a_write) = (b, a);
    let _ = io::copy(&mut b_read, &mut a_write);
    a_write.shutdown_write();
    let _ = t.join();
    Ok(())
}

/// How to start the Android container, as declared by its OCI image. Serialised as one
/// `key=value` per line (`env=`, `entrypoint=`, `cmd=`; repeated keys keep their order) so the
/// guest can read it without a JSON parser.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunConfig {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
}

impl RunConfig {
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        for (k, vals) in [("env", &self.env), ("entrypoint", &self.entrypoint), ("cmd", &self.cmd)] {
            for v in vals {
                s.push_str(k);
                s.push('=');
                s.push_str(v);
                s.push('\n');
            }
        }
        s
    }

    pub fn parse(text: &str) -> Self {
        let mut c = RunConfig::default();
        for line in text.lines() {
            match line.split_once('=') {
                Some(("env", v)) => c.env.push(v.to_string()),
                Some(("entrypoint", v)) => c.entrypoint.push(v.to_string()),
                Some(("cmd", v)) => c.cmd.push(v.to_string()),
                _ => {}
            }
        }
        c
    }

    /// Full argv: entrypoint followed by cmd.
    pub fn argv(&self) -> Vec<String> {
        self.entrypoint.iter().chain(self.cmd.iter()).cloned().collect()
    }
}

/// Where the run configuration lives inside the Android image.
pub const RUN_CONFIG_PATH: &str = ".moniqi-run";

/// Parse a control request line into (verb, rest).
pub fn parse_command(line: &str) -> (&str, &str) {
    let line = line.trim();
    match line.split_once(char::is_whitespace) {
        Some((v, r)) => (v, r.trim()),
        None => (line, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (s, _) = l.accept().unwrap();
        (c, s)
    }

    #[test]
    fn relays_both_directions_and_half_close() {
        // client1 <-> [relay] <-> client2, with an echo server behind client2's peer
        let (mut front_client, front_server) = pair();
        let (back_client, mut back_server) = pair();
        let t = thread::spawn(move || relay(front_server, back_client).unwrap());

        front_client.write_all(b"ping").unwrap();
        front_client.shutdown(Shutdown::Write).unwrap();
        let mut got = Vec::new();
        back_server.read_to_end(&mut got).unwrap(); // sees EOF thanks to half-close
        assert_eq!(got, b"ping");
        back_server.write_all(b"pong").unwrap();
        back_server.shutdown(Shutdown::Write).unwrap();
        let mut resp = Vec::new();
        front_client.read_to_end(&mut resp).unwrap();
        assert_eq!(resp, b"pong");
        t.join().unwrap();
    }

    #[test]
    fn run_config_round_trips() {
        let c = RunConfig {
            entrypoint: vec!["/init".into()],
            cmd: vec!["qemu=1".into(), "androidboot.hardware=redroid".into()],
            env: vec!["PATH=/a:/b".into(), "X=a=b".into()],
        };
        assert_eq!(RunConfig::parse(&c.to_text()), c);
        assert_eq!(c.argv(), vec!["/init", "qemu=1", "androidboot.hardware=redroid"]);
    }

    #[test]
    fn parses_commands() {
        assert_eq!(parse_command("ping\n"), ("ping", ""));
        assert_eq!(parse_command("  exec  ls -l \n"), ("exec", "ls -l"));
        assert_eq!(parse_command(""), ("", ""));
    }
}
