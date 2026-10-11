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
#[cfg(unix)]
pub fn addresses(interface: &str) -> Vec<IpAddr> {
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
                out.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr))));
            }
            libc::AF_INET6 => {
                let sa = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in6) };
                out.push(IpAddr::V6(Ipv6Addr::from(sa.sin6_addr.s6_addr)));
            }
            _ => {}
        }
    }
    // SAFETY: the list getifaddrs returned, freed once.
    unsafe { libc::freeifaddrs(head) };
    out
}

#[cfg(not(unix))]
pub fn addresses(_interface: &str) -> Vec<IpAddr> {
    Vec::new()
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

/// The data path's address, process-wide (set once the configuration is
/// resolved). `None`: no storage network, binds are left as configured.
static DATA_ADDRESS: RwLock<Option<IpAddr>> = RwLock::new(None);

/// Set the address every wildcard data-path bind is narrowed to.
pub fn set_data_address(addr: Option<IpAddr>) {
    *DATA_ADDRESS.write().unwrap_or_else(|e| e.into_inner()) = addr;
}

/// The address set by [`set_data_address`].
pub fn data_address() -> Option<IpAddr> {
    *DATA_ADDRESS.read().unwrap_or_else(|e| e.into_inner())
}

/// Where a data-path listener (NVMe/TCP, iSCSI) really binds: a wildcard
/// address becomes the storage network's, port kept. A concrete address is
/// the operator's choice and is kept. Without a storage network, `addr`.
pub fn data_bind(addr: SocketAddr) -> SocketAddr {
    match data_address() {
        Some(ip) if addr.ip().is_unspecified() => SocketAddr::new(ip, addr.port()),
        _ => addr,
    }
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
}
