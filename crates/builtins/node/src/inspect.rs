// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `util.inspect`, `util.format` and deep equality, which `util` and `assert` share. See
//! <https://nodejs.org/api/util.html#utilinspectobject-options>.
//!
//! The formatting follows Node.js's `lib/internal/util/inspect.js` and the comparison its
//! `lib/internal/util/comparisons.js`. Colors are not supported, `sorted` comparator functions sort
//! like `sorted: true`, and the entries of `Map` and `Set` iterators and of weak collections are
//! not shown. Proxies are inspected as their targets. Builtin types are recognized by the
//! engine's brand checks where the `js` crate offers one, and by `Object.prototype.toString` and
//! the builtin prototypes' `valueOf` methods otherwise.

use std::ffi::CStr;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::{CallbackArgs, EmptyArgs};
use js::gc::handle::RootedHeap;
use js::gc::scope::Scope;
use js::native::{SymbolCode, Value};
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::number_to_string;

/// The symbol a custom inspection method is stored under, `Symbol.for('nodejs.util.inspect.custom')`.
pub const INSPECT_CUSTOM: &str = "nodejs.util.inspect.custom";

/// The `compact` option: `true`, `false`, or the number of innermost levels combined on one line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Compact {
    Bool(bool),
    Number(f64),
}

/// The `getters` option: which getters are called to show their values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Getters {
    None,
    All,
    /// Getters without a setter, for `'get'`.
    Get,
    /// Getters with a setter, for `'set'`.
    Set,
}

/// The options of an `inspect` call, Node.js's `inspectDefaultOptions` keys.
#[derive(Clone, Debug)]
pub struct InspectOptions {
    pub show_hidden: bool,
    /// How many levels of nested objects to show, or `None` for `null`, all of them.
    pub depth: Option<f64>,
    pub colors: bool,
    pub custom_inspect: bool,
    pub show_proxy: bool,
    pub max_array_length: f64,
    pub max_string_length: f64,
    pub break_length: f64,
    pub compact: Compact,
    pub sorted: bool,
    pub getters: Getters,
    pub numeric_separator: bool,
}

impl Default for InspectOptions {
    fn default() -> Self {
        Self {
            show_hidden: false,
            depth: Some(2.0),
            colors: false,
            custom_inspect: true,
            show_proxy: false,
            max_array_length: 100.0,
            max_string_length: 10000.0,
            break_length: 80.0,
            compact: Compact::Number(3.0),
            sorted: false,
            getters: Getters::None,
            numeric_separator: false,
        }
    }
}

impl InspectOptions {
    /// The options `util.inspect.defaultOptions` holds.
    pub fn current(scope: &Scope<'_>) -> Result<Self, ExnThrown> {
        let defaults = Self::default();
        let Some(util) = core_runtime::module::builtin_namespace(scope, "node:util")? else {
            return Ok(defaults);
        };
        let Ok(inspect) = Object::from_value(scope, *util.get_property(scope, c"inspect")?) else {
            return Ok(defaults);
        };
        match Object::from_value(scope, *inspect.get_property(scope, c"defaultOptions")?) {
            Ok(options) => defaults.read(scope, options),
            Err(_) => Ok(defaults),
        }
    }

    /// Read the options from an `inspect` options object, starting from `self`. As in Node.js,
    /// only own properties are read, each once.
    pub fn read(mut self, scope: &Scope<'_>, options: Object<'_>) -> Result<Self, ExnThrown> {
        let names: [&CStr; 12] = [
            c"showHidden",
            c"depth",
            c"colors",
            c"customInspect",
            c"showProxy",
            c"maxArrayLength",
            c"maxStringLength",
            c"breakLength",
            c"compact",
            c"sorted",
            c"getters",
            c"numericSeparator",
        ];
        for name in names {
            if options.has_own_property(scope, name)? {
                let value = options.get_property(scope, name)?;
                self.set(scope, name, value)?;
            }
        }
        Ok(self)
    }

    /// Set the option `name` to `value`, as Node.js's `inspect` copies an option.
    pub fn set(
        &mut self,
        scope: &Scope<'_>,
        name: &CStr,
        value: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        let truthy = || bool::from_jsval(scope, value, ()).unwrap_or(false);
        let number = || f64::from_jsval(scope, value, ()).map_err(|_| ExnThrown);
        match name.to_bytes() {
            b"showHidden" => self.show_hidden = truthy(),
            b"depth" => {
                self.depth = if value.is_null() {
                    None
                } else {
                    Some(number()?)
                }
            }
            b"colors" => self.colors = truthy(),
            b"customInspect" => self.custom_inspect = truthy(),
            b"showProxy" => self.show_proxy = truthy(),
            b"maxArrayLength" => {
                self.max_array_length = if value.is_null() {
                    f64::INFINITY
                } else {
                    number()?
                }
            }
            b"maxStringLength" => {
                self.max_string_length = if value.is_null() {
                    f64::INFINITY
                } else {
                    number()?
                }
            }
            b"breakLength" => self.break_length = number()?,
            b"compact" => {
                self.compact = if value.is_boolean() {
                    Compact::Bool(value.to_boolean())
                } else if value.is_number() {
                    Compact::Number(value.to_number())
                } else {
                    Compact::Bool(false)
                }
            }
            b"sorted" => self.sorted = truthy(),
            b"getters" => {
                self.getters = if value.is_boolean() && value.to_boolean() {
                    Getters::All
                } else if value.is_string() {
                    match String::from_jsval(scope, value, ())
                        .map_err(|_| ExnThrown)?
                        .as_str()
                    {
                        "get" => Getters::Get,
                        "set" => Getters::Set,
                        _ => Getters::None,
                    }
                } else {
                    Getters::None
                }
            }
            b"numericSeparator" => self.numeric_separator = truthy(),
            _ => {}
        }
        Ok(())
    }

    /// The options object a custom inspection method receives, with a `stylize` function that
    /// returns its first argument.
    pub fn to_object<'r>(&self, scope: &'r Scope<'_>) -> Result<Object<'r>, ExnThrown> {
        let object = Object::new_plain(scope)?;
        let stylize = Function::new_callback(scope, c"stylize", 2, stylize_no_color, ())?;
        object.set_property(scope, c"stylize", *stylize)?;
        object.set_property(scope, c"showHidden", self.show_hidden)?;
        match self.depth {
            Some(depth) => object.set_property(scope, c"depth", depth)?,
            None => object.set_property(scope, c"depth", HandleValue::null())?,
        }
        object.set_property(scope, c"colors", self.colors)?;
        object.set_property(scope, c"customInspect", self.custom_inspect)?;
        object.set_property(scope, c"showProxy", self.show_proxy)?;
        object.set_property(scope, c"maxArrayLength", self.max_array_length)?;
        object.set_property(scope, c"maxStringLength", self.max_string_length)?;
        object.set_property(scope, c"breakLength", self.break_length)?;
        match self.compact {
            Compact::Bool(compact) => object.set_property(scope, c"compact", compact)?,
            Compact::Number(compact) => object.set_property(scope, c"compact", compact)?,
        }
        object.set_property(scope, c"sorted", self.sorted)?;
        match self.getters {
            Getters::None => object.set_property(scope, c"getters", false)?,
            Getters::All => object.set_property(scope, c"getters", true)?,
            Getters::Get => object.set_property(scope, c"getters", "get")?,
            Getters::Set => object.set_property(scope, c"getters", "set")?,
        }
        object.set_property(scope, c"numericSeparator", self.numeric_separator)?;
        Ok(object)
    }
}

/// Node.js's `stylizeNoColor`: the first argument as it is.
fn stylize_no_color(
    _scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    Ok(args.get(0).get())
}

/// A rooted id of the well-known symbol `code`.
fn well_known<'r>(scope: &'r Scope<'_>, code: SymbolCode) -> js::native::HandleId<'r> {
    scope.root_id(js::symbol::get_well_known_key(scope, code))
}

/// A property descriptor, as `Object.getOwnPropertyDescriptor` returns it.
struct Descriptor<'r> {
    value: HandleValue<'r>,
    get: HandleValue<'r>,
    set: HandleValue<'r>,
    enumerable: bool,
    /// Whether this is a data descriptor.
    data: bool,
}

/// The realm's functions an inspection or comparison calls, looked up once.
struct Intrinsics<'s> {
    get_own_property_descriptor: HandleValue<'s>,
    to_string: HandleValue<'s>,
    custom: HandleValue<'s>,
    /// `util.inspect`, which custom inspection methods receive.
    inspect: HandleValue<'s>,
}

impl<'s> Intrinsics<'s> {
    fn new(scope: &'s Scope<'_>) -> Result<Self, ExnThrown> {
        let object = global_object(scope, c"Object")?;
        let get_own_property_descriptor =
            object.get_property(scope, c"getOwnPropertyDescriptor")?;
        let proto = Object::from_value(scope, *object.get_property(scope, c"prototype")?)
            .map_err(|e| e.throw(scope))?;
        let to_string = proto.get_property(scope, c"toString")?;
        let custom = crate::symbol(scope, INSPECT_CUSTOM, true)?;
        let inspect = match core_runtime::module::builtin_namespace(scope, "node:util")? {
            Some(util) => util.get_property(scope, c"inspect")?,
            None => HandleValue::undefined(),
        };
        Ok(Self {
            get_own_property_descriptor,
            to_string,
            custom,
            inspect,
        })
    }

    /// `Object.getOwnPropertyDescriptor(object, key)`, or `None` if `object` has no such
    /// property.
    fn descriptor<'r>(
        &self,
        scope: &'r Scope<'_>,
        object: Object<'_>,
        key: HandleValue<'_>,
    ) -> Result<Option<Descriptor<'r>>, ExnThrown> {
        let descriptor = Function::call(
            scope,
            HandleValue::undefined(),
            self.get_own_property_descriptor,
            &[object.to_jsval_throwing(scope)?, key],
        )?;
        let Ok(descriptor) = Object::from_value(scope, *descriptor) else {
            return Ok(None);
        };
        let enumerable = descriptor.get_property(scope, c"enumerable")?;
        Ok(Some(Descriptor {
            value: descriptor.get_property(scope, c"value")?,
            get: descriptor.get_property(scope, c"get")?,
            set: descriptor.get_property(scope, c"set")?,
            enumerable: enumerable.is_boolean() && enumerable.to_boolean(),
            data: !descriptor.get_property(scope, c"writable")?.is_undefined(),
        }))
    }

    /// The descriptor of `object`'s own property `name`.
    fn named_descriptor<'r>(
        &self,
        scope: &'r Scope<'_>,
        object: Object<'_>,
        name: &str,
    ) -> Result<Option<Descriptor<'r>>, ExnThrown> {
        let key = scope.root_value(js::JSString::from_str(scope, name)?.as_value());
        self.descriptor(scope, object, key)
    }

    /// `Object.prototype.toString.call(object)`.
    fn object_to_string(&self, scope: &Scope<'_>, object: Object<'_>) -> Result<String, ExnThrown> {
        let tag = Function::call(scope, object, self.to_string, EmptyArgs)?;
        String::from_jsval(scope, tag, ()).map_err(|_| ExnThrown)
    }

    /// The tag in `Object.prototype.toString.call(object)`, such as `Date` for `[object Date]`.
    fn builtin_tag(&self, scope: &Scope<'_>, object: Object<'_>) -> Result<String, ExnThrown> {
        let tag = self.object_to_string(scope, object)?;
        Ok(tag
            .strip_prefix("[object ")
            .and_then(|tag| tag.strip_suffix(']'))
            .unwrap_or("Object")
            .to_string())
    }
}

/// The global `name` as an object.
fn global_object<'s>(scope: &'s Scope<'_>, name: &'static CStr) -> Result<Object<'s>, ExnThrown> {
    Object::from_value(scope, *scope.global().get_property(scope, name)?)
        .map_err(|e| e.throw(scope))
}

/// Whether `value` is an object whose `typeof` is `'object'`: not `null` and not callable.
fn is_typeof_object(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
    Object::from_value(scope, *value).is_ok_and(|object| !object.is_callable())
}

/// Whether `a` and `b` are the same object.
fn same_object(a: HandleValue<'_>, b: Object<'_>) -> bool {
    a.is_object() && a.to_object() == b.as_raw()
}

// ---------------------------------------------------------------------------
// Brand checks, shared with `util.types`
// ---------------------------------------------------------------------------

/// The kinds of boxed primitive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Boxed {
    Number,
    String,
    Boolean,
    BigInt,
    Symbol,
}

impl Boxed {
    const ALL: [Boxed; 5] = [
        Boxed::Number,
        Boxed::String,
        Boxed::Boolean,
        Boxed::BigInt,
        Boxed::Symbol,
    ];

    fn name(self) -> &'static str {
        match self {
            Boxed::Number => "Number",
            Boxed::String => "String",
            Boxed::Boolean => "Boolean",
            Boxed::BigInt => "BigInt",
            Boxed::Symbol => "Symbol",
        }
    }

    fn global(self) -> &'static CStr {
        match self {
            Boxed::Number => c"Number",
            Boxed::String => c"String",
            Boxed::Boolean => c"Boolean",
            Boxed::BigInt => c"BigInt",
            Boxed::Symbol => c"Symbol",
        }
    }
}

/// `object[Symbol.toStringTag]` if it is a string, with an exception from a getter cleared.
fn string_tag(scope: &Scope<'_>, object: Object<'_>) -> Option<String> {
    match object.get_property_by_id(scope, well_known(scope, SymbolCode::toStringTag)) {
        Ok(tag) if tag.is_string() => String::from_jsval(scope, tag, ()).ok(),
        Ok(_) => None,
        Err(ExnThrown) => {
            js::exception::clear(scope);
            None
        }
    }
}

/// The primitive `X.prototype.valueOf.call(object)` returns for the global `X` of `kind`, or
/// `None` if `object` isn't a boxed primitive of that kind.
fn unbox<'r>(
    scope: &'r Scope<'_>,
    object: Object<'_>,
    kind: Boxed,
) -> Result<Option<HandleValue<'r>>, ExnThrown> {
    let Ok(ctor) = Object::from_value(scope, *scope.global().get_property(scope, kind.global())?)
    else {
        return Ok(None);
    };
    let proto = Object::from_value(scope, *ctor.get_property(scope, c"prototype")?)
        .map_err(|e| e.throw(scope))?;
    let value_of = proto.get_property(scope, c"valueOf")?;
    let this = scope.root_value(object.to_jsval_raw_throwing(scope)?);
    match Function::call(scope, this, value_of, EmptyArgs) {
        Ok(value) => Ok(Some(value)),
        Err(ExnThrown) => {
            js::exception::clear(scope);
            Ok(None)
        }
    }
}

/// The kind of boxed primitive `object` is, if it is one. The builtin `valueOf` methods check
/// the brand, and are tried only if `Object.prototype.toString` names a boxed type or a
/// `Symbol.toStringTag` could hide one.
pub(crate) fn boxed_kind(
    scope: &Scope<'_>,
    object: Object<'_>,
) -> Result<Option<Boxed>, ExnThrown> {
    let intrinsics = Intrinsics::new(scope)?;
    boxed_kind_with(scope, &intrinsics, object)
}

fn boxed_kind_with(
    scope: &Scope<'_>,
    intrinsics: &Intrinsics<'_>,
    object: Object<'_>,
) -> Result<Option<Boxed>, ExnThrown> {
    let tag = intrinsics.builtin_tag(scope, object)?;
    let candidates: Vec<Boxed> = match Boxed::ALL.into_iter().find(|kind| kind.name() == tag) {
        Some(kind) => vec![kind],
        None if string_tag(scope, object).is_some() => Boxed::ALL.to_vec(),
        None => return Ok(None),
    };
    for kind in candidates {
        if unbox(scope, object, kind)?.is_some() {
            return Ok(Some(kind));
        }
    }
    Ok(None)
}

/// Whether `Object.prototype.toString` gives `object` one of `tags` from its builtin tag or an
/// inherited `Symbol.toStringTag`: an own `Symbol.toStringTag` doesn't count.
pub(crate) fn has_builtin_tag(
    scope: &Scope<'_>,
    object: Object<'_>,
    tags: &[&str],
) -> Result<bool, ExnThrown> {
    let intrinsics = Intrinsics::new(scope)?;
    has_builtin_tag_with(scope, &intrinsics, object, tags)
}

fn has_builtin_tag_with(
    scope: &Scope<'_>,
    intrinsics: &Intrinsics<'_>,
    object: Object<'_>,
    tags: &[&str],
) -> Result<bool, ExnThrown> {
    let tag = intrinsics.builtin_tag(scope, object)?;
    if !tags.contains(&tag.as_str()) {
        return Ok(false);
    }
    Ok(!object.has_own_property_by_id(scope, well_known(scope, SymbolCode::toStringTag))?)
}

/// Whether `object` is an arguments object.
pub(crate) fn is_arguments(scope: &Scope<'_>, object: Object<'_>) -> Result<bool, ExnThrown> {
    let intrinsics = Intrinsics::new(scope)?;
    is_arguments_with(scope, &intrinsics, object)
}

