// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Resource handles: imported wrappers, the exported-resource slab, and
//! finalization.
//!
//! The Component Model has two kinds of resource a JS guest deals with:
//!
//! - Imported resources are owned by the host. The guest holds an opaque
//!   canonical handle, a `u32`, and JS sees a wrapper object holding the handle
//!   in a hidden field plus a drop callback. [`wrap_imported`] creates a wrapper
//!   from a handle on lift, and [`unwrap_to_canon`] reads the handle back out on
//!   lower.
//!
//! - Exported resources are owned by the guest. Their backing JS object lives in
//!   the [`ExportedResources`] slab, the slab index is the resource's rep, and
//!   the host refers to it through a canonical handle obtained from the
//!   component's generated `[resource-new]` import.
//!   [`exported_resource_to_canon`] lowers a JS object to a new handle, and
//!   each lowering of the same object mints another rep and handle. The slab is
//!   consulted on lift and emptied by [`ExportedResources::remove`], on the
//!   `resource_dtor` path.
//!
//! Which kind a resource type is, and the function pointers that create, resolve
//! and drop its canonical handles, are wit-dylib metadata that exists only on
//! wasm, so this module never names a wit-dylib type. The caller hands in a
//! [`ResourceDesc`] per operation instead, whose `new`, `rep` and `drop` are
//! `&dyn Fn` borrowed for the call.

use std::cell::RefCell;

use js::conversion::ToJSVal;
use js::error::{throw_type_error, ExnThrown};
use js::function::EmptyArgs;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::heap::Trace;
use js::native::JSTracer;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::stack::CallStack;

/// Hidden field holding the canonical resource handle (`u32`) on a wrapper.
pub const HANDLE_FIELD: &std::ffi::CStr = c"_componentizeJsHandle";
/// Hidden field holding the resource type index on a wrapper.
pub const TYPE_FIELD: &std::ffi::CStr = c"_componentizeJsType";
/// Hidden field holding the drop callback, a JS function, which the
/// finalization registry reads. The guest-facing way to run it is
/// `[Symbol.dispose]()`, installed on the wrapper's class prototype (see
/// [`define_dispose`]).
pub const DISPOSE_FIELD: &std::ffi::CStr = c"_componentizeJsDispose";
/// Hidden field present on a wrapper of a handle the guest received as a
/// `borrow`.
pub const BORROW_FIELD: &std::ffi::CStr = c"_componentizeJsBorrow";
/// Hidden field counting the import calls a wrapper is lent to that have not
/// returned. The handle cannot be dropped or moved while the count is not 0.
pub const LENDS_FIELD: &std::ffi::CStr = c"_componentizeJsLends";
/// Hidden field present on a wrapper disposed of while lent. The handle is
/// dropped once the last lend ends.
pub const DISPOSE_DEFERRED_FIELD: &std::ffi::CStr = c"_componentizeJsDisposeDeferred";

/// Converts a value lowered as an owned handle of an imported resource type into
/// such a handle, for a value that is not one of the type's wrappers. Returns
/// `None` for a value it does not convert, which is then lowered as a wrapper.
pub type OwnAdapter = fn(&Scope<'_>, HandleValue<'_>) -> Result<Option<u32>, ExnThrown>;

/// Checks whether an [`OwnAdapter`] converts a value, without converting it:
/// `Ok(true)` if it does, `Ok(false)` if the value is not of a kind it converts,
/// and `Err` with the reason if the value is of that kind but cannot be
/// converted. Validation runs it, so a value the adapter would refuse throws a
/// `TypeError` instead of trapping the lowering.
pub type OwnAdapterCheck = fn(&Scope<'_>, HandleValue<'_>) -> Result<bool, String>;

js::instance_local! {
    /// The adapters [`register_own_adapter`] registered, each with the interface
    /// name and the resource name it applies to.
    static OWN_ADAPTERS: RefCell<Vec<(String, String, OwnAdapter, OwnAdapterCheck)>> =
        const { RefCell::new(Vec::new()) };
}

/// Register `adapter`, and `check`, which must accept exactly the values
/// `adapter` converts, for the imported resource type `resource` of the
/// interface `interface`, such as `wasi:http/types@0.3.0`, and of every
/// interface [`same_interface`](crate::naming::same_interface) matches with it.
pub fn register_own_adapter(
    interface: &str,
    resource: &str,
    adapter: OwnAdapter,
    check: OwnAdapterCheck,
) {
    OWN_ADAPTERS.with(|cell| {
        cell.borrow_mut()
            .push((interface.to_string(), resource.to_string(), adapter, check))
    });
}

