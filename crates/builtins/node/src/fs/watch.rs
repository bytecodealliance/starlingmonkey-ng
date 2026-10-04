// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `fs.watch`, `fs.watchFile`, `fs.unwatchFile` and `fsPromises.watch`, implemented by polling.
//!
//! Each watcher's poll state is in an `FSEvent` handle, which a JS timer polls. `fs.watch` and
//! `fsPromises.watch` compare snapshots of the watched file or directory every [`WATCH_INTERVAL`],
//! and pass each difference to the handle's `onchange` function. `fs.watchFile` stats the file
//! every `interval` milliseconds and reports changes as libuv's `uv_fs_poll` does. Snapshots and
//! stats read the filesystem synchronously on the JS thread. Where blocking host I/O isn't
//! allowed, polls are skipped.
//!
//! `fs.watch` creates the watcher Node.js uses on the platform `process.platform` names: with
//! `recursive` on Linux, the class of Node's `internal/fs/recursive_watch`, and otherwise the
//! `FSWatcher` of `internal/fs/watchers`.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};
use platform::fs::{FileKind, FsError, Stat};

use super::{option, path_arg, text_value, UvError};
use crate::buffer::Encoding;
use crate::errors::{
    abort_error, validate_abort_signal, validate_function, validate_integer, validate_plain_object,
    ErrorKind, NodeError, Thrown,
};
use crate::glob::{platform_nocase, Glob, GlobOptions};
use crate::stream::{
    call_method, closure, is_function, next_tick_call, string, truthy, Captures, Props,
};

/// The time between the polls of an `fs.watch` or `fsPromises.watch` watcher. A file created and
/// removed between two polls is only reported by the event without a filename.
const WATCH_INTERVAL: Duration = Duration::from_millis(100);

/// The default `interval` of `fs.watchFile`, in milliseconds.
const WATCH_FILE_INTERVAL: f64 = 5007.0;

js::instance_local! {
    /// The ids of the queued poll timers, each with whether it belongs to a `StatWatcher`.
    static POLL_TIMERS: RefCell<BTreeMap<u64, bool>> = const { RefCell::new(BTreeMap::new()) };
}

/// Whether the timer `id` is a watcher's poll timer.
pub(crate) fn is_poll_timer(id: u64) -> bool {
    POLL_TIMERS.with_borrow(|timers| timers.contains_key(&id))
}

/// The numbers of watchers whose poll timers are queued and keep the event loop running: the
/// `fs.watch` and `fsPromises.watch` watchers, and the `StatWatcher`s.
pub(crate) fn active_watchers() -> (usize, usize) {
    let referenced = core_runtime::event_loop::with_active_event_loop(|event_loop| {
        event_loop.referenced_js_timers()
    })
    .unwrap_or_default();
    POLL_TIMERS.with_borrow(|timers| {
        let stat = referenced
            .iter()
            .filter(|id| timers.get(id) == Some(&true))
            .count();
        let tree = referenced
            .iter()
            .filter(|id| timers.get(id) == Some(&false))
            .count();
        (tree, stat)
    })
}

// ============================================================================
// Snapshots
// ============================================================================

/// The metadata of an entry that [`diff`] compares.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Meta {
    kind: FileKind,
    ino: u64,
    size: u64,
    mode: u32,
    mtime_ms: f64,
    ctime_ms: f64,
}

impl From<&std::fs::Metadata> for Meta {
    fn from(metadata: &std::fs::Metadata) -> Self {
        let stat = Stat::from(metadata);
        Meta {
            kind: stat.kind,
            ino: stat.ino,
            size: stat.size,
            mode: stat.mode,
            mtime_ms: stat.mtime_ms,
            ctime_ms: stat.ctime_ms,
        }
    }
}

impl Meta {
    /// Whether `other` is a different entry than `self`: one of another kind, or with another
    /// inode number where the host reports them.
    fn replaced_by(&self, other: &Meta) -> bool {
        self.kind != other.kind || (self.ino != 0 && other.ino != 0 && self.ino != other.ino)
    }

    /// Whether the contents or attributes of a file changed between `self` and `other`.
    fn changed_to(&self, other: &Meta) -> bool {
        self.size != other.size
            || self.mode != other.mode
            || self.mtime_ms != other.mtime_ms
            || self.ctime_ms != other.ctime_ms
    }
}

/// The state of a watched file or directory.
#[derive(Debug)]
enum Snapshot {
    Missing,
    /// The metadata of the watched path, which symlinks are followed to, and of the entries
    /// below it by path relative to it, without following symlinks.
    Present {
        root: Meta,
        entries: BTreeMap<String, Meta>,
    },
}

/// The snapshot of `path`, with the entries of its subdirectories if `recursive`. Entries that
/// disappear while the snapshot is taken, and directories that can't be read, are left out.
fn snapshot(path: &str, recursive: bool) -> std::io::Result<Snapshot> {
    let root = std::fs::metadata(path)?;
    let mut entries = BTreeMap::new();
    if root.is_dir() {
        let mut pending = vec![(std::path::PathBuf::from(path), String::new())];
        while let Some((directory, prefix)) = pending.pop() {
            let Ok(listing) = std::fs::read_dir(&directory) else {
                continue;
            };
            for entry in listing.flatten() {
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                let key = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}{}{name}", std::path::MAIN_SEPARATOR)
                };
                if recursive && metadata.is_dir() {
                    pending.push((entry.path(), key.clone()));
                }
                entries.insert(key, Meta::from(&metadata));
            }
        }
    }
    Ok(Snapshot::Present {
        root: Meta::from(&root),
        entries,
    })
}

/// A change `fs.watch` reports: the event type and the filename, if known.
#[derive(Debug, PartialEq)]
struct Change {
    event: &'static str,
    filename: Option<String>,
}

fn change(event: &'static str, filename: &str) -> Change {
    Change {
        event,
        filename: Some(filename.to_string()),
    }
}

/// The changes between the snapshots `old` and `new` of a watched path, in path order. A new,
/// removed or replaced entry is a `rename`, and a file whose contents or attributes changed is a
/// `change`. A `change` of the watched path itself is named `root_name`, and a `rename` of it is
/// named `root_name`, or `''` if `linux_recursive`. If the watched directory changed without a
/// new, removed or replaced entry, which happens when an entry is created and removed between
/// polls, the change is a `rename` without a filename, unless `linux_recursive`.
fn diff(old: &Snapshot, new: &Snapshot, root_name: &str, linux_recursive: bool) -> Vec<Change> {
    let root_rename = if linux_recursive { "" } else { root_name };
    let none = BTreeMap::new();
    let (old_root, old_entries) = match old {
        Snapshot::Missing => (None, &none),
        Snapshot::Present { root, entries } => (Some(root), entries),
    };
    let (new_root, new_entries) = match new {
        Snapshot::Missing => (None, &none),
        Snapshot::Present { root, entries } => (Some(root), entries),
    };
    let mut changes = Vec::new();
    match (old_root, new_root) {
        (None, Some(_)) => changes.push(change("rename", root_rename)),
        (Some(old), Some(new)) if old.replaced_by(new) => {
            changes.push(change("rename", root_rename));
        }
        (Some(old), Some(new)) if old.kind != FileKind::Directory && old.changed_to(new) => {
            changes.push(change("change", root_name));
        }
        _ => {}
    }
    let names: BTreeSet<&String> = old_entries.keys().chain(new_entries.keys()).collect();
    let mut child_renamed = false;
    for name in names {
        let kind = match (old_entries.get(name), new_entries.get(name)) {
            (Some(old), Some(new)) if old.replaced_by(new) => "rename",
            (Some(old), Some(new)) if old.kind != FileKind::Directory && old.changed_to(new) => {
                "change"
            }
            (Some(_), Some(_)) => continue,
            _ => "rename",
        };
        if kind == "rename" && !name.contains(std::path::MAIN_SEPARATOR) {
            child_renamed = true;
        }
        changes.push(change(kind, name));
    }
    match (old_root, new_root) {
        (Some(_), None) => changes.push(change("rename", root_rename)),
        (Some(old), Some(new))
            if !linux_recursive
                && old.kind == FileKind::Directory
                && new.kind == FileKind::Directory
                && old.mtime_ms != new.mtime_ms
                && !child_renamed =>
        {
            changes.push(Change {
                event: "rename",
                filename: None,
            });
        }
        _ => {}
    }
    changes
}

// ============================================================================
// Handles
// ============================================================================

