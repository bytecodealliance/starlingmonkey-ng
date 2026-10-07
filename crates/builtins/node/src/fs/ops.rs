// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The operations of `node:fs`: for each, the names of its functions and the parser that turns
//! the arguments into a [`Prepared`] operation.

use std::io::ErrorKind as IoErrorKind;
use std::time::{Duration, SystemTime};

use js::conversion::{FromJSVal, ToJSVal};
use js::gc::handle::RootedHeap;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Object;
use platform::fs::{FileKind, Stat};

use super::{
    boolean_option, data_arg, encoding_option, fd_arg, flag_option, flags_arg, mode_arg, option,
    path_arg, remove_file, target_arg, Entry, Finish, Handle, Output, Prepared, Target, UvError,
    Variant,
};
use crate::buffer::Encoding;
use crate::errors::{validate_integer, ErrorKind, NodeError, Thrown};

pub(super) const SYNC: u8 = 1;
pub(super) const CALLBACK: u8 = 2;
pub(super) const PROMISE: u8 = 4;
const ALL: u8 = SYNC | CALLBACK | PROMISE;
const NOT_PROMISE: u8 = SYNC | CALLBACK;

/// The parser of an operation's arguments.
type Parse = fn(&Scope<'_>, &[HandleValue<'_>], Variant) -> Result<Prepared, Thrown>;

/// An operation: the name of its callback and promise functions, the name of its synchronous
/// function, the variants it has functions for, the `length` of its synchronous, callback and
/// promise functions, and its parser.
pub(super) struct OpDef {
    pub(super) name: &'static str,
    pub(super) sync: &'static str,
    pub(super) variants: u8,
    pub(super) lengths: [u32; 3],
    pub(super) parse: Parse,
}

const fn op(
    name: &'static str,
    sync: &'static str,
    variants: u8,
    lengths: [u32; 3],
    parse: Parse,
) -> OpDef {
    OpDef {
        name,
        sync,
        variants,
        lengths,
        parse,
    }
}

pub(super) const OPS: [OpDef; 34] = [
    op("access", "accessSync", ALL, [2, 3, 1], access),
    op("appendFile", "appendFileSync", ALL, [3, 4, 3], append_file),
    op("chmod", "chmodSync", ALL, [2, 3, 2], chmod),
    op("chown", "chownSync", ALL, [3, 4, 3], chown),
    op("close", "closeSync", NOT_PROMISE, [1, 1, 1], close),
    op("copyFile", "copyFileSync", ALL, [3, 4, 3], copy_file),
    op("exists", "existsSync", NOT_PROMISE, [1, 2, 1], exists),
    op("fchmod", "fchmodSync", NOT_PROMISE, [2, 3, 2], fchmod),
    op("fchown", "fchownSync", NOT_PROMISE, [3, 4, 3], fchown),
    op(
        "fdatasync",
        "fdatasyncSync",
        NOT_PROMISE,
        [1, 2, 1],
        fdatasync,
    ),
    op("fstat", "fstatSync", NOT_PROMISE, [1, 1, 1], fstat),
    op("fsync", "fsyncSync", NOT_PROMISE, [1, 2, 1], fsync),
    op(
        "ftruncate",
        "ftruncateSync",
        NOT_PROMISE,
        [1, 1, 1],
        ftruncate,
    ),
    op("futimes", "futimesSync", NOT_PROMISE, [3, 4, 3], futimes),
    op("link", "linkSync", ALL, [2, 3, 2], link),
    op("lstat", "lstatSync", ALL, [1, 1, 1], lstat),
    op("mkdir", "mkdirSync", ALL, [2, 3, 2], mkdir),
    op("mkdtemp", "mkdtempSync", ALL, [2, 3, 2], mkdtemp),
    op("open", "openSync", NOT_PROMISE, [3, 4, 3], open),
    op("read", "readSync", NOT_PROMISE, [5, 6, 5], read),
    op("readFile", "readFileSync", ALL, [2, 3, 2], read_file),
    op("readdir", "readdirSync", ALL, [2, 3, 2], readdir),
    op("readlink", "readlinkSync", ALL, [2, 3, 2], readlink),
    op("realpath", "realpathSync", ALL, [2, 3, 2], realpath),
    op("rename", "renameSync", ALL, [2, 3, 2], rename),
    op("rm", "rmSync", ALL, [2, 3, 2], rm),
    op("rmdir", "rmdirSync", ALL, [2, 3, 2], rmdir),
    op("stat", "statSync", ALL, [1, 1, 1], stat),
    op("symlink", "symlinkSync", ALL, [3, 4, 3], symlink),
    op("truncate", "truncateSync", ALL, [2, 3, 1], truncate),
    op("unlink", "unlinkSync", ALL, [1, 2, 1], unlink),
    op("utimes", "utimesSync", ALL, [3, 4, 3], utimes),
    op("write", "writeSync", NOT_PROMISE, [5, 6, 5], write),
    op("writeFile", "writeFileSync", ALL, [3, 4, 3], write_file),
];

/// The index of the operation named `name` in [`OPS`].
pub(super) fn index_of(name: &str) -> usize {
    OPS.iter()
        .position(|op| op.name == name)
        .expect("the operation exists")
}

/// On WASI, return `Ok($prepared)` from the enclosing parser for the callback and promise
/// variants, if `$suspends` is true or missing. `$prepared` is an asynchronous operation, and
/// isn't compiled natively.
macro_rules! on_wasi {
    ($variant:expr, $prepared:expr) => {
        on_wasi!($variant, true, $prepared)
    };
    ($variant:expr, $suspends:expr, $prepared:expr) => {
        #[cfg(target_arch = "wasm32")]
        if $variant != Variant::Sync && $suspends {
            return Ok($prepared);
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = $variant;
    };
}

/// Whether the asynchronous operations can use `handle`: a `platform::fs::wasi` file, or an
/// error, which they report without blocking.
#[cfg(target_arch = "wasm32")]
fn suspends(handle: &Result<Handle, UvError>) -> bool {
    handle
        .as_ref()
        .map_or(true, |handle| handle.wasi().is_some())
}

/// The `platform::fs::wasi` file of `handle`, for which [`suspends`] is true.
#[cfg(target_arch = "wasm32")]
fn wasi_file(
    handle: Result<Handle, UvError>,
) -> Result<std::sync::Arc<platform::fs::wasi::File>, UvError> {
    Ok(handle?
        .wasi()
        .cloned()
        .expect("asynchronous operations get `platform::fs::wasi` files"))
}

/// Argument `index`, or `undefined`.
pub(super) fn arg<'a>(args: &[HandleValue<'a>], index: usize) -> HandleValue<'a> {
    args.get(index).copied().unwrap_or(HandleValue::undefined())
}

/// Argument `index`, unless it is missing or `undefined`.
fn opt<'a>(args: &[HandleValue<'a>], index: usize) -> Option<HandleValue<'a>> {
    args.get(index)
        .copied()
        .filter(|value| !value.is_undefined())
}

/// Argument `index` as a file descriptor, and the file it refers to, or the `EBADF` to report
/// for `syscall` when the operation runs.
fn handle_arg(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    index: usize,
    syscall: &'static str,
) -> Result<Result<Handle, UvError>, Thrown> {
    Ok(Handle::of(fd_arg(scope, arg(args, index))?, syscall))
}

/// The position the argument `value` gives: `None` for `null`, `undefined` or -1, which read or
/// write at the current position. A read of `read_length` bytes takes an integer or a `bigint`
/// from -1 and throws for other values, as Node's `validatePosition` does. A write takes a safe
/// integer from 0, and writes at the current position for other values, as Node's C++
/// `GetOffset` reads it.
fn position_arg(
    scope: &Scope<'_>,
    value: Option<HandleValue<'_>>,
    read_length: Option<usize>,
) -> Result<Option<u64>, Thrown> {
    let Some(value) = value.filter(|value| !value.is_null() && !value.is_undefined()) else {
        return Ok(None);
    };
    if read_length.is_none() {
        let position = value.is_number().then(|| value.to_number());
        let safe = |position: &f64| {
            position.fract() == 0.0 && (0.0..=crate::errors::MAX_SAFE_INTEGER).contains(position)
        };
        return Ok(position.filter(safe).map(|position| position as u64));
    }
    if value.is_number() {
        let number = validate_integer(
            scope,
            value,
            "position",
            -1.0,
            crate::errors::MAX_SAFE_INTEGER,
        )?;
        return Ok((number >= 0.0).then_some(number as u64));
    }
    let Some(read_length) = read_length else {
        return Ok(None);
    };
    if !value.is_bigint() {
        return Err(NodeError::invalid_arg_type(
            scope,
            "position",
            "of type bigint or integer",
            value,
        )
        .into());
    }
    let bigint = js::bigint::to_bigint(scope, value)?;
    let max = i64::MAX - read_length as i64;
    match js::bigint::fits_i64(bigint) {
        Some(position) if (-1..=max).contains(&position) => {
            Ok((position >= 0).then_some(position as u64))
        }
        position => {
            let text = js::bigint::to_string(scope, bigint, 10)?;
            let text = js::JSString::from_handle(text)
                .ok_or(js::error::ExnThrown)?
                .to_utf8(scope)?;
            let text = match position {
                Some(position) if position.unsigned_abs() <= 1 << 32 => text,
                _ => crate::errors::add_numerical_separator(&text),
            };
            Err(NodeError::out_of_range(
                "position",
                &format!(">= -1 && <= {max}"),
                &format!("{text}n"),
            )
            .into())
        }
    }
}

