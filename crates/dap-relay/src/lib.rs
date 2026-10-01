// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `starling-dap-relay`: a `wasi:cli/run` component that bridges one debuggee connecting with
//! `--debug=connect:<host>:<port>` to a DAP client.
//!
//! ```text
//! wasmtime run -Sinherit-network=y dap_relay.wasm [--instance <addr>:<port>] [--client <port>]
//! ```
//!
//! The relay listens for instances on `--instance` (default `127.0.0.1:9230`) and prints
//! `relay listening for instances on <addr>:<port>` to stderr once it listens. The DAP client is
//! on stdio. With `--client <port>`, the relay instead listens on `127.0.0.1:<port>`, prints
//! `relay listening for the client on 127.0.0.1:<port>`, and accepts one client before it accepts
//! an instance. Port 0 picks a free port, which the printed line names. `--help` prints the usage.
//!
//! The relay reads nothing from the client until an instance connects, and then forwards bytes in
//! both directions. It bridges the first instance only and closes every later instance connection
//! as soon as it is accepted. When the instance disconnects, the relay sends the client a DAP
//! `terminated` event with `seq` 1 and exits with status 0. When the client's input ends, the
//! relay closes the instance connection and exits with status 0.
//!
//! Only `wasm32-wasip3` builds contain the relay. Other targets compile an empty library.

#![cfg(all(target_arch = "wasm32", target_env = "p3"))]

use std::future::{pending, poll_fn, Future, IntoFuture};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::pin::{pin, Pin};
use std::task::Poll;

use wasip3::sockets::types::{
    IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress, TcpSocket,
};
use wasip3::wit_bindgen::rt::async_support::{FutureReader, StreamReader, StreamResult};

const USAGE: &str = "usage: starling-dap-relay [--instance <addr>:<port>] [--client <port>]";

/// The body of the event the relay sends the client when the instance disconnects.
const TERMINATED: &str = r#"{"seq":1,"type":"event","event":"terminated"}"#;

/// The capacity of the buffer for each read from the instance.
const CHUNK: usize = 16 * 1024;

struct Relay;

impl wasip3::exports::cli::run::Guest for Relay {
    async fn run() -> Result<(), ()> {
        let options = match Options::parse(std::env::args().skip(1)) {
            Ok(Some(options)) => options,
            Ok(None) => {
                println!("{USAGE}");
                return Ok(());
            }
            Err(error) => {
                eprintln!("relay: {error}\n{USAGE}");
                return Err(());
            }
        };
        relay(options)
            .await
            .map_err(|error| eprintln!("relay: {error}"))
    }
}

wasip3::cli::command::export!(Relay);

struct Options {
    /// The address the relay listens on for instances.
    instance: SocketAddr,
    /// The port on 127.0.0.1 the relay listens on for the client, or `None` for a client on
    /// stdio.
    client: Option<u16>,
}

impl Options {
    /// The options `args` give, or `None` for `--help` or `-h`. Each option may be given once.
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let mut instance = None;
        let mut client = None;
        while let Some(arg) = args.next() {
            if arg == "--help" || arg == "-h" {
                return Ok(None);
            }
            let mut value = || args.next().ok_or(format!("`{arg}` needs a value"));
            match arg.as_str() {
                "--instance" if instance.is_none() => {
                    let value = value()?;
                    let address = value.parse().map_err(|_| {
                        format!(
                            "`--instance {value}` is not <ip>:<port> with an IP address, such as \
                             127.0.0.1:9230 or [::1]:9230"
                        )
                    })?;
                    instance = Some(address);
                }
                "--client" if client.is_none() => {
                    let value = value()?;
                    let port = value
                        .parse()
                        .map_err(|_| format!("`--client {value}` is not a port"))?;
                    client = Some(port);
                }
                "--instance" | "--client" => return Err(format!("`{arg}` is given twice")),
                _ => return Err(format!("unknown argument `{arg}`")),
            }
        }
        Ok(Some(Options {
            instance: instance.unwrap_or(SocketAddr::from((Ipv4Addr::LOCALHOST, 9230))),
            client,
        }))
    }
}

async fn relay(options: Options) -> Result<(), String> {
    let mut instances = Listener::bind(options.instance)?;
    eprintln!("relay listening for instances on {}", instances.address);
    let client = match options.client {
        None => Client::Stdio,
        Some(port) => {
            let mut clients = Listener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
            eprintln!("relay listening for the client on {}", clients.address);
            Client::Tcp(clients.accept().await?)
        }
    };
    let instance = instances.accept().await?;
    eprintln!(
        "relay: an instance connected from {}",
        remote_address(&instance)
    );

    let close_later_instances = async {
        while let Some(later) = instances.incoming.next().await {
            eprintln!(
                "relay: closed the connection of another instance from {}",
                remote_address(&later)
            );
        }
        pending::<()>().await
    };

    let (client_input, client_input_result) = client.input();
    let client_input_sent = instance.send(client_input).into_future();

    let (mut instance_output, _instance_output_result) = instance.receive();
    let (mut to_client, to_client_reader) = wasip3::wit_stream::new::<u8>();
    let client_output_written = client.write(to_client_reader);
    let forward_instance_output = async {
        let mut buffer = Vec::with_capacity(CHUNK);
        loop {
            let result;
            (result, buffer) = instance_output.read(buffer).await;
            if !buffer.is_empty() {
                buffer = to_client.write_all(buffer).await;
                if !buffer.is_empty() {
                    // Writing to the client failed, and `client_output_written` resolves with
                    // the error.
                    break;
                }
            }
            if matches!(result, StreamResult::Dropped) {
                let terminated =
                    format!("Content-Length: {}\r\n\r\n{TERMINATED}", TERMINATED.len());
                to_client.write_all(terminated.into_bytes()).await;
                break;
            }
        }
        drop(to_client);
        client_output_written.await
    };

    let mut forward_instance_output = pin!(forward_instance_output);
    let mut client_input_sent = pin!(client_input_sent);
    let mut client_input_failed = false;
    let mut close_later_instances = pin!(close_later_instances);
    let ended = poll_fn(|cx| {
        if let Poll::Ready(result) = forward_instance_output.as_mut().poll(cx) {
            return Poll::Ready(Some(result));
        }
        // Sending the client's input completes once that input ended. It fails when the instance
        // connection fails, and then we keep forwarding the instance's output until it ends.
        if !client_input_failed {
            if let Poll::Ready(result) = client_input_sent.as_mut().poll(cx) {
                match result {
                    Ok(()) => return Poll::Ready(None),
                    Err(_) => client_input_failed = true,
                }
            }
        }
        let _ = close_later_instances.as_mut().poll(cx);
        Poll::Pending
    })
    .await;
    match ended {
        Some(written) => written,
        None => client_input_result.get().await,
    }
}

