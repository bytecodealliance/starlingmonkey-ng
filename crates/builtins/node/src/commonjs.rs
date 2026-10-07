// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! CommonJS modules and `node:module`. See <https://nodejs.org/api/modules.html> and
//! <https://nodejs.org/api/module.html>.
//!
//! A CommonJS module is a `Module` object, cached in `Module._cache` (`require.cache`) under its
//! canonical path before it runs, so a cycle gets the partially populated `module.exports`. Its
//! source runs as the body of a function with the parameters `exports`, `require`, `module`,
//! `__filename` and `__dirname`, with `this` the module's `exports`.
//!
//! `require` loads builtin modules, CommonJS modules, JSON files and ES modules, which it
//! evaluates synchronously. As in Node.js, each step goes through the property that code can
//! replace: `require` calls `module.require`, which calls `Module._load`, which calls
//! `Module._resolveFilename` and `module.load`, which calls the `Module._extensions` loader for the
//! file's extension, and the `.js` loader calls `module._compile`. `core_runtime::module` calls
//! [`load_for_import`] for an ES module's `import` of a CommonJS module, which starts at
//! `module.load`.
//!
//! Reading a module file blocks, which isn't allowed while serving requests. So when a script
//! that serves requests loads a CommonJS module, we also resolve and read the CommonJS modules
//! and JSON files its source `require`s with a string literal, and theirs in turn. A later
//! `require` of one of them uses that resolution and source without host I/O.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use core_runtime::module::{
    builtin_module, resolve_require, ResolveError, ResolveErrorKind, SourceFormat,
};
use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{validate_string, ErrorKind, NodeError, Thrown};

/// The parameters of the function a CommonJS module's source is the body of.
const WRAPPER_PARAMS: [&std::ffi::CStr; 5] = [
    c"exports",
    c"require",
    c"module",
    c"__filename",
    c"__dirname",
];

/// `Module._cache`, the realm's `require.cache`.
fn module_cache<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    let module = module_class::class_constructor(scope)?;
    let cache = module.get_property(scope, c"_cache")?;
    Object::from_value(scope, cache).map_err(|e| e.throw(scope))
}

/// `MODULE_NOT_FOUND`: `specifier` resolves to no module.
fn module_not_found(specifier: &str, reason: &str) -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "MODULE_NOT_FOUND",
        format!("Cannot find module '{specifier}': {reason}"),
    )
}

/// The error of a `require` whose specifier failed to resolve with `error`.
fn resolve_failed(error: ResolveError) -> NodeError {
    NodeError::new(ErrorKind::Error, error.kind.require_code(), error.message)
}

js::instance_local! {
    /// The sources [`prefetch_requires`] read, by canonical path.
    static PREFETCHED: RefCell<BTreeMap<PathBuf, String>> = const { RefCell::new(BTreeMap::new()) };

    /// The resolutions [`prefetch_requires`] found, by the directory a `require` resolves
    /// against and its specifier.
    static RESOLVED: RefCell<BTreeMap<(PathBuf, String), (PathBuf, SourceFormat)>> =
        const { RefCell::new(BTreeMap::new()) };

    /// The formats of the files [`resolve`] and [`load_for_import`] resolved, by path.
    static FORMATS: RefCell<BTreeMap<PathBuf, SourceFormat>> = const { RefCell::new(BTreeMap::new()) };
}

/// The source of the file at `path`, with a byte order mark and a `#!` line removed: the source
/// [`prefetch_requires`] read, or the file's current content.
///
/// Reading the file blocks, so it throws `ERR_BLOCKING_NOT_ALLOWED` where blocking isn't allowed.
fn read_source(path: &Path) -> Result<String, NodeError> {
    if let Some(source) = PREFETCHED.with_borrow(|sources| sources.get(path).cloned()) {
        return Ok(source);
    }
    if let Some(error) =
        crate::errors::blocking_not_allowed(&format!("Loading '{}'", path.display()))
    {
        return Err(error);
    }
    read_file(path)
}

/// The content of the file at `path`, with a byte order mark and a `#!` line removed.
fn read_file(path: &Path) -> Result<String, NodeError> {
    let mut source = std::fs::read_to_string(path).map_err(|error| {
        NodeError::new(
            ErrorKind::Error,
            crate::errors::io_error_code(&error),
            format!("{error}, open '{}'", path.display()),
        )
    })?;
    if let Some(stripped) = source.strip_prefix('\u{feff}') {
        source = stripped.to_string();
    }
    if source.starts_with("#!") {
        // Keep the line, so line numbers stay right.
        source.replace_range(..2, "//");
    }
    Ok(source)
}

