// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:vm`. See <https://nodejs.org/api/vm.html>.
//!
//! A context's global is a global of [`js::realm::GLOBAL_CLASS`] in a new realm of the compartment
//! of the global that creates it. It has the standard classes and a `console` whose methods do
//! nothing, as the console of a new V8 context has, and none of the runtime's web or Node.js
//! globals. Objects pass between the realms without wrappers, so objects created in a context
//! have its prototypes, as in Node.js. A contextified object's global forwards to it as
//! [`js::realm`] describes, and scripts run in the context with a proxy of the contextified
//! object, [`js::realm::sandbox_environment`], on their environment chain, so they find its
//! properties before the global's, and their `var` and function declarations define its
//! properties. A declaration that the contextified object refuses defines a property of the
//! global instead.
//!
//! A contextified object, and a global created with `vm.constants.DONT_CONTEXTIFY`, holds its
//! context's global under the private name symbol [`CONTEXT_KEY`], as Node.js marks contextified
//! objects with a private symbol.
//!
//! These differ from Node.js:
//!
//! - The top-level `this` of a script that runs in a context is the proxy of the contextified
//!   object on its environment chain, and so is the `this` of a function that a script calls by a
//!   name the contextified object has. In Node.js, both are the context's global.
//! - `Object.keys`, `Object.getOwnPropertyNames`, `hasOwnProperty`,
//!   `Object.getOwnPropertyDescriptor` and `delete` on a context's global see its own properties
//!   only, not the contextified object's. An assignment to one of the global's own properties,
//!   such as `Object = 1`, changes the global's property, not the contextified object's.
//! - `var` and function declarations define non-configurable properties of the contextified
//!   object. A `var` declaration without an initializer defines one too, which Node.js leaves off
//!   the contextified object. A function declaration that the contextified object refuses for a
//!   property it has throws a `TypeError`, where Node.js throws a `SyntaxError`.
//! - `timeout` isn't enforced on wasm, which has no threads. `breakOnSigint` is validated and has
//!   no effect.
//! - Microtasks always run in the runtime's microtask queue: `microtaskMode` is validated and
//!   has no effect. So are `codeGeneration.wasm`, `name` and `origin`.
//! - An `importModuleDynamically` function isn't called: `import()` fails with
//!   `ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING_FLAG`, as in Node.js without
//!   `--experimental-vm-modules`. With `vm.constants.USE_MAIN_CONTEXT_DEFAULT_LOADER`, `import()`
//!   in a script that runs in a context fails with `ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING`.
//! - Code caches aren't produced or used: `createCachedData` and `produceCachedData` produce an
//!   empty `Buffer`, and `cachedDataRejected` is `true` for any `cachedData`.
//! - An error that a script throws gets the source line and caret Node.js prepends to its
//!   `stack` only if the script created it. The caret marks the `throw` keyword before the place
//!   it was created, on its line, or else that place, where Node.js's marks where it was thrown.
//! - `compileFunction`'s `columnOffset` doesn't apply to the first line of the function's body.
//!   Line numbers below 1 are 1.

use std::time::Duration;

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::gc::scope::{RootScope, Scope};
use js::prelude::HandleValue;
use js::{JSString, Object};

use crate::errors::{
    validate_boolean, validate_integer, validate_plain_object, validate_string, ErrorKind,
    NodeError, Thrown,
};

/// The default `filename` of a script.
const DEFAULT_FILENAME: &str = "evalmachine.<anonymous>";

/// The largest column SpiderMonkey compiles a script from, `JS::LimitedColumnNumberOneOrigin`'s
/// limit. A larger `columnOffset` is taken as this one.
const MAX_COLUMN: i64 = (1 << 30) - 1;

/// The methods of the `console` of a context, in the order V8 defines them.
const CONSOLE_METHODS: [&std::ffi::CStr; 24] = [
    c"debug",
    c"error",
    c"info",
    c"log",
    c"warn",
    c"dir",
    c"dirxml",
    c"table",
    c"trace",
    c"group",
    c"groupCollapsed",
    c"groupEnd",
    c"clear",
    c"count",
    c"countReset",
    c"assert",
    c"profile",
    c"profileEnd",
    c"time",
    c"timeLog",
    c"timeEnd",
    c"timeStamp",
    c"context",
    c"createTask",
];

/// `vm.constants.USE_MAIN_CONTEXT_DEFAULT_LOADER`.
static USE_MAIN_CONTEXT_DEFAULT_LOADER: js::symbol::CachedSymbol =
    js::symbol::CachedSymbol::new(c"vm_dynamic_import_main_context_default");

/// `vm.constants.DONT_CONTEXTIFY`.
static DONT_CONTEXTIFY: js::symbol::CachedSymbol =
    js::symbol::CachedSymbol::new(c"vm_context_no_contextify");

/// The key under which a context object holds its context's global.
static CONTEXT_KEY: js::symbol::CachedSymbol = js::symbol::CachedSymbol::private_name(c"kContext");

/// The context `object` is, if it is one: the global of its context, and whether `object` is a
/// contextified object rather than the global itself. Runs no JavaScript.
fn context_of<'s>(
    scope: &'s Scope<'_>,
    object: Object<'s>,
) -> Result<Option<(Object<'s>, bool)>, ExnThrown> {
    let key = CONTEXT_KEY.id(scope)?;
    let Some(own) = object.own_property_descriptor(scope, key)? else {
        return Ok(None);
    };
    let Some(Ok(global)) = own.value.map(|value| Object::from_value(scope, value)) else {
        return Ok(None);
    };
    let contextified = global.as_raw() != object.as_raw();
    Ok(Some((global, contextified)))
}

/// A new context for `object`, contextified unless it is `vm.constants.DONT_CONTEXTIFY`, whose
/// code can compile strings if `options.strings` is set. Returns `object` if it is contextified,
/// and the new global otherwise.
fn make_context<'s>(
    scope: &'s Scope<'_>,
    object: HandleValue<'s>,
    options: &ContextOptions,
) -> Result<Object<'s>, ExnThrown> {
    let global = js::realm::new_global(scope, core_runtime::runtime::realm_options())?;
    js::realm::set_code_generation_from_strings(global, options.strings);
    {
        let realm = enter(scope, global);
        define_console(&realm, global)?;
    }
    let key = CONTEXT_KEY.id(scope)?;
    if is_dont_contextify(scope, object)? {
        global.define_value_by_id(scope, key, global, 0)?;
        return Ok(global);
    }
    let sandbox = Object::from_value(scope, object).map_err(|e| e.throw(scope))?;
    js::realm::contextify(scope, global, sandbox)?;
    sandbox.define_value_by_id(scope, key, global, 0)?;
    Ok(sandbox)
}

