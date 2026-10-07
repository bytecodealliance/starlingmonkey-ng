// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://urlpattern.spec.whatwg.org/>
//!
//! The `urlpattern` crate parses patterns and generates the regular expressions of their
//! components. The regular expressions are SpiderMonkey `RegExp`s with the `v` flag, as the
//! standard specifies: [`EngineRegExp`] checks their syntax and runs them while the crate parses a
//! pattern, and each `URLPattern` compiles them once for matching.

use std::cell::Cell;

use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods};
use js::conversion::{ConversionError, FromJSVal};
use js::error::{ExnThrown, ThrowException, TypeError};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{JSString, Object};
use urlpattern::quirks;
use urlpattern::regexp::RegExp as PatternRegExp;
use urlpattern::{RegexSyntax, UrlPatternInit, UrlPatternOptions};

/// The names of a URL pattern's components, in the order of [`URLPattern`]'s components.
const COMPONENTS: [&std::ffi::CStr; 8] = [
    c"protocol",
    c"username",
    c"password",
    c"hostname",
    c"port",
    c"pathname",
    c"search",
    c"hash",
];

/// A component of a URL pattern.
///
/// <https://urlpattern.spec.whatwg.org/#component>
#[js::must_root]
#[derive(core_runtime::Traceable)]
struct Component {
    /// <https://urlpattern.spec.whatwg.org/#component-pattern-string>
    #[no_trace]
    pattern_string: String,
    /// <https://urlpattern.spec.whatwg.org/#component-regular-expression>
    regular_expression: Heap<js::regexp::RegExp>,
    /// <https://urlpattern.spec.whatwg.org/#component-group-name-list>
    #[no_trace]
    group_name_list: Vec<String>,
}

/// <https://urlpattern.spec.whatwg.org/#urlpattern>
#[webidl_interface]
pub struct URLPattern {
    /// The components of the
    /// <https://urlpattern.spec.whatwg.org/#urlpattern-associated-url-pattern>, in the order of
    /// [`COMPONENTS`]. Set by the constructor.
    components: Vec<Component>,
    /// <https://urlpattern.spec.whatwg.org/#url-pattern-has-regexp-groups>
    #[no_trace]
    has_regexp_groups: bool,
}

/// <https://urlpattern.spec.whatwg.org/#dictdef-urlpatterninit>
#[webidl_dictionary]
#[derive(Default)]
pub struct URLPatternInit {
    pub protocol: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub hostname: Option<String>,
    pub port: Option<String>,
    pub pathname: Option<String>,
    pub search: Option<String>,
    pub hash: Option<String>,
    #[webidl(name = "baseURL")]
    pub base_url: Option<String>,
}

/// <https://urlpattern.spec.whatwg.org/#dictdef-urlpatternoptions>
#[webidl_dictionary]
#[derive(Default)]
pub struct URLPatternOptions {
    #[webidl(default = false, name = "ignoreCase")]
    pub ignore_case: bool,
}

/// <https://urlpattern.spec.whatwg.org/#typedefdef-urlpatterninput>
enum URLPatternInput {
    String(String),
    Init(URLPatternInit),
}

