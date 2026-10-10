// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The debug session: the source of its clients, and for each client the debugger realm, the
//! adapter evaluated in it, and the state the host functions share.

use std::cell::{Cell, RefCell};
use std::ffi::c_char;
use std::future::Future;
use std::net::SocketAddr;
use std::ptr::NonNull;
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::Duration;

use core_runtime::config::{DebugConfig, DebugTransport, RuntimeConfig};
use core_runtime::event_loop::{
    blocking_allowed, run_microtasks, run_until, set_suspended, with_event_loop, EventLoop,
};
use core_runtime::invocation::InvocationState;
use core_runtime::runtime::{register_before_content_hook, register_shutdown_hook, Runtime};
use js::gc::handle::RootedHeap;
use js::gc::scope::{OnNewGlobalHookOption, RootScope, Scope};
use serde_json::json;

use crate::connection::{Connection, Listener, LocalFuture, Received};
#[cfg(not(target_arch = "wasm32"))]
use crate::native::tcp;
#[cfg(target_arch = "wasm32")]
use crate::wasm::tcp;

/// The adapter bundle `just build-debugger-adapter` produces.
const ADAPTER_SOURCE: &str = include_str!("../adapter/dist/adapter.js");

/// Passed to the adapter when the client closes the connection without sending `disconnect`.
const DISCONNECT: &str = r#"{"type":"request","seq":0,"command":"disconnect","arguments":{}}"#;

/// The source of a session's clients.
pub(crate) enum Source {
    /// The process's stdin and stdout.
    Stdio,
    /// Clients that connect to a listener.
    Listen(Box<dyn Listener>),
    /// A client the runtime connects to.
    Connect {
        host: String,
        port: u16,
        timeout: Duration,
    },
}

pub(crate) struct Session {
    source: Source,
    client: RefCell<Option<Rc<Client>>>,
    /// Woken when a client is attached, and when the attached client detaches.
    client_waker: RefCell<Option<Waker>>,
    /// Set when a `connect` session reported running without debugging, and cleared when a client
    /// configures the session.
    undebugged_reported: Cell<bool>,
}

/// A DAP client attached to the session.
pub(crate) struct Client {
    connection: Box<dyn Connection>,
    /// The debugger realm's global. `None` once the runtime's shutdown hook released it.
    global: RefCell<Option<RootedHeap<js::object::Object>>>,
    paused: Cell<bool>,
    resume_requested: Cell<bool>,
    configured: Cell<bool>,
    disconnected: Cell<bool>,
    /// Set once the client detached with a `disconnect` request.
    detached: Cell<bool>,
}

js::instance_local! {
    static SESSION: RefCell<Option<Rc<Session>>> = const { RefCell::new(None) };

    /// Set while a snapshot is taken with debugging enabled, so it is set in the snapshot. See
    /// [`allow_debugging_in_snapshot`].
    static DEBUGGING_ALLOWED: Cell<bool> = const { Cell::new(false) };

    /// Set by the first `start_after_resume` call.
    static RESUME_CHECKED: Cell<bool> = const { Cell::new(false) };

    /// Set when a session starts, and cleared by `before_export`.
    static STARTED_SINCE_EXPORT: Cell<bool> = const { Cell::new(false) };

    /// Set once `before_export` spawned the running-mode task.
    static RUNNING_TASK_SPAWNED: Cell<bool> = const { Cell::new(false) };

    /// The debugger's event loop (see [`debugger_loop`]). Registered for tracing with the runtime
    /// passed to [`debugger_loop`] until [`release`] runs.
    static DEBUGGER_LOOP: RefCell<Option<Rc<InvocationState>>> = const { RefCell::new(None) };
}

/// The active session. The `Rc` is a clone, so no borrow of the session slot is held while the
/// adapter runs and calls host functions.
pub(crate) fn current_session() -> Option<Rc<Session>> {
    SESSION.with(|session| session.borrow().clone())
}

/// The active session's client. The `Rc` is a clone, like [`current_session`]'s.
pub(crate) fn current_client() -> Option<Rc<Client>> {
    current_session().and_then(|session| session.client.borrow().clone())
}

/// Start a session before the content script when `--debug` is set, and release the debugger
/// global and the debugger's event loop when the runtime is dropped. A session that fails to start
/// fails the run.
pub fn register() {
    register_before_content_hook(before_content);
    register_shutdown_hook(release);
}

/// [`register`] for a server: a session that fails to start is reported on stderr, and the server
/// runs without debugging.
pub fn register_for_serve() {
    register_before_content_hook(before_serving);
    register_shutdown_hook(release);
}

