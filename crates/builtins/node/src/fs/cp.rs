// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `fs.cp`, `fs.cpSync` and `fs.promises.cp`, ported from Node.js's `lib/internal/fs/cp/cp.js`,
//! `lib/internal/fs/cp/cp-sync.js` and the C++ functions `cpSync` calls. See
//! <https://nodejs.org/api/fs.html#fscpsrc-dest-options-callback>.
//!
//! `cpSync` copies on the JS thread and calls `filter` as it walks the source. The callback and
//! promise functions run [`AsyncCopy::advance`] on `platform::fs::offload`'s worker threads. It
//! copies until it needs `filter`'s decision on an entry. The JS thread then calls `filter`, waits
//! for the promise it may return, and runs `advance` again with the decision. Without a filter,
//! one `advance` copies everything.

use std::fs::Metadata;
use std::io::ErrorKind as IoErrorKind;

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::handle::RootedHeap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::promise::{PromiseFuture, PromiseOutcome};
use js::{Function, Object};
use platform::fs::{FileKind, FsError, Stat};

use super::ops::{copy_file_blocking, set_mode, time_of_seconds};
use super::{path_arg, refuse_blocking, UvError, Variant};
use crate::errors::{
    describe_value, validate_function, validate_integer, validate_plain_object, ErrorKind,
    NodeError, Thrown,
};
use crate::path::{dirname_of, normalize_path, resolve_paths};

/// The options of a copy, as Node's `validateCpOptions` validates them.
#[derive(Clone, Copy)]
struct Options {
    dereference: bool,
    error_on_exist: bool,
    force: bool,
    preserve_timestamps: bool,
    recursive: bool,
    verbatim_symlinks: bool,
    /// The `COPYFILE_*` flags files are copied with.
    mode: u32,
    /// Whether there is a `filter` function.
    filter: bool,
}

/// The options `options` gives, the object `filter` is called on, and `filter`. As in Node, that
/// object is a copy of `options`' own enumerable properties over the defaults, with `mode` set.
fn parse_options<'s>(
    scope: &'s Scope<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<(Options, Object<'s>, HandleValue<'s>), Thrown> {
    let copy = Object::new_plain(scope)?;
    copy.set_property(scope, c"dereference", false)?;
    copy.set_property(scope, c"errorOnExist", false)?;
    copy.set_property(scope, c"filter", HandleValue::undefined())?;
    copy.set_property(scope, c"force", true)?;
    copy.set_property(scope, c"preserveTimestamps", false)?;
    copy.set_property(scope, c"recursive", false)?;
    copy.set_property(scope, c"verbatimSymlinks", false)?;
    if let Some(options) = options.filter(|options| !options.is_undefined()) {
        let options = validate_plain_object(scope, options, "options", false)?;
        copy.assign(scope, options)?;
    }
    let boolean = |name: &'static std::ffi::CStr, label: &str| -> Result<bool, Thrown> {
        let value = copy.get_property(scope, name)?;
        if !value.is_boolean() {
            return Err(NodeError::invalid_arg_type(scope, label, "of type boolean", value).into());
        }
        Ok(value.to_boolean())
    };
    let dereference = boolean(c"dereference", "options.dereference")?;
    let error_on_exist = boolean(c"errorOnExist", "options.errorOnExist")?;
    let force = boolean(c"force", "options.force")?;
    let preserve_timestamps = boolean(c"preserveTimestamps", "options.preserveTimestamps")?;
    let recursive = boolean(c"recursive", "options.recursive")?;
    let verbatim_symlinks = boolean(c"verbatimSymlinks", "options.verbatimSymlinks")?;
    let mode = copy.get_property(scope, c"mode")?;
    let mode = match mode.is_null_or_undefined() {
        true => 0,
        false => validate_integer(scope, mode, "mode", 0.0, 7.0)? as u32,
    };
    copy.set_property(scope, c"mode", mode)?;
    if dereference && verbatim_symlinks {
        return Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_INCOMPATIBLE_OPTION_PAIR",
            "Option \"dereference\" cannot be used in combination with option \"verbatimSymlinks\"",
        )
        .into());
    }
    let filter = copy.get_property(scope, c"filter")?;
    if !filter.is_undefined() {
        validate_function(scope, filter, "options.filter")?;
    }
    let options = Options {
        dereference,
        error_on_exist,
        force,
        preserve_timestamps,
        recursive,
        verbatim_symlinks,
        mode,
        filter: !filter.is_undefined(),
    };
    Ok((options, copy, filter))
}

// ============================================================================
// Errors
// ============================================================================

/// An error of a copy.
pub(super) enum CpError {
    /// A failed system call.
    Uv(UvError),
    /// A `SystemError` of Node's JS implementation: the code `key`, such as `ERR_FS_CP_EINVAL`,
    /// the errno name `code`, the description `message`, and the path the error is about.
    System {
        key: &'static str,
        code: &'static str,
        message: String,
        path: String,
    },
    /// An `Error` with the code `code`, as Node's C++ implementation throws.
    Plain { code: &'static str, message: String },
}

impl From<UvError> for CpError {
    fn from(error: UvError) -> Self {
        CpError::Uv(error)
    }
}

impl CpError {
    fn system(key: &'static str, code: &'static str, message: String, path: &str) -> Self {
        CpError::System {
            key,
            code,
            message,
            path: path.to_string(),
        }
    }

    fn plain(code: &'static str, message: String) -> Self {
        CpError::Plain { code, message }
    }
}

/// The message of the `SystemError` with the code `key`, before the system call's details.
fn system_error_prefix(key: &str) -> &'static str {
    match key {
        "ERR_FS_CP_DIR_TO_NON_DIR" => "Cannot overwrite non-directory with directory",
        "ERR_FS_CP_EEXIST" => "Target already exists",
        "ERR_FS_CP_EINVAL" => "Invalid src or dest",
        "ERR_FS_CP_FIFO_PIPE" => "Cannot copy a FIFO pipe",
        "ERR_FS_CP_NON_DIR_TO_DIR" => "Cannot overwrite directory with non-directory",
        "ERR_FS_CP_SOCKET" => "Cannot copy a socket file",
        "ERR_FS_CP_SYMLINK_TO_SUBDIRECTORY" => "Cannot overwrite symlink in subdirectory of self",
        "ERR_FS_CP_UNKNOWN" => "Cannot copy an unknown file type",
        _ => "Path is a directory",
    }
}

impl ThrowException for CpError {
    fn throw(self, scope: &Scope<'_>) -> ExnThrown {
        match self {
            CpError::Uv(error) => error.throw(scope),
            CpError::Plain { code, message } => {
                NodeError::new(ErrorKind::Error, code, message).throw(scope)
            }
            CpError::System {
                key,
                code,
                message,
                path,
            } => {
                if let Ok(error) = system_error_value(scope, key, code, &message, &path) {
                    js::exception::set_pending(
                        scope,
                        error,
                        js::native::ExceptionStackBehavior::Capture,
                    );
                }
                ExnThrown
            }
        }
    }
}

