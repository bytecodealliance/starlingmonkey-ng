// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Helpers the modules share that have no equivalent in the `js` crate: reading the receiver and
//! JS values the way Node's JS code does, the bytes of written chunks, the operations that run on
//! the event loop, and typed property reads.
//!
//! [`Props`] reads caller options, and the Node-public properties that Node's own code reads
//! dynamically, such as an `fs.ReadStream`'s `fd` or an `AbortSignal`'s `aborted`. Instance state
//! is in the Rust data of a `#[jsclass]` instead: a class's private data, or a
//! `#[jsclass(hidden)]` payload that [`js::function::cast_payload`] recovers. Properties that
//! mirror that state are written, and never read back.

use std::ffi::CStr;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::ExnThrown;
use js::function::{Callback, CallbackArgs};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::Object;

/// Typed reads of caller options and Node-public properties.
pub(crate) trait Props {
    /// Whether `self[name]` is `true`.
    fn flag(&self, scope: &Scope<'_>, name: &'static CStr) -> Result<bool, ExnThrown>;
    /// `self[name]` if it is a number, and 0 otherwise.
    fn num(&self, scope: &Scope<'_>, name: &'static CStr) -> Result<f64, ExnThrown>;
    /// `self[name]` as an object, if it is one.
    fn obj<'s>(
        &self,
        scope: &'s Scope<'_>,
        name: &'static CStr,
    ) -> Result<Option<Object<'s>>, ExnThrown>;
    /// `self[name]`.
    fn val<'s>(
        &self,
        scope: &'s Scope<'_>,
        name: &'static CStr,
    ) -> Result<HandleValue<'s>, ExnThrown>;
}

impl Props for Object<'_> {
    fn flag(&self, scope: &Scope<'_>, name: &'static CStr) -> Result<bool, ExnThrown> {
        Ok(is_true(self.get_property(scope, name)?))
    }

    fn num(&self, scope: &Scope<'_>, name: &'static CStr) -> Result<f64, ExnThrown> {
        let value = self.get_property(scope, name)?;
        Ok(if value.is_number() {
            value.to_number()
        } else {
            0.0
        })
    }

    fn obj<'s>(
        &self,
        scope: &'s Scope<'_>,
        name: &'static CStr,
    ) -> Result<Option<Object<'s>>, ExnThrown> {
        let value = self.get_property(scope, name)?;
        Ok(Object::from_value(scope, value).ok())
    }

    fn val<'s>(
        &self,
        scope: &'s Scope<'_>,
        name: &'static CStr,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        self.get_property(scope, name)
    }
}

/// The `this` of a callback as an object, or a `TypeError` if it isn't one.
pub(crate) fn this_object<'s>(
    scope: &'s Scope<'_>,
    args: &js::function::CallbackArgs<'_>,
) -> Result<Object<'s>, ExnThrown> {
    Object::from_value(scope, args.this())
        .map_err(|_| js::error::TypeError("the receiver is not an object".into()).throw(scope))
}

/// The realm's constructor of the standard class `key`.
pub(crate) fn class_object<'s>(
    scope: &'s Scope<'_>,
    key: js::class_spec::JSProtoKey,
) -> Result<Object<'s>, ExnThrown> {
    Object::from_handle(js::class::get_class_object(scope, key)?).ok_or(ExnThrown)
}

/// The elements of the array-like object `value`, as `CreateListFromArrayLike` reads them for
/// `Reflect.apply`: `value[0]` to `value[length - 1]`. Throws a `TypeError` if `value` isn't an
/// object.
pub(crate) fn list_from_array_like<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Vec<HandleValue<'s>>, ExnThrown> {
    let object = Object::from_value(scope, value).map_err(|_| {
        js::error::TypeError("CreateListFromArrayLike called on non-object".into()).throw(scope)
    })?;
    let length = object.get_property(scope, c"length")?;
    let length = f64::from_jsval(scope, length, ()).map_err(|e| e.throw(scope))?;
    let length = if length.is_nan() || length <= 0.0 {
        0
    } else {
        length.min(f64::from(u32::MAX)) as u32
    };
    (0..length)
        .map(|index| object.get_element(scope, index))
        .collect()
}

/// Whether `value` is `true`.
pub(crate) fn is_true(value: HandleValue<'_>) -> bool {
    value.is_boolean() && value.to_boolean()
}

/// Whether `object[name]` is a function.
pub(crate) fn has_method(
    scope: &Scope<'_>,
    object: Object<'_>,
    name: &'static CStr,
) -> Result<bool, ExnThrown> {
    Ok(js::function::is_callable(object.get_property(scope, name)?))
}

