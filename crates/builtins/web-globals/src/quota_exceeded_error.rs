// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `QuotaExceededError`, a `DOMException` subclass.
//! <https://webidl.spec.whatwg.org/#quotaexceedederror>

use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods};
use js::conversion::Finite;
use js::error::RangeError;

use crate::dom_exception::{DOMException, DOMExceptionImpl};

/// <https://webidl.spec.whatwg.org/#quotaexceedederror>
#[webidl_interface(extends = DOMException)]
pub struct QuotaExceededError {
    parent: DOMExceptionImpl,
    /// <https://webidl.spec.whatwg.org/#dom-quotaexceedederror-quota>
    quota: Option<f64>,
    /// <https://webidl.spec.whatwg.org/#dom-quotaexceedederror-requested>
    requested: Option<f64>,
}

/// <https://webidl.spec.whatwg.org/#dictdef-quotaexceedederroroptions>
///
/// Both members are restricted `double`s, so NaN and ±Infinity are `TypeError`s during conversion.
#[derive(Default)]
#[webidl_dictionary]
pub struct QuotaExceededErrorOptions {
    pub quota: Option<Finite<f64>>,
    pub requested: Option<Finite<f64>>,
}

#[webidl_methods]
impl QuotaExceededError {
    /// <https://webidl.spec.whatwg.org/#dom-quotaexceedederror-quotaexceedederror>
    ///
    /// `constructor(optional DOMString message = "", optional QuotaExceededErrorOptions options = {})`
    #[constructor]
    fn new(
        message: Option<String>,
        options: Option<QuotaExceededErrorOptions>,
    ) -> Result<Self, RangeError> {
        let options = options.unwrap_or_default();
        Self::create(
            message.unwrap_or_default(),
            options.quota,
            options.requested,
        )
    }

    /// <https://webidl.spec.whatwg.org/#dom-quotaexceedederror-quota>
    #[getter]
    fn quota(&self) -> Option<f64> {
        // Step 1: Return `this`'s `quota`.
        self.data().quota
    }

    /// <https://webidl.spec.whatwg.org/#dom-quotaexceedederror-requested>
    #[getter]
    fn requested(&self) -> Option<f64> {
        // Step 1: Return `this`'s `requested`.
        self.data().requested
    }
}

impl QuotaExceededErrorImpl {
    /// Run the constructor steps: validate _quota_ and _requested_, then build the object's
    /// state, whose `DOMException` name is "QuotaExceededError".
    ///
    /// TODO: Rust code can't yet throw a `QuotaExceededError` with a quota or requested. The only
    /// way to throw one is `DOMExceptionError::new("QuotaExceededError", ..)`, which goes through
    /// `throw_dom_exception`, and that always passes `None` for both. That's right for
    /// `getRandomValues`, whose spec gives no values. When a spec says to "throw a
    /// `QuotaExceededError` whose quota is _q_ and requested is _r_" (storage APIs, for example),
    /// add an error type such as `QuotaExceededErrorError { message, quota, requested }` that
    /// implements `ThrowException`: call `create_instance_with::<QuotaExceededErrorImpl>` with
    /// `create(message, quota, requested)` and set the result as the pending exception, as
    /// `throw_dom_exception` does. WebIDL forbids specs from passing a _requested_ that is less
    /// than _quota_, so that type can `expect` that `create` succeeds.
    pub fn create(
        message: String,
        quota: Option<Finite<f64>>,
        requested: Option<Finite<f64>>,
    ) -> Result<Self, RangeError> {
        let (quota, requested) = (quota.map(Finite::get), requested.map(Finite::get));
        // Step 1: Set `this`'s `name` to "QuotaExceededError".
        // Step 2: Set `this`'s `message` to _message_.
        //         (Both are the `DOMException` state built at the end. Throwing before it is
        //         built is not observable.)
        // Step 3: If _options_["``quota``"] is present:
        // Step 3.1: If _options_["``quota``"] is less than 0, then `throw` a ``RangeError``.
        if quota.is_some_and(|q| q < 0.0) {
            return Err(RangeError("quota must not be negative".into()));
        }
        // Step 3.2: Set `this`'s `quota` to _options_["``quota``"].
        // Step 4: If _options_["``requested``"] is present:
        // Step 4.1: If _options_["``requested``"] is less than 0, then `throw` a ``RangeError``.
        if requested.is_some_and(|r| r < 0.0) {
            return Err(RangeError("requested must not be negative".into()));
        }
        // Step 4.2: Set `this`'s `requested` to _options_["``requested``"].
        // Step 5: If `this`'s `quota` is not null, `this`'s `requested` is not null, and
        //         `this`'s `requested` is less than `this`'s `quota`, then `throw` a
        //         ``RangeError``.
        if let (Some(quota), Some(requested)) = (quota, requested) {
            if requested < quota {
                return Err(RangeError("requested must not be less than quota".into()));
            }
        }
        Ok(Self {
            parent: DOMExceptionImpl::new(message, "QuotaExceededError".into()),
            quota,
            requested,
        })
    }
}
