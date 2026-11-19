use std::{
    collections::BTreeMap,
    io,
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

const REQUEST: [u8; 6] = [0, 0, 0, 0, 0, 0x11];
const PORT: u16 = 6811;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gateway {
    pub ip: Ipv4Addr,
    pub address: u8,
    pub mac: String,
    pub vin: String,
}

pub fn parse_identification(data: &[u8], ip: Ipv4Addr) -> Option<Gateway> {
    let size = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
    if data.get(4..6)? != [0, 0x11] || size != data.len().checked_sub(6)? {
        return None;
    }
    let bytes = data.get(6..)?;
    fn field<'a>(bytes: &'a [u8], marker: &[u8], len: usize) -> Option<&'a [u8]> {
        let i = bytes
            .windows(marker.len())
            .position(|part| part == marker)?
            + marker.len();
        bytes.get(i..i + len)
    }
    let address = std::str::from_utf8(field(bytes, b"DIAGADR", 2)?).ok()?;
    let mac = std::str::from_utf8(field(bytes, b"BMWMAC", 12)?).ok()?;
    let vin = std::str::from_utf8(field(bytes, b"BMWVIN", 17)?).ok()?;
    if !mac.bytes().all(|b| b.is_ascii_hexdigit())
        || !vin.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(Gateway {
        ip,
        address: u8::from_str_radix(address, 16).ok()?,
        mac: mac.to_ascii_uppercase(),
        vin: vin.to_ascii_uppercase(),
    })
}

/// What a search found, and the local addresses it asked from.
pub struct Search {
    pub gateways: Vec<Gateway>,
    pub sources: Vec<Ipv4Addr>,
}

/// More local addresses than a computer plugged into one car has.
const MAX_SOURCES: usize = 16;
const LIMITED: Ipv4Addr = Ipv4Addr::BROADCAST;
const LINK_LOCAL: Ipv4Addr = Ipv4Addr::new(169, 254, 255, 255);

/// Asks from every local IPv4 address, not only from the one the system
/// picks: Windows sends a 255.255.255.255 broadcast out of one interface, and
/// a car that took an address from a Wi-Fi ENET adapter's DHCP server keeps
/// it after the adapter is unplugged, answering only on that subnet.
pub fn discover(timeout: Duration) -> io::Result<Search> {
    let plan = plan(&local_addresses());
    let sources = plan
        .iter()
        .map(|(ip, _)| *ip)
        .filter(|ip| !ip.is_unspecified())
        .collect();
    Ok(Search {
        gateways: discover_at(timeout, &plan)?,
        sources,
    })
}

/// Up interfaces' IPv4 addresses and netmasks, link-local ones included, or
/// the car network's own on a phone (see `net`).
pub use crate::net::local_addresses;

/// Whether `host` is a link-local address this computer cannot reach, having
/// no link-local address itself. The ENET cable leaves both ends on
/// `169.254`; a Wi-Fi ENET adapter gives this computer a DHCP address instead,
/// and the head unit keeps only its link-local one (seen on the car).
pub fn needs_link_local(locals: &[(Ipv4Addr, Ipv4Addr)], host: &str) -> bool {
    host.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_link_local())
        && !locals.iter().any(|(ip, _)| ip.is_link_local())
}

/// Where to send from and to. The first entry is the search as it was before
/// per-address sources (the system picks the source address); each local
/// address then sends to its own subnet's broadcast and the limited broadcast.
fn plan(locals: &[(Ipv4Addr, Ipv4Addr)]) -> Vec<(Ipv4Addr, Vec<SocketAddr>)> {
    let to = |ip: Ipv4Addr| SocketAddr::from((ip, PORT));
    let mut plan = vec![(Ipv4Addr::UNSPECIFIED, vec![to(LINK_LOCAL), to(LIMITED)])];
    for &(ip, netmask) in locals {
        if plan.len() > MAX_SOURCES {
            break;
        }
        if ip.is_loopback()
            || ip.is_unspecified()
            || ip.is_multicast()
            || ip.is_broadcast()
            || plan.iter().any(|(source, _)| *source == ip)
        {
            continue;
        }
        let mut targets = vec![to(LIMITED)];
        // A /31 or /32 has no broadcast address of its own.
        if netmask.to_bits().count_zeros() >= 2 {
            let own = Ipv4Addr::from_bits(ip.to_bits() | !netmask.to_bits());
            if own != LIMITED {
                targets.insert(0, to(own));
            }
        }
        plan.push((ip, targets));
    }
    plan
}