/// What a handle polls.
enum Poll {
    /// A file or directory, for `fs.watch` and `fsPromises.watch`.
    Tree {
        /// The absolute path of the watched file or directory.
        path: String,
        /// The name of the watched path's own changes, its basename.
        root_name: String,
        recursive: bool,
        /// Whether changes follow the recursive watcher Node.js uses on Linux, as [`diff`]
        /// describes.
        linux_recursive: bool,
        /// The encoding of filenames, `None` for `Buffer`s.
        encoding: Option<Encoding>,
        snapshot: Snapshot,
    },
    /// A file's stats, for `fs.watchFile`.
    Stat {
        path: String,
        interval: Duration,
        /// The last stat that succeeded, zeroed before the first.
        previous: Stat,
        /// libuv's `busy_polling`: 0 before the first stat, 1 after one that succeeded, and the
        /// negated errno after one that failed.
        status: i32,
    },
}

/// A zeroed `Stat`, which libuv reports for a file it can't stat.
fn zero_stat() -> Stat {
    Stat {
        kind: FileKind::Unknown,
        dev: 0,
        mode: 0,
        nlink: 0,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 0,
        ino: 0,
        size: 0,
        blocks: 0,
        atime_ms: 0.0,
        mtime_ms: 0.0,
        ctime_ms: 0.0,
        birthtime_ms: 0.0,
    }
}

/// Whether two stats are equal as libuv's `statbuf_eq` compares them.
fn same_stat(a: &Stat, b: &Stat) -> bool {
    a.ctime_ms == b.ctime_ms
        && a.mtime_ms == b.mtime_ms
        && a.birthtime_ms == b.birthtime_ms
        && a.size == b.size
        && a.mode == b.mode
        && a.uid == b.uid
        && a.gid == b.gid
        && a.ino == b.ino
        && a.dev == b.dev
}

/// The poll state of a watcher. Its `onchange` property is the function that polls report
/// changes to.
#[core_runtime::jsclass(hidden)]
pub struct FSEvent {
    /// What the handle polls, once started.
    #[no_trace]
    poll: Option<Poll>,
    /// The queued poll timer.
    #[no_trace]
    timer: Option<u64>,
    /// Whether the handle keeps the event loop running.
    #[no_trace]
    referenced: bool,
    #[no_trace]
    closed: bool,
    /// The `StatWatcher` reference counts Node.js keeps, `KFSStatWatcherRefCount` and
    /// `KFSStatWatcherMaxRefCount`.
    #[no_trace]
    ref_count: u32,
    #[no_trace]
    max_ref_count: u32,
    /// The `StatWatcher`'s `kOldStatus`.
    #[no_trace]
    old_status: i32,
}

#[core_runtime::jsmethods]
impl FSEvent {
    #[constructor]
    fn new() -> Result<Self, NodeError> {
        Err(NodeError::new(
            ErrorKind::TypeError,
            "ERR_ILLEGAL_CONSTRUCTOR",
            "Illegal constructor",
        ))
    }
}

/// A new handle that polls nothing yet, with `onchange` as its `onchange` property.
fn new_handle<'s>(
    scope: &'s Scope<'_>,
    onchange: HandleValue<'_>,
) -> Result<FSEvent<'s>, ExnThrown> {
    let handle = js::class::create_instance_with::<FSEventImpl>(scope, |_| FSEventImpl {
        poll: None,
        timer: None,
        referenced: true,
        closed: false,
        ref_count: 1,
        max_ref_count: 1,
        old_status: -1,
    })?;
    handle.set_property(scope, c"onchange", onchange)?;
    Ok(handle)
}

/// The handle `value` holds, if it is one.
fn as_handle<'s>(scope: &'s Scope<'_>, value: HandleValue<'_>) -> Option<FSEvent<'s>> {
    Object::from_value(scope, *value)
        .ok()
        .and_then(|object| object.cast::<FSEvent>().ok())
}

/// The handle in `object._handle`, if it has one.
fn handle_of<'s>(
    scope: &'s Scope<'_>,
    object: Object<'_>,
) -> Result<Option<FSEvent<'s>>, ExnThrown> {
    Ok(as_handle(scope, object.val(scope, c"_handle")?))
}

/// Set whether `handle` keeps the event loop running.
fn set_referenced(handle: &FSEvent<'_>, referenced: bool) {
    let timer = {
        let mut data = handle.data_mut();
        data.referenced = referenced;
        data.timer
    };
    if let Some(id) = timer {
        core_runtime::event_loop::with_active_event_loop(|event_loop| {
            event_loop.set_js_timer_ref(id, referenced)
        });
    }
}

/// Stop polling with `handle`. Stopping a stopped handle does nothing.
fn close_handle(handle: &FSEvent<'_>) {
    let timer = {
        let mut data = handle.data_mut();
        data.closed = true;
        data.timer.take()
    };
    if let Some(id) = timer {
        POLL_TIMERS.with_borrow_mut(|timers| timers.remove(&id));
        core_runtime::event_loop::with_active_event_loop(|event_loop| {
            event_loop.clear_js_timer(id)
        });
    }
}

/// Whether `handle` was started.
fn is_started(handle: &FSEvent<'_>) -> bool {
    handle.data().poll.is_some()
}

/// Queue the next poll of `handle` after `delay`.
fn arm(scope: &Scope<'_>, handle: &FSEvent<'_>, delay: Duration) -> Result<(), ExnThrown> {
    let (referenced, is_stat) = {
        let data = handle.data();
        (
            data.referenced,
            matches!(data.poll, Some(Poll::Stat { .. })),
        )
    };
    let callback = closure(scope, 0, poll, &[handle.to_jsval_raw_throwing(scope)?])?;
    let callback = js::Callable::from_jsval(scope, callback.to_jsval_throwing(scope)?, ())
        .map_err(|e| e.throw(scope))?;
    let undefined = HandleValue::undefined();
    let id = core_runtime::event_loop::timer::start_timer(
        scope,
        callback,
        undefined,
        &[],
        delay,
        false,
        None,
    )
    .ok_or_else(|| core_runtime::event_loop::throw_no_event_loop(scope, "fs.watch"))?;
    if !referenced {
        core_runtime::event_loop::with_active_event_loop(|event_loop| {
            event_loop.set_js_timer_ref(id, false)
        });
    }
    handle.data_mut().timer = Some(id);
    POLL_TIMERS.with_borrow_mut(|timers| timers.insert(id, is_stat));
    Ok(())
}

/// What a poll found.
enum Found {
    Tree(Vec<Change>, Option<Encoding>),
    Stat(Option<(i32, Stat, Stat)>),
}

/// The body of a poll timer, whose captures are `[handle]`: poll, queue the next poll, and pass
/// what changed to the handle's `onchange`, until it is closed.
fn poll(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let handle_value = Captures::of(scope, payload)?.get(0)?;
    let Some(handle) = as_handle(scope, handle_value) else {
        return Ok(js::value::undefined());
    };
    {
        let mut data = handle.data_mut();
        if let Some(id) = data.timer.take() {
            POLL_TIMERS.with_borrow_mut(|timers| timers.remove(&id));
        }
        if data.closed {
            return Ok(js::value::undefined());
        }
    }
    let (found, interval) = {
        let mut data = handle.data_mut();
        let blocked = core_runtime::event_loop::blocking_host_io_refusal().is_some();
        match data.poll.as_mut() {
            None => return Ok(js::value::undefined()),
            Some(Poll::Tree { .. }) if blocked => (None, WATCH_INTERVAL),
            Some(Poll::Stat { interval, .. }) if blocked => (None, *interval),
            Some(Poll::Tree {
                path,
                root_name,
                recursive,
                linux_recursive,
                encoding,
                snapshot: old,
            }) => {
                let new = self::snapshot(path, *recursive).unwrap_or(Snapshot::Missing);
                let changes = diff(old, &new, root_name, *linux_recursive);
                *old = new;
                (Some(Found::Tree(changes, *encoding)), WATCH_INTERVAL)
            }
            Some(Poll::Stat {
                path,
                interval,
                previous,
                status,
            }) => {
                let found = match std::fs::metadata(&*path) {
                    Err(error) => {
                        let errno = FsError::from(error).errno;
                        let event = (*status != errno).then_some((errno, zero_stat(), *previous));
                        *status = errno;
                        event
                    }
                    Ok(metadata) => {
                        let stat = Stat::from(&metadata);
                        let event = (*status != 0 && (*status < 0 || !same_stat(previous, &stat)))
                            .then_some((0, stat, *previous));
                        *previous = stat;
                        *status = 1;
                        event
                    }
                };
                (Some(Found::Stat(found)), *interval)
            }
        }
    };
    arm(scope, &handle, interval)?;
    let onchange = handle.get_property(scope, c"onchange")?;
    let this = handle.to_jsval_throwing(scope)?;
    match found {
        None => {}
        Some(Found::Tree(changes, encoding)) => {
            for change in changes {
                if handle.data().closed {
                    break;
                }
                let filename = match &change.filename {
                    Some(name) => text_value(scope, name, encoding)?,
                    None => scope.root_value(js::value::null()),
                };
                let status = scope.root_value(js::value::from_f64(0.0));
                Function::call(
                    scope,
                    this,
                    onchange,
                    &[status, string(scope, change.event)?, filename],
                )?;
            }
        }
        Some(Found::Stat(Some((status, current, previous)))) => {
            let status = scope.root_value(js::value::from_f64(f64::from(status)));
            let current = super::classes::stats_value(scope, &current)?;
            let previous = super::classes::stats_value(scope, &previous)?;
            Function::call(scope, this, onchange, &[status, current, previous])?;
        }
        Some(Found::Stat(None)) => {}
    }
    Ok(js::value::undefined())
}

