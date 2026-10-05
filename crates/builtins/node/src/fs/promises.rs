// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:fs/promises`, which `fs.promises` also is, and its `FileHandle`.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::gc::handle::Heap;
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

/// `prepared`, with the file `open` produces wrapped in a `FileHandle`. The file is closed if the
/// handle can't be created.
fn as_file_handle(prepared: Prepared) -> Prepared {
    let Prepared { work, finish } = prepared;
    Prepared {
        work,
        finish: Box::new(move |scope, output, variant| {
            let fd = finish(scope, output, variant)?;
            let fd = fd.first().map_or(-1, |fd| fd.to_number() as i32);
            let handle = new_file_handle(scope, fd).inspect_err(|_| {
                let _ = super::remove_file(fd);
            })?;
            Ok(vec![handle.to_jsval_throwing(scope)?])
        }),
    }
}

/// The descriptor of `value`, if it is a `FileHandle`.
pub(crate) fn file_handle_fd(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<i32>, Thrown> {
    let handle = Object::from_value(scope, *value)
        .ok()
        .and_then(|object| object.cast::<FileHandle>().ok());
    let Some(handle) = handle else {
        return Ok(None);
    };
    let fd = handle.data().fd;
    Ok(Some(super::fd_arg(
        scope,
        scope.root_value(js::value::from_f64(f64::from(fd))),
    )?))
}

/// A new `FileHandle` of the descriptor `fd`, which closes the descriptor when it is finalized
/// while open.
fn new_file_handle<'s>(scope: &'s Scope<'_>, fd: i32) -> Result<FileHandle<'s>, Thrown> {
    let file = super::Handle::of(fd, "open")
        .ok()
        .and_then(|file| file.downgrade());
    let handle = js::class::create_instance_with::<FileHandleImpl>(scope, |_| FileHandleImpl {
        fd,
        refs: 1,
        closing: None,
        file,
    })?;
    crate::events::event_emitter::init(scope, handle.as_object(), None)?;
    Ok(handle)
}

/// The body of `FileHandle.prototype[Symbol.asyncDispose]`: `this.close()`.
fn async_dispose(
    scope: &Scope<'_>,
    args: js::function::CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<js::native::Value, ExnThrown> {
    let this =
        Object::from_value(scope, scope.root_value(args.this())).map_err(|e| e.throw(scope))?;
    Function::call_by_name(scope, this.handle(), c"close", js::function::EmptyArgs)
        .map(|promise| promise.get())
}

/// A `FileHandle`: an `EventEmitter` holding a file descriptor, or -1 once it was closed. See
/// <https://nodejs.org/api/fs.html#class-filehandle>.
///
/// As in Node.js, the handle counts references to the descriptor: one of its own until `close()`,
/// and one for each stream created from it until the stream closes. The descriptor is closed when
/// the count drops to zero.
#[core_runtime::jsclass(
    name = "FileHandle",
    hidden,
    proto_parent = "crate::events::event_emitter"
)]
pub struct FileHandle {
    #[no_trace]
    fd: i32,
    #[no_trace]
    refs: u32,
    /// The promise `close()` returned while streams still referenced the descriptor, which is
    /// settled once it is closed.
    closing: Option<Heap<js::promise::Promise>>,
    /// The file that `fd` referred to when an `open` created the handle. `None` once the handle is
    /// closed, and for a handle created by its constructor.
    #[no_trace]
    file: Option<super::WeakHandle>,
}

#[core_runtime::jsmethods(node_args)]
impl FileHandle {
    /// A handle of the descriptor `fd`, or of none if `fd` isn't an integer.
    #[constructor]
    pub fn new(&self, scope: &Scope<'_>, fd: HandleValue<'_>) -> Result<(), Thrown> {
        crate::events::event_emitter::init(scope, self.as_object(), None)?;
        let number = fd.is_number().then(|| fd.to_number());
        let mut data = self.data_mut();
        data.fd = number
            .filter(|fd| fd.fract() == 0.0 && (0.0..=2147483647.0).contains(fd))
            .map_or(-1, |fd| fd as i32);
        data.refs = 1;
        Ok(())
    }

