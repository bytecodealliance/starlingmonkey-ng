// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://html.spec.whatwg.org/multipage/workers.html#worker-locations>
//!
//! `WorkerLocation` exposes the runtime's current location URL via
//! `globalThis.location`. The HTML spec sources this URL from the surrounding
//! `WorkerGlobalScope`. Starling has no `WorkerGlobalScope`, so the URL set with
//! [`set_init_location`] backs every `WorkerLocation` accessor instead.
//! If no URL has been configured, every accessor throws a `TypeError`.

use std::cell::RefCell;

use core_runtime::{webidl_interface, webidl_methods};
use js::error::TypeError;
use js::gc::scope::Scope;
use js::Object;

js::instance_local! {
    /// The URL backing `globalThis.location`. Set once before the runtime is initialized and read
    /// by every accessor after, so it is scoped to the runtime rather than to the call that set it.
    static LOCATION_URL: RefCell<Option<url::Url>> = const { RefCell::new(None) };
}

/// Configure the URL exposed via `globalThis.location`.
pub fn set_init_location(url: url::Url) {
    LOCATION_URL.with(|cell| *cell.borrow_mut() = Some(url));
}

/// Clear the configured location URL.
///
/// Subsequent accessor calls will throw until [`set_init_location`] is called
/// again. Provided primarily for tests.
pub fn clear_init_location() {
    LOCATION_URL.with(|cell| cell.borrow_mut().take());
}

/// The configured `globalThis.location` URL, if any.
///
/// This is the runtime's API base URL, used to resolve relative URLs — e.g. by
/// the Fetch `Request` constructor.
pub fn current_location_url() -> Option<url::Url> {
    LOCATION_URL.with(|cell| cell.borrow().clone())
}

/// Run `f` against the configured URL, or yield a `TypeError` if no URL is set.
///
/// `field` is the accessor name; it appears in the error message so JS callers
/// can identify which property triggered the error.
fn with_url<T>(field: &str, f: impl FnOnce(&url::Url) -> T) -> Result<T, TypeError> {
    LOCATION_URL.with(|cell| {
        cell.borrow().as_ref().map(f).ok_or_else(|| {
            TypeError(format!(
                "location.{field} can only be used during request handling, \
                 or if an initialization-time location was set using `--init-location`"
            ))
        })
    })
}

/// The `WorkerLocation` interface.
///
/// `Symbol.toStringTag` is `"Location"` rather than the WebIDL default of
/// `"WorkerLocation"`.
///
/// Instantiated as a singleton in [`add_to_global`].
#[webidl_interface(to_string_tag = "Location")]
pub struct WorkerLocation {}

#[webidl_methods]
impl WorkerLocation {
    /// <https://html.spec.whatwg.org/multipage/workers.html#dom-workerlocation-href>
    #[getter]
    fn href(&self) -> Result<String, TypeError> {
        // Step 1: Return this's WorkerGlobalScope object's url, serialized.
        with_url("href", |url| url.as_str().to_string())
    }

    /// <https://html.spec.whatwg.org/multipage/workers.html#dom-workerlocation-origin>
    #[getter]
    fn origin(&self) -> Result<String, TypeError> {
        // Step 1: Return the serialization of this's WorkerGlobalScope object's url's origin.
        with_url("origin", |url| url.origin().ascii_serialization())
    }

    /// <https://html.spec.whatwg.org/multipage/workers.html#dom-workerlocation-protocol>
    #[getter]
    fn protocol(&self) -> Result<String, TypeError> {
        // Step 1: Return this's WorkerGlobalScope object's url's scheme, followed by ":".
        with_url("protocol", |url| url::quirks::protocol(url).to_string())
    }

    /// <https://html.spec.whatwg.org/multipage/workers.html#dom-workerlocation-host>
    #[getter]
    fn host(&self) -> Result<String, TypeError> {
        // Step 1: Let _url_ be `this`'s ``WorkerGlobalScope` object`'s `url`.
        // Step 2: If _url_'s `host` is null, return the empty string.
        // Step 3: If _url_'s `port` is null, return _url_'s `host`, `serialized`.
        // Step 4: Return _url_'s `host`, `serialized`, followed by "`:`" and _url_'s `port`,
        //         `serialized`.
        // `url::quirks::host` performs all four steps in one call.
        with_url("host", |url| url::quirks::host(url).to_string())
    }

    /// <https://html.spec.whatwg.org/multipage/workers.html#dom-workerlocation-hostname>
    #[getter]
    fn hostname(&self) -> Result<String, TypeError> {
        // Step 1: Let _host_ be `this`'s ``WorkerGlobalScope` object`'s `url`'s `host`.
        // Step 2: If _host_ is null, return the empty string.
        // Step 3: Return _host_, `serialized`.
        with_url("hostname", |url| url::quirks::hostname(url).to_string())
    }

    /// <https://html.spec.whatwg.org/multipage/workers.html#dom-workerlocation-port>
    #[getter]
    fn port(&self) -> Result<String, TypeError> {
        // Step 1: Let _port_ be `this`'s ``WorkerGlobalScope` object`'s `url`'s `port`.
        // Step 2: If _port_ is null, return the empty string.
        // Step 3: Return _port_, `serialized`.
        with_url("port", |url| url::quirks::port(url).to_string())
    }

    /// <https://html.spec.whatwg.org/multipage/workers.html#dom-workerlocation-pathname>
    #[getter]
    fn pathname(&self) -> Result<String, TypeError> {
        // Step 1: Return the result of URL path serializing this's WorkerGlobalScope object's url.
        with_url("pathname", |url| url.path().to_string())
    }

    /// <https://html.spec.whatwg.org/multipage/workers.html#dom-workerlocation-search>
    #[getter]
    fn search(&self) -> Result<String, TypeError> {
        // Step 1: Let _query_ be `this`'s ``WorkerGlobalScope` object`'s `url`'s `query`.
        // Step 2: If _query_ is either null or the empty string, return the empty string.
        // Step 3: Return "`?`", followed by _query_.
        with_url("search", |url| url::quirks::search(url).to_string())
    }

    /// <https://html.spec.whatwg.org/multipage/workers.html#dom-workerlocation-hash>
    #[getter]
    fn hash(&self) -> Result<String, TypeError> {
        // Step 1: Let _fragment_ be `this`'s ``WorkerGlobalScope` object`'s `url`'s `fragment`.
        // Step 2: If _fragment_ is either null or the empty string, return the empty string.
        // Step 3: Return "`#`", followed by _fragment_.
        with_url("hash", |url| url::quirks::hash(url).to_string())
    }

    /// IDL `stringifier;` — returns the same string as the `href` getter.
    #[allow(clippy::wrong_self_convention)]
    #[method(name = "toString")]
    fn to_string(&self) -> Result<String, TypeError> {
        self.href()
    }
}

/// Register the `WorkerLocation` class on `global` and install the singleton
/// `location` instance as a property on it.
///
/// The URL backing the singleton's accessors is the one set via
/// [`set_init_location`]; if none has been set when an accessor runs, it throws.
pub fn add_to_global<'s>(scope: &'s Scope<'_>, global: Object<'s>) {
    WorkerLocation::add_to_global(scope, global);

    let location =
        js::class::create_instance_with::<WorkerLocationImpl>(scope, |_| WorkerLocationImpl {})
            .expect("failed to allocate WorkerLocation singleton");

    global
        .set_property(scope, c"location", location)
        .expect("failed to define globalThis.location");
}
