// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Child processes: spawning them, waiting for them to exit, sending them signals, and the pipes
//! to their standard streams. Supported natively on Unix only. Elsewhere, and on wasm, where WASI
//! can't spawn processes, [`spawn`] and [`run_sync`] fail with [`std::io::ErrorKind::Unsupported`].
//!
//! A pipe is one end of a Unix socket pair, as libuv creates them, whose other end the child gets
//! as one of its descriptors.

use std::io;
use std::time::Duration;

#[cfg(all(unix, not(target_arch = "wasm32")))]
mod imp {
    pub use super::unix::*;
}
#[cfg(not(all(unix, not(target_arch = "wasm32"))))]
mod imp {
    pub use super::unsupported::*;
}

pub use imp::{kill, run_sync, spawn, Child, Pipe, PipeReader, PipeWriter};

/// Whether child processes can be spawned here.
pub const SUPPORTED: bool = cfg!(all(unix, not(target_arch = "wasm32")));

/// A spawned child process, or the reason spawning it failed, and the [`Pipe`] of each of its
/// descriptors that is a [`Stdio::Pipe`]. The pipes exist after the spawn failed too, as libuv's
/// do, and their other ends are closed then. They are missing if creating them failed.
pub struct Spawned {
    pub child: io::Result<Child>,
    pub pipes: Vec<Option<Pipe>>,
}

/// What a descriptor of a child process is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stdio {
    /// One end of a new socket pair, whose other end the parent gets as a [`Pipe`].
    Pipe,
    /// `/dev/null` for descriptors 0, 1 and 2, and closed for the others.
    Ignore,
    /// A duplicate of the parent's descriptor.
    Fd(i32),
}

/// How to spawn a child process.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The program, which is looked up in the `PATH` of `env` unless it contains a `/`.
    pub file: String,
    /// The arguments, starting with the program name the child gets as its `argv[0]`.
    pub args: Vec<String>,
    /// The working directory, or the parent's if `None`.
    pub cwd: Option<String>,
    /// The environment variables, which replace the parent's, or `None` for the parent's.
    pub env: Option<Vec<(String, String)>>,
    /// The child's descriptors, in order from 0.
    pub stdio: Vec<Stdio>,
    /// Whether the child leads a new session, detached from the parent's terminal.
    pub detached: bool,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

/// How a child process ended: its exit code, or the signal that ended it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// The limits and the input of [`run_sync`].
#[derive(Clone, Debug, Default)]
pub struct SyncOptions {
    /// The bytes to write to each descriptor that is a [`Stdio::Pipe`], by descriptor. A pipe
    /// without input is ended at once.
    pub input: Vec<Option<Vec<u8>>>,
    /// The time after which the child is sent `kill_signal`.
    pub timeout: Option<Duration>,
    /// The number of bytes the pipes may receive together before the child is sent
    /// `kill_signal`.
    pub max_buffer: Option<usize>,
    pub kill_signal: i32,
}

/// The outcome of [`run_sync`].
#[derive(Debug, Default)]
pub struct SyncOutput {
    pub pid: u32,
    /// How the child ended, or `None` if it couldn't be spawned.
    pub status: Option<ExitStatus>,
    /// The bytes each pipe received, by descriptor, for the descriptors that are
    /// [`Stdio::Pipe`]s other than descriptor 0.
    pub output: Vec<Option<Vec<u8>>>,
    /// Why spawning or running the child failed: the spawn error, `ETIMEDOUT` once the timeout
    /// passed, or `ENOBUFS` once the pipes received more than the maximum, or more than memory
    /// holds.
    pub error: Option<io::Error>,
}

#[cfg(all(unix, not(target_arch = "wasm32")))]
mod unix {
    use std::cell::RefCell;
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::rc::Rc;
    use std::time::Instant;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{ExitStatus, Options, Stdio, SyncOptions, SyncOutput};
    use crate::net::NetError;

    /// The size of each read from a pipe.
    const READ_SIZE: usize = 64 * 1024;

    /// A spawned child process, which stays unreaped until [`Child::exit`] observes its exit, so
    /// its process ID can't be reused before then.
    pub struct Child(Rc<RefCell<Inner>>);

    struct Inner {
        child: std::process::Child,
        /// The status, once the child was reaped.
        status: Option<ExitStatus>,
    }

