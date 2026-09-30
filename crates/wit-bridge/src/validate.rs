// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Checking whether a JavaScript value can be lowered to a WIT type.
//!
//! [`check`] checks a value against a [`TypeShape`] and reports the first part
//! of it that can't be lowered, with the part's path and the reason. Booleans,
//! numbers, chars, strings, enums and flags are converted with the `pop_*`
//! functions used for lowering, on a separate [`CallStack`]. Other values are
//! checked with the same rules that lowering applies. [`snapshot`] also returns
//! the value converted to a [`TypeShapeValue`].
//!
//! Resource handles, streams and futures are checked without lowering them,
//! because lowering moves them out of JavaScript:
//!
//! - A resource must be an object. A wrapper of an imported resource must be of
//!   the expected type and still hold its handle. A wrapper received as a
//!   `borrow` can't be lowered as an `own`. A wrapper lowered as an `own` must
//!   not be lent to an import call that hasn't returned, and must not appear
//!   anywhere else in the value or in the other arguments of the same call. A
//!   value that isn't a wrapper is accepted as an `own` if an own adapter
//!   accepts it, and must not appear anywhere else either.
//! - A stream must be an unlocked `ReadableStream` that appears nowhere else,
//!   or an iterable object. A snapshot holds an iterable object as the
//!   `ReadableStream` created from it.
//! - A future can be any value.
//! - Streams and futures require an active event loop.
//!
//! Import calls and writes to streams and futures lower snapshots. An import
//! call throws a `TypeError` for an argument that can't be lowered. Export calls
//! use [`check`] after lowering a result failed, to report which part failed.

use std::ffi::CStr;
use std::fmt::{self, Write as _};
use std::ops::Range;

use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{Map, Object};

use crate::resources::ImportedResource;
use crate::stack::CallStack;
use crate::value::{self, NumericList, TypeShape, TypeShapeValue};

/// The reason a value can't be lowered, and the path to the part that can't
/// be.
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
    /// Formats `what` followed by the path, if there is one, and the reason, such
    /// as ``argument 1 `.name`: expected a string, got 7``.
    pub fn describe(&self, what: &str) -> String {
        if self.path.is_empty() {
            format!("{what}: {}", self.reason)
        } else {
            format!("{what} `{}`: {}", self.path, self.reason)
        }
    }
}

/// Check that `value` can be lowered to `shape`, and return the first part of
/// it that can't be.
///
/// Reading record fields and the tag and value of a variant runs their getters.
/// An exception thrown by a getter becomes the mismatch's reason and is cleared.
pub fn check(scope: &Scope<'_>, value: HandleValue<'_>, shape: &TypeShape) -> Result<(), Mismatch> {
    let mut walker = Walker::new(scope);
    walk(scope, value, shape, &Path::Root, &mut walker)?;
    walker.check_wrappers().map_err(|(_, mismatch)| mismatch)
}

/// Like [`check`], but also return `value` converted for lowering.
///
/// Scalars, strings, enums and flags are converted to Rust values, and a numeric
/// list's typed array to a copy of its elements. Records, tuples, variants,
/// options, results and other lists hold their converted parts. Resources,
/// `ReadableStream`s, futures and values of unsupported types are held as
/// [`TypeShapeValue::Js`]. An iterable object passed as a stream is held as the
/// `ReadableStream` it is converted to.
///
/// Each part of `value` is read once, so its getters and proxy traps run once,
/// and later changes to `value` don't affect the result.
pub fn snapshot(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    shape: &TypeShape,
) -> Result<TypeShapeValue, Mismatch> {
    let mut walker = Walker::new(scope);
    let converted = walk(scope, value, shape, &Path::Root, &mut walker)?;
    walker.check_wrappers().map_err(|(_, mismatch)| mismatch)?;
    Ok(converted)
}

