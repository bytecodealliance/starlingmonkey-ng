// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Promise creation, resolution, rejection, and reaction management.
//!
//! The [`Promise`] marker type implements [`JSType`](crate::gc::handle::JSType),
//! enabling [`Promise<'s>`](crate::Promise) as the scope-rooted
//! handle type. It provides methods for state inspection, resolution/rejection,
//! and adding reactions.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::builtins::JSType;
use crate::function::Callback;
use crate::gc::handle::{RootedHeap, Stack};
use crate::gc::scope::Scope;
use crate::prelude::ToJSVal;
use mozjs::jsapi::{JSObject, PromiseState};
use mozjs::rust::wrappers2;
use mozjs::rust::{HandleObject, HandleValue};

use super::error::{ExnThrown, OutOfMemory, ThrowException};
use crate::Object;

/// Marker type for JavaScript `Promise` objects.
///
/// [`Promise<'s>`](crate::Promise) is the scope-rooted handle type:
///
/// ```ignore
/// let promise = Promise::new(&scope, executor.handle())?;
/// let state = promise.state();
/// ```
pub struct Promise;

impl JSType for Promise {
    type Rooted<'s> = Stack<'s, Self>;
    const JS_NAME: &'static str = "Promise";

    fn js_class() -> *const mozjs::jsapi::JSClass {
        crate::class::proto_key_to_class(mozjs::jsapi::JSProtoKey::JSProto_Promise)
    }
}

