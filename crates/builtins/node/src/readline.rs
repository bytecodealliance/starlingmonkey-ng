// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:readline` and `node:readline/promises` in line mode. See
//! <https://nodejs.org/api/readline.html>.
//!
//! An `Interface` reads its input's `data` events, decodes `Buffer` chunks as UTF-8, and emits a
//! `line` event for each line, which ends at `\n`, `\r\n`, `\r`, U+2028 or U+2029. A line read
//! while a `question` is pending goes to the question's callback instead. Terminal features (raw
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
    /// The prompt, the value `setPrompt` was last given, which `prompt()` writes.
    prompt: Heap<Value>,
    /// The prompt a pending question replaced, which answering or cancelling it restores.
    old_prompt: Heap<Value>,
    /// The UTF-8 decoder of `Buffer` input chunks, which holds an incomplete character until the
    /// next chunk.
    #[no_trace]
    decoder: crate::string_decoder::NativeDecoderImpl,
    /// The `crlfDelay` option: for how many milliseconds after a chunk that ends with `\r` a `\n`
    /// starting the next chunk ends no line.
    #[no_trace]
    crlf_delay: f64,
    /// When a chunk that ended with `\r` was written, in milliseconds of the monotonic clock.
    #[no_trace]
    saw_return_at: Option<f64>,
    /// The input of the line being read, a string.
    partial: Heap<Value>,
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

/// The monotonic clock, in milliseconds.
fn now_ms() -> f64 {
    platform::clock::monotonic_ns() as f64 / 1e6
}

/// Node.js's `kOnLine`: answer the pending question with `line`, restoring the prompt it
/// replaced, or emit `line` as a `line` event.
fn on_line(
    scope: &Scope<'_>,
    interface: Object<'_>,
    line: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let state = state_of(scope, interface)?;
    let question = state.data().question.get(scope);
    if js::function::is_callable(question) {
        {
            let data = state.data();
            data.question.set(js::value::undefined());
            let old_prompt = data.old_prompt.get(scope);
            data.prompt.set(old_prompt.get());
        }
        Function::call(scope, js::value::undefined(), question, &[line])?;
        return Ok(());
    }
    crate::events::emit(scope, interface, c"line", &[line])?;
    Ok(())
}

/// The line endings in `units`, as Node.js's `lineEnding` pattern `/\r?\n|\r(?!\n)|\u2028|\u2029/`
/// matches them: the index of each and of the character after it. Returns `None` if allocating
/// the list fails.
fn line_endings(units: impl Iterator<Item = u16>) -> Option<Vec<(usize, usize)>> {
    let mut units = units.enumerate().peekable();
    crate::support::try_collect(std::iter::from_fn(move || loop {
        let (index, unit) = units.next()?;
        match unit {
            0x0a | 0x2028 | 0x2029 => return Some((index, index + 1)),
            0x0d if units.next_if(|(_, next)| *next == 0x0a).is_some() => {
                return Some((index, index + 2))
            }
            0x0d => return Some((index, index + 1)),
            _ => {}
        }
    }))
}

