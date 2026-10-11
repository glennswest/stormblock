//! Which network carries what (#85).
//!
//! A node may sit on two networks: a storage VLAN for the data path (NVMe/TCP,
//! iSCSI, the ring's remote legs) and the ordinary LAN for the control channel
//! (the management API, the discovery beacon, health). Each is a different
//! interface — a VLAN sub-interface (`eth1.100`) or a second port — with its
//! own subnet, MTU and routing, and the storage one usually has no gateway.
//!
//! The engine is told the interface (`[storage] interface`, `[management]
//! interface`), never the routing table's guess. This module reads what that
//! interface has: its address (getifaddrs), whether its link is up and its MTU
//! (sysfs). It also keeps the one process-wide fact every data-path listener
//! needs: the address a wildcard bind (`0.0.0.0:4420`) is narrowed to, so a
//! target never listens on the LAN when the node has a storage network.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::Serialize;

/// Where `/sys/class/net` is; tests point it at a tree of their own.
fn sysfs_net() -> PathBuf {
    std::env::var_os("STORMBLOCK_SYSFS_NET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/sys/class/net"))
}

/// Every address on `interface`, in the order the kernel lists them.
/// Empty when it has none or does not exist.
pub fn addresses(interface: &str) -> Vec<IpAddr> {
    addresses_with_prefix(interface).into_iter().map(|(a, _)| a).collect()
}

/// Every address on `interface` with its prefix length (#90).
#[cfg(unix)]
pub fn addresses_with_prefix(interface: &str) -> Vec<(IpAddr, u8)> {
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list we free below; every node
    // is read only while the list is alive.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return out;
    }
    let mut cur = head;
    while !cur.is_null() {
        // SAFETY: `cur` is a node of the list getifaddrs returned.
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_addr.is_null() || ifa.ifa_name.is_null() {
            continue;
        }
        // SAFETY: a NUL-terminated name owned by the list.
        let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) };
        if name.to_bytes() != interface.as_bytes() {
            continue;
        }
        // SAFETY: the family says which sockaddr this is.
        match unsafe { (*ifa.ifa_addr).sa_family } as i32 {
            libc::AF_INET => {
                let sa = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
                let prefix = if ifa.ifa_netmask.is_null() {
                    32
                } else {
                    // SAFETY: an AF_INET address's netmask is a sockaddr_in.
                    let m = unsafe { &*(ifa.ifa_netmask as *const libc::sockaddr_in) };
                    u32::from_be(m.sin_addr.s_addr).count_ones() as u8
                };
                out.push((IpAddr::V4(Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr))), prefix));
            }
            libc::AF_INET6 => {
                let sa = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in6) };
                let prefix = if ifa.ifa_netmask.is_null() {
                    128
                } else {
                    // SAFETY: an AF_INET6 address's netmask is a sockaddr_in6.
                    let m = unsafe { &*(ifa.ifa_netmask as *const libc::sockaddr_in6) };
                    m.sin6_addr.s6_addr.iter().map(|b| b.count_ones()).sum::<u32>() as u8
                };
                out.push((IpAddr::V6(Ipv6Addr::from(sa.sin6_addr.s6_addr)), prefix));
            }
            _ => {}
        }
    }
    // SAFETY: the list getifaddrs returned, freed once.
    unsafe { libc::freeifaddrs(head) };
    out
}

#[cfg(not(unix))]
pub fn addresses_with_prefix(_interface: &str) -> Vec<(IpAddr, u8)> {
    Vec::new()
}

/// Whether `addr` is in `net/prefix`.
pub fn in_subnet(addr: IpAddr, net: IpAddr, prefix: u8) -> bool {
    match (addr, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => {
            let p = prefix.min(32) as u32;
            let mask = if p == 0 { 0 } else { u32::MAX << (32 - p) };
            u32::from(a) & mask == u32::from(n) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(n)) => {
            let p = prefix.min(128) as u32;
            let mask = if p == 0 { 0 } else { u128::MAX << (128 - p) };
            u128::from(a) & mask == u128::from(n) & mask
        }
        _ => false,
    }
}