/// Start polling the file or directory `path` with `handle`: take the first snapshot, and poll
/// every [`WATCH_INTERVAL`]. Unless `persistent`, the handle is unreferenced. Errors name
/// `reported` as the path. Starting a started handle does nothing.
#[allow(clippy::too_many_arguments)]
fn start_tree(
    scope: &Scope<'_>,
    handle: &FSEvent<'_>,
    path: &str,
    reported: &str,
    persistent: bool,
    recursive: bool,
    linux_recursive: bool,
    encoding: Option<Encoding>,
) -> Result<(), Thrown> {
    if is_started(handle) {
        return Ok(());
    }
    super::refuse_blocking("watch")?;
    let resolved = crate::path::resolve_paths(&[path.to_string()]);
    let snapshot =
        snapshot(&resolved, recursive).map_err(|error| watch_error(scope, error, reported))?;
    {
        let mut data = handle.data_mut();
        data.poll = Some(Poll::Tree {
            path: resolved,
            root_name: crate::path::basename_of(path, None).to_string(),
            recursive,
            linux_recursive,
            encoding,
            snapshot,
        });
        if !persistent {
            data.referenced = false;
        }
    }
    arm(scope, handle, WATCH_INTERVAL)?;
    Ok(())
}

/// The error of `fs.watch` failing on `path` with `error`, which has `filename` as well as `path`.
fn watch_error(scope: &Scope<'_>, error: std::io::Error, path: &str) -> Thrown {
    let thrown = UvError::new(error, "watch").path(path).throw(scope);
    if let Some(error) = js::exception::take_pending(scope) {
        if let Ok(object) = Object::from_value(scope, error) {
            let _ = object.set_property(scope, c"filename", path);
        }
        js::exception::set_pending(
            scope,
            error,
            js::native::ExceptionStackBehavior::DoNotCapture,
        );
    }
    thrown.into()
}

/// The `code` of the pending exception, if it has one.
fn pending_code(scope: &Scope<'_>) -> Option<String> {
    let error = js::exception::take_pending(scope)?;
    let code = Object::from_value(scope, error)
        .ok()
        .and_then(|object| object.get_property(scope, c"code").ok())
        .filter(|code| code.is_string())
        .and_then(|code| String::from_jsval(scope, code, ()).ok());
    js::exception::set_pending(
        scope,
        error,
        js::native::ExceptionStackBehavior::DoNotCapture,
    );
    code
}

// ============================================================================
// Options
// ============================================================================

/// `value` as a boolean, or `ERR_INVALID_ARG_TYPE` naming `name`.
fn validate_boolean(scope: &Scope<'_>, value: HandleValue<'_>, name: &str) -> Result<bool, Thrown> {
    if !value.is_boolean() {
        return Err(NodeError::invalid_arg_type(scope, name, "of type boolean", value).into());
    }
    Ok(value.to_boolean())
}

/// Whether `value` is `true`.
fn is_true(value: HandleValue<'_>) -> bool {
    value.is_boolean() && value.to_boolean()
}

