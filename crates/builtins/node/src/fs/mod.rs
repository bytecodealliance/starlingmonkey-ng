// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:fs` and `node:fs/promises` over the host filesystem. See <https://nodejs.org/api/fs.html>.
//!
//! Each operation is defined once, in [`ops`], as a function that turns the arguments into a
//! blocking operation and a conversion of its result. That definition provides the operation's
//! synchronous function, callback function and promise function. The blocking operations run on
//! `platform::fs::offload`'s worker threads natively, and block the calling thread on WASI.
//! On WASI, the callback and promise functions of some operations run an asynchronous operation
//! over `platform::fs::wasi` instead. Synchronous functions, and on WASI the other functions
//! that block, throw `ERR_BLOCKING_NOT_ALLOWED` where blocking host I/O isn't allowed.
//!
//! File descriptors index a table of open files, starting at 3. Descriptors 0, 1 and 2 read from
//! stdin and write to stdout and stderr. On WASI, the files that asynchronous `open` calls open
//! are `platform::fs::wasi` files, which the synchronous functions block on.

mod classes;
mod cp;
mod ops;
mod promises;
mod streams;
mod watch;

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::sync::Arc;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::RootedHeap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Function, Object};
use platform::fs::{FileKind, FsError, Stat};

use crate::buffer::{buffer_from_vec, decode_bytes, encode_string, new_buffer, Encoding};
use crate::errors::{validate_function, validate_integer, NodeError, Thrown};

pub(crate) use classes::{new_dirent, DIRENT_PROPERTIES, STATS_PROPERTIES};
pub(crate) use promises::register_promises;
pub(crate) use watch::{active_watchers, is_poll_timer};

/// How an operation reports its result: returned from a synchronous function, passed to a
/// callback, or fulfilling a promise.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    Sync,
    Callback,
    Promise,
}

/// A filesystem error with the system call and the paths Node.js reports with it.
pub(crate) struct UvError {
    error: FsError,
    syscall: &'static str,
    path: Option<String>,
    dest: Option<String>,
    /// Whether this is the `ERR_FS_EISDIR` `SystemError` of `rm` without `recursive`.
    rm_directory: bool,
    /// Whether `errno` is reported positive, as Node's C++ `rm` reports it.
    positive_errno: bool,
    /// Whether the error is reported as Node's C++ `ErrnoException`: with a positive `errno`, and
    /// the message `CODE, strerror 'path'`, which omits the system call.
    errno_exception: bool,
    /// The details of a network error, which has the message `syscall CODE` followed by its
    /// `suffix`.
    net: Option<Box<NetDetail>>,
}

/// The message suffix and the extra properties of a network error.
#[derive(Default)]
pub(crate) struct NetDetail {
    suffix: String,
    address: Option<String>,
    port: Option<u16>,
    hostname: Option<String>,
}

impl UvError {
    pub(crate) fn new(error: impl Into<FsError>, syscall: &'static str) -> Self {
        UvError {
            error: error.into(),
            syscall,
            path: None,
            dest: None,
            rm_directory: false,
            positive_errno: false,
            errno_exception: false,
            net: None,
        }
    }

    /// The network error `error` of `syscall`, with the message `syscall CODE`. `ENOTFOUND` and
    /// `EAI_AGAIN` have libuv's `getaddrinfo` error numbers.
    pub(crate) fn net(error: &platform::net::NetError, syscall: &'static str) -> Self {
        Self::net_code(error.code(), syscall)
    }

    /// The network error with the POSIX error name `code` of `syscall`, with the message
    /// `syscall CODE`, as [`UvError::net`] describes it.
    pub(crate) fn net_code(code: &'static str, syscall: &'static str) -> Self {
        let error = match code {
            "ENOTFOUND" => FsError {
                code,
                errno: -3008,
                description: "unknown node or service",
            },
            "EAI_AGAIN" => FsError {
                code,
                errno: -3001,
                description: "temporary failure",
            },
            code => FsError::from_code(code),
        };
        UvError {
            net: Some(Box::default()),
            ..Self::new(error, syscall)
        }
    }

    /// The network error with `suffix` after the code in its message.
    pub(crate) fn suffix(mut self, suffix: String) -> Self {
        if let Some(net) = &mut self.net {
            net.suffix = suffix;
        }
        self
    }

    /// The network error with the `address` and `port` properties of `addr`.
    pub(crate) fn addr(mut self, addr: std::net::SocketAddr) -> Self {
        if let Some(net) = &mut self.net {
            net.address = Some(addr.ip().to_string());
            net.port = Some(addr.port());
        }
        self
    }

    /// The network error as Node.js's `ExceptionWithHostPort` reports it for `address` and
    /// `port`: the message ends in ` address:port` for a port above 0, and in ` address`
    /// otherwise, if there is an address. `address` is the address property if given, and `port`
    /// the port property if it isn't 0.
    pub(crate) fn host_port(mut self, address: Option<&str>, port: u16) -> Self {
        if let Some(net) = &mut self.net {
            net.suffix = match address {
                Some(address) if port > 0 => format!(" {address}:{port}"),
                Some(address) if !address.is_empty() => format!(" {address}"),
                _ => String::new(),
            };
            net.address = address.map(str::to_string);
            net.port = (port > 0).then_some(port);
        }
        self
    }

    /// The network error with the `hostname` property `hostname`.
    pub(crate) fn hostname(mut self, hostname: &str) -> Self {
        if let Some(net) = &mut self.net {
            net.hostname = Some(hostname.into());
        }
        self
    }

    /// The error with a positive `errno`.
    pub(crate) fn positive_errno(mut self) -> Self {
        self.positive_errno = true;
        self
    }

    /// The error as Node's C++ `ErrnoException` reports it.
    pub(crate) fn errno_exception(mut self) -> Self {
        self.errno_exception = true;
        self
    }

    /// The `ERR_FS_EISDIR` `SystemError` of `rm` without `recursive` on the directory `path`.
    pub(crate) fn rm_directory(path: &str) -> Self {
        UvError {
            rm_directory: true,
            ..Self::code("EISDIR", "rm").path(path)
        }
    }

    pub(crate) fn code(code: &str, syscall: &'static str) -> Self {
        Self::new(FsError::from_code(code), syscall)
    }

    pub(crate) fn path(mut self, path: &str) -> Self {
        self.path = Some(path.into());
        self
    }

    pub(crate) fn dest(mut self, dest: &str) -> Self {
        self.dest = Some(dest.into());
        self
    }