/// Whether `value` is `vm.constants.DONT_CONTEXTIFY`.
fn is_dont_contextify(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
    Ok(value.is_symbol() && value.get() == DONT_CONTEXTIFY.value(scope)?.get())
}

/// A scope entered into the realm of `global`.
fn enter<'s>(
    scope: &'s Scope<'_>,
    global: Object<'_>,
) -> RootScope<'s, js::gc::scope::EnteredRealm> {
    RootScope::new_with_realm(
        scope.cx_mut(),
        global.as_non_null().expect("a rooted object is not null"),
    )
}

/// Define the `console` of a context on its global, an object whose methods do nothing.
fn define_console(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    let console = Object::new_plain(scope)?;
    for name in CONSOLE_METHODS {
        let method = js::Function::new_callback(scope, name, 0, do_nothing, ())?;
        console.set_property_by_name(scope, name, *method)?;
    }
    let tag = js::symbol::get_well_known_key(scope, js::native::SymbolCode::toStringTag);
    let tag = scope.root_id(tag);
    let name = JSString::from_str(scope, "console")?;
    console.define_value_by_id(scope, tag, name, u32::from(js::class_spec::JSPROP_READONLY))?;
    global.define_property(scope, c"console", console, 0)
}

/// The methods of a context's `console`.
fn do_nothing(
    _scope: &Scope<'_>,
    _args: js::function::CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<js::native::Value, ExnThrown> {
    Ok(js::value::undefined())
}

/// Node.js's `isContext` validation of the argument `object`: an object, including arrays, that
/// isn't callable.
fn validate_context_object<'s>(
    scope: &'s Scope<'_>,
    object: HandleValue<'_>,
    name: &str,
) -> Result<Object<'s>, Thrown> {
    Object::from_value(scope, *object)
        .ok()
        .filter(|object| !object.is_callable())
        .ok_or_else(|| NodeError::invalid_arg_type(scope, name, "of type object", object).into())
}

/// Node.js's `validateContext`: the global of the context `object`, which must be a context.
fn validate_context<'s>(
    scope: &'s Scope<'_>,
    object: HandleValue<'_>,
) -> Result<(Object<'s>, Object<'s>, bool), Thrown> {
    let candidate = validate_context_object(scope, object, "object")?;
    match context_of(scope, candidate)? {
        Some((global, contextified)) => Ok((candidate, global, contextified)),
        None => {
            Err(
                NodeError::invalid_arg_type(scope, "contextifiedObject", "an vm.Context", object)
                    .into(),
            )
        }
    }
}

/// The options of a script, as `new vm.Script` takes them.
struct ScriptOptions {
    filename: String,
    line_offset: i32,
    column_offset: i32,
    cached_data: bool,
    produce_cached_data: bool,
    dynamic_import: DynamicImport,
}

/// The `options` argument of a function that takes options or a filename, as an object:
/// `{ filename: options }` for a string, `{}` for `undefined`, and `ERR_INVALID_ARG_TYPE`
/// unless it is an object otherwise.
fn options_object<'s>(
    scope: &'s Scope<'_>,
    options: Option<HandleValue<'s>>,
    filename_shorthand: bool,
) -> Result<Object<'s>, Thrown> {
    let options = options.unwrap_or(HandleValue::undefined());
    if options.is_undefined() {
        return Ok(Object::new_plain(scope)?);
    }
    if filename_shorthand && options.is_string() {
        let object = Object::new_plain(scope)?;
        object.set_property(scope, c"filename", options)?;
        return Ok(object);
    }
    validate_plain_object(scope, options, "options", false)
}

/// The `options` of `vm.runInContext` and `vm.runInNewContext`, which Node.js spreads into a new
/// object: `{ filename: options }` for a string, a copy of the own enumerable properties of an
/// object, including an array or a function, and `{}` otherwise.
fn spread_options<'s>(
    scope: &'s Scope<'_>,
    options: Option<HandleValue<'s>>,
) -> Result<Object<'s>, ExnThrown> {
    let object = Object::new_plain(scope)?;
    match options {
        Some(options) if options.is_string() => {
            object.set_property(scope, c"filename", options)?;
        }
        Some(options) if options.is_object() => {
            object.assign(
                scope,
                Object::from_value(scope, *options).map_err(|e| e.throw(scope))?,
            )?;
        }
        _ => {}
    }
    Ok(object)
}

/// Node.js's `validateInt32` of `value`, named `name`, if it isn't `undefined`, and `0` otherwise.
fn int32_option(scope: &Scope<'_>, value: HandleValue<'_>, name: &str) -> Result<i32, Thrown> {
    if value.is_undefined() {
        return Ok(0);
    }
    Ok(validate_integer(scope, value, name, -2147483648.0, 2147483647.0)? as i32)
}

/// `value`, named `name`, which must be `undefined` or a boolean, or `default` for `undefined`.
fn boolean_option(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
    default: bool,
) -> Result<bool, Thrown> {
    if value.is_undefined() {
        return Ok(default);
    }
    validate_boolean(scope, value, name)
}

