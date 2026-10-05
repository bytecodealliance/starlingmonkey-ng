// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:readline` and `node:readline/promises` in line mode. See
//! <https://nodejs.org/api/readline.html>.
//!
//! An `Interface` reads its input's `data` events, decodes `Buffer` chunks as UTF-8, and emits a
//! `line` event for each line, which ends at `\n`, `\r\n` or `\r`. A line read while a
//! `question` is pending goes to the question's callback instead. Terminal features (raw
//! keypresses, history, completion and cursor editing) are not supported, so `terminal` is always
//! `false`.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs};

use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{ErrorKind, NodeError, Thrown};
use crate::support::{next_tick_call, Props};

/// The key of the shared object `readline.promises` is.
static PROMISES_KEY: u8 = 0;

/// `ERR_USE_AFTER_CLOSE`.
fn use_after_close() -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "ERR_USE_AFTER_CLOSE",
        "readline was closed",
    )
}

/// The state of an `Interface`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct InterfaceState {
    /// The callback of the pending question, or `undefined`.
    question: Heap<Value>,
    #[no_trace]
    prompt: String,
    /// The UTF-8 decoder of `Buffer` input chunks, which holds an incomplete character until the
    /// next chunk.
    #[no_trace]
    decoder: crate::string_decoder::NativeDecoderImpl,
    /// Whether the last input ended with `\r`, so that a `\n` starting the next one ends no line.
    #[no_trace]
    saw_return: bool,
    /// The input of the line being read.
    #[no_trace]
    partial: String,
    #[no_trace]
    closed: bool,
    #[no_trace]
    paused: bool,
    /// The listeners on the input's `data`, `end` and `error` events.
    on_data: Option<Heap<js::object::Object>>,
    on_end: Option<Heap<js::object::Object>>,
    on_error: Option<Heap<js::object::Object>>,
    /// The `signal` option and the abort listener on it, which `close` removes.
    signal: Option<Heap<js::object::Object>>,
    on_abort: Option<Heap<js::object::Object>>,
    /// The iterator `[Symbol.asyncIterator]()` returns, once created.
    line_iterator: Option<Heap<js::object::Object>>,
}

#[core_runtime::jsmethods]
impl InterfaceState {}

/// The state of the interface `interface`.
fn state_of<'s>(
    scope: &'s Scope<'_>,
    interface: Object<'s>,
) -> Result<InterfaceState<'s>, ExnThrown> {
    interface_class::state_of(scope, interface)
}

/// The text of the input chunk `chunk`: a string, or a `Buffer` decoded by the interface's UTF-8
/// decoder.
fn decode(
    scope: &Scope<'_>,
    state: InterfaceState<'_>,
    chunk: HandleValue<'_>,
) -> Result<String, ExnThrown> {
    if chunk.is_string() {
        return Ok(String::from_jsval(scope, chunk, ()).unwrap_or_default());
    }
    let Some(view) = js::ArrayBufferView::from_value(scope, chunk) else {
        return Ok(String::new());
    };
    let bytes = view.copy_bytes(scope)?;
    let units = state.data_mut().decoder.decode(&bytes);
    Ok(String::from_utf16_lossy(&units))
}

/// Handle the line `line`: answer the pending question with it, or emit it as a `line` event.
fn on_line(scope: &Scope<'_>, interface: Object<'_>, line: &str) -> Result<(), ExnThrown> {
    let state = state_of(scope, interface)?;
    let line = js::JSString::from_str(scope, line)?.to_jsval_throwing(scope)?;
    let question = state.data().question.get(scope);
    if js::function::is_callable(question) {
        state.data_mut().question.set(js::value::undefined());
        Function::call(scope, js::value::undefined(), question, &[line])?;
        return Ok(());
    }
    crate::events::emit(scope, interface, c"line", &[line])?;
    Ok(())
}

