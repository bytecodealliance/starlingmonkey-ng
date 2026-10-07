// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The statistics of `node:perf_hooks`' histograms, as Node.js's `src/histogram.cc` computes them
//! from an HdrHistogram.
//!
//! The `hdrhistogram` crate holds the counts, with the bucket layout of HdrHistogram_c, which
//! Node.js uses. The queries are ports of HdrHistogram_c's and Node.js's, since the crate's own
//! round percentiles and averages differently: a percentile's count is rounded half up, the
//! `percentiles` map holds the lowest value of each bucket, and the mean sums integer products.

use hdrhistogram::Histogram;

/// One histogram's counts and the state Node.js keeps next to them.
#[derive(Clone)]
pub(crate) struct HistogramData {
    counts: Histogram<u64>,
    /// The `platform::clock::monotonic_ns` reading of the last `recordDelta`, or 0 before it.
    prev: u64,
    /// The number of values that couldn't be recorded, as they were out of range.
    exceeds: u64,
    ewma: Ewma,
}

/// The exponentially weighted moving averages a histogram created with `halfLife` keeps.
#[derive(Clone, Copy, Default)]
struct Ewma {
    /// The smoothing factor, 0 if the averages are off.
    alpha: f64,
    mean: f64,
    variance: f64,
    initialized: bool,
    /// The value over which a recorded value counts as an error, 0 if errors aren't tracked.
    threshold: i64,
    error_rate: f64,
}

/// The results of `welchTest`: the t statistic, the degrees of freedom, the p-value, and the
/// lower and upper bounds of the confidence interval.
pub(crate) type WelchResult = [f64; 5];

impl Default for HistogramData {
    /// An empty histogram of the values 1 and 2, which the classes' private data starts out as
    /// before it is replaced.
    fn default() -> Self {
        Self::new(1, 2, 0, 0.0, 0).expect("the bounds are valid")
    }
}

/// Why [`HistogramData::new`] couldn't create a histogram.
#[derive(Debug)]
pub(crate) enum CreateError {
    /// The crate rejects the bounds.
    InvalidBounds,
    /// The counts array couldn't be allocated.
    OutOfMemory,
}

/// The length of the counts array of a histogram of the values from `lowest` to `highest` with
/// `figures` significant decimal digits, as the crate computes it, or `None` for bounds it
/// rejects.
fn counts_len(lowest: u64, highest: u64, figures: u8) -> Option<usize> {
    let largest = 2 * 10u64.checked_pow(u32::from(figures))?;
    let unit_magnitude = (lowest as f64).log2().floor() as u32;
    let sub_bucket_count_magnitude = (largest as f64).log2().ceil() as u32;
    if unit_magnitude + sub_bucket_count_magnitude > 63 {
        return None;
    }
    let sub_bucket_count = 1u64 << sub_bucket_count_magnitude;
    let mut smallest_untrackable = sub_bucket_count << unit_magnitude;
    let mut buckets = 1u64;
    while smallest_untrackable <= highest {
        if smallest_untrackable > u64::MAX / 2 {
            buckets += 1;
            break;
        }
        smallest_untrackable <<= 1;
        buckets += 1;
    }
    usize::try_from((buckets + 1) * (sub_bucket_count / 2)).ok()
}

/// A histogram of the values from `lowest` to `highest` with `figures` significant decimal
/// digits, after checking that memory for its counts array can be allocated.
fn new_counts(lowest: u64, highest: u64, figures: u8) -> Result<Histogram<u64>, CreateError> {
    let len = counts_len(lowest, highest, figures).ok_or(CreateError::InvalidBounds)?;
    Vec::<u64>::new()
        .try_reserve_exact(len)
        .map_err(|_| CreateError::OutOfMemory)?;
    Histogram::new_with_bounds(lowest, highest, figures).map_err(|_| CreateError::InvalidBounds)
}

