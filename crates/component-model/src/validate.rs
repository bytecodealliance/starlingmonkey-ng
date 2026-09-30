// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Checking that a JavaScript value lowers to a WIT type.
//!
//! [`check`] walks a value along its [`TypeShape`] and reports the first part
//! of it that does not lower, as a path from the value's root and the reason.
//! Scalars, strings, enums and flags go through the same `pop_*` conversions
//! lowering uses, on a scratch [`CallStack`], so the two cannot disagree about
//! them. Composites are walked here with the same shape rules lowering applies.
//!
//! Lowering a resource handle, a stream or a future moves it out of JavaScript,
//! so those are checked without lowering them: a resource value must be an
//! object, and a wrapper of an imported resource must be of the expected type
//! and still hold its handle. A wrapper the guest received as a `borrow` does
//! not lower to an `own`, and a wrapper used as an `own` must not appear
//! anywhere else in the value, or in the other arguments of the same import
//! call. A stream must be a `ReadableStream` that is not locked and appears
//! nowhere else, or an iterable object, which a copy holds as the
//! `ReadableStream` it is converted to. Futures accept any value. Both need an
//! active event loop, which drives their transfer.
//!
//! Imports check their arguments before lowering them, so a value the guest
//! passes that does not lower throws a `TypeError` the guest can catch.
//! Exports check a result only after lowering it failed, to name the part that
//! failed in the trap message.

use std::fmt::Write as _;

use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Map, Object};

use crate::resources::{
    BORROW_FIELD, DISPOSE_DEFERRED_FIELD, DISPOSE_FIELD, HANDLE_FIELD, LENDS_FIELD, TYPE_FIELD,
};
use crate::stack::CallStack;
use crate::value::{self, TypeShape};

/// Why a value does not lower: the path to the part that does not, and the
/// reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    /// The path from the checked value's root, such as `.items[2].name`. Empty
    /// for the root itself.
    pub path: String,
    /// What the part should have been and what it is, such as
    /// `expected a u8, got 310`.
    pub reason: String,
}

impl Mismatch {
    /// `what` followed by the path, if there is one, and the reason, such as
    /// ``argument 1 `.name`: expected a string, got 7``.
    pub fn describe(&self, what: &str) -> String {
        if self.path.is_empty() {
            format!("{what}: {}", self.reason)
        } else {
            format!("{what} `{}`: {}", self.path, self.reason)
        }
    }
}

/// Check that `value` lowers to `shape`, and return the first part of it that
/// does not.
///
/// Reading a record's fields and a variant's tag and payload runs any getters
/// they have, which lowering then runs again. A pending exception from such a
/// getter becomes the mismatch's reason and is cleared.
pub fn check(scope: &Scope<'_>, value: HandleValue<'_>, shape: &TypeShape) -> Result<(), Mismatch> {
    let mut walker = Walker::new(scope, Mode::Check);
    let mut path = String::new();
    walk(scope, value, shape, &mut path, &mut walker)?;
    walker.check_wrappers().map_err(|(_, mismatch)| mismatch)
}

/// Check that `value` lowers to `shape`, as [`check`] does, and return a copy of
/// it that lowers the same way without running any JavaScript.
///
/// Records, tuples, variants, nested options and results become plain objects
/// and arrays of copies of their parts, defined as own data properties. Lists
/// become arrays of copies of their elements, and a numeric list's typed array
/// a new typed array with the same elements. An iterable object passed as a
/// stream becomes the `ReadableStream` it is converted to. Scalars, strings,
/// enums, flags, resources, `ReadableStream`s and futures are the values
/// themselves.
///
/// Each part is read once, here, so a getter, a proxy trap or a change the
/// application makes to `value` afterwards has no effect on the copy.
pub fn snapshot<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    shape: &TypeShape,
) -> Result<HandleValue<'s>, Mismatch> {
    copy(scope, value, shape, Mode::Copy)
}

/// [`snapshot`], except that a typed array is the value itself rather than a
/// copy, so its elements are read when it is lowered.
pub fn snapshot_composites<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    shape: &TypeShape,
) -> Result<HandleValue<'s>, Mismatch> {
    copy(scope, value, shape, Mode::CopyComposites)
}

