// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Tests of the class, namespace and dictionary macros.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

/// Tests for `#[jsclass(js_proto = "Error")]` — inheriting from a JS built-in prototype.
mod js_proto_tests {
    use core_runtime::jsclass;
    use core_runtime::jsmethods;
    use core_runtime::test_util::eval_with_setup;

    /// Minimal class inheriting from Error.prototype via `js_proto`.
    #[jsclass(js_proto = "Error", to_string_tag = "CustomError")]
    struct CustomError {
        detail: String,
    }

    #[jsmethods]
    impl CustomError {
        #[constructor]
        fn construct(detail: String) -> Self {
            Self { detail }
        }

        #[getter]
        pub fn detail(&self) -> String {
            self.data().detail.clone()
        }
    }

    fn eval(code: &str) -> String {
        eval_with_setup(
            || {
                core_runtime::runtime::register_global_initializer(|scope, global| {
                    CustomError::add_to_global(scope, global);
                });
            },
            code,
        )
    }

    #[test]
    fn js_proto_classes_inherit_from_the_builtin_prototype() {
        assert_eq!(
            eval(
                "const e = new CustomError('d');
                 [e instanceof CustomError, Object.getPrototypeOf(e) === CustomError.prototype,
                  Object.getPrototypeOf(CustomError.prototype) === Error.prototype,
                  e.detail, Object.prototype.toString.call(e),
                  CustomError.prototype[Symbol.toStringTag]].join()"
            ),
            "true,true,true,d,[object CustomError],CustomError"
        );
    }

    /// `#[jsclass]` accessors are non-enumerable. A call of the constructor without `new` throws
    /// the same `TypeError` as a JS class constructor.
    #[test]
    fn jsclass_accessors_are_not_enumerable_and_constructors_require_new() {
        assert_eq!(
            eval(
                "class Plain {}
                 const error = (f) => {
                     try { f(); return 'no throw'; } catch (e) { return `${e.constructor.name}: ${e.message}`; }
                 };
                 const own = error(() => CustomError('x'));
                 [Object.getOwnPropertyDescriptor(CustomError.prototype, 'detail').enumerable,
                  own, own === error(() => Plain())].join('|')"
            ),
            "false|TypeError: class constructors must be invoked with 'new'|true"
        );
    }

    /// The stack is the one at construction, formatted when it is first read.
    #[test]
    fn stack_is_captured_at_construction() {
        assert_eq!(
            eval(
                "function make() { return new CustomError('x'); }
                 function read(e) { return e.stack; }
                 const s = read(make());
                 [s.includes('make@'), s.includes('read@')].join()"
            ),
            "true,false"
        );
    }

    /// Like `Error.prototype.stack`, `stack` is an accessor on the prototype: instances have no
    /// own `stack` until one is assigned, and the getter returns `undefined` for other objects and
    /// throws for primitives.
    #[test]
    fn stack_is_a_prototype_accessor() {
        assert_eq!(
            eval(
                "const e = new CustomError('x');
                 const d = Object.getOwnPropertyDescriptor(CustomError.prototype, 'stack');
                 const own = Object.getOwnPropertyDescriptor(e, 'stack');
                 e.stack = 'replaced';
                 const after = Object.getOwnPropertyDescriptor(e, 'stack');
                 let primitive;
                 try { d.get.call(1); primitive = 'no throw'; } catch (e) { primitive = e.constructor.name; }
                 [typeof d.get, typeof d.set, d.enumerable, d.configurable, own,
                  after.value, after.enumerable, after.writable,
                  d.get.call(CustomError.prototype), d.get.call({}), primitive].join()"
            ),
            "function,function,false,true,,replaced,true,true,,,TypeError"
        );
    }
}

mod constant_tests {
    use core_runtime::jsclass;
    use core_runtime::jsmethods;
    use core_runtime::test_util::eval_with_setup;

    /// Class with pub const items exposed as constructor constants.
    #[jsclass]
    struct StatusCode {}