/// Add `text` to the line being read, and handle each line it completes. A `\r` that ends one
/// piece of text and a `\n` that starts the next end a single line.
fn feed(scope: &Scope<'_>, interface: Object<'_>, text: &str) -> Result<(), ExnThrown> {
    let state = state_of(scope, interface)?;
    let mut lines = Vec::new();
    {
        let mut data = state.data_mut();
        let mut text = text;
        if data.saw_return {
            text = text.strip_prefix('\n').unwrap_or(text);
            data.saw_return = false;
        }
        let mut buffer = std::mem::take(&mut data.partial);
        buffer.push_str(text);
        let mut rest = buffer.as_str();
        while let Some(end) = rest.find(['\r', '\n']) {
            lines.push(rest[..end].to_string());
            let after = &rest[end..];
            rest = if let Some(after_crlf) = after.strip_prefix("\r\n") {
                after_crlf
            } else {
                if after == "\r" {
                    data.saw_return = true;
                }
                &after[1..]
            };
        }
        data.partial = rest.to_string();
    }
    for line in lines {
        on_line(scope, interface, &line)?;
        if state.data().closed {
            return Ok(());
        }
    }
    Ok(())
}

/// The `data` listener of an interface's input, whose payload is the interface.
fn on_data(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let interface = cast_payload::<Object>(scope, payload);
    let state = state_of(scope, interface)?;
    if !state.data().closed {
        let text = decode(scope, state, args.get(0))?;
        feed(scope, interface, &text)?;
    }
    Ok(js::value::undefined())
}

/// The `end` listener of an interface's input, whose payload is the interface: handle the last
/// line, if it has no line ending, and close the interface.
fn on_end(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let interface = cast_payload::<Object>(scope, payload);
    let state = state_of(scope, interface)?;
    if state.data().closed {
        return Ok(js::value::undefined());
    }
    let line = std::mem::take(&mut state.data_mut().partial);
    if !line.is_empty() {
        on_line(scope, interface, &line)?;
    }
    Function::call_by_name(scope, interface.handle(), c"close", js::function::EmptyArgs)?;
    Ok(js::value::undefined())
}

