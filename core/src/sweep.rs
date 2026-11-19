//! Finding the head unit when it does not answer at the configured address.
//!
//! The configured address is the head unit's link-local one, which is what it
//! falls back to over the ENET cable, where nothing serves DHCP. It is not the
//! only address it has: the car's modules are DHCP clients, and a Wi-Fi ENET
//! adapter serves them, so behind one the head unit also holds an address on
//! the adapter's network. This computer holds one there too and none on
//! `169.254`, so the built-in address is unreachable and the head unit has to
//! be found instead.
//!
//! Seen in captures on the car (2026-11-15): every module asks for an address
//! with a host name naming it, `DIAGADR62BMWMAC001CD7A4F1CA` for the head unit
//! (`10` is the gateway, `63` the module at `169.254.199.99`), and the gateway
//! asks for the address an adapter gave it earlier, holding it until a server
//! answers or the bus sleeps.
//!
//! Only networks where a vehicle gateway answered are searched, so an ordinary
//! home or office network is never touched, and the gateways themselves are
//! left out: the car's password must not be offered to a module that is not
//! the head unit. Whatever answers is checked with `uname -s` as always, so a
//! wrong guess cannot be installed onto.

use crate::discovery;
use std::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

/// Host bits of a network small enough to search: at most 256 addresses. The
/// link-local 169.254.0.0/16 is far larger, and the only address of interest
/// in it is the configured one, already tried.
const HOST_BITS: std::ops::RangeInclusive<u32> = 2..=8;
/// Connections open at once.
const AT_ONCE: usize = 64;
const PER_HOST: Duration = Duration::from_millis(400);
/// A bound on the whole search, whatever the interfaces report.
const MAX_CANDIDATES: usize = 512;

/// Every host address of the local networks on which a vehicle gateway
/// answered, without this computer's own addresses, the gateways and `skip`.
pub fn candidates(
    locals: &[(Ipv4Addr, Ipv4Addr)],
    gateways: &[Ipv4Addr],
    skip: Option<Ipv4Addr>,
) -> Vec<Ipv4Addr> {
    let mine: Vec<Ipv4Addr> = locals.iter().map(|(ip, _)| *ip).collect();
    let mut out = Vec::new();
    for &(ip, netmask) in locals {
        let (network, broadcast) = (
            ip.to_bits() & netmask.to_bits(),
            ip.to_bits() | !netmask.to_bits(),
        );
        let carries_gateway = gateways
            .iter()
            .any(|gateway| gateway.to_bits() & netmask.to_bits() == network);
        if ip.is_loopback()
            || ip.is_unspecified()
            || !HOST_BITS.contains(&netmask.to_bits().count_zeros())
            || !carries_gateway
        {
            continue;
        }
        for bits in (network + 1)..broadcast {
            let candidate = Ipv4Addr::from_bits(bits);
            if mine.contains(&candidate)
                || gateways.contains(&candidate)
                || skip == Some(candidate)
                || out.contains(&candidate)
            {
                continue;
            }
            out.push(candidate);
            if out.len() == MAX_CANDIDATES {
                return out;
            }
        }
    }
    out
}

/// The candidates answering on `port`, in address order. Each connection is
/// bounded, and only a few are open at a time.
pub async fn scan(port: u16, candidates: &[Ipv4Addr]) -> Vec<Ipv4Addr> {
    let mut found = Vec::new();
    for chunk in candidates.chunks(AT_ONCE) {
        let mut set = tokio::task::JoinSet::new();
        for &ip in chunk {
            set.spawn(async move {
                let target = SocketAddr::from((ip, port));
                match tokio::time::timeout(PER_HOST, crate::net::car_tcp(target)).await {
                    Ok(Ok(_)) => Some(ip),
                    _ => None,
                }
            });
        }
        while let Some(result) = set.join_next().await {
            if let Ok(Some(ip)) = result {
                found.push(ip);
            }
        }
    }
    found.sort();
    found
}

/// Scans again every `interval` until something answers or `patience` has
/// passed, and returns what answered; a single scan when `patience` is zero.
/// Just after SSH is enabled, the head unit can take a while to accept.
pub async fn wait(
    port: u16,
    candidates: &[Ipv4Addr],
    patience: Duration,
    interval: Duration,
) -> Vec<Ipv4Addr> {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        let found = scan(port, candidates).await;
        if !found.is_empty() || tokio::time::Instant::now() + interval > deadline {
            return found;
        }
        tokio::time::sleep(interval).await;
    }
}