    /// The error as a JS `Error`, such as `ENOENT: no such file or directory, open 'a'`, with
    /// `errno`, `code`, `syscall` and, where known, `path` and `dest`.
    pub(crate) fn into_value<'s>(self, scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
        if let Some(net) = &self.net {
            let message = format!("{} {}{}", self.syscall, self.error.code, net.suffix);
            let error = js::error::new_error(scope, js::error::JSExnType::JSEXN_ERR, &message)?;
            let object = Object::from_value(scope, *error).map_err(|e| e.throw(scope))?;
            object.set_property(scope, c"errno", self.error.errno)?;
            object.set_property(scope, c"code", self.error.code)?;
            object.set_property(scope, c"syscall", self.syscall)?;
            if let Some(hostname) = &net.hostname {
                object.set_property(scope, c"hostname", hostname.as_str())?;
            }
            if let Some(address) = &net.address {
                object.set_property(scope, c"address", address.as_str())?;
            }
            if let Some(port) = net.port {
                object.set_property(scope, c"port", f64::from(port))?;
            }
            return Ok(error);
        }
        if self.rm_directory {
            let path = self.path.unwrap_or_default();
            let message =
                format!("Path is a directory: rm returned EISDIR (is a directory) {path}");
            return crate::errors::system_error(scope, "ERR_FS_EISDIR", message, |info| {
                info.set_property(scope, c"code", "EISDIR")?;
                info.set_property(scope, c"message", "is a directory")?;
                info.set_property(scope, c"path", path.as_str())?;
                info.set_property(scope, c"syscall", "rm")?;
                info.set_property(scope, c"errno", -self.error.errno)
            });
        }
        if self.errno_exception {
            let mut message = format!("{}, {}", self.error.code, strerror(-self.error.errno));
            if let Some(path) = &self.path {
                message.push_str(&format!(" '{path}'"));
            }
            let error = js::error::new_error(scope, js::error::JSExnType::JSEXN_ERR, &message)?;
            let object = Object::from_value(scope, *error).map_err(|e| e.throw(scope))?;
            object.set_property(scope, c"errno", -self.error.errno)?;
            object.set_property(scope, c"code", self.error.code)?;
            if let Some(path) = &self.path {
                object.set_property(scope, c"path", path.as_str())?;
            }
            object.set_property(scope, c"syscall", self.syscall)?;
            return Ok(error);
        }
        let mut message = format!(
            "{}: {}, {}",
            self.error.code, self.error.description, self.syscall
        );
        if let Some(path) = &self.path {
            message.push_str(&format!(" '{path}'"));
        }
        if let Some(dest) = &self.dest {
            message.push_str(&format!(" -> '{dest}'"));
        }
        let error = js::error::new_error(scope, js::error::JSExnType::JSEXN_ERR, &message)?;
        let object = Object::from_value(scope, *error).map_err(|e| e.throw(scope))?;
        let errno = match self.positive_errno {
            true => -self.error.errno,
            false => self.error.errno,
        };
        object.set_property(scope, c"errno", errno)?;
        object.set_property(scope, c"code", self.error.code)?;
        object.set_property(scope, c"syscall", self.syscall)?;
        if let Some(path) = &self.path {
            object.set_property(scope, c"path", path.as_str())?;
        }
        if let Some(dest) = &self.dest {
            object.set_property(scope, c"dest", dest.as_str())?;
        }
        Ok(error)
    }
}

/// The host's description of the OS error number `errno`, as strerror(3) gives it.
fn strerror(errno: i32) -> String {
    let text = std::io::Error::from_raw_os_error(errno).to_string();
    match text.rfind(" (os error ") {
        Some(end) => text[..end].to_string(),
        None => text,
    }
}

impl ThrowException for UvError {
    fn throw(self, scope: &Scope<'_>) -> ExnThrown {
        if let Ok(error) = self.into_value(scope) {
            js::exception::set_pending(scope, error, js::native::ExceptionStackBehavior::Capture);
        }
        ExnThrown
    }
}

/// An entry of a directory listing.
pub(crate) struct Entry {
    name: String,
    parent: String,
    kind: FileKind,
}

/// The result of a blocking operation.
pub(crate) enum Output {
    Unit,
    Bytes(Vec<u8>),
    Count(usize),
    Stat(Option<Stat>),
    Bool(bool),
    Text(String),
    /// A path that may not exist, such as the first directory `mkdir` created.
    MaybeText(Option<String>),
    Names(Vec<String>),
    Entries(Vec<Entry>),
    /// A file `open` opened, which [`convert`] adds to the table of open files.
    File(File),
    /// A file an asynchronous `open` opened, which [`convert`] adds to the table of open files.
    #[cfg(target_arch = "wasm32")]
    WasiFile(platform::fs::wasi::File),
}

/// A blocking operation, which runs off the JS thread natively.
type Run = Box<dyn FnOnce() -> Result<Output, UvError> + Send>;

/// An operation that suspends instead of blocking, on WASI.
#[cfg(target_arch = "wasm32")]
type RunAsync = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Output, UvError>>>>;

/// How an operation runs.
pub(crate) enum Work {
    Blocking(Run),
    #[cfg(target_arch = "wasm32")]
    Async(RunAsync),
}

/// A conversion of an operation's result to the values the caller receives, for the variant
/// it was called as.
type Finish =
    Box<dyn for<'s> FnOnce(&'s Scope<'_>, Output, Variant) -> Result<Vec<HandleValue<'s>>, Thrown>>;

/// An operation ready to run.
pub(crate) struct Prepared {
    work: Work,
    finish: Finish,
}

impl Prepared {
    /// `run`, with its result converted by [`convert`] with `encoding`, where `None` makes bytes
    /// and paths `Buffer`s.
    pub(crate) fn new(
        encoding: Option<Encoding>,
        run: impl FnOnce() -> Result<Output, UvError> + Send + 'static,
    ) -> Self {
        Prepared {
            work: Work::Blocking(Box::new(run)),
            finish: Box::new(move |scope, output, _| Ok(convert(scope, output, encoding)?)),
        }
    }

    /// `run`, with its result converted by `finish`.
    pub(crate) fn custom(
        run: impl FnOnce() -> Result<Output, UvError> + Send + 'static,
        finish: impl for<'s> FnOnce(&'s Scope<'_>, Output, Variant) -> Result<Vec<HandleValue<'s>>, Thrown>
            + 'static,
    ) -> Self {
        Prepared {
            work: Work::Blocking(Box::new(run)),
            finish: Box::new(finish),
        }
    }

    /// The asynchronous operation `run`, with its result converted by [`convert`] with
    /// `encoding`.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn future(
        encoding: Option<Encoding>,
        run: impl std::future::Future<Output = Result<Output, UvError>> + 'static,
    ) -> Self {
        Prepared {
            work: Work::Async(Box::pin(run)),
            finish: Box::new(move |scope, output, _| Ok(convert(scope, output, encoding)?)),
        }
    }

    /// The asynchronous operation `run`, with its result converted by `finish`.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn custom_future(
        run: impl std::future::Future<Output = Result<Output, UvError>> + 'static,
        finish: impl for<'s> FnOnce(&'s Scope<'_>, Output, Variant) -> Result<Vec<HandleValue<'s>>, Thrown>
            + 'static,
    ) -> Self {
        Prepared {
            work: Work::Async(Box::pin(run)),
            finish: Box::new(finish),
        }
    }
}

/// `text` in `encoding`, or as a `Buffer` of its UTF-8 bytes for `None`.
pub(crate) fn text_value<'s>(
    scope: &'s Scope<'_>,
    text: &str,
    encoding: Option<Encoding>,
) -> Result<HandleValue<'s>, ExnThrown> {
    match encoding {
        Some(Encoding::Utf8) => js::JSString::from_str(scope, text)?.to_jsval_throwing(scope),
        Some(encoding) => decode_bytes(scope, text.as_bytes(), encoding)?.to_jsval_throwing(scope),
        None => new_buffer(scope, text.as_bytes())?.to_jsval_throwing(scope),
    }
}

