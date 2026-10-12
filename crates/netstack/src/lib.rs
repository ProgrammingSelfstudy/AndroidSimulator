//! User-mode networking for the guest (what QEMU calls "slirp").
//!
//! The VM's NIC is a datagram socket whose messages are Ethernet frames
//! (`VZFileHandleNetworkDeviceAttachment`). This crate plays the gateway on that wire:
//! ARP, a one-lease DHCP server, DNS/UDP forwarding, ICMP echo to the gateway, and a TCP
//! proxy built on smoltcp. Every guest connection becomes an ordinary socket owned by this
//! process, so traffic follows the host's own routing, VPN and proxy settings — unlike
//! macOS's kernel NAT, which a TUN-mode VPN can bypass.
mod dhcp;
mod frame;

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet};
use smoltcp::phy::{Checksum, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpListenEndpoint, Ipv4Address};

use frame::{Mac, BROADCAST};

pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
pub const DNS: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
pub const GUEST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
const GATEWAY_MAC: Mac = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];
const MTU: usize = 1500;
const TCP_BUF: usize = 256 * 1024;
const UDP_IDLE: Duration = Duration::from_secs(60);
const MAX_CONNS: usize = 1024;

fn debug() -> bool {
    use std::sync::OnceLock;
    static D: OnceLock<bool> = OnceLock::new();
    *D.get_or_init(|| std::env::var_os("MONIQI_NET_DEBUG").is_some())
}

fn describe(f: &[u8]) -> String {
    match frame::parse_ipv4(f) {
        Some(ip) if ip.proto == 6 && ip.payload.len() >= 14 => format!(
            "tcp {}:{} -> {}:{} flags={:#04x} len={}",
            ip.src,
            u16::from_be_bytes([ip.payload[0], ip.payload[1]]),
            ip.dst,
            u16::from_be_bytes([ip.payload[2], ip.payload[3]]),
            ip.payload[13],
            ip.payload.len()
        ),
        Some(ip) => format!("ip proto={} {} -> {}", ip.proto, ip.src, ip.dst),
        None => format!("eth type={:#06x} len={}", frame::eth_type(f).unwrap_or(0), f.len()),
    }
}

/// Start the network thread on `fd` (our end of the guest NIC's socketpair).
pub fn spawn(fd: OwnedFd) -> io::Result<JoinHandle<()>> {
    set_nonblocking(fd.as_raw_fd())?;
    std::thread::Builder::new().name("moniqi-net".into()).spawn(move || {
        let mut stack = Stack::new(fd);
        stack.run();
    })
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain fcntl on an fd we own.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl < 0 || libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// smoltcp device fed by hand: the loop pushes TCP frames in and drains what it emits.
#[derive(Default)]
struct Dev {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}

struct Rx(Vec<u8>);
struct Tx<'a>(&'a mut VecDeque<Vec<u8>>);

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}
impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        self.0.push_back(buf);
        r
    }
}
impl Device for Dev {
    type RxToken<'a> = Rx where Self: 'a;
    type TxToken<'a> = Tx<'a> where Self: 'a;

    fn receive(&mut self, _: SmolInstant) -> Option<(Rx, Tx<'_>)> {
        let f = self.rx.pop_front()?;
        Some((Rx(f), Tx(&mut self.tx)))
    }
    fn transmit(&mut self, _: SmolInstant) -> Option<Tx<'_>> {
        Some(Tx(&mut self.tx))
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = MTU + 14;
        // The guest's virtio-net hands over frames with checksum offload pending; verifying
        // them would drop every TCP segment. Compute on transmit only.
        c.checksum.ipv4 = Checksum::Tx;
        c.checksum.tcp = Checksum::Tx;
        c.checksum.udp = Checksum::Tx;
        c.checksum.icmpv4 = Checksum::Tx;
        c
    }
}

enum Host {
    /// Connect running on a helper thread.
    Connecting(mpsc::Receiver<io::Result<TcpStream>>),
    Open(TcpStream),
    Gone,
}

struct Conn {
    handle: SocketHandle,
    dst: SocketAddrV4,
    host: Host,
    started: bool,
    to_host: Vec<u8>,
    shut_wr: bool,
    host_eof: bool,
}

struct UdpFlow {
    sock: UdpSocket,
    guest_port: u16,
    /// What the guest believes it is talking to (replies are sent from here).
    seen_as: SocketAddrV4,
    last: Instant,
}

