// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:diagnostics_channel`. See <https://nodejs.org/api/diagnostics_channel.html>.
//!
//! A realm keeps its channels by name for as long as it exists, rather than letting a channel
//! without subscribers or references be collected. An exception a subscriber or a store's
//! transform throws is reported as uncaught in a microtask, and the remaining subscribers still
//! run.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{validate_function, NodeError, Thrown};

/// The key of the realm's `Map` of channels by name.
struct Channels;

/// The realm's `Map` of channels by name.
fn channels<'s>(scope: &'s Scope<'_>) -> Result<js::Map<'s>, ExnThrown> {
    if let Some(map) = js::class::realm_object::<Channels>(scope) {
        return map
            .cast::<js::Map>()
            .map_err(|_| js::error::TypeError("not a Map".into()).throw(scope));
    }
    let map = js::Map::new(scope)?;
    js::class::register_realm_object::<Channels>(scope, *map);
    Ok(map)
}

/// The realm's channel named `name`, a string or symbol, created if it doesn't exist yet.
fn channel_named<'s>(scope: &'s Scope<'_>, name: HandleValue<'_>) -> Result<Object<'s>, Thrown> {
    if !name.is_string() && !name.is_symbol() {
        return Err(NodeError::invalid_arg_type(
            scope,
            "channel",
            "one of type string or symbol",
            name,
        )
        .into());
    }
    let map = channels(scope)?;
    let name = scope.root_value(name.get());
    let existing = map.lookup(scope, name)?;
    if let Ok(channel) = Object::from_value(scope, existing) {
        return Ok(channel);
    }
    let ctor = channel::class_constructor(scope)?;
    let created = Function::construct(scope, ctor, &[name])?;
    Ok(Object::from_value(scope, created)?)
}

/// Whether `value` is truthy, as JS's `ToBoolean` converts it.
fn truthy(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
    js::conversion::FromJSVal::from_jsval(scope, value, ()).unwrap_or(false)
}

/// Whether `object` inherits from `Channel.prototype`.
fn is_channel(scope: &Scope<'_>, object: Object<'_>) -> Result<bool, ExnThrown> {
    let ctor = channel::class_constructor(scope)?;
    let proto = Object::from_value(scope, ctor.get_property(scope, c"prototype")?)
        .map_err(|e| e.throw(scope))?;
    let mut current = object.get_prototype(scope)?;
    while let Some(candidate) = current {
        if candidate.as_raw() == proto.as_raw() {
            return Ok(true);
        }
        current = candidate.get_prototype(scope)?;
    }
    Ok(false)
}

/// The elements of `array`, an array object.
fn elements<'s>(
    scope: &'s Scope<'_>,
    array: HandleValue<'_>,
) -> Result<Vec<HandleValue<'s>>, ExnThrown> {
    let Ok(array) = Object::from_value(scope, array) else {
        return Ok(Vec::new());
    };
    let length = array.get_property(scope, c"length")?;
    let length = if length.is_number() {
        length.to_number() as u32
    } else {
        0
    };
    (0..length)
        .map(|index| array.get_element(scope, index))
        .collect()
}

/// Report `error` as uncaught in a microtask.
fn report_later(scope: &Scope<'_>, error: HandleValue<'_>) -> Result<(), ExnThrown> {
    core_runtime::event_loop::timer::rethrow_in_microtask(scope, error)
}

