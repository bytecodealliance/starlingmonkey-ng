// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `MockTracker` and `MockFunctionContext`: <https://nodejs.org/api/test.html#class-mocktracker>.
//!
//! A mock function is a `Proxy` of the function it mocks, whose `mock` property is its
//! `MockFunctionContext`. Calls and constructions go to the context's implementation, and are
//! recorded in its `calls`. Mock timers and module mocks are not implemented.

use js::class_spec::JSProtoKey;
use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::{cast_payload, CallbackArgs};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::object::{PropertyInit, PropertyTemplate, TemplateKey};
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{
    validate_boolean, validate_function, validate_plain_object, NodeError, Thrown,
};

/// A new `MockTracker`.
pub(super) fn new_tracker<'s>(scope: &'s Scope<'_>) -> Result<MockTracker<'s>, ExnThrown> {
    js::class::create_instance_with::<MockTrackerImpl>(scope, |_| MockTrackerImpl::default())
}

/// The empty function a mock of no function mocks, the same one for every mock in a realm, named
/// `original` as the parameter it is the default of in Node.js. It can be called and constructed,
/// and has a `prototype` as a function declaration has.
fn empty_function<'s>(scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
    let function =
        js::class::get_or_init_shared_function(scope, empty as *const () as usize, |scope| {
            let function = Function::new_constructor_callback(scope, c"original", 0, empty, ())?;
            let prototype = Object::new_plain(scope)?;
            prototype.define_property(scope, c"constructor", *function, 0)?;
            let permanent = js::class_spec::JSPROP_PERMANENT as u32;
            function.define_property(scope, c"prototype", prototype, permanent)?;
            Ok(function)
        })?;
    function.to_jsval_throwing(scope)
}

/// The body of [`empty_function`]. A construction creates an object whose prototype is `new.target.prototype`, as a constructor
/// with an empty body does.
fn empty(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    if !args.is_constructing() {
        return Ok(js::value::undefined());
    }
    let new_target = Object::from_value(scope, args.new_target()).map_err(|e| e.throw(scope))?;
    let object = match Object::from_value(scope, *new_target.get_property(scope, c"prototype")?) {
        Ok(proto) => Object::new_plain_with_proto(scope, proto.handle())?,
        Err(_) => Object::new_plain(scope)?,
    };
    object.to_jsval_raw_throwing(scope)
}

/// `ERR_INVALID_ARG_TYPE` unless `times` is `Infinity` or an integer of at least 1. Returns
/// `times` as a number.
fn validate_times(scope: &Scope<'_>, times: HandleValue<'_>, name: &str) -> Result<f64, Thrown> {
    if times.is_undefined() || (times.is_number() && times.to_number() == f64::INFINITY) {
        return Ok(f64::INFINITY);
    }
    let times =
        crate::errors::validate_integer(scope, times, name, 1.0, crate::errors::MAX_SAFE_INTEGER)?;
    Ok(times)
}

/// Whether the option `name` of `options` is `true`, or `ERR_INVALID_ARG_TYPE` if it is neither
/// `undefined` nor a boolean.
fn boolean_option(
    scope: &Scope<'_>,
    options: Object<'_>,
    name: &'static std::ffi::CStr,
    label: &str,
) -> Result<bool, Thrown> {
    let value = options.get_property(scope, name)?;
    if value.is_undefined() {
        return Ok(false);
    }
    validate_boolean(scope, value, label)
}

/// How a mock is restored.
#[js::must_root]
#[derive(core_runtime::Traceable, Default)]
enum MockRestore {
    /// The mock of a function calls the original function again.
    Function { original: Heap<Value> },
    /// The mocked property of an object is defined with its original descriptor again.
    Method {
        object: Heap<js::object::Object>,
        name: Heap<Value>,
        descriptor: Heap<Value>,
    },
    #[default]
    None,
}

/// The implementation a call of a mock calls instead of the context's, by the call's index.
#[js::must_root]
#[derive(core_runtime::Traceable)]
struct OnceImplementation {
    #[no_trace]
    call: f64,
    implementation: Heap<Value>,
}

/// A `MockTracker`. See <https://nodejs.org/api/test.html#class-mocktracker>.
#[core_runtime::jsclass(name = "MockTracker", hidden)]
pub struct MockTracker {
    mocks: Vec<Heap<MockFunctionContextImpl>>,
}