fn before_serving(runtime: &Runtime, config: &RuntimeConfig) -> Result<(), String> {
    if let Err(error) = before_content(runtime, config) {
        eprintln!("debugger: {error}. Serving without debugging.");
    }
    Ok(())
}

/// Start a session from `--debug`. While a snapshot is taken, this only allows debugging in the
/// instances resumed from it.
fn before_content(runtime: &Runtime, config: &RuntimeConfig) -> Result<(), String> {
    match &config.debug {
        Some(_) if config.pre_initialize => {
            allow_debugging_in_snapshot();
            Ok(())
        }
        Some(debug) => start(runtime, debug),
        None => Ok(()),
    }
}

/// Let instances resumed from the snapshot being taken start a session from `STARLING_DEBUG`
/// (see `start_after_resume`).
pub fn allow_debugging_in_snapshot() {
    DEBUGGING_ALLOWED.with(|allowed| allowed.set(true));
}

/// Start a session from the `STARLING_DEBUG` environment variable in an instance resumed from a
/// snapshot that [`allow_debugging_in_snapshot`] allowed debugging in. Does nothing after the first
/// call, without that permission, or without the variable.
///
/// The variable is read from `wasi:cli/environment`, since the environment libc holds is the one
/// captured in the snapshot. A value that does not parse as a `--debug` configuration, and a
/// session that fails to start, are reported on stderr, and the instance runs without a session.
#[cfg(target_arch = "wasm32")]
pub fn start_after_resume(runtime: &Runtime) {
    if RESUME_CHECKED.with(|checked| checked.replace(true)) || !DEBUGGING_ALLOWED.with(Cell::get) {
        return;
    }
    let Some((_, value)) = wasip3::cli::environment::get_environment()
        .into_iter()
        .find(|(name, _)| name == "STARLING_DEBUG")
    else {
        return;
    };
    let started = value
        .parse::<DebugConfig>()
        .map_err(|error| format!("STARLING_DEBUG is invalid: {error}"))
        .and_then(|debug| start(runtime, &debug));
    if let Err(error) = started {
        eprintln!("debugger: {error}. Running without debugging.");
    }
}

/// Prepare the debugger for an export call of a wasm instance, before the call runs any JS.
///
/// 1. In an instance resumed from a snapshot, the first call starts a session (see
///    [`start_after_resume`]).
/// 2. A `connect` session without a client connects (see [`before_request`]), unless the session
///    started since the last call. Starting a `connect` session makes the first attempt.
/// 3. Once a session exists, a task is spawned that runs for the instance's lifetime. It passes
///    the messages arriving while nothing is paused to the adapter, with the debugger's event loop
///    active, and drives that loop (see [`RunningLoop::Debugger`]).
///
/// # Safety
///
/// `raw_cx` must be valid for the instance's lifetime, with the default global's realm entered
/// whenever the spawned task is polled.
#[cfg(target_arch = "wasm32")]
pub unsafe fn before_export(runtime: &Rc<Runtime>, raw_cx: *mut js::native::RawJSContext) {
    start_after_resume(runtime);
    if !STARTED_SINCE_EXPORT.with(|started| started.replace(false)) {
        before_request(runtime);
    }
    if current_session().is_none() || RUNNING_TASK_SPAWNED.with(|spawned| spawned.replace(true)) {
        return;
    }
    // The task holds the debugger's event loop, which `release` unregisters when the runtime is
    // dropped, so it holds the runtime too.
    let runtime = Rc::clone(runtime);
    wit_bindgen::spawn_local(async move {
        // SAFETY: guaranteed by this function's caller.
        unsafe {
            serve_while_running(raw_cx, RunningLoop::Debugger, platform::clock::sleep).await;
        }
        drop(runtime);
    });
}

/// The debugger's event loop, created and registered with `runtime` for tracing on the first
/// call. Messages a client sends before `configurationDone` are dispatched with it active, as are
/// running-mode messages with [`RunningLoop::Debugger`].
fn debugger_loop(runtime: &Runtime) -> Rc<InvocationState> {
    DEBUGGER_LOOP.with(|slot| {
        let mut slot = slot.borrow_mut();
        let state = slot.get_or_insert_with(|| {
            let state = Rc::new(InvocationState::new());
            // SAFETY: the `Rc` keeps the state at this address, and `release` unregisters it
            // before dropping the slot's reference.
            unsafe { runtime.register_invocation(&state) };
            state
        });
        Rc::clone(state)
    })
}