/// The JS value of `output`: nothing for `Unit`, bytes and paths in `encoding` or as `Buffer`s,
/// `Stats` objects, `Dirent` objects for entries, and numbers and booleans as such.
fn convert<'s>(
    scope: &'s Scope<'_>,
    output: Output,
    encoding: Option<Encoding>,
) -> Result<Vec<HandleValue<'s>>, ExnThrown> {
    let value = match output {
        Output::Unit => return Ok(Vec::new()),
        Output::Bytes(bytes) => match encoding {
            Some(encoding) => decode_bytes(scope, &bytes, encoding)?.to_jsval_throwing(scope)?,
            None => buffer_from_vec(scope, bytes)?.to_jsval_throwing(scope)?,
        },
        Output::Count(count) => scope.root_value(js::value::from_f64(count as f64)),
        Output::Stat(None) => HandleValue::undefined(),
        Output::Stat(Some(stat)) => classes::stats_value(scope, &stat)?,
        Output::Bool(value) => scope.root_value(js::value::from_bool(value)),
        Output::Text(text) => text_value(scope, &text, encoding)?,
        Output::MaybeText(None) => HandleValue::undefined(),
        Output::MaybeText(Some(text)) => text_value(scope, &text, encoding)?,
        Output::Names(names) => {
            let names = names
                .iter()
                .map(|name| text_value(scope, name, encoding))
                .collect::<Result<Vec<_>, _>>()?;
            js::Array::with_contents(scope, &names)?.to_jsval_throwing(scope)?
        }
        Output::File(file) => scope.root_value(js::value::from_f64(f64::from(insert_file(file)))),
        #[cfg(target_arch = "wasm32")]
        Output::WasiFile(file) => {
            let fd = insert_handle(Handle::Wasi(Arc::new(file)));
            scope.root_value(js::value::from_f64(f64::from(fd)))
        }
        Output::Entries(entries) => {
            let dirents = entries
                .iter()
                .map(|entry| classes::dirent_value(scope, entry, encoding))
                .collect::<Result<Vec<_>, _>>()?;
            js::Array::with_contents(scope, &dirents)?.to_jsval_throwing(scope)?
        }
    };
    Ok(vec![value])
}

// ============================================================================
// Arguments
// ============================================================================

/// The path the argument `value` names: a string, a `Buffer` or `Uint8Array` of UTF-8, or a
/// `file:` `URL`.
pub(crate) fn path_arg(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<String, Thrown> {
    let invalid = || {
        NodeError::invalid_arg_type(
            scope,
            name,
            "of type string or an instance of Buffer or URL",
            value,
        )
    };
    let path = if value.is_string() {
        String::from_jsval(scope, value, ())?
    } else {
        let object = Object::from_value(scope, *value).map_err(|_| invalid())?;
        if let Some(view) = js::ArrayBufferView::from_object(object) {
            view.with_bytes(|bytes| String::from_utf8_lossy(bytes).into_owned())
        } else if crate::url::is_url(scope, value)? {
            crate::url::file_url_to_path(scope, object)?
        } else {
            return Err(invalid().into());
        }
    };
    if path.contains('\0') {
        return Err(NodeError::invalid_arg_value(
            scope,
            name,
            value,
            "must be a string, Uint8Array, or URL without null bytes",
        )
        .into());
    }
    Ok(path)
}

/// A file named by a path or by a file descriptor.
pub(crate) enum Target {
    Path(String),
    Fd(i32),
}

/// The file the argument `value` names: a file descriptor, which is a non-negative 32-bit
/// integer, a `FileHandle`, or a path.
pub(crate) fn target_arg(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<Target, Thrown> {
    let number = value.is_number().then(|| value.to_number());
    if let Some(fd) =
        number.filter(|number| number.fract() == 0.0 && (0.0..=2147483647.0).contains(number))
    {
        return Ok(Target::Fd(fd as i32));
    }
    if let Some(fd) = promises::file_handle_fd(scope, value)? {
        return Ok(Target::Fd(fd));
    }
    Ok(Target::Path(path_arg(scope, value, name)?))
}

/// The file descriptor the argument `value` is: an integer from 0 to 2147483647.
pub(crate) fn fd_arg(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<i32, Thrown> {
    validate_integer(scope, value, "fd", 0.0, 2147483647.0).map(|fd| fd as i32)
}

/// `options[name]`, if `options` is an object and the property isn't `undefined`.
pub(crate) fn option<'s>(
    scope: &'s Scope<'_>,
    options: Option<HandleValue<'_>>,
    name: &'static std::ffi::CStr,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    let Some(object) = options.and_then(|options| Object::from_value(scope, *options).ok()) else {
        return Ok(None);
    };
    let value = object.get_property(scope, name)?;
    Ok((!value.is_undefined()).then_some(value))
}

/// Whether `options[name]` is truthy.
pub(crate) fn flag_option(
    scope: &Scope<'_>,
    options: Option<HandleValue<'_>>,
    name: &'static std::ffi::CStr,
) -> Result<bool, ExnThrown> {
    Ok(option(scope, options, name)?.is_some_and(|value| js::conversion::to_boolean(value)))
}

/// `options[name]`, which must be a boolean if it isn't `undefined`, as Node's `validateBoolean`
/// checks it, with errors naming `label`. `undefined` is `false`.
pub(crate) fn boolean_option(
    scope: &Scope<'_>,
    options: Option<HandleValue<'_>>,
    name: &'static std::ffi::CStr,
    label: &str,
) -> Result<bool, Thrown> {
    match option(scope, options, name)? {
        Some(value) => crate::errors::validate_boolean(scope, value, label),
        None => Ok(false),
    }
}

/// The encoding of `options`, which is the encoding's name or an object with an `encoding`
/// property: `None` for `buffer`, and `default` if there is none. Other values than these,
/// `undefined` and `null` throw `ERR_INVALID_ARG_TYPE`.
pub(crate) fn encoding_option(
    scope: &Scope<'_>,
    options: Option<HandleValue<'_>>,
    default: Option<Encoding>,
) -> Result<Option<Encoding>, Thrown> {
    let value = match options {
        Some(options) if options.is_string() => Some(options),
        Some(options) if !options.is_object() && !options.is_null_or_undefined() => {
            return Err(NodeError::invalid_arg_type(
                scope,
                "options",
                "one of type string or object",
                options,
            )
            .into())
        }
        _ => option(scope, options, c"encoding")?,
    };
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(default);
    };
    let name = String::from_jsval(scope, value, ())?;
    if name == "buffer" {
        return Ok(None);
    }
    match Encoding::parse(&name) {
        Some(encoding) => Ok(Some(encoding)),
        None => Err(
            NodeError::invalid_arg_value(scope, "encoding", value, "is invalid encoding").into(),
        ),
    }
}