    #[jsmethods]
    impl StatusCode {
        pub const OK: u16 = 200;
        pub const NOT_FOUND: u16 = 404;
        pub const INTERNAL_SERVER_ERROR: u16 = 500;

        #[constructor]
        fn construct() -> Self {
            Self {}
        }
    }

    fn eval(code: &str) -> String {
        eval_with_setup(
            || {
                core_runtime::runtime::register_global_initializer(|scope, global| {
                    StatusCode::add_to_global(scope, global);
                });
            },
            code,
        )
    }

    /// Constants are read-only, enumerable properties of the constructor only.
    #[test]
    fn constants_are_defined_on_the_constructor() {
        assert_eq!(
            eval(
                "'use strict';
                 let assigned;
                 try { StatusCode.OK = 999; assigned = 'changed'; } catch (e) { assigned = 'error'; }
                 [StatusCode.OK, StatusCode.NOT_FOUND, StatusCode.INTERNAL_SERVER_ERROR, assigned,
                  Object.keys(StatusCode).includes('OK'), StatusCode.prototype.OK,
                  new StatusCode().OK].map(String).join()"
            ),
            "200,404,500,error,true,undefined,undefined"
        );
    }
}

/// Tests for `#[webidl_interface]`: auto Symbol.toStringTag and
/// constants on both constructor AND prototype.
mod webidl_interface_tests {
    use core_runtime::test_util::eval_with_setup;
    use core_runtime::{webidl_interface, webidl_methods};

    /// A WebIDL-style interface with constants and an attribute.
    #[webidl_interface]
    struct MediaError {}

    #[webidl_methods]
    impl MediaError {
        pub const MEDIA_ERR_ABORTED: u16 = 1;
        pub const MEDIA_ERR_NETWORK: u16 = 2;
        pub const MEDIA_ERR_DECODE: u16 = 3;
        pub const MEDIA_ERR_SRC_NOT_SUPPORTED: u16 = 4;

        #[constructor]
        fn construct() -> Self {
            Self {}
        }

        #[getter]
        fn code(&self) -> u16 {
            1
        }
    }

    fn eval(code: &str) -> String {
        eval_with_setup(
            || {
                core_runtime::runtime::register_global_initializer(|scope, global| {
                    MediaError::add_to_global(scope, global);
                });
            },
            code,
        )
    }

    /// The constants are read-only properties of the constructor and of the prototype (WebIDL
    /// §3.7.3), so instances inherit them.
    #[test]
    fn interfaces_get_a_to_string_tag_and_constants_on_the_prototype() {
        assert_eq!(
            eval(
                "'use strict';
                 const assign = (target) => {
                     try { target.MEDIA_ERR_ABORTED = 99; return 'changed'; } catch (e) { return 'error'; }
                 };
                 [Object.prototype.toString.call(new MediaError()),
                  MediaError.prototype[Symbol.toStringTag], MediaError.MEDIA_ERR_ABORTED,
                  MediaError.MEDIA_ERR_NETWORK, MediaError.prototype.MEDIA_ERR_DECODE,
                  MediaError.prototype.MEDIA_ERR_SRC_NOT_SUPPORTED, new MediaError().MEDIA_ERR_ABORTED,
                  assign(MediaError), assign(MediaError.prototype)].join()"
            ),
            "[object MediaError],MediaError,1,2,3,4,1,error,error"
        );
    }

    /// Attributes are enumerable (WebIDL §3.7.6), and a call of the constructor without `new`
    /// throws a `TypeError`.
    #[test]
    fn interface_attributes_are_enumerable_and_constructors_require_new() {
        assert_eq!(
            eval(
                "let call;
                 try { MediaError(); call = 'no throw'; } catch (e) { call = e.constructor.name; }
                 [Object.getOwnPropertyDescriptor(MediaError.prototype, 'code').enumerable,
                  new MediaError().code, call].join()"
            ),
            "true,1,TypeError"
        );
    }
}

