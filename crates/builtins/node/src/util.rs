// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:util` and `node:util/types`. See <https://nodejs.org/api/util.html>.

use std::ffi::CStr;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::object::ESClass;
use js::prelude::HandleValue;
use js::typedarray::ViewKind;
use js::{Function, Object};

use crate::errors::{
    out_of_range_received, validate_boolean, validate_function, validate_integer,
    validate_plain_object, validate_string, ErrorKind, NodeError, Thrown,
};
use crate::inspect::{self, InspectOptions};

/// The symbol a function's promisified version is stored under,
/// `Symbol.for('nodejs.util.promisify.custom')`.
pub(crate) static PROMISIFY_CUSTOM: js::symbol::CachedSymbol =
    js::symbol::CachedSymbol::registered(c"nodejs.util.promisify.custom");

/// Node's internal `customPromisifyArgs` symbol. A function's property under it names the values
/// its callback passes after the error, and its promisified version resolves with an object of
/// them.
pub(crate) static CUSTOM_PROMISIFY_ARGS: js::symbol::CachedSymbol =
    js::symbol::CachedSymbol::new(c"customPromisifyArgs");

/// Define `function[customPromisifyArgs]` as an array of `names`, non-enumerable, non-writable and
/// non-configurable, as Node.js defines it on its functions.
pub(crate) fn define_promisify_args(
    scope: &Scope<'_>,
    function: Object<'_>,
    names: &[&str],
) -> Result<(), ExnThrown> {
    let names = js::Array::with_contents(scope, names)?;
    let id = CUSTOM_PROMISIFY_ARGS.id(scope)?;
    let attrs = js::class_spec::JSPROP_READONLY | js::class_spec::JSPROP_PERMANENT;
    function.define_value_by_id(scope, id, names.to_jsval_throwing(scope)?, attrs as u32)
}

/// The body of `util.inspect(value, options)`. A boolean `options` is `showHidden`, and a third
/// and fourth argument other than `undefined` are `depth` and `colors`, as in the legacy
/// `inspect(value, showHidden, depth, colors)` form.
fn inspect_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let mut options = InspectOptions::current(scope)?;
    if args.len() > 2 && !args.get(2).is_undefined() {
        options.set(scope, c"depth", args.get(2))?;
    }
    if args.len() > 3 && !args.get(3).is_undefined() {
        options.set(scope, c"colors", args.get(3))?;
    }
    if args.get(1).is_boolean() {
        options.show_hidden = args.get(1).to_boolean();
    } else if let Ok(given) = Object::from_value(scope, *args.get(1)) {
        options = options.read(scope, given)?;
    }
    let text = inspect::inspect(scope, args.get(0), options)?;
    text.as_str().to_jsval_raw_throwing(scope)
}

/// The object `util.inspect.defaultOptions` holds: the default options without `stylize`, sealed
/// as Node.js seals its `inspectDefaultOptions`.
fn default_options_object<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let options = InspectOptions::default().to_object(scope)?;
    let defaults = Object::new_plain(scope)?;
    let attrs = js::class_spec::JSPROP_ENUMERATE | js::class_spec::JSPROP_PERMANENT;
    for key in inspect::CUSTOM_OPTIONS_PROPERTIES.keys() {
        if let js::object::TemplateKey::Name(name) = key {
            if *name != c"stylize" {
                let value = options.get_property(scope, name)?;
                defaults.define_property(scope, name, value, attrs as u32)?;
            }
        }
    }
    defaults.try_prevent_extensions(scope)?;
    Ok(defaults)
}

/// The getter of `util.inspect.defaultOptions`, whose payload is the options object.
fn default_options_get(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(payload.get())
}

/// The setter of `util.inspect.defaultOptions`, whose payload is the options object: copy the
/// properties of the object assigned onto it.
fn default_options_set(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let run = || -> Result<(), Thrown> {
        let options = validate_plain_object(scope, args.get(0), "options", false)?;
        let defaults = Object::from_value(scope, *payload)?;
        defaults.assign(scope, options)?;
        Ok(())
    };
    run().map_err(|error| error.throw(scope))?;
    Ok(js::value::undefined())
}

/// The state of a function `util.promisify` returned for a function with `customPromisifyArgs`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct Promisified {
    original: Heap<js::object::Object>,
    /// The original function's `customPromisifyArgs`.
    names: Heap<Value>,
}

#[core_runtime::jsmethods]
impl Promisified {}

/// The state of the callback a promisified function with `customPromisifyArgs` passes on.
#[core_runtime::jsclass(hidden)]
pub(crate) struct PromisifySettle {
    promise: Heap<js::promise::Promise>,
    /// The original function's `customPromisifyArgs`.
    names: Heap<Value>,
}

#[core_runtime::jsmethods]
impl PromisifySettle {}

/// The body of a promisified function, whose payload is the original function, or its
/// [`Promisified`] state if it has `customPromisifyArgs`: call it with the arguments and a
/// callback that settles the returned promise. A promise returned by the original function emits
/// the `DEP0174` warning.
fn promisified(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let promise = js::Promise::new_pending(scope)?;
    let state = Object::from_value(scope, *payload)
        .map_err(|e| e.throw(scope))?
        .cast::<Promisified>()
        .ok();
    let (original, settle) = match state {
        Some(state) => {
            let (original, names) = {
                let data = state.data();
                (data.original.get(scope), data.names.get(scope))
            };
            let settle = js::class::create_instance_with::<PromisifySettleImpl>(scope, |_| {
                PromisifySettleImpl {
                    promise: Heap::from(promise),
                    names: Heap::from(names.get()),
                }
            })?;
            let settle = Function::new_callback(scope, c"", 1, settle_with_names, settle)?;
            (original.to_jsval_throwing(scope)?, settle)
        }
        None => (
            payload,
            Function::new_callback(scope, c"", 1, settle_promise, *promise)?,
        ),
    };
    let mut call_args = args.rest(0);
    call_args.push((*settle).to_jsval_throwing(scope)?);
    match Function::call(scope, args.this(), original, &call_args) {
        Ok(result) => {
            if Object::from_value(scope, *result)
                .is_ok_and(|result| js::Promise::is_promise(result.handle()))
            {
                crate::process::deprecation_warning(
                    scope,
                    "Calling promisify on a function that returns a Promise is likely a mistake.",
                    "DEP0174",
                )?;
            }
        }
        Err(ExnThrown) => promise.reject_with_pending(scope)?,
    }
    promise.to_jsval_raw_throwing(scope)
}

/// The callback a promisified function passes on, whose payload is the promise: reject it with a
/// truthy first argument, and resolve it with the second argument otherwise.
fn settle_promise(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let promise = js::function::cast_payload::<js::Promise>(scope, payload);
    let error = args.get(0);
    if bool::from_jsval(scope, error, ()).unwrap_or(false) {
        promise.reject(scope, error)?;
    } else {
        promise.resolve(scope, args.get(1))?;
    }
    Ok(js::value::undefined())
}

/// The callback a promisified function with `customPromisifyArgs` passes on, whose payload is a
/// [`PromisifySettle`]. A truthy first argument rejects the promise. Otherwise, a call with more
/// than two arguments resolves it with an object that maps the `customPromisifyArgs` names to the
/// arguments after the first, in order, and any other call resolves it with the second argument.
/// Names that aren't an object map nothing.
fn settle_with_names(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<PromisifySettle>(scope, payload);
    let (promise, names) = {
        let data = state.data();
        (data.promise.get(scope), data.names.get(scope))
    };
    let error = args.get(0);
    if bool::from_jsval(scope, error, ()).unwrap_or(false) {
        promise.reject(scope, error)?;
        return Ok(js::value::undefined());
    }
    if args.len() <= 2 {
        promise.resolve(scope, args.get(1))?;
        return Ok(js::value::undefined());
    }
    let values = || -> Result<Object<'_>, ExnThrown> {
        let result = Object::new_plain(scope)?;
        let Ok(names) = Object::from_value(scope, *names) else {
            return Ok(result);
        };
        let length = f64::from_jsval_throwing(scope, names.get_property(scope, c"length")?, ())?;
        let mut index = 0;
        while f64::from(index) < length {
            let name = names.get_element(scope, index)?;
            let id = scope.root_id(js::id::value_to_id(scope, name)?);
            result.set_property_by_id(scope, id, args.get(index + 1))?;
            index += 1;
        }
        Ok(result)
    };
    match values() {
        Ok(result) => promise.resolve(scope, result)?,
        Err(ExnThrown) => promise.reject_with_pending(scope)?,
    }
    Ok(js::value::undefined())
}