    /// Close the descriptor if the handle is finalized while it is open and the descriptor still
    /// refers to the file the handle opened. Node.js also warns about this, which a finalizer
    /// can't do, since it can't run JavaScript.
    #[destructor]
    fn close_on_finalize(&mut self) {
        if let Some(file) = self.file.take() {
            if self.fd != -1 {
                super::remove_file_of(self.fd, &file);
            }
        }
    }

    /// <https://nodejs.org/api/fs.html#filehandlefd>
    #[getter]
    pub fn fd(&self) -> f64 {
        f64::from(self.data().fd)
    }

    /// <https://nodejs.org/api/fs.html#filehandleclose>: the handle emits `close`, and releases
    /// its reference to the descriptor. If no stream references it, `fd` becomes -1 and the
    /// descriptor is closed. Otherwise the returned promise settles once the last stream released
    /// it, and later calls return the same promise. Closing a closed handle does nothing.
    #[method]
    pub fn close<'r>(&self, scope: &'r Scope<'_>) -> Result<js::Promise<'r>, ExnThrown> {
        let (fd, refs, closing) = {
            let data = self.data();
            let closing = data.closing.as_ref().map(|closing| closing.get(scope));
            (data.fd, data.refs, closing)
        };
        if fd == -1 {
            return js::Promise::new_resolved_with_value(scope, js::value::undefined());
        }
        if let Some(closing) = closing {
            return Ok(closing);
        }
        if refs > 1 {
            let closing = js::Promise::new_pending(scope)?;
            {
                let mut data = self.data_mut();
                data.refs -= 1;
                data.closing = Some(Heap::from(closing));
            }
            crate::events::emit(scope, self.as_object(), c"close", &[])?;
            return Ok(closing);
        }
        // A close refused for blocking host I/O leaves the handle open.
        let refused = super::close_blocks(fd)
            && core_runtime::event_loop::blocking_host_io_refusal().is_some();
        if !refused {
            self.closed();
        }
        let fd = scope.root_value(js::value::from_f64(f64::from(fd)));
        let promise = run_promise(scope, "close", &[fd], None)?;
        if !refused {
            crate::events::emit(scope, self.as_object(), c"close", &[])?;
        }
        Ok(promise)
    }