/// Like [`snapshot`], for a value written to a `stream` or `future` whose
/// payload type is `shape`. If `shape` is `None`, `value` must be `undefined`,
/// and `None` is returned.
pub fn snapshot_payload(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    shape: Option<&TypeShape>,
) -> Result<Option<TypeShapeValue>, Mismatch> {
    match shape {
        Some(shape) => snapshot(scope, value, shape).map(Some),
        None if value.is_undefined() => Ok(None),
        None => Err(mismatch(
            &Path::Root,
            format!("expected undefined, got {}", describe(scope, value)),
        )),
    }
}

/// Check that `values`, values of `shape` returned by [`snapshot`], can still be
/// lowered together. Only their [`TypeShapeValue::Js`] parts are checked, with
/// the same rules as in [`check`]. A resource wrapper lowered as an `own` must
/// not appear in any other of the values. Returns the index of the first value
/// that can't be lowered, and the reason.
pub fn check_each<'v>(
    scope: &Scope<'_>,
    values: impl IntoIterator<Item = &'v TypeShapeValue>,
    shape: &TypeShape,
) -> Result<(), (usize, Mismatch)> {
    let mut walker = Walker::new(scope);
    for (index, value) in values.into_iter().enumerate() {
        walker.argument = index;
        check_parts(scope, value, shape, &Path::Root, &mut walker)
            .map_err(|mismatch| (index, mismatch))?;
    }
    walker.check_wrappers()
}

/// Whether a value of `shape` can contain an `own` handle of an imported
/// resource or a stream. Lowering the value moves them out of JavaScript.
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

/// Like [`snapshot`], for the arguments `args` of an import call, with
/// `shapes` holding the parameter types. A resource wrapper lowered as an
/// `own` must not appear in any other argument. On failure, returns the index
/// of the argument that can't be lowered, and the reason.
pub fn snapshot_arguments(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    shapes: &[TypeShape],
) -> Result<Vec<TypeShapeValue>, (usize, Mismatch)> {
    let mut walker = Walker::new(scope);
    let mut converted = Vec::with_capacity(args.len());
    for (index, (arg, shape)) in args.iter().zip(shapes).enumerate() {
        walker.argument = index;
        converted.push(
            walk(scope, *arg, shape, &Path::Root, &mut walker)
                .map_err(|mismatch| (index, mismatch))?,
        );
    }
    walker.check_wrappers()?;
    Ok(converted)
}

/// The state of a walk over the values of one lowering.
struct Walker<'s, 'cx> {
    /// The scope in which the wrappers visited by the walk are rooted.
    scope: &'s Scope<'cx>,
    /// The index of the argument being walked.
    argument: usize,
    /// Maps each imported resource wrapper visited to its index in `wrappers`, and each other
    /// object recorded by [`Walker::record_moved`] to `undefined`. Created on
    /// first use.
    seen: Option<Map<'s>>,
    wrappers: Vec<Wrapper<'s>>,
    /// The paths of the wrappers in `wrappers`, one after the other.
    paths: String,
    /// The stack on which the walk checks scalars.
    stack: CallStack,
}

/// An imported resource wrapper visited by a walk, the argument and path of its
/// first use, and whether that use was an `own`.
struct Wrapper<'s> {
    object: ImportedResource<'s>,
    argument: usize,
    /// The range of [`Walker::paths`] that holds the path.
    path: Range<usize>,
    own: bool,
}

impl<'s, 'cx> Walker<'s, 'cx> {
    fn new(scope: &'s Scope<'cx>) -> Self {
        Walker {
            scope,
            argument: 0,
            seen: None,
            wrappers: Vec::new(),
            paths: String::new(),
            stack: CallStack::new(),
        }
    }

