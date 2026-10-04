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
    data_arg, encoding_option, fd_arg, flag_option, flags_arg, mode_arg, option, path_arg,
    remove_file, target_arg, Entry, Finish, Handle, Output, Prepared, Target, UvError, Variant,
};
use crate::buffer::Encoding;
use crate::errors::{validate_integer, NodeError, Thrown};

pub(super) const SYNC: u8 = 1;
pub(super) const CALLBACK: u8 = 2;
pub(super) const PROMISE: u8 = 4;
const ALL: u8 = SYNC | CALLBACK | PROMISE;
const NOT_PROMISE: u8 = SYNC | CALLBACK;

/// The parser of an operation's arguments.
type Parse = fn(&Scope<'_>, &[HandleValue<'_>], Variant) -> Result<Prepared, Thrown>;

/// An operation: the name of its callback and promise functions, the name of its synchronous
/// function, the variants it has functions for, the `length` of the synchronous function, and
/// its parser.
pub(super) struct OpDef {
    pub(super) name: &'static str,
    pub(super) sync: &'static str,
    pub(super) variants: u8,
    pub(super) nargs: u32,
    pub(super) parse: Parse,
}

const fn op(
    name: &'static str,
    sync: &'static str,
    variants: u8,
    nargs: u32,
    parse: Parse,
) -> OpDef {
    OpDef {
        name,
        sync,
        variants,
        nargs,
        parse,
    }
}