#[core_runtime::jsmethods(node_args)]
impl MockTracker {
    /// A tracker without mocks.
    #[constructor]
    pub fn create() -> Self {
        MockTrackerImpl::default()
    }

    /// <https://nodejs.org/api/test.html#mocktimers>: not implemented.
    #[getter]
    pub fn timers<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        Err(NodeError::not_implemented("mock.timers").throw(scope))
    }

    /// <https://nodejs.org/api/test.html#mockfnoriginal-implementation-options>
    #[method(name = "fn")]
    pub fn mock_fn<'r>(
        &self,
        scope: &'r Scope<'_>,
        original: Option<HandleValue<'_>>,
        implementation: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'r>, Thrown> {
        let mut original = original.filter(|value| !value.is_undefined());
        let mut implementation = implementation.filter(|value| !value.is_undefined());
        let mut options = options.filter(|value| !value.is_undefined());
        if let Some(value) =
            original.filter(|value| value.is_object() && !js::function::is_callable(*value))
        {
            options = Some(value);
            original = None;
            implementation = None;
        } else if let Some(value) =
            implementation.filter(|value| value.is_object() && !js::function::is_callable(*value))
        {
            options = Some(value);
            implementation = None;
        }
        let original = match original {
            Some(original) => original,
            None => empty_function(scope)?,
        };
        let implementation = implementation.unwrap_or(original);
        validate_function(scope, original, "original")?;
        validate_function(scope, implementation, "implementation")?;
        let times = match options {
            Some(options) => {
                let options = validate_plain_object(scope, options, "options", false)?;
                validate_times(
                    scope,
                    options.get_property(scope, c"times")?,
                    "options.times",
                )?
            }
            None => f64::INFINITY,
        };
        let ctx = new_context(scope, implementation, Restore::Function(original), times)?;
        Ok(self.setup_mock(scope, ctx, original)?)
    }

    /// <https://nodejs.org/api/test.html#mockmethodobject-methodname-implementation-options>
    #[method]
    pub fn method<'r>(
        &self,
        scope: &'r Scope<'_>,
        object: HandleValue<'_>,
        method_name: HandleValue<'_>,
        implementation: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'r>, Thrown> {
        self.mock_method(scope, object, method_name, implementation, options)
    }

    /// <https://nodejs.org/api/test.html#mockgetterobject-methodname-implementation-options>
    #[method]
    pub fn getter<'r>(
        &self,
        scope: &'r Scope<'_>,
        object: HandleValue<'_>,
        method_name: HandleValue<'_>,
        implementation: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'r>, Thrown> {
        let undefined = HandleValue::undefined();
        let args = [
            object,
            method_name,
            implementation.unwrap_or(undefined),
            options.unwrap_or(undefined),
        ];
        self.mock_accessor(scope, args, c"getter")
    }

    /// <https://nodejs.org/api/test.html#mocksetterobject-methodname-implementation-options>
    #[method]
    pub fn setter<'r>(
        &self,
        scope: &'r Scope<'_>,
        object: HandleValue<'_>,
        method_name: HandleValue<'_>,
        implementation: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'r>, Thrown> {
        let undefined = HandleValue::undefined();
        let args = [
            object,
            method_name,
            implementation.unwrap_or(undefined),
            options.unwrap_or(undefined),
        ];
        self.mock_accessor(scope, args, c"setter")
    }

    /// <https://nodejs.org/api/test.html#mockreset>: restore the mocks, and forget them.
    #[method]
    pub fn reset(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        self.restore_all(scope)?;
        self.data_mut().mocks.clear();
        Ok(())
    }

    /// <https://nodejs.org/api/test.html#mockrestoreall>
    #[method]
    pub fn restore_all(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let mocks: Vec<MockFunctionContext<'_>> =
            self.data().mocks.iter().map(|ctx| ctx.get(scope)).collect();
        for ctx in mocks {
            ctx.restore(scope)?;
        }
        Ok(())
    }
}

