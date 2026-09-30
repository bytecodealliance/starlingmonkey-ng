// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Resource handles: imported wrappers, the exported-resource slab, and
//! finalization.
//!
//! A resource type is either imported or exported:
//!
//! - Imported resources are implemented by the host. JavaScript sees a wrapper
//!   object, an [`ImportedResource`], whose private data holds the resource's
//!   handle. [`wrap_imported`] creates a wrapper when a handle is lifted, and
//!   [`unwrap_to_canon`] returns the wrapper's handle when it is lowered.
//! - Exported resources are implemented by JavaScript objects. Lowering an
//!   object stores it in the [`ExportedResources`] slab, with the slab index as
//!   the resource's rep, and creates a handle for it. Each lowering of the same
//!   object creates another rep and handle. An object is removed from the slab
//!   by [`drop_exported`] when the host drops the resource, by [`push_own`] when
//!   the host passes ownership back, and by [`release_borrows`] when a call that
//!   borrowed it ends.
//!
//! Each operation takes a [`ResourceDesc`], which describes the resource type
//! and holds the functions that create, resolve and drop handles of exported
//! resources.

use std::cell::{Cell, RefCell};

use core_runtime::{jsclass, jsmethods};
use js::conversion::FromJSVal;
use js::error::{throw_type_error, ExnThrown};
use js::function::{CallbackArgs, EmptyArgs};
use js::gc::handle::RootedHeap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use slab::Slab;

use crate::stack::CallStack;

/// Drops a canonical handle of an imported resource type. The arguments after
/// the scope are the type index and the handle.
pub type ImportedDrop = fn(&Scope<'_>, u32, u32) -> Result<(), ExnThrown>;

js::instance_local! {
    /// The function set by [`set_imported_drop`].
    static IMPORTED_DROP: Cell<Option<ImportedDrop>> = const { Cell::new(None) };

    /// The type index and handle of every collected wrapper whose handle is not
    /// dropped yet, queued by the wrapper's finalizer and dropped by the next
    /// [`release_borrows`].
    static COLLECTED_HANDLES: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
}

/// Set the function that drops the canonical handle of a wrapper of an imported
/// resource when the wrapper is disposed of, released at the end of the call it
/// was lifted for, or collected. Must run before any wrapper is disposed of.
pub fn set_imported_drop(drop: ImportedDrop) {
    IMPORTED_DROP.with(|cell| cell.set(Some(drop)));
}

/// Drop `handle`, which has the imported resource type `type_idx`, through the
/// function set by [`set_imported_drop`]. Throws a `TypeError` if no function is
/// set.
fn drop_imported_handle(scope: &Scope<'_>, type_idx: u32, handle: u32) -> Result<(), ExnThrown> {
    let drop = IMPORTED_DROP.with(Cell::get).ok_or_else(|| {
        throw_type_error(scope, c"no drop function is set for imported resources")
    })?;
    drop(scope, type_idx, handle)
}

/// The state of a wrapper of a host-owned resource handle. The wrapper's
/// prototype is the `prototype` of its resource type's class, or
/// `ImportedResource.prototype` for a type without a registered class (see
/// [`register_resource_prototype`]).
///
/// If a wrapper is collected while it owns a handle, the handle is dropped by
/// the next [`release_borrows`].
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
}

#[jsmethods]
impl ImportedResource {
    /// A wrapper of `handle`, a handle of the resource type `type_idx` that the
    /// guest received as a `borrow` if `borrowed`.
    fn new(type_idx: u32, handle: Option<u32>, borrowed: bool) -> Self {
        ImportedResourceImpl {
            type_idx,
            handle,
            borrowed,
            lends: 0,
            dispose_deferred: false,
        }
    }

    /// Queue the handle for [`release_borrows`] to drop, if the wrapper still
    /// holds one it owns. A collected wrapper is lent to no call, since a lend
    /// keeps it reachable.
    #[destructor]
    fn queue_handle_drop(&mut self) {
        if let (Some(handle), false) = (self.handle, self.borrowed) {
            let type_idx = self.type_idx;
            // Native targets destroy the queue at thread exit, and a wrapper
            // finalized after that leaves its handle undropped.
            let _ = COLLECTED_HANDLES.try_with(|cell| cell.borrow_mut().push((type_idx, handle)));
        }
    }
}