/// `Object.defineProperties(target, Object.getOwnPropertyDescriptors(source))`, with the
/// descriptor of each property with a string key passed to `adjust` first.
fn copy_descriptors(
    scope: &Scope<'_>,
    target: Object<'_>,
    source: Object<'_>,
    adjust: impl Fn(&Scope<'_>, &str, Object<'_>) -> Result<(), ExnThrown>,
) -> Result<(), ExnThrown> {
    let mut descriptors = Vec::new();
    for key in source.own_keys(scope)? {
        let id = scope.root_id(js::id::value_to_id(scope, key)?);
        let descriptor = source.own_property_descriptor_value(scope, id)?;
        let Ok(object) = Object::from_value(scope, *descriptor) else {
            continue;
        };
        object.set_prototype(scope, js::native::HandleObject::null())?;
        if let Some(name) = crate::support::rust_string(scope, key) {
            adjust(scope, &name, object)?;
        }
        descriptors.push((key, id, descriptor));
    }
    for (key, id, descriptor) in descriptors {
        if !target.define_property_from_descriptor(scope, id, descriptor)? {
            let key = crate::errors::js_string(scope, key)?;
            let message = format!("Cannot redefine property: {key}");
            return Err(js::error::TypeError(message).throw(scope));
        }
    }
    Ok(())
}

/// Define `function[symbol]` as `value`, non-enumerable and non-writable.
fn define_custom(
    scope: &Scope<'_>,
    function: Object<'_>,
    symbol: HandleValue<'_>,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let id = scope.root_id(js::id::value_to_id(scope, symbol)?);
    function.define_value_by_id(scope, id, value, js::class_spec::JSPROP_READONLY as u32)
}

/// The body of `util.promisify(original)`.
fn promisify_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let run = || -> Result<Value, Thrown> {
        let original = validate_function(scope, args.get(0), "original")?;
        let custom = PROMISIFY_CUSTOM.value(scope)?;
        let custom_id = PROMISIFY_CUSTOM.id(scope)?;
        let existing = original.get_property_by_id(scope, custom_id)?;
        if bool::from_jsval(scope, existing, ()).unwrap_or(false) {
            let existing = validate_function(scope, existing, "util.promisify.custom")?;
            define_custom(scope, existing, custom, existing.to_jsval_throwing(scope)?)?;
            return Ok(existing.to_jsval_raw_throwing(scope)?);
        }
        let names = original.get_property_by_id(scope, CUSTOM_PROMISIFY_ARGS.id(scope)?)?;
        let function = if names.is_undefined() {
            Function::new_callback(scope, c"fn", 0, promisified, original)?
        } else {
            let state =
                js::class::create_instance_with::<PromisifiedImpl>(scope, |_| PromisifiedImpl {
                    original: Heap::from(original),
                    names: Heap::from(names.get()),
                })?;
            Function::new_callback(scope, c"fn", 0, promisified, state)?
        };
        if let Some(proto) = original.get_prototype(scope)? {
            function.set_prototype(scope, proto.handle())?;
        }
        define_custom(scope, *function, custom, function.to_jsval_throwing(scope)?)?;
        copy_descriptors(scope, *function, original, |_, _, _| Ok(()))?;
        Ok(function.to_jsval_raw_throwing(scope)?)
    };
    run().map_err(|error| error.throw(scope))
}

/// The state of a call of a callbackified function: its callback, and the `this` of the call,
/// which the callback is called with.
#[core_runtime::jsclass(hidden)]
pub(crate) struct CallbackifyCall {
    callback: Heap<js::object::Object>,
    this: Heap<Value>,
}

#[core_runtime::jsmethods]
impl CallbackifyCall {}

/// The body of a callbackified function, whose payload is the original async function: call it
/// with all arguments but the last, the callback, and call the result's `then` with reactions that
/// queue a call of the callback with the outcome in the tick queue. A result that isn't an object
/// throws a `TypeError`.
fn callbackified(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let run = || -> Result<Value, Thrown> {
        let mut call_args = args.rest(0);
        let callback = call_args
            .pop()
            .unwrap_or(scope.root_value(js::value::undefined()));
        let callback = validate_function(scope, callback, "last argument")?;
        let this = scope.root_value(args.this());
        let call = js::class::create_instance_with::<CallbackifyCallImpl>(scope, |_| {
            CallbackifyCallImpl {
                callback: Heap::from(callback),
                this: Heap::from(this.get()),
            }
        })?;
        let result = Function::call(scope, this, payload, &call_args)?;
        let result = Object::from_value(scope, *result).map_err(|_| {
            js::error::TypeError("ReflectApply(...).then is not a function".into()).throw(scope)
        })?;
        let on_fulfilled = Function::new_callback(scope, c"", 1, callbackify_fulfilled, call)?;
        let on_rejected = Function::new_callback(scope, c"", 1, callbackify_rejected, call)?;
        Function::call_by_name(
            scope,
            result.handle(),
            c"then",
            &[*on_fulfilled, *on_rejected],
        )?;
        Ok(js::value::undefined())
    };
    run().map_err(|error| error.throw(scope))
}

/// The fulfillment reaction of a callbackified function's result, whose payload is the
/// [`CallbackifyCall`]: queue the call of the callback with `null` and the value.
fn callbackify_fulfilled(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    js::jobs::queue_native_tick(scope, call_back_fulfilled, payload, &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// The rejection reaction of a callbackified function's result, whose payload is the
/// [`CallbackifyCall`]: queue the call of the callback with the reason.
fn callbackify_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    js::jobs::queue_native_tick(scope, call_back_rejected, payload, &[args.get(0)])?;
    Ok(js::value::undefined())
}

/// The tick that calls a callbackified function's callback with `null` and the value, whose
/// payload is the [`CallbackifyCall`].
fn call_back_fulfilled(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let call = cast_payload::<CallbackifyCall>(scope, payload);
    let (callback, this) = {
        let data = call.data();
        (data.callback.get(scope), data.this.get(scope))
    };
    let null = scope.root_value(js::value::null());
    Function::call(scope, this, callback, &[null, args.get(0)])?;
    Ok(js::value::undefined())
}

/// The tick that calls a callbackified function's callback with the rejection reason, whose
/// payload is the [`CallbackifyCall`]. A falsy reason is wrapped in an `ERR_FALSY_VALUE_REJECTION`
/// error whose `reason` is the reason, because a falsy first argument reports success.
fn call_back_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let call = cast_payload::<CallbackifyCall>(scope, payload);
    let (callback, this) = {
        let data = call.data();
        (data.callback.get(scope), data.this.get(scope))
    };
    let mut reason = args.get(0);
    if !js::conversion::to_boolean(reason) {
        let error = NodeError::new(
            ErrorKind::Error,
            "ERR_FALSY_VALUE_REJECTION",
            "Promise was rejected with falsy value",
        )
        .to_value(scope)?;
        Object::from_value(scope, *error)
            .map_err(|e| e.throw(scope))?
            .set_property(scope, c"reason", reason)?;
        reason = error;
    }
    Function::call(scope, this, callback, &[reason])?;
    Ok(js::value::undefined())
}

/// The state of a function `util.deprecate` returned.
#[core_runtime::jsclass(hidden)]
pub(crate) struct Deprecated {
    /// The deprecated function.
    function: Heap<Value>,
    message: Heap<Value>,
    /// The deprecation code, or `None` if there is none.
    #[no_trace]
    code: Option<String>,
    /// Whether the function's deprecation warning was handled.
    #[no_trace]
    warned: bool,
}