/// Tests for `#[jsnamespace]`: plain singleton object on global.
mod jsnamespace_tests {
    use core_runtime::jsnamespace;
    use core_runtime::test_util::eval_with_setup;

    #[jsnamespace(name = "math")]
    mod math_ns {
        use js::gc::scope::Scope;

        pub const PI: f64 = 3.5;

        pub fn add(a: f64, b: f64) -> f64 {
            a + b
        }

        pub fn multiply(a: f64, b: f64) -> f64 {
            a * b
        }

        pub fn greet(scope: &Scope<'_>, name: String) -> String {
            let _ = scope; // Verify scope is passed through
            format!("Hello, {name}!")
        }
    }

    fn eval(code: &str) -> String {
        eval_with_setup(
            || {
                core_runtime::runtime::register_global_initializer(|scope, global| {
                    math_ns::add_to_global(scope, global);
                });
            },
            code,
        )
    }

    /// The namespace is a plain object without a `Symbol.toStringTag`, and a non-enumerable
    /// property of the global (WebIDL §3.13). Its functions aren't constructors.
    #[test]
    fn namespaces_are_plain_objects_with_functions() {
        assert_eq!(
            eval(
                "let constructed;
                 try { new math.add(); constructed = 'ok'; } catch (e) { constructed = 'error'; }
                 [typeof math, math.add(2, 3), math.multiply(4, 5), math.greet('World'), math.PI,
                  constructed, Object.prototype.toString.call(math),
                  Object.getOwnPropertyDescriptor(globalThis, 'math').enumerable].join()"
            ),
            "object,5,20,Hello, World!,3.5,error,[object Object],false"
        );
    }
}

/// Tests for `#[webidl_namespace]`: namespace with auto Symbol.toStringTag.
mod webidl_namespace_tests {
    use core_runtime::{test_util::eval_with_setup, webidl_namespace};

    #[webidl_namespace(name = "CSS")]
    mod css_ns {
        pub fn escape(value: String) -> String {
            // Simplified CSS.escape for testing
            value.replace('\\', "\\\\")
        }
    }

    fn eval(code: &str) -> String {
        eval_with_setup(
            || {
                core_runtime::runtime::register_global_initializer(|scope, global| {
                    css_ns::add_to_global(scope, global);
                });
            },
            code,
        )
    }

    #[test]
    fn webidl_namespaces_get_a_to_string_tag() {
        assert_eq!(
            eval(
                "[typeof CSS, CSS.escape('hello'), Object.prototype.toString.call(CSS),
                  CSS[Symbol.toStringTag]].join()"
            ),
            "object,hello,[object CSS],CSS"
        );
    }
}

/// Tests for setup-style constructors (using `&self`) with `extends` inheritance.
mod setup_style_inheritance {
    use core_runtime::jsclass;
    use core_runtime::jsmethods;
    use core_runtime::test_util::eval_with_setup;

    #[jsclass]
    struct Pet {
        kind: String,
    }

    #[jsmethods]
    impl Pet {
        #[constructor]
        fn new(&self, kind: String) -> Result<(), String> {
            let mut data = self.data_mut();
            data.kind = kind;
            Ok(())
        }

        #[getter]
        fn kind(&self) -> String {
            self.data().kind.clone()
        }

        #[method]
        fn sound(&self) -> String {
            "<generic pet sound>".to_string()
        }
    }

    #[jsclass(extends = Pet)]
    struct Lily {
        parent: PetImpl,
        cuteness: f64,
    }

    #[jsmethods]
    impl Lily {
        #[constructor]
        fn new(&self, cuteness: f64) -> Result<(), String> {
            let mut data = self.data_mut();
            data.parent = PetImpl {
                kind: "The very cutest".to_string(),
            };
            data.cuteness = cuteness;
            Ok(())
        }

        #[getter]
        fn cuteness(&self) -> f64 {
            self.data().cuteness
        }

        #[method]
        fn sound(&self) -> String {
            "The cutest sound!".to_string()
        }

