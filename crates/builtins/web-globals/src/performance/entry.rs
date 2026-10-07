// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The `PerformanceEntry` interface of [Performance Timeline], and the `PerformanceMark` and
//! `PerformanceMeasure` interfaces of [User Timing].
//!
//! [Performance Timeline]: https://w3c.github.io/performance-timeline/
//! [User Timing]: https://w3c.github.io/user-timing/

use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods};
use js::conversion::Finite;
use js::error::{ExnThrown, ThrowException};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::object::{PropertyTemplate, TemplateKey};
use js::prelude::HandleValue;
use js::Object;

use crate::dom_exception::throw_dom_exception;

/// The keys of the objects `PerformanceEntry.prototype.toJSON` returns, in order.
pub static ENTRY_JSON_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"name"),
    TemplateKey::Name(c"entryType"),
    TemplateKey::Name(c"startTime"),
    TemplateKey::Name(c"duration"),
]);

/// The keys of the objects the `toJSON` of entries with a `detail` returns, in order.
pub static DETAILED_ENTRY_JSON_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"name"),
    TemplateKey::Name(c"entryType"),
    TemplateKey::Name(c"startTime"),
    TemplateKey::Name(c"duration"),
    TemplateKey::Name(c"detail"),
]);

/// <https://w3c.github.io/performance-timeline/#the-performanceentry-interface>
#[webidl_interface]
pub struct PerformanceEntry {
    #[no_trace]
    name: String,
    #[no_trace]
    entry_type: &'static str,
    #[no_trace]
    start_time: f64,
    #[no_trace]
    duration: f64,
}

impl PerformanceEntryImpl {
    /// The data of an entry named `name` of the type `entry_type`, which starts at `start_time`
    /// and lasts `duration` milliseconds.
    pub fn with_timing(
        name: String,
        entry_type: &'static str,
        start_time: f64,
        duration: f64,
    ) -> Self {
        PerformanceEntryImpl {
            name,
            entry_type,
            start_time,
            duration,
        }
    }

    /// The entry's `name`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The entry's `entryType`.
    pub fn entry_type(&self) -> &'static str {
        self.entry_type
    }

    /// The entry's `startTime`.
    pub fn start_time(&self) -> f64 {
        self.start_time
    }
}

#[webidl_methods]
impl PerformanceEntry {
    /// <https://w3c.github.io/performance-timeline/#dom-performanceentry-name>
    #[getter]
    pub fn name(&self) -> String {
        self.data().name.clone()
    }

    /// <https://w3c.github.io/performance-timeline/#dom-performanceentry-entrytype>
    #[getter]
    pub fn entry_type(&self) -> &'static str {
        self.data().entry_type
    }

    /// <https://w3c.github.io/performance-timeline/#dom-performanceentry-starttime>
    #[getter]
    pub fn start_time(&self) -> f64 {
        self.data().start_time
    }

    /// <https://w3c.github.io/performance-timeline/#dom-performanceentry-duration>
    #[getter]
    pub fn duration(&self) -> f64 {
        self.data().duration
    }

    /// <https://w3c.github.io/performance-timeline/#dom-performanceentry-tojson>: an object with
    /// the entry's `name`, `entryType`, `startTime` and `duration`.
    #[allow(clippy::wrong_self_convention)]
    #[method(name = "toJSON")]
    pub fn to_json<'r>(&self, scope: &'r Scope<'_>) -> Result<Object<'r>, ExnThrown> {
        self.json(scope, None)
    }

    /// The object `toJSON` returns for this entry, with `detail` after the timing if it is
    /// given.
    pub fn json<'r>(
        &self,
        scope: &'r Scope<'_>,
        detail: Option<HandleValue<'r>>,
    ) -> Result<Object<'r>, ExnThrown> {
        let (name, entry_type, start_time, duration) = {
            let data = self.data();
            (
                data.name.clone(),
                data.entry_type,
                data.start_time,
                data.duration,
            )
        };
        let mut values = vec![
            js::conversion::ToJSVal::to_jsval_throwing(&name, scope)?,
            js::conversion::ToJSVal::to_jsval_throwing(&entry_type, scope)?,
            scope.root_value(js::value::from_f64(start_time)),
            scope.root_value(js::value::from_f64(duration)),
        ];
        match detail {
            Some(detail) => {
                values.push(detail);
                Object::new_plain_with_template(scope, &DETAILED_ENTRY_JSON_PROPERTIES, &values)
            }
            None => Object::new_plain_with_template(scope, &ENTRY_JSON_PROPERTIES, &values),
        }
    }
}

/// <https://w3c.github.io/user-timing/#dom-performancemarkoptions>
#[derive(Default)]
#[webidl_dictionary]
pub struct PerformanceMarkOptions<'a> {
    pub detail: Option<HandleValue<'a>>,
    pub start_time: Option<Finite<f64>>,
}