impl HistogramData {
    /// A histogram of the values from `lowest` to `highest` with `figures` significant decimal
    /// digits, with moving averages over `half_life` samples if it is positive, which count the
    /// values over `threshold` as errors if it is positive.
    pub(crate) fn new(
        lowest: u64,
        highest: u64,
        figures: u8,
        half_life: f64,
        threshold: i64,
    ) -> Result<Self, CreateError> {
        let counts = new_counts(lowest, highest, figures)?;
        let alpha = if half_life > 0.0 {
            1.0 - (-std::f64::consts::LN_2 / half_life).exp()
        } else {
            0.0
        };
        Ok(Self {
            counts,
            prev: 0,
            exceeds: 0,
            ewma: Ewma {
                alpha,
                threshold,
                ..Ewma::default()
            },
        })
    }

    /// The buckets with a count, in order, as the lowest value of each bucket and its count.
    fn recorded(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.counts.iter_recorded().map(|value| {
            (
                self.counts.lowest_equivalent(value.value_iterated_to()),
                value.count_at_value(),
            )
        })
    }

    /// Every bucket, in order, as the lowest value of each bucket and its count.
    fn all(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.counts.iter_all().map(|value| {
            (
                self.counts.lowest_equivalent(value.value_iterated_to()),
                value.count_at_value(),
            )
        })
    }

    /// HdrHistogram_c's `hdr_record_values`: whether `value` was in range and recorded `count`
    /// times.
    fn record_n(&mut self, value: i64, count: u64) -> bool {
        if value < 0 || value as u64 > self.counts.high() {
            return false;
        }
        self.counts.record_n(value as u64, count).is_ok()
    }

    /// Node.js's `Histogram::UpdateEwma`.
    fn update_ewma(&mut self, value: f64) {
        let ewma = &mut self.ewma;
        if ewma.alpha <= 0.0 {
            return;
        }
        let exceeded = if ewma.threshold > 0 && value > ewma.threshold as f64 {
            1.0
        } else {
            0.0
        };
        if !ewma.initialized {
            ewma.mean = value;
            ewma.variance = 0.0;
            ewma.initialized = true;
            if ewma.threshold > 0 {
                ewma.error_rate = exceeded;
            }
            return;
        }
        let diff = value - ewma.mean;
        ewma.mean += ewma.alpha * diff;
        ewma.variance = (1.0 - ewma.alpha) * (ewma.variance + ewma.alpha * diff * diff);
        if ewma.threshold > 0 {
            ewma.error_rate += ewma.alpha * (exceeded - ewma.error_rate);
        }
    }

    /// Record `value`, or count it as exceeding the range.
    pub(crate) fn record(&mut self, value: i64) {
        if self.record_n(value, 1) {
            self.update_ewma(value as f64);
        } else {
            self.exceeds += 1;
        }
    }

    /// Record `value`, and the values `expected_interval` apart below it that a stall of the
    /// recording omitted, as HdrHistogram_c's `hdr_record_corrected_values` does.
    pub(crate) fn record_corrected(&mut self, value: i64, expected_interval: i64) {
        let recorded = self.record_n(value, 1) && {
            let mut missing = value.saturating_sub(expected_interval);
            let mut recorded = true;
            if expected_interval > 0 && value > expected_interval {
                while missing >= expected_interval {
                    if !self.record_n(missing, 1) {
                        recorded = false;
                        break;
                    }
                    missing -= expected_interval;
                }
            }
            recorded
        };
        if recorded {
            self.update_ewma(value as f64);
        } else {
            self.exceeds += 1;
        }
    }

    /// Record the nanoseconds since the previous call, if there was one.
    pub(crate) fn record_delta(&mut self) {
        let time = platform::clock::monotonic_ns();
        if self.prev > 0 {
            let delta = time.saturating_sub(self.prev).min(i64::MAX as u64) as i64;
            self.record(delta);
        }
        self.prev = time;
    }

