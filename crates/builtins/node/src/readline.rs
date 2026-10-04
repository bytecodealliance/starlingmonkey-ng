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
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{ErrorKind, NodeError, Thrown};
use crate::stream::{call_method, closure, is_function, next_tick_call, string, Captures, Props};

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

/// The text of the input chunk `chunk`: a string, or a `Buffer` decoded by the interface's UTF-8
/// `StringDecoder` in `_starlingDecoder`, which buffers an incomplete character until the next
/// chunk.
fn decode(
    scope: &Scope<'_>,
    interface: Object<'_>,
    chunk: HandleValue<'_>,
) -> Result<String, ExnThrown> {
    if chunk.is_string() {
        return Ok(String::from_jsval(scope, chunk, ()).unwrap_or_default());
    }
    let decoder = match interface.obj(scope, c"_starlingDecoder")? {
        Some(decoder) => decoder,
        None => {
            let class = crate::string_decoder::string_decoder_class::class_constructor(scope)?;
            let decoder = Function::construct(scope, class, &[string(scope, "utf8")?])?;
            interface.define_property(scope, c"_starlingDecoder", decoder, 0)?;
            decoder
        }
    };
    let text = call_method(scope, decoder, c"write", &[chunk])?;
    Ok(String::from_jsval(scope, text, ()).unwrap_or_default())
}

/// Handle the line `line`: answer the pending question with it, or emit it as a `line` event.
fn on_line(scope: &Scope<'_>, interface: Object<'_>, line: &str) -> Result<(), ExnThrown> {
    let line = js::JSString::from_str(scope, line)?.to_jsval_throwing(scope)?;
    let question = interface.val(scope, c"_starlingQuestion")?;
    if is_function(scope, question) {
        interface.set_property(scope, c"_starlingQuestion", js::value::undefined())?;
        Function::call(scope, js::value::undefined(), question, &[line])?;
        return Ok(());
    }
    call_method(scope, interface, c"emit", &[string(scope, "line")?, line])?;
    Ok(())
}

/// Add `text` to the line being read, and handle each line it completes. A `\r` that ends one
/// piece of text and a `\n` that starts the next end a single line.
fn feed(scope: &Scope<'_>, interface: Object<'_>, text: &str) -> Result<(), ExnThrown> {
    let mut buffer =
        String::from_jsval(scope, interface.val(scope, c"line")?, ()).unwrap_or_default();
    let mut text = text;
    if interface.flag(scope, c"_starlingCarriage")? {
        text = text.strip_prefix('\n').unwrap_or(text);
        interface.set_property(scope, c"_starlingCarriage", false)?;
    }
    buffer.push_str(text);
    let mut lines = Vec::new();
    let mut rest = buffer.as_str();
    while let Some(end) = rest.find(['\r', '\n']) {
        lines.push(&rest[..end]);
        let after = &rest[end..];
        rest = if let Some(after_crlf) = after.strip_prefix("\r\n") {
            after_crlf
        } else {
            if after == "\r" {
                interface.set_property(scope, c"_starlingCarriage", true)?;
            }
            &after[1..]
        };
    }
    interface.set_property(scope, c"line", rest)?;
    for line in lines {
        on_line(scope, interface, line)?;
        if interface.flag(scope, c"closed")? {
            return Ok(());
        }
    }
    Ok(())
}

/// The `data` listener of an interface's input, whose captures are `[interface]`.
fn on_data(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let interface = Captures::of(scope, payload)?.obj(0)?;
    if !interface.flag(scope, c"closed")? {
        let text = decode(scope, interface, args.get(0))?;
        feed(scope, interface, &text)?;
    }
    Ok(js::value::undefined())
}

/// The `end` listener of an interface's input, whose captures are `[interface]`: handle the last
/// line, if it has no line ending, and close the interface.
fn on_end(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let interface = Captures::of(scope, payload)?.obj(0)?;
    if interface.flag(scope, c"closed")? {
        return Ok(js::value::undefined());
    }
    let line = String::from_jsval(scope, interface.val(scope, c"line")?, ()).unwrap_or_default();
    if !line.is_empty() {
        interface.set_property(scope, c"line", "")?;
        on_line(scope, interface, &line)?;
    }
    interface_class::close(scope, interface)?;
    Ok(js::value::undefined())
}

