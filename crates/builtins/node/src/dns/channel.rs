// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The transport of `dns.Resolver`'s queries: DNS over UDP, and over TCP after a truncated
//! response, over the sockets of `platform::net`, with c-ares's timeouts and retries.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::{poll_fn, Future};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;
use std::time::Duration;

use hickory_proto::op::Message;
use hickory_proto::rr::{Name, RecordType};
use platform::net::{UdpBindOptions, UdpSocket};

use super::wire::{self, Reply, Status};

/// The timeout c-ares uses for a `timeout` of 0 or below, in milliseconds.
const DEFAULT_TIMEOUT_MS: u64 = 2000;

/// The lowest timeout of an attempt, in milliseconds, as c-ares bounds it.
const MIN_TIMEOUT_MS: u64 = 250;

/// The highest timeout of a first attempt without `maxTimeout`, in milliseconds, as c-ares bounds
/// it.
const MAX_TIMEOUT_MS: u64 = 5000;

/// The highest timeout of any attempt, in milliseconds.
const TIMEOUT_CAP_MS: u64 = u32::MAX as u64;

/// The settings of a resolver's queries.
#[derive(Clone, Default)]
pub(crate) struct Config {
    pub(crate) servers: Vec<SocketAddr>,
    /// The `timeout` option, in milliseconds, which is -1 for the default.
    pub(crate) timeout: i32,
    pub(crate) tries: u32,
    /// The `maxTimeout` option, in milliseconds, which is 0 for none.
    pub(crate) max_timeout: u32,
    /// The local addresses `setLocalAddress` set, from which UDP queries are sent.
    pub(crate) local_v4: Option<Ipv4Addr>,
    pub(crate) local_v6: Option<Ipv6Addr>,
}

impl Config {
    /// The timeout of the attempts of round `round`, a pass through all servers, as c-ares's
    /// `ares_calc_query_timeout` computes it: the `timeout`, at least 250 ms and at most the
    /// `maxTimeout` or 5 s, doubled for each earlier round and at most `maxTimeout`, then
    /// reduced by a random amount of up to half after the first round, and at least the
    /// `timeout`.
    fn attempt_timeout(&self, round: u32) -> Duration {
        let timeout = match self.timeout {
            timeout if timeout <= 0 => DEFAULT_TIMEOUT_MS,
            timeout => timeout as u64,
        };
        let max_timeout = u64::from(self.max_timeout);
        let upper = if max_timeout > 0 {
            max_timeout
        } else {
            MAX_TIMEOUT_MS
        };
        let base = timeout.max(MIN_TIMEOUT_MS).min(upper);
        let mut timeplus = base
            .checked_shl(round)
            .filter(|doubled| doubled >> round == base)
            .unwrap_or(TIMEOUT_CAP_MS)
            .min(TIMEOUT_CAP_MS);
        if max_timeout > 0 {
            timeplus = timeplus.min(max_timeout);
        }
        if round > 0 {
            let mut bytes = [0u8; 4];
            platform::random::fill(&mut bytes);
            let fraction = f64::from(u32::from_le_bytes(bytes)) / f64::from(u32::MAX) / 2.0;
            timeplus -= (timeplus as f64 * fraction) as u64;
        }
        Duration::from_millis(timeplus.max(timeout))
    }
}

/// The state a resolver shares with its queries in progress: their cancellations, the failures
/// of each server, and whether the last query got an answer.
#[derive(Default)]
pub(crate) struct Shared {
    pending: RefCell<HashMap<u64, futures_channel::oneshot::Sender<()>>>,
    next_id: Cell<u64>,
    /// The consecutive failures of each server, by its index. Each query tries the servers with
    /// fewer failures first.
    failures: RefCell<Vec<u32>>,
    /// Whether the last query that completed failed with `ECONNREFUSED`.
    last_refused: Cell<bool>,
}

impl Shared {
    /// The number of queries in progress.
    pub(crate) fn pending(&self) -> usize {
        self.pending.borrow().len()
    }

    /// Whether the last query that completed didn't fail with `ECONNREFUSED`.
    pub(crate) fn last_ok(&self) -> bool {
        !self.last_refused.get()
    }