/// The specifiers of the `require` calls in `source` whose argument is a string literal without
/// substitutions. Calls in comments and strings count too.
fn required_specifiers(source: &str) -> Vec<&str> {
    let bytes = source.as_bytes();
    let mut specifiers = Vec::new();
    let mut from = 0;
    while let Some(offset) = source[from..].find("require") {
        let start = from + offset;
        from = start + "require".len();
        let preceded_by_name = start > 0
            && matches!(bytes[start - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$');
        if preceded_by_name {
            continue;
        }
        let rest = source[from..].trim_start();
        let Some(rest) = rest.strip_prefix('(') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(quote) = rest
            .chars()
            .next()
            .filter(|c| matches!(c, '\'' | '"' | '`'))
        else {
            continue;
        };
        let Some(end) = rest[1..].find(quote) else {
            continue;
        };
        let specifier = &rest[1..1 + end];
        let closed = rest[1 + end + 1..].trim_start().starts_with(')');
        if closed && !specifier.contains(['\\', '\n']) && !specifier.contains("${") {
            specifiers.push(specifier);
        }
    }
    specifiers
}

/// Resolve and read the CommonJS modules and JSON files `source`, the source of a module in
/// `dir`, `require`s with string literals, and the modules their sources `require` in turn, for
/// [`read_source`] and [`resolve`] to use. This only happens while blocking is allowed in a
/// script that doesn't run as a command. Specifiers that fail to resolve or read are skipped.
fn prefetch_requires(dir: &Path, source: &str) {
    if core_runtime::event_loop::running_as_command()
        || core_runtime::event_loop::blocking_host_io_refusal().is_some()
    {
        return;
    }
    let mut pending: Vec<(PathBuf, String)> = required_specifiers(source)
        .into_iter()
        .map(|specifier| (dir.to_path_buf(), specifier.to_string()))
        .collect();
    while let Some((dir, specifier)) = pending.pop() {
        if builtin_module(&specifier).is_some() {
            continue;
        }
        let key = (dir, specifier);
        if RESOLVED.with_borrow(|resolved| resolved.contains_key(&key)) {
            continue;
        }
        let Ok((path, format)) = resolve_now(&key.1, &key.0) else {
            continue;
        };
        RESOLVED.with_borrow_mut(|resolved| resolved.insert(key, (path.clone(), format)));
        let readable = matches!(
            format,
            SourceFormat::CommonJs | SourceFormat::Ambiguous | SourceFormat::Json
        );
        if !readable || PREFETCHED.with_borrow(|sources| sources.contains_key(&path)) {
            continue;
        }
        let Ok(source) = read_file(&path) else {
            continue;
        };
        if format != SourceFormat::Json {
            let dir = path
                .parent()
                .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
            pending.extend(
                required_specifiers(&source)
                    .into_iter()
                    .map(|specifier| (dir.clone(), specifier.to_string())),
            );
        }
        PREFETCHED.with_borrow_mut(|sources| sources.insert(path, source));
    }
}

/// The path and format `specifier` resolves to from `dir`: the resolution
/// [`prefetch_requires`] found, or the result of resolving it now.
///
/// Resolving reads the file system, which blocks, so it throws `ERR_BLOCKING_NOT_ALLOWED` where
/// blocking isn't allowed.
fn resolve(specifier: &str, dir: &Path) -> Result<(PathBuf, SourceFormat), NodeError> {
    let key = (dir.to_path_buf(), specifier.to_string());
    let resolution = match RESOLVED.with_borrow(|resolved| resolved.get(&key).cloned()) {
        Some(resolution) => resolution,
        None => {
            if let Some(error) =
                crate::errors::blocking_not_allowed(&format!("Resolving '{specifier}'"))
            {
                return Err(error);
            }
            resolve_now(specifier, dir).map_err(resolve_failed)?
        }
    };
    FORMATS.with_borrow_mut(|formats| formats.insert(resolution.0.clone(), resolution.1));
    Ok(resolution)
}

/// The path and format `specifier` resolves to from `dir`: the `node_modules` lookup of
/// `resolve_require`, and for a bare specifier it doesn't find, the file or directory it names in
/// each of the [`global_paths`] in turn, as Node.js searches `Module.globalPaths`.
fn resolve_now(specifier: &str, dir: &Path) -> Result<(PathBuf, SourceFormat), ResolveError> {
    let error = match resolve_require(specifier, dir) {
        Ok(resolution) => return Ok(resolution),
        Err(error) => error,
    };
    let bare = !specifier.starts_with('.') && !Path::new(specifier).is_absolute();
    if !bare || error.kind != ResolveErrorKind::NotFound {
        return Err(error);
    }
    global_paths()
        .iter()
        .find_map(|global| resolve_require(&format!("./{specifier}"), global).ok())
        .ok_or(error)
}

/// The format of the file at `path`: the one [`resolve`] or [`load_for_import`] found for it, or
/// the one resolving `path` finds now, or CommonJS if that fails.
fn format_of(path: &Path) -> SourceFormat {
    if let Some(format) = FORMATS.with_borrow(|formats| formats.get(path).copied()) {
        return format;
    }
    let dir = path.parent().unwrap_or(Path::new("/"));
    resolve(&path.to_string_lossy(), dir).map_or(SourceFormat::CommonJs, |(_, format)| format)
}

/// What loading a module file produced.
enum Loaded<'s> {
    /// The module's `module.exports`.
    Exports(HandleValue<'s>),
    /// The file is an ES module, which must be loaded as one. Nothing was cached for it.
    EsModule,
}

/// Load the module file at `path` for a `require` from `parent`, or for an `import` without one,
/// as Node.js's `Module._load` does after resolving: a module loaded before comes from
/// `require.cache`, and otherwise a new `Module` is cached and its `load` method runs the file. An
/// error removes the module from the cache.
fn load_file<'s>(
    scope: &'s Scope<'_>,
    path: &Path,
    parent: Option<Object<'_>>,
) -> Result<Loaded<'s>, Thrown> {
    let cache = module_cache(scope)?;
    let key = path.to_string_lossy().into_owned();
    let c_key =
        std::ffi::CString::new(key.as_str()).map_err(|_| module_not_found(&key, "invalid path"))?;
    let cached = cache.get_property_by_name(scope, &c_key)?;
    if let Ok(cached) = Object::from_value(scope, cached) {
        if let Some(parent) = parent {
            add_child(scope, parent, cached)?;
        }
        return Ok(Loaded::Exports(cached.get_property(scope, c"exports")?));
    }

    let ctor = module_class::class_constructor(scope)?;
    let parent_value = match parent {
        Some(parent) => parent.to_jsval_throwing(scope)?,
        None => scope.root_value(js::value::undefined()),
    };
    let module = Function::construct(
        scope,
        ctor,
        &[key.as_str().to_jsval_throwing(scope)?, parent_value],
    )?;
    cache.set_property_by_name(scope, &c_key, module)?;
    if js::class::realm_object::<MainModule>(scope).is_none()
        && core_runtime::module::entry_path().as_deref() == Some(key.as_str())
    {
        js::class::register_realm_object::<MainModule>(scope, module);
        module.set_property(scope, c"id", ".")?;
    }

    let loaded = Function::call_by_name(scope, module.handle(), c"load", &[key.as_str()]);
    match loaded {
        Ok(_) if is_es_module(scope, module)? => {
            cache.delete_property_by_name(scope, &c_key)?;
            if let Some(parent) = parent {
                remove_child(scope, parent, module)?;
            }
            FORMATS.with_borrow_mut(|formats| {
                formats.insert(path.to_path_buf(), SourceFormat::Module)
            });
            Ok(Loaded::EsModule)
        }
        Ok(_) => Ok(Loaded::Exports(module.get_property(scope, c"exports")?)),
        Err(ExnThrown) => {
            // The exception stays pending while the entry is removed.
            let exception = js::exception::take_pending(scope);
            let _ = cache.delete_property_by_name(scope, &c_key);
            if let Some(exception) = exception {
                js::exception::throw_value(scope, exception);
            }
            Err(Thrown::Pending)
        }
    }
}

