//! Host TCP listeners that feed [`Machine::pump_forwards`](crate::machine::Machine).
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::mpsc::{channel, Receiver};
use std::thread;

use anyhow::{Context, Result};

pub struct Forward {
    pub host_port: u16,
    pub guest_port: u32,
    rx: Receiver<TcpStream>,
    /// Set after the first reported failure; cleared by a success. The guest refuses
    /// connections for the first seconds of every boot and retrying clients would flood the log.
    warned: Arc<AtomicBool>,
}

impl Forward {
    /// Listen on `127.0.0.1:host_port` (loopback only: the guest is not a network service).
    pub fn listen(host_port: u16, guest_port: u32) -> Result<Self> {
        let l = TcpListener::bind(("127.0.0.1", host_port))
            .with_context(|| format!("bind 127.0.0.1:{host_port} (already in use?)"))?;
        let (tx, rx) = channel();
        thread::spawn(move || {
            for s in l.incoming().flatten() {
                let _ = s.set_nodelay(true);
                if tx.send(s).is_err() {
                    break;
                }
            }
        });
        Ok(Self { host_port, guest_port, rx, warned: Arc::new(AtomicBool::new(false)) })
    }

    pub fn try_accept(&self) -> Option<TcpStream> {
        self.rx.try_recv().ok()
    }

    /// Handle for connection callbacks: report the first failure after a success, then stay
    /// quiet until one succeeds again.
    pub fn failure_gate(&self) -> Arc<AtomicBool> {
        self.warned.clone()
    }
}

/// Parse `HOST:GUEST` (both ports).
pub fn parse_spec(spec: &str) -> Result<(u16, u32)> {
    let (h, g) = spec.split_once(':').context("expected HOST:GUEST")?;
    Ok((h.parse().context("host port")?, g.parse().context("guest port")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_specs() {
        assert_eq!(parse_spec("5555:5555").unwrap(), (5555, 5555));
        assert_eq!(parse_spec("15000:5000").unwrap(), (15000, 5000));
        assert!(parse_spec("5555").is_err());
        assert!(parse_spec("a:1").is_err());
        assert!(parse_spec("70000:1").is_err());
    }
}