    /// One end of a socket pair whose other end a child process has.
    pub struct Pipe(UnixStream);

    /// The reading half of a [`Pipe`]. It registers with tokio on its first read.
    pub struct PipeReader(Half<tokio::net::unix::OwnedReadHalf>);

    /// The writing half of a [`Pipe`]. It registers with tokio on its first write that has to
    /// wait.
    pub struct PipeWriter(Half<tokio::net::unix::OwnedWriteHalf>);

    /// A half of a pipe: a descriptor of the pipe's socket until its first use in a tokio
    /// runtime, then that half of the registered stream, which owns the same descriptor.
    enum Half<T> {
        Unregistered(UnixStream),
        Registered(T),
        /// Registering the half with tokio failed, which closed its descriptor.
        Closed,
    }

    impl<T: AsRef<tokio::net::UnixStream>> Half<T> {
        /// The half's descriptor, or `EBADF` once it is closed.
        fn fd(&self) -> io::Result<RawFd> {
            match self {
                Half::Unregistered(stream) => Ok(stream.as_raw_fd()),
                Half::Registered(half) => Ok(half.as_ref().as_raw_fd()),
                Half::Closed => Err(io::Error::from_raw_os_error(libc::EBADF)),
            }
        }

        /// Register the half with tokio, unless it is registered, as the half that `split` takes
        /// from the registered stream. The socket is made non-blocking first, since a child
        /// process that got it as a standard stream makes it blocking.
        fn register(&mut self, split: impl FnOnce(tokio::net::UnixStream) -> T) -> io::Result<()> {
            if let Half::Unregistered(stream) = self {
                stream.set_nonblocking(true)?;
                let Half::Unregistered(stream) = std::mem::replace(self, Half::Closed) else {
                    unreachable!("the half is unregistered");
                };
                *self = Half::Registered(split(tokio::net::UnixStream::from_std(stream)?));
            }
            Ok(())
        }
    }

    fn convert(status: std::process::ExitStatus) -> ExitStatus {
        ExitStatus {
            code: status.code(),
            signal: status.signal(),
        }
    }

    impl Child {
        pub fn pid(&self) -> u32 {
            self.0.borrow().child.id()
        }

