//! A one-lease DHCP server: the guest always gets `GUEST_IP`, with the gateway as router/DNS.
use std::net::Ipv4Addr;

const MAGIC: [u8; 4] = [99, 130, 83, 99];

pub struct Lease {
    pub guest: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub mask: Ipv4Addr,
}

/// Build the reply (OFFER for DISCOVER, ACK for REQUEST) to a client message, or `None` if it
/// isn't one we answer.
pub fn reply(req: &[u8], l: &Lease) -> Option<Vec<u8>> {
    if req.len() < 240 || req[0] != 1 || req[236..240] != MAGIC {
        return None;
    }
    let mut msg_type = 0;
    let mut i = 240;
    while i < req.len() {
        match req[i] {
            0 => i += 1,
            255 => break,
            code => {
                let len = *req.get(i + 1)? as usize;
                let v = req.get(i + 2..i + 2 + len)?;
                if code == 53 {
                    msg_type = *v.first()?;
                }
                i += 2 + len;
            }
        }
    }
    let reply_type = match msg_type {
        1 => 2, // DISCOVER -> OFFER
        3 => 5, // REQUEST  -> ACK
        _ => return None,
    };
    let mut p = vec![0u8; 236];
    p[0] = 2; // BOOTREPLY
    p[1] = 1;
    p[2] = 6;
    p[4..8].copy_from_slice(&req[4..8]); // xid
    p[10..12].copy_from_slice(&req[10..12]); // flags
    p[16..20].copy_from_slice(&l.guest.octets()); // yiaddr
    p[20..24].copy_from_slice(&l.gateway.octets()); // siaddr
    p[28..44].copy_from_slice(&req[28..44]); // chaddr
    p.extend_from_slice(&MAGIC);
    p.extend_from_slice(&[53, 1, reply_type]);
    p.extend_from_slice(&[54, 4]);
    p.extend_from_slice(&l.gateway.octets());
    p.extend_from_slice(&[51, 4, 0, 1, 0x51, 0x80]); // 86400 s
    p.extend_from_slice(&[1, 4]);
    p.extend_from_slice(&l.mask.octets());
    p.extend_from_slice(&[3, 4]);
    p.extend_from_slice(&l.gateway.octets());
    p.extend_from_slice(&[6, 4]);
    p.extend_from_slice(&l.gateway.octets());
    p.extend_from_slice(&[26, 2, 0x05, 0xdc]); // MTU 1500
    p.push(255);
    if p.len() < 300 {
        p.resize(300, 0);
    }
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discover() -> Vec<u8> {
        let mut p = vec![0u8; 236];
        p[0] = 1;
        p[4..8].copy_from_slice(&[1, 2, 3, 4]);
        p[28..34].copy_from_slice(&[0xaa; 6]);
        p.extend_from_slice(&MAGIC);
        p.extend_from_slice(&[53, 1, 1, 255]);
        p
    }

    #[test]
    fn offers_the_fixed_lease() {
        let l = Lease { guest: Ipv4Addr::new(10, 0, 2, 15), gateway: Ipv4Addr::new(10, 0, 2, 2), mask: Ipv4Addr::new(255, 255, 255, 0) };
        let r = reply(&discover(), &l).unwrap();
        assert_eq!(&r[4..8], &[1, 2, 3, 4]);
        assert_eq!(&r[16..20], &[10, 0, 2, 15]);
        assert_eq!(&r[240..243], &[53, 1, 2]);
    }

    #[test]
    fn ignores_garbage() {
        let l = Lease { guest: Ipv4Addr::LOCALHOST, gateway: Ipv4Addr::LOCALHOST, mask: Ipv4Addr::LOCALHOST };
        assert!(reply(&[0u8; 10], &l).is_none());
    }
}
