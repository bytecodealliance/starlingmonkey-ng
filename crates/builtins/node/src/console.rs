// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The `console` global's formatting under `--node-compat`. `log`, `info`, `debug`, `warn`,
//! `error` and `trace` format their arguments with `util.format`, as Node.js's do, and pass the
//! text to the runtime's method of the same name, which prints it. `dir` prints `util.inspect` of
//! its argument with its options through `log`. `assert` calls `warn` with Node's `Assertion
//! failed` message. `Console` creates consoles that write to the given streams.

use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{ErrorKind, NodeError};
use crate::inspect::InspectOptions;
use crate::stream::{
    call_method, has_method, is_function, string, take_exception, truthy, Captures, Props,
};

/// Wrap the methods of `global`'s `console`, if it has one.
pub(crate) fn install(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    let Some(console) = global.obj(scope, c"console")? else {
        return Ok(());
    };
    for name in [c"log", c"info", c"debug", c"warn", c"error", c"trace"] {
        let original = console.val(scope, name)?;
        if !is_function(scope, original) {
            continue;
        }
        let payload = js::Array::with_contents(scope, &[original])?;
        let wrapper = Function::new_callback(scope, name, 0, formatted, *payload)?;
        console.set_property(scope, name, *wrapper)?;
    }
    let log = console.val(scope, c"log")?;
    let payload = js::Array::with_contents(scope, &[log])?;
    let dir = Function::new_callback(scope, c"dir", 0, dir, *payload)?;
    console.set_property(scope, c"dir", *dir)?;
    let assert = Function::new_callback(scope, c"assert", 0, assert, ())?;
    console.set_property(scope, c"assert", *assert)?;
    let class = Function::new_constructor_callback(scope, c"Console", 1, console_class, ())?;
    console.set_property(scope, c"Console", *class)
}

/// `new Console(stdout[, stderr])` or `new Console({ stdout, stderr })`: a console whose `log`,
/// `info`, `debug` and `dirxml` write their `util.format`ted arguments and a newline to `stdout`,
/// and whose `warn`, `error` and `trace` write to `stderr`, which defaults to `stdout`. `dir`
/// writes to `stdout` and `assert` calls `warn`. The other methods are the global `console`'s.
/// Throws `ERR_CONSOLE_WRITABLE_STREAM` if a stream has no `write` method.
fn console_class(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let first = args.get(0);
    let (stdout, stderr) = match Object::from_value(scope, first) {
        Ok(options) if !has_method(scope, options, c"write")? => (
            options.val(scope, c"stdout")?,
            options.val(scope, c"stderr")?,
        ),
        _ => (
            scope.root_value(first.get()),
            scope.root_value(args.get(1).get()),
        ),
    };
    let stderr = if stderr.is_undefined() {
        stdout
    } else {
        stderr
    };
    for (stream, name) in [(stdout, "stdout"), (stderr, "stderr")] {
        let writable = match Object::from_value(scope, stream) {
            Ok(object) => has_method(scope, object, c"write")?,
            Err(_) => false,
        };
        if !writable {
            return Err(NodeError::new(
                ErrorKind::TypeError,
                "ERR_CONSOLE_WRITABLE_STREAM",
                format!("Console expects a writable stream instance for {name}"),
            )
            .throw(scope));
        }
    }
    let console = Object::new_plain(scope)?;
    if let Some(global) = scope.global().obj(scope, c"console")? {
        for name in [
            c"group",
            c"groupCollapsed",
            c"groupEnd",
            c"count",
            c"countReset",
            c"time",
            c"timeEnd",
            c"timeLog",
            c"table",
            c"clear",
        ] {
            console.set_property(scope, name, global.val(scope, name)?)?;
        }
    }
    for (name, stream) in [
        (c"log", stdout),
        (c"info", stdout),
        (c"debug", stdout),
        (c"dirxml", stdout),
        (c"warn", stderr),
        (c"error", stderr),
        (c"trace", stderr),
    ] {
        let payload = js::Array::with_contents(scope, &[stream])?;
        let method = Function::new_callback(scope, name, 0, write_formatted, *payload)?;
        console.set_property(scope, name, *method)?;
    }
    let payload = js::Array::with_contents(scope, &[stdout])?;
    let dir = Function::new_callback(scope, c"dir", 0, write_dir, *payload)?;
    console.set_property(scope, c"dir", *dir)?;
    let assert = Function::new_callback(scope, c"assert", 0, assert, ())?;
    console.set_property(scope, c"assert", *assert)?;
    Ok(console.as_value())
}

