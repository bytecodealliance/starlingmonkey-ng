// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Node.js compatibility: the `node:` builtin modules and the Node globals.
//!
//! See `docs/node-compat.md` for what is implemented and how it behaves under WASI. [`register`]
//! enables everything. It is called for runtimes started with `--node-compat`.

pub mod assert;
pub mod async_hooks;
pub mod buffer;
pub mod commonjs;
mod console;
pub mod constants;
pub mod crypto;
pub mod diagnostics_channel;
pub mod dns;
pub mod errors;
pub mod events;
mod events_async;
mod exports_table;
pub mod fs;
pub mod http;
mod inspect;
mod internal_modules;
pub mod net;
pub mod os;
pub mod path;
pub mod process;
pub mod punycode;
pub mod querystring;
pub mod readline;
mod stack_trace;
pub mod stream;
pub mod string_decoder;
mod stub;
pub mod timers;
pub mod tty;
pub mod url;
pub mod util;
pub mod zlib;

use core_runtime::module::{register_builtin_module, BuiltinModule};
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::prelude::HandleValue;

/// The modules that import an implemented module besides its name and bare alias.
const EXTRA_ALIASES: &[(&str, &[&str])] =
    &[("node:path", &["path", "path/posix", "node:path/posix"])];

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
        "node:constants" => constants::register_module(scope, name),
        "node:crypto" => crypto::register_module(scope, name),
        "node:diagnostics_channel" => {
            stub::register_implemented::<diagnostics_channel::module::js_module>(scope, name)
        }
        "node:http" => http::register_module(scope, name),
        "node:https" => http::register_https(scope, name),
        "node:net" => stub::register_implemented::<net::module::js_module>(scope, name),
        "node:dns" => dns::register_module(scope, name),
        "node:dns/promises" => dns::register_promises(scope, name),
        "node:os" => os::register_module(scope, name),
        "node:punycode" => punycode::register_module(scope, name),
        "node:stream" => stream::register_module(scope, name),
        "node:stream/promises" => {
            stub::register_implemented::<stream::promises::js_module>(scope, name)
        }
        "node:tty" => stub::register_implemented::<tty::module::js_module>(scope, name),
        "node:readline" => readline::register_module(scope, name),
        "node:readline/promises" => readline::register_promises(scope, name),
        "node:timers" => timers::register_module(scope, name),
        "node:timers/promises" => timers::register_promises(scope, name),
        "node:url" => url::register_module(scope, name),
        "node:util" => util::register_module(scope, name),
        "node:util/types" => stub::register_implemented::<util::types::js_module>(scope, name),
        "node:querystring" => {
            stub::register_implemented::<querystring::module::js_module>(scope, name)
        }
        "node:string_decoder" => {
            stub::register_implemented::<string_decoder::module::js_module>(scope, name)
        }
        "node:module" => stub::register_implemented::<commonjs::module::js_module>(scope, name),
        "node:events" => stub::register_implemented::<events::module::js_module>(scope, name),
        "node:fs" => fs::register_module(scope, name),
        "node:fs/promises" => fs::register_promises(scope, name),
        "node:path" => stub::register_implemented::<path::path::js_module>(scope, name),
        "node:process" => process::register_module(scope, name),
        "node:zlib" => zlib::register_module(scope, name),
        name if internal_modules::is_implemented(name) => internal_modules::register(scope, name),
        _ => stub::register_stub(scope, name),
    }
}

/// Install the Node.js globals on `global`: `process`, `Buffer`, `global`, `setImmediate` and
/// `clearImmediate`, and the `setTimeout` and `setInterval` that return `Timeout` objects.
pub fn add_to_global(scope: &Scope<'_>, global: js::Object<'_>) {
    let install = || -> Result<(), ExnThrown> {
        register_classes(scope, global)?;
        js::async_context::install_job_queue(scope)?;
        process::install(scope, global)?;
        timers::install(scope, global)?;
        stack_trace::install(scope)?;
        console::install(scope, global)?;
        let buffer = buffer::buffer::class_constructor(scope)?;
        global.define_property(scope, c"Buffer", buffer, 0)?;
        global.set_property(scope, c"global", global)
    };
    install().expect("installing the Node.js globals can only fail due to OOM");
}

/// Register the classes of the Node.js modules on `global`, without defining their constructors
/// on it.
fn register_classes(scope: &Scope<'_>, global: js::Object<'_>) -> Result<(), ExnThrown> {
    events::event_emitter::register(scope)?;
    stream::register_classes(scope)?;
    fs::register_classes(scope, global)?;
    http::register_classes(scope, global)?;
    net::register_classes(scope, global)?;
    crypto::register_classes(scope, global)?;
    zlib::register_classes(scope, global)?;
    assert::assertion_error::register(scope)?;
    assert::call_tracker::register(scope)?;
    async_hooks::async_local_storage::register(scope)?;
    async_hooks::async_resource::register(scope)?;
    buffer::buffer::register(scope)?;
    commonjs::module_class::register(scope)?;
    diagnostics_channel::channel::register(scope)?;
    diagnostics_channel::tracing_channel::register(scope)?;
    errors::abort_error_class::register(scope)?;
    readline::interface_class::register(scope)?;
    readline::promises_interface_class::register(scope)?;
    stack_trace::CallSite::add_to_global(scope, global);
    string_decoder::NativeDecoder::add_to_global(scope, global);
    string_decoder::string_decoder_class::register(scope)?;
    timers::timeout::register(scope)?;
    timers::immediate::register(scope)?;
    tty::StdinHandle::add_to_global(scope, global);
    tty::read_stream_class::register(scope)?;
    url::url_class::register(scope)?;
    Ok(())
}

/// Register every `node:` builtin module and the Node.js globals, so they exist in every runtime
/// created after this call.
pub fn register() {
    core_runtime::runtime::register_global_initializer(add_to_global);
    core_runtime::runtime::register_resume_fixup(process::on_resume);
    core_runtime::module::set_commonjs_loader(Some(commonjs::load_for_import));
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
}

/// The arguments of a native function call.
pub(crate) fn callback_args<'a>(args: &'a js::function::CallbackArgs<'_>) -> Vec<HandleValue<'a>> {
    (0..args.len()).map(|index| args.get(index)).collect()
}

/// The builtin module names `module.builtinModules` lists: the bare names, and the names of the
/// modules that only exist with the `node:` prefix.
pub(crate) fn builtin_module_names() -> impl Iterator<Item = &'static str> {
    exports_table::BUILTIN_EXPORTS
        .iter()
        .map(|&(name, aliases, _)| aliases.first().copied().unwrap_or(name))
}

/// A new symbol with `description`, registered under that key in the global symbol registry
/// (`Symbol.for`) if `registered`.
pub(crate) fn symbol<'s>(
    scope: &'s Scope<'_>,
    description: &str,
    registered: bool,
) -> Result<HandleValue<'s>, ExnThrown> {
    let description = js::JSString::from_str(scope, description)?;
    let symbol = if registered {
        js::symbol::get_symbol_for(scope, description.handle())?
    } else {
        js::symbol::new_symbol(scope, description.handle())?
    };
    Ok(scope.root_value(js::value::from_symbol(symbol)))
}