/// The time the argument `value`, named `name`, gives, as Node's `toUnixTimestamp` reads it:
/// seconds since the epoch as a finite number, where negative numbers give the current time, a
/// string that converts to a number, or a `Date`. `None` leaves the time unchanged, which an
/// invalid `Date` does.
fn time_arg(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<Option<SystemTime>, Thrown> {
    let invalid = || -> Thrown {
        NodeError::invalid_arg_type(
            scope,
            name,
            "an instance of Date or an Time in seconds",
            value,
        )
        .into()
    };
    let seconds = if value.is_string() {
        let seconds = f64::from_jsval(scope, value, ())?;
        if seconds.is_nan() {
            return Err(invalid());
        }
        seconds
    } else if value.is_number() && value.to_number().is_finite() {
        let seconds = value.to_number();
        if seconds < 0.0 {
            return Ok(Some(SystemTime::now()));
        }
        seconds
    } else if crate::util::types::is_date(scope, value)? {
        let date = js::gc::handle::Stack::<js::date::Date>::from_jsval(scope, value, ())?;
        date.msec_since_epoch(scope)? / 1000.0
    } else {
        return Err(invalid());
    };
    Ok(time_of_seconds(seconds))
}

/// The time `seconds` after the epoch. As in libuv, infinite times give the current time and NaN
/// gives `None`, which leaves the time unchanged. Times beyond the range of `i64` seconds are
/// clamped to it, and times `SystemTime` can't represent give `None`.
pub(super) fn time_of_seconds(seconds: f64) -> Option<SystemTime> {
    if seconds.is_nan() {
        return None;
    }
    if seconds.is_infinite() {
        return Some(SystemTime::now());
    }
    let offset = match Duration::try_from_secs_f64(seconds.abs()) {
        Ok(offset) => offset.min(Duration::from_secs(i64::MAX as u64)),
        Err(_) => Duration::from_secs(i64::MAX as u64),
    };
    match seconds < 0.0 {
        true => SystemTime::UNIX_EPOCH.checked_sub(offset),
        false => SystemTime::UNIX_EPOCH.checked_add(offset),
    }
}

/// The `mode` argument of `access` and `copyFile`, as Node's C++ `GetValidFileMode` reads it: 0
/// for `undefined` and `null`, and otherwise a finite number that is from 0 to 7 once truncated.
fn file_mode_arg(value: HandleValue<'_>) -> Result<u32, Thrown> {
    if value.is_null_or_undefined() {
        return Ok(0);
    }
    let out_of_range = |message: &str| -> Thrown {
        NodeError::new(ErrorKind::RangeError, "ERR_OUT_OF_RANGE", message).into()
    };
    if !value.is_number() {
        let message = "mode must be int32 or null/undefined";
        return Err(NodeError::new(ErrorKind::TypeError, "ERR_INVALID_ARG_TYPE", message).into());
    }
    let mode = value.to_number().trunc();
    if !mode.is_finite() {
        return Err(out_of_range("mode is out of range"));
    }
    if !(0.0..=7.0).contains(&mode) {
        return Err(out_of_range("mode is out of range: >= 0 && <= 7"));
    }
    Ok(mode as u32)
}

fn access(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let path = path_arg(scope, arg(args, 0), "path")?;
    let mode = file_mode_arg(arg(args, 1))?;
    // WASI reports no permissions, so only existence is checked, as `platform::fs::access` does.
    on_wasi!(
        variant,
        Prepared::future(None, async move {
            platform::fs::wasi::stat(&path, true)
                .await
                .map_err(|error| UvError::new(error, "access").path(&path))?;
            Ok(Output::Unit)
        })
    );
    Ok(Prepared::new(None, move || {
        platform::fs::access(&path, mode)
            .map_err(|error| UvError::new(error, "access").path(&path))?;
        Ok(Output::Unit)
    }))
}

/// The body of `writeFile` and `appendFile`, which open paths with `default_flag`.
fn write_to_file(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
    default_flag: &str,
) -> Result<Prepared, Thrown> {
    let options = opt(args, 2);
    let encoding = encoding_option(scope, options, Some(Encoding::Utf8))?.unwrap_or_default();
    let data = data_arg(scope, arg(args, 1), encoding)?;
    let target = target_arg(scope, arg(args, 0), "path")?;
    let flags = flags_arg(scope, option(scope, options, c"flag")?, default_flag)?;
    let mode = mode_arg(scope, option(scope, options, c"mode")?, Some(0o666))?;
    let handle = match target {
        Target::Fd(fd) => Some(Handle::of(fd, "write")),
        Target::Path(_) => None,
    };
    on_wasi!(
        variant,
        handle.as_ref().is_none_or(suspends),
        Prepared::future(None, async move {
            let file = match (target, handle) {
                (_, Some(handle)) => wasi_file(handle)?,
                (Target::Path(path), None) => flags.open_wasi(&path).await?.into(),
                (Target::Fd(_), None) => unreachable!("descriptors have a handle"),
            };
            // Empty data is not written, so writing it to a read-only descriptor succeeds.
            if !data.is_empty() {
                file.write(data, None)
                    .await
                    .map_err(|error| UvError::new(error, "write"))?;
            }
            Ok(Output::Unit)
        })
    );
    Ok(Prepared::new(None, move || {
        // Empty data is not written, so writing it to a read-only descriptor succeeds.
        if data.is_empty() {
            match (target, handle) {
                (_, Some(handle)) => drop(handle?),
                (Target::Path(path), None) => drop(flags.open(&path, mode)?),
                (Target::Fd(_), None) => unreachable!("descriptors have a handle"),
            }
            return Ok(Output::Unit);
        }
        match (target, handle) {
            (_, Some(handle)) => handle?.write(&data, None)?,
            (Target::Path(path), None) => {
                let file = flags.open(&path, mode)?;
                Handle::File(file.into()).write(&data, None)?;
            }
            (Target::Fd(_), None) => unreachable!("descriptors have a handle"),
        }
        Ok(Output::Unit)
    }))
}

fn append_file(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    write_to_file(scope, args, variant, "a")
}

fn write_file(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    write_to_file(scope, args, variant, "w")
}

/// Set the permissions of `path` to `mode`. Without Unix permissions, only the owner's write bit
/// counts, as the read-only flag.
pub(super) fn set_mode(path: &str, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_readonly(mode & 0o200 == 0);
        std::fs::set_permissions(path, permissions)
    }
}

fn chmod(scope: &Scope<'_>, args: &[HandleValue<'_>], _: Variant) -> Result<Prepared, Thrown> {
    let path = path_arg(scope, arg(args, 0), "path")?;
    let mode = mode_arg(scope, Some(arg(args, 1)), None)?;
    Ok(Prepared::new(None, move || {
        set_mode(&path, mode).map_err(|error| UvError::new(error, "chmod").path(&path))?;
        Ok(Output::Unit)
    }))
}

fn fchmod(scope: &Scope<'_>, args: &[HandleValue<'_>], _: Variant) -> Result<Prepared, Thrown> {
    let handle = handle_arg(scope, args, 0, "fchmod")?;
    let mode = mode_arg(scope, Some(arg(args, 1)), None)?;
    Ok(Prepared::new(None, move || {
        let handle = handle?;
        // As for `std::fs` files on WASI, the metadata is read, and then wasi-libc's `fchmod`
        // fails with `ENOSYS`.
        #[cfg(target_arch = "wasm32")]
        if let Some(file) = handle.wasi() {
            platform::block_on(file.stat()).map_err(|error| UvError::new(error, "fchmod"))?;
            return Err(UvError::code("ENOSYS", "fchmod"));
        }
        let file = handle.file("fchmod")?;
        #[cfg(unix)]
        let permissions = std::os::unix::fs::PermissionsExt::from_mode(mode);
        #[cfg(not(unix))]
        let permissions = {
            let mut permissions = file
                .metadata()
                .map_err(|error| UvError::new(error, "fchmod"))?
                .permissions();
            permissions.set_readonly(mode & 0o200 == 0);
            permissions
        };
        file.set_permissions(permissions)
            .map_err(|error| UvError::new(error, "fchmod"))?;
        Ok(Output::Unit)
    }))
}

/// The owner arguments of `chown` and `fchown`, from `index`: `None` for -1, which leaves the
/// owner or group unchanged.
fn owner_args(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    index: usize,
) -> Result<(Option<u32>, Option<u32>), Thrown> {
    let id = |offset, name| -> Result<Option<u32>, Thrown> {
        let id = validate_integer(scope, arg(args, index + offset), name, -1.0, 4294967295.0)?;
        Ok((id >= 0.0).then_some(id as u32))
    };
    Ok((id(0, "uid")?, id(1, "gid")?))
}

