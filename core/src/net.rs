//! Every socket the installer opens, to the car or to the activation service.
//!
//! A computer leaves [`Networks`] unset: the car is on the ENET cable or the
//! adapter's Wi-Fi, the activation service on whatever has internet, and the
//! system's routes pick. A phone cannot rely on that. A Wi-Fi ENET adapter
//! gives no internet, so Android may keep mobile data as its default network
//! and send car traffic there, or make the adapter's Wi-Fi the default and
//! lose the internet. The phone app therefore passes both networks' handles
//! (`android.net.Network.getNetworkHandle()`): car sockets are tied to the
//! adapter's Wi-Fi and the activation service's to mobile data, with its
//! name resolved there too.

use std::{
    io,
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    sync::RwLock,
};
use tokio::net::{TcpSocket, TcpStream};

/// The networks to use on a phone; everything unset on a computer.
#[derive(Clone, Debug, Default)]
pub struct Networks {
    /// The network the car is on (the Wi-Fi ENET adapter's Wi-Fi).
    pub car: Option<u64>,
    /// That network's own IPv4 addresses and netmasks. When set, they replace
    /// the interface list for the gateway search and the head-unit search,
    /// which then never look at mobile data's addresses.
    pub car_addresses: Option<Vec<(Ipv4Addr, Ipv4Addr)>>,
    /// A network with internet (mobile data), for the activation service.
    pub internet: Option<u64>,
}

static NETWORKS: RwLock<Networks> = RwLock::new(Networks {
    car: None,
    car_addresses: None,
    internet: None,
});

/// Sets the networks for every socket opened from now on.
pub fn configure(networks: Networks) {
    *NETWORKS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = networks;
}

/// Whether a phone configured the car's network (the phone app always does).
pub fn is_phone() -> bool {
    networks().car.is_some()
}

fn networks() -> Networks {
    NETWORKS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// The local IPv4 addresses and netmasks the car can be reached from: the car
/// network's when configured, else every up interface's, link-local included.
pub fn local_addresses() -> Vec<(Ipv4Addr, Ipv4Addr)> {
    if let Some(addresses) = networks().car_addresses {
        return addresses;
    }
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|interface| interface.oper_status != if_addrs::IfOperStatus::Down)
        .filter_map(|interface| match interface.addr {
            if_addrs::IfAddr::V4(v4) => Some((v4.ip, v4.netmask)),
            if_addrs::IfAddr::V6(_) => None,
        })
        .collect()
}

/// A TCP connection to the car. Callers bound the wait.
pub async fn car_tcp(target: SocketAddr) -> io::Result<TcpStream> {
    tcp(target, networks().car).await
}

/// A UDP socket from `local` (unspecified: the system picks) on the car's
/// network, with broadcasts allowed.
pub fn car_udp(local: Ipv4Addr) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind((local, 0))?;
    socket.set_broadcast(true)?;
    tie(&socket, networks().car)?;
    Ok(socket)
}

/// A TCP connection to `host:port` on the internet, its name resolved on the
/// same network. Callers bound the wait.
pub async fn internet_tcp(host: &str, port: u16) -> io::Result<TcpStream> {
    let network = networks().internet;
    let addresses = resolve(host, port, network).await?;
    let mut last = io::Error::new(io::ErrorKind::NotFound, "the name has no address");
    for address in addresses {
        match tcp(address, network).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last = error,
        }
    }
    Err(last)
}

async fn tcp(target: SocketAddr, network: Option<u64>) -> io::Result<TcpStream> {
    let socket = if target.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    tie(&socket, network)?;
    socket.connect(target).await
}

#[cfg(not(target_os = "android"))]
async fn resolve(host: &str, port: u16, _network: Option<u64>) -> io::Result<Vec<SocketAddr>> {
    Ok(tokio::net::lookup_host((host, port)).await?.collect())
}