/// `detail` structured-cloned, or `null` if it is absent, `undefined` or `null`, as the
/// `PerformanceMark` constructor and `measure` set an entry's `detail`.
pub(crate) fn clone_detail<'s>(
    scope: &'s Scope<'_>,
    detail: Option<HandleValue<'_>>,
) -> Result<HandleValue<'s>, ExnThrown> {
    match detail.filter(|detail| !detail.is_null_or_undefined()) {
        Some(detail) => crate::messaging::message_port::structured_clone(scope, detail, &[]),
        None => Ok(HandleValue::null()),
    }
}

/// <https://w3c.github.io/user-timing/#performancemark>
#[webidl_interface(extends = PerformanceEntry)]
pub struct PerformanceMark {
    parent: PerformanceEntryImpl,
    detail: Heap<Value>,
}

#[webidl_methods]
impl PerformanceMark {
    /// <https://w3c.github.io/user-timing/#dom-performancemark-constructor>
    #[constructor]
    pub fn create(
        scope: &Scope<'_>,
        mark_name: String,
        mark_options: Option<PerformanceMarkOptions<'_>>,
    ) -> Result<Self, ExnThrown> {
        let options = mark_options.unwrap_or_default();
        // Step 1: If the current global object is a Window object and markName uses the same
        //         name as a read only attribute in the PerformanceTiming interface, throw a
        //         SyntaxError.
        // Note: the names of a registered `TimelineExtension`'s timestamps take the place of
        // `PerformanceTiming`'s attributes.
        if super::reserved_timestamp(scope, &mark_name).is_some() {
            let message = format!("\"{mark_name}\" is a reserved performance mark name");
            return Err(throw_dom_exception(scope, "SyntaxError", &message));
        }
        // Step 2-4: Set entry's name to markName, and its entryType to "mark".
        // Step 5: If markOptions's startTime member is present, then:
        let start_time = match options.start_time {
            // Step 5.1: If markOptions's startTime is negative, throw a TypeError.
            Some(start_time) if start_time.get() < 0.0 => {
                return Err(super::invalid_timestamp(
                    js::class::constructor_site::<PerformanceMarkImpl>(),
                    "The mark's startTime can't be negative",
                    start_time.get(),
                )
                .throw(scope));
            }
            // Step 5.2: Otherwise, set entry's startTime to the value of markOptions's
            //           startTime.
            Some(start_time) => start_time.get(),
            // Step 6: Otherwise, set it to the value that would be returned by the Performance
            //         object's now() method.
            None => super::now(),
        };
        // Step 7: Set entry's duration attribute to 0.
        // Step 8-9: Set entry's detail to a structured clone of markOptions's detail, or to null.
        let detail = clone_detail(scope, options.detail)?;
        Ok(PerformanceMarkImpl {
            parent: PerformanceEntryImpl::with_timing(mark_name, "mark", start_time, 0.0),
            detail: Heap::from(detail.get()),
        })
    }

    /// <https://w3c.github.io/user-timing/#dom-performancemark-detail>
    #[getter]
    pub fn detail<'r>(&self, scope: &'r Scope<'_>) -> HandleValue<'r> {
        self.data().detail.get(scope)
    }

    /// The entry's `name`, `entryType`, `startTime`, `duration` and `detail`, as Node.js's
    /// `PerformanceMark.prototype.toJSON` returns them.
    #[allow(clippy::wrong_self_convention)]
    #[method(name = "toJSON")]
    pub fn to_json<'r>(&self, scope: &'r Scope<'_>) -> Result<Object<'r>, ExnThrown> {
        let detail = self.data().detail.get(scope);
        self.json(scope, Some(detail))
    }
}

/// <https://w3c.github.io/user-timing/#performancemeasure>
#[webidl_interface(extends = PerformanceEntry)]
pub struct PerformanceMeasure {
    parent: PerformanceEntryImpl,
    detail: Heap<Value>,
}

impl PerformanceMeasureImpl {
    /// The data of a measure named `name` from `start_time` to `start_time + duration`.
    pub(crate) fn with_detail(
        name: String,
        start_time: f64,
        duration: f64,
        detail: HandleValue<'_>,
    ) -> Self {
        PerformanceMeasureImpl {
            parent: PerformanceEntryImpl::with_timing(name, "measure", start_time, duration),
            detail: Heap::from(detail.get()),
        }
    }
}

#[webidl_methods]
impl PerformanceMeasure {
    /// <https://w3c.github.io/user-timing/#dom-performancemeasure-detail>
    #[getter]
    pub fn detail<'r>(&self, scope: &'r Scope<'_>) -> HandleValue<'r> {
        self.data().detail.get(scope)
    }

    /// The entry's `name`, `entryType`, `startTime`, `duration` and `detail`, as Node.js's
    /// `PerformanceMeasure.prototype.toJSON` returns them.
    #[allow(clippy::wrong_self_convention)]
    #[method(name = "toJSON")]
    pub fn to_json<'r>(&self, scope: &'r Scope<'_>) -> Result<Object<'r>, ExnThrown> {
        let detail = self.data().detail.get(scope);
        self.json(scope, Some(detail))
    }
}
