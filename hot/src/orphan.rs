#[derive(Debug, Clone, PartialEq)]
pub struct Holding {
    pub lane: String,
    pub token: String,
    pub ours: f64,
    pub his: f64,
    pub reserved: f64,
    pub legacy: f64,
}
impl Holding {
    pub fn strandable(&self) -> f64 {
        (self.ours - self.reserved).max(0.0)
    }
    fn he_is_out(&self) -> bool {
        (self.his - self.legacy).max(0.0) <= 1e-9
    }
}
#[derive(Debug, Clone, PartialEq)]
pub enum Sweep {
    Idle,
    Fire(Vec<Holding>),
    Refused { why: String, would_have_fired: usize, held: usize },
}
pub const MIN_SHARES: f64 = 1.0;
pub const MAX_ORPHAN_FRAC: f64 = 0.34;
pub const FRAC_APPLIES_ABOVE: usize = 5;
pub const MAX_PER_SWEEP: usize = 3;
pub fn plan(rows: &[Holding], min_shares: f64) -> Sweep {
    let held: Vec<&Holding> = rows.iter().filter(|h| h.ours > 1e-9).collect();
    if held.is_empty() {
        return Sweep::Idle;
    }
    let mut orphans: Vec<Holding> = held
        .iter()
        .filter(|h| h.he_is_out() && h.strandable() >= min_shares)
        .map(|h| (*h).clone())
        .collect();
    if orphans.is_empty() {
        return Sweep::Idle;
    }
    if held.len() > FRAC_APPLIES_ABOVE {
        let frac = orphans.len() as f64 / held.len() as f64;
        if frac >= MAX_ORPHAN_FRAC {
            return Sweep::Refused {
                why: format!(
                    "{}/{} held positions ({:.0}%) look orphaned at once — that is \
                              a stale or unseeded view of his book, not {} exits",
                    orphans.len(), held.len(), frac * 100.0, orphans.len()
                ),
                would_have_fired: orphans.len(),
                held: held.len(),
            };
        }
    }
    orphans
        .sort_by(|a, b| {
            b
                .strandable()
                .partial_cmp(&a.strandable())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    orphans.truncate(MAX_PER_SWEEP);
    Sweep::Fire(orphans)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn h(lane: &str, token: &str, ours: f64, his: f64) -> Holding {
        Holding {
            lane: lane.into(),
            token: token.into(),
            ours,
            his,
            reserved: 0.0,
            legacy: 0.0,
        }
    }
    #[test]
    fn a_position_he_has_fully_left_is_fired() {
        match plan(
            &[h("example_lane_26", "T", 40.0, 0.0), h("example_lane_26", "U", 10.0, 500.0)],
            MIN_SHARES,
        ) {
            Sweep::Fire(v) => {
                assert_eq!(v.len(), 1);
                assert_eq!(v[0].token, "T");
            }
            s => panic!("expected a fire, got {s:?}"),
        }
    }
    #[test]
    fn a_position_he_STILL_HOLDS_is_never_touched() {
        assert_eq!(plan(& [h("example_lane_26", "T", 40.0, 500.0)], MIN_SHARES), Sweep::Idle);
    }
    #[test]
    fn shares_already_ON_THEIR_WAY_OUT_are_not_fired_again() {
        let mut r = h("example_lane_26", "T", 40.0, 0.0);
        r.reserved = 40.0;
        assert_eq!(plan(& [r], MIN_SHARES), Sweep::Idle);
    }
    #[test]
    fn only_the_FREE_remainder_is_fired_when_a_sell_is_partly_in_flight() {
        let mut r = h("example_lane_26", "T", 40.0, 0.0);
        r.reserved = 25.0;
        match plan(&[r], MIN_SHARES) {
            Sweep::Fire(v) => assert!((v[0].strandable() - 15.0).abs() < 1e-9),
            s => panic!("expected a fire, got {s:?}"),
        }
    }
    #[test]
    fn his_LEGACY_shares_do_not_count_as_him_still_being_there() {
        let mut r = h("example_lane_26", "T", 40.0, 300.0);
        r.legacy = 300.0;
        assert!(matches!(plan(& [r], MIN_SHARES), Sweep::Fire(_)));
    }
    #[test]
    fn DUST_is_left_to_the_dust_sweeper() {
        assert_eq!(plan(& [h("example_lane_26", "T", 0.4, 0.0)], MIN_SHARES), Sweep::Idle);
    }
    #[test]
    fn a_MASS_orphan_is_REFUSED_because_it_describes_our_data_not_his_trading() {
        let rows: Vec<Holding> = (0..10)
            .map(|i| h("example_lane_26", &format!("T{i}"), 40.0, 0.0))
            .collect();
        match plan(&rows, MIN_SHARES) {
            Sweep::Refused { would_have_fired, held, why } => {
                assert_eq!((would_have_fired, held), (10, 10));
                assert!(why.contains("his book"), "the reason must name the real cause");
            }
            s => panic!("a whole-book orphan must REFUSE, got {s:?}"),
        }
    }
    #[test]
    fn a_MINORITY_of_orphans_still_fires() {
        let mut rows: Vec<Holding> = (0..8)
            .map(|i| h("example_lane_26", &format!("T{i}"), 40.0, 500.0))
            .collect();
        rows.push(h("example_lane_26", "X", 40.0, 0.0));
        rows.push(h("example_lane_26", "Y", 40.0, 0.0));
        assert!(matches!(plan(& rows, MIN_SHARES), Sweep::Fire(v) if v.len() == 2));
    }
    #[test]
    fn the_fraction_guard_does_not_apply_to_a_TINY_book() {
        let rows = vec![h("example_lane_26", "T", 40.0, 0.0), h("example_lane_26", "U", 40.0, 500.0)];
        assert!(matches!(plan(& rows, MIN_SHARES), Sweep::Fire(v) if v.len() == 1));
    }
    #[test]
    fn a_single_sweep_is_CAPPED_and_spends_itself_on_the_biggest() {
        let mut rows: Vec<Holding> = (0..16)
            .map(|i| h("m", &format!("T{i}"), 10.0, 9.0))
            .collect();
        for (i, sz) in [("A", 5.0), ("B", 90.0), ("C", 50.0), ("D", 70.0)] {
            rows.push(h("m", i, sz, 0.0));
        }
        match plan(&rows, MIN_SHARES) {
            Sweep::Fire(v) => {
                assert_eq!(v.len(), MAX_PER_SWEEP);
                assert_eq!(
                    v.iter().map(| x | x.token.as_str()).collect::< Vec < _ >> (),
                    vec!["B", "D", "C"], "largest first"
                );
            }
            s => panic!("expected a capped fire, got {s:?}"),
        }
    }
    #[test]
    fn an_empty_or_zero_book_is_idle_not_refused() {
        assert_eq!(plan(& [], MIN_SHARES), Sweep::Idle);
        assert_eq!(plan(& [h("example_lane_26", "T", 0.0, 0.0)], MIN_SHARES), Sweep::Idle);
    }
}