#[cfg(not(target_os = "android"))]
fn tie<S>(_socket: &S, _network: Option<u64>) -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "android")]
async fn resolve(host: &str, port: u16, network: Option<u64>) -> io::Result<Vec<SocketAddr>> {
    let Some(network) = network else {
        return Ok(tokio::net::lookup_host((host, port)).await?.collect());
    };
    let host = host.to_owned();
    tokio::task::spawn_blocking(move || android::resolve(network, &host, port))
        .await
        .map_err(io::Error::other)?
}

#[cfg(target_os = "android")]
fn tie<S: std::os::fd::AsRawFd>(socket: &S, network: Option<u64>) -> io::Result<()> {
    match network {
        Some(network) => android::tie(network, socket.as_raw_fd()),
        None => Ok(()),
    }
}

/// libandroid's network calls, in the NDK since API 23.
#[cfg(target_os = "android")]
mod android {
    use std::{
        ffi::{CStr, CString, c_char, c_int},
        io,
        net::{IpAddr, SocketAddr},
        ptr,
    };

    #[link(name = "android")]
    unsafe extern "C" {
        fn android_setsocknetwork(network: u64, fd: c_int) -> c_int;
        fn android_getaddrinfofornetwork(
            network: u64,
            node: *const c_char,
            service: *const c_char,
            hints: *const libc::addrinfo,
            result: *mut *mut libc::addrinfo,
        ) -> c_int;
    }

    pub fn tie(network: u64, fd: c_int) -> io::Result<()> {
        // SAFETY: `fd` is an open socket owned by the caller for this call.
        if unsafe { android_setsocknetwork(network, fd) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn resolve(network: u64, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        let node = CString::new(host).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: a zeroed addrinfo is the documented empty hints value.
        let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
        hints.ai_family = libc::AF_UNSPEC;
        hints.ai_socktype = libc::SOCK_STREAM;
        let mut list: *mut libc::addrinfo = ptr::null_mut();
        // SAFETY: valid C strings and hints; `list` receives an allocation
        // freed below with freeaddrinfo.
        let status = unsafe {
            android_getaddrinfofornetwork(network, node.as_ptr(), ptr::null(), &hints, &mut list)
        };
        if status != 0 {
            // SAFETY: gai_strerror returns a static string.
            let text = unsafe { CStr::from_ptr(libc::gai_strerror(status)) };
            return Err(io::Error::other(format!(
                "name lookup failed: {}",
                text.to_string_lossy()
            )));
        }
        let mut found = Vec::new();
        let mut entry = list;
        while !entry.is_null() {
            // SAFETY: entries of the list getaddrinfo returned, read before
            // it is freed; the address length is checked against its family.
            unsafe {
                let info = &*entry;
                let address = info.ai_addr;
                if !address.is_null() {
                    match (*address).sa_family as c_int {
                        libc::AF_INET
                            if info.ai_addrlen as usize >= size_of::<libc::sockaddr_in>() =>
                        {
                            let v4 = &*(address as *const libc::sockaddr_in);
                            let ip = u32::from_be(v4.sin_addr.s_addr);
                            found.push(SocketAddr::new(IpAddr::from(ip.to_be_bytes()), port));
                        }
                        libc::AF_INET6
                            if info.ai_addrlen as usize >= size_of::<libc::sockaddr_in6>() =>
                        {
                            let v6 = &*(address as *const libc::sockaddr_in6);
                            found.push(SocketAddr::new(IpAddr::from(v6.sin6_addr.s6_addr), port));
                        }
                        _ => {}
                    }
                }
                entry = info.ai_next;
            }
        }
        // SAFETY: `list` came from getaddrinfo and is freed once.
        unsafe { libc::freeaddrinfo(list) };
        // IPv4 first: mobile networks often have both.
        found.sort_by_key(|address| !address.is_ipv4());
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sockets_open_without_networks_configured() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        car_tcp(address).await.unwrap();
        internet_tcp("127.0.0.1", address.port()).await.unwrap();
        let udp = car_udp(Ipv4Addr::LOCALHOST).unwrap();
        assert!(udp.broadcast().unwrap());
    }
}