fn copy<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    shape: &TypeShape,
    mode: Mode,
) -> Result<HandleValue<'s>, Mismatch> {
    let mut walker = Walker::new(scope, mode);
    let copy = walker.copy(value, shape)?;
    walker.check_wrappers().map_err(|(_, mismatch)| mismatch)?;
    Ok(copy)
}

/// Check each of `values` against `shape`, as parts of one lowering (see
/// [`check`]): a resource wrapper used as an `own` must not appear in any other
/// of them either. Returns the index of the first that does not lower and why.
pub fn check_each<'v>(
    scope: &Scope<'_>,
    values: impl IntoIterator<Item = HandleValue<'v>>,
    shape: &TypeShape,
) -> Result<(), (usize, Mismatch)> {
    let mut walker = Walker::new(scope, Mode::Check);
    for (index, value) in values.into_iter().enumerate() {
        walker.argument = index;
        let mut path = String::new();
        walk(scope, value, shape, &mut path, &mut walker).map_err(|mismatch| (index, mismatch))?;
    }
    walker.check_wrappers()
}

/// Whether a value of `shape` can hold a wrapper of an imported resource as an
/// `own`, or a stream, which lowering moves out of JavaScript.
pub fn moves_handles(shape: &TypeShape) -> bool {
    match shape {
        TypeShape::OwnResource { imported, .. } => *imported,
        TypeShape::Stream(_) => true,
        TypeShape::Record(fields) => fields.iter().any(|(_, field)| moves_handles(field)),
        TypeShape::Tuple(elements) => elements.iter().any(moves_handles),
        TypeShape::Variant(cases) => cases
            .iter()
            .any(|(_, payload)| payload.as_ref().is_some_and(moves_handles)),
        TypeShape::Option(inner) | TypeShape::List(inner) => moves_handles(inner),
        TypeShape::Result(ok, err) => {
            ok.as_deref().is_some_and(moves_handles) || err.as_deref().is_some_and(moves_handles)
        }
        _ => false,
    }
}

/// [`snapshot_composites`] for each of an import's `args`, checked against the
/// parameter shape at the same index of `shapes`. On failure, returns the index
/// of the argument that does not lower and why.
///
/// A wrapper of an imported resource used as an `own` must not appear in any
/// other argument either.
pub fn snapshot_arguments<'s>(
    scope: &'s Scope<'_>,
    args: &[HandleValue<'_>],
    shapes: &[TypeShape],
) -> Result<Vec<HandleValue<'s>>, (usize, Mismatch)> {
    let mut walker = Walker::new(scope, Mode::CopyComposites);
    let mut copies = Vec::with_capacity(args.len());
    for (index, (arg, shape)) in args.iter().zip(shapes).enumerate() {
        walker.argument = index;
        copies.push(
            walker
                .copy(*arg, shape)
                .map_err(|mismatch| (index, mismatch))?,
        );
    }
    walker.check_wrappers()?;
    Ok(copies)
}

/// Whether [`walk`] only checks a value, or also returns a copy of it, and
/// whether that copy includes typed arrays.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Check,
    Copy,
    CopyComposites,
}

/// The state of a walk over the values one lowering lowers together.
struct Walker<'s, 'cx> {
    /// The scope the wrappers the walk visits are rooted in.
    scope: &'s Scope<'cx>,
    mode: Mode,
    /// The argument the walk is in, for [`snapshot_arguments`].
    argument: usize,
    /// Maps each imported resource wrapper visited to its index in `wrappers`,
    /// and each other object [`Walker::record_moved`] records to `undefined`. Created when the walk
    /// visits the first one.
    seen: Option<Map<'s>>,
    wrappers: Vec<Wrapper<'s>>,
}

/// An imported resource wrapper a walk visited, where it first visited it, and
/// whether that use was an `own`.
struct Wrapper<'s> {
    object: Object<'s>,
    argument: usize,
    path: String,
    own: bool,
}

impl<'s, 'cx> Walker<'s, 'cx> {
    fn new(scope: &'s Scope<'cx>, mode: Mode) -> Self {
        Walker {
            scope,
            mode,
            argument: 0,
            seen: None,
            wrappers: Vec::new(),
        }
    }