/// The `error` listener of an interface's input, whose payload is the interface: emit the
/// error as the interface's `error` event.
fn on_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let interface = cast_payload::<Object>(scope, payload);
    crate::events::emit(scope, interface, c"error", &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// Write `text` to the interface's output, if it has one.
fn write_output(scope: &Scope<'_>, interface: Object<'_>, text: &str) -> Result<(), ExnThrown> {
    if let Some(output) = interface.obj(scope, c"output")? {
        Function::call_by_name(
            scope,
            output.handle(),
            c"write",
            &[js::conversion::ToJSVal::to_jsval_throwing(text, scope)?],
        )?;
    }
    Ok(())
}

/// Ask `query` on `interface`, with `callback` receiving the answer.
fn ask(
    scope: &Scope<'_>,
    interface: Object<'_>,
    query: HandleValue<'_>,
    callback: HandleValue<'_>,
) -> Result<(), Thrown> {
    let state = state_of(scope, interface)?;
    if state.data().closed {
        return Err(use_after_close().into());
    }
    if js::function::is_callable(state.data().question.get(scope)) {
        interface_class::prompt(scope, interface)?;
        return Ok(());
    }
    state.data_mut().question.set(callback.get());
    let query = js::JSString::from_value(scope, query)?.to_utf8(scope)?;
    write_output(scope, interface, &query)?;
    interface_class::resume(scope, interface)?;
    Ok(())
}

/// The arguments of `createInterface` and the `Interface` constructor: an options object with
/// `input`, or the input followed by the output.
fn init_interface(
    scope: &Scope<'_>,
    this: Object<'_>,
    input: HandleValue<'_>,
    output: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    crate::events::event_emitter::init(scope, this, None)?;
    let (input, output, signal, prompt) = match Object::from_value(scope, *input) {
        Ok(options) if !options.val(scope, c"input")?.is_undefined() => (
            options.val(scope, c"input")?,
            options.val(scope, c"output")?,
            options.val(scope, c"signal")?,
            options.val(scope, c"prompt")?,
        ),
        _ => (
            input,
            output.unwrap_or(HandleValue::undefined()),
            HandleValue::undefined(),
            HandleValue::undefined(),
        ),
    };
    // As in Node.js, a falsy `signal` is ignored.
    let signal = if js::conversion::to_boolean(signal) {
        crate::errors::validate_abort_signal(scope, signal, "options.signal")?;
        Some(Object::from_value(scope, *signal)?)
    } else {
        None
    };
    let input_object = Object::from_value(scope, *input)
        .map_err(|_| NodeError::invalid_arg_type(scope, "input", "an instance of Stream", input))?;
    let prompt = match crate::support::rust_string(scope, prompt) {
        Some(prompt) => prompt,
        None => "> ".to_string(),
    };
    let data = Function::new_callback(scope, c"", 1, on_data, this)?;
    let end = Function::new_callback(scope, c"", 0, on_end, this)?;
    let error = Function::new_callback(scope, c"", 1, on_error, this)?;
    *interface_class::state_of(scope, this)?.data_mut() = InterfaceStateImpl {
        question: Heap::from(js::value::undefined()),
        prompt,
        decoder: crate::string_decoder::NativeDecoderImpl::default(),
        saw_return: false,
        partial: String::new(),
        closed: false,
        paused: false,
        on_data: Some(Heap::from(*data)),
        on_end: Some(Heap::from(*end)),
        on_error: Some(Heap::from(*error)),
        signal: None,
        on_abort: None,
        line_iterator: None,
    };
    this.set_property(scope, c"completer", js::value::undefined())?;
    this.set_property(scope, c"crlfDelay", 100)?;
    this.set_property(scope, c"escapeCodeTimeout", 500)?;
    this.set_property(scope, c"history", *js::Array::new(scope, 0)?)?;
    this.set_property(scope, c"historyIndex", -1)?;
    this.set_property(scope, c"historySize", 30)?;
    this.set_property(scope, c"input", input)?;
    this.set_property(scope, c"isCompletionEnabled", true)?;
    this.set_property(scope, c"isFlushing", false)?;
    this.set_property(scope, c"line", "")?;
    this.set_property(scope, c"output", output)?;
    this.set_property(scope, c"tabSize", 8)?;
    this.set_property(scope, c"terminal", false)?;
    for (event, listener) in [(c"error", error), (c"data", data), (c"end", end)] {
        crate::events::listen(scope, input_object, c"on", event, *listener)?;
    }
    if crate::support::has_method(scope, input_object, c"resume")? {
        Function::call_by_name(
            scope,
            input_object.handle(),
            c"resume",
            js::function::EmptyArgs,
        )?;
    }
    if let Some(signal) = signal {
        // An aborted signal closes the interface in a tick, and any other signal closes it when
        // it aborts.
        if signal.flag(scope, c"aborted")? {
            js::jobs::queue_native_tick(scope, close_on_abort, this, &[])?;
        } else {
            let listener = Function::new_callback(scope, c"", 0, close_on_abort, this)?;
            crate::events::listen(scope, signal, c"addEventListener", c"abort", *listener)?;
            let state = state_of(scope, this)?;
            let mut data = state.data_mut();
            data.signal = Some(Heap::from(signal));
            data.on_abort = Some(Heap::from(*listener));
        }
    }
    Ok(())
}

/// The abort listener of an interface's `signal`, whose payload is the interface: close it.
fn close_on_abort(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let interface = cast_payload::<Object>(scope, payload);
    Function::call_by_name(scope, interface.handle(), c"close", js::function::EmptyArgs)?;
    Ok(js::value::undefined())
}

/// `[Symbol.asyncIterator]()` of an interface: Node.js's `events.on` iterator over its `line`
/// events, which yields each line, pauses the interface while more than 1024 lines are queued, and
/// ends when the interface closes. Every call returns the iterator the first call created.
fn async_iterator(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let interface = crate::support::this_object(scope, &args)?;
    let state = state_of(scope, interface)?;
    let cached = state.data().line_iterator.get(scope);
    if let Some(iterator) = cached {
        return iterator.to_jsval_raw_throwing(scope);
    }
    let settings = crate::events_async::OnSettings {
        high_water_mark: 1024.0,
        low_water_mark: 1.0,
        signal: None,
        first_event_param: true,
    };
    let line = js::class::get_or_init_atom(scope, c"line")?;
    let iterator = crate::events_async::on_iterator(scope, interface, line, settings, |scope| {
        Ok(vec![js::class::get_or_init_atom(scope, c"close")?])
    })
    .map_err(|thrown| thrown.throw(scope))?;
    state.data_mut().line_iterator = Some(Heap::from(iterator));
    iterator.to_jsval_raw_throwing(scope)
}

#[core_runtime::jsfunction_class(
    name = "Interface",
    extends = "crate::events::event_emitter",
    self_construct,
    state = "InterfaceState",
    invalid_this = "crate::errors::invalid_this"
)]
pub mod interface_class {
    use super::*;