    /// Complete every query in progress with `ECANCELLED`.
    pub(crate) fn cancel(&self) {
        let pending = std::mem::take(&mut *self.pending.borrow_mut());
        for (_, cancel) in pending {
            let _ = cancel.send(());
        }
    }

    /// Forget the failures of the servers, for a new list of servers.
    pub(crate) fn reset_servers(&self) {
        self.failures.borrow_mut().clear();
    }
}

/// Removes a query from the queries in progress when it completes or is dropped.
struct PendingGuard {
    shared: Rc<Shared>,
    id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.shared.pending.borrow_mut().remove(&self.id);
    }
}

/// A query for the records of type `record_type` of `name`, with `config`, which completes with
/// `ECANCELLED` once [`Shared::cancel`] is called. Its response passed the checks of
/// [`wire::response_status`].
pub(crate) fn query(
    config: Config,
    shared: Rc<Shared>,
    name: Name,
    record_type: RecordType,
) -> impl Future<Output = Result<Message, Status>> {
    let id = shared.next_id.get();
    shared.next_id.set(id + 1);
    let (cancel, cancelled) = futures_channel::oneshot::channel();
    shared.pending.borrow_mut().insert(id, cancel);
    let guard = PendingGuard { shared, id };
    async move {
        let outcome = crate::support::until_cancelled(
            cancelled,
            attempts(&config, &guard.shared, &name, record_type),
        )
        .await
        .unwrap_or(Err(Status::Cancelled));
        guard
            .shared
            .last_refused
            .set(matches!(outcome, Err(Status::ConnRefused)));
        drop(guard);
        let message = outcome?;
        wire::response_status(&message)?;
        Ok(message)
    }
}

/// Send the query to the servers until one responds, `tries` times through all servers, as c-ares
/// does with `ARES_FLAG_NOCHECKRESP`: an error response is an answer. A truncated UDP response
/// makes this and the later attempts use TCP. A failed attempt moves to the next server, and the
/// last one's error is the query's.
async fn attempts(
    config: &Config,
    shared: &Shared,
    name: &Name,
    record_type: RecordType,
) -> Result<Message, Status> {
    let servers = &config.servers;
    if servers.is_empty() {
        return Err(Status::NoServer);
    }
    let mut id = [0u8; 2];
    platform::random::fill(&mut id);
    let id = u16::from_be_bytes(id);
    let query = wire::encode_query(id, name, record_type)?;
    let order: Vec<usize> = {
        let mut failures = shared.failures.borrow_mut();
        failures.resize(servers.len(), 0);
        let mut order: Vec<usize> = (0..servers.len()).collect();
        order.sort_by_key(|&index| failures[index]);
        order
    };
    let total = u64::from(config.tries) * servers.len() as u64;
    let mut sockets: Vec<Option<UdpSocket>> = servers.iter().map(|_| None).collect();
    let mut tcp = false;
    let mut last = Status::Timeout;
    let mut attempt = 0u64;
    while attempt < total {
        let index = order[(attempt % servers.len() as u64) as usize];
        let round = u32::try_from(attempt / servers.len() as u64).unwrap_or(u32::MAX);
        let timeout = config.attempt_timeout(round);
        let server = servers[index];
        let outcome = match tcp {
            false => {
                let slot = &mut sockets[index];
                udp_attempt(config, slot, server, &query, id, name, record_type, timeout).await
            }
            true => tcp_attempt(server, &query, id, name, record_type, timeout).await,
        };
        match outcome {
            Ok(message) if message.metadata.truncation && !tcp => tcp = true,
            Ok(message) => {
                if let Some(failures) = shared.failures.borrow_mut().get_mut(index) {
                    *failures = 0;
                }
                return Ok(message);
            }
            Err(status) => {
                if let Some(failures) = shared.failures.borrow_mut().get_mut(index) {
                    *failures = failures.saturating_add(1);
                }
                last = status;
                attempt += 1;
            }
        }
    }
    Err(last)
}

/// The output of `future`, or `ETIMEOUT` once `timeout` passed.
async fn within<T>(
    timeout: Duration,
    future: impl Future<Output = Result<T, Status>>,
) -> Result<T, Status> {
    let mut future = pin!(future);
    let mut sleep = pin!(platform::clock::sleep(timeout));
    poll_fn(|cx| {
        if let Poll::Ready(output) = future.as_mut().poll(cx) {
            return Poll::Ready(output);
        }
        sleep.as_mut().poll(cx).map(|()| Err(Status::Timeout))
    })
    .await
}