    /// Walk `value` along `shape` and return its rooted copy.
    fn copy(
        &mut self,
        value: HandleValue<'_>,
        shape: &TypeShape,
    ) -> Result<HandleValue<'s>, Mismatch> {
        let scope = self.scope;
        let mut path = String::new();
        let copy =
            walk(scope, value, shape, &mut path, self)?.expect("a copying walk returns a value");
        Ok(scope.root_value(copy))
    }

    /// The map of the objects visited, created on first use.
    fn seen(&mut self, path: &str) -> Result<&Map<'s>, Mismatch> {
        let scope = self.scope;
        match self.seen {
            Some(ref seen) => Ok(seen),
            None => Ok(self
                .seen
                .insert(Map::new(scope).map_err(|_| thrown(scope, path))?)),
        }
    }

    /// Record a use of `object`, a `ReadableStream` or a value an own adapter
    /// converts, at `path`. A `what` used more than once is a mismatch, since
    /// lowering it moves it out of JavaScript.
    fn record_moved(
        &mut self,
        object: &Object<'_>,
        path: &str,
        what: &str,
    ) -> Result<(), Mismatch> {
        let scope = self.scope;
        let key = scope.root_value(object.as_value());
        let seen = self.seen(path)?;
        if seen.has(scope, key).map_err(|_| thrown(scope, path))? {
            return Err(mismatch(
                path,
                format!("the {what} is used elsewhere in the same call"),
            ));
        }
        seen.insert(scope, key, HandleValue::undefined())
            .map_err(|_| thrown(scope, path))
    }

    /// Record a use of the imported resource wrapper `object` at `path`, which
    /// is an `own` if `own` is set. A wrapper used more than once with an `own`
    /// among its uses is a mismatch.
    fn record(&mut self, object: &Object<'_>, own: bool, path: &str) -> Result<(), Mismatch> {
        let scope = self.scope;
        let key = scope.root_value(object.as_value());
        let next = js::value::from_i32(self.wrappers.len() as i32);
        let seen = self.seen(path)?;
        let index = seen.lookup(scope, key).map_err(|_| thrown(scope, path))?;
        if index.is_int32() {
            let wrapper = &self.wrappers[index.to_int32() as usize];
            if own || wrapper.own {
                return Err(mismatch(
                    path,
                    "the resource is used elsewhere in the same call, and an `own` use must be \
                     its only use"
                        .to_string(),
                ));
            }
            return Ok(());
        }
        seen.insert(scope, key, scope.root_value(next))
            .map_err(|_| thrown(scope, path))?;
        let object = Object::from_value(scope, key.get()).expect("the key is an object");
        self.wrappers.push(Wrapper {
            object,
            argument: self.argument,
            path: path.to_string(),
            own,
        });
        Ok(())
    }

    /// Check that each wrapper [`Walker::record`] recorded still holds its handle,
    /// and that one used as an `own` is lent to no import call. A getter run later
    /// in the walk can dispose of a wrapper, or pass it to an `async` import.
    /// Returns the argument and the mismatch of the first that fails.
    fn check_wrappers(&self) -> Result<(), (usize, Mismatch)> {
        let scope = self.scope;
        for wrapper in &self.wrappers {
            let failed = |_| (wrapper.argument, thrown(scope, &wrapper.path));
            let handle = crate::resources::read_u32_field(scope, &wrapper.object, HANDLE_FIELD)
                .map_err(failed)?;
            if handle.is_none() {
                return Err((
                    wrapper.argument,
                    mismatch(&wrapper.path, DISPOSED.to_string()),
                ));
            }
            let lends = crate::resources::read_u32_field(scope, &wrapper.object, LENDS_FIELD)
                .map_err(failed)?;
            if wrapper.own && lends.is_some_and(|lends| lends > 0) {
                return Err((wrapper.argument, mismatch(&wrapper.path, LENT.to_string())));
            }
        }
        Ok(())
    }
}

/// The reason for a wrapper of an imported resource that lost its handle.
const DISPOSED: &str = "the resource was disposed of, or moved to the host";

/// The reason for an `own` use of a wrapper lent to an import call.
const LENT: &str = "the resource is lent to an import call that has not returned";