/// The adapter and its check registered for the imported resource type
/// `resource` of the interface `interface`, if there is one.
#[cfg(target_arch = "wasm32")]
pub(crate) fn own_adapter(
    interface: Option<&str>,
    resource: &str,
) -> Option<(OwnAdapter, OwnAdapterCheck)> {
    let interface = interface?;
    OWN_ADAPTERS.with(|cell| {
        cell.borrow()
            .iter()
            .find(|(registered, name, _, _)| {
                crate::naming::same_interface(registered, interface) && name == resource
            })
            .map(|(_, _, adapter, check)| (*adapter, *check))
    })
}

/// Whether `value` is an object carrying a resource type index, as every
/// resource wrapper does.
#[cfg(target_arch = "wasm32")]
pub(crate) fn is_resource_wrapper(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<bool, ExnThrown> {
    match Object::from_value(scope, value) {
        Ok(obj) => Ok(read_u32_field(scope, &obj, TYPE_FIELD)?.is_some()),
        Err(_) => Ok(false),
    }
}

/// Install `callback`, non-enumerable and
/// non-writable, so `using` and an explicit `[Symbol.dispose]()` call drop what
/// `obj` holds.
pub fn define_dispose<'s>(
    scope: &'s Scope<'_>,
    obj: &Object<'_>,
    callback: impl ToJSVal<'s>,
) -> Result<(), ExnThrown> {
    js::class::define_well_known_symbol_property(
        scope,
        *obj,
        js::native::SymbolCode::dispose,
        callback,
    )
}

/// `obj[Symbol.dispose]`, or `None` if it is not a function.
fn dispose_method<'s>(
    scope: &'s Scope<'_>,
    obj: &Object<'_>,
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    let key = scope.root_id(js::symbol::get_well_known_key(
        scope,
        js::native::SymbolCode::dispose,
    ));
    let value = obj.get_property_by_id(scope, key)?;
    Ok(value.get().is_object().then_some(value))
}

/// Attribute flags for a runtime-stamped resource field: non-enumerable, and
/// non-writable so a guest cannot forge it by assignment. Fields using this stay
/// configurable, so the ownership-transfer strip's `delete_property` keeps
/// working, at the cost of a guest being able to redefine them via
/// `Object.defineProperty`.
pub(crate) const HIDDEN_FIELD_ATTRS: std::ffi::c_uint =
    js::class_spec::JSPROP_READONLY as std::ffi::c_uint;
/// As [`HIDDEN_FIELD_ATTRS`], plus non-configurable. Used for fields the runtime
/// never deletes (`TYPE_FIELD`, `DISPOSE_FIELD`), making them fully unforgeable.
pub(crate) const HIDDEN_PERMANENT_FIELD_ATTRS: std::ffi::c_uint =
    (js::class_spec::JSPROP_READONLY | js::class_spec::JSPROP_PERMANENT) as std::ffi::c_uint;

/// A resource type, resolved by the caller from wit-dylib metadata (or built by
/// hand in native tests). `Imported` needs no operations, since its drop is the
/// JS function passed to [`wrap_imported`] and lowering only reads the handle
/// back.
pub enum ResourceDesc<'a> {
    /// A host-owned resource type, identified by its type index, stored on the
    /// wrapper for diagnostics.
    Imported { type_idx: u32 },
    /// A guest-owned resource type backed by the slab.
    Exported {
        /// The resource type index, stored on the slab-backed object.
        type_idx: u32,
        /// `[resource-new]`: create a canonical handle for a slab rep.
        new: &'a dyn Fn(u32) -> u32,
        /// `[resource-rep]`: resolve a canonical handle back to its slab rep.
        rep: &'a dyn Fn(u32) -> u32,
        /// `[resource-drop]`: release a canonical handle from the guest's table.
        drop: &'a dyn Fn(u32),
    },
}

js::instance_local! {
    /// The JS prototype object for each imported resource type, keyed by type
    /// index. Populated by [`crate::imports`] when it builds a resource class, so
    /// a new wrapper's `__proto__` is that class's `prototype`. Traced by
    /// [`trace_resources`].
    static RESOURCE_PROTOTYPES: RefCell<Vec<Option<Heap<js::object::Object>>>> =
        const { RefCell::new(Vec::new()) };
}

