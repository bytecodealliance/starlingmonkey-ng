// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:tty`'s `isatty`, and `ReadStream`, which `process.stdin` is. See
//! <https://nodejs.org/api/tty.html>.
//!
//! A `ReadStream` is a `net.Socket` that reads standard input through `platform::process::Stdin`.
//! It reads only while it flows or has `readable` listeners, and pausing it cancels the read in
//! progress, so a paused `process.stdin` doesn't keep the event loop running.

use std::io::IsTerminal;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{ErrorKind, NodeError, Thrown};
use crate::net::socket::{Link, SocketState};
use crate::support::{spawn_cancellable, until_cancelled, Canceller};

/// Whether `fd` is a standard stream connected to a terminal.
fn is_terminal(fd: f64) -> bool {
    match fd {
        0.0 => std::io::stdin().is_terminal(),
        1.0 => std::io::stdout().is_terminal(),
        2.0 => std::io::stderr().is_terminal(),
        _ => false,
    }
}

/// The standard input of a `ReadStream`, opened on the first read, which a read in progress
/// takes, and the cancellation of that read.
#[derive(Default)]
pub(crate) struct StdinLink {
    stdin: Option<platform::process::Stdin>,
    /// Whether standard input was opened, so a missing `stdin` is held by the read in progress.
    opened: bool,
    cancel: Option<Canceller>,
    /// Whether `unref` was called, so the stream doesn't read until `ref` is.
    unref: bool,
}

/// The state of the `ReadStream` `stream`.
fn state_of<'s>(scope: &'s Scope<'_>, stream: Object<'s>) -> Result<SocketState<'s>, ExnThrown> {
    crate::net::socket::socket_class::state_of(scope, stream)
}

/// Whether `stream` is to be read: it flows, or has `readable` listeners.
fn should_read(scope: &Scope<'_>, stream: Object<'_>) -> Result<bool, ExnThrown> {
    let flowing = crate::stream::state::ReadableSide::try_of(scope, stream)?
        .is_some_and(|side| side.get().flowing == Some(true));
    if flowing {
        return Ok(true);
    }
    let readable = js::class::get_or_init_atom(scope, c"readable")?;
    Ok(crate::events::listener_count(scope, stream, readable)? > 0.0)
}

/// Set whether a read waits until the stream can read.
fn set_pending(state: &SocketState<'_>, pending: bool) {
    state.data_mut().read_pending = pending;
}

/// The completion of a read: push the `Buffer` read, or `null` at the end, or destroy the
/// stream with the error. A cancelled read pushes nothing. Unless the input ended, a read
/// requested while this one held standard input is started now.
fn read_done(
    scope: &Scope<'_>,
    stream: Object<'_>,
    result: Result<HandleValue<'_>, HandleValue<'_>>,
) -> Result<(), ExnThrown> {
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            Function::call_by_name(scope, stream.handle(), c"destroy", &[error])?;
            return Ok(());
        }
    };
    if !outcome.is_undefined() {
        Function::call_by_name(scope, stream.handle(), c"push", &[outcome])?;
    }
    if outcome.is_null() {
        return Ok(());
    }
    let state = state_of(scope, stream)?;
    let pending = state.data().read_pending && !state.data().destroyed();
    if pending && should_read(scope, stream)? {
        start_read(scope, stream, &state)?;
    }
    Ok(())
}