/// The `SystemError` with the code `key` that `cp` reports for the errno name `code`, the
/// description `message` and `path`: its message is `${prefix}: cp returned ${code} (${message})
/// ${path}`, and it has the properties `code`, `info`, `errno`, `syscall` and `path`.
fn system_error_value<'s>(
    scope: &'s Scope<'_>,
    key: &'static str,
    code: &'static str,
    message: &str,
    path: &str,
) -> Result<HandleValue<'s>, ExnThrown> {
    let errno = -FsError::from_code(code).errno;
    let text = format!(
        "{}: cp returned {code} ({message}) {path}",
        system_error_prefix(key)
    );
    let error = crate::stream::error_value(scope, NodeError::new(ErrorKind::Error, key, text))?;
    let object = Object::from_value(scope, *error).map_err(|e| e.throw(scope))?;
    object.define_property(scope, c"name", "SystemError", 0)?;
    let to_string =
        Function::new_callback(scope, c"toString", 0, super::system_error_to_string, ())?;
    object.define_property(scope, c"toString", to_string, 0)?;
    let info = Object::new_plain(scope)?;
    info.set_property(scope, c"message", message)?;
    info.set_property(scope, c"path", path)?;
    info.set_property(scope, c"syscall", "cp")?;
    info.set_property(scope, c"errno", errno)?;
    info.set_property(scope, c"code", code)?;
    object.set_property(scope, c"info", info)?;
    object.set_property(scope, c"errno", errno)?;
    object.set_property(scope, c"syscall", "cp")?;
    object.set_property(scope, c"path", path)?;
    Ok(error)
}

// ============================================================================
// Paths and metadata
// ============================================================================

/// `path.join(directory, name)`.
fn join(directory: &str, name: &str) -> String {
    match directory.is_empty() {
        true => normalize_path(name),
        false => normalize_path(&format!("{directory}/{name}")),
    }
}

/// `path.resolve(...paths)`.
fn resolve(paths: &[&str]) -> String {
    let paths: Vec<String> = paths.iter().map(|path| path.to_string()).collect();
    resolve_paths(&paths)
}

/// Whether the absolute path `dest` resolves to is inside, or is, the one `src` resolves to,
/// comparing path components, as Node's `isSrcSubdir` does.
fn is_src_subdir(src: &str, dest: &str) -> bool {
    let src = resolve(&[src]);
    let dest = resolve(&[dest]);
    let dest: Vec<&str> = dest.split('/').filter(|part| !part.is_empty()).collect();
    src.split('/')
        .filter(|part| !part.is_empty())
        .enumerate()
        .all(|(index, part)| dest.get(index) == Some(&part))
}

/// The metadata of `path`, following symlinks if `follow`.
fn metadata(path: &str, follow: bool) -> std::io::Result<Metadata> {
    match follow {
        true => std::fs::metadata(path),
        false => std::fs::symlink_metadata(path),
    }
}

/// The metadata of `path`, following symlinks if `follow`, as `fs.statSync` or `fs.lstatSync`
/// reports it.
fn stat(path: &str, follow: bool) -> Result<Stat, UvError> {
    let syscall = if follow { "stat" } else { "lstat" };
    metadata(path, follow)
        .map(|metadata| Stat::from(&metadata))
        .map_err(|error| UvError::new(error, syscall).path(path))
}