/// Append `child` to `parent.children` unless it is in it, as Node.js's `updateChildren` does for
/// a module that comes from `require.cache`.
fn add_child(scope: &Scope<'_>, parent: Object<'_>, child: Object<'_>) -> Result<(), ExnThrown> {
    let children = parent.get_property(scope, c"children")?;
    let Some(children) = crate::support::as_array(scope, children) else {
        return Ok(());
    };
    let child = child.to_jsval_throwing(scope)?;
    if children.index_of(scope, child)?.is_none() {
        children.push(scope, child)?;
    }
    Ok(())
}

/// Remove `child` from `parent.children`, where the `Module` constructor added it.
fn remove_child(scope: &Scope<'_>, parent: Object<'_>, child: Object<'_>) -> Result<(), ExnThrown> {
    let children = parent.get_property(scope, c"children")?;
    let Some(children) = crate::support::as_array(scope, children) else {
        return Ok(());
    };
    if let Some(index) = children.index_of(scope, child.to_jsval_throwing(scope)?)? {
        children.remove(scope, index, 1)?;
    }
    Ok(())
}

/// The state of `module`, if it is a `Module` or holds the state of one.
fn module_state<'s>(
    scope: &'s Scope<'_>,
    module: Object<'s>,
) -> Result<Option<ModuleState<'s>>, ExnThrown> {
    js::class::find_instance_state::<ModuleState>(scope, module, false)
}

/// Whether running the file of `module` found an ES module, which didn't run.
fn is_es_module<'s>(scope: &'s Scope<'_>, module: Object<'s>) -> Result<bool, ExnThrown> {
    Ok(module_state(scope, module)?.is_some_and(|state| state.data().es_module))
}

/// Run `source`, the CommonJS source of the file at `path`, as `module`. With `ambiguous`, a
/// `SyntaxError` from compiling it marks `module` as an ES module instead, and nothing runs.
fn run_source<'s>(
    scope: &'s Scope<'_>,
    module: Object<'s>,
    source: &str,
    path: &Path,
    ambiguous: bool,
) -> Result<(), Thrown> {
    let filename = path.to_string_lossy();
    let dirname = path
        .parent()
        .map_or_else(|| "/".into(), |dir| dir.to_string_lossy().into_owned());
    let exports = module.get_property(scope, c"exports")?;
    let loaded = run_wrapped(
        scope, module, source, &filename, &dirname, exports, ambiguous,
    )?;
    if let Loaded::EsModule = loaded {
        if let Some(state) = module_state(scope, module)? {
            state.data_mut().es_module = true;
        }
    }
    Ok(())
}

/// Run `source` as the body of the CommonJS wrapper function of `module`, called with `this` and
/// the `__filename` and `__dirname` given. With `ambiguous`, a `SyntaxError` from compiling the
/// body makes it an ES module, and nothing runs.
fn run_wrapped<'s>(
    scope: &'s Scope<'_>,
    module: Object<'_>,
    source: &str,
    filename: &str,
    dirname: &str,
    this: HandleValue<'_>,
    ambiguous: bool,
) -> Result<Loaded<'s>, Thrown> {
    // The function's body starts a line after the header `compile_function` puts before it, so
    // starting at line 0 numbers the file's first line 1.
    let wrapper =
        match js::compile::compile_function(scope, c"", &WRAPPER_PARAMS, source, filename, 0) {
            Ok(wrapper) => wrapper,
            Err(ExnThrown) if ambiguous && js::exception::is_syntax_error_pending(scope) => {
                js::exception::clear(scope);
                return Ok(Loaded::EsModule);
            }
            Err(ExnThrown) => return Err(Thrown::Pending),
        };
    let exports = module.get_property(scope, c"exports")?;
    let require = make_require(scope, module)?;
    let args = [
        exports,
        require.to_jsval_throwing(scope)?,
        module.to_jsval_throwing(scope)?,
        filename.to_jsval_throwing(scope)?,
        dirname.to_jsval_throwing(scope)?,
    ];
    Function::call(scope, this, *wrapper, &args)?;
    Ok(Loaded::Exports(exports))
}

js::instance_local! {
    /// Whether the `-e` script came from `-p`, whose completion value is printed at exit.
    static PRINT_EVAL: Cell<bool> = const { Cell::new(false) };
}

/// Set whether the `-e` script came from `-p`.
pub(crate) fn set_print_eval(print: bool) {
    PRINT_EVAL.with(|cell| cell.set(print));
}