/// Record the prototype object for imported resource type `type_idx`, so wrappers
/// created for that type are re-parented to it. A later registration for the same
/// index replaces the previous prototype.
pub fn register_resource_prototype(type_idx: u32, proto: &Object<'_>) {
    RESOURCE_PROTOTYPES.with(|cell| {
        let mut protos = cell.borrow_mut();
        let idx = type_idx as usize;
        if protos.len() <= idx {
            protos.resize_with(idx + 1, || None);
        }
        protos[idx] = Some(Heap::from(*proto));
    });
}

/// Run `f` with the registered prototype for `type_idx`, if any. The prototype is
/// rooted on `scope` before `f` runs, so a moving GC during wrapping cannot
/// invalidate it.
fn with_resource_prototype<R>(
    scope: &Scope<'_>,
    type_idx: u32,
    f: impl FnOnce(Option<&Object<'_>>) -> R,
) -> R {
    let proto = RESOURCE_PROTOTYPES.with(|cell| {
        cell.borrow()
            .get(type_idx as usize)
            .and_then(Option::as_ref)
            .map(|heap| heap.get(scope))
    });
    f(proto.as_ref())
}

/// Lift a host-owned resource handle into its JS wrapper object.
///
/// `proto`, when given, becomes the wrapper's prototype, so the class's methods
/// resolve. `dispose_fn`, the JS function that drops the handle, is stored under
/// [`DISPOSE_FIELD`] and registered with the finalization registry, so the
/// resource is dropped if the wrapper is collected without an explicit drop.
pub fn wrap_imported<'s>(
    scope: &'s Scope<'_>,
    handle: u32,
    type_idx: u32,
    proto: Option<&Object<'_>>,
    dispose_fn: HandleValue<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let wrapper = Object::new_plain(scope)?;
    if let Some(proto) = proto {
        wrapper.set_prototype(scope, proto.handle())?;
    }

    wrapper.define_property(scope, TYPE_FIELD, type_idx, HIDDEN_PERMANENT_FIELD_ATTRS)?;

    wrapper.define_property(
        scope,
        DISPOSE_FIELD,
        dispose_fn,
        HIDDEN_PERMANENT_FIELD_ATTRS,
    )?;

    register_resource(scope, &wrapper, handle)?;

    Ok(wrapper)
}

/// Lower a JS wrapper of the imported resource type `type_idx` to its canonical
/// handle.
///
/// Reads [`HANDLE_FIELD`]. A wrapper of another resource type is rejected with a
/// TypeError. When `owned`, ownership transfers out of JS: the finalizer is
/// unregistered and the handle field stripped, so neither the finalizer nor a
/// later explicit drop double-drops the now-transferred handle. A wrapper the
/// guest received as a `borrow`, one lent to an import call that has not
/// returned, and one whose disposal waits for such a call cannot be owned, and
/// throw a TypeError too.
pub fn unwrap_to_canon(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    type_idx: u32,
    owned: bool,
) -> Result<u32, ExnThrown> {
    let obj = Object::from_value(scope, value)
        .map_err(|_| throw_type_error(scope, c"resource value is not an object"))?;
    if read_u32_field(scope, &obj, TYPE_FIELD)? != Some(type_idx) {
        return Err(throw_type_error(
            scope,
            c"resource value is not a wrapper of the expected resource type",
        ));
    }
    let handle = read_handle(scope, &obj)?;

    if owned {
        if obj.has_own_property(scope, BORROW_FIELD)? {
            return Err(throw_type_error(
                scope,
                c"a borrowed resource cannot be passed as an owned one",
            ));
        }
        if read_u32_field(scope, &obj, LENDS_FIELD)?.is_some_and(|lends| lends > 0)
            || obj.has_own_property(scope, DISPOSE_DEFERRED_FIELD)?
        {
            return Err(throw_type_error(
                scope,
                c"the resource is lent to an import call that has not returned",
            ));
        }
        unregister_resource(scope, &obj)?;
        obj.delete_property(scope, HANDLE_FIELD)?;
    }

    Ok(handle)
}