impl<'s> Stack<'s, Promise> {
    /// Create a new `Promise` object from an executor function.
    ///
    /// The executor is called immediately with `(resolve, reject)` functions.
    pub fn new(scope: &'s Scope<'_>, executor: HandleObject) -> Result<Self, ExnThrown> {
        let obj = unsafe { wrappers2::NewPromiseObject(scope.cx_mut(), executor) };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Create a new unresolved `Promise` without an executor.
    ///
    /// The returned promise starts in the "pending" state and must be
    /// resolved or rejected later via [`resolve`](Self::resolve) /
    /// [`reject`](Self::reject).
    pub fn new_pending(scope: &'s Scope<'_>) -> Result<Self, ExnThrown> {
        let obj = unsafe { wrappers2::NewPromiseObject(scope.cx_mut(), HandleObject::null()) };
        // SAFETY: NewPromiseObject returns a Promise object or null.
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Create a new `Promise` resolved with the given `value`.
    ///
    /// This always creates a new `Promise` and resolves it with `value`.
    /// Use [`call_original_resolve`](Self::call_original_resolve) to get the
    /// behavior of `Promise.resolve(value)`, where if `value` already is a
    /// `Promise`, it's returned unchanged.
    pub fn new_resolved_with_value(
        scope: &'s Scope<'_>,
        value: impl ToJSVal<'s>,
    ) -> Result<Self, ExnThrown> {
        let value = value.to_jsval_throwing(scope)?;
        let promise = Self::new_pending(scope)?;
        promise.resolve(scope, value)?;
        Ok(promise)
    }

    /// Create a new `Promise` that is immediately rejected with the given `error`.
    pub fn new_rejected_with_error(
        scope: &'s Scope<'_>,
        error: impl ToJSVal<'s>,
    ) -> Result<Self, ExnThrown> {
        let error = error.to_jsval_throwing(scope)?;
        let obj = unsafe { wrappers2::CallOriginalPromiseReject(scope.cx_mut(), error) };
        // SAFETY: CallOriginalPromiseReject returns a Promise object or null.
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Create a new `Promise` that is immediately rejected with the pending exception.
    /// If no exception is pending (for example because of an uncatchable exception),
    /// the promise is resolved with `undefined`.
    pub fn new_rejected_with_pending_error(scope: &'s Scope<'_>) -> Result<Self, ExnThrown> {
        let error = crate::exception::take_pending_or_undefined(scope);
        Self::new_rejected_with_error(scope, error)
    }

    /// The per-global reused resolved-undefined promise for internal plumbing.
    ///
    /// Reactions on a settled promise enqueue their job immediately and leave
    /// nothing behind on the promise, so one instance serves every internal
    /// "already resolved" site — an absent algorithm's result, a `then` source
    /// for a derived promise — indefinitely, without allocating a fresh
    /// promise per use. (To merely defer a callback by one tick, use
    /// [`crate::jobs::queue_microtask`] instead.)
    /// The promise must never be handed to author code: an author-visible
    /// "a promise resolved with undefined" needs a fresh promise per call, or
    /// the shared identity becomes observable. Derived promises (`then`
    /// results) are fine to expose.
    pub fn shared_resolved_undefined(scope: &'s Scope<'_>) -> Result<Self, ExnThrown> {
        // Keyed by a static's address: stable and unique for the process,
        // unlike a generic function's address, which codegen may duplicate.
        static KEY: u8 = 0;
        let obj = crate::class::get_or_init_shared_object(
            scope,
            std::ptr::from_ref(&KEY) as usize,
            |scope| {
                let p = Self::new_resolved_with_value(scope, HandleValue::undefined())?;
                Ok(*p)
            },
        )?;
        Ok(obj
            .cast::<crate::Promise>()
            .expect("the shared resolved promise is a Promise"))
    }

    /// Check whether an object is a `Promise`.
    pub fn is_promise(obj: HandleObject) -> bool {
        // SAFETY: IsPromiseObject only inspects the object's class pointer.
        unsafe { wrappers2::IsPromiseObject(obj) }
    }

    /// Get the current state of this promise.
    pub fn state(&self) -> PromiseState {
        // SAFETY: self is a rooted handle to a valid Promise object.
        unsafe { wrappers2::GetPromiseState(self.handle()) }
    }

    /// Check whether this promise is already rejected.
    pub fn is_rejected(&self) -> bool {
        self.state() == PromiseState::Rejected
    }

    /// Check whether this promise is still pending (not settled).
    pub fn is_pending(&self) -> bool {
        self.state() == PromiseState::Pending
    }

    /// Get the result value of a settled promise.
    ///
    /// For a fulfilled promise this is the fulfillment value; for a rejected
    /// promise this is the rejection reason. On a pending promise this returns
    /// `None`.
    pub fn result<'a>(&self, scope: &'a Scope<'_>) -> Option<HandleValue<'a>> {
        if self.is_pending() {
            return None;
        }
        let mut val = scope.root_value_mut(mozjs::jsval::UndefinedValue());
        // SAFETY: self is a rooted handle to a valid, settled Promise object.
        unsafe { mozjs::glue::JS_GetPromiseResult(self.handle().into(), val.reborrow().into()) };
        Some(val.handle())
    }

    /// Get the unique ID of this promise (for debugging/tracking).
    pub fn id(&self) -> u64 {
        // SAFETY: self is a rooted handle to a valid Promise object.
        unsafe { wrappers2::GetPromiseID(self.handle()) }
    }

    /// Check whether this promise has been handled (i.e., has a rejection handler).
    pub fn is_handled(&self) -> bool {
        // SAFETY: self is a rooted handle to a valid Promise object.
        unsafe { wrappers2::GetPromiseIsHandled(self.handle()) }
    }

    /// Mark a settled promise as handled, suppressing unhandled rejection warnings.
    pub fn set_settled_is_handled(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::SetSettledPromiseIsHandled(scope.cx_mut(), self.handle()) };
        ExnThrown::check(ok)
    }

    /// Mark any promise (including pending) as handled.
    pub fn set_any_is_handled(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let ok = unsafe { wrappers2::SetAnyPromiseIsHandled(scope.cx_mut(), self.handle()) };
        ExnThrown::check(ok)
    }

    /// Get the allocation site of this promise (a `SavedFrame`, if available).
    pub fn allocation_site(&self) -> Option<NonNull<JSObject>> {
        NonNull::new(unsafe { wrappers2::GetPromiseAllocationSite(self.handle()) })
    }

    /// Get the resolution site of this promise (a `SavedFrame`, if available).
    pub fn resolution_site(&self) -> Option<NonNull<JSObject>> {
        NonNull::new(unsafe { wrappers2::GetPromiseResolutionSite(self.handle()) })
    }

    /// Resolve this promise with the given value.
    pub fn resolve<'a>(
        &self,
        scope: &'a Scope<'_>,
        resolution: impl ToJSVal<'a>,
    ) -> Result<(), ExnThrown> {
        let value = resolution.to_jsval_throwing(scope)?;
        let ok = unsafe { wrappers2::ResolvePromise(scope.cx_mut(), self.handle(), value) };
        ExnThrown::check(ok)
    }

    /// Reject this promise with the given value.
    pub fn reject<'a>(
        &self,
        scope: &'a Scope<'_>,
        error: impl ToJSVal<'a>,
    ) -> Result<(), ExnThrown> {
        let value = error.to_jsval_throwing(scope)?;
        let ok = unsafe { wrappers2::RejectPromise(scope.cx_mut(), self.handle(), value) };
        ExnThrown::check(ok)
    }

