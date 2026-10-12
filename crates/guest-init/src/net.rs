//! Link bring-up and a one-shot DHCP lease using plain ioctls + UDP (no netlink, no daemon).
use std::io;
use std::mem;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::os::fd::AsRawFd;
use std::time::Duration;

use crate::dhcp::{self, Reply};
use crate::sys::log;

fn ifreq(name: &str) -> libc::ifreq {
    // SAFETY: ifreq is plain-old-data; all-zero is a valid value.
    let mut r: libc::ifreq = unsafe { mem::zeroed() };
    for (d, s) in r.ifr_name.iter_mut().zip(name.bytes().take(libc::IFNAMSIZ - 1)) {
        *d = s as libc::c_char;
    }
    r
}

fn ctl_socket() -> io::Result<libc::c_int> {
    // SAFETY: plain socket(2) call.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

fn with_socket<T>(f: impl FnOnce(libc::c_int) -> io::Result<T>) -> io::Result<T> {
    let fd = ctl_socket()?;
    let r = f(fd);
    // SAFETY: fd came from socket() above and is closed exactly once.
    unsafe { libc::close(fd) };
    r
}

fn ioctl<T>(fd: libc::c_int, req: libc::c_ulong, arg: &mut T) -> io::Result<()> {
    // SAFETY: `arg` is a valid exclusive reference to the struct the request expects.
    if unsafe { libc::ioctl(fd, req as _, arg as *mut T) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn link_up(name: &str) -> io::Result<()> {
    with_socket(|fd| {
        let mut r = ifreq(name);
        ioctl(fd, libc::SIOCGIFFLAGS as _, &mut r)?;
        // SAFETY: SIOCGIFFLAGS filled the flags member of the union.
        unsafe { r.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short };
        ioctl(fd, libc::SIOCSIFFLAGS as _, &mut r)
    })
}

pub fn mac(name: &str) -> io::Result<[u8; 6]> {
    with_socket(|fd| {
        let mut r = ifreq(name);
        ioctl(fd, libc::SIOCGIFHWADDR as _, &mut r)?;
        // SAFETY: SIOCGIFHWADDR filled the hwaddr member of the union.
        let data = unsafe { r.ifr_ifru.ifru_hwaddr.sa_data };
        let mut m = [0u8; 6];
        for (d, s) in m.iter_mut().zip(data.iter()) {
            *d = *s as u8;
        }
        Ok(m)
    })
}

fn sockaddr_in(ip: Ipv4Addr) -> libc::sockaddr {
    // SAFETY: sockaddr_in is POD; we reinterpret it as the generic sockaddr the ioctls take.
    unsafe {
        let mut a: libc::sockaddr_in = mem::zeroed();
        a.sin_family = libc::AF_INET as libc::sa_family_t;
        a.sin_addr.s_addr = u32::from_ne_bytes(ip.octets());
        mem::transmute::<libc::sockaddr_in, libc::sockaddr>(a)
    }
}

fn set_addr(name: &str, ip: Ipv4Addr, mask: Ipv4Addr) -> io::Result<()> {
    with_socket(|fd| {
        let mut r = ifreq(name);
        r.ifr_ifru.ifru_addr = sockaddr_in(ip);
        ioctl(fd, libc::SIOCSIFADDR as _, &mut r)?;
        let mut r = ifreq(name);
        r.ifr_ifru.ifru_netmask = sockaddr_in(mask);
        ioctl(fd, libc::SIOCSIFNETMASK as _, &mut r)
    })
}

fn add_default_route(name: &str, gw: Ipv4Addr) -> io::Result<()> {
    with_socket(|fd| {
        // SAFETY: rtentry is POD; zeroed is valid. `name` must outlive the ioctl call.
        let mut rt: libc::rtentry = unsafe { mem::zeroed() };
        rt.rt_dst = sockaddr_in(Ipv4Addr::UNSPECIFIED);
        rt.rt_genmask = sockaddr_in(Ipv4Addr::UNSPECIFIED);
        rt.rt_gateway = sockaddr_in(gw);
        rt.rt_flags = libc::RTF_UP | libc::RTF_GATEWAY;
        let dev = std::ffi::CString::new(name).unwrap();
        rt.rt_dev = dev.as_ptr() as *mut libc::c_char;
        ioctl(fd, libc::SIOCADDRT as _, &mut rt)
    })
}

/// `AF_PACKET` datagram socket bound to one interface. Replies to a client that has no IP yet
/// are dropped by the normal UDP path (rp_filter treats the server as a martian source), so
/// DHCP replies are read at this layer instead — the same trick udhcpc uses.
struct PacketSock(libc::c_int);

use std::sync::atomic::{AtomicU32, Ordering};
static SEEN_IP: AtomicU32 = AtomicU32::new(0);
static SEEN_DHCP_PORT: AtomicU32 = AtomicU32::new(0);
static PARSE_FAILED: AtomicU32 = AtomicU32::new(0);

/// Human-readable counters for the failure log.
pub fn dhcp_debug() -> String {
    format!(
        "ip_pkts={} udp68_pkts={} unparsable={}",
        SEEN_IP.load(Ordering::Relaxed),
        SEEN_DHCP_PORT.load(Ordering::Relaxed),
        PARSE_FAILED.load(Ordering::Relaxed)
    )
}

impl PacketSock {
    fn open(ifname: &str) -> io::Result<Self> {
        let proto = (libc::ETH_P_IP as u16).to_be() as libc::c_int;
        // SAFETY: plain socket(2) call.
        let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, proto) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let sock = PacketSock(fd);
        let name = std::ffi::CString::new(ifname).unwrap();
        // SAFETY: valid NUL-terminated name.
        let idx = unsafe { libc::if_nametoindex(name.as_ptr()) };
        if idx == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: sockaddr_ll is POD; zeroed then filled.
        let mut sll: libc::sockaddr_ll = unsafe { mem::zeroed() };
        sll.sll_family = libc::AF_PACKET as u16;
        sll.sll_protocol = (libc::ETH_P_IP as u16).to_be();
        sll.sll_ifindex = idx as i32;
        // SAFETY: sll is a valid sockaddr_ll of the given size.
        let r = unsafe {
            libc::bind(fd, &sll as *const _ as *const libc::sockaddr, mem::size_of::<libc::sockaddr_ll>() as u32)
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        let tv = libc::timeval { tv_sec: 2, tv_usec: 0 };
        // SAFETY: tv outlives the call; size matches.
        let r = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const _ as *const libc::c_void,
                mem::size_of::<libc::timeval>() as u32,
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(sock)
    }

    /// Next DHCP-client-port UDP payload, or `None` on timeout/error.
    fn recv_dhcp(&self, buf: &mut [u8]) -> Option<usize> {
        let n = unsafe { libc::recv(self.0, buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n <= 0 {
            return None;
        }
        let pkt = &buf[..n as usize];
        SEEN_IP.fetch_add(1, Ordering::Relaxed);
        let payload = dhcp::udp_payload(pkt, dhcp::CLIENT_PORT)?;
        SEEN_DHCP_PORT.fetch_add(1, Ordering::Relaxed);
        let (start, len) = (payload.as_ptr() as usize - pkt.as_ptr() as usize, payload.len());
        buf.copy_within(start..start + len, 0);
        Some(len)
    }
}

impl Drop for PacketSock {
    fn drop(&mut self) {
        // SAFETY: fd owned by this struct, closed once.
        unsafe { libc::close(self.0) };
    }
}

fn exchange(tx: &UdpSocket, rx: &PacketSock, send: &[u8], xid: u32, want: u8) -> Option<Reply> {
    let dest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, dhcp::SERVER_PORT));
    for _ in 0..10 {
        if tx.send_to(send, dest).is_err() {
            continue;
        }
        let mut buf = [0u8; 1500];
        // Unrelated traffic may arrive first; keep reading until the timeout.
        for _ in 0..16 {
            let Some(n) = rx.recv_dhcp(&mut buf) else { break };
            let parsed = dhcp::parse_reply(&buf[..n], xid);
            if parsed.is_none() {
                PARSE_FAILED.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(r) = parsed {
                if r.msg_type == want {
                    return Some(r);
                }
                if r.msg_type == dhcp::NAK {
                    return None;
                }
            }
        }
    }
    None
}

/// First non-loopback interface backed by a real device (initramfs udev may have renamed
/// `eth0` to `enp0s*`, so the name can't be assumed). Waits briefly for it to appear.
pub fn find_ethernet() -> Option<String> {
    for _ in 0..50 {
        if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
            let mut names: Vec<String> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n != "lo" && std::path::Path::new(&format!("/sys/class/net/{n}/device")).exists())
                .collect();
            names.sort();
            if let Some(n) = names.into_iter().next() {
                return Some(n);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// Bring up `ifname` and configure it from a DHCP lease. Returns the lease on success.
pub fn configure_dhcp(ifname: &str) -> io::Result<Reply> {
    link_up(ifname)?;
    let hw = mac(ifname)?;
    let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, dhcp::CLIENT_PORT))?;
    sock.set_broadcast(true)?;
    let rx = PacketSock::open(ifname)?;
    let name = std::ffi::CString::new(ifname).unwrap();
    // SAFETY: valid fd, option buffer is the NUL-terminated interface name.
    let r = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            name.as_ptr().cast(),
            name.as_bytes().len() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }

    // Transaction id: any value works; derive from MAC + clock so VMs differ.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let xid = now ^ u32::from_be_bytes([hw[2], hw[3], hw[4], hw[5]]);

    let offer = exchange(&sock, &rx, &dhcp::discover(xid, hw), xid, dhcp::OFFER)
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "no DHCP offer"))?;
    let server = offer.server_id.unwrap_or(Ipv4Addr::BROADCAST);
    let ack = exchange(&sock, &rx, &dhcp::request(xid, hw, offer.yiaddr, server), xid, dhcp::ACK)
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "DHCP request not acknowledged"))?;

    let mask = ack.subnet.unwrap_or(Ipv4Addr::new(255, 255, 255, 0));
    set_addr(ifname, ack.yiaddr, mask)?;
    if let Some(gw) = ack.router {
        if let Err(e) = add_default_route(ifname, gw) {
            log(&format!("default route via {gw}: {e}"));
        }
    }
    if !ack.dns.is_empty() {
        let _ = std::fs::create_dir_all("/run/moniqi");
        let body: String = ack.dns.iter().map(|d| format!("nameserver {d}\n")).collect();
        let _ = std::fs::write("/run/moniqi/resolv.conf", body);
    }
    Ok(ack)
}