fn is_arguments_with(
    scope: &Scope<'_>,
    intrinsics: &Intrinsics<'_>,
    object: Object<'_>,
) -> Result<bool, ExnThrown> {
    Ok(
        intrinsics.builtin_tag(scope, object)? == "Arguments"
            && string_tag(scope, object).is_none(),
    )
}

/// Whether `object` is an `ArrayBuffer`.
pub(crate) fn is_array_buffer(object: Object<'_>) -> bool {
    object.cast::<js::ArrayBuffer>().is_ok()
}

/// Whether `object` is a `SharedArrayBuffer`, by its builtin tag.
pub(crate) fn is_shared_array_buffer(
    scope: &Scope<'_>,
    object: Object<'_>,
) -> Result<bool, ExnThrown> {
    has_builtin_tag(scope, object, &["SharedArrayBuffer"])
}

/// Whether `object` is a `WeakSet`, by `WeakSet.prototype.has`'s brand check.
pub(crate) fn is_weak_set(scope: &Scope<'_>, object: Object<'_>) -> Result<bool, ExnThrown> {
    let Ok(ctor) = Object::from_value(scope, *scope.global().get_property(scope, c"WeakSet")?)
    else {
        return Ok(false);
    };
    let proto = Object::from_value(scope, *ctor.get_property(scope, c"prototype")?)
        .map_err(|e| e.throw(scope))?;
    let has = proto.get_property(scope, c"has")?;
    let probe = Object::new_plain(scope)?;
    match Function::call(scope, object, has, &[probe.to_jsval_throwing(scope)?]) {
        Ok(_) => Ok(true),
        Err(ExnThrown) => {
            js::exception::clear(scope);
            Ok(false)
        }
    }
}

/// Whether `object` is a `WeakMap`.
pub(crate) fn is_weak_map(scope: &Scope<'_>, object: Object<'_>) -> Result<bool, ExnThrown> {
    js::WeakMap::is_weak_map(scope, object)
}

/// Whether `object` is a typed array or a `DataView`.
fn view_of(object: Object<'_>) -> Option<js::ArrayBufferView<'_>> {
    js::ArrayBufferView::from_object(object)
}

/// Whether `object` is a `Date`.
pub(crate) fn is_date(scope: &Scope<'_>, object: Object<'_>) -> Result<bool, ExnThrown> {
    js::Date::is_date(scope, object.handle())
}

/// Whether `object` is a `RegExp`.
pub(crate) fn is_regexp(scope: &Scope<'_>, object: Object<'_>) -> Result<bool, ExnThrown> {
    js::RegExp::is_regexp(scope, object.handle())
}

/// Whether `object` is a `Map`.
pub(crate) fn is_map(scope: &Scope<'_>, object: Object<'_>) -> Result<bool, ExnThrown> {
    js::Map::is_map(scope, object.handle())
}

/// Whether `object` is a `Set`.
pub(crate) fn is_set(scope: &Scope<'_>, object: Object<'_>) -> Result<bool, ExnThrown> {
    js::Set::is_set(scope, object.handle())
}

/// Whether `object` is an `Error`.
pub(crate) fn is_error(scope: &Scope<'_>, object: Object<'_>) -> Result<bool, ExnThrown> {
    Ok(js::exception::is_error_object(
        scope,
        scope.root_value(object.to_jsval_raw_throwing(scope)?),
    ))
}

/// The entries of the `Map` or `Set` `object`, as `[key, value]` pairs, with each value of a
/// `Set` as both. `forEach`'s builtin implementation reads them.
fn collection_entries<'r>(
    scope: &'r Scope<'_>,
    object: Object<'_>,
    map: bool,
) -> Result<Vec<(HandleValue<'r>, HandleValue<'r>)>, ExnThrown> {
    let store = js::Array::with_contents(scope, EmptyArgs)?;
    let callback = Function::new_callback(scope, c"", 2, collect_entry, *store)?;
    let callback = callback.to_jsval_throwing(scope)?;
    if map {
        let map = object
            .cast::<js::Map>()
            .map_err(|_| js::error::TypeError("not a Map".into()).throw(scope))?;
        map.for_each(scope, callback, HandleValue::undefined())?;
    } else {
        let set = object
            .cast::<js::Set>()
            .map_err(|_| js::error::TypeError("not a Set".into()).throw(scope))?;
        set.for_each(scope, callback, HandleValue::undefined())?;
    }
    let length = store.length(scope)?;
    let mut entries = Vec::with_capacity(length as usize / 2);
    for index in (0..length).step_by(2) {
        entries.push((
            store.get_element(scope, index)?,
            store.get_element(scope, index + 1)?,
        ));
    }
    Ok(entries)
}

/// The `forEach` callback of [`collection_entries`], whose payload is the array it appends the
/// key and the value to.
fn collect_entry(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let store = Object::from_value(scope, *payload)
        .map_err(|e| e.throw(scope))?
        .cast::<js::Array>()
        .map_err(|_| js::error::TypeError("the payload is not an array".into()).throw(scope))?;
    let length = store.length(scope)?;
    store.define_element(scope, length, args.get(1), 0)?;
    store.define_element(scope, length + 1, args.get(0), 0)?;
    Ok(js::value::undefined())
}

/// The UTF-16 code units of the JS string `value`.
fn string_units(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<Vec<u16>, ExnThrown> {
    let string = js::JSString::from_value(scope, value)?;
    // SAFETY: the closure makes no call into SpiderMonkey.
    unsafe {
        string.with_chars(scope, |chars| match chars {
            js::string::StrChars::Latin1(bytes) => bytes.iter().map(|b| u16::from(*b)).collect(),
            js::string::StrChars::TwoByte(units) => units.to_vec(),
        })
    }
}

/// `ToString(value)`.
fn to_string(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<String, ExnThrown> {
    String::from_jsval(scope, value, ()).map_err(|_| ExnThrown)
}

/// The length of `text` in UTF-16 code units, as JS's `length` counts it.
fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `key` as a string, or `None` for a symbol.
fn key_text(scope: &Scope<'_>, key: HandleValue<'_>) -> Result<Option<String>, ExnThrown> {
    if key.is_symbol() {
        return Ok(None);
    }
    Ok(Some(to_string(scope, key)?))
}

/// Whether `key` is an array index: the canonical form of an integer below 2**32 - 1.
fn is_index(key: &str) -> bool {
    key.parse::<u32>()
        .is_ok_and(|index| index != u32::MAX && index.to_string() == key)
}

/// The id of the property `key`.
fn key_id<'r>(
    scope: &'r Scope<'_>,
    key: HandleValue<'_>,
) -> Result<js::native::HandleId<'r>, ExnThrown> {
    Ok(scope.root_id(js::id::value_to_id(scope, key)?))
}

/// The id of the element `index`.
fn index_id<'r>(scope: &'r Scope<'_>, index: u32) -> Result<js::native::HandleId<'r>, ExnThrown> {
    key_id(
        scope,
        scope.root_value(js::value::from_f64(f64::from(index))),
    )
}

/// `array.length` of an array.
fn array_length(scope: &Scope<'_>, object: Object<'_>) -> Result<u32, ExnThrown> {
    match object.cast::<js::Array>() {
        Ok(array) => array.length(scope),
        Err(_) => Ok(
            f64::from_jsval(scope, object.get_property(scope, c"length")?, ()).unwrap_or(0.0)
                as u32,
        ),
    }
}

// ---------------------------------------------------------------------------
// Strings and numbers
// ---------------------------------------------------------------------------

/// The escape `inspect` writes for the code unit `unit`, if it has one: Node.js's `meta` table.
fn meta(unit: u16, single_quote: bool) -> Option<String> {
    Some(match unit {
        0x08 => "\\b".into(),
        0x09 => "\\t".into(),
        0x0a => "\\n".into(),
        0x0c => "\\f".into(),
        0x0d => "\\r".into(),
        0x27 if single_quote => "\\'".into(),
        0x5c => "\\\\".into(),
        0x00..=0x1f | 0x7f..=0x9f => format!("\\x{unit:02X}"),
        _ => return None,
    })
}

/// Escape the control characters, backslashes, lone surrogates and, if `single_quote`, single
/// quotes of `units`.
fn escape_units(units: &[u16], single_quote: bool) -> String {
    let mut out = String::with_capacity(units.len());
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        if let Some(escape) = meta(unit, single_quote) {
            out.push_str(&escape);
        } else if (0xd800..0xdc00).contains(&unit)
            && units
                .get(index + 1)
                .is_some_and(|next| (0xdc00..0xe000).contains(next))
        {
            let pair = [unit, units[index + 1]];
            out.extend(char::decode_utf16(pair).map(|c| c.unwrap_or('\u{fffd}')));
            index += 1;
        } else if (0xd800..0xe000).contains(&unit) {
            out.push_str(&format!("\\u{unit:x}"));
        } else {
            out.push(char::from_u32(u32::from(unit)).unwrap_or('\u{fffd}'));
        }
        index += 1;
    }
    out
}

/// Node.js's `strEscape`: `units` as a quoted string literal, in single quotes, or double quotes
/// or backticks if that saves escaping single quotes.
fn str_escape(units: &[u16]) -> String {
    let contains = |unit: u8| units.contains(&u16::from(unit));
    let mut quote = '\'';
    if contains(b'\'') {
        if !contains(b'"') {
            quote = '"';
        } else if !contains(b'`') && !units.windows(2).any(|pair| pair == [0x24, 0x7b]) {
            quote = '`';
        }
    }
    let body = escape_units(units, quote == '\'');
    format!("{quote}{body}{quote}")
}

/// Node.js's `addNumericSeparator`: `_` between groups of three digits from the end.
fn add_numeric_separator(integer: &str) -> String {
    let start = usize::from(integer.starts_with('-'));
    let mut end = integer.len();
    let mut groups = Vec::new();
    while end >= start + 4 {
        groups.push(&integer[end - 3..end]);
        end -= 3;
    }
    let mut out = integer[..end].to_string();
    for group in groups.iter().rev() {
        out.push('_');
        out.push_str(group);
    }
    out
}

/// Node.js's `addNumericSeparatorEnd`: `_` between groups of three digits from the start.
fn add_numeric_separator_end(digits: &str) -> String {
    let mut out = String::new();
    let mut index = 0;
    while index + 3 < digits.len() {
        out.push_str(&digits[index..index + 3]);
        out.push('_');
        index += 3;
    }
    out.push_str(&digits[index..]);
    out
}

/// Node.js's `formatNumber`: `-0` keeps its sign, and with `numeric_separator`, the digits are
/// grouped by `_`.
fn format_number(number: f64, numeric_separator: bool) -> String {
    if number == 0.0 && number.is_sign_negative() {
        return "-0".into();
    }
    let text = number_to_string(number);
    if !numeric_separator || !number.is_finite() || text.contains('e') {
        return text;
    }
    match text.split_once('.') {
        None => add_numeric_separator(&text),
        Some((integer, fraction)) => format!(
            "{}.{}",
            add_numeric_separator(integer),
            add_numeric_separator_end(fraction)
        ),
    }
}

/// Node.js's `formatBigInt` for the digits `text`.
fn format_bigint(text: &str, numeric_separator: bool) -> String {
    if numeric_separator {
        format!("{}n", add_numeric_separator(text))
    } else {
        format!("{text}n")
    }
}

/// Whether the code point `code` takes two columns, by Node.js's `isFullWidthCodePoint`.
fn is_full_width(code: u32) -> bool {
    code >= 0x1100
        && (code <= 0x115f
            || code == 0x2329
            || code == 0x232a
            || ((0x2e80..=0x3247).contains(&code) && code != 0x303f)
            || (0x3250..=0x4dbf).contains(&code)
            || (0x4e00..=0xa4c6).contains(&code)
            || (0xa960..=0xa97c).contains(&code)
            || (0xac00..=0xd7a3).contains(&code)
            || (0xf900..=0xfaff).contains(&code)
            || (0xfe10..=0xfe19).contains(&code)
            || (0xfe30..=0xfe6b).contains(&code)
            || (0xff01..=0xff60).contains(&code)
            || (0xffe0..=0xffe6).contains(&code)
            || (0x1b000..=0x1b001).contains(&code)
            || (0x1f200..=0x1f251).contains(&code)
            || (0x1f300..=0x1f64f).contains(&code)
            || (0x20000..=0x3fffd).contains(&code))
}

/// Whether the code point `code` takes no column, by Node.js's `isZeroWidthCodePoint`.
fn is_zero_width(code: u32) -> bool {
    code <= 0x1f
        || (0x7f..=0x9f).contains(&code)
        || (0x300..=0x36f).contains(&code)
        || (0x200b..=0x200f).contains(&code)
        || (0x20d0..=0x20ff).contains(&code)
        || (0xfe00..=0xfe0f).contains(&code)
        || (0xfe20..=0xfe2f).contains(&code)
        || (0xe0100..=0xe01ef).contains(&code)
}

/// The columns `text` takes on a terminal, without its escape sequences.
fn string_width(text: &str) -> usize {
    crate::util::strip_vt_control_characters(text)
        .chars()
        .map(|c| {
            let code = c as u32;
            if is_full_width(code) {
                2
            } else if is_zero_width(code) {
                0
            } else {
                1
            }
        })
        .sum()
}

/// Node.js's `getPrefix`: the constructor name, size and tag an object's braces start with.
fn get_prefix(constructor: Option<&str>, tag: &str, fallback: &str, size: &str) -> String {
    let Some(constructor) = constructor else {
        if !tag.is_empty() && fallback != tag {
            return format!("[{fallback}{size}: null prototype] [{tag}] ");
        }
        return format!("[{fallback}{size}: null prototype] ");
    };
    let mut result = format!("{constructor}{size} ");
    if !tag.is_empty() {
        match constructor.find(tag) {
            None => result.push_str(&format!("[{tag}] ")),
            Some(position) => {
                let end = position + tag.len();
                if let Some(next) = constructor[end..].chars().next() {
                    if next.to_lowercase().eq(std::iter::once(next)) {
                        result.push_str(&format!("[{tag}] "));
                    }
                }
            }
        }
    }
    result
}

/// Node.js's `getIteratorBraces`.
fn iterator_braces(kind: &str, tag: &str) -> (String, String) {
    let mut tag = tag.to_string();
    let name = format!("{kind} Iterator");
    if tag != name {
        if !tag.is_empty() {
            tag.push_str("] [");
        }
        tag.push_str(&name);
    }
    (format!("[{tag}] {{"), "}".into())
}

/// Node.js's `remainingText`.
fn remaining_text(remaining: f64) -> String {
    format!(
        "... {} more item{}",
        number_to_string(remaining),
        if remaining > 1.0 { "s" } else { "" }
    )
}

/// The number of entries out of `length` shown with `max_array_length`.
fn shown_entries(max_array_length: f64, length: usize) -> usize {
    let max = max_array_length.max(0.0);
    if max >= length as f64 {
        length
    } else {
        max as usize
    }
}

/// `ms` since the epoch as `Date.prototype.toISOString` writes it.
fn iso_string(ms: f64) -> String {
    let days = (ms / 86_400_000.0).floor();
    let time = ms - days * 86_400_000.0;
    // Howard Hinnant's `civil_from_days`.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let time = time as i64;
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{:06}", -year)
    } else {
        format!("+{year:06}")
    };
    format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        time / 3_600_000,
        time / 60_000 % 60,
        time / 1000 % 60,
        time % 1000
    )
}

/// Which entries an object's output holds before its properties, Node.js's `kObjectType`,
/// `kArrayType` and `kArrayAndKeysType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Extras {
    Object,
    Array,
    ArrayAndKeys,
}

/// How an object's entries before its properties are formatted.
enum Formatter {
    None,
    Array,
    TypedArray,
    Set,
    Map,
    ArrayBuffer,
    Promise,
    WeakCollection,
    Iterator,
}

/// An own property to show: its key and its descriptor.
struct Key<'r> {
    key: HandleValue<'r>,
    descriptor: Descriptor<'r>,
}

/// An inspection in progress, Node.js's `ctx`.
struct Inspector<'s> {
    options: InspectOptions,
    intrinsics: Intrinsics<'s>,
    /// The objects being inspected, outermost first, to detect cycles.
    seen: Vec<RootedHeap<js::object::Object>>,
    /// The objects found to be circular, numbered by their position plus one.
    circular: Vec<RootedHeap<js::object::Object>>,
    indentation: usize,
    current_depth: usize,
}

impl<'s> Inspector<'s> {
    fn new(scope: &'s Scope<'_>, options: InspectOptions) -> Result<Self, ExnThrown> {
        Ok(Self {
            options,
            intrinsics: Intrinsics::new(scope)?,
            seen: Vec::new(),
            circular: Vec::new(),
            indentation: 0,
            current_depth: 0,
        })
    }

    /// Whether `recurse` levels are more than the `depth` option shows.
    fn depth_exceeded(&self, recurse: usize) -> bool {
        self.options
            .depth
            .is_some_and(|limit| recurse as f64 > limit)
    }