    /// Remove every value and reset the averages.
    pub(crate) fn reset(&mut self) {
        self.counts.reset();
        self.exceeds = 0;
        self.prev = 0;
        let alpha = self.ewma.alpha;
        let threshold = self.ewma.threshold;
        self.ewma = Ewma {
            alpha,
            threshold,
            ..Ewma::default()
        };
    }

    /// The number of recorded values.
    pub(crate) fn count(&self) -> u64 {
        self.counts.len()
    }

    /// The number of values that were out of range.
    pub(crate) fn exceeds(&self) -> u64 {
        self.exceeds
    }

    /// HdrHistogram_c's `hdr_min`: `i64::MAX` if the histogram is empty.
    pub(crate) fn min(&self) -> i64 {
        if self.counts.is_empty() {
            return i64::MAX;
        }
        self.counts.min() as i64
    }

    /// HdrHistogram_c's `hdr_max`: 0 if the histogram is empty.
    pub(crate) fn max(&self) -> i64 {
        self.counts.max() as i64
    }

    /// HdrHistogram_c's `hdr_mean`: NaN if the histogram is empty.
    pub(crate) fn mean(&self) -> f64 {
        let total = self.recorded().fold(0i64, |total, (value, count)| {
            let median = self.counts.median_equivalent(value) as i64;
            total.wrapping_add((count as i64).wrapping_mul(median))
        });
        total as f64 / self.count() as f64
    }

    /// HdrHistogram_c's `hdr_stddev`: NaN if the histogram is empty.
    pub(crate) fn stddev(&self) -> f64 {
        let mean = self.mean();
        let total = self.recorded().fold(0.0, |total, (value, count)| {
            let deviation = self.counts.median_equivalent(value) as f64 - mean;
            total + deviation * deviation * count as f64
        });
        (total / self.count() as f64).sqrt()
    }

    /// The first `(m2, m3, m4)` central moment sums over the recorded values, as Node.js's
    /// `Skewness` and `Kurtosis` compute them.
    fn moments(&self) -> (f64, f64, f64) {
        let mean = self.mean();
        self.recorded()
            .fold((0.0, 0.0, 0.0), |(m2, m3, m4), (value, count)| {
                let deviation = self.counts.median_equivalent(value) as f64 - mean;
                let d2 = deviation * deviation;
                let count = count as f64;
                (
                    m2 + count * d2,
                    m3 + count * d2 * deviation,
                    m4 + count * d2 * d2,
                )
            })
    }

    /// Node.js's `Histogram::Skewness`: 0 for fewer than 3 values.
    pub(crate) fn skewness(&self) -> f64 {
        let total = self.count();
        if total < 3 {
            return 0.0;
        }
        let (m2, m3, _) = self.moments();
        let n = total as f64;
        let variance = m2 / n;
        if variance == 0.0 {
            return 0.0;
        }
        (m3 / n) / (variance * variance.sqrt())
    }

    /// Node.js's `Histogram::Kurtosis`, the excess kurtosis: 0 for fewer than 4 values.
    pub(crate) fn kurtosis(&self) -> f64 {
        let total = self.count();
        if total < 4 {
            return 0.0;
        }
        let (m2, _, m4) = self.moments();
        let n = total as f64;
        let variance = m2 / n;
        if variance == 0.0 {
            return 0.0;
        }
        (m4 / n) / (variance * variance) - 3.0
    }

    /// The moving average of the recorded values, 0 before the first.
    pub(crate) fn ewma_mean(&self) -> f64 {
        if self.ewma.initialized {
            self.ewma.mean
        } else {
            0.0
        }
    }

    /// The moving standard deviation of the recorded values, 0 before the first.
    pub(crate) fn ewma_stddev(&self) -> f64 {
        if self.ewma.initialized {
            self.ewma.variance.sqrt()
        } else {
            0.0
        }
    }

