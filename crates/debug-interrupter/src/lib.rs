// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `starling-debug-interrupter`: a debugger component for `wasmtime -Ddebugger=` that requests
//! the interrupt callback of the debug session in a StarlingMonkey instance.
//!
//! ```text
//! wasmtime run -Ddebugger=debug_interrupter.wasm -Darg=<addr>:<port> starling.wasm --debug=...
//! ```
//!
//! A StarlingMonkey instance has no thread that could notice a DAP message while JavaScript
//! runs, so this component requests the callback from outside the instance. It listens on
//! `<addr>:<port>`, and `wasmtime` prints `Debugger: debug interrupter listening on
//! <addr>:<port>` once it listens. Port 0 picks a free port, which the printed line names. Each
//! connection it accepts is a request: the component closes the connection, stops the instance,
//! sets the bit that requests the interrupt callback in the instance's memory, and resumes the
//! instance. The session's interrupt callback then takes in the DAP messages that have arrived
//! on the session's own connection.
//!
//! The instance publishes where to write in its `starling_debug_interrupt` global (see
//! `crates/debugger/src/wasm/interrupt.rs`). A request made while the instance has no debug
//! session writes nothing. After each write, the component stops the instance again
//! [`RECHECK_DELAY_NS`] later and writes again if the instance reports that the callback left
//! messages queued because the debug adapter was the interrupted code.
//!
//! Only `wasm32-wasip2` builds contain the component. Other targets compile an empty library.

#![cfg(all(target_arch = "wasm32", target_env = "p2"))]

use std::net::SocketAddr;

use bytecodealliance::wasmtime::debuggee::{Debuggee, Event, EventFuture, Memory, ResumptionValue};
use wasi::clocks::monotonic_clock::subscribe_duration;
use wasi::io::poll::{poll, Pollable};
use wasi::sockets::instance_network::instance_network;
use wasi::sockets::network::{
    ErrorCode, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress,
};
use wasi::sockets::tcp::TcpSocket;
use wasi::sockets::tcp_create_socket::create_tcp_socket;

wit_bindgen::generate!({
    path: "wit",
    world: "interrupter",
    generate_all,
});

/// The global whose value is the address of the instance's interrupt request (see
/// `crates/debugger/src/wasm/interrupt.rs`).
const INTERRUPT_GLOBAL: &str = "starling_debug_interrupt";

/// How long after a write the instance is stopped again to read whether the write has to be
/// repeated.
const RECHECK_DELAY_NS: u64 = 2_000_000;

struct Interrupter;

export!(Interrupter);

impl exports::bytecodealliance::wasmtime::debugger::Guest for Interrupter {
    /// Run the debuggee to completion, with an interrupt request for each connection to the
    /// address in `args[1]`. Without a usable address, the debuggee runs without requests.
    fn debug(debuggee: &Debuggee, args: Vec<String>) {
        let listener = match args.get(1).map(|addr| listen(addr)) {
            Some(Ok(listener)) => Some(listener),
            Some(Err(error)) => {
                print_debugger_info(&format!("debug interrupter: {error}"));
                None
            }
            None => {
                print_debugger_info("usage: starling-debug-interrupter <addr>:<port>");
                None
            }
        };
        let accept = listener.as_ref().map(TcpSocket::subscribe);
        let mut session = Session {
            debuggee,
            target: None,
            requested: false,
            stopping: false,
            recheck: None,
        };
        let mut run = debuggee.continue_(ResumptionValue::Normal);
        loop {
            let stopped = run.subscribe();
            let mut pollables = vec![&stopped];
            pollables.extend(&accept);
            pollables.extend(&session.recheck);
            let ready = poll(&pollables);
            drop(pollables);
            drop(stopped);
            if ready.contains(&0) {
                match EventFuture::finish(run, debuggee) {
                    Ok(Event::Complete) => return,
                    Ok(Event::Interrupted) => session.stopped(),
                    Ok(_) => {}
                    Err(error) => {
                        print_debugger_info(&format!("debug interrupter: {error:?}"));
                        return;
                    }
                }
                run = debuggee.continue_(ResumptionValue::Normal);
                continue;
            }
            // The recheck timer is the last pollable, after the listener if there is one.
            let recheck =
                session.recheck.is_some() && ready.contains(&(1 + accept.iter().count() as u32));
            if recheck {
                session.recheck = None;
            }
            if accept.is_some() && ready.contains(&1) {
                if let Some(Ok((connection, input, output))) =
                    listener.as_ref().map(TcpSocket::accept)
                {
                    // The connection is the request. Dropping it closes it, and its streams are
                    // dropped first, as the host requires.
                    drop((input, output));
                    drop(connection);
                    session.requested = true;
                }
            }
            if session.requested || recheck {
                session.stop();
            }
        }
    }
}

/// The state of the interrupt requests for one debuggee.
struct Session<'a> {
    debuggee: &'a Debuggee,
    /// Where the instance has the interrupt requests written, once it was found.
    target: Option<Target>,
    /// Set by a connection whose request was not written yet.
    requested: bool,
    /// Set while the debuggee was asked to stop and has not reported it.
    stopping: bool,
    /// Ready once the instance is stopped again to read whether a write has to be repeated.
    recheck: Option<Pollable>,
}

