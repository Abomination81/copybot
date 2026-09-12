use std::collections::VecDeque;
use std::sync::Mutex;
pub const WINDOW_SECS: i64 = 30;
pub const DEFAULT_LIMIT: usize = 10;
const MAX_LANES: usize = 256;
pub const OVERCOPY_TOLERANCE: f64 = 1.5;
pub fn is_overcopy(
    his_filled: f64,
    our_copied: f64,
    intended_frac: f64,
    tolerance: f64,
) -> bool {
    if !(his_filled > 0.0) || !(intended_frac > 0.0) || !our_copied.is_finite() {
        return false;
    }
    our_copied > his_filled * intended_frac * tolerance
}
#[derive(Default)]
pub struct FireRate {
    inner: Mutex<Vec<VecDeque<i64>>>,
    limit: usize,
}
impl FireRate {
    pub fn new(lanes: usize, limit: usize) -> Self {
        Self {
            inner: Mutex::new((0..lanes).map(|_| VecDeque::new()).collect()),
            limit,
        }
    }
    pub fn record(&self, lane_ix: usize, now: i64) -> bool {
        let mut g = self.inner.lock().unwrap();
        if lane_ix >= g.len() {
            if lane_ix >= MAX_LANES {
                return true;
            }
            g.resize_with(lane_ix + 1, VecDeque::new);
        }
        let q = &mut g[lane_ix];
        q.push_back(now);
        let cutoff = now - WINDOW_SECS;
        while q.front().is_some_and(|&t| t <= cutoff) {
            q.pop_front();
        }
        q.len() >= self.limit
    }
    pub fn count(&self, lane_ix: usize, now: i64) -> usize {
        let mut g = self.inner.lock().unwrap();
        if lane_ix >= g.len() {
            return 0;
        }
        let q = &mut g[lane_ix];
        let cutoff = now - WINDOW_SECS;
        while q.front().is_some_and(|&t| t <= cutoff) {
            q.pop_front();
        }
        q.len()
    }
    pub fn limit(&self) -> usize {
        self.limit
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn HIS_measured_maximum_never_trips_it() {
        let f = FireRate::new(1, DEFAULT_LIMIT);
        for i in 0..3 {
            assert!(! f.record(0, 1000 + i), "tripped on his REAL measured behaviour");
        }
    }
    #[test]
    fn a_comfortable_burst_below_the_limit_is_allowed() {
        let f = FireRate::new(1, DEFAULT_LIMIT);
        for i in 0..DEFAULT_LIMIT - 1 {
            assert!(
                ! f.record(0, 1000 + i as i64), "tripped at fire {i}, limit is {}",
                DEFAULT_LIMIT
            );
        }
    }
    #[test]
    fn the_RUNAWAY_trips_it_within_the_burst() {
        let f = FireRate::new(1, DEFAULT_LIMIT);
        let mut tripped_at = None;
        for i in 0..120 {
            if f.record(0, 1000) && tripped_at.is_none() {
                tripped_at = Some(i);
            }
        }
        assert_eq!(
            tripped_at, Some(DEFAULT_LIMIT - 1),
            "the limit-th attempted fire must be stopped, got {tripped_at:?}"
        );
    }
    #[test]
    fn the_window_ROLLS_so_a_slow_steady_rate_never_trips() {
        let f = FireRate::new(1, DEFAULT_LIMIT);
        for i in 0..500 {
            assert!(! f.record(0, 1000 + i * 10), "a sustainable rate tripped at {i}");
        }
    }
    #[test]
    fn lanes_are_INDEPENDENT() {
        let f = FireRate::new(2, DEFAULT_LIMIT);
        for _ in 0..(DEFAULT_LIMIT + 5) {
            f.record(0, 1000);
        }
        assert!(f.record(0, 1000), "lane 0 should be over");
        assert!(! f.record(1, 1000), "lane 1 must be unaffected by lane 0");
    }
    #[test]
    fn old_fires_leave_the_window() {
        let f = FireRate::new(1, DEFAULT_LIMIT);
        for _ in 0..DEFAULT_LIMIT {
            f.record(0, 1000);
        }
        assert_eq!(f.count(0, 1000), DEFAULT_LIMIT);
        assert_eq!(f.count(0, 1000 + WINDOW_SECS), 0, "the window must expire fires");
        assert!(! f.record(0, 1000 + WINDOW_SECS), "a fresh window starts clean");
    }
    #[test]
    fn the_window_boundary_is_EXCLUSIVE_at_exactly_WINDOW_SECS() {
        let f = FireRate::new(1, DEFAULT_LIMIT);
        f.record(0, 1000);
        assert_eq!(
            f.count(0, 1000 + WINDOW_SECS - 1), 1, "one second short: still inside"
        );
        assert_eq!(f.count(0, 1000 + WINDOW_SECS), 0, "exactly at the edge: outside");
    }
    #[test]
    fn a_lane_index_beyond_MAX_LANES_still_fails_closed() {
        let f = FireRate::new(1, DEFAULT_LIMIT);
        assert!(f.record(MAX_LANES, 1000));
        assert_eq!(f.count(MAX_LANES, 1000), 0);
    }
    #[test]
    fn a_RUNTIME_LOADED_lane_starts_with_a_clean_window_not_an_instant_breach() {
        let f = FireRate::new(1, DEFAULT_LIMIT);
        assert!(
            ! f.record(1, 1000), "a brand-new lane's FIRST fire must not be a breach"
        );
        for i in 1..DEFAULT_LIMIT - 1 {
            assert!(
                ! f.record(1, 1000 + i as i64), "tripped at fire {i} for a fresh lane"
            );
        }
        assert!(
            f.record(1, 1000 + DEFAULT_LIMIT as i64 - 1),
            "a runtime-loaded lane still enforces its OWN limit once it exists"
        );
    }
    #[test]
    fn runtime_lanes_can_arrive_OUT_OF_ORDER_without_failing_closed() {
        let f = FireRate::new(1, DEFAULT_LIMIT);
        assert!(
            ! f.record(2, 1000), "lane 2 loading and firing before lane 1 ever does"
        );
        assert!(! f.record(1, 1000), "lane 1's own first fire, independently clean");
    }
    #[test]
    fn growth_never_disturbs_an_EARLIER_lanes_own_window() {
        let f = FireRate::new(1, DEFAULT_LIMIT);
        for _ in 0..(DEFAULT_LIMIT + 5) {
            f.record(0, 1000);
        }
        assert!(f.record(0, 1000), "lane 0 (example_lane_26) already over its limit");
        assert!(
            ! f.record(3, 1000), "growing to fit a new lane 3 must not affect lane 0"
        );
        assert!(f.record(0, 1000), "lane 0 is still over its limit after the growth");
    }
}
#[cfg(test)]
mod overcopy_tests {
    use super::*;
    #[test]
    fn the_2026_07_31_RUNAWAY_is_still_caught() {
        assert!(
            is_overcopy(5000.0, 1560.0, 0.10, OVERCOPY_TOLERANCE),
            "the incident this guard exists for must still trip it"
        );
    }
    #[test]
    fn the_example_lane_23_2026_08_16_FALSE_POSITIVE_is_not_caught() {
        assert!(
            ! is_overcopy(19000.0, 1614.375, 0.10, OVERCOPY_TOLERANCE),
            "correct multi-tranche mirroring must NOT halt the lane"
        );
    }
    #[test]
    fn an_UNKNOWN_denominator_is_not_a_malfunction() {
        assert!(! is_overcopy(0.0, 500.0, 0.10, OVERCOPY_TOLERANCE));
        assert!(! is_overcopy(- 1.0, 500.0, 0.10, OVERCOPY_TOLERANCE));
        assert!(! is_overcopy(1000.0, 500.0, 0.0, OVERCOPY_TOLERANCE));
        assert!(! is_overcopy(f64::NAN, 500.0, 0.10, OVERCOPY_TOLERANCE));
    }
    #[test]
    fn it_scales_with_the_LANE_not_a_hardcoded_fraction() {
        assert!(is_overcopy(1000.0, 250.0, 0.10, OVERCOPY_TOLERANCE));
        assert!(! is_overcopy(1000.0, 250.0, 0.20, OVERCOPY_TOLERANCE));
    }
    #[test]
    fn exactly_at_tolerance_does_NOT_trip() {
        assert!(! is_overcopy(1000.0, 150.0, 0.10, 1.5), "boundary is inclusive-safe");
        assert!(is_overcopy(1000.0, 150.01, 0.10, 1.5));
    }
}