#[core_runtime::jsmethods]
impl Deprecated {}

/// The body of a deprecated function, whose payload is its [`Deprecated`] state: emit the
/// deprecation warning the first time, unless `process.noDeprecation` is set or another function
/// deprecated with the same code has emitted it, then call or construct the function.
fn deprecated(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<Deprecated>(scope, payload);
    let process = Object::from_value(scope, *crate::process::process_value(scope)?);
    let silenced = match &process {
        Ok(process) => js::conversion::to_boolean(process.get_property(scope, c"noDeprecation")?),
        Err(_) => true,
    };
    if !silenced && !state.data().warned {
        state.data_mut().warned = true;
        let (message, code) = {
            let data = state.data();
            (data.message.get(scope), data.code.clone())
        };
        let first = code.as_deref().is_none_or(crate::process::mark_code_warned);
        if let (true, Ok(process)) = (first, process) {
            let kind = js::class::get_or_init_atom(scope, c"DeprecationWarning")?;
            match code {
                Some(code) => {
                    let code = scope.root_value(js::JSString::from_str(scope, &code)?.as_value());
                    let args = [message, kind, code];
                    Function::call_by_name(scope, process.handle(), c"emitWarning", &args)?
                }
                None => {
                    let args = [message, kind];
                    Function::call_by_name(scope, process.handle(), c"emitWarning", &args)?
                }
            };
        }
    }
    let function = state.data().function.get(scope);
    let call_args = args.rest(0);
    if args.is_constructing() {
        let new_target =
            Object::from_value(scope, args.new_target()).map_err(|e| e.throw(scope))?;
        let object =
            Function::construct_with_new_target(scope, function, new_target.handle(), &call_args)?;
        return object.to_jsval_raw_throwing(scope);
    }
    Ok(Function::call(scope, args.this(), function, &call_args)?.get())
}

/// Whether the `NODE_DEBUG` environment variable enables `section`: it lists sections separated
/// by commas or spaces, case-insensitively, with `*` matching any characters.
fn debug_enabled(section: &str) -> bool {
    let Some(spec) = platform::process::env_vars()
        .into_iter()
        .find(|(key, _)| key == "NODE_DEBUG")
        .map(|(_, value)| value)
    else {
        return false;
    };
    let section = section.to_ascii_lowercase();
    spec.split([',', ' '])
        .filter(|pattern| !pattern.is_empty())
        .any(|pattern| glob_matches(&pattern.to_ascii_lowercase(), &section))
}

/// Whether `text` matches `pattern`, in which `*` matches any characters.
fn glob_matches(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((prefix, rest)) => {
            let Some(text) = text.strip_prefix(prefix) else {
                return false;
            };
            (0..=text.len())
                .any(|start| text.is_char_boundary(start) && glob_matches(rest, &text[start..]))
        }
    }
}

/// Print `args`, formatted, to stderr as `SECTION PID: message`.
fn print_debug(
    scope: &Scope<'_>,
    section: &str,
    args: &[HandleValue<'_>],
) -> Result<(), ExnThrown> {
    let message = inspect::format(scope, args, &InspectOptions::default())?;
    // A wasm instance has no process ID, so we print 1 there.
    let pid = if cfg!(target_arch = "wasm32") {
        1
    } else {
        std::process::id()
    };
    eprintln!("{} {pid}: {message}", section.to_ascii_uppercase());
    Ok(())
}

/// The body of an enabled `debuglog` logger, whose payload is the section's name: print the
/// formatted arguments to stderr as `SECTION PID: message`.
fn debug_log(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let section = String::from_jsval(scope, payload, ()).unwrap_or_default();
    print_debug(scope, &section, &args.rest(0))?;
    Ok(js::value::undefined())
}

/// The state of a `debuglog` logger created with a callback.
#[core_runtime::jsclass(hidden)]
pub(crate) struct DebugLogger {
    /// The callback, which the logger's first call calls with the function that logs.
    callback: Heap<Value>,
    /// The section's name.
    #[no_trace]
    section: String,
    /// Whether `NODE_DEBUG` enables the section.
    #[no_trace]
    enabled: bool,
    /// Whether the logger was called.
    #[no_trace]
    called: bool,
}

#[core_runtime::jsmethods]
impl DebugLogger {}

/// A function named `name` that logs for `section` with [`debug_log`] if `enabled` and does
/// nothing otherwise, with an `enabled` property of `enabled`.
fn debug_function<'s>(
    scope: &'s Scope<'_>,
    name: &'static CStr,
    section: &str,
    enabled: bool,
) -> Result<Object<'s>, ExnThrown> {
    let function = if enabled {
        Function::new_callback(scope, name, 0, debug_log, section)?
    } else {
        Function::new_callback(scope, name, 0, do_nothing, ())?
    };
    function.set_property(scope, c"enabled", enabled)?;
    Ok(*function)
}

/// The body of a `debuglog` logger created with a callback, whose payload is its [`DebugLogger`]
/// state. The first call calls the callback with a function that logs for the section, and every
/// call then logs if the section is enabled.
fn debug_log_with_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<DebugLogger>(scope, payload);
    let (section, enabled, first) = {
        let mut data = state.data_mut();
        let first = !data.called;
        data.called = true;
        (data.section.clone(), data.enabled, first)
    };
    if first {
        let debug = debug_function(scope, c"debug", &section, enabled)?;
        let callback = state.data().callback.get(scope);
        Function::call(scope, HandleValue::undefined(), callback, &[debug])?;
    }
    if enabled {
        print_debug(scope, &section, &args.rest(0))?;
    }
    Ok(js::value::undefined())
}

/// A function that does nothing, the logger of a disabled `debuglog` section.
fn do_nothing(
    _scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(js::value::undefined())
}

/// The style names `util.inspect.colors` holds, in its property order.
const STYLE_NAMES: &[&str] = &[
    "reset",
    "bold",
    "dim",
    "italic",
    "underline",
    "blink",
    "inverse",
    "hidden",
    "strikethrough",
    "doubleunderline",
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "bgBlack",
    "bgRed",
    "bgGreen",
    "bgYellow",
    "bgBlue",
    "bgMagenta",
    "bgCyan",
    "bgWhite",
    "framed",
    "overlined",
    "gray",
    "redBright",
    "greenBright",
    "yellowBright",
    "blueBright",
    "magentaBright",
    "cyanBright",
    "whiteBright",
    "bgGray",
    "bgRedBright",
    "bgGreenBright",
    "bgYellowBright",
    "bgBlueBright",
    "bgMagentaBright",
    "bgCyanBright",
    "bgWhiteBright",
    "grey",
    "blackBright",
    "bgGrey",
    "bgBlackBright",
    "faint",
    "crossedout",
    "strikeThrough",
    "crossedOut",
    "conceal",
    "swapColors",
    "swapcolors",
    "doubleUnderline",
];