/// Check `cachedData`, which must be `undefined` or an `ArrayBufferView`. Returns whether it is
/// one.
fn cached_data_option(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<bool, Thrown> {
    if value.is_undefined() {
        return Ok(false);
    }
    let view = Object::from_value(scope, *value)
        .ok()
        .and_then(js::ArrayBufferView::from_object);
    if view.is_none() {
        return Err(NodeError::invalid_arg_type(
            scope,
            name,
            "an instance of Buffer, TypedArray, or DataView",
            value,
        )
        .into());
    }
    Ok(true)
}

/// What `import()` in a script does, as its `importModuleDynamically` option selects.
#[derive(Clone, Copy, Default)]
enum DynamicImport {
    /// It fails with `ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING`, without the option.
    #[default]
    Refused,
    /// It fails with `ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING_FLAG`, for a function.
    Callback,
    /// It loads modules as the runtime's module loader does if the script runs in the current
    /// global, and fails otherwise, for `vm.constants.USE_MAIN_CONTEXT_DEFAULT_LOADER`.
    MainContextLoader,
}

impl DynamicImport {
    /// Make `import()` in `script`, which runs in a context if `in_context`, behave as `self`
    /// describes.
    fn apply(
        self,
        scope: &Scope<'_>,
        script: js::native::GCHandle<'_, *mut js::native::JSScript>,
        in_context: bool,
    ) {
        use core_runtime::module::DynamicImportRefusal;
        let refusal = match self {
            DynamicImport::MainContextLoader if !in_context => return,
            DynamicImport::Refused | DynamicImport::MainContextLoader => {
                DynamicImportRefusal::CallbackMissing
            }
            DynamicImport::Callback => DynamicImportRefusal::CallbackMissingFlag,
        };
        core_runtime::module::refuse_dynamic_import(scope, script, refusal);
    }
}

/// Check `importModuleDynamically`, which must be `undefined`, a function or
/// `vm.constants.USE_MAIN_CONTEXT_DEFAULT_LOADER`.
fn check_import_module_dynamically(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<DynamicImport, Thrown> {
    if value.is_undefined() {
        return Ok(DynamicImport::Refused);
    }
    if value.is_symbol() && value.get() == USE_MAIN_CONTEXT_DEFAULT_LOADER.value(scope)?.get() {
        crate::internals::warn_experimental(scope, "vm.USE_MAIN_CONTEXT_DEFAULT_LOADER")?;
        return Ok(DynamicImport::MainContextLoader);
    }
    crate::errors::validate_function(scope, value, "options.importModuleDynamically")?;
    Ok(DynamicImport::Callback)
}

impl ScriptOptions {
    /// The script options of `options`, read and checked as `new vm.Script` reads and checks them.
    fn read(scope: &Scope<'_>, options: Object<'_>) -> Result<ScriptOptions, Thrown> {
        let filename = options.get_property(scope, c"filename")?;
        let line_offset = options.get_property(scope, c"lineOffset")?;
        let column_offset = options.get_property(scope, c"columnOffset")?;
        let cached_data = options.get_property(scope, c"cachedData")?;
        let produce_cached_data = options.get_property(scope, c"produceCachedData")?;
        let import_module_dynamically = options.get_property(scope, c"importModuleDynamically")?;
        let filename = if filename.is_undefined() {
            DEFAULT_FILENAME.to_string()
        } else {
            validate_string(scope, filename, "options.filename")?
        };
        let line_offset = int32_option(scope, line_offset, "options.lineOffset")?;
        let column_offset = int32_option(scope, column_offset, "options.columnOffset")?;
        let cached_data = cached_data_option(scope, cached_data, "options.cachedData")?;
        let produce_cached_data = boolean_option(
            scope,
            produce_cached_data,
            "options.produceCachedData",
            false,
        )?;
        let dynamic_import = check_import_module_dynamically(scope, import_module_dynamically)?;
        Ok(ScriptOptions {
            filename,
            line_offset,
            column_offset,
            cached_data,
            produce_cached_data,
            dynamic_import,
        })
    }
}

/// The options of running a script, as `script.runInContext` takes them.
struct RunOptions {
    timeout: Option<u32>,
    display_errors: bool,
}

impl RunOptions {
    /// The run options of `options`, read and checked as Node.js's `getRunInContextArgs` reads
    /// and checks them.
    fn read(scope: &Scope<'_>, options: Option<HandleValue<'_>>) -> Result<RunOptions, Thrown> {
        let options = match options.filter(|options| !options.is_undefined()) {
            Some(options) => validate_plain_object(scope, options, "options", false)?,
            None => Object::new_plain(scope)?,
        };
        let timeout = options.get_property(scope, c"timeout")?;
        let timeout = if timeout.is_undefined() {
            None
        } else {
            Some(validate_integer(scope, timeout, "options.timeout", 1.0, 4294967295.0)? as u32)
        };
        let display_errors = options.get_property(scope, c"displayErrors")?;
        let break_on_sigint = options.get_property(scope, c"breakOnSigint")?;
        let display_errors = boolean_option(scope, display_errors, "options.displayErrors", true)?;
        boolean_option(scope, break_on_sigint, "options.breakOnSigint", false)?;
        Ok(RunOptions {
            timeout,
            display_errors,
        })
    }
}

/// The options of a new context that have an effect.
struct ContextOptions {
    /// `codeGeneration.strings`: whether `eval` and the `Function` constructors work.
    strings: bool,
}

/// The options of a new context, as `vm.createContext` takes them, checked as Node.js checks
/// them.
fn check_context_options(
    scope: &Scope<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<ContextOptions, Thrown> {
    let mut result = ContextOptions { strings: true };
    let Some(options) = options.filter(|options| !options.is_undefined()) else {
        return Ok(result);
    };
    let options = validate_plain_object(scope, options, "options", false)?;
    let name = options.get_property(scope, c"name")?;
    let origin = options.get_property(scope, c"origin")?;
    let code_generation = options.get_property(scope, c"codeGeneration")?;
    let microtask_mode = options.get_property(scope, c"microtaskMode")?;
    let import_module_dynamically = options.get_property(scope, c"importModuleDynamically")?;
    if !name.is_undefined() {
        validate_string(scope, name, "options.name")?;
    }
    if !origin.is_undefined() {
        validate_string(scope, origin, "options.origin")?;
    }
    if !code_generation.is_undefined() {
        let code_generation =
            validate_plain_object(scope, code_generation, "options.codeGeneration", false)?;
        let strings = code_generation.get_property(scope, c"strings")?;
        let wasm = code_generation.get_property(scope, c"wasm")?;
        result.strings = boolean_option(scope, strings, "options.codeGeneration.strings", true)?;
        boolean_option(scope, wasm, "options.codeGeneration.wasm", true)?;
    }
    check_microtask_mode(scope, microtask_mode, "options.microtaskMode")?;
    check_import_module_dynamically(scope, import_module_dynamically)?;
    Ok(result)
}

/// Check `microtaskMode`, which must be `undefined` or `'afterEvaluate'`.
fn check_microtask_mode(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<(), Thrown> {
    if value.is_undefined()
        || (value.is_string()
            && JSString::from_value(scope, value)?.to_utf8(scope)? == "afterEvaluate")
    {
        return Ok(());
    }
    Err(NodeError::invalid_arg_value(
        scope,
        name,
        value,
        "must be one of: 'afterEvaluate', undefined",
    )
    .into())
}

/// The context options `runInNewContext` takes, as Node.js's `getContextOptions` checks them:
/// `contextName`, `contextOrigin`, `contextCodeGeneration` and `microtaskMode`.
fn check_new_context_options(
    scope: &Scope<'_>,
    options: Object<'_>,
) -> Result<ContextOptions, Thrown> {
    let mut result = ContextOptions { strings: true };
    let name = options.get_property(scope, c"contextName")?;
    let origin = options.get_property(scope, c"contextOrigin")?;
    let microtask_mode = options.get_property(scope, c"microtaskMode")?;
    if !name.is_undefined() {
        validate_string(scope, name, "options.contextName")?;
    }
    if !origin.is_undefined() {
        validate_string(scope, origin, "options.contextOrigin")?;
    }
    let code_generation = options.get_property(scope, c"contextCodeGeneration")?;
    if !code_generation.is_undefined() {
        let code_generation = validate_plain_object(
            scope,
            code_generation,
            "options.contextCodeGeneration",
            false,
        )?;
        let strings = code_generation.get_property(scope, c"strings")?;
        let wasm = code_generation.get_property(scope, c"wasm")?;
        result.strings = boolean_option(
            scope,
            strings,
            "options.contextCodeGeneration.strings",
            true,
        )?;
        boolean_option(scope, wasm, "options.contextCodeGeneration.wasm", true)?;
    }
    if !microtask_mode.is_undefined() {
        validate_string(scope, microtask_mode, "options.microtaskMode")?;
    }
    check_microtask_mode(scope, microtask_mode, "options.microtaskMode")?;
    Ok(result)
}

/// `code` as a string, as `${code}` converts it.
fn code_units(scope: &Scope<'_>, code: HandleValue<'_>) -> Result<Vec<u16>, ExnThrown> {
    JSString::from_value(scope, code)?.to_utf16_vec(scope)
}

/// Compile options for a script from `options`, for a contextified global if `contextified`.
fn compile_options(
    scope: &Scope<'_>,
    options: &SourceOrigin,
    contextified: bool,
) -> js::compile::CompileOptionsWrapper {
    let filename =
        std::ffi::CString::new(options.filename.replace('\0', "")).expect("NULs were removed");
    let line = (i64::from(options.line_offset) + 1).max(1) as u32;
    let compile = js::compile::options(scope, filename, line);
    let column = (i64::from(options.column_offset) + 1).clamp(1, MAX_COLUMN) as u32;
    // SAFETY: `compile.ptr` is a valid pointer created by `NewCompileOptions`, owned by `compile`.
    unsafe {
        (*compile.ptr).column._base = column;
        (*compile.ptr)._base.nonSyntacticScope = contextified;
    }
    compile
}

/// Where a script's source comes from: its file name and the line and column of its start.
#[derive(Default)]
struct SourceOrigin {
    filename: String,
    line_offset: i32,
    column_offset: i32,
}

/// The state of a `vm.Script`: its source and where it comes from. A script is compiled in the
/// realm of the global it runs in each time it runs, since a compiled script belongs to one realm.
#[core_runtime::jsclass(hidden)]
pub(crate) struct ScriptState {
    #[no_trace]
    source: Vec<u16>,
    #[no_trace]
    origin: SourceOrigin,
    #[no_trace]
    dynamic_import: DynamicImport,
}

#[core_runtime::jsmethods]
impl ScriptState {}

/// Compile `source` from `origin` as a script of the current realm, for a contextified global if
/// `contextified`.
fn compile<'s>(
    scope: &'s Scope<'_>,
    source: &[u16],
    origin: &SourceOrigin,
    contextified: bool,
) -> Result<js::native::GCHandle<'s, *mut js::native::JSScript>, ExnThrown> {
    let options = compile_options(scope, origin, contextified);
    js::compile::compile_utf16(scope, source, &options)
}

/// Run the script of `state` in the global of `context`: the current global if `context` is
/// `None`, and otherwise the context's global, with the global's
/// [`js::realm::sandbox_environment`] on the environment chain if `contextified`.
fn run_script<'s>(
    scope: &'s Scope<'_>,
    state: ScriptState<'_>,
    context: Option<(Object<'_>, Object<'_>, bool)>,
    options: &RunOptions,
) -> Result<HandleValue<'s>, Thrown> {
    let (global, contextified) = match context {
        Some((_, global, contextified)) => (global, contextified),
        None => (scope.global(), false),
    };
    let realm = enter(scope, global);
    let environment = contextified
        .then(|| js::realm::sandbox_environment(&realm, global))
        .flatten();
    let decorate_with_source = |realm: &Scope<'_>| {
        decorate_pending(realm, |error| {
            let data = state.data();
            arrow_message(realm, error, &data.source, &data.origin)
        });
    };
    let script = {
        let data = state.data();
        compile(&realm, &data.source, &data.origin, environment.is_some())
            .inspect(|&script| data.dynamic_import.apply(&realm, script, context.is_some()))
    };
    let script = match script {
        Ok(script) => script,
        Err(ExnThrown) => {
            if js::exception::is_pending(&realm) {
                decorate_with_source(&realm);
            }
            return Err(Thrown::Pending);
        }
    };
    let execute = || match environment {
        Some(environment) => js::compile::execute_script_with_env(&realm, script, &[environment]),
        None => js::compile::execute_script(&realm, script),
    };
    let (result, timed_out) = match options.timeout {
        Some(timeout) => js::watchdog::with_time_limit(
            &realm,
            Duration::from_millis(u64::from(timeout)),
            execute,
        ),
        None => (execute(), false),
    };
    match result {
        Ok(value) => Ok(scope.root_value(value.get())),
        Err(ExnThrown) if js::exception::is_pending(&realm) => {
            if options.display_errors {
                decorate_with_source(&realm);
            }
            Err(Thrown::Pending)
        }
        Err(ExnThrown) if timed_out => {
            let timeout = options.timeout.unwrap_or_default();
            Err(NodeError::new(
                ErrorKind::Error,
                "ERR_SCRIPT_EXECUTION_TIMEOUT",
                format!("Script execution timed out after {timeout}ms"),
            )
            .into())
        }
        Err(ExnThrown) => Err(Thrown::Pending),
    }
}

