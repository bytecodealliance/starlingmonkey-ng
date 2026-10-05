// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! V8's stack trace API: <https://v8.dev/docs/stack-trace-api>.
//!
//! While `Error.prepareStackTrace` is a function, the `stack` of an error and of an object
//! `Error.captureStackTrace` was called on is the value that function returns for the error and
//! an array of `CallSite` objects, one per frame. The frames come from SpiderMonkey's stack
//! string. The result replaces the `stack` property, so the function runs once per object.
//! `stack` reads during the call get SpiderMonkey's string.

use std::cell::Cell;

use crate::support::Props;
use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

js::instance_local! {
    /// Set while `Error.prepareStackTrace` runs.
    static PREPARING: Cell<bool> = const { Cell::new(false) };
}

/// One frame of a stack trace.
#[derive(Default)]
struct Frame {
    /// The function's name, or `None` for top-level code and anonymous functions.
    function: Option<String>,
    /// The script's file name or URL, or `None` for code that `eval` or `Function` compiled.
    file: Option<String>,
    line: u32,
    column: u32,
    is_async: bool,
    /// Where the `eval` or `Function` call that compiled the code is, as SpiderMonkey describes
    /// it, for code that one compiled.
    eval_origin: Option<String>,
}

/// `text`, a line of SpiderMonkey's stack string, split into the function name and the location
/// at the first `@` that a location starts after: an absolute path, a URL, or a name in `[` or `<`.
/// Without one, the location is a relative path, and the split is at the last `@`. A function
/// name can hold `@`, as a name inferred from the property key `'a@b'` does, and an absolute path
/// can too, as a scoped package's does.
pub(crate) fn split_frame(text: &str) -> Option<(&str, &str)> {
    let starts_location = |rest: &str| {
        rest.starts_with(['/', '[', '<'])
            || rest.split_once(':').is_some_and(|(scheme, _)| {
                scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                    && scheme
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
            })
    };
    let at = text
        .match_indices('@')
        .map(|(index, _)| index)
        .find(|&index| starts_location(&text[index + 1..]))
        .or_else(|| text.rfind('@'))?;
    Some((&text[..at], &text[at + 1..]))
}

impl Frame {
    /// The frame a line of SpiderMonkey's stack string describes: `[async*]name@file:line:column`,
    /// where `file` is `file line N > eval` for code `eval` compiled.
    fn parse(text: &str) -> Option<Frame> {
        let (name, location) = split_frame(text)?;
        let (name, is_async) = match name.strip_prefix("async*") {
            Some(name) => (name, true),
            None => (name, false),
        };
        let (rest, column) = location.rsplit_once(':')?;
        let (file, line) = rest.rsplit_once(':')?;
        // SpiderMonkey names an anonymous function after the function containing it, followed
        // by `/<`.
        let function = (!name.is_empty() && !name.ends_with('<'))
            .then(|| name.rsplit('/').next().unwrap_or(name).to_string());
        let (file, eval_origin) = match file.find(" line ") {
            Some(_) if file.ends_with("> eval") || file.ends_with("> Function") => {
                (None, Some(file.to_string()))
            }
            _ => (Some(file.to_string()), None),
        };
        Some(Frame {
            function,
            file,
            line: line.parse().ok()?,
            column: column.parse().ok()?,
            is_async,
            eval_origin,
        })
    }

    /// V8's text for the frame: `[async ]name (location)`, or `[async ]location` without a name.
    fn describe(&self) -> String {
        let file = self
            .file
            .as_deref()
            .or(self.eval_origin.as_deref())
            .unwrap_or("<anonymous>");
        let location = format!("{file}:{}:{}", self.line, self.column);
        let prefix = if self.is_async { "async " } else { "" };
        match &self.function {
            Some(function) => format!("{prefix}{function} ({location})"),
            None => format!("{prefix}{location}"),
        }
    }
}

/// A frame of a stack trace, as `Error.prepareStackTrace` receives it.
#[core_runtime::jsclass(hidden)]
pub struct CallSite {
    #[no_trace]
    frame: Frame,
}