/// The mode the argument `value` gives, a 32-bit unsigned integer or a string of octal digits, as
/// Node's `parseFileMode` reads it. A missing, `undefined` or `null` value gives `default`, and
/// without a default throws `ERR_INVALID_ARG_TYPE`.
pub(crate) fn mode_arg(
    scope: &Scope<'_>,
    value: Option<HandleValue<'_>>,
    default: Option<u32>,
) -> Result<u32, Thrown> {
    let value = value.filter(|value| !value.is_undefined() && !value.is_null());
    let value = match (value, default) {
        (Some(value), _) => value,
        (None, Some(default)) => return Ok(default),
        (None, None) => HandleValue::undefined(),
    };
    if value.is_string() {
        let text = String::from_jsval(scope, value, ())?;
        if text.is_empty() || !text.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
            return Err(NodeError::invalid_arg_value(
                scope,
                "mode",
                value,
                "must be a 32-bit unsigned integer or an octal string",
            )
            .into());
        }
        let mode = text
            .bytes()
            .fold(0.0, |mode, digit| mode * 8.0 + f64::from(digit - b'0'));
        if mode > 4294967295.0 {
            return Err(NodeError::out_of_range(
                "mode",
                ">= 0 && <= 4294967295",
                &crate::errors::out_of_range_received(mode),
            )
            .into());
        }
        return Ok(mode as u32);
    }
    Ok(validate_integer(scope, value, "mode", 0.0, 4294967295.0)? as u32)
}

/// How to open a file.
#[derive(Clone, Copy, Default)]
pub(crate) struct OpenFlags {
    read: bool,
    write: bool,
    append: bool,
    create: bool,
    truncate: bool,
    exclusive: bool,
    /// The `O_*` bits other than the access mode, for flags given as a number. On Unix, these
    /// replace `append`, `create`, `truncate` and `exclusive`.
    #[cfg_attr(not(unix), allow(dead_code))]
    other_bits: Option<i32>,
}

impl OpenFlags {
    /// `options` for these flags and the mode `mode`.
    fn options(self, mode: u32) -> std::fs::OpenOptions {
        let mut options = std::fs::OpenOptions::new();
        #[cfg(unix)]
        if let Some(bits) = self.other_bits {
            options.read(self.read).write(self.write);
            std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, bits);
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
            return options;
        }
        options
            .read(self.read)
            .write(self.write && !self.append)
            .append(self.append)
            .truncate(self.truncate);
        if self.exclusive && self.create {
            options.create_new(true);
        } else {
            options.create(self.create);
        }
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
        #[cfg(not(unix))]
        let _ = mode;
        options
    }

    /// Open `path` with these flags and `mode`.
    pub(crate) fn open(self, path: &str, mode: u32) -> Result<File, UvError> {
        self.options(mode)
            .open(path)
            .map_err(|error| UvError::new(error, "open").path(path))
    }

    /// Open `path` with these flags as a `platform::fs::wasi` file. WASI has no modes.
    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn open_wasi(self, path: &str) -> Result<platform::fs::wasi::File, UvError> {
        let options = platform::fs::wasi::OpenOptions {
            read: self.read,
            write: self.write && !self.append,
            append: self.append,
            truncate: self.truncate,
            create: self.create && !self.exclusive,
            create_new: self.create && self.exclusive,
        };
        platform::fs::wasi::File::open(path, options)
            .await
            .map_err(|error| UvError::new(error, "open").path(path))
    }
}

/// The flags the argument `value` gives, a string such as `'r+'` or a number of `O_*` constants,
/// or `default`.
pub(crate) fn flags_arg(
    scope: &Scope<'_>,
    value: Option<HandleValue<'_>>,
    default: &str,
) -> Result<OpenFlags, Thrown> {
    let value = value.filter(|value| !value.is_undefined() && !value.is_null());
    let invalid = |value: HandleValue<'_>| -> Thrown {
        NodeError::invalid_arg_value(scope, "flags", value, "is invalid").into()
    };
    let text = match value {
        Some(value) if value.is_number() => {
            let bits = validate_integer(scope, value, "flags", -2147483648.0, 2147483647.0)?;
            return numeric_flags(bits as i32);
        }
        Some(value) if value.is_string() => String::from_jsval(scope, value, ())?,
        Some(value) => return Err(invalid(value)),
        None => default.to_string(),
    };
    let flags = |read, write, append, create, truncate, exclusive| OpenFlags {
        read,
        write,
        append,
        create,
        truncate,
        exclusive,
        other_bits: None,
    };
    Ok(match text.as_str() {
        "r" | "rs" | "sr" => flags(true, false, false, false, false, false),
        "r+" | "rs+" | "sr+" => flags(true, true, false, false, false, false),
        "w" => flags(false, true, false, true, true, false),
        "wx" | "xw" => flags(false, true, false, true, true, true),
        "w+" => flags(true, true, false, true, true, false),
        "wx+" | "xw+" => flags(true, true, false, true, true, true),
        "a" | "as" | "sa" => flags(false, true, true, true, false, false),
        "ax" | "xa" => flags(false, true, true, true, false, true),
        "a+" | "as+" | "sa+" => flags(true, true, true, true, false, false),
        "ax+" | "xa+" => flags(true, true, true, true, false, true),
        _ => {
            return Err(match value {
                Some(value) => invalid(value),
                None => invalid(HandleValue::undefined()),
            })
        }
    })
}

/// The flags the `O_*` constants in `bits` give.
fn numeric_flags(bits: i32) -> Result<OpenFlags, Thrown> {
    let constant = |name: &str| {
        platform::fs::open_flags()
            .into_iter()
            .find(|(constant, _)| *constant == name)
            .map_or(0, |(_, value)| value)
    };
    let access = bits & 3;
    Ok(OpenFlags {
        read: access == constant("O_RDONLY") || access == constant("O_RDWR"),
        write: access == constant("O_WRONLY") || access == constant("O_RDWR"),
        append: bits & constant("O_APPEND") != 0,
        create: bits & constant("O_CREAT") != 0,
        truncate: bits & constant("O_TRUNC") != 0,
        exclusive: bits & constant("O_EXCL") != 0,
        other_bits: Some(bits & !3),
    })
}

/// The bytes of the argument `value`: a string in `encoding`, or the bytes of a `Buffer`,
/// `TypedArray` or `DataView`.
pub(crate) fn data_arg(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    encoding: Encoding,
) -> Result<Vec<u8>, Thrown> {
    if value.is_string() {
        let string = js::JSString::from_value(scope, value)?;
        return Ok(encode_string(scope, string, encoding)?);
    }
    let view = Object::from_value(scope, *value)
        .ok()
        .and_then(js::ArrayBufferView::from_object);
    if let Some(view) = view {
        return Ok(view.copy_bytes(scope)?);
    }
    Err(NodeError::invalid_arg_type(
        scope,
        "data",
        "of type string or an instance of Buffer, TypedArray, or DataView",
        value,
    )
    .into())
}

// ============================================================================
// File descriptors
// ============================================================================

js::instance_local! {
    /// The open files by file descriptor, each a `Handle::File` or `Handle::Wasi`.
    static FILES: RefCell<HashMap<i32, Handle>> = RefCell::new(HashMap::new());
}

/// Add `file` to the table and return its descriptor, the lowest one from 3 that is free.
pub(crate) fn insert_file(file: File) -> i32 {
    insert_handle(Handle::File(Arc::new(file)))
}