    /// <https://nodejs.org/api/readline.html#class-interfaceconstructor>
    #[constructor(length = 4)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        input: HandleValue<'_>,
        output: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_interface(scope, this, input, output)
    }

    /// <https://nodejs.org/api/readline.html#rlclose>: stop reading the input, pause, and emit
    /// `close`.
    pub fn close(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let state = state_of(scope, this)?;
        if state.data().closed {
            return Ok(());
        }
        let listeners = {
            let data = state.data();
            [
                (c"data", data.on_data.get(scope)),
                (c"end", data.on_end.get(scope)),
                (c"error", data.on_error.get(scope)),
            ]
        };
        if let Some(input) = this.obj(scope, c"input")? {
            for (event, listener) in listeners {
                if let Some(listener) = listener {
                    crate::events::listen(scope, input, c"removeListener", event, listener)?;
                }
            }
        }
        interface_class::pause(scope, this)?;
        state.data_mut().closed = true;
        this.set_property(scope, c"closed", true)?;
        let (signal, on_abort) = {
            let mut data = state.data_mut();
            (
                data.signal.take_rooted(scope),
                data.on_abort.take_rooted(scope),
            )
        };
        if let (Some(signal), Some(on_abort)) = (signal, on_abort) {
            crate::events::listen(scope, signal, c"removeEventListener", c"abort", on_abort)?;
        }
        crate::events::emit(scope, this, c"close", &[])?;
        Ok(())
    }

    /// <https://nodejs.org/api/readline.html#rlpause>
    pub fn pause<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        let state = state_of(scope, this)?;
        if !state.data().paused {
            if let Some(input) = this.obj(scope, c"input")? {
                Function::call_by_name(scope, input.handle(), c"pause", js::function::EmptyArgs)?;
            }
            state.data_mut().paused = true;
            this.set_property(scope, c"paused", true)?;
            crate::events::emit(scope, this, c"pause", &[])?;
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/readline.html#rlresume>
    pub fn resume<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        let state = state_of(scope, this)?;
        let (paused, closed) = {
            let data = state.data();
            (data.paused, data.closed)
        };
        if paused && !closed {
            if let Some(input) = this.obj(scope, c"input")? {
                Function::call_by_name(scope, input.handle(), c"resume", js::function::EmptyArgs)?;
            }
            state.data_mut().paused = false;
            this.set_property(scope, c"paused", false)?;
            crate::events::emit(scope, this, c"resume", &[])?;
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/readline.html#rlquestionquery-options-callback>
    #[method(length = 3)]
    pub fn question(
        scope: &Scope<'_>,
        this: Object<'_>,
        query: HandleValue<'_>,
        options: HandleValue<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let callback = if js::function::is_callable(options) {
            options
        } else {
            callback.unwrap_or_else(|| scope.root_value(js::value::undefined()))
        };
        let signal = match Object::from_value(scope, *options) {
            Ok(options) if !options.is_callable() => options.val(scope, c"signal")?,
            _ => scope.root_value(js::value::undefined()),
        };
        let callback = if bool::from_jsval(scope, signal, ()).unwrap_or(false) {
            crate::errors::validate_abort_signal(scope, signal, "options.signal")?;
            if Object::from_value(scope, *signal)?.flag(scope, c"aborted")? {
                return Ok(());
            }
            super::watch_signal(scope, this, signal, callback, None)?
        } else {
            callback
        };
        if js::function::is_callable(callback) {
            super::ask(scope, this, query, callback)?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/readline.html#rlsetpromptprompt>
    pub fn set_prompt(
        scope: &Scope<'_>,
        this: Object<'_>,
        prompt: String,
    ) -> Result<(), ExnThrown> {
        state_of(scope, this)?.data_mut().prompt = prompt;
        Ok(())
    }

    /// <https://nodejs.org/api/readline.html#rlgetprompt>
    pub fn get_prompt(scope: &Scope<'_>, this: Object<'_>) -> Result<String, ExnThrown> {
        Ok(state_of(scope, this)?.data().prompt.clone())
    }

    /// <https://nodejs.org/api/readline.html#rlpromptpreservecursor>
    pub fn prompt(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        if state.data().closed {
            return Err(use_after_close().into());
        }
        interface_class::resume(scope, this)?;
        let prompt = state.data().prompt.clone();
        super::write_output(scope, this, &prompt)?;
        Ok(())
    }

    /// <https://nodejs.org/api/readline.html#rlwritedata-key>: handle `data` as input.
    pub fn write(scope: &Scope<'_>, this: Object<'_>, data: HandleValue<'_>) -> Result<(), Thrown> {
        if state_of(scope, this)?.data().closed {
            return Err(use_after_close().into());
        }
        if data.is_string() {
            let text = String::from_jsval(scope, data, ())?;
            super::feed(scope, this, &text)?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/readline.html#rlgetcursorpos>: the column after the prompt.
    pub fn get_cursor_pos<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<Object<'s>, ExnThrown> {
        let columns = crate::inspect::columns(&state_of(scope, this)?.data().prompt);
        let position = Object::new_plain(scope)?;
        position.set_property(scope, c"cols", columns as f64)?;
        position.set_property(scope, c"rows", 0)?;
        Ok(position)
    }

    /// Define `[Symbol.asyncIterator]`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let iterator = Function::new_callback(
            scope,
            c"[Symbol.asyncIterator]",
            0,
            super::async_iterator,
            (),
        )?;
        js::class::define_well_known_symbol_property(
            scope,
            proto,
            js::native::SymbolCode::asyncIterator,
            *iterator,
        )
    }
}

/// The answer callback of a promise question, whose payload is the promise: fulfill the promise
/// with the answer.
fn resolve_answer(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let promise = cast_payload::<js::Promise>(scope, payload);
    promise.resolve(scope, args.get(0))?;
    Ok(js::value::undefined())
}

#[core_runtime::jsfunction_class(
    name = "Interface",
    extends = "super::interface_class",
    state = "InterfaceState",
    invalid_this = "crate::errors::invalid_this"
)]
pub mod promises_interface_class {
    use super::*;