/// Read the next bytes of standard input into `stream`. While `stream` is unreferenced, or a
/// cancelled read still holds standard input, the read is marked pending instead.
fn start_read(
    scope: &Scope<'_>,
    stream: Object<'_>,
    state: &SocketState<'_>,
) -> Result<(), ExnThrown> {
    let stdin = {
        let mut data = state.data_mut();
        let Link::Stdin(link) = &mut data.link else {
            return Ok(());
        };
        if link.unref {
            None
        } else if !link.opened {
            link.opened = true;
            Some(platform::process::Stdin::open())
        } else {
            link.stdin.take()
        }
    };
    let Some(mut stdin) = stdin else {
        set_pending(state, true);
        return Ok(());
    };
    set_pending(state, false);
    let canceller = spawn_cancellable(
        scope,
        stream,
        |cancelled| async move {
            let result = until_cancelled(cancelled, stdin.read()).await;
            Some((stdin, result))
        },
        |scope, stream, (stdin, result)| {
            let state = state_of(scope, stream)?;
            if let Link::Stdin(link) = &mut state.data_mut().link {
                link.cancel = None;
                link.stdin = Some(stdin);
            }
            match result {
                None => Ok(HandleValue::undefined()),
                Some(Ok(Some(bytes))) => {
                    crate::buffer::new_buffer(scope, &bytes)?.to_jsval_throwing(scope)
                }
                Some(Ok(None)) => Ok(HandleValue::null()),
                Some(Err(error)) => Err(crate::fs::UvError::new(error, "read").throw(scope)),
            }
        },
        read_done,
    )?;
    if let Link::Stdin(link) = &mut state.data_mut().link {
        link.cancel = Some(canceller);
    }
    Ok(())
}

/// Cancel the read in progress of the stream of `state`, if any, so a read starts again once the
/// stream can read.
fn cancel_read(state: &SocketState<'_>) {
    let cancelled = match &mut state.data_mut().link {
        Link::Stdin(link) => link.cancel.take().is_some(),
        _ => false,
    };
    if cancelled {
        set_pending(state, true);
    }
}

/// The `pause` listener of a `ReadStream`, whose `this` is the stream: cancel the read in
/// progress.
fn on_pause(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = crate::support::this_object(scope, &args)?;
    if let Some(state) = crate::net::socket::socket_class::try_state_of(scope, stream)? {
        cancel_read(&state);
    }
    Ok(js::value::undefined())
}

/// The `resume` listener of a `ReadStream`, whose `this` is the stream: start the read `_read`
/// deferred while the stream was paused.
fn on_resume(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = crate::support::this_object(scope, &args)?;
    if let Some(state) = crate::net::socket::socket_class::try_state_of(scope, stream)? {
        if state.data().read_pending {
            start_read(scope, stream, &state)?;
        }
    }
    Ok(js::value::undefined())
}

#[core_runtime::jsfunction_class(
    name = "ReadStream",
    extends = "crate::net::socket::socket_class",
    self_construct
)]
pub mod read_stream_class {
    use super::*;

    /// <https://nodejs.org/api/tty.html#class-ttyreadstream>: a socket that reads the
    /// descriptor `fd`. A value that isn't an integer from 0 to 2**31 - 1 is `ERR_INVALID_FD`.
    /// Only descriptor 0, standard input, can be read.
    #[constructor(length = 2)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        fd: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let fd = super::fd_arg(scope, fd.unwrap_or(HandleValue::undefined()))?;
        if fd != 0.0 {
            return Err(
                NodeError::not_implemented("ReadStream for descriptors other than 0").into(),
            );
        }
        let options = Object::new_plain(scope)?;
        options.set_property(scope, c"writable", false)?;
        crate::net::socket::socket_class::init(
            scope,
            this,
            Some(options.to_jsval_throwing(scope)?),
        )?;
        state_of(scope, this)?.data_mut().link = Link::Stdin(StdinLink::default());
        this.set_property(scope, c"fd", fd)?;
        // Node.js's standard input has `isTTY` only when it is a terminal.
        if is_terminal(fd) {
            this.set_property(scope, c"isTTY", true)?;
        }
        this.set_property(scope, c"isRaw", false)?;
        let pause = crate::support::shared_function(scope, on_pause, 0)?;
        crate::events::listen(scope, this, c"on", c"pause", pause)?;
        let resume = crate::support::shared_function(scope, on_resume, 0)?;
        crate::events::listen(scope, this, c"on", c"resume", resume)?;
        Ok(())
    }

    #[method(name = "_read")]
    pub fn read_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let state = state_of(scope, this)?;
        if super::should_read(scope, this)? {
            super::start_read(scope, this, &state)
        } else {
            set_pending(&state, true);
            Ok(())
        }
    }

    /// <https://nodejs.org/api/tty.html#readstreamsetrawmodemode>: records `mode` in `isRaw`.
    /// Input stays line-buffered by the terminal.
    pub fn set_raw_mode<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        mode: bool,
    ) -> Result<Object<'s>, ExnThrown> {
        this.set_property(scope, c"isRaw", mode)?;
        Ok(this)
    }

    /// `socket.unref()`: stop reading standard input until `ref` is called, so a pending read
    /// doesn't keep the event loop running.
    pub fn unref<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        let state = state_of(scope, this)?;
        if let Link::Stdin(link) = &mut state.data_mut().link {
            link.unref = true;
        }
        super::cancel_read(&state);
        Ok(this)
    }

    /// `socket.ref()`: read standard input again after `unref`, if the stream is to be read.
    #[method(name = "ref")]
    pub fn ref_impl<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        let state = state_of(scope, this)?;
        if let Link::Stdin(link) = &mut state.data_mut().link {
            link.unref = false;
        }
        let pending = state.data().read_pending;
        if pending && super::should_read(scope, this)? {
            super::start_read(scope, this, &state)?;
        }
        Ok(this)
    }
}