        /// Send `signal` to the child, or check that it exists for signal 0. Fails with `ESRCH`
        /// once the child was reaped.
        pub fn kill(&self, signal: i32) -> io::Result<()> {
            let inner = self.0.borrow();
            if inner.status.is_some() {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            let pid = inner.child.id() as libc::pid_t;
            // SAFETY: the child is unreaped, so `pid` names it.
            if unsafe { libc::kill(pid, signal) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// The child's exit, which reaps it. Must be polled inside a tokio runtime.
        pub fn exit(&self) -> impl std::future::Future<Output = io::Result<ExitStatus>> + 'static {
            let inner = self.0.clone();
            async move {
                use tokio::signal::unix::{signal, SignalKind};
                let mut exits = signal(SignalKind::child())?;
                loop {
                    let reaped = {
                        let mut inner = inner.borrow_mut();
                        if let Some(status) = inner.status {
                            return Ok(status);
                        }
                        inner.child.try_wait()?.map(convert)
                    };
                    if let Some(status) = reaped {
                        inner.borrow_mut().status = Some(status);
                        return Ok(status);
                    }
                    exits.recv().await;
                }
            }
        }
    }

    impl Pipe {
        /// The pipe on the descriptor `fd`, which a parent process gave this process, as the
        /// channel of a process that `fork` spawned. Fails with `EINVAL` unless `fd` is a socket.
        pub fn open(fd: RawFd) -> io::Result<Pipe> {
            // SAFETY: `fstat` writes the status of an open descriptor to `status`.
            let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
            if unsafe { libc::fstat(fd, &mut status) } != 0 {
                return Err(io::Error::last_os_error());
            }
            if status.st_mode & libc::S_IFMT != libc::S_IFSOCK {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            // SAFETY: `fd` is an open socket that nothing else in this process owns.
            let stream = unsafe { UnixStream::from_raw_fd(fd) };
            stream.set_nonblocking(true)?;
            // SAFETY: setting `FD_CLOEXEC` reads no memory.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
            Ok(Pipe(stream))
        }

        /// The pipe's descriptor, which stays open while the [`PipeReader`] of [`Pipe::split`]
        /// exists.
        pub fn as_raw_fd(&self) -> RawFd {
            self.0.as_raw_fd()
        }

        /// The halves of the pipe, which share its socket.
        pub fn split(self) -> io::Result<(PipeReader, PipeWriter)> {
            let writer = self.0.try_clone()?;
            Ok((
                PipeReader(Half::Unregistered(self.0)),
                PipeWriter(Half::Unregistered(writer)),
            ))
        }
    }

    impl PipeReader {
        /// The next bytes the child sent, or `None` once it ended its side. Bytes that already
        /// arrived are read at once, before the half registers with tokio or waits for its
        /// readiness, so the future is ready on its first poll. The bytes are returned in a buffer
        /// of capacity [`READ_SIZE`].
        pub async fn read(&mut self) -> Result<Option<Vec<u8>>, NetError> {
            let fd = self.0.fd()?;
            let mut buffer: Vec<u8> = Vec::with_capacity(READ_SIZE);
            // SAFETY: `fd` is an open socket for the duration of the call, and `buffer` has
            // `READ_SIZE` bytes of spare capacity.
            let count = unsafe {
                libc::recv(
                    fd,
                    buffer.as_mut_ptr().cast(),
                    READ_SIZE,
                    libc::MSG_DONTWAIT,
                )
            };
            let count = match usize::try_from(count) {
                Ok(count) => {
                    // SAFETY: `recv` initialized the first `count` bytes.
                    unsafe { buffer.set_len(count) };
                    count
                }
                Err(_) => {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::WouldBlock
                        && error.kind() != io::ErrorKind::Interrupted
                    {
                        return Err(error.into());
                    }
                    self.0.register(|stream| {
                        // The unused writing half doesn't shut the socket's sending side down
                        // when dropped.
                        let (read, write) = stream.into_split();
                        write.forget();
                        read
                    })?;
                    let Half::Registered(read) = &mut self.0 else {
                        unreachable!("the half was registered above")
                    };
                    read.read_buf(&mut buffer).await?
                }
            };
            if count == 0 {
                return Ok(None);
            }
            Ok(Some(buffer))
        }
    }

    impl PipeWriter {
        /// Send as much of `data` as the pipe takes without waiting, and return the bytes not
        /// sent. An error sends nothing, and the next [`PipeWriter::write`] reports it.
        pub fn try_write(&mut self, mut data: Vec<u8>) -> Vec<u8> {
            let Ok(fd) = self.0.fd() else {
                return data;
            };
            // SAFETY: `fd` is an open socket for the duration of the call, and `data` is valid
            // for its length.
            let written = unsafe { libc::send(fd, data.as_ptr().cast(), data.len(), send_flags()) };
            data.drain(..usize::try_from(written).unwrap_or(0));
            data
        }

        /// Send all of `data`. Must be awaited inside a tokio runtime.
        pub async fn write(&mut self, data: Vec<u8>) -> Result<(), NetError> {
            self.0.register(|stream| stream.into_split().1)?;
            let Half::Registered(write) = &mut self.0 else {
                unreachable!("the half was registered above")
            };
            Ok(write.write_all(&data).await?)
        }

        /// End the parent's sending side of the pipe.
        pub async fn shutdown(&mut self) -> Result<(), NetError> {
            let fd = self.0.fd()?;
            // SAFETY: the half owns `fd` for the duration of the call.
            if unsafe { libc::shutdown(fd, libc::SHUT_WR) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ENOTCONN) {
                    return Err(error.into());
                }
            }
            Ok(())
        }
    }

    /// The flags of a `send` that doesn't wait and doesn't raise `SIGPIPE`.
    fn send_flags() -> libc::c_int {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            libc::MSG_DONTWAIT
        }
    }