/// Check `value` against `shape`, and with [`Mode::Copy`] return its copy (see
/// [`snapshot`]). The copy is returned unrooted, for the caller to root or
/// store before anything can allocate.
fn walk(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    shape: &TypeShape,
    path: &mut String,
    walker: &mut Walker<'_, '_>,
) -> Result<Option<Value>, Mismatch> {
    let mode = walker.mode;
    // Scalars, resources, `ReadableStream`s and futures are copied as
    // themselves.
    let itself = || (mode != Mode::Check).then(|| value.get());
    match shape {
        TypeShape::Bool => scalar(scope, value, path, value::pop_bool).map(|()| itself()),
        TypeShape::U8 => scalar(scope, value, path, value::pop_u8).map(|()| itself()),
        TypeShape::U16 => scalar(scope, value, path, value::pop_u16).map(|()| itself()),
        TypeShape::U32 => scalar(scope, value, path, value::pop_u32).map(|()| itself()),
        TypeShape::U64 => scalar(scope, value, path, value::pop_u64).map(|()| itself()),
        TypeShape::S8 => scalar(scope, value, path, value::pop_s8).map(|()| itself()),
        TypeShape::S16 => scalar(scope, value, path, value::pop_s16).map(|()| itself()),
        TypeShape::S32 => scalar(scope, value, path, value::pop_s32).map(|()| itself()),
        TypeShape::S64 => scalar(scope, value, path, value::pop_s64).map(|()| itself()),
        TypeShape::F32 => scalar(scope, value, path, value::pop_f32).map(|()| itself()),
        TypeShape::F64 => scalar(scope, value, path, value::pop_f64).map(|()| itself()),
        TypeShape::Char => scalar(scope, value, path, value::pop_char).map(|()| itself()),
        TypeShape::String => scalar(scope, value, path, value::pop_string).map(|()| itself()),
        TypeShape::Enum(names) => {
            let count = names.len() as u32;
            scalar(scope, value, path, |s, sc| value::pop_enum(s, sc, count)).map(|()| itself())
        }
        TypeShape::Flags(names) => {
            let count = names.len() as u32;
            scalar(scope, value, path, |s, sc| value::pop_flags(s, sc, count)).map(|()| itself())
        }
        TypeShape::Record(fields) => {
            let obj = object(scope, value, path, "a record (object)")?;
            let copy = new_object(scope, mode, path)?;
            for (name, field) in fields {
                let inner = scope.inner_scope();
                let item = property(&inner, &obj, name, path)?;
                let item = nested(path, &format!(".{name}"), |path| {
                    walk(&inner, item, field, path, walker)
                })?;
                define(&inner, copy.as_ref(), name, item, path)?;
            }
            Ok(copy.map(|copy| copy.as_value()))
        }
        TypeShape::Tuple(elements) => {
            let obj = object(scope, value, path, "a tuple (array)")?;
            let copy = new_array(scope, mode, elements.len(), path)?;
            for (index, element) in elements.iter().enumerate() {
                let inner = scope.inner_scope();
                let item = obj
                    .get_element(&inner, index as u32)
                    .map_err(|_| thrown(&inner, path))?;
                let item = nested(path, &format!("[{index}]"), |path| {
                    walk(&inner, item, element, path, walker)
                })?;
                define_element(&inner, copy.as_ref(), index as u32, item, path)?;
            }
            Ok(copy.map(|copy| copy.as_value()))
        }
        TypeShape::Variant(cases) => {
            let obj = object(scope, value, path, "a variant (object with a `tag`)")?;
            let tag = tag(scope, &obj, path)?;
            let Some((_, payload)) = cases.iter().find(|(name, _)| *name == tag) else {
                let expected = cases
                    .iter()
                    .map(|(name, _)| format!("\"{name}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(mismatch(
                    path,
                    format!("the tag \"{tag}\" is none of the cases {expected}"),
                ));
            };
            tagged(scope, &obj, &tag, payload.as_ref(), path, walker)
        }
        TypeShape::Option(inner) => {
            if value.is_undefined() || value.is_null() {
                return Ok(itself());
            }
            if matches!(**inner, TypeShape::Option(_)) {
                let obj = object(scope, value, path, "a nested option (object with `val`)")?;
                let copy = new_object(scope, mode, path)?;
                let item = property(scope, &obj, "val", path)?;
                let item = nested(path, ".val", |path| walk(scope, item, inner, path, walker))?;
                define(scope, copy.as_ref(), "val", item, path)?;
                Ok(copy.map(|copy| copy.as_value()))
            } else {
                walk(scope, value, inner, path, walker)
            }
        }
        TypeShape::Result(ok, err) => {
            let obj = object(scope, value, path, "a result (object with a `tag`)")?;
            let tag = tag(scope, &obj, path)?;
            let payload = match tag.as_str() {
                "ok" => ok,
                "err" => err,
                other => {
                    return Err(mismatch(
                        path,
                        format!("the tag \"{other}\" is neither \"ok\" nor \"err\""),
                    ))
                }
            };
            tagged(scope, &obj, &tag, payload.as_deref(), path, walker)
        }
        TypeShape::List(element) => {
            if let Some((_, kind)) = element.numeric_layout() {
                if let Some(view) = Object::from_value(scope, value.get())
                    .ok()
                    .and_then(js::ArrayBufferView::from_object)
                {
                    let actual = view.view_kind();
                    let matches = actual == kind
                        || (kind == js::typedarray::ViewKind::Uint8
                            && actual == js::typedarray::ViewKind::Uint8Clamped);
                    if !matches {
                        return Err(mismatch(
                            path,
                            format!("expected {}, got {}", kind.name(), actual.name()),
                        ));
                    }
                    return match mode {
                        Mode::Check => Ok(None),
                        Mode::Copy => copy_numeric_list(scope, view, element, path).map(Some),
                        Mode::CopyComposites => Ok(itself()),
                    };
                }
            }
            let array = Object::from_value(scope, value.get())
                .ok()
                .and_then(|obj| obj.cast::<js::Array>().ok())
                .ok_or_else(|| {
                    mismatch(
                        path,
                        format!("expected a list (array), got {}", describe(scope, value)),
                    )
                })?;
            let length = array.length(scope).map_err(|_| thrown(scope, path))?;
            let copy = new_array(scope, mode, length as usize, path)?;
            for index in 0..length {
                let inner = scope.inner_scope();
                let item = array
                    .get_element(&inner, index)
                    .map_err(|_| thrown(&inner, path))?;
                let item = nested(path, &format!("[{index}]"), |path| {
                    walk(&inner, item, element, path, walker)
                })?;
                define_element(&inner, copy.as_ref(), index, item, path)?;
            }
            Ok(copy.map(|copy| copy.as_value()))
        }
        TypeShape::OwnResource { index, imported }
        | TypeShape::BorrowResource { index, imported } => {
            let obj = object(scope, value, path, "a resource (object)")?;
            let recorded = if *imported {
                crate::resources::read_u32_field(scope, &obj, TYPE_FIELD)
            } else {
                crate::resources::exported_type(scope, &obj)
            }
            .map_err(|_| thrown(scope, path))?;
            match recorded {
                Some(recorded) if recorded != *index => Err(mismatch(
                    path,
                    "expected a resource of another type".to_string(),
                )),
                // Only an imported resource's wrapper has a drop callback, and it
                // loses its handle once disposed of or moved to the host.
                Some(_)
                    if crate::resources::read_u32_field(scope, &obj, HANDLE_FIELD)
                        .map_err(|_| thrown(scope, path))?
                        .is_none()
                        && obj
                            .has_own_property(scope, DISPOSE_FIELD)
                            .map_err(|_| thrown(scope, path))? =>
                {
                    Err(mismatch(path, DISPOSED.to_string()))
                }
                // Only an imported resource's wrappers are recorded: an object
                // backing an exported resource gets a new handle per use.
                Some(_) if *imported => {
                    let own = matches!(shape, TypeShape::OwnResource { .. });
                    if obj
                        .has_own_property(scope, DISPOSE_DEFERRED_FIELD)
                        .map_err(|_| thrown(scope, path))?
                    {
                        return Err(mismatch(path, DISPOSED.to_string()));
                    }
                    if own
                        && crate::resources::read_u32_field(scope, &obj, LENDS_FIELD)
                            .map_err(|_| thrown(scope, path))?
                            .is_some_and(|lends| lends > 0)
                    {
                        return Err(mismatch(path, LENT.to_string()));
                    }
                    if own
                        && obj
                            .has_own_property(scope, BORROW_FIELD)
                            .map_err(|_| thrown(scope, path))?
                    {
                        return Err(mismatch(
                            path,
                            "expected an owned resource, got a borrowed one".to_string(),
                        ));
                    }
                    walker.record(&obj, own, path)?;
                    Ok(itself())
                }
                // A value an own adapter converts moves to the host too.
                None if *imported => {
                    let check = match shape {
                        TypeShape::OwnResource { .. } => adapter_check(*index),
                        _ => None,
                    };
                    match check.map(|check| check(scope, value)) {
                        Some(Ok(true)) => {
                            walker.record_moved(&obj, path, "value")?;
                            Ok(itself())
                        }
                        Some(Err(reason)) => Err(mismatch(path, reason)),
                        Some(Ok(false)) | None => Err(mismatch(
                            path,
                            format!(
                                "expected an instance of the imported resource's class, got {}",
                                describe(scope, value)
                            ),
                        )),
                    }
                }
                _ => Ok(itself()),
            }
        }
        TypeShape::Stream(_) | TypeShape::Future(_) if !has_event_loop() => {
            let what = match shape {
                TypeShape::Stream(_) => "passing a stream",
                _ => "passing a future",
            };
            core_runtime::event_loop::throw_no_event_loop(scope, what);
            let reason = js::error::ExnThrown::capture(scope)
                .message
                .unwrap_or_else(|| format!("{what} needs an active event loop"));
            Err(mismatch(path, reason))
        }
        TypeShape::Stream(_) => {
            let obj = object(scope, value, path, "a ReadableStream or an iterable object")?;
            if let Ok(stream) = web_streams::readable::ReadableStream::from_jsval(scope, value, ())
            {
                if stream.is_locked() {
                    return Err(mismatch(path, "the stream is locked".to_string()));
                }
                walker.record_moved(&obj, path, "stream")?;
                return Ok(itself());
            }
            match web_streams::readable::ReadableStream::from_iterable(scope, value) {
                Ok(stream) => Ok((mode != Mode::Check).then(|| stream.as_value())),
                Err(ExnThrown) => {
                    let reason = js::error::ExnThrown::capture(scope)
                        .message
                        .unwrap_or_else(|| "the value is not iterable".to_string());
                    Err(mismatch(
                        path,
                        format!(
                            "expected a ReadableStream or an iterable object, got {}: {reason}",
                            describe(scope, value)
                        ),
                    ))
                }
            }
        }
        TypeShape::Future(_) | TypeShape::Unsupported(_) => Ok(itself()),
    }
}

