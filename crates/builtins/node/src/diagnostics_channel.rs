// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:diagnostics_channel`. See <https://nodejs.org/api/diagnostics_channel.html>.
//!
//! A `Channel` and a `TracingChannel` keep their state in Rust. A channel's own data properties
//! `_subscribers`, `_stores`, `name` and `_index` are the ones Node's channels have, and are
//! written when the state changes. A channel with subscribers or bound stores has the prototype of
//! Node's internal `ActiveChannel` class, as in Node.
//!
//! A realm keeps its channels by name in a `Map`, as Node's `WeakRefMap` does: a channel with
//! subscribers or bound stores directly, and other channels through a `WeakRef`, so that a channel
//! nothing else references can be collected. A `FinalizationRegistry` then deletes its entry. An
//! exception a subscriber or a store's transform throws is reported as uncaught in a microtask,
//! and the remaining subscribers still run.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{validate_function, NodeError, Thrown};

/// The key of the realm's `Map` of channels by name.
struct Channels;

/// The key of the realm's `ActiveChannel.prototype`.
struct ActiveChannelPrototype;

/// The realm's `Map` of channels by name, whose values are channels and `WeakRef`s of channels.
fn channels<'s>(scope: &'s Scope<'_>) -> Result<js::Map<'s>, ExnThrown> {
    js::class::realm_object::<Channels>(scope)
        .and_then(|map| map.cast::<js::Map>().ok())
        .ok_or_else(|| js::error::TypeError("the channels are not registered".into()).throw(scope))
}

/// What the realm's channel map needs to hold channels weakly, captured when the realm is set up.
#[core_runtime::jsclass(hidden)]
struct ChannelReferences {
    /// The `WeakRef` constructor.
    weak_ref: Heap<Value>,
    /// `WeakRef.prototype.deref`.
    deref: Heap<Value>,
    /// The `FinalizationRegistry` constructor.
    finalization_registry: Heap<Value>,
    /// `FinalizationRegistry.prototype.register`.
    register: Heap<Value>,
    /// The registry that deletes the entries of collected channels, created with the first
    /// channel.
    registry: Option<Heap<Value>>,
}

#[core_runtime::jsmethods]
impl ChannelReferences {}

/// The realm's [`ChannelReferences`].
fn references<'s>(scope: &'s Scope<'_>) -> Result<ChannelReferences<'s>, ExnThrown> {
    js::class::realm_object::<ChannelReferences>(scope)
        .and_then(|references| references.cast::<ChannelReferences>().ok())
        .ok_or_else(|| js::error::TypeError("the channels are not registered".into()).throw(scope))
}

/// The channel the channel map holds for `entry`: `entry` itself, the channel of a `WeakRef`, or
/// `None` for a collected channel or a missing entry.
fn entry_channel<'s>(
    scope: &'s Scope<'_>,
    entry: HandleValue<'s>,
) -> Result<Option<Channel<'s>>, ExnThrown> {
    let Ok(entry) = Object::from_value(scope, entry) else {
        return Ok(None);
    };
    if let Ok(channel) = entry.cast::<Channel>() {
        return Ok(Some(channel));
    }
    let deref = references(scope)?.data().deref.get(scope);
    let target = Function::call(scope, entry, deref, js::function::EmptyArgs)?;
    Ok(Object::from_value(scope, target)
        .ok()
        .and_then(|target| target.cast::<Channel>().ok()))
}

/// The realm's channel named `name`, if it has one.
fn existing_channel<'s>(
    scope: &'s Scope<'_>,
    name: HandleValue<'_>,
) -> Result<Option<Channel<'s>>, ExnThrown> {
    let entry = channels(scope)?.lookup(scope, scope.root_value(name.get()))?;
    entry_channel(scope, entry)
}

/// Make the channel map hold `channel` under `name` directly if `strong`, and through a `WeakRef`
/// otherwise.
fn hold_channel(
    scope: &Scope<'_>,
    name: HandleValue<'_>,
    channel: Object<'_>,
    strong: bool,
) -> Result<(), ExnThrown> {
    let entry = if strong {
        channel.to_jsval_throwing(scope)?
    } else {
        let weak_ref = references(scope)?.data().weak_ref.get(scope);
        Function::construct(scope, weak_ref, &[channel])?.to_jsval_throwing(scope)?
    };
    channels(scope)?.insert(scope, name, entry)
}