    /// <https://nodejs.org/api/readline.html#class-readlinepromisesinterface>
    #[constructor(length = 0)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        input: HandleValue<'_>,
        output: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_interface(scope, this, input, output)
    }

    /// <https://nodejs.org/api/readline.html#rlquestionquery-options>: a promise for the answer.
    pub fn question<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        query: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        let promise = js::Promise::new_pending(scope)?;
        let answer = Function::new_callback(scope, c"", 1, super::resolve_answer, promise)?;
        let mut answer = answer.to_jsval_throwing(scope)?;
        let signal = match options.and_then(|options| Object::from_value(scope, *options).ok()) {
            Some(options) => options.val(scope, c"signal")?,
            None => scope.root_value(js::value::undefined()),
        };
        let mut run = || -> Result<Option<HandleValue<'_>>, Thrown> {
            if bool::from_jsval(scope, signal, ()).unwrap_or(false) {
                crate::errors::validate_abort_signal(scope, signal, "options.signal")?;
                let signal_object = Object::from_value(scope, *signal)?;
                if signal_object.flag(scope, c"aborted")? {
                    let reason = signal_object.val(scope, c"reason")?;
                    return Ok(Some(crate::errors::abort_error(scope, Some(reason))?));
                }
                answer = super::watch_signal(scope, this, signal, answer, Some(&promise))?;
            }
            super::ask(scope, this, query, answer)?;
            Ok(None)
        };
        match run() {
            Ok(Some(abort)) => promise.reject(scope, abort)?,
            Ok(None) => {}
            Err(thrown) => {
                thrown.throw(scope);
                return js::Promise::new_rejected_with_pending_error(scope);
            }
        }
        Ok(promise)
    }
}