    /// The moving rate of values over the threshold, 0 before the first.
    pub(crate) fn ewma_error_rate(&self) -> f64 {
        if self.ewma.initialized {
            self.ewma.error_rate
        } else {
            0.0
        }
    }

    /// The count a percentile falls at, as HdrHistogram_c rounds it, and at least 1.
    fn count_at_percentile(&self, percentile: f64) -> u64 {
        let requested = percentile.min(100.0);
        let count = ((requested / 100.0) * self.count() as f64 + 0.5) as i64;
        count.max(1) as u64
    }

    /// HdrHistogram_c's `hdr_value_at_percentile`.
    pub(crate) fn percentile(&self, percentile: f64) -> i64 {
        let target = self.count_at_percentile(percentile);
        let mut cumulative = 0;
        let value = self
            .recorded()
            .find(|(_, count)| {
                cumulative += count;
                cumulative >= target
            })
            .map_or(0, |(value, _)| value);
        if percentile == 0.0 {
            self.counts.lowest_equivalent(value) as i64
        } else {
            self.counts.highest_equivalent(value) as i64
        }
    }

    /// HdrHistogram_c's `hdr_value_at_percentiles` for `percentiles`, sorted in ascending order.
    /// A percentile past the recorded values keeps the count it falls at, as there.
    pub(crate) fn percentiles_at(&self, percentiles: &[f64]) -> Vec<i64> {
        let mut values: Vec<i64> = percentiles
            .iter()
            .map(|percentile| self.count_at_percentile(*percentile) as i64)
            .collect();
        let mut total = 0i64;
        let mut position = 0;
        for (value, count) in self.recorded() {
            if position >= values.len() {
                break;
            }
            total += count as i64;
            while position < values.len() && total >= values[position] {
                values[position] = self.counts.highest_equivalent(value) as i64;
                position += 1;
            }
        }
        values
    }

    /// The `percentiles` map of Node.js's histograms: HdrHistogram_c's percentile iteration with
    /// one tick per half distance, as each percentile and the lowest value of its bucket.
    pub(crate) fn percentiles(&self) -> Vec<(f64, i64)> {
        let total = self.count();
        let mut entries = Vec::new();
        let mut buckets = self.recorded();
        let Some(mut bucket) = buckets.next() else {
            entries.push((100.0, 0));
            return entries;
        };
        let mut cumulative = bucket.1;
        let mut percentile_to_iterate_to = 0.0f64;
        // The first step starts before the first bucket, with nothing accounted for.
        let mut first = true;
        loop {
            // Every value is accounted for once the cumulative count reaches the total.
            if !std::mem::take(&mut first) && cumulative >= total {
                entries.push((100.0, bucket.0 as i64));
                return entries;
            }
            loop {
                let current = (100.0 * cumulative as f64) / total as f64;
                if percentile_to_iterate_to <= current {
                    entries.push((percentile_to_iterate_to, bucket.0 as i64));
                    let half_distance = 2f64
                        .powf(
                            ((100.0 / (100.0 - percentile_to_iterate_to)).ln() / 2f64.ln()).trunc()
                                + 1.0,
                        )
                        .trunc();
                    percentile_to_iterate_to += 100.0 / half_distance;
                    break;
                }
                match buckets.next() {
                    Some(next) => {
                        bucket = next;
                        cumulative += next.1;
                    }
                    None => {
                        entries.push((100.0, bucket.0 as i64));
                        return entries;
                    }
                }
            }
        }
    }

    /// Node.js's `Histogram::Cdf`: the fraction of the values in the buckets up to the one
    /// `value` falls into.
    pub(crate) fn cdf(&self, value: i64) -> f64 {
        let total = self.count();
        if total == 0 {
            return 0.0;
        }
        let mut cumulative = 0;
        for (lowest, count) in self.all() {
            cumulative += count;
            if self.counts.highest_equivalent(lowest) as i64 >= value {
                return cumulative as f64 / total as f64;
            }
            if cumulative >= total {
                break;
            }
        }
        1.0
    }

