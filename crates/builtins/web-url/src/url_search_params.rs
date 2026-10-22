// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://url.spec.whatwg.org/>

use crate::url_search_params_iterator::URLSearchParamsIterator;
use core_runtime::{webidl_interface, webidl_methods, webidl_union};
use js::conversion::{Record, ToJSVal};
use js::error::{ExnThrown, TypeError};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::iteration::IterationKind;
use js::prelude::HandleValue;
use js::Callable;

/// WebIDL: `(sequence<sequence<USVString>> or record<USVString, USVString> or USVString)`
/// — the union accepted by the [`URLSearchParams` constructor].
///
/// [`URLSearchParams` constructor]: https://url.spec.whatwg.org/#dom-urlsearchparams-urlsearchparams
#[webidl_union]
pub enum URLSearchParamsInit {
    Pairs(Vec<Vec<String>>),
    Record(Record<String, String>),
    Str(String),
}

/// <https://url.spec.whatwg.org/#interface-urlsearchparams>
#[webidl_interface]
pub struct URLSearchParams {
    pub(crate) list: Vec<(String, String)>,
    pub(crate) url_object: Option<Heap<crate::url::URLImpl>>,
}

#[webidl_methods]
impl URLSearchParams<'_> {
    /// <https://url.spec.whatwg.org/#dom-urlsearchparams-urlsearchparams>
    #[constructor]
    fn new(&self, init: Option<URLSearchParamsInit>) -> Result<(), TypeError> {
        // Step 1 (for a string _init_) is below.
        // Step 2: `Initialize` `this` with _init_.
        // (inlined) <https://url.spec.whatwg.org/#urlsearchparams-initialize>
        let list = match init {
            None => Vec::new(),
            // [inlined] Step 1: If _init_ is a `sequence`, then `for each` _innerSequence_ of
            //                   _init_:
            Some(URLSearchParamsInit::Pairs(pairs)) => {
                let mut list = Vec::with_capacity(pairs.len());
                for pair in pairs {
                    // [inlined] Step 1.1: If _innerSequence_'s `size` is not 2, then `throw` a
                    //                     ``TypeError``.
                    if pair.len() != 2 {
                        return Err(TypeError(
                            "Each URLSearchParams sequence element must contain exactly two items"
                                .into(),
                        ));
                    }
                    // [inlined] Step 1.2: `Append` (_innerSequence_[0], _innerSequence_[1]) to
                    //                     _query_'s `list`.
                    let mut iter = pair.into_iter();
                    let name = iter.next().unwrap();
                    let value = iter.next().unwrap();
                    list.push((name, value));
                }
                list
            }
            // [inlined] Step 2: Otherwise, if _init_ is a `record`, then `for each` _name_ →
            //                   _value_ of _init_, `append` (_name_, _value_) to _query_'s `list`.
            Some(URLSearchParamsInit::Record(record)) => record.into_iter().collect(),
            // [inlined] Step 3: Otherwise:
            // [inlined] Step 3.1: Assert: _init_ is a string.
            Some(URLSearchParamsInit::Str(mut s)) => {
                // Step 1: If _init_ is a string and starts with U+003F (?), then remove
                //         the first code point from _init_.
                if s.starts_with('?') {
                    s.remove(0);
                }
                // [inlined] Step 3.2: Set _query_'s `list` to the result of `parsing` _init_.
                crate::algorithms::urlencoded_parse(&s)
            }
        };

        self.data_mut().list = list;
        Ok(())
    }

    /// <https://url.spec.whatwg.org/#dom-urlsearchparams-size>
    #[getter]
    pub fn size(&self) -> u32 {
        // Step 1: Return this’s list’s size.
        self.data().list.len() as u32
    }

    /// <https://url.spec.whatwg.org/#dom-urlsearchparams-append>
    #[method]
    pub fn append(&self, scope: &Scope<'_>, name: String, value: String) -> Result<(), ExnThrown> {
        // Step 1: `Append` (_name_, _value_) to `this`’s `list`.
        self.data_mut().list.push((name, value));
        // Step 2: `Update` `this`.
        self.update(scope)
    }

    /// <https://url.spec.whatwg.org/#dom-urlsearchparams-delete>
    #[method]
    pub fn delete(
        &self,
        scope: &Scope<'_>,
        name: String,
        value: Option<String>,
    ) -> Result<(), ExnThrown> {
        // Step 1: If _value_ is given, then `remove` all `tuples` whose name is _name_ and value is
        //         _value_ from `this`’s `list`.
        // Step 2: Otherwise, `remove` all `tuples` whose name is _name_ from `this`’s `list`.
        match value {
            Some(value) => {
                self.data_mut().list.retain(|(tuple_name, tuple_value)| {
                    tuple_name != &name || tuple_value != &value
                });
            }
            None => {
                self.data_mut()
                    .list
                    .retain(|(tuple_name, _)| tuple_name != &name);
            }
        }

        // Step 3: `Update` `this`.
        self.update(scope)
    }

    /// <https://url.spec.whatwg.org/#dom-urlsearchparams-get>
    #[method]
    pub fn get(&self, name: String) -> Option<String> {
        // Step 1: Return the value of the first tuple whose name is name in this’s list, if there
        //         is such a tuple; otherwise null.
        self.data()
            .list
            .iter()
            .find(|(tuple_name, _)| tuple_name == &name)
            .map(|(_, value)| value.clone())
    }

    /// <https://url.spec.whatwg.org/#dom-urlsearchparams-getall>
    #[method]
    pub fn get_all(&self, name: String) -> Vec<String> {
        // Step 1: Return the values of all tuples whose name is name in this’s list, in list
        //         order; otherwise the empty sequence.
        self.data()
            .list
            .iter()
            .filter(|(tuple_name, _)| tuple_name == &name)
            .map(|(_, value)| value.clone())
            .collect()
    }

    /// <https://url.spec.whatwg.org/#dom-urlsearchparams-has>
    #[method]
    pub fn has(&self, name: String, value: Option<String>) -> bool {
        // Step 1: If _value_ is given and there is a `tuple` whose name is _name_ and value is
        //         _value_ in `this`’s `list`, then return true.
        // Step 2: If _value_ is not given and there is a `tuple` whose name is _name_ in `this`’s
        //         `list`, then return true.
        // Step 3: Return false.
        let data = self.data();
        match value {
            Some(value) => data
                .list
                .iter()
                .any(|(tuple_name, tuple_value)| tuple_name == &name && tuple_value == &value),
            None => data.list.iter().any(|(tuple_name, _)| tuple_name == &name),
        }
    }

    /// <https://url.spec.whatwg.org/#dom-urlsearchparams-set>
    #[method]
    pub fn set(&self, scope: &Scope<'_>, name: String, value: String) -> Result<(), ExnThrown> {
        // Step 1: If `this`’s `list` `contains` any `tuples` whose name is _name_, then set the
        //         value of the first such `tuple` to _value_ and `remove` the others.
        // Step 2: Otherwise, `append` (_name_, _value_) to `this`’s `list`.
        let first_index = self
            .data()
            .list
            .iter()
            .position(|(tuple_name, _)| tuple_name == &name);
        if let Some(first_index) = first_index {
            self.data_mut().list[first_index].1 = value.clone();
            let mut seen_first = false;
            self.data_mut().list.retain(|(tuple_name, _)| {
                if tuple_name != &name {
                    return true;
                }
                if !seen_first {
                    seen_first = true;
                    true
                } else {
                    false
                }
            });
        } else {
            self.data_mut().list.push((name, value));
        }

        // Step 3: `Update` `this`.
        self.update(scope)
    }

    /// <https://url.spec.whatwg.org/#dom-urlsearchparams-sort>
    #[method]
    pub fn sort(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        // Step 1: Set `this`’s `list` to the result of `sorting in ascending order` `this`’s
        //         `list`, with _a_ being less than _b_ if _a_’s name is `code unit less than`
        //         _b_’s name.
        self.data_mut().list.sort_by(|(a_name, _), (b_name, _)| {
            if a_name.is_ascii() {
                a_name.cmp(b_name)
            } else {
                a_name.encode_utf16().cmp(b_name.encode_utf16())
            }
        });

        // Step 2: `Update` `this`.
        self.update(scope)
    }

    /// <https://webidl.spec.whatwg.org/#js-iterable>: `forEach`.
    #[method]
    fn for_each(
        &self,
        scope: &Scope<'_>,
        callback: Callable<'_>,
        this_arg: Option<HandleValue>,
    ) -> Result<(), ExnThrown> {
        let this_arg = this_arg.unwrap_or(HandleValue::undefined());
        js::iteration::for_each_pair(scope, *self, callback, this_arg, |i| {
            let data = self.data();
            let Some((name, value)) = data.list.get(i) else {
                return Ok(None);
            };
            let name = name.as_str().to_jsval_throwing(scope)?;
            let value = value.as_str().to_jsval_throwing(scope)?;
            Ok(Some((name, value)))
        })
    }

    /// <https://url.spec.whatwg.org/#urlsearchparams-stringification-behavior>
    #[allow(clippy::wrong_self_convention)]
    #[method]
    pub fn to_string(&self) -> String {
        // Return the `serialization` of `this`'s `list`.
        crate::algorithms::urlencoded_serialize(&self.data().list)
    }

    /// <https://webidl.spec.whatwg.org/#js-iterable>: `entries`.
    #[method]
    fn entries<'r>(&self, scope: &'r Scope<'_>) -> Result<URLSearchParamsIterator<'r>, ExnThrown> {
        // Return a newly created `default iterator object` for _definition_, with _jsValue_ as
        // its `target`, "`key+value`" as its `kind`, and `index` set to 0.
        URLSearchParamsIterator::new(scope, *self, IterationKind::KeyValue)
    }

    /// <https://webidl.spec.whatwg.org/#js-iterable>: `keys`.
    #[method]
    fn keys<'r>(&self, scope: &'r Scope<'_>) -> Result<URLSearchParamsIterator<'r>, ExnThrown> {
        // Return a newly created `default iterator object` for _definition_, with _jsValue_ as
        // its `target`, "`key`" as its `kind`, and `index` set to 0.
        URLSearchParamsIterator::new(scope, *self, IterationKind::Key)
    }

    /// <https://webidl.spec.whatwg.org/#js-iterable>: `values`.
    #[method]
    fn values<'r>(&self, scope: &'r Scope<'_>) -> Result<URLSearchParamsIterator<'r>, ExnThrown> {
        // Return a newly created `default iterator object` for _definition_, with _jsValue_ as
        // its `target`, "`value`" as its `kind`, and `index` set to 0.
        URLSearchParamsIterator::new(scope, *self, IterationKind::Value)
    }
}