/// Release every borrow recorded on `stack` during the just-finished call.
///
/// An imported borrow is unregistered from finalization, its [`DISPOSE_FIELD`]
/// callback is invoked, and its [`HANDLE_FIELD`] is stripped, so a wrapper the
/// guest stashed cannot pass the released handle back to the host.
///
/// An exported borrow gives its slab entry back and its handle to
/// `drop_exported_handle`, which is called with the resource's type index and
/// the handle. The slab entry is removed first, so the `resource_dtor` the drop
/// triggers finds no rep and does nothing, leaving the guest's object and its
/// destructor hook alone.
///
/// Borrows are popped one at a time so the undrained ones stay traced through
/// any GC the drop triggers.
pub fn release_borrows(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    drop_exported_handle: &dyn Fn(u32, u32),
) -> Result<(), ExnThrown> {
    // Every borrow is drained even if one's unregister or dispose throws, so a
    // single throwing drop cannot leak the rest. The first failure's exception is
    // captured, cleared so the next dispose runs with none pending, and re-raised
    // once the list is drained.
    let mut first_exn: Option<HandleValue> = None;
    while let Some(borrow) = stack.pop_borrow() {
        let object = borrow.heap().get(scope);
        let result = match &borrow {
            crate::stack::Borrow::Imported { .. } => (|| -> Result<(), ExnThrown> {
                // The callback unregisters the wrapper and reads the handle off
                // `this`, so the field is stripped only after it has run.
                let dispose = object.get_property(scope, DISPOSE_FIELD)?;
                if dispose.get().is_object() {
                    Function::call(scope, object, dispose, EmptyArgs)?;
                } else {
                    unregister_resource(scope, &object)?;
                }
                object.delete_property(scope, HANDLE_FIELD)?;
                Ok(())
            })(),
            crate::stack::Borrow::Lent { .. } => (|| -> Result<(), ExnThrown> {
                let lends = read_u32_field(scope, &object, LENDS_FIELD)?
                    .unwrap_or(1)
                    .saturating_sub(1);
                object.define_property(scope, LENDS_FIELD, lends, HIDDEN_FIELD_ATTRS)?;
                // A dispose while the wrapper was lent waits for its last lend.
                if lends == 0 && object.has_own_property(scope, DISPOSE_DEFERRED_FIELD)? {
                    object.delete_property(scope, DISPOSE_DEFERRED_FIELD)?;
                    let dispose = object.get_property(scope, DISPOSE_FIELD)?;
                    Function::call(scope, object, dispose, EmptyArgs)?;
                }
                Ok(())
            })(),
            crate::stack::Borrow::Exported {
                type_idx,
                rep,
                handle,
                ..
            } => {
                with_exported_resources(|slab| slab.remove(scope, *rep));
                drop_exported_handle(*type_idx, *handle);
                Ok(())
            }
        };
        if result.is_err() {
            match (first_exn, js::exception::take_pending(scope)) {
                // The first catchable exception, kept rooted in `scope`.
                (None, Some(exn)) => first_exn = Some(exn),
                // A later failure, or an uncatchable one such as OOM.
                _ => js::exception::clear(scope),
            }
        }
    }
    if let Some(exn) = first_exn {
        return Err(js::exception::set_pending(
            scope,
            exn,
            js::native::ExceptionStackBehavior::Capture,
        ));
    }
    Ok(())
}

/// Read a `u32` field off `obj`, or `None` if the field does not hold a number.
pub(crate) fn read_u32_field(
    scope: &Scope<'_>,
    obj: &Object<'_>,
    name: &std::ffi::CStr,
) -> Result<Option<u32>, ExnThrown> {
    let v = obj.get_property(scope, name)?;
    // A `u32` past `i32::MAX` is stored as a double, so the field is read back
    // through the number path for the full `u32` range to round-trip.
    Ok(v.is_number().then(|| v.to_number() as u32))
}

/// Read the canonical handle off a wrapper, or `None` if the field is absent, as
/// on a wrapper whose handle was stripped when ownership left JS.
fn peek_handle(scope: &Scope<'_>, obj: &Object<'_>) -> Result<Option<u32>, ExnThrown> {
    read_u32_field(scope, obj, HANDLE_FIELD)
}

/// Read the canonical handle off a wrapper, erroring if it is missing, as on an
/// already-disposed wrapper whose handle was stripped.
fn read_handle(scope: &Scope<'_>, obj: &Object<'_>) -> Result<u32, ExnThrown> {
    peek_handle(scope, obj)?.ok_or_else(|| {
        throw_type_error(
            scope,
            c"resource wrapper has no live handle (already disposed?)",
        )
    })
}