    /// Reject this promise with the pending exception.
    pub fn reject_with_pending(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let error = crate::exception::take_pending_or_undefined(scope);
        let ok = unsafe { wrappers2::RejectPromise(scope.cx_mut(), self.handle(), error) };
        ExnThrown::check(ok)
    }

    /// Add `then` reactions (fulfillment and rejection handlers) to this promise.
    pub fn add_reactions(
        &self,
        scope: &Scope<'_>,
        on_fulfilled: Option<Object<'_>>,
        on_rejected: Option<Object<'_>>,
    ) -> Result<(), ExnThrown> {
        let ok = unsafe {
            wrappers2::AddPromiseReactions(
                scope.cx_mut(),
                self.handle(),
                on_fulfilled.map_or(HandleObject::null(), |o| o.handle()),
                on_rejected.map_or(HandleObject::null(), |o| o.handle()),
            )
        };
        ExnThrown::check(ok)
    }

    /// Add `then` reactions, as [`add_reactions`](Self::add_reactions) does,
    /// except that a rejection with no `on_rejected` reaction is not reported
    /// through a derived promise. Like any reaction, these mark the promise
    /// handled, so its own rejection is not reported as unhandled either.
    pub fn add_reactions_ignoring_unhandled_rejection(
        &self,
        scope: &Scope<'_>,
        on_fulfilled: Option<Object<'_>>,
        on_rejected: Option<Object<'_>>,
    ) -> Result<(), ExnThrown> {
        let ok = unsafe {
            wrappers2::AddPromiseReactionsIgnoringUnhandledRejection(
                scope.cx_mut(),
                self.handle(),
                on_fulfilled.map_or(HandleObject::null(), |o| o.handle()),
                on_rejected.map_or(HandleObject::null(), |o| o.handle()),
            )
        };
        ExnThrown::check(ok)
    }

    /// Call `Promise.resolve(value)` using the original `Promise` constructor.
    ///
    /// If `resolution_value` is already a `Promise`, this returns it unchanged.
    /// Use [`new_resolved_with_value`](Self::new_resolved_with_value) if you
    /// need to ensure a fresh promise.
    pub fn call_original_resolve(
        scope: &'s Scope<'_>,
        resolution_value: impl ToJSVal<'s>,
    ) -> Result<Self, ExnThrown> {
        let resolution_value = resolution_value.to_jsval_throwing(scope)?;
        let obj =
            unsafe { wrappers2::CallOriginalPromiseResolve(scope.cx_mut(), resolution_value) };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Call `Promise.reject(value)` using the original `Promise` constructor.
    pub fn call_original_reject(
        scope: &'s Scope<'_>,
        rejection_value: impl ToJSVal<'s>,
    ) -> Result<Self, ExnThrown> {
        let rejection_value = rejection_value.to_jsval_throwing(scope)?;
        let obj = unsafe { wrappers2::CallOriginalPromiseReject(scope.cx_mut(), rejection_value) };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Call the original `Promise.prototype.then` with the given handlers.
    ///
    /// Returns a new promise for the result.
    pub fn then(
        &self,
        scope: &'s Scope<'_>,
        on_fulfilled: Option<Object<'_>>,
        on_rejected: Option<Object<'_>>,
    ) -> Result<Self, ExnThrown> {
        let obj = unsafe {
            wrappers2::CallOriginalPromiseThen(
                scope.cx_mut(),
                self.handle(),
                on_fulfilled.map_or(HandleObject::null(), |o| o.handle()),
                on_rejected.map_or(HandleObject::null(), |o| o.handle()),
            )
        };
        unsafe { Self::from_mozjs_rval(scope, obj) }
    }

    /// Create a `Promise.all`-style promise from a vector of promises.
    ///
    /// # Safety
    ///
    /// `promises` must be a handle to a valid `ObjectVector`.
    pub unsafe fn wait_for_all(
        scope: &'s Scope<'_>,
        promises: mozjs::jsapi::HandleObjectVector,
    ) -> Result<Self, ExnThrown> {
        let obj = wrappers2::GetWaitForAllPromise(scope.cx_mut(), promises);
        NonNull::new(obj)
            .map(|nn| Self::from_handle_unchecked(scope.root_object(nn)))
            .ok_or(ExnThrown)
    }

    /// Create a `Promise.all`-style promise that settles once every promise in
    /// `promises` settles: it fulfills with their values, or rejects as soon as
    /// one of them rejects. This is the spec's "getting a promise to wait for
    /// all" operation.
    pub fn wait_for_all_from(
        scope: &'s Scope<'_>,
        promises: &[HandleObject],
    ) -> Result<Self, ExnThrown> {
        let vector = mozjs::rust::RootedObjectVectorWrapper::new(scope.cx_mut());
        for promise in promises {
            if !vector.append(promise.get()) {
                return Err(OutOfMemory.throw(scope));
            }
        }
        // SAFETY: `vector` is a live, valid ObjectVector for the duration of the call;
        // `GetWaitForAllPromise` reads it synchronously.
        unsafe { Self::wait_for_all(scope, vector.handle()) }
    }

    /// Get the `Promise` constructor for the current realm.
    pub fn constructor(
        scope: &'s Scope<'_>,
    ) -> Result<mozjs::gc::Handle<'s, *mut JSObject>, ExnThrown> {
        let obj = unsafe { wrappers2::GetPromiseConstructor(scope.cx_mut()) };
        NonNull::new(obj)
            .map(|p| scope.root_object(p))
            .ok_or(ExnThrown)
    }

    /// Get the `Promise.prototype` for the current realm.
    pub fn prototype(
        scope: &'s Scope<'_>,
    ) -> Result<mozjs::gc::Handle<'s, *mut JSObject>, ExnThrown> {
        let obj = unsafe { wrappers2::GetPromisePrototype(scope.cx_mut()) };
        NonNull::new(obj)
            .map(|p| scope.root_object(p))
            .ok_or(ExnThrown)
    }