struct Stack {
    fd: OwnedFd,
    dev: Dev,
    iface: Interface,
    sockets: SocketSet<'static>,
    conns: Vec<Conn>,
    udp: HashMap<(u16, SocketAddrV4), UdpFlow>,
    guest_mac: Option<Mac>,
    resolvers: Vec<SocketAddr>,
    epoch: Instant,
}

impl Stack {
    fn new(fd: OwnedFd) -> Self {
        let mut dev = Dev::default();
        let mut cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(GATEWAY_MAC)));
        cfg.random_seed = 0x6d6f_6e69_7169;
        let mut iface = Interface::new(cfg, &mut dev, SmolInstant::from_millis(0));
        let [a, b, c, d] = GATEWAY.octets();
        iface.update_ip_addrs(|v| {
            v.push(IpCidr::new(IpAddress::v4(a, b, c, d), 24)).ok();
        });
        // `any_ip`: accept TCP aimed at any address, routed "via" our own address.
        iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(a, b, c, d)).ok();
        iface.set_any_ip(true);
        Stack {
            fd,
            dev,
            iface,
            sockets: SocketSet::new(Vec::new()),
            conns: Vec::new(),
            udp: HashMap::new(),
            guest_mac: None,
            resolvers: host_resolvers(),
            epoch: Instant::now(),
        }
    }

    fn now(&self) -> SmolInstant {
        SmolInstant::from_millis(self.epoch.elapsed().as_millis() as i64)
    }

    fn send_frame(&self, f: &[u8]) {
        // SAFETY: valid fd and buffer; a full socket buffer just drops the frame (TCP resends).
        unsafe { libc::send(self.fd.as_raw_fd(), f.as_ptr().cast(), f.len(), 0) };
    }

    fn run(&mut self) {
        let mut buf = vec![0u8; 65536];
        let mut last_gc = Instant::now();
        loop {
            let mut busy = false;
            // Guest -> us.
            loop {
                // SAFETY: valid fd and buffer.
                let n = unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
                if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if n <= 0 {
                    break; // nothing queued (EAGAIN) or a zero-length datagram
                }
                busy = true;
                let frame = buf[..n as usize].to_vec();
                self.on_guest_frame(frame);
            }
            busy |= self.service_udp();
            let now = self.now();
            let polled = self.iface.poll(now, &mut self.dev, &mut self.sockets);
            busy |= matches!(polled, PollResult::SocketStateChanged);
            busy |= self.service_tcp();
            // One more pass so freshly queued data goes out in this iteration.
            self.iface.poll(self.now(), &mut self.dev, &mut self.sockets);
            while let Some(f) = self.dev.tx.pop_front() {
                busy = true;
                if debug() {
                    eprintln!("net: tx {}", describe(&f));
                }
                self.send_frame(&f);
            }
            if last_gc.elapsed() > Duration::from_secs(5) {
                last_gc = Instant::now();
                self.udp.retain(|_, f| f.last.elapsed() < UDP_IDLE);
            }
            if !busy {
                self.wait();
            }
        }
    }

    /// Sleep until the guest socket or any host socket is readable (bounded: timers, connects).
    fn wait(&self) {
        let mut fds = vec![libc::pollfd { fd: self.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        for c in &self.conns {
            if let Host::Open(s) = &c.host {
                fds.push(libc::pollfd { fd: s.as_raw_fd(), events: libc::POLLIN, revents: 0 });
            }
        }
        for f in self.udp.values() {
            fds.push(libc::pollfd { fd: f.sock.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        }
        // SAFETY: fds is a valid array of the given length.
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 10) };
    }

    fn on_guest_frame(&mut self, f: Vec<u8>) {
        if f.len() < 14 {
            return;
        }
        let src = frame::eth_src(&f);
        if src != GATEWAY_MAC {
            self.guest_mac = Some(src);
        }
        // Proxy ARP: answer for the gateway, the DNS address, and anything else on the subnet.
        if let Some((sha, spa, tpa)) = frame::arp_request_target(&f) {
            if tpa != spa && tpa != GUEST {
                self.send_frame(&frame::build_arp_reply(GATEWAY_MAC, tpa, sha, spa));
            }
            return;
        }
        // ARP replies (and anything else ARP) belong to smoltcp: they fill its neighbor cache,
        // which it needs before it can address a single frame to the guest.
        if frame::eth_type(&f) == Some(frame::ETH_ARP) {
            self.dev.rx.push_back(f);
            return;
        }
        if debug() {
            eprintln!("net: rx {}", describe(&f));
        }
        let Some(ip) = frame::parse_ipv4(&f) else { return };
        match ip.proto {
            17 => self.on_udp(&ip),
            1 => self.on_icmp(&ip),
            6 => self.on_tcp_frame(f.clone()),
            _ => {}
        }
    }

    fn guest_dst(&self) -> Mac {
        self.guest_mac.unwrap_or(BROADCAST)
    }

    // ---- ICMP: answer pings to the gateway; everything else has no unprivileged route out.
    fn on_icmp(&mut self, ip: &frame::Ipv4) {
        let p = ip.payload;
        if p.len() < 8 || p[0] != 8 || (ip.dst != GATEWAY && ip.dst != DNS) {
            return;
        }
        let mut r = p.to_vec();
        r[0] = 0;
        r[2] = 0;
        r[3] = 0;
        let c = frame::csum(&r);
        r[2..4].copy_from_slice(&c.to_be_bytes());
        self.send_frame(&frame::build_ipv4(self.guest_dst(), GATEWAY_MAC, ip.dst, ip.src, 1, &r));
    }

    // ---- UDP: DHCP, DNS and generic datagram flows.
    fn on_udp(&mut self, ip: &frame::Ipv4) {
        let Some(u) = frame::parse_udp(ip.payload) else { return };
        if u.dport == 67 {
            let lease = dhcp::Lease { guest: GUEST, gateway: GATEWAY, mask: NETMASK };
            if let Some(reply) = dhcp::reply(u.data, &lease) {
                let f = frame::build_udp(BROADCAST, GATEWAY_MAC, GATEWAY, 67, Ipv4Addr::BROADCAST, 68, &reply);
                self.send_frame(&f);
            }
            return;
        }
        let seen_as = SocketAddrV4::new(ip.dst, u.dport);
        let key = (u.sport, seen_as);
        if !self.udp.contains_key(&key) {
            if self.udp.len() >= MAX_CONNS {
                return;
            }
            let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return };
            let _ = sock.set_nonblocking(true);
            self.udp.insert(key, UdpFlow { sock, guest_port: u.sport, seen_as, last: Instant::now() });
        }
        let is_dns = u.dport == 53 && (ip.dst == GATEWAY || ip.dst == DNS);
        let flow = self.udp.get_mut(&key).unwrap();
        flow.last = Instant::now();
        if is_dns {
            for r in &self.resolvers {
                let _ = flow.sock.send_to(u.data, r);
            }
        } else if !ip.dst.is_broadcast() && !ip.dst.is_multicast() {
            let _ = flow.sock.send_to(u.data, SocketAddr::V4(seen_as));
        }
    }

    fn service_udp(&mut self) -> bool {
        let mut busy = false;
        let mac = self.guest_dst();
        let mut out = Vec::new();
        let mut buf = [0u8; 2048];
        for f in self.udp.values_mut() {
            while let Ok((n, _from)) = f.sock.recv_from(&mut buf) {
                busy = true;
                f.last = Instant::now();
                out.push(frame::build_udp(mac, GATEWAY_MAC, *f.seen_as.ip(), f.seen_as.port(), GUEST, f.guest_port, &buf[..n]));
            }
        }
        for fr in out {
            self.send_frame(&fr);
        }
        busy
    }

    // ---- TCP: one smoltcp listener per SYN, proxied to a host socket.
    fn on_tcp_frame(&mut self, f: Vec<u8>) {
        if let Some(ip) = frame::parse_ipv4(&f) {
            let t = ip.payload;
            if t.len() >= 14 {
                let (flags, dport) = (t[13], u16::from_be_bytes([t[2], t[3]]));
                let is_syn = flags & 0x02 != 0 && flags & 0x10 == 0;
                if is_syn && self.conns.len() < MAX_CONNS {
                    self.ensure_listener(SocketAddrV4::new(ip.dst, dport));
                }
            }
        }
        self.dev.rx.push_back(f);
    }

    fn ensure_listener(&mut self, dst: SocketAddrV4) {
        let have = self.conns.iter().any(|c| {
            c.dst == dst && !c.started && self.sockets.get::<tcp::Socket>(c.handle).state() == tcp::State::Listen
        });
        if have {
            return;
        }
        let mut s = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; TCP_BUF]), tcp::SocketBuffer::new(vec![0; TCP_BUF]));
        let [a, b, c, d] = dst.ip().octets();
        let ep = IpListenEndpoint { addr: Some(IpAddress::v4(a, b, c, d)), port: dst.port() };
        if s.listen(ep).is_err() {
            return;
        }
        let handle = self.sockets.add(s);
        self.conns.push(Conn { handle, dst, host: Host::Gone, started: false, to_host: Vec::new(), shut_wr: false, host_eof: false });
    }

    fn service_tcp(&mut self) -> bool {
        let mut busy = false;
        let mut i = 0;
        while i < self.conns.len() {
            let c = &mut self.conns[i];
            let sock = self.sockets.get_mut::<tcp::Socket>(c.handle);
            let state = sock.state();

            if !c.started {
                if state == tcp::State::Listen {
                    i += 1;
                    continue;
                }
                if state == tcp::State::Closed {
                    // Listener that never saw its SYN consumed, or a reset: drop it.
                    self.sockets.remove(c.handle);
                    self.conns.swap_remove(i);
                    continue;
                }
                // Handshake started: connect out on a helper thread.
                c.started = true;
                if debug() {
                    eprintln!("net: connect {} (smoltcp state {:?})", c.dst, state);
                }
                let (tx, rx) = mpsc::channel();
                let to = SocketAddr::V4(c.dst);
                std::thread::spawn(move || {
                    let _ = tx.send(TcpStream::connect_timeout(&to, Duration::from_secs(15)));
                });
                c.host = Host::Connecting(rx);
                busy = true;
            }

            if let Host::Connecting(rx) = &c.host {
                match rx.try_recv() {
                    Ok(Ok(s)) => {
                        let _ = s.set_nonblocking(true);
                        let _ = s.set_nodelay(true);
                        c.host = Host::Open(s);
                        busy = true;
                    }
                    Ok(Err(ref e)) if debug() => {
                        eprintln!("net: connect {} failed: {e}", c.dst);
                        sock.abort();
                        c.host = Host::Gone;
                        busy = true;
                    }
                    Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                        sock.abort();
                        c.host = Host::Gone;
                        busy = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }

            if let Host::Open(stream) = &mut c.host {
                // guest -> host
                if c.to_host.is_empty() && sock.can_recv() {
                    let _ = sock.recv(|b| {
                        let n = b.len().min(64 * 1024);
                        c.to_host.extend_from_slice(&b[..n]);
                        (n, ())
                    });
                }
                if !c.to_host.is_empty() {
                    match stream.write(&c.to_host) {
                        Ok(n) => {
                            c.to_host.drain(..n);
                            busy = true;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => {
                            sock.abort();
                            c.host = Host::Gone;
                            continue;
                        }
                    }
                }
                // The guest's FIN, not a handshake still in flight (`may_recv` is false in
                // SynReceived too, which would half-close the host side immediately).
                let guest_fin = matches!(
                    sock.state(),
                    tcp::State::CloseWait | tcp::State::LastAck | tcp::State::Closing | tcp::State::TimeWait
                );
                if guest_fin && c.to_host.is_empty() && !c.shut_wr && !sock.can_recv() {
                    let _ = stream.shutdown(std::net::Shutdown::Write);
                    c.shut_wr = true;
                }
                // host -> guest
                if sock.can_send() && !c.host_eof {
                    let r = sock.send(|b| match stream.read(b) {
                        Ok(0) => (0, Some(true)),
                        Ok(n) => (n, Some(false)),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => (0, None),
                        Err(_) => (0, Some(true)),
                    });
                    match r {
                        Ok(Some(true)) => {
                            c.host_eof = true;
                            sock.close();
                            busy = true;
                        }
                        Ok(Some(false)) => busy = true,
                        _ => {}
                    }
                }
            }

            if matches!(state, tcp::State::Closed | tcp::State::TimeWait) && c.started {
                self.sockets.remove(c.handle);
                self.conns.swap_remove(i);
                continue;
            }
            i += 1;
        }
        busy
    }
}

/// Nameservers from the host's resolver configuration (`/etc/resolv.conf`), else public ones.
fn host_resolvers() -> Vec<SocketAddr> {
    let mut v: Vec<SocketAddr> = std::fs::read_to_string("/etc/resolv.conf")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver "))
        .filter_map(|a| a.trim().parse::<Ipv4Addr>().ok())
        .filter(|a| !a.is_loopback())
        .map(|a| SocketAddr::V4(SocketAddrV4::new(a, 53)))
        .collect();
    if v.is_empty() {
        v.push(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 53)));
    }
    v
}