/// Lift an owned resource handle onto the stack.
///
/// For an exported resource the handle is resolved to its slab rep via `rep`,
/// and removed from the slab so ownership returns to JS. For an imported
/// resource a fresh wrapper is created.
pub fn push_own(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    desc: &ResourceDesc<'_>,
    handle: u32,
    dispose_fn: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    match desc {
        ResourceDesc::Exported { rep, drop, .. } => {
            let slab_index = rep(handle);
            let obj = EXPORTED_RESOURCES
                .with(|slab| slab.borrow_mut().remove(scope, slab_index))
                .ok_or_else(|| {
                    throw_type_error(scope, c"exported resource rep is not in the slab")
                })?;
            stack.push_object(&obj);
            // The host's lower added this handle to the guest's table, and
            // nothing else releases it. The slab entry is already gone, so the
            // `resource_dtor` this triggers finds no rep and does nothing.
            drop(handle);
        }
        ResourceDesc::Imported { type_idx } => {
            let wrapper = with_resource_prototype(scope, *type_idx, |proto| {
                wrap_imported(scope, handle, *type_idx, proto, dispose_fn)
            })?;
            stack.push_object(&wrapper);
        }
    }
    Ok(())
}

/// Lift a borrowed resource handle onto the stack.
///
/// For an exported resource the handle is the slab rep directly, and the slab
/// object is looked up and pushed, with the host keeping ownership. For an
/// imported resource a fresh wrapper is created and recorded as a per-call borrow
/// so it is dropped at call end.
pub fn push_borrow(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    desc: &ResourceDesc<'_>,
    handle: u32,
    dispose_fn: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    match desc {
        ResourceDesc::Exported { .. } => {
            let obj = EXPORTED_RESOURCES
                .with(|slab| slab.borrow().lookup(handle).map(|h| h.get(scope)))
                .ok_or_else(|| {
                    throw_type_error(scope, c"borrowed exported resource is not in the slab")
                })?;
            stack.push_object(&obj);
        }
        ResourceDesc::Imported { type_idx } => {
            let wrapper = with_resource_prototype(scope, *type_idx, |proto| {
                wrap_imported(scope, handle, *type_idx, proto, dispose_fn)
            })?;
            wrapper.define_property(scope, BORROW_FIELD, true, HIDDEN_PERMANENT_FIELD_ATTRS)?;
            stack.push_imported_borrow(&wrapper);
            stack.push_object(&wrapper);
        }
    }
    Ok(())
}

/// Lower an owned resource wrapper to its canonical handle.
///
/// Ownership transfers out of JS in both directions. A guest-owned object gets
/// a handle minted for this lowering, which the host then owns. An imported
/// wrapper's [`HANDLE_FIELD`] is stripped, since the generated code removes the
/// handle from the guest's table right after this returns.
pub fn pop_own(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    desc: &ResourceDesc<'_>,
) -> Result<u32, ExnThrown> {
    let value = scope.root_value(stack.pop_value());
    match desc {
        ResourceDesc::Exported { .. } => Ok(lower_exported(scope, desc, value)?.handle),
        ResourceDesc::Imported { type_idx } => unwrap_to_canon(scope, value, *type_idx, true),
    }
}

/// Lower a borrowed resource wrapper to its canonical handle.
///
/// A guest-owned object gets a handle minted for this lowering, which pins it
/// in the slab. The borrow is recorded on `stack` so [`release_borrows`]
/// releases both at call end. An object lent to several calls at once has one
/// handle per call, so each call's end releases only its own. An imported
/// wrapper is recorded on `stack` too, which keeps it reachable until the call
/// ends.
pub fn pop_borrow(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    desc: &ResourceDesc<'_>,
) -> Result<u32, ExnThrown> {
    let value = scope.root_value(stack.pop_value());
    match desc {
        ResourceDesc::Exported { type_idx, .. } => {
            let lowered = lower_exported(scope, desc, value)?;
            stack.push_exported_borrow(&lowered.object, *type_idx, lowered.rep, lowered.handle);
            Ok(lowered.handle)
        }
        ResourceDesc::Imported { type_idx } => {
            let handle = unwrap_to_canon(scope, value, *type_idx, false)?;
            let wrapper =
                Object::from_value(scope, value.get()).expect("unwrap_to_canon accepted an object");
            let lends = read_u32_field(scope, &wrapper, LENDS_FIELD)?.unwrap_or(0);
            wrapper.define_property(scope, LENDS_FIELD, lends + 1, HIDDEN_FIELD_ATTRS)?;
            stack.push_lent_wrapper(&wrapper);
            Ok(handle)
        }
    }
}

/// A free-list-backed table of guest-owned resource objects, keyed by rep, which
/// is the slab index.
///
/// Stored objects are traced by [`trace_resources`], so a slab-held resource
/// survives GC while the host owns its handle.
#[js::allow_unrooted_interior]
#[derive(Default)]
pub struct ExportedResources {
    entries: Vec<Option<Heap<js::object::Object>>>,
    free: Vec<u32>,
}