impl URLPatternInput {
    /// The `URLPatternInput` `value` converts to: a `URLPatternInit` for `undefined`, `null` and
    /// objects, and a string otherwise.
    fn from_value(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<Self, ExnThrown> {
        if value.is_object() || value.is_null_or_undefined() {
            let init =
                URLPatternInit::from_jsval(scope, value, ()).map_err(|error| match error {
                    ConversionError::ExnPending => ExnThrown,
                    failure => failure.throw(scope),
                })?;
            return Ok(URLPatternInput::Init(init));
        }
        Ok(URLPatternInput::String(String::from_jsval_throwing(
            scope,
            value,
            (),
        )?))
    }
}

/// A `TypeError` with the message of the `urlpattern` crate's `error`.
fn type_error(scope: &Scope<'_>, error: urlpattern::Error) -> ExnThrown {
    TypeError(error.to_string()).throw(scope)
}

#[webidl_methods]
impl URLPattern {
    /// <https://urlpattern.spec.whatwg.org/#dom-urlpattern-urlpattern> and
    /// <https://urlpattern.spec.whatwg.org/#dom-urlpattern-urlpattern-input-options>
    ///
    /// The second argument is the `baseURL` of the first overload unless it is `undefined`,
    /// `null` or an object, which the second overload's `options` is.
    #[constructor(length = 0)]
    fn new(
        &self,
        scope: &Scope<'_>,
        input: Option<HandleValue<'_>>,
        base_url_or_options: Option<HandleValue<'_>>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        let input = URLPatternInput::from_value(scope, input.unwrap_or(HandleValue::undefined()))?;
        let second = base_url_or_options.unwrap_or(HandleValue::undefined());
        let (base_url, options) = if second.is_object() || second.is_null_or_undefined() {
            (None, second)
        } else {
            let base_url = String::from_jsval_throwing(scope, second, ())?;
            (Some(base_url), options.unwrap_or(HandleValue::undefined()))
        };
        let options =
            URLPatternOptions::from_jsval(scope, options, ()).map_err(|error| match error {
                ConversionError::ExnPending => ExnThrown,
                failure => failure.throw(scope),
            })?;
        // The new URLPattern(_input_, _baseURL_, _options_) constructor steps are:
        // Step 1: Run `initialize` given `this`, _input_, _baseURL_, and _options_.
        // The new URLPattern(_input_, _options_) constructor steps are:
        // Step 1: Run `initialize` given `this`, _input_, null, and _options_.
        self.initialize(scope, input, base_url, &options)
    }

    /// <https://urlpattern.spec.whatwg.org/#urlpattern-initialize>
    fn initialize(
        &self,
        scope: &Scope<'_>,
        input: URLPatternInput,
        base_url: Option<String>,
        options: &URLPatternOptions,
    ) -> Result<(), ExnThrown> {
        // Step 1: Set `this`'s `associated URL pattern` to the result of `create` given _input_,
        //         _baseURL_, and _options_.
        // `urlpattern` implements `create`, with the base URL parsed first.
        let parse_url = |url: &str| {
            url::Url::parse(url).map_err(|error| type_error(scope, urlpattern::Error::Url(error)))
        };
        let init = match input {
            URLPatternInput::String(input) => {
                let base_url = base_url.as_deref().map(parse_url).transpose()?;
                with_parse_scope(scope, || {
                    UrlPatternInit::parse_constructor_string::<EngineRegExp>(&input, base_url)
                })
                .map_err(|error| type_error(scope, error))?
            }
            URLPatternInput::Init(input) => {
                if base_url.is_some() {
                    return Err(type_error(scope, urlpattern::Error::BaseUrlWithInit));
                }
                UrlPatternInit {
                    protocol: input.protocol,
                    username: input.username,
                    password: input.password,
                    hostname: input.hostname,
                    port: input.port,
                    pathname: input.pathname,
                    search: input.search,
                    hash: input.hash,
                    base_url: input.base_url.as_deref().map(parse_url).transpose()?,
                }
            }
        };
        let pattern_options = UrlPatternOptions {
            regex_syntax: RegexSyntax::EcmaScript,
            ignore_case: options.ignore_case,
        };
        let pattern = with_parse_scope(scope, || {
            urlpattern::UrlPattern::<EngineRegExp>::parse(init, pattern_options)
        })
        .map_err(|error| type_error(scope, error))?;
        let has_regexp_groups = pattern.has_regexp_groups();
        let flags = if options.ignore_case { "vi" } else { "v" };
        for component in [
            pattern.protocol,
            pattern.username,
            pattern.password,
            pattern.hostname,
            pattern.port,
            pattern.pathname,
            pattern.search,
            pattern.hash,
        ] {
            let regexp = component.regexp.map_err(|error| type_error(scope, error))?;
            let source: Vec<u16> = regexp.source.encode_utf16().collect();
            let regular_expression =
                js::RegExp::from_utf16(scope, &source, js::regexp::flags_from_str(flags)).map_err(
                    |_| {
                        js::exception::clear(scope);
                        TypeError("Invalid regular expression".into()).throw(scope)
                    },
                )?;
            self.data_mut().components.push(Component {
                pattern_string: component.pattern_string,
                regular_expression: Heap::from(regular_expression),
                group_name_list: component.group_name_list,
            });
        }
        self.data_mut().has_regexp_groups = has_regexp_groups;
        Ok(())
    }