/// Node.js's `validateIgnoreOption`: `value` is `undefined`, `null`, a non-empty string, a
/// `RegExp`, a function, or an array of those.
fn validate_ignore_option(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<(), Thrown> {
    if value.is_null_or_undefined() {
        return Ok(());
    }
    if let Ok(object) = Object::from_value(scope, *value) {
        if js::Array::is_array(scope, object.handle())? {
            let length = object.num(scope, c"length")? as u32;
            for index in 0..length {
                let element = object.get_element(scope, index)?;
                validate_ignore_element(scope, element, &format!("{name}[{index}]"))?;
            }
            return Ok(());
        }
    }
    validate_ignore_element(scope, value, name)
}

/// One element of an `ignore` option, as Node.js's `validateIgnoreOptionElement` checks it.
fn validate_ignore_element(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<(), Thrown> {
    if value.is_string() {
        if String::from_jsval(scope, value, ())?.is_empty() {
            return Err(NodeError::invalid_arg_value(
                scope,
                name,
                value,
                "must be a non-empty string",
            )
            .into());
        }
        return Ok(());
    }
    if let Ok(object) = Object::from_value(scope, *value) {
        if object.is_callable() || crate::inspect::is_regexp(scope, object)? {
            return Ok(());
        }
    }
    Err(NodeError::invalid_arg_type(
        scope,
        name,
        "one of type string or function or an instance of RegExp",
        value,
    )
    .into())
}

/// Node.js's `createIgnoreMatcher`: `null` for a missing `ignore`, and otherwise a function
/// returning whether a filename matches one of the patterns. Glob patterns match as minimatch
/// matches them with `matchBase`, case-insensitively on macOS and Windows.
fn create_ignore_matcher<'s>(
    scope: &'s Scope<'_>,
    ignore: HandleValue<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    if ignore.is_null_or_undefined() {
        return Ok(scope.root_value(js::value::null()));
    }
    let matchers = js::Array::new(scope, 0)?;
    let object = Object::from_value(scope, *ignore).ok();
    match object {
        Some(array) if js::Array::is_array(scope, array.handle())? => {
            let length = array.num(scope, c"length")? as u32;
            for index in 0..length {
                matchers.set_element(scope, index, array.get_element(scope, index)?)?;
            }
        }
        _ => matchers.set_element(scope, 0, ignore)?,
    }
    closure(
        scope,
        1,
        ignore_matches,
        &[matchers.to_jsval_raw_throwing(scope)?],
    )?
    .to_jsval_throwing(scope)
}

/// The body of an ignore matcher, whose captures are `[patterns]`: whether the filename argument
/// matches a pattern.
fn ignore_matches(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let matchers = Captures::of(scope, payload)?.obj(0)?;
    let filename = args.get(0);
    let length = matchers.num(scope, c"length")? as u32;
    for index in 0..length {
        let matcher = matchers.get_element(scope, index)?;
        let hit = if matcher.is_string() {
            let pattern = String::from_jsval(scope, matcher, ()).map_err(|e| e.throw(scope))?;
            let text = js::JSString::from_value(scope, filename)?.to_utf8(scope)?;
            let options = GlobOptions {
                nocase: platform_nocase(),
                nocase_magic_only: false,
                match_base: true,
            };
            Glob::new(scope, &pattern, options)?.matches(scope, &text)?
        } else {
            let object = Object::from_value(scope, *matcher).map_err(|e| e.throw(scope))?;
            if object.is_callable() {
                truthy(
                    scope,
                    crate::stream::call(scope, HandleValue::undefined(), matcher, &[filename])?,
                )
            } else {
                !call_method(scope, object, c"exec", &[filename])?.is_null()
            }
        };
        if hit {
            return Ok(js::value::from_bool(true));
        }
    }
    Ok(js::value::from_bool(false))
}

/// Whether `fs.watch` with `recursive` uses Node's Linux watcher on this platform.
fn linux_recursive_platform() -> bool {
    !matches!(crate::process::platform(), "darwin" | "win32")
}

/// Emit `event` on `emitter` in the tick queue.
fn emit_in_tick(scope: &Scope<'_>, emitter: Object<'_>, event: &str) -> Result<(), ExnThrown> {
    let emit = emitter.obj(scope, c"emit")?.ok_or(ExnThrown)?;
    let bound = call_method(scope, emit, c"bind", &[emitter.to_jsval_throwing(scope)?])?;
    next_tick_call(scope, bound, &[string(scope, event)?])
}

// ============================================================================
// FSWatcher
// ============================================================================

/// The `onchange` of an `FSWatcher`'s handle, whose captures are `[watcher]`: emit `change` with
/// the event type and filename, unless the watcher's `_ignoreMatcher` matches the filename.
fn watcher_onchange(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let watcher = Captures::of(scope, payload)?.obj(0)?;
    let filename = args.get(2);
    let matcher = watcher.val(scope, c"_ignoreMatcher")?;
    if !filename.is_null_or_undefined() && is_function(scope, matcher) {
        let ignored = crate::stream::call(
            scope,
            watcher.to_jsval_throwing(scope)?,
            matcher,
            &[filename],
        )?;
        if truthy(scope, ignored) {
            return Ok(js::value::undefined());
        }
    }
    crate::stream::emit(scope, watcher, "change", &[args.get(1), filename])?;
    Ok(js::value::undefined())
}

/// Node's `FSWatcher.prototype[kFSWatchStart]`: validate `ignore`, and start the handle of
/// `watcher` on `filename`. With `throw_if_no_entry` false, a missing file leaves the watcher
/// unstarted. Starting a closed or started watcher does nothing.
#[allow(clippy::too_many_arguments)]
fn start_watcher(
    scope: &Scope<'_>,
    watcher: Object<'_>,
    filename: HandleValue<'_>,
    persistent: HandleValue<'_>,
    recursive: HandleValue<'_>,
    encoding: Option<Encoding>,
    ignore: HandleValue<'_>,
    throw_if_no_entry: bool,
) -> Result<(), Thrown> {
    let Some(handle) = handle_of(scope, watcher)? else {
        return Ok(());
    };
    if is_started(&handle) {
        return Ok(());
    }
    let path = path_arg(scope, filename, "filename")?;
    validate_ignore_option(scope, ignore, "options.ignore")?;
    let matcher = create_ignore_matcher(scope, ignore)?;
    watcher.set_property(scope, c"_ignoreMatcher", matcher)?;
    let started = start_tree(
        scope,
        &handle,
        &path,
        &path,
        is_true(persistent),
        is_true(recursive),
        false,
        encoding,
    );
    match started {
        Err(Thrown::Pending)
            if !throw_if_no_entry && pending_code(scope).as_deref() == Some("ENOENT") =>
        {
            js::exception::clear(scope);
            Ok(())
        }
        result => result,
    }
}

#[core_runtime::jsfunction_class(name = "FSWatcher", extends = "crate::events::event_emitter")]
pub mod fs_watcher_class {
    use super::*;

    /// Node's `FSWatcher` of `internal/fs/watchers`: an emitter with an unstarted handle.
    #[constructor]
    pub fn init(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        crate::events::event_emitter::init(scope, this, None)?;
        let onchange = closure(
            scope,
            3,
            watcher_onchange,
            &[this.to_jsval_raw_throwing(scope)?],
        )?;
        let handle = new_handle(scope, onchange.to_jsval_throwing(scope)?)?;
        this.set_property(scope, c"_handle", handle)?;
        this.set_property(scope, c"_ignoreMatcher", js::value::null())?;
        Ok(())
    }

    /// Does nothing, as Node's undocumented `start` does.
    pub fn start() {}

    /// <https://nodejs.org/api/fs.html#watcherclose>: stop watching, and emit `close` in the
    /// tick queue. Closing an unstarted or closed watcher does nothing.
    pub fn close(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let Some(handle) = handle_of(scope, this)? else {
            return Ok(());
        };
        if !is_started(&handle) {
            return Ok(());
        }
        close_handle(&handle);
        this.set_property(scope, c"_handle", js::value::null())?;
        emit_in_tick(scope, this, "close")
    }

    /// <https://nodejs.org/api/fs.html#watcherref>
    #[method(name = "ref")]
    pub fn ref_impl<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        if let Some(handle) = handle_of(scope, this)? {
            set_referenced(&handle, true);
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/fs.html#watcherunref>
    pub fn unref<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        if let Some(handle) = handle_of(scope, this)? {
            set_referenced(&handle, false);
        }
        Ok(this)
    }
}

// ============================================================================
// The recursive watcher of Node.js on Linux
// ============================================================================

/// The `onchange` of a recursive watcher's handle, whose captures are `[watcher]`: emit `change`,
/// unless the watcher's ignore matcher matches the filename or a directory above it.
fn recursive_onchange(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let watcher = Captures::of(scope, payload)?.obj(0)?;
    let filename = args.get(2);
    let matcher = watcher.val(scope, c"_starlingIgnore")?;
    if filename.is_string() && is_function(scope, matcher) {
        let name = String::from_jsval(scope, filename, ()).map_err(|e| e.throw(scope))?;
        let separator = std::path::MAIN_SEPARATOR;
        let mut prefixes: Vec<&str> = name
            .match_indices(separator)
            .map(|(index, _)| &name[..index])
            .collect();
        if !name.is_empty() {
            prefixes.push(&name);
        }
        let this = watcher.to_jsval_throwing(scope)?;
        for prefix in prefixes {
            let prefix = string(scope, prefix)?;
            if truthy(scope, crate::stream::call(scope, this, matcher, &[prefix])?) {
                return Ok(js::value::undefined());
            }
        }
    }
    crate::stream::emit(scope, watcher, "change", &[args.get(1), filename])?;
    Ok(js::value::undefined())
}

/// Node's Linux `FSWatcher[kFSWatchStart]`: start watching the resolved `filename`. With
/// `throwIfNoEntry` false, a missing file throws, and other errors leave the watcher unstarted,
/// as Node.js's implementation does.
fn start_recursive(
    scope: &Scope<'_>,
    watcher: Object<'_>,
    filename: HandleValue<'_>,
) -> Result<(), Thrown> {
    let path = crate::path::resolve_paths(&[path_arg(scope, filename, "path")?]);
    let Some(handle) = watcher
        .val(scope, c"_starlingHandle")
        .map(|value| as_handle(scope, value))?
    else {
        return Ok(());
    };
    watcher.define_property(scope, c"_starlingClosed", false, 0)?;
    let persistent = watcher.val(scope, c"_starlingPersistent")?;
    let throw_if_no_entry = watcher.flag(scope, c"_starlingThrowIfNoEntry")?;
    match start_tree(
        scope,
        &handle,
        &path,
        &path,
        is_true(persistent),
        true,
        true,
        Some(Encoding::Utf8),
    ) {
        Err(Thrown::Pending) => {
            if !throw_if_no_entry && pending_code(scope).as_deref() == Some("ENOENT") {
                return Err(Thrown::Pending);
            }
            js::exception::clear(scope);
            Ok(())
        }
        result => result,
    }
}

/// The `change` listener of a pending `next()` of a recursive watcher's async iterator, whose
/// captures are `[promise, signal, onAbort]`: fulfill the promise with the change.
fn recursive_next_change(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let promise = captures
        .obj(0)?
        .cast::<js::Promise>()
        .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))?;
    if let Ok(signal) = Object::from_value(scope, *captures.get(1)?) {
        let on_abort = captures.get(2)?;
        call_method(
            scope,
            signal,
            c"removeEventListener",
            &[string(scope, "abort")?, on_abort],
        )?;
    }
    let value = Object::new_plain(scope)?;
    value.set_property(scope, c"eventType", args.get(0))?;
    value.set_property(scope, c"filename", args.get(1))?;
    let result = Object::new_plain(scope)?;
    result.set_prototype(scope, js::native::HandleObject::null())?;
    result.set_property(scope, c"value", value)?;
    promise.resolve(scope, result.to_jsval_throwing(scope)?)?;
    Ok(js::value::undefined())
}

/// The `abort` listener of a pending `next()` of a recursive watcher's async iterator, whose
/// captures are `[promise, watcher, signal]`: close the watcher, and reject the promise with an
/// `AbortError`.
fn recursive_next_abort(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let promise = captures
        .obj(0)?
        .cast::<js::Promise>()
        .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))?;
    let watcher = captures.obj(1)?;
    let signal = captures.obj(2)?;
    recursive_watcher_class::close(scope, watcher)?;
    let reason = signal.val(scope, c"reason")?;
    let error = abort_error(scope, Some(reason))?;
    promise.reject(scope, error)?;
    Ok(js::value::undefined())
}

/// A result of a recursive watcher's async iterator that has `done` and no prototype.
fn done_result(scope: &Scope<'_>) -> Result<Value, ExnThrown> {
    let result = Object::new_plain(scope)?;
    result.set_prototype(scope, js::native::HandleObject::null())?;
    result.set_property(scope, c"done", true)?;
    result.to_jsval_raw_throwing(scope)
}