pub(super) const OPS: [OpDef; 34] = [
    op("access", "accessSync", ALL, 2, access),
    op("appendFile", "appendFileSync", ALL, 3, append_file),
    op("chmod", "chmodSync", ALL, 2, chmod),
    op("chown", "chownSync", ALL, 3, chown),
    op("close", "closeSync", NOT_PROMISE, 1, close),
    op("copyFile", "copyFileSync", ALL, 3, copy_file),
    op("exists", "existsSync", NOT_PROMISE, 1, exists),
    op("fchmod", "fchmodSync", NOT_PROMISE, 2, fchmod),
    op("fchown", "fchownSync", NOT_PROMISE, 3, fchown),
    op("fdatasync", "fdatasyncSync", NOT_PROMISE, 1, fdatasync),
    op("fstat", "fstatSync", NOT_PROMISE, 2, fstat),
    op("fsync", "fsyncSync", NOT_PROMISE, 1, fsync),
    op("ftruncate", "ftruncateSync", NOT_PROMISE, 2, ftruncate),
    op("futimes", "futimesSync", NOT_PROMISE, 3, futimes),
    op("link", "linkSync", ALL, 2, link),
    op("lstat", "lstatSync", ALL, 2, lstat),
    op("mkdir", "mkdirSync", ALL, 2, mkdir),
    op("mkdtemp", "mkdtempSync", ALL, 2, mkdtemp),
    op("open", "openSync", NOT_PROMISE, 3, open),
    op("read", "readSync", NOT_PROMISE, 5, read),
    op("readFile", "readFileSync", ALL, 2, read_file),
    op("readdir", "readdirSync", ALL, 2, readdir),
    op("readlink", "readlinkSync", ALL, 2, readlink),
    op("realpath", "realpathSync", ALL, 2, realpath),
    op("rename", "renameSync", ALL, 2, rename),
    op("rm", "rmSync", ALL, 2, rm),
    op("rmdir", "rmdirSync", ALL, 2, rmdir),
    op("stat", "statSync", ALL, 2, stat),
    op("symlink", "symlinkSync", ALL, 3, symlink),
    op("truncate", "truncateSync", ALL, 2, truncate),
    op("unlink", "unlinkSync", ALL, 1, unlink),
    op("utimes", "utimesSync", ALL, 3, utimes),
    op("write", "writeSync", NOT_PROMISE, 5, write),
    op("writeFile", "writeFileSync", ALL, 3, write_file),
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
fn arg<'a>(args: &[HandleValue<'a>], index: usize) -> HandleValue<'a> {
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
/// from -1 and throws for other values, as Node's `validatePosition` does. A write takes an
/// integer from -1, and writes at the current position for values that aren't numbers.
fn position_arg(
    scope: &Scope<'_>,
    value: Option<HandleValue<'_>>,
    read_length: Option<usize>,
) -> Result<Option<u64>, Thrown> {
    let Some(value) = value.filter(|value| !value.is_null() && !value.is_undefined()) else {
        return Ok(None);
    };
    if value.is_number() {
        let number = validate_integer(scope, value, "position", -1.0, 9007199254740991.0)?;
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

/// The time the argument `value` gives, as Node's `toUnixTimestamp` reads it: seconds since the
/// epoch as a finite number, where negative numbers give the current time, a string that converts
/// to a number, or a `Date`. `None` leaves the time unchanged, which an invalid `Date` does.
fn time_arg(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<Option<SystemTime>, Thrown> {
    let invalid = || -> Thrown {
        NodeError::invalid_arg_type(
            scope,
            "time",
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

fn access(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let path = path_arg(scope, arg(args, 0), "path")?;
    let mode = match opt(args, 1) {
        Some(mode) => validate_integer(scope, mode, "mode", 0.0, 7.0)? as u32,
        None => 0,
    };
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
    let mode = match opt(args, 2) {
        Some(mode) => validate_integer(scope, mode, "mode", 0.0, 7.0)? as u32,
        None => 0,
    };
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
    #[cfg(not(unix))]
    let _ = source_metadata;
    if mode & 1 == 0 {
        std::fs::copy(source, destination).map_err(fail)?;
        return Ok(());
    }
    // `COPYFILE_EXCL` creates the destination with `O_EXCL`, so a file created
    // concurrently makes the copy fail.
    let mut input = std::fs::File::open(source).map_err(fail)?;
    let permissions = input.metadata().map_err(fail)?.permissions();
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(fail)?;
    std::io::copy(&mut input, &mut output).map_err(fail)?;
    output.set_permissions(permissions).map_err(fail)?;
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
        Some(length) => Ok(
            validate_integer(scope, length, "len", f64::MIN, 9007199254740991.0)?.max(0.0) as u64,
        ),
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

/// The access and modification time arguments of `utimes` and `futimes`, from `index`. `None`
/// leaves a time unchanged.
fn times_args(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    index: usize,
) -> Result<(Option<SystemTime>, Option<SystemTime>), Thrown> {
    let accessed = time_arg(scope, arg(args, index))?;
    let modified = time_arg(scope, arg(args, index + 1))?;
    Ok((accessed, modified))
}

fn futimes(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    variant: Variant,
) -> Result<Prepared, Thrown> {
    let handle = handle_arg(scope, args, 0, "futime")?;
    let (accessed, modified) = times_args(scope, args, 1)?;
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
    let (accessed, modified) = times_args(scope, args, 1)?;
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
    let target = path_arg(scope, arg(args, 0), "target")?;
    let path = path_arg(scope, arg(args, 1), "path")?;
    Ok(Prepared::new(None, move || {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(&target, &path);
        #[cfg(windows)]
        let result = if std::path::Path::new(&target).is_dir() {
            std::os::windows::fs::symlink_dir(&target, &path)
        } else {
            std::os::windows::fs::symlink_file(&target, &path)
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
    let throw_if_no_entry = match option(scope, opt(args, 1), c"throwIfNoEntry")? {
        Some(value) => crate::stream::truthy(scope, value),
        None => true,
    };
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
/// does. An existing entry that isn't a directory gives `EEXIST` for the `outermost` path, and
/// `ENOTDIR` for an ancestor. If it can't be read, as a dangling symlink can't, its error is the
/// `outermost` path's error.
fn make_directories(
    builder: &std::fs::DirBuilder,
    path: &std::path::Path,
    outermost: bool,
) -> Result<(), UvError> {
    let name = path.to_string_lossy();
    let fail = |error| UvError::new(error, "mkdir").path(&name);
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == IoErrorKind::AlreadyExists => match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => Ok(()),
            Ok(_) if outermost => Err(fail(error)),
            Err(error) if outermost => Err(fail(error)),
            _ => Err(UvError::code("ENOTDIR", "mkdir").path(&name)),
        },
        Err(error) if error.kind() == IoErrorKind::NotFound => {
            match path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                Some(parent) => make_directories(builder, parent, false)?,
                None => return Err(fail(error)),
            }
            builder.create(path).map_err(fail)
        }
        Err(error) => Err(fail(error)),
    }
}

/// [`make_directories`] over `platform::fs::wasi`, which creates directories without modes.
#[cfg(target_arch = "wasm32")]
fn make_directories_wasi(
    path: String,
    outermost: bool,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), UvError>>>> {
    Box::pin(async move {
        let fail = |error| UvError::new(error, "mkdir").path(&path);
        match platform::fs::wasi::create_dir(&path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == IoErrorKind::AlreadyExists => {
                match platform::fs::wasi::stat(&path, true).await {
                    Ok(stat) if stat.kind == FileKind::Directory => Ok(()),
                    Ok(_) if outermost => Err(fail(error)),
                    Err(error) if outermost => Err(fail(error)),
                    _ => Err(UvError::code("ENOTDIR", "mkdir").path(&path)),
                }
            }
            Err(error) if error.kind() == IoErrorKind::NotFound => {
                match std::path::Path::new(&path)
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                {
                    Some(parent) => {
                        let parent = parent.to_string_lossy().into_owned();
                        make_directories_wasi(parent, false).await?;
                    }
                    None => return Err(fail(error)),
                }
                platform::fs::wasi::create_dir(&path).await.map_err(fail)
            }
            Err(error) => Err(fail(error)),
        }
    })
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
            flag_option(scope, options, c"recursive")?,
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
            make_directories_wasi(path.clone(), true).await?;
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
        make_directories(&builder, std::path::Path::new(&path), true)?;
        Ok(Output::MaybeText(first))
    }))
}

fn mkdtemp(scope: &Scope<'_>, args: &[HandleValue<'_>], _: Variant) -> Result<Prepared, Thrown> {
    let prefix = path_arg(scope, arg(args, 0), "prefix")?;
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
        Some(offset) => validate_integer(scope, offset, "offset", 0.0, size as f64)? as usize,
        None => 0,
    };
    let length = match length.filter(|length| !length.is_null()) {
        Some(length) => {
            validate_integer(scope, length, "length", 0.0, (size - offset) as f64)? as usize
        }
        None => size - offset,
    };
    let position = position_arg(scope, position, read.then_some(length))?;
    Ok((buffer, offset, length, position))
}

/// The values `read` and `write` give for the variant: the byte count for a synchronous call,
/// the count and `buffer` for a callback, and an object with the count, named `count_name`, and
/// `buffer` for a promise.
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
            let result = Object::new_plain(scope)?;
            result.set_property(scope, count_name, count)?;
            result.set_property(scope, c"buffer", buffer)?;
            vec![result.to_jsval_throwing(scope)?]
        }
    })
}