/// Diagnostic (not called on the normal path): send one DISCOVER, then log a summary of every Ethernet frame seen for `secs`.
#[allow(dead_code)]
pub fn sniff_after_discover(ifname: &str, secs: u64) -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: plain socket(2) call.
    let fd = unsafe {
        libc::socket(libc::AF_PACKET, libc::SOCK_RAW | libc::SOCK_CLOEXEC, (libc::ETH_P_ALL as u16).to_be() as libc::c_int)
    };
    if fd < 0 {
        return vec![format!("sniff socket: {}", io::Error::last_os_error())];
    }
    let name = std::ffi::CString::new(ifname).unwrap();
    // SAFETY: valid name; sockaddr_ll zeroed then filled.
    unsafe {
        let mut sll: libc::sockaddr_ll = mem::zeroed();
        sll.sll_family = libc::AF_PACKET as u16;
        sll.sll_protocol = (libc::ETH_P_ALL as u16).to_be();
        sll.sll_ifindex = libc::if_nametoindex(name.as_ptr()) as i32;
        libc::bind(fd, &sll as *const _ as *const libc::sockaddr, mem::size_of::<libc::sockaddr_ll>() as u32);
        let tv = libc::timeval { tv_sec: 1, tv_usec: 0 };
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, &tv as *const _ as *const libc::c_void, mem::size_of::<libc::timeval>() as u32);
    }
    match (UdpSocket::bind("0.0.0.0:68"), mac(ifname)) {
        (Ok(tx), Ok(hw)) => {
            let _ = tx.set_broadcast(true);
            // SAFETY: valid fd and NUL-terminated interface name.
            let r = unsafe {
                libc::setsockopt(tx.as_raw_fd(), libc::SOL_SOCKET, libc::SO_BINDTODEVICE, name.as_ptr().cast(), name.as_bytes().len() as libc::socklen_t)
            };
            out.push(format!("bindtodevice rc={r}"));
            match tx.send_to(&dhcp::discover(0x1234_5678, hw), "255.255.255.255:67") {
                Ok(n) => out.push(format!("sent {n} bytes from mac {hw:02x?}")),
                Err(e) => out.push(format!("send failed: {e}")),
            }
        }
        (Err(e), _) => out.push(format!("bind 0.0.0.0:68: {e}")),
        (_, Err(e)) => out.push(format!("mac: {e}")),
    }
    let start = std::time::Instant::now();
    let mut buf = [0u8; 2048];
    while start.elapsed() < Duration::from_secs(secs) && out.len() < 40 {
        // SAFETY: buf is a valid writable buffer of the given length.
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 14 {
            continue;
        }
        let f = &buf[..n as usize];
        let et = u16::from_be_bytes([f[12], f[13]]);
        let mut d = format!(
            "len={n} dst={:02x?} src={:02x?} type=0x{et:04x}",
            &f[0..6], &f[6..12]
        );
        if et == 0x0800 && n >= 14 + 20 {
            let ip = &f[14..];
            let ihl = (ip[0] & 0xf) as usize * 4;
            d += &format!(" ip {}.{}.{}.{} -> {}.{}.{}.{} proto={}", ip[12], ip[13], ip[14], ip[15], ip[16], ip[17], ip[18], ip[19], ip[9]);
            if ip[9] == 17 && ip.len() >= ihl + 4 {
                d += &format!(" udp {} -> {}", u16::from_be_bytes([ip[ihl], ip[ihl + 1]]), u16::from_be_bytes([ip[ihl + 2], ip[ihl + 3]]));
            }
        }
        out.push(d);
    }
    // SAFETY: fd owned here.
    unsafe { libc::close(fd) };
    out
}