/// Node.js's check `fd >> 0 !== fd || fd < 0` of the `fd` argument of `tty.ReadStream`: `value` if
/// it is an integer from 0 to 2**31 - 1, and `ERR_INVALID_FD` otherwise. A value that isn't a
/// number is converted to a number and to a string, as the check and the message convert it.
fn fd_arg(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<f64, Thrown> {
    if value.is_number() {
        let fd = value.to_number();
        if fd.fract() == 0.0 && (0.0..=f64::from(i32::MAX)).contains(&fd) {
            return Ok(fd);
        }
    } else {
        f64::from_jsval(scope, value, ()).map_err(|_| ExnThrown)?;
    }
    let received = js::JSString::from_value(scope, value)?.to_utf8(scope)?;
    Err(NodeError::new(
        ErrorKind::RangeError,
        "ERR_INVALID_FD",
        format!("\"fd\" must be a positive integer: {received}"),
    )
    .into())
}

/// The getter of `process.stdin`: create the `ReadStream` of standard input, and replace the
/// accessor with a data property holding it.
fn stdin_getter(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let process =
        Object::from_value(scope, scope.root_value(args.this())).map_err(|e| e.throw(scope))?;
    let stdin = read_stream_class::new_instance(scope)?;
    let fd = scope.root_value(js::value::from_f64(0.0));
    read_stream_class::init(scope, stdin, Some(fd)).map_err(|thrown| thrown.throw(scope))?;
    process.define_property(
        scope,
        c"stdin",
        stdin,
        js::class_spec::JSPROP_ENUMERATE as u32,
    )?;
    stdin.to_jsval_raw_throwing(scope)
}

/// Define `process.stdin` on `process`, created on first use.
pub(crate) fn define_stdin(scope: &Scope<'_>, process: Object<'_>) -> Result<(), ExnThrown> {
    let getter = Function::new_callback(scope, c"stdin", 0, stdin_getter, ())?;
    process.define_accessor(
        scope,
        c"stdin",
        Some(*getter),
        None,
        js::class_spec::JSPROP_ENUMERATE as u32,
    )
}

#[core_runtime::jsmodule(node_args, name = "node:tty", default_namespace)]
pub mod module {
    pub use super::read_stream_class as ReadStream;

    /// <https://nodejs.org/api/tty.html#ttyisattyfd>: whether `fd` is a standard stream connected
    /// to a terminal. Other file descriptors are never terminals, and values that aren't numbers
    /// are `false` without being converted.
    #[function(length = 1)]
    pub fn isatty(fd: Option<js::prelude::HandleValue<'_>>) -> bool {
        fd.filter(|fd| fd.is_number())
            .is_some_and(|fd| super::is_terminal(fd.to_number()))
    }
}