impl URLSearchParams<'_> {
    /// <https://url.spec.whatwg.org/#concept-urlsearchparams-update>
    pub(crate) fn update(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        let data = self.data();
        // Step 1: If _query_'s `URL object` is null, then return.
        let Some(url_object_heap) = data.url_object.as_ref() else {
            return Ok(());
        };

        let url_object = url_object_heap.get(scope);

        // Step 2: Let _serializedQuery_ be the `serialization` of _query_'s `list`.
        let serialized = crate::algorithms::urlencoded_serialize(&data.list);
        // Step 3: If _serializedQuery_ is the empty string, then set _serializedQuery_ to null.
        let serialized_query = if serialized.is_empty() {
            None
        } else {
            Some(serialized)
        };

        // Step 4: Set _query_'s `URL object`'s `URL`'s `query` to _serializedQuery_.
        if let Some(url) = url_object.data_mut().url.as_mut() {
            // Per the URL spec's opaque-path state, a trailing U+0020 SPACE before a query
            // delimiter is stored literally, but must be percent-encoded as %20 when the query
            // is removed and that space becomes a true trailing character.
            if serialized_query.is_none() && url.cannot_be_a_base() {
                let path = url.path().to_string();
                if path.ends_with(' ') {
                    let trimmed = path.trim_end_matches(' ');
                    let trailing_spaces = path.len() - trimmed.len();
                    let rebuilt = format!("{}{}%20", trimmed, " ".repeat(trailing_spaces - 1));
                    url.set_path(&rebuilt);
                }
            }
            url.set_query(serialized_query.as_deref());
        }
        Ok(())
    }
}