    /// The pattern string of component `index` of [`COMPONENTS`].
    fn pattern_string(&self, index: usize) -> String {
        self.data().components[index].pattern_string.clone()
    }

    /// <https://urlpattern.spec.whatwg.org/#dom-urlpattern-protocol>
    #[getter]
    fn protocol(&self) -> String {
        // Step 1: Return `this`'s `associated URL pattern`'s `protocol component`'s `pattern
        //         string`.
        self.pattern_string(0)
    }

    /// <https://urlpattern.spec.whatwg.org/#dom-urlpattern-username>
    #[getter]
    fn username(&self) -> String {
        // Step 1: Return `this`'s `associated URL pattern`'s `username component`'s `pattern
        //         string`.
        self.pattern_string(1)
    }

    /// <https://urlpattern.spec.whatwg.org/#dom-urlpattern-password>
    #[getter]
    fn password(&self) -> String {
        // Step 1: Return `this`'s `associated URL pattern`'s `password component`'s `pattern
        //         string`.
        self.pattern_string(2)
    }

    /// <https://urlpattern.spec.whatwg.org/#dom-urlpattern-hostname>
    #[getter]
    fn hostname(&self) -> String {
        // Step 1: Return `this`'s `associated URL pattern`'s `hostname component`'s `pattern
        //         string`.
        self.pattern_string(3)
    }

    /// <https://urlpattern.spec.whatwg.org/#dom-urlpattern-port>
    #[getter]
    fn port(&self) -> String {
        // Step 1: Return `this`'s `associated URL pattern`'s `port component`'s `pattern
        //         string`.
        self.pattern_string(4)
    }

    /// <https://urlpattern.spec.whatwg.org/#dom-urlpattern-pathname>
    #[getter]
    fn pathname(&self) -> String {
        // Step 1: Return `this`'s `associated URL pattern`'s `pathname component`'s `pattern
        //         string`.
        self.pattern_string(5)
    }

    /// <https://urlpattern.spec.whatwg.org/#dom-urlpattern-search>
    #[getter]
    fn search(&self) -> String {
        // Step 1: Return `this`'s `associated URL pattern`'s `search component`'s `pattern
        //         string`.
        self.pattern_string(6)
    }

    /// <https://urlpattern.spec.whatwg.org/#dom-urlpattern-hash>
    #[getter]
    fn hash(&self) -> String {
        // Step 1: Return `this`'s `associated URL pattern`'s `hash component`'s `pattern
        //         string`.
        self.pattern_string(7)
    }

    /// <https://urlpattern.spec.whatwg.org/#dom-urlpattern-hasregexpgroups>
    #[getter(name = "hasRegExpGroups")]
    fn has_regexp_groups(&self) -> bool {
        // Step 1: If `this`'s `associated URL pattern`'s `has regexp groups`, then return true.
        // Step 2: Return false.
        self.data().has_regexp_groups
    }

    /// <https://urlpattern.spec.whatwg.org/#dom-urlpattern-test>
    #[method(length = 0)]
    fn test(
        &self,
        scope: &Scope<'_>,
        input: Option<HandleValue<'_>>,
        base_url: Option<String>,
    ) -> Result<bool, ExnThrown> {
        let input = input.unwrap_or(HandleValue::undefined());
        // Step 1: Let _result_ be the result of `match` given `this`'s `associated URL
        //         pattern`, _input_, and _baseURL_ if given.
        let result = self.match_(scope, input, base_url)?;
        // Step 2: If _result_ is null, return false.
        // Step 3: Return true.
        Ok(result.is_some())
    }

