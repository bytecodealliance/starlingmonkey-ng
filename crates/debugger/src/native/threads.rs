// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! A connection over a blocking reader and writer, each served by its own thread.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;

use event_listener::Event;

use super::check;
use crate::connection::{Connection, LocalFuture, Received};
use crate::session::output_event;
use crate::transport::{read_message, write_message, FrameError, Sequencer};

/// Create a channel whose receiver can wait for a value in a future, without blocking its thread.
pub(crate) fn notifying_channel<T>() -> (NotifyingSender<T>, NotifyingReceiver<T>) {
    let (sender, receiver) = mpsc::channel();
    let ready = Arc::new(Event::new());
    (
        NotifyingSender {
            sender: Some(sender),
            ready: Arc::clone(&ready),
        },
        NotifyingReceiver {
            receiver,
            ready,
            peeked: RefCell::new(None),
        },
    )
}

/// The sending side of a [`notifying_channel`]. Dropping it closes the channel.
pub(crate) struct NotifyingSender<T> {
    /// `None` only while dropping.
    sender: Option<Sender<T>>,
    ready: Arc<Event>,
}

impl<T> NotifyingSender<T> {
    /// Queue `value`. Returns `false` if the receiver was dropped.
    pub(crate) fn send(&self, value: T) -> bool {
        let sent = self
            .sender
            .as_ref()
            .is_some_and(|sender| sender.send(value).is_ok());
        self.ready.notify(usize::MAX);
        sent
    }
}

impl<T> Drop for NotifyingSender<T> {
    fn drop(&mut self) {
        // The channel is closed before the notification, so a woken receiver sees it closed.
        drop(self.sender.take());
        self.ready.notify(usize::MAX);
    }
}

/// The receiving side of a [`notifying_channel`].
pub(crate) struct NotifyingReceiver<T> {
    receiver: Receiver<T>,
    /// Notified after each value is queued, and once when the channel closes.
    ready: Arc<Event>,
    /// A value [`NotifyingReceiver::ready`] took from `receiver`. The next receive returns it.
    peeked: RefCell<Option<T>>,
}

impl<T> NotifyingReceiver<T> {
    /// The next value, blocking until one arrives. `None` once the channel is closed and empty.
    pub(crate) fn recv_blocking(&self) -> Option<T> {
        self.peeked.take().or_else(|| self.receiver.recv().ok())
    }

    /// The next value if one has arrived.
    pub(crate) fn try_recv(&self) -> Result<T, TryRecvError> {
        match self.peeked.take() {
            Some(value) => Ok(value),
            None => self.receiver.try_recv(),
        }
    }

    /// Resolves once [`NotifyingReceiver::try_recv`] would return a value or report the channel
    /// closed.
    pub(crate) async fn ready(&self) {
        loop {
            // The listener exists before the channel is checked, so a value queued after the
            // check notifies it.
            let ready = self.ready.listen();
            if self.peek() {
                return;
            }
            ready.await;
        }
    }

    /// Whether a value has arrived and not been received yet.
    pub(crate) fn has_value(&self) -> bool {
        self.peek() && self.peeked.borrow().is_some()
    }

    /// Whether a receive would return without blocking. Moves an arrived value into `peeked`.
    fn peek(&self) -> bool {
        let mut peeked = self.peeked.borrow_mut();
        if peeked.is_some() {
            return true;
        }
        match self.receiver.try_recv() {
            Ok(value) => {
                *peeked = Some(value);
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => true,
        }
    }
}

/// A connection whose messages are read on one thread and written on another.
pub(crate) struct ThreadConnection {
    /// Incoming message bodies in arrival order. Closed once the reader reaches its end.
    incoming: NotifyingReceiver<String>,
    /// Outgoing message bodies without `seq`. `None` after `close`.
    outgoing: RefCell<Option<Sender<String>>>,
    writer_thread: RefCell<Option<JoinHandle<()>>>,
    /// `None` without a capture, and after `close`.
    capture: RefCell<Option<StdioCapture>>,
}

impl ThreadConnection {
    /// Start the reader and writer threads. With `capture`, what is written to fds 1 and 2 is
    /// sent as `output` events until `close`.
    pub(crate) fn new(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        capture: Option<StdioCapture>,
    ) -> io::Result<ThreadConnection> {
        let (outgoing, outgoing_rx) = mpsc::channel::<String>();
        let capture = capture
            .map(|capture| capture.forward(&outgoing))
            .transpose()?;
        let writer_thread = std::thread::Builder::new()
            .name("dap-writer".into())
            .spawn(move || write_loop(writer, outgoing_rx))?;

        let (incoming_tx, incoming) = notifying_channel();
        std::thread::Builder::new()
            .name("dap-reader".into())
            .spawn(move || read_loop(BufReader::new(reader), incoming_tx))?;

        Ok(ThreadConnection {
            incoming,
            outgoing: RefCell::new(Some(outgoing)),
            writer_thread: RefCell::new(Some(writer_thread)),
            capture: RefCell::new(capture),
        })
    }
}

impl Connection for ThreadConnection {
    fn recv_blocking(&self) -> Option<String> {
        self.incoming.recv_blocking()
    }

