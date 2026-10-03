// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The `console` global's formatting under `--node-compat`. `log`, `info`, `debug`, `warn`,
//! `error` and `trace` format their arguments with `util.format`, as Node.js's do, and pass the
//! text to the runtime's method of the same name, which prints it. `dir` prints `util.inspect` of
//! its argument with its options through `log`. `assert` calls `warn` with Node's `Assertion
//! failed` message.

use js::error::ExnThrown;
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::inspect::InspectOptions;
use crate::stream::{call_method, is_function, string, truthy, Captures, Props};

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
    console.set_property(scope, c"assert", *assert)
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