/// `next()` of a recursive watcher's async iterator, whose captures are `[watcher]`: a done result
/// once the watcher is closed, and otherwise a promise for the next change.
fn recursive_next(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let watcher = Captures::of(scope, payload)?.obj(0)?;
    if watcher.flag(scope, c"_starlingClosed")? {
        return done_result(scope);
    }
    let promise = js::Promise::new_pending(scope)?;
    let promise_value = promise.to_jsval_throwing(scope)?;
    let signal = watcher.val(scope, c"_starlingSignal")?;
    let mut on_abort = HandleValue::undefined();
    if let Ok(signal_object) = Object::from_value(scope, *signal) {
        let abort = closure(
            scope,
            0,
            recursive_next_abort,
            &[
                *promise_value,
                watcher.to_jsval_raw_throwing(scope)?,
                *signal,
            ],
        )?;
        if signal_object.flag(scope, c"aborted")? {
            Function::call(
                scope,
                HandleValue::undefined(),
                abort.to_jsval_throwing(scope)?,
                js::function::EmptyArgs,
            )?;
            return promise.to_jsval_raw_throwing(scope);
        }
        on_abort = abort.to_jsval_throwing(scope)?;
        let options = Object::new_plain(scope)?;
        options.set_property(scope, c"once", true)?;
        call_method(
            scope,
            signal_object,
            c"addEventListener",
            &[
                string(scope, "abort")?,
                on_abort,
                options.to_jsval_throwing(scope)?,
            ],
        )?;
    }
    let listener = closure(
        scope,
        2,
        recursive_next_change,
        &[*promise_value, *signal, *on_abort],
    )?;
    crate::stream::listen(scope, watcher, c"once", "change", listener)?;
    promise.to_jsval_raw_throwing(scope)
}

/// `return()` of a recursive watcher's async iterator, whose captures are `[watcher]`: close the
/// watcher, and return a done result.
fn recursive_return(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let watcher = Captures::of(scope, payload)?.obj(0)?;
    recursive_watcher_class::close(scope, watcher)?;
    done_result(scope)
}

/// `[Symbol.asyncIterator]()` of an async iterator: the iterator itself.
fn iterator_self(
    _scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(args.this())
}

/// `[Symbol.asyncIterator]()` of a recursive watcher: an iterator whose `next()` waits for the
/// next `change` event, and which ends once the watcher closes.
fn recursive_async_iterator(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let watcher = crate::stream::this_object(scope, &args)?;
    let watcher_value = watcher.to_jsval_throwing(scope)?;
    let iterator = Object::new_plain(scope)?;
    iterator.set_property(
        scope,
        c"next",
        closure(scope, 0, recursive_next, &[watcher_value])?,
    )?;
    iterator.set_property(
        scope,
        c"return",
        closure(scope, 0, recursive_return, &[watcher_value])?,
    )?;
    let itself = Function::new_callback(scope, c"[Symbol.asyncIterator]", 0, iterator_self, ())?;
    js::class::define_well_known_symbol_property(
        scope,
        iterator,
        js::native::SymbolCode::asyncIterator,
        *itself,
    )?;
    iterator.to_jsval_raw_throwing(scope)
}

#[core_runtime::jsfunction_class(
    name = "FSWatcher",
    extends = "crate::events::event_emitter",
    class_syntax
)]
pub mod recursive_watcher_class {
    use super::*;

    /// Node's `FSWatcher` of `internal/fs/recursive_watch`, with `options` validated as Node
    /// validates them.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        crate::events::event_emitter::init(scope, this, None)?;
        let options = options.filter(|options| !options.is_undefined());
        let get =
            |name| option(scope, options, name).map(|value| value.filter(|value| !value.is_null()));
        if let Some(recursive) = get(c"recursive")? {
            validate_boolean(scope, recursive, "options.recursive")?;
        }
        let persistent = get(c"persistent")?;
        if let Some(persistent) = persistent {
            validate_boolean(scope, persistent, "options.persistent")?;
        }
        let signal = get(c"signal")?;
        if let Some(signal) = signal {
            validate_abort_signal(scope, signal, "options.signal")?;
        }
        let throw_if_no_entry = match get(c"throwIfNoEntry")? {
            Some(value) => validate_boolean(scope, value, "options.throwIfNoEntry")?,
            None => true,
        };
        if let Some(encoding) = get(c"encoding")? {
            if !encoding.is_string() {
                return Err(NodeError::invalid_arg_value(
                    scope,
                    "options.encoding",
                    encoding,
                    "is invalid",
                )
                .into());
            }
        }
        let ignore = option(scope, options, c"ignore")?.unwrap_or(HandleValue::undefined());
        validate_ignore_option(scope, ignore, "options.ignore")?;
        let matcher = create_ignore_matcher(scope, ignore)?;
        let onchange = closure(
            scope,
            3,
            recursive_onchange,
            &[this.to_jsval_raw_throwing(scope)?],
        )?;
        let handle = new_handle(scope, onchange.to_jsval_throwing(scope)?)?;
        let undefined = HandleValue::undefined();
        this.define_property(scope, c"_starlingHandle", handle, 0)?;
        this.define_property(scope, c"_starlingIgnore", matcher, 0)?;
        this.define_property(
            scope,
            c"_starlingPersistent",
            persistent.unwrap_or(undefined),
            0,
        )?;
        this.define_property(scope, c"_starlingSignal", signal.unwrap_or(undefined), 0)?;
        this.define_property(scope, c"_starlingThrowIfNoEntry", throw_if_no_entry, 0)?;
        this.define_property(scope, c"_starlingClosed", false, 0)?;
        Ok(())
    }

    /// <https://nodejs.org/api/fs.html#watcherclose>: stop watching, and emit `close`. Closing a
    /// closed watcher does nothing.
    pub fn close(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        if this.flag(scope, c"_starlingClosed")? {
            return Ok(());
        }
        this.define_property(scope, c"_starlingClosed", true, 0)?;
        if let Some(handle) = as_handle(scope, this.val(scope, c"_starlingHandle")?) {
            close_handle(&handle);
        }
        crate::stream::emit(scope, this, "close", &[])?;
        Ok(())
    }

    /// Does nothing, as on Linux in Node.js.
    #[method(name = "ref")]
    pub fn ref_impl() {}

    /// Does nothing, as on Linux in Node.js.
    pub fn unref() {}

    /// Define `[Symbol.asyncIterator]`.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let iterator = Function::new_callback(
            scope,
            c"[Symbol.asyncIterator]",
            0,
            recursive_async_iterator,
            (),
        )?;
        js::class::define_well_known_symbol_property(
            scope,
            proto,
            js::native::SymbolCode::asyncIterator,
            *iterator,
        )
    }
}

// ============================================================================
// fs.watch
// ============================================================================

/// The body of an `abort` listener or tick callback, whose captures are `[watcher]`: close the
/// watcher.
fn close_watcher(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let watcher = Captures::of(scope, payload)?.obj(0)?;
    call_method(scope, watcher, c"close", &[])?;
    Ok(js::value::undefined())
}

/// The `close` listener of a watcher with a signal, whose captures are `[signal, listener]`:
/// remove the `abort` listener.
fn remove_abort_listener(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let signal = captures.obj(0)?;
    call_method(
        scope,
        signal,
        c"removeEventListener",
        &[string(scope, "abort")?, captures.get(1)?],
    )?;
    Ok(js::value::undefined())
}