/// Call `callee` with `this` and `args`, and report an exception it throws as uncaught in a
/// microtask. Returns whether the call completed.
fn call_reporting(
    scope: &Scope<'_>,
    this: HandleValue<'_>,
    callee: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<Option<Value>, ExnThrown> {
    match Function::call(scope, this, callee, args) {
        Ok(result) => Ok(Some(result.get())),
        Err(_) => {
            let error = js::exception::take_pending_or_undefined(scope);
            report_later(scope, error)?;
            Ok(None)
        }
    }
}

/// `channel.publish(data)`: call each subscriber with `data` and the channel's name.
fn publish(scope: &Scope<'_>, channel: Object<'_>, data: HandleValue<'_>) -> Result<(), ExnThrown> {
    let subscribers = elements(scope, channel.get_property(scope, c"_subscribers")?)?;
    if subscribers.is_empty() {
        return Ok(());
    }
    let name = channel.get_property(scope, c"name")?;
    let undefined = scope.root_value(js::value::undefined());
    for subscriber in subscribers {
        call_reporting(scope, undefined, subscriber, &[data, name])?;
    }
    Ok(())
}

/// Whether `channel` has subscribers or bound stores.
fn has_subscribers(scope: &Scope<'_>, channel: Object<'_>) -> Result<bool, ExnThrown> {
    if !elements(scope, channel.get_property(scope, c"_subscribers")?)?.is_empty() {
        return Ok(true);
    }
    Ok(stores(scope, channel)?.size(scope) > 0)
}

/// The `Map` of `channel`'s bound stores to their transforms.
fn stores<'s>(scope: &'s Scope<'_>, channel: Object<'_>) -> Result<js::Map<'s>, ExnThrown> {
    Object::from_value(scope, channel.get_property(scope, c"_stores")?)
        .map_err(|e| e.throw(scope))?
        .cast::<js::Map>()
        .map_err(|_| js::error::TypeError("not a Map".into()).throw(scope))
}

/// The body of the callback [`store_entries`] collects entries with, whose payload is the array
/// they are collected in.
fn collect_entry(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let collected = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    let entry = js::Array::with_contents(scope, &[args.get(1), args.get(0)])?;
    let length = elements(scope, payload)?.len() as u32;
    collected.set_element(scope, length, *entry)?;
    Ok(js::value::undefined())
}

/// `channel`'s bound stores and their transforms, in the order they were bound, as an array of
/// `[store, transform]` pairs.
fn store_entries<'s>(
    scope: &'s Scope<'_>,
    channel: Object<'_>,
) -> Result<js::Array<'s>, ExnThrown> {
    let collected = js::Array::with_contents(scope, &[] as &[Value])?;
    let collect = Function::new_callback(scope, c"", 2, collect_entry, *collected)?;
    let undefined = scope.root_value(js::value::undefined());
    stores(scope, channel)?.for_each(scope, (*collect).to_jsval_throwing(scope)?, undefined)?;
    Ok(collected)
}

/// The body of the function each bound store runs its part of `runStores` in, whose payload is
/// `[state, index]`. See [`run_stores_from`].
fn continue_run_stores(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let payload = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    let state =
        Object::from_value(scope, payload.get_element(scope, 0)?).map_err(|e| e.throw(scope))?;
    let index = payload.get_element(scope, 1)?.to_number() as u32;
    run_stores_from(scope, state, index)
}

/// Run the part of `runStores` that starts at the store at `index - 1`: run the rest with that
/// store's value set to its transform of the data, or, at index 0, publish the data and call the
/// function. `state` is `[channel, data, fn, thisArg, args, entries]`, where `entries` is the
/// result of [`store_entries`].
///
/// A transform that throws has its exception reported, and the rest runs without that store's
/// value set.
fn run_stores_from(scope: &Scope<'_>, state: Object<'_>, index: u32) -> Result<Value, ExnThrown> {
    let channel =
        Object::from_value(scope, state.get_element(scope, 0)?).map_err(|e| e.throw(scope))?;
    let data = state.get_element(scope, 1)?;
    if index == 0 {
        publish(scope, channel, data)?;
        let callback = state.get_element(scope, 2)?;
        let this = state.get_element(scope, 3)?;
        let args = elements(scope, state.get_element(scope, 4)?)?;
        return Ok(Function::call(scope, this, callback, &args)?.get());
    }
    let entries = elements(scope, state.get_element(scope, 5)?)?;
    let entry =
        Object::from_value(scope, entries[index as usize - 1]).map_err(|e| e.throw(scope))?;
    let store = entry.get_element(scope, 0)?;
    let transform = entry.get_element(scope, 1)?;
    let value = if transform.is_undefined() {
        Some(scope.root_value(data.get()))
    } else {
        let undefined = scope.root_value(js::value::undefined());
        call_reporting(scope, undefined, transform, &[data])?.map(|value| scope.root_value(value))
    };
    let Some(value) = value else {
        return run_stores_from(scope, state, index - 1);
    };
    let next_state = js::Array::with_contents(
        scope,
        &[
            state.to_jsval_raw_throwing(scope)?,
            js::value::from_f64(f64::from(index - 1)),
        ],
    )?;
    let next = Function::new_callback(scope, c"", 0, continue_run_stores, *next_state)?;
    let store_object = Object::from_value(scope, store).map_err(|e| e.throw(scope))?;
    let run = store_object.get_property(scope, c"run")?;
    let next = (*next).to_jsval_throwing(scope)?;
    Ok(Function::call(scope, store, run, &[value, next])?.get())
}