fn chown(scope: &Scope<'_>, args: &[HandleValue<'_>], _: Variant) -> Result<Prepared, Thrown> {
    let path = path_arg(scope, arg(args, 0), "path")?;
    let (uid, gid) = owner_args(scope, args, 1)?;
    Ok(Prepared::new(None, move || {
        #[cfg(unix)]
        std::os::unix::fs::chown(&path, uid, gid)
            .map_err(|error| UvError::new(error, "chown").path(&path))?;
        #[cfg(not(unix))]
        let _ = (path, uid, gid);
        Ok(Output::Unit)
    }))
}

fn fchown(scope: &Scope<'_>, args: &[HandleValue<'_>], _: Variant) -> Result<Prepared, Thrown> {
    let handle = handle_arg(scope, args, 0, "fchown")?;
    let (uid, gid) = owner_args(scope, args, 1)?;
    Ok(Prepared::new(None, move || {
        let handle = handle?;
        #[cfg(unix)]
        std::os::unix::fs::fchown(handle.file("fchown")?, uid, gid)
            .map_err(|error| UvError::new(error, "fchown"))?;
        #[cfg(not(unix))]
        {
            handle.check_file("fchown")?;
            let _ = (uid, gid);
        }
        Ok(Output::Unit)
    }))
}

fn close(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let fd = fd_arg(scope, arg(args, 0))?;
    // The descriptor leaves the table here, so a refused close must throw first.
    if variant == Variant::Sync {
        super::refuse_blocking("closeSync")?;
    } else if super::close_blocks(fd) {
        super::refuse_blocking("close")?;
    }
    let result = remove_file(fd);
    on_wasi!(
        variant,
        Prepared::future(None, async move { result.map(|()| Output::Unit) })
    );
    Ok(Prepared::new(None, move || result.map(|()| Output::Unit)))
}

fn copy_file(scope: &Scope<'_>, args: &[HandleValue<'_>], _: Variant) -> Result<Prepared, Thrown> {
    let source = path_arg(scope, arg(args, 0), "src")?;
    let destination = path_arg(scope, arg(args, 1), "dest")?;
    let mode = file_mode_arg(arg(args, 2))?;
    Ok(Prepared::new(None, move || {
        copy_file_blocking(&source, &destination, mode, "copyfile")?;
        Ok(Output::Unit)
    }))
}

/// Copy the file `source` to `destination` with the `COPYFILE_*` flags `mode`, as libuv's
/// `uv_fs_copyfile` does. Errors name `syscall`, `source` and `destination`.
pub(super) fn copy_file_blocking(
    source: &str,
    destination: &str,
    mode: u32,
    syscall: &'static str,
) -> Result<(), UvError> {
    let fail = |error| UvError::new(error, syscall).path(source).dest(destination);
    let source_metadata = std::fs::metadata(source).map_err(fail)?;
    // Node.js on macOS fails to copy a directory: with `EEXIST` onto an existing entry with
    // `COPYFILE_EXCL`, with `EISDIR` onto a directory, with the error of a missing destination
    // parent, and otherwise with `ENOTSUP`, after removing a destination that isn't a
    // directory.
    #[cfg(target_os = "macos")]
    if source_metadata.is_dir() {
        let error = |code| Err(UvError::code(code, syscall).path(source).dest(destination));
        let existing = std::fs::symlink_metadata(destination).ok();
        if let Some(existing) = &existing {
            if mode & 1 != 0 {
                return error("EEXIST");
            }
            if std::fs::metadata(destination).is_ok_and(|metadata| metadata.is_dir()) {
                return error("EISDIR");
            }
            if !existing.is_dir() {
                std::fs::remove_file(destination).map_err(fail)?;
            }
        }
        let parent = std::path::Path::new(destination)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        std::fs::metadata(parent).map_err(fail)?;
        return error("ENOTSUP");
    }
    // Copying a file to itself leaves it unchanged.
    #[cfg(unix)]
    if let Ok(destination_metadata) = std::fs::metadata(destination) {
        use std::os::unix::fs::MetadataExt;
        if (source_metadata.dev(), source_metadata.ino())
            == (destination_metadata.dev(), destination_metadata.ino())
        {
            return match mode & 1 {
                0 => Ok(()),
                _ => Err(UvError::code("EEXIST", syscall)
                    .path(source)
                    .dest(destination)),
            };
        }
    }
    let exclusive = mode & 1 != 0;
    if !exclusive && source_metadata.is_file() {
        std::fs::copy(source, destination).map_err(fail)?;
        return Ok(());
    }
    // `COPYFILE_EXCL` creates the destination with `O_EXCL`, so a file created
    // concurrently makes the copy fail. As libuv does, the copy is the size the source reports,
    // which is 0 for a device such as `/dev/null`.
    let input = std::fs::File::open(source).map_err(fail)?;
    let metadata = input.metadata().map_err(fail)?;
    let mut output = std::fs::OpenOptions::new();
    output.write(true);
    if exclusive {
        output.create_new(true);
    } else {
        output.create(true).truncate(true);
    }
    let mut output = output.open(destination).map_err(fail)?;
    std::io::copy(&mut std::io::Read::take(input, metadata.len()), &mut output).map_err(fail)?;
    output
        .set_permissions(metadata.permissions())
        .map_err(fail)?;
    Ok(())
}

fn exists(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let path = path_arg(scope, arg(args, 0), "path").ok();
    if path.is_none() {
        js::exception::clear(scope);
    }
    on_wasi!(
        variant,
        Prepared::future(None, async move {
            let exists = match path {
                Some(path) => platform::fs::wasi::stat(&path, true).await.is_ok(),
                None => false,
            };
            Ok(Output::Bool(exists))
        })
    );
    Ok(Prepared::new(None, move || {
        Ok(Output::Bool(
            path.is_some_and(|path| std::fs::metadata(path).is_ok()),
        ))
    }))
}

fn fdatasync(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let handle = handle_arg(scope, args, 0, "fdatasync")?;
    on_wasi!(
        variant,
        suspends(&handle),
        Prepared::future(None, async move {
            wasi_file(handle)?
                .sync_data()
                .await
                .map_err(|error| UvError::new(error, "fdatasync"))?;
            Ok(Output::Unit)
        })
    );
    Ok(Prepared::new(None, move || {
        let result = match handle? {
            Handle::File(file) => file.sync_data(),
            #[cfg(target_arch = "wasm32")]
            Handle::Wasi(file) => platform::block_on(file.sync_data()),
            _ => Ok(()),
        };
        result.map_err(|error| UvError::new(error, "fdatasync"))?;
        Ok(Output::Unit)
    }))
}

fn fsync(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let handle = handle_arg(scope, args, 0, "fsync")?;
    on_wasi!(
        variant,
        suspends(&handle),
        Prepared::future(None, async move {
            wasi_file(handle)?
                .sync()
                .await
                .map_err(|error| UvError::new(error, "fsync"))?;
            Ok(Output::Unit)
        })
    );
    Ok(Prepared::new(None, move || {
        let result = match handle? {
            Handle::File(file) => file.sync_all(),
            #[cfg(target_arch = "wasm32")]
            Handle::Wasi(file) => platform::block_on(file.sync()),
            _ => Ok(()),
        };
        result.map_err(|error| UvError::new(error, "fsync"))?;
        Ok(Output::Unit)
    }))
}

fn fstat(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let handle = handle_arg(scope, args, 0, "fstat")?;
    on_wasi!(
        variant,
        suspends(&handle),
        Prepared::future(None, async move {
            let stat = wasi_file(handle)?
                .stat()
                .await
                .map_err(|error| UvError::new(error, "fstat"))?;
            Ok(Output::Stat(Some(stat)))
        })
    );
    Ok(Prepared::new(None, move || {
        Ok(Output::Stat(Some(handle?.stat()?)))
    }))
}

/// The length argument of `truncate` and `ftruncate`, at `index`: 0 if missing, and 0 for
/// negative lengths.
fn length_arg(scope: &Scope<'_>, args: &[HandleValue<'_>], index: usize) -> Result<u64, Thrown> {
    match opt(args, index) {
        Some(length) => Ok(validate_integer(
            scope,
            length,
            "len",
            -crate::errors::MAX_SAFE_INTEGER,
            crate::errors::MAX_SAFE_INTEGER,
        )?
        .max(0.0) as u64),
        None => Ok(0),
    }
}

/// Truncate or extend the file `handle` refers to to `length` bytes, or fail with `EINVAL` for
/// the standard streams.
fn set_length(handle: &Handle, length: u64) -> Result<(), UvError> {
    #[cfg(target_arch = "wasm32")]
    if let Some(file) = handle.wasi() {
        return platform::block_on(file.set_size(length))
            .map_err(|error| UvError::new(error, "ftruncate"));
    }
    handle
        .file("ftruncate")?
        .set_len(length)
        .map_err(|error| UvError::new(error, "ftruncate"))
}