/// The [`core_runtime::module::CommonJsEval`]: run `-e` code as Node.js does. The code runs as a
/// classic script named `[eval]`, after `module`, `exports`, `require`, `__filename` (`[eval]`)
/// and `__dirname` (`.`) were set on the global object for the CommonJS module `[eval]`, whose
/// `filename` is `[eval]` in the working directory and whose `require` resolves from there. The
/// module isn't in `require.cache` and isn't `require.main`. With `-p`, the script's completion
/// value is logged when the process exits. Code that doesn't compile as a script, such as code
/// with ES module syntax or a top-level `await`, runs as an ES module instead, except with `-p`,
/// where code that compiles as an ES module throws `ERR_EVAL_ESM_CANNOT_PRINT`.
pub(crate) fn run_eval(scope: &Scope<'_>, source: &str) -> Result<bool, ExnThrown> {
    let script = match js::compile::compile_with_filename(scope, source, "[eval]", 1) {
        Ok(script) => script,
        Err(ExnThrown) if js::exception::is_syntax_error_pending(scope) => {
            js::exception::clear(scope);
            if PRINT_EVAL.with(Cell::get) && compiles_as_module(scope, source) {
                let message = "--print cannot be used with ESM input";
                let error = NodeError::new(ErrorKind::Error, "ERR_EVAL_ESM_CANNOT_PRINT", message);
                return Err(error.throw(scope));
            }
            return Ok(false);
        }
        Err(ExnThrown) => return Err(ExnThrown),
    };
    let set_globals = || -> Result<(), Thrown> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let ctor = module_class::class_constructor(scope)?;
        let undefined = scope.root_value(js::value::undefined());
        let module = Function::construct(
            scope,
            ctor,
            &["[eval]".to_jsval_throwing(scope)?, undefined],
        )?;
        module.set_property(
            scope,
            c"filename",
            cwd.join("[eval]").to_string_lossy().as_ref(),
        )?;
        set_node_module_paths(scope, module, &cwd)?;
        let global = scope.global();
        global.set_property(scope, c"module", module)?;
        global.set_property(scope, c"exports", module.get_property(scope, c"exports")?)?;
        global.set_property(scope, c"__dirname", ".")?;
        global.set_property(scope, c"require", make_require(scope, module)?)?;
        global.set_property(scope, c"__filename", "[eval]")?;
        Ok(())
    };
    set_globals().map_err(|thrown| thrown.throw(scope))?;
    let value = js::compile::execute_script(scope, script)?;
    if PRINT_EVAL.with(Cell::get) {
        let print = Function::new_callback(scope, c"", 0, print_value, value)?;
        let process = crate::process::global_process(scope)?;
        crate::events::listen(scope, process, c"on", c"exit", *print)?;
    }
    Ok(true)
}

/// Whether `source` compiles as an ES module. Nothing is evaluated.
fn compiles_as_module(scope: &Scope<'_>, source: &str) -> bool {
    let options = js::compile::options(scope, c"[eval]".into(), 1);
    let mut text = js::module_raw::transform_str_to_source_text(source);
    // SAFETY: `options` and `text` are live locals.
    let compiled = unsafe { js::module::compile_module(scope, options.ptr, &mut text) }.is_ok();
    js::exception::clear(scope);
    compiled
}

/// The `exit` listener `-p` adds, whose payload is the script's completion value: log it as
/// `console.log` does.
fn print_value(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    value: HandleValue,
) -> Result<Value, ExnThrown> {
    if let Some(console) = crate::console::console_object(scope) {
        Function::call_by_name(scope, console.handle(), c"log", &[value])?;
    }
    Ok(js::value::undefined())
}

/// The value `require` returns for an ES module with namespace `namespace`: its export named
/// `"module.exports"` if it has one, and the namespace otherwise.
fn esm_exports<'s>(
    scope: &'s Scope<'_>,
    namespace: Object<'s>,
) -> Result<HandleValue<'s>, ExnThrown> {
    if namespace.has_own_property(scope, c"module.exports")? {
        return namespace.get_property(scope, c"module.exports");
    }
    namespace.to_jsval_throwing(scope)
}

/// The path a `require` of `request` from `parent`, a module or `undefined`, resolves to, or
/// `request` itself if it names a builtin module, as Node.js's `Module._resolveFilename` returns
/// it.
fn resolve_filename(
    scope: &Scope<'_>,
    request: &str,
    parent: HandleValue<'_>,
) -> Result<String, Thrown> {
    if builtin_module(request).is_some() {
        return Ok(request.to_string());
    }
    let base = match Object::from_value(scope, *parent) {
        Ok(parent) => module_dir(scope, parent)?,
        Err(_) => PathBuf::from(crate::process::cwd()),
    };
    let (path, _) = resolve(request, &base)?;
    Ok(path.to_string_lossy().into_owned())
}