/// Whether `object[name]` is truthy.
pub(crate) fn truthy(
    scope: &Scope<'_>,
    object: Object<'_>,
    name: &'static CStr,
) -> Result<bool, ExnThrown> {
    Ok(js::conversion::to_boolean(
        object.get_property(scope, name)?,
    ))
}

/// Whether `value` is the string `text`.
pub(crate) fn is_str(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    text: &CStr,
) -> Result<bool, ExnThrown> {
    if !value.is_string() {
        return Ok(false);
    }
    js::JSString::from_value(scope, value.get())?.equals_ascii(scope, text)
}

/// `value` as a Rust string, if it is a string.
pub(crate) fn rust_string(scope: &Scope<'_>, value: HandleValue<'_>) -> Option<String> {
    value
        .is_string()
        .then(|| String::from_jsval(scope, value, ()).ok())
        .flatten()
}

/// Call `callee` with `args` and an undefined `this`, and report an exception it throws as
/// uncaught in a microtask, as Node.js reports an exception a callback throws. Returns the result
/// if the call completed.
pub(crate) fn call_reporting<'s>(
    scope: &'s Scope<'_>,
    callee: impl ToJSVal<'s>,
    args: &[impl ToJSVal<'s>],
) -> Result<Option<HandleValue<'s>>, ExnThrown> {
    match js::Function::call(scope, js::value::undefined(), callee, args) {
        Ok(result) => Ok(Some(result)),
        Err(ExnThrown) => {
            let error = js::exception::take_pending_or_undefined(scope);
            core_runtime::event_loop::timer::rethrow_in_microtask(scope, error)?;
            Ok(None)
        }
    }
}

/// A function that does nothing, the same one for every caller in a global.
pub(crate) fn noop_function<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    shared_function(scope, noop, 0)
}

/// The body of [`noop_function`].
fn noop(
    _scope: &Scope<'_>,
    _args: js::function::CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(js::value::undefined())
}

/// The tick callback of [`emit_later`], whose payload is the emitter: emit the event and the
/// arguments it got.
fn emit_tick(
    scope: &Scope<'_>,
    args: js::function::CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let emitter = js::function::cast_payload::<js::Object>(scope, payload);
    let values: Vec<HandleValue<'_>> = (0..args.len()).map(|index| args.get(index)).collect();
    js::Function::call_by_name(scope, emitter.handle(), c"emit", &values)?;
    Ok(js::value::undefined())
}

/// Emit `event` with `args` on `emitter` in the tick queue, through its `emit` method.
pub(crate) fn emit_later(
    scope: &Scope<'_>,
    emitter: Object<'_>,
    event: &'static CStr,
    args: &[HandleValue<'_>],
) -> Result<(), ExnThrown> {
    let mut values = Vec::with_capacity(args.len() + 1);
    values.push(js::class::get_or_init_atom(scope, event)?);
    values.extend_from_slice(args);
    js::jobs::queue_native_tick(scope, emit_tick, emitter, &values)
}

/// A function of the current global for `body`, created once, so listeners added with it can be
/// removed with it.
pub(crate) fn shared_function<'s>(
    scope: &'s Scope<'_>,
    body: Callback,
    nargs: u32,
) -> Result<Object<'s>, ExnThrown> {
    shared_named_function(scope, body, c"", nargs)
}

/// [`shared_function`] with the name `name`, which the first call for `body` gives it.
pub(crate) fn shared_named_function<'s>(
    scope: &'s Scope<'_>,
    body: Callback,
    name: &'static CStr,
    nargs: u32,
) -> Result<Object<'s>, ExnThrown> {
    let function =
        js::class::get_or_init_shared_function(scope, body as *const () as usize, |scope| {
            js::Function::new_callback(scope, name, nargs, body, ())
        })?;
    Ok(*function)
}

/// `value` as an array, if it is one.
pub(crate) fn as_array<'s>(scope: &'s Scope<'_>, value: HandleValue<'_>) -> Option<js::Array<'s>> {
    Object::from_value(scope, *value)
        .ok()?
        .cast::<js::Array>()
        .ok()
}

/// The elements of `value`, read by index up to its `length`, if `Array.isArray` accepts it.
pub(crate) fn array_elements<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<Vec<HandleValue<'s>>>, ExnThrown> {
    if !js::Array::is_array_value(scope, value)? {
        return Ok(None);
    }
    let array = Object::from_value(scope, *value).map_err(|e| e.throw(scope))?;
    let length = array.get_property(scope, c"length")?;
    let length = f64::from_jsval_throwing(scope, length, ())? as u32;
    let elements = (0..length)
        .map(|index| array.get_element(scope, index))
        .collect::<Result<_, _>>()?;
    Ok(Some(elements))
}