/// The metadata of `path` as [`stat`] reports it, or `None` if `path` doesn't exist. Following
/// symlinks, a path through a file doesn't exist either.
fn stat_if_exists(path: &str, follow: bool) -> Result<Option<Stat>, UvError> {
    match stat(path, follow) {
        Ok(stat) => Ok(Some(stat)),
        Err(error) if error.error.code == "ENOENT" || follow && error.error.code == "ENOTDIR" => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Whether `a` and `b` are the same file, as Node's `areIdentical` compares them: by device and
/// inode numbers, which must not be 0.
fn are_identical(a: &Stat, b: &Stat) -> bool {
    b.ino != 0 && b.dev != 0 && a.ino == b.ino && a.dev == b.dev
}

/// The target of the symlink `path`, or the error of `readlink`.
fn read_link(path: &str) -> Result<String, UvError> {
    std::fs::read_link(path)
        .map(|target| target.to_string_lossy().into_owned())
        .map_err(|error| UvError::new(error, "readlink").path(path))
}

/// Create the symlink `path` to `target`.
#[cfg(unix)]
fn symlink(target: &str, path: &str, _directory: bool) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

/// Create the symlink `path` to `target`, which is a directory symlink if `directory`.
#[cfg(windows)]
fn symlink(target: &str, path: &str, directory: bool) -> std::io::Result<()> {
    match directory {
        true => std::os::windows::fs::symlink_dir(target, path),
        false => std::os::windows::fs::symlink_file(target, path),
    }
}

/// Create the symlink `path` to `target`.
#[cfg(not(any(unix, windows)))]
#[allow(deprecated)]
fn symlink(target: &str, path: &str, _directory: bool) -> std::io::Result<()> {
    std::fs::soft_link(target, path)
}

/// `fs.symlinkSync(target, path)`, with the type `directory` gives.
fn make_symlink(target: &str, path: &str, directory: bool) -> Result<(), UvError> {
    symlink(target, path, directory)
        .map_err(|error| UvError::new(error, "symlink").path(target).dest(path))
}

/// `fs.unlinkSync(path)`.
fn unlink(path: &str) -> Result<(), UvError> {
    std::fs::remove_file(path).map_err(|error| UvError::new(error, "unlink").path(path))
}

/// `fs.mkdirSync(path)`.
fn mkdir(path: &str) -> Result<(), UvError> {
    std::fs::create_dir(path).map_err(|error| UvError::new(error, "mkdir").path(path))
}

/// `fs.chmodSync(path, mode)`. WASI has no permission bits, so there the `ENOSYS` this fails
/// with is ignored, and copies keep the default permissions.
fn chmod(path: &str, mode: u32) -> Result<(), UvError> {
    match set_mode(path, mode) {
        Err(error) if cfg!(target_arch = "wasm32") && error.raw_os_error() == Some(52) => Ok(()),
        result => result.map_err(|error| UvError::new(error, "chmod").path(path)),
    }
}

/// Copy `src` to `dest` with `fs.copyFileSync`, then set the timestamps of `dest` to those of
/// `src` if `preserve_timestamps`, and the permissions of `dest` to `mode`, as Node's
/// `copyFile` and `_copyFile` do.
fn copy_file_and_mode(src: &str, dest: &str, mode: u32, options: &Options) -> Result<(), UvError> {
    copy_file_blocking(src, dest, options.mode, "copyfile")?;
    if options.preserve_timestamps {
        // As in Node, a read-only file is made writable before its timestamps are set.
        if mode & 0o200 == 0 {
            chmod(dest, mode | 0o200)?;
        }
        let stat = stat(src, true)?;
        // The times pass through the `Date`s of `Stats`, which round to whole milliseconds as
        // `Math.round` does.
        let time = |ms: f64| time_of_seconds((ms + 0.5).floor() / 1000.0);
        platform::fs::set_times(dest, time(stat.atime_ms), time(stat.mtime_ms))
            .map_err(|error| UvError::new(error, "utime").path(dest))?;
    }
    chmod(dest, mode)
}

/// The target of the symlink `src`, resolved against the directory of `src` unless `verbatim`
/// or absolute.
fn resolve_link(src: &str, verbatim: bool) -> Result<String, UvError> {
    let target = read_link(src)?;
    Ok(match !verbatim && !target.starts_with('/') {
        true => resolve(&[dirname_of(src), &target]),
        false => target,
    })
}

/// The target the existing symlink `dest` resolves to, or `None` if `dest` isn't a symlink.
fn resolve_existing_link(dest: &str) -> Result<Option<String>, UvError> {
    let target = match read_link(dest) {
        Ok(target) => target,
        Err(error) if matches!(error.error.code, "EINVAL" | "UNKNOWN") => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(Some(match target.starts_with('/') {
        true => target,
        false => resolve(&[dirname_of(dest), &target]),
    }))
}

// ============================================================================
// fs.cp and fs.promises.cp
// ============================================================================

/// What a copy needs next.
enum Next {
    /// `filter`'s decision on copying the source path to the destination path.
    Filter(String, String),
    Done,
}

/// An entry to copy.
struct Item {
    src: String,
    dest: String,
    /// Whether this is the source path of the copy, rather than an entry of a directory in it.
    root: bool,
}

/// A directory being copied.
struct Frame {
    src: String,
    dest: String,
    /// The names of the entries not copied yet.
    names: std::vec::IntoIter<String>,
    /// The permissions `dest` gets once its entries are copied, if it was created.
    mode: Option<u32>,
}

/// A copy in progress, as Node's `cpFn` makes it.
pub(super) struct AsyncCopy {
    options: Options,
    /// The entry to copy next, which waits for `filter`'s decision if there is a filter.
    pending: Option<Item>,
    frames: Vec<Frame>,
}

impl AsyncCopy {
    fn new(src: String, dest: String, options: Options) -> Self {
        AsyncCopy {
            options,
            pending: Some(Item {
                src,
                dest,
                root: true,
            }),
            frames: Vec::new(),
        }
    }

    /// Copy until `filter`'s decision on an entry is needed, or until everything is copied.
    /// `decision` is the decision on the pending entry if there is a filter.
    fn advance(&mut self, mut decision: Option<bool>) -> Result<Next, CpError> {
        loop {
            if let Some(item) = self.pending.take() {
                if decision.take().unwrap_or(true) {
                    self.copy_item(item)?;
                }
            }
            let Some(frame) = self.frames.last_mut() else {
                return Ok(Next::Done);
            };
            match frame.names.next() {
                Some(name) => {
                    let src = join(&frame.src, &name);
                    let dest = join(&frame.dest, &name);
                    let next = Next::Filter(src.clone(), dest.clone());
                    self.pending = Some(Item {
                        src,
                        dest,
                        root: false,
                    });
                    if self.options.filter {
                        return Ok(next);
                    }
                }
                None => {
                    let frame = self.frames.pop().expect("a directory is being copied");
                    if let Some(mode) = frame.mode {
                        chmod(&frame.dest, mode)?;
                    }
                }
            }
        }
    }

    /// Copy `item`, which `filter` accepted: Node's `checkPaths` after the filter, then for the
    /// source path `checkParentPaths` and `checkParentDir`, then `getStatsForCopy`.
    fn copy_item(&mut self, item: Item) -> Result<(), CpError> {
        let Item { src, dest, root } = item;
        let follow = self.options.dereference;
        let src_stat = stat(&src, follow)?;
        let dest_stat = match stat(&dest, follow) {
            Ok(stat) => Some(stat),
            Err(error) if error.error.code == "ENOENT" => None,
            Err(error) => return Err(error.into()),
        };
        let src_is_dir = src_stat.kind == FileKind::Directory;
        if let Some(dest_stat) = &dest_stat {
            let dest_is_dir = dest_stat.kind == FileKind::Directory;
            if are_identical(&src_stat, dest_stat) {
                return Err(CpError::system(
                    "ERR_FS_CP_EINVAL",
                    "EINVAL",
                    "src and dest cannot be the same".into(),
                    &dest,
                ));
            }
            if src_is_dir && !dest_is_dir {
                return Err(CpError::system(
                    "ERR_FS_CP_DIR_TO_NON_DIR",
                    "EISDIR",
                    format!("cannot overwrite non-directory {dest} with directory {src}"),
                    &dest,
                ));
            }
            if !src_is_dir && dest_is_dir {
                return Err(CpError::system(
                    "ERR_FS_CP_NON_DIR_TO_DIR",
                    "ENOTDIR",
                    format!("cannot overwrite directory {dest} with non-directory {src}"),
                    &dest,
                ));
            }
        }
        if src_is_dir && is_src_subdir(&src, &dest) {
            return Err(subdirectory_of_self(&src, &dest));
        }
        if root {
            check_parent_paths(&src, &src_stat, &dest)?;
            check_parent_dir(&dest)?;
        }
        self.copy_with_stats(dest_stat, &src, &dest)
    }

    /// Node's `getStatsForCopy`: copy `src` by its type to `dest`, whose metadata is `dest_stat`.
    fn copy_with_stats(
        &mut self,
        dest_stat: Option<Stat>,
        src: &str,
        dest: &str,
    ) -> Result<(), CpError> {
        let options = self.options;
        let src_stat = stat(src, options.dereference)?;
        let system = |key, message: &str| {
            Err(CpError::system(
                key,
                "EINVAL",
                format!("{message}: {dest}"),
                dest,
            ))
        };
        match src_stat.kind {
            FileKind::Directory if options.recursive => {
                self.copy_directory(&src_stat, dest_stat, src, dest)
            }
            FileKind::Directory => Err(CpError::system(
                "ERR_FS_EISDIR",
                "EISDIR",
                format!("{src} is a directory (not copied)"),
                src,
            )),
            FileKind::File | FileKind::CharDevice | FileKind::BlockDevice => {
                match dest_stat {
                    None => {}
                    Some(_) if options.force => unlink(dest)?,
                    Some(_) if options.error_on_exist => return Err(already_exists(dest)),
                    Some(_) => return Ok(()),
                }
                Ok(copy_file_and_mode(src, dest, src_stat.mode, &options)?)
            }
            FileKind::Symlink => copy_link(dest_stat.is_some(), src, dest, &options),
            FileKind::Socket => system("ERR_FS_CP_SOCKET", "cannot copy a socket file"),
            FileKind::Fifo => system("ERR_FS_CP_FIFO_PIPE", "cannot copy a FIFO pipe"),
            FileKind::Unknown => system("ERR_FS_CP_UNKNOWN", "cannot copy an unknown file type"),
        }
    }

    /// Node's `onDir`: start copying the directory `src` to `dest`.
    fn copy_directory(
        &mut self,
        src_stat: &Stat,
        dest_stat: Option<Stat>,
        src: &str,
        dest: &str,
    ) -> Result<(), CpError> {
        let mode = match dest_stat {
            None => {
                mkdir(dest)?;
                Some(src_stat.mode)
            }
            Some(_) if self.options.error_on_exist && !self.options.force => {
                return Err(already_exists(dest))
            }
            Some(_) => None,
        };
        let fail = |error| UvError::new(error, "opendir").path(src);
        let mut names = Vec::new();
        for entry in std::fs::read_dir(src).map_err(fail)? {
            names.push(
                entry
                    .map_err(fail)?
                    .file_name()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        self.frames.push(Frame {
            src: src.to_string(),
            dest: dest.to_string(),
            names: names.into_iter(),
            mode,
        });
        Ok(())
    }
}

/// The `ERR_FS_CP_EINVAL` of copying the directory `src` into itself at `dest`.
fn subdirectory_of_self(src: &str, dest: &str) -> CpError {
    CpError::system(
        "ERR_FS_CP_EINVAL",
        "EINVAL",
        format!("cannot copy {src} to a subdirectory of self {dest}"),
        dest,
    )
}

/// The `ERR_FS_CP_EEXIST` of the existing `dest`.
fn already_exists(dest: &str) -> CpError {
    CpError::system(
        "ERR_FS_CP_EEXIST",
        "EEXIST",
        format!("{dest} already exists"),
        dest,
    )
}

/// Node's `checkParentPaths`: fail if an ancestor of `dest`, up to the parent of `src` or the
/// root, is `src`, whose metadata is `src_stat`.
fn check_parent_paths(src: &str, src_stat: &Stat, dest: &str) -> Result<(), CpError> {
    let src_parent = resolve(&[dirname_of(src)]);
    let mut dest = dest.to_string();
    loop {
        let dest_parent = resolve(&[dirname_of(&dest)]);
        if dest_parent == src_parent || dest_parent == "/" {
            return Ok(());
        }
        let dest_stat = match stat(&dest_parent, true) {
            Ok(stat) => stat,
            Err(error) if error.error.code == "ENOENT" => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if are_identical(src_stat, &dest_stat) {
            return Err(subdirectory_of_self(src, &dest));
        }
        dest = dest_parent;
    }
}

/// Node's `checkParentDir`: create the parent of `dest` and its missing ancestors.
fn check_parent_dir(dest: &str) -> Result<(), CpError> {
    let parent = dirname_of(dest);
    match stat(parent, true) {
        Ok(_) => Ok(()),
        Err(error) if error.error.code == "ENOENT" => std::fs::create_dir_all(parent)
            .map_err(|error| UvError::new(error, "mkdir").path(parent).into()),
        Err(error) => Err(error.into()),
    }
}

/// Node's `onLink` for `cp`: copy the symlink `src` to `dest`, which exists if `dest_exists`.
fn copy_link(dest_exists: bool, src: &str, dest: &str, options: &Options) -> Result<(), CpError> {
    let resolved_src = resolve_link(src, options.verbatim_symlinks)?;
    let src_is_dir = std::fs::metadata(src).is_ok_and(|metadata| metadata.is_dir());
    if !dest_exists {
        return Ok(make_symlink(&resolved_src, dest, src_is_dir)?);
    }
    let Some(resolved_dest) = resolve_existing_link(dest)? else {
        return Ok(make_symlink(&resolved_src, dest, src_is_dir)?);
    };
    if src_is_dir && is_src_subdir(&resolved_src, &resolved_dest) {
        return Err(CpError::system(
            "ERR_FS_CP_EINVAL",
            "EINVAL",
            format!("cannot copy {resolved_src} to a subdirectory of self {resolved_dest}"),
            dest,
        ));
    }
    if stat(src, true)?.kind == FileKind::Directory && is_src_subdir(&resolved_dest, &resolved_src)
    {
        return Err(symlink_to_subdirectory(&resolved_dest, &resolved_src, dest));
    }
    unlink(dest)?;
    Ok(make_symlink(&resolved_src, dest, src_is_dir)?)
}

/// The `ERR_FS_CP_SYMLINK_TO_SUBDIRECTORY` of replacing the symlink `dest` to `resolved_dest`
/// with one to `resolved_src`.
fn symlink_to_subdirectory(resolved_dest: &str, resolved_src: &str, dest: &str) -> CpError {
    CpError::system(
        "ERR_FS_CP_SYMLINK_TO_SUBDIRECTORY",
        "EINVAL",
        format!("cannot overwrite {resolved_dest} with {resolved_src}"),
        dest,
    )
}

#[core_runtime::jsclass(hidden)]
pub struct CopyWalk {
    /// The copy, or `None` while [`AsyncCopy::advance`] runs.
    #[no_trace]
    walker: Option<Box<AsyncCopy>>,
}

#[core_runtime::jsmethods]
impl CopyWalk {
    #[constructor]
    fn new() -> Result<Self, NodeError> {
        Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_ILLEGAL_CONSTRUCTOR",
            "Illegal constructor",
        ))
    }
}

/// The indices of the array of values a copy in progress uses on the JS thread, which is the
/// payload of its reactions: the [`CopyWalk`], the `filter` function, the object `filter` is called
/// on, the promise for the copy's completion, and whether that promise is fulfilled with the
/// arguments of a callback.
const WALK: u32 = 0;
const FILTER: u32 = 1;
const FILTER_THIS: u32 = 2;
const PROMISE: u32 = 3;
const CALLBACK: u32 = 4;

/// The [`CopyWalk`] of the copy `context` describes.
fn copy_walk<'s>(scope: &'s Scope<'_>, context: Object<'_>) -> Result<CopyWalk<'s>, ExnThrown> {
    let walk = context.get_element(scope, WALK)?;
    Object::from_value(scope, *walk)
        .map_err(|e| e.throw(scope))?
        .cast::<CopyWalk>()
        .map_err(|_| js::error::TypeError("not a copy".into()).throw(scope))
}

/// The promise for the completion of the copy `context` describes.
fn copy_promise<'s>(
    scope: &'s Scope<'_>,
    context: Object<'_>,
) -> Result<js::Promise<'s>, ExnThrown> {
    let promise = context.get_element(scope, PROMISE)?;
    Object::from_value(scope, *promise)
        .map_err(|e| e.throw(scope))?
        .cast::<js::Promise>()
        .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))
}