/// Node.js's `Module._load(request, parent, isMain)`: the exports of the builtin module a
/// `node:` `request` names, or else of the module that `Module._resolveFilename` resolves
/// `request` to, loaded with [`load_file`]. An ES module is loaded and evaluated synchronously.
fn load_request<'s>(
    scope: &'s Scope<'_>,
    request: HandleValue<'_>,
    parent: HandleValue<'_>,
    is_main: HandleValue<'_>,
) -> Result<HandleValue<'s>, Thrown> {
    let specifier = validate_string(scope, request, "request")?;
    if specifier.starts_with("node:") {
        let Some(namespace) = core_runtime::module::builtin_namespace(scope, &specifier)? else {
            return Err(NodeError::new(
                ErrorKind::Error,
                "ERR_UNKNOWN_BUILTIN_MODULE",
                format!("No such built-in module: {specifier}"),
            )
            .into());
        };
        return Ok(namespace.get_property(scope, c"default")?);
    }
    let ctor = module_class::class_constructor(scope)?;
    let filename = Function::call_by_name(
        scope,
        ctor.handle(),
        c"_resolveFilename",
        &[request, parent, is_main],
    )?;
    let filename = validate_string(scope, filename, "filename")?;
    if let Some(namespace) = core_runtime::module::builtin_namespace(scope, &filename)? {
        return Ok(namespace.get_property(scope, c"default")?);
    }
    let path = Path::new(&filename);
    let parent = Object::from_value(scope, *parent).ok();
    match load_file(scope, path, parent)? {
        Loaded::Exports(exports) => Ok(exports),
        Loaded::EsModule => {
            let namespace = core_runtime::module::require_module(scope, path)?;
            Ok(esm_exports(scope, namespace)?)
        }
    }
}

/// The directory `module`'s relative `require`s resolve against: the directory of its
/// `filename`, or of its `id` if it has no filename, or the current working directory.
fn module_dir(scope: &Scope<'_>, module: Object<'_>) -> Result<PathBuf, ExnThrown> {
    for name in [c"filename", c"id"] {
        let value = module.get_property(scope, name)?;
        if let Ok(path) = String::from_jsval(scope, value, ()) {
            if Path::new(&path).is_absolute() {
                return Ok(Path::new(&path)
                    .parent()
                    .map_or_else(|| PathBuf::from("/"), Path::to_path_buf));
            }
        }
    }
    Ok(PathBuf::from(crate::process::cwd()))
}

/// The body of a `require` function, whose payload is the requiring module: call the module's
/// `require` method with the argument.
fn require_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let parent = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    Ok(Function::call_by_name(scope, parent.handle(), c"require", &[args.get(0)])?.get())
}

/// `require.resolve(request, options)`: `Module._resolveFilename(request, module, false,
/// options)` for the requiring module, the payload.
fn resolve_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    validate_string(scope, args.get(0), "request").map_err(|thrown| thrown.throw(scope))?;
    let ctor = module_class::class_constructor(scope)?;
    let is_main = scope.root_value(js::value::from_bool(false));
    let resolved = Function::call_by_name(
        scope,
        ctor.handle(),
        c"_resolveFilename",
        &[args.get(0), payload, is_main, args.get(1)],
    )?;
    Ok(resolved.get())
}

/// The key of the realm's main module: the CommonJS module of the entry script, if it is one.
struct MainModule;

/// The `require` function of `module`, with `require.resolve`, `require.cache` and
/// `require.main`, the main module or `undefined`.
fn make_require<'s>(scope: &'s Scope<'_>, module: Object<'s>) -> Result<Object<'s>, ExnThrown> {
    let require = Function::new_callback(scope, c"require", 1, require_callback, module)?;
    let resolve = Function::new_callback(scope, c"resolve", 1, resolve_callback, module)?;
    require.set_property(scope, c"resolve", *resolve)?;
    require.set_property(scope, c"cache", module_cache(scope)?)?;
    match js::class::realm_object::<MainModule>(scope) {
        Some(main) => require.set_property(scope, c"main", main)?,
        None => require.set_property(scope, c"main", js::value::undefined())?,
    }
    Ok(*require)
}

/// Set `module.paths` to `Module._nodeModulePaths(dir)`, called through the property.
fn set_node_module_paths(
    scope: &Scope<'_>,
    module: Object<'_>,
    dir: &Path,
) -> Result<(), ExnThrown> {
    let ctor = module_class::class_constructor(scope)?;
    let dir = dir.to_string_lossy();
    let paths = Function::call_by_name(scope, ctor.handle(), c"_nodeModulePaths", &[dir.as_ref()])?;
    module.set_property(scope, c"paths", paths)
}

/// Node.js's `Module._nodeModulePaths(from)`: the `node_modules` directory of `from`, resolved to
/// an absolute path, and of each directory above it, except of directories named `node_modules`,
/// ending with `/node_modules`.
fn node_module_paths(from: &str) -> Vec<String> {
    const NODE_MODULES: &[u8] = b"node_modules";
    let from = crate::path::resolve_paths(&[from.to_string()]);
    if from == "/" {
        return vec!["/node_modules".to_string()];
    }
    let mut paths = Vec::new();
    let mut last = from.len();
    // How many characters before the last `/` matched the end of `node_modules`, or `None` once
    // one didn't.
    let mut matched = Some(0);
    for (index, &byte) in from.as_bytes().iter().enumerate().rev() {
        if byte == b'/' {
            if matched != Some(NODE_MODULES.len()) {
                paths.push(format!("{}/node_modules", &from[..last]));
            }
            last = index;
            matched = Some(0);
        } else if let Some(count) = matched {
            matched = (count < NODE_MODULES.len()
                && NODE_MODULES[NODE_MODULES.len() - 1 - count] == byte)
                .then_some(count + 1);
        }
    }
    paths.push("/node_modules".to_string());
    paths
}

/// The state of a `Module`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct ModuleState {
    /// Node.js's `kModuleParent`, which `Module.prototype.parent` reads and writes.
    parent: Heap<Value>,
    /// Whether the next `_compile` runs the source of a file that is an ES module if it doesn't
    /// compile as CommonJS.
    #[no_trace]
    ambiguous: bool,
    /// Whether the module's file turned out to be an ES module, which didn't run.
    #[no_trace]
    es_module: bool,
}

#[core_runtime::jsmethods]
impl ModuleState {}