fn ftruncate(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let handle = handle_arg(scope, args, 0, "ftruncate")?;
    let length = length_arg(scope, args, 1)?;
    on_wasi!(
        variant,
        suspends(&handle),
        Prepared::future(None, async move {
            wasi_file(handle)?
                .set_size(length)
                .await
                .map_err(|error| UvError::new(error, "ftruncate"))?;
            Ok(Output::Unit)
        })
    );
    Ok(Prepared::new(None, move || {
        set_length(&handle?, length)?;
        Ok(Output::Unit)
    }))
}

fn truncate(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let target = target_arg(scope, arg(args, 0), "path")?;
    let length = length_arg(scope, args, 1)?;
    let handle = match target {
        Target::Fd(fd) => Some(Handle::of(fd, "ftruncate")),
        Target::Path(_) => None,
    };
    on_wasi!(
        variant,
        handle.as_ref().is_none_or(suspends),
        Prepared::future(None, async move {
            let file = match (target, handle) {
                (_, Some(handle)) => wasi_file(handle)?,
                (Target::Path(path), None) => {
                    let options = platform::fs::wasi::OpenOptions {
                        write: true,
                        ..Default::default()
                    };
                    platform::fs::wasi::File::open(&path, options)
                        .await
                        .map_err(|error| UvError::new(error, "open").path(&path))?
                        .into()
                }
                (Target::Fd(_), None) => unreachable!("descriptors have a handle"),
            };
            file.set_size(length)
                .await
                .map_err(|error| UvError::new(error, "ftruncate"))?;
            Ok(Output::Unit)
        })
    );
    Ok(Prepared::new(None, move || {
        let result = match (target, handle) {
            (_, Some(handle)) => return set_length(&handle?, length).map(|()| Output::Unit),
            (Target::Path(path), None) => {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .map_err(|error| UvError::new(error, "open").path(&path))?;
                file.set_len(length)
            }
            (Target::Fd(_), None) => unreachable!("descriptors have a handle"),
        };
        result.map_err(|error| UvError::new(error, "ftruncate"))?;
        Ok(Output::Unit)
    }))
}

/// The access and modification time arguments of `utimes` and `futimes`, from `index`, which
/// errors name `names`. `None` leaves a time unchanged.
fn times_args(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    index: usize,
    names: [&str; 2],
) -> Result<(Option<SystemTime>, Option<SystemTime>), Thrown> {
    let accessed = time_arg(scope, arg(args, index), names[0])?;
    let modified = time_arg(scope, arg(args, index + 1), names[1])?;
    Ok((accessed, modified))
}

fn futimes(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let handle = handle_arg(scope, args, 0, "futime")?;
    let (accessed, modified) = times_args(scope, args, 1, ["atime", "mtime"])?;
    on_wasi!(
        variant,
        suspends(&handle),
        Prepared::future(None, async move {
            wasi_file(handle)?
                .set_times(accessed, modified)
                .await
                .map_err(|error| UvError::new(error, "futime"))?;
            Ok(Output::Unit)
        })
    );
    Ok(Prepared::new(None, move || {
        let handle = handle?;
        #[cfg(target_arch = "wasm32")]
        if let Some(file) = handle.wasi() {
            platform::block_on(file.set_times(accessed, modified))
                .map_err(|error| UvError::new(error, "futime"))?;
            return Ok(Output::Unit);
        }
        let mut times = std::fs::FileTimes::new();
        if let Some(accessed) = accessed {
            times = times.set_accessed(accessed);
        }
        if let Some(modified) = modified {
            times = times.set_modified(modified);
        }
        handle
            .file("futime")?
            .set_times(times)
            .map_err(|error| UvError::new(error, "futime"))?;
        Ok(Output::Unit)
    }))
}

fn utimes(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let path = path_arg(scope, arg(args, 0), "path")?;
    // Node's `utimes` names both times `time`.
    let (accessed, modified) = times_args(scope, args, 1, ["time", "time"])?;
    // The file is opened for reading to set the times, as `platform::fs::set_times` does on WASI.
    on_wasi!(
        variant,
        Prepared::future(None, async move {
            let fail = |error| UvError::new(error, "utime").path(&path);
            let options = platform::fs::wasi::OpenOptions {
                read: true,
                ..Default::default()
            };
            let file = platform::fs::wasi::File::open(&path, options)
                .await
                .map_err(fail)?;
            file.set_times(accessed, modified).await.map_err(fail)?;
            Ok(Output::Unit)
        })
    );
    Ok(Prepared::new(None, move || {
        platform::fs::set_times(&path, accessed, modified)
            .map_err(|error| UvError::new(error, "utime").path(&path))?;
        Ok(Output::Unit)
    }))
}

fn link(scope: &Scope<'_>, args: &[HandleValue<'_>], _: Variant) -> Result<Prepared, Thrown> {
    let existing = path_arg(scope, arg(args, 0), "existingPath")?;
    let new = path_arg(scope, arg(args, 1), "newPath")?;
    Ok(Prepared::new(None, move || {
        std::fs::hard_link(&existing, &new)
            .map_err(|error| UvError::new(error, "link").path(&existing).dest(&new))?;
        Ok(Output::Unit)
    }))
}

fn symlink(scope: &Scope<'_>, args: &[HandleValue<'_>], _: Variant) -> Result<Prepared, Thrown> {
    let kind_value = arg(args, 2);
    let kind = crate::support::rust_string(scope, kind_value)
        .filter(|kind| ["dir", "file", "junction"].contains(&kind.as_str()));
    if kind.is_none() && !kind_value.is_null_or_undefined() {
        let reason = "must be one of: 'dir', 'file', 'junction', null, undefined";
        return Err(NodeError::invalid_arg_value(scope, "type", kind_value, reason).into());
    }
    let target = path_arg(scope, arg(args, 0), "target")?;
    let path = path_arg(scope, arg(args, 1), "path")?;
    Ok(Prepared::new(None, move || {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(&target, &path);
        // Without a type, a link to a directory is a directory link, with the target resolved
        // against the link's directory, as Node's `symlink` resolves it. A junction is created
        // as a directory link.
        #[cfg(windows)]
        let result = {
            let is_dir = match kind.as_deref() {
                Some(kind) => kind != "file",
                None => std::path::Path::new(&path)
                    .parent()
                    .unwrap_or(std::path::Path::new(""))
                    .join(&target)
                    .is_dir(),
            };
            if is_dir {
                std::os::windows::fs::symlink_dir(&target, &path)
            } else {
                std::os::windows::fs::symlink_file(&target, &path)
            }
        };
        #[cfg(not(any(unix, windows)))]
        #[allow(deprecated)]
        let result = std::fs::soft_link(&target, &path);
        result.map_err(|error| UvError::new(error, "symlink").path(&target).dest(&path))?;
        Ok(Output::Unit)
    }))
}

/// The body of `stat` and `lstat`, which follows symlinks if `follow`.
fn stat_path(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
    follow: bool,
) -> Result<Prepared, Thrown> {
    let path = path_arg(scope, arg(args, 0), "path")?;
    // Only `false` keeps a missing entry from throwing, as in Node's C++ binding.
    let throw_if_no_entry =
        !option(scope, opt(args, 1), c"throwIfNoEntry")?.is_some_and(crate::support::is_false);
    on_wasi!(
        variant,
        Prepared::future(None, async move {
            let result = platform::fs::wasi::stat(&path, follow).await;
            stat_output(result, &path, follow, throw_if_no_entry)
        })
    );
    Ok(Prepared::new(None, move || {
        let result = if follow {
            std::fs::metadata(&path)
        } else {
            std::fs::symlink_metadata(&path)
        };
        let result = result.map(|metadata| Stat::from(&metadata));
        stat_output(result, &path, follow, throw_if_no_entry)
    }))
}

/// The result of `stat` or `lstat` of `path`, which follows symlinks if `follow`: the metadata,
/// or nothing for a missing entry without `throw_if_no_entry`.
fn stat_output(
    result: std::io::Result<Stat>,
    path: &str,
    follow: bool,
    throw_if_no_entry: bool,
) -> Result<Output, UvError> {
    let syscall = if follow { "stat" } else { "lstat" };
    match result {
        Ok(stat) => Ok(Output::Stat(Some(stat))),
        Err(error) => {
            let error = UvError::new(error, syscall).path(path);
            // lstat doesn't treat `ENOTDIR` as a missing entry.
            let missing = error.error.code == "ENOENT" || follow && error.error.code == "ENOTDIR";
            if !throw_if_no_entry && missing {
                return Ok(Output::Stat(None));
            }
            Err(error)
        }
    }
}

fn stat(scope: &Scope<'_>, args: &[HandleValue<'_>], variant: Variant) -> Result<Prepared, Thrown> {
    stat_path(scope, args, variant, true)
}

fn lstat(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    stat_path(scope, args, variant, false)
}