/// Node.js's `kNormalWrite`: decode `chunk` with the interface's decoder, add it to the line being
/// read, and handle each line it completes. A `\n` that starts a chunk within `crlfDelay`
/// milliseconds of a chunk that ended with `\r` is dropped. Only the chunk is searched for line
/// endings, and each line is a substring of it.
fn normal_write(
    scope: &Scope<'_>,
    interface: Object<'_>,
    chunk: HandleValue<'_>,
) -> Result<(), Thrown> {
    if chunk.is_undefined() {
        return Ok(());
    }
    let state = state_of(scope, interface)?;
    let mut string = crate::string_decoder::write_chunk(scope, chunk, |bytes| {
        state.data_mut().decoder.decode_chunk(bytes)
    })?;
    let now = now_ms();
    let (saw_return_at, crlf_delay) = {
        let data = state.data();
        (data.saw_return_at, data.crlf_delay)
    };
    if saw_return_at.is_some_and(|at| now - at <= crlf_delay) {
        if !string.is_empty() && string.char_at(scope, 0)? == u16::from(b'\n') {
            string = string.substring(scope, 1, string.len() - 1)?;
        }
        state.data_mut().saw_return_at = None;
    }
    if string.is_empty() {
        return Ok(());
    }
    // SAFETY: the closure makes no call into SpiderMonkey.
    let endings = unsafe {
        string.with_chars(scope, |chars| match chars {
            js::string::StrChars::Latin1(bytes) => {
                line_endings(bytes.iter().map(|byte| u16::from(*byte)))
            }
            js::string::StrChars::TwoByte(units) => line_endings(units.iter().copied()),
        })
    }?
    .ok_or_else(|| {
        js::exception::report_out_of_memory(scope);
        ExnThrown
    })?;
    let partial = js::JSString::from_value(scope, state.data().partial.get(scope))?;
    let (Some(&(first_end, mut start)), Some(&(_, last_start))) = (endings.first(), endings.last())
    else {
        let partial = partial.concat(scope, string)?;
        state.data_mut().partial.set(partial.as_value());
        return Ok(());
    };
    let ends_with_return = string.char_at(scope, string.len() - 1)? == u16::from(b'\r');
    let first = partial.concat(scope, string.substring(scope, 0, first_end)?)?;
    let rest = string.substring(scope, last_start, string.len() - last_start)?;
    {
        let mut data = state.data_mut();
        data.saw_return_at = ends_with_return.then_some(now);
        data.partial.set(rest.as_value());
    }
    on_line(scope, interface, first.to_jsval_throwing(scope)?)?;
    for &(end, next) in &endings[1..] {
        let line = string.substring(scope, start, end - start)?;
        on_line(scope, interface, line.to_jsval_throwing(scope)?)?;
        start = next;
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
    if !state_of(scope, interface)?.data().closed {
        normal_write(scope, interface, args.get(0)).map_err(|thrown| thrown.throw(scope))?;
    }
    Ok(js::value::undefined())
}

/// The `end` listener of an interface's input, whose payload is the interface: emit the last
/// line as a `line` event, if it has no line ending, and close the interface. A pending question
/// doesn't get the line.
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
    let line = js::JSString::from_value(scope, state.data().partial.get(scope))?;
    if !line.is_empty() {
        crate::events::emit(scope, interface, c"line", &[line.to_jsval_throwing(scope)?])?;
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

/// Node.js's `kWriteToOutput`: write `text`, which must be a string, to the interface's output, if
/// it has one.
fn write_output(
    scope: &Scope<'_>,
    interface: Object<'_>,
    text: HandleValue<'_>,
) -> Result<(), Thrown> {
    if !text.is_string() {
        let error = NodeError::invalid_arg_type(scope, "stringToWrite", "of type string", text);
        return Err(error.into());
    }
    if let Some(output) = interface.obj(scope, c"output")? {
        Function::call_by_name(scope, output.handle(), c"write", &[text])?;
    }
    Ok(())
}

/// Node.js's `kQuestion`: make `query` the prompt, until `callback` receives the answer, and
/// prompt. A question asked while another is pending only prompts again.
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
    if !js::function::is_callable(state.data().question.get(scope)) {
        let data = state.data();
        let prompt = data.prompt.get(scope);
        data.old_prompt.set(prompt.get());
        data.prompt.set(query.get());
        data.question.set(callback.get());
    }
    interface_class::prompt(scope, interface)
}

/// The value of the `crlfDelay` option `value`: at least 100, and 100 if `value` is falsy.
fn crlf_delay(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<f64, ExnThrown> {
    if !js::conversion::to_boolean(value) {
        return Ok(100.0);
    }
    let delay = f64::from_jsval(scope, value, ()).map_err(|_| ExnThrown)?;
    // `Math.max(100, delay)`, which is `NaN` for `NaN`.
    Ok(if delay.is_nan() {
        delay
    } else {
        delay.max(100.0)
    })
}

/// Node.js's `InterfaceConstructor`, for the arguments of `createInterface` and the `Interface`
/// constructors: an options object with a truthy `input`, or the input, the output and the
/// completer. Of the history options, `history` and `historySize` are validated and stored.
/// `completer` is stored as it is.
fn init_interface(
    scope: &Scope<'_>,
    this: Object<'_>,
    input: HandleValue<'_>,
    output: Option<HandleValue<'_>>,
    completer: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    this.set_property(scope, c"isCompletionEnabled", true)?;
    this.set_property(scope, c"escapeCodeTimeout", 500)?;
    this.set_property(scope, c"tabSize", 8)?;
    crate::events::event_emitter::init(scope, this, None)?;
    let undefined = HandleValue::undefined();
    let (mut input, mut output, mut completer) = (
        input,
        output.unwrap_or(undefined),
        completer.unwrap_or(undefined),
    );
    let (mut signal, mut prompt, mut delay) = (undefined, undefined, undefined);
    let (mut history, mut history_size) = (undefined, undefined);
    let options = Object::from_value(scope, *input).ok();
    let options = match options {
        Some(options) if js::conversion::to_boolean(options.val(scope, c"input")?) => Some(options),
        _ => None,
    };
    if let Some(options) = options {
        output = options.val(scope, c"output")?;
        completer = options.val(scope, c"completer")?;
        signal = options.val(scope, c"signal")?;
        history_size = options.val(scope, c"historySize")?;
        history = options.val(scope, c"history")?;
        let tab_size = options.val(scope, c"tabSize")?;
        if !tab_size.is_undefined() {
            let tab_size =
                crate::errors::validate_integer(scope, tab_size, "tabSize", 1.0, 4294967295.0)?;
            this.set_property(scope, c"tabSize", tab_size)?;
        }
        prompt = options.val(scope, c"prompt")?;
        let timeout = options.val(scope, c"escapeCodeTimeout")?;
        if !timeout.is_undefined() {
            if !timeout.is_number() || !timeout.to_number().is_finite() {
                let current = this.val(scope, c"escapeCodeTimeout")?;
                let name = "input.escapeCodeTimeout";
                return Err(
                    NodeError::invalid_arg_value(scope, name, current, "is invalid").into(),
                );
            }
            this.set_property(scope, c"escapeCodeTimeout", timeout)?;
        }
        // As in Node.js, a falsy `signal` is ignored.
        if js::conversion::to_boolean(signal) {
            crate::errors::validate_abort_signal(scope, signal, "options.signal")?;
        }
        delay = options.val(scope, c"crlfDelay")?;
        input = options.val(scope, c"input")?;
        if !history.is_undefined() && !js::Array::is_array_value(scope, history)? {
            let error =
                NodeError::invalid_arg_type(scope, "history", "an instance of Array", history);
            return Err(error.into());
        }
        if !history_size.is_undefined() {
            crate::errors::validate_number(scope, history_size, "size", 0.0)?;
        }
    }
    if !completer.is_undefined() && !js::function::is_callable(completer) {
        return Err(
            NodeError::invalid_arg_value(scope, "completer", completer, "is invalid").into(),
        );
    }
    let signal = if js::conversion::to_boolean(signal) {
        Some(Object::from_value(scope, *signal)?)
    } else {
        None
    };
    let input_object = Object::from_value(scope, *input)
        .map_err(|_| NodeError::invalid_arg_type(scope, "input", "an instance of Stream", input))?;
    let prompt = if prompt.is_undefined() {
        js::JSString::from_str(scope, "> ")?.to_jsval_throwing(scope)?
    } else {
        prompt
    };
    let crlf_delay = crlf_delay(scope, delay)?;
    let data = Function::new_callback(scope, c"", 1, on_data, this)?;
    let end = Function::new_callback(scope, c"", 0, on_end, this)?;
    let error = Function::new_callback(scope, c"", 1, on_error, this)?;
    *interface_class::state_of(scope, this)?.data_mut() = InterfaceStateImpl {
        question: Heap::from(js::value::undefined()),
        prompt: Heap::from(prompt.get()),
        old_prompt: Heap::from(js::value::undefined()),
        decoder: crate::string_decoder::NativeDecoderImpl::default(),
        crlf_delay,
        saw_return_at: None,
        partial: Heap::from(js::JSString::empty(scope).as_value()),
        closed: false,
        paused: false,
        on_data: Some(Heap::from(*data)),
        on_end: Some(Heap::from(*end)),
        on_error: Some(Heap::from(*error)),
        signal: None,
        on_abort: None,
        line_iterator: None,
    };
    if history.is_undefined() {
        history = js::Array::new(scope, 0)?.to_jsval_throwing(scope)?;
    }
    this.set_property(scope, c"history", history)?;
    this.set_property(scope, c"historyIndex", -1)?;
    if history_size.is_undefined() {
        this.set_property(scope, c"historySize", 30)?;
    } else {
        this.set_property(scope, c"historySize", history_size)?;
    }
    this.set_property(scope, c"isFlushing", false)?;
    this.set_property(scope, c"line", "")?;
    this.set_property(scope, c"output", output)?;
    this.set_property(scope, c"input", input)?;
    this.set_property(scope, c"crlfDelay", crlf_delay)?;
    this.set_property(scope, c"completer", completer)?;
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
        completer: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_interface(scope, this, input, output, completer)
    }

    /// <https://nodejs.org/api/readline.html#rlclose>: stop reading the input, pause, and emit
    /// `close`.
    pub fn close(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
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

    /// <https://nodejs.org/api/readline.html#rlpause>: returns `this`, or `undefined` if the
    /// interface was already paused.
    pub fn pause<'s>(scope: &'s Scope<'_>, this: Object<'s>) -> Result<HandleValue<'s>, Thrown> {
        let state = state_of(scope, this)?;
        if state.data().closed {
            return Err(use_after_close().into());
        }
        if state.data().paused {
            return Ok(HandleValue::undefined());
        }
        if let Some(input) = this.obj(scope, c"input")? {
            Function::call_by_name(scope, input.handle(), c"pause", js::function::EmptyArgs)?;
        }
        state.data_mut().paused = true;
        this.set_property(scope, c"paused", true)?;
        crate::events::emit(scope, this, c"pause", &[])?;
        Ok(this.to_jsval_throwing(scope)?)
    }

    /// <https://nodejs.org/api/readline.html#rlresume>: returns `this`, or `undefined` if the
    /// interface wasn't paused.
    pub fn resume<'s>(scope: &'s Scope<'_>, this: Object<'s>) -> Result<HandleValue<'s>, Thrown> {
        let state = state_of(scope, this)?;
        if state.data().closed {
            return Err(use_after_close().into());
        }
        if !state.data().paused {
            return Ok(HandleValue::undefined());
        }
        if let Some(input) = this.obj(scope, c"input")? {
            Function::call_by_name(scope, input.handle(), c"resume", js::function::EmptyArgs)?;
        }
        state.data_mut().paused = false;
        this.set_property(scope, c"paused", false)?;
        crate::events::emit(scope, this, c"resume", &[])?;
        Ok(this.to_jsval_throwing(scope)?)
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

    /// <https://nodejs.org/api/readline.html#rlsetpromptprompt>: `prompt` is stored as it is.
    pub fn set_prompt(
        scope: &Scope<'_>,
        this: Object<'_>,
        prompt: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        state_of(scope, this)?.data_mut().prompt.set(prompt.get());
        Ok(())
    }

    /// <https://nodejs.org/api/readline.html#rlgetprompt>
    pub fn get_prompt<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        Ok(state_of(scope, this)?.data().prompt.get(scope))
    }

    /// <https://nodejs.org/api/readline.html#rlpromptpreservecursor>: resume the interface and
    /// write the prompt, which must be a string, to the output.
    pub fn prompt(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        if state.data().paused {
            interface_class::resume(scope, this)?;
        }
        let prompt = state.data().prompt.get(scope);
        super::write_output(scope, this, prompt)
    }

    /// <https://nodejs.org/api/readline.html#rlwritedata-key>: resume the interface and handle
    /// `data` as input, a string or the bytes of a `Buffer`, typed array or `DataView`.
    #[method(length = 2)]
    pub fn write(scope: &Scope<'_>, this: Object<'_>, data: HandleValue<'_>) -> Result<(), Thrown> {
        let state = state_of(scope, this)?;
        if state.data().closed {
            return Err(use_after_close().into());
        }
        if state.data().paused {
            interface_class::resume(scope, this)?;
        }
        super::normal_write(scope, this, data)
    }

    /// <https://nodejs.org/api/readline.html#rlgetcursorpos>: the column after the prompt.
    pub fn get_cursor_pos<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<Object<'s>, ExnThrown> {
        let prompt = state_of(scope, this)?.data().prompt.get(scope);
        let prompt = js::JSString::from_value(scope, prompt)?.to_utf8(scope)?;
        let columns = crate::inspect::columns(&prompt);
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
        completer: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        super::init_interface(scope, this, input, output, completer)
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