    /// Node.js's `formatPrimitive` for a value that isn't an object.
    fn format_primitive(
        &self,
        scope: &Scope<'_>,
        value: HandleValue<'_>,
    ) -> Result<String, ExnThrown> {
        let options = &self.options;
        if value.is_string() {
            let mut units = string_units(scope, value)?;
            let mut trailer = String::new();
            if units.len() as f64 > options.max_string_length {
                let max = options.max_string_length.max(0.0) as usize;
                let remaining = units.len() - max;
                units.truncate(max);
                trailer = format!(
                    "... {remaining} more character{}",
                    if remaining > 1 { "s" } else { "" }
                );
            }
            let length = units.len() as f64;
            if options.compact != Compact::Bool(true)
                && length > 16.0
                && length > options.break_length - self.indentation as f64 - 4.0
            {
                let lines: Vec<String> = units
                    .split_inclusive(|unit| *unit == u16::from(b'\n'))
                    .map(str_escape)
                    .collect();
                let separator = format!(" +\n{}", " ".repeat(self.indentation + 2));
                return Ok(format!("{}{trailer}", lines.join(&separator)));
            }
            return Ok(format!("{}{trailer}", str_escape(&units)));
        }
        if value.is_number() {
            return Ok(format_number(value.to_number(), options.numeric_separator));
        }
        if value.is_bigint() {
            let digits = js::JSString::from_value(scope, value)?.to_utf8(scope)?;
            return Ok(format_bigint(&digits, options.numeric_separator));
        }
        if value.is_boolean() {
            return Ok(value.to_boolean().to_string());
        }
        if value.is_undefined() {
            return Ok("undefined".into());
        }
        if value.is_null() {
            return Ok("null".into());
        }
        Ok(symbol_to_string(scope, value))
    }

    /// Node.js's `formatValue`: inspect `value`, `recurse` levels below the top.
    fn format_value(
        &mut self,
        scope: &Scope<'_>,
        value: HandleValue<'_>,
        recurse: usize,
        typed_array: bool,
    ) -> Result<String, ExnThrown> {
        let Ok(object) = Object::from_value(scope, *value) else {
            return self.format_primitive(scope, value);
        };
        let inner = scope.inner_scope();
        let scope: &Scope<'_> = &inner;

        if self.options.custom_inspect {
            let custom =
                object.get_property_by_id(scope, key_id(scope, self.intrinsics.custom)?)?;
            let callable = Object::from_value(scope, *custom).is_ok_and(|f| f.is_callable());
            let is_inspect = custom.is_object()
                && self.intrinsics.inspect.is_object()
                && custom.to_object() == self.intrinsics.inspect.to_object();
            if callable && !is_inspect && !self.is_prototype_object(scope, object)? {
                let depth = match self.options.depth {
                    Some(limit) => scope.root_value(js::value::from_f64(limit - recurse as f64)),
                    None => HandleValue::null(),
                };
                let options = self.options.to_object(scope)?;
                let result = Function::call(
                    scope,
                    object,
                    custom,
                    &[
                        depth,
                        options.to_jsval_throwing(scope)?,
                        self.intrinsics.inspect,
                    ],
                )?;
                if !same_object(result, object) {
                    if !result.is_string() {
                        return self.format_value(scope, result, recurse, false);
                    }
                    let text = to_string(scope, result)?;
                    return Ok(text.replace('\n', &format!("\n{}", " ".repeat(self.indentation))));
                }
            } else if !callable && self.is_buffer(scope, object)? {
                return self.format_buffer(scope, object);
            }
        }

        if self.seen.iter().any(|seen| *seen == object) {
            let index = match self.circular.iter().position(|seen| *seen == object) {
                Some(index) => index,
                None => {
                    self.circular.push(RootedHeap::new(object));
                    self.circular.len() - 1
                }
            };
            return Ok(format!("[Circular *{}]", index + 1));
        }
        self.format_raw(scope, object, recurse, typed_array)
    }

    /// Whether `object` is a prototype object: the `prototype` of its own `constructor`.
    fn is_prototype_object(
        &self,
        scope: &Scope<'_>,
        object: Object<'_>,
    ) -> Result<bool, ExnThrown> {
        let Some(descriptor) = self
            .intrinsics
            .named_descriptor(scope, object, "constructor")?
        else {
            return Ok(false);
        };
        let Ok(ctor) = Object::from_value(scope, *descriptor.value) else {
            return Ok(false);
        };
        Ok(same_object(ctor.get_property(scope, c"prototype")?, object))
    }

    /// Whether `object` is a `Buffer`.
    fn is_buffer(&self, scope: &Scope<'_>, object: Object<'_>) -> Result<bool, ExnThrown> {
        if view_of(object).is_none() {
            return Ok(false);
        }
        let value = scope.root_value(object.to_jsval_raw_throwing(scope)?);
        crate::buffer::buffer::is_buffer(scope, value).map_err(|error| error.throw(scope))
    }

    /// `Buffer.prototype[inspect.custom]`: the first 50 bytes in hex, and the properties that
    /// aren't indices.
    fn format_buffer(
        &mut self,
        scope: &Scope<'_>,
        object: Object<'_>,
    ) -> Result<String, ExnThrown> {
        let bytes = view_of(object)
            .map(|view| view.copy_bytes(scope))
            .transpose()?
            .unwrap_or_default();
        let max = crate::buffer::inspect_max_bytes() as usize;
        let mut text: Vec<String> = bytes.iter().take(max).map(|b| format!("{b:02x}")).collect();
        let mut text = std::mem::take(&mut text).join(" ");
        if bytes.len() > max {
            let remaining = bytes.len() - max;
            text.push_str(&format!(
                " ... {remaining} more byte{}",
                if remaining > 1 { "s" } else { "" }
            ));
        }
        let keys = self.non_index_keys(scope, object)?;
        if !keys.is_empty() {
            let extras = Object::new_plain(scope)?;
            extras.set_prototype(scope, js::native::HandleObject::null())?;
            for key in &keys {
                let id = key_id(scope, key.key)?;
                extras.set_property_by_id(scope, id, object.get_property_by_id(scope, id)?)?;
            }
            let mut options = self.options.clone();
            options.break_length = f64::INFINITY;
            options.compact = Compact::Bool(true);
            let shown = inspect(
                scope,
                scope.root_value(extras.to_jsval_raw_throwing(scope)?),
                options,
            )?;
            if !bytes.is_empty() {
                text.push_str(", ");
            }
            let units: Vec<u16> = shown.encode_utf16().collect();
            if units.len() > 29 {
                text.push_str(&String::from_utf16_lossy(&units[27..units.len() - 2]));
            }
        }
        let ctor = object.get_property(scope, c"constructor")?;
        let name = match Object::from_value(scope, *ctor) {
            Ok(ctor) => to_string(scope, ctor.get_property(scope, c"name")?)?,
            Err(_) => "Buffer".into(),
        };
        Ok(format!("<{name} {text}>"))
    }

    /// Node.js's `getConstructorName`, or `None` for an object with a null prototype.
    fn constructor_name(
        &mut self,
        scope: &Scope<'_>,
        object: Object<'_>,
        recurse: usize,
    ) -> Result<Option<String>, ExnThrown> {
        let mut current = Some(object);
        let mut first_proto: Option<Option<Object<'_>>> = None;
        while let Some(candidate) = current {
            if let Some(descriptor) =
                self.intrinsics
                    .named_descriptor(scope, candidate, "constructor")?
            {
                if let Ok(ctor) = Object::from_value(scope, *descriptor.value) {
                    if ctor.is_callable() {
                        let name = ctor.get_property(scope, c"name")?;
                        let empty = name.is_string() && to_string(scope, name)?.is_empty();
                        if !empty && instance_of(scope, object, ctor)? {
                            return Ok(Some(to_string(scope, name)?));
                        }
                    }
                }
            }
            current = candidate.get_prototype(scope)?;
            if first_proto.is_none() {
                first_proto = Some(current);
            }
        }
        let Some(Some(first_proto)) = first_proto else {
            return Ok(None);
        };
        let internal = if object.is_callable() {
            "Function"
        } else if js::Array::is_array(scope, object.handle())? {
            "Array"
        } else {
            "Object"
        };
        if self.depth_exceeded(recurse) {
            return Ok(Some(format!("{internal} <Complex prototype>")));
        }
        match self.constructor_name(scope, first_proto, recurse + 1)? {
            Some(proto) => Ok(Some(format!("{internal} <{proto}>"))),
            None => {
                let mut options = self.options.clone();
                options.custom_inspect = false;
                options.depth = Some(-1.0);
                let proto = scope.root_value(first_proto.to_jsval_raw_throwing(scope)?);
                Ok(Some(format!(
                    "{internal} <{}>",
                    inspect(scope, proto, options)?
                )))
            }
        }
    }

    /// The tag `inspect` shows for `object`: its `Symbol.toStringTag` if that is a string that
    /// isn't an own enumerable property (or with `showHidden`, an own property).
    fn tag(&self, scope: &Scope<'_>, object: Object<'_>) -> Result<String, ExnThrown> {
        let Some(tag) = string_tag(scope, object) else {
            return Ok(String::new());
        };
        if tag.is_empty() {
            return Ok(tag);
        }
        let key = well_known(scope, SymbolCode::toStringTag);
        let listed = if self.options.show_hidden {
            object.has_own_property_by_id(scope, key)?
        } else {
            let symbol = js::symbol::get_well_known(scope, SymbolCode::toStringTag);
            let symbol = scope.root_value(js::value::from_symbol(symbol));
            self.intrinsics
                .descriptor(scope, object, symbol)?
                .is_some_and(|descriptor| descriptor.enumerable)
        };
        Ok(if listed { String::new() } else { tag })
    }