        #[method]
        fn describe(&self) -> String {
            let d = self.data();
            format!("{} Lily, cuteness: {}/5", d.parent.kind, d.cuteness)
        }
    }

    fn eval(code: &str) -> String {
        eval_with_setup(
            || {
                core_runtime::runtime::register_global_initializer(|scope, global| {
                    Pet::add_to_global(scope, global);
                    Lily::add_to_global(scope, global);
                });
            },
            code,
        )
    }

    #[test]
    fn setup_style_constructors_initialize_subclasses() {
        assert_eq!(
            eval(
                "const pet = new Pet('Cat');
                 const lily = new Lily(5);
                 [pet.kind, pet.sound(), lily.kind, lily.cuteness, lily.sound(), lily instanceof Pet,
                  lily.describe()].join('|')"
            ),
            "Cat|<generic pet sound>|The very cutest|5|The cutest sound!|true|\
             The very cutest Lily, cuteness: 5/5"
        );
    }
}

/// Tests for `#[webidl_dictionary]` — WebIDL dictionary conversion.
mod webidl_dictionary_tests {
    use core_runtime::test_util::eval_with_setup;
    use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods};
    use js::gc::scope::Scope;
    use js::Object;

    // A dictionary with all required members.
    #[webidl_dictionary]
    pub struct PersonInit {
        pub name: String,
        pub age: f64,
    }

    // A dictionary with optional members.
    #[webidl_dictionary]
    pub struct GreetingOptions {
        pub prefix: Option<String>,
        pub excited: Option<bool>,
    }

    // A dictionary with a default value.
    #[webidl_dictionary]
    pub struct ConfigInit {
        #[webidl(default = 10.0)]
        pub timeout: f64,
        pub label: Option<String>,
    }

    // A lifetimed dictionary with scope-rooted types.
    #[webidl_dictionary]
    pub struct StreamOptions<'a> {
        pub high_water_mark: f64,
        pub source: Option<Object<'a>>,
    }

    // A dictionary with a custom JS name on a field.
    #[webidl_dictionary]
    pub struct CustomNamed {
        #[webidl(name = "mySpecialField")]
        pub special: String,
    }

    // A three-level inheritance chain. Each level adds one member, and the members are named so
    // that sorting them as one flat set would interleave the levels — `bottom` and `base` sort
    // before `middle`, which sorts before `top`, so a flat merge reads them in a different order
    // than the spec's "inherited dictionaries first, least derived first".
    #[webidl_dictionary]
    pub struct BaseInit {
        #[webidl(default = String::new())]
        pub top: String,
        #[webidl(default = String::new())]
        pub base: String,
    }

    #[webidl_dictionary(extends = BaseInit)]
    pub struct MiddleInit {
        pub parent: BaseInit,
        #[webidl(default = String::new())]
        pub middle: String,
    }

    #[webidl_dictionary(extends = MiddleInit)]
    pub struct DerivedInit<'a> {
        pub parent: MiddleInit,
        #[webidl(default = String::new())]
        pub bottom: String,
        /// Present only to make the derived dictionary carry a lifetime while its parents don't —
        /// the shape `FetchEventInit` has.
        pub extra: Option<Object<'a>>,
    }

    // A class that uses dictionary parameters.
    #[webidl_interface]
    struct Greeter {
        greeting: String,
    }

    #[webidl_interface]
    struct Inherited {
        /// The inherited members, read through `Deref` at each level of the chain.
        members: String,
    }

    #[webidl_methods]
    impl Inherited {
        #[constructor]
        fn new(init: &DerivedInit<'_>) -> Self {
            Self {
                // `top` and `base` live two levels up, `middle` one — `Deref` walks the chain.
                members: format!(
                    "{} {} {} {} {}",
                    init.top,
                    init.base,
                    init.middle,
                    init.bottom,
                    init.extra.is_some()
                ),
            }
        }

        #[getter]
        fn members(&self) -> String {
            self.data().members.clone()
        }

        /// An `Option<&T>` parameter, and the reason references are worth having: `&DerivedInit`
        /// coerces to `&MiddleInit` and on to `&BaseInit` at the call, so a value can be handed to
        /// something that wants an ancestor without taking it apart first.
        #[method]
        fn describe(&self, derived: &DerivedInit<'_>, other: Option<&MiddleInit>) -> String {
            fn base(init: &BaseInit) -> String {
                format!("{}/{}", init.top, init.base)
            }
            match other {
                Some(other) => format!("{} {}", base(derived), base(other)),
                None => base(derived),
            }
        }

        /// The same string as `members`, lent rather than copied: the guard holds the borrow open
        /// until the trampoline has built the JS string from the stored bytes.
        #[getter]
        fn borrowed_members(&self) -> js::class::Ref<'_, str> {
            js::class::Ref::map(self.data(), |data| data.members.as_str())
        }

        /// Takes a *mutable* borrow, so it panics if a lent value ever failed to give its
        /// shared borrow back.
        #[method]
        fn touch(&self) {
            self.data_mut().members.push('!');
        }

        /// `&str` parameters. A call from JS still has to copy the string out of the engine. Rust
        /// callers can pass a borrow.
        #[method]
        fn label(&self, prefix: &str, suffix: Option<&str>) -> String {
            format!("{prefix}{}{}", self.data().members, suffix.unwrap_or(""))
        }

        /// `&[T]` parameters. An integer element routes through the configured conversion and a
        /// non-integer one through the plain path, so both are worth covering.
        #[method]
        fn tally(&self, bytes: &[u8], words: Option<&[String]>) -> String {
            let sum: u32 = bytes.iter().map(|byte| u32::from(*byte)).sum();
            format!("{} {} {}", bytes.len(), sum, words.map_or(0, <[_]>::len))
        }

        /// A promise-returning operation puts its argument extractions inside a closure, so the
        /// lent value has to outlive the call there too.
        #[method]
        fn describe_later<'r>(
            &self,
            scope: &'r Scope<'_>,
            derived: &DerivedInit<'_>,
        ) -> Result<js::Promise<'r>, js::error::ExnThrown> {
            js::Promise::new_resolved_with_value(scope, format!("{}!", derived.top))
        }
    }

    #[webidl_methods]
    impl Greeter {
        #[constructor]
        fn new(person: PersonInit, options: Option<GreetingOptions>) -> Self {
            let prefix = options
                .as_ref()
                .and_then(|o| o.prefix.clone())
                .unwrap_or_else(|| "Hello".to_string());
            let excited = options.as_ref().and_then(|o| o.excited).unwrap_or(false);
            let suffix = if excited { "!" } else { "." };
            Self {
                greeting: format!("{}, {} (age {}){}", prefix, person.name, person.age, suffix),
            }
        }

        #[getter]
        fn greeting(&self) -> String {
            self.data().greeting.clone()
        }
    }

    // A class testing default values.
    #[webidl_interface]
    struct Config {
        timeout: f64,
        label: String,
    }

    #[webidl_methods]
    impl Config {
        #[constructor]
        fn new(init: ConfigInit) -> Self {
            Self {
                timeout: init.timeout,
                label: init.label.unwrap_or_else(|| "default".to_string()),
            }
        }

        #[getter]
        fn timeout(&self) -> f64 {
            self.data().timeout
        }

        #[getter]
        fn label(&self) -> String {
            self.data().label.clone()
        }
    }

    fn setup() {
        core_runtime::runtime::register_global_initializer(|scope, global| {
            Greeter::add_to_global(scope, global);
            Config::add_to_global(scope, global);
            Inherited::add_to_global(scope, global);
        });
    }

    fn eval(code: &str) -> String {
        eval_with_setup(setup, code)
    }

    /// A `null` optional dictionary argument converts like an absent one.
    #[test]
    fn dictionary_members_are_extracted() {
        assert_eq!(
            eval(
                "[new Greeter({ name: 'Alice', age: 30 }).greeting,
                  new Greeter({ name: 'Bob', age: 25 }, { prefix: 'Hi', excited: true }).greeting,
                  new Greeter({ name: 'Dana', age: 18 }, null).greeting].join('|')"
            ),
            "Hello, Alice (age 30).|Hi, Bob (age 25)!|Hello, Dana (age 18)."
        );
    }

    #[test]
    fn missing_required_members_and_non_objects_throw() {
        assert_eq!(
            eval(
                "[{ name: 'Alice' }, {}, 42].map(init => {
                     try { new Greeter(init); return 'no throw'; } catch (e) { return e.constructor.name; }
                 }).join()"
            ),
            "TypeError,TypeError,TypeError"
        );
    }

    /// A `null` or `undefined` dictionary converts like an empty one.
    #[test]
    fn absent_members_take_their_defaults() {
        assert_eq!(
            eval(
                "[new Config({}).timeout, new Config({ timeout: 42 }).timeout, new Config({}).label,
                  new Config({ label: 'custom' }).label, new Config(null).timeout,
                  new Config(undefined).timeout].join()"
            ),
            "10,42,default,custom,10,10"
        );
    }

    #[test]
    fn inherited_members_are_read_through_the_chain() {
        assert_eq!(
            eval(
                "[new Inherited({ top: 'T', base: 'B', middle: 'M', bottom: 'b', extra: {} }).members,
                  new Inherited({}).members].join('|')"
            ),
            "T B M b true|    false"
        );
    }

    #[test]
    fn inherited_dictionaries_convert_least_derived_first() {
        // Every member is a property get, so an accessor on the source object records the order
        // in which the conversion reads them. WebIDL: the inherited dictionaries first, least
        // derived first, each one's own members lexicographically. Sorting all five as one flat
        // set would give `base, bottom, extra, middle, top`. This asserts the chain is walked
        // instead.
        assert_eq!(
            eval(
                r#"
                const order = [];
                const spy = (name) => ({
                    get() { order.push(name); return name === 'extra' ? {} : name; },
                    enumerable: true,
                });
                const init = Object.defineProperties({}, {
                    top: spy('top'), base: spy('base'), middle: spy('middle'),
                    bottom: spy('bottom'), extra: spy('extra'),
                });
                new Inherited(init);
                order.join(',')
                "#
            ),
            "base,top,middle,bottom,extra"
        );
    }

    /// Operations take dictionaries, `&str` and `&[T]` parameters by reference, plain and in an
    /// `Option`, and a promise-returning operation takes a dictionary by reference too. A
    /// non-string argument to a `&str` parameter still coerces.
    #[test]
    fn parameters_can_be_taken_by_reference() {
        assert_eq!(
            eval(
                "const i = new Inherited({ top: 'T' });
                 [i.describe({ top: 'T', base: 'B' }, { top: 't', base: 'b' }),
                  i.describe({ top: 'T', base: 'B' }), i.describe({ top: 'T', base: 'B' }, undefined),
                  i.label('[', ']'), i.label('['), i.label(1, 2), i.tally([1, 2, 3], ['a', 'b']),
                  i.tally([]), i.describeLater({ top: 'T' }) instanceof Promise].join('|')"
            ),
            "T/B t/b|T/B|T/B|[T    false]|[T    false|1T    false2|3 6 2|0 0 0|true"
        );
    }

    /// The lent string is an ordinary JS string, and the shared borrow is released after the
    /// call: `touch` borrows the data mutably, which panics otherwise.
    #[test]
    fn a_getter_can_lend_a_string_instead_of_copying_it() {
        assert_eq!(
            eval(
                "const i = new Inherited({ top: 'T', base: 'B' });
                 const lent = i.borrowedMembers;
                 i.touch();
                 [typeof lent, lent === 'T B   false', i.borrowedMembers.endsWith('!')].join()"
            ),
            "string,true,true"
        );
    }
}