impl<'s> MockTracker<'s> {
    /// A mock of `target` whose calls `ctx` handles, which the tracker restores.
    fn setup_mock<'r>(
        &self,
        scope: &'r Scope<'_>,
        ctx: MockFunctionContext<'_>,
        target: HandleValue<'_>,
    ) -> Result<Object<'r>, ExnThrown> {
        let handler = Object::new_plain(scope)?;
        let apply = Function::new_callback(scope, c"apply", 3, mock_apply, ctx)?;
        let construct = Function::new_callback(scope, c"construct", 3, mock_construct, ctx)?;
        let get = Function::new_callback(scope, c"get", 3, mock_get, ctx)?;
        handler.set_property(scope, c"apply", *apply)?;
        handler.set_property(scope, c"construct", *construct)?;
        handler.set_property(scope, c"get", *get)?;
        let proxy = js::class::get_class_object(scope, JSProtoKey::JSProto_Proxy)?;
        let proxy = Object::from_handle(proxy).ok_or(ExnThrown)?;
        let mock = Function::construct(
            scope,
            proxy,
            &[target.get(), handler.to_jsval_raw_throwing(scope)?],
        )?;
        self.data_mut().mocks.push(Heap::from(ctx));
        Ok(mock)
    }

    /// `mock.method(object, methodName, implementation, options)`.
    fn mock_method<'r>(
        &self,
        scope: &'r Scope<'_>,
        object: HandleValue<'_>,
        method_name: HandleValue<'_>,
        implementation: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'r>, Thrown> {
        if !method_name.is_string() && !method_name.is_symbol() {
            return Err(NodeError::invalid_arg_type(
                scope,
                "methodName",
                "of type string or symbol",
                method_name,
            )
            .into());
        }
        let target = if js::function::is_callable(object) {
            Object::from_value(scope, object)?
        } else {
            validate_plain_object(scope, object, "object", false)?
        };
        let mut implementation = implementation.filter(|value| !value.is_undefined());
        let mut options = options.filter(|value| !value.is_undefined());
        if let Some(value) = implementation {
            if value.is_object() && !js::function::is_callable(value) {
                options = Some(value);
                implementation = None;
            }
        }
        if let Some(value) = implementation {
            validate_function(scope, value, "implementation")?;
        }
        let (getter, setter, times) = match options {
            Some(options) => {
                let options = validate_plain_object(scope, options, "options", false)?;
                let getter = boolean_option(scope, options, c"getter", "options.getter")?;
                let setter = boolean_option(scope, options, c"setter", "options.setter")?;
                let times = options.get_property(scope, c"times")?;
                (
                    getter,
                    setter,
                    validate_times(scope, times, "options.times")?,
                )
            }
            None => (false, false, f64::INFINITY),
        };
        if getter && setter {
            let value = scope.root_value(js::value::from_bool(setter));
            return Err(NodeError::invalid_arg_value(
                scope,
                "options.setter",
                value,
                "cannot be used with 'options.getter'",
            )
            .into());
        }
        let id = scope.root_id(js::id::value_to_id(scope, method_name)?);
        let property = find_property(scope, target, id)?;
        let original = match &property {
            Some(property) if getter => property
                .getter
                .map(|getter| getter.to_jsval_throwing(scope)),
            Some(property) if setter => property
                .setter
                .map(|setter| setter.to_jsval_throwing(scope)),
            Some(property) => property.value.map(Ok),
            None => None,
        }
        .transpose()?
        .unwrap_or(HandleValue::undefined());
        if !js::function::is_callable(original) {
            return Err(NodeError::invalid_arg_value(
                scope,
                "methodName",
                original,
                "must be a method",
            )
            .into());
        }
        let Some(property) = property else {
            return Err(ExnThrown.into());
        };
        let descriptor = find_descriptor_value(scope, target, id)?;
        let restore = Restore::Method {
            object: target,
            name: method_name,
            descriptor,
        };
        let ctx = new_context(scope, implementation.unwrap_or(original), restore, times)?;
        let mock = self.setup_mock(scope, ctx, original)?;
        let mock_descriptor = Object::new_plain(scope)?;
        mock_descriptor.set_property(scope, c"configurable", property.configurable)?;
        mock_descriptor.set_property(scope, c"enumerable", property.enumerable)?;
        let undefined = HandleValue::undefined();
        if getter {
            mock_descriptor.set_property(scope, c"get", mock)?;
            let set = match property.setter {
                Some(set) => set.to_jsval_throwing(scope)?,
                None => undefined,
            };
            mock_descriptor.set_property(scope, c"set", set)?;
        } else if setter {
            let get = match property.getter {
                Some(get) => get.to_jsval_throwing(scope)?,
                None => undefined,
            };
            mock_descriptor.set_property(scope, c"get", get)?;
            mock_descriptor.set_property(scope, c"set", mock)?;
        } else {
            mock_descriptor.set_property(scope, c"writable", property.writable)?;
            mock_descriptor.set_property(scope, c"value", mock)?;
        }
        define_from_descriptor(scope, target, id, mock_descriptor.to_jsval_throwing(scope)?)?;
        Ok(mock)
    }

    /// `mock.getter()` and `mock.setter()`: `mock.method()` with the option `accessor` set to
    /// `true`.
    fn mock_accessor<'r>(
        &self,
        scope: &'r Scope<'_>,
        args: [HandleValue<'_>; 4],
        accessor: &'static std::ffi::CStr,
    ) -> Result<Object<'r>, Thrown> {
        let [object, method_name, mut implementation, mut options] = args;
        let label = format!("options.{}", accessor.to_str().unwrap_or_default());
        if implementation.is_object() && !js::function::is_callable(implementation) {
            options = implementation;
            implementation = HandleValue::undefined();
        } else if !options.is_undefined() {
            validate_plain_object(scope, options, "options", false)?;
        }
        let copy = Object::new_plain(scope)?;
        if let Ok(options) = Object::from_value(scope, options) {
            copy.assign(scope, options)?;
        }
        let value = copy.get_property(scope, accessor)?;
        if value.is_boolean() && !value.to_boolean() {
            return Err(
                NodeError::invalid_arg_value(scope, &label, value, "cannot be false").into(),
            );
        }
        if value.is_undefined() {
            copy.set_property(scope, accessor, true)?;
        }
        let copy = copy.to_jsval_throwing(scope)?;
        self.mock_method(scope, object, method_name, Some(implementation), Some(copy))
    }
}