/// Whether an event loop is active to drive the transfer of a stream or future.
fn has_event_loop() -> bool {
    core_runtime::event_loop::with_active_event_loop(|_| ()).is_some()
}

/// The check of the own adapter that converts values to the imported resource
/// type `index`, if one is registered.
fn adapter_check(index: u32) -> Option<crate::resources::OwnAdapterCheck> {
    #[cfg(target_arch = "wasm32")]
    return crate::interpreter::own_adapter_check(index);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = index;
        None
    }
}

/// Walk the payload of a variant or result `obj` whose tag is `tag`: its `val`,
/// if `payload` has a type. With [`Mode::Copy`], return a `{ tag, val }` copy.
fn tagged(
    scope: &Scope<'_>,
    obj: &Object<'_>,
    tag: &str,
    payload: Option<&TypeShape>,
    path: &mut String,
    walker: &mut Walker<'_, '_>,
) -> Result<Option<Value>, Mismatch> {
    let copy = new_object(scope, walker.mode, path)?;
    if let Some(copy) = &copy {
        let tag = js::JSString::from_str(scope, tag).map_err(|_| thrown(scope, path))?;
        copy.define_property(scope, c"tag", tag.as_value(), ENUMERATE)
            .map_err(|_| thrown(scope, path))?;
    }
    if let Some(payload) = payload {
        let item = property(scope, obj, "val", path)?;
        let item = nested(path, ".val", |path| {
            walk(scope, item, payload, path, walker)
        })?;
        define(scope, copy.as_ref(), "val", item, path)?;
    }
    Ok(copy.map(|copy| copy.as_value()))
}

