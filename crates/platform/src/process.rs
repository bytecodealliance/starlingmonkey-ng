// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The running process or instance.

/// The function [`exit`] calls first. Set by [`set_exit_hook`].
static EXIT_HOOK: std::sync::Mutex<Option<fn(u8)>> = std::sync::Mutex::new(None);

/// Make [`exit`] call `hook` with the exit code before it exits, replacing the hook set before.
/// The hook runs at most once: [`exit`] removes it before calling it.
pub fn set_exit_hook(hook: fn(u8)) {
    *EXIT_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
}

/// Exit the process (natively) or the instance (on wasm) with `code`, after calling the hook
/// [`set_exit_hook`] set.
///
/// Natively, this flushes stdout and stderr, then exits without running thread-local destructors
/// or `atexit` handlers: the runtime still holds engine state, such as rooted handles in
/// thread-locals, whose destructors fail once the engine's own thread-locals are gone.
pub fn exit(code: u8) -> ! {
    let hook = EXIT_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(hook) = hook {
        hook(code);
    }
    #[cfg(target_arch = "wasm32")]
    {
        wasip3::cli::exit::exit_with_code(code);
        unreachable!("`exit-with-code` returned")
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        // SAFETY: `_exit` ends the process at once, so nothing can observe state it leaves behind.
        unsafe { libc::_exit(code.into()) }
    }
}

/// Abort the process (natively) or trap the instance (on wasm).
///
/// Natively, this flushes stdout and stderr, then raises `SIGABRT` with its default action
/// restored.
pub fn abort() -> ! {
    #[cfg(target_arch = "wasm32")]
    {
        core::arch::wasm32::unreachable()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        // SAFETY: restoring the default action of `SIGABRT` and raising it end the process.
        unsafe {
            libc::signal(libc::SIGABRT, libc::SIG_DFL);
            libc::raise(libc::SIGABRT);
            libc::_exit(134)
        }
    }
}

/// The environment variables the host gives this process or instance.
///
/// On wasm, this queries the host on every call, so an instance resumed from a Wizer snapshot gets
/// its own environment rather than the one the snapshot was taken with.
pub fn env_vars() -> Vec<(String, String)> {
    #[cfg(target_arch = "wasm32")]
    {
        wasip3::cli::environment::get_environment()
    }
    #[cfg(not(target_arch = "wasm32"))]
    std::env::vars_os()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

/// Set the environment variable `key` to `value` in this process's environment, which child
/// processes inherit. Does nothing on wasm, where the host fixes the environment. Natively, this
/// must be called before other threads start.
pub fn set_env_var(key: &str, value: &str) {
    #[cfg(not(target_arch = "wasm32"))]
    std::env::set_var(key, value);
    #[cfg(target_arch = "wasm32")]
    let _ = (key, value);
}

/// The host's name, as `gethostname` reports it. `None` on wasm, on platforms without
/// `gethostname`, and when it fails.
pub fn hostname() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        // SAFETY: `buffer` is valid for writes of its length.
        let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
        if result != 0 {
            return None;
        }
        let end = buffer.iter().position(|&byte| byte == 0)?;
        Some(String::from_utf8_lossy(&buffer[..end]).into_owned())
    }
    #[cfg(not(unix))]
    None
}

/// The command-line arguments the host gives this process or instance, starting with the program
/// name.
///
/// On wasm, this queries the host on every call, as [`env_vars`] does.
pub fn args() -> Vec<String> {
    #[cfg(target_arch = "wasm32")]
    {
        wasip3::cli::environment::get_arguments()
    }
    #[cfg(not(target_arch = "wasm32"))]
    std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

/// The process's standard input, read asynchronously.
///
/// Natively, a thread started on first use reads one chunk of stdin for each read that finds no
/// chunk waiting, and reads nothing otherwise. A read that is cancelled by dropping its future
/// loses no input: the next read receives the chunk it requested. On wasm, this reads the
/// `wasi:cli` stdin stream, and a cancelled read can lose the bytes it was receiving.
pub struct Stdin {
    #[cfg(not(target_arch = "wasm32"))]
    reader: &'static std::sync::Mutex<StdinReader>,
    #[cfg(target_arch = "wasm32")]
    stream: Option<wit_bindgen::StreamReader<u8>>,
}

/// The chunk of standard input the stdin thread reads.
#[cfg(not(target_arch = "wasm32"))]
type Chunk = std::io::Result<Vec<u8>>;

/// The requests to the stdin thread, and the reply to the request in progress.
#[cfg(not(target_arch = "wasm32"))]
struct StdinReader {
    /// Each request is the sender of the reply, which is the next chunk. The thread drops the
    /// sender without a reply at the end of stdin.
    requests: std::sync::mpsc::Sender<futures_channel::oneshot::Sender<Chunk>>,
    reply: Option<futures_channel::oneshot::Receiver<Chunk>>,
}

impl Stdin {
    /// Standard input. On wasm, each `Stdin` opens its own `wasi:cli` stdin stream, and under
    /// wasmtime the streams of several `Stdin`s each receive a part of the input.
    pub fn open() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            use std::sync::{Mutex, OnceLock};
            static READER: OnceLock<Mutex<StdinReader>> = OnceLock::new();
            let reader = READER.get_or_init(|| {
                let (requests, received) =
                    std::sync::mpsc::channel::<futures_channel::oneshot::Sender<Chunk>>();
                std::thread::Builder::new()
                    .name("stdin".into())
                    .spawn(move || {
                        use std::io::Read;
                        let mut buffer = vec![0; 64 * 1024];
                        for reply in received {
                            let chunk = loop {
                                match std::io::stdin().read(&mut buffer) {
                                    Ok(0) => return,
                                    Ok(count) => break Ok(buffer[..count].to_vec()),
                                    Err(error)
                                        if error.kind() == std::io::ErrorKind::Interrupted => {}
                                    Err(error) => break Err(error),
                                }
                            };
                            let failed = chunk.is_err();
                            if reply.send(chunk).is_err() || failed {
                                return;
                            }
                        }
                    })
                    .expect("spawning the stdin thread succeeds");
                Mutex::new(StdinReader {
                    requests,
                    reply: None,
                })
            });
            Stdin { reader }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let (stream, _outcome) = wasip3::cli::stdin::read_via_stream();
            Stdin {
                stream: Some(stream),
            }
        }
    }

    /// The next bytes of standard input, or `None` at its end.
    pub async fn read(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            use std::future::Future;
            use std::task::Poll;
            std::future::poll_fn(|cx| {
                let mut reader = self
                    .reader
                    .lock()
                    .expect("the stdin lock is never poisoned");
                let StdinReader { requests, reply } = &mut *reader;
                let receiver = match reply {
                    Some(receiver) => receiver,
                    None => {
                        let (sender, receiver) = futures_channel::oneshot::channel();
                        if requests.send(sender).is_err() {
                            return Poll::Ready(Ok(None));
                        }
                        reply.insert(receiver)
                    }
                };
                let chunk = std::task::ready!(std::pin::Pin::new(receiver).poll(cx));
                *reply = None;
                Poll::Ready(chunk.ok().transpose())
            })
            .await
        }
        #[cfg(target_arch = "wasm32")]
        {
            while let Some(stream) = self.stream.as_mut() {
                let (result, bytes) = stream.read(Vec::with_capacity(64 * 1024)).await;
                if result == wit_bindgen::StreamResult::Dropped {
                    self.stream = None;
                }
                if !bytes.is_empty() {
                    return Ok(Some(bytes));
                }
            }
            Ok(None)
        }
    }
}