/// The bytes of a written chunk, a string in `encoding` or an `ArrayBufferView`, and its length as
/// `Buffer.byteLength` computes it, which for a string in an encoding other than UTF-8 comes from
/// the string's length rather than from the bytes. `None` for a chunk of another type, and
/// `ERR_UNKNOWN_ENCODING` for a string in an encoding Node.js doesn't know.
pub(crate) fn chunk_bytes(
    scope: &Scope<'_>,
    chunk: HandleValue<'_>,
    encoding: Option<HandleValue<'_>>,
) -> Result<Option<(Vec<u8>, f64)>, crate::errors::Thrown> {
    if chunk.is_string() {
        let encoding = encoding.filter(|encoding| encoding.is_string());
        let parsed = match encoding {
            Some(name) => {
                let name = String::from_jsval(scope, name, ())?;
                crate::buffer::Encoding::parse(&name).ok_or_else(|| {
                    crate::errors::NodeError::new(
                        crate::errors::ErrorKind::TypeError,
                        "ERR_UNKNOWN_ENCODING",
                        format!("Unknown encoding: {name}"),
                    )
                })?
            }
            None => crate::buffer::Encoding::default(),
        };
        let string = js::JSString::from_value(scope, chunk)?;
        let bytes = crate::buffer::encode_string(scope, string, parsed)?;
        let length = match parsed {
            crate::buffer::Encoding::Utf8 => bytes.len() as f64,
            _ => crate::buffer::buffer::byte_length(scope, chunk, encoding)?,
        };
        return Ok(Some((bytes, length)));
    }
    let Some(view) = Object::from_value(scope, *chunk)
        .ok()
        .and_then(js::ArrayBufferView::from_object)
    else {
        return Ok(None);
    };
    let bytes = view.copy_bytes(scope)?;
    let length = bytes.len() as f64;
    Ok(Some((bytes, length)))
}

/// The receiver of a cancellation of an operation [`spawn_cancellable`] runs. It completes once the
/// operation's [`Canceller`] is used or dropped.
pub(crate) type Cancelled = futures_channel::oneshot::Receiver<()>;

/// Cancels the operation [`spawn_cancellable`] returned it for, when it is used or dropped.
pub(crate) type Canceller = futures_channel::oneshot::Sender<()>;

/// The completion of an operation [`spawn_cancellable`] runs: it gets the operation's target, and
/// the value `settle` returned or the exception it threw.
pub(crate) type OpDone =
    fn(&Scope<'_>, Object<'_>, Result<HandleValue<'_>, HandleValue<'_>>) -> Result<(), ExnThrown>;

/// An operation [`spawn_cancellable`] runs, the payload of its promise reaction.
#[core_runtime::jsclass(hidden)]
pub(crate) struct PendingOp {
    target: js::gc::handle::Heap<js::object::Object>,
    #[no_trace]
    done: Option<OpDone>,
    /// Whether the operation was cancelled, so the reaction does nothing.
    #[no_trace]
    cancelled: bool,
    /// Whether `settle` threw, so the reaction gets the exception.
    #[no_trace]
    rejected: bool,
}

#[core_runtime::jsmethods]
impl PendingOp {}

/// The promise reaction of an operation, whose payload is the [`PendingOp`]: call its completion,
/// unless the operation was cancelled.
fn op_reaction(
    scope: &Scope<'_>,
    args: js::function::CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let op = js::function::cast_payload::<PendingOp>(scope, payload);
    let (target, done, cancelled, rejected) = {
        let data = op.data();
        (
            data.target.get(scope),
            data.done,
            data.cancelled,
            data.rejected,
        )
    };
    let Some(done) = done.filter(|_| !cancelled) else {
        return Ok(js::value::undefined());
    };
    let value = args.get(0);
    done(scope, target, if rejected { Err(value) } else { Ok(value) })?;
    Ok(js::value::undefined())
}

/// Run the operation `operation` creates on the event loop, and call `done` with `target` once it
/// completes, unless the returned [`Canceller`] cancelled it first.
///
/// `operation` gets the [`Cancelled`] receiver, and its future completes with `None` once the
/// operation was cancelled. When the future completes with a value, `settle` runs with `target`
/// and the value as the event loop settles the operation's promise, and returns the value `done`
/// gets. `done` runs as the promise's reaction, in the order of other promise reactions, and gets
/// the exception `settle` threw instead if it threw.
pub(crate) fn spawn_cancellable<T, F, S>(
    scope: &Scope<'_>,
    target: Object<'_>,
    operation: impl FnOnce(Cancelled) -> F,
    settle: S,
    done: OpDone,
) -> Result<Canceller, ExnThrown>
where
    T: 'static,
    F: std::future::Future<Output = Option<T>> + 'static,
    S: for<'r> FnOnce(&'r Scope<'_>, Object<'r>, T) -> Result<HandleValue<'r>, ExnThrown> + 'static,
{
    spawn_operation(scope, target, operation, settle, done, None)
}