/// The `error` listener of an interface's input, whose captures are `[interface]`: emit the
/// error as the interface's `error` event.
fn on_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let interface = Captures::of(scope, payload)?.obj(0)?;
    call_method(
        scope,
        interface,
        c"emit",
        &[string(scope, "error")?, args.get(0)],
    )?;
    Ok(js::value::undefined())
}

/// Write `text` to the interface's output, if it has one.
fn write_output(scope: &Scope<'_>, interface: Object<'_>, text: &str) -> Result<(), ExnThrown> {
    if let Some(output) = interface.obj(scope, c"output")? {
        call_method(scope, output, c"write", &[string(scope, text)?])?;
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
    if interface.flag(scope, c"closed")? {
        return Err(use_after_close().into());
    }
    if is_function(scope, interface.val(scope, c"_starlingQuestion")?) {
        interface_class::prompt(scope, interface)?;
        return Ok(());
    }
    interface.set_property(scope, c"_starlingQuestion", callback)?;
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
    let (input, output, prompt) = match Object::from_value(scope, *input) {
        Ok(options) if !options.val(scope, c"input")?.is_undefined() => (
            options.val(scope, c"input")?,
            options.val(scope, c"output")?,
            options.val(scope, c"prompt")?,
        ),
        _ => (
            scope.root_value(input.get()),
            output.map_or_else(
                || scope.root_value(js::value::undefined()),
                |output| scope.root_value(output.get()),
            ),
            scope.root_value(js::value::undefined()),
        ),
    };
    let input_object = Object::from_value(scope, *input)
        .map_err(|_| NodeError::invalid_arg_type(scope, "input", "an instance of Stream", input))?;
    this.set_property(scope, c"input", input)?;
    this.set_property(scope, c"output", output)?;
    this.set_property(scope, c"terminal", false)?;
    this.set_property(scope, c"line", "")?;
    this.set_property(scope, c"cursor", 0)?;
    this.set_property(scope, c"history", *js::Array::new(scope, 0)?)?;
    this.set_property(scope, c"closed", false)?;
    this.set_property(scope, c"paused", false)?;
    if prompt.is_string() {
        this.set_property(scope, c"_starlingPrompt", prompt)?;
    } else {
        this.set_property(scope, c"_starlingPrompt", "> ")?;
    }
    let data = closure(scope, 1, on_data, &[this.to_jsval_raw_throwing(scope)?])?;
    let end = closure(scope, 0, on_end, &[this.to_jsval_raw_throwing(scope)?])?;
    let error = closure(scope, 1, on_error, &[this.to_jsval_raw_throwing(scope)?])?;
    this.define_property(scope, c"_starlingOnData", data, 0)?;
    this.define_property(scope, c"_starlingOnEnd", end, 0)?;
    this.define_property(scope, c"_starlingOnError", error, 0)?;
    call_method(
        scope,
        input_object,
        c"on",
        &[string(scope, "error")?, error.to_jsval_throwing(scope)?],
    )?;
    call_method(
        scope,
        input_object,
        c"on",
        &[string(scope, "data")?, data.to_jsval_throwing(scope)?],
    )?;
    call_method(
        scope,
        input_object,
        c"on",
        &[string(scope, "end")?, end.to_jsval_throwing(scope)?],
    )?;
    if crate::stream::has_method(scope, input_object, c"resume")? {
        call_method(scope, input_object, c"resume", &[])?;
    }
    Ok(())
}

/// The `line` listener of an async iterator, whose captures are `[state]`: fulfill the oldest
/// pending `next()` with the line, or queue it.
fn iterator_line(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Captures::of(scope, payload)?.obj(0)?;
    let waiting = state.obj(scope, c"waiting")?.ok_or(ExnThrown)?;
    if waiting.num(scope, c"length")? > 0.0 {
        let promise = call_method(scope, waiting, c"shift", &[])?;
        let promise = Object::from_value(scope, *promise)
            .map_err(|e| e.throw(scope))?
            .cast::<js::Promise>()
            .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))?;
        let result = js::iteration::create_iter_result(scope, args.get(0), false)?;
        promise.resolve(scope, result.to_jsval_throwing(scope)?)?;
    } else {
        call_method(
            scope,
            state.obj(scope, c"queue")?.ok_or(ExnThrown)?,
            c"push",
            &[args.get(0)],
        )?;
    }
    Ok(js::value::undefined())
}