/// The ANSI escape codes `styleText` applies for `name`: the opening and closing code.
fn style_codes(name: &str) -> Option<(u8, u8)> {
    Some(match name {
        "reset" => (0, 0),
        "bold" => (1, 22),
        "dim" | "faint" => (2, 22),
        "italic" => (3, 23),
        "underline" => (4, 24),
        "blink" => (5, 25),
        "inverse" | "swapColors" | "swapcolors" => (7, 27),
        "hidden" | "conceal" => (8, 28),
        "strikethrough" | "crossedout" | "strikeThrough" | "crossedOut" => (9, 29),
        "doubleunderline" | "doubleUnderline" => (21, 24),
        "framed" => (51, 54),
        "overlined" => (53, 55),
        "black" => (30, 39),
        "red" => (31, 39),
        "green" => (32, 39),
        "yellow" => (33, 39),
        "blue" => (34, 39),
        "magenta" => (35, 39),
        "cyan" => (36, 39),
        "white" => (37, 39),
        "gray" | "grey" | "blackBright" => (90, 39),
        "redBright" => (91, 39),
        "greenBright" => (92, 39),
        "yellowBright" => (93, 39),
        "blueBright" => (94, 39),
        "magentaBright" => (95, 39),
        "cyanBright" => (96, 39),
        "whiteBright" => (97, 39),
        "bgBlack" => (40, 49),
        "bgRed" => (41, 49),
        "bgGreen" => (42, 49),
        "bgYellow" => (43, 49),
        "bgBlue" => (44, 49),
        "bgMagenta" => (45, 49),
        "bgCyan" => (46, 49),
        "bgWhite" => (47, 49),
        "bgGray" | "bgGrey" | "bgBlackBright" => (100, 49),
        "bgRedBright" => (101, 49),
        "bgGreenBright" => (102, 49),
        "bgYellowBright" => (103, 49),
        "bgBlueBright" => (104, 49),
        "bgMagentaBright" => (105, 49),
        "bgCyanBright" => (106, 49),
        "bgWhiteBright" => (107, 49),
        _ => return None,
    })
}

/// The character at byte `index` of `text` and the byte index after it, if `index` is the start
/// of a character.
fn char_at(text: &str, index: usize) -> Option<(char, usize)> {
    let c = text.get(index..)?.chars().next()?;
    Some((c, index + c.len_utf8()))
}

/// The byte index after the escape sequence Node.js's `ansi` pattern matches at byte `start` of
/// `text`, if any: an OSC sequence (`ESC ]` up to BEL, `ESC \` or `0x9c`), or `ESC` or `0x9b`
/// followed by any of `[]()#;?`, optional numeric parameters and a final character.
fn ansi_sequence_at(text: &str, start: usize) -> Option<usize> {
    let (introducer, after) = char_at(text, start)?;
    if introducer == '\u{1b}' && text.as_bytes().get(after) == Some(&b']') {
        let mut index = after + 1;
        while let Some((c, next)) = char_at(text, index) {
            match c {
                '\u{7}' | '\u{9c}' => return Some(next),
                '\u{1b}' if text.as_bytes().get(next) == Some(&b'\\') => return Some(next + 1),
                _ => index = next,
            }
        }
    }
    if introducer != '\u{1b}' && introducer != '\u{9b}' {
        return None;
    }
    let is_final = |index: usize| -> Option<usize> {
        let (c, next) = char_at(text, index)?;
        let matches = c.is_ascii_digit()
            || ('A'..='P').contains(&c)
            || ('R'..='T').contains(&c)
            || c == 'Z'
            || c == 'c'
            || ('f'..='n').contains(&c)
            || ('q'..='u').contains(&c)
            || "y=><~".contains(c);
        matches.then_some(next)
    };
    // The prefix characters are ASCII, so every byte index up to `prefix_end` starts a character.
    let mut prefix_end = after;
    while text
        .as_bytes()
        .get(prefix_end)
        .is_some_and(|byte| b"[]()#;?".contains(byte))
    {
        prefix_end += 1;
    }
    for parameters in (after..=prefix_end).rev() {
        if let Some(end) = ansi_parameters(text.as_bytes(), parameters, &is_final) {
            return Some(end);
        }
        if let Some(end) = is_final(parameters) {
            return Some(end);
        }
    }
    None
}

/// The end of `\d{1,4}(?:[;:]\d{0,4})*` at byte `start` of `bytes` followed by a character
/// `is_final` matches, at the end a backtracking regular expression engine finds, or `None` if
/// `start` holds no digit. `is_final` returns the byte index after the character it matches, and
/// matches every digit.
fn ansi_parameters(
    bytes: &[u8],
    start: usize,
    is_final: &dyn Fn(usize) -> Option<usize>,
) -> Option<usize> {
    let digits = |from: usize| {
        let mut count = 0;
        while count < 4 && bytes.get(from + count).is_some_and(u8::is_ascii_digit) {
            count += 1;
        }
        count
    };
    let first = digits(start);
    if first == 0 {
        return None;
    }
    let mut at = start + first;
    let mut last_digit_end = at;
    while matches!(bytes.get(at), Some(b';' | b':')) {
        let count = digits(at + 1);
        at += 1 + count;
        if count > 0 {
            last_digit_end = at;
        }
    }
    // If the character after the longest repetition isn't final, the match ends after the last
    // digit, which backtracking matches as the final character.
    Some(is_final(at).unwrap_or(last_digit_end))
}

/// Call `part` with each part of `text` between the ANSI escape sequences that Node.js's
/// `stripVTControlCharacters` removes, in order.
pub(crate) fn for_each_visible_part<'t>(text: &'t str, mut part: impl FnMut(&'t str)) {
    let mut start = 0;
    let mut index = 0;
    while let Some(offset) = text[index..].find(['\u{1b}', '\u{9b}']) {
        let at = index + offset;
        match ansi_sequence_at(text, at) {
            Some(end) => {
                if start < at {
                    part(&text[start..at]);
                }
                start = end;
                index = end;
            }
            None => index = at + char_at(text, at).map_or(1, |(c, _)| c.len_utf8()),
        }
    }
    if start < text.len() {
        part(&text[start..]);
    }
}

/// `text` without ANSI escape sequences, as Node.js's `stripVTControlCharacters` removes them, or
/// `None` if allocating the result fails.
pub fn strip_vt_control_characters(text: &str) -> Option<String> {
    let mut out = String::new();
    // The result is never longer than `text`, so nothing below reallocates.
    out.try_reserve_exact(text.len()).ok()?;
    for_each_visible_part(text, |part| out.push_str(part));
    Some(out)
}

/// Node.js's `replaceCloseCode`: each `close` in `text` that doesn't end it replaced by `open`,
/// or by `close` and `open` if `keep_close`.
fn replace_close_code(text: &str, close: &str, open: &str, keep_close: bool) -> String {
    let replacement = if keep_close {
        format!("{close}{open}")
    } else {
        open.to_string()
    };
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    while let Some(offset) = text[last..].find(close) {
        let index = last + offset;
        let after = index + close.len();
        if after >= text.len() {
            break;
        }
        out.push_str(&text[last..index]);
        out.push_str(&replacement);
        last = after;
    }
    out.push_str(&text[last..]);
    out
}

/// The red, green and blue components of a `#RGB` or `#RRGGBB` color, if `text` is one.
fn hex_color(text: &str) -> Option<(u8, u8, u8)> {
    let digits = text.strip_prefix('#')?;
    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let component = |hex: &str| u8::from_str_radix(hex, 16).ok();
    match digits.len() {
        3 => {
            let double = |index: usize| component(&digits[index..=index].repeat(2));
            Some((double(0)?, double(1)?, double(2)?))
        }
        6 => Some((
            component(&digits[0..2])?,
            component(&digits[2..4])?,
            component(&digits[4..6])?,
        )),
        _ => None,
    }
}