/// Converts a value that isn't a wrapper into an `own` handle of an imported
/// resource type, when the value is lowered as such a handle. Returns `None` if
/// it doesn't convert the value, which is then lowered as a wrapper.
pub type OwnAdapter = fn(&Scope<'_>, HandleValue<'_>) -> Result<Option<u32>, ExnThrown>;

/// Checks whether an [`OwnAdapter`] converts a value, without converting it:
/// `Ok(true)` if it does, `Ok(false)` if the value is not of a kind it converts,
/// and `Err` with the reason if the value is of that kind but can't be
/// converted. [`crate::validate`] uses it so that an import call throws a
/// `TypeError` for a value that the adapter would refuse.
pub type OwnAdapterCheck = fn(&Scope<'_>, HandleValue<'_>) -> Result<bool, String>;

js::instance_local! {
    /// The adapters registered by [`register_own_adapter`], each with the
    /// interface name and the resource name it applies to.
    static OWN_ADAPTERS: RefCell<Vec<(String, String, OwnAdapter, OwnAdapterCheck)>> =
        const { RefCell::new(Vec::new()) };
}

/// Register `adapter` for the imported resource type `resource` of the
/// interface `interface`, such as `wasi:http/types@0.3.0`, and of every
/// interface for which [`wit_name_mapping::same_interface`] returns `true`.
/// `check` must accept exactly the values that `adapter` converts.
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
                wit_name_mapping::same_interface(registered, interface) && name == resource
            })
            .map(|(_, _, adapter, check)| (*adapter, *check))
    })
}

/// Returns `value` as a wrapper of an imported resource, or `None` if it is not one.
pub(crate) fn imported_wrapper<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
) -> Option<ImportedResource<'s>> {
    ImportedResource::from_jsval(scope, value, ()).ok()
}

/// Return the handle held by `value`, a wrapper of an imported resource, or
/// `None` if `value` isn't such a wrapper or holds no handle.
pub fn imported_handle(scope: &Scope<'_>, value: HandleValue<'_>) -> Option<u32> {
    let wrapper = imported_wrapper(scope, value)?;
    let handle = wrapper.data().handle;
    handle
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
    Ok(js::function::is_callable(value).then_some(value))
}

/// A resource type, with the functions for handles of exported resource types.
/// Handles of imported resource types are dropped with the function set by
/// [`set_imported_drop`].
pub enum ResourceDesc<'a> {
    /// An imported resource type.
    Imported { type_idx: u32 },
    /// An exported resource type.
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
    /// The prototype of each imported resource type's class, indexed by type
    /// index. New wrappers get this prototype.
    static RESOURCE_PROTOTYPES: RefCell<Vec<Option<RootedHeap<js::object::Object>>>> =
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
        protos[idx] = Some(RootedHeap::new(*proto));
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

/// Create a wrapper of `handle`, a handle of the resource type `type_idx` that
/// the guest received as a `borrow` if `borrowed`, with `proto` as its
/// prototype, or the prototype of [`ImportedResource`] if `proto` is `None`.
fn new_wrapper<'s>(
    scope: &'s Scope<'_>,
    proto: Option<Object<'s>>,
    type_idx: u32,
    handle: Option<u32>,
    borrowed: bool,
) -> Result<ImportedResource<'s>, ExnThrown> {
    match proto {
        Some(proto) => js::class::create_instance_with_proto(scope, proto, |_| {
            ImportedResourceImpl::new(type_idx, handle, borrowed)
        }),
        None => ImportedResource::new(scope, type_idx, handle, borrowed),
    }
}

/// Create a wrapper of the owned handle `handle` of the imported resource type
/// `type_idx`, with the prototype `proto`, or the prototype of
/// [`ImportedResource`] if `proto` is `None`. The handle is dropped if the
/// wrapper is collected while it holds the handle.
pub fn wrap_imported<'s>(
    scope: &'s Scope<'_>,
    handle: u32,
    type_idx: u32,
    proto: Option<Object<'s>>,
) -> Result<Object<'s>, ExnThrown> {
    let wrapper = new_wrapper(scope, proto, type_idx, Some(handle), false)?;
    Ok(*wrapper)
}

