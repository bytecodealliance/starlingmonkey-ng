// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:fs/promises`, which `fs.promises` also is, and its `FileHandle`.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{Function, Object};

use super::{ops, run_async, Prepared, Variant};
use crate::errors::{ErrorKind, NodeError, Thrown};

/// The key of the shared object `fs.promises` is, so every module of a global uses one.
static PROMISES_KEY: u8 = 0;

/// Run the operation `name` as a promise function with `args`, and return the promise. Argument
/// errors reject the promise.
fn run_promise<'s>(
    scope: &'s Scope<'_>,
    name: &str,
    args: &[HandleValue<'_>],
    map: Option<fn(Prepared) -> Prepared>,
) -> Result<js::Promise<'s>, ExnThrown> {
    let op = &ops::OPS[ops::index_of(name)];
    let started = (op.parse)(scope, args, Variant::Promise)
        .map(|prepared| match map {
            Some(map) => map(prepared),
            None => prepared,
        })
        .and_then(|prepared| run_async(scope, name, prepared, Variant::Promise));
    match started {
        Ok(promise) => Ok(promise),
        Err(thrown) => {
            thrown.throw(scope);
            js::Promise::new_rejected_with_pending_error(scope)
        }
    }
}

/// `prepared`, with the file `open` produces wrapped in a `FileHandle`.
fn as_file_handle(prepared: Prepared) -> Prepared {
    let Prepared { work, finish } = prepared;
    Prepared {
        work,
        finish: Box::new(move |scope, output, variant| {
            let fd = finish(scope, output, variant)?;
            let class = file_handle_class::class_constructor(scope)?;
            let handle = Function::construct(scope, class, &fd)?;
            Ok(vec![handle.to_jsval_throwing(scope)?])
        }),
    }
}

/// The descriptor of `value`, if it is a `FileHandle`.
pub(crate) fn file_handle_fd(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<i32>, Thrown> {
    let Ok(object) = Object::from_value(scope, *value) else {
        return Ok(None);
    };
    let class = file_handle_class::class_constructor(scope)?;
    let prototype = class.get_property(scope, c"prototype")?;
    let is_handle = object
        .get_prototype(scope)?
        .is_some_and(|actual| actual.handle().get() == prototype.to_object_or_null());
    if !is_handle {
        return Ok(None);
    }
    let fd = object.get_property(scope, c"fd")?;
    Ok(Some(super::fd_arg(scope, fd)?))
}

/// Run the descriptor operation `name` with the descriptor of the `FileHandle` `this` followed by
/// `args`. On a closed handle, the promise rejects with an `EBADF` error with `syscall`.
fn run_method<'s>(
    scope: &'s Scope<'_>,
    this: Object<'_>,
    name: &str,
    syscall: &str,
    args: &[HandleValue<'_>],
) -> Result<js::Promise<'s>, ExnThrown> {
    let fd = this.get_property(scope, c"fd")?;
    if fd.is_number() && fd.to_number() == -1.0 {
        let error = NodeError::new(ErrorKind::Error, "EBADF", "file closed");
        let error = crate::stream::error_value(scope, error)?;
        let object = Object::from_value(scope, *error).map_err(|e| e.throw(scope))?;
        object.set_property(scope, c"syscall", syscall)?;
        return js::Promise::new_rejected_with_error(scope, error);
    }
    let mut values = vec![fd];
    values.extend_from_slice(args);
    run_promise(scope, name, &values, None)
}