/// The own property `id` of `object`, or of the first object on its prototype chain that has it.
fn find_property<'s>(
    scope: &'s Scope<'_>,
    object: Object<'s>,
    id: js::native::HandleId<'_>,
) -> Result<Option<js::object::OwnProperty<'s>>, ExnThrown> {
    let mut current = Some(object);
    while let Some(host) = current {
        if let Some(property) = host.own_property_descriptor(scope, id)? {
            return Ok(Some(property));
        }
        current = host.get_prototype(scope)?;
    }
    Ok(None)
}

/// The descriptor object of the own property `id` of `object`, or of the first object on its
/// prototype chain that has it, or `undefined`.
fn find_descriptor_value<'s>(
    scope: &'s Scope<'_>,
    object: Object<'s>,
    id: js::native::HandleId<'_>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let mut current = Some(object);
    while let Some(host) = current {
        let descriptor = host.own_property_descriptor_value(scope, id)?;
        if !descriptor.is_undefined() {
            return Ok(descriptor);
        }
        current = host.get_prototype(scope)?;
    }
    Ok(HandleValue::undefined())
}

/// Define the property `id` of `object` from the descriptor object `descriptor`, as
/// `Object.defineProperty` does.
fn define_from_descriptor(
    scope: &Scope<'_>,
    object: Object<'_>,
    id: js::native::HandleId<'_>,
    descriptor: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    if !object.define_property_from_descriptor(scope, id, descriptor)? {
        return Err(
            js::error::TypeError("can't redefine non-configurable property".into()).throw(scope),
        );
    }
    Ok(())
}

/// How a new mock is restored, as [`MockRestore`] records it.
enum Restore<'a> {
    /// The mock of a function calls the original function again.
    Function(HandleValue<'a>),
    /// The mocked property of an object is defined with its original descriptor again.
    Method {
        object: Object<'a>,
        name: HandleValue<'a>,
        descriptor: HandleValue<'a>,
    },
}

/// A new `MockFunctionContext` calling `implementation` at most `times` times before it is
/// restored as `restore` describes.
fn new_context<'s>(
    scope: &'s Scope<'_>,
    implementation: HandleValue<'_>,
    restore: Restore<'_>,
    times: f64,
) -> Result<MockFunctionContext<'s>, ExnThrown> {
    js::class::create_instance_with::<MockFunctionContextImpl>(scope, |_| MockFunctionContextImpl {
        calls: Vec::new(),
        once: Vec::new(),
        implementation: Heap::from(implementation.get()),
        restore: match restore {
            Restore::Function(original) => MockRestore::Function {
                original: Heap::from(original.get()),
            },
            Restore::Method {
                object,
                name,
                descriptor,
            } => MockRestore::Method {
                object: Heap::from(object),
                name: Heap::from(name.get()),
                descriptor: Heap::from(descriptor.get()),
            },
        },
        times,
    })
}