    /// <https://urlpattern.spec.whatwg.org/#dom-urlpattern-exec>
    #[method(length = 0)]
    fn exec<'r>(
        &self,
        scope: &'r Scope<'_>,
        input: Option<HandleValue<'_>>,
        base_url: Option<String>,
    ) -> Result<Option<Object<'r>>, ExnThrown> {
        let input = input.unwrap_or(HandleValue::undefined());
        // Step 1: Return the result of `match` given `this`'s `associated URL pattern`, _input_,
        //         and _baseURL_ if given.
        self.match_(scope, input, base_url)
    }

    /// <https://urlpattern.spec.whatwg.org/#url-pattern-match>, which returns the
    /// `URLPatternResult` as an object.
    fn match_<'r>(
        &self,
        scope: &'r Scope<'_>,
        input: HandleValue<'_>,
        base_url_string: Option<String>,
    ) -> Result<Option<Object<'r>>, ExnThrown> {
        let input = URLPatternInput::from_value(scope, input)?;
        // Step 1: Let _protocol_ be the empty string.
        // Step 2: Let _username_ be the empty string.
        // Step 3: Let _password_ be the empty string.
        // Step 4: Let _hostname_ be the empty string.
        // Step 5: Let _port_ be the empty string.
        // Step 6: Let _pathname_ be the empty string.
        // Step 7: Let _search_ be the empty string.
        // Step 8: Let _hash_ be the empty string.
        // Step 9: Let _inputs_ be an empty list.
        // Step 10: If _input_ is a URL, then append the serialization of _input_ to _inputs_.
        // Step 11: Otherwise, append _input_ to _inputs_.
        let inputs = js::Array::new(scope, 0)?;
        match &input {
            URLPatternInput::String(input) => inputs.push(scope, input.as_str())?,
            URLPatternInput::Init(init) => inputs.push(scope, init_object(scope, init)?)?,
        }
        // Step 12: If _input_ is a ``URLPatternInit`` then:
        // Step 12.1: If _baseURLString_ was given, throw a ``TypeError``.
        // Step 12.2: Let _applyResult_ be the result of `process a URLPatternInit` given _input_,
        //            "url", _protocol_, _username_, _password_, _hostname_, _port_, _pathname_,
        //            _search_, and _hash_. If this throws an exception, catch it, and return
        //            null.
        // Step 12.3-12.10: Set each component to _applyResult_'s.
        // Step 13: Otherwise:
        // Step 13.1: Let _url_ be _input_.
        // Step 13.2: If _input_ is a ``USVString``:
        // Step 13.2.1: Let _baseURL_ be null.
        // Step 13.2.2: If _baseURLString_ was given, then:
        // Step 13.2.2.1: Set _baseURL_ to the result of running the `basic URL parser` on
        //                _baseURLString_.
        // Step 13.2.2.2: If _baseURL_ is failure, return null.
        // Step 13.2.2.3: Append _baseURLString_ to _inputs_.
        // Step 13.2.3: Set _url_ to the result of running the `basic URL parser` on _input_
        //              with _baseURL_.
        // Step 13.2.4: If _url_ is failure, return null.
        // Step 13.3-13.11: Set each component to _url_'s.
        // `urlpattern`'s `process_match_input` and `parse_match_input` run steps 12 and 13.
        let quirks_input = match input {
            URLPatternInput::String(input) => quirks::StringOrInit::String(input.into()),
            URLPatternInput::Init(init) => quirks::StringOrInit::Init(quirks::UrlPatternInit {
                protocol: init.protocol,
                username: init.username,
                password: init.password,
                hostname: init.hostname,
                port: init.port,
                pathname: init.pathname,
                search: init.search,
                hash: init.hash,
                base_url: init.base_url,
            }),
        };
        let processed = quirks::process_match_input(quirks_input, base_url_string.as_deref())
            .map_err(|error| type_error(scope, error))?;
        let Some((match_input, _)) = processed else {
            return Ok(None);
        };
        if let Some(base_url_string) = base_url_string {
            inputs.push(scope, base_url_string.as_str())?;
        }
        let Some(match_input) = quirks::parse_match_input(match_input) else {
            return Ok(None);
        };
        let component_inputs = [
            match_input.protocol,
            match_input.username,
            match_input.password,
            match_input.hostname,
            match_input.port,
            match_input.pathname,
            match_input.search,
            match_input.hash,
        ];
        // Step 14-21: Let _protocolExecResult_ ... _hashExecResult_ be
        //             `RegExpBuiltinExec`(_urlPattern_'s component's `regular expression`, the
        //             component).
        // Step 22: If _protocolExecResult_, _usernameExecResult_, _passwordExecResult_,
        //          _hostnameExecResult_, _portExecResult_, _pathnameExecResult_,
        //          _searchExecResult_, or _hashExecResult_ are null then return null.
        let mut exec_results = Vec::with_capacity(COMPONENTS.len());
        for (index, component_input) in component_inputs.iter().enumerate() {
            let regular_expression = self.data().components[index].regular_expression.get(scope);
            let text: Vec<u16> = component_input.encode_utf16().collect();
            let mut last_index = 0;
            // SAFETY: `text` stays alive and unchanged for the call.
            let exec_result = unsafe {
                regular_expression.execute_no_statics(scope, &text, &mut last_index, false)?
            };
            if !exec_result.is_object() {
                return Ok(None);
            }
            exec_results.push(Object::from_value(scope, *exec_result).map_err(|_| ExnThrown)?);
        }
        // Step 23: Let _result_ be a new ``URLPatternResult``.
        // Step 24: Set _result_["``inputs``"] to _inputs_.
        // Step 25-32: Set _result_'s component members to the result of `creating a component
        //             match result` given _urlPattern_'s component, the component, and its exec
        //             result.
        // Step 33: Return _result_.
        // The dictionary's members are defined in lexicographic order, as WebIDL converts
        // dictionaries.
        let result = Object::new_plain(scope)?;
        let attrs = js::class_spec::JSPROP_ENUMERATE as std::ffi::c_uint;
        for name in [
            c"hash",
            c"hostname",
            c"inputs",
            c"password",
            c"pathname",
            c"port",
            c"protocol",
            c"search",
            c"username",
        ] {
            if name == c"inputs" {
                result.define_property(scope, name, *inputs, attrs)?;
                continue;
            }
            let index = COMPONENTS
                .iter()
                .position(|component| *component == name)
                .expect("a component name");
            let component_result = self.create_a_component_match_result(
                scope,
                index,
                &component_inputs[index],
                &exec_results[index],
            )?;
            result.define_property(scope, name, component_result, attrs)?;
        }
        Ok(Some(result))
    }

    /// <https://urlpattern.spec.whatwg.org/#create-a-component-match-result> for component
    /// `index` of [`COMPONENTS`].
    fn create_a_component_match_result<'r>(
        &self,
        scope: &'r Scope<'_>,
        index: usize,
        input: &str,
        exec_result: &Object<'_>,
    ) -> Result<Object<'r>, ExnThrown> {
        let attrs = js::class_spec::JSPROP_ENUMERATE as std::ffi::c_uint;
        // Step 1: Let _result_ be a new ``URLPatternComponentResult``.
        let result = Object::new_plain(scope)?;
        // Step 2: Set _result_["``input``"] to _input_.
        // Step 3: Let _groups_ be a ``record<USVString, (USVString or undefined)>``.
        let groups = Object::new_plain(scope)?;
        // Step 4: Let _index_ be 1.
        // Step 5: While _index_ is less than or equal to _component_'s `group name list`'s
        //         size:
        // Step 5.1: Let _name_ be _component_'s `group name list`[_index_ − 1].
        // Step 5.2: Let _value_ be `Get`(_execResult_, `ToString`(_index_)).
        // Step 5.3: Set _groups_[_name_] to _value_.
        // Step 5.4: Increment _index_ by 1.
        let names = self.data().components[index].group_name_list.clone();
        for (position, name) in names.iter().enumerate() {
            let value = exec_result.get_element(scope, position as u32 + 1)?;
            let name = JSString::from_str(scope, name)?;
            let id = scope.root_id(js::id::string_to_id(scope, name.handle())?);
            groups.define_value_by_id(scope, id, value, attrs)?;
        }
        // Step 6: Set _result_["``groups``"] to _groups_.
        // Step 7: Return _result_.
        // The dictionary's members are defined in lexicographic order, as WebIDL converts
        // dictionaries.
        result.define_property(scope, c"groups", groups, attrs)?;
        result.define_property(scope, c"input", input, attrs)?;
        Ok(result)
    }
}