    /// <https://nodejs.org/api/fs.html#filehandlereadbuffer-offset-length-position>
    #[method]
    pub fn read<'r>(
        &self,
        scope: &'r Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'r>, ExnThrown> {
        self.run(scope, "read", "read", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandlewritebuffer-offset-length-position>
    #[method]
    pub fn write<'r>(
        &self,
        scope: &'r Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'r>, ExnThrown> {
        self.run(scope, "write", "write", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandlereadfileoptions>
    #[method]
    pub fn read_file<'r>(
        &self,
        scope: &'r Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'r>, ExnThrown> {
        self.run(scope, "readFile", "readFile", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandlewritefiledata-options>
    #[method]
    pub fn write_file<'r>(
        &self,
        scope: &'r Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'r>, ExnThrown> {
        self.run(scope, "writeFile", "writeFile", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandleappendfiledata-options>
    #[method]
    pub fn append_file<'r>(
        &self,
        scope: &'r Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'r>, ExnThrown> {
        self.run(scope, "appendFile", "writeFile", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandlestatoptions>
    #[method]
    pub fn stat<'r>(
        &self,
        scope: &'r Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'r>, ExnThrown> {
        self.run(scope, "fstat", "fstat", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandletruncatelen>
    #[method]
    pub fn truncate<'r>(
        &self,
        scope: &'r Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'r>, ExnThrown> {
        self.run(scope, "ftruncate", "ftruncate", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandlesync>
    #[method]
    pub fn sync<'r>(&self, scope: &'r Scope<'_>) -> Result<js::Promise<'r>, ExnThrown> {
        self.run(scope, "fsync", "fsync", &[])
    }

    /// <https://nodejs.org/api/fs.html#filehandledatasync>
    #[method]
    pub fn datasync<'r>(&self, scope: &'r Scope<'_>) -> Result<js::Promise<'r>, ExnThrown> {
        self.run(scope, "fdatasync", "fdatasync", &[])
    }

    /// <https://nodejs.org/api/fs.html#filehandlechmodmode>
    #[method]
    pub fn chmod<'r>(
        &self,
        scope: &'r Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'r>, ExnThrown> {
        self.run(scope, "fchmod", "fchmod", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandlechownuid-gid>
    #[method]
    pub fn chown<'r>(
        &self,
        scope: &'r Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'r>, ExnThrown> {
        self.run(scope, "fchown", "fchown", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandleutimesatime-mtime>
    #[method]
    pub fn utimes<'r>(
        &self,
        scope: &'r Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<js::Promise<'r>, ExnThrown> {
        self.run(scope, "futimes", "futimes", &args)
    }

    /// <https://nodejs.org/api/fs.html#filehandlecreatereadstreamoptions>
    #[method]
    pub fn create_read_stream<'r>(
        &self,
        scope: &'r Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'r>, ExnThrown> {
        let class = super::streams::read_stream_class::class_constructor(scope)?;
        super::streams::with_fd_stream(scope, class, self.as_object(), options)
    }

    /// <https://nodejs.org/api/fs.html#filehandlecreatewritestreamoptions>
    #[method]
    pub fn create_write_stream<'r>(
        &self,
        scope: &'r Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'r>, ExnThrown> {
        let class = super::streams::write_stream_class::class_constructor(scope)?;
        super::streams::with_fd_stream(scope, class, self.as_object(), options)
    }
}

impl<'s> FileHandle<'s> {
    /// Add a reference to the descriptor, for a stream created from the handle.
    pub(super) fn retain(&self) {
        self.data_mut().refs += 1;
    }

    /// Release a stream's reference to the descriptor. Releasing the last one sets `fd` to -1 and
    /// closes the descriptor, as `close()` does, and settles the promise of a pending `close()`
    /// with the outcome.
    pub(super) fn release(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let (fd, refs) = {
            let mut data = self.data_mut();
            data.refs = data.refs.saturating_sub(1);
            (data.fd, data.refs)
        };
        if refs > 0 || fd == -1 {
            return Ok(());
        }
        let closing = self
            .data_mut()
            .closing
            .take()
            .map(|closing| closing.take(scope));
        // A close refused for blocking host I/O leaves the handle open.
        if !super::close_blocks(fd)
            || core_runtime::event_loop::blocking_host_io_refusal().is_none()
        {
            self.closed();
        }
        let fd = scope.root_value(js::value::from_f64(f64::from(fd)));
        let promise = run_promise(scope, "close", &[fd], None)?;
        if let Some(closing) = closing {
            closing.resolve(scope, promise)?;
        }
        Ok(())
    }

    /// Mark the handle closed: `fd` becomes -1, and the handle no longer refers to its file.
    fn closed(&self) {
        let mut data = self.data_mut();
        data.fd = -1;
        data.file = None;
    }

    /// Run the descriptor operation `name` with this handle's descriptor followed by `args`. On a
    /// closed handle, the promise rejects with an `EBADF` error with `syscall`.
    fn run<'r>(
        &self,
        scope: &'r Scope<'_>,
        name: &str,
        syscall: &str,
        args: &[HandleValue<'_>],
    ) -> Result<js::Promise<'r>, ExnThrown> {
        let fd = self.data().fd;
        if fd == -1 {
            let error = NodeError::new(ErrorKind::Error, "EBADF", "file closed");
            let error = error.to_value(scope)?;
            let object = Object::from_value(scope, *error).map_err(|e| e.throw(scope))?;
            object.set_property(scope, c"syscall", syscall)?;
            return js::Promise::new_rejected_with_error(scope, error);
        }
        let mut values = vec![scope.root_value(js::value::from_f64(f64::from(fd)))];
        values.extend_from_slice(args);
        run_promise(scope, name, &values, None)
    }
}

/// Register `FileHandle`, and define `FileHandle.prototype[Symbol.asyncDispose]`, which closes
/// the handle.
pub(super) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    FileHandle::add_to_global(scope, global);
    let proto = js::class::get_prototype_object_for::<FileHandleImpl>(scope)
        .expect("FileHandle was registered above");
    let dispose = Function::new_callback(scope, c"[Symbol.asyncDispose]", 0, async_dispose, ())?;
    js::class::define_well_known_symbol_property(
        scope,
        proto,
        js::native::SymbolCode::asyncDispose,
        *dispose,
    )
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
                } else if name == "watch" {
                    Function::new_callback(scope, c"watch", 1, super::watch::promises_watch, ())?
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