/// Return the handle held by `value`, a wrapper of the imported resource type
/// `type_idx`. If `owned` is set, the handle is removed from the wrapper, so it
/// isn't dropped when the wrapper is disposed of or collected.
///
/// Throws a `TypeError` if `value` isn't a wrapper of that type, or holds no
/// handle. If `owned` is set, also throws if the wrapper was received as a
/// `borrow`, or is lent to an import call that hasn't returned.
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
            Some(handle) => Ok(handle),
        }
    };
    let handle = checked.map_err(|message| throw_type_error(scope, message))?;
    if owned {
        wrapper.data_mut().handle = None;
    }
    Ok(handle)
}

/// Drop the handle held by `wrapper` with the function set by
/// [`set_imported_drop`]. Does nothing if `wrapper` holds no handle.
///
/// A wrapper lent to an import call that hasn't returned keeps its handle,
/// which is dropped once the last such call returns.
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
                Some((data.type_idx, handle))
            }
        }
    };
    match taken {
        Some((type_idx, handle)) => drop_imported_handle(scope, type_idx, handle),
        None => Ok(()),
    }
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

/// Move the handle held by `from` to `to`, a wrapper of the same type that
/// holds no handle. Does nothing if `from` holds no handle.
#[cfg(target_arch = "wasm32")]
pub(crate) fn move_handle(from: ImportedResource<'_>, to: ImportedResource<'_>) {
    let handle = from.data_mut().handle.take();
    if handle.is_some() {
        to.data_mut().handle = handle;
    }
}

/// Release the borrows recorded on `stack` during a call that has ended. The
/// handles of collected wrappers (see [`ImportedResource`]) are dropped first.
///
/// - An imported resource lifted as a `borrow` is disposed of, so its wrapper
///   no longer holds the handle. If the wrapper is lent to an import call that
///   hasn't returned, the handle is dropped when the last such call returns.
/// - A wrapper lent to the call has its lend count decremented. If it was
///   disposed of while lent, and this was its last lend, its handle is dropped.
/// - An exported resource lent to the host is removed from the slab, and its
///   handle is passed to `drop_exported_handle` with the resource's type index.
///   The object is removed first, so that its `[Symbol.dispose]` method isn't
///   called when the handle is dropped.
///
/// Every borrow is released even if releasing one throws. The first exception
/// is rethrown afterwards.
pub fn release_borrows(
    stack: &mut CallStack,
    scope: &Scope<'_>,
    drop_exported_handle: &dyn Fn(u32, u32),
) -> Result<(), ExnThrown> {
    // The first exception is kept and cleared, so that the remaining releases
    // run without a pending exception.
    let mut first_exn: Option<HandleValue> = None;
    let mut keep_first_failure = |result: Result<(), ExnThrown>| {
        if result.is_err() {
            match (first_exn, js::exception::take_pending(scope)) {
                // The first catchable exception, kept rooted in `scope`.
                (None, Some(exn)) => first_exn = Some(exn),
                // A later failure, or an uncatchable one such as OOM.
                _ => js::exception::clear(scope),
            }
        }
    };
    let collected = COLLECTED_HANDLES.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
    for (type_idx, handle) in collected {
        keep_first_failure(drop_imported_handle(scope, type_idx, handle));
    }
    // Borrows are popped one at a time, so the remaining ones stay traced
    // during a GC triggered by a release.
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
        keep_first_failure(result);
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

/// Lift an `own` handle. For an exported resource, the object is removed from
/// the slab and the handle is dropped. For an imported resource, a new wrapper
/// is created.
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

/// Lift a `borrow` handle. For an exported resource, `handle` is the rep, and
/// the object in the slab is pushed. For an imported resource, a new wrapper is
/// created and recorded on `stack`, so that [`release_borrows`] disposes of it
/// when the call ends.
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
            let proto = resource_prototype(scope, *type_idx);
            let wrapper = new_wrapper(scope, proto, *type_idx, Some(handle), true)?;
            stack.push_imported_borrow(&wrapper);
            stack.push_object(&wrapper);
        }
    }
    Ok(())
}

/// Lower an `own` handle. For an exported resource, the object is stored in
/// the slab and a new handle is created for it. For an imported resource, the
/// handle is removed from the wrapper (see [`unwrap_to_canon`]).
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