/// Prepend the arrow message `arrow` returns for the pending exception to its `stack`, as
/// Node.js's `decorateErrorStack` does, unless `arrow` returns `None` or the `stack` isn't a
/// string or starts with the arrow message's location line already. `arrow` must not run
/// JavaScript.
fn decorate_pending(scope: &Scope<'_>, arrow: impl FnOnce(HandleValue<'_>) -> Option<String>) {
    let Some(exception) = js::exception::take_pending(scope) else {
        return;
    };
    if let Some(arrow) = arrow(exception) {
        if prepend_to_stack(scope, exception, &arrow).is_err() {
            js::exception::clear(scope);
        }
    }
    js::exception::throw_value(scope, exception);
}

/// Prepend `arrow` to the `stack` of `error`, as [`decorate_pending`] describes. A stack in
/// SpiderMonkey's format is rewritten in V8's, as `util.inspect` shows it.
fn prepend_to_stack(
    scope: &Scope<'_>,
    error: HandleValue<'_>,
    arrow: &str,
) -> Result<(), ExnThrown> {
    let error = Object::from_value(scope, error).map_err(|e| e.throw(scope))?;
    let stack = error.get_property(scope, c"stack")?;
    if !stack.is_string() {
        return Ok(());
    }
    let stack = JSString::from_value(scope, stack)?.to_utf8(scope)?;
    let location = arrow.split_inclusive('\n').next().unwrap_or_default();
    if stack.starts_with(location) {
        return Ok(());
    }
    let stack = crate::inspect::v8_stack(scope, error, stack)?;
    error.set_property(scope, c"stack", format!("{arrow}{stack}").as_str())
}