    /// A socket pair whose first end, the parent's, doesn't block, and doesn't raise `SIGPIPE` on
    /// Apple platforms.
    fn socket_pair() -> io::Result<(UnixStream, UnixStream)> {
        let (ours, theirs) = UnixStream::pair()?;
        ours.set_nonblocking(true)?;
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
        {
            let on: libc::c_int = 1;
            // SAFETY: `ours` is an open socket, and `on` outlives the call.
            unsafe {
                libc::setsockopt(
                    ours.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_NOSIGPIPE,
                    (&on as *const libc::c_int).cast(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }
        Ok((ours, theirs))
    }

    /// A duplicate of `fd` at `min` or above, closed on exec.
    fn duplicate(fd: RawFd, min: RawFd) -> io::Result<OwnedFd> {
        // SAFETY: `fcntl` with `F_DUPFD_CLOEXEC` reads no memory, and a new descriptor it returns
        // is owned by no one else.
        let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, min) };
        if new < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `new` is a new descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(new) })
    }

    /// Whether `fd` is open in this process.
    fn is_open(fd: RawFd) -> bool {
        // SAFETY: `F_GETFD` reads no memory.
        unsafe { libc::fcntl(fd, libc::F_GETFD) >= 0 }
    }

    /// Spawn a child process as `options` describe it.
    pub fn spawn(options: &Options) -> super::Spawned {
        let mut pipes = Vec::with_capacity(options.stdio.len());
        let child = spawn_with(options, &mut pipes);
        super::Spawned { child, pipes }
    }