/// The candidates on the networks the car is on, as this device sees them.
/// `gateways` comes from the HSFZ search, which also says which networks
/// those are.
pub fn around(gateways: &[Ipv4Addr], skip: Option<Ipv4Addr>) -> Vec<Ipv4Addr> {
    candidates(&discovery::local_addresses(), gateways, skip)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADAPTER: (Ipv4Addr, Ipv4Addr) = (
        Ipv4Addr::new(192, 168, 16, 120),
        Ipv4Addr::new(255, 255, 255, 0),
    );
    const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 16, 105);

    #[test]
    fn only_a_network_a_gateway_answered_on_is_searched() {
        let home = (
            Ipv4Addr::new(192, 168, 1, 50),
            Ipv4Addr::new(255, 255, 255, 0),
        );
        let locals = [
            ADAPTER,
            home,
            (Ipv4Addr::LOCALHOST, Ipv4Addr::new(255, 0, 0, 0)),
        ];
        let found = candidates(&locals, &[GATEWAY], Some(Ipv4Addr::new(192, 168, 16, 9)));
        // The adapter's 254 hosts, less this computer, the gateway and the skip.
        assert_eq!(found.len(), 251);
        assert!(found.iter().all(|ip| ip.octets()[..3] == [192, 168, 16]));
        for absent in [
            Ipv4Addr::new(192, 168, 16, 0),   // the network address
            Ipv4Addr::new(192, 168, 16, 255), // its broadcast
            ADAPTER.0,                        // this computer
            GATEWAY,                          // not the head unit; never offered the password
            Ipv4Addr::new(192, 168, 16, 9),   // already tried
            Ipv4Addr::new(192, 168, 1, 50),   // the home network, never searched
            Ipv4Addr::new(192, 168, 1, 7),
        ] {
            assert!(!found.contains(&absent), "{absent}");
        }
        // No gateway answered: nothing is searched at all.
        assert!(candidates(&locals, &[], None).is_empty());
    }

    #[test]
    fn the_link_local_network_is_too_large_to_search() {
        let cable = (
            Ipv4Addr::new(169, 254, 1, 10),
            Ipv4Addr::new(255, 255, 0, 0),
        );
        assert!(candidates(&[cable], &[Ipv4Addr::new(169, 254, 24, 4)], None).is_empty());
        // A /31 has no hosts, and 0.0.0.0 as a netmask none to search.
        for netmask in [Ipv4Addr::new(255, 255, 255, 254), Ipv4Addr::UNSPECIFIED] {
            let local = (Ipv4Addr::new(10, 0, 0, 5), netmask);
            assert!(candidates(&[local], &[Ipv4Addr::new(10, 0, 0, 6)], None).is_empty());
        }
    }

    #[tokio::test]
    async fn the_search_reports_only_what_answers() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        // 127.0.0.2 is on the loopback network but nothing listens there.
        let found = scan(port, &[Ipv4Addr::LOCALHOST, Ipv4Addr::new(127, 0, 0, 2)]).await;
        assert_eq!(found, [Ipv4Addr::LOCALHOST]);
        drop(listener);
        assert!(scan(port, &[Ipv4Addr::LOCALHOST]).await.is_empty());
    }

    #[tokio::test]
    async fn the_search_waits_for_a_host_that_starts_accepting_late() {
        let late = Ipv4Addr::new(127, 0, 0, 3);
        let port = std::net::TcpListener::bind((late, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let interval = Duration::from_millis(200);
        // Nothing yet, and no patience: one scan, nothing found.
        assert!(
            wait(port, &[late], Duration::ZERO, interval)
                .await
                .is_empty()
        );
        let started = tokio::time::Instant::now();
        let opener = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(700)).await;
            tokio::net::TcpListener::bind((late, port)).await.unwrap()
        });
        let found = wait(port, &[late], Duration::from_secs(10), interval).await;
        assert_eq!(found, [late]);
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(opener.await.unwrap());
        // Patience runs out when nothing ever answers.
        let started = tokio::time::Instant::now();
        assert!(
            wait(port, &[late], Duration::from_millis(600), interval)
                .await
                .is_empty()
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