/// Node.js's arrow message for `error` if it is an error that the script with `source` from
/// `origin` created: its location, its line of source, and a caret under its column, followed by
/// a blank line. Runs no JavaScript.
fn arrow_message(
    scope: &Scope<'_>,
    error: HandleValue<'_>,
    source: &[u16],
    origin: &SourceOrigin,
) -> Option<String> {
    if !error.is_object() {
        return None;
    }
    let error = Object::from_value(scope, error).ok()?;
    let report = js::exception::error_from_exception(scope, error.handle())?;
    // SAFETY: a report's filename is null or a NUL-terminated string.
    let filename = unsafe {
        let filename = report._base.filename.data_;
        (!filename.is_null()).then(|| std::ffi::CStr::from_ptr(filename).to_string_lossy())
    };
    if filename.as_deref() != Some(origin.filename.as_str()) {
        return None;
    }
    let line = report._base.lineno;
    let column = report._base.column._base;
    let first_line = (i64::from(origin.line_offset) + 1).max(1);
    let index = usize::try_from(i64::from(line) - first_line).ok()?;
    let units = source_line(source, index)?;
    let mut caret = column.saturating_sub(1) as usize;
    if index == 0 {
        caret = caret.saturating_sub(origin.column_offset.max(0) as usize);
    }
    let caret = caret.min(units.len());
    let caret = throw_keyword_before(units, caret).unwrap_or(caret);
    let text = String::from_utf16_lossy(units);
    Some(format!(
        "{}:{line}\n{text}\n{}^\n\n",
        origin.filename,
        " ".repeat(caret)
    ))
}

/// The position of the last `throw` keyword of `line` before `end`, where V8's caret for an error
/// created in a `throw` statement points, or `None` if there is none.
fn throw_keyword_before(line: &[u16], end: usize) -> Option<usize> {
    let keyword: Vec<u16> = "throw".encode_utf16().collect();
    let is_identifier = |unit: Option<&u16>| {
        unit.and_then(|&unit| char::from_u32(u32::from(unit)))
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$')
    };
    (0..end.saturating_sub(keyword.len() - 1))
        .rev()
        .find(|&start| {
            line[start..].starts_with(&keyword)
                && !is_identifier(start.checked_sub(1).and_then(|before| line.get(before)))
                && !is_identifier(line.get(start + keyword.len()))
        })
}

/// The line `index` of `source`, without its terminator. Lines end at `\n`, `\r\n`, `\r`, U+2028
/// and U+2029.
fn source_line(source: &[u16], index: usize) -> Option<&[u16]> {
    let mut start = 0;
    let mut line = 0;
    let mut position = 0;
    while position < source.len() {
        let unit = source[position];
        if matches!(unit, 0x0a | 0x0d | 0x2028 | 0x2029) {
            if line == index {
                return Some(&source[start..position]);
            }
            if unit == 0x0d && source.get(position + 1) == Some(&0x0a) {
                position += 1;
            }
            line += 1;
            start = position + 1;
        }
        position += 1;
    }
    (line == index).then(|| &source[start..])
}

/// The value of the last `//# name=value` or `//@ name=value` comment in `source`, as V8 reads a
/// script's `sourceURL` and `sourceMappingURL`, or `None` if there is none.
fn magic_comment(source: &str, name: &str) -> Option<String> {
    let found = ["//# ", "//@ "]
        .iter()
        .filter_map(|prefix| source.rfind(&format!("{prefix}{name}=")))
        .max()?;
    let value = &source[found + 4 + name.len() + 1..];
    let end = value
        .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
        .unwrap_or(value.len());
    Some(value[..end].to_string())
}

/// Initialize the `vm.Script` `this` with `code` and `options`, as `new vm.Script` does.
fn init_script(
    scope: &Scope<'_>,
    this: Object<'_>,
    code: Option<HandleValue<'_>>,
    options: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    let source = code_units(scope, code.unwrap_or(HandleValue::undefined()))?;
    let options = options_object(scope, options, true)?;
    let options = ScriptOptions::read(scope, options)?;
    let origin = SourceOrigin {
        filename: options.filename,
        line_offset: options.line_offset,
        column_offset: options.column_offset,
    };
    if compile(scope, &source, &origin, false).is_err() {
        decorate_pending(scope, |error| arrow_message(scope, error, &source, &origin));
        return Err(Thrown::Pending);
    }
    if options.cached_data {
        this.set_property(scope, c"cachedDataRejected", true)?;
    }
    if options.produce_cached_data {
        this.set_property(scope, c"cachedData", crate::buffer::new_buffer(scope, &[])?)?;
        this.set_property(scope, c"cachedDataProduced", true)?;
    }
    let text = String::from_utf16_lossy(&source);
    let source_url = magic_comment(&text, "sourceURL");
    let source_map_url = magic_comment(&text, "sourceMappingURL");
    set_optional_string(scope, this, c"sourceURL", source_url.as_deref())?;
    set_optional_string(scope, this, c"sourceMapURL", source_map_url.as_deref())?;
    let state = contextify_script_class::state_of(scope, this)?;
    *state.data_mut() = ScriptStateImpl {
        source,
        origin,
        dynamic_import: options.dynamic_import,
    };
    Ok(())
}