#[core_runtime::jsmethods]
impl CallSite {
    /// Throws, as V8's does.
    #[constructor]
    fn new(scope: &Scope<'_>) -> Result<Self, ExnThrown> {
        Err(js::error::throw_plain_error(scope, c"Not supported"))
    }

    /// The frame's `this`, which isn't recorded.
    #[method]
    fn get_this(&self) {}

    /// The type of the frame's `this`, which isn't recorded.
    #[method]
    fn get_type_name(&self) -> Option<String> {
        None
    }

    /// The frame's function, which isn't recorded.
    #[method]
    fn get_function(&self) {}

    #[method]
    fn get_function_name(&self) -> Option<String> {
        self.data().frame.function.clone()
    }

    /// The name of the property the frame's function was called through, which isn't recorded.
    #[method]
    fn get_method_name(&self) -> Option<String> {
        None
    }

    #[method]
    fn get_file_name(&self) -> Option<String> {
        self.data().frame.file.clone()
    }

    #[method]
    fn get_script_name_or_source_url(&self) -> Option<String> {
        self.data().frame.file.clone()
    }

    #[method]
    fn get_script_hash(&self) -> String {
        String::new()
    }

    #[method]
    fn get_line_number(&self) -> f64 {
        f64::from(self.data().frame.line)
    }

    #[method]
    fn get_column_number(&self) -> f64 {
        f64::from(self.data().frame.column)
    }

    #[method]
    fn get_enclosing_line_number(&self) -> f64 {
        f64::from(self.data().frame.line)
    }

    #[method]
    fn get_enclosing_column_number(&self) -> f64 {
        f64::from(self.data().frame.column)
    }

    /// The source position, which isn't recorded.
    #[method]
    fn get_position(&self) -> f64 {
        0.0
    }

    #[method]
    fn get_eval_origin(&self) -> Option<String> {
        self.data().frame.eval_origin.clone()
    }

    #[method]
    fn get_promise_index(&self) -> Option<f64> {
        None
    }

    /// Whether the frame's `this` is the global object or `undefined`, which isn't recorded. This
    /// reports it for frames whose function's name isn't a property path such as `obj.method`.
    #[method]
    fn is_toplevel(&self) -> bool {
        !self
            .data()
            .frame
            .function
            .as_deref()
            .unwrap_or("")
            .contains('.')
    }

    #[method]
    fn is_eval(&self) -> bool {
        self.data().frame.eval_origin.is_some()
    }

    #[method]
    fn is_native(&self) -> bool {
        false
    }

    #[method]
    fn is_constructor(&self) -> bool {
        false
    }

    #[method]
    fn is_async(&self) -> bool {
        self.data().frame.is_async
    }

    #[method]
    fn is_promise_all(&self) -> bool {
        false
    }

    #[method(name = "toString")]
    fn describe(&self) -> String {
        self.data().frame.describe()
    }
}

/// Run `f` with `Error.prepareStackTrace` ignored, so the `stack` reads it does get SpiderMonkey's
/// strings.
pub(crate) fn without_hook<T>(f: impl FnOnce() -> T) -> T {
    let previous = PREPARING.replace(true);
    let result = f();
    PREPARING.set(previous);
    result
}

/// `Error.prepareStackTrace`, if it is a function and isn't running.
fn prepare_function<'s>(
    scope: &'s Scope<'_>,
    error_ctor: Object<'s>,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    if PREPARING.get() {
        return Ok(None);
    }
    let prepare = error_ctor.val(scope, c"prepareStackTrace")?;
    Ok(js::function::is_callable(prepare).then_some(prepare))
}

/// The result of `prepare` for `error` and the frames of SpiderMonkey's stack string `stack`.
fn prepare_stack<'s>(
    scope: &'s Scope<'_>,
    error_ctor: Object<'s>,
    prepare: HandleValue<'s>,
    error: Object<'s>,
    stack: &str,
) -> Result<HandleValue<'s>, ExnThrown> {
    let sites = js::Array::new(scope, 0)?;
    for (index, frame) in stack.lines().filter_map(Frame::parse).enumerate() {
        let site =
            js::class::create_instance_with::<CallSiteImpl>(scope, |_| CallSiteImpl { frame })?;
        sites.set_element(scope, index as u32, site)?;
    }
    PREPARING.set(true);
    let result = Function::call(
        scope,
        error_ctor,
        prepare,
        &[
            error.to_jsval_throwing(scope)?,
            sites.to_jsval_throwing(scope)?,
        ],
    );
    PREPARING.set(false);
    result
}