/// Lower a `borrow` handle. For an exported resource, the object is stored in
/// the slab and a new handle is created for it. For an imported resource, the
/// wrapper's handle is used, and the wrapper's lend count is incremented. In
/// both cases, the borrow is recorded on `stack`, so that [`release_borrows`]
/// releases it when the call ends.
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

/// The objects implementing exported resources, indexed by rep, each with its
/// resource type index. Freed indices are reused. The objects are rooted.
#[derive(Default)]
pub struct ExportedResources {
    entries: Slab<(RootedHeap<js::object::Object>, u32)>,
}

impl ExportedResources {
    /// Insert `obj`, which implements a resource of type `type_idx`, and return
    /// its rep.
    pub fn insert(&mut self, obj: &Object<'_>, type_idx: u32) -> u32 {
        let rep = self.entries.insert((RootedHeap::new(*obj), type_idx));
        u32::try_from(rep).expect("too many exported resource objects")
    }

    /// Look up the object at `rep`, or `None` if the slot is free or out of
    /// range.
    pub fn lookup(&self, rep: u32) -> Option<&RootedHeap<js::object::Object>> {
        self.entries.get(rep as usize).map(|(object, _)| object)
    }

    /// Remove the object at `rep`, freeing the slot, and root it into `scope`.
    /// Returns `None` if the slot was already free or out of range.
    pub fn remove<'s>(&mut self, scope: &'s Scope<'_>, rep: u32) -> Option<Object<'s>> {
        self.remove_with_type(scope, rep).map(|(object, _)| object)
    }

    /// Like [`remove`](Self::remove), but also returns the type index the
    /// object was inserted with.
    fn remove_with_type<'s>(
        &mut self,
        scope: &'s Scope<'_>,
        rep: u32,
    ) -> Option<(Object<'s>, u32)> {
        let (object, type_idx) = self.entries.try_remove(rep as usize)?;
        Some((object.get(scope), type_idx))
    }
}

/// Insert `value` into the slab, and return a new handle for it, created with
/// `desc`'s `new`. Each call creates another rep and handle, even for the same
/// object.
///
/// Throws a `TypeError` if `desc` isn't a [`ResourceDesc::Exported`], or if
/// `value` isn't an object.
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
    /// The handle passed to the host.
    handle: u32,
    /// The object's rep.
    rep: u32,
}

/// Like [`exported_resource_to_canon`], but also returns the rooted object and the rep.
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

    let object = Object::from_raw_value(scope, value.get())
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
    /// The slab of guest-owned resource objects.
    static EXPORTED_RESOURCES: RefCell<ExportedResources> =
        RefCell::new(ExportedResources::default());
}

/// Run `f` with a mutable borrow of the exported-resource slab.
pub fn with_exported_resources<R>(f: impl FnOnce(&mut ExportedResources) -> R) -> R {
    EXPORTED_RESOURCES.with(|slab| f(&mut slab.borrow_mut()))
}

/// Drop an exported resource that the host has dropped: remove the object with
/// the rep `rep` from the slab, and call its `[Symbol.dispose]` method if it
/// has one. Does nothing if there is no object with that rep.
///
/// In debug builds, panics if the object's type index isn't
/// `expected_type_idx`.
pub fn drop_exported(scope: &Scope<'_>, rep: u32, expected_type_idx: u32) -> Result<(), ExnThrown> {
    let Some((obj, type_idx)) = with_exported_resources(|slab| slab.remove_with_type(scope, rep))
    else {
        // Dropping an unknown rep does nothing. Such a rep comes from a double
        // drop or from a handle that never entered the slab.
        return Ok(());
    };

    debug_assert_eq!(
        type_idx, expected_type_idx,
        "resource_dtor type index mismatch"
    );

    if let Some(dispose) = dispose_method(scope, &obj)? {
        Function::call(scope, obj, dispose, EmptyArgs)?;
    }
    Ok(())
}

/// Drop the slab and the registered prototypes. See
/// [`crate::tracing::clear_traced_roots`].
pub(crate) fn clear_traced_roots() {
    EXPORTED_RESOURCES.with(|slab| *slab.borrow_mut() = ExportedResources::default());
    RESOURCE_PROTOTYPES.with(|cell| cell.borrow_mut().clear());
}
