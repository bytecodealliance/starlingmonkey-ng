// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Node.js compatibility: the `node:` builtin modules and the Node globals.
//!
//! See `docs/node-compat.md` for what is implemented and how it behaves under WASI. [`register`]
//! enables everything. It is called for runtimes started with `--node-compat`.

pub mod assert;
pub mod async_hooks;
pub mod buffer;
pub mod child_process;
pub mod commonjs;
mod console;
pub mod constants;
pub mod crypto;
pub mod dgram;
pub mod diagnostics_channel;
pub mod dns;
mod domain;
pub mod errors;
pub mod events;
mod events_async;
mod exports_table;
pub mod fs;
mod glob;
pub mod http;
mod inspect;
mod internal_modules;
mod internals;
pub mod net;
pub mod os;
mod parse_args;
pub mod path;
pub mod perf_hooks;
pub mod process;
pub mod punycode;
pub mod querystring;
pub mod readline;
pub mod runtime_flags;
mod stack_trace;
pub mod stream;
pub mod string_decoder;
mod stub;
mod support;
pub mod test_runner;
pub mod timers;
pub mod tty;
pub mod url;
pub mod util;
mod vm;
pub mod zlib;

use core_runtime::module::{register_builtin_module, BuiltinModule};
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::prelude::HandleValue;

/// The modules that import an implemented module besides its name and bare alias.
const EXTRA_ALIASES: &[(&str, &[&str])] = &[
    ("node:path", &["path", "path/posix", "node:path/posix"]),
    ("node:util", &["util", "sys", "node:sys"]),
];

/// Register the builtin module `name`: its implementation if there is one, and its stub otherwise.
fn create_module(scope: &Scope<'_>, name: &'static str) -> bool {
    match name {
        "node:assert" => assert::register_module(scope, name),
        "node:async_hooks" => {
            stub::register_implemented::<async_hooks::module::js_module>(scope, name)
        }
        "node:assert/strict" => {
            stub::register_implemented::<assert::strict::js_module>(scope, name)
        }
        "node:buffer" => buffer::register_module(scope, name),
        "node:child_process" => child_process::register_module(scope, name),
        "node:constants" => constants::register_module(scope, name),
        "node:crypto" => crypto::register_module(scope, name),
        "node:dgram" => stub::register_implemented::<dgram::module::js_module>(scope, name),
        "node:diagnostics_channel" => {
            stub::register_implemented::<diagnostics_channel::module::js_module>(scope, name)
        }
        "node:http" => http::register_module(scope, name),
        "node:https" => http::register_https(scope, name),
        "node:net" => stub::register_implemented::<net::module::js_module>(scope, name),
        "node:dns" => dns::register_module(scope, name),
        "node:dns/promises" => dns::register_promises(scope, name),
        "node:domain" => domain::register_module(scope, name),
        "node:os" => os::register_module(scope, name),
        "node:punycode" => punycode::register_module(scope, name),
        "node:stream" => stream::register_module(scope, name),
        "node:stream/promises" => {
            stub::register_implemented::<stream::promises::js_module>(scope, name)
        }
        "node:tty" => stub::register_implemented::<tty::module::js_module>(scope, name),
        "node:readline" => readline::register_module(scope, name),
        "node:readline/promises" => readline::register_promises(scope, name),
        "node:test" => test_runner::register_module(scope, name),
        "node:timers" => timers::register_module(scope, name),
        "node:timers/promises" => timers::register_promises(scope, name),
        "node:url" => url::register_module(scope, name),
        "node:util" => util::register_module(scope, name),
        "node:util/types" => stub::register_implemented::<util::types::js_module>(scope, name),
        "node:vm" => vm::register_module(scope, name),
        "node:querystring" => querystring::register_module(scope, name),
        "node:string_decoder" => {
            stub::register_implemented::<string_decoder::module::js_module>(scope, name)
        }
        "node:module" => stub::register_implemented::<commonjs::module::js_module>(scope, name),
        "node:events" => stub::register_implemented::<events::module::js_module>(scope, name),
        "node:fs" => fs::register_module(scope, name),
        "node:fs/promises" => fs::register_promises(scope, name),
        "node:path" => path::register_module(scope, name),
        "node:perf_hooks" => perf_hooks::register_module(scope, name),
        "node:process" => process::register_module(scope, name),
        "node:zlib" => zlib::register_module(scope, name),
        name if internal_modules::is_implemented(name) => internal_modules::register(scope, name),
        name if name.starts_with("node:internal/") => internals::register(scope, name),
        _ => stub::register_stub(scope, name),
    }
}