    /// The body of [`spawn`], which pushes the pipes it creates to `pipes`.
    fn spawn_with(options: &Options, pipes: &mut Vec<Option<Pipe>>) -> io::Result<Child> {
        let mut command = std::process::Command::new(&options.file);
        if let Some(argv0) = options.args.first() {
            command.arg0(argv0);
        }
        command.args(options.args.iter().skip(1));
        if let Some(env) = &options.env {
            command.env_clear();
            command.envs(env.iter().map(|(key, value)| (key, value)));
        }
        if let Some(cwd) = &options.cwd {
            command.current_dir(cwd);
        }
        if let Some(uid) = options.uid {
            command.uid(uid);
        }
        if let Some(gid) = options.gid {
            command.gid(gid);
        }
        let count = options.stdio.len().max(3);
        let high = RawFd::try_from(count).unwrap_or(RawFd::MAX);
        // The descriptors above 2: each source, a duplicate closed on exec, and its target.
        let mut extra: Vec<(OwnedFd, RawFd)> = Vec::new();
        for (target, spec) in options.stdio.iter().enumerate() {
            let target = target as RawFd;
            let source = match *spec {
                Stdio::Pipe => {
                    let (ours, theirs) = socket_pair()?;
                    pipes.push(Some(Pipe(ours)));
                    Some(duplicate(theirs.as_raw_fd(), high)?)
                }
                Stdio::Ignore => {
                    pipes.push(None);
                    None
                }
                Stdio::Fd(fd) => {
                    pipes.push(None);
                    if fd == target && target <= 2 {
                        continue;
                    }
                    Some(duplicate(fd, high)?)
                }
            };
            if target > 2 {
                extra.extend(source.map(|source| (source, target)));
                continue;
            }
            let stdio = match source {
                Some(source) => std::process::Stdio::from(source),
                None => std::process::Stdio::null(),
            };
            match target {
                0 => command.stdin(stdio),
                1 => command.stdout(stdio),
                _ => command.stderr(stdio),
            };
        }
        // Each target above 2 is held open while spawning, so that no descriptor the spawn
        // creates in the parent, such as the one reporting exec errors, has its number.
        let mut placeholders = Vec::new();
        for (_, target) in &extra {
            if !is_open(*target) {
                let null = std::fs::File::open("/dev/null")?;
                // SAFETY: `dup2` onto a closed descriptor reads no memory, and the new
                // descriptor is owned by the placeholder.
                if unsafe { libc::dup2(null.as_raw_fd(), *target) } >= 0 {
                    // SAFETY: `target` is the new descriptor.
                    let placeholder = unsafe { OwnedFd::from_raw_fd(*target) };
                    // SAFETY: setting `FD_CLOEXEC` reads no memory.
                    unsafe { libc::fcntl(*target, libc::F_SETFD, libc::FD_CLOEXEC) };
                    placeholders.push(placeholder);
                }
            }
        }
        let detached = options.detached;
        let moves: Vec<(RawFd, RawFd)> = extra
            .iter()
            .map(|(source, target)| (source.as_raw_fd(), *target))
            .collect();
        // The child starts with the default action for every signal, so it doesn't inherit the
        // signals the parent ignores.
        // SAFETY: the closure only calls `signal`, `setsid`, `dup2` and `fcntl`, which are
        // async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                for signal in 1..32 {
                    if signal != libc::SIGKILL && signal != libc::SIGSTOP {
                        libc::signal(signal, libc::SIG_DFL);
                    }
                }
                if detached && libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                for &(source, target) in &moves {
                    if libc::dup2(source, target) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                // The child's standard streams block, as libuv makes them, even where the parent
                // shares one that doesn't, as the end of a pipe it reads from.
                for fd in 0..=2 {
                    let flags = libc::fcntl(fd, libc::F_GETFL);
                    if flags >= 0 && flags & libc::O_NONBLOCK != 0 {
                        libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
                    }
                }
                Ok(())
            });
        }
        let child = command.spawn();
        drop(placeholders);
        let inner = Inner {
            child: child?,
            status: None,
        };
        Ok(Child(Rc::new(RefCell::new(inner))))
    }

    /// Send `signal` to the process `pid`, or check that it exists for signal 0. A `pid` of 0 or
    /// below names a process group, as for `kill(2)`.
    pub fn kill(pid: i32, signal: i32) -> io::Result<()> {
        // SAFETY: `kill` reads no memory.
        if unsafe { libc::kill(pid, signal) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Spawn a child process as `options` describe it, write the input of `sync` to its pipes,
    /// read what it writes to them, and wait for it to exit, blocking the thread.
    pub fn run_sync(options: &Options, sync: SyncOptions) -> SyncOutput {
        let super::Spawned { child, pipes } = spawn(options);
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                return SyncOutput {
                    error: Some(error),
                    ..SyncOutput::default()
                }
            }
        };
        let pid = child.pid();
        let deadline = sync.timeout.map(|timeout| Instant::now() + timeout);
        let mut output: Vec<Option<Vec<u8>>> = Vec::with_capacity(pipes.len());
        // Each open pipe: its descriptor, the input still to write, and whether it still reads.
        let mut open: Vec<(usize, UnixStream, Option<Vec<u8>>, bool)> = Vec::new();
        for (index, pipe) in pipes.into_iter().enumerate() {
            let Some(Pipe(stream)) = pipe else {
                output.push(None);
                continue;
            };
            let input = sync
                .input
                .get(index)
                .cloned()
                .flatten()
                .filter(|input| !input.is_empty());
            let reads = index != 0;
            output.push(reads.then(Vec::new));
            if input.is_none() {
                let _ = stream.shutdown(std::net::Shutdown::Write);
            }
            open.push((index, stream, input, reads));
        }
        let mut error: Option<io::Error> = None;
        // Send the kill signal once, and record `error_code` as the error.
        let kill = |error_code: i32, error: &mut Option<io::Error>| {
            if error.is_none() {
                let _ = child.kill(sync.kill_signal);
                *error = Some(io::Error::from_raw_os_error(error_code));
            }
        };
        let mut buffer = vec![0; READ_SIZE];
        // The bytes all pipes received.
        let mut total = 0;
        while open
            .iter()
            .any(|(_, _, input, reads)| input.is_some() || *reads)
        {
            let mut fds: Vec<libc::pollfd> = open
                .iter()
                .map(|(_, stream, input, reads)| libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: (if *reads { libc::POLLIN } else { 0 })
                        | (if input.is_some() { libc::POLLOUT } else { 0 }),
                    revents: 0,
                })
                .collect();
            let wait = match deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        kill(libc::ETIMEDOUT, &mut error);
                        break;
                    }
                    i32::try_from(left.as_millis().max(1)).unwrap_or(i32::MAX)
                }
                None => -1,
            };
            // SAFETY: `fds` holds `fds.len()` entries for the duration of the call.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, wait) };
            if ready < 0 {
                let failure = io::Error::last_os_error();
                if failure.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                kill(failure.raw_os_error().unwrap_or(libc::EIO), &mut error);
                break;
            }
            let mut exceeded = false;
            for (entry, polled) in open.iter_mut().zip(&fds) {
                let (index, stream, input, reads) = entry;
                if polled.revents == 0 {
                    continue;
                }
                if let Some(bytes) = input {
                    match (&*stream).write(bytes) {
                        Ok(written) => {
                            bytes.drain(..written);
                        }
                        Err(failure) if failure.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => bytes.clear(),
                    }
                    if bytes.is_empty() {
                        *input = None;
                        let _ = stream.shutdown(std::net::Shutdown::Write);
                    }
                }
                if *reads && polled.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    match (&*stream).read(&mut buffer) {
                        Ok(0) => *reads = false,
                        Ok(count) => {
                            let received = output[*index].get_or_insert_with(Vec::new);
                            if received.try_reserve(count).is_err() {
                                exceeded = true;
                            } else {
                                received.extend_from_slice(&buffer[..count]);
                                total += count;
                                exceeded |= sync.max_buffer.is_some_and(|max| total > max);
                            }
                        }
                        Err(failure) if failure.kind() == io::ErrorKind::WouldBlock => {}
                        Err(failure) if failure.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => *reads = false,
                    }
                }
            }
            if exceeded {
                kill(libc::ENOBUFS, &mut error);
                break;
            }
            open.retain(|(_, _, input, reads)| input.is_some() || *reads);
        }
        drop(open);
        let status = loop {
            let status = match deadline {
                Some(deadline) if error.is_none() => {
                    let polled = child.0.borrow_mut().child.try_wait();
                    match polled {
                        Ok(Some(status)) => Some(convert(status)),
                        Ok(None) if Instant::now() >= deadline => {
                            kill(libc::ETIMEDOUT, &mut error);
                            continue;
                        }
                        Ok(None) => {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                            continue;
                        }
                        Err(_) => None,
                    }
                }
                _ => child.0.borrow_mut().child.wait().ok().map(convert),
            };
            break status;
        };
        SyncOutput {
            pid,
            status,
            output,
            error,
        }
    }
}