/// Release the debugger global, and unregister the debugger's event loop and cancel its pending
/// futures. Runs when `runtime` is dropped, while the context is alive.
fn release(runtime: &Runtime) {
    if let Some(client) = current_client() {
        client.global.borrow_mut().take();
    }
    if let Some(state) = DEBUGGER_LOOP.with(|slot| slot.borrow_mut().take()) {
        runtime.unregister_invocation(&state);
        state.event_loop().cancel_pending_futures();
    }
}

fn start(runtime: &Runtime, debug: &DebugConfig) -> Result<(), String> {
    let (source, connection): (_, Option<Box<dyn Connection>>) = match &debug.transport {
        DebugTransport::Stdio => {
            #[cfg(not(target_arch = "wasm32"))]
            let connection = crate::native::stdio::connect()
                .map_err(|e| format!("Starting the debugger's stdio transport failed: {e}"))?;
            #[cfg(target_arch = "wasm32")]
            let connection = crate::wasm::stdio::connect();
            (Source::Stdio, Some(Box::new(connection)))
        }
        DebugTransport::Listen { addr, port, wait } => {
            let (listener, bound) = tcp::listen(*addr, *port).map_err(|e| {
                let addr = SocketAddr::from((*addr, *port));
                format!("Binding the debug listener to {addr} failed: {e}")
            })?;
            eprintln!("debugger listening on {bound}");
            let connection = if *wait {
                let connection = listener
                    .accept_blocking()
                    .ok_or("Accepting a debug client failed")?;
                Some(connection)
            } else {
                None
            };
            (Source::Listen(Box::new(listener)), connection)
        }
        DebugTransport::Connect {
            host,
            port,
            timeout,
        } => (
            Source::Connect {
                host: host.clone(),
                port: *port,
                timeout: *timeout,
            },
            None,
        ),
    };
    let session = Rc::new(Session {
        source,
        client: RefCell::new(None),
        client_waker: RefCell::new(None),
        undebugged_reported: Cell::new(false),
    });
    SESSION.with(|slot| *slot.borrow_mut() = Some(Rc::clone(&session)));
    platform::process::set_exit_hook(finish_on_exit);
    STARTED_SINCE_EXPORT.with(|started| started.set(true));
    let debugger_loop = debugger_loop(runtime);
    if let Some(connection) = connection.or_else(|| session.connect()) {
        let waiting = Some(debugger_loop.event_loop());
        match attach_client(&runtime.default_global(), connection, waiting) {
            Ok(client) => session.check_configured(&client),
            Err(error) => {
                // A start that fails leaves no session behind.
                let session = SESSION.with(|slot| slot.borrow_mut().take());
                drop(session);
                return Err(error);
            }
        }
    }
    Ok(())
}

/// Connect to the client of a `connect` session that has none, and attach it, blocking until the
/// client has configured the session or disconnected. Does nothing for other sessions or while a
/// client is attached.
///
/// First, the client of a `listen` or `connect` session is released if its connection ended or
/// it detached.
pub fn before_request(runtime: &Runtime) {
    let Some(session) = current_session() else {
        return;
    };
    session.release_disconnected_client();
    if session.client.borrow().is_some() {
        return;
    }
    if let Some(connection) = session.connect() {
        let debugger_loop = debugger_loop(runtime);
        let waiting = Some(debugger_loop.event_loop());
        match attach_client(&runtime.default_global(), connection, waiting) {
            Ok(client) => session.check_configured(&client),
            Err(error) => eprintln!("debugger: {error}"),
        }
    }
}

impl Session {
    /// A connection to the client of a `connect` session, or `None` for other sessions and when
    /// no connection was made. A failed attempt is reported with [`Session::report_undebugged`].
    fn connect(&self) -> Option<Box<dyn Connection>> {
        let Source::Connect {
            host,
            port,
            timeout,
        } = &self.source
        else {
            return None;
        };
        match tcp::connect(host, *port, *timeout) {
            Ok(connection) => Some(Box::new(connection)),
            Err(error) => {
                self.report_undebugged(&format!("no client at {host}:{port} ({error})"));
                None
            }
        }
    }

    /// For a `connect` session, record whether `client`, attached with `wait`, configured the
    /// session. A client that disconnected or detached before configuring it is reported with
    /// [`Session::report_undebugged`]. Does nothing for other sessions.
    ///
    /// Note: the adapter also marks the session configured when the client disconnects or
    /// detaches, so `configured` alone does not tell the cases apart.
    fn check_configured(&self, client: &Client) {
        let Source::Connect { host, port, .. } = &self.source else {
            return;
        };
        if !client.disconnected.get() && !client.detached.get() {
            self.undebugged_reported.set(false);
        } else {
            self.report_undebugged(&format!(
                "the client at {host}:{port} disconnected before configuring the session"
            ));
        }
    }

