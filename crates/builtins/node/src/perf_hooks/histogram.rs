// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The histograms of `node:perf_hooks`: `Histogram`, `RecordableHistogram`, which
//! `createHistogram` creates, and `ELDHistogram`, the `IntervalHistogram` that
//! `monitorEventLoopDelay` creates. See <https://nodejs.org/api/perf_hooks.html#class-histogram>.
//!
//! A histogram's counts and statistics are [`HistogramData`] in its private data. The argument
//! checks are those of Node.js's `lib/internal/histogram.js`.

use std::time::Duration;

use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::object::{PropertyInit, PropertyTemplate, TemplateKey};
use js::prelude::HandleValue;
use js::Object;

use super::hdr::{CreateError, HistogramData};
use crate::errors::{
    number_to_string, out_of_range_received, validate_boolean, validate_integer,
    validate_plain_object, ErrorKind, NodeError, Thrown,
};

/// The largest integer a double represents exactly, `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// The keys of the objects `histogram.toJSON()` returns, in Node.js's order.
pub(crate) static HISTOGRAM_JSON_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"count"),
    TemplateKey::Name(c"min"),
    TemplateKey::Name(c"max"),
    TemplateKey::Name(c"mean"),
    TemplateKey::Name(c"exceeds"),
    TemplateKey::Name(c"stddev"),
    TemplateKey::Name(c"skewness"),
    TemplateKey::Name(c"kurtosis"),
    TemplateKey::Name(c"ewmaMean"),
    TemplateKey::Name(c"ewmaStddev"),
    TemplateKey::Name(c"ewmaErrorRate"),
    TemplateKey::Name(c"percentiles"),
]);

/// The keys of the object a histogram's custom inspection shows, in Node.js's order.
pub(crate) static HISTOGRAM_INSPECT_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"min"),
    TemplateKey::Name(c"max"),
    TemplateKey::Name(c"mean"),
    TemplateKey::Name(c"exceeds"),
    TemplateKey::Name(c"stddev"),
    TemplateKey::Name(c"skewness"),
    TemplateKey::Name(c"kurtosis"),
    TemplateKey::Name(c"count"),
    TemplateKey::Name(c"percentiles"),
]);

/// The keys of the objects `welchTest` returns.
pub(crate) static WELCH_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"tStatistic"),
    TemplateKey::Name(c"degreesOfFreedom"),
    TemplateKey::Name(c"pValue"),
    TemplateKey::Name(c"confidenceInterval"),
]);

/// The keys of the `confidenceInterval` objects `welchTest` returns.
pub(crate) static INTERVAL_PROPERTIES: PropertyTemplate =
    PropertyTemplate::new(&[TemplateKey::Name(c"lower"), TemplateKey::Name(c"upper")]);

/// The keys of the objects `mannWhitneyTest` returns.
pub(crate) static MANN_WHITNEY_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"uStatistic"),
    TemplateKey::Name(c"zScore"),
    TemplateKey::Name(c"pValue"),
]);

/// The keys of the objects `percentileCI` returns.
pub(crate) static PERCENTILE_CI_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"value"),
    TemplateKey::Name(c"lower"),
    TemplateKey::Name(c"upper"),
]);

/// A new object with a `null` prototype and the properties of `template` with `values`.
fn null_prototype_object<'s>(
    scope: &'s Scope<'_>,
    template: &'static PropertyTemplate,
    values: &[HandleValue<'_>],
) -> Result<Object<'s>, ExnThrown> {
    let object = Object::new_with_null_prototype(scope)?;
    // SAFETY: the object was just created, and no JavaScript runs before the writer finishes.
    let mut properties = unsafe { object.property_writer(scope, template, PropertyInit::Define) }?;
    for (index, value) in values.iter().enumerate() {
        properties.write(scope, template_key(template, index), *value)?;
    }
    properties.finish(scope)?;
    Ok(object)
}

/// The key at `index` of `template`, which has at least `index + 1` keys.
fn template_key(template: &'static PropertyTemplate, index: usize) -> TemplateKey {
    template.keys()[index]
}

/// `number` as a JS value.
fn number<'s>(scope: &'s Scope<'_>, number: f64) -> HandleValue<'s> {
    scope.root_value(js::value::from_f64(number))
}

/// `value` as a JS `BigInt`.
fn bigint_i64<'s>(scope: &'s Scope<'_>, value: i64) -> Result<HandleValue<'s>, ExnThrown> {
    Ok(scope.root_value(js::value::from_bigint(js::bigint::from_i64(scope, value)?)))
}

/// Node.js's `validateNumber(value, name)`: `value` as a number, or `ERR_INVALID_ARG_TYPE`.
fn number_arg(scope: &Scope<'_>, value: HandleValue<'_>, name: &str) -> Result<f64, Thrown> {
    if !value.is_number() {
        return Err(NodeError::invalid_arg_type(scope, name, "of type number", value).into());
    }
    Ok(value.to_number())
}

