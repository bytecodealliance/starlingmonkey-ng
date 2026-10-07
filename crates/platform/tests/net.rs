// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! TCP connections, host name lookup and `resolv.conf` parsing.

#![cfg(not(target_arch = "wasm32"))]

use platform::net::{connect_stream, listen_stream, parse_resolv_conf, resolve};
use std::net::{IpAddr, SocketAddr};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn name_servers_can_have_ports() {
    let servers = parse_resolv_conf(
        "nameserver 1.2.3.4\n\
         nameserver 1.2.3.4:5353\n\
         nameserver [1.2.3.4]:5354\n\
         nameserver ::1\n\
         nameserver [::1]\n\
         nameserver [::1]:5355\n\
         nameserver [fe80::1]:5356%7\n\
         nameserver fe80::1%7\n\
         nameserver 1.2.3.4:0\n\
         nameserver 1.2.3.4:x\n\
         nameserver [::1]5353\n\
         nameserver 1.2.3.4%7\n",
    );
    let expected: Vec<SocketAddr> = [
        "1.2.3.4:53",
        "1.2.3.4:5353",
        "1.2.3.4:5354",
        "[::1]:53",
        "[::1]:53",
        "[::1]:5355",
        "[fe80::1%7]:5356",
        "[fe80::1%7]:53",
        "1.2.3.4:53",
    ]
    .iter()
    .map(|text| text.parse().unwrap())
    .collect();
    assert_eq!(servers, expected);
}

#[test]
fn localhost_resolves_to_a_loopback_address() {
    let addrs = runtime().block_on(resolve("localhost")).unwrap();
    assert!(addrs.iter().all(IpAddr::is_loopback), "{addrs:?}");
    assert!(!addrs.is_empty());
}

#[test]
fn an_unknown_host_is_not_found() {
    let error = runtime()
        .block_on(resolve("nonexistent.invalid"))
        .unwrap_err();
    assert_eq!(error.code(), "ENOTFOUND");
}

#[test]
fn a_connection_reads_what_its_peer_sent() {
    runtime().block_on(async {
        let mut listener = listen_stream(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let client = connect_stream(listener.local_addr, None).await.unwrap();
        let mut server = std::future::poll_fn(|cx| listener.poll_accept(cx))
            .await
            .unwrap();
        let mut writer = client.writer;
        writer.write(b"hello".to_vec()).await.unwrap();
        writer.shutdown().await.unwrap();
        let mut received = Vec::new();
        while let Some(bytes) = server.reader.read().await.unwrap() {
            received.extend(bytes);
        }
        assert_eq!(received, b"hello");
    });
}