/// The attributes of a copy's properties, those of a property an object literal
/// creates.
const ENUMERATE: std::ffi::c_uint = js::class_spec::JSPROP_ENUMERATE as std::ffi::c_uint;

/// A new plain object for a copy, with [`Mode::Copy`].
fn new_object<'s>(
    scope: &'s Scope<'_>,
    mode: Mode,
    path: &str,
) -> Result<Option<Object<'s>>, Mismatch> {
    match mode {
        Mode::Check => Ok(None),
        Mode::Copy | Mode::CopyComposites => Object::new_plain(scope)
            .map(Some)
            .map_err(|_| thrown(scope, path)),
    }
}

/// A new array of `length` elements for a copy, with [`Mode::Copy`].
fn new_array<'s>(
    scope: &'s Scope<'_>,
    mode: Mode,
    length: usize,
    path: &str,
) -> Result<Option<Object<'s>>, Mismatch> {
    match mode {
        Mode::Check => Ok(None),
        Mode::Copy | Mode::CopyComposites => js::Array::new(scope, length)
            .ok()
            .and_then(|array| Object::from_value(scope, array.as_value()).ok())
            .map(Some)
            .ok_or_else(|| thrown(scope, path)),
    }
}

/// Define `copy`'s property `name` as `item`, when both are present.
fn define(
    scope: &Scope<'_>,
    copy: Option<&Object<'_>>,
    name: &str,
    item: Option<Value>,
    path: &str,
) -> Result<(), Mismatch> {
    let (Some(copy), Some(item)) = (copy, item) else {
        return Ok(());
    };
    let item = scope.root_value(item);
    copy.define_property(scope, value::to_cstring(name).as_c_str(), item, ENUMERATE)
        .map_err(|_| thrown(scope, path))
}

