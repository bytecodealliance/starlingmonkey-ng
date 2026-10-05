// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Node.js's `internal/` modules, which `require` and `import` resolve with
//! `--expose-internals`: the ones Node.js's own tests load. Each module is an object of the
//! members implemented here, with stubs for its other exports, as `exports_table` records them.
//! The members that are also public are the public module's values.

mod binding;
mod fs;
mod functions;

use std::ffi::{CStr, CString};

use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::function::Callback;
use js::gc::scope::Scope;
use js::{Function, Object};
use web_globals::events::custom_event::CustomEventImpl;
use web_globals::events::event::EventImpl;
use web_globals::events::event_target::EventTargetImpl;

pub(crate) use binding::process_binding;

/// Where the value of a member of an internal module comes from.
enum Source {
    /// The export of this name of the builtin module of this name.
    Module(&'static str, &'static CStr),
    /// The constructor this function returns.
    Class(for<'s> fn(&'s Scope<'_>) -> Result<Object<'s>, ExnThrown>),
    /// A native function with this length.
    Native(Callback, u32),
}

use Source::*;

/// The implemented members of each internal module, and where their values come from.
const MEMBERS: &[(&str, &[(&CStr, Source)])] = &[
    (
        "node:internal/assert/myers_diff",
        &[(c"myersDiff", Native(functions::myers_diff, 3))],
    ),
    (
        "node:internal/blocklist",
        &[(c"BlockList", Module("node:net", c"BlockList"))],
    ),
    (
        "node:internal/event_target",
        &[
            (
                c"CustomEvent",
                Class(js::class::class_constructor::<CustomEventImpl>),
            ),
            (c"Event", Class(js::class::class_constructor::<EventImpl>)),
            (
                c"EventTarget",
                Class(js::class::class_constructor::<EventTargetImpl>),
            ),
        ],
    ),
    (
        "node:internal/fs/utils",
        &[
            (c"Dirent", Module("node:fs", c"Dirent")),
            (c"Stats", Module("node:fs", c"Stats")),
            (c"getDirent", Native(fs::get_dirent, 4)),
            (c"getDirents", Native(fs::get_dirents, 3)),
            (c"stringToFlags", Native(fs::string_to_flags, 2)),
            (
                c"validateOffsetLengthRead",
                Native(fs::validate_offset_length_read, 3),
            ),
            (
                c"validateOffsetLengthWrite",
                Native(fs::validate_offset_length_write, 3),
            ),
        ],
    ),
    (
        "node:internal/linkedlist",
        &[
            (c"append", Native(functions::list_append, 2)),
            (c"init", Native(functions::list_init, 1)),
            (c"isEmpty", Native(functions::list_is_empty, 1)),
            (c"peek", Native(functions::list_peek, 1)),
            (c"remove", Native(functions::list_remove, 1)),
        ],
    ),
    (
        "node:internal/net",
        &[
            (c"isIP", Module("node:net", c"isIP")),
            (c"isIPv4", Module("node:net", c"isIPv4")),
            (c"isIPv6", Module("node:net", c"isIPv6")),
        ],
    ),
    (
        "node:internal/options",
        &[(c"getOptionValue", Native(functions::get_option_value, 1))],
    ),
    (
        "node:internal/socketaddress",
        &[(c"SocketAddress", Module("node:net", c"SocketAddress"))],
    ),
    (
        "node:internal/streams/add-abort-signal",
        &[
            (c"addAbortSignal", Module("node:stream", c"addAbortSignal")),
            (
                c"addAbortSignalNoValidate",
                Native(functions::add_abort_signal_no_validate, 2),
            ),
        ],
    ),
    (
        "node:internal/streams/end-of-stream",
        &[
            (c"eos", Module("node:stream", c"finished")),
            (c"finished", Module("node:stream", c"finished")),
        ],
    ),
    (
        "node:internal/streams/state",
        &[
            (
                c"getDefaultHighWaterMark",
                Module("node:stream", c"getDefaultHighWaterMark"),
            ),
            (
                c"setDefaultHighWaterMark",
                Module("node:stream", c"setDefaultHighWaterMark"),
            ),
        ],
    ),
    (
        "node:internal/timers",
        &[(c"setUnrefTimeout", Native(functions::set_unref_timeout, 2))],
    ),
    (
        "node:internal/url",
        &[
            (c"URL", Module("node:url", c"URL")),
            (c"URLSearchParams", Module("node:url", c"URLSearchParams")),
            (c"domainToASCII", Module("node:url", c"domainToASCII")),
            (c"domainToUnicode", Module("node:url", c"domainToUnicode")),
            (c"fileURLToPath", Module("node:url", c"fileURLToPath")),
            (c"isURL", Native(functions::is_url, 1)),
            (c"pathToFileURL", Module("node:url", c"pathToFileURL")),
            (c"urlToHttpOptions", Module("node:url", c"urlToHttpOptions")),
        ],
    ),
    (
        "node:internal/util",
        &[
            (c"deprecate", Module("node:util", c"deprecate")),
            (
                c"emitExperimentalWarning",
                Native(functions::emit_experimental_warning, 1),
            ),
            (c"pendingDeprecate", Native(functions::pending_deprecate, 3)),
            (c"promisify", Module("node:util", c"promisify")),
            (c"sleep", Native(functions::sleep, 1)),
        ],
    ),
    (
        "node:internal/util/inspect",
        &[
            (c"format", Module("node:util", c"format")),
            (
                c"formatWithOptions",
                Module("node:util", c"formatWithOptions"),
            ),
            (c"getStringWidth", Native(functions::get_string_width, 2)),
            (c"inspect", Module("node:util", c"inspect")),
            (
                c"stripVTControlCharacters",
                Module("node:util", c"stripVTControlCharacters"),
            ),
        ],
    ),
];