    /// Queue a future that will resolve or reject a JS `Promise`.
    ///
    /// Stores the `Promise` in a `RootedHeap<Promise>` for GC safety
    /// and queues the future for [`poll_pending_futures`] to drive.
    pub fn spawn(&self, js_promise: PromiseFuture) {
        let pending = PendingPromise {
            owner: current_future_owner(|owner_cell| owner_cell.get()),
            promise: RootedHeap::new(*self),
            future: js_promise.future,
            origin: crate::stack::current_origin(),
        };
        pending_futures(|f| f.borrow_mut().push(pending));
    }

    /// Like [`Promise::spawn`], but polls the future once right away, with a waker
    /// that does nothing. The work of the future's first poll, such as starting host
    /// I/O, happens before this returns. The event loop's later polls register its
    /// own waker, and settle the promise as for [`Promise::spawn`].
    ///
    /// The future must register the waker of every poll that returns `Pending`,
    /// not only the first one's, or it is never woken.
    pub fn spawn_polled(&self, js_promise: PromiseFuture) {
        let mut future = js_promise.future;
        let mut first_poll = std::task::Context::from_waker(std::task::Waker::noop());
        if let std::task::Poll::Ready(outcome) = future.as_mut().poll(&mut first_poll) {
            future = Box::pin(std::future::ready(outcome));
        }
        self.spawn(PromiseFuture { future });
    }
}

crate::gc::handle::deref_to_object!(Promise);

crate::gc::handle::from_jsval_via_cast!(Promise, c"Value isn't a Promise");

/// The WebIDL [`Promise<T>`](https://webidl.spec.whatwg.org/#idl-promise) type:
/// a `Promise<'s>` that carries the type its value is checked against.
///
/// Instead of immediately type-checking the value, it's wrapped in a `Promise`
/// with a resolution callback that checks the value and rejects if it's not of
/// the right type. Consumers of a `PromiseOf<'s, T>` are guaranteed to get a
/// resolution value that is of type `T`, or a rejection.
///
/// The builtins macros perform this conversion wherever a `PromiseOf<'_, T>`
/// appears as a method parameter or a `#[webidl_dictionary]` member, so the
/// code behind it receives the already-wrapped promise. A bare `Promise<'_>` in
/// either position is the same conversion without the check, for `Promise<any>`
/// and `Promise<undefined>` (whose `T` every value converts to).
///
/// `PromiseOf<'s, T>` derefs to [`Promise<'s>`](crate::Promise).
pub struct PromiseOf<'s, T> {
    promise: crate::Promise<'s>,
    /// `T` is a witness for the check the promise carries, not something the
    /// handle owns, so it must not constrain variance or auto traits.
    value_type: PhantomData<fn() -> T>,
}