/// A `MockFunctionContext`. See <https://nodejs.org/api/test.html#class-mockfunctioncontext>.
#[core_runtime::jsclass(name = "MockFunctionContext", hidden)]
pub struct MockFunctionContext {
    /// The recorded calls, as objects with the call's `arguments`, `error`, `result`, `stack`,
    /// `target` and `this`.
    calls: Vec<Heap<js::object::Object>>,
    once: Vec<OnceImplementation>,
    implementation: Heap<Value>,
    restore: MockRestore,
    /// The number of calls after which the mock is restored.
    #[no_trace]
    times: f64,
}

#[core_runtime::jsmethods(node_args)]
impl MockFunctionContext {
    /// A context without calls, whose mock calls `undefined`.
    #[constructor]
    pub fn create() -> Self {
        MockFunctionContextImpl {
            times: f64::INFINITY,
            ..Default::default()
        }
    }

    /// <https://nodejs.org/api/test.html#ctxcalls>: a copy of the recorded calls.
    #[getter]
    pub fn calls<'r>(&self, scope: &'r Scope<'_>) -> Result<js::Array<'r>, ExnThrown> {
        let calls: Vec<Object<'r>> = self
            .data()
            .calls
            .iter()
            .map(|call| call.get(scope))
            .collect();
        js::Array::with_contents(scope, &calls)
    }

    /// <https://nodejs.org/api/test.html#ctxcallcount>
    #[method]
    pub fn call_count(&self) -> f64 {
        self.data().calls.len() as f64
    }

    /// <https://nodejs.org/api/test.html#ctxmockimplementationimplementation>
    #[method]
    pub fn mock_implementation(
        &self,
        scope: &Scope<'_>,
        implementation: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        validate_function(scope, implementation, "implementation")?;
        self.data().implementation.set(implementation.get());
        Ok(())
    }

    /// <https://nodejs.org/api/test.html#ctxmockimplementationonceimplementation-oncall>
    #[method]
    pub fn mock_implementation_once(
        &self,
        scope: &Scope<'_>,
        implementation: HandleValue<'_>,
        on_call: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        validate_function(scope, implementation, "implementation")?;
        let next = self.data().calls.len() as f64;
        let call = match on_call.filter(|value| !value.is_undefined() && !value.is_null()) {
            Some(value) => crate::errors::validate_integer(
                scope,
                value,
                "onCall",
                next,
                crate::errors::MAX_SAFE_INTEGER,
            )?,
            None => next,
        };
        let mut data = self.data_mut();
        data.once.retain(|once| once.call != call);
        data.once.push(OnceImplementation {
            call,
            implementation: Heap::from(implementation.get()),
        });
        Ok(())
    }

    /// <https://nodejs.org/api/test.html#ctxrestore>: define the mocked property of a method mock
    /// with its original descriptor, or make a function mock call the original function.
    #[method]
    pub fn restore(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let restore = {
            let data = self.data();
            match &data.restore {
                MockRestore::Function { original } => {
                    data.implementation.set(original.get(scope).get());
                    None
                }
                MockRestore::Method {
                    object,
                    name,
                    descriptor,
                } => Some((object.get(scope), name.get(scope), descriptor.get(scope))),
                MockRestore::None => None,
            }
        };
        if let Some((object, name, descriptor)) = restore {
            let id = scope.root_id(js::id::value_to_id(scope, name)?);
            define_from_descriptor(scope, object, id, descriptor)?;
        }
        Ok(())
    }

    /// <https://nodejs.org/api/test.html#ctxresetcalls>
    #[method]
    pub fn reset_calls(&self) {
        self.data_mut().calls.clear();
    }
}