    /// Print that a `connect` session runs without debugging because of `reason`, unless that was
    /// already reported and no client configured the session since.
    fn report_undebugged(&self, reason: &str) {
        if !self.undebugged_reported.replace(true) {
            eprintln!("debugger: {reason}, running without debugging");
        }
    }

    /// Resolves once the session has a client whose connection has not ended.
    async fn connected_client(&self) {
        std::future::poll_fn(|cx| {
            let connected = self
                .client
                .borrow()
                .as_ref()
                .is_some_and(|client| !client.disconnected.get());
            if connected {
                return Poll::Ready(());
            }
            *self.client_waker.borrow_mut() = Some(cx.waker().clone());
            Poll::Pending
        })
        .await
    }

    /// Resolves once [`Session::release_disconnected_client`] would release `client`, or
    /// [`Connection::try_recv`] would return something other than `Empty` for its connection.
    async fn client_ready(&self, client: &Client) {
        let mut ready = client.connection.ready();
        std::future::poll_fn(|cx| {
            if self.releases(client) {
                return Poll::Ready(());
            }
            *self.client_waker.borrow_mut() = Some(cx.waker().clone());
            ready.as_mut().poll(cx)
        })
        .await
    }

    /// Whether [`Session::release_disconnected_client`] releases `client`: its connection ended or
    /// it detached, and the session is not a `Stdio` session.
    fn releases(&self, client: &Client) -> bool {
        !matches!(self.source, Source::Stdio)
            && (client.disconnected.get() || client.detached.get())
    }

    fn wake_client_waker(&self) {
        if let Some(waker) = self.client_waker.borrow_mut().take() {
            waker.wake();
        }
    }

    /// Close and drop the client once its connection ended or it detached, and stop forwarding
    /// output to it (see [`forward_output`]). Messages queued before are written before the
    /// connection closes. A `Stdio` session keeps its client until [`finish`].
    fn release_disconnected_client(&self) {
        let client = self
            .client
            .borrow_mut()
            .take_if(|client| self.releases(client));
        if let Some(client) = client {
            forward_output(false);
            client.connection.close(Vec::new());
        }
    }
}

/// Create a debugger realm for `connection`, make it the session's client, and evaluate the
/// adapter in it. With a `wait` loop, this returns once the client has configured the session or
/// disconnected, and the connection was flushed with [`Connection::flush`]. Messages that arrive
/// meanwhile are dispatched with that loop active, and microtasks are drained after each. The loop
/// is not driven. On error, the session has no client.
///
/// `content` must have the default global's realm entered. The adapter receives that global as
/// `contentGlobal`.
pub(crate) fn attach_client(
    content: &Scope<'_>,
    connection: Box<dyn Connection>,
    wait: Option<&EventLoop>,
) -> Result<Rc<Client>, String> {
    let Some(session) = current_session() else {
        return Err("Attaching a debug client failed: no debug session is active".to_string());
    };
    // We take the content global before entering the debugger realm, since `global()` returns
    // the current realm's global.
    let content_global = content.global();
    // The debugger realm stays entered only while the adapter is evaluated.
    let client = {
        let mut options = js::debug::debugger_realm_options();
        js::class::install_global_private_trace(&mut options);
        let scope = RootScope::new_global(
            content.cx_mut(),
            &js::class::STARLING_GLOBAL_CLASS,
            OnNewGlobalHookOption::DontFireOnNewGlobalHook,
            options,
        );
        if js::debug::define_debugger_object(&scope, scope.global().handle()).is_err() {
            return Err(format!(
                "Defining `Debugger` failed: {}",
                js::error::ExnThrown::capture(&scope)
            ));
        }
        crate::host::host_globals::add_to_global(&scope, scope.global());
        let defined = content_global.wrap_object(&scope).and_then(|global| {
            scope
                .global()
                .define_property(&scope, c"contentGlobal", global, 0)
        });
        if defined.is_err() {
            return Err(format!(
                "Defining `contentGlobal` failed: {}",
                js::error::ExnThrown::capture(&scope)
            ));
        }

        let client = Rc::new(Client {
            connection,
            global: RefCell::new(Some(RootedHeap::new(scope.global()))),
            paused: Cell::new(false),
            resume_requested: Cell::new(false),
            configured: Cell::new(false),
            disconnected: Cell::new(false),
            detached: Cell::new(false),
        });
        *session.client.borrow_mut() = Some(Rc::clone(&client));
        session.wake_client_waker();
        forward_output(true);

        if js::compile::evaluate_with_filename(
            &scope,
            ADAPTER_SOURCE,
            "starling:debugger/adapter.js",
            1,
        )
        .is_err()
        {
            let error = js::error::ExnThrown::capture(&scope);
            session.client.borrow_mut().take();
            forward_output(false);
            return Err(format!("Evaluating the debug adapter failed: {error}"));
        }
        client
    };
    if let Some(event_loop) = wait {
        // SAFETY: the pointer is only passed to `in_debugger_realm`, which roots what it uses.
        let raw_cx = unsafe { content.raw_cx_no_gc() };
        while !client.configured.get()
            && !client.disconnected.get()
            && client.global_ptr().is_some()
        {
            // SAFETY: `raw_cx` is valid, with the content realm entered.
            unsafe {
                client.in_debugger_realm(raw_cx, event_loop, |client, scope| {
                    client.receive_blocking(scope)
                });
            }
        }
        client.connection.flush();
    }
    Ok(client)
}