/// `channel.runStores(data, fn, thisArg, ...args)`.
fn run_stores(
    scope: &Scope<'_>,
    channel: Object<'_>,
    data: HandleValue<'_>,
    callback: HandleValue<'_>,
    this: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<Value, ExnThrown> {
    let entries = store_entries(scope, channel)?;
    let count = elements(scope, (*entries).to_jsval_throwing(scope)?)?.len() as u32;
    let args = js::Array::with_contents(scope, args)?;
    let state = js::Array::with_contents(
        scope,
        &[
            channel.to_jsval_raw_throwing(scope)?,
            data.get(),
            callback.get(),
            this.get(),
            args.to_jsval_raw_throwing(scope)?,
            entries.to_jsval_raw_throwing(scope)?,
        ],
    )?;
    run_stores_from(scope, *state, count)
}

#[core_runtime::jsfunction_class(name = "Channel", class_syntax)]
pub mod channel {
    use super::*;

    /// <https://nodejs.org/api/diagnostics_channel.html#class-channel>: a channel named `name`,
    /// without subscribers or bound stores, which becomes the realm's channel of that name.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        name: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        this.set_property(scope, c"name", name)?;
        let subscribers = js::Array::with_contents(scope, &[] as &[Value])?;
        this.define_property(scope, c"_subscribers", *subscribers, 0)?;
        this.define_property(scope, c"_stores", *js::Map::new(scope)?, 0)?;
        channels(scope)?.insert(scope, name, this.to_jsval_throwing(scope)?)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#channelpublishmessage>
    pub fn publish(
        scope: &Scope<'_>,
        this: Object<'_>,
        message: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        super::publish(scope, this, message)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#channelsubscribeonmessage>
    pub fn subscribe(
        scope: &Scope<'_>,
        this: Object<'_>,
        on_message: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        validate_function(scope, on_message, "subscription")?;
        let mut subscribers = elements(scope, this.get_property(scope, c"_subscribers")?)?;
        subscribers.push(on_message);
        let subscribers = js::Array::with_contents(scope, &subscribers)?;
        Ok(this.define_property(scope, c"_subscribers", *subscribers, 0)?)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#channelunsubscribeonmessage>
    pub fn unsubscribe(
        scope: &Scope<'_>,
        this: Object<'_>,
        on_message: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        let mut subscribers = elements(scope, this.get_property(scope, c"_subscribers")?)?;
        let Some(index) = subscribers
            .iter()
            .position(|subscriber| subscriber.get() == on_message.get())
        else {
            return Ok(false);
        };
        subscribers.remove(index);
        let subscribers = js::Array::with_contents(scope, &subscribers)?;
        this.define_property(scope, c"_subscribers", *subscribers, 0)?;
        Ok(true)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#channelbindstorestore-transform>
    pub fn bind_store(
        scope: &Scope<'_>,
        this: Object<'_>,
        store: HandleValue<'_>,
        transform: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let transform = transform.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        stores(scope, this)?.insert(scope, store, transform)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#channelunbindstorestore>
    pub fn unbind_store(
        scope: &Scope<'_>,
        this: Object<'_>,
        store: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        stores(scope, this)?.delete(scope, store)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#channelrunstorescontext-fn-thisarg-args>
    pub fn run_stores<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        context: HandleValue<'_>,
        callback: HandleValue<'_>,
        this_arg: Option<HandleValue<'_>>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        validate_function(scope, callback, "fn")?;
        let this_arg = this_arg.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        Ok(scope.root_value(super::run_stores(
            scope, this, context, callback, this_arg, &args,
        )?))
    }

    /// Define the `hasSubscribers` accessor.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let getter =
            Function::new_callback(scope, c"get hasSubscribers", 0, channel_has_subscribers, ())?;
        proto.define_accessor(scope, c"hasSubscribers", Some(*getter), None, 0)
    }
}

/// `channel.hasSubscribers`.
fn channel_has_subscribers(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    Ok(js::value::from_bool(has_subscribers(scope, this)?))
}

/// The names of a tracing channel's channels, as properties and in channel names.
const TRACING_EVENTS: [(&std::ffi::CStr, &str); 5] = [
    (c"start", "start"),
    (c"end", "end"),
    (c"asyncStart", "asyncStart"),
    (c"asyncEnd", "asyncEnd"),
    (c"error", "error"),
];

/// The channel `tracing` has for `event`, one of [`TRACING_EVENTS`].
fn tracing_part<'s>(
    scope: &'s Scope<'_>,
    tracing: Object<'_>,
    event: &'static std::ffi::CStr,
) -> Result<Object<'s>, ExnThrown> {
    Object::from_value(scope, tracing.get_property(scope, event)?).map_err(|e| e.throw(scope))
}

/// Whether any of `tracing`'s channels has subscribers.
fn tracing_has_subscribers(scope: &Scope<'_>, tracing: Object<'_>) -> Result<bool, ExnThrown> {
    for (event, _) in TRACING_EVENTS {
        if has_subscribers(scope, tracing_part(scope, tracing, event)?)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Set `context.error` to the pending exception, publish `context` on `tracing`'s `error`
/// channel, and leave the exception pending again.
fn publish_error(
    scope: &Scope<'_>,
    tracing: Object<'_>,
    context: Object<'_>,
) -> Result<ExnThrown, ExnThrown> {
    let error = js::exception::take_pending_or_undefined(scope);
    context.set_property(scope, c"error", error)?;
    let context_value = context.to_jsval_throwing(scope)?;
    publish(
        scope,
        tracing_part(scope, tracing, c"error")?,
        context_value,
    )?;
    Ok(js::exception::set_pending(
        scope,
        error,
        js::native::ExceptionStackBehavior::DoNotCapture,
    ))
}

/// The `context` argument of the `trace*` methods: the object passed, or a new one.
fn trace_context<'s>(
    scope: &'s Scope<'_>,
    context: Option<HandleValue<'_>>,
) -> Result<Object<'s>, ExnThrown> {
    match context.filter(|context| !context.is_undefined()) {
        Some(context) => {
            Object::from_value(scope, scope.root_value(context.get())).map_err(|e| e.throw(scope))
        }
        None => Object::new_plain(scope),
    }
}

/// The state the bodies of the `trace*` methods share: `[tracing, fn, context, thisArg, args]`.
fn trace_state<'s>(
    scope: &'s Scope<'_>,
    tracing: Object<'_>,
    callback: HandleValue<'_>,
    context: Object<'_>,
    this: HandleValue<'_>,
    args: &[HandleValue<'_>],
) -> Result<js::Array<'s>, ExnThrown> {
    let args = js::Array::with_contents(scope, args)?;
    js::Array::with_contents(
        scope,
        &[
            tracing.to_jsval_raw_throwing(scope)?,
            callback.get(),
            context.to_jsval_raw_throwing(scope)?,
            this.get(),
            args.to_jsval_raw_throwing(scope)?,
        ],
    )
}

/// The parts of a [`trace_state`] array.
struct TraceState<'s> {
    tracing: Object<'s>,
    callback: HandleValue<'s>,
    context: Object<'s>,
    this: HandleValue<'s>,
    args: Vec<HandleValue<'s>>,
}

