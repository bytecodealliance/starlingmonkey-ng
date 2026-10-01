// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Connections over p3 streams. Each stream's in-flight read or write is kept in a pump, which
//! the running-mode future polls while the debuggee runs and a pause polls from inside
//! `block_on_detached`. A p3 operation is registered with the task that polled it last.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::task::{ready, Context, Poll, Waker};

use wit_bindgen::{block_on_detached, StreamReader, StreamResult, StreamWriter};

use crate::connection::{Connection, LocalFuture, Received};
use crate::transport::{write_message, FrameError, Framer, Sequencer};

/// The capacity of each read of a connection's input.
const READ_CAPACITY: usize = 16 * 1024;

type ReadOp<T> = Pin<Box<dyn Future<Output = (StreamReader<T>, StreamResult, Vec<T>)>>>;
type WriteOp = Pin<Box<dyn Future<Output = (StreamWriter<u8>, Vec<u8>)>>>;

/// The in-flight read of a p3 stream, kept in a slot that any task can poll. The task that
/// polls it owns the read's waitable at that moment.
pub(crate) struct ReadPump<T: 'static> {
    /// `None` while a read is in flight, and once the stream ended.
    rx: Option<StreamReader<T>>,
    op: Option<ReadOp<T>>,
    /// The capacity of each read.
    capacity: usize,
    eof: bool,
    /// The waker of the running-mode future's last poll.
    pub(crate) driver: Option<Waker>,
}

impl<T: 'static> ReadPump<T> {
    pub(crate) fn new(rx: StreamReader<T>, capacity: usize) -> Self {
        Self {
            rx: Some(rx),
            op: None,
            capacity,
            eof: false,
            driver: None,
        }
    }

    /// Whether the writing end was dropped and every value was read, or [`ReadPump::end`] was
    /// called.
    pub(crate) fn eof(&self) -> bool {
        self.eof
    }

    /// Drop the reading end and any read in flight. The pump then reports the stream ended.
    pub(crate) fn end(&mut self) {
        self.op = None;
        self.rx = None;
        self.eof = true;
    }

    /// Poll the in-flight read with `cx`, starting one if none is in flight. Returns the values
    /// read, which can be none, or `None` once the stream ended.
    pub(crate) fn poll_read(&mut self, cx: &mut Context<'_>) -> Poll<Option<Vec<T>>> {
        if self.eof {
            return Poll::Ready(None);
        }
        let capacity = self.capacity;
        let rx = &mut self.rx;
        let op = self.op.get_or_insert_with(|| {
            let mut rx = rx
                .take()
                .expect("a stream that has not ended has a reader or a read in flight");
            Box::pin(async move {
                let (result, values) = rx.read(Vec::with_capacity(capacity)).await;
                (rx, result, values)
            })
        });
        let (rx, result, values) = ready!(op.as_mut().poll(cx));
        self.op = None;
        if result == StreamResult::Dropped {
            self.eof = true;
            if values.is_empty() {
                return Poll::Ready(None);
            }
        } else {
            self.rx = Some(rx);
        }
        Poll::Ready(Some(values))
    }
}

/// The queued output of a p3 byte stream and its in-flight write, polled like [`ReadPump`].
pub(crate) struct WritePump {
    /// `None` while a write is in flight, once the reading end was dropped, and after
    /// [`WritePump::end`].
    tx: Option<StreamWriter<u8>>,
    /// Bytes not yet passed to a write.
    queued: Vec<u8>,
    op: Option<WriteOp>,
    /// The waker of the running-mode future's last poll.
    pub(crate) driver: Option<Waker>,
}

impl WritePump {
    pub(crate) fn new(tx: StreamWriter<u8>) -> Self {
        Self {
            tx: Some(tx),
            queued: Vec::new(),
            op: None,
            driver: None,
        }
    }

    /// The buffer to append outgoing bytes to, or `None` once the stream is closed.
    fn queue(&mut self) -> Option<&mut Vec<u8>> {
        (self.tx.is_some() || self.op.is_some()).then_some(&mut self.queued)
    }

    /// Whether bytes are queued or a write is in flight.
    fn is_busy(&self) -> bool {
        !self.queued.is_empty() || self.op.is_some()
    }

    /// Poll the in-flight write with `cx`, and write queued bytes until none are left. Queued
    /// bytes are discarded once the reading end was dropped.
    pub(crate) fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            if self.op.is_none() {
                if self.queued.is_empty() {
                    return Poll::Ready(());
                }
                let Some(mut tx) = self.tx.take() else {
                    self.queued.clear();
                    return Poll::Ready(());
                };
                let bytes = std::mem::take(&mut self.queued);
                self.op = Some(Box::pin(async move {
                    let unwritten = tx.write_all(bytes).await;
                    (tx, unwritten)
                }));
            }
            let op = self.op.as_mut().expect("a write is in flight");
            let (tx, unwritten) = ready!(op.as_mut().poll(cx));
            self.op = None;
            if unwritten.is_empty() {
                self.tx = Some(tx);
            }
        }
    }

    /// Drop the writing end, which ends the stream. Bytes still queued are discarded.
    fn end(&mut self) {
        self.tx = None;
        self.op = None;
        self.queued.clear();
    }
}

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
    /// started while a framed message is queued. A malformed message is discarded, except one whose
    /// `Content-Length` exceeds the maximum, which ends the input. Both are reported on stderr.
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
                    Err(error @ FrameError::ContentTooLong) => {
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
        let message = block_on_detached(poll_fn(|cx| {
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
        block_on_detached(poll_fn(|cx| self.write.borrow_mut().poll_flush(cx)));
        self.wake_driver();
    }

    /// Does nothing after the first call.
    fn close(&self, last: Vec<String>) {
        if self.closed.get() {
            return;
        }
        for body in last {
            self.send(body);
        }
        self.closed.set(true);
        let written = self.written.borrow_mut().take();
        block_on_detached(async {
            poll_fn(|cx| self.write.borrow_mut().poll_flush(cx)).await;
            self.write.borrow_mut().end();
            if let Some(written) = written {
                written.await;
            }
        });
        self.wake_driver();
    }
}