/// Cancel the pending question of `interface` when `signal` aborts, rejecting `promise` with an
/// `AbortError` if given. Returns the answer callback to use instead of `callback`, which removes
/// the abort listener before calling `callback`.
fn watch_signal<'s>(
    scope: &'s Scope<'_>,
    interface: Object<'_>,
    signal: HandleValue<'_>,
    callback: HandleValue<'_>,
    promise: Option<&js::Promise<'_>>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let signal_object = Object::from_value(scope, *signal).map_err(|e| e.throw(scope))?;
    let abort =
        js::class::create_instance_with::<QuestionAbortImpl>(scope, |_| QuestionAbortImpl {
            interface: Heap::from(interface),
            signal: Heap::from(signal_object),
            promise: promise.map(|promise| Heap::from(**promise)),
        })?;
    let on_abort = Function::new_callback(scope, c"", 0, question_aborted, abort)?;
    let once = Object::new_plain(scope)?;
    once.set_property(scope, c"once", true)?;
    Function::call_by_name(
        scope,
        signal_object.handle(),
        c"addEventListener",
        &[
            js::class::get_or_init_atom(scope, c"abort")?,
            on_abort.to_jsval_throwing(scope)?,
            once.to_jsval_throwing(scope)?,
        ],
    )?;
    let answer =
        js::class::create_instance_with::<QuestionAnswerImpl>(scope, |_| QuestionAnswerImpl {
            signal: Heap::from(signal_object),
            on_abort: Heap::from(*on_abort),
            callback: Heap::from(callback.get()),
        })?;
    let answered = Function::new_callback(scope, c"", 1, question_answered, answer)?;
    answered.to_jsval_throwing(scope)
}

/// The payload of [`question_aborted`]: the interface asking a question with a signal, the
/// signal, and the promise of a promise question.
#[core_runtime::jsclass(hidden)]
pub(crate) struct QuestionAbort {
    interface: Heap<js::object::Object>,
    signal: Heap<js::object::Object>,
    promise: Option<Heap<js::object::Object>>,
}

#[core_runtime::jsmethods]
impl QuestionAbort {}

/// The payload of [`question_answered`]: the signal of a question, its abort listener, and the
/// answer callback.
#[core_runtime::jsclass(hidden)]
pub(crate) struct QuestionAnswer {
    signal: Heap<js::object::Object>,
    on_abort: Heap<js::object::Object>,
    callback: Heap<Value>,
}

#[core_runtime::jsmethods]
impl QuestionAnswer {}

/// The abort listener of a question, whose payload is a [`QuestionAbort`]: cancel the question,
/// and reject the promise, if there is one, with an `AbortError`.
fn question_aborted(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let abort = cast_payload::<QuestionAbort>(scope, payload);
    let interface = abort.data().interface.get(scope);
    let signal = abort.data().signal.get(scope);
    let promise = abort.data().promise.get(scope);
    state_of(scope, interface)?
        .data_mut()
        .question
        .set(js::value::undefined());
    if let Some(promise) = promise {
        let promise = promise
            .cast::<js::Promise>()
            .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))?;
        let reason = signal.val(scope, c"reason")?;
        let error = crate::errors::abort_error(scope, Some(reason))?;
        promise.reject(scope, error)?;
    }
    Ok(js::value::undefined())
}

/// The answer callback of a question with a signal, whose payload is a [`QuestionAnswer`]:
/// remove the abort listener, then call the callback with the answer.
fn question_answered(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let answer = cast_payload::<QuestionAnswer>(scope, payload);
    let signal = answer.data().signal.get(scope);
    let on_abort = answer.data().on_abort.get(scope).to_jsval_throwing(scope)?;
    let callback = answer.data().callback.get(scope);
    Function::call_by_name(
        scope,
        signal.handle(),
        c"removeEventListener",
        &[js::class::get_or_init_atom(scope, c"abort")?, on_abort],
    )?;
    if js::function::is_callable(callback) {
        Function::call(scope, js::value::undefined(), callback, &[args.get(0)])?;
    }
    Ok(js::value::undefined())
}

/// A number as a template literal interpolates it.
fn js_number(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e21 {
        format!("{}", number as i64)
    } else {
        number.to_string()
    }
}

/// Call `callback` with `null` in a tick, if it is a function, and return `true`, as Node.js's
/// cursor functions do without a stream to write to.
fn without_stream(scope: &Scope<'_>, callback: Option<HandleValue<'_>>) -> Result<bool, ExnThrown> {
    if let Some(callback) = callback.filter(|callback| js::function::is_callable(*callback)) {
        next_tick_call(scope, callback, &[scope.root_value(js::value::null())])?;
    }
    Ok(true)
}