/// The event loop running-mode dispatch runs with.
pub enum RunningLoop<'a> {
    /// The content invocation's loop, which the content driver drives (command modes).
    Content(&'a EventLoop),
    /// The debugger's event loop, which `serve_while_running` drives (serve modes).
    Debugger,
}

/// Pass messages that arrive while the debuggee runs to the adapter, attach clients that connect
/// to a `listen` session's listener while no client is attached, and drive a
/// [`RunningLoop::Debugger`] loop with [`run_until`] and `sleep`. Never completes.
///
/// Without a client and a listener, this waits until a client is attached elsewhere, such as by
/// [`before_request`] in a `connect` session. With [`RunningLoop::Debugger`] and no session started
/// before the first poll, this does nothing.
///
/// A [`RunningLoop::Debugger`] loop is driven while it is alive. Once [`run_until`] returned, the
/// loop is driven again only after a message was dispatched, since [`run_until`] returns after one
/// step for a loop whose stop was requested.
///
/// The adapter runs with `running`'s loop active, so content code an `evaluate` calls can use it.
/// Microtasks are drained after each message, since `EventLoop::step` requires an empty queue.
///
/// # Safety
///
/// `raw_cx` must be valid for the lifetime of the future, with the default global's realm entered
/// whenever the future is polled.
pub async unsafe fn serve_while_running<S, F>(
    raw_cx: *mut js::native::RawJSContext,
    running: RunningLoop<'_>,
    sleep: S,
) where
    S: Fn(Duration) -> F,
    F: Future<Output = ()>,
{
    let debugger_loop = matches!(running, RunningLoop::Debugger)
        .then(|| DEBUGGER_LOOP.with(|slot| slot.borrow().clone()))
        .flatten();
    let (event_loop, drive) = match (running, &debugger_loop) {
        (RunningLoop::Content(event_loop), _) => (event_loop, false),
        (RunningLoop::Debugger, Some(state)) => (state.event_loop(), true),
        (RunningLoop::Debugger, None) => return std::future::pending().await,
    };
    // The `run_until` future driving a `Debugger` loop. `None` until the loop is alive, and
    // again once `run_until` returned.
    let mut driving: Option<LocalFuture<'_>> = None;
    // Whether a message was dispatched since `run_until` last returned.
    let mut dispatched = true;
    loop {
        if let Some(client) = current_client() {
            // SAFETY: guaranteed by this function's caller.
            dispatched |= unsafe { client.dispatch_received(raw_cx, event_loop) };
        }
        let session = current_session();
        let listener = session.as_deref().and_then(|session| {
            session.release_disconnected_client();
            match &session.source {
                Source::Listen(listener) if session.client.borrow().is_none() => Some(listener),
                _ => None,
            }
        });
        if let Some(connection) = listener.and_then(|listener| listener.try_accept()) {
            // SAFETY: guaranteed by this function's caller.
            let content = unsafe { RootScope::from_current_realm(raw_cx) };
            if let Err(error) = attach_client(&content, connection, None) {
                eprintln!("debugger: {error}");
            }
            continue;
        }
        if drive && driving.is_none() && dispatched && event_loop.is_alive() {
            // SAFETY: guaranteed by this function's caller.
            driving = Some(Box::pin(unsafe {
                run_until(raw_cx, event_loop, &sleep, |_| false)
            }));
        }
        let client = current_client().filter(|client| !client.disconnected.get());
        let mut ready: LocalFuture<'_> = match (&client, listener, &session) {
            (Some(client), _, Some(session)) => Box::pin(session.client_ready(client)),
            (None, Some(listener), _) => listener.ready(),
            (None, None, Some(session)) => Box::pin(session.connected_client()),
            (_, _, None) => Box::pin(std::future::pending()),
        };
        match &mut driving {
            None => ready.await,
            Some(run) => {
                let returned = std::future::poll_fn(|cx| {
                    if ready.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(false);
                    }
                    run.as_mut().poll(cx).map(|()| true)
                })
                .await;
                if returned {
                    driving = None;
                    dispatched = false;
                }
            }
        }
    }
}

