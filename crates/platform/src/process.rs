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
/// Natively, a thread started on first use reads stdin into a channel, so a read that is
/// cancelled by dropping its future loses no input. The channel holds one chunk, and the thread
/// stops reading while it is full. On wasm, this reads the `wasi:cli` stdin stream, and a
/// cancelled read can lose the bytes it was receiving.
pub struct Stdin {
    #[cfg(not(target_arch = "wasm32"))]
    chunks: &'static std::sync::Mutex<futures_channel::mpsc::Receiver<std::io::Result<Vec<u8>>>>,
    #[cfg(target_arch = "wasm32")]
    stream: Option<wit_bindgen::StreamReader<u8>>,
}

impl Stdin {
    /// Standard input. On wasm, only the first `Stdin` receives input.
    pub fn open() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            use std::sync::{Mutex, OnceLock};
            type Chunks = Mutex<futures_channel::mpsc::Receiver<std::io::Result<Vec<u8>>>>;
            static CHUNKS: OnceLock<Chunks> = OnceLock::new();
            let chunks = CHUNKS.get_or_init(|| {
                let (mut sender, receiver) = futures_channel::mpsc::channel(0);
                std::thread::Builder::new()
                    .name("stdin".into())
                    .spawn(move || {
                        use std::io::Read;
                        let mut buffer = vec![0; 64 * 1024];
                        loop {
                            let chunk = match std::io::stdin().read(&mut buffer) {
                                Ok(0) => return,
                                Ok(count) => Ok(buffer[..count].to_vec()),
                                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                                    continue
                                }
                                Err(error) => Err(error),
                            };
                            let failed = chunk.is_err();
                            let sent = futures_lite::future::block_on(crate::http::send_on(
                                &mut sender,
                                chunk,
                            ));
                            if !sent || failed {
                                return;
                            }
                        }
                    })
                    .expect("spawning the stdin thread succeeds");
                Mutex::new(receiver)
            });
            Stdin { chunks }
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
            use futures_lite::StreamExt;
            let chunk = std::future::poll_fn(|cx| {
                self.chunks
                    .lock()
                    .expect("the stdin lock is never poisoned")
                    .poll_next(cx)
            });
            chunk.await.transpose()
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