/// Create the directory `path` and its missing ancestors with `builder`, as Node's `MKDirpSync`
/// does, without recursion. An existing entry that isn't a directory gives `EEXIST` for the
/// outermost path, and `ENOTDIR` for an ancestor. If it can't be read, as a dangling symlink can't,
/// its error is the outermost path's error. Every error names `path`, as in Node.js.
fn make_directories(builder: &std::fs::DirBuilder, path: &std::path::Path) -> Result<(), UvError> {
    let name = path.to_string_lossy();
    let fail = |error| UvError::new(error, "mkdir").path(&name);
    // The directories to create, innermost last, each with whether its parent was created.
    let mut pending = vec![(path, false)];
    while let Some(&(current, retry)) = pending.last() {
        let outermost = pending.len() == 1;
        if retry {
            builder.create(current).map_err(fail)?;
            pending.pop();
            continue;
        }
        match builder.create(current) {
            Ok(()) => {
                pending.pop();
            }
            Err(error) if error.kind() == IoErrorKind::AlreadyExists => {
                match std::fs::metadata(current) {
                    Ok(metadata) if metadata.is_dir() => {
                        pending.pop();
                    }
                    Ok(_) if outermost => return Err(fail(error)),
                    Ok(_) => return Err(UvError::code("ENOTDIR", "mkdir").path(&name)),
                    // A dangling symbolic link exists, but its target doesn't.
                    Err(error) => return Err(fail(error)),
                }
            }
            Err(error) if error.kind() == IoErrorKind::NotFound => {
                let parent = current
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty());
                let Some(parent) = parent else {
                    return Err(fail(error));
                };
                if let Some(last) = pending.last_mut() {
                    last.1 = true;
                }
                pending.push((parent, false));
            }
            Err(error) => return Err(fail(error)),
        }
    }
    Ok(())
}

/// [`make_directories`] over `platform::fs::wasi`, which creates directories without modes.
#[cfg(target_arch = "wasm32")]
async fn make_directories_wasi(path: &str) -> Result<(), UvError> {
    let fail = |error| UvError::new(error, "mkdir").path(path);
    // The directories to create, innermost last, each with whether its parent was created.
    let mut pending = vec![(path.to_string(), false)];
    while let Some((current, retry)) = pending.last().cloned() {
        let outermost = pending.len() == 1;
        if retry {
            platform::fs::wasi::create_dir(&current)
                .await
                .map_err(fail)?;
            pending.pop();
            continue;
        }
        match platform::fs::wasi::create_dir(&current).await {
            Ok(()) => {
                pending.pop();
            }
            Err(error) if error.kind() == IoErrorKind::AlreadyExists => {
                match platform::fs::wasi::stat(&current, true).await {
                    Ok(stat) if stat.kind == FileKind::Directory => {
                        pending.pop();
                    }
                    Ok(_) if outermost => return Err(fail(error)),
                    Ok(_) => return Err(UvError::code("ENOTDIR", "mkdir").path(path)),
                    // A dangling symbolic link exists, but its target doesn't.
                    Err(error) => return Err(fail(error)),
                }
            }
            Err(error) if error.kind() == IoErrorKind::NotFound => {
                let parent = std::path::Path::new(&current)
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .map(|parent| parent.to_string_lossy().into_owned());
                let Some(parent) = parent else {
                    return Err(fail(error));
                };
                if let Some(last) = pending.last_mut() {
                    last.1 = true;
                }
                pending.push((parent, false));
            }
            Err(error) => return Err(fail(error)),
        }
    }
    Ok(())
}

fn mkdir(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let path = path_arg(scope, arg(args, 0), "path")?;
    let options = opt(args, 1);
    let (recursive, mode) = match options {
        Some(mode) if mode.is_number() || mode.is_string() => {
            (false, mode_arg(scope, Some(mode), Some(0o777))?)
        }
        _ => (
            boolean_option(scope, options, c"recursive", "options.recursive")?,
            mode_arg(scope, option(scope, options, c"mode")?, Some(0o777))?,
        ),
    };
    on_wasi!(variant, {
        let path = path.clone();
        Prepared::future(Some(Encoding::Utf8), async move {
            if !recursive {
                platform::fs::wasi::create_dir(&path)
                    .await
                    .map_err(|error| UvError::new(error, "mkdir").path(&path))?;
                return Ok(Output::Unit);
            }
            // The first directory created is the outermost ancestor that doesn't exist yet.
            let mut first = None;
            let mut candidate = Some(std::path::Path::new(&path));
            while let Some(directory) =
                candidate.filter(|directory| !directory.as_os_str().is_empty())
            {
                let name = directory.to_string_lossy();
                match platform::fs::wasi::stat(&name, true).await {
                    Err(error) if error.kind() == IoErrorKind::NotFound => {
                        first = Some(name.into_owned())
                    }
                    _ => break,
                }
                candidate = directory.parent();
            }
            make_directories_wasi(&path).await?;
            Ok(Output::MaybeText(first))
        })
    });
    Ok(Prepared::new(Some(Encoding::Utf8), move || {
        let fail = |error| UvError::new(error, "mkdir").path(&path);
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(recursive);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, mode);
        #[cfg(not(unix))]
        let _ = mode;
        if !recursive {
            builder.create(&path).map_err(fail)?;
            return Ok(Output::Unit);
        }
        // The first directory created is the outermost ancestor that doesn't exist yet.
        let mut first = None;
        let mut candidate = Some(std::path::Path::new(&path));
        while let Some(directory) = candidate.filter(|directory| !directory.as_os_str().is_empty())
        {
            match std::fs::metadata(directory) {
                Err(error) if error.kind() == IoErrorKind::NotFound => {
                    first = Some(directory.to_string_lossy().into_owned())
                }
                _ => break,
            }
            candidate = directory.parent();
        }
        builder.recursive(false);
        make_directories(&builder, std::path::Path::new(&path))?;
        Ok(Output::MaybeText(first))
    }))
}

js::instance_local! {
    /// Whether `mkdtemp` warned that a prefix ending with `X` isn't portable.
    static WARNED_NON_PORTABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn mkdtemp(scope: &Scope<'_>, args: &[HandleValue<'_>], _: Variant) -> Result<Prepared, Thrown> {
    let prefix = path_arg(scope, arg(args, 0), "prefix")?;
    // Node's `warnOnNonPortableTemplate`, which warns once.
    if prefix.ends_with('X') && !WARNED_NON_PORTABLE.get() {
        WARNED_NON_PORTABLE.set(true);
        let message = "mkdtemp() templates ending with X are not portable. For details see: \
                       https://nodejs.org/api/fs.html";
        crate::process::emit_warning(scope, message, "Warning")?;
    }
    let encoding = encoding_option(scope, opt(args, 1), Some(Encoding::Utf8))?;
    Ok(Prepared::new(encoding, move || {
        const CHARACTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        loop {
            let mut random = [0u8; 6];
            platform::random::fill(&mut random);
            let suffix: String = random
                .iter()
                .map(|byte| CHARACTERS[usize::from(*byte) % CHARACTERS.len()] as char)
                .collect();
            let path = format!("{prefix}{suffix}");
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Output::Text(path)),
                Err(error) if error.kind() == IoErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(UvError::new(error, "mkdtemp").path(&format!("{prefix}XXXXXX")))
                }
            }
        }
    }))
}

fn open(scope: &Scope<'_>, args: &[HandleValue<'_>], variant: Variant) -> Result<Prepared, Thrown> {
    let path = path_arg(scope, arg(args, 0), "path")?;
    let flags = flags_arg(scope, opt(args, 1), "r")?;
    let mode = mode_arg(scope, opt(args, 2), Some(0o666))?;
    on_wasi!(
        variant,
        Prepared::future(None, async move {
            Ok(Output::WasiFile(flags.open_wasi(&path).await?))
        })
    );
    Ok(Prepared::new(None, move || {
        Ok(Output::File(flags.open(&path, mode)?))
    }))
}