/// Define `copy`'s element `index` as `item`, when both are present.
fn define_element(
    scope: &Scope<'_>,
    copy: Option<&Object<'_>>,
    index: u32,
    item: Option<Value>,
    path: &str,
) -> Result<(), Mismatch> {
    let (Some(copy), Some(item)) = (copy, item) else {
        return Ok(());
    };
    let item = scope.root_value(item);
    copy.define_element(scope, index, item, ENUMERATE)
        .map_err(|_| thrown(scope, path))
}

/// A new typed array of `element`'s kind holding a copy of `view`'s elements.
fn copy_numeric_list(
    scope: &Scope<'_>,
    view: js::ArrayBufferView<'_>,
    element: &TypeShape,
    path: &str,
) -> Result<Value, Mismatch> {
    let bytes = view.copy_bytes();
    let (size, _) = element
        .numeric_layout()
        .expect("only a numeric list is copied as a typed array");
    // `push_numeric_list` reads the elements as a slice of their type, so they
    // are copied into a buffer aligned for the widest one.
    let mut aligned = vec![0u64; bytes.len().div_ceil(8)];
    // SAFETY: `aligned` holds at least `bytes.len()` bytes, and the two buffers
    // do not overlap.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            aligned.as_mut_ptr().cast::<u8>(),
            bytes.len(),
        )
    };
    let mut stack = CallStack::new();
    // SAFETY: `aligned` holds `bytes.len() / size` elements of `element`'s type,
    // at an alignment of 8.
    unsafe {
        value::push_numeric_list(
            &mut stack,
            scope,
            element,
            aligned.as_ptr().cast::<u8>(),
            bytes.len() / size,
        )
    }
    .map_err(|_| thrown(scope, path))?;
    Ok(stack.pop_value())
}

/// Run `f` with `segment` appended to `path`, and remove it again afterwards.
fn nested<T>(path: &mut String, segment: &str, f: impl FnOnce(&mut String) -> T) -> T {
    let len = path.len();
    path.push_str(segment);
    let result = f(path);
    path.truncate(len);
    result
}