/// Send `query` to `server` over UDP from the local address of the configuration, and wait up
/// to `timeout` for its answer. Datagrams for other queries are ignored, a malformed answer is
/// `EBADRESP`, and a socket error, such as the port unreachable error of a server that isn't
/// listening, is `ECONNREFUSED`.
///
/// `slot` holds the socket connected to `server` across the attempts of a query. The socket is
/// created if `slot` is empty, and dropped after a socket error.
#[allow(clippy::too_many_arguments)]
async fn udp_attempt(
    config: &Config,
    slot: &mut Option<UdpSocket>,
    server: SocketAddr,
    query: &[u8],
    id: u16,
    name: &Name,
    record_type: RecordType,
    timeout: Duration,
) -> Result<Message, Status> {
    let socket = match slot {
        Some(socket) => socket,
        None => {
            let local: IpAddr = match server {
                SocketAddr::V4(_) => config.local_v4.unwrap_or(Ipv4Addr::UNSPECIFIED).into(),
                SocketAddr::V6(_) => config.local_v6.unwrap_or(Ipv6Addr::UNSPECIFIED).into(),
            };
            let mut socket = UdpSocket::bind(SocketAddr::new(local, 0), UdpBindOptions::default())
                .map_err(|_| Status::ConnRefused)?;
            socket.connect(server).map_err(|_| Status::ConnRefused)?;
            slot.insert(socket)
        }
    };
    let mut query = query.to_vec();
    let outcome = within(timeout, async {
        poll_fn(|cx| socket.poll_send(cx, &mut query, None))
            .await
            .map_err(|_| Status::ConnRefused)?;
        loop {
            let (bytes, _) = poll_fn(|cx| socket.poll_recv(cx))
                .await
                .map_err(|_| Status::ConnRefused)?;
            match wire::check_reply(&bytes, id, name, record_type) {
                Reply::Other => continue,
                Reply::Malformed => return Err(Status::BadResp),
                Reply::Answer(message) => return Ok(message),
            }
        }
    })
    .await;
    if matches!(outcome, Err(Status::ConnRefused)) {
        *slot = None;
    }
    outcome
}

/// Send `query` to `server` over TCP, framed with its length, and wait up to `timeout` for its
/// answer. Messages for other queries are ignored, a malformed answer is `EBADRESP`, and a
/// connection error, or a connection closed before the answer, is `ECONNREFUSED`.
async fn tcp_attempt(
    server: SocketAddr,
    query: &[u8],
    id: u16,
    name: &Name,
    record_type: RecordType,
    timeout: Duration,
) -> Result<Message, Status> {
    within(timeout, async {
        let mut connection = platform::net::connect_stream(server, None)
            .await
            .map_err(|_| Status::ConnRefused)?;
        let length = u16::try_from(query.len()).map_err(|_| Status::BadName)?;
        let mut framed = Vec::new();
        framed
            .try_reserve_exact(query.len() + 2)
            .map_err(|_| Status::NoMem)?;
        framed.extend_from_slice(&length.to_be_bytes());
        framed.extend_from_slice(query);
        connection
            .writer
            .write(framed)
            .await
            .map_err(|_| Status::ConnRefused)?;
        let mut received: Vec<u8> = Vec::new();
        loop {
            while received.len() >= 2 {
                let length = usize::from(u16::from_be_bytes([received[0], received[1]]));
                if received.len() < 2 + length {
                    break;
                }
                let reply = wire::check_reply(&received[2..2 + length], id, name, record_type);
                received.drain(..2 + length);
                match reply {
                    Reply::Other => {}
                    Reply::Malformed => return Err(Status::BadResp),
                    Reply::Answer(message) => return Ok(message),
                }
            }
            let bytes = connection
                .reader
                .read()
                .await
                .map_err(|_| Status::ConnRefused)?
                .ok_or(Status::ConnRefused)?;
            received
                .try_reserve(bytes.len())
                .map_err(|_| Status::NoMem)?;
            received.extend_from_slice(&bytes);
        }
    })
    .await
}
