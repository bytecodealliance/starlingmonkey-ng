// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Pumps for WASIp3 streams: an in-flight read or write kept in a slot that any task can poll.
//!
//! A p3 operation is registered with the task that polled it last, so a read or write that a
//! blocking call polls from inside [`block_on`](crate::block_on) moves to that call's task. The
//! pumps record the waker of the last non-blocking poll in `driver`, which a caller wakes after a
//! blocking poll so that task polls the operation again.

use std::future::Future;
use std::pin::Pin;
use std::task::{ready, Context, Poll, Waker};

use wit_bindgen::{StreamReader, StreamResult, StreamWriter};

type ReadOp<T> = Pin<Box<dyn Future<Output = (StreamReader<T>, StreamResult, Vec<T>)>>>;
type WriteOp = Pin<Box<dyn Future<Output = (StreamWriter<u8>, Vec<u8>)>>>;

/// The in-flight read of a p3 stream.
pub struct ReadPump<T: 'static> {
    /// `None` while a read is in flight, and once the stream ended.
    rx: Option<StreamReader<T>>,
    op: Option<ReadOp<T>>,
    /// The capacity of each read.
    capacity: usize,
    eof: bool,
    /// The waker of the last non-blocking poll.
    pub driver: Option<Waker>,
}

impl<T: 'static> ReadPump<T> {
    /// A pump reading at most `capacity` values per read from `rx`.
    pub fn new(rx: StreamReader<T>, capacity: usize) -> Self {
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
    pub fn eof(&self) -> bool {
        self.eof
    }

    /// Drop the reading end and any read in flight. The pump then reports the stream ended.
    pub fn end(&mut self) {
        self.op = None;
        self.rx = None;
        self.eof = true;
    }

    /// Poll the in-flight read with `cx`, starting one if none is in flight. Returns the values
    /// read, which can be none, or `None` once the stream ended.
    pub fn poll_read(&mut self, cx: &mut Context<'_>) -> Poll<Option<Vec<T>>> {
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

/// The queued output of a p3 byte stream and its in-flight write.
pub struct WritePump {
    /// `None` while a write is in flight, once the reading end was dropped, and after
    /// [`WritePump::end`].
    tx: Option<StreamWriter<u8>>,
    /// Bytes not yet passed to a write.
    queued: Vec<u8>,
    op: Option<WriteOp>,
    /// The waker of the last non-blocking poll.
    pub driver: Option<Waker>,
}

impl WritePump {
    /// A pump writing to `tx`.
    pub fn new(tx: StreamWriter<u8>) -> Self {
        Self {
            tx: Some(tx),
            queued: Vec::new(),
            op: None,
            driver: None,
        }
    }

    /// The buffer to append outgoing bytes to, or `None` once the stream is closed.
    pub fn queue(&mut self) -> Option<&mut Vec<u8>> {
        (self.tx.is_some() || self.op.is_some()).then_some(&mut self.queued)
    }

    /// Whether bytes are queued or a write is in flight.
    pub fn is_busy(&self) -> bool {
        !self.queued.is_empty() || self.op.is_some()
    }

    /// Poll the in-flight write with `cx`, and write queued bytes until none are left. Queued
    /// bytes are discarded once the reading end was dropped.
    pub fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<()> {
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
    pub fn end(&mut self) {
        self.tx = None;
        self.op = None;
        self.queued.clear();
    }
}