/// The value of internal module `name`: an object of the members [`MEMBERS`] lists for it, and
/// the members that need more than a lookup.
fn module_value<'s>(scope: &'s Scope<'_>, name: &str) -> Result<Object<'s>, ExnThrown> {
    let value = Object::new_plain(scope)?;
    let members = MEMBERS
        .iter()
        .find(|(module, _)| *module == name)
        .map_or(&[][..], |(_, members)| members);
    for (member, source) in members {
        let member_value = match source {
            Module(module, export) => crate::internal_modules::module_default(scope, module)?
                .get_property(scope, export)?,
            Class(constructor) => constructor(scope)?.to_jsval_throwing(scope)?,
            Native(callback, length) => {
                Function::new_callback(scope, member, *length, *callback, ())?
                    .to_jsval_throwing(scope)?
            }
        };
        value.set_property(scope, member, member_value)?;
    }
    match name {
        "node:internal/buffer" => {
            let buffer = crate::buffer::buffer::class_constructor(scope)?;
            let species = js::symbol::get_well_known_key(scope, js::native::SymbolCode::species);
            let species = buffer.get_property_by_id(scope, scope.root_id(species))?;
            value.set_property(scope, c"FastBuffer", species)?;
        }
        "node:internal/net" => {
            let symbol = crate::net::normalized_args_symbol(scope)?;
            value.set_property(scope, c"normalizedArgsSymbol", symbol)?;
        }
        "node:internal/readline/utils" => {
            value.set_property(scope, c"CSI", functions::csi_tag(scope)?)?;
        }
        "node:internal/test/binding" => binding::add_module_members(scope, value)?,
        "node:internal/util/inspect" => {
            let util = crate::internal_modules::module_default(scope, "node:util")?;
            let inspect = util.get_property(scope, c"inspect")?;
            let inspect = Object::from_value(scope, inspect).map_err(|_| ExnThrown)?;
            let defaults = inspect.get_property(scope, c"defaultOptions")?;
            value.set_property(scope, c"inspectDefaultOptions", defaults)?;
        }
        _ => {}
    }
    Ok(value)
}

/// Register internal module `name`, whose value is the `default` export. Its named exports are the
/// value's properties of the names Node.js exports, with stubs set on the value for the names it
/// lacks. A module whose value is a class in Node.js is a stub class.
pub(crate) fn register(scope: &Scope<'_>, name: &'static str) -> bool {
    let register = || -> Result<(), ExnThrown> {
        let &(_, _, kind, exports) = crate::stub::flagged_entry(name).ok_or(ExnThrown)?;
        let value = match kind {
            crate::stub::ExportKind::Class(_) | crate::stub::ExportKind::Function(_) => {
                let export = name.rsplit('/').next().unwrap_or(name);
                let stub = crate::stub::stub_value(scope, name, export, kind)?;
                Object::from_value(scope, stub).map_err(|_| ExnThrown)?
            }
            _ => module_value(scope, name)?,
        };
        let mut values = Vec::with_capacity(exports.len() + 1);
        for &(export, kind) in exports {
            let c_export = CString::new(export).map_err(|_| ExnThrown)?;
            if !value.has_own_property_by_name(scope, &c_export)? {
                let stub = crate::stub::stub_value(scope, name, export, kind)?;
                value.set_property_by_name(scope, &c_export, stub)?;
            }
            values.push((export, value.get_property_by_name(scope, &c_export)?));
        }
        values.push(("default", value.to_jsval_throwing(scope)?));
        if name == "node:internal/test/binding" {
            crate::process::emit_warning(
                scope,
                "These APIs are for internal testing only. Do not use them.",
                "internal/test/binding",
            )?;
        }
        // SAFETY: the caller guarantees the module loader is initialized, and every value is
        // rooted.
        core_runtime::module::register_synthetic_module(scope, name, &values)
    };
    register().is_ok()
}
