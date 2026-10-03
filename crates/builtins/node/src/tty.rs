// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:tty`'s `isatty`, and `ReadStream`, which `process.stdin` is. See
//! <https://nodejs.org/api/tty.html>.
//!
//! A `ReadStream` for descriptor 0 reads standard input through `platform::process::Stdin`. It
//! reads only while it flows or has `readable` listeners, and pausing it cancels the read in
//! progress, so a paused `process.stdin` doesn't keep the event loop running.

use std::io::IsTerminal;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::RootedHeap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Function, Object};

use crate::errors::{ErrorKind, NodeError, Thrown};
use crate::stream::{call_method, closure, string, Captures, Props};

/// Whether `fd` is a standard stream connected to a terminal.
fn is_terminal(fd: f64) -> bool {
    match fd {
        0.0 => std::io::stdin().is_terminal(),
        1.0 => std::io::stdout().is_terminal(),
        2.0 => std::io::stderr().is_terminal(),
        _ => false,
    }
}

/// The standard input of a `ReadStream`, which a read in progress takes.
#[core_runtime::jsclass(hidden)]
pub struct StdinHandle {
    #[no_trace]
    stdin: Option<platform::process::Stdin>,
    /// Cancels the read in progress.
    #[no_trace]
    cancel: Option<futures_channel::oneshot::Sender<()>>,
}

#[core_runtime::jsmethods]
impl StdinHandle {
    #[constructor]
    fn new() -> Result<Self, NodeError> {
        Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_ILLEGAL_CONSTRUCTOR",
            "Illegal constructor",
        ))
    }
}

/// The standard input handle of `stream`, created on first use.
fn handle_of<'s>(scope: &'s Scope<'_>, stream: Object<'_>) -> Result<StdinHandle<'s>, ExnThrown> {
    if let Some(handle) = stream.obj(scope, c"_starlingStdin")? {
        if let Ok(handle) = handle.cast::<StdinHandle>() {
            return Ok(handle);
        }
    }
    let handle = js::class::create_instance_with::<StdinHandleImpl>(scope, |_| StdinHandleImpl {
        stdin: Some(platform::process::Stdin::open()),
        cancel: None,
    })?;
    stream.define_property(
        scope,
        c"_starlingStdin",
        handle.to_jsval_throwing(scope)?,
        0,
    )?;
    Ok(handle)
}

/// Whether `stream` is to be read: it flows, or has `readable` listeners.
fn should_read(scope: &Scope<'_>, stream: Object<'_>) -> Result<bool, ExnThrown> {
    if stream.flag(scope, c"readableFlowing")? {
        return Ok(true);
    }
    let listeners = call_method(
        scope,
        stream,
        c"listenerCount",
        &[string(scope, "readable")?],
    )?;
    Ok(listeners.to_number() > 0.0)
}

/// The fulfillment reaction of a read, whose captures are `[stream]`: push the `Buffer` read, or
/// `null` at the end. A cancelled read is fulfilled with `undefined`. Unless the input ended, a
/// read requested while this one held standard input is started now.
fn on_read(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    let outcome = args.get(0);
    if !outcome.is_undefined() {
        call_method(scope, stream, c"push", &[outcome])?;
    }
    if !outcome.is_null()
        && !stream.flag(scope, c"destroyed")?
        && stream.flag(scope, c"_starlingReadPending")?
        && should_read(scope, stream)?
    {
        start_read(scope, stream)?;
    }
    Ok(js::value::undefined())
}

/// The rejection reaction of a read, whose captures are `[stream]`: destroy the stream with the
/// error.
fn on_read_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    call_method(scope, stream, c"destroy", &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// Read the next bytes of standard input into `stream`. While `stream` is unreferenced, or a
/// cancelled read still holds standard input, the read is marked pending instead.
fn start_read(scope: &Scope<'_>, stream: Object<'_>) -> Result<(), ExnThrown> {
    if stream.flag(scope, c"_starlingUnref")? {
        return stream.set_property(scope, c"_starlingReadPending", true);
    }
    stream.set_property(scope, c"_starlingReadPending", false)?;
    let handle = handle_of(scope, stream)?;
    let (sender, cancelled) = futures_channel::oneshot::channel();
    let Some(mut stdin) = ({
        let mut state = handle.data_mut();
        let stdin = state.stdin.take();
        if stdin.is_some() {
            state.cancel = Some(sender);
        }
        stdin
    }) else {
        return stream.set_property(scope, c"_starlingReadPending", true);
    };
    let target = RootedHeap::<js::object::Object>::new(*handle);
    let future = async move {
        let read = async { Some(stdin.read().await) };
        let cancel = async {
            let _ = cancelled.await;
            None
        };
        let result = futures_lite::future::or(read, cancel).await;
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            if let Ok(handle) = target.get(scope).cast::<StdinHandle>() {
                let mut state = handle.data_mut();
                state.cancel = None;
                state.stdin = Some(stdin);
            }
            match result {
                None => Ok(scope.root_value(js::value::undefined())),
                Some(Ok(Some(bytes))) => {
                    crate::buffer::new_buffer(scope, &bytes)?.to_jsval_throwing(scope)
                }
                Some(Ok(None)) => Ok(scope.root_value(js::value::null())),
                Some(Err(error)) => Err(crate::fs::UvError::new(error, "read").throw(scope)),
            }
        }))
    };
    let promise = js::Promise::new_pending(scope)?;
    promise.spawn(PromiseFuture::new(future));
    let on_read = closure(scope, 1, on_read, &[stream.to_jsval_raw_throwing(scope)?])?;
    let on_error = closure(
        scope,
        1,
        on_read_error,
        &[stream.to_jsval_raw_throwing(scope)?],
    )?;
    promise.then(scope, Some(on_read), Some(on_error))?;
    Ok(())
}