/// The address a peer dials on `interface`: its first IPv4 address, else its
/// first IPv6 address that is not link-local (a link-local one needs a scope
/// no URI we hand out carries).
pub fn primary_address(interface: &str) -> Option<IpAddr> {
    let all = addresses(interface);
    all.iter()
        .find(|a| a.is_ipv4())
        .or_else(|| {
            all.iter().find(|a| match a {
                IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
                _ => false,
            })
        })
        .copied()
}

/// True when `interface` exists on this machine.
pub fn exists(interface: &str) -> bool {
    sysfs_net().join(interface).exists()
}

/// The interface's MTU, from sysfs.
pub fn mtu(interface: &str) -> Option<u32> {
    read_trimmed(&sysfs_net().join(interface).join("mtu"))?.parse().ok()
}

/// The kernel's `operstate`: `up`, `down`, `lowerlayerdown`, `dormant`,
/// `unknown` (what a loopback or a dummy interface says while it works).
pub fn operstate(interface: &str) -> Option<String> {
    read_trimmed(&sysfs_net().join(interface).join("operstate"))
}

fn read_trimmed(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// What one network looks like right now, as health reports it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NetState {
    pub interface: String,
    /// `up`; `down` (no link); `no_address` (link, but not the address the
    /// engine bound to); `missing` (no such interface).
    pub state: &'static str,
    /// The address the engine serves on (bound or advertised there).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
    /// The MTU the configuration says this network has, when it says one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_mtu: Option<u32>,
    /// The interface's MTU is not the expected one: large transfers on it
    /// stall or fragment (the hardest failure there is to diagnose).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub mtu_mismatch: bool,
}

/// Read the state of `interface` against the address the engine serves on.
pub fn state_of(interface: &str, address: Option<IpAddr>, expected_mtu: Option<u32>) -> NetState {
    let mtu = mtu(interface);
    let state = if !exists(interface) {
        "missing"
    } else if matches!(operstate(interface).as_deref(), Some("down") | Some("lowerlayerdown")) {
        "down"
    } else {
        let present = addresses(interface);
        match address {
            Some(a) if present.contains(&a) => "up",
            Some(_) => "no_address",
            None if present.is_empty() => "no_address",
            None => "up",
        }
    };
    NetState {
        interface: interface.to_string(),
        state,
        address: address.map(|a| a.to_string()),
        mtu,
        expected_mtu,
        mtu_mismatch: matches!((mtu, expected_mtu), (Some(m), Some(e)) if m != e),
    }
}

/// The data path's addresses, process-wide (set once the configuration is
/// resolved): one per storage interface (#85, #90). Empty: no storage
/// network, binds are left as configured.
static DATA_ADDRESSES: RwLock<Vec<IpAddr>> = RwLock::new(Vec::new());

/// Set the one address every wildcard data-path bind is narrowed to.
pub fn set_data_address(addr: Option<IpAddr>) {
    set_data_addresses(addr.into_iter().collect());
}

/// Set every address the data path listens on (#90).
pub fn set_data_addresses(addrs: Vec<IpAddr>) {
    *DATA_ADDRESSES.write().unwrap_or_else(|e| e.into_inner()) = addrs;
}

/// The first data address.
pub fn data_address() -> Option<IpAddr> {
    data_addresses().first().copied()
}