    fn try_recv(&self) -> Received {
        match self.incoming.try_recv() {
            Ok(message) => Received::Message(message),
            Err(TryRecvError::Empty) => Received::Empty,
            Err(TryRecvError::Disconnected) => Received::Closed,
        }
    }

    fn ready(&self) -> LocalFuture<'_> {
        Box::pin(self.incoming.ready())
    }

    /// Queue an outgoing message. Dropped after `close`.
    fn send(&self, body: String) {
        if let Some(outgoing) = &*self.outgoing.borrow() {
            let _ = outgoing.send(body);
        }
    }

    /// End the capture if there is one, send `last`, and wait until every queued message is
    /// written.
    fn close(&self, last: Vec<String>) {
        let capture = self.capture.borrow_mut().take();
        if let Some(capture) = capture {
            capture.stop();
        }
        for body in last {
            self.send(body);
        }
        drop(self.outgoing.borrow_mut().take());
        let writer_thread = self.writer_thread.borrow_mut().take();
        if let Some(thread) = writer_thread {
            let _ = thread.join();
        }
    }
}

/// File descriptors 1 and 2 redirected into pipes. Once a [`ThreadConnection`] forwards them,
/// what is written to them becomes `output` events in the `stdout` and `stderr` categories.
pub(crate) struct StdioCapture {
    /// The pipes' read ends and the category of their output, until forwarding starts.
    pipes: Vec<(File, &'static str)>,
    /// The read end of the pipe that `stop` writes to, which the forwarding threads poll.
    stop_requested: Arc<File>,
    /// The write end of that pipe.
    request_stop: File,
    /// One receiver per forwarding thread, which receives once the thread forwarded what its pipe
    /// held when `stop` wrote to `request_stop`.
    drained: Vec<Receiver<()>>,
}

impl StdioCapture {
    /// Redirect fds 1 and 2 into pipes.
    pub(crate) fn start() -> io::Result<StdioCapture> {
        let (stop_requested, request_stop) = pipe()?;
        Ok(StdioCapture {
            pipes: vec![(redirect(1)?, "stdout"), (redirect(2)?, "stderr")],
            stop_requested: Arc::new(stop_requested),
            request_stop,
            drained: Vec::new(),
        })
    }

    /// Start one thread per pipe that sends what arrives on it to `outgoing` as output events.
    fn forward(mut self, outgoing: &Sender<String>) -> io::Result<StdioCapture> {
        for (pipe, category) in std::mem::take(&mut self.pipes) {
            let outgoing = outgoing.clone();
            let stop_requested = Arc::clone(&self.stop_requested);
            let (drained, drained_rx) = mpsc::channel();
            std::thread::Builder::new()
                .name(format!("dap-{category}"))
                .spawn(move || {
                    forward_pipe(&pipe, &stop_requested, |text| {
                        outgoing.send(output_event(category, text)).is_ok()
                    });
                    drop(outgoing);
                    let _ = drained.send(());
                    // What arrives later is discarded. The pipe stays open, so writes to it by
                    // processes that inherited the redirected fds keep succeeding.
                    let mut buffer = [0; 4096];
                    while let Ok(1..) = read_pipe(&pipe, &mut buffer) {}
                })?;
            self.drained.push(drained_rx);
        }
        Ok(self)
    }

