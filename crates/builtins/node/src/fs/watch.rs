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
use std::collections::VecDeque;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::{Heap, OptionHeapExt};
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
use crate::support::{next_tick_call, Props};

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

/// The poll state of a watcher.
#[core_runtime::jsclass(hidden)]
pub struct FSEvent {
    /// The function that polls report changes to, which the `onchange` property mirrors.
    onchange: Heap<Value>,
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
    /// The callback of the poll timers, created for the first poll.
    poll_callback: Option<Heap<js::object::Object>>,
}

#[core_runtime::jsmethods]
impl FSEvent {}

/// The state of an `FSWatcher` or a `StatWatcher`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct WatcherState {
    /// The handle, which `_handle` mirrors, until the watcher is closed or stopped.
    handle: Option<Heap<FSEventImpl>>,
    /// The ignore matcher of an `FSWatcher`, which `_ignoreMatcher` mirrors.
    ignore_matcher: Option<Heap<Value>>,
}

#[core_runtime::jsmethods]
impl WatcherState {}

/// A new handle that polls nothing yet and reports changes to `onchange`.
fn new_handle<'s>(
    scope: &'s Scope<'_>,
    onchange: HandleValue<'_>,
) -> Result<FSEvent<'s>, ExnThrown> {
    let handle = js::class::create_instance_with::<FSEventImpl>(scope, |_| FSEventImpl {
        onchange: Heap::from(onchange.get()),
        poll: None,
        timer: None,
        referenced: true,
        closed: false,
        ref_count: 1,
        max_ref_count: 1,
        old_status: -1,
        poll_callback: None,
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

/// The handle of the watcher `watcher`, if it has one.
fn handle_of<'s>(
    scope: &'s Scope<'_>,
    watcher: Object<'s>,
) -> Result<Option<FSEvent<'s>>, ExnThrown> {
    Ok(fs_watcher_class::try_state_of(scope, watcher)?
        .and_then(|state| state.data().handle.get(scope)))
}

/// Set the handle of the watcher `watcher`, and its `_handle` mirror.
fn set_handle(
    scope: &Scope<'_>,
    watcher: Object<'_>,
    handle: Option<FSEvent<'_>>,
) -> Result<(), ExnThrown> {
    fs_watcher_class::state_of(scope, watcher)?
        .data_mut()
        .handle = handle.map(Heap::from);
    match handle {
        Some(handle) => watcher.set_property(scope, c"_handle", handle),
        None => watcher.set_property(scope, c"_handle", js::value::null()),
    }
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
    let cached = handle.data().poll_callback.get(scope);
    let callback = match cached {
        Some(callback) => callback,
        None => {
            let callback = *Function::new_callback(scope, c"", 0, poll, *handle)?;
            handle.data_mut().poll_callback = Some(Heap::from(callback));
            callback
        }
    };
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

/// The body of a poll timer, whose payload is the handle: poll, queue the next poll, and pass
/// what changed to the handle's `onchange`, until it is closed. A stat event with status
/// -1 that follows one with status -1 isn't passed if its two stats have the same link count, as
/// Node's `onchange` ignores it.
fn poll(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Some(handle) = as_handle(scope, payload) else {
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
    let onchange = handle.data().onchange.get(scope);
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
                    &[
                        status,
                        js::conversion::ToJSVal::to_jsval_throwing(change.event, scope)?,
                        filename,
                    ],
                )?;
            }
        }
        Some(Found::Stat(Some((status, current, previous)))) => {
            let old_status = handle.data().old_status;
            if old_status == -1 && status == -1 && current.nlink == previous.nlink {
                return Ok(js::value::undefined());
            }
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
        js::exception::throw_value(scope, error);
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
    js::exception::throw_value(scope, error);
    code
}

// ============================================================================
// Options
// ============================================================================

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
    let elements = match Object::from_value(scope, *ignore) {
        Ok(array) if js::Array::is_array(scope, array.handle())? => {
            let length = array.num(scope, c"length")? as u32;
            (0..length)
                .map(|index| array.get_element(scope, index))
                .collect::<Result<Vec<_>, _>>()?
        }
        _ => vec![ignore],
    };
    let options = GlobOptions {
        nocase: platform_nocase(),
        nocase_magic_only: false,
        match_base: true,
    };
    let matcher =
        js::class::create_instance_with::<IgnoreMatcherImpl>(scope, |_| IgnoreMatcherImpl {
            patterns: Vec::new(),
        })?;
    for element in elements {
        if element.is_string() {
            let pattern = String::from_jsval(scope, element, ()).map_err(|e| e.throw(scope))?;
            matcher
                .data_mut()
                .patterns
                .push(IgnorePattern::Glob(Glob::new(scope, &pattern, options)?));
        } else if js::function::is_callable(element) {
            matcher
                .data_mut()
                .patterns
                .push(IgnorePattern::Function(Heap::from(element.get())));
        } else {
            matcher
                .data_mut()
                .patterns
                .push(IgnorePattern::RegExp(Heap::from(element.get())));
        }
    }
    Function::new_callback(scope, c"", 1, ignore_matches, matcher)?.to_jsval_throwing(scope)
}