/// Set `object[name]` to `value`, or to `undefined` for `None`.
fn set_optional_string(
    scope: &Scope<'_>,
    object: Object<'_>,
    name: &'static std::ffi::CStr,
    value: Option<&str>,
) -> Result<(), ExnThrown> {
    match value {
        Some(value) => object.set_property(scope, name, value),
        None => object.set_property(scope, name, HandleValue::undefined()),
    }
}

/// The base class of `vm.Script`, which Node.js doesn't export.
#[core_runtime::jsfunction_class(
    name = "ContextifyScript",
    class_syntax,
    state = "ScriptState",
    invalid_this = "crate::errors::invalid_this"
)]
pub mod contextify_script_class {
    use super::*;

    /// Compile `code` with `options`, as `new vm.Script(code, options)` does.
    #[constructor(length = 1)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        code: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        init_script(scope, this, code, options)
    }

    /// <https://nodejs.org/api/vm.html#scriptcreatecacheddata>: an empty `Buffer`, since no code
    /// cache is produced.
    pub fn create_cached_data<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<Object<'s>, Thrown> {
        state_of(scope, this)?;
        Ok(crate::buffer::new_buffer(scope, &[])?)
    }

    /// Node.js's native `runInContext(contextifiedObject, timeout, displayErrors, breakOnSigint,
    /// breakOnFirstLine)`: run the script in the context of `contextified_object`, or in the
    /// current global if it is `null`, with a time limit of `timeout` milliseconds if it is a
    /// number of at least 1.
    #[method(length = 5)]
    pub fn run_in_context<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        contextified_object: HandleValue<'_>,
        timeout: Option<HandleValue<'_>>,
        display_errors: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let state = state_of(scope, this)?;
        let context = if contextified_object.is_null() {
            None
        } else {
            Some(validate_context(scope, contextified_object)?)
        };
        let timeout = timeout
            .filter(|timeout| timeout.is_number() && timeout.to_number() >= 1.0)
            .map(|timeout| timeout.to_number().min(f64::from(u32::MAX)) as u32);
        let display_errors = display_errors.is_none_or(js::conversion::to_boolean);
        let options = RunOptions {
            timeout,
            display_errors,
        };
        run_script(scope, state, context, &options)
    }
}

/// <https://nodejs.org/api/vm.html#class-vmscript>
#[core_runtime::jsfunction_class(
    name = "Script",
    class_syntax,
    extends = "super::contextify_script_class"
)]
pub mod script_class {
    use super::*;

    /// <https://nodejs.org/api/vm.html#new-vmscriptcode-options>: `code` is converted to a string,
    /// and a string `options` is the `filename`.
    #[constructor(length = 1)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        code: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        init_script(scope, this, code, options)
    }

    /// <https://nodejs.org/api/vm.html#scriptruninthiscontextoptions>
    #[method(length = 1)]
    pub fn run_in_this_context<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let state = contextify_script_class::state_of(scope, this)?;
        let options = RunOptions::read(scope, options)?;
        run_script(scope, state, None, &options)
    }

    /// <https://nodejs.org/api/vm.html#scriptrunincontextcontextifiedobject-options>
    #[method(length = 2)]
    pub fn run_in_context<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        contextified_object: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let state = contextify_script_class::state_of(scope, this)?;
        let context = validate_context(scope, contextified_object)?;
        let options = RunOptions::read(scope, options)?;
        run_script(scope, state, Some(context), &options)
    }

    /// <https://nodejs.org/api/vm.html#scriptruninnewcontextcontextobject-options>
    #[method(length = 0)]
    pub fn run_in_new_context<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        context_object: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let state = contextify_script_class::state_of(scope, this)?;
        let context_options = match options.filter(|options| options.is_object()) {
            Some(options) => {
                let options = Object::from_value(scope, *options).map_err(|e| e.throw(scope))?;
                check_new_context_options(scope, options)?
            }
            None => ContextOptions { strings: true },
        };
        let context_object = context_object.map(|object| scope.root_value(object.get()));
        let context = create_context(scope, context_object, || Ok(context_options))?;
        let context = validate_context(scope, context)?;
        let options = RunOptions::read(scope, options)?;
        run_script(scope, state, Some(context), &options)
    }
}

/// Create the classes of `node:vm` in the current realm.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    ScriptState::add_to_global(scope, global);
    contextify_script_class::register(scope)?;
    script_class::register(scope)?;
    Ok(())
}

/// Register `node:vm` as builtin module `name`, with its `constants`.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let constants = || -> Result<HandleValue<'_>, ExnThrown> {
        let constants = Object::new_with_null_prototype(scope)?;
        let flags = u32::from(js::class_spec::JSPROP_ENUMERATE);
        constants.define_property(
            scope,
            c"USE_MAIN_CONTEXT_DEFAULT_LOADER",
            USE_MAIN_CONTEXT_DEFAULT_LOADER.value(scope)?,
            flags,
        )?;
        constants.define_property(
            scope,
            c"DONT_CONTEXTIFY",
            DONT_CONTEXTIFY.value(scope)?,
            flags,
        )?;
        constants.freeze(scope)?;
        constants.to_jsval_throwing(scope)
    };
    let Ok(constants) = constants() else {
        return false;
    };
    crate::stub::register_implemented_with::<module::js_module>(
        scope,
        name,
        &[("constants", constants)],
    )
}

/// Compile and run `code` with `options`, which holds the options of both, in the context of
/// `context`, or in the current global for `None`.
fn compile_and_run<'s>(
    scope: &'s Scope<'_>,
    code: HandleValue<'_>,
    options: Object<'_>,
    context: Option<(Object<'_>, Object<'_>, bool)>,
) -> Result<HandleValue<'s>, Thrown> {
    let state = contextify_script_class::new_instance(scope)?;
    let script_options = scope.root_value(options.to_jsval_throwing(scope)?.get());
    let source = code_units(scope, code)?;
    let script = ScriptOptions::read(scope, options)?;
    let origin = SourceOrigin {
        filename: script.filename,
        line_offset: script.line_offset,
        column_offset: script.column_offset,
    };
    let instance = contextify_script_class::state_of(scope, state)?;
    *instance.data_mut() = ScriptStateImpl {
        source,
        origin,
        dynamic_import: script.dynamic_import,
    };
    let run = RunOptions::read(scope, Some(script_options))?;
    run_script(scope, instance, context, &run)
}