impl Client {
    /// Pass every message that has arrived to the adapter, with the debugger realm entered and
    /// `event_loop` active. A closed connection passes [`DISCONNECT`] instead, once. Returns
    /// whether anything was passed.
    ///
    /// # Safety
    ///
    /// `raw_cx` must be valid, with the content realm entered.
    unsafe fn dispatch_received(
        &self,
        raw_cx: *mut js::native::RawJSContext,
        event_loop: &EventLoop,
    ) -> bool {
        let mut dispatched = false;
        while !self.disconnected.get() {
            let message = match self.connection.try_recv() {
                Received::Empty => break,
                Received::Message(message) => Some(message),
                Received::Closed => None,
            };
            // SAFETY: guaranteed by this function's caller.
            unsafe {
                self.in_debugger_realm(raw_cx, event_loop, |client, scope| match message {
                    Some(message) => client.dispatch(scope, message),
                    None => client.disconnect(scope),
                });
            }
            dispatched = true;
        }
        dispatched
    }

    /// Run `f` with the debugger realm entered and `event_loop` active, then drain microtasks.
    /// Does nothing once the debugger global is released.
    ///
    /// # Safety
    ///
    /// `raw_cx` must be valid, with the content realm entered.
    unsafe fn in_debugger_realm(
        &self,
        raw_cx: *mut js::native::RawJSContext,
        event_loop: &EventLoop,
        f: impl FnOnce(&Client, &Scope<'_>),
    ) {
        // SAFETY: guaranteed by this function's caller.
        let content = unsafe { RootScope::from_current_realm(raw_cx) };
        let Some(global) = self.global_ptr() else {
            return;
        };
        with_event_loop(event_loop, |_| {
            {
                let scope = RootScope::new_with_realm(content.cx_mut(), global);
                f(self, &scope);
            }
            run_microtasks(&content);
        });
    }

    /// The debugger global's address, or `None` once the runtime's shutdown hook released it.
    fn global_ptr(&self) -> Option<NonNull<js::native::JSObject>> {
        let global = self.global.borrow();
        // SAFETY: `self.global` roots the object while `self` is alive, until `release` takes it.
        // `release` runs only when the runtime is dropped, so not while `in_debugger_realm` uses
        // the pointer.
        global
            .as_ref()
            .and_then(|global| NonNull::new(unsafe { global.as_ptr() }))
    }

    pub(crate) fn send(&self, body: String) {
        self.connection.send(body);
    }

    pub(crate) fn mark_configured(&self) {
        self.configured.set(true);
    }

    pub(crate) fn mark_detached(&self) {
        self.detached.set(true);
        stop_forwarding_output();
        if let Some(session) = current_session() {
            session.wake_client_waker();
        }
    }

    pub(crate) fn request_resume(&self) {
        self.resume_requested.set(true);
    }

    /// Whether [`pause`] would block: blocking is allowed, and the client is connected and not
    /// already paused.
    fn may_pause(&self) -> bool {
        !self.paused.get() && !self.disconnected.get() && blocking_allowed()
    }

    /// Wait for the next message and pass it to the adapter. A closed connection passes
    /// [`DISCONNECT`] instead, once.
    ///
    /// `scope` must have the debugger realm entered.
    fn receive_blocking(&self, scope: &Scope<'_>) {
        match self.connection.recv_blocking() {
            Some(message) => self.dispatch(scope, message),
            None => self.disconnect(scope),
        }
    }

    fn disconnect(&self, scope: &Scope<'_>) {
        if !self.disconnected.replace(true) {
            stop_forwarding_output();
            self.dispatch(scope, DISCONNECT.to_string());
        }
    }

    /// Call the adapter's `handleMessage` with `message`. An exception it throws becomes an
    /// `important` output event.
    ///
    /// `scope` must have the debugger realm entered.
    fn dispatch(&self, scope: &Scope<'_>, message: String) {
        if js::Function::call_by_name(scope, scope.global().handle(), c"handleMessage", &[message])
            .is_err()
        {
            let error = js::error::ExnThrown::capture(scope);
            self.send(output_event(
                "important",
                &format!("Debug adapter error: {error}\n"),
            ));
        }
    }

    /// End the session: send `error` as output, then `exited` with `exit_code` and
    /// `terminated`, and close the connection once they are written.
    fn end(&self, error: Option<&str>, exit_code: i32) {
        forward_output(false);
        let mut last = Vec::with_capacity(3);
        if let Some(message) = error {
            last.push(output_event("stderr", &format!("{message}\n")));
        }
        last.push(
            json!({ "type": "event", "event": "exited", "body": { "exitCode": exit_code } })
                .to_string(),
        );
        last.push(json!({ "type": "event", "event": "terminated" }).to_string());
        self.connection.close(last);
    }
}

/// Block in the adapter's hook, passing messages to it, until it calls `resume` or the client
/// disconnects, and then flush the connection with [`Connection::flush`]. Returns `false` without
/// blocking if [`may_pause`] is `false`. Every event loop is suspended while this blocks.
///
/// Microtasks queued while this blocks, such as the reactions of a promise an `evaluate`
/// settles, are held until the next microtask drain after the hook returns (see
/// [`js::jobs::hold_jobs_queued_during`]).
///
/// `scope` must have the debugger realm entered, which holds for host functions the adapter
/// calls.
pub(crate) fn pause(scope: &Scope<'_>) -> bool {
    let Some(client) = current_client() else {
        return false;
    };
    if !client.may_pause() {
        return false;
    }
    client.paused.set(true);
    client.resume_requested.set(false);
    set_suspended(true);
    js::jobs::hold_jobs_queued_during(scope, || {
        while !client.resume_requested.get() && !client.disconnected.get() {
            client.receive_blocking(scope);
        }
    });
    client.connection.flush();
    set_suspended(false);
    client.paused.set(false);
    true
}

/// Whether [`pause`] would block: blocking is allowed, and a client is attached, connected, and
/// not already paused.
pub(crate) fn may_pause() -> bool {
    current_client().is_some_and(|client| client.may_pause())
}

/// End the session as if the process exited successfully, and exit. Held microtasks are dropped.
pub(crate) fn terminate() -> ! {
    end_session(None, 0);
    js::jobs::clear_held_jobs();
    platform::process::exit(0)
}

/// End the session: send `result`'s error as output, then `exited` and `terminated`, and wait
/// until they are written. `exited` reports exit code 1 for an error and 0 otherwise. Does nothing
/// without a session.
pub fn finish(result: &Result<(), String>) {
    end_session(
        result.as_ref().err().map(String::as_str),
        i32::from(result.is_err()),
    );
}

/// The exit hook a session sets: end the session with the code the process exits with.
fn finish_on_exit(code: u8) {
    end_session(None, code.into());
}

/// End the session: send `error` as output, then `exited` with `exit_code` and `terminated`, and
/// wait until they are written. Does nothing without a session.
fn end_session(error: Option<&str>, exit_code: i32) {
    let Some(session) = SESSION.with(|slot| slot.borrow_mut().take()) else {
        return;
    };
    let client = session.client.borrow_mut().take();
    if let Some(client) = client {
        client.end(error, exit_code);
    }
}

/// An `output` event body with `output` as its text.
pub(crate) fn output_event(category: &str, output: &str) -> String {
    json!({
        "type": "event",
        "event": "output",
        "body": { "category": category, "output": output },
    })
    .to_string()
}

/// Forward console messages and reports of uncaught errors to the client as `output` events, or
/// with `false`, write them to stdout and stderr again.
fn forward_output(forward: bool) {
    if forward {
        cpp_builtins::set_console_sink(Some(console_sink));
        core_runtime::diagnostics::set_sink(Some(diagnostics_sink));
    } else {
        cpp_builtins::set_console_sink(None);
        core_runtime::diagnostics::set_sink(None);
    }
}

/// Write console messages and reports of uncaught errors to stdout and stderr again, for a client
/// that disconnected or detached. A `Stdio` session keeps forwarding them to its client until
/// [`finish`].
fn stop_forwarding_output() {
    if current_session().is_some_and(|session| !matches!(session.source, Source::Stdio)) {
        forward_output(false);
    }
}

/// Forwards reports of uncaught errors as `stderr` output events while a connected client is
/// attached, and writes them to stderr otherwise.
fn diagnostics_sink(message: &str) {
    match current_client().filter(|client| !client.disconnected.get()) {
        Some(client) => client.send(output_event("stderr", &format!("{message}\n"))),
        None => eprintln!("{message}"),
    }
}

/// Forwards console messages as `output` events. `warn` and `error` keep the prefixes the
/// console prints without a session.
unsafe extern "C" fn console_sink(level: u8, message: *const c_char, length: usize) {
    // SAFETY: the console passes `length` bytes at `message`, valid for this call.
    let message =
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(message.cast(), length) });
    let (category, prefix) = match level {
        3 => ("stderr", "Warn: "),
        4 => ("stderr", "Error: "),
        _ => ("stdout", ""),
    };
    if let Some(client) = current_client() {
        client.send(output_event(category, &format!("{prefix}{message}\n")));
    }
}