/// Cancel the read in progress of `stream`, if any.
fn cancel_read(scope: &Scope<'_>, stream: Object<'_>) -> Result<(), ExnThrown> {
    if let Some(handle) = stream
        .obj(scope, c"_starlingStdin")?
        .and_then(|handle| handle.cast::<StdinHandle>().ok())
    {
        if let Some(cancel) = handle.data_mut().cancel.take() {
            let _ = cancel.send(());
            stream.set_property(scope, c"_starlingReadPending", true)?;
        }
    }
    Ok(())
}

/// The `pause` listener of a `ReadStream`, whose captures are `[stream]`: cancel the read in
/// progress.
fn on_pause(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    cancel_read(scope, Captures::of(scope, payload)?.obj(0)?)?;
    Ok(js::value::undefined())
}

/// The `resume` listener of a `ReadStream`, whose captures are `[stream]`: start the read `_read`
/// deferred while the stream was paused.
fn on_resume(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.obj(0)?;
    if stream.flag(scope, c"_starlingReadPending")? {
        start_read(scope, stream)?;
    }
    Ok(js::value::undefined())
}

#[core_runtime::jsfunction_class(
    name = "ReadStream",
    extends = "crate::stream::readable_class",
    self_construct
)]
pub mod read_stream_class {
    use super::*;

    /// <https://nodejs.org/api/tty.html#class-ttyreadstream>: a stream of the descriptor `fd`.
    /// A value that isn't an integer from 0 to 2**31 - 1 is `ERR_INVALID_FD`. Only descriptor 0,
    /// standard input, can be read.
    #[constructor]
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
        crate::stream::readable_class::init(scope, this, None)?;
        this.set_property(scope, c"fd", fd)?;
        // Node.js's standard input has `isTTY` only when it is a terminal.
        if is_terminal(fd) {
            this.set_property(scope, c"isTTY", true)?;
        }
        this.set_property(scope, c"isRaw", false)?;
        this.set_property(scope, c"_starlingReadPending", false)?;
        this.set_property(scope, c"_starlingUnref", false)?;
        let pause = closure(
            scope,
            0,
            super::on_pause,
            &[this.to_jsval_raw_throwing(scope)?],
        )?;
        call_method(
            scope,
            this,
            c"on",
            &[string(scope, "pause")?, pause.to_jsval_throwing(scope)?],
        )?;
        let resume = closure(
            scope,
            0,
            super::on_resume,
            &[this.to_jsval_raw_throwing(scope)?],
        )?;
        call_method(
            scope,
            this,
            c"on",
            &[string(scope, "resume")?, resume.to_jsval_throwing(scope)?],
        )?;
        Ok(())
    }

    #[method(name = "_read")]
    pub fn read_impl(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        if super::should_read(scope, this)? {
            super::start_read(scope, this)
        } else {
            this.set_property(scope, c"_starlingReadPending", true)
        }
    }

    #[method(name = "_destroy")]
    pub fn destroy_impl(
        scope: &Scope<'_>,
        this: Object<'_>,
        error: HandleValue<'_>,
        callback: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        super::cancel_read(scope, this)?;
        Function::call(scope, js::value::undefined(), callback, &[error])?;
        Ok(())
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
        this.set_property(scope, c"_starlingUnref", true)?;
        super::cancel_read(scope, this)?;
        Ok(this)
    }

    /// `socket.ref()`: read standard input again after `unref`, if the stream is to be read.
    #[method(name = "ref")]
    pub fn ref_impl<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        this.set_property(scope, c"_starlingUnref", false)?;
        if this.flag(scope, c"_starlingReadPending")? && super::should_read(scope, this)? {
            super::start_read(scope, this)?;
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
    let class = read_stream_class::class_constructor(scope)?;
    let zero = scope.root_value(js::value::from_f64(0.0));
    let stdin = Function::construct(scope, class, &[zero])?;
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
    /// to a terminal. Other file descriptors are never terminals.
    pub fn isatty(fd: Option<js::prelude::HandleValue<'_>>) -> bool {
        fd.filter(|fd| fd.is_number())
            .is_some_and(|fd| super::is_terminal(fd.to_number()))
    }
}