impl<'s> TraceState<'s> {
    fn read(scope: &'s Scope<'_>, state: HandleValue<'_>) -> Result<Self, ExnThrown> {
        let state = Object::from_value(scope, state).map_err(|e| e.throw(scope))?;
        let object = |index| -> Result<Object<'s>, ExnThrown> {
            Object::from_value(scope, state.get_element(scope, index)?).map_err(|e| e.throw(scope))
        };
        Ok(TraceState {
            tracing: object(0)?,
            callback: state.get_element(scope, 1)?,
            context: object(2)?,
            this: state.get_element(scope, 3)?,
            args: elements(scope, state.get_element(scope, 4)?)?,
        })
    }

    /// Call the traced function, publishing an exception it throws on the `error` channel, and
    /// publish the context on the `end` channel either way. If `record_result`, the context's
    /// `result` is set to the function's result before `end` is published.
    fn call(
        &self,
        scope: &'s Scope<'_>,
        record_result: bool,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        let result = Function::call(scope, self.this, self.callback, &self.args);
        let result = match result {
            Ok(result) => {
                if record_result {
                    self.context.set_property(scope, c"result", result)?;
                }
                Ok(result)
            }
            Err(_) => Err(publish_error(scope, self.tracing, self.context)?),
        };
        let pending = js::exception::take_pending(scope);
        let context = self.context.to_jsval_throwing(scope)?;
        publish(scope, tracing_part(scope, self.tracing, c"end")?, context)?;
        if let Some(pending) = pending {
            js::exception::set_pending(
                scope,
                pending,
                js::native::ExceptionStackBehavior::DoNotCapture,
            );
        }
        result
    }
}

