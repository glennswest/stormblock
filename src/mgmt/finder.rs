//! Finding the appliance on a network nobody prepared (#104).
//!
//! A diskless node finds its appliance from what a person set: the command
//! line, DHCP option 17, `boothost.<domain>`, the lease's servers. On a
//! network with none of those, it asks: a query on a multicast group, and
//! every engine that serves boot claims (forge mode) answers with its API's
//! URL. Both ends are ours, so it is a query and an answer, not mDNS.
//!
//! **Off by default** (the master's decision B on #104): the node asks only
//! with `rd.stormblock.discover=on`, and what answers is tried **last**, after
//! every configured source, each asked `/api/v1/health` like the rest. An
//! answer is unauthenticated: anyone on the segment can give one, which is
//! why it is never asked for unless someone asks for it.
//!
//! The group is the cluster beacon's (239.255.42.99), the port its own
//! ([`FIND_PORT`]); the query is [`QUERY`], the answer one line of JSON:
//! `{"url": "http://192.168.31.202:9090", "node": "forge"}`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::mgmt::discovery::DISCOVERY_GROUP;
use crate::mgmt::AppState;

/// The port a finder query goes to.
pub const FIND_PORT: u16 = 7448;
/// What a node asks.
pub const QUERY: &[u8] = b"STORMBLOCK-FIND-APPLIANCE/1\n";

/// An answer: where the appliance's API is, and its name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Answer {
    pub url: String,
    #[serde(default)]
    pub node: String,
}

/// The URL this engine answers with to an asker at `asker`: the advertised
/// host when one is set, else the address the reply leaves from (on a
/// network nobody prepared, the one the asker can reach); the API's port; and
/// `https` when the API serves TLS.
pub fn answer_url(state: &AppState, asker: SocketAddr) -> Option<String> {
    let m = &state.config.management;
    let port = m.listen_addr.rsplit_once(':').map(|(_, p)| p).unwrap_or("9090");
    let host = match m.advertised_host() {
        Some(h) => h,
        None => crate::netif::route_source(asker)?.to_string(),
    };
    let scheme = if m.tls_cert.is_some() { "https" } else { "http" };
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host };
    Some(format!("{scheme}://{host}:{port}"))
}

/// Answer finder queries while this engine serves boot claims (forge mode on):
/// joined to the group on every IPv4 interface that has an address.
pub fn spawn_responder(state: Arc<AppState>) {
    let sock = match bind_responder() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("finder (#104): not answering on {DISCOVERY_GROUP}:{FIND_PORT}: {e}");
            return;
        }
    };
    let Ok(sock) = tokio::net::UdpSocket::from_std(sock) else { return };
    tokio::spawn(async move {
        let mut buf = [0u8; 256];
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf).await else { continue };
            if &buf[..n] != QUERY {
                continue;
            }
            // Only an engine that serves boot claims is an appliance.
            if state.nvmeof_target.read().await.is_none() {
                continue;
            }
            let Some(url) = answer_url(&state, from) else { continue };
            let a = Answer { url, node: state.local_node_name() };
            if let Ok(body) = serde_json::to_vec(&a) {
                let _ = sock.send_to(&body, from).await;
                tracing::info!("finder (#104): {from} asked; answered {}", a.url);
            }
        }
    });
}

fn bind_responder() -> std::io::Result<std::net::UdpSocket> {
    let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    s.set_reuse_address(true)?;
    s.set_nonblocking(true)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, FIND_PORT)).into())?;
    let mut joined = 0;
    for a in crate::netif::interface_addresses() {
        if let IpAddr::V4(v4) = a.address {
            if !a.loopback && s.join_multicast_v4(&DISCOVERY_GROUP, &v4).is_ok() {
                joined += 1;
            }
        }
    }
    if joined == 0 {
        s.join_multicast_v4(&DISCOVERY_GROUP, &Ipv4Addr::UNSPECIFIED)?;
    }
    Ok(s.into())
}

/// Ask on every IPv4 interface that is up (or only to `to`, for tests) and
/// collect the answers for `wait`: their URLs, each once, in the order they
/// came.
pub fn discover(wait: Duration, to: Option<SocketAddr>) -> std::io::Result<Vec<Answer>> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    sock.set_multicast_ttl_v4(1)?;
    let group = SocketAddr::from((DISCOVERY_GROUP, FIND_PORT));
    match to {
        Some(t) => {
            sock.send_to(QUERY, t)?;
        }
        None => {
            let s2 = socket2::SockRef::from(&sock);
            let mut sent = 0;
            for a in crate::netif::interface_addresses() {
                if let IpAddr::V4(v4) = a.address {
                    if a.loopback {
                        continue;
                    }
                    if s2.set_multicast_if_v4(&v4).is_ok() && sock.send_to(QUERY, group).is_ok() {
                        sent += 1;
                    }
                }
            }
            if sent == 0 {
                sock.send_to(QUERY, group)?;
            }
        }
    }
    let deadline = Instant::now() + wait;
    let mut out: Vec<Answer> = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        sock.set_read_timeout(Some(left))?;
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                if let Ok(a) = serde_json::from_slice::<Answer>(&buf[..n]) {
                    if (a.url.starts_with("http://") || a.url.starts_with("https://")) && !out.iter().any(|o| o.url == a.url) {
                        out.push(a);
                    }
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An answer round-trips, and a reply that is not one is not taken.
    #[test]
    fn an_answer_is_one_line_of_json() {
        let a = Answer { url: "http://192.168.31.202:9090".into(), node: "forge".into() };
        let b = serde_json::to_vec(&a).unwrap();
        assert_eq!(serde_json::from_slice::<Answer>(&b).unwrap(), a);
        assert!(serde_json::from_slice::<Answer>(b"hello").is_err());
    }

    /// The querier against a stand-in responder on loopback: the answer
    /// listed once though sent twice, a non-URL dropped.
    #[test]
    fn discover_lists_each_answer_once() {
        let r = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let at = r.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let (n, from) = r.recv_from(&mut buf).unwrap();
            assert_eq!(&buf[..n], QUERY);
            for body in [
                br#"{"url":"http://10.0.0.5:9090","node":"forge"}"#.as_slice(),
                br#"{"url":"http://10.0.0.5:9090","node":"forge"}"#.as_slice(),
                br#"{"url":"file:///etc/passwd","node":"x"}"#.as_slice(),
            ] {
                r.send_to(body, from).unwrap();
            }
        });
        let got = discover(Duration::from_millis(500), Some(at)).unwrap();
        t.join().unwrap();
        assert_eq!(got, vec![Answer { url: "http://10.0.0.5:9090".into(), node: "forge".into() }]);
    }
}
