// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! TCP and UDP sockets and host name lookup, over tokio natively
//! ([`net_native`](crate::net_native)) and over `wasi:sockets` on WASIp3
//! ([`net_wasm`](crate::net_wasm)), whose items are re-exported here.
//!
//! Both backends provide `resolve`, `lookup_service`, `name_servers`, `connect_stream`,
//! `connect_stream_from`, `listen_stream`, `Connection` with its `ConnectionReader` and
//! `ConnectionWriter`, `StreamListener`, `UdpSocket`, `interface_index`, and `NetError` with its
//! Node.js error `code`. The WASIp3 backend also provides the raw `wasi:sockets` sockets.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV6};

#[cfg(not(target_arch = "wasm32"))]
pub use crate::net_native::*;
#[cfg(target_arch = "wasm32")]
pub use crate::net_wasm::*;

/// What [`ConnectionReader::receive`] returns.
#[derive(Debug)]
pub enum Received {
    /// Bytes received.
    Bytes(Vec<u8>),
    /// The peer ended its side.
    End,
    /// No more bytes will be received, because the peer ended its side or the connection failed.
    /// The next [`ConnectionReader::receive`] returns `End` or the error. Only the WASIp3 backend
    /// returns this.
    Stopped,
}

/// The options of [`UdpSocket::bind`].
#[derive(Clone, Copy, Debug, Default)]
pub struct UdpBindOptions {
    /// Allow other sockets with this option to bind the same address and port.
    pub reuse_addr: bool,
    /// Distribute the datagrams for the address and port among the sockets bound with this option.
    pub reuse_port: bool,
    /// Receive only IPv6 datagrams on an IPv6 socket.
    pub ipv6_only: bool,
}

/// The interface of a multicast operation: an IPv4 interface address, or an IPv6 interface index.
/// The unspecified address and the index 0 select the default interface.
#[derive(Clone, Copy, Debug)]
pub enum MulticastInterface {
    V4(Ipv4Addr),
    V6(u32),
}

/// The name servers of the `nameserver` lines of `text`, the contents of a `resolv.conf` file, in
/// order. A server is an address with an optional port, which is 53 if omitted or 0, written as
/// c-ares accepts it: `1.2.3.4`, `1.2.3.4:5353`, `::1`, `[::1]` or `[::1]:5353`. An IPv6 address
/// can be followed by a zone, `%` and the name or number of its interface, after its port if it
/// has one. Lines whose server doesn't parse are skipped.
pub fn parse_resolv_conf(text: &str) -> Vec<SocketAddr> {
    let mut servers = Vec::new();
    for line in text.lines() {
        let line = line.split(['#', ';']).next().unwrap_or_default();
        let mut words = line.split_whitespace();
        if words.next() != Some("nameserver") {
            continue;
        }
        servers.extend(words.next().and_then(name_server));
    }
    servers
}

/// The server `text` of a `nameserver` line, as [`parse_resolv_conf`] describes it.
fn name_server(text: &str) -> Option<SocketAddr> {
    let (address, zone) = match text.split_once('%') {
        Some((address, zone)) => (address, Some(zone)),
        None => (text, None),
    };
    let (ip, port) = if let Some(rest) = address.strip_prefix('[') {
        let (ip, rest) = rest.split_once(']')?;
        let port = match rest {
            "" => 0,
            rest => rest.strip_prefix(':')?.parse().ok()?,
        };
        (ip.parse::<IpAddr>().ok()?, port)
    } else if let Ok(ip) = address.parse::<IpAddr>() {
        (ip, 0)
    } else {
        let (ip, port) = address.rsplit_once(':')?;
        (IpAddr::V4(ip.parse().ok()?), port.parse().ok()?)
    };
    let port = if port == 0 { 53 } else { port };
    match (ip, zone) {
        (IpAddr::V4(ip), None) => Some(SocketAddr::new(ip.into(), port)),
        (IpAddr::V6(ip), zone) => {
            let scope = zone.map_or(Some(0), |zone| {
                zone.parse().ok().or_else(|| interface_index(zone))
            })?;
            Some(SocketAddrV6::new(ip, port, 0, scope).into())
        }
        (IpAddr::V4(_), Some(_)) => None,
    }
}