/// Replace `object`'s `stack` with `value`, as a writable, configurable, non-enumerable data
/// property.
fn define_stack(
    scope: &Scope<'_>,
    object: Object<'_>,
    value: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    object.define_property(scope, c"stack", value, 0)
}

/// The realm's `Error` constructor.
pub(crate) fn error_constructor<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let error = js::class::get_class_object(scope, js::class_spec::JSProtoKey::JSProto_Error)?;
    Object::from_handle(error).ok_or(ExnThrown)
}

/// `Error.captureStackTrace(object[, constructorOpt])`, whose payload is `original`: call
/// SpiderMonkey's `original`, then replace the `stack` it defined with `Error.prepareStackTrace`'s
/// result if that is a function.
fn capture_stack_trace(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    original: HandleValue,
) -> Result<Value, ExnThrown> {
    let args = crate::callback_args(&args);
    Function::call(scope, js::value::undefined(), original, &args)?;
    let error_ctor = error_constructor(scope)?;
    let (Some(prepare), Some(object)) = (
        prepare_function(scope, error_ctor)?,
        args.first()
            .and_then(|target| Object::from_value(scope, *target).ok()),
    ) else {
        return Ok(js::value::undefined());
    };
    let stack = object.val(scope, c"stack")?;
    if !stack.is_string() {
        return Ok(js::value::undefined());
    }
    let stack = String::from_jsval(scope, stack, ()).map_err(|e| e.throw(scope))?;
    let result = prepare_stack(scope, error_ctor, prepare, object, &stack)?;
    define_stack(scope, object, result)?;
    Ok(js::value::undefined())
}

/// The `Error.prototype.stack` getter, whose payload is `original`: SpiderMonkey's `original`
/// getter's string, or, if `Error.prepareStackTrace` is a function, its result, which then
/// replaces the error's `stack`.
fn stack_getter(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    original: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    let stack = Function::call(scope, this, original, js::function::EmptyArgs)?;
    let error_ctor = error_constructor(scope)?;
    let (Some(prepare), Some(error), true) = (
        prepare_function(scope, error_ctor)?,
        Object::from_value(scope, this).ok(),
        stack.is_string(),
    ) else {
        return Ok(stack.get());
    };
    let text = String::from_jsval(scope, stack, ()).map_err(|e| e.throw(scope))?;
    let result = prepare_stack(scope, error_ctor, prepare, error, &text)?;
    define_stack(scope, error, result)?;
    Ok(result.get())
}

/// Wrap `Error.captureStackTrace` and the `Error.prototype.stack` getter of `global`'s realm so
/// they call `Error.prepareStackTrace`.
pub(crate) fn install(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    let error_ctor = error_constructor(scope)?;
    let capture = error_ctor.val(scope, c"captureStackTrace")?;
    if js::function::is_callable(capture) {
        let wrapper =
            Function::new_callback(scope, c"captureStackTrace", 2, capture_stack_trace, capture)?;
        error_ctor.define_property(scope, c"captureStackTrace", *wrapper, 0)?;
    }
    let prototype =
        js::class::get_class_prototype(scope, js::class_spec::JSProtoKey::JSProto_Error)?;
    let Some(prototype) = Object::from_handle(prototype) else {
        return Ok(());
    };
    let stack = js::class::get_or_init_property_id(scope, c"stack")?;
    let Some(property) = prototype.own_property_descriptor(scope, stack)? else {
        return Ok(());
    };
    let Some(getter) = property.getter.filter(|getter| getter.is_callable()) else {
        return Ok(());
    };
    let wrapper = Function::new_callback(scope, c"stack", 0, stack_getter, getter)?;
    prototype.define_accessor(scope, c"stack", Some(*wrapper), property.setter, 0)
}