impl ExportedResources {
    /// Insert an object, returning its rep (slab index).
    pub fn insert(&mut self, obj: &Object<'_>) -> u32 {
        if let Some(index) = self.free.pop() {
            self.entries[index as usize] = Some(Heap::from(*obj));
            index
        } else {
            let index = self.entries.len() as u32;
            self.entries.push(Some(Heap::from(*obj)));
            index
        }
    }

    /// Look up the object at `rep`, or `None` if the slot is free or out of
    /// range.
    pub fn lookup(&self, rep: u32) -> Option<&Heap<js::object::Object>> {
        self.entries.get(rep as usize).and_then(Option::as_ref)
    }

    /// Remove the object at `rep`, freeing the slot, and root it into `scope`.
    /// Returns `None` if the slot was already free or out of range.
    pub fn remove<'s>(&mut self, scope: &'s Scope<'_>, rep: u32) -> Option<Object<'s>> {
        let slot = self.entries.get_mut(rep as usize)?;
        if slot.is_some() {
            self.free.push(rep);
        }
        slot.take_rooted(scope)
    }
}

/// Lower an exported-resource JS object to a fresh canonical handle: the object
/// is inserted into the slab, and a handle is created from its rep via the
/// descriptor's `new`. Each lowering of the same object mints another rep and
/// handle.
///
/// `desc` must be a [`ResourceDesc::Exported`].
pub fn exported_resource_to_canon(
    scope: &Scope<'_>,
    desc: &ResourceDesc<'_>,
    value: HandleValue<'_>,
) -> Result<u32, ExnThrown> {
    lower_exported(scope, desc, value).map(|lowered| lowered.handle)
}

/// The outcome of one [`lower_exported`] call.
struct LoweredExport<'s> {
    /// The object that was lowered, rooted for the caller.
    object: Object<'s>,
    /// The canonical handle the host receives.
    handle: u32,
    /// The slab index the handle was minted from.
    rep: u32,
}

/// [`exported_resource_to_canon`], also reporting the rooted object and the rep.
fn lower_exported<'s>(
    scope: &'s Scope<'_>,
    desc: &ResourceDesc<'_>,
    value: HandleValue<'_>,
) -> Result<LoweredExport<'s>, ExnThrown> {
    let (type_idx, new) = match desc {
        ResourceDesc::Exported { type_idx, new, .. } => (*type_idx, *new),
        ResourceDesc::Imported { .. } => {
            return Err(throw_type_error(
                scope,
                c"exported_resource_to_canon called for an imported resource",
            ))
        }
    };

    let object = Object::from_value(scope, value.get())
        .map_err(|_| throw_type_error(scope, c"exported resource value is not an object"))?;

    // One JS object backs one resource type for its whole life.
    let recorded_type = exported_type(scope, &object)?;
    if recorded_type.is_some_and(|recorded| recorded != type_idx) {
        return Err(throw_type_error(
            scope,
            c"this object was already lowered as a different exported resource type",
        ));
    }

    let rep = EXPORTED_RESOURCES.with(|slab| slab.borrow_mut().insert(&object));
    let handle = new(rep);

    if recorded_type.is_none() {
        set_exported_type(scope, &object, type_idx)?;
    }

    Ok(LoweredExport {
        object,
        handle,
        rep,
    })
}

js::instance_local! {
    /// Process-global slab of guest-owned resource objects. Traced by
    /// [`trace_resources`].
    static EXPORTED_RESOURCES: RefCell<ExportedResources> =
        RefCell::new(ExportedResources::default());
}

/// Run `f` with a mutable borrow of the exported-resource slab.
pub fn with_exported_resources<R>(f: impl FnOnce(&mut ExportedResources) -> R) -> R {
    EXPORTED_RESOURCES.with(|slab| f(&mut slab.borrow_mut()))
}