/// `percentile` as a number in `(0, 100]`, or the error Node.js's histograms throw for it.
fn percentile_arg(scope: &Scope<'_>, value: HandleValue<'_>, name: &str) -> Result<f64, Thrown> {
    let percentile = number_arg(scope, value, name)?;
    if percentile.is_nan() || percentile <= 0.0 || percentile > 100.0 {
        return Err(NodeError::out_of_range(
            name,
            "> 0 && <= 100",
            &out_of_range_received(percentile),
        )
        .into());
    }
    Ok(percentile)
}

/// `value` as a number in `(0, 1)`, or `ERR_OUT_OF_RANGE` naming `name`.
fn fraction_arg(scope: &Scope<'_>, value: HandleValue<'_>, name: &str) -> Result<f64, Thrown> {
    let fraction = number_arg(scope, value, name)?;
    if fraction.is_nan() || fraction <= 0.0 || fraction >= 1.0 {
        return Err(
            NodeError::out_of_range(name, "> 0 && < 1", &out_of_range_received(fraction)).into(),
        );
    }
    Ok(fraction)
}

/// The `options.confidence` of `welchTest` and `percentileCI`, 0.95 if `options` or its
/// `confidence` is `undefined`.
fn confidence_option(scope: &Scope<'_>, options: HandleValue<'_>) -> Result<f64, Thrown> {
    if options.is_undefined() {
        return Ok(0.95);
    }
    let options = validate_plain_object(scope, options, "options", false)?;
    let confidence = options.get_property(scope, c"confidence")?;
    if confidence.is_undefined() {
        return Ok(0.95);
    }
    fraction_arg(scope, confidence, "options.confidence")
}

/// `value` as a 64-bit integer of at least 1, as Node.js's native histogram methods take it
/// after the argument checks, or `ERR_OUT_OF_RANGE` with `message`.
fn int64_arg(scope: &Scope<'_>, value: HandleValue<'_>, message: &str) -> Result<i64, Thrown> {
    let out_of_range = || NodeError::new(ErrorKind::RangeError, "ERR_OUT_OF_RANGE", message);
    let value = if value.is_bigint() {
        let bigint = js::bigint::to_bigint(scope, value)?;
        js::bigint::fits_i64(bigint).ok_or_else(out_of_range)?
    } else {
        value.to_number() as i64
    };
    if value < 1 {
        return Err(out_of_range().into());
    }
    Ok(value)
}

/// The histogram `value` is, if it is one.
fn histogram_of<'s>(scope: &'s Scope<'_>, value: HandleValue<'_>) -> Option<Histogram<'s>> {
    Object::from_value(scope, *value)
        .ok()
        .and_then(|object| object.cast::<Histogram>().ok())
}

/// The histogram `value` is, or `ERR_INVALID_ARG_TYPE` naming the argument `other`.
fn other_histogram<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Histogram<'s>, Thrown> {
    histogram_of(scope, value).ok_or_else(|| {
        NodeError::invalid_arg_type(scope, "other", "an instance of Histogram", value).into()
    })
}

/// A new `Map` of `entries`, as numbers.
fn number_map<'s>(
    scope: &'s Scope<'_>,
    entries: impl IntoIterator<Item = (f64, f64)>,
) -> Result<js::Map<'s>, ExnThrown> {
    let map = js::Map::new(scope)?;
    for (key, value) in entries {
        map.insert(scope, number(scope, key), number(scope, value))?;
    }
    Ok(map)
}

/// A histogram of recorded values. See <https://nodejs.org/api/perf_hooks.html#class-histogram>.
#[core_runtime::jsclass(
    name = "Histogram",
    hidden,
    invalid_this = "crate::errors::invalid_this"
)]
pub struct Histogram {
    #[no_trace]
    data: HistogramData,
}

