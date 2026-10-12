//! Minimal DHCPv4 client packet codec (RFC 2131/2132): DISCOVER, REQUEST, and reply parsing.
use std::net::Ipv4Addr;

pub const SERVER_PORT: u16 = 67;
pub const CLIENT_PORT: u16 = 68;
const MAGIC: [u8; 4] = [99, 130, 83, 99];

const OPT_SUBNET: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_REQ_IP: u8 = 50;
const OPT_LEASE: u8 = 51;
const OPT_MSG_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_PARAM_LIST: u8 = 55;
const OPT_HOSTNAME: u8 = 12;
const OPT_MAX_MSG: u8 = 57;
const OPT_VENDOR: u8 = 60;
const OPT_CLIENT_ID: u8 = 61;
const OPT_END: u8 = 255;

pub const DISCOVER: u8 = 1;
pub const OFFER: u8 = 2;
pub const REQUEST: u8 = 3;
pub const ACK: u8 = 5;
pub const NAK: u8 = 6;

#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub msg_type: u8,
    pub yiaddr: Ipv4Addr,
    pub server_id: Option<Ipv4Addr>,
    pub subnet: Option<Ipv4Addr>,
    pub router: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub lease_secs: Option<u32>,
}

fn header(xid: u32, mac: [u8; 6]) -> Vec<u8> {
    let mut p = vec![0u8; 236];
    p[0] = 1; // BOOTREQUEST
    p[1] = 1; // ethernet
    p[2] = 6; // hlen
    p[4..8].copy_from_slice(&xid.to_be_bytes());
    p[10] = 0x80; // flags: ask for broadcast reply (we have no IP yet)
    p[28..34].copy_from_slice(&mac);
    p.extend_from_slice(&MAGIC);
    p
}

fn finish(mut p: Vec<u8>) -> Vec<u8> {
    p.push(OPT_END);
    // Some servers drop BOOTP packets shorter than 300 bytes.
    if p.len() < 300 {
        p.resize(300, 0);
    }
    p
}

/// The identity/capability options mainstream clients (dhclient, udhcpc) always send. Apple's
/// bootpd is picky about minimal clients, so we look like a normal one.
fn client_options(p: &mut Vec<u8>, mac: [u8; 6]) {
    p.extend_from_slice(&[OPT_CLIENT_ID, 7, 1]); // type 1 = ethernet
    p.extend_from_slice(&mac);
    p.extend_from_slice(&[OPT_MAX_MSG, 2, 0x05, 0xdc]); // 1500
    p.extend_from_slice(&[OPT_HOSTNAME, 6]);
    p.extend_from_slice(b"moniqi");
    p.extend_from_slice(&[OPT_VENDOR, 6]);
    p.extend_from_slice(b"moniqi");
}

pub fn discover(xid: u32, mac: [u8; 6]) -> Vec<u8> {
    let mut p = header(xid, mac);
    p.extend_from_slice(&[OPT_MSG_TYPE, 1, DISCOVER]);
    client_options(&mut p, mac);
    p.extend_from_slice(&[OPT_PARAM_LIST, 4, OPT_SUBNET, OPT_ROUTER, OPT_DNS, 15]);
    finish(p)
}

pub fn request(xid: u32, mac: [u8; 6], ip: Ipv4Addr, server: Ipv4Addr) -> Vec<u8> {
    let mut p = header(xid, mac);
    p.extend_from_slice(&[OPT_MSG_TYPE, 1, REQUEST]);
    p.extend_from_slice(&[OPT_REQ_IP, 4]);
    p.extend_from_slice(&ip.octets());
    p.extend_from_slice(&[OPT_SERVER_ID, 4]);
    p.extend_from_slice(&server.octets());
    client_options(&mut p, mac);
    p.extend_from_slice(&[OPT_PARAM_LIST, 4, OPT_SUBNET, OPT_ROUTER, OPT_DNS, 15]);
    finish(p)
}

/// Extract the UDP payload from a raw IPv4 packet (as delivered by an `AF_PACKET` datagram
/// socket) if it is addressed to UDP `dst_port`. Fragments are rejected.
pub fn udp_payload(ip: &[u8], dst_port: u16) -> Option<&[u8]> {
    let ihl = (*ip.first()? & 0x0f) as usize * 4;
    if ip[0] >> 4 != 4 || ihl < 20 || ip.len() < ihl + 8 || ip[9] != 17 {
        return None;
    }
    if u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff != 0 {
        return None; // fragment
    }
    let udp = &ip[ihl..];
    if u16::from_be_bytes([udp[2], udp[3]]) != dst_port {
        return None;
    }
    let len = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    udp.get(8..len.min(udp.len()))
}

fn ip4(b: &[u8]) -> Option<Ipv4Addr> {
    (b.len() >= 4).then(|| Ipv4Addr::new(b[0], b[1], b[2], b[3]))
}