/// A method of a `Console`, whose captures are `[stream]`: write the arguments formatted by
/// `util.format` and a newline to `stream`.
fn write_formatted(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.get(0)?;
    let arguments = crate::callback_args(&args);
    let text = crate::inspect::format(scope, &arguments, &InspectOptions::current(scope)?)?;
    write_line(scope, stream, &text)
}

/// `dir(object, options)` of a `Console`, whose captures are `[stdout]`.
fn write_dir(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let stream = Captures::of(scope, payload)?.get(0)?;
    let mut options = InspectOptions::current(scope)?;
    if let Ok(given) = Object::from_value(scope, args.get(1)) {
        options = options.read(scope, given)?;
    }
    options.custom_inspect = false;
    let text = crate::inspect::inspect(scope, args.get(0), options)?;
    write_line(scope, stream, &text)
}

/// Call `stream.write` with `text` and a newline. An exception `write` throws is discarded, as
/// with Node's default `ignoreErrors`.
fn write_line(scope: &Scope<'_>, stream: HandleValue<'_>, text: &str) -> Result<Value, ExnThrown> {
    let stream = Object::from_value(scope, stream).map_err(|e| e.throw(scope))?;
    let line = string(scope, &format!("{text}\n"))?;
    if call_method(scope, stream, c"write", &[line]).is_err() {
        take_exception(scope);
    }
    Ok(js::value::undefined())
}

/// A wrapped method, whose captures are `[original]`: call the runtime's method with the
/// arguments formatted by `util.format`.
fn formatted(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let original = Captures::of(scope, payload)?.get(0)?;
    let arguments = crate::callback_args(&args);
    let text = crate::inspect::format(scope, &arguments, &InspectOptions::current(scope)?)?;
    let this = scope.root_value(args.this());
    Function::call(scope, this, original, &[string(scope, &text)?])?;
    Ok(js::value::undefined())
}

/// `console.dir(object, options)`, whose captures are `[log]`: print `util.inspect` of `object`
/// with `options` and without custom inspection.
fn dir(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let log = Captures::of(scope, payload)?.get(0)?;
    let mut options = InspectOptions::current(scope)?;
    if let Ok(given) = Object::from_value(scope, args.get(1)) {
        options = options.read(scope, given)?;
    }
    options.custom_inspect = false;
    let text = crate::inspect::inspect(scope, args.get(0), options)?;
    let this = scope.root_value(args.this());
    Function::call(scope, this, log, &[string(scope, &text)?])?;
    Ok(js::value::undefined())
}

/// `console.assert(value, ...args)`: if `value` is falsy, call `console.warn` with the arguments,
/// the first prefixed with `Assertion failed: `, or with just `Assertion failed`.
fn assert(scope: &Scope<'_>, args: CallbackArgs<'_>, _: HandleValue) -> Result<Value, ExnThrown> {
    if truthy(scope, args.get(0)) {
        return Ok(js::value::undefined());
    }
    let mut arguments = crate::callback_args(&args);
    if !arguments.is_empty() {
        arguments.remove(0);
    }
    let prefix = match arguments.first() {
        None => "Assertion failed".to_string(),
        Some(first) => {
            let first = js::JSString::from_value(scope, *first)?.to_utf8(scope)?;
            format!("Assertion failed: {first}")
        }
    };
    let prefix = string(scope, &prefix)?;
    match arguments.first_mut() {
        Some(first) => *first = prefix,
        None => arguments.push(prefix),
    }
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    call_method(scope, this, c"warn", &arguments)?;
    Ok(js::value::undefined())
}