    /// Point fds 1 and 2 at `/dev/null`, and wait until the threads forwarded what the pipes
    /// held then. Child processes can hold write ends of the pipes, so the threads keep running
    /// and discard what arrives later.
    fn stop(self) {
        // SAFETY: flushing every C stdio stream has no precondition.
        unsafe { libc::fflush(std::ptr::null_mut()) };
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
        if let Ok(null) = OpenOptions::new().write(true).open("/dev/null") {
            // SAFETY: both descriptors are open.
            unsafe {
                libc::dup2(null.as_raw_fd(), 1);
                libc::dup2(null.as_raw_fd(), 2);
            }
        }
        let _ = (&self.request_stop).write_all(&[0]);
        for drained in self.drained {
            let _ = drained.recv();
        }
    }
}

/// Pass what arrives on `pipe` to `forward` until `forward` returns `false`, the pipe ends, or
/// `stop_requested` becomes readable. In the last case, what `pipe` holds then is passed on before
/// this returns.
fn forward_pipe(pipe: &File, stop_requested: &File, mut forward: impl FnMut(&str) -> bool) {
    let mut buffer = [0; 4096];
    let mut forwarding = true;
    let mut forward = |read: usize, buffer: &[u8]| {
        if forwarding {
            forwarding = forward(&String::from_utf8_lossy(&buffer[..read]));
        }
    };
    loop {
        let mut fds = [pipe.as_raw_fd(), stop_requested.as_raw_fd()].map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        // SAFETY: `fds` holds two valid `pollfd`s.
        if let Err(error) = check(unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) }) {
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if fds[0].revents != 0 {
            match read_pipe(pipe, &mut buffer) {
                Ok(read @ 1..) => forward(read, &buffer),
                _ => return,
            }
        } else if fds[1].revents != 0 {
            // SAFETY: `pipe` is open.
            let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
            // SAFETY: as above.
            unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
            while let Ok(read @ 1..) = read_pipe(pipe, &mut buffer) {
                forward(read, &buffer);
            }
            // SAFETY: as above.
            unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags) };
            return;
        }
    }
}

