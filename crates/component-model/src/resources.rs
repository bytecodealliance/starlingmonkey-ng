// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Resource handles: imported wrappers, the exported-resource slab, and
//! finalization.
//!
//! The Component Model has two kinds of resource a JS guest deals with:
//!
//! - Imported resources are owned by the host. The guest holds an opaque
//!   canonical handle, a `u32`, and JS sees a wrapper object, an
//!   [`ImportedResource`] whose private data holds the handle. [`wrap_imported`]
//!   creates a wrapper from a handle on lift, and [`unwrap_to_canon`] reads the
//!   handle back out on lower.
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

use std::cell::{Cell, RefCell};

use core_runtime::{jsclass, jsmethods};
use js::conversion::{FromJSVal, ToJSVal};
use js::error::{throw_type_error, ExnThrown};
use js::function::{CallbackArgs, EmptyArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::heap::Trace;
use js::native::{JSTracer, Value};
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::stack::CallStack;

/// Drops a canonical handle of an imported resource type. The arguments after
/// the scope are the type index and the handle.
pub type ImportedDrop = fn(&Scope<'_>, u32, u32) -> Result<(), ExnThrown>;

js::instance_local! {
    /// The function [`set_imported_drop`] set.
    static IMPORTED_DROP: Cell<Option<ImportedDrop>> = const { Cell::new(None) };
}

/// Set the function that drops the canonical handle of a wrapper of an imported
/// resource when the wrapper is disposed of, released at the end of the call it
/// was lifted for, or collected. Must run before any wrapper is disposed of.
pub fn set_imported_drop(drop: ImportedDrop) {
    IMPORTED_DROP.with(|cell| cell.set(Some(drop)));
}

/// The state of a wrapper of a host-owned resource handle. The wrapper's
/// prototype is the `prototype` of its resource type's class.
#[jsclass(hidden)]
pub struct ImportedResource {
    /// The resource type index.
    #[no_trace]
    pub(crate) type_idx: u32,
    /// The canonical handle, or `None` once the wrapper was disposed of, released
    /// at the end of the call it was lifted for, or moved to the host.
    #[no_trace]
    pub(crate) handle: Option<u32>,
    /// Whether the guest received the handle as a `borrow`.
    #[no_trace]
    pub(crate) borrowed: bool,
    /// The number of import calls the wrapper is lent to that have not returned.
    /// The handle cannot be dropped or moved while it is not 0.
    #[no_trace]
    pub(crate) lends: u32,
    /// Whether the wrapper was disposed of while lent. The handle is dropped once
    /// the last lend ends.
    #[no_trace]
    pub(crate) dispose_deferred: bool,
    /// Whether the wrapper is registered with the finalization registry.
    #[no_trace]
    registered: bool,
}

#[jsmethods]
impl ImportedResource {
    fn new() -> Self {
        // This constructor only registers the prototype. Wrappers are created by
        // `new_wrapper`, with their resource class's prototype.
        ImportedResourceImpl::default()
    }
}

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

/// `value` as a wrapper of an imported resource, or `None` if it is not one.
pub(crate) fn imported_wrapper<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
) -> Option<ImportedResource<'s>> {
    ImportedResource::from_jsval(scope, value, ()).ok()
}

/// The canonical handle the wrapper of an imported resource `value` holds, or
/// `None` if `value` is not such a wrapper or holds no handle.
pub fn imported_handle(scope: &Scope<'_>, value: HandleValue<'_>) -> Option<u32> {
    let wrapper = imported_wrapper(scope, value)?;
    let handle = wrapper.data().handle;
    handle
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

/// A resource type, resolved by the caller from wit-dylib metadata (or built by
/// hand in native tests). `Imported` needs no operations, since its drop is the
/// function [`set_imported_drop`] set and lowering only reads the handle back.
pub enum ResourceDesc<'a> {
    /// A host-owned resource type, identified by its type index, stored in the
    /// wrapper's private data.
    Imported { type_idx: u32 },
    /// A guest-owned resource type backed by the slab.
    Exported {
        /// The resource type index, stored in the slab entry.
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
/// created for that type get it as their prototype. A later registration for the
/// same index replaces the previous prototype.
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

/// The registered prototype for `type_idx`, rooted on `scope`, if any.
fn resource_prototype<'s>(scope: &'s Scope<'_>, type_idx: u32) -> Option<Object<'s>> {
    RESOURCE_PROTOTYPES.with(|cell| {
        cell.borrow()
            .get(type_idx as usize)
            .and_then(Option::as_ref)
            .map(|heap| heap.get(scope))
    })
}

/// Create a wrapper with `data` as its state and `proto` as its prototype, or
/// the prototype of [`ImportedResource`] if `proto` is `None`.
fn new_wrapper<'s>(
    scope: &'s Scope<'_>,
    proto: Option<Object<'s>>,
    data: ImportedResourceImpl,
) -> Result<ImportedResource<'s>, ExnThrown> {
    match proto {
        Some(proto) => js::class::create_instance_with_proto(scope, proto, data, 0),
        None => js::class::create_instance_with(scope, |_| data),
    }
}