/// <https://nodejs.org/api/fs.html#fswatchfilename-options-listener>
pub(crate) fn watch<'s>(
    scope: &'s Scope<'_>,
    filename: HandleValue<'_>,
    options: HandleValue<'_>,
    listener: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let (options, listener) = if is_function(scope, options) {
        (HandleValue::undefined(), options)
    } else {
        (options, listener)
    };
    let encoding = super::encoding_option(scope, Some(options), Some(Encoding::Utf8))?;
    let encoding_value = if options.is_string() {
        Some(options)
    } else {
        option(scope, Some(options), c"encoding")?
    };
    let options = if options.is_string() {
        None
    } else {
        Some(options)
    };
    let signal = option(scope, options, c"signal")?;
    if let Some(signal) = signal {
        validate_abort_signal(scope, signal, "options.signal")?;
    }
    let get = |name, default: Value| -> Result<HandleValue<'s>, ExnThrown> {
        Ok(option(scope, options, name)?.unwrap_or_else(|| scope.root_value(default)))
    };
    let persistent = get(c"persistent", js::value::from_bool(true))?;
    let recursive = get(c"recursive", js::value::from_bool(false))?;
    let throw_if_no_entry = get(c"throwIfNoEntry", js::value::from_bool(true))?;
    let ignore = get(c"ignore", js::value::undefined())?;
    let path = path_arg(scope, filename, "path")?;
    let path_value = js::JSString::from_str(scope, &path)?.to_jsval_throwing(scope)?;
    let watcher = if truthy(scope, recursive) && linux_recursive_platform() {
        let class = recursive_watcher_class::class_constructor(scope)?;
        let copy = Object::new_plain(scope)?;
        for (name, value) in [
            (c"persistent", persistent),
            (c"recursive", recursive),
            (c"throwIfNoEntry", throw_if_no_entry),
            (c"ignore", ignore),
        ] {
            copy.set_property(scope, name, value)?;
        }
        if let Some(signal) = signal {
            copy.set_property(scope, c"signal", signal)?;
        }
        if let Some(encoding) = encoding_value {
            copy.set_property(scope, c"encoding", encoding)?;
        }
        let watcher = Function::construct(scope, class, &[copy.to_jsval_throwing(scope)?])?;
        start_recursive(scope, watcher, path_value)?;
        watcher
    } else {
        let class = fs_watcher_class::class_constructor(scope)?;
        let watcher = Function::construct(scope, class, js::function::EmptyArgs)?;
        start_watcher(
            scope,
            watcher,
            path_value,
            persistent,
            recursive,
            encoding,
            ignore,
            truthy(scope, throw_if_no_entry),
        )?;
        watcher
    };
    if truthy(scope, listener) {
        call_method(
            scope,
            watcher,
            c"addListener",
            &[string(scope, "change")?, listener],
        )?;
    }
    if let Some(signal) = signal.and_then(|signal| Object::from_value(scope, *signal).ok()) {
        let watcher_value = watcher.to_jsval_throwing(scope)?;
        if signal.flag(scope, c"aborted")? {
            crate::stream::next_tick(scope, close_watcher, &[watcher_value.get()])?;
        } else {
            let abort = closure(scope, 0, close_watcher, &[watcher_value])?;
            crate::stream::listen(scope, signal, c"addEventListener", "abort", abort)?;
            let remove = closure(
                scope,
                0,
                remove_abort_listener,
                &[
                    signal.to_jsval_raw_throwing(scope)?,
                    abort.to_jsval_raw_throwing(scope)?,
                ],
            )?;
            crate::stream::listen(scope, watcher, c"once", "close", remove)?;
        }
    }
    Ok(watcher)
}

// ============================================================================
// StatWatcher, fs.watchFile and fs.unwatchFile
// ============================================================================

/// The `onchange` of a `StatWatcher`'s handle, whose captures are `[watcher, handle]`: emit
/// `change` with the current and previous stats, as Node's `onchange` does.
fn stat_onchange(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let captures = Captures::of(scope, payload)?;
    let watcher = captures.obj(0)?;
    let Some(handle) = as_handle(scope, captures.get(1)?) else {
        return Ok(js::value::undefined());
    };
    let status = args.get(0).to_number() as i32;
    let current = args.get(1);
    let previous = args.get(2);
    let nlink = |stats: HandleValue<'_>| -> Result<f64, ExnThrown> {
        Object::from_value(scope, *stats)
            .map_err(|e| e.throw(scope))?
            .num(scope, c"nlink")
    };
    if handle.data().old_status == -1 && status == -1 && nlink(current)? == nlink(previous)? {
        return Ok(js::value::undefined());
    }
    handle.data_mut().old_status = status;
    crate::stream::emit(scope, watcher, "change", &[current, previous])?;
    Ok(js::value::undefined())
}

/// Node's `StatWatcher.prototype[kFSStatWatcherStart]`: start polling the stats of `filename`
/// every `interval` milliseconds. Starting a started watcher does nothing.
fn start_stat_watcher(
    scope: &Scope<'_>,
    watcher: Object<'_>,
    filename: HandleValue<'_>,
    persistent: HandleValue<'_>,
    interval: HandleValue<'_>,
) -> Result<(), Thrown> {
    if handle_of(scope, watcher)?.is_some() {
        return Ok(());
    }
    let handle = new_handle(scope, HandleValue::undefined())?;
    let handle_value = handle.to_jsval_throwing(scope)?;
    let onchange = closure(
        scope,
        3,
        stat_onchange,
        &[watcher.to_jsval_raw_throwing(scope)?, *handle_value],
    )?;
    handle.set_property(scope, c"onchange", onchange)?;
    watcher.set_property(scope, c"_handle", handle_value)?;
    if !truthy(scope, persistent) {
        stat_watcher_class::unref(scope, watcher)?;
    }
    let path = path_arg(scope, filename, "filename")?;
    let interval = validate_integer(scope, interval, "interval", 0.0, 4294967295.0)?;
    handle.data_mut().poll = Some(Poll::Stat {
        path,
        interval: Duration::from_millis(interval.max(1.0) as u64),
        previous: zero_stat(),
        status: 0,
    });
    arm(scope, &handle, Duration::ZERO)?;
    Ok(())
}

/// Node's `StatWatcher.prototype[kFSStatWatcherAddOrCleanRef]` for `operate`.
fn add_or_clean_ref(
    scope: &Scope<'_>,
    watcher: Object<'_>,
    operate: &str,
) -> Result<(), ExnThrown> {
    let Some(handle) = handle_of(scope, watcher)? else {
        return Ok(());
    };
    match operate {
        "add" => {
            let mut data = handle.data_mut();
            data.ref_count += 1;
            data.max_ref_count += 1;
        }
        "clean" => {
            {
                let mut data = handle.data_mut();
                data.max_ref_count = data.max_ref_count.saturating_sub(1);
            }
            stat_watcher_class::unref(scope, watcher)?;
        }
        _ => {
            {
                let mut data = handle.data_mut();
                data.max_ref_count = 0;
                data.ref_count = 0;
            }
            set_referenced(&handle, false);
        }
    }
    Ok(())
}

#[core_runtime::jsfunction_class(name = "StatWatcher", extends = "crate::events::event_emitter")]
pub mod stat_watcher_class {
    use super::*;

    /// Node's `StatWatcher` of `internal/fs/watchers`: an emitter without a handle. Stats are
    /// never `BigInt`s.
    #[constructor]
    pub fn init(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        crate::events::event_emitter::init(scope, this, None)?;
        this.set_property(scope, c"_handle", js::value::null())?;
        Ok(())
    }

    /// Does nothing, as Node's undocumented `start` does.
    pub fn start() {}

    /// Stop polling, and emit `stop` in the tick queue. Stopping a stopped watcher does nothing.
    pub fn stop(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        let Some(handle) = handle_of(scope, this)? else {
            return Ok(());
        };
        emit_in_tick(scope, this, "stop")?;
        close_handle(&handle);
        this.set_property(scope, c"_handle", js::value::null())
    }

    /// <https://nodejs.org/api/fs.html#statwatcherref>: count a reference, and reference the
    /// handle when the count leaves 0. The count stops at the number of listeners added.
    #[method(name = "ref")]
    pub fn ref_impl<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        if let Some(handle) = handle_of(scope, this)? {
            let referenced = {
                let mut data = handle.data_mut();
                if data.ref_count == data.max_ref_count {
                    return Ok(this);
                }
                data.ref_count += 1;
                data.ref_count == 1
            };
            if referenced {
                set_referenced(&handle, true);
            }
        }
        Ok(this)
    }

    /// <https://nodejs.org/api/fs.html#statwatcherunref>: drop a counted reference, and
    /// unreference the handle when the count reaches 0.
    pub fn unref<'s>(scope: &Scope<'_>, this: Object<'s>) -> Result<Object<'s>, ExnThrown> {
        if let Some(handle) = handle_of(scope, this)? {
            let unreferenced = {
                let mut data = handle.data_mut();
                if data.ref_count == 0 {
                    return Ok(this);
                }
                data.ref_count -= 1;
                data.ref_count == 0
            };
            if unreferenced {
                set_referenced(&handle, false);
            }
        }
        Ok(this)
    }
}

/// The key of the per-realm `Map` of the `StatWatcher`s by resolved filename.
static STAT_WATCHERS_KEY: u8 = 0;

/// The `Map` of the `StatWatcher`s `fs.watchFile` started, by resolved filename.
fn stat_watchers<'s>(scope: &'s Scope<'_>) -> Result<js::Map<'s>, ExnThrown> {
    let map = js::class::get_or_init_shared_object(
        scope,
        &STAT_WATCHERS_KEY as *const u8 as usize,
        |scope| Ok(*js::Map::new(scope)?),
    )?;
    map.cast::<js::Map>()
        .map_err(|_| js::error::TypeError("not a Map".into()).throw(scope))
}