#[core_runtime::jsmodule(node_args, name = "node:vm", default_namespace)]
pub mod module {
    use super::*;

    pub use super::script_class as Script;

    /// <https://nodejs.org/api/vm.html#vmcreatecontextcontextobject-options>: `options.name`,
    /// `options.origin`, `options.codeGeneration.wasm`, `options.microtaskMode` and
    /// `options.importModuleDynamically` are checked, and have no effect.
    #[function(length = 0)]
    pub fn create_context<'s>(
        scope: &'s Scope<'_>,
        context_object: Option<HandleValue<'s>>,
        options: Option<HandleValue<'s>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        super::create_context(scope, context_object, || {
            check_context_options(scope, options)
        })
    }

    /// <https://nodejs.org/api/vm.html#vmiscontextobject>
    pub fn is_context(scope: &Scope<'_>, object: HandleValue<'_>) -> Result<bool, Thrown> {
        let object = validate_context_object(scope, object, "object")?;
        Ok(context_of(scope, object)?.is_some())
    }

    /// <https://nodejs.org/api/vm.html#vmcreatescriptcode-options>
    #[function(length = 2)]
    pub fn create_script<'s>(
        scope: &'s Scope<'_>,
        code: Option<HandleValue<'s>>,
        options: Option<HandleValue<'s>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let class = script_class::class_constructor(scope)?;
        let code = code.unwrap_or(HandleValue::undefined());
        let options = options.unwrap_or(HandleValue::undefined());
        js::Function::construct(scope, class, &[code, options])
    }

    /// <https://nodejs.org/api/vm.html#vmrunincontextcode-contextifiedobject-options>
    #[function(length = 3)]
    pub fn run_in_context<'s>(
        scope: &'s Scope<'_>,
        code: HandleValue<'s>,
        contextified_object: HandleValue<'s>,
        options: Option<HandleValue<'s>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let context = validate_context(scope, contextified_object)?;
        let options = spread_options(scope, options)?;
        compile_and_run(scope, code, options, Some(context))
    }

    /// <https://nodejs.org/api/vm.html#vmruninnewcontextcode-contextobject-options>
    #[function(length = 3)]
    pub fn run_in_new_context<'s>(
        scope: &'s Scope<'_>,
        code: HandleValue<'s>,
        context_object: Option<HandleValue<'s>>,
        options: Option<HandleValue<'s>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let options = spread_options(scope, options)?;
        let context_options = check_new_context_options(scope, options)?;
        let context = super::create_context(scope, context_object, || Ok(context_options))?;
        let context = validate_context(scope, context)?;
        compile_and_run(scope, code, options, Some(context))
    }

    /// <https://nodejs.org/api/vm.html#vmruninthiscontextcode-options>
    #[function(length = 2)]
    pub fn run_in_this_context<'s>(
        scope: &'s Scope<'_>,
        code: HandleValue<'s>,
        options: Option<HandleValue<'s>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let options = options_object(scope, options, true)?;
        compile_and_run(scope, code, options, None)
    }

    /// <https://nodejs.org/api/vm.html#vmcompilefunctioncode-params-options>: the function is
    /// compiled in the context of `options.parsingContext` if given, with the objects of
    /// `options.contextExtensions` in its scope, the last innermost.
    #[function(length = 2)]
    pub fn compile_function<'s>(
        scope: &'s Scope<'_>,
        code: HandleValue<'s>,
        params: Option<HandleValue<'s>>,
        options: Option<HandleValue<'s>>,
    ) -> Result<Object<'s>, Thrown> {
        super::compile_function(scope, code, params, options)
    }

    /// <https://nodejs.org/api/vm.html#vmmeasurememoryoptions>: the estimate is the size of the
    /// GC heap, and the promise is resolved at once in both execution modes.
    #[function(length = 0)]
    pub fn measure_memory<'s>(
        scope: &'s Scope<'_>,
        options: Option<HandleValue<'s>>,
    ) -> Result<Object<'s>, Thrown> {
        Ok(*super::measure_memory(scope, options)?)
    }
}

/// `vm.createContext(context_object, options)`, where `options` returns the checked options of a
/// context that is created.
fn create_context<'s>(
    scope: &'s Scope<'_>,
    context_object: Option<HandleValue<'s>>,
    options: impl FnOnce() -> Result<ContextOptions, Thrown>,
) -> Result<HandleValue<'s>, Thrown> {
    let object = match context_object.filter(|object| !object.is_undefined()) {
        Some(object) => object,
        None => Object::new_plain(scope)?.to_jsval_throwing(scope)?,
    };
    if !is_dont_contextify(scope, object)? {
        let candidate = validate_context_object(scope, object, "object")?;
        if context_of(scope, candidate)?.is_some() {
            return Ok(object);
        }
    }
    let options = options()?;
    Ok(make_context(scope, object, &options)?.to_jsval_throwing(scope)?)
}