/// Write the escape sequence `code` to `stream`, and call `callback` once written. Without a
/// stream, `callback` is called with `null` in a tick.
fn write_escape(
    scope: &Scope<'_>,
    stream: HandleValue<'_>,
    code: &str,
    callback: Option<HandleValue<'_>>,
) -> Result<bool, ExnThrown> {
    let callback = callback.filter(|callback| js::function::is_callable(*callback));
    if stream.is_null_or_undefined() {
        return without_stream(scope, callback);
    }
    let stream = Object::from_value(scope, *stream).map_err(|e| e.throw(scope))?;
    let code = js::conversion::ToJSVal::to_jsval_throwing(code, scope)?;
    let written = match callback {
        Some(callback) => {
            Function::call_by_name(scope, stream.handle(), c"write", &[code, callback])?
        }
        None => Function::call_by_name(scope, stream.handle(), c"write", &[code])?,
    };
    Ok(!written.is_boolean() || written.to_boolean())
}

/// A `createInterface` result: an interface of `class` for the arguments `args`.
fn create<'s>(
    scope: &'s Scope<'_>,
    class: Object<'s>,
    args: &[HandleValue<'_>],
) -> Result<Object<'s>, ExnThrown> {
    let args: Vec<HandleValue<'_>> = args.iter().map(|arg| scope.root_value(arg.get())).collect();
    Function::construct(scope, class, &args)
}

/// The body of `readlinePromises.createInterface`.
fn create_promises_interface(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let values = crate::callback_args(&args);
    let class = promises_interface_class::class_constructor(scope)?;
    create(scope, class, &values)?.to_jsval_raw_throwing(scope)
}

/// The object `readline.promises` and `node:readline/promises` export: `Interface`,
/// `createInterface`, and a stub `Readline`.
fn promises_object<'s>(scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    let object = js::class::get_or_init_shared_object(
        scope,
        &PROMISES_KEY as *const u8 as usize,
        |scope| {
            let object = Object::new_plain(scope)?;
            for &(name, kind) in
                crate::stub::exports_of("node:readline/promises").unwrap_or_default()
            {
                let key = std::ffi::CString::new(name).expect("names have no NULs");
                let value = match name {
                    "Interface" => promises_interface_class::class_constructor(scope)?
                        .to_jsval_throwing(scope)?,
                    "createInterface" => Function::new_callback(
                        scope,
                        c"createInterface",
                        4,
                        create_promises_interface,
                        (),
                    )?
                    .to_jsval_throwing(scope)?,
                    _ => crate::stub::stub_value(scope, "node:readline/promises", name, kind)?,
                };
                object.set_property_by_name(scope, &key, value)?;
            }
            Ok(object)
        },
    )?;
    object.to_jsval_throwing(scope)
}

#[core_runtime::jsmodule(node_args, name = "node:readline", default_namespace)]
pub mod module {
    use super::*;

    pub use super::interface_class as Interface;

