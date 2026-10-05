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
//! evaluates synchronously. `core_runtime::module` calls [`load_for_import`] for an ES module's
//! `import` of a CommonJS module.
//!
//! Reading a module file blocks, which isn't allowed while serving requests. So when a script
//! that serves requests loads a CommonJS module, we also resolve and read the CommonJS modules
//! and JSON files its source `require`s with a string literal, and theirs in turn. A later
//! `require` of one of them uses that resolution and source without host I/O.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use core_runtime::module::{builtin_module, resolve_require, ResolveError, SourceFormat};
use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
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
        let Ok((path, format)) = resolve_require(&key.1, &key.0) else {
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
fn resolve(specifier: &str, dir: &Path) -> Result<(PathBuf, SourceFormat), NodeError> {
    let key = (dir.to_path_buf(), specifier.to_string());
    if let Some(resolution) = RESOLVED.with_borrow(|resolved| resolved.get(&key).cloned()) {
        return Ok(resolution);
    }
    resolve_require(specifier, dir).map_err(resolve_failed)
}

/// What loading a module file produced.
enum Loaded<'s> {
    /// The module's `module.exports`.
    Exports(HandleValue<'s>),
    /// The file is an `Ambiguous` one with ES module syntax, which must be loaded as an ES
    /// module. Nothing was cached for it.
    EsModule,
}

/// Load the module file at `path`, of `format`, for a `require` from `parent`, or for an
/// `import` without one. A module loaded before comes from `require.cache`.
fn load_file<'s>(
    scope: &'s Scope<'_>,
    path: &Path,
    format: SourceFormat,
    parent: Option<Object<'_>>,
) -> Result<Loaded<'s>, Thrown> {
    let cache = module_cache(scope)?;
    let key = path.to_string_lossy().into_owned();
    let c_key =
        std::ffi::CString::new(key.as_str()).map_err(|_| module_not_found(&key, "invalid path"))?;
    let cached = cache.get_property_by_name(scope, &c_key)?;
    if let Ok(cached) = Object::from_value(scope, cached) {
        return Ok(Loaded::Exports(cached.get_property(scope, c"exports")?));
    }
    if format == SourceFormat::Other {
        return Err(NodeError::new(
            ErrorKind::Error,
            "ERR_DLOPEN_FAILED",
            format!("Native addons and other non-JavaScript modules can't be loaded: {key}"),
        )
        .into());
    }
    if format == SourceFormat::Module {
        let namespace = core_runtime::module::require_module(scope, path)?;
        return Ok(Loaded::Exports(esm_exports(scope, namespace)?));
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
    module.set_property(scope, c"filename", key.as_str())?;
    set_node_module_paths(scope, module, path.parent().unwrap_or(Path::new("/")))?;
    cache.set_property_by_name(scope, &c_key, module)?;
    if js::class::realm_object::<MainModule>(scope).is_none()
        && core_runtime::module::entry_path().as_deref() == Some(key.as_str())
    {
        js::class::register_realm_object::<MainModule>(scope, module);
    }

    let loaded = run_file(scope, module, path, format);
    match loaded {
        Ok(Loaded::Exports(_)) => {
            module.set_property(scope, c"loaded", true)?;
            Ok(Loaded::Exports(module.get_property(scope, c"exports")?))
        }
        Ok(Loaded::EsModule) => {
            cache.delete_property_by_name(scope, &c_key)?;
            Ok(Loaded::EsModule)
        }
        Err(error) => {
            // The exception stays pending while the entry is removed.
            let exception = js::exception::take_pending(scope);
            let _ = cache.delete_property_by_name(scope, &c_key);
            if let Some(exception) = exception {
                js::exception::throw_value(scope, exception);
            }
            Err(error)
        }
    }
}

/// Run the file at `path`, of `format`, as `module`.
fn run_file<'s>(
    scope: &'s Scope<'_>,
    module: Object<'_>,
    path: &Path,
    format: SourceFormat,
) -> Result<Loaded<'s>, Thrown> {
    let source = read_source(path)?;
    if format != SourceFormat::Json {
        prefetch_requires(path.parent().unwrap_or(Path::new("/")), &source);
    }
    run_source(scope, module, &source, path, format)
}