/// Lift a host-owned resource handle into its JS wrapper object.
///
/// `proto`, when given, becomes the wrapper's prototype, so the class's methods
/// resolve. The wrapper is registered with the finalization registry, so the
/// handle is dropped if the wrapper is collected without an explicit drop.
pub fn wrap_imported<'s>(
    scope: &'s Scope<'_>,
    handle: u32,
    type_idx: u32,
    proto: Option<Object<'s>>,
) -> Result<Object<'s>, ExnThrown> {
    let data = ImportedResourceImpl {
        type_idx,
        handle: Some(handle),
        ..Default::default()
    };
    let wrapper = new_wrapper(scope, proto, data)?;
    register_resource(scope, wrapper)?;
    Ok(*wrapper)
}

/// Lower a JS wrapper of the imported resource type `type_idx` to its canonical
/// handle.
///
/// A value that is not a wrapper of that type, and a wrapper that holds no
/// handle, are rejected with a TypeError. When `owned`, ownership transfers out
/// of JS: the wrapper is unregistered from finalization and loses its handle, so
/// neither the finalizer nor a later explicit drop double-drops the transferred
/// handle. A wrapper the guest received as a `borrow`, one lent to an import call
/// that has not returned, and one whose disposal waits for such a call cannot be
/// owned, and throw a TypeError too.
pub fn unwrap_to_canon(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    type_idx: u32,
    owned: bool,
) -> Result<u32, ExnThrown> {
    if !value.is_object() {
        return Err(throw_type_error(scope, c"resource value is not an object"));
    }
    let wrapper = imported_wrapper(scope, value)
        .filter(|wrapper| wrapper.data().type_idx == type_idx)
        .ok_or_else(|| {
            throw_type_error(
                scope,
                c"resource value is not a wrapper of the expected resource type",
            )
        })?;
    let checked = {
        let data = wrapper.data();
        match data.handle {
            None => Err(c"resource wrapper has no live handle (already disposed?)"),
            Some(_) if owned && data.borrowed => {
                Err(c"a borrowed resource cannot be passed as an owned one")
            }
            Some(_) if owned && (data.lends > 0 || data.dispose_deferred) => {
                Err(c"the resource is lent to an import call that has not returned")
            }
            Some(handle) => Ok((handle, data.registered)),
        }
    };
    let (handle, registered) = checked.map_err(|message| throw_type_error(scope, message))?;

    if owned {
        if registered {
            unregister_resource(scope, &wrapper)?;
        }
        let mut data = wrapper.data_mut();
        data.handle = None;
        data.registered = false;
    }

    Ok(handle)
}

/// Drop the handle `wrapper` holds, through the function [`set_imported_drop`]
/// set, and unregister `wrapper` from finalization.
///
/// A wrapper without a handle is left alone, so a second dispose does nothing. A
/// wrapper lent to an import call that has not returned keeps its handle, which
/// is dropped once the last such call returns.
fn dispose_wrapper(scope: &Scope<'_>, wrapper: ImportedResource<'_>) -> Result<(), ExnThrown> {
    let taken = {
        let mut data = wrapper.data_mut();
        match data.handle {
            None => None,
            Some(_) if data.lends > 0 => {
                data.dispose_deferred = true;
                None
            }
            Some(handle) => {
                data.handle = None;
                Some((data.type_idx, handle, std::mem::take(&mut data.registered)))
            }
        }
    };
    let Some((type_idx, handle, registered)) = taken else {
        return Ok(());
    };
    if registered {
        unregister_resource(scope, &wrapper)?;
    }
    let drop = IMPORTED_DROP.with(Cell::get).ok_or_else(|| {
        throw_type_error(scope, c"no drop function is set for imported resources")
    })?;
    drop(scope, type_idx, handle)
}