/// Parse a server reply. Returns `None` for anything malformed or not matching `xid`.
pub fn parse_reply(buf: &[u8], xid: u32) -> Option<Reply> {
    if buf.len() < 240 || buf[0] != 2 || buf[4..8] != xid.to_be_bytes() || buf[236..240] != MAGIC {
        return None;
    }
    let mut r = Reply {
        msg_type: 0,
        yiaddr: ip4(&buf[16..20])?,
        server_id: None,
        subnet: None,
        router: None,
        dns: Vec::new(),
        lease_secs: None,
    };
    let mut i = 240;
    while i < buf.len() {
        match buf[i] {
            0 => i += 1, // pad
            OPT_END => break,
            code => {
                let len = *buf.get(i + 1)? as usize;
                let v = buf.get(i + 2..i + 2 + len)?;
                match code {
                    OPT_MSG_TYPE => r.msg_type = *v.first()?,
                    OPT_SERVER_ID => r.server_id = ip4(v),
                    OPT_SUBNET => r.subnet = ip4(v),
                    OPT_ROUTER => r.router = ip4(v),
                    OPT_DNS => r.dns = v.chunks_exact(4).filter_map(ip4).collect(),
                    OPT_LEASE if len == 4 => r.lease_secs = Some(u32::from_be_bytes(v.try_into().ok()?)),
                    _ => {}
                }
                i += 2 + len;
            }
        }
    }
    (r.msg_type != 0).then_some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

    fn server_reply(xid: u32, msg: u8) -> Vec<u8> {
        let mut p = vec![0u8; 236];
        p[0] = 2;
        p[4..8].copy_from_slice(&xid.to_be_bytes());
        p[16..20].copy_from_slice(&[192, 168, 64, 7]);
        p.extend_from_slice(&MAGIC);
        p.extend_from_slice(&[OPT_MSG_TYPE, 1, msg]);
        p.extend_from_slice(&[OPT_SERVER_ID, 4, 192, 168, 64, 1]);
        p.extend_from_slice(&[OPT_SUBNET, 4, 255, 255, 255, 0]);
        p.extend_from_slice(&[OPT_ROUTER, 4, 192, 168, 64, 1]);
        p.extend_from_slice(&[OPT_DNS, 8, 192, 168, 64, 1, 8, 8, 8, 8]);
        p.extend_from_slice(&[OPT_LEASE, 4, 0, 0, 0x0e, 0x10]);
        p.push(OPT_END);
        p
    }

    fn ip_udp(dst_port: u16, payload: &[u8], proto: u8, ihl_words: u8) -> Vec<u8> {
        let ihl = ihl_words as usize * 4;
        let mut p = vec![0u8; ihl];
        p[0] = 0x40 | ihl_words;
        p[9] = proto;
        let total = (ihl + 8 + payload.len()) as u16;
        p[2..4].copy_from_slice(&total.to_be_bytes());
        let mut udp = vec![0u8; 8];
        udp[0..2].copy_from_slice(&67u16.to_be_bytes());
        udp[2..4].copy_from_slice(&dst_port.to_be_bytes());
        udp[4..6].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        p.extend_from_slice(&udp);
        p.extend_from_slice(payload);
        p
    }

    #[test]
    fn extracts_udp_payload() {
        let pkt = ip_udp(68, b"hello", 17, 5);
        assert_eq!(udp_payload(&pkt, 68), Some(&b"hello"[..]));
        // IP options (ihl = 6 words)
        assert_eq!(udp_payload(&ip_udp(68, b"hi", 17, 6), 68), Some(&b"hi"[..]));
        // wrong port, wrong protocol, truncated, fragment
        assert!(udp_payload(&pkt, 67).is_none());
        assert!(udp_payload(&ip_udp(68, b"x", 6, 5), 68).is_none());
        assert!(udp_payload(&pkt[..10], 68).is_none());
        let mut frag = pkt.clone();
        frag[6] = 0x20; // more-fragments
        assert!(udp_payload(&frag, 68).is_none());
    }

    #[test]
    fn discover_layout() {
        let p = discover(0xdead_beef, MAC);
        assert!(p.len() >= 300);
        assert_eq!(&p[4..8], &0xdead_beefu32.to_be_bytes());
        assert_eq!(&p[28..34], &MAC);
        assert_eq!(&p[236..240], &MAGIC);
        assert_eq!(&p[240..243], &[OPT_MSG_TYPE, 1, DISCOVER]);
    }

    #[test]
    fn request_carries_ip_and_server() {
        let p = request(1, MAC, Ipv4Addr::new(192, 168, 64, 7), Ipv4Addr::new(192, 168, 64, 1));
        let s = &p[240..];
        assert!(s.windows(6).any(|w| w == [OPT_REQ_IP, 4, 192, 168, 64, 7]));
        assert!(s.windows(6).any(|w| w == [OPT_SERVER_ID, 4, 192, 168, 64, 1]));
    }

    #[test]
    fn parses_offer() {
        let r = parse_reply(&server_reply(42, OFFER), 42).unwrap();
        assert_eq!(r.msg_type, OFFER);
        assert_eq!(r.yiaddr, Ipv4Addr::new(192, 168, 64, 7));
        assert_eq!(r.server_id, Some(Ipv4Addr::new(192, 168, 64, 1)));
        assert_eq!(r.subnet, Some(Ipv4Addr::new(255, 255, 255, 0)));
        assert_eq!(r.router, Some(Ipv4Addr::new(192, 168, 64, 1)));
        assert_eq!(r.dns, vec![Ipv4Addr::new(192, 168, 64, 1), Ipv4Addr::new(8, 8, 8, 8)]);
        assert_eq!(r.lease_secs, Some(3600));
    }

    #[test]
    fn rejects_wrong_xid_truncated_and_bad_cookie() {
        let good = server_reply(42, ACK);
        assert!(parse_reply(&good, 43).is_none());
        assert!(parse_reply(&good[..100], 42).is_none());
        let mut bad = good.clone();
        bad[236] = 0;
        assert!(parse_reply(&bad, 42).is_none());
        // option claims more bytes than remain
        let mut trunc = good[..240].to_vec();
        trunc.extend_from_slice(&[OPT_DNS, 200, 1, 2]);
        assert!(parse_reply(&trunc, 42).is_none());
    }
}