/// Register `channel`, named `name`, with the realm's registry that deletes the entries of
/// collected channels, which is created with the first channel.
fn register_channel(
    scope: &Scope<'_>,
    name: HandleValue<'_>,
    channel: Object<'_>,
) -> Result<(), ExnThrown> {
    let references = references(scope)?;
    let registry = references
        .data()
        .registry
        .as_ref()
        .map(|registry| registry.get(scope));
    let registry = match registry {
        Some(registry) => registry,
        None => {
            let constructor = references.data().finalization_registry.get(scope);
            let cleanup = Function::new_callback(scope, c"", 1, forget_channel, ())?;
            let registry = Function::construct(scope, constructor, &[*cleanup])?;
            let registry = registry.to_jsval_throwing(scope)?;
            references.data_mut().registry = Some(Heap::from(registry.get()));
            registry
        }
    };
    let register = references.data().register.get(scope);
    Function::call(
        scope,
        registry,
        register,
        &[channel.to_jsval_throwing(scope)?, name],
    )?;
    Ok(())
}

/// The cleanup callback of the realm's channel registry, called with the name of a collected
/// channel: delete the name's entry unless it holds a channel again.
fn forget_channel(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let name = args.get(0);
    let map = channels(scope)?;
    let entry = map.lookup(scope, name)?;
    if !entry.is_undefined() && entry_channel(scope, entry)?.is_none() {
        map.delete(scope, name)?;
    }
    Ok(js::value::undefined())
}

/// The realm's channel named `name`, a string or symbol, created if it doesn't exist yet.
fn channel_named<'s>(scope: &'s Scope<'_>, name: HandleValue<'_>) -> Result<Channel<'s>, Thrown> {
    if !name.is_string() && !name.is_symbol() {
        return Err(NodeError::invalid_arg_type(
            scope,
            "channel",
            "one of type string or symbol",
            name,
        )
        .into());
    }
    if let Some(channel) = existing_channel(scope, name)? {
        return Ok(channel);
    }
    Ok(Channel::new(scope, name)?)
}

use crate::support::call_reporting;

/// A store a channel bound, and the transform of its value.
#[js::must_root]
#[derive(core_runtime::Traceable)]
struct BoundStore {
    store: Heap<Value>,
    transform: Heap<Value>,
}

/// A `Channel`. See <https://nodejs.org/api/diagnostics_channel.html#class-channel>.
#[core_runtime::jsclass(name = "Channel", hidden)]
pub struct Channel {
    name: Heap<Value>,
    subscribers: Vec<Heap<Value>>,
    stores: Vec<BoundStore>,
}