#[cfg(test)]
mod tests {
    use std::task::{Context, Waker};

    use core_runtime::event_loop::{request_stop, with_active_event_loop, Task, TaskId};
    use js::error::ExnThrown;
    use js::native::JSTracer;

    use super::*;

    /// Sets a flag when it runs.
    struct SetFlag(Rc<Cell<bool>>);

    impl Task for SetFlag {
        fn kind(&self) -> &'static str {
            "set-flag"
        }

        fn run(self: Box<Self>, _scope: &Scope<'_>, _id: TaskId) -> Result<(), ExnThrown> {
            self.0.set(true);
            Ok(())
        }

        fn trace(&self, _trc: *mut JSTracer) {}
    }

    /// Counts its runs, requests that the active loop stop, and queues itself again until it ran
    /// three times.
    struct StopAndRequeue(Rc<Cell<u32>>);

    impl Task for StopAndRequeue {
        fn kind(&self) -> &'static str {
            "stop-and-requeue"
        }

        fn run(self: Box<Self>, _scope: &Scope<'_>, _id: TaskId) -> Result<(), ExnThrown> {
            let runs = self.0.get() + 1;
            self.0.set(runs);
            request_stop();
            if runs < 3 {
                with_active_event_loop(|event_loop| event_loop.queue_ready(self));
            }
            Ok(())
        }

        fn trace(&self, _trc: *mut JSTracer) {}
    }

    /// Poll `serve_while_running` once, without a session, with a ready task queued on its loop:
    /// the debugger's event loop with `RunningLoop::Debugger` if `use_debugger_loop` is set, and
    /// another loop with `RunningLoop::Content` otherwise. Returns whether the task ran.
    fn task_ran(runtime: &Runtime, use_debugger_loop: bool) -> bool {
        let content = runtime.default_global();
        let content_loop = EventLoop::new();
        let state = debugger_loop(runtime);
        let (event_loop, running) = if use_debugger_loop {
            (state.event_loop(), RunningLoop::Debugger)
        } else {
            (&content_loop, RunningLoop::Content(&content_loop))
        };
        let ran = Rc::new(Cell::new(false));
        event_loop.queue_ready(Box::new(SetFlag(Rc::clone(&ran))));
        {
            // SAFETY: `content` keeps its realm entered and outlives the future, which is dropped
            // at the end of this block.
            let serve = unsafe {
                serve_while_running(content.raw_cx_no_gc(), running, |_| std::future::pending())
            };
            let mut serve = std::pin::pin!(serve);
            let mut cx = Context::from_waker(Waker::noop());
            assert!(serve.as_mut().poll(&mut cx).is_pending());
        }
        release(runtime);
        ran.get()
    }

    #[test]
    fn serve_while_running_drives_only_the_debugger_loop() {
        let runtime = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
        assert!(!task_ran(&runtime, false));
        assert!(task_ran(&runtime, true));
    }

    #[test]
    fn serve_while_running_does_not_restart_a_stopped_loop_without_a_dispatch() {
        let runtime = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
        let content = runtime.default_global();
        let state = debugger_loop(&runtime);
        let runs = Rc::new(Cell::new(0));
        state
            .event_loop()
            .queue_ready(Box::new(StopAndRequeue(Rc::clone(&runs))));
        {
            // SAFETY: `content` keeps its realm entered and outlives the future, which is dropped
            // at the end of this block.
            let serve = unsafe {
                serve_while_running(content.raw_cx_no_gc(), RunningLoop::Debugger, |_| {
                    std::future::pending()
                })
            };
            let mut serve = std::pin::pin!(serve);
            let mut cx = Context::from_waker(Waker::noop());
            assert!(serve.as_mut().poll(&mut cx).is_pending());
        }
        release(&runtime);
        assert_eq!(runs.get(), 1);
    }
}