impl<'s, T> PromiseOf<'s, T> {
    /// [Convert a JavaScript value to a `Promise<T>`][convert], then arrange
    /// for the eventual value to be checked against `T`.
    ///
    /// `check` is the fulfillment reaction: it converts the value to `T` and
    /// returns it, or rejects. It is supplied by the calling macro, which knows
    /// `T` syntactically; [`check_promise_value_type`] is what it runs. Nothing
    /// ties `check` to `T` — pairing them is the caller's obligation, which is
    /// why this is meant for generated code rather than hand-written builtins.
    ///
    /// [convert]: https://webidl.spec.whatwg.org/#js-to-promise
    #[doc(hidden)]
    pub fn new(
        scope: &'s Scope<'_>,
        value: impl ToJSVal<'s>,
        check: Callback,
    ) -> Result<Self, ExnThrown> {
        // Steps 1–2: `NewPromiseCapability(%Promise%)`, then resolve it with the value.
        let wrapped = Stack::<Promise>::new_resolved_with_value(scope, value)?;
        // `react` to the wrapper with the conversion to `T` as its fulfillment steps. Resolving
        // with a non-thenable settles `wrapped` immediately, so the check runs in the next
        // microtask; resolving with a thenable defers it to that thenable's resolution.
        let on_fulfilled =
            crate::class::get_or_init_shared_function(scope, check as usize, |scope| {
                crate::Function::new_callback(scope, c"", 1, check, HandleValue::undefined())
            })?;
        // A `None` rejection reaction forwards the rejection unchanged, which is what the spec's
        // `onRejectedSteps` amount to for a caller that supplies no rejection steps.
        let checked = wrapped.then(scope, Some(*on_fulfilled), None)?;
        Ok(Self {
            promise: checked,
            value_type: PhantomData,
        })
    }

    /// The underlying promise resolving to the type-checked result.
    pub fn promise(&self) -> crate::Promise<'s> {
        self.promise
    }
}

/// The body of the fulfillment reaction [`PromiseOf::new`] attaches: convert
/// `value` to `T` and hand it back, per [reacting to a promise][react]'s
/// `onFulfilledSteps` ("convert V to an IDL value of type T", then "return
/// result, converted to a JavaScript value"). A failed conversion throws, which
/// rejects the promise the method holds.
///
/// The reaction itself can't be generic — a [`Callback`] is a plain `fn`
/// pointer, and a rooted `T` ties its lifetime to the scope it was rooted in,
/// which no `for<'a>` bound can express. Callers wrap this in a `fn` item
/// instantiated for one concrete `T`, which is what the builtins macros emit.
/// `type_name` is how they spell `T` for the failure message.
///
/// [react]: https://webidl.spec.whatwg.org/#dfn-perform-steps-once-promise-is-settled
pub fn check_promise_value_type<'s, 'v, T>(
    scope: &'s Scope<'s>,
    value: HandleValue<'v>,
    type_name: &str,
) -> Result<crate::native::Value, ExnThrown>
where
    T: crate::conversion::FromJSVal<'s, 'v, Config = ()> + ToJSVal<'s>,
{
    match T::from_jsval(scope, value, ()) {
        Ok(_) => Ok(value.get()),
        Err(_) => {
            // Clear any exception the attempted conversion might have thrown, and throw a more
            // descriptive one.
            crate::exception::clear(scope);
            let _ = crate::exception::take_pending_or_undefined(scope);
            let message = std::ffi::CString::new(format!(
                "promise resolved with a value that is not a {type_name}"
            ))
            .unwrap_or_else(|_| c"promise resolved with a value of the wrong type".into());
            Err(crate::error::throw_type_error(scope, &message))
        }
    }
}

impl<T> Clone for PromiseOf<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for PromiseOf<'_, T> {}

impl<'s, T> std::ops::Deref for PromiseOf<'s, T> {
    type Target = crate::Promise<'s>;
    fn deref(&self) -> &Self::Target {
        &self.promise
    }
}

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;

/// Callback that sets a resolved value on a `MutableHandleValue`.
type ResolveCallback = Box<dyn for<'s> FnOnce(&'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown>>;

