// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://fetch.spec.whatwg.org/>

use std::cell::OnceCell;
use std::ops::{Deref, DerefMut};

use core_runtime::{webidl_interface, webidl_methods, webidl_union};
use js::conversion::{Record, ToJSVal};
use js::error::{throw_type_error, ExnThrown};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::iteration::{create_iter_result, pair_iterator_result, IterationKind};
use js::prelude::HandleValue;
use js::{Callable, JSString, Object};

use crate::algorithms;
use crate::algorithms::sort_and_combine_a_header_list;
use crate::byte_string::ByteString;

/// A header list: an ordered list of (name, value) pairs.
///
/// <https://fetch.spec.whatwg.org/#concept-header-list>
///
/// Header names are ASCII tokens, so byte-case-insensitive comparison is ASCII
/// case-insensitive.
///
/// Values are stored as the strings the WebIDL `ByteString` conversion produced,
/// whose code units are all ≤ 0xFF, each standing for one byte. The conversion
/// to and from wire bytes is `platform::http::isomorphic_encode`/`_decode`, so
/// `é` (U+00E9) is the single byte 0xE9 — not its UTF-8 encoding.
// TODO: consider interning all header names, along with lower-cased versions of them.
// Entries would then become `(interned(name), interned(lower-cased(name)), value)`.
// This should ideally support returning `&'static str` references to the names.
// If that's too much of a DoS vector, Cow might work instead, with the most common
// names being interned.
pub(crate) type HeaderList = Vec<(String, String)>;

/// A `Headers` object's header list together with the result of `sort and combine` on it,
/// computed on first use and dropped by every mutable access to the list.
#[derive(Default)]
pub(crate) struct CachedHeaderList {
    list: HeaderList,
    sorted_and_combined: OnceCell<HeaderList>,
}

impl CachedHeaderList {
    /// <https://fetch.spec.whatwg.org/#concept-header-list-sort-and-combine> of the list.
    pub(crate) fn sorted_and_combined(&self) -> &HeaderList {
        self.sorted_and_combined
            .get_or_init(|| sort_and_combine_a_header_list(&self.list))
    }
}

impl From<HeaderList> for CachedHeaderList {
    fn from(list: HeaderList) -> Self {
        Self {
            list,
            sorted_and_combined: OnceCell::new(),
        }
    }
}

impl Deref for CachedHeaderList {
    type Target = HeaderList;
    fn deref(&self) -> &HeaderList {
        &self.list
    }
}

impl DerefMut for CachedHeaderList {
    fn deref_mut(&mut self) -> &mut HeaderList {
        self.sorted_and_combined.take();
        &mut self.list
    }
}

/// A headers guard.
///
/// <https://fetch.spec.whatwg.org/#concept-headers-guard>
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Guard {
    /// `"none"`
    #[default]
    None,
    /// `"immutable"`
    Immutable,
    /// `"request"`
    Request,
    /// `"request-no-cors"`
    RequestNoCors,
    /// `"response"`
    Response,
}

/// WebIDL `typedef (sequence<sequence<ByteString>> or record<ByteString, ByteString>) HeadersInit`
#[webidl_union]
pub enum HeadersInit {
    Sequence(Vec<Vec<ByteString>>),
    Record(Record<ByteString, ByteString>),
}

/// <https://fetch.spec.whatwg.org/#headers-class>
#[webidl_interface]
pub struct Headers {
    /// <https://fetch.spec.whatwg.org/#concept-headers-header-list>
    /// (a header list), which is initially empty. The spec allows this to be a pointer to
    /// another object's header list (e.g. a request's); here the `Headers` object owns its
    /// list and `Request`/`Response` reference the `Headers` object.
    #[no_trace]
    pub(crate) header_list: CachedHeaderList,
    /// <https://fetch.spec.whatwg.org/#concept-headers-guard>
    /// which is a headers guard. A headers guard is "immutable", "request", "request-no-cors",
    /// "response" or "none".
    #[no_trace]
    pub(crate) guard: Guard,
}

#[webidl_methods]
impl Headers {
    /// <https://fetch.spec.whatwg.org/#dom-headers>
    #[constructor]
    fn new(&self, scope: &Scope<'_>, init: Option<HeadersInit>) -> Result<(), ExnThrown> {
        // Step 1: Set `this`’s `guard` to "`none`".
        self.data_mut().guard = Guard::None;
        // Step 2: If _init_ is given, then `fill` `this` with _init_.
        if let Some(init) = init {
            algorithms::fill_headers(scope, self, init)?;
        }
        Ok(())
    }

    /// Create a `Headers` instance from a header list and guard.
    pub fn from_list(list: HeaderList, guard: Guard) -> Self {
        Self {
            header_list: list.into(),
            guard,
        }
    }

    /// The header list: every header in the order it was added, duplicates included.
    pub fn list(&self) -> Vec<(String, String)> {
        self.data().header_list.to_vec()
    }

    /// <https://fetch.spec.whatwg.org/#dom-headers-append>
    #[method]
    pub fn append(
        &self,
        scope: &Scope<'_>,
        name: ByteString,
        value: ByteString,
    ) -> Result<(), ExnThrown> {
        // Step 1: Append (name, value) to this.
        algorithms::append_to_headers(scope, self, name.into_string(), value.into_string())
    }