/// Check a value with a scalar `pop_*` conversion, on a scratch stack.
fn scalar<T>(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    path: &str,
    pop: impl FnOnce(&mut CallStack, &Scope<'_>) -> Result<T, ExnThrown>,
) -> Result<(), Mismatch> {
    let mut stack = CallStack::new();
    stack.push_value(value.get());
    match pop(&mut stack, scope) {
        Ok(_) => Ok(()),
        Err(ExnThrown) => {
            let reason = js::error::ExnThrown::capture(scope)
                .message
                .unwrap_or_else(|| "the value does not lower".to_string());
            Err(mismatch(
                path,
                format!("{reason}, got {}", describe(scope, value)),
            ))
        }
    }
}

/// `value` as an object, or a mismatch expecting `expected`.
fn object<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    path: &str,
    expected: &str,
) -> Result<Object<'s>, Mismatch> {
    Object::from_value(scope, value.get()).map_err(|_| {
        mismatch(
            path,
            format!("expected {expected}, got {}", describe(scope, value)),
        )
    })
}

/// The property `name` of `obj`.
fn property<'s>(
    scope: &'s Scope<'_>,
    obj: &Object<'_>,
    name: &str,
    path: &str,
) -> Result<HandleValue<'s>, Mismatch> {
    obj.get_property(scope, value::to_cstring(name).as_c_str())
        .map_err(|_| thrown(scope, path))
}

/// The `tag` property of `obj`, which must be a string.
fn tag(scope: &Scope<'_>, obj: &Object<'_>, path: &str) -> Result<String, Mismatch> {
    let tag = property(scope, obj, "tag", path)?;
    if !tag.is_string() {
        return Err(mismatch(
            path,
            format!("expected a string `tag`, got {}", describe(scope, tag)),
        ));
    }
    String::from_jsval_throwing(scope, tag, ()).map_err(|_| thrown(scope, path))
}

/// A mismatch at `path` for the exception a getter or conversion left pending,
/// which is cleared.
fn thrown(scope: &Scope<'_>, path: &str) -> Mismatch {
    let message = js::error::ExnThrown::capture(scope)
        .message
        .unwrap_or_else(|| "reading the value threw".to_string());
    mismatch(path, format!("reading the value threw: {message}"))
}

fn mismatch(path: &str, reason: String) -> Mismatch {
    Mismatch {
        path: path.to_string(),
        reason,
    }
}

/// A short description of `value` for a mismatch's reason, which runs no JS: a
/// number, BigInt, boolean or short string as a literal, and anything else by
/// its kind.
pub fn describe(scope: &Scope<'_>, value: HandleValue<'_>) -> String {
    const MAX_STRING: usize = 40;
    if value.is_undefined() {
        "undefined".to_string()
    } else if value.is_null() {
        "null".to_string()
    } else if value.is_boolean() {
        value.to_boolean().to_string()
    } else if value.is_int32() {
        value.to_int32().to_string()
    } else if value.is_double() {
        let n = value.to_double();
        if n.fract() == 0.0 && n.is_finite() && n.abs() < 1e21 {
            format!("{n:.0}")
        } else {
            n.to_string()
        }
    } else if value.is_string() {
        match String::from_jsval(scope, value, ()) {
            Ok(s) if s.chars().count() <= MAX_STRING => format!("{s:?}"),
            Ok(s) => {
                let mut short: String = s.chars().take(MAX_STRING).collect();
                short.push('…');
                format!("{short:?}")
            }
            Err(_) => "a string".to_string(),
        }
    } else if value.is_bigint() {
        match js::bigint::to_bigint(scope, value).ok().and_then(|bi| {
            js::bigint::fits_i64(bi)
                .map(|n| n.to_string())
                .or_else(|| js::bigint::fits_u64(bi).map(|n| n.to_string()))
        }) {
            Some(n) => format!("{n}n"),
            None => "a BigInt".to_string(),
        }
    } else if value.is_symbol() {
        "a symbol".to_string()
    } else {
        let mut kind = String::from("an object");
        if let Ok(obj) = Object::from_value(scope, value.get()) {
            if obj.cast::<js::Array>().is_ok() {
                kind = "an array".to_string();
            } else if let Some(view) = js::ArrayBufferView::from_object(obj) {
                kind.clear();
                let _ = write!(kind, "{}", view.view_kind().name());
            } else if obj.cast::<js::Function>().is_ok() {
                kind = "a function".to_string();
            } else if obj.cast::<js::Promise>().is_ok() {
                kind = "a Promise".to_string();
            }
        }
        kind
    }
}