/// A listening TCP socket.
struct Listener {
    _socket: TcpSocket,
    incoming: StreamReader<TcpSocket>,
    /// The address the socket is bound to, with the port the host picked for port 0.
    address: SocketAddr,
}

impl Listener {
    fn bind(address: SocketAddr) -> Result<Self, String> {
        let family = match address {
            SocketAddr::V4(_) => IpAddressFamily::Ipv4,
            SocketAddr::V6(_) => IpAddressFamily::Ipv6,
        };
        let socket = TcpSocket::create(family)
            .map_err(|error| format!("creating a socket for {address} failed: {error:?}"))?;
        socket
            .bind(to_wasi(address))
            .map_err(|error| format!("binding {address} failed: {error:?}"))?;
        let incoming = socket
            .listen()
            .map_err(|error| format!("listening on {address} failed: {error:?}"))?;
        let address = socket
            .get_local_address()
            .map(from_wasi)
            .map_err(|error| format!("reading the address of {address} failed: {error:?}"))?;
        Ok(Listener {
            _socket: socket,
            incoming,
            address,
        })
    }

    async fn accept(&mut self) -> Result<TcpSocket, String> {
        self.incoming
            .next()
            .await
            .ok_or_else(|| format!("the listener on {} closed", self.address))
    }
}

/// The DAP client's connection.
enum Client {
    Stdio,
    Tcp(TcpSocket),
}

impl Client {
    /// The bytes the client sends, and the result of reading them.
    fn input(&self) -> (StreamReader<u8>, InputResult) {
        match self {
            Client::Stdio => {
                let (input, result) = wasip3::cli::stdin::read_via_stream();
                (input, InputResult::Stdio(result))
            }
            Client::Tcp(socket) => {
                let (input, result) = socket.receive();
                (input, InputResult::Tcp(result))
            }
        }
    }

    /// Write `output` to the client. The returned future resolves once all of it was written.
    fn write(&self, output: StreamReader<u8>) -> Pin<Box<dyn Future<Output = Result<(), String>>>> {
        match self {
            Client::Stdio => {
                let written = wasip3::cli::stdout::write_via_stream(output);
                Box::pin(async move {
                    written
                        .await
                        .map_err(|error| format!("writing to stdout failed: {error:?}"))
                })
            }
            Client::Tcp(socket) => {
                let written = socket.send(output);
                Box::pin(async move {
                    written
                        .await
                        .map_err(|error| format!("writing to the client failed: {error:?}"))
                })
            }
        }
    }
}

/// The result of reading the bytes the client sends, available once they ended.
enum InputResult {
    Stdio(FutureReader<Result<(), wasip3::cli::types::ErrorCode>>),
    Tcp(FutureReader<Result<(), wasip3::sockets::types::ErrorCode>>),
}

impl InputResult {
    async fn get(self) -> Result<(), String> {
        match self {
            InputResult::Stdio(result) => result
                .await
                .map_err(|error| format!("reading stdin failed: {error:?}")),
            InputResult::Tcp(result) => result
                .await
                .map_err(|error| format!("reading from the client failed: {error:?}")),
        }
    }
}

fn remote_address(socket: &TcpSocket) -> String {
    match socket.get_remote_address() {
        Ok(address) => from_wasi(address).to_string(),
        Err(error) => format!("an unknown address ({error:?})"),
    }
}

fn to_wasi(address: SocketAddr) -> IpSocketAddress {
    match address {
        SocketAddr::V4(address) => {
            let [a, b, c, d] = address.ip().octets();
            IpSocketAddress::Ipv4(Ipv4SocketAddress {
                port: address.port(),
                address: (a, b, c, d),
            })
        }
        SocketAddr::V6(address) => {
            let [a, b, c, d, e, f, g, h] = address.ip().segments();
            IpSocketAddress::Ipv6(Ipv6SocketAddress {
                port: address.port(),
                flow_info: address.flowinfo(),
                address: (a, b, c, d, e, f, g, h),
                scope_id: address.scope_id(),
            })
        }
    }
}

fn from_wasi(address: IpSocketAddress) -> SocketAddr {
    match address {
        IpSocketAddress::Ipv4(Ipv4SocketAddress {
            port,
            address: (a, b, c, d),
        }) => SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), port).into(),
        IpSocketAddress::Ipv6(Ipv6SocketAddress {
            port,
            flow_info,
            address: (a, b, c, d, e, f, g, h),
            scope_id,
        }) => SocketAddrV6::new(
            Ipv6Addr::new(a, b, c, d, e, f, g, h),
            port,
            flow_info,
            scope_id,
        )
        .into(),
    }
}