/// The body `traceSync` runs inside the `start` channel's stores, whose payload is a
/// [`trace_state`]: call the function, recording its result in the context.
fn trace_sync_body(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(TraceState::read(scope, payload)?.call(scope, true)?.get())
}

/// Publish `context` on `tracing`'s `asyncStart` and `asyncEnd` channels.
fn publish_async(
    scope: &Scope<'_>,
    tracing: Object<'_>,
    context: Object<'_>,
) -> Result<(), ExnThrown> {
    let context = context.to_jsval_throwing(scope)?;
    publish(scope, tracing_part(scope, tracing, c"asyncStart")?, context)?;
    publish(scope, tracing_part(scope, tracing, c"asyncEnd")?, context)
}

/// The fulfillment handler of the promise `tracePromise` returns, whose payload is
/// `[tracing, context]`: set `context.result`, and publish the async channels.
fn trace_promise_fulfilled(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let payload = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    let tracing =
        Object::from_value(scope, payload.get_element(scope, 0)?).map_err(|e| e.throw(scope))?;
    let context =
        Object::from_value(scope, payload.get_element(scope, 1)?).map_err(|e| e.throw(scope))?;
    context.set_property(scope, c"result", args.get(0))?;
    publish_async(scope, tracing, context)?;
    Ok(args.get(0).get())
}

/// The rejection handler of the promise `tracePromise` returns, whose payload is
/// `[tracing, context]`: set `context.error`, publish the `error` and async channels, and reject
/// with the same reason.
fn trace_promise_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let payload = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    let tracing =
        Object::from_value(scope, payload.get_element(scope, 0)?).map_err(|e| e.throw(scope))?;
    let context =
        Object::from_value(scope, payload.get_element(scope, 1)?).map_err(|e| e.throw(scope))?;
    context.set_property(scope, c"error", args.get(0))?;
    let context_value = context.to_jsval_throwing(scope)?;
    publish(
        scope,
        tracing_part(scope, tracing, c"error")?,
        context_value,
    )?;
    publish_async(scope, tracing, context)?;
    Err(js::exception::set_pending(
        scope,
        args.get(0),
        js::native::ExceptionStackBehavior::DoNotCapture,
    ))
}