/// The buffer, offset, length and position arguments of `read` and `write` from `index`: either
/// a buffer followed by an offset, length and position, or by an object with those properties.
/// Without a buffer, `options.buffer` is the buffer, or for a `read` a new 16 KiB `Buffer`.
fn buffer_args<'s>(
    scope: &'s Scope<'_>,
    args: &[HandleValue<'_>],
    index: usize,
    read: bool,
    variant: Variant,
) -> Result<(Object<'s>, usize, usize, Option<u64>), Thrown> {
    let first = arg(args, index);
    let is_view = |value: HandleValue<'_>| {
        Object::from_value(scope, *value)
            .ok()
            .and_then(js::ArrayBufferView::from_object)
            .is_some()
    };
    let (buffer, offset, length, position) = if is_view(first) {
        let buffer = Object::from_value(scope, *first).map_err(|e| Thrown::from(e.throw(scope)))?;
        match opt(args, index + 1) {
            Some(options) if !options.is_number() && !options.is_null() => (
                buffer,
                option(scope, Some(options), c"offset")?,
                option(scope, Some(options), c"length")?,
                option(scope, Some(options), c"position")?,
            ),
            _ => (
                buffer,
                opt(args, index + 1),
                opt(args, index + 2),
                opt(args, index + 3),
            ),
        }
    } else {
        let options = Some(first);
        let buffer = match option(scope, options, c"buffer")? {
            Some(buffer) if is_view(buffer) => {
                Object::from_value(scope, *buffer).map_err(|e| Thrown::from(e.throw(scope)))?
            }
            Some(buffer) => {
                return Err(NodeError::invalid_arg_type(
                    scope,
                    "buffer",
                    "an instance of Buffer, TypedArray, or DataView",
                    buffer,
                )
                .into())
            }
            None if read => crate::buffer::new_buffer(scope, &[0; 16384])?,
            None => {
                return Err(NodeError::invalid_arg_type(
                    scope,
                    "buffer",
                    "an instance of Buffer, TypedArray, or DataView",
                    first,
                )
                .into())
            }
        };
        (
            buffer,
            option(scope, options, c"offset")?,
            option(scope, options, c"length")?,
            option(scope, options, c"position")?,
        )
    };
    let size = js::ArrayBufferView::from_object(buffer).map_or(0, |view| view.byte_length());
    let offset = match offset.filter(|offset| !offset.is_null()) {
        Some(offset) => validate_integer(
            scope,
            offset,
            "offset",
            0.0,
            crate::errors::MAX_SAFE_INTEGER,
        )?,
        None => 0.0,
    };
    let out_of_range = |name: &str, range: String, received: f64| -> Thrown {
        let received = crate::errors::out_of_range_received(received);
        NodeError::out_of_range(name, &range, &received).into()
    };
    let available = size as f64 - offset;
    let length = if read {
        // Node.js's `length |= 0`, then `validateOffsetLengthRead`. `FileHandle.prototype.read`
        // reads to the end of the buffer for a `null` length instead.
        let length =
            match length.filter(|length| !(variant == Variant::Promise && length.is_null())) {
                Some(length) => to_int32(f64::from_jsval(scope, length, ())?),
                None => available,
            };
        if length != 0.0 {
            if size == 0 {
                let buffer = buffer.to_jsval_throwing(scope)?;
                let reason = "is empty and cannot be written";
                return Err(NodeError::invalid_arg_value(scope, "buffer", buffer, reason).into());
            }
            if length < 0.0 {
                return Err(out_of_range("length", ">= 0".into(), length));
            }
            if length > available {
                return Err(out_of_range("length", format!("<= {available}"), length));
            }
        }
        length
    } else {
        // Node.js's `validateOffsetLengthWrite`, for a length that is a number.
        if offset > size as f64 {
            return Err(out_of_range("offset", format!("<= {size}"), offset));
        }
        match length.filter(|length| length.is_number()) {
            Some(length) => {
                let number = length.to_number();
                if number > available {
                    return Err(out_of_range("length", format!("<= {available}"), number));
                }
                if number < 0.0 {
                    return Err(out_of_range("length", ">= 0".into(), number));
                }
                validate_integer(scope, length, "length", 0.0, 2147483647.0)?
            }
            None => available,
        }
    };
    let (offset, length) = (offset as usize, length as usize);
    let position = position_arg(scope, position, read.then_some(length))?;
    Ok((buffer, offset, length, position))
}

/// `number` converted to a 32-bit signed integer, as JS's `| 0` converts it.
fn to_int32(number: f64) -> f64 {
    if !number.is_finite() {
        return 0.0;
    }
    let wrapped = number.trunc().rem_euclid(4294967296.0);
    if wrapped >= 2147483648.0 {
        wrapped - 4294967296.0
    } else {
        wrapped
    }
}

/// The values `read` and `write` give for the variant: the byte count for a synchronous call,
/// the count and `buffer` for a callback, and an object without a prototype with the count, named
/// `count_name`, and `buffer` for a promise.
fn count_values<'s>(
    scope: &'s Scope<'_>,
    variant: Variant,
    count: usize,
    count_name: &'static std::ffi::CStr,
    buffer: HandleValue<'s>,
) -> Result<Vec<HandleValue<'s>>, Thrown> {
    let count = scope.root_value(js::value::from_f64(count as f64));
    Ok(match variant {
        Variant::Sync => vec![count],
        Variant::Callback => vec![count, buffer],
        Variant::Promise => {
            let result = Object::new_with_null_prototype(scope)?;
            result.set_property(scope, count_name, count)?;
            result.set_property(scope, c"buffer", buffer)?;
            vec![result.to_jsval_throwing(scope)?]
        }
    })
}

fn read(scope: &Scope<'_>, args: &[HandleValue<'_>], variant: Variant) -> Result<Prepared, Thrown> {
    let handle = handle_arg(scope, args, 0, "read")?;
    let (buffer, offset, length, position) = buffer_args(scope, args, 1, true, variant)?;
    let buffer = RootedHeap::<js::object::Object>::new(buffer);
    if variant == Variant::Sync {
        // The synchronous read runs in `finish`, and reads into the buffer itself.
        let finish: Finish = Box::new(move |scope, _, variant| {
            let buffer = buffer.get(scope);
            let read = move |target: &mut [u8]| handle?.read(target, position);
            let count = match js::ArrayBufferView::from_object(buffer) {
                Some(view) => view.with_bytes_mut(|bytes| {
                    let end = (offset + length).min(bytes.len());
                    read(bytes.get_mut(offset..end).unwrap_or(&mut []))
                }),
                None => read(&mut []),
            }
            .map_err(|error| Thrown::from(js::error::ThrowException::throw(error, scope)))?;
            let buffer = buffer.to_jsval_throwing(scope)?;
            count_values(scope, variant, count, c"bytesRead", buffer)
        });
        return Ok(Prepared::custom(|| Ok(Output::Unit), finish));
    }
    let finish: Finish = Box::new(move |scope, output, variant| {
        let Output::Bytes(bytes) = output else {
            unreachable!("read produces bytes");
        };
        let buffer = buffer.get(scope);
        if let Some(view) = js::ArrayBufferView::from_object(buffer) {
            // The buffer may have been detached or shrunk while the read ran.
            let end = (offset + bytes.len()).min(view.byte_length());
            if offset < end {
                view.with_bytes_mut(|target| {
                    target[offset..end].copy_from_slice(&bytes[..end - offset]);
                });
            }
        }
        let buffer = buffer.to_jsval_throwing(scope)?;
        count_values(scope, variant, bytes.len(), c"bytesRead", buffer)
    });
    on_wasi!(
        variant,
        suspends(&handle),
        Prepared::custom_future(
            async move {
                let bytes = wasi_file(handle)?
                    .read(length, position)
                    .await
                    .map_err(|error| UvError::new(error, "read"))?;
                Ok(Output::Bytes(bytes))
            },
            finish
        )
    );
    let Some(mut bytes) = crate::support::try_filled_vec(length) else {
        js::exception::report_out_of_memory(scope);
        return Err(Thrown::Pending);
    };
    Ok(Prepared::custom(
        move || {
            let count = handle?.read(&mut bytes, position)?;
            bytes.truncate(count);
            Ok(Output::Bytes(bytes))
        },
        finish,
    ))
}

/// A read of up to `length` bytes from the descriptor `fd` at `position`, as `fs.read` with a new
/// buffer of `length` bytes reads them, which gives the callback the count and a new `Buffer` of
/// the bytes read. The memory for the bytes is allocated fallibly. The bytes of a short read are
/// copied into memory of their size, as Node's `ReadStream` copies them, unless allocating it
/// fails.
pub(super) fn read_chunk(
    scope: &Scope<'_>,
    fd: HandleValue<'_>,
    length: usize,
    position: HandleValue<'_>,
) -> Result<Prepared, Thrown> {
    let handle = handle_arg(scope, &[fd], 0, "read")?;
    let position = position_arg(scope, Some(position), Some(length))?;
    let finish: Finish = Box::new(move |scope, output, _| {
        let Output::Bytes(bytes) = output else {
            unreachable!("read produces bytes");
        };
        let count = scope.root_value(js::value::from_f64(bytes.len() as f64));
        let chunk = crate::buffer::buffer_from_vec(scope, bytes)?.to_jsval_throwing(scope)?;
        Ok(vec![count, chunk])
    });
    on_wasi!(
        Variant::Callback,
        suspends(&handle),
        Prepared::custom_future(
            async move {
                let bytes = wasi_file(handle)?
                    .read(length, position)
                    .await
                    .map_err(|error| UvError::new(error, "read"))?;
                Ok(Output::Bytes(bytes))
            },
            finish
        )
    );
    let Some(mut bytes) = crate::support::try_filled_vec(length) else {
        js::exception::report_out_of_memory(scope);
        return Err(Thrown::Pending);
    };
    Ok(Prepared::custom(
        move || {
            let count = handle?.read(&mut bytes, position)?;
            bytes.truncate(count);
            if count < length {
                bytes = js::typedarray::try_copy(&bytes).unwrap_or(bytes);
            }
            Ok(Output::Bytes(bytes))
        },
        finish,
    ))
}