/// Whether `value` is an object that `check` accepts.
fn object_is(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    check: impl FnOnce(Object<'_>) -> Result<bool, ExnThrown>,
) -> Result<bool, ExnThrown> {
    match Object::from_value(scope, *value) {
        Ok(object) => check(object),
        Err(_) => Ok(false),
    }
}

/// Whether `value` is a boxed primitive of `kind`, or of any kind for `None`.
fn is_boxed(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    kind: Option<inspect::Boxed>,
) -> Result<bool, ExnThrown> {
    object_is(scope, value, |object| {
        Ok(match inspect::boxed_kind(scope, object)? {
            Some(found) => kind.is_none_or(|kind| kind == found),
            None => false,
        })
    })
}

/// Whether `value` is a typed array of `kind`.
fn is_typed_array_of(scope: &Scope<'_>, value: HandleValue<'_>, kind: ViewKind) -> bool {
    js::ArrayBufferView::from_value(scope, value).is_some_and(|view| view.view_kind() == kind)
}

#[core_runtime::jsmodule(node_args, name = "node:util/types", default_namespace)]
pub mod types {
    use super::*;

    /// <https://nodejs.org/api/util.html#utiltypesisanyarraybuffervalue>
    #[function(length = 0)]
    pub fn is_any_array_buffer(
        scope: &Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| {
            Ok(inspect::is_array_buffer(object) || inspect::is_shared_array_buffer(scope, object)?)
        })
    }
    /// <https://nodejs.org/api/util.html#utiltypesisarraybuffervalue>
    #[function(length = 0)]
    pub fn is_array_buffer(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| Ok(inspect::is_array_buffer(object)))
    }
    /// <https://nodejs.org/api/util.html#utiltypesissharedarraybuffervalue>
    #[function(length = 0)]
    pub fn is_shared_array_buffer(
        scope: &Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| {
            inspect::is_shared_array_buffer(scope, object)
        })
    }
    /// <https://nodejs.org/api/util.html#utiltypesisarraybufferviewvalue>
    pub fn is_array_buffer_view(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        js::ArrayBufferView::from_value(scope, value).is_some()
    }
    /// <https://nodejs.org/api/util.html#utiltypesistypedarrayvalue>
    pub fn is_typed_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        js::ArrayBufferView::from_value(scope, value)
            .is_some_and(|view| view.view_kind().is_typed_array())
    }
    /// <https://nodejs.org/api/util.html#utiltypesisdataviewvalue>
    pub fn is_data_view(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        js::ArrayBufferView::from_value(scope, value)
            .is_some_and(|view| !view.view_kind().is_typed_array())
    }
    /// <https://nodejs.org/api/util.html#utiltypesisuint8arrayvalue>
    pub fn is_uint8_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, ViewKind::Uint8)
    }
    /// <https://nodejs.org/api/util.html#utiltypesisuint8clampedarrayvalue>
    pub fn is_uint8_clamped_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, ViewKind::Uint8Clamped)
    }
    /// <https://nodejs.org/api/util.html#utiltypesisuint16arrayvalue>
    pub fn is_uint16_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, ViewKind::Uint16)
    }
    /// <https://nodejs.org/api/util.html#utiltypesisuint32arrayvalue>
    pub fn is_uint32_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, ViewKind::Uint32)
    }
    /// <https://nodejs.org/api/util.html#utiltypesisint8arrayvalue>
    pub fn is_int8_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, ViewKind::Int8)
    }
    /// <https://nodejs.org/api/util.html#utiltypesisint16arrayvalue>
    pub fn is_int16_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, ViewKind::Int16)
    }
    /// <https://nodejs.org/api/util.html#utiltypesisint32arrayvalue>
    pub fn is_int32_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, ViewKind::Int32)
    }
    /// <https://nodejs.org/api/util.html#utiltypesisfloat16arrayvalue>
    pub fn is_float16_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, ViewKind::Float16)
    }
    /// <https://nodejs.org/api/util.html#utiltypesisfloat32arrayvalue>
    pub fn is_float32_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, ViewKind::Float32)
    }
    /// <https://nodejs.org/api/util.html#utiltypesisfloat64arrayvalue>
    pub fn is_float64_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, ViewKind::Float64)
    }
    /// <https://nodejs.org/api/util.html#utiltypesisbigint64arrayvalue>
    pub fn is_big_int64_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, ViewKind::BigInt64)
    }
    /// <https://nodejs.org/api/util.html#utiltypesisbiguint64arrayvalue>
    pub fn is_big_uint64_array(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        is_typed_array_of(scope, value, ViewKind::BigUint64)
    }
    /// <https://nodejs.org/api/util.html#utiltypesisargumentsobjectvalue>
    #[function(length = 0)]
    pub fn is_arguments_object(
        scope: &Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| inspect::is_arguments(scope, object))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisasyncfunctionvalue>
    #[function(length = 0)]
    pub fn is_async_function(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| {
            Ok(object.is_callable()
                && inspect::has_builtin_tag(
                    scope,
                    object,
                    &["AsyncFunction", "AsyncGeneratorFunction"],
                )?)
        })
    }
    /// <https://nodejs.org/api/util.html#utiltypesisgeneratorfunctionvalue>
    #[function(length = 0)]
    pub fn is_generator_function(
        scope: &Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| {
            Ok(object.is_callable()
                && inspect::has_builtin_tag(
                    scope,
                    object,
                    &["GeneratorFunction", "AsyncGeneratorFunction"],
                )?)
        })
    }
    /// <https://nodejs.org/api/util.html#utiltypesisgeneratorobjectvalue>
    #[function(length = 0)]
    pub fn is_generator_object(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        let Ok(object) = Object::from_value(scope, *value) else {
            return false;
        };
        // SAFETY: `object` is a live, rooted object, and a class's name is a static C string.
        let name = unsafe { CStr::from_ptr((*js::object::get_object_class(object.as_raw())).name) };
        matches!(name.to_bytes(), b"Generator" | b"AsyncGenerator")
    }
    /// <https://nodejs.org/api/util.html#utiltypesisdatevalue>
    #[function(length = 0)]
    pub fn is_date(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| inspect::is_date(scope, object))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisregexpvalue>
    #[function(length = 0)]
    pub fn is_reg_exp(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| inspect::is_regexp(scope, object))
    }
    /// <https://nodejs.org/api/util.html#utiltypesismapvalue>
    #[function(length = 0)]
    pub fn is_map(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| inspect::is_map(scope, object))
    }
    /// <https://nodejs.org/api/util.html#utiltypesissetvalue>
    #[function(length = 0)]
    pub fn is_set(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| inspect::is_set(scope, object))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisweakmapvalue>
    #[function(length = 0)]
    pub fn is_weak_map(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| inspect::is_weak_map(scope, object))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisweaksetvalue>
    #[function(length = 0)]
    pub fn is_weak_set(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| inspect::is_weak_set(scope, object))
    }
    /// <https://nodejs.org/api/util.html#utiltypesismapiteratorvalue>
    #[function(length = 0)]
    pub fn is_map_iterator(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| {
            Ok(inspect::es_class(scope, object) == ESClass::MapIterator)
        })
    }
    /// <https://nodejs.org/api/util.html#utiltypesissetiteratorvalue>
    #[function(length = 0)]
    pub fn is_set_iterator(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| {
            Ok(inspect::es_class(scope, object) == ESClass::SetIterator)
        })
    }
    /// <https://nodejs.org/api/util.html#utiltypesispromisevalue>
    #[function(length = 0)]
    pub fn is_promise(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        Object::from_value(scope, *value)
            .is_ok_and(|object| js::Promise::is_promise(object.handle()))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisnativeerrorvalue>
    #[function(length = 0)]
    pub fn is_native_error(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        value.is_object() && js::exception::is_error_object(scope, value)
    }
    /// <https://nodejs.org/api/util.html#utiltypesisnumberobjectvalue>
    #[function(length = 0)]
    pub fn is_number_object(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        is_boxed(scope, value, Some(inspect::Boxed::Number))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisstringobjectvalue>
    #[function(length = 0)]
    pub fn is_string_object(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        is_boxed(scope, value, Some(inspect::Boxed::String))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisbooleanobjectvalue>
    #[function(length = 0)]
    pub fn is_boolean_object(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        is_boxed(scope, value, Some(inspect::Boxed::Boolean))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisbigintobjectvalue>
    #[function(length = 0)]
    pub fn is_big_int_object(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        is_boxed(scope, value, Some(inspect::Boxed::BigInt))
    }
    /// <https://nodejs.org/api/util.html#utiltypesissymbolobjectvalue>
    #[function(length = 0)]
    pub fn is_symbol_object(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        is_boxed(scope, value, Some(inspect::Boxed::Symbol))
    }
    /// <https://nodejs.org/api/util.html#utiltypesisboxedprimitivevalue>
    #[function(length = 0)]
    pub fn is_boxed_primitive(
        scope: &Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        is_boxed(scope, value, None)
    }
    /// <https://nodejs.org/api/util.html#utiltypesismodulenamespaceobjectvalue>
    #[function(length = 0)]
    pub fn is_module_namespace_object(
        scope: &Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        object_is(scope, value, |object| {
            Ok(object.get_prototype(scope)?.is_none()
                && !object.is_extensible(scope)?
                && inspect::has_builtin_tag(scope, object, &["Module"])?)
        })
    }
    /// <https://nodejs.org/api/util.html#utiltypesisproxyvalue>
    #[function(length = 0)]
    pub fn is_proxy(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        Object::from_value(scope, *value).is_ok_and(|object| object.is_proxy())
    }
    /// <https://nodejs.org/api/util.html#utiltypesisexternalvalue>: there are no external values,
    /// so always `false`.
    pub fn is_external() -> bool {
        false
    }
    /// <https://nodejs.org/api/util.html#utiltypesiskeyobjectvalue>
    pub fn is_key_object(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
        crate::crypto::key_object(scope, value).is_some()
    }
    /// <https://nodejs.org/api/util.html#utiltypesiscryptokeyvalue>: `false`, as Web Crypto keys
    /// aren't implemented.
    #[function(length = 1)]
    pub fn is_crypto_key() -> bool {
        false
    }
}

#[core_runtime::jsmodule(node_args, name = "node:util", default_namespace)]
pub mod module {
    use super::*;

    /// <https://nodejs.org/api/util.html#utilformatformat-args>: a single string argument is
    /// returned as it is.
    pub fn format<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        if let [only] = &args[..] {
            if only.is_string() {
                return Ok(scope.root_value(only.get()));
            }
        }
        inspect::format_current(scope, &args)?.to_jsval_throwing(scope)
    }

    /// <https://nodejs.org/api/util.html#utilformatwithoptionsinspectoptions-format-args>
    pub fn format_with_options(
        scope: &Scope<'_>,
        options: HandleValue<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<String, Thrown> {
        let options = validate_plain_object(scope, options, "inspectOptions", true)?;
        let options = InspectOptions::current(scope)?.read(scope, options)?;
        Ok(inspect::format(scope, &args, &options)?)
    }

    /// <https://nodejs.org/api/util.html#utilisdeepstrictequalval1-val2-options>: a truthy
    /// `skipPrototype` leaves prototypes uncompared.
    #[function(length = 3)]
    pub fn is_deep_strict_equal(
        scope: &Scope<'_>,
        a: HandleValue<'_>,
        b: HandleValue<'_>,
        skip_prototype: Option<HandleValue<'_>>,
    ) -> Result<bool, ExnThrown> {
        let skip =
            skip_prototype.is_some_and(|skip| bool::from_jsval(scope, skip, ()).unwrap_or(false));
        let mode = if skip {
            inspect::Mode::StrictWithoutPrototypes
        } else {
            inspect::Mode::Strict
        };
        inspect::deep_equal(scope, a, b, mode)
    }

    /// <https://nodejs.org/api/util.html#utilinheritsconstructor-superconstructor>
    pub fn inherits(
        scope: &Scope<'_>,
        ctor: HandleValue<'_>,
        super_ctor: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let ctor = validate_function(scope, ctor, "ctor")?;
        let super_ctor = validate_function(scope, super_ctor, "superCtor")?;
        let super_proto = super_ctor.get_property(scope, c"prototype")?;
        let super_proto = Object::from_value(scope, super_proto).map_err(|_| {
            NodeError::invalid_arg_type(scope, "superCtor.prototype", "of type object", super_proto)
        })?;
        ctor.define_property(scope, c"super_", super_ctor, 0)?;
        let proto = Object::from_value(scope, ctor.get_property(scope, c"prototype")?)?;
        proto.set_prototype(scope, super_proto.handle())?;
        Ok(())
    }

    /// <https://nodejs.org/api/util.html#utilcallbackifyoriginal>
    pub fn callbackify<'s>(
        scope: &'s Scope<'_>,
        original: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        let original = validate_function(scope, original, "original")?;
        let function = Function::new_callback(scope, c"callbackified", 0, callbackified, original)?;
        copy_descriptors(scope, *function, original, |scope, name, descriptor| {
            let value = descriptor.get_property(scope, c"value")?;
            if name == "length" && value.is_number() {
                descriptor.set_property(scope, c"value", value.to_number() + 1.0)?;
            } else if name == "name" && value.is_string() {
                let name = String::from_jsval(scope, value, ()).map_err(|_| ExnThrown)?;
                descriptor.set_property(
                    scope,
                    c"value",
                    format!("{name}Callbackified").as_str(),
                )?;
            }
            Ok(())
        })?;
        Ok(*function)
    }

    /// <https://nodejs.org/api/util.html#utilgetcallsitesframecount-options>: the `frameCount`
    /// innermost frames of the calling code, 10 by default. An object or `null` as the only
    /// argument is `options`. `scriptId` is always `"0"`, and `options.sourceMap` is validated but
    /// not applied.
    #[function(length = 0)]
    pub fn get_call_sites<'s>(
        scope: &'s Scope<'_>,
        frame_count: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<js::Array<'s>, Thrown> {
        let (frame_count, options) = match (frame_count, options) {
            (Some(value), None) if value.is_null() || inspect::is_typeof_object(scope, value) => {
                (None, Some(value))
            }
            pair => pair,
        };
        if let Some(options) = options {
            let options = validate_plain_object(scope, options, "options", false)?;
            let source_map = options.get_property(scope, c"sourceMap")?;
            if !source_map.is_undefined() {
                validate_boolean(scope, source_map, "options.sourceMap")?;
            }
        }
        let count = match frame_count {
            Some(count) => validate_integer(scope, count, "frameCount", 1.0, 200.0)? as usize,
            None => 10,
        };
        let error_class = crate::stack_trace::error_constructor(scope)?;
        let error = Function::construct(scope, error_class, &[] as &[HandleValue<'_>])?;
        let stack = crate::stack_trace::without_hook(|| error.get_property(scope, c"stack"))?;
        let stack = String::from_jsval(scope, stack, ()).unwrap_or_default();
        let mut sites = Vec::new();
        for line in stack.lines().filter(|line| !line.is_empty()).take(count) {
            let (name, location) = line.split_once('@').unwrap_or(("", line));
            let mut parts = location.rsplitn(3, ':');
            let column = parts
                .next()
                .and_then(|column| column.parse::<f64>().ok())
                .unwrap_or(0.0);
            let line_number = parts
                .next()
                .and_then(|line| line.parse::<f64>().ok())
                .unwrap_or(0.0);
            let script = parts.next().unwrap_or_default();
            let site = Object::new_plain(scope)?;
            site.set_property(scope, c"functionName", name)?;
            site.set_property(scope, c"scriptId", "0")?;
            site.set_property(scope, c"scriptName", script)?;
            site.set_property(scope, c"lineNumber", line_number)?;
            site.set_property(scope, c"columnNumber", column)?;
            site.set_property(scope, c"column", column)?;
            sites.push(site.to_jsval_throwing(scope)?);
        }
        Ok(js::Array::with_contents(scope, &sites)?)
    }

    /// <https://nodejs.org/api/util.html#utildeprecatefn-msg-code-options>: the wrapper has `fn`'s
    /// `length`, `fn` as its prototype, and `fn.prototype` as its `prototype` if that is truthy.
    /// With the option `modifyPrototype: false`, it has none of these. A wrapper without
    /// `fn.prototype` has a `prototype` object of its own.
    #[function(length = 3)]
    pub fn deprecate<'s>(
        scope: &'s Scope<'_>,
        function: HandleValue<'_>,
        message: Option<HandleValue<'_>>,
        code: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let function = validate_function(scope, function, "fn")?;
        let modify_prototype =
            match options.and_then(|options| Object::from_value(scope, options).ok()) {
                Some(options) => {
                    let value = options.get_property(scope, c"modifyPrototype")?;
                    value.is_undefined() || bool::from_jsval(scope, value, ()).unwrap_or(false)
                }
                None => true,
            };
        let code = match code.filter(|code| !code.is_undefined()) {
            Some(code) => Some(validate_string(scope, code, "code")?),
            None => None,
        };
        let state = js::class::create_instance_with::<DeprecatedImpl>(scope, |_| DeprecatedImpl {
            function: Heap::from(function.to_jsval_raw_throwing(scope).unwrap_or_default()),
            message: Heap::from(message.map_or(js::value::undefined(), |message| message.get())),
            code,
            warned: false,
        })?;
        let wrapper =
            Function::new_constructor_callback(scope, c"deprecated", 0, deprecated, state)?;
        let prototype = Object::new_plain(scope)?;
        prototype.define_property(scope, c"constructor", *wrapper, 0)?;
        let attrs = js::class_spec::JSPROP_PERMANENT as u32;
        wrapper.define_property(scope, c"prototype", prototype, attrs)?;
        if !modify_prototype {
            return Ok(*wrapper);
        }
        wrapper.set_prototype(scope, function.handle())?;
        let prototype = function.get_property(scope, c"prototype")?;
        if bool::from_jsval(scope, prototype, ()).unwrap_or(false) {
            wrapper.set_property(scope, c"prototype", prototype)?;
        }
        let length = function.get_property(scope, c"length")?;
        wrapper.define_property(
            scope,
            c"length",
            length,
            js::class_spec::JSPROP_READONLY as u32,
        )?;
        Ok(*wrapper)
    }

    /// <https://nodejs.org/api/util.html#utildebuglogsection-callback>: a logger that prints to
    /// stderr if `NODE_DEBUG` enables `section`, and does nothing otherwise. Its `enabled`
    /// property is `true` in the first case. With a `callback` function, the logger's first call
    /// calls it with a function that logs for the section, also when `enabled` was read before,
    /// which makes Node.js skip the call.
    #[function(length = 2)]
    pub fn debuglog<'s>(
        scope: &'s Scope<'_>,
        section: String,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let enabled = debug_enabled(&section);
        let Some(callback) = callback.filter(|callback| {
            Object::from_value(scope, **callback).is_ok_and(|callback| callback.is_callable())
        }) else {
            return debug_function(scope, c"logger", &section, enabled);
        };
        let state =
            js::class::create_instance_with::<DebugLoggerImpl>(scope, |_| DebugLoggerImpl {
                callback: Heap::from(callback.get()),
                section,
                enabled,
                called: false,
            })?;
        let logger = Function::new_callback(scope, c"logger", 0, debug_log_with_callback, state)?;
        logger.set_property(scope, c"enabled", enabled)?;
        Ok(*logger)
    }

    /// <https://nodejs.org/api/util.html#utildebugsection>: `debuglog`.
    #[function(length = 2)]
    pub fn debug<'s>(
        scope: &'s Scope<'_>,
        section: String,
        callback: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        debuglog(scope, section, callback)
    }

    /// <https://nodejs.org/api/util.html#utilstripvtcontrolcharactersstr>
    #[function(name = "stripVTControlCharacters")]
    pub fn strip_control_characters(
        scope: &Scope<'_>,
        text: HandleValue<'_>,
    ) -> Result<String, Thrown> {
        strip_vt_control_characters(&validate_string(scope, text, "str")?).ok_or_else(|| {
            js::exception::report_out_of_memory(scope);
            Thrown::Pending
        })
    }

    /// <https://nodejs.org/api/util.html#utilstyletextformat-text-options>: always styled,
    /// whether or not the output is a terminal, so `options.stream` is not consulted. `format` is
    /// a style name, `'none'`, a `#RGB` or `#RRGGBB` color, or an array of these.
    #[function(length = 3)]
    pub fn style_text(
        scope: &Scope<'_>,
        format: HandleValue<'_>,
        text: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<String, Thrown> {
        let text = validate_string(scope, text, "text")?;
        if let Some(options) = options.filter(|options| !options.is_undefined()) {
            let options = validate_plain_object(scope, options, "options", false)?;
            let validate_stream = options.get_property(scope, c"validateStream")?;
            if !validate_stream.is_undefined() && !validate_stream.is_boolean() {
                return Err(NodeError::invalid_arg_type(
                    scope,
                    "options.validateStream",
                    "of type boolean",
                    validate_stream,
                )
                .into());
            }
        }
        let mut keys = Vec::new();
        match Object::from_value(scope, *format) {
            Ok(array) if js::Array::is_array(scope, array.handle())? => {
                let length = array
                    .cast::<js::Array>()
                    .map_err(|_| Thrown::Pending)?
                    .length(scope)?;
                for index in 0..length {
                    keys.push(array.get_element(scope, index)?);
                }
            }
            _ => keys.push(format),
        }
        let mut open_codes = String::new();
        let mut close_codes = String::new();
        let mut processed = text;
        for key in keys {
            let name = if key.is_string() {
                Some(String::from_jsval(scope, key, ())?)
            } else {
                None
            };
            if name.as_deref() == Some("none") {
                continue;
            }
            if let Some(color) = name.as_deref().filter(|name| name.starts_with('#')) {
                let (r, g, b) = hex_color(color).ok_or_else(|| {
                    NodeError::invalid_arg_value(
                        scope,
                        "format",
                        key,
                        "must be a valid hex color (#RGB or #RRGGBB)",
                    )
                })?;
                let open = format!("\u{1b}[38;2;{r};{g};{b}m");
                let close = "\u{1b}[39m";
                open_codes.push_str(&open);
                close_codes = format!("{close}{close_codes}");
                processed = replace_close_code(&processed, close, &open, false);
                continue;
            }
            let Some((open, close)) = name.as_deref().and_then(style_codes) else {
                let allowed: Vec<String> =
                    STYLE_NAMES.iter().map(|name| format!("'{name}'")).collect();
                return Err(NodeError::invalid_arg_value(
                    scope,
                    "format",
                    key,
                    &format!("must be one of: {}", allowed.join(", ")),
                )
                .into());
            };
            let open_seq = format!("\u{1b}[{open}m");
            let close_seq = format!("\u{1b}[{close}m");
            open_codes.push_str(&open_seq);
            close_codes = format!("{close_seq}{close_codes}");
            processed =
                replace_close_code(&processed, &close_seq, &open_seq, open == 1 || open == 2);
        }
        Ok(format!("{open_codes}{processed}{close_codes}"))
    }

    /// <https://nodejs.org/api/util.html#utiltousvstringstring>: lone surrogates become U+FFFD
    /// in the conversion to a Rust string.
    #[function(name = "toUSVString")]
    pub fn to_usv_string(text: String) -> String {
        text
    }

    /// <https://nodejs.org/api/util.html#utilgetsystemerrornameerr>
    pub fn get_system_error_name(
        scope: &Scope<'_>,
        err: HandleValue<'_>,
    ) -> Result<String, Thrown> {
        if !err.is_number() {
            return Err(NodeError::invalid_arg_type(scope, "err", "of type number", err).into());
        }
        let errno = err.to_number();
        if errno >= 0.0 || errno.fract() != 0.0 || errno.abs() > crate::errors::MAX_SAFE_INTEGER {
            return Err(NodeError::out_of_range(
                "err",
                "a negative integer",
                &out_of_range_received(errno),
            )
            .into());
        }
        let name = crate::os::errno_table()
            .iter()
            .find(|(_, value)| f64::from(*value) == -errno);
        if let Some((name, _)) = name {
            return Ok(name.to_string_lossy().into_owned());
        }
        Ok(format!(
            "Unknown system error {}",
            crate::errors::number_to_string(errno)
        ))
    }

    /// <https://nodejs.org/api/util.html#utilisarrayobject>: `Array.isArray`, which emits DEP0044
    /// once.
    pub fn is_array(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
        crate::process::deprecate_once(
            scope,
            "The `util.isArray` API is deprecated. Please use `Array.isArray()` instead.",
            "DEP0044",
        )?;
        match Object::from_value(scope, *value) {
            Ok(object) => js::Array::is_array_or_array_proxy(scope, object.handle()),
            Err(_) => Ok(false),
        }
    }

    /// <https://nodejs.org/api/util.html#utilparseargsconfig>
    pub fn parse_args<'s>(
        scope: &'s Scope<'_>,
        config: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        crate::parse_args::parse_args(scope, config)
    }

    /// `util._extend(target, source)`: copy `source`'s own enumerable properties onto `target`.
    /// Emits DEP0060 once.
    #[function(name = "_extend")]
    pub fn extend<'s>(
        scope: &'s Scope<'_>,
        target: HandleValue<'_>,
        source: HandleValue<'_>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        crate::process::deprecate_once(
            scope,
            "The `util._extend` API is deprecated. Please use Object.assign() instead.",
            "DEP0060",
        )?;
        let target_value = scope.root_value(target.get());
        if let (Ok(target), Ok(source)) = (
            Object::from_value(scope, *target),
            Object::from_value(scope, *source),
        ) {
            target.assign(scope, source)?;
        }
        Ok(target_value)
    }
}