/// Install the Node.js globals on `global`: `process`, `Buffer`, `global`, `setImmediate` and
/// `clearImmediate`, the `setTimeout` and `setInterval` that return `Timeout` objects, and `gc`
/// with `--expose-gc`. With `--no-experimental-websocket`, remove the globals `WebSocket` and
/// `CloseEvent`. A process that `child_process.fork` spawned connects `process` to its parent.
pub fn add_to_global(scope: &Scope<'_>, global: js::Object<'_>) {
    let install = || -> Result<(), ExnThrown> {
        perf_hooks::mark_start();
        register_classes(scope, global)?;
        perf_hooks::mark_environment();
        perf_hooks::install(scope)?;
        js::async_context::install_job_queue(scope)?;
        process::install(scope, global)?;
        timers::install(scope, global)?;
        stack_trace::install(scope)?;
        console::install(scope, global)?;
        let buffer = buffer::buffer::class_constructor(scope)?;
        buffer::define_species(scope, buffer)?;
        global.define_property(scope, c"Buffer", buffer, 0)?;
        if runtime_flags::get().expose_gc {
            let gc = js::Function::new_callback(scope, c"gc", 0, collect_garbage, ())?;
            // Enumerable and non-configurable, as Node.js defines it.
            let flags = js::class_spec::JSPROP_ENUMERATE | js::class_spec::JSPROP_PERMANENT;
            global.define_property(scope, c"gc", *gc, flags as u32)?;
        }
        global.set_property(scope, c"global", global)?;
        if !runtime_flags::get().experimental_websocket {
            global.delete_property(scope, c"WebSocket")?;
            global.delete_property(scope, c"CloseEvent")?;
        }
        child_process::connect_to_parent(scope);
        perf_hooks::mark_bootstrap_complete();
        Ok(())
    };
    if install().is_err() {
        js::exception::report_and_clear(scope, "installing the Node.js globals");
        panic!("installing the Node.js globals can only fail due to OOM");
    }
}

/// Intern the keys of the property templates that the builtins create objects with.
fn register_templates(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    events::EMITTER_PROPERTIES.register(scope)?;
    stream::READABLE_PROPERTIES.register(scope)?;
    http::register_templates(scope)?;
    url::URL_PROPERTIES.register(scope)?;
    timers::TIMEOUT_PROPERTIES.register(scope)?;
    timers::IMMEDIATE_PROPERTIES.register(scope)?;
    timers::ASYNC_RESOURCE_PROPERTIES.register(scope)?;
    fs::STATS_PROPERTIES.register(scope)?;
    fs::DIRENT_PROPERTIES.register(scope)?;
    inspect::CUSTOM_OPTIONS_PROPERTIES.register(scope)?;
    net::socket::ADDRESS_PROPERTIES.register(scope)?;
    dns::register_templates(scope)?;
    test_runner::MOCK_CALL_PROPERTIES.register(scope)
}

/// Register the classes of the Node.js modules on `global`, without defining their constructors
/// on it.
fn register_classes(scope: &Scope<'_>, global: js::Object<'_>) -> Result<(), ExnThrown> {
    register_templates(scope)?;
    events::event_emitter::register(scope)?;
    stream::register_classes(scope, global)?;
    dns::register_classes(scope, global)?;
    domain::register_classes(scope, global)?;
    events::OnceState::add_to_global(scope, global);
    events::RejectionState::add_to_global(scope, global);
    events_async::OnIteration::add_to_global(scope, global);
    events_async::OnceWait::add_to_global(scope, global);
    fs::register_classes(scope, global)?;
    http::register_classes(scope, global)?;
    net::register_classes(scope, global)?;
    readline::InterfaceState::add_to_global(scope, global);
    readline::QuestionAbort::add_to_global(scope, global);
    readline::QuestionAnswer::add_to_global(scope, global);
    stack_trace::CallSite::add_to_global(scope, global);
    string_decoder::NativeDecoder::add_to_global(scope, global);
    support::PendingOp::add_to_global(scope, global);
    zlib::ParamsRequest::add_to_global(scope, global);
    zlib::ZlibState::add_to_global(scope, global);
    assert::register_classes(scope, global)?;
    async_hooks::register_classes(scope, global)?;
    buffer::buffer::register(scope)?;
    child_process::register_classes(scope, global)?;
    commonjs::ModuleState::add_to_global(scope, global);
    commonjs::module_class::register(scope)?;
    crypto::register_classes(scope, global)?;
    dgram::register_classes(scope, global)?;
    perf_hooks::register_classes(scope, global)?;
    querystring::ModuleExports::add_to_global(scope, global);
    inspect::register_classes(scope, global)?;
    util::register_classes(scope, global);
    diagnostics_channel::register_classes(scope, global)?;
    errors::abort_error_class::register(scope)?;
    readline::interface_class::register(scope)?;
    readline::promises_interface_class::register(scope)?;
    string_decoder::string_decoder_class::register(scope)?;
    test_runner::register_classes(scope, global)?;
    timers::register_classes(scope, global)?;
    tty::read_stream_class::register(scope)?;
    url::url_class::register(scope)?;
    vm::register_classes(scope, global)?;
    zlib::register_classes(scope)?;
    Ok(())
}