/// Run `source`, the source of the file at `path`, of `format`, as `module`.
fn run_source<'s>(
    scope: &'s Scope<'_>,
    module: Object<'_>,
    source: &str,
    path: &Path,
    format: SourceFormat,
) -> Result<Loaded<'s>, Thrown> {
    if format == SourceFormat::Json {
        let value = js::json::parse(scope, source)?;
        module.set_property(scope, c"exports", value)?;
        return Ok(Loaded::Exports(value));
    }
    let filename = path.to_string_lossy();
    // The function's body starts a line after the header `compile_function` puts before it, so
    // starting at line 0 numbers the file's first line 1.
    let wrapper =
        match js::compile::compile_function(scope, c"", &WRAPPER_PARAMS, source, &filename, 0) {
            Ok(wrapper) => wrapper,
            Err(ExnThrown)
                if format == SourceFormat::Ambiguous
                    && js::exception::is_syntax_error_pending(scope) =>
            {
                js::exception::clear(scope);
                return Ok(Loaded::EsModule);
            }
            Err(ExnThrown) => return Err(Thrown::Pending),
        };
    let exports = module.get_property(scope, c"exports")?;
    let require = make_require(scope, module)?;
    let dirname = path
        .parent()
        .map_or_else(|| "/".into(), |dir| dir.to_string_lossy().into_owned());
    let args = [
        exports,
        require.to_jsval_throwing(scope)?,
        module.to_jsval_throwing(scope)?,
        filename.as_ref().to_jsval_throwing(scope)?,
        dirname.as_str().to_jsval_throwing(scope)?,
    ];
    Function::call(scope, exports, *wrapper, &args)?;
    Ok(Loaded::Exports(exports))
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