/// The getter of `Module.prototype.parent`: the parent in the state `this` holds itself, not
/// through its prototype chain, or `undefined` if it holds none.
fn get_parent(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = crate::support::this_object(scope, &args)?;
    let state = js::class::find_instance_state::<ModuleState>(scope, this, false)?;
    Ok(state.map_or_else(js::value::undefined, |state| {
        state.data().parent.get(scope).get()
    }))
}

/// The setter of `Module.prototype.parent`, which gives `this` a state of its own if it holds
/// none.
fn set_parent(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = crate::support::this_object(scope, &args)?;
    module_class::__ensure_state(scope, this)?;
    if let Some(state) = js::class::find_instance_state::<ModuleState>(scope, this, false)? {
        state.data().parent.set(args.get(0).get());
    }
    Ok(js::value::undefined())
}

/// The path a `createRequire` argument names: an absolute path, a `file:` URL string, or a URL
/// object with a `file:` `href`. A path ending in `/` names a directory, and `noop.js` in it is
/// returned.
fn require_base_path(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<String, Thrown> {
    let invalid = || {
        NodeError::invalid_arg_value(
            scope,
            "filename",
            value,
            "must be a file URL object, file URL string, or absolute path string",
        )
    };
    let url = if value.is_string() {
        let filename = String::from_jsval(scope, value, ()).map_err(|_| invalid())?;
        if filename.starts_with('/') {
            return Ok(directory_noop(filename));
        }
        filename
    } else if let Ok(object) = Object::from_value(scope, value) {
        let href = object.get_property(scope, c"href")?;
        if !href.is_string() {
            return Err(invalid().into());
        }
        String::from_jsval(scope, href, ()).map_err(|_| invalid())?
    } else {
        return Err(invalid().into());
    };
    let path = url::Url::parse(&url)
        .ok()
        .filter(|url| url.scheme() == "file")
        .and_then(|url| url.to_file_path().ok())
        .map(|path| path.to_string_lossy().into_owned())
        .ok_or_else(invalid)?;
    let path = if url.ends_with('/') && !path.ends_with('/') {
        format!("{path}/")
    } else {
        path
    };
    Ok(directory_noop(path))
}

/// `path`, or `noop.js` in it if it ends in `/`.
fn directory_noop(path: String) -> String {
    if path.ends_with('/') {
        format!("{path}noop.js")
    } else {
        path
    }
}

/// The [`core_runtime::module::CommonJsLoader`]: run the CommonJS module at `path` for an
/// `import`, or return `None` if it is an `Ambiguous` file with ES module syntax. An `Ambiguous`
/// file that an earlier load found to be an ES module isn't compiled again.
pub(crate) fn load_for_import<'s>(
    scope: &'s Scope<'_>,
    path: &Path,
    format: SourceFormat,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    let known = FORMATS.with_borrow(|formats| formats.get(path).copied());
    if format == SourceFormat::Ambiguous && known == Some(SourceFormat::Module) {
        return Ok(None);
    }
    FORMATS.with_borrow_mut(|formats| formats.insert(path.to_path_buf(), format));
    match load_file(scope, path, None) {
        Ok(Loaded::Exports(exports)) => Ok(Some(exports)),
        Ok(Loaded::EsModule) => Ok(None),
        Err(error) => Err(error.throw(scope)),
    }
}

/// The `Module._extensions` loader for `.js` files, `(module, filename)`: mark `module` as an ES
/// module for an ES module file, or else read the file and run it with `module._compile`. A file
/// whose format is neither JavaScript nor JSON can't be loaded.
fn load_js_extension(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let run = || -> Result<(), Thrown> {
        let module = Object::from_value(scope, args.get(0))?;
        let filename = validate_string(scope, args.get(1), "filename")?;
        let path = Path::new(&filename);
        let format = format_of(path);
        match format {
            SourceFormat::Json => return load_json(scope, module, path),
            SourceFormat::Other => {
                return Err(NodeError::new(
                    ErrorKind::Error,
                    "ERR_DLOPEN_FAILED",
                    format!(
                        "Native addons and other non-JavaScript modules can't be loaded: \
                         {filename}"
                    ),
                )
                .into())
            }
            SourceFormat::Module => {
                if let Some(state) = module_state(scope, module)? {
                    state.data_mut().es_module = true;
                }
                return Ok(());
            }
            SourceFormat::CommonJs | SourceFormat::Ambiguous => {}
        }
        let source = read_source(path)?;
        prefetch_requires(path.parent().unwrap_or(Path::new("/")), &source);
        if let Some(state) = module_state(scope, module)? {
            state.data_mut().ambiguous = format == SourceFormat::Ambiguous;
        }
        Function::call_by_name(
            scope,
            module.handle(),
            c"_compile",
            &[source.as_str(), filename.as_str()],
        )?;
        Ok(())
    };
    run().map_err(|thrown| thrown.throw(scope))?;
    Ok(js::value::undefined())
}

/// Set `module.exports` to the value of the JSON file at `path`.
fn load_json(scope: &Scope<'_>, module: Object<'_>, path: &Path) -> Result<(), Thrown> {
    let source = read_source(path)?;
    let value = js::json::parse(scope, &source)?;
    module.set_property(scope, c"exports", value)?;
    Ok(())
}

/// The `Module._extensions` loader for `.json` files, `(module, filename)`: set `module.exports`
/// to the file's value.
fn load_json_extension(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let run = || -> Result<(), Thrown> {
        let module = Object::from_value(scope, args.get(0))?;
        let filename = validate_string(scope, args.get(1), "filename")?;
        load_json(scope, module, Path::new(&filename))
    };
    run().map_err(|thrown| thrown.throw(scope))?;
    Ok(js::value::undefined())
}