/// Read from `pipe`, retrying reads that a signal interrupted.
fn read_pipe(mut pipe: &File, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match pipe.read(buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

/// A new pipe's read and write ends, both closed in child processes.
fn pipe() -> io::Result<(File, File)> {
    let mut ends = [0; 2];
    // SAFETY: `ends` has room for the two descriptors `pipe` writes.
    check(unsafe { libc::pipe(ends.as_mut_ptr()) })?;
    // SAFETY: `pipe` just created both descriptors, and nothing else owns them.
    let ends = unsafe { (File::from_raw_fd(ends[0]), File::from_raw_fd(ends[1])) };
    for end in [&ends.0, &ends.1] {
        // SAFETY: the descriptor is open.
        check(unsafe { libc::fcntl(end.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
    }
    Ok(ends)
}

/// Redirect `fd` into a pipe, and return the pipe's read end. Child processes inherit `fd`.
fn redirect(fd: RawFd) -> io::Result<File> {
    let (read_end, write_end) = pipe()?;
    // SAFETY: both descriptors are open. After `dup2`, `fd` is the only write end, and `dup2`
    // clears `FD_CLOEXEC` on it.
    check(unsafe { libc::dup2(write_end.as_raw_fd(), fd) })?;
    Ok(read_end)
}

fn write_loop(mut out: impl Write, bodies: Receiver<String>) {
    let mut sequencer = Sequencer::default();
    for body in bodies {
        if write_message(&mut out, &sequencer.stamp(&body)).is_err() {
            return;
        }
    }
}

/// Pass the messages read from `reader` to `incoming` until the input ends or fails. A malformed
/// message is discarded, except one for which [`FrameError::ends_input`] is `true`, which ends the
/// input. Both are reported on stderr, as is input that ends inside a message or is not UTF-8.
fn read_loop(mut reader: impl BufRead, incoming: NotifyingSender<String>) {
    loop {
        let error = match read_message(&mut reader) {
            Ok(Some(body)) => {
                if !incoming.send(body) {
                    return;
                }
                continue;
            }
            Ok(None) => return,
            Err(error) => error,
        };
        match error
            .get_ref()
            .and_then(|error| error.downcast_ref::<FrameError>())
        {
            Some(&frame_error) if !frame_error.ends_input() => {
                eprintln!("debugger: discarded a malformed DAP message: {frame_error}");
                continue;
            }
            Some(_) => eprintln!("debugger: ended the connection: {error}"),
            None if matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
            ) =>
            {
                eprintln!("debugger: ended the connection: {error}");
            }
            None => {}
        }
        return;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::task::{Context, Wake, Waker};

    use super::*;
    use crate::transport::MAX_CONTENT_LENGTH;

    /// A writer that appends to a buffer the test reads afterwards.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<u8>>>);

    impl Write for Recorder {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Poll `future` to completion, parking the thread while it is pending.
    fn block_on(mut future: LocalFuture<'_>) {
        struct Unpark(std::thread::Thread);
        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        while future.as_mut().poll(&mut cx).is_pending() {
            std::thread::park();
        }
    }

    #[test]
    fn messages_arrive_in_order_and_close_writes_every_queued_message() {
        let mut wire = Vec::new();
        write_message(&mut wire, r#"{"a":1}"#).unwrap();
        write_message(&mut wire, r#"{"b":2}"#).unwrap();
        let written = Recorder::default();
        let connection =
            ThreadConnection::new(io::Cursor::new(wire), written.clone(), None).unwrap();

        assert_eq!(connection.recv_blocking().as_deref(), Some(r#"{"a":1}"#));
        block_on(connection.ready());
        assert!(matches!(connection.try_recv(), Received::Message(m) if m == r#"{"b":2}"#));
        block_on(connection.ready());
        assert!(matches!(connection.try_recv(), Received::Closed));
        assert_eq!(connection.recv_blocking(), None);

        connection.send(r#"{"event":"first"}"#.to_string());
        connection.close(vec![r#"{"event":"last"}"#.to_string()]);
        connection.send(r#"{"event":"dropped"}"#.to_string());

        let bytes = written.0.lock().unwrap().clone();
        let mut reader = BufReader::new(&bytes[..]);
        let mut messages = Vec::new();
        while let Some(body) = read_message(&mut reader).unwrap() {
            messages.push(serde_json::from_str::<serde_json::Value>(&body).unwrap());
        }
        assert_eq!(
            messages,
            [
                serde_json::json!({ "event": "first", "seq": 1 }),
                serde_json::json!({ "event": "last", "seq": 2 }),
            ]
        );
    }

    #[test]
    fn a_message_over_the_maximum_length_ends_the_connection() {
        let mut wire = Vec::new();
        write_message(&mut wire, r#"{"a":1}"#).unwrap();
        let oversized = format!("Content-Length: {}\r\n\r\n", MAX_CONTENT_LENGTH + 1);
        wire.extend_from_slice(oversized.as_bytes());
        write_message(&mut wire, r#"{"b":2}"#).unwrap();
        let connection = ThreadConnection::new(io::Cursor::new(wire), io::sink(), None).unwrap();

        assert_eq!(connection.recv_blocking().as_deref(), Some(r#"{"a":1}"#));
        assert_eq!(connection.recv_blocking(), None);
    }

    #[test]
    fn a_malformed_message_is_discarded_and_later_messages_arrive() {
        let mut wire = b"Content-Length: abc\r\n\r\n".to_vec();
        write_message(&mut wire, r#"{"a":1}"#).unwrap();
        wire.extend_from_slice(b"Content-Length: 1\r\n\r\n\xff");
        write_message(&mut wire, r#"{"b":2}"#).unwrap();
        let connection = ThreadConnection::new(io::Cursor::new(wire), io::sink(), None).unwrap();

        assert_eq!(connection.recv_blocking().as_deref(), Some(r#"{"a":1}"#));
        assert_eq!(connection.recv_blocking().as_deref(), Some(r#"{"b":2}"#));
        assert_eq!(connection.recv_blocking(), None);
    }

    #[test]
    fn a_message_peeked_by_ready_is_received_next() {
        let mut wire = Vec::new();
        write_message(&mut wire, r#"{"a":1}"#).unwrap();
        write_message(&mut wire, r#"{"b":2}"#).unwrap();
        let connection = ThreadConnection::new(io::Cursor::new(wire), io::sink(), None).unwrap();

        block_on(connection.ready());
        assert_eq!(connection.recv_blocking().as_deref(), Some(r#"{"a":1}"#));
        assert_eq!(connection.recv_blocking().as_deref(), Some(r#"{"b":2}"#));
        assert_eq!(connection.recv_blocking(), None);
    }
}
