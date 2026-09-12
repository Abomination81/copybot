#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LeaderStats {
    pub max_order_usd: f64,
    pub peak_exposure_usd: f64,
}
impl LeaderStats {
    pub fn validate(&self, lane: &str) -> Result<(), String> {
        for (label, v) in [
            ("leader_max_order_usd", self.max_order_usd),
            ("leader_peak_exposure_usd", self.peak_exposure_usd),
        ] {
            if !v.is_finite() || v <= 0.0 {
                return Err(
                    format!(
                        "lane {lane}: {label} must be a positive measured dollar amount, got {v}. \
                     Measure it from the leader's own order flow (tools/leader_stats.py); \
                     do NOT copy another lane's number."
                    ),
                );
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Caps {
    pub max_open_usd: f64,
    pub per_market_usd: f64,
    pub max_usd_per_fill: f64,
    pub daily_usd: f64,
}
#[derive(Debug, Clone, Copy)]
pub struct Fracs {
    pub open: f64,
    pub per_market: f64,
    pub per_fill: f64,
    pub daily: f64,
}
impl Default for Fracs {
    fn default() -> Self {
        Self {
            open: 1.0,
            per_market: 1.0,
            per_fill: 1.0,
            daily: 10.0,
        }
    }
}
pub fn derive(bankroll_usd: f64, f: &Fracs) -> Caps {
    let max_open_usd = bankroll_usd * f.open;
    let per_market_usd = max_open_usd * f.per_market;
    let max_usd_per_fill = per_market_usd * f.per_fill;
    let daily_usd = max_open_usd * f.daily;
    Caps {
        max_open_usd,
        per_market_usd,
        max_usd_per_fill,
        daily_usd,
    }
}
pub fn validate_fracs(f: &Fracs) -> Result<(), String> {
    for (name, v) in [
        ("open", f.open),
        ("per_market", f.per_market),
        ("per_fill", f.per_fill),
    ] {
        if !(v > 0.0 && v <= 1.0) {
            return Err(
                format!(
                    "budget fraction `{name}_frac` is {v} — must be greater than 0 and at \
                 most 1.0, or the derived caps stop being ordered"
                ),
            );
        }
    }
    if !(f.daily > 0.0 && f.daily.is_finite() && f.daily <= 1000.0) {
        return Err(
            format!(
                "budget fraction `daily_frac` is {} — must be greater than 0 and finite (it is a \
             TURNOVER multiple of the open cap, so values above 1.0 are expected)",
                f.daily
            ),
        );
    }
    Ok(())
}
pub fn virtual_bankroll(seed: f64, realised_pnl: f64) -> f64 {
    if !seed.is_finite() || seed <= 0.0 || !realised_pnl.is_finite() {
        return seed.max(0.0);
    }
    (seed + realised_pnl).max(0.0)
}
pub fn wallet_equity(
    free_cash: Option<f64>,
    marked_positions: Option<f64>,
) -> Option<f64> {
    match (free_cash, marked_positions) {
        (Some(c), Some(m)) if c.is_finite() && m.is_finite() => Some(c + m.max(0.0)),
        _ => None,
    }
}
pub fn lane_available(bankroll: f64, deployed: f64) -> f64 {
    if !bankroll.is_finite() || !deployed.is_finite() {
        return 0.0;
    }
    (bankroll - deployed.max(0.0)).max(0.0)
}
pub fn buy_fits(available: f64, free_cash: Option<f64>, cost: f64) -> bool {
    buy_fits_aged(available, free_cash, cost, Some(0))
}
pub const CASH_STALE_SECS: i64 = 300;
pub fn buy_fits_aged(
    available: f64,
    free_cash: Option<f64>,
    cost: f64,
    last_good_age_secs: Option<i64>,
) -> bool {
    if !available.is_finite() || !cost.is_finite() {
        return false;
    }
    if free_cash.is_some_and(|c| !c.is_finite()) {
        return false;
    }
    if !(cost > 0.0) {
        return false;
    }
    let eps = 1e-6;
    if available + eps < cost {
        return false;
    }
    if !matches!(last_good_age_secs, Some(age) if age <= CASH_STALE_SECS) {
        return false;
    }
    match free_cash {
        Some(c) => c + eps >= cost,
        None => true,
    }
}
pub fn seeds_fit_wallet(
    total_seed: f64,
    physical_usd: Option<f64>,
) -> Result<(), String> {
    let Some(physical) = physical_usd else { return Ok(()) };
    if total_seed > physical + 1e-6 {
        return Err(
            format!(
                "lane seeds total ${total_seed:.2} but the wallet holds ${physical:.2} — \
             the virtual budgets cannot exceed the one wallet they share. Lower a seed \
             by ${:.2}, or fund the wallet.",
                total_seed - physical
            ),
        );
    }
    Ok(())
}
pub fn bankroll_scale(live: f64, seed: f64) -> f64 {
    if !seed.is_finite() || seed <= 0.0 || !live.is_finite() || live < 0.0 {
        return 1.0;
    }
    live / seed
}
pub const MAX_SCALE_STEP: f64 = 0.25;
pub fn slew(old: f64, new: f64) -> f64 {
    if !old.is_finite() || old <= 0.0 || !new.is_finite() || new < 0.0 {
        return old;
    }
    if new <= old {
        return new;
    }
    new.min(old * (1.0 + MAX_SCALE_STEP))
}
pub const MAX_EFFECTIVE_PCT: f64 = 0.05;
pub const COMPOUND_HEADROOM: f64 = 3.0;
pub fn effective_ceiling(configured: f64, explicit: f64) -> f64 {
    if explicit.is_finite() && explicit > configured {
        explicit
    } else {
        configured * COMPOUND_HEADROOM
    }
}
pub fn effective_pct(configured: f64, scale: f64, ceiling: f64, compound: bool) -> f64 {
    let s = if compound && scale.is_finite() && scale >= 0.0 { scale } else { 1.0 };
    let cap = if ceiling.is_finite() && ceiling > 0.0 {
        ceiling
    } else {
        MAX_EFFECTIVE_PCT
    };
    (configured * s).min(cap)
}
pub fn max_safe_pct(caps: &Caps, leader: &LeaderStats) -> f64 {
    let by_fill = caps.max_usd_per_fill / leader.max_order_usd;
    let by_exposure = caps.max_open_usd / leader.peak_exposure_usd;
    by_fill.min(by_exposure)
}
pub fn check_pct_fits(
    lane: &str,
    pct: f64,
    caps: &Caps,
    leader: &LeaderStats,
) -> Result<(), String> {
    leader.validate(lane)?;
    let fits = |need: f64, cap: f64| need <= cap * (1.0 + 1e-9);
    let need_fill = pct * leader.max_order_usd;
    if !fits(need_fill, caps.max_usd_per_fill) {
        let fill_bound = caps.max_usd_per_fill / leader.max_order_usd;
        return Err(
            format!(
                "lane {lane}: at {:.3}% his largest order (${:.0}) needs a ${:.2} clip, but \
             max_usd_per_fill derives to ${:.2}. His biggest orders are his \
             highest-conviction ones — clamping them is an anti-signal filter, not a risk \
             control. Raise bankroll_usd to ${:.0}, or lower pct to {:.3}%.",
                pct * 100.0, leader.max_order_usd, need_fill, caps.max_usd_per_fill,
                bankroll_for_fill(pct, & Fracs::default(), leader), fill_bound * 100.0
            ),
        );
    }
    Ok(())
}
pub fn open_cap_advisory(
    lane: &str,
    pct: f64,
    caps: &Caps,
    leader: &LeaderStats,
) -> Option<String> {
    let need_open = pct * leader.peak_exposure_usd;
    if need_open <= caps.max_open_usd * (1.0 + 1e-9) {
        return None;
    }
    Some(
        format!(
            "lane {lane}: at {:.3}% a full mirror of his peak exposure (${:.0}) would need \
         ${:.0} open, but the open cap is ${:.0}. Buys will PAUSE once open capital hits \
         the cap (exits keep mirroring); add capital to lift it. Full-mirror fit is {:.3}%.",
            pct * 100.0, leader.peak_exposure_usd, need_open, caps.max_open_usd,
            max_safe_pct(caps, leader) * 100.0
        ),
    )
}
pub fn bankroll_for_pct(pct: f64, f: &Fracs, leader: &LeaderStats) -> f64 {
    let by_exposure = pct * leader.peak_exposure_usd / f.open;
    by_exposure.max(bankroll_for_fill(pct, f, leader))
}
pub fn bankroll_for_fill(pct: f64, f: &Fracs, leader: &LeaderStats) -> f64 {
    pct * leader.max_order_usd / (f.open * f.per_market * f.per_fill)
}
pub fn target_shares(
    leader_size: f64,
    leader_avg_price: f64,
    our_avg_price: f64,
    pct: f64,
    scale: f64,
    min_order_usd: f64,
    max_effective_pct: f64,
    compound: bool,
) -> Option<f64> {
    if leader_size <= 1e-9 {
        return Some(0.0);
    }
    if leader_avg_price <= 0.0 || our_avg_price <= 0.0 || pct <= 0.0 || scale <= 0.0 {
        return None;
    }
    let effective = effective_pct(pct, scale, max_effective_pct, compound);
    let proportional = leader_size * leader_avg_price * effective / our_avg_price;
    let venue_floor = min_order_usd.max(0.0) / our_avg_price;
    Some(proportional.max(venue_floor) + 1.0)
}
#[cfg(test)]
mod tests {
    use super::*;
    const EPS: f64 = 1e-6;
    #[test]
    fn PRICE_DIVERGENCE_is_where_the_two_old_formulas_split() {
        let share_ratio = 1000.0 * 0.015;
        let t = target_shares(1000.0, 0.30, 0.60, 0.015, 1.0, 0.0, 1.0, false).unwrap();
        assert!((t - (7.5 + 1.0)).abs() < 1e-9, "got {t}");
        assert!(t < share_ratio, "the two really do disagree, and by a lot");
    }
    #[test]
    fn EQUAL_prices_reduce_to_the_share_ratio() {
        let t = target_shares(1000.0, 0.50, 0.50, 0.015, 1.0, 0.0, 1.0, false).unwrap();
        assert!((t - (15.0 + 1.0)).abs() < 1e-9, "got {t}");
    }
    #[test]
    fn a_FLAT_leader_targets_ZERO_not_none() {
        assert_eq!(target_shares(0.0, 0.5, 0.5, 0.015, 1.0, 1.0, 1.0, false), Some(0.0));
    }
    #[test]
    fn MISSING_prices_produce_NO_answer_rather_than_a_guess() {
        assert_eq!(target_shares(100.0, 0.0, 0.5, 0.015, 1.0, 0.0, 1.0, false), None);
        assert_eq!(target_shares(100.0, 0.5, 0.0, 0.015, 1.0, 0.0, 1.0, false), None);
        assert_eq!(target_shares(100.0, 0.5, 0.5, 0.0, 1.0, 0.0, 1.0, false), None);
        assert_eq!(target_shares(100.0, 0.5, 0.5, 0.015, 0.0, 0.0, 1.0, false), None);
    }
    #[test]
    fn the_VENUE_FLOOR_keeps_the_target_sellable() {
        let t = target_shares(1.0, 0.5, 0.5, 0.015, 1.0, 1.0, 1.0, false).unwrap();
        assert!(t >= 1.0 / 0.5, "must reach the $1 minimum: got {t}");
    }
    #[test]
    fn COMPOUNDING_OFF_ignores_the_scale_for_both_callers() {
        let off = target_shares(1000.0, 0.5, 0.5, 0.015, 2.0, 0.0, 1.0, false).unwrap();
        let on = target_shares(1000.0, 0.5, 0.5, 0.015, 2.0, 0.0, 1.0, true).unwrap();
        assert!((off - 16.0).abs() < 1e-9, "a fixed-rate lane must ignore scale: {off}");
        assert!(on > off, "a compounding lane must not");
    }
    #[test]
    fn the_ordering_invariant_holds_BY_CONSTRUCTION() {
        let mut checked = 0;
        for bankroll in [0.01, 1.0, 716.0, 10_000.0, 1e6, 1e9] {
            for open in [0.01, 0.5, 0.85, 1.0] {
                for per_market in [0.01, 0.6, 1.0] {
                    for per_fill in [0.01, 0.4167, 1.0] {
                        let f = Fracs {
                            open,
                            per_market,
                            per_fill,
                            daily: 0.7,
                        };
                        let c = derive(bankroll, &f);
                        assert!(
                            c.max_usd_per_fill <= c.per_market_usd + EPS,
                            "fill {} > market {} at bankroll {bankroll}", c
                            .max_usd_per_fill, c.per_market_usd
                        );
                        assert!(
                            c.per_market_usd <= c.max_open_usd + EPS,
                            "market {} > open {} at bankroll {bankroll}", c
                            .per_market_usd, c.max_open_usd
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(
            checked, 6 * 4 * 3 * 3, "the sweep did not run the cases it claims to"
        );
    }
    #[test]
    fn default_fracs_are_THE_SEED_ITSELF_with_no_daily_throttle() {
        let seed = 40_000.0;
        let c = derive(seed, &Fracs::default());
        assert!(
            (c.max_open_usd - seed).abs() < 0.01,
            "a $40k seed must permit $40k of exposure, got {}", c.max_open_usd
        );
        assert!(
            (c.per_market_usd - seed).abs() < 0.01,
            "one market may take the whole seed, got {}", c.per_market_usd
        );
        assert!(
            (c.max_usd_per_fill - seed).abs() < 0.01,
            "one ORDER may take the whole seed, got {}", c.max_usd_per_fill
        );
        assert!(
            c.daily_usd > c.max_open_usd * 5.0,
            "daily {} must be far above the open cap {} or turnover throttles the copy \
again",
            c.daily_usd, c.max_open_usd
        );
        assert!(
            validate_fracs(& Fracs::default()).is_ok(),
            "the shipped defaults must be legal"
        );
    }
    const example_lane_26: LeaderStats = LeaderStats {
        max_order_usd: 9_600.0,
        peak_exposure_usd: 390_000.0,
    };
    #[test]
    fn the_measured_numbers_agree_across_BOTH_recorded_rows() {
        assert!((0.0055 * example_lane_26.max_order_usd - 52.80).abs() < 0.01);
        assert!((0.015 * example_lane_26.max_order_usd - 144.0).abs() < 0.01);
        assert!((0.0055 * example_lane_26.peak_exposure_usd - 2_145.0).abs() < 1.0);
        assert!((0.015 * example_lane_26.peak_exposure_usd - 5_850.0).abs() < 1.0);
    }
    #[test]
    fn CHANGING_BANKROLL_MOVES_EVERY_CAP_TOGETHER() {
        let f = Fracs::default();
        let small = derive(1_000.0, &f);
        let big = derive(10_000.0, &f);
        assert!((big.max_open_usd / small.max_open_usd - 10.0).abs() < EPS);
        assert!((big.per_market_usd / small.per_market_usd - 10.0).abs() < EPS);
        assert!((big.max_usd_per_fill / small.max_usd_per_fill - 10.0).abs() < EPS);
        assert!((big.daily_usd / small.daily_usd - 10.0).abs() < EPS);
    }
    #[test]
    fn a_full_mirror_over_the_open_cap_ADVISES_it_does_not_refuse() {
        let caps = Caps {
            max_open_usd: 1_000.0,
            per_market_usd: 600.0,
            max_usd_per_fill: 250.0,
            daily_usd: 700.0,
        };
        assert!(
            check_pct_fits("example_lane_26", 0.015, & caps, & example_lane_26).is_ok(),
            "an open overage is advisory now, not fatal"
        );
        let adv = open_cap_advisory("example_lane_26", 0.015, &caps, &example_lane_26).expect("should advise");
        assert!(
            adv.contains("5850") || adv.contains("5,850"),
            "quote what a full mirror needs: {adv}"
        );
        assert!(adv.to_lowercase().contains("pause"), "explain that buys pause: {adv}");
    }
    #[test]
    fn the_open_overage_advises_but_the_fill_side_still_gates() {
        let caps = Caps {
            max_open_usd: 1_000.0,
            per_market_usd: 600.0,
            max_usd_per_fill: 250.0,
            daily_usd: 700.0,
        };
        assert!(check_pct_fits("example_lane_26", 0.0055, & caps, & example_lane_26).is_ok());
        assert!(open_cap_advisory("example_lane_26", 0.0055, & caps, & example_lane_26).is_some());
        let ok = derive(
            bankroll_for_pct(0.0055, &Fracs::default(), &example_lane_26),
            &Fracs::default(),
        );
        assert!(check_pct_fits("example_lane_26", 0.0055, & ok, & example_lane_26).is_ok());
        assert!(open_cap_advisory("example_lane_26", 0.0055, & ok, & example_lane_26).is_none());
    }
    #[test]
    fn a_10k_bankroll_fits_1_5_pct_and_ADVISES_above_the_full_mirror_ceiling() {
        let caps = derive(10_000.0, &Fracs::default());
        assert!(
            check_pct_fits("example_lane_26", 0.015, & caps, & example_lane_26).is_ok(),
            "1.5% fits a $10k bankroll"
        );
        assert!(
            check_pct_fits("example_lane_26", 0.03, & caps, & example_lane_26).is_ok(),
            "3% boots; the open cap binds at runtime, not at config time"
        );
        assert!(
            open_cap_advisory("example_lane_26", 0.03, & caps, & example_lane_26).is_some(),
            "3% advises a full-mirror pause"
        );
        assert!(
            check_pct_fits("example_lane_26", 0.05, & caps, & example_lane_26).is_ok(),
            "5% (the operator target) boots"
        );
        assert!(open_cap_advisory("example_lane_26", 0.05, & caps, & example_lane_26).is_some());
        let ceiling = max_safe_pct(&caps, &example_lane_26);
        assert!(
            (ceiling - 0.0256).abs() < 0.0006,
            "full-mirror ceiling ~2.56%, got {ceiling}"
        );
    }
    #[test]
    fn bankroll_for_pct_is_the_INVERSE_of_max_safe_pct() {
        let f = Fracs::default();
        for pct in [0.001, 0.0055, 0.015, 0.02, 0.05] {
            let need = bankroll_for_pct(pct, &f, &example_lane_26);
            let caps = derive(need, &f);
            assert!(
                check_pct_fits("t", pct, & caps, & example_lane_26).is_ok(),
                "bankroll_for_pct({pct}) = {need} does not actually fit {pct}"
            );
            assert!(
                max_safe_pct(& caps, & example_lane_26) >= pct - EPS,
                "round-trip ceiling {} below {pct}", max_safe_pct(& caps, & example_lane_26)
            );
        }
    }
    #[test]
    fn the_binding_constraint_can_be_EITHER_of_the_two() {
        let exposure_bound = derive(10_000.0, &Fracs::default());
        assert!(
            exposure_bound.max_open_usd / example_lane_26.peak_exposure_usd < exposure_bound
            .max_usd_per_fill / example_lane_26.max_order_usd,
            "default fractions should be exposure-bound"
        );
        let f = Fracs {
            open: 1.0,
            per_market: 1.0,
            per_fill: 0.001,
            daily: 0.7,
        };
        let fill_bound = derive(10_000.0, &f);
        assert!(
            fill_bound.max_usd_per_fill / example_lane_26.max_order_usd < fill_bound.max_open_usd /
            example_lane_26.peak_exposure_usd, "a tiny per_fill fraction should be fill-bound"
        );
        let err = check_pct_fits(
                "t",
                max_safe_pct(&fill_bound, &example_lane_26) * 2.0,
                &fill_bound,
                &example_lane_26,
            )
            .unwrap_err();
        assert!(err.contains("clip"), "fill-bound failure should name the clip: {err}");
    }
    #[test]
    fn the_open_advisory_boundary_is_exact() {
        let f = Fracs::default();
        let caps = derive(bankroll_for_pct(0.0055, &f, &example_lane_26), &f);
        let need = 0.0055 * example_lane_26.peak_exposure_usd;
        assert!(
            (need - caps.max_open_usd).abs() < 1e-6,
            "only meaningful ON the boundary: {need} vs {}", caps.max_open_usd
        );
        assert!(
            open_cap_advisory("t", 0.0055, & caps, & example_lane_26).is_none(),
            "exact fit needs no advisory"
        );
        let over = Caps {
            max_open_usd: caps.max_open_usd - 1.0,
            ..caps
        };
        assert!(
            open_cap_advisory("t", 0.0055, & over, & example_lane_26).is_some(),
            "a real $1 shortfall advises"
        );
        assert!(
            check_pct_fits("t", 0.0055, & over, & example_lane_26).is_ok(), "and it is never fatal"
        );
    }
    #[test]
    fn regression_a_ceiling_equal_to_the_rate_used_to_KILL_compounding() {
        let pct = 0.05;
        let scale = 1.0211;
        assert_eq!(
            effective_pct(pct, scale, 0.05, true), 0.05,
            "this is the bug: growth is clipped back to the starting rate"
        );
        let ceiling = effective_ceiling(pct, 0.05);
        assert_eq!(ceiling, pct * COMPOUND_HEADROOM);
        let eff = effective_pct(pct, scale, ceiling, true);
        assert!((eff - 0.051055).abs() < 1e-6, "growth must reach the rate, got {eff}");
    }
    #[test]
    fn compounding_OFF_pins_the_lane_to_its_configured_rate() {
        let ceiling = effective_ceiling(0.05, 0.05);
        assert_eq!(
            effective_pct(0.05, 2.0, ceiling, false), 0.05, "off ignores scale entirely"
        );
        assert_eq!(effective_pct(0.05, 0.4, ceiling, false), 0.05, "including a LOSS");
        assert!((effective_pct(0.05, 1.5, ceiling, true) - 0.075).abs() < 1e-12);
        assert!((effective_pct(0.05, 0.4, ceiling, true) - 0.02).abs() < 1e-12);
    }
    #[test]
    fn the_ceiling_still_bounds_runaway_growth() {
        let ceiling = effective_ceiling(0.05, 0.05);
        assert!(
            (effective_pct(0.05, 100.0, ceiling, true) - 0.15).abs() < 1e-12,
            "compounding is bounded, not unlimited"
        );
        assert_eq!(effective_ceiling(0.05, 0.08), 0.08);
        assert_eq!(effective_pct(0.05, 100.0, 0.08, true), 0.08);
    }
    #[test]
    fn seeds_within_the_wallet_pass_and_over_it_fail() {
        assert!(seeds_fit_wallet(9_000.0, Some(10_000.0)).is_ok());
        assert!(
            seeds_fit_wallet(10_000.0, Some(10_000.0)).is_ok(), "exact fit is allowed"
        );
        let e = seeds_fit_wallet(11_000.0, Some(10_000.0)).unwrap_err();
        assert!(e.contains("1000"), "must name the overage: {e}");
    }
    #[test]
    fn an_unknown_wallet_balance_never_blocks_boot() {
        assert!(seeds_fit_wallet(999_999.0, None).is_ok());
    }
    #[test]
    fn each_virtual_bankroll_compounds_on_its_OWN_realised_pnl() {
        assert_eq!(virtual_bankroll(10_000.0, 1_000.0), 11_000.0);
        assert_eq!(virtual_bankroll(10_000.0, - 1_000.0), 9_000.0);
        let a = virtual_bankroll(10_000.0, 600.0);
        let b = virtual_bankroll(10_000.0, -400.0);
        assert_eq!((a, b), (10_600.0, 9_600.0));
        assert_eq!(a + b, 20_200.0);
    }
    #[test]
    fn losses_cannot_produce_a_negative_virtual_bankroll() {
        assert_eq!(virtual_bankroll(100.0, - 150.0), 0.0);
        assert_eq!(virtual_bankroll(0.0, 50.0), 0.0);
        assert_eq!(virtual_bankroll(100.0, f64::NAN), 100.0);
    }
    #[test]
    fn wallet_equity_is_cash_plus_marked_positions_never_cash_alone() {
        assert_eq!(wallet_equity(Some(9363.0), Some(883.0)), Some(10246.0));
        assert_eq!(wallet_equity(Some(19363.0), Some(843.0)), Some(20206.0));
        assert_eq!(wallet_equity(None, Some(500.0)), None);
        assert_eq!(wallet_equity(Some(500.0), None), None);
        assert_eq!(wallet_equity(Some(1000.0), Some(- 50.0)), Some(1000.0));
    }
    #[test]
    fn lane_available_is_bankroll_minus_deployed_and_never_negative() {
        assert_eq!(lane_available(10_000.0, 3_000.0), 7_000.0);
        assert_eq!(
            lane_available(10_000.0, 12_000.0), 0.0, "over-deployed = zero, not a debt"
        );
        assert_eq!(lane_available(f64::NAN, 1.0), 0.0);
    }
    #[test]
    fn a_lane_stops_buying_when_its_VIRTUAL_balance_is_low_even_if_cash_is_plentiful() {
        let available = lane_available(10_000.0, 9_900.0);
        assert!((available - 100.0).abs() < 1e-9);
        assert!(
            ! buy_fits(available, Some(50_000.0), 250.0),
            "virtual balance too low -> STOP buying, regardless of physical cash"
        );
        assert!(buy_fits(available, Some(50_000.0), 80.0));
    }
    #[test]
    fn buying_RESUMES_automatically_when_the_virtual_balance_recovers() {
        let recovered = lane_available(10_000.0, 2_000.0);
        assert!(
            buy_fits(recovered, Some(50_000.0), 250.0),
            "virtual balance recovered -> buying is allowed again"
        );
    }
    #[test]
    fn a_buy_still_needs_real_cash_to_pay_for_it() {
        assert!(! buy_fits(5_000.0, Some(10.0), 250.0), "no cash -> cannot buy");
        assert!(buy_fits(5_000.0, None, 250.0));
        assert!(! buy_fits(5_000.0, Some(5_000.0), 0.0));
    }
    #[test]
    fn scale_is_ONE_when_live_equals_seed_and_tracks_growth_after() {
        assert!((bankroll_scale(10_000.0, 10_000.0) - 1.0).abs() < 1e-9);
        assert!((bankroll_scale(12_500.0, 10_000.0) - 1.25).abs() < 1e-9);
        assert!(
            (bankroll_scale(8_000.0, 10_000.0) - 0.8).abs() < 1e-9,
            "a drawdown must SHRINK the caps — that is deleveraging, and it is correct"
        );
    }
    #[test]
    fn a_broken_reading_falls_back_to_the_CONFIGURED_caps_not_to_zero() {
        assert_eq!(bankroll_scale(10_000.0, 0.0), 1.0);
        assert_eq!(bankroll_scale(f64::NAN, 10_000.0), 1.0);
        assert_eq!(bankroll_scale(- 1.0, 10_000.0), 1.0);
    }
    #[test]
    fn a_profit_jump_cannot_INCREASE_risk_in_one_tick_but_losses_apply_immediately() {
        let capped_up = slew(1.0, 10.0);
        assert!(
            (capped_up - 1.25).abs() < 1e-9,
            "10x read must clamp to +25%, got {capped_up}"
        );
        let deleveraged = slew(1.0, 0.0001);
        assert!(
            (deleveraged - 0.0001).abs() < 1e-9,
            "realised losses must deleverage immediately, got {deleveraged}"
        );
        assert!((slew(1.0, 1.1) - 1.1).abs() < 1e-9);
    }
    #[test]
    fn compounding_still_REACHES_a_large_target_it_is_only_paced() {
        let mut s = 1.0;
        for _ in 0..20 {
            s = slew(s, 3.0);
        }
        assert!((s - 3.0).abs() < 0.01, "should converge to 3.0x, reached {s}");
    }
    #[test]
    fn a_wiped_out_lane_slews_to_ZERO_and_cannot_size_a_trade() {
        let s = slew(1.0, 0.0);
        assert_eq!(s, 0.0, "a zero virtual bankroll must stop sizing immediately");
        assert!(
            effective_pct(0.015, 0.0, MAX_EFFECTIVE_PCT, true).abs() < 1e-12,
            "zero bankroll must mean zero copy percentage, not configured percentage"
        );
    }
    #[test]
    fn a_per_wallet_ceiling_lets_one_lane_size_up_without_touching_the_default() {
        assert_eq!(effective_pct(0.05, 100.0, MAX_EFFECTIVE_PCT, true), 0.05);
        assert_eq!(effective_pct(0.25, 1.0, 0.25, true), 0.25);
        assert_eq!(
            effective_pct(0.25, 100.0, 0.25, true), 0.25,
            "still clamps compounding at its own ceiling"
        );
        assert_eq!(effective_pct(0.25, 1.0, 0.0, true), MAX_EFFECTIVE_PCT);
    }
    #[test]
    fn a_scaled_cap_set_KEEPS_the_ordering_invariant() {
        let caps = derive(10_000.0, &Fracs::default());
        for scale in [0.1f64, 0.75, 1.0, 2.5, 40.0] {
            let (f, m, o) = (
                caps.max_usd_per_fill * scale,
                caps.per_market_usd * scale,
                caps.max_open_usd * scale,
            );
            assert!(
                f <= m + 1e-9 && m <= o + 1e-9,
                "ordering broke at scale {scale}: {f} / {m} / {o}"
            );
        }
    }
    #[test]
    fn illegal_fractions_are_REFUSED() {
        assert!(validate_fracs(& Fracs::default()).is_ok());
        for bad in [0.0, -0.1, 1.5, f64::NAN] {
            let f = Fracs {
                open: bad,
                ..Fracs::default()
            };
            assert!(validate_fracs(& f).is_err(), "open_frac {bad} should be refused");
        }
        let f = Fracs {
            open: 1.0,
            per_market: 1.0,
            per_fill: 1.0,
            daily: 1.0,
        };
        assert!(validate_fracs(& f).is_ok());
    }
    #[test]
    fn a_zero_bankroll_derives_zero_caps_and_refuses_every_pct() {
        let caps = derive(0.0, &Fracs::default());
        assert_eq!(caps.max_open_usd, 0.0);
        assert!(
            check_pct_fits("t", 0.0001, & caps, & example_lane_26).is_err(),
            "a zero bankroll must not permit any copying"
        );
        assert_eq!(max_safe_pct(& caps, & example_lane_26), 0.0);
    }
    #[test]
    fn a_DRAWN_DOWN_lane_stops_buying_even_with_a_flush_wallet() {
        let avail = lane_available(10_000.0, 9_950.0);
        assert!((avail - 50.0).abs() < 1e-9);
        assert!(buy_fits(avail, Some(37_000.0), 40.0), "inside its own budget: allowed");
        assert!(
            ! buy_fits(avail, Some(37_000.0), 60.0),
            "over its own budget must be refused however flush the WALLET is"
        );
    }
    #[test]
    fn a_lane_within_budget_still_cannot_spend_cash_that_is_not_there() {
        assert!(
            ! buy_fits(5_000.0, Some(10.0), 100.0),
            "no real cash means no order, whatever the virtual budget says"
        );
    }
    #[test]
    fn UNKNOWN_cash_is_ADVISORY_and_never_halts_trading() {
        assert!(buy_fits(5_000.0, None, 100.0));
    }
    #[test]
    fn a_FULLY_deployed_lane_has_exactly_zero_and_never_negative() {
        assert_eq!(lane_available(10_000.0, 10_000.0), 0.0);
        assert_eq!(
            lane_available(10_000.0, 12_000.0), 0.0, "over-deployed clamps to zero"
        );
        assert_eq!(
            lane_available(10_000.0, - 5.0), 10_000.0, "nonsense deployed is ignored"
        );
        assert!(! buy_fits(0.0, Some(1e9), 1.0), "zero available buys nothing");
    }
    #[test]
    fn nonfinite_inputs_fail_CLOSED_on_the_buy_side() {
        assert_eq!(lane_available(f64::NAN, 0.0), 0.0);
        assert_eq!(lane_available(10_000.0, f64::NAN), 0.0);
        assert!(! buy_fits(f64::NAN, Some(1e9), 10.0));
        assert!(! buy_fits(1e9, Some(1e9), 0.0), "a zero-cost buy is not a buy");
        assert!(! buy_fits(1e9, Some(1e9), - 5.0));
        assert!(! buy_fits(1e9, Some(f64::NAN), 10.0));
        assert!(! buy_fits(1e9, Some(f64::INFINITY), 10.0));
        assert!(buy_fits(1e9, None, 10.0), "unknown stays advisory");
    }
    #[test]
    fn the_TONIGHT_configuration_can_still_trade() {
        let avail = lane_available(20_000.0, 2_343.44);
        assert!(
            buy_fits(avail, Some(37_799.03), 41.0), "a normal clip must not be blocked"
        );
        assert!(
            buy_fits(avail, Some(37_799.03), 4_250.34), "nor one at the per-fill cap"
        );
    }
}
#[test]
fn a_FRESH_read_keeps_the_old_semantics() {
    assert!(buy_fits_aged(1_000.0, Some(500.0), 100.0, Some(10)));
    assert!(! buy_fits_aged(1_000.0, Some(50.0), 100.0, Some(10)), "cash must cover");
    assert!(
        buy_fits_aged(1_000.0, None, 100.0, Some(10)), "fresh blindness is advisory"
    );
}
#[test]
fn an_EXPIRED_read_refuses_even_a_generous_looking_figure() {
    assert!(
        ! buy_fits_aged(1_000.0, Some(50_000.0), 100.0, Some(CASH_STALE_SECS + 1)),
        "a stale figure is not knowledge, however large"
    );
    assert!(
        buy_fits_aged(1_000.0, Some(50_000.0), 100.0, Some(CASH_STALE_SECS)),
        "at the boundary it still counts"
    );
}
#[test]
fn NEVER_having_read_the_wallet_refuses_buys() {
    assert!(
        ! buy_fits_aged(1_000.0, None, 100.0, None),
        "no good read ever = no basis to spend"
    );
    assert!(
        ! buy_fits_aged(1_000.0, Some(9e9), 100.0, None),
        "a figure with no timestamp is a claim without a date"
    );
}