/// `util.inspect.colors`: the ANSI codes that turn each style on and off.
const COLORS: &[(&CStr, f64, f64)] = &[
    (c"reset", 0.0, 0.0),
    (c"bold", 1.0, 22.0),
    (c"dim", 2.0, 22.0),
    (c"italic", 3.0, 23.0),
    (c"underline", 4.0, 24.0),
    (c"blink", 5.0, 25.0),
    (c"inverse", 7.0, 27.0),
    (c"hidden", 8.0, 28.0),
    (c"strikethrough", 9.0, 29.0),
    (c"doubleunderline", 21.0, 24.0),
    (c"black", 30.0, 39.0),
    (c"red", 31.0, 39.0),
    (c"green", 32.0, 39.0),
    (c"yellow", 33.0, 39.0),
    (c"blue", 34.0, 39.0),
    (c"magenta", 35.0, 39.0),
    (c"cyan", 36.0, 39.0),
    (c"white", 37.0, 39.0),
    (c"bgBlack", 40.0, 49.0),
    (c"bgRed", 41.0, 49.0),
    (c"bgGreen", 42.0, 49.0),
    (c"bgYellow", 43.0, 49.0),
    (c"bgBlue", 44.0, 49.0),
    (c"bgMagenta", 45.0, 49.0),
    (c"bgCyan", 46.0, 49.0),
    (c"bgWhite", 47.0, 49.0),
    (c"framed", 51.0, 54.0),
    (c"overlined", 53.0, 55.0),
    (c"gray", 90.0, 39.0),
    (c"redBright", 91.0, 39.0),
    (c"greenBright", 92.0, 39.0),
    (c"yellowBright", 93.0, 39.0),
    (c"blueBright", 94.0, 39.0),
    (c"magentaBright", 95.0, 39.0),
    (c"cyanBright", 96.0, 39.0),
    (c"whiteBright", 97.0, 39.0),
    (c"bgGray", 100.0, 49.0),
    (c"bgRedBright", 101.0, 49.0),
    (c"bgGreenBright", 102.0, 49.0),
    (c"bgYellowBright", 103.0, 49.0),
    (c"bgBlueBright", 104.0, 49.0),
    (c"bgMagentaBright", 105.0, 49.0),
    (c"bgCyanBright", 106.0, 49.0),
    (c"bgWhiteBright", 107.0, 49.0),
];