/// <https://nodejs.org/api/fs.html#fswatchfilefilename-options-listener>
pub(crate) fn watch_file<'s>(
    scope: &'s Scope<'_>,
    filename: HandleValue<'_>,
    options: HandleValue<'_>,
    listener: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let path = crate::path::resolve_paths(&[path_arg(scope, filename, "path")?]);
    let (options, listener) = match Object::from_value(scope, *options) {
        Ok(options) if !options.is_callable() => (Some(options), listener),
        _ => (None, options),
    };
    let read =
        |name: &'static std::ffi::CStr, default: Value| -> Result<HandleValue<'s>, ExnThrown> {
            match options {
                Some(options) if options.has_own_property(scope, name)? => {
                    options.get_property(scope, name)
                }
                _ => Ok(scope.root_value(default)),
            }
        };
    let interval = read(c"interval", js::value::from_f64(WATCH_FILE_INTERVAL))?;
    let persistent = read(c"persistent", js::value::from_bool(true))?;
    validate_function(scope, listener, "listener")?;
    let key = js::JSString::from_str(scope, &path)?.to_jsval_throwing(scope)?;
    let map = stat_watchers(scope)?;
    let existing = map.lookup(scope, key)?;
    let watcher = match Object::from_value(scope, *existing) {
        Ok(watcher) => {
            add_or_clean_ref(scope, watcher, "add")?;
            watcher
        }
        Err(_) => {
            let class = stat_watcher_class::class_constructor(scope)?;
            let watcher = Function::construct(scope, class, js::function::EmptyArgs)?;
            start_stat_watcher(scope, watcher, key, persistent, interval)?;
            map.insert(scope, key, watcher.to_jsval_throwing(scope)?)?;
            watcher
        }
    };
    call_method(
        scope,
        watcher,
        c"addListener",
        &[string(scope, "change")?, listener],
    )?;
    Ok(watcher)
}

/// <https://nodejs.org/api/fs.html#fsunwatchfilefilename-listener>
pub(crate) fn unwatch_file(
    scope: &Scope<'_>,
    filename: HandleValue<'_>,
    listener: HandleValue<'_>,
) -> Result<(), Thrown> {
    let path = crate::path::resolve_paths(&[path_arg(scope, filename, "path")?]);
    let key = js::JSString::from_str(scope, &path)?.to_jsval_throwing(scope)?;
    let map = stat_watchers(scope)?;
    let Ok(watcher) = Object::from_value(scope, *map.lookup(scope, key)?) else {
        return Ok(());
    };
    let change = string(scope, "change")?;
    if is_function(scope, listener) {
        let before = crate::stream::listener_count(scope, watcher, change)?;
        call_method(scope, watcher, c"removeListener", &[change, listener])?;
        if crate::stream::listener_count(scope, watcher, change)? < before {
            add_or_clean_ref(scope, watcher, "clean")?;
        }
    } else {
        call_method(scope, watcher, c"removeAllListeners", &[change])?;
        add_or_clean_ref(scope, watcher, "cleanAll")?;
    }
    if crate::stream::listener_count(scope, watcher, change)? == 0.0 {
        stat_watcher_class::stop(scope, watcher)?;
        map.delete(scope, key)?;
    }
    Ok(())
}

// ============================================================================
// fsPromises.watch
// ============================================================================

/// `ERR_FS_WATCH_QUEUE_OVERFLOW` for `max` events.
fn queue_overflow(max: f64) -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "ERR_FS_WATCH_QUEUE_OVERFLOW",
        format!(
            "fs.watch() queued more than {} events",
            crate::errors::number_to_string(max)
        ),
    )
}

/// The `onchange` of the handle of an `fsPromises.watch` iterator, whose captures are `[state]`:
/// queue the change, or pass it to a waiting `next()`, unless the ignore matcher matches the
/// filename. Changes beyond `maxQueue` queued ones are dropped with a warning, or replace the
/// queue with an `ERR_FS_WATCH_QUEUE_OVERFLOW` error if `overflow` is `'error'`.
fn iterator_onchange(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Captures::of(scope, payload)?.obj(0)?;
    let filename = args.get(2);
    let matcher = state.val(scope, c"ignore")?;
    if !filename.is_null_or_undefined() && is_function(scope, matcher) {
        let ignored = crate::stream::call(scope, HandleValue::undefined(), matcher, &[filename])?;
        if truthy(scope, ignored) {
            return Ok(js::value::undefined());
        }
    }
    let queue = state.obj(scope, c"queue")?.ok_or(ExnThrown)?;
    let max = state.num(scope, c"maxQueue")?;
    if queue.num(scope, c"length")? < max {
        let item = Object::new_plain(scope)?;
        item.set_prototype(scope, js::native::HandleObject::null())?;
        item.set_property(scope, c"eventType", args.get(1))?;
        item.set_property(scope, c"filename", filename)?;
        call_method(scope, queue, c"push", &[item.to_jsval_throwing(scope)?])?;
    } else if state.flag(scope, c"overflowError")? {
        queue.set_property(scope, c"length", 0)?;
        let error = crate::stream::error_value(scope, queue_overflow(max))?;
        state.set_property(scope, c"error", error)?;
    } else {
        crate::process::emit_warning(scope, "fs.watch maxQueue exceeded", "Warning")?;
    }
    settle_waiting(scope, state)
}

/// Settle the waiting `next()` promises of an `fsPromises.watch` iterator's `state` with what
/// is queued: changes first, then the error or the abort, which ends the iteration.
fn settle_waiting(scope: &Scope<'_>, state: Object<'_>) -> Result<Value, ExnThrown> {
    let waiting = state.obj(scope, c"waiting")?.ok_or(ExnThrown)?;
    while waiting.num(scope, c"length")? > 0.0 {
        let Some(outcome) = next_outcome(scope, state)? else {
            break;
        };
        let promise = call_method(scope, waiting, c"shift", &[])?;
        let promise = Object::from_value(scope, *promise)
            .map_err(|e| e.throw(scope))?
            .cast::<js::Promise>()
            .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))?;
        match outcome {
            Ok(value) => promise.resolve(scope, value)?,
            Err(error) => promise.reject(scope, error)?,
        }
    }
    Ok(js::value::undefined())
}

/// What the next `next()` of an `fsPromises.watch` iterator's `state` settles with, if it is
/// known: a queued change, a done result once the iteration ended, or the error or abort that
/// ends it. Ending the iteration closes the handle.
fn next_outcome<'s>(
    scope: &'s Scope<'_>,
    state: Object<'_>,
) -> Result<Option<Result<HandleValue<'s>, HandleValue<'s>>>, ExnThrown> {
    if state.flag(scope, c"done")? {
        let result = js::iteration::create_iter_result(scope, js::value::undefined(), true)?;
        return Ok(Some(Ok(result.to_jsval_throwing(scope)?)));
    }
    let queue = state.obj(scope, c"queue")?.ok_or(ExnThrown)?;
    if queue.num(scope, c"length")? > 0.0 {
        let item = call_method(scope, queue, c"shift", &[])?;
        let result = js::iteration::create_iter_result(scope, item, false)?;
        return Ok(Some(Ok(result.to_jsval_throwing(scope)?)));
    }
    let error = state.val(scope, c"error")?;
    if !error.is_undefined() {
        finish_iterator(scope, state)?;
        return Ok(Some(Err(error)));
    }
    let signal = state.val(scope, c"signal")?;
    if let Ok(signal) = Object::from_value(scope, *signal) {
        if signal.flag(scope, c"aborted")? {
            let reason = signal.val(scope, c"reason")?;
            finish_iterator(scope, state)?;
            return Ok(Some(Err(abort_error(scope, Some(reason))?)));
        }
    }
    Ok(None)
}

/// End the iteration of an `fsPromises.watch` iterator's `state`: close its handle, and remove
/// its `abort` listener, as the generator's `finally` block does.
fn finish_iterator(scope: &Scope<'_>, state: Object<'_>) -> Result<(), ExnThrown> {
    state.set_property(scope, c"done", true)?;
    if let Some(handle) = as_handle(scope, state.val(scope, c"handle")?) {
        close_handle(&handle);
    }
    let signal = state.val(scope, c"signal")?;
    let oncancel = state.val(scope, c"oncancel")?;
    if let (Ok(signal), false) = (Object::from_value(scope, *signal), oncancel.is_undefined()) {
        call_method(
            scope,
            signal,
            c"removeEventListener",
            &[string(scope, "abort")?, oncancel],
        )?;
    }
    Ok(())
}

/// The `abort` listener of an `fsPromises.watch` iterator, whose captures are `[state]`: stop
/// watching, and settle the waiting `next()` promises.
fn iterator_cancel(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Captures::of(scope, payload)?.obj(0)?;
    if let Some(handle) = as_handle(scope, state.val(scope, c"handle")?) {
        close_handle(&handle);
    }
    settle_waiting(scope, state)
}