/// The `close` listener of an async iterator, whose captures are `[state]`: fulfill every pending
/// `next()` as done.
fn iterator_close(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Captures::of(scope, payload)?.obj(0)?;
    state.set_property(scope, c"done", true)?;
    let waiting = state.obj(scope, c"waiting")?.ok_or(ExnThrown)?;
    while waiting.num(scope, c"length")? > 0.0 {
        let promise = call_method(scope, waiting, c"shift", &[])?;
        let promise = Object::from_value(scope, *promise)
            .map_err(|e| e.throw(scope))?
            .cast::<js::Promise>()
            .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))?;
        let result = js::iteration::create_iter_result(scope, js::value::undefined(), true)?;
        promise.resolve(scope, result.to_jsval_throwing(scope)?)?;
    }
    Ok(js::value::undefined())
}

/// The `error` listener of an async iterator, whose captures are `[state]`: reject the oldest
/// pending `next()` with the error, or the next call of `next()` if none is pending, and end the
/// iteration.
fn iterator_error(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Captures::of(scope, payload)?.obj(0)?;
    state.set_property(scope, c"done", true)?;
    let waiting = state.obj(scope, c"waiting")?.ok_or(ExnThrown)?;
    if waiting.num(scope, c"length")? > 0.0 {
        let promise = call_method(scope, waiting, c"shift", &[])?;
        let promise = Object::from_value(scope, *promise)
            .map_err(|e| e.throw(scope))?
            .cast::<js::Promise>()
            .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))?;
        promise.reject(scope, args.get(0))?;
    } else {
        state.set_property(scope, c"error", args.get(0))?;
    }
    Ok(js::value::undefined())
}

/// `next()` of an async iterator, whose captures are `[state]`: a promise for the next line.
fn iterator_next(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Captures::of(scope, payload)?.obj(0)?;
    let queue = state.obj(scope, c"queue")?.ok_or(ExnThrown)?;
    if queue.num(scope, c"length")? > 0.0 {
        let line = call_method(scope, queue, c"shift", &[])?;
        let result = js::iteration::create_iter_result(scope, line, false)?;
        return js::Promise::new_resolved_with_value(scope, result)?.to_jsval_raw_throwing(scope);
    }
    let error = state.val(scope, c"error")?;
    if !error.is_undefined() {
        state.set_property(scope, c"error", js::value::undefined())?;
        return js::Promise::new_rejected_with_error(scope, error)?.to_jsval_raw_throwing(scope);
    }
    if state.flag(scope, c"done")? {
        let result = js::iteration::create_iter_result(scope, js::value::undefined(), true)?;
        return js::Promise::new_resolved_with_value(scope, result)?.to_jsval_raw_throwing(scope);
    }
    let promise = js::Promise::new_pending(scope)?;
    call_method(
        scope,
        state.obj(scope, c"waiting")?.ok_or(ExnThrown)?,
        c"push",
        &[promise.to_jsval_throwing(scope)?],
    )?;
    promise.to_jsval_raw_throwing(scope)
}

/// `return()` of an async iterator, whose captures are `[state, interface]`: close the interface,
/// and return a promise for a done result.
fn iterator_return(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    interface_class::close(scope, captures.obj(1)?)?;
    let result = js::iteration::create_iter_result(scope, js::value::undefined(), true)?;
    js::Promise::new_resolved_with_value(scope, result)?.to_jsval_raw_throwing(scope)
}

/// `[Symbol.asyncIterator]()` of an async iterator: the iterator itself.
fn iterator_self(
    _scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(args.this())
}