/// Add the open file `handle` to the table and return its descriptor, the lowest one from 3 that
/// is free.
fn insert_handle(handle: Handle) -> i32 {
    FILES.with(|files| {
        let mut files = files.borrow_mut();
        let fd = (3..)
            .find(|fd| !files.contains_key(fd))
            .expect("a descriptor is free");
        files.insert(fd, handle);
        fd
    })
}

/// Whether a callback or promise `close` of `fd` is refused where blocking host I/O isn't
/// allowed: on WASI, for a `std::fs` file, and natively never.
pub(crate) fn close_blocks(fd: i32) -> bool {
    cfg!(target_arch = "wasm32") && matches!(Handle::of(fd, "close"), Ok(Handle::File(_)))
}

/// The host's descriptor of the file open as `fd`, which isn't 0, 1 or 2. `None` if `fd` isn't
/// open, and on hosts without descriptors.
pub(crate) fn host_fd(fd: i32) -> Option<i32> {
    #[cfg(unix)]
    if let Ok(Handle::File(file)) = Handle::of(fd, "spawn") {
        return Some(std::os::fd::AsRawFd::as_raw_fd(&*file));
    }
    let _ = fd;
    None
}

/// Remove `fd` from the table, or return `EBADF`. The file closes once the operations on it that
/// are still running finish. Descriptors 0, 1 and 2 stay open.
pub(crate) fn remove_file(fd: i32) -> Result<(), UvError> {
    if (0..=2).contains(&fd) {
        return Ok(());
    }
    FILES
        .with(|files| files.borrow_mut().remove(&fd))
        .map(|_| ())
        .ok_or_else(|| UvError::code("EBADF", "close"))
}

/// Remove `fd` from the table if it still refers to `file`, for a `FileHandle` finalized while
/// open. Does nothing while the table is in use or once the thread has destroyed it.
pub(crate) fn remove_file_of(fd: i32, file: &WeakHandle) {
    let _ = FILES.try_with(|files| {
        let Ok(mut files) = files.try_borrow_mut() else {
            return;
        };
        if files.get(&fd).is_some_and(|handle| file.refers_to(handle)) {
            files.remove(&fd);
        }
    });
}

/// A reference to the file of a [`Handle`] that doesn't keep the file open.
pub(crate) enum WeakHandle {
    File(std::sync::Weak<File>),
    #[cfg(target_arch = "wasm32")]
    Wasi(std::sync::Weak<platform::fs::wasi::File>),
}

impl WeakHandle {
    /// Whether `handle` is the file this refers to.
    fn refers_to(&self, handle: &Handle) -> bool {
        match (self, handle) {
            (WeakHandle::File(weak), Handle::File(file)) => {
                std::ptr::eq(weak.as_ptr(), Arc::as_ptr(file))
            }
            #[cfg(target_arch = "wasm32")]
            (WeakHandle::Wasi(weak), Handle::Wasi(file)) => {
                std::ptr::eq(weak.as_ptr(), Arc::as_ptr(file))
            }
            _ => false,
        }
    }
}

/// The file a file descriptor refers to.
#[derive(Clone)]
pub(crate) enum Handle {
    File(Arc<File>),
    /// A file an asynchronous `open` opened. Synchronous operations on it block on its futures.
    #[cfg(target_arch = "wasm32")]
    Wasi(Arc<platform::fs::wasi::File>),
    Stdin,
    Stdout,
    Stderr,
}

/// Standard input for reads of descriptor 0. On Unix, it reads the descriptor itself, without a
/// buffer, so a read takes only the bytes it returns, as Node's `read(2)` does.
#[cfg(unix)]
fn stdin_reader() -> std::mem::ManuallyDrop<std::fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: `ManuallyDrop` keeps the file from closing descriptor 0, which the process owns. A
    // read of a closed descriptor fails with `EBADF`.
    std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) })
}

/// Standard input for reads of descriptor 0.
#[cfg(not(unix))]
fn stdin_reader() -> std::io::Stdin {
    std::io::stdin()
}

impl Handle {
    /// The file `fd` refers to, or `EBADF` for `syscall`.
    pub(crate) fn of(fd: i32, syscall: &'static str) -> Result<Handle, UvError> {
        match fd {
            0 => Ok(Handle::Stdin),
            1 => Ok(Handle::Stdout),
            2 => Ok(Handle::Stderr),
            _ => FILES
                .with(|files| files.borrow().get(&fd).cloned())
                .ok_or_else(|| UvError::code("EBADF", syscall)),
        }
    }

    /// A [`WeakHandle`] of the file, or `None` for the standard streams.
    pub(crate) fn downgrade(&self) -> Option<WeakHandle> {
        match self {
            Handle::File(file) => Some(WeakHandle::File(Arc::downgrade(file))),
            #[cfg(target_arch = "wasm32")]
            Handle::Wasi(file) => Some(WeakHandle::Wasi(Arc::downgrade(file))),
            _ => None,
        }
    }

    /// The open `std::fs` file, or `EINVAL` for `syscall` for the standard streams.
    pub(crate) fn file(&self, syscall: &'static str) -> Result<&File, UvError> {
        match self {
            Handle::File(file) => Ok(file),
            _ => Err(UvError::code("EINVAL", syscall)),
        }
    }

    /// `Ok` for an open file of either kind, or `EINVAL` for `syscall` for the standard streams.
    #[cfg_attr(unix, allow(dead_code))]
    pub(crate) fn check_file(&self, syscall: &'static str) -> Result<(), UvError> {
        #[cfg(target_arch = "wasm32")]
        if let Handle::Wasi(_) = self {
            return Ok(());
        }
        self.file(syscall).map(|_| ())
    }