/// [`spawn_cancellable`], with the operation keeping the event loop alive only while
/// `referenced` is set.
pub(crate) fn spawn_cancellable_with_ref<T, F, S>(
    scope: &Scope<'_>,
    target: Object<'_>,
    operation: impl FnOnce(Cancelled) -> F,
    settle: S,
    done: OpDone,
    referenced: js::promise::FutureRef,
) -> Result<Canceller, ExnThrown>
where
    T: 'static,
    F: std::future::Future<Output = Option<T>> + 'static,
    S: for<'r> FnOnce(&'r Scope<'_>, Object<'r>, T) -> Result<HandleValue<'r>, ExnThrown> + 'static,
{
    spawn_operation(scope, target, operation, settle, done, Some(referenced))
}

/// The body of [`spawn_cancellable`] and [`spawn_cancellable_with_ref`].
fn spawn_operation<T, F, S>(
    scope: &Scope<'_>,
    target: Object<'_>,
    operation: impl FnOnce(Cancelled) -> F,
    settle: S,
    done: OpDone,
    referenced: Option<js::promise::FutureRef>,
) -> Result<Canceller, ExnThrown>
where
    T: 'static,
    F: std::future::Future<Output = Option<T>> + 'static,
    S: for<'r> FnOnce(&'r Scope<'_>, Object<'r>, T) -> Result<HandleValue<'r>, ExnThrown> + 'static,
{
    let (canceller, cancelled) = futures_channel::oneshot::channel();
    let future = operation(cancelled);
    let op = js::class::create_instance_with::<PendingOpImpl>(scope, |_| PendingOpImpl {
        target: js::gc::handle::Heap::from(target),
        done: Some(done),
        cancelled: false,
        rejected: false,
    })?;
    let rooted = js::gc::handle::RootedHeap::<js::object::Object>::new(op.as_object());
    let promise = js::Promise::new_pending(scope)?;
    let future = js::promise::PromiseFuture::new(async move {
        let output = future.await;
        js::promise::PromiseOutcome::Resolve(Box::new(move |scope: &Scope<'_>| {
            let op = rooted
                .get(scope)
                .cast::<PendingOp>()
                .expect("the operation's payload is a PendingOp");
            let Some(output) = output else {
                op.data_mut().cancelled = true;
                return Ok(HandleValue::undefined());
            };
            let target = op.data().target.get(scope);
            let result = settle(scope, target, output);
            if result.is_err() {
                op.data_mut().rejected = true;
            }
            result
        }))
    });
    match referenced {
        Some(referenced) => promise.spawn_with_ref(future, referenced),
        None => promise.spawn(future),
    }
    let reaction = js::Function::new_callback(scope, c"", 1, op_reaction, op)?;
    promise.add_reactions(scope, Some(*reaction), Some(*reaction))?;
    Ok(canceller)
}

/// The output of `future`, or `None` once `cancelled` completes first.
pub(crate) async fn until_cancelled<O>(
    cancelled: Cancelled,
    future: impl std::future::Future<Output = O>,
) -> Option<O> {
    let cancel = async {
        let _ = cancelled.await;
        None
    };
    futures_lite::future::or(async { Some(future.await) }, cancel).await
}

/// Call `callback` with `args` in the tick queue, if it is a function.
pub(crate) fn next_tick_call(
    scope: &Scope<'_>,
    callback: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<(), ExnThrown> {
    if !js::function::is_callable(callback) {
        return Ok(());
    }
    js::jobs::queue_tick(scope, callback, args)
}

/// `process.nextTick(() => { throw error; })`, whose payload is the error.
pub(crate) fn throw_in_tick(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Err(js::exception::set_pending(
        scope,
        payload,
        js::native::ExceptionStackBehavior::Capture,
    ))
}

/// A `Vec` of `length` default values, or `None` if allocating it fails.
pub(crate) fn try_filled_vec<T: Clone + Default>(length: usize) -> Option<Vec<T>> {
    let mut vec = Vec::new();
    vec.try_reserve_exact(length).ok()?;
    vec.resize(length, T::default());
    Some(vec)
}