fn write(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let handle = handle_arg(scope, args, 0, "write")?;
    let data = arg(args, 1);
    if data.is_string() {
        // `write(fd, string, position, encoding)`, where an unknown encoding is UTF-8.
        let encoding = match opt(args, 3).filter(|encoding| encoding.is_string()) {
            Some(encoding) => Encoding::parse(&String::from_jsval(scope, encoding, ())?),
            None => None,
        };
        let encoding = encoding.unwrap_or_default();
        let length = js::JSString::from_value(scope, data)?.len();
        if encoding == Encoding::Hex && length % 2 != 0 {
            let reason = format!("is invalid for data of length {length}");
            return Err(
                NodeError::invalid_arg_value(scope, "encoding", arg(args, 3), &reason).into(),
            );
        }
        let bytes = data_arg(scope, data, encoding)?;
        let position = position_arg(scope, opt(args, 2), None)?;
        let text = String::from_jsval(scope, data, ())?;
        let finish: Finish = Box::new(move |scope, output, variant| {
            let Output::Count(count) = output else {
                unreachable!("write produces a count");
            };
            let text = js::JSString::from_str(scope, &text)?.to_jsval_throwing(scope)?;
            count_values(scope, variant, count, c"bytesWritten", text)
        });
        return Ok(write_bytes(variant, handle, bytes, position, finish));
    }
    let (buffer, offset, length, position) = buffer_args(scope, args, 1, false, variant)?;
    if variant == Variant::Sync {
        // The synchronous write runs in `finish`, and writes from the buffer itself.
        let buffer = RootedHeap::<js::object::Object>::new(buffer);
        let finish: Finish = Box::new(move |scope, _, variant| {
            let buffer = buffer.get(scope);
            let write = move |bytes: &[u8]| -> Result<usize, UvError> {
                handle?.write(bytes, position)?;
                Ok(bytes.len())
            };
            let count = match js::ArrayBufferView::from_object(buffer) {
                Some(view) => view.with_bytes(|bytes| {
                    write(bytes.get(offset..offset + length).unwrap_or_default())
                }),
                None => write(&[]),
            }
            .map_err(|error| Thrown::from(js::error::ThrowException::throw(error, scope)))?;
            let buffer = buffer.to_jsval_throwing(scope)?;
            count_values(scope, variant, count, c"bytesWritten", buffer)
        });
        return Ok(Prepared::custom(|| Ok(Output::Unit), finish));
    }
    let bytes = match js::ArrayBufferView::from_object(buffer) {
        Some(view) => view
            .with_bytes(|bytes| js::typedarray::try_copy(&bytes[offset..offset + length]))
            .ok_or_else(|| {
                js::exception::report_out_of_memory(scope);
                js::error::ExnThrown
            })?,
        None => Vec::new(),
    };
    let buffer = RootedHeap::<js::object::Object>::new(buffer);
    let finish: Finish = Box::new(move |scope, output, variant| {
        let Output::Count(count) = output else {
            unreachable!("write produces a count");
        };
        let buffer = buffer.get(scope).to_jsval_throwing(scope)?;
        count_values(scope, variant, count, c"bytesWritten", buffer)
    });
    Ok(write_bytes(variant, handle, bytes, position, finish))
}

/// The operation of `write` called as `variant`: write all of `bytes` to the file `handle` at
/// `position`, or at the current position for `None`, and convert the count with `finish`.
fn write_bytes(
    variant: Variant,
    handle: Result<Handle, UvError>,
    bytes: Vec<u8>,
    position: Option<u64>,
    finish: Finish,
) -> Prepared {
    #[cfg(target_arch = "wasm32")]
    if variant != Variant::Sync && suspends(&handle) {
        let run = async move {
            let count = bytes.len();
            wasi_file(handle)?
                .write(bytes, position)
                .await
                .map_err(|error| UvError::new(error, "write"))?;
            Ok(Output::Count(count))
        };
        return Prepared::custom_future(run, finish);
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = variant;
    Prepared::custom(
        move || {
            handle?.write(&bytes, position)?;
            Ok(Output::Count(bytes.len()))
        },
        finish,
    )
}

fn read_file(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let target = target_arg(scope, arg(args, 0), "path")?;
    let options = opt(args, 1);
    let encoding = encoding_option(scope, options, None)?;
    let flags = flags_arg(scope, option(scope, options, c"flag")?, "r")?;
    let handle = match target {
        Target::Fd(fd) => Some(Handle::of(fd, "read")),
        Target::Path(_) => None,
    };
    on_wasi!(
        variant,
        handle.as_ref().is_none_or(suspends),
        Prepared::future(encoding, async move {
            let file = match (target, handle) {
                (_, Some(handle)) => wasi_file(handle)?,
                (Target::Path(path), None) => flags.open_wasi(&path).await?.into(),
                (Target::Fd(_), None) => unreachable!("descriptors have a handle"),
            };
            let bytes = file
                .read_to_end()
                .await
                .map_err(|error| UvError::new(error, "read"))?;
            Ok(Output::Bytes(bytes))
        })
    );
    Ok(Prepared::new(encoding, move || {
        let bytes = match (target, handle) {
            (_, Some(handle)) => handle?.read_to_end()?,
            (Target::Path(path), None) => {
                let file = flags.open(&path, 0o666)?;
                Handle::File(file.into()).read_to_end()?
            }
            (Target::Fd(_), None) => unreachable!("descriptors have a handle"),
        };
        Ok(Output::Bytes(bytes))
    }))
}

/// The entries of the directory `path`, each directory's sorted by name, and with `recursive`
/// those of its subdirectories after them in breadth-first order, named relative to `path`.
/// Symlinks to directories are followed. Each entry's `parent` is its directory with
/// `with_parent`, and empty otherwise.
fn list_directory(path: &str, recursive: bool, with_parent: bool) -> Result<Vec<Entry>, UvError> {
    let mut entries = Vec::new();
    let mut pending = vec![(path.to_string(), String::new())];
    let mut next = 0;
    while let Some((directory, relative)) = pending.get(next).cloned() {
        next += 1;
        let fail = |error| UvError::new(error, "scandir").path(&directory);
        let mut listed = Vec::new();
        for entry in std::fs::read_dir(&directory).map_err(fail)? {
            let entry = entry.map_err(fail)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = entry.file_type().map_or(FileKind::Unknown, FileKind::of);
            listed.push((name, kind));
        }
        listed.sort_by(|(a, _), (b, _)| a.cmp(b));
        for (name, kind) in listed {
            if recursive {
                let full_path = crate::path::normalize_path(&format!("{directory}/{name}"));
                if kind == FileKind::Directory
                    || std::fs::metadata(&full_path).is_ok_and(|metadata| metadata.is_dir())
                {
                    pending.push((full_path, format!("{relative}{name}/")));
                }
            }
            entries.push(Entry {
                name: if recursive {
                    format!("{relative}{name}")
                } else {
                    name
                },
                parent: if with_parent {
                    directory.clone()
                } else {
                    String::new()
                },
                kind,
            });
        }
    }
    Ok(entries)
}

/// [`list_directory`] over `platform::fs::wasi`.
#[cfg(target_arch = "wasm32")]
async fn list_directory_wasi(
    path: &str,
    recursive: bool,
    with_parent: bool,
) -> Result<Vec<Entry>, UvError> {
    let mut entries = Vec::new();
    let mut pending = vec![(path.to_string(), String::new())];
    let mut next = 0;
    while let Some((directory, relative)) = pending.get(next).cloned() {
        next += 1;
        let mut listed = platform::fs::wasi::read_dir(&directory)
            .await
            .map_err(|error| UvError::new(error, "scandir").path(&directory))?;
        listed.sort_by(|(a, _), (b, _)| a.cmp(b));
        for (name, kind) in listed {
            if recursive {
                let full_path = crate::path::normalize_path(&format!("{directory}/{name}"));
                if kind == FileKind::Directory
                    || platform::fs::wasi::stat(&full_path, true)
                        .await
                        .is_ok_and(|stat| stat.kind == FileKind::Directory)
                {
                    pending.push((full_path, format!("{relative}{name}/")));
                }
            }
            entries.push(Entry {
                name: if recursive {
                    format!("{relative}{name}")
                } else {
                    name
                },
                parent: if with_parent {
                    directory.clone()
                } else {
                    String::new()
                },
                kind,
            });
        }
    }
    Ok(entries)
}

/// The result of `readdir`: the entries' names, or with `file_types` the entries with their own
/// names.
fn readdir_output(entries: Vec<Entry>, file_types: bool) -> Output {
    if file_types {
        return Output::Entries(
            entries
                .into_iter()
                .map(|entry| Entry {
                    name: entry
                        .name
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                    ..entry
                })
                .collect(),
        );
    }
    Output::Names(entries.into_iter().map(|entry| entry.name).collect())
}

fn readdir(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let path = path_arg(scope, arg(args, 0), "path")?;
    let options = opt(args, 1);
    let encoding = encoding_option(scope, options, Some(Encoding::Utf8))?;
    let file_types = flag_option(scope, options, c"withFileTypes")?;
    let recursive = match option(scope, options, c"recursive")?.filter(|value| !value.is_null()) {
        Some(value) => crate::errors::validate_boolean(scope, value, "options.recursive")?,
        None => false,
    };
    on_wasi!(
        variant,
        Prepared::future(encoding, async move {
            let entries = list_directory_wasi(&path, recursive, file_types).await?;
            Ok(readdir_output(entries, file_types))
        })
    );
    Ok(Prepared::new(encoding, move || {
        let entries = list_directory(&path, recursive, file_types)?;
        Ok(readdir_output(entries, file_types))
    }))
}

fn readlink(scope: &Scope<'_>, args: &[HandleValue<'_>], _: Variant) -> Result<Prepared, Thrown> {
    let path = path_arg(scope, arg(args, 0), "path")?;
    let encoding = encoding_option(scope, opt(args, 1), Some(Encoding::Utf8))?;
    Ok(Prepared::new(encoding, move || {
        let target = std::fs::read_link(&path)
            .map_err(|error| UvError::new(error, "readlink").path(&path))?;
        Ok(Output::Text(target.to_string_lossy().into_owned()))
    }))
}