    /// The `platform::fs::wasi` file, if this is one.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn wasi(&self) -> Option<&Arc<platform::fs::wasi::File>> {
        match self {
            Handle::Wasi(file) => Some(file),
            _ => None,
        }
    }

    /// Read into `buffer` at `position`, or at the current position for `None`.
    pub(crate) fn read(&self, buffer: &mut [u8], position: Option<u64>) -> Result<usize, UvError> {
        use std::io::Read;
        let result = match (self, position) {
            (Handle::File(file), Some(position)) => platform::fs::read_at(file, buffer, position),
            (Handle::File(file), None) => (&**file).read(buffer),
            #[cfg(target_arch = "wasm32")]
            (Handle::Wasi(file), _) => {
                platform::block_on(file.read(buffer.len(), position)).map(|bytes| {
                    buffer[..bytes.len()].copy_from_slice(&bytes);
                    bytes.len()
                })
            }
            (Handle::Stdin, _) => stdin_reader().read(buffer),
            _ => return Err(UvError::code("EBADF", "read")),
        };
        result.map_err(|error| UvError::new(error, "read"))
    }

    /// Read everything from the current position to the end.
    pub(crate) fn read_to_end(&self) -> Result<Vec<u8>, UvError> {
        use std::io::Read;
        let mut bytes = Vec::new();
        let result = match self {
            Handle::File(file) => (&**file).read_to_end(&mut bytes),
            #[cfg(target_arch = "wasm32")]
            Handle::Wasi(file) => platform::block_on(file.read_to_end()).map(|read| {
                bytes = read;
                bytes.len()
            }),
            Handle::Stdin => stdin_reader().read_to_end(&mut bytes),
            _ => return Err(UvError::code("EBADF", "read")),
        };
        result.map_err(|error| UvError::new(error, "read"))?;
        Ok(bytes)
    }

    /// Write all of `data` at `position`, or at the current position for `None`. Empty data is
    /// written with one write(2) call, which reports errors such as `EBADF`.
    pub(crate) fn write(&self, data: &[u8], position: Option<u64>) -> Result<(), UvError> {
        use std::io::Write;
        if let (Handle::File(file), true) = (self, data.is_empty()) {
            let result = match position {
                Some(position) => platform::fs::write_at(file, data, position),
                None => (&**file).write(data),
            };
            return result
                .map(|_| ())
                .map_err(|error| UvError::new(error, "write"));
        }
        let result = match (self, position) {
            (Handle::File(file), Some(mut position)) => {
                let mut data = data;
                while !data.is_empty() {
                    match platform::fs::write_at(file, data, position) {
                        Ok(0) => break,
                        Ok(written) => {
                            data = &data[written..];
                            position += written as u64;
                        }
                        Err(error) => return Err(UvError::new(error, "write")),
                    }
                }
                Ok(())
            }
            (Handle::File(file), None) => (&**file).write_all(data),
            #[cfg(target_arch = "wasm32")]
            (Handle::Wasi(file), _) => platform::block_on(file.write(data.to_vec(), position)),
            (Handle::Stdout, _) => {
                let mut stdout = std::io::stdout();
                stdout.write_all(data).and_then(|()| stdout.flush())
            }
            (Handle::Stderr, _) => std::io::stderr().write_all(data),
            (Handle::Stdin, _) => return Err(UvError::code("EBADF", "write")),
        };
        result.map_err(|error| UvError::new(error, "write"))
    }

    /// The metadata of the file or standard stream.
    pub(crate) fn stat(&self) -> Result<Stat, UvError> {
        let result = match self {
            Handle::File(file) => file.metadata().map(|metadata| Stat::from(&metadata)),
            #[cfg(target_arch = "wasm32")]
            Handle::Wasi(file) => platform::block_on(file.stat()),
            Handle::Stdin => platform::fs::stdio_stat(0),
            Handle::Stdout => platform::fs::stdio_stat(1),
            Handle::Stderr => platform::fs::stdio_stat(2),
        };
        result.map_err(|error| UvError::new(error, "fstat"))
    }
}

// ============================================================================
// Running operations
// ============================================================================

/// `ERR_BLOCKING_NOT_ALLOWED` naming `function` where blocking host I/O isn't allowed.
pub(super) fn refuse_blocking(function: &str) -> Result<(), Thrown> {
    match crate::errors::blocking_not_allowed(&format!("fs.{function}")) {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

/// Run `prepared` now and return its value, or `undefined` if it has none.
pub(crate) fn run_sync<'s>(
    scope: &'s Scope<'_>,
    function: &str,
    prepared: Prepared,
) -> Result<HandleValue<'s>, Thrown> {
    refuse_blocking(function)?;
    let result = match prepared.work {
        Work::Blocking(run) => run(),
        #[cfg(target_arch = "wasm32")]
        Work::Async(run) => platform::block_on(run),
    };
    let output = result.map_err(|error| Thrown::from(error.throw(scope)))?;
    let values = (prepared.finish)(scope, output, Variant::Sync)?;
    Ok(values
        .into_iter()
        .next()
        .unwrap_or(HandleValue::undefined()))
}

/// An interest in the current event loop, which keeps it running until the handle is dropped, or
/// `None` without an event loop.
pub(crate) fn hold_event_loop() -> Option<core_runtime::event_loop::InterestHandle> {
    core_runtime::event_loop::with_active_event_loop(|event_loop| {
        event_loop.acquire_interest_handle()
    })
}

/// The conversion of an operation's values, or of the exception its failure left pending, to the
/// value its promise settles with.
type Deliver = Box<
    dyn for<'r> FnOnce(
        &'r Scope<'_>,
        Result<Vec<HandleValue<'r>>, ExnThrown>,
    ) -> Result<HandleValue<'r>, ExnThrown>,
>;

/// Start `prepared` as `function`, and return a promise that settles with what `deliver` makes
/// of the operation's values once it finished. Blocking work throws where blocking host I/O
/// isn't allowed. Until the promise settles, the operation keeps the current event loop
/// running, as a pending libuv request does in Node.js.
fn start<'s>(
    scope: &'s Scope<'_>,
    function: &str,
    prepared: Prepared,
    variant: Variant,
    deliver: Deliver,
) -> Result<js::Promise<'s>, Thrown> {
    let Prepared { work, finish } = prepared;
    let interest = hold_event_loop();
    let settle = move |result: Result<Output, UvError>| {
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            drop(interest);
            let values = result
                .map_err(|error| error.throw(scope))
                .and_then(|output| {
                    finish(scope, output, variant).map_err(|thrown| thrown.throw(scope))
                });
            deliver(scope, values)
        }))
    };
    match work {
        Work::Blocking(run) => {
            if cfg!(target_arch = "wasm32") {
                refuse_blocking(function)?;
            }
            let promise = js::Promise::new_pending(scope)?;
            let future = async move { settle(platform::fs::offload(run).await) };
            promise.spawn_polled(PromiseFuture::new(future));
            Ok(promise)
        }
        #[cfg(target_arch = "wasm32")]
        Work::Async(run) => {
            let promise = js::Promise::new_pending(scope)?;
            promise.spawn(PromiseFuture::new(async move { settle(run.await) }));
            Ok(promise)
        }
    }
}

/// Start `prepared` as `function`, and return a promise for its first value.
pub(crate) fn run_async<'s>(
    scope: &'s Scope<'_>,
    function: &str,
    prepared: Prepared,
    variant: Variant,
) -> Result<js::Promise<'s>, Thrown> {
    let deliver: Deliver = Box::new(|_scope, values| {
        Ok(values?
            .into_iter()
            .next()
            .unwrap_or(HandleValue::undefined()))
    });
    start(scope, function, prepared, variant, deliver)
}

/// Start `prepared` as `function`, and once it finished, call `callback` with its error or with
/// `null` and its values. Without `with_error`, `callback` receives only the values.
pub(crate) fn run_callback(
    scope: &Scope<'_>,
    function: &str,
    prepared: Prepared,
    callback: HandleValue<'_>,
    with_error: bool,
) -> Result<(), Thrown> {
    let callback = RootedHeap::new(Object::from_value(scope, *callback)?);
    let deliver: Deliver = Box::new(move |scope, values| {
        let callback = callback.get(scope);
        let args = match values {
            Ok(values) if with_error => {
                std::iter::once(HandleValue::null()).chain(values).collect()
            }
            Ok(values) => values,
            Err(ExnThrown) => vec![js::exception::take_pending_or_undefined(scope)],
        };
        crate::support::call_reporting(scope, callback, &args)?;
        Ok(HandleValue::undefined())
    });
    start(scope, function, prepared, Variant::Callback, deliver)?;
    Ok(())
}