    /// Convert `value` with the `pop_*` function `pop`, which roots values on
    /// `scope`.
    fn scalar<T>(
        &mut self,
        scope: &Scope<'_>,
        value: HandleValue<'_>,
        path: &Path<'_>,
        pop: impl FnOnce(&mut CallStack, &Scope<'_>) -> Result<T, ExnThrown>,
    ) -> Result<T, Mismatch> {
        self.stack.push_value(value.get());
        match pop(&mut self.stack, scope) {
            Ok(converted) => Ok(converted),
            Err(ExnThrown) => {
                let reason = ExnThrown::capture(scope)
                    .message
                    .unwrap_or_else(|| "the value does not lower".to_string());
                Err(mismatch(
                    path,
                    format!("{reason}, got {}", describe(scope, value)),
                ))
            }
        }
    }

    /// The map of the objects visited, created on first use.
    fn seen(&mut self, path: &Path<'_>) -> Result<&Map<'s>, Mismatch> {
        let scope = self.scope;
        match self.seen {
            Some(ref seen) => Ok(seen),
            None => Ok(self
                .seen
                .insert(Map::new(scope).map_err(|_| thrown(scope, path))?)),
        }
    }

    /// Record a use at `path` of `object`, a `ReadableStream` or a value
    /// converted by an own adapter. Reports a mismatch if `object` was already
    /// recorded. `what` names the kind of value in the mismatch.
    fn record_moved(
        &mut self,
        object: &Object<'_>,
        path: &Path<'_>,
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
    /// is an `own` if `own` is set. A mismatch is reported when a wrapper is used
    /// more than once and one of its uses is an `own`.
    fn record(
        &mut self,
        object: ImportedResource<'_>,
        own: bool,
        path: &Path<'_>,
    ) -> Result<(), Mismatch> {
        let scope = self.scope;
        let key = scope.root_value(object.as_value());
        let next = self.wrappers.len() as i32;
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
        seen.insert(scope, key, scope.root_value(js::value::from_i32(next)))
            .map_err(|_| thrown(scope, path))?;
        let object = crate::resources::imported_wrapper(scope, key).expect("the key is a wrapper");
        let start = self.paths.len();
        let _ = write!(self.paths, "{path}");
        self.wrappers.push(Wrapper {
            object,
            argument: self.argument,
            path: start..self.paths.len(),
            own,
        });
        Ok(())
    }

    /// Check that each wrapper recorded by [`Walker::record`] still holds its
    /// handle, and that no wrapper used as an `own` is lent to an import call. A
    /// getter that runs later in the walk can dispose of a wrapper or pass it to
    /// an async import. Returns the argument index and the mismatch of the first
    /// wrapper that fails.
    fn check_wrappers(&self) -> Result<(), (usize, Mismatch)> {
        for wrapper in &self.wrappers {
            let (handle, lends) = {
                let data = wrapper.object.data();
                (data.handle, data.lends)
            };
            let reason = if handle.is_none() {
                DISPOSED
            } else if wrapper.own && lends > 0 {
                LENT
            } else {
                continue;
            };
            let mismatch = Mismatch {
                path: self.paths[wrapper.path.clone()].to_string(),
                reason: reason.to_string(),
            };
            return Err((wrapper.argument, mismatch));
        }
        Ok(())
    }
}

/// The reason for a wrapper of an imported resource that lost its handle.
const DISPOSED: &str = "the resource was disposed of, or moved to the host";

/// The reason for an `own` use of a wrapper lent to an import call.
const LENT: &str = "the resource is lent to an import call that has not returned";

/// Check `value` against `shape`, and return it converted for lowering (see
/// [`snapshot`]).
fn walk(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    shape: &TypeShape,
    path: &Path<'_>,
    walker: &mut Walker<'_, '_>,
) -> Result<TypeShapeValue, Mismatch> {
    use TypeShapeValue as V;
    match shape {
        TypeShape::Bool => walker
            .scalar(scope, value, path, value::pop_bool)
            .map(V::Bool),
        TypeShape::U8 => walker.scalar(scope, value, path, value::pop_u8).map(V::U8),
        TypeShape::U16 => walker
            .scalar(scope, value, path, value::pop_u16)
            .map(V::U16),
        TypeShape::U32 => walker
            .scalar(scope, value, path, value::pop_u32)
            .map(V::U32),
        TypeShape::U64 => walker
            .scalar(scope, value, path, value::pop_u64)
            .map(V::U64),
        TypeShape::S8 => walker.scalar(scope, value, path, value::pop_s8).map(V::S8),
        TypeShape::S16 => walker
            .scalar(scope, value, path, value::pop_s16)
            .map(V::S16),
        TypeShape::S32 => walker
            .scalar(scope, value, path, value::pop_s32)
            .map(V::S32),
        TypeShape::S64 => walker
            .scalar(scope, value, path, value::pop_s64)
            .map(V::S64),
        TypeShape::F32 => walker
            .scalar(scope, value, path, value::pop_f32)
            .map(V::F32),
        TypeShape::F64 => walker
            .scalar(scope, value, path, value::pop_f64)
            .map(V::F64),
        TypeShape::Char => walker
            .scalar(scope, value, path, value::pop_char)
            .map(V::Char),
        TypeShape::String => walker
            .scalar(scope, value, path, value::pop_string)
            .map(V::String),
        TypeShape::Enum(names) => {
            let count = names.len() as u32;
            walker
                .scalar(scope, value, path, |s, sc| value::pop_enum(s, sc, count))
                .map(V::Enum)
        }
        TypeShape::Flags(names) => {
            let count = names.len() as u32;
            walker
                .scalar(scope, value, path, |s, sc| value::pop_flags(s, sc, count))
                .map(V::Flags)
        }
        TypeShape::Record(fields) => {
            let obj = object(scope, value, path, "a record (object)")?;
            let mut converted = Vec::with_capacity(fields.len());
            for (name, field) in fields {
                let inner = scope.inner_scope();
                let key = js::class::static_property_name(name);
                let item = property(&inner, &obj, key, path)?;
                converted.push(walk(&inner, item, field, &Path::Field(path, name), walker)?);
            }
            Ok(V::Record(converted))
        }
        TypeShape::Tuple(elements) => {
            let obj = object(scope, value, path, "a tuple (array)")?;
            let mut converted = Vec::with_capacity(elements.len());
            for (index, element) in elements.iter().enumerate() {
                let inner = scope.inner_scope();
                let item = obj
                    .get_element(&inner, index as u32)
                    .map_err(|_| thrown(&inner, path))?;
                let path = Path::Element(path, index as u32);
                converted.push(walk(&inner, item, element, &path, walker)?);
            }
            Ok(V::Tuple(converted))
        }
        TypeShape::Variant(cases) => {
            let obj = object(scope, value, path, "a variant (object with a `tag`)")?;
            let tag = tag(scope, &obj, path)?;
            let Some(case) = cases.iter().position(|(name, _)| *name == tag) else {
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
            let payload = payload(scope, &obj, cases[case].1.as_ref(), path, walker)?;
            Ok(V::Variant(case as u32, payload))
        }
        TypeShape::Option(inner) => {
            if value.is_undefined() || value.is_null() {
                return Ok(V::Option(None));
            }
            if matches!(**inner, TypeShape::Option(_)) {
                let obj = object(scope, value, path, "a nested option (object with `val`)")?;
                let item = property(scope, &obj, c"val", path)?;
                let converted = walk(scope, item, inner, &Path::Field(path, "val"), walker)?;
                Ok(V::Option(Some(Box::new(converted))))
            } else {
                let converted = walk(scope, value, inner, path, walker)?;
                Ok(V::Option(Some(Box::new(converted))))
            }
        }
        TypeShape::Result(ok, err) => {
            let obj = object(scope, value, path, "a result (object with a `tag`)")?;
            let tag = tag(scope, &obj, path)?;
            match tag.as_str() {
                "ok" => Ok(V::Result(Ok(payload(
                    scope,
                    &obj,
                    ok.as_deref(),
                    path,
                    walker,
                )?))),
                "err" => Ok(V::Result(Err(payload(
                    scope,
                    &obj,
                    err.as_deref(),
                    path,
                    walker,
                )?))),
                other => Err(mismatch(
                    path,
                    format!("the tag \"{other}\" is neither \"ok\" nor \"err\""),
                )),
            }
        }
        TypeShape::List(element) => {
            if let Some(kind) = element.view_kind() {
                if let Ok(view) = js::ArrayBufferView::from_jsval(scope, value, ()) {
                    let actual = view.view_kind();
                    if !element.accepts_view_kind(actual) {
                        return Err(mismatch(
                            path,
                            format!("expected {}, got {}", kind.name(), actual.name()),
                        ));
                    }
                    return NumericList::copy(scope, view, element)
                        .map(V::NumericList)
                        .map_err(|_| thrown(scope, path));
                }
            }
            let array = js::Array::from_jsval(scope, value, ()).map_err(|_| {
                mismatch(
                    path,
                    format!("expected a list (array), got {}", describe(scope, value)),
                )
            })?;
            let length = array.length(scope).map_err(|_| thrown(scope, path))?;
            let mut converted = Vec::new();
            converted.try_reserve_exact(length as usize).map_err(|_| {
                mismatch(
                    path,
                    format!("out of memory while reading a list of {length} elements"),
                )
            })?;
            for index in 0..length {
                let inner = scope.inner_scope();
                let item = array
                    .get_element(&inner, index)
                    .map_err(|_| thrown(&inner, path))?;
                let path = Path::Element(path, index);
                converted.push(walk(&inner, item, element, &path, walker)?);
            }
            Ok(V::List(converted))
        }
        TypeShape::OwnResource { .. }
        | TypeShape::BorrowResource { .. }
        | TypeShape::Stream(_)
        | TypeShape::Future(_)
        | TypeShape::Unsupported(_) => Ok(match leaf(scope, value, shape, path, walker)? {
            Some(converted) => V::js(converted),
            None => V::js(value),
        }),
    }
}

/// Check `value`, a value of the resource, stream, future or unsupported type
/// `shape`. Returns the `ReadableStream` that an iterable object passed as a
/// stream is converted to, and `None` for every other value, which is lowered
/// itself.
fn leaf<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    shape: &TypeShape,
    path: &Path<'_>,
    walker: &mut Walker<'_, '_>,
) -> Result<Option<HandleValue<'s>>, Mismatch> {
    match shape {
        TypeShape::OwnResource { index, imported }
        | TypeShape::BorrowResource { index, imported } => {
            let obj = object(scope, value, path, "a resource (object)")?;
            // An object backing an exported resource gets a new handle per use, so
            // any object lowers as one.
            if !*imported {
                return Ok(None);
            }
            match crate::resources::imported_wrapper(scope, value) {
                Some(wrapper) => {
                    let own = matches!(shape, TypeShape::OwnResource { .. });
                    let (type_idx, handle, borrowed, lends, dispose_deferred) = {
                        let data = wrapper.data();
                        (
                            data.type_idx,
                            data.handle,
                            data.borrowed,
                            data.lends,
                            data.dispose_deferred,
                        )
                    };
                    if type_idx != *index {
                        return Err(mismatch(
                            path,
                            "expected a resource of another type".to_string(),
                        ));
                    }
                    // A wrapper loses its handle once disposed of or moved to the
                    // host.
                    if handle.is_none() || dispose_deferred {
                        return Err(mismatch(path, DISPOSED.to_string()));
                    }
                    if own && lends > 0 {
                        return Err(mismatch(path, LENT.to_string()));
                    }
                    if own && borrowed {
                        return Err(mismatch(
                            path,
                            "expected an owned resource, got a borrowed one".to_string(),
                        ));
                    }
                    walker.record(wrapper, own, path)?;
                    Ok(None)
                }
                // A value converted by an own adapter is also moved to the host.
                None => {
                    let check = match shape {
                        TypeShape::OwnResource { .. } => adapter_check(*index),
                        _ => None,
                    };
                    match check.map(|check| check(scope, value)) {
                        Some(Ok(true)) => {
                            walker.record_moved(&obj, path, "value")?;
                            Ok(None)
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
            }
        }
        TypeShape::Stream(_) | TypeShape::Future(_)
            if !core_runtime::event_loop::has_active_event_loop() =>
        {
            let what = match shape {
                TypeShape::Stream(_) => "passing a stream",
                _ => "passing a future",
            };
            Err(mismatch(
                path,
                core_runtime::event_loop::no_event_loop_message(what),
            ))
        }
        TypeShape::Stream(_) => {
            let obj = object(scope, value, path, "a ReadableStream or an iterable object")?;
            if let Ok(stream) = web_streams::readable::ReadableStream::from_jsval(scope, value, ())
            {
                if stream.is_locked() {
                    return Err(mismatch(path, "the stream is locked".to_string()));
                }
                walker.record_moved(&obj, path, "stream")?;
                return Ok(None);
            }
            match web_streams::readable::ReadableStream::from_iterable(scope, value) {
                Ok(stream) => Ok(Some(scope.root_value(stream.as_value()))),
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
        _ => Ok(None),
    }
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

/// Walk the `val` of the variant or result `obj` if `payload` is `Some`, and
/// return it converted for lowering.
fn payload(
    scope: &Scope<'_>,
    obj: &Object<'_>,
    payload: Option<&TypeShape>,
    path: &Path<'_>,
    walker: &mut Walker<'_, '_>,
) -> Result<Option<Box<TypeShapeValue>>, Mismatch> {
    let Some(payload) = payload else {
        return Ok(None);
    };
    let item = property(scope, obj, c"val", path)?;
    let converted = walk(scope, item, payload, &Path::Field(path, "val"), walker)?;
    Ok(Some(Box::new(converted)))
}

/// Check the [`TypeShapeValue::Js`] parts of `value`, a value of `shape`
/// returned by [`walk`], with the same rules as [`walk`].
fn check_parts(
    scope: &Scope<'_>,
    value: &TypeShapeValue,
    shape: &TypeShape,
    path: &Path<'_>,
    walker: &mut Walker<'_, '_>,
) -> Result<(), Mismatch> {
    use TypeShapeValue as V;
    match (shape, value) {
        (_, V::Js(js_value)) => {
            let inner = scope.inner_scope();
            leaf(&inner, js_value.get(&inner), shape, path, walker).map(|_| ())
        }
        (TypeShape::Record(fields), V::Record(values)) => {
            for ((name, field), value) in fields.iter().zip(values) {
                check_parts(scope, value, field, &Path::Field(path, name), walker)?;
            }
            Ok(())
        }
        (TypeShape::Tuple(elements), V::Tuple(values)) => {
            for (index, (element, value)) in elements.iter().zip(values).enumerate() {
                check_parts(
                    scope,
                    value,
                    element,
                    &Path::Element(path, index as u32),
                    walker,
                )?;
            }
            Ok(())
        }
        (TypeShape::List(element), V::List(values)) => {
            for (index, value) in values.iter().enumerate() {
                check_parts(
                    scope,
                    value,
                    element,
                    &Path::Element(path, index as u32),
                    walker,
                )?;
            }
            Ok(())
        }
        (TypeShape::Variant(cases), V::Variant(case, Some(payload))) => {
            match cases
                .get(*case as usize)
                .and_then(|(_, shape)| shape.as_ref())
            {
                Some(shape) => {
                    check_parts(scope, payload, shape, &Path::Field(path, "val"), walker)
                }
                None => Ok(()),
            }
        }
        (TypeShape::Option(inner), V::Option(Some(payload))) => {
            if matches!(**inner, TypeShape::Option(_)) {
                check_parts(scope, payload, inner, &Path::Field(path, "val"), walker)
            } else {
                check_parts(scope, payload, inner, path, walker)
            }
        }
        (TypeShape::Result(ok, err), V::Result(result)) => {
            let (shape, payload) = match result {
                Ok(payload) => (ok, payload),
                Err(payload) => (err, payload),
            };
            match (shape.as_deref(), payload) {
                (Some(shape), Some(payload)) => {
                    check_parts(scope, payload, shape, &Path::Field(path, "val"), walker)
                }
                _ => Ok(()),
            }
        }
        _ => Ok(()),
    }
}

/// The path from a walked value's root to one of its parts, as a chain of
/// segments on the stack. It is formatted only when a mismatch is reported or
/// a wrapper is recorded.
#[derive(Clone, Copy)]
enum Path<'a> {
    /// The walked value itself, formatted as the empty string.
    Root,
    /// The property `name` of the part at the parent path, formatted as
    /// `.name`.
    Field(&'a Path<'a>, &'a str),
    /// The element `index` of the part at the parent path, formatted as
    /// `[index]`.
    Element(&'a Path<'a>, u32),
}

impl fmt::Display for Path<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Path::Root => Ok(()),
            Path::Field(parent, name) => write!(f, "{parent}.{name}"),
            Path::Element(parent, index) => write!(f, "{parent}[{index}]"),
        }
    }
}

/// Returns `value` as an object, or a mismatch expecting `expected` if it is not one.
fn object<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
    path: &Path<'_>,
    expected: &str,
) -> Result<Object<'s>, Mismatch> {
    Object::from_raw_value(scope, value.get()).map_err(|_| {
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
    name: &'static CStr,
    path: &Path<'_>,
) -> Result<HandleValue<'s>, Mismatch> {
    obj.get_property(scope, name)
        .map_err(|_| thrown(scope, path))
}

/// The `tag` property of `obj`, which must be a string.
fn tag(scope: &Scope<'_>, obj: &Object<'_>, path: &Path<'_>) -> Result<String, Mismatch> {
    let tag = property(scope, obj, c"tag", path)?;
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
fn thrown(scope: &Scope<'_>, path: &Path<'_>) -> Mismatch {
    let message = js::error::ExnThrown::capture(scope)
        .message
        .unwrap_or_else(|| "reading the value threw".to_string());
    mismatch(path, format!("reading the value threw: {message}"))
}

fn mismatch(path: &Path<'_>, reason: String) -> Mismatch {
    Mismatch {
        path: path.to_string(),
        reason,
    }
}

/// Return a short description of `value` for a mismatch's reason, without
/// running JS. Numbers, BigInts, booleans and the first 40 code units of a
/// string are shown as literals. Other values are described by their kind.
pub fn describe(scope: &Scope<'_>, value: HandleValue<'_>) -> String {
    const MAX_STRING: usize = 40;
    // `ToString` of a primitive runs no JS, and fails only if it cannot allocate.
    let to_string = |value: HandleValue<'_>| {
        String::from_jsval(scope, value, ()).map_err(|_| js::exception::clear(scope))
    };
    if value.is_undefined() {
        "undefined".to_string()
    } else if value.is_null() {
        "null".to_string()
    } else if value.is_boolean() || value.is_number() {
        to_string(value).unwrap_or_else(|()| "a number".to_string())
    } else if value.is_bigint() {
        match to_string(value) {
            Ok(n) => format!("{n}n"),
            Err(()) => "a BigInt".to_string(),
        }
    } else if value.is_string() {
        let short = js::JSString::from_value(scope, value).and_then(|string| {
            let truncated = string.len() > MAX_STRING;
            let string = if truncated {
                string.substring(scope, 0, MAX_STRING)?
            } else {
                string
            };
            Ok((string, truncated))
        });
        match short.map_err(|_| js::exception::clear(scope)) {
            Ok((string, truncated)) => match to_string(scope.root_value(string.as_value())) {
                Ok(text) if truncated => format!("{:?}", text + "…"),
                Ok(text) => format!("{text:?}"),
                Err(()) => "a string".to_string(),
            },
            Err(()) => "a string".to_string(),
        }
    } else if value.is_symbol() {
        "a symbol".to_string()
    } else {
        let mut kind = String::from("an object");
        if let Ok(obj) = Object::from_raw_value(scope, value.get()) {
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