/// The body `tracePromise` runs inside the `start` channel's stores, whose payload is a
/// [`trace_state`]: call the function, and return a promise for its result that publishes the
/// async channels once it settles.
fn trace_promise_body(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = TraceState::read(scope, payload)?;
    let result = state.call(scope, false)?;
    let promise = js::Promise::call_original_resolve(scope, result)?;
    let handlers = js::Array::with_contents(
        scope,
        &[
            state.tracing.to_jsval_raw_throwing(scope)?,
            state.context.to_jsval_raw_throwing(scope)?,
        ],
    )?;
    let fulfilled = Function::new_callback(scope, c"", 1, trace_promise_fulfilled, *handlers)?;
    let rejected = Function::new_callback(scope, c"", 1, trace_promise_rejected, *handlers)?;
    promise
        .then(scope, Some(*fulfilled), Some(*rejected))?
        .to_jsval_raw_throwing(scope)
}

/// The callback `traceCallback` passes in place of the traced function's, whose payload is
/// `[tracing, context, callback]`: set `context.error` or `context.result` from the arguments, then
/// call the original callback inside the `asyncStart` channel's stores, and publish `asyncEnd`.
fn trace_callback_wrapper(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = Object::from_value(scope, payload).map_err(|e| e.throw(scope))?;
    let tracing =
        Object::from_value(scope, state.get_element(scope, 0)?).map_err(|e| e.throw(scope))?;
    let context =
        Object::from_value(scope, state.get_element(scope, 1)?).map_err(|e| e.throw(scope))?;
    let callback = state.get_element(scope, 2)?;
    let error = args.get(0);
    if !truthy(scope, error) {
        context.set_property(scope, c"result", args.get(1))?;
    } else {
        context.set_property(scope, c"error", error)?;
        let context_value = context.to_jsval_throwing(scope)?;
        publish(
            scope,
            tracing_part(scope, tracing, c"error")?,
            context_value,
        )?;
    }
    let call_args: Vec<HandleValue<'_>> = (0..args.len()).map(|index| args.get(index)).collect();
    let this = scope.root_value(args.this());
    let context_value = context.to_jsval_throwing(scope)?;
    let async_start = tracing_part(scope, tracing, c"asyncStart")?;
    let result = run_stores(
        scope,
        async_start,
        context_value,
        callback,
        this,
        &call_args,
    )
    .map(|value| scope.root_value(value));
    let pending = js::exception::take_pending(scope);
    publish(
        scope,
        tracing_part(scope, tracing, c"asyncEnd")?,
        context_value,
    )?;
    if let Some(pending) = pending {
        js::exception::set_pending(
            scope,
            pending,
            js::native::ExceptionStackBehavior::DoNotCapture,
        );
    }
    result.map(|value| value.get())
}

/// The body `traceCallback` runs inside the `start` channel's stores, whose payload is a
/// [`trace_state`].
fn trace_callback_body(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(TraceState::read(scope, payload)?.call(scope, false)?.get())
}

/// Run `body` with `state` as its payload inside `tracing`'s `start` channel's stores.
fn run_traced(
    scope: &Scope<'_>,
    tracing: Object<'_>,
    context: Object<'_>,
    body: js::function::Callback,
    state: js::Array<'_>,
) -> Result<Value, ExnThrown> {
    let body = Function::new_callback(scope, c"", 0, body, *state)?;
    let body = (*body).to_jsval_throwing(scope)?;
    let undefined = scope.root_value(js::value::undefined());
    let context = context.to_jsval_throwing(scope)?;
    run_stores(
        scope,
        tracing_part(scope, tracing, c"start")?,
        context,
        body,
        undefined,
        &[],
    )
}

#[core_runtime::jsfunction_class(name = "TracingChannel", class_syntax)]
pub mod tracing_channel {
    use super::*;