/// Tests for Rust-side factory functions: unannotated constructor-shaped fns
/// in a `#[jsmethods]` block, including the fallible `Result<Self, E>` shape.
mod implicit_factory_tests {
    use core_runtime::config::RuntimeConfig;
    use core_runtime::jsclass;
    use core_runtime::jsmethods;
    use core_runtime::runtime::Runtime;
    use js::error::{throw_type_error, ExnThrown};
    use js::gc::scope::Scope;

    #[jsclass]
    struct Checked {
        value: i32,
    }

    #[jsmethods]
    impl Checked {
        #[constructor]
        fn construct() -> Self {
            Self { value: 0 }
        }

        /// Fallible Rust-side factory, detected by its `Result<Self, E>` shape.
        fn try_new(scope: &Scope<'_>, value: i32) -> Result<Self, ExnThrown> {
            if value < 0 {
                return Err(throw_type_error(scope, c"value must be non-negative"));
            }
            Ok(Self { value })
        }
    }

    #[test]
    fn fallible_factory_ok_and_err() {
        core_runtime::runtime::register_global_initializer(|scope, global| {
            Checked::add_to_global(scope, global);
        });
        let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
        let scope = rt.default_global();

        let ok = Checked::try_new(&scope, 7).expect("factory should succeed");
        assert_eq!(ok.data().value, 7);

        assert!(Checked::try_new(&scope, -1).is_err());
        // The error path must leave the thrown exception pending; capture
        // clears it and yields the message.
        let captured = ExnThrown::capture(&scope);
        assert_eq!(
            captured.message.as_deref(),
            Some("value must be non-negative")
        );
    }
}

