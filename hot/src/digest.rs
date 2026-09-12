pub const BUCKETS: usize = 80;
const SUB: f64 = 4.0;
pub const OUTLIER_SIGMA: f64 = 3.0;
pub const FLAT_SPIKE_RATIO: f64 = 2.0;
pub const OUTLIER_KEEP: usize = 64;
const MIN_N_FOR_OUTLIER: u64 = 30;
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Outlier {
    pub ms: f64,
    pub at: u128,
    pub sigma: f64,
    pub by: &'static str,
}
pub struct Digest {
    n: u64,
    mean: f64,
    m2: f64,
    min: f64,
    max: f64,
    hist: [u32; BUCKETS],
    out: Vec<Outlier>,
    out_next: usize,
    pub outlier_total: u64,
}
fn bucket(ms: f64) -> usize {
    if ms <= 0.0 {
        return 0;
    }
    let idx = ((ms.log2() + 4.0) * SUB).floor();
    idx.clamp(0.0, (BUCKETS - 1) as f64) as usize
}
fn bucket_ms(i: usize) -> f64 {
    2f64.powf(i as f64 / SUB - 4.0)
}
impl Default for Digest {
    fn default() -> Self {
        Self {
            n: 0,
            mean: 0.0,
            m2: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            hist: [0u32; BUCKETS],
            out: Vec::new(),
            out_next: 0,
            outlier_total: 0,
        }
    }
}
impl Digest {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, ms: f64, at: u128) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        if self.n >= MIN_N_FOR_OUTLIER {
            let sd = self.stddev();
            let (hit, sigma, by) = if sd > 0.0 {
                let z = (ms - self.mean) / sd;
                (z > OUTLIER_SIGMA, z, "sigma")
            } else {
                (self.mean > 0.0 && ms > self.mean * FLAT_SPIKE_RATIO, 0.0, "ratio")
            };
            if hit {
                self.outlier_total += 1;
                let o = Outlier { ms, at, sigma, by };
                if self.out.len() < OUTLIER_KEEP {
                    self.out.push(o);
                } else {
                    self.out[self.out_next] = o;
                    self.out_next = (self.out_next + 1) % OUTLIER_KEEP;
                }
            }
        }
        self.n += 1;
        let d = ms - self.mean;
        self.mean += d / self.n as f64;
        self.m2 += d * (ms - self.mean);
        if ms < self.min {
            self.min = ms;
        }
        if ms > self.max {
            self.max = ms;
        }
        self.hist[bucket(ms)] += 1;
    }
    pub fn n(&self) -> u64 {
        self.n
    }
    pub fn mean(&self) -> f64 {
        if self.n == 0 { 0.0 } else { self.mean }
    }
    pub fn stddev(&self) -> f64 {
        if self.n < 2 { 0.0 } else { (self.m2 / (self.n - 1) as f64).sqrt() }
    }
    pub fn pct(&self, p: f64) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        let want = (p * self.n as f64).ceil().max(1.0) as u64;
        let mut acc = 0u64;
        for (i, c) in self.hist.iter().enumerate() {
            acc += *c as u64;
            if acc >= want {
                return bucket_ms(i);
            }
        }
        self.max
    }
    pub fn outliers(&self) -> Vec<Outlier> {
        let mut v = self.out.clone();
        v.sort_by(|a, b| b.at.cmp(&a.at));
        v
    }
    pub fn json(&self) -> serde_json::Value {
        if self.n == 0 {
            return serde_json::json!({ "n" : 0 });
        }
        let r = |x: f64| (x * 100.0).round() / 100.0;
        serde_json::json!(
            { "n" : self.n, "mean" : r(self.mean), "sd" : r(self.stddev()), "min" :
            r(self.min), "max" : r(self.max), "p50" : r(self.pct(0.50)), "p90" : r(self
            .pct(0.90)), "p99" : r(self.pct(0.99)), "outliers" : self.outlier_total,
            "recent_outliers" : self.outliers().iter().take(8).map(| o |
            serde_json::json!({ "ms" : r(o.ms), "at" : o.at, "sigma" : r(o.sigma), "by" :
            o.by })).collect::< Vec < _ >> (), }
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn feed(d: &mut Digest, vals: &[f64]) {
        for (i, v) in vals.iter().enumerate() {
            d.push(*v, 1_000_000 + i as u128);
        }
    }
    #[test]
    fn memory_is_constant_regardless_of_sample_count() {
        let mut d = Digest::new();
        for i in 0..200_000 {
            d.push(20.0 + (i % 7) as f64, i as u128);
        }
        assert_eq!(d.n(), 200_000);
        assert!(d.out.len() <= OUTLIER_KEEP);
        assert_eq!(d.hist.len(), BUCKETS);
        assert_eq!(std::mem::size_of_val(& d.hist), BUCKETS * 4);
    }
    #[test]
    fn welford_mean_matches_the_naive_mean() {
        let mut d = Digest::new();
        let vals: Vec<f64> = (1..=1000).map(|i| i as f64).collect();
        feed(&mut d, &vals);
        let naive = vals.iter().sum::<f64>() / vals.len() as f64;
        assert!((d.mean() - naive).abs() < 1e-9, "{} vs {}", d.mean(), naive);
    }
    #[test]
    fn stddev_is_the_sample_stddev() {
        let mut d = Digest::new();
        feed(&mut d, &[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]);
        assert!((d.stddev() - 2.1380899).abs() < 1e-6, "got {}", d.stddev());
    }
    #[test]
    fn percentiles_survive_compaction_within_bucket_width() {
        let mut d = Digest::new();
        for i in 1..=1000 {
            d.push(i as f64, i as u128);
        }
        for (p, want) in [(0.50, 500.0), (0.90, 900.0), (0.99, 990.0)] {
            let got = d.pct(p);
            assert!((got - want).abs() / want < 0.20, "p{p} got {got} want ~{want}");
        }
    }
    #[test]
    fn only_HIGH_side_deviations_are_kept() {
        let mut d = Digest::new();
        feed(&mut d, &vec![20.0; 200]);
        let before = d.outlier_total;
        d.push(0.001, 9);
        assert_eq!(d.outlier_total, before, "a fast sample must not be recorded");
        d.push(9999.0, 10);
        assert_eq!(d.outlier_total, before + 1);
    }
    #[test]
    fn outliers_carry_when_and_how_bad() {
        let mut d = Digest::new();
        feed(&mut d, &vec![10.0; 100]);
        for i in 0..5 {
            d.push(10.0 + i as f64, 500 + i as u128);
        }
        d.push(400.0, 777);
        let o = d.outliers();
        assert!(! o.is_empty());
        assert_eq!(o[0].at, 777, "newest first");
        assert!(o[0].sigma > OUTLIER_SIGMA);
        assert_eq!(o[0].ms, 400.0);
    }
    #[test]
    fn a_flat_series_produces_no_outliers() {
        let mut d = Digest::new();
        feed(&mut d, &vec![25.0; 500]);
        assert_eq!(d.outlier_total, 0);
        assert_eq!(d.stddev(), 0.0);
    }
    #[test]
    fn nothing_is_flagged_before_the_mean_is_trustworthy() {
        let mut d = Digest::new();
        feed(&mut d, &[5.0, 5.0, 900.0]);
        assert_eq!(d.outlier_total, 0, "too few samples to call anything anomalous");
    }
    #[test]
    fn the_outlier_ring_is_bounded_but_counts_every_incident() {
        let mut d = Digest::new();
        feed(&mut d, &vec![10.0; 100]);
        let mut t = 1000u128;
        for _ in 0..(OUTLIER_KEEP * 2) {
            for _ in 0..20 {
                d.push(10.0, t);
                t += 1;
            }
            d.push(5000.0, t);
            t += 1;
        }
        assert!(
            d.outlier_total > OUTLIER_KEEP as u64,
            "every isolated spike is an incident, got {}", d.outlier_total
        );
        assert_eq!(d.outliers().len(), OUTLIER_KEEP, "retention is capped");
        assert_eq!(d.outliers() [0].at, t - 1);
    }
    #[test]
    fn a_sustained_shift_is_a_regime_change_not_endless_incidents() {
        let mut d = Digest::new();
        feed(&mut d, &vec![10.0; 100]);
        for i in 0..500 {
            d.push(500.0, 2000 + i as u128);
        }
        assert!(
            d.outlier_total < 50, "a new plateau must not flag every sample: {}", d
            .outlier_total
        );
    }
    #[test]
    fn a_bad_sample_cannot_poison_the_running_stats() {
        let mut d = Digest::new();
        feed(&mut d, &[10.0, 20.0]);
        d.push(f64::NAN, 1);
        d.push(f64::INFINITY, 2);
        d.push(-3.0, 3);
        assert_eq!(d.n(), 2);
        assert_eq!(d.mean(), 15.0);
    }
    #[test]
    fn buckets_round_trip_within_their_own_width() {
        for ms in [0.1, 1.0, 9.0, 21.0, 175.0, 1500.0, 10_000.0, 30_000.0] {
            let lo = bucket_ms(bucket(ms));
            assert!(
                lo <= ms * 1.001 && ms < lo * 2f64.powf(1.0 / SUB) * 1.001,
                "{ms} landed in bucket starting {lo}"
            );
        }
    }
    #[test]
    fn an_empty_digest_reports_nothing_rather_than_zeros() {
        let d = Digest::new();
        assert_eq!(d.json() ["n"], 0);
        assert!(d.json().get("p50").is_none(), "zeros would read as a blazing venue");
    }
}
