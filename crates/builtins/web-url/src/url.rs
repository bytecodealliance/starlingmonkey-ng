// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://url.spec.whatwg.org/>

use super::url_search_params::URLSearchParams;
use crate::algorithms;
use core_runtime::{webidl_interface, webidl_methods};
use js::class::Ref;
use js::error::{ExnThrown, JSExnType, MemberKind, OperationError, ThrowException};
use js::exception;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;

/// The `TypeError` "Invalid URL" of the member at `site` for the URL `input` and the base URL
/// `base`, whose code is `ERR_INVALID_URL`.
fn invalid_url(
    site: js::error::BindingSite,
    input: String,
    base: Option<String>,
) -> OperationError {
    OperationError {
        site,
        kind: JSExnType::JSEXN_TYPEERR,
        message: "Invalid URL".into(),
        code: "ERR_INVALID_URL",
        args: std::iter::once(input).chain(base).collect(),
    }
}

/// <https://url.spec.whatwg.org/#url-class>
#[webidl_interface]
pub struct URL {
    #[no_trace]
    pub(crate) url: Option<url::Url>,
    pub(crate) query_object: Option<Heap<crate::url_search_params::URLSearchParamsImpl>>,
}

#[webidl_methods]
impl URL<'_> {
    /// <https://url.spec.whatwg.org/#dom-url-url>
    #[constructor]
    fn new(&self, scope: &Scope<'_>, url: String, base: Option<String>) -> Result<(), ExnThrown> {
        // Step 1: Let _parsedURL_ be the result of running the `API URL parser` on _url_ with
        //         _base_, if given.
        // Step 2: If _parsedURL_ is failure, then `throw` a ``TypeError``.
        let parsed_url = algorithms::api_url_parser(&url, base.as_deref()).ok_or_else(|| {
            let site = js::class::constructor_site::<URLImpl>();
            invalid_url(site, url.clone(), base.clone()).throw(scope)
        })?;

        // Step 3: `Initialize` `this` with _parsedURL_.
        self.initialize_with_url_record(scope, parsed_url)
    }

    /// <https://url.spec.whatwg.org/#dom-url-href>
    #[getter]
    fn href(&self) -> Ref<'_, str> {
        // Step 1: Return the serialization of this’s URL.
        Ref::map(self.data(), |data| {
            data.url.as_ref().map(|url| url.as_str()).unwrap_or("")
        })
    }

    /// <https://url.spec.whatwg.org/#dom-url-href>
    #[setter]
    fn set_href(&self, scope: &Scope<'_>, value: String) -> Result<(), ExnThrown> {
        // Step 1: Let _parsedURL_ be the result of running the `basic URL parser` on the given
        //         value.
        // Step 2: If _parsedURL_ is failure, then `throw` a ``TypeError``.
        let parsed_url = algorithms::basic_url_parser(&value, None).ok_or_else(|| {
            let site = js::class::member_site::<URLImpl>("href", MemberKind::Setter);
            invalid_url(site, value.clone(), None).throw(scope)
        })?;

        // Step 3: Set `this`’s `URL` to _parsedURL_.
        self.data_mut().url = Some(parsed_url.clone());

        // Step 4: Empty `this`’s `query object`’s `list`.
        let query_object = self.data().query_object.get(scope);
        if let Some(query_object) = query_object {
            query_object.data_mut().list.clear();

            // Step 5: Let _query_ be `this`’s `URL`’s `query`.
            let query = parsed_url.query();

            // Step 6: If _query_ is non-null, then set `this`’s `query object`’s `list` to the
            //         result of `parsing` _query_.
            if let Some(query) = query {
                query_object.data_mut().list = algorithms::urlencoded_parse(query);
            }
        }

        Ok(())
    }

    /// <https://url.spec.whatwg.org/#dom-url-origin>
    #[getter]
    fn origin(&self) -> String {
        // Step 1: Return the serialization of this’s URL’s origin. [HTML]
        self.data()
            .url
            .as_ref()
            .map(|url| url.origin().ascii_serialization())
            .unwrap_or_default()
    }

    /// <https://url.spec.whatwg.org/#dom-url-protocol>
    #[getter]
    fn protocol(&self) -> Ref<'_, str> {
        // Step 1: Return this’s URL’s scheme, followed by U+003A (:).
        Ref::map(self.data(), |data| {
            data.url.as_ref().map(url::quirks::protocol).unwrap_or("")
        })
    }

    /// <https://url.spec.whatwg.org/#dom-url-protocol>
    #[setter]
    fn set_protocol(&self, value: String) {
        // Step 1: Basic URL parse the given value, followed by U+003A (:), with this’s URL as url
        //         and scheme start state as state override.
        let mut url_data = self.data_mut();
        let Some(url) = url_data.url.as_mut() else {
            return;
        };
        let _ = url::quirks::set_protocol(url, &value);
    }

    /// <https://url.spec.whatwg.org/#dom-url-username>
    #[getter]
    fn username(&self) -> Ref<'_, str> {
        // Step 1: Return this’s URL’s username.
        Ref::map(self.data(), |data| {
            data.url.as_ref().map(|url| url.username()).unwrap_or("")
        })
    }

    /// <https://url.spec.whatwg.org/#dom-url-username>
    #[setter]
    fn set_username(&self, value: String) {
        // Step 1: If `this`’s `URL` `cannot have a username/password/port`, then return.
        let mut url_data = self.data_mut();
        let Some(url) = url_data.url.as_mut() else {
            return;
        };
        if url.cannot_be_a_base() || url.host_str().is_none() {
            return;
        }

        // Step 2: `Set the username` given `this`’s `URL` and the given value.
        let _ = url::quirks::set_username(url, &value);
    }

    /// <https://url.spec.whatwg.org/#dom-url-password>
    #[getter]
    fn password(&self) -> Ref<'_, str> {
        // Step 1: Return this’s URL’s password.
        Ref::map(self.data(), |data| {
            data.url.as_ref().map(url::quirks::password).unwrap_or("")
        })
    }

    /// <https://url.spec.whatwg.org/#dom-url-password>
    #[setter]
    fn set_password(&self, value: String) {
        // Step 1: If `this`’s `URL` `cannot have a username/password/port`, then return.
        let mut url_data = self.data_mut();
        let Some(url) = url_data.url.as_mut() else {
            return;
        };
        if url.cannot_be_a_base() || url.host_str().is_none() {
            return;
        }

        // Step 2: `Set the password` given `this`’s `URL` and the given value.
        let _ = url::quirks::set_password(url, &value);
    }

    /// <https://url.spec.whatwg.org/#dom-url-host>
    #[getter]
    fn host(&self) -> Ref<'_, str> {
        // Step 1: Let _url_ be `this`’s `URL`.
        // Step 2: If _url_’s `host` is null, then return the empty string.
        // Step 3: If _url_’s `port` is null, return _url_’s `host`, `serialized`.
        // Step 4: Return _url_’s `host`, `serialized`, followed by U+003A (:) and _url_’s
        //         `port`, `serialized`.
        Ref::map(self.data(), |data| {
            data.url.as_ref().map(url::quirks::host).unwrap_or("")
        })
    }

    /// <https://url.spec.whatwg.org/#dom-url-host>
    #[setter]
    fn set_host(&self, value: String) {
        // Step 1: If `this`'s `URL` has an `opaque path`, then return.
        let mut url_data = self.data_mut();
        let Some(url) = url_data.url.as_mut() else {
            return;
        };

        // Step 2: `Basic URL parse` the given value with `this`'s `URL` as `_url_` and `host
        //         state` as `_state override_`.
        let _ = url::quirks::set_host(url, &value);
    }

    /// <https://url.spec.whatwg.org/#dom-url-hostname>
    #[getter]
    fn hostname(&self) -> Ref<'_, str> {
        // Step 1: If `this`’s `URL`’s `host` is null, then return the empty string.
        // Step 2: Return `this`’s `URL`’s `host`, `serialized`.
        Ref::map(self.data(), |data| {
            data.url.as_ref().map(url::quirks::hostname).unwrap_or("")
        })
    }

    /// <https://url.spec.whatwg.org/#dom-url-hostname>
    #[setter]
    fn set_hostname(&self, value: String) {
        // Step 1: If `this`’s `URL` has an `opaque path`, then return.
        let mut url_data = self.data_mut();
        let Some(url) = url_data.url.as_mut() else {
            return;
        };

        // Step 2: `Basic URL parse` the given value with `this`’s `URL` as `_url_` and `hostname
        //         state` as `_state override_`.
        let _ = url::quirks::set_hostname(url, &value);
    }

    /// <https://url.spec.whatwg.org/#dom-url-port>
    #[getter]
    fn port(&self) -> Ref<'_, str> {
        // Step 1: If `this`’s `URL`’s `port` is null, then return the empty string.
        // Step 2: Return `this`’s `URL`’s `port`, `serialized`.
        Ref::map(self.data(), |data| {
            data.url.as_ref().map(url::quirks::port).unwrap_or("")
        })
    }

    /// <https://url.spec.whatwg.org/#dom-url-port>
    #[setter]
    fn set_port(&self, value: String) {
        // Step 1: If `this`’s `URL` `cannot have a username/password/port`, then return.
        let mut url_data = self.data_mut();
        let Some(url) = url_data.url.as_mut() else {
            return;
        };

        // Step 2: If the given value is the empty string, then set `this`’s `URL`’s `port` to
        //         null.
        // Step 3: Otherwise, `basic URL parse` the given value with `this`’s `URL` as `_url_` and
        //         `port state` as `_state override_`.
        let _ = url::quirks::set_port(url, &value);
    }

    /// <https://url.spec.whatwg.org/#dom-url-pathname>
    #[getter]
    fn pathname(&self) -> Ref<'_, str> {
        // Step 1: Return the result of URL path serializing this’s URL.
        Ref::map(self.data(), |data| {
            data.url.as_ref().map(|url| url.path()).unwrap_or("")
        })
    }

    /// <https://url.spec.whatwg.org/#dom-url-pathname>
    #[setter]
    fn set_pathname(&self, value: String) {
        // Step 1: If `this`’s `URL` has an `opaque path`, then return.
        let mut url_data = self.data_mut();
        let Some(url) = url_data.url.as_mut() else {
            return;
        };

        // Step 2: `Empty` `this`’s `URL`’s `path`.
        // Step 3: `Basic URL parse` the given value with `this`'s `URL` as `_url_` and `path
        //         start state` as `_state override_`.
        url::quirks::set_pathname(url, &value);
    }

    /// <https://url.spec.whatwg.org/#dom-url-search>
    #[getter]
    fn search(&self) -> Ref<'_, str> {
        // Step 1: If `this`’s `URL`’s `query` is either null or the empty string, then return
        //         the empty string.
        // Step 2: Return U+003F (?), followed by `this`’s `URL`’s `query`.
        Ref::map(self.data(), |data| {
            data.url.as_ref().map(url::quirks::search).unwrap_or("")
        })
    }

    /// <https://url.spec.whatwg.org/#dom-url-search>
    #[setter]
    fn set_search(&self, scope: &Scope<'_>, value: String) -> Result<(), ExnThrown> {
        // Step 1: Let _url_ be `this`’s `URL`.
        // Step 2: If the given value is the empty string, then set _url_’s `query` to null,
        //         `empty` `this`’s `query object`’s `list`, and return.
        // Step 3: Let _input_ be the given value with a single leading U+003F (?) removed, if any.
        // Step 4: Set _url_’s `query` to the empty string.
        // Step 5: `Basic URL parse` _input_ with _url_ as `_url_` and `query state` as `_state
        //         override_`.
        let input = value.strip_prefix('?').unwrap_or(&value);
        // Drop `data_mut()` guard before `self.data()` in Step 6.
        {
            let mut url_data = self.data_mut();
            let Some(url) = url_data.url.as_mut() else {
                return Ok(());
            };
            url::quirks::set_search(url, &value);
        }

        // Step 6: Set `this`’s `query object`’s `list` to the result of `parsing` _input_.
        let query_object = self.data().query_object.get(scope);
        if let Some(query_object) = query_object {
            if value.is_empty() {
                query_object.data_mut().list.clear();
            } else {
                query_object.data_mut().list = algorithms::urlencoded_parse(input);
            }
        }
        Ok(())
    }

    /// <https://url.spec.whatwg.org/#dom-url-searchparams>
    #[getter]
    fn search_params<'r>(&self, scope: &'r Scope<'_>) -> URLSearchParams<'r> {
        // Step 1: Return this’s query object.
        self.data()
            .query_object
            .get(scope)
            .expect("URL query object must be initialized")
    }

    /// <https://url.spec.whatwg.org/#dom-url-hash>
    #[getter]
    fn hash(&self) -> Ref<'_, str> {
        // Step 1: If `this`’s `URL`’s `fragment` is either null or the empty string, then
        //         return the empty string.
        // Step 2: Return U+0023 (#), followed by `this`’s `URL`’s `fragment`.
        Ref::map(self.data(), |data| {
            data.url.as_ref().map(url::quirks::hash).unwrap_or("")
        })
    }

    /// <https://url.spec.whatwg.org/#dom-url-hash>
    #[setter]
    fn set_hash(&self, value: String) {
        // Step 1: If the given value is the empty string, then set `this`’s `URL`’s `fragment`
        //         to null and return.
        // Step 2: Let _input_ be the given value with a single leading U+0023 (#) removed, if any.
        // Step 3: Set `this`’s `URL`’s `fragment` to the empty string.
        let mut url_data = self.data_mut();
        let Some(url) = url_data.url.as_mut() else {
            return;
        };
        url::quirks::set_hash(url, &value);
    }

    /// <https://url.spec.whatwg.org/#dom-url-tojson>
    #[allow(clippy::wrong_self_convention)]
    #[method(name = "toJSON")]
    fn to_json(&self) -> Ref<'_, str> {
        // Step 1: Return the serialization of this’s URL.
        self.href()
    }

    /// <https://url.spec.whatwg.org/#dom-url-href>: `href` is the interface's stringifier.
    #[allow(clippy::wrong_self_convention)]
    #[method(name = "toString")]
    fn to_string(&self) -> Ref<'_, str> {
        // Return the serialization of `this`'s URL.
        self.href()
    }

    /// <https://url.spec.whatwg.org/#dom-url-parse>
    #[static_method]
    fn parse<'r>(scope: &'r Scope<'_>, url: String, base: Option<String>) -> Option<URL<'r>> {
        // Step 1: Let _parsedURL_ be the result of running the `API URL parser` on _url_ with
        //         _base_, if given.
        // Step 2: If _parsedURL_ is failure, then return null.
        // Step 3: Let _url_ be a new ``URL`` object.
        // Step 4: `Initialize` _url_ with _parsedURL_.
        // Step 5: Return _url_.
        // All steps are implemented in URL::new, so we can just call that and return None on failure.
        match URL::new(scope, url, base) {
            Ok(url_object) => Some(url_object),
            Err(_) => {
                exception::clear(scope);
                None
            }
        }
    }

    /// <https://url.spec.whatwg.org/#dom-url-canparse>
    #[static_method]
    fn can_parse(url: String, base: Option<String>) -> bool {
        // Step 1: Let _parsedURL_ be the result of running the `API URL parser` on _url_ with
        //         _base_, if given.
        // Step 2: If _parsedURL_ is failure, then return false.
        // Step 3: Return true.
        algorithms::api_url_parser(&url, base.as_deref()).is_some()
    }
}

impl URL<'_> {
    fn initialize_with_url_record(
        &self,
        scope: &Scope<'_>,
        url_record: url::Url,
    ) -> Result<(), ExnThrown> {
        let query = url_record.query().unwrap_or("");
        let query_list: Vec<(String, String)> = algorithms::urlencoded_parse(query);

        let query_object = js::class::create_instance_with::<
            crate::url_search_params::URLSearchParamsImpl,
        >(scope, |_| crate::url_search_params::URLSearchParamsImpl {
            list: query_list,
            url_object: None,
        })?;

        self.data_mut().url = Some(url_record);
        self.data_mut().query_object = Some(query_object.into());
        query_object.data_mut().url_object = Some((*self).into());
        Ok(())
    }
}