/// The non-enumerable aliases of `util.inspect.colors` entries: the alias, then the entry.
const COLOR_ALIASES: &[(&CStr, &CStr)] = &[
    (c"grey", c"gray"),
    (c"blackBright", c"gray"),
    (c"bgGrey", c"bgGray"),
    (c"bgBlackBright", c"bgGray"),
    (c"faint", c"dim"),
    (c"crossedout", c"strikethrough"),
    (c"strikeThrough", c"strikethrough"),
    (c"crossedOut", c"strikethrough"),
    (c"conceal", c"hidden"),
    (c"swapColors", c"inverse"),
    (c"swapcolors", c"inverse"),
    (c"doubleUnderline", c"doubleunderline"),
];

/// `util.inspect.styles`: the color of each kind of value.
const STYLES: &[(&CStr, &str)] = &[
    (c"special", "cyan"),
    (c"number", "yellow"),
    (c"bigint", "yellow"),
    (c"boolean", "yellow"),
    (c"undefined", "grey"),
    (c"null", "bold"),
    (c"string", "green"),
    (c"symbol", "green"),
    (c"date", "magenta"),
    (c"regexp", "red"),
    (c"module", "underline"),
];

/// Set `inspect.colors` and `inspect.styles`, objects with null prototypes.
fn define_color_tables(scope: &Scope<'_>, inspect: Object<'_>) -> Result<(), ExnThrown> {
    let colors = Object::new_plain(scope)?;
    colors.set_prototype(scope, js::native::HandleObject::null())?;
    for &(name, on, off) in COLORS {
        colors.set_property(scope, name, js::Array::with_contents(scope, &[on, off])?)?;
    }
    for &(alias, name) in COLOR_ALIASES {
        let codes = colors.get_property(scope, name)?;
        colors.define_property(scope, alias, codes, 0)?;
    }
    inspect.set_property(scope, c"colors", colors)?;
    let styles = Object::new_plain(scope)?;
    styles.set_prototype(scope, js::native::HandleObject::null())?;
    for &(name, color) in STYLES {
        styles.set_property(scope, name, color)?;
    }
    inspect.set_property(scope, c"styles", styles)
}