/// Every data address.
pub fn data_addresses() -> Vec<IpAddr> {
    DATA_ADDRESSES.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Where a data-path listener (NVMe/TCP, iSCSI) really binds: a wildcard
/// address becomes the storage network's, port kept. A concrete address is
/// the operator's choice and is kept. Without a storage network, `addr`.
pub fn data_bind(addr: SocketAddr) -> SocketAddr {
    data_binds(addr)[0]
}

/// Every address a data-path listener binds (#90): a wildcard — or any one
/// of the storage addresses — becomes every storage address, port kept, so
/// a target is reachable on each storage NIC. Anything else, itself.
pub fn data_binds(addr: SocketAddr) -> Vec<SocketAddr> {
    let all = data_addresses();
    if all.is_empty() || !(addr.ip().is_unspecified() || all.contains(&addr.ip())) {
        return vec![addr];
    }
    all.into_iter().map(|ip| SocketAddr::new(ip, addr.port())).collect()
}

/// Bind a data-path listener (#90). An address on a storage interface is
/// bound to that interface's device too, so every connection it accepts
/// answers by the NIC it arrived on: with two NICs on one subnet the
/// routing table would send every reply out of the first one, whose
/// neighbours (rightly, `arp_ignore=1`) do not answer for the second's
/// peers. Anything else is an ordinary bind.
pub async fn bind_data_listener(addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    let Some(src) = path_sources().into_iter().find(|s| s.address == addr.ip()) else {
        return tokio::net::TcpListener::bind(addr).await;
    };
    let sock = if addr.is_ipv4() { tokio::net::TcpSocket::new_v4()? } else { tokio::net::TcpSocket::new_v6()? };
    sock.set_reuseaddr(true)?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Err(e) = socket2::SockRef::from(&sock).bind_device(Some(src.interface.as_bytes())) {
        tracing::warn!(
            "listener {addr}: cannot bind to {} ({e}): its replies leave by the routing table's choice of NIC",
            src.interface
        );
    }
    sock.bind(addr)?;
    sock.listen(1024)
}

/// A local interface an initiator's connection can leave by (#90).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSource {
    pub interface: String,
    pub address: IpAddr,
    pub prefix: u8,
}

/// The storage interfaces an initiator binds its connections to, set with
/// the data addresses. Empty: connections leave as the routing table says.
static PATH_SOURCES: RwLock<Vec<PathSource>> = RwLock::new(Vec::new());

/// Whether the sources were set (by the configuration, or from the
/// environment at first use).
static PATH_SOURCES_SET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_path_sources(sources: Vec<PathSource>) {
    PATH_SOURCES_SET.store(true, std::sync::atomic::Ordering::SeqCst);
    *PATH_SOURCES.write().unwrap_or_else(|e| e.into_inner()) = sources;
}

/// The storage interfaces connections leave by. A process that resolved no
/// configuration (a CLI command, `boot-local` in the initramfs) takes them
/// from `$STORMBLOCK_STORAGE_INTERFACE` (comma-separated) at first use.
pub fn path_sources() -> Vec<PathSource> {
    if !PATH_SOURCES_SET.swap(true, std::sync::atomic::Ordering::SeqCst) {
        if let Ok(v) = std::env::var("STORMBLOCK_STORAGE_INTERFACE") {
            let found = sources_of(v.split(',').map(str::trim).filter(|s| !s.is_empty()));
            *PATH_SOURCES.write().unwrap_or_else(|e| e.into_inner()) = found;
        }
    }
    PATH_SOURCES.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Each interface's first address (IPv4 first) with its prefix, for those
/// that have one now.
pub fn sources_of<'a>(interfaces: impl Iterator<Item = &'a str>) -> Vec<PathSource> {
    interfaces
        .filter_map(|i| {
            let a = primary_address(i)?;
            let prefix = addresses_with_prefix(i).into_iter().find(|(x, _)| *x == a).map(|(_, p)| p)?;
            Some(PathSource { interface: i.to_string(), address: a, prefix })
        })
        .collect()
}