fn read(scope: &Scope<'_>, args: &[HandleValue<'_>], variant: Variant) -> Result<Prepared, Thrown> {
    let handle = handle_arg(scope, args, 0, "read")?;
    let (buffer, offset, length, position) = buffer_args(scope, args, 1, true)?;
    let buffer = RootedHeap::<js::object::Object>::new(buffer);
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
                    target[offset..end].copy_from_slice(&bytes[..end - offset])
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
    Ok(Prepared::custom(
        move || {
            let mut bytes = vec![0; length];
            let count = handle?.read(&mut bytes, position)?;
            bytes.truncate(count);
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
    let (buffer, offset, length, position) = buffer_args(scope, args, 1, false)?;
    let bytes = js::ArrayBufferView::from_object(buffer)
        .map(|view| view.with_bytes(|bytes| bytes[offset..offset + length].to_vec()))
        .unwrap_or_default();
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
/// Symlinks to directories are followed.
fn list_directory(path: &str, recursive: bool) -> Result<Vec<Entry>, UvError> {
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
                parent: directory.clone(),
                kind,
            });
        }
    }
    Ok(entries)
}

/// [`list_directory`] over `platform::fs::wasi`.
#[cfg(target_arch = "wasm32")]
async fn list_directory_wasi(path: &str, recursive: bool) -> Result<Vec<Entry>, UvError> {
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
                parent: directory.clone(),
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
    let recursive = flag_option(scope, options, c"recursive")?;
    on_wasi!(
        variant,
        Prepared::future(encoding, async move {
            let entries = list_directory_wasi(&path, recursive).await?;
            Ok(readdir_output(entries, file_types))
        })
    );
    Ok(Prepared::new(encoding, move || {
        let entries = list_directory(&path, recursive)?;
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

/// The canonical absolute path of `path`, with every symlink resolved.
fn canonicalize(path: &str) -> std::io::Result<String> {
    Ok(std::fs::canonicalize(path)?.to_string_lossy().into_owned())
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
    let path = path_arg(scope, arg(args, 0), "path")?;
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

fn rm(scope: &Scope<'_>, args: &[HandleValue<'_>], variant: Variant) -> Result<Prepared, Thrown> {
    let path = path_arg(scope, arg(args, 0), "path")?;
    let options = opt(args, 1);
    let recursive = flag_option(scope, options, c"recursive")?;
    let force = flag_option(scope, options, c"force")?;
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
        if metadata.is_dir() {
            if !recursive {
                return Err(UvError::rm_directory(&path));
            }
            std::fs::remove_dir_all(&path).map_err(|error| fail(error, "rmdir"))?;
        } else {
            std::fs::remove_file(&path).map_err(|error| fail(error, "unlink"))?;
        }
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
