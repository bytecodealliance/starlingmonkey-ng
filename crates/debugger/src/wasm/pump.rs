// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Connections over p3 streams. Each stream's in-flight read or write is kept in a pump, which
//! the running-mode future polls while the debuggee runs and a pause polls from inside
//! [`block_on`]. A p3 operation is registered with the task that polled it last.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::{poll_fn, Future};
use std::task::{ready, Context, Poll};
use std::time::Duration;

use platform::block_on;
use platform::stream::{ReadPump, WritePump};
use wit_bindgen::{StreamReader, StreamWriter};

use crate::connection::{Connection, LocalFuture, Received};
use crate::transport::{write_message, Framer, Sequencer};

/// The capacity of each read of a connection's input.
const READ_CAPACITY: usize = 16 * 1024;

/// How long [`PumpConnection::close`] waits for the host to handle the outgoing bytes.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

/// A DAP connection over a p3 byte stream in each direction.
pub(crate) struct PumpConnection {
    read: RefCell<ReadPump<u8>>,
    write: RefCell<WritePump>,
    framer: RefCell<Framer>,
    sequencer: RefCell<Sequencer>,
    /// Framed incoming messages in arrival order.
    messages: RefCell<VecDeque<String>>,
    /// Set by `close`. Later outgoing messages are discarded.
    closed: Cell<bool>,
    /// Resolves once the host has handled every byte of the outgoing stream after it ended.
    /// Taken by `close`.
    written: RefCell<Option<LocalFuture<'static>>>,
    /// Values held for the connection's lifetime: the input stream's result future, and the
    /// socket of a TCP connection.
    _resources: Box<dyn Any>,
}

impl PumpConnection {
    /// A connection that reads from `rx` and writes to `tx`. `written` resolves once the host
    /// has handled every byte written to `tx` after `tx` was dropped. `resources` is held until
    /// the connection is dropped.
    pub(crate) fn new(
        rx: StreamReader<u8>,
        tx: StreamWriter<u8>,
        written: impl Future<Output = ()> + 'static,
        resources: impl Any,
    ) -> Self {
        Self {
            read: RefCell::new(ReadPump::new(rx, READ_CAPACITY)),
            write: RefCell::new(WritePump::new(tx)),
            framer: RefCell::default(),
            sequencer: RefCell::default(),
            messages: RefCell::default(),
            closed: Cell::new(false),
            written: RefCell::new(Some(Box::pin(written))),
            _resources: Box::new(resources),
        }
    }

    /// Poll the write pump, then read until a message is framed or the input ended. No read is
    /// started while a framed message is queued. A malformed message is discarded, except one for
    /// which [`FrameError::ends_input`](crate::transport::FrameError::ends_input) is `true`, which
    /// ends the input. Both are reported on stderr.
    fn poll_framed(&self, cx: &mut Context<'_>) -> Poll<()> {
        let _ = self.write.borrow_mut().poll_flush(cx);
        loop {
            if !self.messages.borrow().is_empty() {
                return Poll::Ready(());
            }
            let Some(bytes) = ready!(self.read.borrow_mut().poll_read(cx)) else {
                return Poll::Ready(());
            };
            let mut framer = self.framer.borrow_mut();
            framer.push(&bytes);
            loop {
                match framer.next_message() {
                    Ok(Some(message)) => self.messages.borrow_mut().push_back(message),
                    Ok(None) => break,
                    Err(error) if error.ends_input() => {
                        eprintln!("debugger: ended the connection: {error}");
                        self.read.borrow_mut().end();
                        break;
                    }
                    Err(error) => eprintln!("debugger: discarded a malformed DAP message: {error}"),
                }
            }
        }
    }

    /// Wake the running-mode future, whose read or write a blocking call may have polled last.
    fn wake_driver(&self) {
        let read = self.read.borrow_mut().driver.take();
        let write = self.write.borrow_mut().driver.take();
        for waker in [read, write].into_iter().flatten() {
            waker.wake();
        }
    }
}

impl Connection for PumpConnection {
    fn recv_blocking(&self) -> Option<String> {
        if let Some(message) = self.messages.borrow_mut().pop_front() {
            return Some(message);
        }
        let message = block_on(poll_fn(|cx| {
            ready!(self.poll_framed(cx));
            Poll::Ready(self.messages.borrow_mut().pop_front())
        }));
        self.wake_driver();
        message
    }

    fn try_recv(&self) -> Received {
        match self.messages.borrow_mut().pop_front() {
            Some(message) => Received::Message(message),
            None if self.read.borrow().eof() => Received::Closed,
            None => Received::Empty,
        }
    }

    /// Records the polling task's waker in both pumps, and writes queued messages while it
    /// waits.
    fn ready(&self) -> LocalFuture<'_> {
        Box::pin(poll_fn(|cx| {
            self.read.borrow_mut().driver = Some(cx.waker().clone());
            self.write.borrow_mut().driver = Some(cx.waker().clone());
            self.poll_framed(cx)
        }))
    }

    /// Queue an outgoing message and wake the running-mode future. Messages sent after `close`,
    /// or after the reading end of the outgoing stream was dropped, are discarded.
    fn send(&self, body: String) {
        if self.closed.get() {
            return;
        }
        let body = self.sequencer.borrow_mut().stamp(&body);
        let driver = {
            let mut write = self.write.borrow_mut();
            let Some(queued) = write.queue() else {
                return;
            };
            write_message(queued, &body).expect("writing to a `Vec` succeeds");
            write.driver.take()
        };
        if let Some(driver) = driver {
            driver.wake();
        }
    }

    fn flush(&self) {
        if !self.write.borrow().is_busy() {
            return;
        }
        block_on(poll_fn(|cx| self.write.borrow_mut().poll_flush(cx)));
        self.wake_driver();
    }

    /// Waits at most [`CLOSE_TIMEOUT`] for the host to handle the outgoing bytes, and discards
    /// what it has not handled by then. Does nothing after the first call.
    fn close(&self, last: Vec<String>) {
        if self.closed.get() {
            return;
        }
        for body in last {
            self.send(body);
        }
        self.closed.set(true);
        let written = self.written.borrow_mut().take();
        let mut flushed = std::pin::pin!(async {
            poll_fn(|cx| self.write.borrow_mut().poll_flush(cx)).await;
            self.write.borrow_mut().end();
            if let Some(written) = written {
                written.await;
            }
        });
        let mut timeout = std::pin::pin!(platform::clock::sleep(CLOSE_TIMEOUT));
        block_on(poll_fn(|cx| {
            if flushed.as_mut().poll(cx).is_ready() || timeout.as_mut().poll(cx).is_ready() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }));
        self.wake_driver();
    }
}