/// Start `prepared` as `function`, and once it finished, report its error as uncaught.
fn run_reporting(scope: &Scope<'_>, function: &str, prepared: Prepared) -> Result<(), Thrown> {
    let deliver: Deliver = Box::new(|scope, values| {
        if let Err(ExnThrown) = values {
            let error = js::exception::take_pending_or_undefined(scope);
            core_runtime::event_loop::timer::rethrow_in_microtask(scope, error)?;
        }
        Ok(HandleValue::undefined())
    });
    start(scope, function, prepared, Variant::Callback, deliver)?;
    Ok(())
}

/// The body of `fs.exists[util.promisify.custom](path)`, whose payload is unused: a promise that
/// resolves with whether `path` exists. An invalid `path` resolves it with `false`.
fn exists_promise(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let prepared =
        (ops::OPS[ops::index_of("exists")].parse)(scope, &[args.get(0)], Variant::Promise)
            .map_err(|thrown| thrown.throw(scope))?;
    run_async(scope, "exists", prepared, Variant::Promise)
        .map_err(|thrown| thrown.throw(scope))?
        .to_jsval_raw_throwing(scope)
}

/// The body of every function [`ops::OPS`] defines, whose payload is the index of the operation
/// times 3 plus the variant's index: parse the arguments and run the operation as the variant.
fn call_op(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let index = payload.to_number() as usize;
    let op = &ops::OPS[index / 3];
    let variant = [Variant::Sync, Variant::Callback, Variant::Promise][index % 3];
    let mut values = args.rest(0);
    let mut run = || -> Result<Value, Thrown> {
        match variant {
            Variant::Sync => {
                let prepared = (op.parse)(scope, &values, variant)?;
                Ok(run_sync(scope, op.sync, prepared)?.get())
            }
            Variant::Callback => {
                // `fs.close(fd, callback)` takes the callback from its second argument, and
                // reports a failure as uncaught when it is `undefined`.
                let callback = if op.name == "close" {
                    let callback = values.get(1).copied().filter(|value| !value.is_undefined());
                    values.truncate(1);
                    callback
                } else {
                    Some(values.pop().unwrap_or(HandleValue::undefined()))
                };
                if let Some(callback) = callback {
                    validate_function(scope, callback, "cb")?;
                }
                // `exists` calls back with `false` before returning for a path that `access`
                // rejects, as Node's `exists` catches the error of `fs.access`.
                if let (Some(callback), "exists") = (callback, op.name) {
                    let path = values.first().copied().unwrap_or(HandleValue::undefined());
                    if path_arg(scope, path, "path").is_err() {
                        js::exception::clear(scope);
                        let args = [false];
                        js::Function::call(scope, js::value::undefined(), callback, &args)?;
                        return Ok(js::value::undefined());
                    }
                }
                let prepared = (op.parse)(scope, &values, variant)?;
                match callback {
                    Some(callback) => {
                        run_callback(scope, op.name, prepared, callback, op.name != "exists")?
                    }
                    None => run_reporting(scope, op.name, prepared)?,
                }
                Ok(js::value::undefined())
            }
            Variant::Promise => {
                let promise = match (op.parse)(scope, &values, variant)
                    .and_then(|prepared| run_async(scope, op.name, prepared, variant))
                {
                    Ok(promise) => promise,
                    Err(thrown) => {
                        thrown.throw(scope);
                        js::Promise::new_rejected_with_pending_error(scope)?
                    }
                };
                Ok(promise.to_jsval_raw_throwing(scope)?)
            }
        }
    };
    run().map_err(|thrown| thrown.throw(scope))
}

/// The function of `op` for `variant`, named `name`.
fn op_function<'s>(
    scope: &'s Scope<'_>,
    index: usize,
    variant: Variant,
    name: &str,
) -> Result<HandleValue<'s>, ExnThrown> {
    let op = &ops::OPS[index];
    let variant_index = match variant {
        Variant::Sync => 0,
        Variant::Callback => 1,
        Variant::Promise => 2,
    };
    let name = std::ffi::CString::new(name).expect("names have no NULs");
    let function = Function::new_callback(
        scope,
        &name,
        op.lengths[variant_index],
        call_op,
        js::value::from_f64((index * 3 + variant_index) as f64),
    )?;
    function.to_jsval_throwing(scope)
}

/// The key of the shared object `fs.constants` is, which `fs.promises.constants` also is.
static CONSTANTS_KEY: u8 = 0;

/// `fs.constants`: an object without a prototype, with the constants in Node's order. Open flags
/// the host lacks are left out.
pub(crate) fn constants<'s>(scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    let object = js::class::get_or_init_shared_object(
        scope,
        &CONSTANTS_KEY as *const u8 as usize,
        |scope| {
            let object = Object::new_plain(scope)?;
            object.set_prototype(scope, js::native::HandleObject::null())?;
            let flags = platform::fs::open_flags();
            // `None` values are open flags, looked up in `flags`.
            let names: [(&str, Option<i32>); 61] = [
                ("UV_FS_SYMLINK_DIR", Some(1)),
                ("UV_FS_SYMLINK_JUNCTION", Some(2)),
                ("O_RDONLY", None),
                ("O_WRONLY", None),
                ("O_RDWR", None),
                ("UV_DIRENT_UNKNOWN", Some(0)),
                ("UV_DIRENT_FILE", Some(1)),
                ("UV_DIRENT_DIR", Some(2)),
                ("UV_DIRENT_LINK", Some(3)),
                ("UV_DIRENT_FIFO", Some(4)),
                ("UV_DIRENT_SOCKET", Some(5)),
                ("UV_DIRENT_CHAR", Some(6)),
                ("UV_DIRENT_BLOCK", Some(7)),
                ("S_IFMT", Some(0o170000)),
                ("S_IFREG", Some(0o100000)),
                ("S_IFDIR", Some(0o040000)),
                ("S_IFCHR", Some(0o020000)),
                ("S_IFBLK", Some(0o060000)),
                ("S_IFIFO", Some(0o010000)),
                ("S_IFLNK", Some(0o120000)),
                ("S_IFSOCK", Some(0o140000)),
                ("O_CREAT", None),
                ("O_EXCL", None),
                ("UV_FS_O_FILEMAP", Some(0)),
                ("UV_FS_O_TEMPORARY", Some(0)),
                ("UV_FS_O_SHORT_LIVED", Some(0)),
                ("UV_FS_O_SEQUENTIAL", Some(0)),
                ("UV_FS_O_RANDOM", Some(0)),
                ("O_NOCTTY", None),
                ("O_TRUNC", None),
                ("O_APPEND", None),
                ("O_DIRECTORY", None),
                ("O_NOATIME", None),
                ("O_NOFOLLOW", None),
                ("O_SYNC", None),
                ("O_DSYNC", None),
                ("O_SYMLINK", None),
                ("O_DIRECT", None),
                ("O_NONBLOCK", None),
                ("S_IRWXU", Some(0o700)),
                ("S_IRUSR", Some(0o400)),
                ("S_IWUSR", Some(0o200)),
                ("S_IXUSR", Some(0o100)),
                ("S_IRWXG", Some(0o070)),
                ("S_IRGRP", Some(0o040)),
                ("S_IWGRP", Some(0o020)),
                ("S_IXGRP", Some(0o010)),
                ("S_IRWXO", Some(0o007)),
                ("S_IROTH", Some(0o004)),
                ("S_IWOTH", Some(0o002)),
                ("S_IXOTH", Some(0o001)),
                ("F_OK", Some(0)),
                ("R_OK", Some(4)),
                ("W_OK", Some(2)),
                ("X_OK", Some(1)),
                ("UV_FS_COPYFILE_EXCL", Some(1)),
                ("COPYFILE_EXCL", Some(1)),
                ("UV_FS_COPYFILE_FICLONE", Some(2)),
                ("COPYFILE_FICLONE", Some(2)),
                ("UV_FS_COPYFILE_FICLONE_FORCE", Some(4)),
                ("COPYFILE_FICLONE_FORCE", Some(4)),
            ];
            for (name, value) in names {
                let value = value.or_else(|| {
                    flags
                        .iter()
                        .find(|(flag, _)| *flag == name)
                        .map(|(_, value)| *value)
                });
                if let Some(value) = value {
                    let name = std::ffi::CString::new(name).expect("names have no NUL");
                    object.set_property_by_name(scope, &name, value)?;
                }
            }
            Ok(object)
        },
    )?;
    object.to_jsval_throwing(scope)
}