/// `gc(options)`, which `--expose-gc` installs: run a full garbage collection. Returns a promise
/// resolved after it if `options.execution` is `'async'`, and `undefined` otherwise. The other
/// options are ignored.
fn collect_garbage(
    scope: &Scope<'_>,
    args: js::function::CallbackArgs<'_>,
    _: HandleValue,
) -> Result<js::native::Value, ExnThrown> {
    js::gc::gc(scope, js::gc::GCReason::API);
    let options = args.get(0);
    if let Ok(options) = js::Object::from_value(scope, options) {
        let execution = options.get_property(scope, c"execution")?;
        if execution.is_string()
            && js::JSString::from_value(scope, execution)?.to_utf8(scope)? == "async"
        {
            let promise = js::Promise::new_resolved_with_value(scope, HandleValue::undefined())?;
            return js::conversion::ToJSVal::to_jsval_raw_throwing(&promise, scope);
        }
    }
    Ok(js::value::undefined())
}

/// Set whether the `-e` script came from `-p`, whose completion value is printed when the process
/// exits.
pub fn set_print_eval(print: bool) {
    commonjs::set_print_eval(print);
}

/// Register every `node:` builtin module and the Node.js globals, so they exist in every runtime
/// created after this call. The modules that only exist with a runtime flag are registered if
/// [`runtime_flags::set`] enabled them: the `internal/` modules with `--expose-internals`, and
/// `stream/iter` and `zlib/iter` with `--experimental-stream-iter`.
pub fn register() {
    perf_hooks::register();
    core_runtime::runtime::register_global_initializer(add_to_global);
    core_runtime::runtime::register_resume_fixup(process::on_resume);
    core_runtime::module::set_commonjs_loader(Some(commonjs::load_for_import));
    core_runtime::module::set_commonjs_eval(Some(commonjs::run_eval));
    errors::install_web_errors();
    for &(name, aliases, _) in exports_table::BUILTIN_EXPORTS {
        let extra = EXTRA_ALIASES
            .iter()
            .find(|(module, _)| *module == name)
            .map(|(_, extra)| *extra);
        // A module another one imports under its name is that module.
        if EXTRA_ALIASES
            .iter()
            .any(|(module, extra)| *module != name && extra.contains(&name))
        {
            continue;
        }
        register_builtin_module(BuiltinModule {
            name,
            aliases: extra.unwrap_or(aliases),
            create: create_module,
        });
    }
    let flags = runtime_flags::get();
    for &(name, aliases, _, _) in exports_table::FLAGGED_EXPORTS {
        let enabled = if name.starts_with("node:internal/") {
            flags.expose_internals
        } else {
            flags.experimental_stream_iter
        };
        if enabled {
            register_builtin_module(BuiltinModule {
                name,
                aliases,
                create: create_module,
            });
        }
    }
}

/// The builtin module names `module.builtinModules` lists: the bare names, then the names of the
/// modules that only exist with the `node:` prefix, as Node.js orders them.
pub(crate) fn builtin_module_names() -> impl Iterator<Item = &'static str> {
    let names = || {
        exports_table::BUILTIN_EXPORTS
            .iter()
            .map(|&(name, aliases, _)| aliases.first().copied().unwrap_or(name))
    };
    names()
        .filter(|name| !name.starts_with("node:"))
        .chain(names().filter(|name| name.starts_with("node:")))
}