impl Session<'_> {
    /// Ask the running debuggee to stop, unless it was asked to already.
    fn stop(&mut self) {
        if !self.stopping {
            self.stopping = true;
            self.debuggee.interrupt();
        }
    }

    /// Write the interrupt request into the stopped debuggee if a connection asked for it, or if
    /// the instance reports that the callback of an earlier write left messages queued.
    fn stopped(&mut self) {
        self.stopping = false;
        if self.target.is_none() {
            self.target = Target::find(self.debuggee);
        }
        let Some(target) = &self.target else {
            // No instance has the global: no request can be written.
            self.requested = false;
            return;
        };
        if self.requested || target.deferred(self.debuggee) {
            self.requested = false;
            if target.request(self.debuggee) {
                self.recheck = Some(subscribe_duration(RECHECK_DELAY_NS));
            }
        }
    }
}

/// The interrupt request of an instance: the memory that holds it, and its address.
struct Target {
    memory: Memory,
    request: u64,
}

impl Target {
    /// The interrupt request of the first instance whose module exports [`INTERRUPT_GLOBAL`].
    fn find(debuggee: &Debuggee) -> Option<Target> {
        debuggee.all_instances().into_iter().find_map(|instance| {
            let bytecode = instance.get_module(debuggee).bytecode()?;
            let global = exported_global(&bytecode, INTERRUPT_GLOBAL)?;
            let value = instance
                .get_global(debuggee, global)
                .ok()?
                .get(debuggee)
                .ok()?;
            Some(Target {
                memory: instance.get_memory(debuggee, 0).ok()?,
                request: value.unwrap_i32().into(),
            })
        })
    }

    /// Set the bit that requests the interrupt callback. Returns `false` without writing if the
    /// instance has no debug session, which it reports with a null address.
    fn request(&self, debuggee: &Debuggee) -> bool {
        let read = |address| self.memory.get_u32(debuggee, address).ok();
        let Some(bits) = read(self.request).filter(|&bits| bits != 0) else {
            return false;
        };
        let (Some(callback_bit), Some(pending)) = (read(self.request + 4), read(bits.into()))
        else {
            return false;
        };
        self.memory
            .set_u32(debuggee, bits.into(), pending | callback_bit)
            .is_ok()
    }

    /// Whether the last interrupt callback left messages queued because the debug adapter was
    /// the interrupted code.
    fn deferred(&self, debuggee: &Debuggee) -> bool {
        self.memory
            .get_u32(debuggee, self.request + 8)
            .is_ok_and(|deferred| deferred != 0)
    }
}

/// The index of the global that the module `bytecode` exports as `name`.
fn exported_global(bytecode: &[u8], name: &str) -> Option<u32> {
    wasmparser::Parser::new(0)
        .parse_all(bytecode)
        .find_map(|payload| match payload {
            Ok(wasmparser::Payload::ExportSection(exports)) => Some(exports),
            _ => None,
        })?
        .into_iter()
        .filter_map(Result::ok)
        .find(|export| export.kind == wasmparser::ExternalKind::Global && export.name == name)
        .map(|export| export.index)
}

/// Listen on `addr`, and print the address the listener is bound to.
fn listen(addr: &str) -> Result<TcpSocket, String> {
    let addr: SocketAddr = addr
        .parse()
        .map_err(|error| format!("`{addr}` is no socket address: {error}"))?;
    let failed = |error: ErrorCode| format!("listening on {addr} failed: {error:?}");
    let (family, address) = match addr {
        SocketAddr::V4(v4) => (
            IpAddressFamily::Ipv4,
            IpSocketAddress::Ipv4(Ipv4SocketAddress {
                port: v4.port(),
                address: v4.ip().octets().into(),
            }),
        ),
        SocketAddr::V6(v6) => (
            IpAddressFamily::Ipv6,
            IpSocketAddress::Ipv6(Ipv6SocketAddress {
                port: v6.port(),
                flow_info: v6.flowinfo(),
                address: v6.ip().segments().into(),
                scope_id: v6.scope_id(),
            }),
        ),
    };
    let socket = create_tcp_socket(family).map_err(failed)?;
    socket
        .start_bind(&instance_network(), address)
        .map_err(failed)?;
    finish(&socket, TcpSocket::finish_bind).map_err(failed)?;
    socket.start_listen().map_err(failed)?;
    finish(&socket, TcpSocket::finish_listen).map_err(failed)?;
    let port = match socket.local_address().map_err(failed)? {
        IpSocketAddress::Ipv4(bound) => bound.port,
        IpSocketAddress::Ipv6(bound) => bound.port,
    };
    let bound = SocketAddr::new(addr.ip(), port);
    print_debugger_info(&format!("debug interrupter listening on {bound}"));
    Ok(socket)
}

/// Call `finish` until the operation `socket` started has completed.
fn finish(
    socket: &TcpSocket,
    finish: impl Fn(&TcpSocket) -> Result<(), ErrorCode>,
) -> Result<(), ErrorCode> {
    loop {
        match finish(socket) {
            Err(ErrorCode::WouldBlock) => socket.subscribe().block(),
            result => return result,
        }
    }
}