/// `[Symbol.asyncIterator]()` of an interface: an async iterator over its lines, which ends once
/// it closes.
fn async_iterator(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let interface =
        Object::from_value(scope, scope.root_value(args.this())).map_err(|e| e.throw(scope))?;
    let state = Object::new_plain(scope)?;
    state.set_property(scope, c"queue", *js::Array::new(scope, 0)?)?;
    state.set_property(scope, c"waiting", *js::Array::new(scope, 0)?)?;
    state.set_property(scope, c"done", interface.flag(scope, c"closed")?)?;
    let line = closure(
        scope,
        1,
        iterator_line,
        &[state.to_jsval_raw_throwing(scope)?],
    )?;
    call_method(
        scope,
        interface,
        c"on",
        &[string(scope, "line")?, line.to_jsval_throwing(scope)?],
    )?;
    let close = closure(
        scope,
        0,
        iterator_close,
        &[state.to_jsval_raw_throwing(scope)?],
    )?;
    call_method(
        scope,
        interface,
        c"once",
        &[string(scope, "close")?, close.to_jsval_throwing(scope)?],
    )?;
    let error = closure(
        scope,
        1,
        iterator_error,
        &[state.to_jsval_raw_throwing(scope)?],
    )?;
    call_method(
        scope,
        interface,
        c"once",
        &[string(scope, "error")?, error.to_jsval_throwing(scope)?],
    )?;
    let iterator = Object::new_plain(scope)?;
    let next = closure(
        scope,
        0,
        iterator_next,
        &[state.to_jsval_raw_throwing(scope)?],
    )?;
    iterator.set_property(scope, c"next", next)?;
    let finish = closure(
        scope,
        0,
        iterator_return,
        &[
            state.to_jsval_raw_throwing(scope)?,
            interface.to_jsval_raw_throwing(scope)?,
        ],
    )?;
    iterator.set_property(scope, c"return", finish)?;
    let itself = Function::new_callback(scope, c"[Symbol.asyncIterator]", 0, iterator_self, ())?;
    js::class::define_well_known_symbol_property(
        scope,
        iterator,
        js::native::SymbolCode::asyncIterator,
        *itself,
    )?;
    iterator.to_jsval_raw_throwing(scope)
}

#[core_runtime::jsfunction_class(
    name = "Interface",
    extends = "crate::events::event_emitter",
    self_construct
)]
pub mod interface_class {
    use super::*;