/// Start the copy of `src` to `dest` and return the promise for its completion. For
/// [`Variant::Callback`], the promise is fulfilled with an array of the callback's arguments
/// after the error.
fn start<'s>(
    scope: &'s Scope<'_>,
    src: String,
    dest: String,
    options: Options,
    filter_this: Object<'_>,
    filter: HandleValue<'_>,
    variant: Variant,
) -> Result<js::Promise<'s>, ExnThrown> {
    let promise = js::Promise::new_pending(scope)?;
    let walker = Box::new(AsyncCopy::new(src.clone(), dest.clone(), options));
    let walk = js::class::create_instance_with::<CopyWalkImpl>(scope, |_| CopyWalkImpl {
        walker: Some(walker),
    })?;
    let context = js::Array::with_contents(
        scope,
        &[
            walk.to_jsval_throwing(scope)?,
            scope.root_value(filter.get()),
            filter_this.to_jsval_throwing(scope)?,
            promise.to_jsval_throwing(scope)?,
            scope.root_value(js::value::from_bool(variant == Variant::Callback)),
        ],
    )?;
    // Node calls `filter` on the source path before `cp` returns.
    match options.filter {
        true => call_filter(scope, *context, &src, &dest)?,
        false => step(scope, *context, None)?,
    }
    Ok(promise)
}

/// Run [`AsyncCopy::advance`] with `decision` off the JS thread, and continue on the JS thread
/// with [`after_step`] or [`fail_copy`].
fn step(scope: &Scope<'_>, context: Object<'_>, decision: Option<bool>) -> Result<(), ExnThrown> {
    let walker = copy_walk(scope, context)?
        .data_mut()
        .walker
        .take()
        .expect("one step runs at a time");
    let context_root = RootedHeap::<js::object::Object>::new(context);
    let future = async move {
        let (walker, result) = platform::fs::offload(move || {
            let mut walker = walker;
            let result = walker.advance(decision);
            (walker, result)
        })
        .await;
        PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            copy_walk(scope, context_root.get(scope))?.data_mut().walker = Some(walker);
            match result {
                Ok(Next::Done) => Ok(HandleValue::undefined()),
                Ok(Next::Filter(src, dest)) => {
                    let src = js::JSString::from_str(scope, &src)?.to_jsval_throwing(scope)?;
                    let dest = js::JSString::from_str(scope, &dest)?.to_jsval_throwing(scope)?;
                    js::Array::with_contents(scope, &[src, dest])?.to_jsval_throwing(scope)
                }
                Err(error) => Err(error.throw(scope)),
            }
        }))
    };
    let promise = js::Promise::new_pending(scope)?;
    promise.spawn_polled(PromiseFuture::new(future));
    let context = context.to_jsval_throwing(scope)?;
    let on_step = Function::new_callback(scope, c"", 1, after_step, *context)?;
    let on_error = Function::new_callback(scope, c"", 1, fail_copy, *context)?;
    promise.add_reactions(scope, Some(*on_step), Some(*on_error))
}