/// The `require` of `parent`: resolve `specifier` from it and load the module.
fn require_from<'s>(
    scope: &'s Scope<'_>,
    parent: Object<'_>,
    specifier: &str,
) -> Result<HandleValue<'s>, Thrown> {
    if let Some(namespace) = core_runtime::module::builtin_namespace(scope, specifier)? {
        return Ok(namespace.get_property(scope, c"default")?);
    }
    let base = module_dir(scope, parent)?;
    let (path, format) = resolve(specifier, &base)?;
    match load_file(scope, &path, format, Some(parent))? {
        Loaded::Exports(exports) => Ok(exports),
        Loaded::EsModule => {
            let namespace = core_runtime::module::require_module(scope, &path)?;
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

/// The body of a `require` function, whose payload is the requiring module.
fn require_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let run = || -> Result<Value, Thrown> {
        let parent = Object::from_value(scope, payload)?;
        let specifier = validate_string(scope, args.get(0), "id")?;
        if specifier.is_empty() {
            return Err(NodeError::invalid_arg_value(
                scope,
                "id",
                args.get(0),
                "must be a non-empty string",
            )
            .into());
        }
        Ok(require_from(scope, parent, &specifier)?.get())
    };
    run().map_err(|error| error.throw(scope))
}

/// `require.resolve(request)`: the path `request` resolves to from the requiring module, the
/// payload, or the name of the builtin module it names.
fn resolve_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let run = || -> Result<Value, Thrown> {
        let parent = Object::from_value(scope, payload)?;
        let specifier = validate_string(scope, args.get(0), "request")?;
        if let Some(builtin) = builtin_module(&specifier) {
            let name = if specifier.starts_with("node:") {
                builtin.name
            } else {
                builtin.name.trim_start_matches("node:")
            };
            return Ok(name.to_jsval_raw_throwing(scope)?);
        }
        let base = module_dir(scope, parent)?;
        let (path, _) = resolve(&specifier, &base)?;
        Ok(path
            .to_string_lossy()
            .as_ref()
            .to_jsval_raw_throwing(scope)?)
    };
    run().map_err(|error| error.throw(scope))
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

/// Set `module.paths` to Node.js's `Module._nodeModulePaths(dir)`: the `node_modules` directory
/// of `dir` and of each directory above it, except in directories named `node_modules`.
fn set_node_module_paths(
    scope: &Scope<'_>,
    module: Object<'_>,
    dir: &Path,
) -> Result<(), ExnThrown> {
    let paths: Vec<String> = dir
        .ancestors()
        .filter(|dir| dir.file_name().is_none_or(|name| name != "node_modules"))
        .map(|dir| dir.join("node_modules").to_string_lossy().into_owned())
        .collect();
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    module.set_property(scope, c"paths", js::Array::with_contents(scope, &paths)?)
}

/// The key of the parent of a module, which its `parent` accessor reads, Node.js's
/// `kModuleParent`.
static MODULE_PARENT: js::symbol::CachedSymbol = js::symbol::CachedSymbol::new(c"kModuleParent");

/// The getter of `Module.prototype.parent`.
fn get_parent(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = crate::support::this_object(scope, &args)?;
    Ok(this
        .get_property_by_id(scope, MODULE_PARENT.id(scope)?)?
        .get())
}

/// The setter of `Module.prototype.parent`.
fn set_parent(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = crate::support::this_object(scope, &args)?;
    this.set_property_by_id(scope, MODULE_PARENT.id(scope)?, args.get(0))?;
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
/// `import`, or return `None` if it is an `Ambiguous` file with ES module syntax.
pub(crate) fn load_for_import<'s>(
    scope: &'s Scope<'_>,
    path: &Path,
    format: SourceFormat,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    match load_file(scope, path, format, None) {
        Ok(Loaded::Exports(exports)) => Ok(Some(exports)),
        Ok(Loaded::EsModule) => Ok(None),
        Err(error) => Err(error.throw(scope)),
    }
}

/// Run the file at `path` as `module`: as JSON for a `.json` file, and as CommonJS otherwise. A
/// `.node` file is a native addon, which can't be loaded.
fn run_by_extension(scope: &Scope<'_>, module: Object<'_>, path: &Path) -> Result<(), Thrown> {
    let format = match path.extension().and_then(|extension| extension.to_str()) {
        Some("json") => SourceFormat::Json,
        Some("node") => SourceFormat::Other,
        _ => SourceFormat::CommonJs,
    };
    if format == SourceFormat::Other {
        return Err(NodeError::new(
            ErrorKind::Error,
            "ERR_DLOPEN_FAILED",
            format!("Native addons can't be loaded: {}", path.display()),
        )
        .into());
    }
    run_file(scope, module, path, format)?;
    Ok(())
}

/// A `Module._extensions` loader, `(module, filename)`: run the file at `filename` as `module`
/// according to its extension.
fn load_extension(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let module = Object::from_value(scope, args.get(0)).map_err(|e| e.throw(scope))?;
    let filename =
        validate_string(scope, args.get(1), "filename").map_err(|thrown| thrown.throw(scope))?;
    run_by_extension(scope, module, Path::new(&filename)).map_err(|thrown| thrown.throw(scope))?;
    Ok(js::value::undefined())
}

/// `Module.globalPaths`: `.node_modules` and `.node_libraries` in the home directory, then
/// `lib/node` in the installation prefix, the directory above the executable's.
fn global_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
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

#[core_runtime::jsfunction_class(name = "Module")]
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
        this.set_property_by_id(scope, MODULE_PARENT.id(scope)?, parent)?;
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

    /// <https://nodejs.org/api/module.html#moduleisbuiltinmodulename>
    #[static_method]
    pub fn is_builtin(scope: &Scope<'_>, name: HandleValue<'_>) -> bool {
        String::from_jsval(scope, name, ()).is_ok_and(|name| builtin_module(&name).is_some())
    }

    /// <https://nodejs.org/api/module.html#modulesyncbuiltinesmexports>: builtin modules'
    /// named exports aren't synchronized with their `default` exports, so this does nothing.
    #[static_method(name = "syncBuiltinESMExports")]
    pub fn sync_builtin_esm_exports() {}

    /// `module.require(id)`: `require(id)` from this module.
    pub fn require<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        id: HandleValue<'_>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let id = validate_string(scope, id, "id")?;
        if id.is_empty() {
            let value = js::class::get_or_init_atom(scope, c"")?;
            return Err(NodeError::invalid_arg_value(
                scope,
                "id",
                value,
                "must be a non-empty string",
            )
            .into());
        }
        require_from(scope, this, &id)
    }

    /// `module.load(filename)`: set this module's `filename` and run the file at `filename` as
    /// it: as JSON for a `.json` file, and as CommonJS otherwise.
    pub fn load(
        scope: &Scope<'_>,
        this: Object<'_>,
        filename: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let filename = validate_string(scope, filename, "filename")?;
        this.set_property(scope, c"filename", filename.as_str())?;
        run_by_extension(scope, this, Path::new(&filename))?;
        this.set_property(scope, c"loaded", true)?;
        Ok(())
    }

    /// `module._compile(content, filename)`: run `content` as the CommonJS source of this module,
    /// whose file is `filename`.
    #[method(name = "_compile", length = 3)]
    pub fn compile(
        scope: &Scope<'_>,
        this: Object<'_>,
        content: HandleValue<'_>,
        filename: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let content = validate_string(scope, content, "content")?;
        let filename = validate_string(scope, filename, "filename")?;
        run_source(
            scope,
            this,
            &content,
            Path::new(&filename),
            SourceFormat::CommonJs,
        )?;
        Ok(())
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
        for name in [c".js", c".json", c".node"] {
            let loader = Function::new_callback(scope, name, 2, load_extension, ())?;
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
    mirror = "builtinModules, createRequire, isBuiltin, syncBuiltinESMExports"
)]
pub mod module {
    pub use super::module_class as Module;
}