/// [`module::compile_function`].
fn compile_function<'s>(
    scope: &'s Scope<'_>,
    code: HandleValue<'s>,
    params: Option<HandleValue<'s>>,
    options: Option<HandleValue<'s>>,
) -> Result<Object<'s>, Thrown> {
    validate_string(scope, code, "code")?;
    let mut names = Vec::new();
    if let Some(params) = params.filter(|params| !params.is_undefined()) {
        let Some(elements) = crate::support::array_elements(scope, params)? else {
            return Err(NodeError::invalid_arg_type(
                scope,
                "params",
                "an instance of Array",
                params,
            )
            .into());
        };
        for (index, param) in elements.into_iter().enumerate() {
            let name = format!("params[{index}]");
            let text = validate_string(scope, param, &name)?;
            let text = std::ffi::CString::new(text).map_err(|_| {
                NodeError::invalid_arg_value(scope, &name, param, "must not contain null bytes")
            })?;
            names.push(text);
        }
    }
    let options = match options.filter(|options| !options.is_undefined()) {
        Some(options) => validate_plain_object(scope, options, "options", false)?,
        None => Object::new_plain(scope)?,
    };
    let filename = options.get_property(scope, c"filename")?;
    let column_offset = options.get_property(scope, c"columnOffset")?;
    let line_offset = options.get_property(scope, c"lineOffset")?;
    let cached_data = options.get_property(scope, c"cachedData")?;
    let produce_cached_data = options.get_property(scope, c"produceCachedData")?;
    let parsing_context = options.get_property(scope, c"parsingContext")?;
    let context_extensions = options.get_property(scope, c"contextExtensions")?;
    let import_module_dynamically = options.get_property(scope, c"importModuleDynamically")?;
    let filename = if filename.is_undefined() {
        String::new()
    } else {
        validate_string(scope, filename, "options.filename")?
    };
    let column_offset = int32_option(scope, column_offset, "options.columnOffset")?;
    let line_offset = int32_option(scope, line_offset, "options.lineOffset")?;
    let cached_data = cached_data_option(scope, cached_data, "options.cachedData")?;
    let produce_cached_data = boolean_option(
        scope,
        produce_cached_data,
        "options.produceCachedData",
        false,
    )?;
    let context = if parsing_context.is_undefined() {
        None
    } else {
        let not_context = || {
            NodeError::invalid_arg_type(
                scope,
                "options.parsingContext",
                "an instance of Context",
                parsing_context,
            )
        };
        let object = Object::from_value(scope, *parsing_context).map_err(|_| not_context())?;
        let (global, contextified) = context_of(scope, object)?.ok_or_else(not_context)?;
        Some((object, global, contextified))
    };
    let mut extensions = Vec::new();
    if !context_extensions.is_undefined() {
        let Some(elements) = crate::support::array_elements(scope, context_extensions)? else {
            return Err(NodeError::invalid_arg_type(
                scope,
                "options.contextExtensions",
                "an instance of Array",
                context_extensions,
            )
            .into());
        };
        for (index, extension) in elements.into_iter().enumerate() {
            if extension.is_null() {
                continue;
            }
            let name = format!("options.contextExtensions[{index}]");
            extensions.push(validate_plain_object(scope, extension, &name, false)?);
        }
    }
    let dynamic_import = check_import_module_dynamically(scope, import_module_dynamically)?;
    // The last extension is the innermost, and the contextified object encloses them all.
    extensions.reverse();
    if let Some((object, _, true)) = context {
        extensions.push(object);
    }
    let body = code_units(scope, code)?;
    let origin = SourceOrigin {
        filename,
        line_offset,
        column_offset,
    };
    let params: Vec<&std::ffi::CStr> = names.iter().map(|name| name.as_c_str()).collect();
    let function = {
        let global = context.map_or_else(|| scope.global(), |(_, global, _)| global);
        let realm = enter(scope, global);
        let filename =
            std::ffi::CString::new(origin.filename.replace('\0', "")).expect("NULs were removed");
        let line = origin.line_offset.max(0) as u32;
        let options = js::compile::options(&realm, filename, line);
        match js::compile::compile_function_with_env(
            &realm,
            &extensions,
            c"",
            &params,
            &body,
            &options,
        ) {
            Ok(function) => {
                let script = function
                    .script(&realm)
                    .expect("a compiled function has a script");
                dynamic_import.apply(&realm, script, context.is_some());
                scope.root_object(function.as_non_null().expect("a function is not null"))
            }
            Err(ExnThrown) => {
                let header = SourceOrigin {
                    filename: origin.filename.clone(),
                    line_offset: origin.line_offset,
                    column_offset: 0,
                };
                decorate_pending(&realm, |error| arrow_message(&realm, error, &body, &header));
                return Err(Thrown::Pending);
            }
        }
    };
    let function = Object::from_handle(function).expect("a function is not null");
    if produce_cached_data {
        function.set_property(scope, c"cachedDataProduced", true)?;
        function.set_property(scope, c"cachedData", crate::buffer::new_buffer(scope, &[])?)?;
    }
    if cached_data {
        function.set_property(scope, c"cachedDataRejected", true)?;
    }
    Ok(function)
}

/// [`module::measure_memory`].
fn measure_memory<'s>(
    scope: &'s Scope<'_>,
    options: Option<HandleValue<'s>>,
) -> Result<js::Promise<'s>, Thrown> {
    crate::internals::warn_experimental(scope, "vm.measureMemory")?;
    let options = match options.filter(|options| !options.is_undefined()) {
        Some(options) => validate_plain_object(scope, options, "options", false)?,
        None => Object::new_plain(scope)?,
    };
    let mode = options.get_property(scope, c"mode")?;
    let execution = options.get_property(scope, c"execution")?;
    let detailed = one_of(scope, mode, "options.mode", &["summary", "detailed"])? == Some(1);
    one_of(scope, execution, "options.execution", &["default", "eager"])?;
    let bytes = js::gc::get_parameter(scope, js::gc::JSGCParamKey::JSGC_BYTES);
    let estimate = || -> Result<Object<'s>, ExnThrown> {
        let estimate = Object::new_plain(scope)?;
        estimate.set_property(scope, c"jsMemoryEstimate", bytes)?;
        let range = js::Array::with_contents(scope, &[bytes, bytes])?;
        estimate.set_property(scope, c"jsMemoryRange", range)?;
        Ok(estimate)
    };
    let result = Object::new_plain(scope)?;
    result.set_property(scope, c"total", estimate()?)?;
    let wasm = Object::new_plain(scope)?;
    wasm.set_property(scope, c"code", 0)?;
    wasm.set_property(scope, c"metadata", 0)?;
    result.set_property(scope, c"WebAssembly", wasm)?;
    if detailed {
        result.set_property(scope, c"current", estimate()?)?;
        let other: [HandleValue<'_>; 0] = [];
        result.set_property(scope, c"other", js::Array::with_contents(scope, &other)?)?;
    }
    let result = result.to_jsval_throwing(scope)?;
    Ok(js::Promise::new_resolved_with_value(scope, result)?)
}

/// The index of `value` in `allowed`, Node.js's `validateOneOf` check of the property `name`,
/// with `undefined` for its default, `None`.
fn one_of(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
    allowed: &[&str],
) -> Result<Option<usize>, Thrown> {
    if value.is_undefined() {
        return Ok(None);
    }
    if value.is_string() {
        let text = JSString::from_value(scope, value)?.to_utf8(scope)?;
        if let Some(index) = allowed.iter().position(|allowed| *allowed == text) {
            return Ok(Some(index));
        }
    }
    let list = allowed
        .iter()
        .map(|allowed| format!("'{allowed}'"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(NodeError::invalid_arg_value(scope, name, value, &format!("must be one of: {list}")).into())
}