    /// <https://fetch.spec.whatwg.org/#dom-headers-delete>
    #[method]
    pub fn delete(&self, scope: &Scope<'_>, name: ByteString) -> Result<(), ExnThrown> {
        let name = name.as_str();
        // Step 1: If `validating` (_name_, `) for `this` returns false, then return. Passing a
        //     dummy `header value` ought not to have any negative repercussions.
        if !algorithms::validate_header_name(scope, self, name, "")? {
            return Ok(());
        }
        // Step 2: If `this`’s `guard` is "`request-no-cors`", _name_ is not a `no-CORS-safelisted
        //     request-header name`, and _name_ is not a `privileged no-CORS request-header name`,
        //     then return.
        if self.data().guard == Guard::RequestNoCors
            && !algorithms::is_no_cors_safelisted_request_header_name(name)
            && !algorithms::is_privileged_no_cors_request_header_name(name)
        {
            return Ok(());
        }

        // Step 3: If `this`’s `header list` `does not contain` _name_, then return.
        if !algorithms::contains(&self.data().header_list, name) {
            return Ok(());
        }

        // Step 4: `Delete` _name_ from `this`’s `header list`.
        algorithms::delete_header(&mut self.data_mut().header_list, name);

        // Step 5: If `this`’s `guard` is "`request-no-cors`", then `remove privileged no-CORS
        //     request-headers` from `this`.
        // Note: at first glance, it seems very random for no-CORS headers to be removed here.
        // The rationale is that they persist as long as content doesn't apply *any* modifications
        // to the `Headers` object.
        // That also means that the early return in step 3 can't be omitted, because while the
        // check happens implicitly in step 4, the early return doesn't.
        if self.data().guard == Guard::RequestNoCors {
            algorithms::remove_privileged_no_cors_request_headers(self);
        }
        Ok(())
    }

    /// <https://fetch.spec.whatwg.org/#dom-headers-get>
    #[method]
    pub fn get<'r>(
        &self,
        scope: &'r Scope<'_>,
        name: ByteString,
    ) -> Result<Option<JSString<'r>>, ExnThrown> {
        let name = name.as_str();
        // Step 1: If _name_ is not a `header name`, then `throw` a `TypeError`.
        if !algorithms::is_header_name(name) {
            return Err(throw_type_error(scope, c"Invalid header name"));
        }
        // Step 2: Return the result of `getting` _name_ from `this`’s `header list`.
        let data = self.data();
        algorithms::get_header_for_name(&data.header_list, name)
            .map(|value| JSString::from_str(scope, &value))
            .transpose()
    }

    /// <https://fetch.spec.whatwg.org/#dom-headers-getsetcookie>
    #[method]
    pub fn get_set_cookie(&self) -> Vec<String> {
        // Step 1: If `this`’s `header list` `does not contain` `Set-Cookie`, then return « ».
        // Step 2: Return the `values` of all `headers` in `this`’s `header list` whose `name` is a
        //     `byte-case-insensitive` match for `Set-Cookie`, in order.
        // An empty header list yields an empty sequence, subsuming step 1.
        self.data()
            .header_list
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, value)| value.clone())
            .collect()
    }

    /// <https://fetch.spec.whatwg.org/#dom-headers-has>
    #[method]
    pub fn has(&self, scope: &Scope<'_>, name: ByteString) -> Result<bool, ExnThrown> {
        let name = name.as_str();
        // Step 1: If _name_ is not a `header name`, then `throw` a `TypeError`.
        if !algorithms::is_header_name(name) {
            return Err(throw_type_error(scope, c"Invalid header name"));
        }
        // Step 2: Return true if `this`’s `header list` `contains` _name_; otherwise false.
        Ok(algorithms::contains(&self.data().header_list, name))
    }

    /// <https://fetch.spec.whatwg.org/#dom-headers-set>
    #[method]
    pub fn set(
        &self,
        scope: &Scope<'_>,
        name: ByteString,
        value: ByteString,
    ) -> Result<(), ExnThrown> {
        let name = name.into_string();
        // Step 1: `Normalize` _value_.
        let value = algorithms::normalize_byte_sequence(value.into_string());
        // Step 2: If `validating` (_name_, _value_) for `this` returns false, then return.
        if !algorithms::validate_header_name(scope, self, &name, &value)? {
            return Ok(());
        }
        // Step 3: If `this`’s `guard` is "`request-no-cors`" and (_name_, _value_) is not a
        //     `no-CORS-safelisted request-header`, then return.
        if self.data().guard == Guard::RequestNoCors
            && !algorithms::is_no_cors_safelisted_request_header(&name, &value)
        {
            return Ok(());
        }
        // Step 4: `Set` (_name_, _value_) in `this`’s `header list`.
        algorithms::set_a_header(&mut self.data_mut().header_list, name, value);
        // Step 5: If `this`’s `guard` is "`request-no-cors`", then `remove privileged no-CORS
        //     request-headers` from `this`.
        if self.data().guard == Guard::RequestNoCors {
            algorithms::remove_privileged_no_cors_request_headers(self);
        }
        Ok(())
    }

    /// <https://webidl.spec.whatwg.org/#js-iterable>: `entries`. The value pairs to iterate over
    /// are the result of `sort and combine` of this's header list.
    #[method]
    fn entries<'r>(&self, scope: &'r Scope<'_>) -> Result<HeadersIterator<'r>, ExnThrown> {
        // Return a newly created `default iterator object` for _definition_, with _jsValue_ as
        // its `target`, "`key+value`" as its `kind`, and `index` set to 0.
        HeadersIterator::new(scope, *self, IterationKind::KeyValue)
    }

    /// <https://webidl.spec.whatwg.org/#js-iterable>: `keys`.
    #[method]
    fn keys<'r>(&self, scope: &'r Scope<'_>) -> Result<HeadersIterator<'r>, ExnThrown> {
        // Return a newly created `default iterator object` for _definition_, with _jsValue_ as
        // its `target`, "`key`" as its `kind`, and `index` set to 0.
        HeadersIterator::new(scope, *self, IterationKind::Key)
    }

    /// <https://webidl.spec.whatwg.org/#js-iterable>: `values`.
    #[method]
    fn values<'r>(&self, scope: &'r Scope<'_>) -> Result<HeadersIterator<'r>, ExnThrown> {
        // Return a newly created `default iterator object` for _definition_, with _jsValue_ as
        // its `target`, "`value`" as its `kind`, and `index` set to 0.
        HeadersIterator::new(scope, *self, IterationKind::Value)
    }

    /// <https://webidl.spec.whatwg.org/#js-iterable>: `forEach`.
    #[method(name = "forEach")]
    fn for_each(
        &self,
        scope: &Scope<'_>,
        callback: Callable<'_>,
        this_arg: Option<HandleValue>,
    ) -> Result<(), ExnThrown> {
        let this_arg = this_arg.unwrap_or(HandleValue::undefined());
        js::iteration::for_each_pair(scope, *self, callback, this_arg, |i| {
            let data = self.data();
            let Some((name, value)) = data.header_list.sorted_and_combined().get(i) else {
                return Ok(None);
            };
            let name = name.as_str().to_jsval_throwing(scope)?;
            let value = value.as_str().to_jsval_throwing(scope)?;
            Ok(Some((name, value)))
        })
    }
}