/// The `Module._extensions` loader for `.node` files: native addons can't be loaded.
fn load_node_extension(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let filename = crate::support::rust_string(scope, args.get(1)).unwrap_or_default();
    Err(NodeError::new(
        ErrorKind::Error,
        "ERR_DLOPEN_FAILED",
        format!("Native addons can't be loaded: {filename}"),
    )
    .throw(scope))
}

/// Node.js's `findLongestRegisteredExtension`: the longest extension of the base name of
/// `filename`, after a dot that doesn't start it, that `extensions` has a truthy loader for, or
/// `.js`.
fn registered_extension(
    scope: &Scope<'_>,
    extensions: Object<'_>,
    filename: &str,
) -> Result<String, Thrown> {
    let name = Path::new(filename)
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    for (index, _) in name.match_indices('.').filter(|(index, _)| *index > 0) {
        let extension = &name[index..];
        let Ok(key) = std::ffi::CString::new(extension) else {
            continue;
        };
        let loader = extensions.get_property_by_name(scope, &key)?;
        if js::conversion::to_boolean(loader) {
            return Ok(extension.to_string());
        }
    }
    Ok(".js".to_string())
}

/// `Module.globalPaths`: the non-empty entries of the `:`-separated `NODE_PATH` environment
/// variable, `.node_modules` and `.node_libraries` in the home directory, then `lib/node` in the
/// installation prefix, the directory above the executable's.
fn global_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::env::var_os("NODE_PATH")
        .map(|node_path| {
            std::env::split_paths(&node_path)
                .filter(|path| !path.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        let home = PathBuf::from(home);
        paths.push(home.join(".node_modules"));
        paths.push(home.join(".node_libraries"));
    }
    if let Some(prefix) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().and_then(Path::parent).map(Path::to_path_buf))
    {
        paths.push(prefix.join("lib/node"));
    }
    paths
}

#[core_runtime::jsfunction_class(name = "Module", state = "ModuleState")]
pub mod module_class {
    use super::*;