    /// <https://nodejs.org/api/diagnostics_channel.html#class-tracingchannel>: the channels
    /// `tracing:${name}:start` and so on for a string `name_or_channels`, or the channels of an
    /// object with `start`, `end`, `asyncStart`, `asyncEnd` and `error` properties.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        name_or_channels: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        if name_or_channels.is_string() {
            let name: String = js::conversion::FromJSVal::from_jsval(scope, name_or_channels, ())
                .map_err(|_| ExnThrown)?;
            for (property, event) in TRACING_EVENTS {
                let channel_name =
                    js::JSString::from_str(scope, &format!("tracing:{name}:{event}"))?;
                let channel = channel_named(scope, channel_name.to_jsval_throwing(scope)?)?;
                this.set_property(scope, property, channel)?;
            }
            return Ok(());
        }
        let Ok(channels) = Object::from_value(scope, name_or_channels) else {
            return Err(NodeError::invalid_arg_type(
                scope,
                "nameOrChannels",
                "of type string or an instance of TracingChannel or Object",
                name_or_channels,
            )
            .into());
        };
        for (property, event) in TRACING_EVENTS {
            let channel = channels.get_property(scope, property)?;
            let is_channel = match Object::from_value(scope, channel) {
                Ok(channel) => is_channel(scope, channel)?,
                Err(_) => false,
            };
            if !is_channel {
                return Err(NodeError::invalid_arg_type(
                    scope,
                    &format!("nameOrChannels.{event}"),
                    "an instance of Channel",
                    channel,
                )
                .into());
            }
            this.set_property(scope, property, channel)?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#tracingchannelsubscribesubscribers>
    pub fn subscribe(
        scope: &Scope<'_>,
        this: Object<'_>,
        subscribers: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let subscribers = crate::errors::validate_object(scope, subscribers, "subscribers")?;
        for (property, _) in TRACING_EVENTS {
            let handler = subscribers.get_property(scope, property)?;
            if truthy(scope, handler) {
                channel::subscribe(scope, tracing_part(scope, this, property)?, handler)?;
            }
        }
        Ok(())
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#tracingchannelunsubscribesubscribers>
    pub fn unsubscribe(
        scope: &Scope<'_>,
        this: Object<'_>,
        subscribers: HandleValue<'_>,
    ) -> Result<bool, Thrown> {
        let subscribers = crate::errors::validate_object(scope, subscribers, "subscribers")?;
        let mut done = true;
        for (property, _) in TRACING_EVENTS {
            let handler = subscribers.get_property(scope, property)?;
            if truthy(scope, handler)
                && !channel::unsubscribe(scope, tracing_part(scope, this, property)?, handler)?
            {
                done = false;
            }
        }
        Ok(done)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#tracingchanneltracesyncfn-context-thisarg-args>
    pub fn trace_sync<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        callback: HandleValue<'_>,
        context: Option<HandleValue<'_>>,
        this_arg: Option<HandleValue<'_>>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        validate_function(scope, callback, "fn")?;
        let this_arg = this_arg.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        if !tracing_has_subscribers(scope, this)? {
            return Ok(scope.root_value(Function::call(scope, this_arg, callback, &args)?.get()));
        }
        let context = trace_context(scope, context)?;
        let state = trace_state(scope, this, callback, context, this_arg, &args)?;
        Ok(scope.root_value(run_traced(scope, this, context, trace_sync_body, state)?))
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#tracingchanneltracepromisefn-context-thisarg-args>
    pub fn trace_promise<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        callback: HandleValue<'_>,
        context: Option<HandleValue<'_>>,
        this_arg: Option<HandleValue<'_>>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        validate_function(scope, callback, "fn")?;
        let this_arg = this_arg.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        if !tracing_has_subscribers(scope, this)? {
            return Ok(scope.root_value(Function::call(scope, this_arg, callback, &args)?.get()));
        }
        let context = trace_context(scope, context)?;
        let state = trace_state(scope, this, callback, context, this_arg, &args)?;
        Ok(scope.root_value(run_traced(scope, this, context, trace_promise_body, state)?))
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#tracingchanneltracecallbackfn-position-context-thisarg-args>
    pub fn trace_callback<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        callback: HandleValue<'_>,
        position: Option<f64>,
        context: Option<HandleValue<'_>>,
        this_arg: Option<HandleValue<'_>>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        validate_function(scope, callback, "fn")?;
        let this_arg = this_arg.unwrap_or_else(|| scope.root_value(js::value::undefined()));
        if !tracing_has_subscribers(scope, this)? {
            return Ok(scope.root_value(Function::call(scope, this_arg, callback, &args)?.get()));
        }
        let mut args: Vec<HandleValue<'_>> = args.to_vec();
        let position = position.unwrap_or(-1.0) as i64;
        let index = if position < 0 {
            args.len() as i64 + position
        } else {
            position
        };
        let original = usize::try_from(index)
            .ok()
            .and_then(|index| args.get(index).copied())
            .unwrap_or_else(|| scope.root_value(js::value::undefined()));
        validate_function(scope, original, "callback")?;
        let context = trace_context(scope, context)?;
        let wrapper_state = js::Array::with_contents(
            scope,
            &[
                this.to_jsval_raw_throwing(scope)?,
                context.to_jsval_raw_throwing(scope)?,
                original.get(),
            ],
        )?;
        let wrapper =
            Function::new_callback(scope, c"", 2, trace_callback_wrapper, *wrapper_state)?;
        args[index as usize] = (*wrapper).to_jsval_throwing(scope)?;
        let state = trace_state(scope, this, callback, context, this_arg, &args)?;
        Ok(scope.root_value(run_traced(
            scope,
            this,
            context,
            trace_callback_body,
            state,
        )?))
    }

    /// Define the `hasSubscribers` accessor.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let getter = Function::new_callback(
            scope,
            c"get hasSubscribers",
            0,
            tracing_channel_has_subscribers,
            (),
        )?;
        proto.define_accessor(scope, c"hasSubscribers", Some(*getter), None, 0)
    }
}