/// The fulfillment reaction of a step, whose payload is the copy's context: finish the copy if
/// the step was fulfilled with `undefined`, and otherwise call `filter` with the source and
/// destination paths in the array it was fulfilled with.
fn after_step(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let context = Object::from_value(scope, *payload).map_err(|e| e.throw(scope))?;
    let value = scope.root_value(args.get(0).get());
    if value.is_undefined() {
        let promise = copy_promise(scope, context)?;
        let value = match context.get_element(scope, CALLBACK)?.to_boolean() {
            true => js::Array::with_contents(scope, &[HandleValue::undefined()])?
                .to_jsval_throwing(scope)?,
            false => HandleValue::undefined(),
        };
        promise.resolve(scope, value)?;
        return Ok(js::value::undefined());
    }
    let paths = Object::from_value(scope, *value).map_err(|e| e.throw(scope))?;
    let path = |index| -> Result<String, ExnThrown> {
        js::JSString::from_value(scope, paths.get_element(scope, index)?)?.to_utf8(scope)
    };
    call_filter(scope, context, &path(0)?, &path(1)?)?;
    Ok(js::value::undefined())
}

/// The rejection reaction of a step or of `filter`'s result, whose payload is the copy's context:
/// reject the copy's promise with the error.
fn fail_copy(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let context = Object::from_value(scope, *payload).map_err(|e| e.throw(scope))?;
    copy_promise(scope, context)?.reject(scope, args.get(0))?;
    Ok(js::value::undefined())
}

/// Call `filter` with `src` and `dest`, and continue the copy with its result once that settles.
/// An exception `filter` throws rejects the copy's promise.
fn call_filter(
    scope: &Scope<'_>,
    context: Object<'_>,
    src: &str,
    dest: &str,
) -> Result<(), ExnThrown> {
    let filter = context.get_element(scope, FILTER)?;
    let this = context.get_element(scope, FILTER_THIS)?;
    let src = js::JSString::from_str(scope, src)?.to_jsval_throwing(scope)?;
    let dest = js::JSString::from_str(scope, dest)?.to_jsval_throwing(scope)?;
    let context_value = context.to_jsval_throwing(scope)?;
    let result = match Function::call(scope, this, filter, &[src, dest]) {
        Ok(result) => js::Promise::new_resolved_with_value(scope, result)?,
        Err(ExnThrown) => js::Promise::new_rejected_with_pending_error(scope)?,
    };
    let on_value = Function::new_callback(scope, c"", 1, after_filter, *context_value)?;
    let on_error = Function::new_callback(scope, c"", 1, fail_copy, *context_value)?;
    result.add_reactions(scope, Some(*on_value), Some(*on_error))
}

/// The fulfillment reaction of `filter`'s result, whose payload is the copy's context: continue
/// the copy with the result's truthiness as the decision.
fn after_filter(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let context = Object::from_value(scope, *payload).map_err(|e| e.throw(scope))?;
    let decision = crate::stream::truthy(scope, args.get(0));
    step(scope, context, Some(decision))?;
    Ok(js::value::undefined())
}

/// Argument `index`, or `undefined`.
fn arg<'a>(args: &[HandleValue<'a>], index: usize) -> HandleValue<'a> {
    args.get(index).copied().unwrap_or(HandleValue::undefined())
}

/// The body of `fs.cp(src, dest[, options], callback)`.
pub(super) fn cp_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let values = crate::callback_args(&args);
    let run = || -> Result<(), Thrown> {
        let (options, callback) = match crate::stream::is_function(scope, arg(&values, 2)) {
            true => (None, arg(&values, 2)),
            false => (Some(arg(&values, 2)), arg(&values, 3)),
        };
        validate_function(scope, callback, "cb")?;
        let (options, filter_this, filter) = parse_options(scope, options)?;
        let src = path_arg(scope, arg(&values, 0), "src")?;
        let dest = path_arg(scope, arg(&values, 1), "dest")?;
        if cfg!(target_arch = "wasm32") {
            refuse_blocking("cp")?;
        }
        let promise = start(
            scope,
            src,
            dest,
            options,
            filter_this,
            filter,
            Variant::Callback,
        )?;
        let on_values = Function::new_callback(
            scope,
            c"",
            1,
            super::call_back_with_values,
            *js::Array::with_contents(
                scope,
                &[callback, scope.root_value(js::value::from_bool(true))],
            )?,
        )?;
        let on_error = Function::new_callback(
            scope,
            c"",
            1,
            super::call_back_with_error,
            *js::Array::with_contents(scope, &[callback])?,
        )?;
        promise.then(scope, Some(*on_values), Some(*on_error))?;
        Ok(())
    };
    run().map_err(|thrown| thrown.throw(scope))?;
    Ok(js::value::undefined())
}

/// The body of `fsPromises.cp(src, dest[, options])`. Argument errors reject the promise.
pub(super) fn cp_promise(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let values = crate::callback_args(&args);
    let run = || -> Result<js::Promise<'_>, Thrown> {
        let (options, filter_this, filter) = parse_options(scope, Some(arg(&values, 2)))?;
        let src = path_arg(scope, arg(&values, 0), "src")?;
        let dest = path_arg(scope, arg(&values, 1), "dest")?;
        if cfg!(target_arch = "wasm32") {
            refuse_blocking("cp")?;
        }
        Ok(start(
            scope,
            src,
            dest,
            options,
            filter_this,
            filter,
            Variant::Promise,
        )?)
    };
    let promise = match run() {
        Ok(promise) => promise,
        Err(thrown) => {
            thrown.throw(scope);
            js::Promise::new_rejected_with_pending_error(scope)?
        }
    };
    promise.to_jsval_raw_throwing(scope)
}

// ============================================================================
// fs.cpSync
// ============================================================================

/// An error of `cpSync`: an error of the copy, or of calling `filter`.
enum SyncError {
    Copy(CpError),
    Thrown(Thrown),
}

impl From<CpError> for SyncError {
    fn from(error: CpError) -> Self {
        SyncError::Copy(error)
    }
}

impl From<UvError> for SyncError {
    fn from(error: UvError) -> Self {
        SyncError::Copy(error.into())
    }
}

impl From<Thrown> for SyncError {
    fn from(thrown: Thrown) -> Self {
        SyncError::Thrown(thrown)
    }
}

/// `filter`, called with a source path and a destination path.
type Filter<'f> = dyn FnMut(&str, &str) -> Result<bool, Thrown> + 'f;