/// `[Symbol.dispose]` of every imported resource class: drop the handle the
/// wrapper `this` holds. Throws a `TypeError` if `this` is not a wrapper.
fn dispose_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    let wrapper = imported_wrapper(scope, this).ok_or_else(|| {
        throw_type_error(
            scope,
            c"dispose was called on an object that is not a resource wrapper",
        )
    })?;
    dispose_wrapper(scope, wrapper)?;
    Ok(js::value::undefined())
}

/// The function installed as `[Symbol.dispose]` on the prototype of every
/// imported resource class.
pub fn dispose_function<'s>(scope: &'s Scope<'_>) -> Result<Function<'s>, ExnThrown> {
    js::class::get_or_init_shared_function(scope, dispose_callback as *const () as usize, |scope| {
        Function::new_callback(scope, c"[Symbol.dispose]", 0, dispose_callback, ())
    })
}

/// Move the handle `from` holds to `to`, a wrapper of the same type without one,
/// and register `to` with the finalization registry. `from` keeps no handle and
/// is unregistered. Does nothing if `from` holds no handle.
#[cfg(target_arch = "wasm32")]
pub(crate) fn move_handle(
    scope: &Scope<'_>,
    from: ImportedResource<'_>,
    to: ImportedResource<'_>,
) -> Result<(), ExnThrown> {
    let (handle, registered) = {
        let mut data = from.data_mut();
        let Some(handle) = data.handle.take() else {
            return Ok(());
        };
        (handle, std::mem::take(&mut data.registered))
    };
    if registered {
        unregister_resource(scope, &from)?;
    }
    to.data_mut().handle = Some(handle);
    register_resource(scope, to)
}

/// Release every borrow recorded on `stack` during the just-finished call.
///
/// An imported borrow's handle is dropped through the function
/// [`set_imported_drop`] set, and the wrapper keeps no handle, so a wrapper the
/// guest stashed cannot pass the released handle back to the host. A borrow
/// lent to an import call that has not returned is dropped once the last such
/// call returns.
///
/// A wrapper lent to an import call has its lend count decremented. Once the
/// last lend ends, a dispose that waited for it drops the handle.
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
    // Every borrow is drained even if one's drop throws, so a single throwing
    // drop cannot leak the rest. The first failure's exception is captured,
    // cleared so the next drop runs with none pending, and re-raised once the
    // list is drained.
    let mut first_exn: Option<HandleValue> = None;
    while let Some(borrow) = stack.pop_borrow() {
        let object = borrow.heap().get(scope);
        let result = match &borrow {
            crate::stack::Borrow::Imported { .. } => {
                let wrapper = object
                    .cast::<ImportedResource>()
                    .expect("an imported borrow holds a wrapper");
                dispose_wrapper(scope, wrapper)
            }
            crate::stack::Borrow::Lent { .. } => {
                let wrapper = object
                    .cast::<ImportedResource>()
                    .expect("a lent borrow holds a wrapper");
                let deferred = {
                    let mut data = wrapper.data_mut();
                    data.lends = data.lends.saturating_sub(1);
                    data.lends == 0 && std::mem::take(&mut data.dispose_deferred)
                };
                if deferred {
                    dispose_wrapper(scope, wrapper)
                } else {
                    Ok(())
                }
            }
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
            let proto = resource_prototype(scope, *type_idx);
            let wrapper = wrap_imported(scope, handle, *type_idx, proto)?;
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
/// so its handle is dropped at call end. The wrapper is not registered with the
/// finalization registry, since the borrow record keeps it reachable until then.
pub fn push_borrow(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    desc: &ResourceDesc<'_>,
    handle: u32,
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
            let data = ImportedResourceImpl {
                type_idx: *type_idx,
                handle: Some(handle),
                borrowed: true,
                ..Default::default()
            };
            let wrapper = new_wrapper(scope, resource_prototype(scope, *type_idx), data)?;
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
/// wrapper loses its handle, since the generated code removes the handle from
/// the guest's table right after this returns.
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
                imported_wrapper(scope, value).expect("unwrap_to_canon accepted a wrapper");
            wrapper.data_mut().lends += 1;
            stack.push_lent_wrapper(&wrapper);
            Ok(handle)
        }
    }
}