/// Drop a guest-owned resource the host has released: remove its backing object
/// from the slab at `rep`, check the recorded type index in debug builds, and
/// run the guest's drop hook if it set one.
///
/// `expected_type_idx` is checked against the type index recorded for the slab
/// object when its handle was created. A mismatch means a rep collided across
/// resource types.
///
/// The object's `[Symbol.dispose]` method, if it has one, is called with the
/// removed object as `this`. Exported objects are not registered under
/// [`DISPOSE_FIELD`], so the imported-resource finalizer never races this path.
pub fn drop_exported(scope: &Scope<'_>, rep: u32, expected_type_idx: u32) -> Result<(), ExnThrown> {
    let Some(obj) = with_exported_resources(|slab| slab.remove(scope, rep)) else {
        // An unknown rep, from a double drop or a handle that never entered the
        // slab, is a no-op.
        return Ok(());
    };

    // The type index is validated before running any guest code.
    if let Some(type_idx) = exported_type(scope, &obj)? {
        debug_assert_eq!(
            type_idx, expected_type_idx,
            "resource_dtor type index mismatch"
        );
    }

    if let Some(dispose) = dispose_method(scope, &obj)? {
        Function::call(scope, obj, dispose, EmptyArgs)?;
    }
    Ok(())
}

js::instance_local! {
    /// The resource type index of each object backing an exported resource,
    /// keyed by the object. A side table rather than a property, so a frozen or
    /// sealed object can back a resource too. Traced by [`trace_resources`].
    static EXPORTED_TYPES: RefCell<Option<Heap<js::collections::weak_map::WeakMap>>> =
        const { RefCell::new(None) };
}

/// The resource type index `object` backs an exported resource of, or `None` if
/// it backs none yet.
pub(crate) fn exported_type(
    scope: &Scope<'_>,
    object: &Object<'_>,
) -> Result<Option<u32>, ExnThrown> {
    let Some(types) =
        EXPORTED_TYPES.with(|cell| cell.borrow().as_ref().map(|heap| heap.get(scope)))
    else {
        return Ok(None);
    };
    let types: js::WeakMap<'_> = types;
    let index = types.lookup(scope, scope.root_value(object.as_value()))?;
    Ok(index.is_number().then(|| index.to_number() as u32))
}

/// Record that `object` backs an exported resource of type `type_idx`.
fn set_exported_type(
    scope: &Scope<'_>,
    object: &Object<'_>,
    type_idx: u32,
) -> Result<(), ExnThrown> {
    let existing = EXPORTED_TYPES.with(|cell| cell.borrow().as_ref().map(|heap| heap.get(scope)));
    let types: js::WeakMap<'_> = match existing {
        Some(types) => types,
        None => {
            let types = js::WeakMap::new(scope)?;
            EXPORTED_TYPES.with(|cell| *cell.borrow_mut() = Some(Heap::from(types)));
            types
        }
    };
    let index = scope.root_value(js::value::from_u32(type_idx));
    types.insert(scope, scope.root_value(object.as_value()), index)
}

js::instance_local! {
    /// The finalization registry's `register` and `unregister` JS functions, set
    /// once by [`init_finalization`] and traced by [`trace_resources`].
    static FINALIZATION: RefCell<Option<Finalization>> = const { RefCell::new(None) };
}

/// The two JS functions that drive the `FinalizationRegistry` created at init.
#[js::allow_unrooted_interior]
struct Finalization {
    register: Heap<js::native::Value>,
    unregister: Heap<js::native::Value>,
}

/// The JS bootstrap creating the finalization registry and its `register` and
/// `unregister` helpers. The drop callback is read from the hidden
/// [`DISPOSE_FIELD`] own-property.
///
/// `register` clones the handle, type and dispose into a detached object, so the
/// held value does not strongly reference the wrapper. The unregister token is the
/// wrapper itself.
///
/// The finalizer does nothing for a value registered without a `dispose`, and
/// otherwise calls `dispose` on the clone. `unregister` returns whether its
/// argument was registered or is a clone the finalizer passed to `dispose`, which
/// only the first call for either does.
fn finalization_bootstrap() -> String {
    let [handle, ty, dispose] = [HANDLE_FIELD, TYPE_FIELD, DISPOSE_FIELD]
        .map(|field| field.to_str().expect("field names are ASCII"));
    format!(
        r#"
(() => {{
    const registry = new FinalizationRegistry((v) => v());
    const finalizing = new WeakSet();
    const register = function (value) {{
        const clone = {{
            {handle}: value.{handle},
            {ty}: value.{ty},
        }};
        const dispose = value.{dispose};
        registry.register(value, () => {{
            if (dispose) {{
                finalizing.add(clone);
                dispose.call(clone);
            }}
        }}, value);
    }};
    const unregister = function (value) {{
        return registry.unregister(value) || finalizing.delete(value);
    }};
    return {{ register, unregister }};
}})()
"#
    )
}