fn discover_at(
    timeout: Duration,
    plan: &[(Ipv4Addr, Vec<SocketAddr>)],
) -> io::Result<Vec<Gateway>> {
    let mut sockets = Vec::new();
    let mut last_error = None;
    for (local, targets) in plan {
        let sock = match crate::net::car_udp(*local) {
            Ok(sock) => sock,
            Err(e) => {
                last_error = Some(e);
                continue;
            }
        };
        let mut sent = false;
        for target in targets {
            match sock.send_to(&REQUEST, target) {
                Ok(_) => sent = true,
                Err(e) => last_error = Some(e),
            }
        }
        if sent {
            sockets.push(sock);
        }
    }
    if sockets.is_empty() {
        return Err(last_error.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "no discovery targets")
        }));
    }
    let deadline = Instant::now() + timeout;
    let answers = std::thread::scope(|scope| {
        let workers: Vec<_> = sockets
            .iter()
            .map(|sock| scope.spawn(move || receive(sock, deadline)))
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap_or_default())
            .collect::<Vec<_>>()
    });
    // One car answering on two subnets is one gateway: keep its first answer.
    let mut found = BTreeMap::new();
    for gateway in answers {
        found.entry(gateway.vin.clone()).or_insert(gateway);
    }
    Ok(found.into_values().collect())
}