/// A free-list-backed table of guest-owned resource objects, keyed by rep, which
/// is the slab index, each with the type index of the resource it backs.
///
/// Stored objects are traced by [`trace_resources`], so a slab-held resource
/// survives GC while the host owns its handle.
#[js::allow_unrooted_interior]
#[derive(Default)]
pub struct ExportedResources {
    entries: Vec<Option<(Heap<js::object::Object>, u32)>>,
    free: Vec<u32>,
}

impl ExportedResources {
    /// Insert an object backing a resource of type `type_idx`, returning its rep
    /// (slab index).
    pub fn insert(&mut self, obj: &Object<'_>, type_idx: u32) -> u32 {
        let entry = Some((Heap::from(*obj), type_idx));
        if let Some(index) = self.free.pop() {
            self.entries[index as usize] = entry;
            index
        } else {
            let index = self.entries.len() as u32;
            self.entries.push(entry);
            index
        }
    }

    /// Look up the object at `rep`, or `None` if the slot is free or out of
    /// range.
    pub fn lookup(&self, rep: u32) -> Option<&Heap<js::object::Object>> {
        self.entries
            .get(rep as usize)
            .and_then(Option::as_ref)
            .map(|(object, _)| object)
    }

    /// Remove the object at `rep`, freeing the slot, and root it into `scope`.
    /// Returns `None` if the slot was already free or out of range.
    pub fn remove<'s>(&mut self, scope: &'s Scope<'_>, rep: u32) -> Option<Object<'s>> {
        self.remove_with_type(scope, rep).map(|(object, _)| object)
    }

    /// [`remove`](Self::remove), also returning the type index the object was
    /// inserted with.
    fn remove_with_type<'s>(
        &mut self,
        scope: &'s Scope<'_>,
        rep: u32,
    ) -> Option<(Object<'s>, u32)> {
        let (object, type_idx) = self.entries.get_mut(rep as usize)?.take()?;
        self.free.push(rep);
        Some((object.get(scope), type_idx))
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

    let rep = EXPORTED_RESOURCES.with(|slab| slab.borrow_mut().insert(&object, type_idx));
    let handle = new(rep);

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
/// `expected_type_idx` is checked against the type index the slab entry was
/// inserted with. A mismatch means a rep collided across resource types.
///
/// The object's `[Symbol.dispose]` method, if it has one, is called with the
/// removed object as `this`.
pub fn drop_exported(scope: &Scope<'_>, rep: u32, expected_type_idx: u32) -> Result<(), ExnThrown> {
    let Some((obj, type_idx)) = with_exported_resources(|slab| slab.remove_with_type(scope, rep))
    else {
        // An unknown rep, from a double drop or a handle that never entered the
        // slab, is a no-op.
        return Ok(());
    };

    // The type index is validated before running any guest code.
    debug_assert_eq!(
        type_idx, expected_type_idx,
        "resource_dtor type index mismatch"
    );

    if let Some(dispose) = dispose_method(scope, &obj)? {
        Function::call(scope, obj, dispose, EmptyArgs)?;
    }
    Ok(())
}

js::instance_local! {
    /// The finalization registry and its `register` and `unregister` methods,
    /// set once by [`init_finalization`] and traced by [`trace_resources`].
    static FINALIZATION: RefCell<Option<Finalization>> = const { RefCell::new(None) };
}

/// The `FinalizationRegistry` created at init, and the intrinsic
/// `FinalizationRegistry.prototype.register` and `unregister`.
#[js::allow_unrooted_interior]
struct Finalization {
    registry: Heap<js::object::Object>,
    register: Heap<Value>,
    unregister: Heap<Value>,
}

/// The registry's cleanup callback: drop the handle of a collected wrapper. The
/// held value is a wrapper that [`register_resource`] created with the
/// collected wrapper's type and handle.
fn finalize_callback(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue<'_>,
) -> Result<Value, ExnThrown> {
    if let Some(held) = imported_wrapper(scope, args.get(0)) {
        dispose_wrapper(scope, held)?;
    }
    Ok(js::value::undefined())
}

/// Create the finalization registry and store it with its methods. Idempotent,
/// and must run after [`crate::tracing::install_tracer`] and before any resource
/// is wrapped.
pub fn init_finalization(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    if FINALIZATION.with(|cell| cell.borrow().is_some()) {
        return Ok(());
    }

    use js::class_spec::JSProtoKey::JSProto_FinalizationRegistry;
    let constructor = js::class::get_class_object(scope, JSProto_FinalizationRegistry)?;
    let constructor = Object::from_handle(constructor).ok_or(ExnThrown)?;
    let cleanup = Function::new_callback(scope, c"", 1, finalize_callback, ())?;
    let registry = Function::construct(scope, constructor, &[cleanup])?;
    let proto = js::class::get_class_prototype(scope, JSProto_FinalizationRegistry)?;
    let proto = Object::from_handle(proto).ok_or(ExnThrown)?;
    let register = proto.get_property(scope, c"register")?;
    let unregister = proto.get_property(scope, c"unregister")?;

    FINALIZATION.with(|cell| {
        *cell.borrow_mut() = Some(Finalization {
            registry: Heap::from(registry),
            register: Heap::from(register.get()),
            unregister: Heap::from(unregister.get()),
        });
    });
    Ok(())
}

/// The registry and the method for `op`. Throws rather than panicking if
/// [`init_finalization`] has not run, since this is reached next to the FFI
/// boundary and the interpreter turns a thrown error into a clean trap.
fn finalization<'s>(
    scope: &'s Scope<'_>,
    op: FinalizationOp,
) -> Result<(Object<'s>, HandleValue<'s>), ExnThrown> {
    FINALIZATION.with(|cell| {
        let borrow = cell.borrow();
        let fin = borrow.as_ref().ok_or_else(|| {
            throw_type_error(
                scope,
                c"finalization not initialized (call init_finalization)",
            )
        })?;
        let method = match op {
            FinalizationOp::Register => &fin.register,
            FinalizationOp::Unregister => &fin.unregister,
        };
        Ok((fin.registry.get(scope), method.get(scope)))
    })
}