/// The body of `FileHandle.prototype[Symbol.asyncDispose]`: `this.close()`.
fn async_dispose(
    scope: &Scope<'_>,
    args: js::function::CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<js::native::Value, ExnThrown> {
    let this =
        Object::from_value(scope, scope.root_value(args.this())).map_err(|e| e.throw(scope))?;
    crate::stream::call_method(scope, this, c"close", &[])?;
    Ok(js::value::undefined())
}

#[core_runtime::jsfunction_class(name = "FileHandle", class_syntax)]
pub mod file_handle_class {
    use super::*;

    /// <https://nodejs.org/api/fs.html#class-filehandle>: the handle of the descriptor `fd`.
    #[constructor]
    pub fn init(scope: &Scope<'_>, this: Object<'_>, fd: HandleValue<'_>) -> Result<(), ExnThrown> {
        this.set_property(scope, c"fd", fd)
    }

    /// <https://nodejs.org/api/fs.html#filehandleclose>: `fd` becomes -1. Closing a closed handle
    /// does nothing.
    pub fn close<'s>(scope: &'s Scope<'_>, this: Object<'_>) -> Result<js::Promise<'s>, ExnThrown> {
        let fd = this.get_property(scope, c"fd")?;
        if fd.is_number() && fd.to_number() == -1.0 {
            return js::Promise::new_resolved_with_value(scope, js::value::undefined());
        }
        // A close refused for blocking host I/O leaves the handle open.
        let refused = fd.is_number()
            && super::super::close_blocks(fd.to_number() as i32)
            && core_runtime::event_loop::blocking_host_io_refusal().is_some();
        if !refused {
            this.set_property(scope, c"fd", -1)?;
        }
        run_promise(scope, "close", &[fd], None)
    }

    /// <https://nodejs.org/api/fs.html#filehandlereadbuffer-offset-length-position>
    pub fn read<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        run_method(scope, this, "read", "read", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandlewritebuffer-offset-length-position>
    pub fn write<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        run_method(scope, this, "write", "write", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandlereadfileoptions>
    pub fn read_file<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        run_method(scope, this, "readFile", "readFile", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandlewritefiledata-options>
    pub fn write_file<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        run_method(scope, this, "writeFile", "writeFile", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandleappendfiledata-options>
    pub fn append_file<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        run_method(scope, this, "appendFile", "writeFile", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandlestatoptions>
    pub fn stat<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        run_method(scope, this, "fstat", "fstat", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandletruncatelen>
    pub fn truncate<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        run_method(scope, this, "ftruncate", "ftruncate", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandlesync>
    pub fn sync<'s>(scope: &'s Scope<'_>, this: Object<'_>) -> Result<js::Promise<'s>, ExnThrown> {
        run_method(scope, this, "fsync", "fsync", &[])
    }

    /// <https://nodejs.org/api/fs.html#filehandledatasync>
    pub fn datasync<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        run_method(scope, this, "fdatasync", "fdatasync", &[])
    }

    /// <https://nodejs.org/api/fs.html#filehandlechmodmode>
    pub fn chmod<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        run_method(scope, this, "fchmod", "fchmod", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandlechownuid-gid>
    pub fn chown<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        run_method(scope, this, "fchown", "fchown", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandleutimesatime-mtime>
    pub fn utimes<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'s>, ExnThrown> {
        run_method(scope, this, "futimes", "futimes", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandlecreatereadstreamoptions>
    pub fn create_read_stream<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let class = super::super::streams::read_stream_class::class_constructor(scope)?;
        super::super::streams::with_fd_stream(scope, class, this, options)
    }

    /// <https://nodejs.org/api/fs.html#filehandlecreatewritestreamoptions>
    pub fn create_write_stream<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let class = super::super::streams::write_stream_class::class_constructor(scope)?;
        super::super::streams::with_fd_stream(scope, class, this, options)
    }

    /// Define `[Symbol.asyncDispose]`, which closes the handle.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let dispose =
            Function::new_callback(scope, c"[Symbol.asyncDispose]", 0, async_dispose, ())?;
        js::class::define_well_known_symbol_property(
            scope,
            proto,
            js::native::SymbolCode::asyncDispose,
            *dispose,
        )
    }
}

/// The body of `fsPromises.open`, whose payload is unused.
fn open(
    scope: &Scope<'_>,
    args: js::function::CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<js::native::Value, ExnThrown> {
    let values = crate::callback_args(&args);
    run_promise(scope, "open", &values, Some(as_file_handle))?.to_jsval_raw_throwing(scope)
}

/// The object `fs.promises` and `node:fs/promises` export: the promise functions of
/// [`ops::OPS`], `open`, `cp`, `constants`, and stubs for the rest of `node:fs/promises`.
pub(super) fn promises_object<'s>(scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    let object = js::class::get_or_init_shared_object(
        scope,
        &PROMISES_KEY as *const u8 as usize,
        |scope| {
            let object = Object::new_plain(scope)?;
            let exports = crate::stub::exports_of("node:fs/promises").unwrap_or_default();
            for &(name, kind) in exports {
                let key = std::ffi::CString::new(name).expect("names have no NULs");
                let value = if name == "open" {
                    Function::new_callback(scope, c"open", 3, open, ())?.to_jsval_throwing(scope)?
                } else if name == "cp" {
                    Function::new_callback(scope, c"cp", 3, super::cp::cp_promise, ())?
                        .to_jsval_throwing(scope)?
                } else if name == "constants" {
                    super::constants(scope)?
                } else if let Some(index) = ops::OPS
                    .iter()
                    .position(|op| op.name == name && op.variants & ops::PROMISE != 0)
                {
                    super::op_function(scope, index, Variant::Promise, name)?
                } else {
                    crate::stub::stub_value(scope, "node:fs/promises", name, kind)?
                };
                object.set_property_by_name(scope, &key, value)?;
            }
            Ok(object)
        },
    )?;
    object.to_jsval_throwing(scope)
}

#[core_runtime::jsmodule(node_args, name = "node:fs/promises", default_namespace)]
pub mod module {}

/// Register `node:fs/promises` with the properties of [`promises_object`].
pub(crate) fn register_promises(scope: &Scope<'_>, name: &'static str) -> bool {
    let exports = || -> Result<Vec<(&'static str, HandleValue<'_>)>, ExnThrown> {
        let object = promises_object(scope)?;
        let object = Object::from_value(scope, *object).map_err(|e| e.throw(scope))?;
        let mut exports = Vec::new();
        for &(export, _) in crate::stub::exports_of(name).unwrap_or_default() {
            let key = std::ffi::CString::new(export).expect("names have no NULs");
            let value = object.get_property_by_name(scope, &key)?;
            exports.push((export, value));
        }
        Ok(exports)
    };
    let Ok(exports) = exports() else {
        return false;
    };
    crate::stub::register_implemented_with::<module::js_module>(scope, name, &exports)
}