/// The canonical absolute path of `path`, with every symlink resolved. On Windows, the verbatim
/// prefix `std::fs::canonicalize` adds is removed: `\\?\C:\` becomes `C:\`, and `\\?\UNC\`
/// becomes `\\`.
fn canonicalize(path: &str) -> std::io::Result<String> {
    let canonical = std::fs::canonicalize(path)?.to_string_lossy().into_owned();
    #[cfg(windows)]
    if let Some(unc) = canonical.strip_prefix(r"\\?\UNC\") {
        return Ok(format!(r"\\{unc}"));
    } else if let Some(local) = canonical.strip_prefix(r"\\?\") {
        return Ok(local.to_string());
    }
    Ok(canonical)
}

/// The error of Node's JS `realpath` for `path`, which `canonicalize` failed with `error`: that
/// of lstat(2) on the first path component that doesn't exist, or of stat(2) on the first
/// dangling symlink, after the components before it are resolved, or else `error` as
/// realpath(3)'s.
fn realpath_error(path: &str, error: std::io::Error) -> UvError {
    let absolute = crate::path::resolve_paths(&[path.to_string()]);
    let mut resolved = String::new();
    for component in absolute
        .split('/')
        .filter(|component| !component.is_empty())
    {
        let candidate = format!("{resolved}/{component}");
        match std::fs::symlink_metadata(&candidate) {
            Err(error) => return UvError::new(error, "lstat").path(&candidate),
            Ok(metadata) if metadata.is_symlink() => {
                if let Err(error) = std::fs::metadata(&candidate) {
                    return UvError::new(error, "stat").path(&candidate);
                }
            }
            Ok(_) => {}
        }
        match canonicalize(&candidate) {
            Ok(canonical) => resolved = canonical.trim_end_matches('/').to_string(),
            Err(_) => break,
        }
    }
    UvError::new(error, "realpath").path(path)
}

fn realpath(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let mut path = path_arg(scope, arg(args, 0), "path")?;
    // Node's JS `realpath` resolves the path first, so the empty path is the working directory.
    if path.is_empty() && variant != Variant::Promise {
        path.push('.');
    }
    let encoding = encoding_option(scope, opt(args, 1), Some(Encoding::Utf8))?;
    Ok(Prepared::new(encoding, move || {
        let resolved = canonicalize(&path).map_err(|error| match variant {
            // `fs.promises.realpath` reports the error of realpath(3). The other variants report
            // the error of Node's JS implementation.
            Variant::Promise => UvError::new(error, "realpath").path(&path),
            Variant::Sync | Variant::Callback => realpath_error(&path, error),
        })?;
        Ok(Output::Text(resolved))
    }))
}

fn rename(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let old = path_arg(scope, arg(args, 0), "oldPath")?;
    let new = path_arg(scope, arg(args, 1), "newPath")?;
    on_wasi!(variant, {
        let (old, new) = (old.clone(), new.clone());
        Prepared::future(None, async move {
            platform::fs::wasi::rename(&old, &new)
                .await
                .map_err(|error| UvError::new(error, "rename").path(&old).dest(&new))?;
            Ok(Output::Unit)
        })
    });
    Ok(Prepared::new(None, move || {
        std::fs::rename(&old, &new)
            .map_err(|error| UvError::new(error, "rename").path(&old).dest(&new))?;
        Ok(Output::Unit)
    }))
}

/// The options of `rm`, validated as Node's `validateRmOptions` validates them: `recursive` and
/// `force` are booleans, `retryDelay` an integer from 0 to 2**31 - 1, and `maxRetries` an integer
/// from 0 to 2**32 - 1. Missing options have their defaults, `false`, `false`, 100 and 0.
struct RmOptions {
    recursive: bool,
    force: bool,
    retry_delay: f64,
    max_retries: f64,
}

impl RmOptions {
    fn of(scope: &Scope<'_>, options: Option<HandleValue<'_>>) -> Result<Self, Thrown> {
        let Some(options) = options else {
            return Ok(RmOptions {
                recursive: false,
                force: false,
                retry_delay: 100.0,
                max_retries: 0.0,
            });
        };
        crate::errors::validate_plain_object(scope, options, "options", false)?;
        let options = Some(options);
        let recursive = boolean_option(scope, options, c"recursive", "options.recursive")?;
        let retry_delay = match option(scope, options, c"retryDelay")? {
            Some(delay) => validate_integer(scope, delay, "options.retryDelay", 0.0, 2147483647.0)?,
            None => 100.0,
        };
        let max_retries = match option(scope, options, c"maxRetries")? {
            Some(retries) => {
                validate_integer(scope, retries, "options.maxRetries", 0.0, 4294967295.0)?
            }
            None => 0.0,
        };
        let force = boolean_option(scope, options, c"force", "options.force")?;
        Ok(RmOptions {
            recursive,
            force,
            retry_delay,
            max_retries,
        })
    }
}

/// Run `remove` once, and again up to `max_retries` times while it fails with `EBUSY`, `EMFILE`,
/// `ENFILE`, `ENOTEMPTY` or `EPERM`, waiting `retry_delay` milliseconds times the attempt's number
/// before each retry, as Node's `rm` retries.
fn retrying(
    max_retries: f64,
    retry_delay: f64,
    mut remove: impl FnMut() -> Result<(), UvError>,
) -> Result<(), UvError> {
    let mut attempt = 0.0;
    loop {
        match remove() {
            Err(error)
                if attempt < max_retries
                    && ["EBUSY", "EMFILE", "ENFILE", "ENOTEMPTY", "EPERM"]
                        .contains(&error.error.code) =>
            {
                attempt += 1.0;
                std::thread::sleep(Duration::from_secs_f64(attempt * retry_delay / 1000.0));
            }
            result => return result,
        }
    }
}

fn rm(scope: &Scope<'_>, args: &[HandleValue<'_>], variant: Variant) -> Result<Prepared, Thrown> {
    let path = path_arg(scope, arg(args, 0), "path")?;
    let RmOptions {
        recursive,
        force,
        retry_delay,
        max_retries,
    } = RmOptions::of(scope, opt(args, 1))?;
    let sync = variant == Variant::Sync;
    Ok(Prepared::new(None, move || {
        // `rmSync` reports removal errors with a positive `errno` and the syscall `rm`, and skips
        // the lstat(2) check for `force` with `recursive`, as Node's `validateRmOptionsSync`
        // does. The other variants report the syscall that failed.
        let fail = |error, syscall| match sync {
            true => UvError::new(error, "rm").path(&path).positive_errno(),
            false => UvError::new(error, syscall).path(&path),
        };
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if force && error.kind() == IoErrorKind::NotFound => {
                return Ok(Output::Unit)
            }
            Err(error) if sync && force && recursive => {
                if error.kind() == IoErrorKind::NotADirectory {
                    return Ok(Output::Unit);
                }
                return Err(fail(error, "lstat"));
            }
            Err(error) => return Err(UvError::new(error, "lstat").path(&path)),
        };
        if metadata.is_dir() && !recursive {
            return Err(UvError::rm_directory(&path));
        }
        retrying(max_retries, retry_delay, || match metadata.is_dir() {
            true => std::fs::remove_dir_all(&path).map_err(|error| fail(error, "rmdir")),
            false => std::fs::remove_file(&path).map_err(|error| fail(error, "unlink")),
        })?;
        Ok(Output::Unit)
    }))
}

fn rmdir(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let path = path_arg(scope, arg(args, 0), "path")?;
    let recursive = flag_option(scope, opt(args, 1), c"recursive")?;
    on_wasi!(variant, !recursive, {
        let path = path.clone();
        Prepared::future(None, async move {
            platform::fs::wasi::remove_dir(&path)
                .await
                .map_err(|error| UvError::new(error, "rmdir").path(&path))?;
            Ok(Output::Unit)
        })
    });
    Ok(Prepared::new(None, move || {
        let result = if recursive {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_dir(&path)
        };
        result.map_err(|error| UvError::new(error, "rmdir").path(&path))?;
        Ok(Output::Unit)
    }))
}

fn unlink(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let path = path_arg(scope, arg(args, 0), "path")?;
    on_wasi!(variant, {
        let path = path.clone();
        Prepared::future(None, async move {
            platform::fs::wasi::remove_file(&path)
                .await
                .map_err(|error| UvError::new(error, "unlink").path(&path))?;
            Ok(Output::Unit)
        })
    });
    Ok(Prepared::new(None, move || {
        std::fs::remove_file(&path).map_err(|error| UvError::new(error, "unlink").path(&path))?;
        Ok(Output::Unit)
    }))
}