#[core_runtime::jsmethods(node_args)]
impl Channel {
    /// <https://nodejs.org/api/diagnostics_channel.html#class-channel>: a channel named `name`,
    /// without subscribers or bound stores, which becomes the realm's channel of that name. The
    /// realm holds it weakly.
    #[constructor]
    pub fn new(&self, scope: &Scope<'_>, name: HandleValue<'_>) -> Result<(), ExnThrown> {
        self.data().name.set(name.get());
        let this = self.as_object();
        this.set_property(scope, c"_subscribers", HandleValue::undefined())?;
        this.set_property(scope, c"_stores", HandleValue::undefined())?;
        this.set_property(scope, c"name", name)?;
        this.set_property(scope, c"_index", HandleValue::undefined())?;
        register_channel(scope, name, this)?;
        hold_channel(scope, name, this, false)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#channelsubscribeonmessage>
    #[method]
    pub fn subscribe(
        &self,
        scope: &Scope<'_>,
        subscription: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        validate_function(scope, subscription, "subscription")?;
        self.data_mut()
            .subscribers
            .push(Heap::from(subscription.get()));
        self.update_mirrors(scope)?;
        Ok(())
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#channelunsubscribeonmessage>
    #[method(length = 0)]
    pub fn unsubscribe(
        &self,
        scope: &Scope<'_>,
        subscription: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        let removed = {
            let mut data = self.data_mut();
            let index = data
                .subscribers
                .iter()
                .position(|subscriber| subscriber.get(scope).get() == subscription.get());
            index.map(|index| data.subscribers.remove(index)).is_some()
        };
        if removed {
            self.update_mirrors(scope)?;
        }
        Ok(removed)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#channelbindstorestore-transform>
    #[method(length = 2)]
    pub fn bind_store(
        &self,
        scope: &Scope<'_>,
        store: HandleValue<'_>,
        transform: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let transform = transform.unwrap_or(HandleValue::undefined());
        {
            let mut data = self.data_mut();
            match data
                .stores
                .iter()
                .position(|bound| bound.store.get(scope).get() == store.get())
            {
                Some(index) => data.stores[index].transform.set(transform.get()),
                None => data.stores.push(BoundStore {
                    store: Heap::from(store.get()),
                    transform: Heap::from(transform.get()),
                }),
            }
        }
        self.update_mirrors(scope)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#channelunbindstorestore>
    #[method(length = 0)]
    pub fn unbind_store(
        &self,
        scope: &Scope<'_>,
        store: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        let removed = {
            let mut data = self.data_mut();
            let index = data
                .stores
                .iter()
                .position(|bound| bound.store.get(scope).get() == store.get());
            index.map(|index| data.stores.remove(index)).is_some()
        };
        if removed {
            self.update_mirrors(scope)?;
        }
        Ok(removed)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#channelhassubscribers>: whether the
    /// channel has subscribers or bound stores.
    #[getter]
    pub fn has_subscribers(&self) -> bool {
        let data = self.data();
        !data.subscribers.is_empty() || !data.stores.is_empty()
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#channelpublishmessage>: call each
    /// subscriber with `message` and the channel's name.
    #[method(length = 0)]
    pub fn publish(&self, scope: &Scope<'_>, message: HandleValue<'_>) -> Result<(), ExnThrown> {
        let (subscribers, name) = {
            let data = self.data();
            if data.subscribers.is_empty() {
                return Ok(());
            }
            let subscribers: Vec<HandleValue<'_>> = data
                .subscribers
                .iter()
                .map(|subscriber| subscriber.get(scope))
                .collect();
            (subscribers, data.name.get(scope))
        };
        for subscriber in subscribers {
            call_reporting(scope, subscriber, &[message.get(), name.get()])?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#channelrunstorescontext-fn-thisarg-args>
    #[method(length = 3)]
    pub fn run_stores<'r>(
        &self,
        scope: &'r Scope<'_>,
        context: HandleValue<'_>,
        callback: HandleValue<'_>,
        this_arg: Option<HandleValue<'_>>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'r>, Thrown> {
        validate_function(scope, callback, "fn")?;
        let this_arg = this_arg.unwrap_or(HandleValue::undefined());
        Ok(self.run_stores_with(scope, context, callback, this_arg, &args)?)
    }
}

impl<'s> Channel<'s> {
    /// Write the channel's `_subscribers` and `_stores` from its state, and give it the prototype
    /// of Node's `ActiveChannel` while it has subscribers or bound stores, and `Channel`'s
    /// otherwise. The realm's channel map holds an active channel directly, and others weakly.
    fn update_mirrors(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let this = self.as_object();
        let name = self.data().name.get(scope);
        hold_channel(scope, name, this, self.has_subscribers())?;
        if !self.has_subscribers() {
            let proto = js::class::get_prototype_object_for::<ChannelImpl>(scope)
                .expect("Channel is registered");
            this.set_prototype(scope, proto.handle())?;
            this.set_property(scope, c"_subscribers", HandleValue::undefined())?;
            return this.set_property(scope, c"_stores", HandleValue::undefined());
        }
        let (subscribers, stores) = {
            let data = self.data();
            let subscribers: Vec<HandleValue<'_>> = data
                .subscribers
                .iter()
                .map(|subscriber| subscriber.get(scope))
                .collect();
            let stores: Vec<(HandleValue<'_>, HandleValue<'_>)> = data
                .stores
                .iter()
                .map(|bound| (bound.store.get(scope), bound.transform.get(scope)))
                .collect();
            (js::Array::with_contents(scope, &subscribers)?, stores)
        };
        let map = js::Map::new(scope)?;
        for (store, transform) in stores {
            map.insert(scope, store, transform)?;
        }
        let active = js::class::realm_object::<ActiveChannelPrototype>(scope)
            .expect("ActiveChannel.prototype is registered");
        this.set_prototype(scope, active.handle())?;
        this.set_property(scope, c"_subscribers", subscribers)?;
        this.set_property(scope, c"_stores", map)
    }

    /// Node's `runStores(data, fn, thisArg, ...args)`: run `fn` with `thisArg` and `args` inside
    /// the bound stores, each set to its transform of `data`, after publishing `data`.
    fn run_stores_with<'r>(
        &self,
        scope: &'r Scope<'_>,
        data: HandleValue<'_>,
        callback: HandleValue<'_>,
        this_arg: HandleValue<'_>,
        args: &[HandleValue<'_>],
    ) -> Result<HandleValue<'r>, ExnThrown> {
        let count = self.data().stores.len() as u32;
        let state = js::class::create_instance_with::<RunStoresImpl>(scope, |_| RunStoresImpl {
            channel: Heap::from(self.as_object()),
            data: Heap::from(data.get()),
            callback: Heap::from(callback.get()),
            this_arg: Heap::from(this_arg.get()),
            args: args.iter().map(|arg| Heap::from(arg.get())).collect(),
        })?;
        run_stores_from(scope, state, count)
    }
}

/// The state of a `runStores` call.
#[core_runtime::jsclass(hidden)]
pub(crate) struct RunStores {
    channel: Heap<js::object::Object>,
    data: Heap<Value>,
    callback: Heap<Value>,
    this_arg: Heap<Value>,
    args: Vec<Heap<Value>>,
}

#[core_runtime::jsmethods]
impl RunStores {}

/// The step of a `runStores` call a bound store's `run` calls next: run the stores before the
/// store at `index`.
#[core_runtime::jsclass(hidden)]
pub(crate) struct RunStoresStep {
    state: Heap<RunStoresImpl>,
    #[no_trace]
    index: u32,
}

#[core_runtime::jsmethods]
impl RunStoresStep {}

/// The function a bound store's `run` calls, whose payload is a [`RunStoresStep`].
fn continue_run_stores(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let step = cast_payload::<RunStoresStep>(scope, payload);
    let (state, index) = {
        let data = step.data();
        (data.state.get(scope), data.index)
    };
    Ok(run_stores_from(scope, state, index)?.get())
}

/// Run the part of `runStores` that starts at the store at `index - 1`: run the rest with that
/// store's value set to its transform of the data, or, at index 0, publish the data and call the
/// function, as Node's `wrapStoreRun` does. A transform that throws has its exception reported,
/// and the rest runs without that store's value set.
fn run_stores_from<'s>(
    scope: &'s Scope<'_>,
    state: RunStores<'_>,
    index: u32,
) -> Result<HandleValue<'s>, ExnThrown> {
    let (channel, data) = {
        let fields = state.data();
        (fields.channel.get(scope), fields.data.get(scope))
    };
    let channel = channel
        .cast::<Channel>()
        .map_err(|_| js::error::TypeError("not a Channel".into()).throw(scope))?;
    if index == 0 {
        channel.publish(scope, data)?;
        let (callback, this_arg, args) = {
            let fields = state.data();
            let args: Vec<HandleValue<'_>> = fields.args.iter().map(|arg| arg.get(scope)).collect();
            (fields.callback.get(scope), fields.this_arg.get(scope), args)
        };
        return Function::call(scope, this_arg, callback, &args);
    }
    let bound = {
        let fields = channel.data();
        fields
            .stores
            .get(index as usize - 1)
            .map(|bound| (bound.store.get(scope), bound.transform.get(scope)))
    };
    let Some((store, transform)) = bound else {
        return run_stores_from(scope, state, index - 1);
    };
    let value = if transform.is_undefined() {
        Some(data)
    } else {
        call_reporting(scope, transform, &[data.get()])?
    };
    let Some(value) = value else {
        return run_stores_from(scope, state, index - 1);
    };
    let step =
        js::class::create_instance_with::<RunStoresStepImpl>(scope, |_| RunStoresStepImpl {
            state: Heap::from(state),
            index: index - 1,
        })?;
    let next = Function::new_callback(scope, c"", 0, continue_run_stores, step)?;
    let store = Object::from_value(scope, store).map_err(|e| e.throw(scope))?;
    Function::call_by_name(
        scope,
        store.handle(),
        c"run",
        &[value.get(), next.to_jsval_raw_throwing(scope)?],
    )
}

/// `Channel[Symbol.hasInstance](instance)`: whether `instance`'s prototype is `Channel.prototype`
/// or `ActiveChannel.prototype`, as in Node.
fn channel_has_instance(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let Ok(instance) = Object::from_value(scope, args.get(0)) else {
        return Ok(js::value::from_bool(false));
    };
    let Some(proto) = instance.get_prototype(scope)? else {
        return Ok(js::value::from_bool(false));
    };
    let channel = js::class::get_prototype_object_for::<ChannelImpl>(scope);
    let active = js::class::realm_object::<ActiveChannelPrototype>(scope);
    let is_channel = [channel, active]
        .into_iter()
        .flatten()
        .any(|candidate| candidate.as_raw() == proto.as_raw());
    Ok(js::value::from_bool(is_channel))
}

/// The constructor of Node's internal `ActiveChannel` class, which nothing calls.
fn active_channel_constructor(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Err(NodeError::illegal_constructor().throw(scope))
}

/// The names of a tracing channel's channels, as properties and in channel names.
const TRACING_EVENTS: [(&std::ffi::CStr, &str); 5] = [
    (c"start", "start"),
    (c"end", "end"),
    (c"asyncStart", "asyncStart"),
    (c"asyncEnd", "asyncEnd"),
    (c"error", "error"),
];

/// A `TracingChannel`. See <https://nodejs.org/api/diagnostics_channel.html#class-tracingchannel>.
#[core_runtime::jsclass(name = "TracingChannel", hidden)]
pub struct TracingChannel {
    /// The channels for [`TRACING_EVENTS`], in that order.
    channels: Vec<Heap<ChannelImpl>>,
}

#[core_runtime::jsmethods(node_args)]
impl TracingChannel {
    /// <https://nodejs.org/api/diagnostics_channel.html#class-tracingchannel>: the channels
    /// `tracing:${name}:start` and so on for a string `name_or_channels`, or the channels of an
    /// object with `start`, `end`, `asyncStart`, `asyncEnd` and `error` properties. They are own
    /// non-enumerable properties, as in Node.
    #[constructor]
    pub fn new(&self, scope: &Scope<'_>, name_or_channels: HandleValue<'_>) -> Result<(), Thrown> {
        let this = self.as_object();
        let name = if name_or_channels.is_string() {
            Some(crate::errors::js_string(scope, name_or_channels)?)
        } else {
            None
        };
        let given = if name.is_none() {
            Some(Object::from_value(scope, name_or_channels).map_err(|_| {
                NodeError::invalid_arg_type(
                    scope,
                    "nameOrChannels",
                    "of type string or an instance of TracingChannel or Object",
                    name_or_channels,
                )
            })?)
        } else {
            None
        };
        for (property, event) in TRACING_EVENTS {
            let channel = match (&name, given) {
                (Some(name), _) => {
                    let channel_name =
                        js::JSString::from_str(scope, &format!("tracing:{name}:{event}"))?;
                    channel_named(scope, channel_name.to_jsval_throwing(scope)?)?
                }
                (None, Some(given)) => {
                    let channel = given.get_property(scope, property)?;
                    Object::from_value(scope, channel)
                        .ok()
                        .and_then(|channel| channel.cast::<Channel>().ok())
                        .ok_or_else(|| {
                            NodeError::invalid_arg_type(
                                scope,
                                &format!("nameOrChannels.{event}"),
                                "an instance of Channel",
                                channel,
                            )
                        })?
                }
                (None, None) => unreachable!("one of them is set"),
            };
            self.data_mut().channels.push(Heap::from(channel));
            this.define_property(
                scope,
                property,
                channel,
                js::class_spec::JSPROP_PERMANENT as u32 | js::class_spec::JSPROP_READONLY as u32,
            )?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#tracingchannelhassubscribers>
    #[getter]
    pub fn has_subscribers(&self, scope: &Scope<'_>) -> bool {
        (0..TRACING_EVENTS.len()).any(|index| self.part(scope, index).has_subscribers())
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#tracingchannelsubscribesubscribers>
    #[method]
    pub fn subscribe(&self, scope: &Scope<'_>, subscribers: HandleValue<'_>) -> Result<(), Thrown> {
        let subscribers = crate::errors::validate_object(scope, subscribers, "subscribers")?;
        for (index, (property, _)) in TRACING_EVENTS.into_iter().enumerate() {
            let handler = subscribers.get_property(scope, property)?;
            if js::conversion::to_boolean(handler) {
                self.part(scope, index).subscribe(scope, handler)?;
            }
        }
        Ok(())
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#tracingchannelunsubscribesubscribers>
    #[method]
    pub fn unsubscribe(
        &self,
        scope: &Scope<'_>,
        subscribers: HandleValue<'_>,
    ) -> Result<bool, Thrown> {
        let subscribers = crate::errors::validate_object(scope, subscribers, "subscribers")?;
        let mut done = true;
        for (index, (property, _)) in TRACING_EVENTS.into_iter().enumerate() {
            let handler = subscribers.get_property(scope, property)?;
            if js::conversion::to_boolean(handler)
                && !self.part(scope, index).unsubscribe(scope, handler)?
            {
                done = false;
            }
        }
        Ok(done)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#tracingchanneltracesyncfn-context-thisarg-args>
    #[method]
    pub fn trace_sync<'r>(
        &self,
        scope: &'r Scope<'_>,
        callback: HandleValue<'_>,
        context: Option<HandleValue<'_>>,
        this_arg: Option<HandleValue<'_>>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'r>, Thrown> {
        self.trace(scope, callback, context, this_arg, &args, trace_sync_body)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#tracingchanneltracepromisefn-context-thisarg-args>
    #[method]
    pub fn trace_promise<'r>(
        &self,
        scope: &'r Scope<'_>,
        callback: HandleValue<'_>,
        context: Option<HandleValue<'_>>,
        this_arg: Option<HandleValue<'_>>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'r>, Thrown> {
        self.trace(
            scope,
            callback,
            context,
            this_arg,
            &args,
            trace_promise_body,
        )
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#tracingchanneltracecallbackfn-position-context-thisarg-args>
    #[method]
    pub fn trace_callback<'r>(
        &self,
        scope: &'r Scope<'_>,
        callback: HandleValue<'_>,
        position: Option<f64>,
        context: Option<HandleValue<'_>>,
        this_arg: Option<HandleValue<'_>>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'r>, Thrown> {
        validate_function(scope, callback, "fn")?;
        if !self.has_subscribers(scope) {
            let this_arg = this_arg.unwrap_or(HandleValue::undefined());
            let result = Function::call(scope, this_arg, callback, &args[..])?;
            return Ok(scope.root_value(result.get()));
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
            .unwrap_or(HandleValue::undefined());
        validate_function(scope, original, "callback")?;
        let context = trace_context(scope, context)?;
        let wrapper_state = TraceState::new(scope, self, original, context, None, &[])?;
        let wrapper = Function::new_callback(scope, c"", 2, trace_callback_wrapper, wrapper_state)?;
        let wrapper = (*wrapper).to_jsval_throwing(scope)?;
        args[index as usize] = wrapper;
        let state = TraceState::new(scope, self, callback, context, this_arg, &args)?;
        Ok(self.run_traced(scope, context, trace_callback_body, state)?)
    }
}

impl<'s> TracingChannel<'s> {
    /// The channel for the event at `index` in [`TRACING_EVENTS`].
    fn part<'r>(&self, scope: &'r Scope<'_>, index: usize) -> Channel<'r> {
        self.data().channels[index].get(scope)
    }

    /// The body of `traceSync` and `tracePromise`: call `callback` with `this_arg` and `args`
    /// directly if no channel has subscribers, and otherwise run `body` with the trace's state
    /// inside the `start` channel's stores.
    fn trace<'r>(
        &self,
        scope: &'r Scope<'_>,
        callback: HandleValue<'_>,
        context: Option<HandleValue<'_>>,
        this_arg: Option<HandleValue<'_>>,
        args: &[HandleValue<'_>],
        body: js::function::Callback,
    ) -> Result<HandleValue<'r>, Thrown> {
        validate_function(scope, callback, "fn")?;
        if !self.has_subscribers(scope) {
            let this_arg = this_arg.unwrap_or(HandleValue::undefined());
            let result = Function::call(scope, this_arg, callback, args)?;
            return Ok(scope.root_value(result.get()));
        }
        let context = trace_context(scope, context)?;
        let state = TraceState::new(scope, self, callback, context, this_arg, args)?;
        Ok(self.run_traced(scope, context, body, state)?)
    }

    /// Run `body` with `state` as its payload inside the `start` channel's stores.
    fn run_traced<'r>(
        &self,
        scope: &'r Scope<'_>,
        context: Object<'_>,
        body: js::function::Callback,
        state: TraceState<'_>,
    ) -> Result<HandleValue<'r>, ExnThrown> {
        let body = Function::new_callback(scope, c"", 0, body, state)?;
        let body = (*body).to_jsval_throwing(scope)?;
        let context = context.to_jsval_throwing(scope)?;
        self.part(scope, 0)
            .run_stores_with(scope, context, body, HandleValue::undefined(), &[])
    }

    /// Set `context.error` to the pending exception, publish `context` on the `error` channel,
    /// and leave the exception pending again.
    fn publish_error(
        &self,
        scope: &Scope<'_>,
        context: Object<'_>,
    ) -> Result<ExnThrown, ExnThrown> {
        let error = js::exception::take_pending_or_undefined(scope);
        context.set_property(scope, c"error", error)?;
        self.part(scope, 4)
            .publish(scope, context.to_jsval_throwing(scope)?)?;
        Ok(js::exception::throw_value(scope, error))
    }

    /// Publish `context` on the `asyncStart` and `asyncEnd` channels.
    fn publish_async(&self, scope: &Scope<'_>, context: Object<'_>) -> Result<(), ExnThrown> {
        let context = context.to_jsval_throwing(scope)?;
        self.part(scope, 2).publish(scope, context)?;
        self.part(scope, 3).publish(scope, context)
    }
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

/// The state of a `trace*` call: the tracing channel, the traced function, the context, and the
/// `this` and arguments of the call. The callback `traceCallback` passes on has one whose function
/// is the callback it replaces.
#[core_runtime::jsclass(hidden)]
pub(crate) struct TraceState {
    tracing: Heap<TracingChannelImpl>,
    callback: Heap<Value>,
    context: Heap<js::object::Object>,
    this_arg: Heap<Value>,
    args: Vec<Heap<Value>>,
}

#[core_runtime::jsmethods]
impl TraceState {}

impl<'s> TraceState<'s> {
    /// A new trace state.
    fn new(
        scope: &'s Scope<'_>,
        tracing: &TracingChannel<'_>,
        callback: HandleValue<'_>,
        context: Object<'_>,
        this_arg: Option<HandleValue<'_>>,
        args: &[HandleValue<'_>],
    ) -> Result<Self, ExnThrown> {
        js::class::create_instance_with::<TraceStateImpl>(scope, |_| TraceStateImpl {
            tracing: Heap::from(*tracing),
            callback: Heap::from(callback.get()),
            context: Heap::from(context),
            this_arg: Heap::from(
                this_arg.map_or(js::value::undefined(), |this_arg| this_arg.get()),
            ),
            args: args.iter().map(|arg| Heap::from(arg.get())).collect(),
        })
    }

    /// The tracing channel and the context.
    fn parts<'r>(&self, scope: &'r Scope<'_>) -> (TracingChannel<'r>, Object<'r>) {
        let data = self.data();
        (data.tracing.get(scope), data.context.get(scope))
    }

    /// Call the traced function, publishing an exception it throws on the `error` channel, and
    /// publish the context on the `end` channel either way. If `record_result`, the context's
    /// `result` is set to the function's result before `end` is published.
    fn call<'r>(
        &self,
        scope: &'r Scope<'_>,
        record_result: bool,
    ) -> Result<HandleValue<'r>, ExnThrown> {
        let (tracing, context) = self.parts(scope);
        let (callback, this_arg, args) = {
            let data = self.data();
            let args: Vec<HandleValue<'_>> = data.args.iter().map(|arg| arg.get(scope)).collect();
            (data.callback.get(scope), data.this_arg.get(scope), args)
        };
        let result = match Function::call(scope, this_arg, callback, &args) {
            Ok(result) => {
                if record_result {
                    context.set_property(scope, c"result", result)?;
                }
                Ok(result)
            }
            Err(_) => Err(tracing.publish_error(scope, context)?),
        };
        let pending = js::exception::take_pending(scope);
        tracing
            .part(scope, 1)
            .publish(scope, context.to_jsval_throwing(scope)?)?;
        if let Some(pending) = pending {
            js::exception::throw_value(scope, pending);
        }
        result
    }
}

/// The body `traceSync` runs inside the `start` channel's stores, whose payload is a
/// [`TraceState`]: call the function, recording its result in the context.
fn trace_sync_body(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(cast_payload::<TraceState>(scope, payload)
        .call(scope, true)?
        .get())
}

/// The fulfillment handler of the promise `tracePromise` returns, whose payload is the
/// [`TraceState`]: set `context.result`, and publish the async channels.
fn trace_promise_fulfilled(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let (tracing, context) = cast_payload::<TraceState>(scope, payload).parts(scope);
    context.set_property(scope, c"result", args.get(0))?;
    tracing.publish_async(scope, context)?;
    Ok(args.get(0).get())
}

/// The rejection handler of the promise `tracePromise` returns, whose payload is the
/// [`TraceState`]: set `context.error`, publish the `error` and async channels, and reject with
/// the same reason.
fn trace_promise_rejected(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let (tracing, context) = cast_payload::<TraceState>(scope, payload).parts(scope);
    context.set_property(scope, c"error", args.get(0))?;
    tracing
        .part(scope, 4)
        .publish(scope, context.to_jsval_throwing(scope)?)?;
    tracing.publish_async(scope, context)?;
    Err(js::exception::throw_value(scope, args.get(0)))
}

/// The body `tracePromise` runs inside the `start` channel's stores, whose payload is a
/// [`TraceState`]: call the function, and return a promise for its result that publishes the
/// async channels once it settles.
fn trace_promise_body(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<TraceState>(scope, payload);
    let result = state.call(scope, false)?;
    let promise = js::Promise::call_original_resolve(scope, result)?;
    let fulfilled = Function::new_callback(scope, c"", 1, trace_promise_fulfilled, state)?;
    let rejected = Function::new_callback(scope, c"", 1, trace_promise_rejected, state)?;
    promise
        .then(scope, Some(*fulfilled), Some(*rejected))?
        .to_jsval_raw_throwing(scope)
}

/// The callback `traceCallback` passes in place of the traced function's, whose payload is a
/// [`TraceState`] holding the original callback: set `context.error` or `context.result` from the
/// arguments, then call the original callback inside the `asyncStart` channel's stores, and
/// publish `asyncEnd`. Returns `undefined`, as Node.js's `wrappedCallback` does.
fn trace_callback_wrapper(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let state = cast_payload::<TraceState>(scope, payload);
    let (tracing, context) = state.parts(scope);
    let callback = state.data().callback.get(scope);
    let error = args.get(0);
    if !js::conversion::to_boolean(error) {
        context.set_property(scope, c"result", args.get(1))?;
    } else {
        context.set_property(scope, c"error", error)?;
        tracing
            .part(scope, 4)
            .publish(scope, context.to_jsval_throwing(scope)?)?;
    }
    let call_args: Vec<HandleValue<'_>> = args.rest(0);
    let context_value = context.to_jsval_throwing(scope)?;
    let this = scope.root_value(args.this());
    let result =
        tracing
            .part(scope, 2)
            .run_stores_with(scope, context_value, callback, this, &call_args);
    let pending = js::exception::take_pending(scope);
    tracing.part(scope, 3).publish(scope, context_value)?;
    if let Some(pending) = pending {
        js::exception::throw_value(scope, pending);
    }
    result?;
    Ok(js::value::undefined())
}

/// The body `traceCallback` runs inside the `start` channel's stores, whose payload is a
/// [`TraceState`].
fn trace_callback_body(
    scope: &Scope<'_>,
    _args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(cast_payload::<TraceState>(scope, payload)
        .call(scope, false)?
        .get())
}

/// Register the classes of `node:diagnostics_channel` on `global`, with the realm's channel map,
/// `ActiveChannel.prototype` and `Channel[Symbol.hasInstance]`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    Channel::add_to_global(scope, global);
    TracingChannel::add_to_global(scope, global);
    RunStores::add_to_global(scope, global);
    RunStoresStep::add_to_global(scope, global);
    TraceState::add_to_global(scope, global);
    ChannelReferences::add_to_global(scope, global);
    js::class::register_realm_object::<Channels>(scope, *js::Map::new(scope)?);
    let weak_ref =
        crate::support::class_object(scope, js::class_spec::JSProtoKey::JSProto_WeakRef)?;
    let finalization_registry = crate::support::class_object(
        scope,
        js::class_spec::JSProtoKey::JSProto_FinalizationRegistry,
    )?;
    let prototype_method = |constructor: Object<'_>, name: &'static std::ffi::CStr| {
        let prototype = constructor.get_property(scope, c"prototype")?;
        Object::from_value(scope, prototype)
            .map_err(|e| e.throw(scope))?
            .get_property(scope, name)
    };
    let deref = prototype_method(weak_ref, c"deref")?;
    let register = prototype_method(finalization_registry, c"register")?;
    let weak_ref = weak_ref.to_jsval_throwing(scope)?;
    let finalization_registry = finalization_registry.to_jsval_throwing(scope)?;
    let references = js::class::create_instance_with::<ChannelReferencesImpl>(scope, |_| {
        ChannelReferencesImpl {
            weak_ref: Heap::from(weak_ref.get()),
            deref: Heap::from(deref.get()),
            finalization_registry: Heap::from(finalization_registry.get()),
            register: Heap::from(register.get()),
            registry: None,
        }
    })?;
    js::class::register_realm_object::<ChannelReferences>(scope, references.as_object());
    let proto = js::class::get_prototype_object_for::<ChannelImpl>(scope)
        .expect("Channel was registered above");
    let active = Object::new_plain(scope)?;
    let constructor =
        Function::new_callback(scope, c"ActiveChannel", 1, active_channel_constructor, ())?;
    active.define_property(scope, c"constructor", *constructor, 0)?;
    constructor.define_property(
        scope,
        c"prototype",
        active,
        js::class_spec::JSPROP_PERMANENT as u32 | js::class_spec::JSPROP_READONLY as u32,
    )?;
    for key in proto.own_keys(scope)? {
        let id = scope.root_id(js::id::value_to_id(scope, key)?);
        if crate::support::is_str(scope, key, c"constructor")? {
            continue;
        }
        if let Some(property) = proto.own_property_descriptor(scope, id)? {
            match property.value {
                Some(value) => active.define_value_by_id(scope, id, value, 0)?,
                None => {
                    active.define_accessor_by_id(scope, id, property.getter, property.setter, 0)?
                }
            }
        }
    }
    js::class::register_realm_object::<ActiveChannelPrototype>(scope, active);
    let ctor = js::class::class_constructor::<ChannelImpl>(scope)?;
    let has_instance =
        Function::new_callback(scope, c"[Symbol.hasInstance]", 1, channel_has_instance, ())?;
    js::class::define_well_known_symbol_property(
        scope,
        ctor,
        js::native::SymbolCode::hasInstance,
        *has_instance,
    )
}

#[core_runtime::jsmodule(node_args, name = "node:diagnostics_channel", default_namespace)]
pub mod module {
    use super::*;

    pub use super::Channel;

    /// <https://nodejs.org/api/diagnostics_channel.html#diagnostics_channelchannelname>
    pub fn channel<'s>(scope: &'s Scope<'_>, name: HandleValue<'_>) -> Result<Channel<'s>, Thrown> {
        channel_named(scope, name)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#diagnostics_channelhassubscribersname>:
    /// `false` for a name without a channel, which this doesn't create.
    pub fn has_subscribers(scope: &Scope<'_>, name: HandleValue<'_>) -> Result<bool, ExnThrown> {
        Ok(existing_channel(scope, name)?.is_some_and(|channel| channel.has_subscribers()))
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#diagnostics_channelsubscribename-onmessage>
    pub fn subscribe(
        scope: &Scope<'_>,
        name: HandleValue<'_>,
        on_message: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        channel_named(scope, name)?.subscribe(scope, on_message)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#diagnostics_channelunsubscribename-onmessage>
    pub fn unsubscribe(
        scope: &Scope<'_>,
        name: HandleValue<'_>,
        on_message: HandleValue<'_>,
    ) -> Result<bool, Thrown> {
        Ok(channel_named(scope, name)?.unsubscribe(scope, on_message)?)
    }

    /// <https://nodejs.org/api/diagnostics_channel.html#diagnostics_channeltracingchannelnameorchannels>
    pub fn tracing_channel<'s>(
        scope: &'s Scope<'_>,
        name_or_channels: HandleValue<'_>,
    ) -> Result<TracingChannel<'s>, ExnThrown> {
        TracingChannel::new(scope, name_or_channels)
    }
}