/// Create the finalization registry and store its helpers. Idempotent, and must
/// run after [`crate::tracing::install_tracer`] and before any resource is
/// wrapped.
pub fn init_finalization(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    if FINALIZATION.with(|cell| cell.borrow().is_some()) {
        return Ok(());
    }

    let result = js::compile::evaluate(scope, &finalization_bootstrap())?;
    let obj = Object::from_value(scope, result)
        .map_err(|_| throw_type_error(scope, c"finalization bootstrap did not return an object"))?;
    let register = obj.get_property(scope, c"register")?;
    let unregister = obj.get_property(scope, c"unregister")?;

    FINALIZATION.with(|cell| {
        *cell.borrow_mut() = Some(Finalization {
            register: Heap::from(register.get()),
            unregister: Heap::from(unregister.get()),
        });
    });
    Ok(())
}

/// Register `wrapper` with the finalization registry, so its [`DISPOSE_FIELD`]
/// callback runs if the wrapper is collected without an explicit drop. Also
/// stamps the canonical handle into [`HANDLE_FIELD`].
pub fn register_resource(
    scope: &Scope<'_>,
    wrapper: &Object<'_>,
    handle: u32,
) -> Result<(), ExnThrown> {
    wrapper.define_property(scope, HANDLE_FIELD, handle, HIDDEN_FIELD_ATTRS)?;
    call_finalization(scope, wrapper, FinalizationOp::Register).map(|_| ())
}

/// Unregister `wrapper` from the finalization registry, so a subsequent
/// collection does not fire its drop. Returns whether `wrapper` was registered.
/// The handle field is left alone, since the owned-lower path deletes it
/// itself.
pub fn unregister_resource(scope: &Scope<'_>, wrapper: &Object<'_>) -> Result<bool, ExnThrown> {
    call_finalization(scope, wrapper, FinalizationOp::Unregister)
}

enum FinalizationOp {
    Register,
    Unregister,
}

/// Call the registry helper for `op` with `wrapper`, returning whether the
/// helper returned a truthy value.
fn call_finalization(
    scope: &Scope<'_>,
    wrapper: &Object<'_>,
    op: FinalizationOp,
) -> Result<bool, ExnThrown> {
    // This throws rather than panicking, since it is reached next to the FFI
    // boundary and the interpreter turns a thrown error into a clean trap.
    let fval = FINALIZATION.with(|cell| {
        let borrow = cell.borrow();
        let fin = borrow.as_ref().ok_or_else(|| {
            throw_type_error(
                scope,
                c"finalization not initialized (call init_finalization)",
            )
        })?;
        let heap = match op {
            FinalizationOp::Register => &fin.register,
            FinalizationOp::Unregister => &fin.unregister,
        };
        Ok(heap.get(scope))
    })?;
    let result = Function::call(scope, HandleValue::undefined(), fval, &[wrapper])?;
    <bool as js::conversion::FromJSVal>::from_jsval(scope, result, ()).map_err(|e| e.throw(scope))
}

/// Drop the slab, the finalization helpers and the registered prototypes. See
/// [`crate::tracing::clear_traced_roots`].
pub(crate) fn clear_traced_roots() {
    EXPORTED_RESOURCES.with(|slab| *slab.borrow_mut() = ExportedResources::default());
    FINALIZATION.with(|cell| *cell.borrow_mut() = None);
    RESOURCE_PROTOTYPES.with(|cell| cell.borrow_mut().clear());
    EXPORTED_TYPES.with(|cell| *cell.borrow_mut() = None);
}

/// Trace every slab-held resource, the finalization helper functions, the
/// registered resource prototypes, and the exported-type side table.
///
/// # Safety
///
/// `trc` must be a valid `JSTracer` provided by SpiderMonkey's GC, and this must
/// run from the crate tracer, with JS execution paused.
pub(crate) unsafe fn trace_resources(trc: *mut JSTracer) {
    EXPORTED_RESOURCES.with(|slab| {
        let slab = &*slab.as_ptr();
        for heap in slab.entries.iter().flatten() {
            heap.trace(trc);
        }
    });
    FINALIZATION.with(|cell| {
        if let Some(fin) = &*cell.as_ptr() {
            fin.register.trace(trc);
            fin.unregister.trace(trc);
        }
    });
    RESOURCE_PROTOTYPES.with(|cell| {
        for heap in (*cell.as_ptr()).iter().flatten() {
            heap.trace(trc);
        }
    });
    EXPORTED_TYPES.with(|cell| {
        if let Some(heap) = &*cell.as_ptr() {
            heap.trace(trc);
        }
    });
}