    /// The count of the bucket `value` falls into.
    pub(crate) fn count_at(&self, value: i64) -> u64 {
        self.counts.count_at(value.max(0) as u64)
    }

    /// Node.js's `Histogram::Add`: add the values of `other`, and its exceeding count. Returns the
    /// number of values that were out of range.
    pub(crate) fn add(&mut self, other: &HistogramData) -> f64 {
        self.exceeds += other.exceeds;
        self.prev = self.prev.max(other.prev);
        let mut dropped = 0;
        for (value, count) in other.recorded() {
            if !self.record_n(value as i64, count) {
                dropped += count;
            }
        }
        dropped as f64
    }

    /// [`add`](Self::add) with the histogram itself as `other`, which doubles each count. Returns
    /// `None` if memory ran out.
    pub(crate) fn add_self(&mut self) -> Option<f64> {
        let mut buckets = Vec::new();
        buckets
            .try_reserve_exact(self.counts.distinct_values())
            .ok()?;
        buckets.extend(self.recorded());
        self.exceeds += self.exceeds;
        let mut dropped = 0;
        for (value, count) in buckets {
            if !self.record_n(value as i64, count) {
                dropped += count;
            }
        }
        Some(dropped as f64)
    }

    /// [`subtract`](Self::subtract) with the histogram itself as `other`, which removes every
    /// value and the exceeding count.
    pub(crate) fn subtract_self(&mut self) {
        self.counts.reset();
        self.exceeds = 0;
    }

    /// Node.js's `Histogram::Subtract`: subtract the bucket counts of `other`, which become 0
    /// where `other`'s is larger, and its exceeding count. Returns the number of values that
    /// couldn't be subtracted, or `None` if memory ran out.
    pub(crate) fn subtract(&mut self, other: &HistogramData) -> Option<f64> {
        let other_len = other.counts.distinct_values();
        let (low, high, figures) = (self.counts.low(), self.counts.high(), self.counts.sigfig());
        let mut remaining = new_counts(low, high, figures).ok()?;
        let mut dropped = 0u64;
        let mut theirs = other.all();
        for (index, (value, mine)) in self.all().enumerate() {
            let count = if index < other_len {
                let (_, subtracted) = theirs.next().unwrap_or_default();
                dropped += subtracted.saturating_sub(mine);
                mine.saturating_sub(subtracted)
            } else {
                mine
            };
            if count > 0 {
                // The bucket's lowest value lands in the bucket it came from.
                let _ = remaining.record_n(value, count);
            }
        }
        self.counts = remaining;
        self.exceeds = self.exceeds.saturating_sub(other.exceeds);
        Some(dropped as f64)
    }