/// A pending promise paired with its future.
pub(crate) struct PendingPromise {
    /// The id of the event loop that owns the future (the loop active when it was spawned), so
    /// concurrent per-request loops drive and settle only their own futures. Id 0 means unowned
    /// (spawned with no active loop), and any loop can drive it.
    owner: u64,
    promise: RootedHeap<Promise>,
    future: Pin<Box<dyn Future<Output = PromiseOutcome> + 'static>>,
    /// Where the future was spawned, if origins were being recorded then. See
    /// [`crate::stack::record_origins`].
    origin: Option<std::rc::Rc<str>>,
}

impl PendingPromise {
    fn owned_by(&self, owner: u64) -> bool {
        self.owner == owner || self.owner == 0
    }
}

/// The outcome of an async method — either resolve with a convertible value
/// or reject with an error message.
pub enum PromiseOutcome {
    /// Resolve the promise. The boxed closure sets the return value on the
    /// provided `MutableHandleValue` and returns `true` on success.
    Resolve(ResolveCallback),
    /// Reject the promise with the given error message.
    Reject(String),
}

/// A future that resolves or rejects a JS Promise.
///
/// Use `PromiseFuture::new` in a `#[method]` to return a promise from an async
/// operation. The macro detects the `PromiseFuture` return type and generates
/// code to create a bare SpiderMonkey Promise, spawn the future, and
/// resolve/reject the promise when the future completes.
///
/// The design is async-runtime agnostic: an event loop drives the futures with
/// [`poll_pending_futures`] and settles their promises with
/// [`settle_completed_futures`].
///
/// # Example
///
/// ```rust,ignore
/// #[method]
/// fn slow_greet(&self, name: String) -> PromiseFuture {
///     let greeting = self.prefix.clone();
///     PromiseFuture::new(async move {
///         // simulate async work
///         Ok(format!("{}, {}!", greeting, name))
///     })
/// }
/// ```
pub struct PromiseFuture {
    pub(crate) future: Pin<Box<dyn Future<Output = PromiseOutcome> + 'static>>,
}

impl PromiseFuture {
    /// Create a `PromiseFuture` from a future that returns `Result<T, E>`.
    ///
    /// - `Ok(value)` resolves the promise; `value` must implement `ToJSVal`.
    /// - `Err(e)` rejects the promise with `e.to_string()` as the error message.
    pub fn from_value<T, E, F>(future: F) -> Self
    where
        T: for<'s> ToJSVal<'s> + 'static,
        E: std::fmt::Display + 'static,
        F: Future<Output = Result<T, E>> + 'static,
    {
        PromiseFuture {
            future: Box::pin(async move {
                match future.await {
                    Ok(value) => PromiseOutcome::Resolve(Box::new(move |scope: &Scope| {
                        value.to_jsval_throwing(scope)
                    })),
                    Err(e) => PromiseOutcome::Reject(e.to_string()),
                }
            }),
        }
    }

    /// Create a `PromiseFuture` from a future that yields a [`PromiseOutcome`]
    /// directly.
    ///
    /// Use this when settling needs scope access — e.g. `fetch` builds the JS
    /// `Response` object inside the resolve callback (which receives the
    /// `JSContext`), rather than producing a `ToJSVal` value up front.
    pub fn new<F>(future: F) -> Self
    where
        F: Future<Output = PromiseOutcome> + 'static,
    {
        PromiseFuture {
            future: Box::pin(future),
        }
    }

    /// Create a `PromiseFuture` from a future that resolves to `()` (void).
    pub fn new_void<E, F>(future: F) -> Self
    where
        E: std::fmt::Display + 'static,
        F: Future<Output = Result<(), E>> + 'static,
    {
        PromiseFuture {
            future: Box::pin(async move {
                match future.await {
                    Ok(()) => PromiseOutcome::Resolve(Box::new(move |_scope: &Scope| {
                        Ok(HandleValue::undefined())
                    })),
                    Err(e) => PromiseOutcome::Reject(e.to_string()),
                }
            }),
        }
    }
}

/// This module's share of the crate's thread-local state. See [`crate::tls`].
///
/// Crown: `PendingPromise` is self-rooting via `RootedHeap`, so neither `Vec`
/// needs rooting.
#[crate::allow_unrooted_interior]
pub(crate) struct PromiseTls {
    /// Futures waiting for an event loop to adopt them.
    pending: RefCell<Vec<PendingPromise>>,
    /// The futures the event loop is currently polling.
    active: RefCell<Vec<PendingPromise>>,
    /// The event-loop id that owns futures spawned now (set by the event loop in `with_event_loop`).
    /// 0 when no loop is active.
    owner: Cell<u64>,
}