/// A new object with the members of `init`, as WebIDL converts a `URLPatternInit` to a JS value:
/// the members that exist, in lexicographic order.
fn init_object<'r>(scope: &'r Scope<'_>, init: &URLPatternInit) -> Result<Object<'r>, ExnThrown> {
    let object = Object::new_plain(scope)?;
    let attrs = js::class_spec::JSPROP_ENUMERATE as std::ffi::c_uint;
    for (name, value) in [
        (c"baseURL", &init.base_url),
        (c"hash", &init.hash),
        (c"hostname", &init.hostname),
        (c"password", &init.password),
        (c"pathname", &init.pathname),
        (c"port", &init.port),
        (c"protocol", &init.protocol),
        (c"search", &init.search),
        (c"username", &init.username),
    ] {
        if let Some(value) = value {
            object.define_property(scope, name, value.as_str(), attrs)?;
        }
    }
    Ok(object)
}

js::instance_local! {
    /// The scope [`EngineRegExp`] uses, while [`with_parse_scope`] runs.
    static PARSE_SCOPE: Cell<*const ()> = const { Cell::new(std::ptr::null()) };
}

/// Run `parse`, a call that parses a pattern with `urlpattern`, with `scope` as the scope of the
/// [`EngineRegExp`]s it creates and runs.
fn with_parse_scope<T>(scope: &Scope<'_>, parse: impl FnOnce() -> T) -> T {
    let previous = PARSE_SCOPE.with(|cell| cell.replace((scope as *const Scope<'_>).cast()));
    let result = parse();
    PARSE_SCOPE.with(|cell| cell.set(previous));
    result
}

/// Run `f` with the scope of the innermost [`with_parse_scope`], or return `None` outside one.
fn in_parse_scope<T>(f: impl FnOnce(&Scope<'_>) -> T) -> Option<T> {
    let scope = PARSE_SCOPE.with(Cell::get);
    if scope.is_null() {
        return None;
    }
    // SAFETY: `with_parse_scope` set the pointer to a scope that outlives its call, which this
    // runs in.
    Some(f(unsafe { &*scope.cast::<Scope<'_>>() }))
}

/// The flags of a component's regular expression for `urlpattern`'s `flags`: "v", and "i" for
/// `ignoreCase`, where `urlpattern` passes "u".
fn engine_flags(flags: &str) -> js::regexp::RegExpFlags {
    js::regexp::flags_from_str(&flags.replace('u', "v"))
}

/// A regular expression of `urlpattern`, which SpiderMonkey checks and runs.
struct EngineRegExp {
    source: String,
    flags: String,
}

impl PatternRegExp for EngineRegExp {
    fn syntax() -> RegexSyntax {
        RegexSyntax::EcmaScript
    }

    fn parse(pattern: &str, flags: &str, _force_eval: bool) -> Result<Self, ()> {
        let valid = in_parse_scope(|scope| {
            let source: Vec<u16> = pattern.encode_utf16().collect();
            match js::RegExp::check_syntax(scope, &source, engine_flags(flags)) {
                Ok(error) => error.is_undefined(),
                Err(ExnThrown) => {
                    js::exception::clear(scope);
                    false
                }
            }
        });
        if valid != Some(true) {
            return Err(());
        }
        Ok(EngineRegExp {
            source: pattern.to_string(),
            flags: flags.to_string(),
        })
    }

    /// Whether `text` matches. `urlpattern` reads only whether a regular expression matches
    /// while it parses a pattern, so the captures are left empty.
    fn matches<'a>(&self, text: &'a str) -> Option<Vec<Option<&'a str>>> {
        let matched = in_parse_scope(|scope| {
            let source: Vec<u16> = self.source.encode_utf16().collect();
            let Ok(regexp) = js::RegExp::from_utf16(scope, &source, engine_flags(&self.flags))
            else {
                js::exception::clear(scope);
                return false;
            };
            let text: Vec<u16> = text.encode_utf16().collect();
            let mut last_index = 0;
            // SAFETY: `text` stays alive and unchanged for the call.
            match unsafe { regexp.execute_no_statics(scope, &text, &mut last_index, true) } {
                Ok(result) => result.is_boolean() && result.to_boolean(),
                Err(ExnThrown) => {
                    js::exception::clear(scope);
                    false
                }
            }
        })?;
        matched.then(Vec::new)
    }

    fn pattern_string(&self) -> &str {
        &self.source
    }
}