/// The body of `fs.cpSync(src, dest[, options])`.
pub(super) fn cp_sync(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let values = crate::callback_args(&args);
    let run = || -> Result<(), Thrown> {
        let (options, filter_this, filter) = parse_options(scope, Some(arg(&values, 2)))?;
        let src = path_arg(scope, arg(&values, 0), "src")?;
        let dest = path_arg(scope, arg(&values, 1), "dest")?;
        refuse_blocking("cpSync")?;
        let mut call_filter = |src: &str, dest: &str| -> Result<bool, Thrown> {
            let src = js::JSString::from_str(scope, src)?.to_jsval_throwing(scope)?;
            let dest = js::JSString::from_str(scope, dest)?.to_jsval_throwing(scope)?;
            let result = Function::call(scope, filter_this, filter, &[src, dest])?;
            let is_promise = Object::from_value(scope, *result)
                .is_ok_and(|object| js::Promise::is_promise(object.handle()));
            if is_promise {
                return Err(NodeError::new(
                    ErrorKind::TypeError,
                    "ERR_INVALID_RETURN_VALUE",
                    format!(
                        "Expected boolean to be returned from the \"filter\" function but got {}.",
                        describe_value(scope, result)
                    ),
                )
                .into());
            }
            Ok(crate::stream::truthy(scope, result))
        };
        match copy_sync(&src, &dest, &options, &mut call_filter) {
            Ok(()) => Ok(()),
            Err(SyncError::Thrown(thrown)) => Err(thrown),
            Err(SyncError::Copy(error)) => Err(error.throw(scope).into()),
        }
    };
    run().map_err(|thrown| thrown.throw(scope))?;
    Ok(js::value::undefined())
}

/// Node's `cpSyncFn`.
fn copy_sync(
    src: &str,
    dest: &str,
    options: &Options,
    filter: &mut Filter<'_>,
) -> Result<(), SyncError> {
    if options.filter && !filter(src, dest)? {
        return Ok(());
    }
    check_paths_sync(src, dest, options.dereference, options.recursive)?;
    let Some(directory) = copy_by_type_sync(src, dest, options)? else {
        return Ok(());
    };
    copy_directories_sync(directory, options, filter)
}

/// Whether `a` and `b` are the same file, as `std::filesystem::equivalent` compares them: by
/// device and inode numbers, or by canonical path where there are none. Either not existing is an
/// error.
fn equivalent(a: &str, b: &str) -> std::io::Result<bool> {
    let a_metadata = std::fs::metadata(a)?;
    let b_metadata = std::fs::metadata(b)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok((a_metadata.dev(), a_metadata.ino()) == (b_metadata.dev(), b_metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = (a_metadata, b_metadata);
        Ok(std::fs::canonicalize(a)? == std::fs::canonicalize(b)?)
    }
}

/// `std::filesystem::path::parent_path` of the POSIX path `path`: `path` without its last
/// element and the separators before it. A path that is only separators is its own parent.
fn parent_path(path: &str) -> &str {
    if path.is_empty() || path.bytes().all(|byte| byte == b'/') {
        return path;
    }
    if path.ends_with('/') {
        return path.trim_end_matches('/');
    }
    match path.rfind('/') {
        None => "",
        Some(end) => match path[..end].trim_end_matches('/') {
            "" => &path[..1],
            parent => parent,
        },
    }
}

/// Whether the POSIX paths `a` and `b` have the same elements, as `std::filesystem::path`
/// compares them.
fn same_elements(a: &str, b: &str) -> bool {
    fn elements(path: &str) -> (bool, Vec<&str>) {
        let relative = path.trim_start_matches('/');
        let mut elements: Vec<&str> = relative.split('/').filter(|e| !e.is_empty()).collect();
        if relative.ends_with('/') {
            elements.push("");
        }
        (path.starts_with('/'), elements)
    }
    elements(a) == elements(b)
}

/// Node's C++ `CpSyncCheckPaths`. As there, `dereference` makes `src` be inspected without
/// following symlinks, and a symlink then counts as a directory.
fn check_paths_sync(
    src: &str,
    dest: &str,
    dereference: bool,
    recursive: bool,
) -> Result<(), CpError> {
    let syscall = if dereference { "stat" } else { "lstat" };
    let src_metadata =
        metadata(src, !dereference).map_err(|error| UvError::new(error, syscall).path(src))?;
    let src_kind = FileKind::of(src_metadata.file_type());
    let dest_metadata = metadata(dest, dereference);
    let src_is_dir =
        src_kind == FileKind::Directory || dereference && src_kind == FileKind::Symlink;
    if let Ok(dest_metadata) = &dest_metadata {
        if equivalent(src, dest).unwrap_or(false) {
            return Err(CpError::plain(
                "ERR_FS_CP_EINVAL",
                format!("src and dest cannot be the same {dest}"),
            ));
        }
        let dest_is_dir = dest_metadata.is_dir();
        if src_is_dir && !dest_is_dir {
            return Err(CpError::plain(
                "ERR_FS_CP_DIR_TO_NON_DIR",
                format!("Cannot overwrite non-directory {dest} with directory {src}"),
            ));
        }
        if !src_is_dir && dest_is_dir {
            return Err(CpError::plain(
                "ERR_FS_CP_NON_DIR_TO_DIR",
                format!("Cannot overwrite directory {dest} with non-directory {src}"),
            ));
        }
    }
    let src_with_separator = match src.ends_with('/') {
        true => src.to_string(),
        false => format!("{src}/"),
    };
    let inside_self = || {
        CpError::plain(
            "ERR_FS_CP_EINVAL",
            format!("Cannot copy {src_with_separator} to a subdirectory of self {dest}"),
        )
    };
    if src_is_dir && dest.starts_with(&src_with_separator) {
        return Err(inside_self());
    }
    // As in Node, each step compares `src` with the parent of `dest`, not with `dest_parent`.
    let src_parent = parent_path(src);
    let mut dest_parent = parent_path(dest);
    while !same_elements(src_parent, dest_parent)
        && !parent_path(dest_parent).is_empty()
        && !same_elements(parent_path(dest_parent), dest_parent)
    {
        match equivalent(src, parent_path(dest)) {
            Ok(true) => return Err(inside_self()),
            Ok(false) => {}
            Err(_) => break,
        }
        dest_parent = parent_path(dest_parent);
    }
    if src_is_dir && !recursive {
        return Err(CpError::plain(
            "ERR_FS_EISDIR",
            format!("Recursive option not enabled, cannot copy a directory: {src_with_separator}"),
        ));
    }
    let unsupported = match src_kind {
        FileKind::Socket => Some(("ERR_FS_CP_SOCKET", "Cannot copy a socket file")),
        FileKind::Fifo => Some(("ERR_FS_CP_FIFO_PIPE", "Cannot copy a FIFO pipe")),
        FileKind::Unknown => Some(("ERR_FS_CP_UNKNOWN", "Cannot copy an unknown file type")),
        _ => None,
    };
    if let Some((code, message)) = unsupported {
        return Err(CpError::plain(code, format!("{message}: {dest}")));
    }
    let dest_parent = parent_path(dest);
    if dest_metadata.is_err() || std::fs::metadata(dest_parent).is_err() {
        // Errors are ignored, as in Node.
        let _ = std::fs::create_dir_all(dest_parent);
    }
    Ok(())
}

/// Node's `getStats` for `cpSync`: copy `src` to `dest` by the type of `src`. A directory copied
/// with a filter is returned as a [`DirectoryCopy`], whose entries are copied by
/// [`copy_directories_sync`].
fn copy_by_type_sync(
    src: &str,
    dest: &str,
    options: &Options,
) -> Result<Option<DirectoryCopy>, SyncError> {
    let src_stat = stat(src, options.dereference)?;
    let dest_stat = stat_if_exists(dest, options.dereference)?;
    match src_stat.kind {
        FileKind::Directory if options.recursive => {
            let mode = dest_stat.is_none().then_some(src_stat.mode);
            open_directory_sync(src, dest, options, mode)
        }
        FileKind::File | FileKind::CharDevice | FileKind::BlockDevice => {
            match dest_stat {
                None => copy_file_and_mode(src, dest, src_stat.mode, options)?,
                Some(_) if options.force => override_file(src, dest, options)?,
                Some(_) if options.error_on_exist => return Err(already_exists(dest).into()),
                Some(_) => {}
            }
            Ok(None)
        }
        FileKind::Symlink => {
            copy_link_sync(dest_stat.is_some(), src, dest, options)?;
            Ok(None)
        }
        _ => Err(CpError::plain(
            "ERR_INTERNAL_ASSERTION",
            "Unreachable code\nThis is caused by either a bug in Node.js or incorrect usage of \
             Node.js internals.\nPlease open an issue with this stack trace at \
             https://github.com/nodejs/node/issues\n"
                .into(),
        )
        .into()),
    }
}

/// Node's C++ `CpSyncOverrideFile`: replace `dest` with a copy of `src`.
fn override_file(src: &str, dest: &str, options: &Options) -> Result<(), CpError> {
    remove(dest).map_err(|error| UvError::new(error, "unlink").path(dest).errno_exception())?;
    match options.mode {
        0 => copy_regular_file(src, dest, Overwrite::Fail)
            .map(|_| ())
            .map_err(|error| UvError::new(error, "cp").path(dest).errno_exception())?,
        mode => copy_file_blocking(src, dest, mode, "cp")?,
    }
    if options.preserve_timestamps {
        copy_times(src, dest)?;
    }
    Ok(())
}

/// Remove the file or empty directory `path`, as remove(3) does.
fn remove(path: &str) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path)?.is_dir() {
        true => std::fs::remove_dir(path),
        false => std::fs::remove_file(path),
    }
}