/// `tracingChannel.hasSubscribers`.
fn tracing_channel_has_subscribers(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = Object::from_value(scope, args.this()).map_err(|e| e.throw(scope))?;
    Ok(js::value::from_bool(tracing_has_subscribers(scope, this)?))
}

#[core_runtime::jsmodule(node_args, name = "node:diagnostics_channel", default_namespace)]
pub mod module {
    use super::*;

    pub use super::channel as Channel;

    /// <https://nodejs.org/api/diagnostics_channel.html#diagnostics_channelchannelname>
    pub fn channel<'s>(scope: &'s Scope<'_>, name: HandleValue<'_>) -> Result<Object<'s>, Thrown> {
        channel_named(scope, name)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#diagnostics_channelhassubscribersname>:
    /// `false` for a name without a channel, which this doesn't create.
    pub fn has_subscribers(scope: &Scope<'_>, name: HandleValue<'_>) -> Result<bool, ExnThrown> {
        let name = scope.root_value(name.get());
        let existing = channels(scope)?.lookup(scope, name)?;
        match Object::from_value(scope, existing) {
            Ok(channel) => super::has_subscribers(scope, channel),
            Err(_) => Ok(false),
        }
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#diagnostics_channelsubscribename-onmessage>
    pub fn subscribe(
        scope: &Scope<'_>,
        name: HandleValue<'_>,
        on_message: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        super::channel::subscribe(scope, channel_named(scope, name)?, on_message)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#diagnostics_channelunsubscribename-onmessage>
    pub fn unsubscribe(
        scope: &Scope<'_>,
        name: HandleValue<'_>,
        on_message: HandleValue<'_>,
    ) -> Result<bool, Thrown> {
        Ok(super::channel::unsubscribe(
            scope,
            channel_named(scope, name)?,
            on_message,
        )?)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#diagnostics_channeltracingchannelnameorchannels>
    pub fn tracing_channel<'s>(
        scope: &'s Scope<'_>,
        name_or_channels: HandleValue<'_>,
    ) -> Result<Object<'s>, Thrown> {
        let ctor = super::tracing_channel::class_constructor(scope)?;
        let created = Function::construct(scope, ctor, &[name_or_channels])?;
        let created = scope.root_value(created.to_jsval_raw_throwing(scope)?);
        Ok(Object::from_value(scope, created)?)
    }
}