/// Register the classes of `node:util`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) {
    Deprecated::add_to_global(scope, global);
    DebugLogger::add_to_global(scope, global);
    Promisified::add_to_global(scope, global);
    PromisifySettle::add_to_global(scope, global);
    CallbackifyCall::add_to_global(scope, global);
}

/// Register `node:util`. `inspect` and `promisify` have the `custom` symbols as properties,
/// `inspect` also the `defaultOptions` accessor and the `colors` and `styles` tables, and `types`, `TextEncoder` and `TextDecoder` are
/// `node:util/types`' `default` export and the global's classes.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let overrides = || -> Result<Vec<(&'static str, HandleValue<'_>)>, ExnThrown> {
        let inspect = Function::new_callback(scope, c"inspect", 2, inspect_callback, ())?;
        inspect.set_property(scope, c"custom", inspect::INSPECT_CUSTOM.value(scope)?)?;
        js::class::register_realm_object::<inspect::InspectFunction>(scope, *inspect);
        let defaults = default_options_object(scope)?;
        js::class::register_realm_object::<inspect::DefaultOptions>(scope, defaults);
        let getter = Function::new_callback(scope, c"get", 0, default_options_get, defaults)?;
        let setter = Function::new_callback(scope, c"set", 1, default_options_set, defaults)?;
        inspect.define_accessor(scope, c"defaultOptions", Some(*getter), Some(*setter), 0)?;
        define_color_tables(scope, *inspect)?;
        let promisify = Function::new_callback(scope, c"promisify", 1, promisify_callback, ())?;
        promisify.set_property(scope, c"custom", PROMISIFY_CUSTOM.value(scope)?)?;
        let types = core_runtime::module::builtin_namespace(scope, "node:util/types")?
            .ok_or(ExnThrown)?
            .get_property(scope, c"default")?;
        Ok(vec![
            ("inspect", (*inspect).to_jsval_throwing(scope)?),
            ("promisify", (*promisify).to_jsval_throwing(scope)?),
            ("types", types),
            (
                "TextEncoder",
                web_globals::encoding::text_encoder_constructor(scope)?.to_jsval_throwing(scope)?,
            ),
            (
                "TextDecoder",
                web_globals::encoding::text_decoder_constructor(scope)?.to_jsval_throwing(scope)?,
            ),
        ])
    };
    let Ok(overrides) = overrides() else {
        return false;
    };
    crate::stub::register_implemented_with::<module::js_module>(scope, name, &overrides)
}