    /// <https://nodejs.org/api/readline.html#readlinecreateinterfaceoptions>
    #[function(length = 4)]
    pub fn create_interface<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let class = super::interface_class::class_constructor(scope)?;
        super::create(scope, class, &args)
    }

    /// <https://nodejs.org/api/readline.html#readlineclearlinestream-dir-callback>
    #[function(length = 3)]
    pub fn clear_line(
        scope: &Scope<'_>,
        stream: HandleValue<'_>,
        dir: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<bool, Thrown> {
        validate_callback(scope, callback)?;
        let dir = match dir {
            Some(dir) => f64::from_jsval(scope, dir, ()).map_err(|_| ExnThrown)?,
            None => f64::NAN,
        };
        let code = if dir < 0.0 {
            "\x1b[1K"
        } else if dir > 0.0 {
            "\x1b[0K"
        } else {
            "\x1b[2K"
        };
        Ok(super::write_escape(scope, stream, code, callback)?)
    }

    /// <https://nodejs.org/api/readline.html#readlineclearscreendownstream-callback>
    #[function(length = 2)]
    pub fn clear_screen_down(
        scope: &Scope<'_>,
        stream: HandleValue<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<bool, Thrown> {
        validate_callback(scope, callback)?;
        Ok(super::write_escape(scope, stream, "\x1b[0J", callback)?)
    }

    /// <https://nodejs.org/api/readline.html#readlinecursortostream-x-y-callback>
    #[function(length = 4)]
    pub fn cursor_to(
        scope: &Scope<'_>,
        stream: HandleValue<'_>,
        x: Option<HandleValue<'_>>,
        y: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<bool, Thrown> {
        validate_callback(scope, callback)?;
        let undefined = scope.root_value(js::value::undefined());
        let (x, mut y, mut callback) = (x.unwrap_or(undefined), y.unwrap_or(undefined), callback);
        if js::function::is_callable(y) {
            callback = Some(y);
            y = undefined;
        }
        for (value, name) in [(x, "x"), (y, "y")] {
            if value.is_number() && value.to_number().is_nan() {
                return Err(NodeError::invalid_arg_value(scope, name, value, "is invalid").into());
            }
        }
        if stream.is_null_or_undefined() || (!x.is_number() && !y.is_number()) {
            return Ok(super::without_stream(scope, callback)?);
        }
        if !x.is_number() {
            let message = "Cannot set cursor row without setting its column";
            return Err(
                NodeError::new(ErrorKind::TypeError, "ERR_INVALID_CURSOR_POS", message).into(),
            );
        }
        let column = super::js_number(x.to_number() + 1.0);
        let code = if y.is_number() {
            format!("\x1b[{};{column}H", super::js_number(y.to_number() + 1.0))
        } else {
            format!("\x1b[{column}G")
        };
        Ok(super::write_escape(scope, stream, &code, callback)?)
    }

    /// <https://nodejs.org/api/readline.html#readlinemovecursorstream-dx-dy-callback>
    #[function(length = 4)]
    pub fn move_cursor(
        scope: &Scope<'_>,
        stream: HandleValue<'_>,
        dx: Option<HandleValue<'_>>,
        dy: Option<HandleValue<'_>>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<bool, Thrown> {
        validate_callback(scope, callback)?;
        let number = |value: Option<HandleValue<'_>>| -> Result<f64, ExnThrown> {
            match value {
                Some(value) => f64::from_jsval(scope, value, ()).map_err(|_| ExnThrown),
                None => Ok(f64::NAN),
            }
        };
        let (dx, dy) = (number(dx)?, number(dy)?);
        let moves = |value: f64| value != 0.0 && !value.is_nan();
        if stream.is_null_or_undefined() || !(moves(dx) || moves(dy)) {
            return Ok(super::without_stream(scope, callback)?);
        }
        let mut code = String::new();
        if dx < 0.0 {
            code.push_str(&format!("\x1b[{}D", super::js_number(-dx)));
        } else if dx > 0.0 {
            code.push_str(&format!("\x1b[{}C", super::js_number(dx)));
        }
        if dy < 0.0 {
            code.push_str(&format!("\x1b[{}A", super::js_number(-dy)));
        } else if dy > 0.0 {
            code.push_str(&format!("\x1b[{}B", super::js_number(dy)));
        }
        Ok(super::write_escape(scope, stream, &code, callback)?)
    }

    /// `ERR_INVALID_ARG_TYPE` unless `callback` is missing, `undefined` or a function.
    fn validate_callback(
        scope: &Scope<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        if let Some(callback) = callback.filter(|callback| !callback.is_undefined()) {
            crate::errors::validate_function(scope, callback, "callback")?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/readline.html#readlineemitkeypresseventsstream-interface>: has no
    /// effect, since keypresses aren't supported.
    #[function(length = 1)]
    pub fn emit_keypress_events() -> Result<(), ExnThrown> {
        Ok(())
    }
}

#[core_runtime::jsmodule(node_args, name = "node:readline/promises", default_namespace)]
pub mod promises_module {}

/// Register `node:readline` with `promises`.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let Ok(promises) = promises_object(scope) else {
        return false;
    };
    crate::stub::register_implemented_with::<module::js_module>(
        scope,
        name,
        &[("promises", promises)],
    )
}

/// Register `node:readline/promises` with the properties of [`promises_object`].
pub(crate) fn register_promises(scope: &Scope<'_>, name: &'static str) -> bool {
    let exports = || -> Result<Vec<(&'static str, HandleValue<'_>)>, ExnThrown> {
        let object = promises_object(scope)?;
        let object = Object::from_value(scope, *object).map_err(|e| e.throw(scope))?;
        let mut exports = Vec::new();
        for &(export, _) in crate::stub::exports_of(name).unwrap_or_default() {
            let key = std::ffi::CString::new(export).expect("names have no NULs");
            let value = object.get_property_by_name(scope, &key)?;
            exports.push((export, scope.root_value(value.get())));
        }
        Ok(exports)
    };
    let Ok(exports) = exports() else {
        return false;
    };
    crate::stub::register_implemented_with::<promises_module::js_module>(scope, name, &exports)
}