    /// <https://nodejs.org/api/readline.html#class-interfaceconstructor>
    #[constructor]
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
        if this.flag(scope, c"closed")? {
            return Ok(());
        }
        if let Some(input) = this.obj(scope, c"input")? {
            let listeners = [
                ("data", c"_starlingOnData"),
                ("end", c"_starlingOnEnd"),
                ("error", c"_starlingOnError"),
            ];
            for (event, listener) in listeners {
                let listener = this.val(scope, listener)?;
                call_method(
                    scope,
                    input,
                    c"removeListener",
                    &[string(scope, event)?, listener],
                )?;
            }
        }
        interface_class::pause(scope, this)?;
        this.set_property(scope, c"closed", true)?;
        call_method(scope, this, c"emit", &[string(scope, "close")?])?;
        Ok(())
    }

    /// <https://nodejs.org/api/readline.html#rlpause>
    pub fn pause<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        if !this.flag(scope, c"paused")? {
            if let Some(input) = this.obj(scope, c"input")? {
                call_method(scope, input, c"pause", &[])?;
            }
            this.set_property(scope, c"paused", true)?;
            call_method(scope, this, c"emit", &[string(scope, "pause")?])?;
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/readline.html#rlresume>
    pub fn resume<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        if this.flag(scope, c"paused")? && !this.flag(scope, c"closed")? {
            if let Some(input) = this.obj(scope, c"input")? {
                call_method(scope, input, c"resume", &[])?;
            }
            this.set_property(scope, c"paused", false)?;
            call_method(scope, this, c"emit", &[string(scope, "resume")?])?;
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/readline.html#rlquestionquery-options-callback>
    pub fn question(
        scope: &Scope<'_>,
        this: Object<'_>,
        query: HandleValue<'_>,
        options: HandleValue<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let callback = if is_function(scope, options) {
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
        if is_function(scope, callback) {
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
        this.set_property(scope, c"_starlingPrompt", prompt.as_str())
    }

    /// <https://nodejs.org/api/readline.html#rlgetprompt>
    pub fn get_prompt<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        this.val(scope, c"_starlingPrompt")
    }

    /// <https://nodejs.org/api/readline.html#rlpromptpreservecursor>
    pub fn prompt(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        if this.flag(scope, c"closed")? {
            return Err(use_after_close().into());
        }
        interface_class::resume(scope, this)?;
        let prompt =
            String::from_jsval(scope, this.val(scope, c"_starlingPrompt")?, ()).unwrap_or_default();
        super::write_output(scope, this, &prompt)?;
        Ok(())
    }

    /// <https://nodejs.org/api/readline.html#rlwritedata-key>: handle `data` as input.
    pub fn write(scope: &Scope<'_>, this: Object<'_>, data: HandleValue<'_>) -> Result<(), Thrown> {
        if this.flag(scope, c"closed")? {
            return Err(use_after_close().into());
        }
        if data.is_string() {
            let text = String::from_jsval(scope, data, ())?;
            super::feed(scope, this, &text)?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/readline.html#rlgetcursorpos>
    pub fn get_cursor_pos<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<Object<'s>, ExnThrown> {
        let position = Object::new_plain(scope)?;
        position.set_property(scope, c"rows", 0)?;
        position.set_property(scope, c"cols", this.val(scope, c"cursor")?)?;
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

/// The answer callback of a promise question, whose captures are `[promise]`: fulfill the
/// promise with the answer.
fn resolve_answer(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let promise = Captures::of(scope, payload)?
        .obj(0)?
        .cast::<js::Promise>()
        .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))?;
    promise.resolve(scope, args.get(0))?;
    Ok(js::value::undefined())
}

#[core_runtime::jsfunction_class(name = "Interface", extends = "super::interface_class")]
pub mod promises_interface_class {
    use super::*;

    /// <https://nodejs.org/api/readline.html#class-readlinepromisesinterface>
    #[constructor]
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
        let answer = closure(
            scope,
            1,
            super::resolve_answer,
            &[promise.to_jsval_raw_throwing(scope)?],
        )?;
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
    let promise = match promise {
        Some(promise) => promise.to_jsval_throwing(scope)?,
        None => scope.root_value(js::value::undefined()),
    };
    let on_abort = closure(
        scope,
        0,
        question_aborted,
        &[
            interface.to_jsval_raw_throwing(scope)?,
            signal.get(),
            promise.get(),
        ],
    )?;
    let signal_object = Object::from_value(scope, *signal).map_err(|e| e.throw(scope))?;
    let once = Object::new_plain(scope)?;
    once.set_property(scope, c"once", true)?;
    call_method(
        scope,
        signal_object,
        c"addEventListener",
        &[
            string(scope, "abort")?,
            on_abort.to_jsval_throwing(scope)?,
            once.to_jsval_throwing(scope)?,
        ],
    )?;
    let answered = closure(
        scope,
        1,
        question_answered,
        &[
            signal.get(),
            on_abort.to_jsval_raw_throwing(scope)?,
            callback.get(),
        ],
    )?;
    answered.to_jsval_throwing(scope)
}

/// The abort listener of a question, whose captures are `[interface, signal, promise]`: cancel
/// the question, and reject the promise, if there is one, with an `AbortError`.
fn question_aborted(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let interface = captures.obj(0)?;
    interface.set_property(scope, c"_starlingQuestion", js::value::undefined())?;
    let promise = captures.get(2)?;
    if let Ok(promise) = Object::from_value(scope, *promise) {
        let promise = promise
            .cast::<js::Promise>()
            .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))?;
        let reason = captures.obj(1)?.val(scope, c"reason")?;
        let error = crate::errors::abort_error(scope, Some(reason))?;
        promise.reject(scope, error)?;
    }
    Ok(js::value::undefined())
}

/// The answer callback of a question with a signal, whose captures are `[signal, onAbort,
/// callback]`: remove the abort listener, then call the callback with the answer.
fn question_answered(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let on_abort = captures.get(1)?;
    call_method(
        scope,
        captures.obj(0)?,
        c"removeEventListener",
        &[string(scope, "abort")?, on_abort],
    )?;
    let callback = captures.get(2)?;
    if is_function(scope, callback) {
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
    if let Some(callback) = callback.filter(|callback| is_function(scope, *callback)) {
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
    let callback = callback.filter(|callback| is_function(scope, *callback));
    if stream.is_null_or_undefined() {
        return without_stream(scope, callback);
    }
    let stream = Object::from_value(scope, *stream).map_err(|e| e.throw(scope))?;
    let code = string(scope, code)?;
    let written = match callback {
        Some(callback) => call_method(scope, stream, c"write", &[code, callback])?,
        None => call_method(scope, stream, c"write", &[code])?,
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
                        1,
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
    pub fn create_interface<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let class = super::interface_class::class_constructor(scope)?;
        super::create(scope, class, &args)
    }

    /// <https://nodejs.org/api/readline.html#readlineclearlinestream-dir-callback>
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
    pub fn clear_screen_down(
        scope: &Scope<'_>,
        stream: HandleValue<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<bool, Thrown> {
        validate_callback(scope, callback)?;
        Ok(super::write_escape(scope, stream, "\x1b[0J", callback)?)
    }

    /// <https://nodejs.org/api/readline.html#readlinecursortostream-x-y-callback>
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
        if is_function(scope, y) {
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