    /// Node.js's `getKeys`: the own enumerable properties of `object`, or with `showHidden`, all
    /// of them, strings before symbols.
    fn keys<'r>(
        &self,
        scope: &'r Scope<'_>,
        object: Object<'_>,
    ) -> Result<Vec<Key<'r>>, ExnThrown> {
        self.filtered_keys(scope, object, false)
    }

    /// Node.js's `getOwnNonIndexProperties`: [`Self::keys`] without the array indices.
    fn non_index_keys<'r>(
        &self,
        scope: &'r Scope<'_>,
        object: Object<'_>,
    ) -> Result<Vec<Key<'r>>, ExnThrown> {
        self.filtered_keys(scope, object, true)
    }

    fn filtered_keys<'r>(
        &self,
        scope: &'r Scope<'_>,
        object: Object<'_>,
        skip_indices: bool,
    ) -> Result<Vec<Key<'r>>, ExnThrown> {
        let mut keys = Vec::new();
        for key in object.own_keys(scope)? {
            if skip_indices && key_text(scope, key)?.is_some_and(|text| is_index(&text)) {
                continue;
            }
            let Some(descriptor) = self.intrinsics.descriptor(scope, object, key)? else {
                continue;
            };
            if descriptor.enumerable || self.options.show_hidden {
                keys.push(Key { key, descriptor });
            }
        }
        Ok(keys)
    }

    /// Node.js's `getCtxStyle`.
    fn ctx_style(&self, constructor: Option<&str>, tag: &str, object: Object<'_>) -> String {
        let mut fallback = "";
        if constructor.is_none() {
            fallback = if object.is_callable() {
                "Function"
            } else {
                "Object"
            };
            if fallback == tag {
                fallback = "Object";
            }
        }
        get_prefix(constructor, tag, fallback, "")
    }

    /// Node.js's `formatRaw`.
    fn format_raw(
        &mut self,
        scope: &Scope<'_>,
        value: Object<'_>,
        recurse: usize,
        typed_array: bool,
    ) -> Result<String, ExnThrown> {
        let mut typed_array = typed_array;
        let constructor = self.constructor_name(scope, value, recurse)?;
        let constructor = constructor.as_deref();
        let tag = self.tag(scope, value)?;
        let mut base = String::new();
        let mut formatter = Formatter::None;
        let mut braces = ("{".to_string(), "}".to_string());
        let mut extras = Extras::Object;
        let mut extra_keys: &[&CStr] = &[];
        let mut keys = Vec::new();
        let mut no_iterator = true;
        let view = view_of(value);

        let has_iterator =
            value.has_property_by_id(scope, well_known(scope, SymbolCode::iterator))?;
        if has_iterator || constructor.is_none() {
            no_iterator = false;
            if js::Array::is_array(scope, value.handle())? {
                let length = array_length(scope, value)?;
                let prefix = if constructor != Some("Array") || !tag.is_empty() {
                    get_prefix(constructor, &tag, "Array", &format!("({length})"))
                } else {
                    String::new()
                };
                keys = self.non_index_keys(scope, value)?;
                braces = (format!("{prefix}["), "]".into());
                if length == 0 && keys.is_empty() {
                    return Ok(format!("{}]", braces.0));
                }
                extras = Extras::ArrayAndKeys;
                formatter = Formatter::Array;
            } else if is_set(scope, value)? || is_map(scope, value)? {
                let map = is_map(scope, value)?;
                let size = if map {
                    value.cast::<js::Map>().map(|m| m.size(scope)).unwrap_or(0)
                } else {
                    value.cast::<js::Set>().map(|s| s.size(scope)).unwrap_or(0)
                };
                let kind = if map { "Map" } else { "Set" };
                let prefix = get_prefix(constructor, &tag, kind, &format!("({size})"));
                keys = self.keys(scope, value)?;
                formatter = if map { Formatter::Map } else { Formatter::Set };
                if size == 0 && keys.is_empty() {
                    return Ok(format!("{prefix}{{}}"));
                }
                braces = (format!("{prefix}{{"), "}".into());
            } else if let Some(view) = view.filter(|view| view.view_kind().is_typed_array()) {
                keys = self.non_index_keys(scope, value)?;
                let fallback = if constructor.is_none() {
                    view.view_kind().name()
                } else {
                    ""
                };
                let size = view.array_length();
                let prefix = get_prefix(constructor, &tag, fallback, &format!("({size})"));
                braces = (format!("{prefix}["), "]".into());
                if size == 0 && keys.is_empty() && !self.options.show_hidden {
                    return Ok(format!("{}]", braces.0));
                }
                formatter = Formatter::TypedArray;
                extras = Extras::ArrayAndKeys;
                if self.options.show_hidden {
                    extra_keys = &[
                        c"BYTES_PER_ELEMENT",
                        c"length",
                        c"byteLength",
                        c"byteOffset",
                        c"buffer",
                    ];
                    typed_array = true;
                }
            } else if has_builtin_tag_with(scope, &self.intrinsics, value, &["Map Iterator"])? {
                keys = self.keys(scope, value)?;
                braces = iterator_braces("Map", &tag);
                formatter = Formatter::Iterator;
            } else if has_builtin_tag_with(scope, &self.intrinsics, value, &["Set Iterator"])? {
                keys = self.keys(scope, value)?;
                braces = iterator_braces("Set", &tag);
                formatter = Formatter::Iterator;
            } else {
                no_iterator = true;
            }
        }
        if no_iterator {
            keys = self.keys(scope, value)?;
            braces = ("{".into(), "}".into());
            if value.is_callable() {
                base = self.function_base(scope, value, constructor, &tag)?;
                if keys.is_empty() {
                    return Ok(base);
                }
            } else if constructor == Some("Object") {
                if is_arguments_with(scope, &self.intrinsics, value)? {
                    braces.0 = "[Arguments] {".into();
                } else if !tag.is_empty() {
                    braces.0 = format!("{}{{", get_prefix(constructor, &tag, "Object", ""));
                }
                if keys.is_empty() {
                    return Ok(format!("{}}}", braces.0));
                }
            } else if is_regexp(scope, value)? {
                let source = to_string(scope, value.get_property(scope, c"source")?)?;
                let flags = to_string(scope, value.get_property(scope, c"flags")?)?;
                base = format!("/{source}/{flags}");
                let prefix = get_prefix(constructor, &tag, "RegExp", "");
                if prefix != "RegExp " {
                    base = format!("{prefix}{base}");
                }
                if keys.is_empty() || self.depth_exceeded(recurse) {
                    return Ok(base);
                }
            } else if is_date(scope, value)? {
                let time = value
                    .cast::<js::Date>()
                    .map_err(|_| js::error::TypeError("not a Date".into()).throw(scope))?
                    .msec_since_epoch(scope)?;
                base = if time.is_nan() {
                    "Invalid Date".into()
                } else {
                    iso_string(time)
                };
                let prefix = get_prefix(constructor, &tag, "Date", "");
                if prefix != "Date " {
                    base = format!("{prefix}{base}");
                }
                if keys.is_empty() {
                    return Ok(base);
                }
            } else if is_error(scope, value)? {
                base = self.format_error(scope, value, constructor, &tag, &mut keys)?;
                if keys.is_empty() {
                    return Ok(base);
                }
            } else if is_array_buffer(value) || is_shared_array_buffer(scope, value)? {
                let kind = if is_array_buffer(value) {
                    "ArrayBuffer"
                } else {
                    "SharedArrayBuffer"
                };
                let prefix = get_prefix(constructor, &tag, kind, "");
                if !typed_array {
                    formatter = Formatter::ArrayBuffer;
                } else if keys.is_empty() {
                    let length = value.get_property(scope, c"byteLength")?;
                    let length = if length.is_number() {
                        self.format_primitive(scope, length)?
                    } else {
                        String::from_jsval_throwing(scope, length, ())?
                    };
                    return Ok(format!("{prefix}{{ [byteLength]: {length} }}"));
                }
                braces.0 = format!("{prefix}{{");
                extra_keys = &[c"byteLength"];
            } else if view.is_some() {
                braces.0 = format!("{}{{", get_prefix(constructor, &tag, "DataView", ""));
                extra_keys = &[c"byteLength", c"byteOffset", c"buffer"];
            } else if js::Promise::is_promise(value.handle()) {
                braces.0 = format!("{}{{", get_prefix(constructor, &tag, "Promise", ""));
                formatter = Formatter::Promise;
            } else if is_weak_set(scope, value)? {
                braces.0 = format!("{}{{", get_prefix(constructor, &tag, "WeakSet", ""));
                formatter = Formatter::WeakCollection;
            } else if is_weak_map(scope, value)? {
                braces.0 = format!("{}{{", get_prefix(constructor, &tag, "WeakMap", ""));
                formatter = Formatter::WeakCollection;
            } else if let Some(kind) = boxed_kind_with(scope, &self.intrinsics, value)? {
                base = self.boxed_base(scope, value, kind, &mut keys, constructor, &tag)?;
                if keys.is_empty() {
                    return Ok(base);
                }
            } else {
                let style = self.ctx_style(constructor, &tag, value);
                if keys.is_empty() {
                    return Ok(format!("{style}{{}}"));
                }
                braces.0 = format!("{style}{{");
            }
        }

        if self.depth_exceeded(recurse) {
            let style = self.ctx_style(constructor, &tag, value);
            let name = &style[..style.len() - 1];
            return Ok(if constructor.is_some() {
                format!("[{name}]")
            } else {
                name.to_string()
            });
        }
        let recurse = recurse + 1;
        self.seen.push(RootedHeap::new(value));
        self.current_depth = recurse;
        let output = self.format_entries(
            scope,
            value,
            recurse,
            formatter,
            extra_keys,
            typed_array,
            &keys,
            extras,
        );
        self.seen.pop();
        let mut output = output?;
        if let Some(index) = self.circular.iter().position(|seen| *seen == value) {
            let reference = format!("<ref *{}>", index + 1);
            if self.options.compact != Compact::Bool(true) {
                base = if base.is_empty() {
                    reference
                } else {
                    format!("{reference} {base}")
                };
            } else {
                braces.0 = format!("{reference} {}", braces.0);
            }
        }
        if self.options.sorted {
            let by_units = |a: &String, b: &String| a.encode_utf16().cmp(b.encode_utf16());
            if extras == Extras::Object {
                output.sort_by(by_units);
            } else if keys.len() > 1 {
                let start = output.len() - keys.len();
                output[start..].sort_by(by_units);
            }
        }
        self.reduce_to_single_string(scope, output, &base, &braces, extras, recurse, Some(value))
    }

    /// The output of `formatRaw`'s formatter, extra keys and properties.
    #[allow(clippy::too_many_arguments)]
    fn format_entries(
        &mut self,
        scope: &Scope<'_>,
        value: Object<'_>,
        recurse: usize,
        formatter: Formatter,
        extra_keys: &[&'static CStr],
        typed_array: bool,
        keys: &[Key<'_>],
        extras: Extras,
    ) -> Result<Vec<String>, ExnThrown> {
        let mut output = match formatter {
            Formatter::None => Vec::new(),
            Formatter::Array => self.format_array(scope, value, recurse)?,
            Formatter::TypedArray => self.format_typed_array(scope, value)?,
            Formatter::Set | Formatter::Map => {
                let map = matches!(formatter, Formatter::Map);
                self.format_collection(scope, value, recurse, map)?
            }
            Formatter::ArrayBuffer => self.format_array_buffer(value),
            Formatter::Promise => self.format_promise(scope, value, recurse)?,
            Formatter::WeakCollection => vec!["<items unknown>".into()],
            Formatter::Iterator => Vec::new(),
        };
        for key in extra_keys {
            self.indentation += 2;
            let property = value.get_property(scope, key);
            let shown = property
                .and_then(|property| self.format_value(scope, property, recurse, typed_array));
            self.indentation -= 2;
            output.push(format!("[{}]: {}", key.to_string_lossy(), shown?));
        }
        for key in keys {
            output.push(self.format_property(scope, value, recurse, key, extras)?);
        }
        Ok(output)
    }

    /// Node.js's `getFunctionBase` and `getClassBase`.
    fn function_base(
        &mut self,
        scope: &Scope<'_>,
        function: Object<'_>,
        constructor: Option<&str>,
        tag: &str,
    ) -> Result<String, ExnThrown> {
        let source = function
            .cast::<js::Function>()
            .ok()
            .and_then(|f| {
                js::compile::decompile_function(
                    scope,
                    scope.root_function(std::ptr::NonNull::new(f.as_function_ptr())?),
                )
                .ok()
            })
            .and_then(|text| std::ptr::NonNull::new(text.get()))
            .map(|text| js::conversion::jsstr_to_string(scope, text))
            .unwrap_or_default();
        if is_class_source(&source) {
            let has_name = function.has_own_property(scope, c"name")?;
            let name = if has_name {
                function.get_property(scope, c"name")?
            } else {
                HandleValue::undefined()
            };
            let name = if bool::from_jsval(scope, name, ()).unwrap_or(false) {
                to_string(scope, name)?
            } else {
                "(anonymous)".into()
            };
            let mut base = format!("class {name}");
            if constructor != Some("Function") {
                if let Some(constructor) = constructor {
                    base.push_str(&format!(" [{constructor}]"));
                }
            }
            if !tag.is_empty() && constructor != Some(tag) {
                base.push_str(&format!(" [{tag}]"));
            }
            if constructor.is_some() {
                if let Some(parent) = function.get_prototype(scope)? {
                    let parent_name = parent.get_property(scope, c"name")?;
                    if bool::from_jsval(scope, parent_name, ()).unwrap_or(false) {
                        base.push_str(&format!(" extends {}", to_string(scope, parent_name)?));
                    }
                }
            } else {
                base.push_str(" extends [null prototype]");
            }
            return Ok(format!("[{base}]"));
        }
        let mut kind = "Function".to_string();
        if has_builtin_tag_with(
            scope,
            &self.intrinsics,
            function,
            &["GeneratorFunction", "AsyncGeneratorFunction"],
        )? {
            kind = format!("Generator{kind}");
        }
        if has_builtin_tag_with(
            scope,
            &self.intrinsics,
            function,
            &["AsyncFunction", "AsyncGeneratorFunction"],
        )? {
            kind = format!("Async{kind}");
        }
        let mut base = format!("[{kind}");
        if constructor.is_none() {
            base.push_str(" (null prototype)");
        }
        let name = function.get_property(scope, c"name")?;
        if name.is_string() && to_string(scope, name)?.is_empty() {
            base.push_str(" (anonymous)");
        } else if name.is_string() {
            base.push_str(&format!(": {}", to_string(scope, name)?));
        } else {
            base.push_str(&format!(": {}", self.format_value(scope, name, 0, false)?));
        }
        base.push(']');
        if let Some(constructor) = constructor.filter(|constructor| *constructor != kind) {
            base.push_str(&format!(" {constructor}"));
        }
        if !tag.is_empty() && constructor != Some(tag) {
            base.push_str(&format!(" [{tag}]"));
        }
        Ok(base)
    }

    /// Node.js's `getBoxedBase`.
    fn boxed_base(
        &mut self,
        scope: &Scope<'_>,
        value: Object<'_>,
        kind: Boxed,
        keys: &mut Vec<Key<'_>>,
        constructor: Option<&str>,
        tag: &str,
    ) -> Result<String, ExnThrown> {
        let primitive = unbox(scope, value, kind)?.unwrap_or(HandleValue::undefined());
        if kind == Boxed::String {
            let length = string_units(scope, primitive)?.len();
            keys.drain(..length.min(keys.len()));
        }
        let name = kind.name();
        let mut base = format!("[{name}");
        if Some(name) != constructor {
            match constructor {
                None => base.push_str(" (null prototype)"),
                Some(constructor) => base.push_str(&format!(" ({constructor})")),
            }
        }
        base.push_str(&format!(": {}]", self.format_primitive(scope, primitive)?));
        if !tag.is_empty() && Some(tag) != constructor {
            base.push_str(&format!(" [{tag}]"));
        }
        Ok(base)
    }

    /// Node.js's `formatError`, for the stack SpiderMonkey writes as well as one in V8's form.
    fn format_error<'r>(
        &mut self,
        scope: &'r Scope<'_>,
        error: Object<'_>,
        constructor: Option<&str>,
        tag: &str,
        keys: &mut Vec<Key<'r>>,
    ) -> Result<String, ExnThrown> {
        let mut stack = match self.stack_string(scope, error) {
            Ok(stack) => stack,
            Err(ExnThrown) => {
                js::exception::clear(scope);
                return self.intrinsics.object_to_string(scope, error);
            }
        };
        let message = error.get_property(scope, c"message");
        let message = match message {
            Ok(message) => Some(message),
            Err(ExnThrown) => {
                js::exception::clear(scope);
                None
            }
        };
        let name = match error.get_property(scope, c"name") {
            Ok(name) => Some(name),
            Err(ExnThrown) => {
                js::exception::clear(scope);
                None
            }
        };
        let key_is = |scope: &Scope<'_>, key: &Key<'_>, name: &str| -> Result<bool, ExnThrown> {
            Ok(key_text(scope, key.key)?.as_deref() == Some(name))
        };
        if self.options.show_hidden {
            // We drop SpiderMonkey's own `fileName`, `lineNumber` and `columnNumber`, which V8
            // errors lack.
            let mut kept = Vec::with_capacity(keys.len());
            for key in keys.drain(..) {
                let text = key_text(scope, key.key)?;
                if !matches!(
                    text.as_deref(),
                    Some("fileName" | "lineNumber" | "columnNumber")
                ) {
                    kept.push(key);
                }
            }
            *keys = kept;
        } else if !keys.is_empty() {
            if let Some(index) = position(scope, keys, |scope, key| key_is(scope, key, "stack"))? {
                keys.remove(index);
            }
            for (field, value) in [("message", message), ("name", name)] {
                let Some(value) = value else {
                    continue;
                };
                if let Some(index) = position(scope, keys, |scope, key| key_is(scope, key, field))?
                {
                    if !value.is_string() || stack.contains(&to_string(scope, value)?) {
                        keys.remove(index);
                    }
                }
            }
        }
        let name_text = match name {
            Some(name) if !name.is_undefined() && !name.is_null() => Some(name),
            _ => None,
        };
        for field in ["cause", "errors"] {
            let field_c = if field == "cause" {
                c"cause"
            } else {
                c"errors"
            };
            if !error.has_own_property(scope, field_c)?
                || position(scope, keys, |scope, key| key_is(scope, key, field))?.is_some()
            {
                continue;
            }
            if field == "errors" {
                match error.get_property(scope, c"errors") {
                    Ok(errors) => {
                        let is_array = Object::from_value(scope, *errors)
                            .ok()
                            .map(|errors| js::Array::is_array(scope, errors.handle()))
                            .transpose()?
                            .unwrap_or(false);
                        if !is_array {
                            continue;
                        }
                    }
                    Err(ExnThrown) => {
                        js::exception::clear(scope);
                        continue;
                    }
                }
            }
            if let Some(descriptor) = self.intrinsics.named_descriptor(scope, error, field)? {
                let key = scope.root_value(js::JSString::from_str(scope, field)?.as_value());
                keys.push(Key { key, descriptor });
            }
        }

        stack = self.improve_stack(scope, stack, constructor, name_text, tag)?;

        let message_text = match message {
            Some(message) if bool::from_jsval(scope, message, ()).unwrap_or(false) => {
                Some(to_string(scope, message)?)
            }
            _ => None,
        };
        let position = message_text
            .as_deref()
            .and_then(|message| {
                let index = stack.find(message)?;
                (index != 0).then_some(index + message.len())
            })
            .unwrap_or_default();
        match stack[position.min(stack.len())..].find("\n    at") {
            None => stack = format!("[{stack}]"),
            Some(offset) => {
                let start = position + offset;
                let head = stack[..start].to_string();
                let frames: Vec<String> =
                    stack[start + 1..].split('\n').map(String::from).collect();
                let frames = self.stack_frames(scope, error, frames)?;
                stack = format!("{head}\n{}", frames.join("\n"));
            }
        }
        if self.indentation != 0 {
            stack = stack.replace('\n', &format!("\n{}", " ".repeat(self.indentation)));
        }
        Ok(stack)
    }

    /// Node.js's `getStackString`, with a stack in SpiderMonkey's form rewritten to V8's.
    fn stack_string(&mut self, scope: &Scope<'_>, error: Object<'_>) -> Result<String, ExnThrown> {
        let stack = match error.get_property(scope, c"stack") {
            Ok(stack) => stack,
            Err(ExnThrown) => {
                js::exception::clear(scope);
                HandleValue::undefined()
            }
        };
        if bool::from_jsval(scope, stack, ()).unwrap_or(false) {
            if stack.is_string() {
                let stack = to_string(scope, stack)?;
                return match spidermonkey_frames(&stack) {
                    Some(frames) => Ok(format!("{}{}", error_to_string(scope, error)?, frames)),
                    None => Ok(stack),
                };
            }
            self.seen.push(RootedHeap::new(error));
            self.indentation += 4;
            let shown = self.format_value(scope, stack, 0, false);
            self.indentation -= 4;
            self.seen.pop();
            return Ok(format!(
                "{}\n    {}",
                error_to_string(scope, error)?,
                shown?
            ));
        }
        error_to_string(scope, error)
    }

    /// Node.js's `getStackFrames`, without the folding of repeated frames: the frames shared with
    /// the stack of the error's `cause` are replaced by a note.
    fn stack_frames(
        &mut self,
        scope: &Scope<'_>,
        error: Object<'_>,
        mut frames: Vec<String>,
    ) -> Result<Vec<String>, ExnThrown> {
        let cause = match error.get_property(scope, c"cause") {
            Ok(cause) => cause,
            Err(ExnThrown) => {
                js::exception::clear(scope);
                HandleValue::undefined()
            }
        };
        let Ok(cause) = Object::from_value(scope, *cause) else {
            return Ok(frames);
        };
        if !is_error(scope, cause)? {
            return Ok(frames);
        }
        let cause_stack = self.stack_string(scope, cause)?;
        if let Some(start) = cause_stack.find("\n    at") {
            let cause_frames: Vec<&str> = cause_stack[start + 1..].split('\n').collect();
            let (length, offset) = identical_sequence_range(&frames, &cause_frames);
            if length > 0 {
                let skipped = length - 2;
                let note = format!("    ... {skipped} lines matching cause stack trace ...");
                frames.splice(offset + 1..offset + 1 + skipped, [note]);
            }
        }
        Ok(frames)
    }

    /// Node.js's `improveStack`: the constructor name and tag in the first line of the stack of an
    /// error whose class doesn't set `name`.
    fn improve_stack(
        &mut self,
        scope: &Scope<'_>,
        mut stack: String,
        constructor: Option<&str>,
        name: Option<HandleValue<'_>>,
        tag: &str,
    ) -> Result<String, ExnThrown> {
        let (name, mut length) = match name {
            Some(name) if name.is_string() => {
                let text = to_string(scope, name)?;
                let length = js_len(&text);
                (text, length)
            }
            Some(name) => {
                let text = to_string(scope, name)?;
                let prefix = get_prefix(constructor, tag, "Error", "");
                stack = stack.replacen(
                    &text,
                    &format!("{text} [{}]", &prefix[..prefix.len() - 1]),
                    1,
                );
                (text, 0)
            }
            None => ("Error".to_string(), 5),
        };
        let starts_with_name = name.ends_with("Error")
            && stack.starts_with(&name)
            && matches!(stack[name.len()..].chars().next(), None | Some(':' | '\n'));
        if constructor.is_none() || starts_with_name {
            let mut fallback = "Error".to_string();
            if constructor.is_none() {
                let found = null_prototype_error_name(&stack);
                length = js_len(&found);
                fallback = if found.is_empty() {
                    "Error".into()
                } else {
                    found
                };
            }
            let prefix = get_prefix(constructor, tag, &fallback, "");
            let prefix = &prefix[..prefix.len() - 1];
            if name != prefix {
                let rest: Vec<u16> = stack.encode_utf16().skip(length).collect();
                let rest = String::from_utf16_lossy(&rest);
                stack = if prefix.contains(&name) {
                    if length == 0 {
                        format!("{prefix}: {stack}")
                    } else {
                        format!("{prefix}{rest}")
                    }
                } else {
                    format!("{prefix} [{name}]{rest}")
                };
            }
        }
        Ok(stack)
    }

    /// Node.js's `formatProperty`.
    fn format_property(
        &mut self,
        scope: &Scope<'_>,
        object: Object<'_>,
        recurse: usize,
        key: &Key<'_>,
        kind: Extras,
    ) -> Result<String, ExnThrown> {
        let descriptor = &key.descriptor;
        let mut extra = " ".to_string();
        let shown = if !descriptor.value.is_undefined() {
            let diff = if self.options.compact != Compact::Bool(true) || kind != Extras::Object {
                2
            } else {
                3
            };
            self.indentation += diff;
            let shown = self.format_value(scope, descriptor.value, recurse, false);
            if let Ok(shown) = &shown {
                if diff == 3 && self.options.break_length < string_width(shown) as f64 {
                    extra = format!("\n{}", " ".repeat(self.indentation));
                }
            }
            self.indentation -= diff;
            shown?
        } else if !descriptor.get.is_undefined() {
            let label = if descriptor.set.is_undefined() {
                "Getter"
            } else {
                "Getter/Setter"
            };
            let call = match self.options.getters {
                Getters::None => false,
                Getters::All => true,
                Getters::Get => descriptor.set.is_undefined(),
                Getters::Set => !descriptor.set.is_undefined(),
            };
            if call {
                self.indentation += 2;
                let shown = self.format_getter(scope, object, descriptor.get, label, recurse);
                self.indentation -= 2;
                shown?
            } else {
                format!("[{label}]")
            }
        } else if !descriptor.set.is_undefined() {
            "[Setter]".into()
        } else {
            "undefined".into()
        };
        if kind == Extras::Array {
            return Ok(shown);
        }
        let mut name = if key.key.is_symbol() {
            let text = symbol_to_string(scope, key.key);
            escape_units(&text.encode_utf16().collect::<Vec<_>>(), true)
        } else {
            let text = to_string(scope, key.key)?;
            if is_key_identifier(&text) {
                if text == "__proto__" {
                    "['__proto__']".into()
                } else {
                    text
                }
            } else {
                str_escape(&string_units(scope, key.key)?)
            }
        };
        if !descriptor.enumerable {
            name = format!("[{name}]");
        }
        Ok(format!("{name}:{extra}{shown}"))
    }

    /// The value of a getter as `formatProperty` shows it with the `getters` option.
    fn format_getter(
        &mut self,
        scope: &Scope<'_>,
        object: Object<'_>,
        getter: HandleValue<'_>,
        label: &str,
        recurse: usize,
    ) -> Result<String, ExnThrown> {
        match Function::call(scope, object, getter, EmptyArgs) {
            Ok(value) if value.is_null() => Ok(format!("[{label}: null]")),
            Ok(value) if value.is_object() => Ok(format!(
                "[{label}] {}",
                self.format_value(scope, value, recurse, false)?
            )),
            Ok(value) => Ok(format!(
                "[{label}: {}]",
                self.format_primitive(scope, value)?
            )),
            Err(ExnThrown) => {
                let error = js::exception::take_pending(scope).ok_or(ExnThrown)?;
                let error = scope.root_value(error.get());
                let shown = self.format_value(scope, error, recurse, false)?;
                Ok(format!("[{label}: <Inspection threw ({shown})>]"))
            }
        }
    }

    /// Node.js's `formatArray` and `formatSpecialArray`.
    fn format_array(
        &mut self,
        scope: &Scope<'_>,
        array: Object<'_>,
        recurse: usize,
    ) -> Result<Vec<String>, ExnThrown> {
        let length = array_length(scope, array)? as usize;
        let max = shown_entries(self.options.max_array_length, length);
        let mut output = Vec::new();
        for index in 0..max {
            let key = scope.root_value(js::value::from_f64(index as f64));
            let Some(descriptor) = self.intrinsics.descriptor(scope, array, key)? else {
                return self.format_special_array(scope, array, recurse, max, output, index);
            };
            let key = Key { key, descriptor };
            output.push(self.format_property(scope, array, recurse, &key, Extras::Array)?);
        }
        if length > max {
            output.push(remaining_text((length - max) as f64));
        }
        Ok(output)
    }

    fn format_special_array(
        &mut self,
        scope: &Scope<'_>,
        array: Object<'_>,
        recurse: usize,
        max: usize,
        mut output: Vec<String>,
        start: usize,
    ) -> Result<Vec<String>, ExnThrown> {
        let mut keys = Vec::new();
        for key in array.own_keys(scope)? {
            let Some(descriptor) = self.intrinsics.descriptor(scope, array, key)? else {
                continue;
            };
            if descriptor.enumerable && !key.is_symbol() {
                keys.push(Key { key, descriptor });
            }
        }
        let mut index = start;
        let mut position = start;
        while position < keys.len() && output.len() < max {
            let key = &keys[position];
            let text = to_string(scope, key.key)?;
            let number: f64 = text.parse().unwrap_or(f64::NAN);
            if number > 4294967294.0 {
                break;
            }
            if index.to_string() != text {
                if !is_canonical_number(&text) {
                    break;
                }
                let empty = number as usize - index;
                output.push(format!(
                    "<{empty} empty item{}>",
                    if empty > 1 { "s" } else { "" }
                ));
                index = number as usize;
                if output.len() == max {
                    break;
                }
            }
            output.push(self.format_property(scope, array, recurse, key, Extras::Array)?);
            index += 1;
            position += 1;
        }
        let length = array_length(scope, array)? as usize;
        let remaining = length.saturating_sub(index);
        if output.len() != max {
            if remaining > 0 {
                output.push(format!(
                    "<{remaining} empty item{}>",
                    if remaining > 1 { "s" } else { "" }
                ));
            }
        } else if remaining > 0 {
            output.push(remaining_text(remaining as f64));
        }
        Ok(output)
    }

    /// Node.js's `formatTypedArray`.
    fn format_typed_array(
        &mut self,
        scope: &Scope<'_>,
        array: Object<'_>,
    ) -> Result<Vec<String>, ExnThrown> {
        let length = view_of(array).map(|view| view.array_length()).unwrap_or(0);
        let max = shown_entries(self.options.max_array_length, length);
        let mut output = Vec::with_capacity(max + 1);
        for index in 0..max {
            let element = array.get_element(scope, index as u32)?;
            output.push(self.format_primitive(scope, element)?);
        }
        if length > max {
            output.push(remaining_text((length - max) as f64));
        }
        Ok(output)
    }

    /// Node.js's `formatSet` and `formatMap`.
    fn format_collection(
        &mut self,
        scope: &Scope<'_>,
        object: Object<'_>,
        recurse: usize,
        map: bool,
    ) -> Result<Vec<String>, ExnThrown> {
        let entries = collection_entries(scope, object, map)?;
        let max = shown_entries(self.options.max_array_length, entries.len());
        let mut output = Vec::with_capacity(max + 1);
        self.indentation += 2;
        let result = (|| -> Result<(), ExnThrown> {
            for (key, value) in entries.iter().take(max) {
                if map {
                    let key = self.format_value(scope, *key, recurse, false)?;
                    let value = self.format_value(scope, *value, recurse, false)?;
                    output.push(format!("{key} => {value}"));
                } else {
                    output.push(self.format_value(scope, *value, recurse, false)?);
                }
            }
            Ok(())
        })();
        self.indentation -= 2;
        result?;
        if entries.len() > max {
            output.push(remaining_text((entries.len() - max) as f64));
        }
        Ok(output)
    }

    /// Node.js's `formatArrayBuffer`.
    fn format_array_buffer(&self, buffer: Object<'_>) -> Vec<String> {
        let max_array_length = self.options.max_array_length;
        let (mut text, length) = js::typedarray::with_buffer_source_bytes(buffer, |bytes| {
            let max = shown_entries(max_array_length, bytes.len());
            let text: Vec<String> = bytes[..max].iter().map(|b| format!("{b:02x}")).collect();
            (text.join(" "), bytes.len())
        })
        .unwrap_or_default();
        let remaining = length as f64 - self.options.max_array_length;
        if remaining > 0.0 {
            text.push_str(&format!(
                " ... {} more byte{}",
                number_to_string(remaining),
                if remaining > 1.0 { "s" } else { "" }
            ));
        }
        vec![format!("[Uint8Contents]: <{text}>")]
    }

    /// Node.js's `formatPromise`.
    fn format_promise(
        &mut self,
        scope: &Scope<'_>,
        promise: Object<'_>,
        recurse: usize,
    ) -> Result<Vec<String>, ExnThrown> {
        let promise = promise
            .cast::<js::Promise>()
            .map_err(|_| js::error::TypeError("not a promise".into()).throw(scope))?;
        if promise.is_pending() {
            return Ok(vec!["<pending>".into()]);
        }
        let result = promise.result(scope).unwrap_or(HandleValue::undefined());
        self.indentation += 2;
        let shown = self.format_value(scope, result, recurse, false);
        self.indentation -= 2;
        let shown = shown?;
        Ok(vec![if promise.is_rejected() {
            format!("<rejected> {shown}")
        } else {
            shown
        }])
    }

    /// Node.js's `isBelowBreakLength`.
    fn is_below_break_length(&self, output: &[String], start: usize, base: &str) -> bool {
        let break_length = self.options.break_length;
        if break_length == f64::INFINITY {
            return !base.contains('\n') && !output.iter().any(|entry| entry.contains('\n'));
        }
        let mut total = (output.len() + start) as f64;
        if total + output.len() as f64 > break_length {
            return false;
        }
        for entry in output {
            total += js_len(entry) as f64;
            if total > break_length {
                return false;
            }
        }
        base.is_empty() || !base.contains('\n')
    }

    /// Node.js's `reduceToSingleString`: the entries on one line if they fit, and on separate
    /// lines otherwise.
    #[allow(clippy::too_many_arguments)]
    fn reduce_to_single_string(
        &self,
        scope: &Scope<'_>,
        mut output: Vec<String>,
        base: &str,
        braces: &(String, String),
        extras: Extras,
        recurse: usize,
        value: Option<Object<'_>>,
    ) -> Result<String, ExnThrown> {
        let base_prefix = if base.is_empty() {
            String::new()
        } else {
            format!("{base} ")
        };
        if self.options.compact != Compact::Bool(true) {
            if let Compact::Number(compact) = self.options.compact {
                if compact >= 1.0 {
                    let entries = output.len();
                    if extras == Extras::ArrayAndKeys && entries > 6 {
                        output = self.group_array_elements(scope, output, value)?;
                    }
                    if ((self.current_depth - recurse) as f64) < compact && entries == output.len()
                    {
                        let start =
                            output.len() + self.indentation + js_len(&braces.0) + js_len(base) + 10;
                        if self.is_below_break_length(&output, start, base) {
                            let joined = output.join(", ");
                            if !joined.contains('\n') {
                                return Ok(format!(
                                    "{base_prefix}{} {joined} {}",
                                    braces.0, braces.1
                                ));
                            }
                        }
                    }
                }
            }
            let indentation = format!("\n{}", " ".repeat(self.indentation));
            return Ok(format!(
                "{base_prefix}{}{indentation}  {}{indentation}{}",
                braces.0,
                output.join(&format!(",{indentation}  ")),
                braces.1
            ));
        }
        if self.is_below_break_length(&output, 0, base) {
            let base = if base.is_empty() {
                String::new()
            } else {
                format!(" {base}")
            };
            return Ok(format!(
                "{}{base} {} {}",
                braces.0,
                output.join(", "),
                braces.1
            ));
        }
        let indentation = " ".repeat(self.indentation);
        let line = if base.is_empty() && js_len(&braces.0) == 1 {
            " ".to_string()
        } else {
            let base = if base.is_empty() {
                String::new()
            } else {
                format!(" {base}")
            };
            format!("{base}\n{indentation}  ")
        };
        Ok(format!(
            "{}{line}{} {}",
            braces.0,
            output.join(&format!(",\n{indentation}  ")),
            braces.1
        ))
    }

    /// Node.js's `groupArrayElements`: the entries of a long array in aligned columns.
    fn group_array_elements(
        &self,
        scope: &Scope<'_>,
        output: Vec<String>,
        value: Option<Object<'_>>,
    ) -> Result<Vec<String>, ExnThrown> {
        let mut total_length = 0usize;
        let mut max_length = 0usize;
        let mut output_length = output.len();
        if self.options.max_array_length < output.len() as f64 {
            output_length -= 1;
        }
        let separator_space = 2;
        let mut data_len = Vec::with_capacity(output_length);
        for entry in &output[..output_length] {
            let length = string_width(entry);
            data_len.push(length);
            total_length += length + separator_space;
            max_length = max_length.max(length);
        }
        let actual_max = max_length + separator_space;
        let break_length = self.options.break_length;
        if ((actual_max * 3 + self.indentation) as f64) < break_length
            && (total_length as f64 / actual_max as f64 > 5.0 || max_length <= 6)
        {
            let approx_char_heights = 2.5;
            let average_bias =
                (actual_max as f64 - total_length as f64 / output.len() as f64).sqrt();
            let biased_max = (actual_max as f64 - 3.0 - average_bias).max(1.0);
            let compact = match self.options.compact {
                Compact::Number(compact) => compact,
                Compact::Bool(_) => f64::INFINITY,
            };
            let columns = ((approx_char_heights * biased_max * output_length as f64).sqrt()
                / biased_max)
                .round()
                .min(((break_length - self.indentation as f64) / actual_max as f64).floor())
                .min(compact * 4.0)
                .min(15.0);
            if columns <= 1.0 {
                return Ok(output);
            }
            let columns = columns as usize;
            let mut max_line_length = Vec::with_capacity(columns);
            for column in 0..columns {
                let mut line_max = 0;
                let mut index = column;
                while index < output_length {
                    line_max = line_max.max(data_len[index]);
                    index += columns;
                }
                max_line_length.push(line_max + separator_space);
            }
            let mut pad_start = true;
            if let Some(value) = value {
                for index in 0..output.len() {
                    let element = value.get_element(scope, index as u32)?;
                    if !element.is_number() && !element.is_bigint() {
                        pad_start = false;
                        break;
                    }
                }
            }
            let pad = |text: String, width: usize, start: bool| -> String {
                let length = js_len(&text);
                if length >= width {
                    return text;
                }
                let fill = " ".repeat(width - length);
                if start {
                    format!("{fill}{text}")
                } else {
                    format!("{text}{fill}")
                }
            };
            let mut grouped = Vec::new();
            let mut index = 0;
            while index < output_length {
                let max = (index + columns).min(output_length);
                let mut line = String::new();
                let mut position = index;
                while position < max - 1 {
                    let padding = max_line_length[position - index] + js_len(&output[position])
                        - data_len[position];
                    line.push_str(&pad(format!("{}, ", output[position]), padding, pad_start));
                    position += 1;
                }
                if pad_start {
                    let padding = max_line_length[position - index] + js_len(&output[position])
                        - data_len[position]
                        - separator_space;
                    line.push_str(&pad(output[position].clone(), padding, true));
                } else {
                    line.push_str(&output[position]);
                }
                grouped.push(line);
                index += columns;
            }
            if self.options.max_array_length < output.len() as f64 {
                grouped.push(output[output_length].clone());
            }
            return Ok(grouped);
        }
        Ok(output)
    }
}