#[core_runtime::jsmethods(node_args)]
impl Histogram {
    /// Always throws `ERR_ILLEGAL_CONSTRUCTOR`, as only `createHistogram` and
    /// `monitorEventLoopDelay` create histograms.
    #[constructor]
    pub fn create() -> Result<Self, Thrown> {
        Err(NodeError::illegal_constructor().into())
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramcount>
    #[getter]
    pub fn count(&self) -> f64 {
        self.data().data.count() as f64
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramcountbigint>
    #[getter]
    pub fn count_big_int<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        let count = self.data().data.count();
        Ok(scope.root_value(js::value::from_bigint(js::bigint::from_u64(scope, count)?)))
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramccdfvalue>: the probability that a recorded value exceeds `value`.
    #[method]
    pub fn ccdf(&self, scope: &Scope<'_>, value: HandleValue<'_>) -> Result<f64, Thrown> {
        let value = number_arg(scope, value, "value")?;
        Ok(1.0 - self.data().data.cdf(value as i64))
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramcdfvalue>: the probability that a recorded value is at most `value`, by
    /// buckets.
    #[method]
    pub fn cdf(&self, scope: &Scope<'_>, value: HandleValue<'_>) -> Result<f64, Thrown> {
        let value = number_arg(scope, value, "value")?;
        Ok(self.data().data.cdf(value as i64))
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramcountatvalue>: the number of recorded values in the bucket of `value`.
    #[method]
    pub fn count_at(&self, scope: &Scope<'_>, value: HandleValue<'_>) -> Result<f64, Thrown> {
        let value = number_arg(scope, value, "value")?;
        Ok(self.data().data.count_at(value as i64) as f64)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogrammin>
    #[getter]
    pub fn min(&self) -> f64 {
        self.data().data.min() as f64
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramminbigint>
    #[getter]
    pub fn min_big_int<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        let min = self.data().data.min();
        bigint_i64(scope, min)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogrammax>
    #[getter]
    pub fn max(&self) -> f64 {
        self.data().data.max() as f64
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogrammaxbigint>
    #[getter]
    pub fn max_big_int<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        let max = self.data().data.max();
        bigint_i64(scope, max)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogrammean>
    #[getter]
    pub fn mean(&self) -> f64 {
        self.data().data.mean()
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramexceeds>
    #[getter]
    pub fn exceeds(&self) -> f64 {
        self.data().data.exceeds() as f64
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramexceedsbigint>
    #[getter]
    pub fn exceeds_big_int<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, ExnThrown> {
        let exceeds = self.data().data.exceeds();
        bigint_i64(scope, exceeds as i64)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramewmamean>
    #[getter]
    pub fn ewma_mean(&self) -> f64 {
        self.data().data.ewma_mean()
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramewmastddev>
    #[getter]
    pub fn ewma_stddev(&self) -> f64 {
        self.data().data.ewma_stddev()
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramewmaerrorrate>
    #[getter]
    pub fn ewma_error_rate(&self) -> f64 {
        self.data().data.ewma_error_rate()
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramburnrateslotarget>
    #[method]
    pub fn burn_rate(&self, scope: &Scope<'_>, slo_target: HandleValue<'_>) -> Result<f64, Thrown> {
        let slo_target = fraction_arg(scope, slo_target, "sloTarget")?;
        Ok(self.data().data.ewma_error_rate() / (1.0 - slo_target))
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramkstestother>
    #[method]
    pub fn ks_test(&self, scope: &Scope<'_>, other: HandleValue<'_>) -> Result<f64, Thrown> {
        let other = other_histogram(scope, other)?;
        if other.as_object().as_raw() == self.as_object().as_raw() {
            return Ok(0.0);
        }
        let result = self.data().data.ks_test(&other.data().data);
        Ok(result)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramwelchtestother-options>
    #[method]
    pub fn welch_test<'r>(
        &self,
        scope: &'r Scope<'_>,
        other: HandleValue<'_>,
        options: HandleValue<'_>,
    ) -> Result<Object<'r>, Thrown> {
        let other = other_histogram(scope, other)?;
        let confidence = confidence_option(scope, options)?;
        let result = if other.as_object().as_raw() == self.as_object().as_raw() {
            [0.0, 0.0, 1.0, 0.0, 0.0]
        } else {
            self.data().data.welch_test(&other.data().data, confidence)
        };
        let interval = null_prototype_object(
            scope,
            &INTERVAL_PROPERTIES,
            &[number(scope, result[3]), number(scope, result[4])],
        )?;
        Ok(null_prototype_object(
            scope,
            &WELCH_PROPERTIES,
            &[
                number(scope, result[0]),
                number(scope, result[1]),
                number(scope, result[2]),
                interval.to_jsval_throwing(scope)?,
            ],
        )?)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogrammannwhitneytestother>
    #[method]
    pub fn mann_whitney_test<'r>(
        &self,
        scope: &'r Scope<'_>,
        other: HandleValue<'_>,
    ) -> Result<Object<'r>, Thrown> {
        let other = other_histogram(scope, other)?;
        let result = if other.as_object().as_raw() == self.as_object().as_raw() {
            [0.0, 0.0, 1.0]
        } else {
            self.data().data.mann_whitney_test(&other.data().data)
        };
        let values = result.map(|value| number(scope, value));
        Ok(null_prototype_object(
            scope,
            &MANN_WHITNEY_PROPERTIES,
            &values,
        )?)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramcohensdother>
    #[method]
    pub fn cohens_d(&self, scope: &Scope<'_>, other: HandleValue<'_>) -> Result<f64, Thrown> {
        let other = other_histogram(scope, other)?;
        if other.as_object().as_raw() == self.as_object().as_raw() {
            return Ok(0.0);
        }
        let result = self.data().data.cohens_d(&other.data().data);
        Ok(result)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramcliffsdother>
    #[method]
    pub fn cliffs_d(&self, scope: &Scope<'_>, other: HandleValue<'_>) -> Result<f64, Thrown> {
        let other = other_histogram(scope, other)?;
        if other.as_object().as_raw() == self.as_object().as_raw() {
            return Ok(0.0);
        }
        let result = self.data().data.cliffs_d(&other.data().data);
        Ok(result)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramkurtosis>
    #[getter]
    pub fn kurtosis(&self) -> f64 {
        self.data().data.kurtosis()
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramlinearbucketsstepsize>
    #[method]
    pub fn linear_buckets<'r>(
        &self,
        scope: &'r Scope<'_>,
        step_size: HandleValue<'_>,
    ) -> Result<js::Map<'r>, Thrown> {
        let step = validate_integer(scope, step_size, "stepSize", 1.0, MAX_SAFE_INTEGER)? as i64;
        let buckets = self.data().data.linear_buckets(step);
        Ok(number_map(
            scope,
            buckets
                .into_iter()
                .map(|(value, count)| (value as f64, count as f64)),
        )?)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramlogbucketsfirstbucket-base>: a level that doesn't grow ends the
    /// buckets, where Node.js's iteration doesn't end.
    #[method]
    pub fn log_buckets<'r>(
        &self,
        scope: &'r Scope<'_>,
        first_bucket: HandleValue<'_>,
        base: HandleValue<'_>,
    ) -> Result<js::Map<'r>, Thrown> {
        let first =
            validate_integer(scope, first_bucket, "firstBucket", 1.0, MAX_SAFE_INTEGER)? as i64;
        let base = number_arg(scope, base, "base")?;
        if base <= 1.0 {
            return Err(
                NodeError::out_of_range("base", "> 1", &out_of_range_received(base)).into(),
            );
        }
        let buckets = self.data().data.log_buckets(first, base);
        Ok(number_map(
            scope,
            buckets
                .into_iter()
                .map(|(value, count)| (value as f64, count as f64)),
        )?)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramskewness>
    #[getter]
    pub fn skewness(&self) -> f64 {
        self.data().data.skewness()
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramstddev>
    #[getter]
    pub fn stddev(&self) -> f64 {
        self.data().data.stddev()
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogrampercentilepercentile>
    #[method]
    pub fn percentile(
        &self,
        scope: &Scope<'_>,
        percentile: HandleValue<'_>,
    ) -> Result<f64, Thrown> {
        let percentile = percentile_arg(scope, percentile, "percentile")?;
        Ok(self.data().data.percentile(percentile) as f64)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogrampercentilebigintpercentile>
    #[method]
    pub fn percentile_big_int<'r>(
        &self,
        scope: &'r Scope<'_>,
        percentile: HandleValue<'_>,
    ) -> Result<HandleValue<'r>, Thrown> {
        let percentile = percentile_arg(scope, percentile, "percentile")?;
        let value = self.data().data.percentile(percentile);
        Ok(bigint_i64(scope, value)?)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogrampercentilecipercentile-options>
    #[method(name = "percentileCI")]
    pub fn percentile_ci<'r>(
        &self,
        scope: &'r Scope<'_>,
        percentile: HandleValue<'_>,
        options: HandleValue<'_>,
    ) -> Result<Object<'r>, Thrown> {
        let percentile = percentile_arg(scope, percentile, "percentile")?;
        let confidence = confidence_option(scope, options)?;
        let result = self.data().data.percentile_ci(percentile, confidence);
        let values = result.map(|value| number(scope, value as f64));
        Ok(null_prototype_object(
            scope,
            &PERCENTILE_CI_PROPERTIES,
            &values,
        )?)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogrampercentiles>
    #[getter]
    pub fn percentiles<'r>(&self, scope: &'r Scope<'_>) -> Result<js::Map<'r>, ExnThrown> {
        let percentiles = self.data().data.percentiles();
        number_map(
            scope,
            percentiles
                .into_iter()
                .map(|(percentile, value)| (percentile, value as f64)),
        )
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogrampercentilesbigint>
    #[getter]
    pub fn percentiles_big_int<'r>(&self, scope: &'r Scope<'_>) -> Result<js::Map<'r>, ExnThrown> {
        let percentiles = self.data().data.percentiles();
        let map = js::Map::new(scope)?;
        for (percentile, value) in percentiles {
            map.insert(scope, number(scope, percentile), bigint_i64(scope, value)?)?;
        }
        Ok(map)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogrampercentilesatpercentiles>
    #[method]
    pub fn percentiles_at<'r>(
        &self,
        scope: &'r Scope<'_>,
        percentiles: HandleValue<'_>,
    ) -> Result<js::Map<'r>, Thrown> {
        let array = Object::from_value(scope, *percentiles)
            .ok()
            .filter(|object| js::Array::is_array(scope, object.handle()).unwrap_or(false))
            .ok_or_else(|| {
                NodeError::invalid_arg_type(
                    scope,
                    "percentiles",
                    "an instance of Array",
                    percentiles,
                )
            })?;
        let length = array
            .cast::<js::Array>()
            .map_or(Ok(0), |array| array.length(scope))? as usize;
        let mut requested = Vec::new();
        requested.try_reserve_exact(length).map_err(|_| {
            js::exception::report_out_of_memory(scope);
            Thrown::Pending
        })?;
        for index in 0..length {
            let value = array.get_element(scope, index as u32)?;
            let name = format!("percentiles[{index}]");
            requested.push(percentile_arg(scope, value, &name)?);
        }
        requested.sort_by(f64::total_cmp);
        let values = self.data().data.percentiles_at(&requested);
        Ok(number_map(
            scope,
            requested
                .into_iter()
                .zip(values)
                .map(|(percentile, value)| (percentile, value as f64)),
        )?)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramreset>
    #[method]
    pub fn reset(&self) {
        self.data_mut().data.reset();
    }

    /// The histogram's statistics, with its percentiles as an object.
    #[allow(clippy::wrong_self_convention)]
    #[method(name = "toJSON")]
    pub fn to_json<'r>(&self, scope: &'r Scope<'_>) -> Result<Object<'r>, ExnThrown> {
        let (stats, percentiles) = {
            let data = &self.data().data;
            let stats = [
                data.count() as f64,
                data.min() as f64,
                data.max() as f64,
                data.mean(),
                data.exceeds() as f64,
                data.stddev(),
                data.skewness(),
                data.kurtosis(),
                data.ewma_mean(),
                data.ewma_stddev(),
                data.ewma_error_rate(),
            ];
            (stats, data.percentiles())
        };
        let percentiles_object = Object::new_plain(scope)?;
        for (percentile, value) in percentiles {
            let key =
                std::ffi::CString::new(number_to_string(percentile)).map_err(|_| ExnThrown)?;
            percentiles_object.set_property_by_name(scope, &key, value as f64)?;
        }
        let mut values: Vec<HandleValue<'r>> =
            stats.iter().map(|value| number(scope, *value)).collect();
        values.push(percentiles_object.to_jsval_throwing(scope)?);
        Object::new_plain_with_template(scope, &HISTOGRAM_JSON_PROPERTIES, &values)
    }
}

/// `histogram[util.inspect.custom](depth, options)`: `this` for a negative `depth`, and
/// otherwise `Histogram` followed by the inspected statistics, as Node.js shows histograms.
fn inspect_histogram(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    let Some(histogram) = histogram_of(scope, this) else {
        return Ok(this.get());
    };
    let depth = args.get(0);
    if depth.is_number() && depth.to_number() < 0.0 {
        return Ok(this.get());
    }
    let mut options = crate::inspect::InspectOptions::current(scope)?;
    if let Ok(given) = Object::from_value(scope, args.get(1)) {
        options = options.read(scope, given)?;
    }
    options.depth = options.depth.map(|depth| depth - 1.0);
    let (stats, percentiles) = {
        let data = &histogram.data().data;
        let stats = [
            data.min() as f64,
            data.max() as f64,
            data.mean(),
            data.exceeds() as f64,
            data.stddev(),
            data.skewness(),
            data.kurtosis(),
            data.count() as f64,
        ];
        (stats, data.percentiles())
    };
    let map = number_map(
        scope,
        percentiles
            .into_iter()
            .map(|(percentile, value)| (percentile, value as f64)),
    )?;
    let mut values: Vec<HandleValue<'_>> =
        stats.iter().map(|value| number(scope, *value)).collect();
    values.push(map.to_jsval_throwing(scope)?);
    let shown = Object::new_plain_with_template(scope, &HISTOGRAM_INSPECT_PROPERTIES, &values)?;
    let shown = shown.to_jsval_throwing(scope)?;
    let text = format!(
        "Histogram {}",
        crate::inspect::inspect(scope, shown, options)?
    );
    js::JSString::from_str(scope, &text)?.to_jsval_raw_throwing(scope)
}

/// A histogram that `createHistogram` creates, which JavaScript records values in. See
/// <https://nodejs.org/api/perf_hooks.html#class-recordablehistogram-extends-histogram>.
#[core_runtime::jsclass(
    name = "RecordableHistogram",
    hidden,
    extends = Histogram,
    invalid_this = "crate::errors::invalid_this"
)]
pub struct RecordableHistogram {
    parent: HistogramImpl,
}

/// Report that memory ran out if `result` is `None`.
fn out_of_memory_unless<T>(scope: &Scope<'_>, result: Option<T>) -> Result<(), Thrown> {
    if result.is_none() {
        js::exception::report_out_of_memory(scope);
        return Err(Thrown::Pending);
    }
    Ok(())
}

/// The error `createHistogram` and `monitorEventLoopDelay` throw when a histogram can't be
/// created: Node.js's `ERR_OUT_OF_RANGE` for bounds the crate rejects, and an out-of-memory error
/// if the counts couldn't be allocated.
fn create_error(scope: &Scope<'_>, error: CreateError) -> Thrown {
    match error {
        CreateError::InvalidBounds => NodeError::new(
            ErrorKind::RangeError,
            "ERR_OUT_OF_RANGE",
            "options.highest is out of range",
        )
        .into(),
        CreateError::OutOfMemory => {
            js::exception::report_out_of_memory(scope);
            Thrown::Pending
        }
    }
}

/// The recordable histogram `value` is, or `ERR_INVALID_ARG_TYPE` naming the argument `other`.
fn other_recordable<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
) -> Result<RecordableHistogram<'s>, Thrown> {
    Object::from_value(scope, *value)
        .ok()
        .and_then(|object| object.cast::<RecordableHistogram>().ok())
        .ok_or_else(|| {
            NodeError::invalid_arg_type(scope, "other", "an instance of RecordableHistogram", value)
                .into()
        })
}

#[core_runtime::jsmethods(node_args)]
impl RecordableHistogram {
    /// Always throws `ERR_ILLEGAL_CONSTRUCTOR`, as only `createHistogram` creates recordable
    /// histograms.
    #[constructor]
    pub fn create() -> Result<Self, Thrown> {
        Err(NodeError::illegal_constructor().into())
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramrecordval>
    #[method]
    pub fn record(&self, scope: &Scope<'_>, val: HandleValue<'_>) -> Result<(), Thrown> {
        if !val.is_bigint() {
            validate_integer(scope, val, "val", 1.0, MAX_SAFE_INTEGER)?;
        }
        let value = int64_arg(scope, val, "value is out of range")?;
        self.data_mut().data.record(value);
        Ok(())
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramrecorddelta>
    #[method]
    pub fn record_delta(&self) {
        self.data_mut().data.record_delta();
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramrecordcorrectedval-expectedinterval>
    #[method]
    pub fn record_corrected(
        &self,
        scope: &Scope<'_>,
        val: HandleValue<'_>,
        expected_interval: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        if val.is_bigint() {
            if !expected_interval.is_bigint() {
                return Err(NodeError::invalid_arg_type(
                    scope,
                    "expectedInterval",
                    "of type bigint",
                    expected_interval,
                )
                .into());
            }
        } else {
            validate_integer(scope, val, "val", 1.0, MAX_SAFE_INTEGER)?;
            validate_integer(
                scope,
                expected_interval,
                "expectedInterval",
                1.0,
                MAX_SAFE_INTEGER,
            )?;
        }
        let value = int64_arg(scope, val, "value is out of range")?;
        let interval = int64_arg(
            scope,
            expected_interval,
            "expected_interval is out of range",
        )?;
        self.data_mut().data.record_corrected(value, interval);
        Ok(())
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramsubtractother>
    #[method]
    pub fn subtract(&self, scope: &Scope<'_>, other: HandleValue<'_>) -> Result<(), Thrown> {
        let other = other_recordable(scope, other)?;
        let subtracted = if other.as_object().as_raw() == self.as_object().as_raw() {
            self.data_mut().data.subtract_self();
            Some(0.0)
        } else {
            let theirs = other.data();
            let subtracted = self.data_mut().data.subtract(&theirs.data);
            subtracted
        };
        out_of_memory_unless(scope, subtracted)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramaddother>
    #[method]
    pub fn add(&self, scope: &Scope<'_>, other: HandleValue<'_>) -> Result<(), Thrown> {
        let other = other_recordable(scope, other)?;
        let added = if other.as_object().as_raw() == self.as_object().as_raw() {
            self.data_mut().data.add_self()
        } else {
            let theirs = other.data();
            let added = self.data_mut().data.add(&theirs.data);
            Some(added)
        };
        out_of_memory_unless(scope, added)
    }
}

/// The histogram `monitorEventLoopDelay` creates, which a timer records the event loop's delay
/// in. See <https://nodejs.org/api/perf_hooks.html#class-eldhistogram-extends-histogram>.
#[core_runtime::jsclass(
    name = "ELDHistogram",
    hidden,
    extends = Histogram,
    invalid_this = "crate::errors::invalid_this"
)]
pub struct EldHistogram {
    parent: HistogramImpl,
    /// The milliseconds between samples.
    #[no_trace]
    resolution: u64,
    /// The event loop timer that samples the delay while the histogram is enabled. The timer
    /// can belong to an event loop that is no longer active, or that finished.
    #[no_trace]
    timer: Option<u64>,
}

#[core_runtime::jsmethods(node_args)]
impl EldHistogram {
    /// Always throws `ERR_ILLEGAL_CONSTRUCTOR`, as only `monitorEventLoopDelay` creates these
    /// histograms.
    #[constructor]
    pub fn create() -> Result<Self, Thrown> {
        Err(NodeError::illegal_constructor().into())
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramenable>: start sampling, with
    /// an unreferenced timer on the active event loop that repeats every `resolution`
    /// milliseconds and records the time since its previous run. Returns `false` if that loop's
    /// timer samples already. A timer of another event loop stops at its next run.
    #[method]
    pub fn enable(&self, scope: &Scope<'_>) -> Result<bool, ExnThrown> {
        let (resolution, timer) = {
            let data = self.data();
            (data.resolution, data.timer)
        };
        let sampling = timer.is_some_and(|id| {
            core_runtime::event_loop::with_active_event_loop(|event_loop| {
                event_loop.has_js_timer(id)
            })
            .unwrap_or(false)
        });
        if sampling {
            return Ok(false);
        }
        let sample = js::class::get_or_init_shared_function(
            scope,
            sample_delay as *const () as usize,
            |scope| js::Function::new_callback(scope, c"", 0, sample_delay, ()),
        )?;
        let this = self.as_object().to_jsval_throwing(scope)?;
        let id = core_runtime::event_loop::reserve_js_timer_id();
        let id_value = (id as f64).to_jsval_throwing(scope)?;
        let Some(id) = core_runtime::event_loop::timer::start_timer(
            scope,
            sample.as_callable(),
            this,
            &[id_value],
            Duration::from_millis(resolution),
            true,
            Some(id),
        ) else {
            return Err(core_runtime::event_loop::throw_no_event_loop(
                scope,
                "monitorEventLoopDelay",
            ));
        };
        core_runtime::event_loop::with_active_event_loop(|event_loop| {
            event_loop.set_js_timer_ref(id, false)
        });
        self.data_mut().timer = Some(id);
        Ok(true)
    }

    /// <https://nodejs.org/api/perf_hooks.html#histogramdisable>: stop sampling.
    /// Returns `false` if sampling had stopped. A timer of another event loop stops at its next
    /// run.
    #[method]
    pub fn disable(&self) -> bool {
        let Some(id) = self.data_mut().timer.take() else {
            return false;
        };
        core_runtime::event_loop::with_active_event_loop(|event_loop| {
            event_loop.clear_js_timer(id)
        });
        true
    }
}

/// The callback of an `ELDHistogram`'s timer, whose `this` is the histogram and whose argument is
/// the timer's ID: record the nanoseconds since the previous sample. A timer that isn't the
/// histogram's current timer clears itself instead.
fn sample_delay(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    let id = args.get(0);
    let id = if id.is_number() {
        id.to_number() as u64
    } else {
        0
    };
    let histogram = Object::from_value(scope, *this)
        .ok()
        .and_then(|object| object.cast::<EldHistogram>().ok());
    if let Some(histogram) = histogram {
        let current = histogram.data().timer == Some(id);
        if current {
            histogram.data_mut().parent.data.record_delta();
        } else {
            core_runtime::event_loop::with_active_event_loop(|event_loop| {
                event_loop.clear_js_timer(id)
            });
        }
    }
    Ok(js::value::undefined())
}

/// <https://nodejs.org/api/perf_hooks.html#histogramsymboldispose>: disable the `ELDHistogram`
/// `this`.
fn dispose_eld_histogram(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let histogram = Object::from_value(scope, args.this())
        .ok()
        .and_then(|object| object.cast::<EldHistogram>().ok())
        .ok_or_else(|| crate::errors::invalid_this(scope, "ELDHistogram"))?;
    histogram.disable();
    Ok(js::value::undefined())
}

/// <https://nodejs.org/api/perf_hooks.html#perf_hookscreatehistogramoptions>
///
/// The options are read and checked in the order of Node.js's `createHistogram`.
pub(crate) fn create_histogram<'s>(
    scope: &'s Scope<'_>,
    options: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let read = |name: &'static std::ffi::CStr| -> Result<Option<HandleValue<'s>>, Thrown> {
        if options.is_undefined() {
            return Ok(None);
        }
        let options = validate_plain_object(scope, options, "options", false)?;
        let value = options.get_property(scope, name)?;
        Ok((!value.is_undefined()).then_some(value))
    };
    if !options.is_undefined() {
        validate_plain_object(scope, options, "options", false)?;
    }
    let lowest = read(c"lowest")?;
    let highest = read(c"highest")?;
    let figures = read(c"figures")?;
    let half_life = read(c"halfLife")?;
    let threshold = read(c"threshold")?;

    let lowest = match lowest {
        Some(value) if value.is_bigint() => {
            int64_arg(scope, value, "options.lowest is out of range")?
        }
        Some(value) => {
            validate_integer(scope, value, "options.lowest", 1.0, MAX_SAFE_INTEGER)? as i64
        }
        None => 1,
    };
    let highest = match highest {
        Some(value) if value.is_bigint() => {
            let highest = int64_arg(scope, value, "options.highest is out of range")?;
            if (highest as i128) < 2 * lowest as i128 {
                let mut error =
                    NodeError::invalid_arg_value(scope, "options.highest", value, "is invalid");
                error.kind = ErrorKind::RangeError;
                return Err(error.into());
            }
            highest
        }
        Some(value) => {
            let min = 2.0 * lowest as f64;
            validate_integer(scope, value, "options.highest", min, MAX_SAFE_INTEGER)? as i64
        }
        None => MAX_SAFE_INTEGER as i64,
    };
    let figures = match figures {
        Some(value) => validate_integer(scope, value, "options.figures", 1.0, 5.0)? as u8,
        None => 3,
    };
    let half_life = match half_life {
        Some(value) => {
            let half_life = number_arg(scope, value, "options.halfLife")?;
            if half_life < 0.0 {
                return Err(NodeError::out_of_range(
                    "options.halfLife",
                    ">= 0",
                    &out_of_range_received(half_life),
                )
                .into());
            }
            half_life
        }
        None => 0.0,
    };
    let threshold = match threshold {
        Some(value) => {
            let threshold = number_arg(scope, value, "options.threshold")?;
            if threshold < 0.0 {
                return Err(NodeError::out_of_range(
                    "options.threshold",
                    ">= 0",
                    &out_of_range_received(threshold),
                )
                .into());
            }
            threshold as i64
        }
        None => 0,
    };
    let data = HistogramData::new(lowest as u64, highest as u64, figures, half_life, threshold)
        .map_err(|error| create_error(scope, error))?;
    let histogram = js::class::create_instance_with::<RecordableHistogramImpl>(scope, |_| {
        RecordableHistogramImpl {
            parent: HistogramImpl { data },
        }
    })?;
    Ok(histogram.as_object())
}

/// <https://nodejs.org/api/perf_hooks.html#perf_hooksmonitoreventloopdelayoptions>
///
/// Deviates from Node.js: with `samplePerIteration`, the histogram samples on a timer as well,
/// as the event loop has no hook that runs on each of its iterations.
pub(crate) fn monitor_event_loop_delay<'s>(
    scope: &'s Scope<'_>,
    options: HandleValue<'_>,
) -> Result<Object<'s>, Thrown> {
    let (sample_per_iteration, resolution) = if options.is_undefined() {
        (None, None)
    } else {
        let options = validate_plain_object(scope, options, "options", false)?;
        let sample_per_iteration = options.get_property(scope, c"samplePerIteration")?;
        let resolution = options.get_property(scope, c"resolution")?;
        (
            (!sample_per_iteration.is_undefined()).then_some(sample_per_iteration),
            (!resolution.is_undefined()).then_some(resolution),
        )
    };
    if let Some(value) = sample_per_iteration {
        validate_boolean(scope, value, "options.samplePerIteration")?;
    }
    let resolution = match resolution {
        Some(value) => {
            validate_integer(scope, value, "options.resolution", 1.0, MAX_SAFE_INTEGER)? as u64
        }
        None => 10,
    };
    let data = HistogramData::new(1, i64::MAX as u64, 3, 0.0, 0)
        .map_err(|error| create_error(scope, error))?;
    let histogram =
        js::class::create_instance_with::<EldHistogramImpl>(scope, |_| EldHistogramImpl {
            parent: HistogramImpl { data },
            resolution,
            timer: None,
        })?;
    Ok(histogram.as_object())
}

/// Whether `value` is a histogram.
pub(crate) fn is_histogram(scope: &Scope<'_>, value: HandleValue<'_>) -> bool {
    histogram_of(scope, value).is_some()
}

/// Register the histogram classes and their templates, and define the symbol-keyed methods of
/// their prototypes.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    HISTOGRAM_JSON_PROPERTIES.register(scope)?;
    HISTOGRAM_INSPECT_PROPERTIES.register(scope)?;
    WELCH_PROPERTIES.register(scope)?;
    INTERVAL_PROPERTIES.register(scope)?;
    MANN_WHITNEY_PROPERTIES.register(scope)?;
    PERCENTILE_CI_PROPERTIES.register(scope)?;
    Histogram::add_to_global(scope, global);
    RecordableHistogram::add_to_global(scope, global);
    EldHistogram::add_to_global(scope, global);
    let proto = js::class::get_prototype_object_for::<HistogramImpl>(scope)
        .expect("Histogram was registered above");
    let inspect = js::Function::new_callback(
        scope,
        c"[nodejs.util.inspect.custom]",
        2,
        inspect_histogram,
        (),
    )?;
    proto.define_value_by_id(
        scope,
        crate::inspect::INSPECT_CUSTOM.id(scope)?,
        *inspect,
        js::class_spec::JSPROP_READONLY as u32,
    )?;
    let proto = js::class::get_prototype_object_for::<EldHistogramImpl>(scope)
        .expect("ELDHistogram was registered above");
    let dispose =
        js::Function::new_callback(scope, c"[Symbol.dispose]", 0, dispose_eld_histogram, ())?;
    js::class::define_well_known_symbol_property(
        scope,
        proto,
        js::native::SymbolCode::dispose,
        *dispose,
    )
}