/// What copying a regular file onto an existing one does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Overwrite {
    Replace,
    Fail,
    Skip,
}

/// Copy the regular file `src` to `dest`, as `std::filesystem::copy_file` does: `overwrite`
/// selects whether an existing `dest` is replaced, fails with `EEXIST` or is kept. The result is
/// `false` when `dest` is kept. Either path not being a regular file fails with `ENOTSUP`, and
/// copying a file to itself fails with `EEXIST`.
fn copy_regular_file(src: &str, dest: &str, overwrite: Overwrite) -> Result<bool, FsError> {
    let src_metadata = std::fs::File::open(src)?.metadata()?;
    if !src_metadata.is_file() {
        return Err(FsError::from_code("ENOTSUP"));
    }
    match std::fs::metadata(dest) {
        Ok(dest_metadata) => {
            if !dest_metadata.is_file() {
                return Err(FsError::from_code("ENOTSUP"));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if (src_metadata.dev(), src_metadata.ino())
                    == (dest_metadata.dev(), dest_metadata.ino())
                {
                    return Err(FsError::from_code("EEXIST"));
                }
            }
            match overwrite {
                Overwrite::Skip => return Ok(false),
                Overwrite::Fail => return Err(FsError::from_code("EEXIST")),
                Overwrite::Replace => {}
            }
        }
        Err(error)
            if matches!(
                error.kind(),
                IoErrorKind::NotFound | IoErrorKind::NotADirectory
            ) => {}
        Err(error) => return Err(error.into()),
    }
    std::fs::copy(src, dest)?;
    Ok(true)
}

/// Node's C++ `CopyUtimes`: set the timestamps of `dest` to those of `src`.
fn copy_times(src: &str, dest: &str) -> Result<(), UvError> {
    let metadata = std::fs::metadata(src).map_err(|error| UvError::new(error, "stat").path(src))?;
    #[cfg(unix)]
    let (accessed, modified) = {
        use std::os::unix::fs::MetadataExt;
        (
            metadata.atime() as f64 + metadata.atime_nsec() as f64 / 1e9,
            metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1e9,
        )
    };
    #[cfg(not(unix))]
    let (accessed, modified) = {
        let stat = Stat::from(&metadata);
        (stat.atime_ms / 1000.0, stat.mtime_ms / 1000.0)
    };
    platform::fs::set_times(dest, time_of_seconds(accessed), time_of_seconds(modified))
        .map_err(|error| UvError::new(error, "utime").path(dest))
}

/// A directory that `cpSync` copies with a filter: the entries of `src` not copied yet, and the
/// permissions `dest` gets once they are, if `dest` was created for the copy.
struct DirectoryCopy {
    entries: std::fs::ReadDir,
    src: String,
    dest: String,
    mode: Option<u32>,
}

/// The start of Node's `copyDir` for `cpSync`: without a filter, copy the directory `src` to
/// `dest`. With one, create `dest` if `mode` is the permissions it gets afterwards, and return the
/// copy of the entries of `src` into it.
fn open_directory_sync(
    src: &str,
    dest: &str,
    options: &Options,
    mode: Option<u32>,
) -> Result<Option<DirectoryCopy>, SyncError> {
    if !options.filter {
        copy_directory_native(src, dest, options)?;
        return Ok(None);
    }
    if mode.is_some() {
        mkdir(dest)?;
    }
    let entries =
        std::fs::read_dir(src).map_err(|error| UvError::new(error, "opendir").path(src))?;
    Ok(Some(DirectoryCopy {
        entries,
        src: src.to_string(),
        dest: dest.to_string(),
        mode,
    }))
}

/// The rest of Node's `copyDir` for `cpSync`, without recursion: copy the entries `filter`
/// accepts of `directory` and of the directories in it, depth first. The first error stops the
/// copy. The permissions of each directory created are set once its entries are copied, even after
/// an error, and an error setting them replaces the copy's.
fn copy_directories_sync(
    directory: DirectoryCopy,
    options: &Options,
    filter: &mut Filter<'_>,
) -> Result<(), SyncError> {
    let mut stack = vec![directory];
    let mut result = Ok(());
    while let Some(directory) = stack.last_mut() {
        let entry = match result {
            Ok(()) => directory.entries.next(),
            Err(_) => None,
        };
        let Some(entry) = entry else {
            let directory = stack.pop().expect("the stack isn't empty");
            if let Some(mode) = directory.mode {
                if let Err(error) = chmod(&directory.dest, mode) {
                    result = Err(error.into());
                }
            }
            continue;
        };
        let name = match entry {
            Ok(entry) => entry.file_name().to_string_lossy().into_owned(),
            Err(error) => {
                result = Err(UvError::new(error, "opendir").path(&directory.src).into());
                continue;
            }
        };
        let src_item = join(&directory.src, &name);
        let dest_item = join(&directory.dest, &name);
        let copied = match filter(&src_item, &dest_item) {
            Ok(true) => copy_by_type_sync(&src_item, &dest_item, options),
            Ok(false) => Ok(None),
            Err(thrown) => Err(thrown.into()),
        };
        match copied {
            Ok(Some(directory)) => stack.push(directory),
            Ok(None) => {}
            Err(error) => result = Err(error),
        }
    }
    result
}

/// `path / name` with `std::filesystem::path`'s `/`: `name` if it is absolute, and otherwise
/// `name` after `path` and a separator unless `path` is empty or ends in one.
fn append(path: &str, name: &str) -> String {
    if name.starts_with('/') || path.is_empty() {
        return name.to_string();
    }
    match path.ends_with('/') {
        true => format!("{path}{name}"),
        false => format!("{path}/{name}"),
    }
}

/// `std::filesystem::absolute(path)`: `path` after the current directory, unless absolute.
fn absolute(path: &str) -> String {
    append(&crate::process::cwd(), path)
}