enum FinalizationOp {
    Register,
    Unregister,
}

/// Register `wrapper` with the finalization registry, so its handle is dropped if
/// the wrapper is collected without an explicit drop. The held value is a new
/// wrapper with the same type and handle, so it does not reference `wrapper`.
/// The unregister token is `wrapper` itself.
fn register_resource(scope: &Scope<'_>, wrapper: ImportedResource<'_>) -> Result<(), ExnThrown> {
    let held = {
        let data = wrapper.data();
        ImportedResourceImpl {
            type_idx: data.type_idx,
            handle: data.handle,
            ..Default::default()
        }
    };
    let held = new_wrapper(scope, None, held)?;
    let (registry, register) = finalization(scope, FinalizationOp::Register)?;
    Function::call(scope, registry, register, &[*wrapper, *held, *wrapper])?;
    wrapper.data_mut().registered = true;
    Ok(())
}

/// Unregister `wrapper` from the finalization registry, so a later collection
/// does not drop its handle.
fn unregister_resource(scope: &Scope<'_>, wrapper: &Object<'_>) -> Result<(), ExnThrown> {
    let (registry, unregister) = finalization(scope, FinalizationOp::Unregister)?;
    Function::call(scope, registry, unregister, &[*wrapper]).map(|_| ())
}

/// Drop the slab, the finalization registry and the registered prototypes. See
/// [`crate::tracing::clear_traced_roots`].
pub(crate) fn clear_traced_roots() {
    EXPORTED_RESOURCES.with(|slab| *slab.borrow_mut() = ExportedResources::default());
    FINALIZATION.with(|cell| *cell.borrow_mut() = None);
    RESOURCE_PROTOTYPES.with(|cell| cell.borrow_mut().clear());
}

/// Trace every slab-held resource, the finalization registry and its methods,
/// and the registered resource prototypes.
///
/// # Safety
///
/// `trc` must be a valid `JSTracer` provided by SpiderMonkey's GC, and this must
/// run from the crate tracer, with JS execution paused.
pub(crate) unsafe fn trace_resources(trc: *mut JSTracer) {
    EXPORTED_RESOURCES.with(|slab| {
        let slab = &*slab.as_ptr();
        for (heap, _) in slab.entries.iter().flatten() {
            heap.trace(trc);
        }
    });
    FINALIZATION.with(|cell| {
        if let Some(fin) = &*cell.as_ptr() {
            fin.registry.trace(trc);
            fin.register.trace(trc);
            fin.unregister.trace(trc);
        }
    });
    RESOURCE_PROTOTYPES.with(|cell| {
        for heap in (*cell.as_ptr()).iter().flatten() {
            heap.trace(trc);
        }
    });
}