#[core_runtime::jsmodule(node_args, name = "node:fs", default_namespace)]
pub mod module {
    use super::*;

    pub use super::classes::dirent_class as Dirent;
    pub use super::classes::stats_class as Stats;
    pub use super::streams::read_stream_class as ReadStream;
    pub use super::streams::write_stream_class as WriteStream;

    /// <https://nodejs.org/api/fs.html#fscreatereadstreampath-options>
    #[function(length = 2)]
    pub fn create_read_stream<'s>(
        scope: &'s Scope<'_>,
        path: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let class = super::streams::read_stream_class::class_constructor(scope)?;
        let options =
            scope.root_value(options.map_or(js::value::undefined(), |options| options.get()));
        let path = scope.root_value(path.get());
        Function::construct(scope, class, &[path, options])
    }

    /// <https://nodejs.org/api/fs.html#fswatchfilename-options-listener>
    pub fn watch<'s>(
        scope: &'s Scope<'_>,
        filename: HandleValue<'_>,
        options: HandleValue<'_>,
        listener: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        super::watch::watch(scope, filename, options, listener)
    }

    /// <https://nodejs.org/api/fs.html#fswatchfilefilename-options-listener>
    pub fn watch_file<'s>(
        scope: &'s Scope<'_>,
        filename: HandleValue<'_>,
        options: HandleValue<'_>,
        listener: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        super::watch::watch_file(scope, filename, options, listener)
    }

    /// <https://nodejs.org/api/fs.html#fsunwatchfilefilename-listener>
    pub fn unwatch_file(
        scope: &Scope<'_>,
        filename: HandleValue<'_>,
        listener: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        super::watch::unwatch_file(scope, filename, listener)
    }

    /// <https://nodejs.org/api/fs.html#fscreatewritestreampath-options>
    #[function(length = 2)]
    pub fn create_write_stream<'s>(
        scope: &'s Scope<'_>,
        path: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, ExnThrown> {
        let class = super::streams::write_stream_class::class_constructor(scope)?;
        let options =
            scope.root_value(options.map_or(js::value::undefined(), |options| options.get()));
        let path = scope.root_value(path.get());
        Function::construct(scope, class, &[path, options])
    }
}

/// Register `node:fs`: the functions of [`ops::OPS`], `cp`, `cpSync`, `constants` and `promises`.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let exports = || -> Result<Vec<(&'static str, HandleValue<'_>)>, ExnThrown> {
        let mut exports = Vec::new();
        for (index, op) in ops::OPS.iter().enumerate() {
            if !op.sync.is_empty() {
                exports.push((op.sync, op_function(scope, index, Variant::Sync, op.sync)?));
            }
            if op.variants & ops::CALLBACK != 0 {
                exports.push((
                    op.name,
                    op_function(scope, index, Variant::Callback, op.name)?,
                ));
            }
        }
        // `realpath.native` and `realpathSync.native` are the functions themselves. `read` and
        // `write` have Node's `customPromisifyArgs`, and `exists` has Node's `promisify.custom`.
        for (name, function) in &exports {
            let function = Object::from_value(scope, **function).map_err(|e| e.throw(scope))?;
            match *name {
                "realpath" | "realpathSync" => function.set_property(scope, c"native", function)?,
                "read" => {
                    crate::util::define_promisify_args(scope, function, &["bytesRead", "buffer"])?
                }
                "write" => crate::util::define_promisify_args(
                    scope,
                    function,
                    &["bytesWritten", "buffer"],
                )?,
                "exists" => {
                    let custom = Function::new_callback(scope, c"exists", 1, exists_promise, ())?;
                    let id = crate::util::PROMISIFY_CUSTOM.id(scope)?;
                    let attrs = js::class_spec::JSPROP_READONLY | js::class_spec::JSPROP_PERMANENT;
                    function.define_value_by_id(scope, id, custom, attrs as u32)?;
                }
                _ => {}
            }
        }
        let cp = Function::new_callback(scope, c"cp", 4, cp::cp_callback, ())?;
        exports.push(("cp", cp.to_jsval_throwing(scope)?));
        let cp_sync = Function::new_callback(scope, c"cpSync", 3, cp::cp_sync, ())?;
        exports.push(("cpSync", cp_sync.to_jsval_throwing(scope)?));
        exports.push(("constants", constants(scope)?));
        exports.push(("promises", promises::promises_object(scope)?));
        let read_stream = streams::read_stream_class::class_constructor(scope)?;
        exports.push(("FileReadStream", read_stream.to_jsval_throwing(scope)?));
        let write_stream = streams::write_stream_class::class_constructor(scope)?;
        exports.push(("FileWriteStream", write_stream.to_jsval_throwing(scope)?));
        Ok(exports)
    };
    let Ok(exports) = exports() else {
        return false;
    };
    crate::stub::register_implemented_with::<module::js_module>(scope, name, &exports)
}

/// Register the internal classes of `node:fs` on `global`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: js::Object<'_>) -> Result<(), ExnThrown> {
    cp::CopyWalk::add_to_global(scope, global);
    streams::CloseRequest::add_to_global(scope, global);
    streams::FileStreamState::add_to_global(scope, global);
    streams::StreamRequest::add_to_global(scope, global);
    watch::AbortListener::add_to_global(scope, global);
    watch::FSEvent::add_to_global(scope, global);
    watch::IgnoreMatcher::add_to_global(scope, global);
    watch::WatchIterator::add_to_global(scope, global);
    watch::RecursiveNext::add_to_global(scope, global);
    watch::StatChange::add_to_global(scope, global);
    watch::WatcherState::add_to_global(scope, global);
    classes::stats_class::register(scope)?;
    classes::dirent_class::register(scope)?;
    promises::register_classes(scope, global)?;
    streams::read_stream_class::register(scope)?;
    streams::write_stream_class::register(scope)?;
    watch::fs_watcher_class::register(scope)?;
    watch::register_recursive_watcher(scope, global)?;
    watch::stat_watcher_class::register(scope)?;
    Ok(())
}