/// Node.js's `kQuestionCancel`: drop the pending question, if there is one, restore the prompt it
/// replaced, and write a line break, as `clearLine()` does without a terminal.
fn cancel_question(scope: &Scope<'_>, interface: Object<'_>) -> Result<(), Thrown> {
    let state = state_of(scope, interface)?;
    if !js::function::is_callable(state.data().question.get(scope)) {
        return Ok(());
    }
    {
        let data = state.data();
        data.question.set(js::value::undefined());
        let old_prompt = data.old_prompt.get(scope);
        data.prompt.set(old_prompt.get());
    }
    let line_break = js::class::get_or_init_atom(scope, c"\r\n")?;
    write_output(scope, interface, line_break)
}

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
    cancel_question(scope, interface).map_err(|thrown| thrown.throw(scope))?;
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
    let values = args.rest(0);
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
        let column = crate::errors::number_to_string(x.to_number() + 1.0);
        let code = if y.is_number() {
            format!(
                "\x1b[{};{column}H",
                crate::errors::number_to_string(y.to_number() + 1.0)
            )
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
            code.push_str(&format!("\x1b[{}D", crate::errors::number_to_string(-dx)));
        } else if dx > 0.0 {
            code.push_str(&format!("\x1b[{}C", crate::errors::number_to_string(dx)));
        }
        if dy < 0.0 {
            code.push_str(&format!("\x1b[{}A", crate::errors::number_to_string(-dy)));
        } else if dy > 0.0 {
            code.push_str(&format!("\x1b[{}B", crate::errors::number_to_string(dy)));
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
