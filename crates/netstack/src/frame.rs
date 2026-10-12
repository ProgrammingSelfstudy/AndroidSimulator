//! Ethernet / IPv4 / UDP parsing and building for the few protocols handled outside smoltcp.
use std::net::Ipv4Addr;

pub type Mac = [u8; 6];

pub const ETH_ARP: u16 = 0x0806;
pub const ETH_IPV4: u16 = 0x0800;
pub const BROADCAST: Mac = [0xff; 6];

pub fn csum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        sum += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    if let [b] = chunks.remainder() {
        sum += (*b as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// A parsed IPv4 packet inside an Ethernet frame.
pub struct Ipv4<'a> {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub proto: u8,
    pub payload: &'a [u8],
}

pub fn eth_type(f: &[u8]) -> Option<u16> {
    (f.len() >= 14).then(|| u16::from_be_bytes([f[12], f[13]]))
}

pub fn eth_src(f: &[u8]) -> Mac {
    f[6..12].try_into().unwrap()
}

pub fn parse_ipv4(frame: &[u8]) -> Option<Ipv4<'_>> {
    if eth_type(frame)? != ETH_IPV4 || frame.len() < 14 + 20 {
        return None;
    }
    let ip = &frame[14..];
    let ihl = (ip[0] & 0x0f) as usize * 4;
    let total = u16::from_be_bytes([ip[2], ip[3]]) as usize;
    if ip[0] >> 4 != 4 || ihl < 20 || total < ihl || ip.len() < total {
        return None;
    }
    // Fragments are not reassembled.
    if u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff != 0 {
        return None;
    }
    Some(Ipv4 {
        src: Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]),
        dst: Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]),
        proto: ip[9],
        payload: &ip[ihl..total],
    })
}

pub struct Udp<'a> {
    pub sport: u16,
    pub dport: u16,
    pub data: &'a [u8],
}

pub fn parse_udp(payload: &[u8]) -> Option<Udp<'_>> {
    if payload.len() < 8 {
        return None;
    }
    let len = u16::from_be_bytes([payload[4], payload[5]]) as usize;
    Some(Udp {
        sport: u16::from_be_bytes([payload[0], payload[1]]),
        dport: u16::from_be_bytes([payload[2], payload[3]]),
        data: payload.get(8..len.clamp(8, payload.len()))?,
    })
}

fn eth_header(out: &mut Vec<u8>, dst: Mac, src: Mac, ty: u16) {
    out.extend_from_slice(&dst);
    out.extend_from_slice(&src);
    out.extend_from_slice(&ty.to_be_bytes());
}

/// Ethernet + IPv4 frame around `payload` (UDP checksum is optional in IPv4, left zero).
pub fn build_ipv4(dst_mac: Mac, src_mac: Mac, src: Ipv4Addr, dst: Ipv4Addr, proto: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(14 + 20 + payload.len());
    eth_header(&mut f, dst_mac, src_mac, ETH_IPV4);
    let total = (20 + payload.len()) as u16;
    let mut ip = [0u8; 20];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&total.to_be_bytes());
    ip[6] = 0x40; // don't fragment
    ip[8] = 64;
    ip[9] = proto;
    ip[12..16].copy_from_slice(&src.octets());
    ip[16..20].copy_from_slice(&dst.octets());
    let c = csum(&ip);
    ip[10..12].copy_from_slice(&c.to_be_bytes());
    f.extend_from_slice(&ip);
    f.extend_from_slice(payload);
    f
}

pub fn build_udp(dst_mac: Mac, src_mac: Mac, src: Ipv4Addr, sport: u16, dst: Ipv4Addr, dport: u16, data: &[u8]) -> Vec<u8> {
    let mut u = Vec::with_capacity(8 + data.len());
    u.extend_from_slice(&sport.to_be_bytes());
    u.extend_from_slice(&dport.to_be_bytes());
    u.extend_from_slice(&((8 + data.len()) as u16).to_be_bytes());
    u.extend_from_slice(&[0, 0]);
    u.extend_from_slice(data);
    build_ipv4(dst_mac, src_mac, src, dst, 17, &u)
}

/// ARP request target IP, if `frame` is an ARP request for IPv4.
pub fn arp_request_target(frame: &[u8]) -> Option<(Mac, Ipv4Addr, Ipv4Addr)> {
    if eth_type(frame)? != ETH_ARP || frame.len() < 14 + 28 {
        return None;
    }
    let a = &frame[14..];
    let is_req = u16::from_be_bytes([a[6], a[7]]) == 1;
    let ipv4_eth = a[0..2] == [0, 1] && a[2..4] == [8, 0] && a[4] == 6 && a[5] == 4;
    if !is_req || !ipv4_eth {
        return None;
    }
    let sha: Mac = a[8..14].try_into().ok()?;
    Some((sha, Ipv4Addr::new(a[14], a[15], a[16], a[17]), Ipv4Addr::new(a[24], a[25], a[26], a[27])))
}

pub fn build_arp_reply(our_mac: Mac, our_ip: Ipv4Addr, to_mac: Mac, to_ip: Ipv4Addr) -> Vec<u8> {
    let mut f = Vec::with_capacity(42);
    eth_header(&mut f, to_mac, our_mac, ETH_ARP);
    f.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 2]);
    f.extend_from_slice(&our_mac);
    f.extend_from_slice(&our_ip.octets());
    f.extend_from_slice(&to_mac);
    f.extend_from_slice(&to_ip.octets());
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_roundtrip() {
        let f = build_udp([1; 6], [2; 6], Ipv4Addr::new(10, 0, 2, 2), 53, Ipv4Addr::new(10, 0, 2, 15), 4000, b"hello");
        let ip = parse_ipv4(&f).unwrap();
        assert_eq!((ip.src, ip.dst, ip.proto), (Ipv4Addr::new(10, 0, 2, 2), Ipv4Addr::new(10, 0, 2, 15), 17));
        let u = parse_udp(ip.payload).unwrap();
        assert_eq!((u.sport, u.dport, u.data), (53, 4000, &b"hello"[..]));
        // A valid header checksums to zero.
        assert_eq!(csum(&f[14..34]), 0);
    }

    #[test]
    fn arp_request_and_reply() {
        let mut f = Vec::new();
        eth_header(&mut f, BROADCAST, [9; 6], ETH_ARP);
        f.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 1]);
        f.extend_from_slice(&[9; 6]);
        f.extend_from_slice(&[10, 0, 2, 15]);
        f.extend_from_slice(&[0; 6]);
        f.extend_from_slice(&[10, 0, 2, 3]);
        let (sha, spa, tpa) = arp_request_target(&f).unwrap();
        assert_eq!((sha, spa, tpa), ([9; 6], Ipv4Addr::new(10, 0, 2, 15), Ipv4Addr::new(10, 0, 2, 3)));
        let r = build_arp_reply([7; 6], tpa, sha, spa);
        assert_eq!(r.len(), 42);
        assert_eq!(&r[0..6], &[9; 6]);
    }
}