impl<'s> MockFunctionContext<'s> {
    /// The implementation the next call of the mock calls: the one `mockImplementationOnce` set
    /// for it, or the context's implementation. The mock is restored before its last call.
    fn next_implementation<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        let (implementation, last) = {
            let mut data = self.data_mut();
            let next = data.calls.len() as f64;
            let once = data.once.iter().position(|once| once.call == next);
            let implementation = match once {
                Some(index) => data.once.remove(index).implementation.get(scope),
                None => data.implementation.get(scope),
            };
            (implementation, next + 1.0 == data.times)
        };
        if last {
            self.restore(scope)?;
        }
        Ok(implementation)
    }

    /// Record a call of the mock.
    fn track_call(
        &self,
        scope: &Scope<'_>,
        arguments: HandleValue<'_>,
        error: HandleValue<'_>,
        result: HandleValue<'_>,
        target: HandleValue<'_>,
        this: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        let call = Object::new_with_property_capacity(scope, None, None, CALL_PROPERTIES.len())?;
        // SAFETY: `call` was just created, and no JavaScript runs before the writer finishes.
        let mut properties =
            unsafe { call.property_writer(scope, &CALL_PROPERTIES, PropertyInit::Define) }?;
        properties.write(scope, c"arguments", arguments)?;
        properties.write(scope, c"error", error)?;
        properties.write(scope, c"result", result)?;
        let stack = js::error::new_error(scope, js::error::JSExnType::JSEXN_ERR, "")?;
        properties.write(scope, c"stack", stack)?;
        properties.write(scope, c"target", target)?;
        properties.write(scope, c"this", this)?;
        properties.finish(scope)?;
        self.data_mut().calls.push(Heap::from(call));
        Ok(())
    }
}

/// The properties of the null-prototype object that records a call of a mock, in Node's order.
pub(crate) static CALL_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"arguments"),
    TemplateKey::Name(c"error"),
    TemplateKey::Name(c"result"),
    TemplateKey::Name(c"stack"),
    TemplateKey::Name(c"target"),
    TemplateKey::Name(c"this"),
]);

/// The `apply` trap of a mock, whose payload is its context: call the next implementation and
/// record the call.
fn mock_apply(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let ctx = cast_payload::<MockFunctionContext>(scope, payload);
    let this = args.get(1);
    let list = args.get(2);
    let implementation = ctx.next_implementation(scope)?;
    let call_args = crate::support::list_from_array_like(scope, list)?;
    let undefined = HandleValue::undefined();
    match Function::call(scope, this, implementation, &call_args) {
        Ok(result) => {
            let result = scope.root_value(result.get());
            ctx.track_call(scope, list, undefined, result, undefined, this)?;
            Ok(result.get())
        }
        Err(ExnThrown) => {
            let error = js::exception::take_pending_or_undefined(scope);
            ctx.track_call(scope, list, error, undefined, undefined, this)?;
            Err(js::exception::throw_value(scope, error))
        }
    }
}

/// The `construct` trap of a mock, whose payload is its context: construct the next
/// implementation and record the construction.
fn mock_construct(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let ctx = cast_payload::<MockFunctionContext>(scope, payload);
    let target = args.get(0);
    let list = args.get(1);
    let new_target = Object::from_value(scope, args.get(2)).map_err(|e| e.throw(scope))?;
    let implementation = ctx.next_implementation(scope)?;
    let call_args = crate::support::list_from_array_like(scope, list)?;
    let undefined = HandleValue::undefined();
    match Function::construct_with_new_target(
        scope,
        implementation,
        new_target.handle(),
        &call_args,
    ) {
        Ok(result) => {
            let result = result.to_jsval_throwing(scope)?;
            ctx.track_call(scope, list, undefined, result, target, result)?;
            Ok(result.get())
        }
        Err(ExnThrown) => {
            let error = js::exception::take_pending_or_undefined(scope);
            ctx.track_call(scope, list, error, undefined, target, undefined)?;
            Err(js::exception::throw_value(scope, error))
        }
    }
}

/// The `get` trap of a mock, whose payload is its context: the context for `mock`, and the mocked
/// function's property otherwise.
fn mock_get(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let property = args.get(1);
    if crate::support::is_str(scope, property, c"mock")? {
        return Ok(payload.get());
    }
    let target = Object::from_value(scope, args.get(0)).map_err(|e| e.throw(scope))?;
    let id = scope.root_id(js::id::value_to_id(scope, property)?);
    Ok(target.get_property_by_id(scope, id)?.get())
}

/// Register the mock classes on `global`.
pub(super) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    MockTracker::add_to_global(scope, global);
    MockFunctionContext::add_to_global(scope, global);
    Ok(())
}