/// The local interface the `index`th path to `target` leaves by: among the
/// storage interfaces on `target`'s subnet, the `index`th in turn — so two
/// paths to two target addresses on one subnet leave by two NICs. None when
/// no storage interface is on that subnet.
pub fn source_for(target: IpAddr, index: usize) -> Option<PathSource> {
    let on: Vec<PathSource> =
        path_sources().into_iter().filter(|s| in_subnet(target, s.address, s.prefix)).collect();
    if on.is_empty() {
        return None;
    }
    Some(on[index % on.len()].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_has_its_address_and_a_state() {
        let a = primary_address("lo").expect("lo has an address");
        assert!(a.is_loopback());
        let s = state_of("lo", Some(a), None);
        assert_eq!(s.state, "up", "{s:?}");
        assert!(s.mtu.is_some());
        let s = state_of("lo", Some("10.255.255.254".parse().unwrap()), Some(1));
        assert_eq!(s.state, "no_address");
        assert!(s.mtu_mismatch);
        assert_eq!(state_of("no-such-if0", None, None).state, "missing");
        assert!(addresses("no-such-if0").is_empty());
    }

    #[test]
    fn a_link_that_is_down_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let ifdir = dir.path().join("lo");
        std::fs::create_dir_all(&ifdir).unwrap();
        std::fs::write(ifdir.join("operstate"), "down\n").unwrap();
        std::fs::write(ifdir.join("mtu"), "9000\n").unwrap();
        std::env::set_var("STORMBLOCK_SYSFS_NET", dir.path());
        let s = state_of("lo", Some(IpAddr::V4(Ipv4Addr::LOCALHOST)), Some(9000));
        std::env::remove_var("STORMBLOCK_SYSFS_NET");
        assert_eq!(s.state, "down");
        assert_eq!(s.mtu, Some(9000));
        assert!(!s.mtu_mismatch);
    }

    #[test]
    fn a_wildcard_bind_is_narrowed_to_the_storage_address() {
        let wild: SocketAddr = "0.0.0.0:4420".parse().unwrap();
        let fixed: SocketAddr = "192.0.2.7:4420".parse().unwrap();
        set_data_address(None);
        assert_eq!(data_bind(wild), wild);
        set_data_address(Some("10.99.0.1".parse().unwrap()));
        assert_eq!(data_bind(wild), "10.99.0.1:4420".parse().unwrap());
        assert_eq!(data_bind(fixed), fixed);
        set_data_address(None);
    }

    #[test]
    fn every_storage_address_is_bound_and_paths_spread_over_the_nics() {
        let a: IpAddr = "10.99.0.1".parse().unwrap();
        let b: IpAddr = "10.99.0.11".parse().unwrap();
        set_data_addresses(vec![a, b]);
        let want: Vec<SocketAddr> = vec!["10.99.0.1:4420".parse().unwrap(), "10.99.0.11:4420".parse().unwrap()];
        assert_eq!(data_binds("0.0.0.0:4420".parse().unwrap()), want);
        assert_eq!(data_binds("10.99.0.11:4420".parse().unwrap()), want, "one of them means all");
        assert_eq!(data_binds("192.0.2.1:4420".parse().unwrap()), vec!["192.0.2.1:4420".parse().unwrap()]);
        set_data_addresses(Vec::new());

        assert!(in_subnet("10.99.0.200".parse().unwrap(), a, 24));
        assert!(!in_subnet("10.98.0.1".parse().unwrap(), a, 24));
        assert!(in_subnet("fd00::5".parse().unwrap(), "fd00::1".parse().unwrap(), 64));
        set_path_sources(vec![
            PathSource { interface: "eth1".into(), address: a, prefix: 24 },
            PathSource { interface: "eth2".into(), address: b, prefix: 24 },
            PathSource { interface: "eth3".into(), address: "10.98.0.1".parse().unwrap(), prefix: 24 },
        ]);
        let t: IpAddr = "10.99.0.2".parse().unwrap();
        assert_eq!(source_for(t, 0).unwrap().interface, "eth1");
        assert_eq!(source_for(t, 1).unwrap().interface, "eth2");
        assert_eq!(source_for(t, 2).unwrap().interface, "eth1");
        assert_eq!(source_for("10.98.0.9".parse().unwrap(), 1).unwrap().interface, "eth3");
        assert!(source_for("192.0.2.1".parse().unwrap(), 0).is_none());
        set_path_sources(Vec::new());
    }
}