/// Answers to one socket until the deadline. An error ends this socket's
/// part of the search only; Windows reports an ICMP port unreachable for an
/// earlier send as a reset on the next receive, which is skipped.
fn receive(sock: &UdpSocket, deadline: Instant) -> Vec<Gateway> {
    let mut found = Vec::new();
    let mut buf = [0; 65535];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || sock.set_read_timeout(Some(remaining)).is_err() {
            break;
        }
        match sock.recv_from(&mut buf) {
            Ok((n, SocketAddr::V4(from))) => {
                if let Some(gateway) = parse_identification(&buf[..n], *from.ip()) {
                    found.push(gateway);
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Err(_) => break,
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    fn response() -> Vec<u8> {
        let payload = b"DIAGADR10BMWMACAABBCCDDEEFFBMWVINWBA12345678901234";
        let mut data = (payload.len() as u32).to_be_bytes().to_vec();
        data.extend([0, 0x11]);
        data.extend(payload);
        data
    }
    #[test]
    fn parses_and_checks_length() {
        let mut bytes = response();
        let g = parse_identification(&bytes, Ipv4Addr::LOCALHOST).unwrap();
        assert_eq!(
            (g.address, g.mac.as_str(), g.vin.as_str()),
            (0x10, "AABBCCDDEEFF", "WBA12345678901234")
        );
        bytes.pop();
        assert!(parse_identification(&bytes, Ipv4Addr::LOCALHOST).is_none());
    }
    #[test]
    fn discovers_and_deduplicates_simulated_gateway() {
        let listener = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let target = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let mut bytes = [0; 16];
            let (n, from) = listener.recv_from(&mut bytes).unwrap();
            assert_eq!(&bytes[..n], &REQUEST);
            listener.send_to(&response(), from).unwrap();
            listener.send_to(&response(), from).unwrap();
        });
        let gateways = discover_at(
            Duration::from_millis(250),
            &[(Ipv4Addr::UNSPECIFIED, vec![target])],
        )
        .unwrap();
        worker.join().unwrap();
        assert_eq!(gateways.len(), 1);
        assert_eq!(gateways[0].ip, Ipv4Addr::LOCALHOST);
    }
    #[test]
    fn a_link_local_target_needs_a_link_local_address_here() {
        let cable = (
            Ipv4Addr::new(169, 254, 1, 10),
            Ipv4Addr::new(255, 255, 0, 0),
        );
        let adapter = (
            Ipv4Addr::new(192, 168, 16, 120),
            Ipv4Addr::new(255, 255, 255, 0),
        );
        // Over the cable this computer has a link-local address: nothing to say.
        assert!(!needs_link_local(&[cable], "169.254.199.119"));
        assert!(!needs_link_local(&[adapter, cable], "169.254.199.119"));
        // Behind the adapter it has none, which is why the head unit is unreachable.
        assert!(needs_link_local(&[adapter], "169.254.199.119"));
        assert!(needs_link_local(&[], "169.254.199.119"));
        // A routable address is reached by ordinary routing, and a name says nothing.
        assert!(!needs_link_local(&[adapter], "192.168.16.50"));
        assert!(!needs_link_local(&[adapter], "headunit.local"));
    }

    #[test]
    fn every_local_address_asks_its_own_subnet() {
        let to = |a, b, c, d| SocketAddr::from(([a, b, c, d], PORT));
        let plan = plan(&[
            (
                Ipv4Addr::new(169, 254, 1, 10),
                Ipv4Addr::new(255, 255, 0, 0),
            ),
            (
                Ipv4Addr::new(192, 168, 16, 120),
                Ipv4Addr::new(255, 255, 255, 0),
            ),
            (
                Ipv4Addr::new(192, 168, 16, 120),
                Ipv4Addr::new(255, 255, 255, 0),
            ),
            (Ipv4Addr::LOCALHOST, Ipv4Addr::new(255, 0, 0, 0)),
            (Ipv4Addr::new(10, 0, 0, 5), Ipv4Addr::BROADCAST),
        ]);
        assert_eq!(
            plan,
            [
                (
                    Ipv4Addr::UNSPECIFIED,
                    vec![to(169, 254, 255, 255), to(255, 255, 255, 255)]
                ),
                (
                    Ipv4Addr::new(169, 254, 1, 10),
                    vec![to(169, 254, 255, 255), to(255, 255, 255, 255)]
                ),
                (
                    Ipv4Addr::new(192, 168, 16, 120),
                    vec![to(192, 168, 16, 255), to(255, 255, 255, 255)]
                ),
                (Ipv4Addr::new(10, 0, 0, 5), vec![to(255, 255, 255, 255)]),
            ]
        );
        let many: Vec<_> = (1..=40)
            .map(|n| (Ipv4Addr::new(10, 1, 0, n), Ipv4Addr::BROADCAST))
            .collect();
        assert_eq!(super::plan(&many).len(), MAX_SOURCES + 1);
    }
    #[test]
    fn a_source_that_cannot_send_does_not_stop_the_others() {
        let listener = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let target = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let mut bytes = [0; 16];
            let (_, from) = listener.recv_from(&mut bytes).unwrap();
            // The same car answering from two addresses is one gateway.
            listener.send_to(&response(), from).unwrap();
            listener.send_to(&response(), from).unwrap();
        });
        // 192.0.2.1 (documentation range) is not a local address: binding fails.
        let gateways = discover_at(
            Duration::from_millis(250),
            &[
                (Ipv4Addr::new(192, 0, 2, 1), vec![target]),
                (Ipv4Addr::LOCALHOST, vec![target]),
            ],
        )
        .unwrap();
        worker.join().unwrap();
        assert_eq!(gateways.len(), 1);
        assert!(
            discover_at(
                Duration::from_millis(50),
                &[(Ipv4Addr::new(192, 0, 2, 1), vec![target])]
            )
            .is_err()
        );
    }
}