/// The body of `fsPromises.watch` up to its first `yield`: validate the arguments and start the
/// handle, as Node's async generator does on the first `next()`.
fn start_iterator(scope: &Scope<'_>, state: Object<'_>) -> Result<(), Thrown> {
    let filename = state.val(scope, c"filename")?;
    let path = path_arg(scope, filename, "path")?;
    let options = state.val(scope, c"options")?;
    let options = if options.is_undefined() {
        None
    } else {
        Some(validate_plain_object(scope, options, "options", false)?)
    };
    let value = |name: &'static std::ffi::CStr| -> Result<Option<HandleValue<'_>>, ExnThrown> {
        match options {
            Some(options) => {
                let value = options.get_property(scope, name)?;
                Ok((!value.is_undefined()).then_some(value))
            }
            None => Ok(None),
        }
    };
    let persistent = match value(c"persistent")? {
        Some(persistent) => validate_boolean(scope, persistent, "options.persistent")?,
        None => true,
    };
    let recursive = match value(c"recursive")? {
        Some(recursive) => validate_boolean(scope, recursive, "options.recursive")?,
        None => false,
    };
    let max_queue = match value(c"maxQueue")? {
        Some(max_queue) => validate_integer(
            scope,
            max_queue,
            "options.maxQueue",
            -9007199254740991.0,
            9007199254740991.0,
        )?,
        None => 2048.0,
    };
    let overflow_error = match value(c"overflow")? {
        Some(overflow) => {
            let name = overflow
                .is_string()
                .then(|| String::from_jsval(scope, overflow, ()))
                .transpose()?;
            match name.as_deref() {
                Some("ignore") => false,
                Some("error") => true,
                _ => {
                    return Err(NodeError::invalid_arg_value(
                        scope,
                        "options.overflow",
                        overflow,
                        "must be one of: 'ignore', 'error'",
                    )
                    .into())
                }
            }
        }
        None => false,
    };
    let signal = value(c"signal")?.unwrap_or(HandleValue::undefined());
    validate_abort_signal(scope, signal, "options.signal")?;
    let ignore = value(c"ignore")?.unwrap_or(HandleValue::undefined());
    validate_ignore_option(scope, ignore, "options.ignore")?;
    let encoding = match value(c"encoding")? {
        None => Some(Encoding::Utf8),
        Some(encoding) if !truthy(scope, encoding) => Some(Encoding::Utf8),
        Some(encoding) => {
            let parsed = encoding
                .is_string()
                .then(|| String::from_jsval(scope, encoding, ()))
                .transpose()?
                .and_then(|name| Encoding::parse(&name));
            match parsed {
                Some(parsed) => Some(parsed),
                None => {
                    return Err(NodeError::invalid_arg_value(
                        scope,
                        "encoding",
                        encoding,
                        "is invalid encoding",
                    )
                    .into())
                }
            }
        }
    };
    if let Ok(signal) = Object::from_value(scope, *signal) {
        if signal.flag(scope, c"aborted")? {
            let reason = signal.val(scope, c"reason")?;
            let error = abort_error(scope, Some(reason))?;
            js::exception::set_pending(
                scope,
                error,
                js::native::ExceptionStackBehavior::DoNotCapture,
            );
            return Err(Thrown::Pending);
        }
    }
    let state_value = state.to_jsval_throwing(scope)?;
    let onchange = closure(scope, 3, iterator_onchange, &[state_value])?;
    let handle = new_handle(scope, onchange.to_jsval_throwing(scope)?)?;
    state.set_property(scope, c"handle", handle)?;
    state.set_property(scope, c"maxQueue", max_queue)?;
    state.set_property(scope, c"overflowError", overflow_error)?;
    state.set_property(scope, c"signal", signal)?;
    state.set_property(scope, c"ignore", create_ignore_matcher(scope, ignore)?)?;
    if let Ok(signal) = Object::from_value(scope, *signal) {
        let oncancel = closure(scope, 0, iterator_cancel, &[state_value])?;
        state.set_property(scope, c"oncancel", oncancel)?;
        let options = Object::new_plain(scope)?;
        options.set_property(scope, c"once", true)?;
        call_method(
            scope,
            signal,
            c"addEventListener",
            &[
                string(scope, "abort")?,
                oncancel.to_jsval_throwing(scope)?,
                options.to_jsval_throwing(scope)?,
            ],
        )?;
    }
    let reported = js::JSString::from_value(scope, filename)?.to_utf8(scope)?;
    let started = start_tree(
        scope, &handle, &path, &reported, persistent, recursive, false, encoding,
    );
    if let Err(thrown) = started {
        if let Thrown::Pending = thrown {
            if let Some(error) = js::exception::take_pending(scope) {
                if let Ok(object) = Object::from_value(scope, error) {
                    object.set_property(scope, c"filename", filename)?;
                }
                js::exception::set_pending(
                    scope,
                    error,
                    js::native::ExceptionStackBehavior::DoNotCapture,
                );
            }
        }
        return Err(thrown);
    }
    Ok(())
}

/// `next()` of an `fsPromises.watch` iterator, whose captures are `[state]`: a promise for the
/// next change.
fn iterator_next(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Captures::of(scope, payload)?.obj(0)?;
    if !state.flag(scope, c"started")? && !state.flag(scope, c"done")? {
        state.set_property(scope, c"started", true)?;
        if let Err(thrown) = start_iterator(scope, state) {
            thrown.throw(scope);
            let error = crate::stream::take_exception(scope);
            finish_iterator(scope, state)?;
            return js::Promise::new_rejected_with_error(scope, error)?
                .to_jsval_raw_throwing(scope);
        }
    }
    let waiting = state.obj(scope, c"waiting")?.ok_or(ExnThrown)?;
    if waiting.num(scope, c"length")? == 0.0 {
        match next_outcome(scope, state)? {
            Some(Ok(value)) => {
                return js::Promise::new_resolved_with_value(scope, value)?
                    .to_jsval_raw_throwing(scope)
            }
            Some(Err(error)) => {
                return js::Promise::new_rejected_with_error(scope, error)?
                    .to_jsval_raw_throwing(scope)
            }
            None => {}
        }
    }
    let promise = js::Promise::new_pending(scope)?;
    call_method(
        scope,
        waiting,
        c"push",
        &[promise.to_jsval_throwing(scope)?],
    )?;
    promise.to_jsval_raw_throwing(scope)
}

/// `return(value)` of an `fsPromises.watch` iterator, whose captures are `[state]`: end the
/// iteration, and return a promise for a done result with `value`.
fn iterator_return(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Captures::of(scope, payload)?.obj(0)?;
    finish_iterator(scope, state)?;
    settle_waiting(scope, state)?;
    let result = js::iteration::create_iter_result(scope, args.get(0), true)?;
    js::Promise::new_resolved_with_value(scope, result)?.to_jsval_raw_throwing(scope)
}

/// `throw(error)` of an `fsPromises.watch` iterator, whose captures are `[state]`: end the
/// iteration, and return a promise rejected with `error`.
fn iterator_throw(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Captures::of(scope, payload)?.obj(0)?;
    finish_iterator(scope, state)?;
    settle_waiting(scope, state)?;
    js::Promise::new_rejected_with_error(scope, args.get(0))?.to_jsval_raw_throwing(scope)
}

/// <https://nodejs.org/api/fs.html#fspromiseswatchfilename-options>: an async iterator over the
/// changes of `filename`. The arguments are validated and watching starts on the first `next()`.
pub(super) fn promises_watch(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Object::new_plain(scope)?;
    state.set_property(scope, c"filename", args.get(0))?;
    state.set_property(scope, c"options", args.get(1))?;
    state.set_property(scope, c"queue", js::Array::new(scope, 0)?)?;
    state.set_property(scope, c"waiting", js::Array::new(scope, 0)?)?;
    let state_value = state.to_jsval_throwing(scope)?;
    let iterator = Object::new_plain(scope)?;
    iterator.set_property(
        scope,
        c"next",
        closure(scope, 0, iterator_next, &[state_value])?,
    )?;
    iterator.set_property(
        scope,
        c"return",
        closure(scope, 1, iterator_return, &[state_value])?,
    )?;
    iterator.set_property(
        scope,
        c"throw",
        closure(scope, 1, iterator_throw, &[state_value])?,
    )?;
    let itself = Function::new_callback(scope, c"[Symbol.asyncIterator]", 0, iterator_self, ())?;
    js::class::define_well_known_symbol_property(
        scope,
        iterator,
        js::native::SymbolCode::asyncIterator,
        *itself,
    )?;
    let tag = string(scope, "AsyncGenerator")?;
    js::class::define_well_known_symbol_property(
        scope,
        iterator,
        js::native::SymbolCode::toStringTag,
        *tag,
    )?;
    iterator.to_jsval_raw_throwing(scope)
}