impl Headers<'_> {
    /// Define `Symbol.iterator` on `Headers.prototype` (an alias of `entries`).
    fn install_symbol_iterator(scope: &Scope<'_>) {
        js::class::add_symbol_alias::<HeadersImpl>(
            scope,
            c"entries",
            js::native::SymbolCode::iterator,
        );
    }
}

/// <https://webidl.spec.whatwg.org/#dfn-default-iterator-object>
#[webidl_interface(hidden, name = "Headers Iterator")]
pub struct HeadersIterator {
    /// The `Headers` object being iterated.
    pub(crate) headers: Heap<HeadersImpl>,
    /// Current position in the sorted-and-combined list.
    pub(crate) index: usize,
    #[no_trace]
    pub(crate) kind: IterationKind,
}

#[webidl_methods]
impl HeadersIterator {
    fn new(headers: Headers, kind: IterationKind) -> Self {
        Self {
            headers: Heap::from(headers),
            index: 0,
            kind,
        }
    }

    /// <https://webidl.spec.whatwg.org/#es-iterator-prototype-next>
    #[method]
    fn next<'a>(&self, scope: &'a Scope<'a>) -> Result<Object<'a>, ExnThrown> {
        // Steps 1-5: Implemented in `#[method]`.
        // Step 6: Let _index_ be _object_'s `index`.
        let index = self.data().index;
        // Step 7: Let _kind_ be _object_'s `kind`.
        let kind = self.data().kind;
        // Step 8: Let _values_ be _object_'s `target`'s `value pairs to iterate over`.
        let headers = self.data().headers.get(scope);
        let headers_data = headers.data();
        let values = headers_data.header_list.sorted_and_combined();
        // Step 9: Let _len_ be the length of _values_.
        // Step 10: If _index_ is greater than or equal to _len_, then return
        //          CreateIteratorResultObject(undefined, true).
        // Step 11: Let _pair_ be the entry in _values_ at index _index_.
        let Some((name, value)) = values.get(index) else {
            return create_iter_result(scope, HandleValue::undefined(), true);
        };
        // Step 12: Set _object_'s index to _index_ + 1.
        self.data_mut().index = index + 1;
        // Step 13: Return the `iterator result` for _pair_ and _kind_.
        pair_iterator_result(scope, name.as_str(), value.as_str(), kind)
    }
}

pub(crate) fn add_to_global(scope: &Scope, global: Object) {
    Headers::add_to_global(scope, global);
    Headers::install_symbol_iterator(scope);
    HeadersIterator::add_to_global(scope, global);
    js::class::inherit_from_iterator_prototype::<HeadersIteratorImpl>(scope)
        .expect("setting a class prototype's prototype can only fail due to OOM");
}