    /// The counts of `self` and `other` index by index, up to the longer of the two, with 0 past
    /// the end of the shorter.
    fn paired_counts<'a>(
        &'a self,
        other: &'a HistogramData,
    ) -> impl Iterator<Item = (u64, u64)> + 'a {
        let mine = self
            .all()
            .map(|(_, count)| Some(count))
            .chain(std::iter::repeat(None));
        let theirs = other
            .all()
            .map(|(_, count)| Some(count))
            .chain(std::iter::repeat(None));
        mine.zip(theirs)
            .take_while(|(a, b)| a.is_some() || b.is_some())
            .map(|(a, b)| (a.unwrap_or(0), b.unwrap_or(0)))
    }

    /// Node.js's `Histogram::KsTest`: the largest difference between the two distributions.
    pub(crate) fn ks_test(&self, other: &HistogramData) -> f64 {
        let (n1, n2) = (self.count(), other.count());
        if n1 == 0 || n2 == 0 {
            return 0.0;
        }
        let (mut cum1, mut cum2) = (0u64, 0u64);
        let mut max_d = 0.0f64;
        for (c1, c2) in self.paired_counts(other) {
            cum1 += c1;
            cum2 += c2;
            let d = (cum1 as f64 / n1 as f64 - cum2 as f64 / n2 as f64).abs();
            if d > max_d {
                max_d = d;
            }
        }
        max_d
    }

    /// The sample variances of `self` and `other`, from the population standard deviations.
    fn sample_variances(&self, other: &HistogramData) -> (f64, f64) {
        let (n1, n2) = (self.count() as f64, other.count() as f64);
        let (sd1, sd2) = (self.stddev(), other.stddev());
        (sd1 * sd1 * n1 / (n1 - 1.0), sd2 * sd2 * n2 / (n2 - 1.0))
    }

    /// Node.js's `Histogram::WelchTest`.
    pub(crate) fn welch_test(&self, other: &HistogramData, confidence: f64) -> WelchResult {
        let (n1, n2) = (self.count(), other.count());
        if n1 < 2 || n2 < 2 {
            return [0.0, 0.0, 1.0, 0.0, 0.0];
        }
        let (mean1, mean2) = (self.mean(), other.mean());
        let (var1, var2) = self.sample_variances(other);
        let (n1, n2) = (n1 as f64, n2 as f64);
        let se1 = var1 / n1;
        let se2 = var2 / n2;
        let se_sum = se1 + se2;
        if se_sum == 0.0 {
            return [0.0, 0.0, 1.0, 0.0, 0.0];
        }
        let t = (mean1 - mean2) / se_sum.sqrt();
        let df = (se_sum * se_sum) / (se1 * se1 / (n1 - 1.0) + se2 * se2 / (n2 - 1.0));
        let p = 2.0 * student_t_cdf(-t.abs(), df);
        let alpha = 1.0 - confidence;
        let t_crit = student_t_quantile(1.0 - alpha / 2.0, df);
        let margin = t_crit * se_sum.sqrt();
        let diff = mean1 - mean2;
        [t, df, p, diff - margin, diff + margin]
    }

    /// Node.js's `Histogram::MannWhitneyTest`: the U statistic, the z-score and the p-value.
    pub(crate) fn mann_whitney_test(&self, other: &HistogramData) -> [f64; 3] {
        let (n1, n2) = (self.count(), other.count());
        if n1 == 0 || n2 == 0 {
            return [0.0, 0.0, 1.0];
        }
        let (concordant, tied) = self.concordance(other);
        let u = concordant + 0.5 * tied;
        let (dn1, dn2) = (n1 as f64, n2 as f64);
        let mu = dn1 * dn2 / 2.0;
        let n_total = dn1 + dn2;
        let tie_correction = self
            .paired_counts(other)
            .map(|(c1, c2)| (c1 + c2) as f64)
            .filter(|tk| *tk > 1.0)
            .map(|tk| tk * tk * tk - tk)
            .sum::<f64>();
        let sigma_sq =
            (dn1 * dn2 / 12.0) * (n_total + 1.0 - tie_correction / (n_total * (n_total - 1.0)));
        if sigma_sq <= 0.0 {
            return [u, 0.0, 1.0];
        }
        let z = (u - mu) / sigma_sq.sqrt();
        let p = 2.0 * normal_cdf(-z.abs());
        [u, z, p]
    }

    /// The number of pairs whose value from `self` is in a higher bucket than the value from
    /// `other`, and the number of pairs in the same bucket.
    fn concordance(&self, other: &HistogramData) -> (f64, f64) {
        let mut cum2 = 0u64;
        let mut concordant = 0.0;
        let mut tied = 0.0;
        for (c1, c2) in self.paired_counts(other) {
            concordant += c1 as f64 * cum2 as f64;
            tied += c1 as f64 * c2 as f64;
            cum2 += c2;
        }
        (concordant, tied)
    }

    /// Node.js's `Histogram::CohensD`.
    pub(crate) fn cohens_d(&self, other: &HistogramData) -> f64 {
        let (n1, n2) = (self.count(), other.count());
        if n1 < 2 || n2 < 2 {
            return 0.0;
        }
        let (var1, var2) = self.sample_variances(other);
        let (n1, n2) = (n1 as f64, n2 as f64);
        let pooled = (((n1 - 1.0) * var1 + (n2 - 1.0) * var2) / (n1 + n2 - 2.0)).sqrt();
        if pooled == 0.0 {
            return 0.0;
        }
        (self.mean() - other.mean()) / pooled
    }

    /// Node.js's `Histogram::CliffsD`.
    pub(crate) fn cliffs_d(&self, other: &HistogramData) -> f64 {
        let (n1, n2) = (self.count(), other.count());
        if n1 == 0 || n2 == 0 {
            return 0.0;
        }
        let (concordant, tied) = self.concordance(other);
        let pairs = n1 as f64 * n2 as f64;
        let discordant = pairs - concordant - tied;
        (concordant - discordant) / pairs
    }

    /// Node.js's `Histogram::PercentileCI`: the value at `percentile`, and the values at the
    /// bounds of its exact binomial confidence interval.
    pub(crate) fn percentile_ci(&self, percentile: f64, confidence: f64) -> [i64; 3] {
        let value = self.percentile(percentile);
        let n = self.count() as i64;
        if n < 2 {
            return [value, value, value];
        }
        let p = percentile / 100.0;
        let alpha = 1.0 - confidence;
        let (mut lo, mut hi) = (0i64, n);
        while lo < hi {
            let mid = lo + (hi - lo + 1) / 2;
            if binomial_cdf(mid - 1, n, p) <= alpha / 2.0 {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let lower = lo as f64 / n as f64 * 100.0;
        let (mut lo, mut hi) = (0i64, n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if binomial_cdf(mid - 1, n, p) >= 1.0 - alpha / 2.0 {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        let upper = lo as f64 / n as f64 * 100.0;
        [value, self.percentile(lower), self.percentile(upper)]
    }

    /// HdrHistogram_c's linear iteration in steps of `step`, as Node.js's `linearBuckets`
    /// reports it: the lowest value of the bucket each step ends in, and the count the step
    /// added.
    pub(crate) fn linear_buckets(&self, step: i64) -> Vec<(i64, i64)> {
        self.stepped_buckets(step, |level| level.saturating_add(step))
    }

    /// HdrHistogram_c's logarithmic iteration from `first`, with each level `base` times the
    /// one before it in integer arithmetic, as Node.js's `logBuckets` reports it.
    pub(crate) fn log_buckets(&self, first: i64, base: f64) -> Vec<(i64, i64)> {
        let factor = base as i64;
        self.stepped_buckets(first, |level| level.saturating_mul(factor))
    }

    /// HdrHistogram_c's linear and logarithmic iterations, from the reporting level `first`,
    /// with `next` giving each following level.
    fn stepped_buckets(&self, first: i64, next: impl Fn(i64) -> i64) -> Vec<(i64, i64)> {
        let total = self.count();
        let lowest_equivalent =
            |level: i64| self.counts.lowest_equivalent(level.max(0) as u64) as i64;
        let mut entries = Vec::new();
        let mut buckets = self.all().peekable();
        // The value of the bucket the iteration is at, which is 0 before the first bucket.
        let mut current = 0u64;
        let mut cumulative = 0u64;
        let mut level = first;
        let mut level_lowest = lowest_equivalent(level);
        loop {
            let has_next = cumulative < total;
            let next_above_level = buckets
                .peek()
                .is_some_and(|(value, _)| *value as i64 > level_lowest);
            if !(has_next || next_above_level) {
                return entries;
            }
            let mut added = 0u64;
            loop {
                if current as i64 >= level_lowest {
                    entries.push((current as i64, added as i64));
                    let next_level = next(level);
                    // A level that doesn't grow would report the same bucket forever.
                    if next_level <= level {
                        return entries;
                    }
                    level = next_level;
                    level_lowest = lowest_equivalent(level);
                    break;
                }
                match buckets.next() {
                    Some((value, count)) => {
                        current = value;
                        cumulative += count;
                        added += count;
                    }
                    None => {
                        entries.push((current as i64, added as i64));
                        return entries;
                    }
                }
            }
        }
    }
}

/// The continued fraction of the regularized incomplete beta function, by Lentz's method, as
/// Node.js's `BetaContinuedFraction` evaluates it.
fn beta_continued_fraction(a: f64, b: f64, x: f64) -> f64 {
    const FPMIN: f64 = 1e-30;
    const MAXIT: i32 = 200;
    const EPS: f64 = 3e-12;
    let qab = a + b;
    let qap = a + 1.0;
    let qam = a - 1.0;
    let mut c = 1.0;
    let mut d = 1.0 - qab * x / qap;
    if d.abs() < FPMIN {
        d = FPMIN;
    }
    d = 1.0 / d;
    let mut h = d;
    for m in 1..=MAXIT {
        let m = f64::from(m);
        let m2 = 2.0 * m;
        let aa = m * (b - m) * x / ((qam + m2) * (a + m2));
        d = 1.0 + aa * d;
        if d.abs() < FPMIN {
            d = FPMIN;
        }
        c = 1.0 + aa / c;
        if c.abs() < FPMIN {
            c = FPMIN;
        }
        d = 1.0 / d;
        h *= d * c;
        let aa = -(a + m) * (qab + m) * x / ((a + m2) * (qap + m2));
        d = 1.0 + aa * d;
        if d.abs() < FPMIN {
            d = FPMIN;
        }
        c = 1.0 + aa / c;
        if c.abs() < FPMIN {
            c = FPMIN;
        }
        d = 1.0 / d;
        let del = d * c;
        h *= del;
        if (del - 1.0).abs() <= EPS {
            break;
        }
    }
    h
}

/// The regularized incomplete beta function `I_x(a, b)`.
fn regularized_incomplete_beta(a: f64, b: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let ln_front =
        libm::lgamma(a + b) - libm::lgamma(a) - libm::lgamma(b) + a * x.ln() + b * (1.0 - x).ln();
    let bt = ln_front.exp();
    if x < (a + 1.0) / (a + b + 2.0) {
        bt * beta_continued_fraction(a, b, x) / a
    } else {
        1.0 - bt * beta_continued_fraction(b, a, 1.0 - x) / b
    }
}

/// The standard normal distribution function.
fn normal_cdf(x: f64) -> f64 {
    0.5 * libm::erfc(-x * std::f64::consts::SQRT_2 / 2.0)
}

/// The distribution function of Student's t-distribution with `df` degrees of freedom.
fn student_t_cdf(t: f64, df: f64) -> f64 {
    let x = df / (df + t * t);
    let ibeta = regularized_incomplete_beta(df / 2.0, 0.5, x);
    if t >= 0.0 {
        1.0 - 0.5 * ibeta
    } else {
        0.5 * ibeta
    }
}

/// The quantile function of Student's t-distribution, by bisection.
fn student_t_quantile(p: f64, df: f64) -> f64 {
    if p <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if p >= 1.0 {
        return f64::INFINITY;
    }
    if p == 0.5 {
        return 0.0;
    }
    let (mut lo, mut hi) = (-1e6, 1e6);
    for _ in 0..100 {
        let mid = (lo + hi) / 2.0;
        if student_t_cdf(mid, df) < p {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo + hi) / 2.0
}

/// The binomial distribution function `P(X <= k)` for `X ~ Binomial(n, p)`.
fn binomial_cdf(k: i64, n: i64, p: f64) -> f64 {
    if k < 0 {
        return 0.0;
    }
    if k >= n {
        return 1.0;
    }
    regularized_incomplete_beta((n - k) as f64, (k + 1) as f64, 1.0 - p)
}