    /// <https://nodejs.org/api/modules.html#the-module-object>: a module with ID `id`, required
    /// by `parent`, whose `exports` is a new object.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        id: Option<HandleValue<'_>>,
        parent: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let id = match id.filter(|id| id.is_string()) {
            Some(id) => String::from_jsval(scope, id, ()).unwrap_or_default(),
            None => String::new(),
        };
        let path = crate::path::dirname_of(&id).to_string();
        this.set_property(scope, c"id", id.as_str())?;
        this.set_property(scope, c"path", path.as_str())?;
        this.set_property(scope, c"exports", Object::new_plain(scope)?)?;
        let parent = parent.unwrap_or(HandleValue::undefined());
        state_of(scope, this)?.data().parent.set(parent.get());
        if let Ok(parent) = Object::from_value(scope, *parent) {
            let siblings = parent.get_property(scope, c"children")?;
            let siblings = Object::from_value(scope, *siblings)
                .ok()
                .and_then(|siblings| siblings.cast::<js::Array>().ok());
            if let Some(siblings) = siblings {
                siblings.push(scope, this)?;
            }
        }
        this.set_property(scope, c"filename", js::value::null())?;
        this.set_property(scope, c"loaded", false)?;
        let children = js::Array::with_contents(scope, &[] as &[Value])?;
        this.set_property(scope, c"children", children)?;
        Ok(())
    }

    /// <https://nodejs.org/api/module.html#modulecreaterequirefilename>
    #[static_method]
    pub fn create_require<'s>(
        scope: &'s Scope<'_>,
        filename: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        let path = require_base_path(scope, filename)?;
        let ctor = class_constructor(scope)?;
        let module = Function::construct(scope, ctor, &[path.as_str()])?;
        module.set_property(scope, c"filename", path.as_str())?;
        let dir = Path::new(&path).parent().unwrap_or(Path::new("/"));
        set_node_module_paths(scope, module, dir)?;
        Ok(make_require(scope, module)?)
    }

    /// Node.js's `Module._nodeModulePaths(from)`, which [`set_node_module_paths`] calls.
    #[static_method(name = "_nodeModulePaths", length = 1)]
    pub fn node_module_paths_static<'s>(
        scope: &'s Scope<'_>,
        from: HandleValue<'_>,
    ) -> Result<js::Array<'s>, Thrown> {
        let from = validate_string(scope, from, "paths[0]")?;
        let paths = node_module_paths(&from);
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        Ok(js::Array::with_contents(scope, &paths)?)
    }

    /// <https://nodejs.org/api/module.html#moduleisbuiltinmodulename>
    #[static_method]
    pub fn is_builtin(scope: &Scope<'_>, name: HandleValue<'_>) -> bool {
        String::from_jsval(scope, name, ()).is_ok_and(|name| builtin_module(&name).is_some())
    }

    /// <https://nodejs.org/api/module.html#modulesyncbuiltinesmexports>: builtin modules'
    /// named exports aren't synchronized with their `default` exports, so this does nothing.
    #[static_method(name = "syncBuiltinESMExports")]
    pub fn sync_builtin_esm_exports() {}

    /// Node.js's `Module._load(request, parent, isMain)`, which `module.require` calls: the
    /// exports of the module `request` names, loaded for `parent`. `isMain` is passed on to
    /// `Module._resolveFilename` and has no other effect.
    #[static_method(name = "_load", length = 3)]
    pub fn load_static<'s>(
        scope: &'s Scope<'_>,
        request: HandleValue<'_>,
        parent: Option<HandleValue<'_>>,
        is_main: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let parent = parent.unwrap_or(HandleValue::undefined());
        let is_main = is_main.unwrap_or(HandleValue::undefined());
        load_request(scope, request, parent, is_main)
    }

    /// Node.js's `Module._resolveFilename(request, parent, isMain, options)`, which `Module._load`
    /// and `require.resolve` call: `request` if it names a builtin module, and otherwise the path
    /// it resolves to from `parent`'s directory, or from the working directory without a parent.
    /// `isMain` and `options`, whose `paths` Node.js searches instead, have no effect.
    #[static_method(name = "_resolveFilename", length = 4)]
    pub fn resolve_filename_static<'s>(
        scope: &'s Scope<'_>,
        request: HandleValue<'_>,
        parent: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let request = validate_string(scope, request, "request")?;
        let parent = parent.unwrap_or(HandleValue::undefined());
        let filename = resolve_filename(scope, &request, parent)?;
        Ok(filename.as_str().to_jsval_throwing(scope)?)
    }

    /// `module.require(id)`: `Module._load(id, this, false)`.
    pub fn require<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        id: HandleValue<'_>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let id_string = validate_string(scope, id, "id")?;
        if id_string.is_empty() {
            return Err(NodeError::invalid_arg_value(
                scope,
                "id",
                id,
                "must be a non-empty string",
            )
            .into());
        }
        let ctor = class_constructor(scope)?;
        let args = [
            scope.root_value(id.get()),
            scope.root_value(this.to_jsval_throwing(scope)?.get()),
            scope.root_value(js::value::from_bool(false)),
        ];
        Ok(Function::call_by_name(
            scope,
            ctor.handle(),
            c"_load",
            &args,
        )?)
    }

    /// `module.load(filename)`: set this module's `filename` and `paths`, run the file at
    /// `filename` as this module with the `Module._extensions` loader of its longest registered
    /// extension, or the `.js` loader, and set `loaded`.
    pub fn load(
        scope: &Scope<'_>,
        this: Object<'_>,
        filename: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let filename = validate_string(scope, filename, "filename")?;
        this.set_property(scope, c"filename", filename.as_str())?;
        let dir = Path::new(&filename).parent().unwrap_or(Path::new("/"));
        set_node_module_paths(scope, this, dir)?;
        let extensions = class_constructor(scope)?.get_property(scope, c"_extensions")?;
        let extensions = Object::from_value(scope, extensions)?;
        let extension = registered_extension(scope, extensions, &filename)?;
        let key = std::ffi::CString::new(extension).map_err(|_| ExnThrown)?;
        let loader = extensions.get_property_by_name(scope, &key)?;
        Function::call(
            scope,
            extensions,
            loader,
            &[
                this.to_jsval_throwing(scope)?,
                filename.as_str().to_jsval_throwing(scope)?,
            ],
        )?;
        this.set_property(scope, c"loaded", true)?;
        Ok(())
    }

    /// `module._compile(content, filename)`: run `content` as the CommonJS source of this module,
    /// whose file is `filename`. When the `.js` loader calls it for a file without a declared
    /// format, source that doesn't compile as CommonJS marks the module as an ES module instead.
    #[method(name = "_compile", length = 3)]
    pub fn compile(
        scope: &Scope<'_>,
        this: Object<'_>,
        content: HandleValue<'_>,
        filename: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let content = validate_string(scope, content, "content")?;
        let filename = validate_string(scope, filename, "filename")?;
        let state = state_of(scope, this)?;
        let ambiguous = std::mem::take(&mut state.data_mut().ambiguous);
        run_source(scope, this, &content, Path::new(&filename), ambiguous)
    }

    /// The static members that aren't functions: `_cache`, `_extensions`, `builtinModules`,
    /// `constants`, `globalPaths` and `Module`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let getter = Function::new_callback(scope, c"get parent", 0, get_parent, ())?;
        let setter = Function::new_callback(scope, c"set parent", 1, set_parent, ())?;
        proto.define_accessor(scope, c"parent", Some(*getter), Some(*setter), 0)?;
        let extensions = Object::new_plain(scope)?;
        extensions.set_prototype(scope, js::native::HandleObject::null())?;
        let loaders: [(&std::ffi::CStr, js::function::Callback); 3] = [
            (c".js", load_js_extension),
            (c".json", load_json_extension),
            (c".node", load_node_extension),
        ];
        for (name, loader) in loaders {
            let loader = Function::new_callback(scope, name, 2, loader, ())?;
            extensions.set_property(scope, name, *loader)?;
        }
        ctor.set_property(scope, c"_extensions", extensions)?;
        let status = Object::new_plain(scope)?;
        for (index, name) in [c"FAILED", c"ENABLED", c"ALREADY_ENABLED", c"DISABLED"]
            .into_iter()
            .enumerate()
        {
            status.set_property(scope, name, index as f64)?;
        }
        let constants = Object::new_plain(scope)?;
        constants.set_property(scope, c"compileCacheStatus", status)?;
        ctor.set_property(scope, c"constants", constants)?;
        let paths: Vec<String> = global_paths()
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        ctor.set_property(
            scope,
            c"globalPaths",
            js::Array::with_contents(scope, &paths)?,
        )?;
        let cache = Object::new_plain(scope)?;
        cache.set_prototype(scope, js::native::HandleObject::null())?;
        ctor.set_property(scope, c"_cache", cache)?;
        let names: Vec<&str> = crate::builtin_module_names().collect();
        ctor.set_property(
            scope,
            c"builtinModules",
            js::Array::with_contents(scope, &names)?,
        )?;
        ctor.set_property(scope, c"Module", ctor)?;
        Ok(())
    }
}

#[core_runtime::jsmodule(
    node_args,
    name = "node:module",
    default = "Module",
    mirror = "_cache, _extensions, _load, _nodeModulePaths, _resolveFilename, builtinModules, \
              constants, createRequire, globalPaths, isBuiltin, syncBuiltinESMExports"
)]
pub mod module {
    pub use super::module_class as Module;
}