/// Tests of class registration.
mod registration_tests {
    use core_runtime::config::RuntimeConfig;
    use core_runtime::runtime::Runtime;
    use core_runtime::test_util::eval_with_setup;
    use core_runtime::{jsclass, jsmethods};
    use js::conversion::ToJSVal;

    #[jsclass]
    struct Registered {
        value: i32,
    }

    #[jsmethods]
    impl Registered {
        #[constructor]
        fn construct() -> Self {
            Self { value: 1 }
        }
    }

    #[test]
    fn class_constructor_ignores_a_replaced_prototype_constructor() {
        core_runtime::runtime::register_global_initializer(|scope, global| {
            Registered::add_to_global(scope, global);
        });
        let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
        let scope = rt.default_global();
        let original = js::compile::evaluate(
            &scope,
            "const original = Registered;
             Registered.prototype.constructor = function Replaced() {};
             original",
        )
        .expect("the script runs");
        let ctor = js::class::class_constructor::<RegisteredImpl>(&scope).expect("registered");
        let ctor = ctor.to_jsval(&scope).expect("an object converts");
        assert!(js::comparison::same_value(&scope, ctor, original).expect("comparing succeeds"));
    }

    #[jsclass]
    struct Visible {}

    #[jsmethods]
    impl Visible {
        #[constructor]
        fn construct() -> Self {
            Self {}
        }
    }

    #[jsclass(hidden, extends = Visible)]
    struct HiddenChild {
        parent: VisibleImpl,
    }

    #[jsmethods]
    impl HiddenChild {
        #[constructor]
        fn construct() -> Self {
            Self {
                parent: VisibleImpl::construct(),
            }
        }
    }

    /// Registering a hidden class before its parent defines the parent's constructor on the
    /// global and leaves the hidden class's constructor off it.
    #[test]
    fn a_hidden_class_registered_first_leaves_its_parent_visible() {
        assert_eq!(
            eval_with_setup(
                || {
                    core_runtime::runtime::register_global_initializer(|scope, global| {
                        HiddenChild::add_to_global(scope, global);
                        Visible::add_to_global(scope, global);
                    });
                },
                "[typeof Visible, typeof HiddenChild].join()"
            ),
            "function,undefined"
        );
    }
}