#[cfg(not(all(unix, not(target_arch = "wasm32"))))]
mod unsupported {
    use std::io;

    use super::{ExitStatus, Options, SyncOptions, SyncOutput};
    use crate::net::NetError;

    fn unsupported() -> io::Error {
        io::ErrorKind::Unsupported.into()
    }

    /// A child process, which can't exist here.
    pub enum Child {}

    /// A pipe to a child process, which can't exist here.
    pub enum Pipe {}

    /// The reading half of a [`Pipe`].
    pub enum PipeReader {}

    /// The writing half of a [`Pipe`].
    pub enum PipeWriter {}

    impl Child {
        pub fn pid(&self) -> u32 {
            match *self {}
        }

        pub fn kill(&self, _signal: i32) -> io::Result<()> {
            match *self {}
        }

        pub fn exit(&self) -> std::future::Ready<io::Result<ExitStatus>> {
            match *self {}
        }
    }

    impl Pipe {
        /// Fails: there are no pipes here.
        pub fn open(_fd: i32) -> io::Result<Pipe> {
            Err(unsupported())
        }

        pub fn as_raw_fd(&self) -> i32 {
            match *self {}
        }

        pub fn split(self) -> io::Result<(PipeReader, PipeWriter)> {
            match self {}
        }
    }

    impl PipeReader {
        pub async fn read(&mut self) -> Result<Option<Vec<u8>>, NetError> {
            match *self {}
        }
    }

    impl PipeWriter {
        pub fn try_write(&mut self, _data: Vec<u8>) -> Vec<u8> {
            match *self {}
        }

        pub async fn write(&mut self, _data: Vec<u8>) -> Result<(), NetError> {
            match *self {}
        }

        pub async fn shutdown(&mut self) -> Result<(), NetError> {
            match *self {}
        }
    }

    /// Fails: child processes aren't supported here.
    pub fn spawn(_options: &Options) -> super::Spawned {
        super::Spawned {
            child: Err(unsupported()),
            pipes: Vec::new(),
        }
    }

    /// Fails: there are no other processes to signal here.
    pub fn kill(_pid: i32, _signal: i32) -> io::Result<()> {
        Err(unsupported())
    }

    /// Fails: child processes aren't supported here.
    pub fn run_sync(_options: &Options, _sync: SyncOptions) -> SyncOutput {
        SyncOutput {
            error: Some(unsupported()),
            ..SyncOutput::default()
        }
    }
}