impl PromiseTls {
    pub(crate) const fn new() -> Self {
        Self {
            pending: RefCell::new(Vec::new()),
            active: RefCell::new(Vec::new()),
            owner: Cell::new(0),
        }
    }
}

fn pending_futures<R>(f: impl FnOnce(&RefCell<Vec<PendingPromise>>) -> R) -> R {
    crate::tls::with(|tls| f(&tls.promise.pending))
}

fn active_futures<R>(f: impl FnOnce(&RefCell<Vec<PendingPromise>>) -> R) -> R {
    crate::tls::with(|tls| f(&tls.promise.active))
}

fn current_future_owner<R>(f: impl FnOnce(&Cell<u64>) -> R) -> R {
    crate::tls::with(|tls| f(&tls.promise.owner))
}

/// Set the owning event-loop id for futures spawned from now on, returning the previous owner so the
/// caller can restore it when the loop's scope ends. The event loop calls this in `with_event_loop`.
pub fn set_current_future_owner(owner: u64) -> u64 {
    current_future_owner(|owner_cell| owner_cell.replace(owner))
}

/// Take all pending promise futures, returning them for execution.
///
/// [`poll_pending_futures`] moves them into the active set it polls.
fn take_pending_futures() -> Vec<PendingPromise> {
    pending_futures(|f| std::mem::take(&mut *f.borrow_mut()))
}

/// Cancel and drop the pending future settling `promise`, if one is queued.
///
/// Dropping the future cancels its in-flight work (e.g. an outstanding HTTP
/// request) and lets the event loop exit once nothing else is pending. Used by
/// `fetch` abort: after rejecting the fetch promise there is no point keeping the
/// request alive. Returns whether a future was found and dropped.
pub fn cancel_pending_future(promise: Stack<Promise>) -> bool {
    let mut removed = false;
    let mut drop_matching = |queue: &RefCell<Vec<PendingPromise>>| {
        queue.borrow_mut().retain(|pending| {
            let matches = pending.promise == promise;
            removed |= matches;
            !matches
        });
    };
    pending_futures(&mut drop_matching);
    active_futures(&mut drop_matching);
    removed
}

/// Drop every pending async-promise future owned by `owner`, which must not be 0.
///
/// Called when an event loop terminates with futures still in flight (e.g. WPT mode stops the loop
/// once a test's completion callback fires, even though a cancelled/disturbed body read is still
/// pending). Dropping each future cancels its in-flight host I/O and unregisters its
/// `RootedHeap` from the engine's extra-roots tracer while the `JSContext` is still alive.
/// Otherwise engine teardown's `finishRoots` would trace a now-freed box and crash.
///
/// Unowned futures (id 0, spawned with no loop active) are left in place: any loop may be
/// waiting on one. [`cancel_all_pending_futures`] drops them.
pub fn cancel_pending_futures_for(owner: u64) {
    debug_assert_ne!(owner, 0, "unowned futures belong to no single loop");
    let drop_owned = |queue: &RefCell<Vec<PendingPromise>>| {
        queue.borrow_mut().retain(|pending| pending.owner != owner);
    };
    pending_futures(drop_owned);
    active_futures(drop_owned);
}

/// Drop every pending async-promise future, whatever its owner. For runtime teardown, which must
/// call it while the `JSContext` is still alive, for the reason [`cancel_pending_futures_for`]
/// gives.
pub fn cancel_all_pending_futures() {
    pending_futures(|f| f.borrow_mut().clear());
    active_futures(|a| a.borrow_mut().clear());
}

/// Whether any async-promise future owned by `owner` (or unowned) is pending (spawned but not yet
/// settled).
///
/// An event loop uses this to stay alive while *its own* async I/O (e.g. a `fetch`) is in flight,
/// even with no tasks or timers — without being held alive by another request's loop's futures.
pub fn has_pending_futures(owner: u64) -> bool {
    let owned_by_caller = |future: &PendingPromise| future.owned_by(owner);
    active_futures(|a| a.borrow().iter().any(owned_by_caller))
        || pending_futures(|f| f.borrow().iter().any(owned_by_caller))
}

/// The origins of the async-promise futures owned by `owner` (or unowned) that are pending, one
/// per future, with `None` for a future spawned while origins were not being recorded. See
/// [`crate::stack::record_origins`].
pub fn pending_future_origins(owner: u64) -> Vec<Option<std::rc::Rc<str>>> {
    let mut origins = Vec::new();
    let mut collect = |queue: &RefCell<Vec<PendingPromise>>| {
        origins.extend(
            queue
                .borrow()
                .iter()
                .filter(|future| future.owned_by(owner))
                .map(|future| future.origin.clone()),
        );
    };
    active_futures(&mut collect);
    pending_futures(&mut collect);
    origins
}