/// One pattern of an `ignore` option.
#[js::must_root]
#[derive(core_runtime::Traceable)]
enum IgnorePattern {
    /// A glob pattern, compiled as minimatch compiles it with `matchBase`.
    Glob(Glob),
    /// A `RegExp`, which matches if `RegExp.prototype.exec` returns non-`null`.
    RegExp(Heap<Value>),
    /// A function, which matches if it returns a truthy value.
    Function(Heap<Value>),
}

/// The payload of an ignore matcher: the patterns of its `ignore` option, in order.
#[core_runtime::jsclass(hidden)]
pub(crate) struct IgnoreMatcher {
    patterns: Vec<IgnorePattern>,
}

#[core_runtime::jsmethods]
impl IgnoreMatcher {}

/// The body of an ignore matcher, whose payload is an [`IgnoreMatcher`]: whether the filename
/// argument matches one of its patterns, tried in order.
fn ignore_matches(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let matcher = cast_payload::<IgnoreMatcher>(scope, payload);
    let filename = args.get(0);
    let count = matcher.data().patterns.len();
    for index in 0..count {
        let function = {
            let data = matcher.data();
            match &data.patterns[index] {
                IgnorePattern::Glob(_) => None,
                IgnorePattern::RegExp(regexp) => Some((regexp.get(scope), true)),
                IgnorePattern::Function(function) => Some((function.get(scope), false)),
            }
        };
        let hit = match function {
            None => {
                let text = js::JSString::from_value(scope, filename)?.to_utf8(scope)?;
                let data = matcher.data();
                match &data.patterns[index] {
                    IgnorePattern::Glob(glob) => glob.matches(scope, &text)?,
                    _ => false,
                }
            }
            Some((regexp, true)) => crate::inspect::regexp_exec_matches(scope, regexp, filename)?,
            Some((function, false)) => js::conversion::to_boolean(Function::call(
                scope,
                HandleValue::undefined(),
                function,
                &[filename],
            )?),
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
    let bound = Function::call_by_name(
        scope,
        emit.handle(),
        c"bind",
        &[emitter.to_jsval_throwing(scope)?],
    )?;
    next_tick_call(
        scope,
        bound,
        &[js::conversion::ToJSVal::to_jsval_throwing(event, scope)?],
    )
}

// ============================================================================
// FSWatcher
// ============================================================================

/// The `onchange` of an `FSWatcher`'s handle, whose payload is the watcher: emit `change` with
/// the event type and filename, unless the watcher's `_ignoreMatcher` matches the filename.
fn watcher_onchange(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let watcher = cast_payload::<Object>(scope, payload);
    let filename = args.get(2);
    let matcher = fs_watcher_class::try_state_of(scope, watcher)?.and_then(|state| {
        state
            .data()
            .ignore_matcher
            .as_ref()
            .map(|matcher| matcher.get(scope))
    });
    if let Some(matcher) = matcher
        .filter(|matcher| !filename.is_null_or_undefined() && js::function::is_callable(*matcher))
    {
        let ignored = Function::call(
            scope,
            watcher.to_jsval_throwing(scope)?,
            matcher,
            &[filename],
        )?;
        if js::conversion::to_boolean(ignored) {
            return Ok(js::value::undefined());
        }
    }
    crate::events::emit(scope, watcher, c"change", &[args.get(1), filename])?;
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
    fs_watcher_class::state_of(scope, watcher)?
        .data_mut()
        .ignore_matcher = Some(Heap::from(matcher.get()));
    watcher.set_property(scope, c"_ignoreMatcher", matcher)?;
    let started = start_tree(
        scope,
        &handle,
        &path,
        &path,
        crate::support::is_true(persistent),
        crate::support::is_true(recursive),
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

#[core_runtime::jsfunction_class(
    name = "FSWatcher",
    extends = "crate::events::event_emitter",
    state = "WatcherState"
)]
pub mod fs_watcher_class {
    use super::*;

    /// Node's `FSWatcher` of `internal/fs/watchers`: an emitter with an unstarted handle.
    #[constructor]
    pub fn init(scope: &Scope<'_>, this: Object<'_>) -> Result<(), Thrown> {
        crate::events::event_emitter::init(scope, this, None)?;
        let onchange = Function::new_callback(scope, c"", 3, watcher_onchange, this)?;
        let handle = new_handle(scope, onchange.to_jsval_throwing(scope)?)?;
        set_handle(scope, this, Some(handle))?;
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
        set_handle(scope, this, None)?;
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

/// The `onchange` of a recursive watcher's handle, whose payload is the watcher: emit `change`,
/// unless the watcher's ignore matcher matches the filename or a directory above it.
fn recursive_onchange(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let watcher = cast_payload::<RecursiveWatcher>(scope, payload);
    let filename = args.get(2);
    let matcher = watcher.data().ignore.get(scope);
    if filename.is_string() && js::function::is_callable(matcher) {
        let name = String::from_jsval(scope, filename, ()).map_err(|e| e.throw(scope))?;
        let separator = std::path::MAIN_SEPARATOR;
        let mut prefixes: Vec<&str> = name
            .match_indices(separator)
            .map(|(index, _)| &name[..index])
            .collect();
        if !name.is_empty() {
            prefixes.push(&name);
        }
        let this = watcher.as_object().to_jsval_throwing(scope)?;
        for prefix in prefixes {
            let prefix = js::conversion::ToJSVal::to_jsval_throwing(prefix, scope)?;
            if js::conversion::to_boolean(Function::call(scope, this, matcher, &[prefix])?) {
                return Ok(js::value::undefined());
            }
        }
    }
    crate::events::emit(
        scope,
        watcher.as_object(),
        c"change",
        &[args.get(1), filename],
    )?;
    Ok(js::value::undefined())
}

/// Node's Linux `FSWatcher[kFSWatchStart]`: start watching the resolved `filename`. With
/// `throwIfNoEntry` false, a missing file throws, and other errors leave the watcher unstarted,
/// as Node.js's implementation does.
fn start_recursive(
    scope: &Scope<'_>,
    watcher: RecursiveWatcher<'_>,
    filename: HandleValue<'_>,
) -> Result<(), Thrown> {
    let path = crate::path::resolve_paths(&[path_arg(scope, filename, "path")?]);
    let (handle, persistent, throw_if_no_entry) = {
        let mut data = watcher.data_mut();
        data.closed = false;
        (
            data.handle.get(scope),
            data.persistent,
            data.throw_if_no_entry,
        )
    };
    match start_tree(
        scope,
        &handle,
        &path,
        &path,
        persistent,
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

/// The payload of the `change` and `abort` listeners of a pending `next()` of a recursive
/// watcher's async iterator: the promise `next()` returned, the watcher, and the watcher's signal
/// and the `abort` listener, if it has a signal.
#[core_runtime::jsclass(hidden)]
pub(crate) struct RecursiveNext {
    promise: Heap<js::object::Object>,
    watcher: Heap<RecursiveWatcherImpl>,
    signal: Option<Heap<js::object::Object>>,
    on_abort: Option<Heap<js::object::Object>>,
}

#[core_runtime::jsmethods]
impl RecursiveNext {}

/// The promise of `next`.
fn next_promise<'s>(
    scope: &'s Scope<'_>,
    next: RecursiveNext<'_>,
) -> Result<js::Promise<'s>, ExnThrown> {
    let promise = next.data().promise.get(scope);
    promise
        .cast::<js::Promise>()
        .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))
}

/// The `change` listener of a pending `next()` of a recursive watcher's async iterator, whose
/// payload is a [`RecursiveNext`]: fulfill the promise with the change.
fn recursive_next_change(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let next = cast_payload::<RecursiveNext>(scope, payload);
    let promise = next_promise(scope, next)?;
    let (signal, on_abort) = {
        let data = next.data();
        (data.signal.get(scope), data.on_abort.get(scope))
    };
    if let Some(signal) = signal {
        let on_abort = match on_abort {
            Some(on_abort) => on_abort.to_jsval_throwing(scope)?,
            None => scope.root_value(js::value::undefined()),
        };
        Function::call_by_name(
            scope,
            signal.handle(),
            c"removeEventListener",
            &[js::class::get_or_init_atom(scope, c"abort")?, on_abort],
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
/// payload is a [`RecursiveNext`]: close the watcher, and reject the promise with an
/// `AbortError`.
fn recursive_next_abort(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let next = cast_payload::<RecursiveNext>(scope, payload);
    let promise = next_promise(scope, next)?;
    let (watcher, signal) = {
        let data = next.data();
        (data.watcher.get(scope), data.signal.get(scope))
    };
    let signal = signal.ok_or_else(|| js::error::TypeError("no signal".into()).throw(scope))?;
    watcher.close(scope)?;
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

/// `next()` of a recursive watcher's async iterator, whose payload is the watcher: a done result
/// once the watcher is closed, and otherwise a promise for the next change.
fn recursive_next(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let watcher = cast_payload::<RecursiveWatcher>(scope, payload);
    if watcher.data().closed {
        return done_result(scope);
    }
    let promise = js::Promise::new_pending(scope)?;
    let signal = watcher.data().signal.get(scope);
    let signal_object = Object::from_value(scope, *signal).ok();
    let next =
        js::class::create_instance_with::<RecursiveNextImpl>(scope, |_| RecursiveNextImpl {
            promise: Heap::from(*promise),
            watcher: Heap::from(watcher),
            signal: signal_object.map(Heap::from),
            on_abort: None,
        })?;
    if let Some(signal_object) = signal_object {
        let abort = Function::new_callback(scope, c"", 0, recursive_next_abort, next)?;
        if signal_object.flag(scope, c"aborted")? {
            Function::call(
                scope,
                HandleValue::undefined(),
                abort.to_jsval_throwing(scope)?,
                js::function::EmptyArgs,
            )?;
            return promise.to_jsval_raw_throwing(scope);
        }
        next.data_mut().on_abort.set(*abort);
        let options = Object::new_plain(scope)?;
        options.set_property(scope, c"once", true)?;
        Function::call_by_name(
            scope,
            signal_object.handle(),
            c"addEventListener",
            &[
                js::class::get_or_init_atom(scope, c"abort")?,
                abort.to_jsval_throwing(scope)?,
                options.to_jsval_throwing(scope)?,
            ],
        )?;
    }
    let listener = Function::new_callback(scope, c"", 2, recursive_next_change, next)?;
    crate::events::listen(scope, watcher.as_object(), c"once", c"change", *listener)?;
    promise.to_jsval_raw_throwing(scope)
}

/// `return()` of a recursive watcher's async iterator, whose payload is the watcher: close the
/// watcher, and return a done result.
fn recursive_return(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let watcher = cast_payload::<RecursiveWatcher>(scope, payload);
    watcher.close(scope)?;
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
    let this = crate::support::this_object(scope, &args)?;
    let watcher = this
        .cast::<RecursiveWatcher>()
        .map_err(|_| NodeError::invalid_this("FSWatcher").throw(scope))?;
    let iterator = Object::new_plain(scope)?;
    let next = Function::new_callback(scope, c"", 0, recursive_next, watcher)?;
    iterator.set_property(scope, c"next", *next)?;
    let finish = Function::new_callback(scope, c"", 0, recursive_return, watcher)?;
    iterator.set_property(scope, c"return", *finish)?;
    let itself = Function::new_callback(scope, c"[Symbol.asyncIterator]", 0, iterator_self, ())?;
    js::class::define_well_known_symbol_property(
        scope,
        iterator,
        js::native::SymbolCode::asyncIterator,
        *itself,
    )?;
    iterator.to_jsval_raw_throwing(scope)
}

/// Node's `FSWatcher` of `internal/fs/recursive_watch`: an `EventEmitter` with the poll handle,
/// the options and whether it is closed.
#[core_runtime::jsclass(
    name = "FSWatcher",
    hidden,
    proto_parent = "crate::events::event_emitter"
)]
pub struct RecursiveWatcher {
    handle: Heap<FSEventImpl>,
    /// The `ignore` matcher.
    ignore: Heap<Value>,
    signal: Heap<Value>,
    #[no_trace]
    persistent: bool,
    #[no_trace]
    throw_if_no_entry: bool,
    #[no_trace]
    closed: bool,
}

#[core_runtime::jsmethods(node_args)]
impl RecursiveWatcher {
    /// A watcher with `options` validated as Node validates them.
    #[constructor]
    pub fn new(&self, scope: &Scope<'_>, options: Option<HandleValue<'_>>) -> Result<(), Thrown> {
        let this = self.as_object();
        crate::events::event_emitter::init(scope, this, None)?;
        let options = options.filter(|options| !options.is_undefined());
        let get =
            |name| option(scope, options, name).map(|value| value.filter(|value| !value.is_null()));
        if let Some(recursive) = get(c"recursive")? {
            crate::errors::validate_boolean(scope, recursive, "options.recursive")?;
        }
        let persistent = match get(c"persistent")? {
            Some(value) => crate::errors::validate_boolean(scope, value, "options.persistent")?,
            None => false,
        };
        let signal = get(c"signal")?;
        if let Some(signal) = signal {
            validate_abort_signal(scope, signal, "options.signal")?;
        }
        let throw_if_no_entry = match get(c"throwIfNoEntry")? {
            Some(value) => crate::errors::validate_boolean(scope, value, "options.throwIfNoEntry")?,
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
        let onchange = Function::new_callback(scope, c"", 3, recursive_onchange, *self)?;
        let handle = new_handle(scope, onchange.to_jsval_throwing(scope)?)?;
        let mut data = self.data_mut();
        data.handle.set(handle);
        data.ignore.set(matcher.get());
        data.signal
            .set(signal.map_or(js::value::undefined(), |signal| signal.get()));
        data.persistent = persistent;
        data.throw_if_no_entry = throw_if_no_entry;
        Ok(())
    }

    /// <https://nodejs.org/api/fs.html#watcherclose>: stop watching, and emit `close`. Closing a
    /// closed watcher does nothing.
    #[method]
    pub fn close(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let handle = {
            let mut data = self.data_mut();
            if data.closed {
                return Ok(());
            }
            data.closed = true;
            data.handle.get(scope)
        };
        close_handle(&handle);
        crate::events::emit(scope, self.as_object(), c"close", &[])?;
        Ok(())
    }

    /// Does nothing, as on Linux in Node.js.
    #[method(name = "ref")]
    pub fn ref_impl(&self) {}

    /// Does nothing, as on Linux in Node.js.
    #[method]
    pub fn unref(&self) {}
}

/// Register the recursive `FSWatcher`, and define its `[Symbol.asyncIterator]`.
pub(super) fn register_recursive_watcher(
    scope: &Scope<'_>,
    global: Object<'_>,
) -> Result<(), ExnThrown> {
    RecursiveWatcher::add_to_global(scope, global);
    let proto = js::class::get_prototype_object_for::<RecursiveWatcherImpl>(scope)
        .expect("FSWatcher was registered above");
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

// ============================================================================
// fs.watch
// ============================================================================

/// The body of an `abort` listener or tick callback, whose payload is the watcher: close the
/// watcher.
fn close_watcher(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let watcher = cast_payload::<Object>(scope, payload);
    Function::call_by_name(scope, watcher.handle(), c"close", js::function::EmptyArgs)?;
    Ok(js::value::undefined())
}

/// The payload of [`remove_abort_listener`]: the signal of a watcher, and the `abort` listener
/// added to it.
#[core_runtime::jsclass(hidden)]
pub(crate) struct AbortListener {
    signal: Heap<js::object::Object>,
    listener: Heap<js::object::Object>,
}

#[core_runtime::jsmethods]
impl AbortListener {}

/// The `close` listener of a watcher with a signal, whose payload is an [`AbortListener`]: remove
/// the `abort` listener.
fn remove_abort_listener(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let abort = cast_payload::<AbortListener>(scope, payload);
    let signal = abort.data().signal.get(scope);
    let listener = abort.data().listener.get(scope);
    Function::call_by_name(
        scope,
        signal.handle(),
        c"removeEventListener",
        &[
            js::class::get_or_init_atom(scope, c"abort")?,
            listener.to_jsval_throwing(scope)?,
        ],
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
    let (options, listener) = if js::function::is_callable(options) {
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
    let watcher = if js::conversion::to_boolean(recursive) && linux_recursive_platform() {
        let class = js::class::class_constructor::<RecursiveWatcherImpl>(scope)?;
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
        let recursive = watcher
            .cast::<RecursiveWatcher>()
            .map_err(|_| js::error::TypeError("not an FSWatcher".into()).throw(scope))?;
        start_recursive(scope, recursive, path_value)?;
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
            js::conversion::to_boolean(throw_if_no_entry),
        )?;
        watcher
    };
    if js::conversion::to_boolean(listener) {
        Function::call_by_name(
            scope,
            watcher.handle(),
            c"addListener",
            &[js::class::get_or_init_atom(scope, c"change")?, listener],
        )?;
    }
    if let Some(signal) = signal.and_then(|signal| Object::from_value(scope, *signal).ok()) {
        if signal.flag(scope, c"aborted")? {
            js::jobs::queue_native_tick(scope, close_watcher, watcher, &[])?;
        } else {
            let abort = Function::new_callback(scope, c"", 0, close_watcher, watcher)?;
            crate::events::listen(scope, signal, c"addEventListener", c"abort", *abort)?;
            let listener = js::class::create_instance_with::<AbortListenerImpl>(scope, |_| {
                AbortListenerImpl {
                    signal: Heap::from(signal),
                    listener: Heap::from(*abort),
                }
            })?;
            let remove = Function::new_callback(scope, c"", 0, remove_abort_listener, listener)?;
            crate::events::listen(scope, watcher, c"once", c"close", *remove)?;
        }
    }
    Ok(watcher)
}

// ============================================================================
// StatWatcher, fs.watchFile and fs.unwatchFile
// ============================================================================

/// The payload of [`stat_onchange`]: a `StatWatcher` and its handle.
#[core_runtime::jsclass(hidden)]
pub(crate) struct StatChange {
    watcher: Heap<js::object::Object>,
    handle: Heap<FSEventImpl>,
}

#[core_runtime::jsmethods]
impl StatChange {}

/// The `onchange` of a `StatWatcher`'s handle, whose payload is a [`StatChange`]: emit `change`
/// with the current and previous stats, as Node's `onchange` does.
fn stat_onchange(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let change = cast_payload::<StatChange>(scope, payload);
    let watcher = change.data().watcher.get(scope);
    let handle = change.data().handle.get(scope);
    let status = args.get(0);
    let current = args.get(1);
    let previous = args.get(2);
    if status.is_number() {
        handle.data_mut().old_status = status.to_number() as i32;
    }
    crate::events::emit(scope, watcher, c"change", &[current, previous])?;
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
    let change = js::class::create_instance_with::<StatChangeImpl>(scope, |_| StatChangeImpl {
        watcher: Heap::from(watcher),
        handle: Heap::from(handle),
    })?;
    let onchange = Function::new_callback(scope, c"", 3, stat_onchange, change)?;
    handle
        .data_mut()
        .onchange
        .set(onchange.to_jsval_throwing(scope)?.get());
    handle.set_property(scope, c"onchange", *onchange)?;
    set_handle(scope, watcher, Some(handle))?;
    if !js::conversion::to_boolean(persistent) {
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

#[core_runtime::jsfunction_class(
    name = "StatWatcher",
    extends = "crate::events::event_emitter",
    state = "WatcherState"
)]
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
        set_handle(scope, this, None)
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
    Function::call_by_name(
        scope,
        watcher.handle(),
        c"addListener",
        &[js::class::get_or_init_atom(scope, c"change")?, listener],
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
    let change = js::class::get_or_init_atom(scope, c"change")?;
    if js::function::is_callable(listener) {
        let before = crate::events::listener_count(scope, watcher, change)?;
        Function::call_by_name(
            scope,
            watcher.handle(),
            c"removeListener",
            &[change, listener],
        )?;
        if crate::events::listener_count(scope, watcher, change)? < before {
            add_or_clean_ref(scope, watcher, "clean")?;
        }
    } else {
        Function::call_by_name(scope, watcher.handle(), c"removeAllListeners", &[change])?;
        add_or_clean_ref(scope, watcher, "cleanAll")?;
    }
    if crate::events::listener_count(scope, watcher, change)? == 0.0 {
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

/// A change an `fsPromises.watch` iterator queued.
#[js::must_root]
#[derive(core_runtime::Traceable)]
struct QueuedChange {
    event_type: Heap<Value>,
    filename: Heap<Value>,
}

/// The state of an `fsPromises.watch` iterator.
#[core_runtime::jsclass(hidden)]
pub(crate) struct WatchIterator {
    filename: Heap<Value>,
    options: Heap<Value>,
    /// The changes no `next()` received yet.
    queue: VecDeque<QueuedChange>,
    /// The promises of the `next()` calls that wait for a change.
    waiting: VecDeque<Heap<js::promise::Promise>>,
    /// The poll handle, once watching started.
    handle: Option<Heap<FSEventImpl>>,
    #[no_trace]
    max_queue: f64,
    /// Whether a change beyond `max_queue` queued ones fails the iteration.
    #[no_trace]
    overflow_error: bool,
    signal: Heap<Value>,
    /// The `ignore` matcher.
    ignore: Heap<Value>,
    /// The `abort` listener added to `signal`.
    oncancel: Option<Heap<js::object::Object>>,
    /// The error that ends the iteration once the queued changes were received.
    error: Heap<Value>,
    #[no_trace]
    started: bool,
    #[no_trace]
    done: bool,
}

#[core_runtime::jsmethods]
impl WatchIterator {}

/// The `onchange` of the handle of an `fsPromises.watch` iterator, whose payload is its
/// [`WatchIterator`]: queue the change, or pass it to a waiting `next()`, unless the ignore
/// matcher matches the filename. Changes beyond `maxQueue` queued ones are dropped with a
/// warning, or replace the queue with an `ERR_FS_WATCH_QUEUE_OVERFLOW` error if `overflow` is
/// `'error'`.
fn iterator_onchange(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<WatchIterator>(scope, payload);
    let filename = args.get(2);
    let matcher = state.data().ignore.get(scope);
    if !filename.is_null_or_undefined() && js::function::is_callable(matcher) {
        let ignored = Function::call(scope, HandleValue::undefined(), matcher, &[filename])?;
        if js::conversion::to_boolean(ignored) {
            return Ok(js::value::undefined());
        }
    }
    let (length, max, overflow_error) = {
        let data = state.data();
        (data.queue.len() as f64, data.max_queue, data.overflow_error)
    };
    if length < max {
        state.data_mut().queue.push_back(QueuedChange {
            event_type: Heap::from(args.get(1).get()),
            filename: Heap::from(filename.get()),
        });
    } else if overflow_error {
        let error = queue_overflow(max).to_value(scope)?;
        let mut data = state.data_mut();
        data.queue.clear();
        data.error.set(error.get());
    } else {
        crate::process::emit_warning(scope, "fs.watch maxQueue exceeded", "Warning")?;
    }
    settle_waiting(scope, state)
}

/// Settle the waiting `next()` promises of the iterator `state` with what is queued: changes
/// first, then the error or the abort, which ends the iteration.
fn settle_waiting(scope: &Scope<'_>, state: WatchIterator<'_>) -> Result<Value, ExnThrown> {
    while !state.data().waiting.is_empty() {
        let Some(outcome) = next_outcome(scope, state)? else {
            break;
        };
        let promise = state
            .data_mut()
            .waiting
            .pop_front()
            .map(|promise| promise.get(scope));
        let Some(promise) = promise else {
            break;
        };
        match outcome {
            Ok(value) => promise.resolve(scope, value)?,
            Err(error) => promise.reject(scope, error)?,
        }
    }
    Ok(js::value::undefined())
}

/// What the next `next()` of the iterator `state` settles with, if it is known: a queued change,
/// a done result once the iteration ended, or the error or abort that ends it. Ending the
/// iteration closes the handle.
fn next_outcome<'s>(
    scope: &'s Scope<'_>,
    state: WatchIterator<'_>,
) -> Result<Option<Result<HandleValue<'s>, HandleValue<'s>>>, ExnThrown> {
    if state.data().done {
        let result = js::iteration::create_iter_result(scope, js::value::undefined(), true)?;
        return Ok(Some(Ok(result.to_jsval_throwing(scope)?)));
    }
    let change = state
        .data_mut()
        .queue
        .pop_front()
        .map(|change| (change.event_type.get(scope), change.filename.get(scope)));
    if let Some((event_type, filename)) = change {
        let item = Object::new_plain(scope)?;
        item.set_prototype(scope, js::native::HandleObject::null())?;
        item.set_property(scope, c"eventType", event_type)?;
        item.set_property(scope, c"filename", filename)?;
        let result = js::iteration::create_iter_result(scope, item, false)?;
        return Ok(Some(Ok(result.to_jsval_throwing(scope)?)));
    }
    let error = state.data().error.get(scope);
    if !error.is_undefined() {
        finish_iterator(scope, state)?;
        return Ok(Some(Err(error)));
    }
    let signal = state.data().signal.get(scope);
    if let Ok(signal) = Object::from_value(scope, *signal) {
        if signal.flag(scope, c"aborted")? {
            let reason = signal.val(scope, c"reason")?;
            finish_iterator(scope, state)?;
            return Ok(Some(Err(abort_error(scope, Some(reason))?)));
        }
    }
    Ok(None)
}

/// End the iteration of the iterator `state`: close its handle, and remove its `abort` listener,
/// as the generator's `finally` block does.
fn finish_iterator(scope: &Scope<'_>, state: WatchIterator<'_>) -> Result<(), ExnThrown> {
    let (handle, signal, oncancel) = {
        let mut data = state.data_mut();
        data.done = true;
        let handle = data.handle.as_ref().map(|handle| handle.get(scope));
        let oncancel = data.oncancel.as_ref().map(|oncancel| oncancel.get(scope));
        (handle, data.signal.get(scope), oncancel)
    };
    if let Some(handle) = handle {
        close_handle(&handle);
    }
    if let (Ok(signal), Some(oncancel)) = (Object::from_value(scope, *signal), oncancel) {
        Function::call_by_name(
            scope,
            signal.handle(),
            c"removeEventListener",
            &[
                js::class::get_or_init_atom(scope, c"abort")?,
                oncancel.to_jsval_throwing(scope)?,
            ],
        )?;
    }
    Ok(())
}

/// The `abort` listener of an `fsPromises.watch` iterator, whose payload is its
/// [`WatchIterator`]: stop watching, and settle the waiting `next()` promises.
fn iterator_cancel(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<WatchIterator>(scope, payload);
    let handle = state.data().handle.as_ref().map(|handle| handle.get(scope));
    if let Some(handle) = handle {
        close_handle(&handle);
    }
    settle_waiting(scope, state)
}

/// The body of `fsPromises.watch` up to its first `yield`: validate the arguments and start the
/// handle, as Node's async generator does on the first `next()`.
fn start_iterator(scope: &Scope<'_>, state: WatchIterator<'_>) -> Result<(), Thrown> {
    let filename = state.data().filename.get(scope);
    let path = path_arg(scope, filename, "path")?;
    let options = state.data().options.get(scope);
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
        Some(persistent) => {
            crate::errors::validate_boolean(scope, persistent, "options.persistent")?
        }
        None => true,
    };
    let recursive = match value(c"recursive")? {
        Some(recursive) => crate::errors::validate_boolean(scope, recursive, "options.recursive")?,
        None => false,
    };
    let max_queue = match value(c"maxQueue")? {
        Some(max_queue) => validate_integer(
            scope,
            max_queue,
            "options.maxQueue",
            -crate::errors::MAX_SAFE_INTEGER,
            crate::errors::MAX_SAFE_INTEGER,
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
        Some(encoding) if !js::conversion::to_boolean(encoding) => Some(Encoding::Utf8),
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
            js::exception::throw_value(scope, error);
            return Err(Thrown::Pending);
        }
    }
    let onchange = Function::new_callback(scope, c"", 3, iterator_onchange, state)?;
    let handle = new_handle(scope, onchange.to_jsval_throwing(scope)?)?;
    let matcher = create_ignore_matcher(scope, ignore)?;
    {
        let mut data = state.data_mut();
        data.handle = Some(Heap::from(handle));
        data.max_queue = max_queue;
        data.overflow_error = overflow_error;
        data.signal.set(signal.get());
        data.ignore.set(matcher.get());
    }
    if let Ok(signal) = Object::from_value(scope, *signal) {
        let oncancel = Function::new_callback(scope, c"", 0, iterator_cancel, state)?;
        state.data_mut().oncancel = Some(Heap::from(*oncancel));
        let options = Object::new_plain(scope)?;
        options.set_property(scope, c"once", true)?;
        Function::call_by_name(
            scope,
            signal.handle(),
            c"addEventListener",
            &[
                js::class::get_or_init_atom(scope, c"abort")?,
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
                js::exception::throw_value(scope, error);
            }
        }
        return Err(thrown);
    }
    Ok(())
}

/// `next()` of an `fsPromises.watch` iterator, whose payload is its [`WatchIterator`]: a promise
/// for the next change.
fn iterator_next(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<WatchIterator>(scope, payload);
    let start = {
        let mut data = state.data_mut();
        let start = !data.started && !data.done;
        data.started = true;
        start
    };
    if start {
        if let Err(thrown) = start_iterator(scope, state) {
            thrown.throw(scope);
            let error = js::exception::take_pending_or_undefined(scope);
            finish_iterator(scope, state)?;
            return js::Promise::new_rejected_with_error(scope, error)?
                .to_jsval_raw_throwing(scope);
        }
    }
    if state.data().waiting.is_empty() {
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
    state.data_mut().waiting.push_back(Heap::from(promise));
    promise.to_jsval_raw_throwing(scope)
}

/// `return(value)` of an `fsPromises.watch` iterator, whose payload is its [`WatchIterator`]:
/// end the iteration, and return a promise for a done result with `value`.
fn iterator_return(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<WatchIterator>(scope, payload);
    finish_iterator(scope, state)?;
    settle_waiting(scope, state)?;
    let result = js::iteration::create_iter_result(scope, args.get(0), true)?;
    js::Promise::new_resolved_with_value(scope, result)?.to_jsval_raw_throwing(scope)
}

/// `throw(error)` of an `fsPromises.watch` iterator, whose payload is its [`WatchIterator`]: end
/// the iteration, and return a promise rejected with `error`.
fn iterator_throw(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<WatchIterator>(scope, payload);
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
    let state =
        js::class::create_instance_with::<WatchIteratorImpl>(scope, |_| WatchIteratorImpl {
            filename: Heap::from(args.get(0).get()),
            options: Heap::from(args.get(1).get()),
            max_queue: 2048.0,
            ..Default::default()
        })?;
    let iterator = Object::new_plain(scope)?;
    let next = Function::new_callback(scope, c"", 0, iterator_next, state)?;
    iterator.set_property(scope, c"next", *next)?;
    let finish = Function::new_callback(scope, c"", 1, iterator_return, state)?;
    iterator.set_property(scope, c"return", *finish)?;
    let throw = Function::new_callback(scope, c"", 1, iterator_throw, state)?;
    iterator.set_property(scope, c"throw", *throw)?;
    let itself = Function::new_callback(scope, c"[Symbol.asyncIterator]", 0, iterator_self, ())?;
    js::class::define_well_known_symbol_property(
        scope,
        iterator,
        js::native::SymbolCode::asyncIterator,
        *itself,
    )?;
    let tag = js::class::get_or_init_atom(scope, c"AsyncGenerator")?;
    js::class::define_well_known_symbol_property(
        scope,
        iterator,
        js::native::SymbolCode::toStringTag,
        *tag,
    )?;
    iterator.to_jsval_raw_throwing(scope)
}