/// Whether the elements of the absolute path of `src` begin those of `dest`, as Node's C++
/// `isInsideDir` compares them.
fn is_inside_dir(src: &str, dest: &str) -> bool {
    let src = absolute(src);
    let dest = absolute(dest);
    let src: Vec<&str> = src.split('/').filter(|part| !part.is_empty()).collect();
    let dest: Vec<&str> = dest.split('/').filter(|part| !part.is_empty()).collect();
    src.len() <= dest.len() && src.iter().zip(&dest).all(|(a, b)| a == b)
}

/// `std::filesystem::weakly_canonical` of the absolute path `path`: the canonical path of its
/// longest existing prefix, followed by the rest of it, normalized.
fn weakly_canonical(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    for end in (0..=parts.len()).rev() {
        let prefix = format!("/{}", parts[..end].join("/"));
        if std::fs::metadata(&prefix).is_err() {
            continue;
        }
        let Ok(canonical) = std::fs::canonicalize(&prefix) else {
            continue;
        };
        let canonical = canonical.to_string_lossy().into_owned();
        if end == parts.len() {
            return canonical;
        }
        let rest = parts[end..].join("/");
        return normalize_path(&format!("{}/{rest}", canonical.trim_end_matches('/')));
    }
    path.to_string()
}

/// Node's C++ `CpSyncCopyDir`, which copies the directory `src` to `dest` without a filter.
fn copy_directory_native(src: &str, dest: &str, options: &Options) -> Result<(), CpError> {
    std::fs::create_dir_all(dest)
        .map_err(|error| UvError::new(error, "cp").path(dest).errno_exception())?;
    let overwrite = match (options.force, options.error_on_exist) {
        (true, _) => Overwrite::Replace,
        (false, true) => Overwrite::Fail,
        (false, false) => Overwrite::Skip,
    };
    copy_contents_native(src, dest, options, overwrite)
}

/// Copy the entries of the directory `src` into the directory `dest`, as `CpSyncCopyDir` does,
/// depth first in the order the directories list their entries. Entries that are neither
/// directories, regular files nor symlinks are skipped. Most errors name the destination
/// directory of the entry, not the entry.
fn copy_contents_native(
    src: &str,
    dest: &str,
    options: &Options,
    overwrite: Overwrite,
) -> Result<(), CpError> {
    let open = |src: &str, dest: &str| {
        std::fs::read_dir(src)
            .map(|entries| (entries, src.to_string(), dest.to_string()))
            .map_err(|error| UvError::new(error, "cp").path(dest).errno_exception())
    };
    // The directories being copied, innermost last, with their remaining entries.
    let mut stack = vec![open(src, dest)?];
    while let Some((entries, src, dest)) = stack.last_mut() {
        let Some(entry) = entries.next() else {
            stack.pop();
            continue;
        };
        let (src, dest) = (src.as_str(), dest.as_str());
        let fail = |error: FsError| UvError::new(error, "cp").path(dest).errno_exception();
        let io_fail = |error: std::io::Error| fail(error.into());
        let link_target = |path: &str| -> Result<String, CpError> {
            std::fs::read_link(path)
                .map(|target| target.to_string_lossy().into_owned())
                .map_err(|error| io_fail(error).into())
        };
        let entry = entry.map_err(io_fail)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let entry_path = append(src, &name);
        let dest_path = append(dest, &name);
        let file_type = entry.file_type().map_err(io_fail)?;
        if file_type.is_symlink() {
            let target = link_target(&entry_path)?;
            if options.verbatim_symlinks {
                symlink(&target, &dest_path, false).map_err(io_fail)?;
                continue;
            }
            if std::fs::metadata(&dest_path).is_ok() {
                let dest_is_link = std::fs::symlink_metadata(&dest_path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink());
                if dest_is_link {
                    let current = link_target(&dest_path)?;
                    let target_is_dir =
                        std::fs::metadata(&target).is_ok_and(|metadata| metadata.is_dir());
                    if !options.dereference && target_is_dir && is_inside_dir(&target, &current) {
                        return Err(CpError::plain(
                            "ERR_FS_CP_EINVAL",
                            format!("Cannot copy {target} to a subdirectory of self {current}"),
                        ));
                    }
                    let dest_is_dir =
                        std::fs::metadata(&dest_path).is_ok_and(|metadata| metadata.is_dir());
                    if dest_is_dir && is_inside_dir(&current, &target) {
                        return Err(CpError::plain(
                            "ERR_FS_CP_SYMLINK_TO_SUBDIRECTORY",
                            format!("cannot overwrite {current} with {target}"),
                        ));
                    }
                    remove(&dest_path).map_err(io_fail)?;
                } else if std::fs::metadata(&dest_path).is_ok_and(|metadata| metadata.is_file())
                    && (!options.dereference || !options.force && options.error_on_exist)
                {
                    return Err(UvError::code("EEXIST", "cp")
                        .path(&dest_path)
                        .errno_exception()
                        .into());
                }
            }
            let target = weakly_canonical(&absolute(&append(src, &target)));
            symlink(&target, &dest_path, false).map_err(io_fail)?;
        } else if file_type.is_dir() {
            match std::fs::create_dir(&dest_path) {
                Ok(()) => {}
                Err(_) if std::fs::metadata(&dest_path).is_ok_and(|m| m.is_dir()) => {}
                Err(error) => return Err(io_fail(error).into()),
            }
            let next = open(&entry_path, &dest_path)?;
            stack.push(next);
        } else if file_type.is_file() {
            match copy_regular_file(&entry_path, &dest_path, overwrite) {
                Ok(_) => {}
                Err(error) if error.code == "EEXIST" => {
                    return Err(CpError::plain(
                        "ERR_FS_CP_EEXIST",
                        format!(
                            "[ERR_FS_CP_EEXIST]: Target already exists: cp returned EEXIST \
                             ({dest_path} already exists)"
                        ),
                    ))
                }
                Err(error) => return Err(fail(error).into()),
            }
            if options.preserve_timestamps {
                copy_times(&entry_path, &dest_path)?;
            }
        }
    }
    Ok(())
}

/// Node's `onLink` for `cpSync`: copy the symlink `src` to `dest`, which exists if
/// `dest_exists`.
fn copy_link_sync(
    dest_exists: bool,
    src: &str,
    dest: &str,
    options: &Options,
) -> Result<(), CpError> {
    let resolved_src = resolve_link(src, options.verbatim_symlinks)?;
    let src_is_dir = std::fs::metadata(src).is_ok_and(|metadata| metadata.is_dir());
    if !dest_exists {
        return Ok(make_symlink(&resolved_src, dest, src_is_dir)?);
    }
    let Some(resolved_dest) = resolve_existing_link(dest)? else {
        return Ok(make_symlink(&resolved_src, dest, src_is_dir)?);
    };
    if src_is_dir && is_src_subdir(&resolved_src, &resolved_dest) {
        return Err(CpError::system(
            "ERR_FS_CP_EINVAL",
            "EINVAL",
            format!("cannot copy {resolved_src} to a subdirectory of self {resolved_dest}"),
            dest,
        ));
    }
    if stat(dest, true)?.kind == FileKind::Directory && is_src_subdir(&resolved_dest, &resolved_src)
    {
        return Err(symlink_to_subdirectory(&resolved_dest, &resolved_src, dest));
    }
    unlink(dest)?;
    Ok(make_symlink(&resolved_src, dest, src_is_dir)?)
}