/// The index of the first of `keys` that `matches`.
fn position(
    scope: &Scope<'_>,
    keys: &[Key<'_>],
    matches: impl Fn(&Scope<'_>, &Key<'_>) -> Result<bool, ExnThrown>,
) -> Result<Option<usize>, ExnThrown> {
    for (index, key) in keys.iter().enumerate() {
        if matches(scope, key)? {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

/// Whether `key` matches Node.js's `keyStrRegExp`, `^[a-zA-Z_][a-zA-Z_0-9]*$`.
fn is_key_identifier(key: &str) -> bool {
    let mut bytes = key.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Whether `key` matches Node.js's `numberRegExp`, `^(0|[1-9][0-9]*)$`.
fn is_canonical_number(key: &str) -> bool {
    key == "0"
        || (key.starts_with(|c: char| ('1'..='9').contains(&c))
            && key.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether the source text of a function is a class, by the test in Node.js's `getFunctionBase`.
fn is_class_source(source: &str) -> bool {
    if !source.starts_with("class") || !source.ends_with('}') {
        return false;
    }
    let slice = &source[5..source.len() - 1];
    let Some(bracket) = slice.find('{') else {
        return false;
    };
    if !slice[..bracket].contains('(') {
        return true;
    }
    let stripped = strip_comments(slice);
    match stripped.find('{') {
        Some(bracket) => {
            let head = &stripped[..bracket];
            head.starts_with(char::is_whitespace) && !head.contains('(')
        }
        None => false,
    }
}

/// `text` without `//` and `/* */` comments, as Node.js's `stripCommentsRegExp` removes them.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let line = rest.find("//");
        let block = rest.find("/*");
        match (line, block) {
            (Some(l), b) if b.is_none_or(|b| l < b) => match rest[l..].find('\n') {
                Some(end) => {
                    out.push_str(&rest[..l]);
                    rest = &rest[l + end + 1..];
                }
                None => {
                    out.push_str(rest);
                    return out;
                }
            },
            (_, Some(b)) => match rest[b + 2..].find("*/") {
                Some(end) => {
                    out.push_str(&rest[..b]);
                    rest = &rest[b + 2 + end + 2..];
                }
                None => {
                    out.push_str(rest);
                    return out;
                }
            },
            _ => {
                out.push_str(rest);
                return out;
            }
        }
    }
}

/// The frames of a stack in SpiderMonkey's `function@location` form, as V8 writes them after the
/// first line, or `None` if `stack` isn't in that form.
fn spidermonkey_frames(stack: &str) -> Option<String> {
    let lines: Vec<&str> = stack.lines().filter(|line| !line.is_empty()).collect();
    if lines.is_empty()
        || stack.contains("\n    at ")
        || !lines.iter().all(|line| line.contains('@'))
    {
        return None;
    }
    let mut frames = String::new();
    for line in lines {
        let (function, location) =
            crate::stack_trace::split_frame(line).expect("every line holds an `@`");
        if function.is_empty() {
            frames.push_str(&format!("\n    at {location}"));
        } else {
            frames.push_str(&format!("\n    at {function} ({location})"));
        }
    }
    Some(frames)
}

/// `Error.prototype.toString` applied to `error`.
fn error_to_string(scope: &Scope<'_>, error: Object<'_>) -> Result<String, ExnThrown> {
    let name = error.get_property(scope, c"name")?;
    let name = if name.is_undefined() {
        "Error".to_string()
    } else {
        to_string(scope, name)?
    };
    let message = error.get_property(scope, c"message")?;
    let message = if message.is_undefined() {
        String::new()
    } else {
        to_string(scope, message)?
    };
    Ok(if name.is_empty() {
        message
    } else if message.is_empty() {
        name
    } else {
        format!("{name}: {message}")
    })
}

/// The error name at the start of `stack`, as `improveStack` finds it for an error with a null
/// prototype: `/^([A-Z][a-z_ A-Z0-9[\]()-]+)(?::|\n {4}at)/` or `/^([a-z_A-Z0-9-]*Error)$/`.
fn null_prototype_error_name(stack: &str) -> String {
    let allowed = |c: char| c.is_ascii_alphanumeric() || " _[]()-".contains(c);
    if stack.starts_with(|c: char| c.is_ascii_uppercase()) {
        let mut end = 1;
        for (index, c) in stack.char_indices().skip(1) {
            if !allowed(c) {
                break;
            }
            end = index + c.len_utf8();
        }
        // The pattern backtracks to the longest prefix followed by `:` or a frame.
        let mut candidate = end;
        while candidate > 1 {
            let rest = &stack[candidate..];
            if rest.starts_with(':') || rest.starts_with("\n    at") {
                return stack[..candidate].to_string();
            }
            candidate -= 1;
            while !stack.is_char_boundary(candidate) {
                candidate -= 1;
            }
        }
    }
    if stack.ends_with("Error")
        && stack
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return stack.to_string();
    }
    String::new()
}

/// Node.js's `identicalSequenceRange`: the length and start in `a` of the first run of more than
/// three entries of `a` that also appear in sequence in `b`.
fn identical_sequence_range(a: &[String], b: &[&str]) -> (usize, usize) {
    for i in 0..a.len().saturating_sub(3) {
        if let Some(pos) = b.iter().position(|line| *line == a[i]) {
            let rest = b.len() - pos;
            if rest > 3 {
                let mut length = 1;
                let max_length = (a.len() - i).min(rest);
                while max_length > length && a[i + length] == b[pos + length] {
                    length += 1;
                }
                if length > 3 {
                    return (length, i);
                }
            }
        }
    }
    (0, 0)
}

/// `Symbol.prototype.toString` applied to the symbol `value`.
pub(crate) fn symbol_to_string(scope: &Scope<'_>, value: HandleValue<'_>) -> String {
    let Some(symbol) = std::ptr::NonNull::new(value.to_symbol()) else {
        return "Symbol()".into();
    };
    match js::symbol::get_description(scope.root_symbol(symbol)) {
        Some(description) => format!(
            "Symbol({})",
            js::conversion::jsstr_to_string(scope, description)
        ),
        None => "Symbol()".into(),
    }
}

/// `object instanceof ctor` by `OrdinaryHasInstance`, with `false` for an exception.
pub(crate) fn instance_of(
    scope: &Scope<'_>,
    object: Object<'_>,
    ctor: Object<'_>,
) -> Result<bool, ExnThrown> {
    let proto = match ctor.get_property(scope, c"prototype") {
        Ok(proto) => proto,
        Err(ExnThrown) => {
            js::exception::clear(scope);
            return Ok(false);
        }
    };
    let Ok(proto) = Object::from_value(scope, *proto) else {
        return Ok(false);
    };
    let mut current = match object.get_prototype(scope) {
        Ok(current) => current,
        Err(ExnThrown) => {
            js::exception::clear(scope);
            return Ok(false);
        }
    };
    while let Some(candidate) = current {
        if candidate.as_raw() == proto.as_raw() {
            return Ok(true);
        }
        current = match candidate.get_prototype(scope) {
            Ok(next) => next,
            Err(ExnThrown) => {
                js::exception::clear(scope);
                return Ok(false);
            }
        };
    }
    Ok(false)
}

/// `Buffer.prototype[inspect.custom]()` for the buffer `object`: `<Buffer` and its first
/// `INSPECT_MAX_BYTES` bytes in hex, and the properties that aren't indices.
pub(crate) fn inspect_buffer(scope: &Scope<'_>, object: Object<'_>) -> Result<String, ExnThrown> {
    let mut inspector = Inspector::new(scope, InspectOptions::current(scope)?)?;
    inspector.format_buffer(scope, object)
}

/// `util.inspect(value, options)`.
pub fn inspect(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    options: InspectOptions,
) -> Result<String, ExnThrown> {
    let mut inspector = Inspector::new(scope, options)?;
    inspector.format_value(scope, value, 0, false)
}

// ---------------------------------------------------------------------------
// util.format
// ---------------------------------------------------------------------------

/// The names of the global constructors Node.js's `builtInObjects` holds.
const BUILTIN_OBJECTS: &[&str] = &[
    "Object",
    "Function",
    "Array",
    "Number",
    "Boolean",
    "String",
    "Symbol",
    "Date",
    "Promise",
    "RegExp",
    "Error",
    "AggregateError",
    "EvalError",
    "RangeError",
    "ReferenceError",
    "SyntaxError",
    "TypeError",
    "URIError",
    "JSON",
    "Math",
    "Intl",
    "ArrayBuffer",
    "Atomics",
    "Uint8Array",
    "Int8Array",
    "Uint16Array",
    "Int16Array",
    "Uint32Array",
    "Int32Array",
    "Float16Array",
    "Float32Array",
    "Float64Array",
    "Uint8ClampedArray",
    "BigUint64Array",
    "BigInt64Array",
    "DataView",
    "Map",
    "BigInt",
    "Set",
    "WeakMap",
    "WeakSet",
    "Proxy",
    "Reflect",
    "FinalizationRegistry",
    "WeakRef",
    "SharedArrayBuffer",
    "WebAssembly",
    "Iterator",
];

/// Node.js's `hasBuiltInToString`: whether `object` converts to a string with a builtin
/// `toString` or `Symbol.toPrimitive`, rather than one its class or itself defines.
fn has_builtin_to_string(
    scope: &Scope<'_>,
    intrinsics: &Intrinsics<'_>,
    object: Object<'_>,
) -> Result<bool, ExnThrown> {
    let callable = |value: HandleValue<'_>| {
        Object::from_value(scope, *value).is_ok_and(|value| value.is_callable())
    };
    let to_primitive = well_known(scope, SymbolCode::toPrimitive);
    let mut check_to_string = true;
    let mut check_to_primitive = true;
    if !callable(object.get_property(scope, c"toString")?) {
        if !callable(object.get_property_by_id(scope, to_primitive)?) {
            return Ok(true);
        } else if object.has_own_property_by_id(scope, to_primitive)? {
            return Ok(false);
        }
        check_to_string = false;
    } else if object.has_own_property(scope, c"toString")? {
        return Ok(false);
    } else if !callable(object.get_property_by_id(scope, to_primitive)?) {
        check_to_primitive = false;
    } else if object.has_own_property_by_id(scope, to_primitive)? {
        return Ok(false);
    }
    let mut pointer = object;
    loop {
        let Some(next) = pointer.get_prototype(scope)? else {
            return Ok(false);
        };
        pointer = next;
        if (check_to_string && pointer.has_own_property(scope, c"toString")?)
            || (check_to_primitive && pointer.has_own_property_by_id(scope, to_primitive)?)
        {
            break;
        }
    }
    let Some(descriptor) = intrinsics.named_descriptor(scope, pointer, "constructor")? else {
        return Ok(false);
    };
    let Ok(ctor) = Object::from_value(scope, *descriptor.value) else {
        return Ok(false);
    };
    if !ctor.is_callable() {
        return Ok(false);
    }
    let name = ctor.get_property(scope, c"name")?;
    Ok(name.is_string() && BUILTIN_OBJECTS.contains(&to_string(scope, name)?.as_str()))
}

/// Whether `c` is white space or a line terminator, as JS's `StrWhiteSpaceChar` defines them.
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// JS's `parseFloat` of `text`.
fn parse_float(text: &str) -> f64 {
    let text = text.trim_start_matches(is_js_space);
    let (sign, rest) = match text.as_bytes().first() {
        Some(b'-') => (-1.0, &text[1..]),
        Some(b'+') => (1.0, &text[1..]),
        _ => (1.0, text),
    };
    if rest.starts_with("Infinity") {
        return sign * f64::INFINITY;
    }
    let bytes = rest.as_bytes();
    let mut end = 0;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    let integer_digits = end;
    let mut fraction_digits = 0;
    if end < bytes.len() && bytes[end] == b'.' {
        let mut next = end + 1;
        while next < bytes.len() && bytes[next].is_ascii_digit() {
            next += 1;
        }
        fraction_digits = next - end - 1;
        if integer_digits > 0 || fraction_digits > 0 {
            end = next;
        }
    }
    if integer_digits == 0 && fraction_digits == 0 {
        return f64::NAN;
    }
    if end < bytes.len() && (bytes[end] == b'e' || bytes[end] == b'E') {
        let mut next = end + 1;
        if next < bytes.len() && (bytes[next] == b'+' || bytes[next] == b'-') {
            next += 1;
        }
        let digits_start = next;
        while next < bytes.len() && bytes[next].is_ascii_digit() {
            next += 1;
        }
        if next > digits_start {
            end = next;
        }
    }
    sign * rest[..end].parse::<f64>().unwrap_or(f64::NAN)
}

/// JS's `parseInt` of `text` without a radix.
fn parse_int(text: &str) -> f64 {
    let text = text.trim_start_matches(is_js_space);
    let (sign, mut rest) = match text.as_bytes().first() {
        Some(b'-') => (-1.0, &text[1..]),
        Some(b'+') => (1.0, &text[1..]),
        _ => (1.0, text),
    };
    let mut radix = 10;
    if rest.starts_with("0x") || rest.starts_with("0X") {
        radix = 16;
        rest = &rest[2..];
    }
    let digits: &str = &rest[..rest
        .find(|c: char| !c.is_digit(radix))
        .unwrap_or(rest.len())];
    if digits.is_empty() {
        return f64::NAN;
    }
    let value = if radix == 10 {
        digits.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        digits.chars().fold(0.0, |value, digit| {
            value * 16.0 + f64::from(digit.to_digit(16).expect("a hex digit"))
        })
    };
    sign * value
}

/// Node.js's `tryStringify` for `%j`: `JSON.stringify(value)`, with `[Circular]` for a circular
/// structure. Other exceptions propagate.
fn try_stringify(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<String, ExnThrown> {
    match js::json::stringify(scope, value, None, None) {
        Ok(json) => Ok(json.unwrap_or_else(|| "undefined".into())),
        Err(_) => {
            let error = js::exception::take_pending(scope).ok_or(ExnThrown)?;
            let error = scope.root_value(error.get());
            let first_line = |scope: &Scope<'_>, error: HandleValue<'_>| -> Option<String> {
                let error = Object::from_value(scope, *error).ok()?;
                let message = error.get_property(scope, c"message").ok()?;
                let message = String::from_jsval(scope, message, ()).ok()?;
                Some(message.split('\n').next().unwrap_or_default().to_string())
            };
            let circular = Object::new_plain(scope)?;
            circular.set_property(scope, c"a", circular)?;
            let circular = scope.root_value(circular.to_jsval_raw_throwing(scope)?);
            let circular_message = match js::json::stringify(scope, circular, None, None) {
                Ok(_) => None,
                Err(_) => {
                    let circular_error = js::exception::take_pending(scope).ok_or(ExnThrown)?;
                    let circular_error = scope.root_value(circular_error.get());
                    first_line(scope, circular_error)
                }
            };
            let is_type_error = Object::from_value(scope, *error)
                .ok()
                .and_then(|error| error.get_property(scope, c"name").ok())
                .and_then(|name| String::from_jsval(scope, name, ()).ok())
                .is_some_and(|name| name == "TypeError");
            if is_type_error
                && circular_message.is_some()
                && first_line(scope, error) == circular_message
            {
                return Ok("[Circular]".into());
            }
            js::exception::set_pending(
                scope,
                error,
                js::native::ExceptionStackBehavior::DoNotCapture,
            );
            Err(ExnThrown)
        }
    }
}

/// `util.formatWithOptions(options, ...args)`, Node.js's `formatWithOptionsInternal`: the first
/// argument's `%` placeholders replaced by the following arguments, and the remaining arguments
/// appended, separated by spaces.
pub fn format(
    scope: &Scope<'_>,
    args: &[HandleValue<'_>],
    options: &InspectOptions,
) -> Result<String, ExnThrown> {
    let Some(first) = args.first() else {
        return Ok(String::new());
    };
    let mut out = String::new();
    let mut next = 0;
    let mut join = "";
    if first.is_string() {
        let template = to_string(scope, *first)?;
        if args.len() == 1 {
            return Ok(template);
        }
        let chars: Vec<char> = template.chars().collect();
        let intrinsics = Intrinsics::new(scope)?;
        let mut last = 0;
        let mut index = 0;
        while index + 1 < chars.len() {
            if chars[index] != '%' {
                index += 1;
                continue;
            }
            index += 1;
            let spec = chars[index];
            if next + 1 != args.len() {
                let text = match spec {
                    's' => {
                        next += 1;
                        let arg = args[next];
                        if arg.is_number() {
                            format_number(arg.to_number(), options.numeric_separator)
                        } else if arg.is_bigint() {
                            let digits = js::JSString::from_value(scope, arg)?.to_utf8(scope)?;
                            format_bigint(&digits, options.numeric_separator)
                        } else if !is_typeof_object(scope, arg)
                            || !has_builtin_to_string(
                                scope,
                                &intrinsics,
                                Object::from_value(scope, *arg).map_err(|e| e.throw(scope))?,
                            )?
                        {
                            if arg.is_symbol() {
                                symbol_to_string(scope, arg)
                            } else {
                                to_string(scope, arg)?
                            }
                        } else {
                            let mut shallow = options.clone();
                            shallow.compact = Compact::Number(3.0);
                            shallow.colors = false;
                            shallow.depth = Some(0.0);
                            inspect(scope, arg, shallow)?
                        }
                    }
                    'j' => {
                        next += 1;
                        try_stringify(scope, args[next])?
                    }
                    'd' => {
                        next += 1;
                        let arg = args[next];
                        if arg.is_bigint() {
                            let digits = js::JSString::from_value(scope, arg)?.to_utf8(scope)?;
                            format_bigint(&digits, options.numeric_separator)
                        } else if arg.is_symbol() {
                            "NaN".into()
                        } else {
                            format_number(number_of(scope, arg)?, options.numeric_separator)
                        }
                    }
                    'O' => {
                        next += 1;
                        inspect(scope, args[next], options.clone())?
                    }
                    'o' => {
                        next += 1;
                        let mut detailed = options.clone();
                        detailed.show_hidden = true;
                        detailed.show_proxy = true;
                        detailed.depth = Some(4.0);
                        inspect(scope, args[next], detailed)?
                    }
                    'i' => {
                        next += 1;
                        let arg = args[next];
                        if arg.is_bigint() {
                            let digits = js::JSString::from_value(scope, arg)?.to_utf8(scope)?;
                            format_bigint(&digits, options.numeric_separator)
                        } else if arg.is_symbol() {
                            "NaN".into()
                        } else {
                            format_number(
                                parse_int(&to_string(scope, arg)?),
                                options.numeric_separator,
                            )
                        }
                    }
                    'f' => {
                        next += 1;
                        let arg = args[next];
                        if arg.is_symbol() {
                            "NaN".into()
                        } else {
                            format_number(
                                parse_float(&to_string(scope, arg)?),
                                options.numeric_separator,
                            )
                        }
                    }
                    'c' => {
                        next += 1;
                        String::new()
                    }
                    '%' => {
                        out.extend(&chars[last..index]);
                        last = index + 1;
                        index += 1;
                        continue;
                    }
                    _ => {
                        index += 1;
                        continue;
                    }
                };
                if last != index - 1 {
                    out.extend(&chars[last..index - 1]);
                }
                out.push_str(&text);
                last = index + 1;
            } else if spec == '%' {
                out.extend(&chars[last..index]);
                last = index + 1;
            }
            index += 1;
        }
        if last != 0 {
            next += 1;
            join = " ";
            if last < chars.len() {
                out.extend(&chars[last..]);
            }
        }
    }
    while next < args.len() {
        let value = args[next];
        out.push_str(join);
        if value.is_string() {
            out.push_str(&to_string(scope, value)?);
        } else {
            out.push_str(&inspect(scope, value, options.clone())?);
        }
        join = " ";
        next += 1;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Deep equality
// ---------------------------------------------------------------------------

/// The comparison modes of Node.js's `comparisons.js`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `assert.deepEqual`: primitives by `==`, prototypes ignored.
    Loose,
    /// `assert.deepStrictEqual` and `util.isDeepStrictEqual`.
    Strict,
    /// `util.isDeepStrictEqual` with `skipPrototype`.
    StrictWithoutPrototypes,
}

/// How `keyCheck` iterates the entries of a pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Iteration {
    None,
    Array,
    Set,
    Map,
}

/// The names of the constructors Node.js's `wellKnownConstructors` holds.
const WELL_KNOWN_CONSTRUCTORS: &[&CStr] = &[
    c"Array",
    c"ArrayBuffer",
    c"BigInt",
    c"BigInt64Array",
    c"BigUint64Array",
    c"Boolean",
    c"DataView",
    c"Date",
    c"Error",
    c"Float32Array",
    c"Float64Array",
    c"Function",
    c"Int16Array",
    c"Int32Array",
    c"Int8Array",
    c"Map",
    c"Number",
    c"Object",
    c"Promise",
    c"RegExp",
    c"Set",
    c"String",
    c"Symbol",
    c"Uint16Array",
    c"Uint32Array",
    c"Uint8Array",
    c"Uint8ClampedArray",
    c"WeakMap",
    c"WeakSet",
];

/// A deep comparison in progress, for `isDeepStrictEqual` and `assert`'s deep assertions.
struct Comparer<'s> {
    intrinsics: Intrinsics<'s>,
    mode: Mode,
    /// The objects of the pairs being compared, to detect cycles.
    memo: Vec<RootedHeap<js::object::Object>>,
}

impl Comparer<'_> {
    /// `a === b`.
    fn identical(
        scope: &Scope<'_>,
        a: HandleValue<'_>,
        b: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        js::comparison::strictly_equal(scope, a, b)
    }

    /// Node.js's `innerDeepEqual`.
    fn inner(
        &mut self,
        scope: &Scope<'_>,
        a: HandleValue<'_>,
        b: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        if Self::identical(scope, a, b)? {
            return Ok(!(a.is_number() && a.to_number() == 0.0)
                || js::comparison::same_value(scope, a, b)?
                || self.mode == Mode::Loose);
        }
        let nan = |value: HandleValue<'_>| value.is_number() && value.to_number().is_nan();
        if self.mode != Mode::Loose {
            if a.is_number() {
                return Ok(nan(a) && nan(b));
            }
            if !is_typeof_object(scope, a) || !is_typeof_object(scope, b) {
                return Ok(false);
            }
        } else {
            if !is_typeof_object(scope, a) {
                return Ok(!is_typeof_object(scope, b)
                    && (js::comparison::loosely_equal(scope, a, b)? || (nan(a) && nan(b))));
            }
            if !is_typeof_object(scope, b) {
                return Ok(false);
            }
        }
        let x = Object::from_value(scope, *a).map_err(|e| e.throw(scope))?;
        let y = Object::from_value(scope, *b).map_err(|e| e.throw(scope))?;
        self.object_comparison_start(scope, x, y)
    }

    /// The comparison `setObjectEquiv` and `mapObjectEquiv` use for the pairs left to match:
    /// `objectComparisonStart` in the strict modes, and `innerDeepEqual` in the loose one.
    fn compare_entries(
        &mut self,
        scope: &Scope<'_>,
        a: HandleValue<'_>,
        b: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        if self.mode != Mode::Loose && is_typeof_object(scope, a) && is_typeof_object(scope, b) {
            let x = Object::from_value(scope, *a).map_err(|e| e.throw(scope))?;
            let y = Object::from_value(scope, *b).map_err(|e| e.throw(scope))?;
            return self.object_comparison_start(scope, x, y);
        }
        self.inner(scope, a, b)
    }

    /// `object[Symbol.toStringTag]`.
    fn tag_of<'r>(scope: &'r Scope<'_>, object: Object<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        object.get_property_by_id(scope, well_known(scope, SymbolCode::toStringTag))
    }

    /// Node.js's `hasUnequalTag`.
    fn unequal_tag(scope: &Scope<'_>, x: Object<'_>, y: Object<'_>) -> Result<bool, ExnThrown> {
        Ok(!Self::identical(
            scope,
            Self::tag_of(scope, x)?,
            Self::tag_of(scope, y)?,
        )?)
    }

    /// Node.js's `slowHasUnequalTag`.
    fn slow_unequal_tag(
        &self,
        scope: &Scope<'_>,
        x_tag: &str,
        x: Object<'_>,
        y: Object<'_>,
    ) -> Result<bool, ExnThrown> {
        let tx = Self::tag_of(scope, x)?;
        let ty = Self::tag_of(scope, y)?;
        if !tx.is_undefined() && !ty.is_undefined() {
            return Ok(!Self::identical(scope, tx, ty)?);
        }
        Ok(x_tag != self.intrinsics.object_to_string(scope, y)?)
    }

    /// Whether `object` has the own enumerable property `name`.
    fn has_enumerable(
        &self,
        scope: &Scope<'_>,
        object: Object<'_>,
        name: &str,
    ) -> Result<bool, ExnThrown> {
        Ok(self
            .intrinsics
            .named_descriptor(scope, object, name)?
            .is_some_and(|descriptor| descriptor.enumerable))
    }

    /// Node.js's `isEnumerableOrIdentical`.
    fn enumerable_or_identical(
        &mut self,
        scope: &Scope<'_>,
        x: Object<'_>,
        y: Object<'_>,
        name: &'static CStr,
    ) -> Result<bool, ExnThrown> {
        if self.has_enumerable(scope, y, &name.to_string_lossy())? {
            return Ok(true);
        }
        let a = x.get_property(scope, name)?;
        let b = y.get_property(scope, name)?;
        self.inner(scope, a, b)
    }

    /// Whether `ctor` is one of Node.js's `wellKnownConstructors`.
    fn well_known_constructor(scope: &Scope<'_>, ctor: HandleValue<'_>) -> Result<bool, ExnThrown> {
        if !ctor.is_object() {
            return Ok(false);
        }
        for name in WELL_KNOWN_CONSTRUCTORS {
            let global = scope.global().get_property(scope, name)?;
            if Self::identical(scope, global, ctor)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The own enumerable keys of `object`: strings, and symbols too if `symbols`, and without
    /// array indices if `skip_indices`.
    fn enumerable_keys<'r>(
        &self,
        scope: &'r Scope<'_>,
        object: Object<'_>,
        symbols: bool,
        skip_indices: bool,
    ) -> Result<Vec<HandleValue<'r>>, ExnThrown> {
        let mut keys = Vec::new();
        for key in object.own_keys(scope)? {
            if key.is_symbol() && !symbols {
                continue;
            }
            if skip_indices && key_text(scope, key)?.is_some_and(|text| is_index(&text)) {
                continue;
            }
            if self
                .intrinsics
                .descriptor(scope, object, key)?
                .is_some_and(|descriptor| descriptor.enumerable)
            {
                keys.push(key);
            }
        }
        Ok(keys)
    }

    /// Node.js's `objectComparisonStart`, with the handles it creates released when it returns.
    fn object_comparison_start(
        &mut self,
        scope: &Scope<'_>,
        x: Object<'_>,
        y: Object<'_>,
    ) -> Result<bool, ExnThrown> {
        let inner = scope.inner_scope();
        self.compare_objects(&inner, x, y)
    }

    fn compare_objects(
        &mut self,
        scope: &Scope<'_>,
        x: Object<'_>,
        y: Object<'_>,
    ) -> Result<bool, ExnThrown> {
        let strict = self.mode != Mode::Loose;
        if self.mode == Mode::Strict {
            let cx = x.get_property(scope, c"constructor")?;
            if Self::well_known_constructor(scope, cx)?
                || (!cx.is_undefined() && !x.has_own_property(scope, c"constructor")?)
            {
                let cy = y.get_property(scope, c"constructor")?;
                if !Self::identical(scope, cx, cy)? {
                    return Ok(false);
                }
            } else {
                let px = x.get_prototype(scope)?;
                let py = y.get_prototype(scope)?;
                if px.map(|p| p.as_raw()) != py.map(|p| p.as_raw()) {
                    return Ok(false);
                }
            }
        }

        if js::Array::is_array(scope, x.handle())? {
            if !js::Array::is_array(scope, y.handle())?
                || array_length(scope, x)? != array_length(scope, y)?
                || Self::unequal_tag(scope, x, y)?
            {
                return Ok(false);
            }
            let keys2 = self.enumerable_keys(scope, y, strict, true)?;
            if keys2.len() != self.enumerable_keys(scope, x, strict, true)?.len() {
                return Ok(false);
            }
            return self.key_check(scope, x, y, Iteration::Array, Some(keys2));
        }

        let mut x_tag = None;
        if Self::tag_of(scope, x)?.is_undefined() {
            let tag = self.intrinsics.object_to_string(scope, x)?;
            if tag == "[object Object]" {
                if self.slow_unequal_tag(scope, &tag, x, y)? {
                    return Ok(false);
                }
                return self.key_check(scope, x, y, Iteration::None, None);
            }
            x_tag = Some(tag);
        }
        if is_set(scope, x)? || is_map(scope, x)? {
            let map = is_map(scope, x)?;
            let same_kind = if map {
                is_map(scope, y)?
            } else {
                is_set(scope, y)?
            };
            if !same_kind {
                return Ok(false);
            }
            let size = |object: Object<'_>| {
                if map {
                    object.cast::<js::Map>().map(|m| m.size(scope)).unwrap_or(0)
                } else {
                    object.cast::<js::Set>().map(|s| s.size(scope)).unwrap_or(0)
                }
            };
            if size(x) != size(y) || Self::unequal_tag(scope, x, y)? {
                return Ok(false);
            }
            let iteration = if map { Iteration::Map } else { Iteration::Set };
            return self.key_check(scope, x, y, iteration, None);
        }
        if let Some(vx) = view_of(x) {
            let Some(vy) = view_of(y) else {
                return Ok(false);
            };
            let typed_tag = |view: js::ArrayBufferView<'_>| {
                view.view_kind()
                    .is_typed_array()
                    .then(|| view.view_kind().name())
            };
            if typed_tag(vx) != typed_tag(vy) {
                return Ok(false);
            }
            let float = matches!(
                vx.view_kind().name(),
                "Float32Array" | "Float64Array" | "Float16Array"
            );
            if self.mode == Mode::Loose && float {
                if vx.byte_length() != vy.byte_length() {
                    return Ok(false);
                }
                for index in 0..vx.array_length() as u32 {
                    let a = x.get_element(scope, index)?;
                    let b = y.get_element(scope, index)?;
                    if !Self::identical(scope, a, b)? {
                        return Ok(false);
                    }
                }
            } else if vx.byte_length() != vy.byte_length() || vx.with_bytes_pair(vy, |x, y| x != y)
            {
                return Ok(false);
            }
            let keys2 = self.enumerable_keys(scope, y, strict, true)?;
            if keys2.len() != self.enumerable_keys(scope, x, strict, true)?.len() {
                return Ok(false);
            }
            return self.key_check(scope, x, y, Iteration::None, Some(keys2));
        }
        if is_date(scope, x)? {
            if !is_date(scope, y)? || Self::unequal_tag(scope, x, y)? {
                return Ok(false);
            }
            let time = |object: Object<'_>| -> Result<f64, ExnThrown> {
                object
                    .cast::<js::Date>()
                    .map_err(|_| js::error::TypeError("not a Date".into()).throw(scope))?
                    .msec_since_epoch(scope)
            };
            let (tx, ty) = (time(x)?, time(y)?);
            if tx != ty && !(tx.is_nan() && ty.is_nan()) {
                return Ok(false);
            }
        } else if is_regexp(scope, x)? {
            if !is_regexp(scope, y)? || Self::unequal_tag(scope, x, y)? {
                return Ok(false);
            }
            for name in [c"source", c"flags", c"lastIndex"] {
                let a = x.get_property(scope, name)?;
                let b = y.get_property(scope, name)?;
                if !Self::identical(scope, a, b)? {
                    return Ok(false);
                }
            }
        } else if is_array_buffer(x) || is_shared_array_buffer(scope, x)? {
            if !(is_array_buffer(y) || is_shared_array_buffer(scope, y)?)
                || Self::unequal_tag(scope, x, y)?
            {
                return Ok(false);
            }
            if js::typedarray::with_buffer_source_bytes_pair(x, y, |x, y| x != y).unwrap_or(true) {
                return Ok(false);
            }
        } else {
            let x_tag = match x_tag {
                Some(tag) => tag,
                None => self.intrinsics.object_to_string(scope, x)?,
            };
            if self.slow_unequal_tag(scope, &x_tag, x, y)?
                || js::Array::is_array(scope, y.handle())?
                || view_of(y).is_some()
                || is_set(scope, y)?
                || is_map(scope, y)?
                || is_date(scope, y)?
                || is_regexp(scope, y)?
                || is_array_buffer(y)
            {
                return Ok(false);
            }
            if is_error(scope, x)? {
                if !is_error(scope, y)?
                    || !self.enumerable_or_identical(scope, x, y, c"message")?
                    || !self.enumerable_or_identical(scope, x, y, c"name")?
                    || !self.enumerable_or_identical(scope, x, y, c"cause")?
                    || !self.enumerable_or_identical(scope, x, y, c"errors")?
                {
                    return Ok(false);
                }
                if y.has_own_property(scope, c"cause")? != x.has_own_property(scope, c"cause")? {
                    return Ok(false);
                }
            } else if let Some(kind) = boxed_kind_with(scope, &self.intrinsics, x)? {
                let a = unbox(scope, x, kind)?;
                let b = unbox(scope, y, kind)?;
                let (Some(a), Some(b)) = (a, b) else {
                    return Ok(false);
                };
                let equal = if kind == Boxed::Number {
                    js::comparison::same_value(scope, a, b)?
                } else {
                    Self::identical(scope, a, b)?
                };
                if !equal {
                    return Ok(false);
                }
            } else if is_url(scope, x)? {
                if !is_url(scope, y)? {
                    return Ok(false);
                }
                let a = x.get_property(scope, c"href")?;
                let b = y.get_property(scope, c"href")?;
                if !Self::identical(scope, a, b)? {
                    return Ok(false);
                }
            } else if boxed_kind_with(scope, &self.intrinsics, y)?.is_some()
                || is_error(scope, y)?
                || is_error_instance(scope, y)?
                || is_weak_map(scope, x)?
                || is_weak_set(scope, x)?
                || js::Promise::is_promise(x.handle())
            {
                return Ok(false);
            }
        }
        self.key_check(scope, x, y, Iteration::None, None)
    }

    /// Node.js's `keyCheck` and `handleCycles`.
    fn key_check<'r>(
        &mut self,
        scope: &'r Scope<'_>,
        x: Object<'_>,
        y: Object<'_>,
        iteration: Iteration,
        keys2: Option<Vec<HandleValue<'r>>>,
    ) -> Result<bool, ExnThrown> {
        let array_like = keys2.is_some();
        let mut keys2 = match keys2 {
            Some(keys) => keys,
            None => self.enumerable_keys(scope, y, false, false)?,
        };
        let mut keys1 = None;
        if !array_like {
            let mut own = self.enumerable_keys(scope, x, false, false)?;
            if own.len() != keys2.len() {
                return Ok(false);
            }
            if self.mode != Mode::Loose {
                for key in self.enumerable_keys(scope, x, true, false)? {
                    if key.is_symbol() {
                        own.push(key);
                    }
                }
                for key in self.enumerable_keys(scope, y, true, false)? {
                    if key.is_symbol() {
                        keys2.push(key);
                    }
                }
                if own.len() != keys2.len() {
                    return Ok(false);
                }
            }
            keys1 = Some(own);
        }
        if keys2.is_empty() {
            let empty = match iteration {
                Iteration::None => true,
                Iteration::Array => array_length(scope, y)? == 0,
                Iteration::Set => y.cast::<js::Set>().map(|s| s.size(scope)).unwrap_or(0) == 0,
                Iteration::Map => y.cast::<js::Map>().map(|m| m.size(scope)).unwrap_or(0) == 0,
            };
            if empty {
                return Ok(true);
            }
        }
        let in_x = self.memo.iter().any(|seen| *seen == x);
        let in_y = self.memo.iter().any(|seen| *seen == y);
        if in_x || in_y {
            return Ok(in_x && in_y);
        }
        self.memo.push(RootedHeap::new(x));
        self.memo.push(RootedHeap::new(y));
        let result = self.object_equiv(scope, x, y, keys1.as_deref(), &keys2, iteration);
        self.memo.pop();
        self.memo.pop();
        result
    }

    /// Node.js's `objEquiv`.
    fn object_equiv(
        &mut self,
        scope: &Scope<'_>,
        x: Object<'_>,
        y: Object<'_>,
        keys1: Option<&[HandleValue<'_>]>,
        keys2: &[HandleValue<'_>],
        iteration: Iteration,
    ) -> Result<bool, ExnThrown> {
        let mut index = 0;
        if let Some(keys1) = keys1 {
            while index < keys2.len() {
                let key = keys2[index];
                if !Self::identical(scope, keys1[index], key)? {
                    break;
                }
                let id = key_id(scope, key)?;
                let a = x.get_property_by_id(scope, id)?;
                let b = y.get_property_by_id(scope, id)?;
                if !self.inner(scope, a, b)? {
                    return Ok(false);
                }
                index += 1;
            }
        }
        while index < keys2.len() {
            let key = keys2[index];
            let Some(descriptor) = self.intrinsics.descriptor(scope, x, key)? else {
                return Ok(false);
            };
            if !descriptor.enumerable {
                return Ok(false);
            }
            let id = key_id(scope, key)?;
            let a = if descriptor.data {
                descriptor.value
            } else {
                x.get_property_by_id(scope, id)?
            };
            let b = y.get_property_by_id(scope, id)?;
            if !self.inner(scope, a, b)? {
                return Ok(false);
            }
            index += 1;
        }
        match iteration {
            Iteration::None => Ok(true),
            Iteration::Array => self.array_equiv(scope, x, y),
            Iteration::Set => self.set_equiv(scope, x, y),
            Iteration::Map => self.map_equiv(scope, x, y),
        }
    }

    /// The element loop of Node.js's `objEquiv` for arrays.
    fn array_equiv(
        &mut self,
        scope: &Scope<'_>,
        x: Object<'_>,
        y: Object<'_>,
    ) -> Result<bool, ExnThrown> {
        let loose = self.mode == Mode::Loose;
        for index in 0..array_length(scope, x)? {
            let a = x.get_element(scope, index)?;
            let b = y.get_element(scope, index)?;
            let id = index_id(scope, index)?;
            if b.is_undefined() {
                if !y.has_own_property_by_id(scope, id)? {
                    return self.sparse_array_equiv(scope, x, y, index as usize);
                }
                if (!a.is_undefined() || !x.has_own_property_by_id(scope, id)?)
                    && (!loose || !a.is_null())
                {
                    return Ok(false);
                }
            } else if (a.is_undefined() || !self.inner(scope, a, b)?) && (!loose || !b.is_null()) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Node.js's `sparseArrayEquiv`.
    fn sparse_array_equiv(
        &mut self,
        scope: &Scope<'_>,
        x: Object<'_>,
        y: Object<'_>,
        start: usize,
    ) -> Result<bool, ExnThrown> {
        let keys_x = self.enumerable_keys(scope, x, false, false)?;
        let keys_y = self.enumerable_keys(scope, y, false, false)?;
        if keys_x.len() != keys_y.len() {
            return Ok(false);
        }
        for key in keys_y.iter().skip(start) {
            let id = key_id(scope, *key)?;
            let a = x.get_property_by_id(scope, id)?;
            if a.is_undefined() && !x.has_own_property_by_id(scope, id)? {
                return Ok(false);
            }
            let b = y.get_property_by_id(scope, id)?;
            if !self.inner(scope, a, b)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `set.has(value)`.
    fn set_has(
        scope: &Scope<'_>,
        set: Object<'_>,
        value: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        set.cast::<js::Set>()
            .map_err(|_| js::error::TypeError("not a Set".into()).throw(scope))?
            .has(scope, value)
    }

    /// Node.js's `findLooseMatchingPrimitives`: `Ok(answer)` if it settles the question.
    /// Otherwise `Err(true)` to look up `null` instead, and `Err(false)` to look up `undefined`.
    fn loose_matching_primitive(
        scope: &Scope<'_>,
        primitive: HandleValue<'_>,
    ) -> Result<Result<bool, bool>, ExnThrown> {
        if primitive.is_undefined() {
            return Ok(Err(true));
        }
        if primitive.is_null() {
            return Ok(Err(false));
        }
        if primitive.is_symbol() {
            return Ok(Ok(false));
        }
        if primitive.is_string() || primitive.is_number() {
            let number = f64::from_jsval(scope, primitive, ()).map_err(|_| ExnThrown)?;
            if number.is_nan() {
                return Ok(Ok(false));
            }
        }
        Ok(Ok(true))
    }

    /// Node.js's `setEquiv`.
    fn set_equiv(
        &mut self,
        scope: &Scope<'_>,
        x: Object<'_>,
        y: Object<'_>,
    ) -> Result<bool, ExnThrown> {
        let values_y: Vec<HandleValue<'_>> = collection_entries(scope, y, false)?
            .into_iter()
            .map(|(_, v)| v)
            .collect();
        let size_x = x.cast::<js::Set>().map(|s| s.size(scope)).unwrap_or(0) as usize;
        let mut pending: Option<Vec<HandleValue<'_>>> = None;
        for (position, value) in values_y.iter().enumerate() {
            if Self::set_has(scope, x, *value)? {
                continue;
            }
            if !is_typeof_object(scope, *value) || value.is_null() {
                if self.mode != Mode::Loose {
                    return Ok(false);
                }
                let might = match Self::loose_matching_primitive(scope, *value)? {
                    Ok(answer) => answer,
                    Err(null) => {
                        let alternative = if null {
                            HandleValue::null()
                        } else {
                            HandleValue::undefined()
                        };
                        !Self::set_has(scope, y, alternative)?
                            && Self::set_has(scope, x, alternative)?
                    }
                };
                if !might {
                    return Ok(false);
                }
            }
            if pending.is_none() {
                if size_x < 3 {
                    return self.compare_small_sets(scope, x, y, *value, &values_y[position + 1..]);
                }
                pending = Some(Vec::new());
            }
            pending.as_mut().expect("just created").push(*value);
        }
        match pending {
            None => Ok(true),
            Some(pending) => self.set_object_equiv(scope, pending, x, y),
        }
    }

    /// Node.js's `compareSmallSets`.
    fn compare_small_sets(
        &mut self,
        scope: &Scope<'_>,
        x: Object<'_>,
        y: Object<'_>,
        value: HandleValue<'_>,
        rest_y: &[HandleValue<'_>],
    ) -> Result<bool, ExnThrown> {
        let values_x: Vec<HandleValue<'_>> = collection_entries(scope, x, false)?
            .into_iter()
            .map(|(_, v)| v)
            .collect();
        fn at<'v>(values: &[HandleValue<'v>], index: usize) -> HandleValue<'v> {
            values
                .get(index)
                .copied()
                .unwrap_or(HandleValue::undefined())
        }
        let size_y = y.cast::<js::Set>().map(|s| s.size(scope)).unwrap_or(0);
        let first_x = at(&values_x, 0);
        if self.inner(scope, first_x, value)? {
            if size_y == 1 {
                return Ok(true);
            }
            let second_x = at(&values_x, 1);
            return Ok(
                Self::set_has(scope, y, second_x)? || self.inner(scope, second_x, at(rest_y, 0))?
            );
        }
        Ok(values_x.len() != 1
            && self.inner(scope, at(&values_x, 1), value)?
            && (size_y == 1
                || Self::set_has(scope, y, first_x)?
                || self.inner(scope, first_x, at(rest_y, 0))?))
    }

    /// Node.js's `setObjectEquiv`, which matches the values of `pending` in the order of `x`
    /// from both ends.
    fn set_object_equiv(
        &mut self,
        scope: &Scope<'_>,
        mut pending: Vec<HandleValue<'_>>,
        x: Object<'_>,
        y: Object<'_>,
    ) -> Result<bool, ExnThrown> {
        let values_x: Vec<HandleValue<'_>> = collection_entries(scope, x, false)?
            .into_iter()
            .map(|(_, v)| v)
            .collect();
        let mut forward = true;
        let mut start = 0usize;
        let mut end = pending.len() as isize - 1;
        let extra_checks = self.mode == Mode::Loose || pending.len() != values_x.len();
        for value in values_x {
            if extra_checks {
                if Self::set_has(scope, y, value)? {
                    continue;
                }
                let typeof_object = value.is_null() || is_typeof_object(scope, value);
                if !typeof_object && self.mode != Mode::Loose {
                    return Ok(false);
                }
            }
            if end < start as isize {
                return Ok(false);
            }
            let mut inner_start = start;
            if forward {
                if self.compare_entries(scope, value, pending[start])? {
                    start += 1;
                    continue;
                }
                if start as isize == end {
                    return Ok(false);
                }
                forward = false;
                inner_start += 1;
            }
            let end_index = end as usize;
            if !self.compare_entries(scope, value, pending[end_index])? {
                forward = true;
                if !self.array_has_equal_element(
                    scope,
                    &mut pending,
                    value,
                    inner_start,
                    end_index,
                )? {
                    return Ok(false);
                }
            }
            end -= 1;
        }
        Ok(true)
    }

    /// Node.js's `arrayHasEqualElement`.
    fn array_has_equal_element(
        &mut self,
        scope: &Scope<'_>,
        pending: &mut [HandleValue<'_>],
        value: HandleValue<'_>,
        start: usize,
        end: usize,
    ) -> Result<bool, ExnThrown> {
        for index in (start..end).rev() {
            if self.compare_entries(scope, value, pending[index])? {
                pending[index] = pending[end];
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `map.get(key)` and `map.has(key)`.
    fn map_get<'r>(
        scope: &'r Scope<'_>,
        map: Object<'_>,
        key: HandleValue<'r>,
    ) -> Result<(HandleValue<'r>, bool), ExnThrown> {
        let map = map
            .cast::<js::Map>()
            .map_err(|_| js::error::TypeError("not a Map".into()).throw(scope))?;
        let value = map.lookup(scope, key)?;
        let has = !value.is_undefined() || map.has(scope, key)?;
        Ok((value, has))
    }

    /// Node.js's `mapEquiv`.
    fn map_equiv(
        &mut self,
        scope: &Scope<'_>,
        x: Object<'_>,
        y: Object<'_>,
    ) -> Result<bool, ExnThrown> {
        let entries_y = collection_entries(scope, y, true)?;
        let size_x = x.cast::<js::Map>().map(|m| m.size(scope)).unwrap_or(0);
        let mut pending: Option<Vec<HandleValue<'_>>> = None;
        for (key, item) in entries_y {
            if is_typeof_object(scope, key) {
                if pending.is_none() {
                    if size_x == 1 {
                        let entries_x = collection_entries(scope, x, true)?;
                        let (key_x, item_x) = entries_x[0];
                        return Ok(
                            self.inner(scope, key_x, key)? && self.inner(scope, item_x, item)?
                        );
                    }
                    pending = Some(Vec::new());
                }
                pending.as_mut().expect("just created").push(key);
            } else {
                let (item_x, has) = Self::map_get(scope, x, key)?;
                if !has || !self.inner(scope, item_x, item)? {
                    if self.mode != Mode::Loose {
                        return Ok(false);
                    }
                    if !self.map_might_have_loose_primitive(scope, x, y, key, item)? {
                        return Ok(false);
                    }
                    pending.get_or_insert_with(Vec::new).push(key);
                }
            }
        }
        match pending {
            None => Ok(true),
            Some(pending) => self.map_object_equiv(scope, pending, x, y),
        }
    }

    /// Node.js's `mapMightHaveLoosePrim`.
    fn map_might_have_loose_primitive(
        &mut self,
        scope: &Scope<'_>,
        x: Object<'_>,
        y: Object<'_>,
        primitive: HandleValue<'_>,
        item: HandleValue<'_>,
    ) -> Result<bool, ExnThrown> {
        let alternative = match Self::loose_matching_primitive(scope, primitive)? {
            Ok(answer) => return Ok(answer),
            Err(true) => HandleValue::null(),
            Err(false) => HandleValue::undefined(),
        };
        let (item_x, has) = Self::map_get(scope, x, alternative)?;
        if !has || !self.inner(scope, item_x, item)? {
            return Ok(false);
        }
        let (_, y_has) = Self::map_get(scope, y, alternative)?;
        Ok(!y_has && self.inner(scope, item_x, item)?)
    }

    /// Node.js's `mapObjectEquiv`.
    fn map_object_equiv(
        &mut self,
        scope: &Scope<'_>,
        mut pending: Vec<HandleValue<'_>>,
        x: Object<'_>,
        y: Object<'_>,
    ) -> Result<bool, ExnThrown> {
        let entries_x = collection_entries(scope, x, true)?;
        let mut forward = true;
        let mut start = 0usize;
        let mut end = pending.len() as isize - 1;
        for (key, item) in entries_x {
            if !is_typeof_object(scope, key) {
                let (item_y, has) = Self::map_get(scope, y, key)?;
                if has {
                    if self.mode != Mode::Loose || self.inner(scope, item, item_y)? {
                        continue;
                    }
                } else if self.mode != Mode::Loose {
                    return Ok(false);
                }
            }
            if end < start as isize {
                return Ok(false);
            }
            let mut inner_start = start;
            if forward {
                let key_y = pending[start];
                if self.compare_entries(scope, key, key_y)? {
                    let (item_y, _) = Self::map_get(scope, y, key_y)?;
                    if self.inner(scope, item, item_y)? {
                        start += 1;
                        continue;
                    }
                }
                if start as isize == end {
                    return Ok(false);
                }
                forward = false;
                inner_start += 1;
            }
            let end_index = end as usize;
            let key_y = pending[end_index];
            let matched = self.compare_entries(scope, key, key_y)? && {
                let (item_y, _) = Self::map_get(scope, y, key_y)?;
                self.inner(scope, item, item_y)?
            };
            if !matched {
                forward = true;
                let mut found = false;
                for index in (inner_start..end_index).rev() {
                    let key_y = pending[index];
                    if self.compare_entries(scope, key, key_y)? {
                        let (item_y, _) = Self::map_get(scope, y, key_y)?;
                        if self.inner(scope, item, item_y)? {
                            pending[index] = pending[end_index];
                            found = true;
                            break;
                        }
                    }
                }
                if !found {
                    return Ok(false);
                }
            }
            end -= 1;
        }
        Ok(true)
    }
}

/// Node.js's `isURL`: an object with a truthy `href` and `protocol`, and neither `auth` nor
/// `path`.
fn is_url(scope: &Scope<'_>, object: Object<'_>) -> Result<bool, ExnThrown> {
    let truthy = |value: HandleValue<'_>| bool::from_jsval(scope, value, ()).unwrap_or(false);
    Ok(truthy(object.get_property(scope, c"href")?)
        && truthy(object.get_property(scope, c"protocol")?)
        && object.get_property(scope, c"auth")?.is_undefined()
        && object.get_property(scope, c"path")?.is_undefined())
}

/// `object instanceof Error`.
fn is_error_instance(scope: &Scope<'_>, object: Object<'_>) -> Result<bool, ExnThrown> {
    let error = global_object(scope, c"Error")?;
    instance_of(scope, object, error)
}

/// Whether `a` and `b` are deeply equal by Node.js's comparison `mode`.
pub fn deep_equal(
    scope: &Scope<'_>,
    a: HandleValue<'_>,
    b: HandleValue<'_>,
    mode: Mode,
) -> Result<bool, ExnThrown> {
    let mut comparer = Comparer {
        intrinsics: Intrinsics::new(scope)?,
        mode,
        memo: Vec::new(),
    };
    comparer.inner(scope, a, b)
}

/// JS's `Number(value)`: `value` converted to a number, where an object whose primitive value is a
/// bigint converts to that bigint's number instead of throwing.
fn number_of(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<f64, ExnThrown> {
    let primitive = match Object::from_value(scope, *value) {
        Ok(object) => object.to_primitive_number(scope)?,
        Err(_) => value,
    };
    if primitive.is_bigint() {
        let digits = js::JSString::from_value(scope, *primitive)?.to_utf8(scope)?;
        return Ok(digits.parse().unwrap_or(f64::NAN));
    }
    f64::from_jsval(scope, primitive, ()).map_err(|_| ExnThrown)
}