/// A promise object whose future completed, paired with its outcome — returned by
/// [`poll_pending_futures`] for [`settle_completed_futures`] to settle.
pub type CompletedFuture = (RootedHeap<Promise>, PromiseOutcome);

/// Poll the async-promise futures owned by `owner` (or unowned, id 0) one turn: adopt any newly
/// spawned futures, poll the matching ones with `task_cx`, and return those that completed. Futures
/// owned by another loop are left untouched for that loop to drive.
///
/// A poll can run JS, such as a stream element's lowering, so the caller polls with the owning loop
/// active. It settles the returned completions with [`settle_completed_futures`] with that loop
/// active too, so a reaction (a timer, or releasing the loop's interest) reaches the right loop.
///
/// The event loop calls this from inside its asynchronous wait, so the futures are polled with a
/// real waker (their I/O readiness wakes the loop). `PromiseFuture`-returning builtins (e.g. `fetch`)
/// rely on this being driven.
pub fn poll_pending_futures(
    owner: u64,
    task_cx: &mut std::task::Context<'_>,
) -> Vec<CompletedFuture> {
    // Take the active set out of the thread-local (so a future's poll can't re-borrow it) and adopt
    // any newly spawned futures.
    let mut futures = active_futures(|a| std::mem::take(&mut *a.borrow_mut()));
    futures.append(&mut take_pending_futures());

    let mut still_pending: Vec<PendingPromise> = Vec::with_capacity(futures.len());
    let mut completed: Vec<CompletedFuture> = Vec::new();
    for mut pending in futures {
        // Leave another loop's future for that loop's wait to poll and settle.
        if !pending.owned_by(owner) {
            still_pending.push(pending);
            continue;
        }
        match pending.future.as_mut().poll(task_cx) {
            std::task::Poll::Ready(outcome) => completed.push((pending.promise, outcome)),
            std::task::Poll::Pending => still_pending.push(pending),
        }
    }

    // Restore the still-pending futures. Anything spawned during the polls above landed in
    // `pending` and is adopted on the next call, which the wake below brings about.
    active_futures(|a| a.borrow_mut().append(&mut still_pending));
    if pending_futures(|f| !f.borrow().is_empty()) {
        task_cx.waker().wake_by_ref();
    }
    completed
}

/// Settle promises whose futures completed (from [`poll_pending_futures`]): resolve/reject each and
/// drain the reactions they queue. Must be called with the owning event loop active so a reaction
/// that touches the loop (a timer, releasing interest) reaches the right loop.
pub fn settle_completed_futures(scope: &Scope<'_>, completed: Vec<CompletedFuture>) {
    if completed.is_empty() {
        return;
    }
    // Each `boxed` keeps its promise rooted until settled.
    for (boxed, outcome) in completed {
        settle_promise(scope, boxed.get(scope), outcome);
    }
    // Settling queues the promises' reactions as microtasks; drain them here so the event loop's
    // next `step` sees an empty job queue (mirroring how `step` drains after each task). An
    // exception left pending (a failed resolve/reject above, or job-level fallout) is reported —
    // not silently dropped — matching the event loop's uncaught-exception handling.
    crate::jobs::run_jobs(scope);
    crate::exception::report_and_clear(scope, "async promise settle");
}

/// Settle one completed promise future: resolve with the produced value, or
/// reject with a `TypeError` carrying the error message.
fn settle_promise(scope: &Scope<'_>, promise: Stack<Promise>, outcome: PromiseOutcome) {
    match outcome {
        PromiseOutcome::Resolve(resolve) => {
            match resolve(scope) {
                Ok(val) => {
                    let _ = promise.resolve(scope, val);
                }
                Err(_) => {
                    // The resolve callback left a pending exception; reject with it.
                    if let Some(error) = crate::exception::take_pending(scope) {
                        let _ = promise.reject(scope, error);
                    }
                }
            }
        }
        PromiseOutcome::Reject(message) => {
            let _ = crate::error::TypeError(message).throw(scope);
            if let Some(error) = crate::exception::take_pending(scope) {
                let _ = promise.reject(scope, error);
            }
        }
    }
}
