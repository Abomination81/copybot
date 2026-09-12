#[inline]
pub fn whole(x: f64) -> f64 {
    (x + 1e-6) as i64 as f64
}
#[inline]
pub fn ceil_shares(x: f64) -> f64 {
    x.ceil()
}
pub const SELL_SHARE_DECIMALS: i32 = 2;
pub const SELL_USDC_DECIMALS: i32 = 5;
#[inline]
pub fn sell_shares(shares: f64) -> f64 {
    let q = 10f64.powi(SELL_SHARE_DECIMALS);
    (shares * q + 1e-9).floor() / q
}
pub const DUST_SHARES: f64 = 1.0;
pub const MIN_VENUE_PRICE: f64 = 0.001;
pub fn buy_limit(limit: f64) -> f64 {
    if !limit.is_finite() || limit <= 0.0 {
        return MIN_VENUE_PRICE;
    }
    let tick = crate::book::tick_size(limit);
    let snapped = (limit / tick).ceil() * tick;
    let decimals = if tick >= 0.01 { 2 } else { 3 };
    let f = 10f64.powi(decimals);
    let out = (snapped * f).round() / f;
    out.clamp(MIN_VENUE_PRICE, 1.0 - MIN_VENUE_PRICE)
}
pub fn sell_limit(limit: f64) -> f64 {
    if !limit.is_finite() || limit <= 0.0 {
        return MIN_VENUE_PRICE;
    }
    let tick = crate::book::tick_size(limit);
    let snapped = (limit / tick).floor() * tick;
    let decimals = if tick >= 0.01 { 2 } else { 3 };
    let f = 10f64.powi(decimals);
    let out = (snapped * f).round() / f;
    out.clamp(MIN_VENUE_PRICE, 1.0 - MIN_VENUE_PRICE)
}
pub const VENUE_MIN_SHARES: f64 = 5.0;
#[inline]
pub fn sell_floor(mark: f64) -> f64 {
    if mark > 0.96 || mark < 0.04 { MIN_VENUE_PRICE } else { 0.01 }
}
pub fn dust_fak_terms(shares: f64, mark: f64) -> (f64, f64) {
    let sh = sell_shares(shares);
    if sh <= 0.0 {
        return (0.0, MIN_VENUE_PRICE);
    }
    let limit = sell_floor(mark);
    let usd_q = 10f64.powi(SELL_USDC_DECIMALS);
    if (sh * limit * usd_q + 1e-9).floor() < 1.0 {
        return (0.0, limit);
    }
    (sh, limit)
}
pub const CLEANUP_MAX_DISCOUNT: f64 = 0.25;
pub fn cleanup_fak_terms(shares: f64, mark: f64) -> (f64, f64) {
    let sh = sell_shares(shares);
    if sh <= 0.0 {
        return (0.0, MIN_VENUE_PRICE);
    }
    if !(mark > 0.0) {
        return dust_fak_terms(shares, mark);
    }
    let raw = (mark * (1.0 - CLEANUP_MAX_DISCOUNT)).max(MIN_VENUE_PRICE).min(mark);
    let tick = crate::book::tick_size(raw);
    let limit = ((raw / tick).floor() * tick).max(MIN_VENUE_PRICE);
    let usd_q = 10f64.powi(SELL_USDC_DECIMALS);
    if (sh * limit * usd_q + 1e-9).floor() < 1.0 {
        return dust_fak_terms(shares, mark);
    }
    (sh, limit)
}
pub fn sell_terms(shares: f64, desired_limit: f64, _band: f64) -> (f64, f64) {
    let sh = sell_shares(shares);
    if sh <= 0.0 {
        return (0.0, desired_limit);
    }
    let usd_q = 10f64.powi(SELL_USDC_DECIMALS);
    if (sh * desired_limit * usd_q + 1e-9).floor() < 1.0 {
        return (0.0, desired_limit);
    }
    (sh, desired_limit)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cleanup_bounds_the_limit_while_dust_stays_permissive() {
        let (_, dust_limit) = dust_fak_terms(500.0, 0.97);
        assert!(
            (dust_limit - MIN_VENUE_PRICE).abs() < 1e-12,
            "the old path really did offer a 0.97 winner at 0.001"
        );
        let (sh, limit) = cleanup_fak_terms(500.0, 0.97);
        assert_eq!(sh, 500.0);
        assert!(
            limit >= 0.97 * (1.0 - CLEANUP_MAX_DISCOUNT) - 0.011,
            "must stay near the mark, got {limit}"
        );
        assert!(limit < 0.97, "but still concede something, got {limit}");
        let (sh_d, l_d) = dust_fak_terms(0.4, 0.50);
        assert_eq!(l_d, 0.01, "sub-share residue still takes any bid");
        let _ = sh_d;
    }
    #[test]
    fn cleanup_limits_land_on_a_venue_tick_and_never_go_sub_floor() {
        for mark in [0.02, 0.05, 0.11, 0.37, 0.55, 0.89, 0.93, 0.99] {
            let (_, limit) = cleanup_fak_terms(100.0, mark);
            assert!(limit >= MIN_VENUE_PRICE, "mark {mark} -> {limit}");
            assert!(
                limit <= mark, "limit must never exceed the mark: {mark} -> {limit}"
            );
            let tick = crate::book::tick_size(limit);
            let rem = (limit / tick).fract();
            assert!(
                rem < 1e-6 || rem > 1.0 - 1e-6,
                "off-tick limit for mark {mark}: {limit} (tick {tick})"
            );
        }
    }
    #[test]
    fn cleanup_falls_back_to_the_permissive_floor_when_it_cannot_encode() {
        let (_, l) = cleanup_fak_terms(100.0, 0.0);
        assert_eq!(l, MIN_VENUE_PRICE);
        let (sh, _) = cleanup_fak_terms(0.02, 0.50);
        let (sh_dust, _) = dust_fak_terms(0.02, 0.50);
        assert_eq!(sh, sh_dust, "a tiny remainder still gets the dust treatment");
    }
    #[test]
    fn whole_truncates_but_survives_the_float_boundary() {
        assert_eq!(whole(5.9), 5.0);
        assert_eq!(whole(4.9999995), 5.0);
        assert_eq!(whole(0.7), 0.0);
    }
    #[test]
    fn sell_terms_obey_the_VENUES_stated_accuracy_rule() {
        for &(sh, px) in &[
            (0.7, 0.20),
            (5.06, 0.85),
            (100.0, 0.62),
            (0.71, 0.99),
            (0.6131, 0.001),
            (221.1788, 0.455),
        ] {
            let (s, l) = sell_terms(sh, px, 0.06);
            if s <= 0.0 {
                continue;
            }
            let sh_dec = s * 100.0;
            assert!(
                (sh_dec - sh_dec.round()).abs() < 1e-9,
                "shares {s} carry more than 2 decimals"
            );
            let usd = s * l * 1e5;
            assert!(
                (usd - usd.round()).abs() < 1e-6, "USDC {} carries more than 5 decimals",
                s * l
            );
            assert!(s <= sh + 1e-9, "never offer more than we hold");
            assert!(l <= px + 1e-9, "never ask above the limit we chose");
        }
    }
    #[test]
    fn dust_can_still_be_sold() {
        let (s, _l) = sell_terms(0.7, 0.20, 0.06);
        assert!(s > 0.0, "a 0.7-share exit must not be abandoned");
    }
    #[test]
    fn sell_terms_never_exceed_what_we_hold() {
        let (s, _) = sell_terms(5.06, 0.85, 0.06);
        assert!(s <= 5.06 + 1e-9);
    }
    #[test]
    fn dust_fak_sells_to_the_venues_REAL_share_granularity() {
        let (s, p) = dust_fak_terms(0.781_817, 0.13);
        assert_eq!(
            s, 0.78, "2dp of shares is the venue's granularity, not a cents hunt"
        );
        assert!(
            (p - 0.01).abs() < 1e-9,
            "cleanup must permit the full downside range, got {p}"
        );
        assert!(0.781_817 - s < 0.01, "residue must be under one share-tick");
    }
    #[test]
    fn dust_fak_never_oversells() {
        let (s, _) = dust_fak_terms(808.3783, 0.13);
        assert!(s <= 808.3783 + 1e-9, "offered {s} against a holding of 808.3783");
        assert_eq!(s, 808.37);
    }
    #[test]
    fn dust_below_the_share_tick_is_REFUSED_not_sent() {
        assert_eq!(dust_fak_terms(0.009, 0.99).0, 0.0);
        assert_eq!(dust_fak_terms(0.0, 0.99).0, 0.0);
    }
    #[test]
    fn the_three_REAL_stuck_positions_all_become_sellable() {
        for (held, expect) in [(0.6131f64, 0.61), (0.3783, 0.37), (0.6000, 0.60)] {
            let (s, _) = dust_fak_terms(held, 0.001);
            assert_eq!(s, expect, "holding {held} should now sell {expect}");
            assert!(held - s < 0.01);
        }
    }
    #[test]
    fn cleanup_floor_tracks_the_venues_dynamic_tick_regime() {
        assert_eq!(sell_floor(0.50), 0.01);
        assert_eq!(sell_floor(0.039), 0.001);
        assert_eq!(sell_floor(0.961), 0.001);
    }
}
#[cfg(test)]
mod buy_limit_tests {
    use super::*;
    #[test]
    fn the_2026_08_17_REJECTION_is_now_impossible() {
        let out = buy_limit(0.0616666666666667);
        assert_eq!(out, 0.07, "must snap onto the venue grid, got {out}");
        assert!(on_grid(out), "{out} must be a valid venue price");
    }
    fn on_grid(p: f64) -> bool {
        let tick = crate::book::tick_size(p);
        let n = p / tick;
        (n - n.round()).abs() < 1e-6
    }
    #[test]
    fn EVERY_price_across_the_range_lands_on_the_grid() {
        let mut p = 0.0005_f64;
        while p < 1.0 {
            let out = buy_limit(p);
            assert!(on_grid(out), "buy_limit({p}) = {out} is not on the venue grid");
            let ceiling = 1.0 - MIN_VENUE_PRICE;
            assert!(
                out >= p - 1e-9 || out >= ceiling - 1e-9,
                "buy_limit({p}) = {out} rounded DOWN below the ceiling — it may not fill"
            );
            assert!(
                out >= MIN_VENUE_PRICE && out < 1.0,
                "buy_limit({p}) = {out} out of range"
            );
            p += 0.0007;
        }
    }
    #[test]
    fn a_price_ALREADY_on_the_grid_is_left_alone() {
        for p in [0.01_f64, 0.05, 0.25, 0.50, 0.75, 0.99] {
            assert!(
                (buy_limit(p) - p).abs() < 1e-9,
                "{p} was already valid and must not move"
            );
        }
    }
    #[test]
    fn float_dust_never_survives_the_rounding() {
        for p in [0.0616666666666667_f64, 0.0700000001, 0.123456789, 0.9612345] {
            let out = buy_limit(p);
            let s = format!("{out}");
            assert!(s.len() <= 6, "buy_limit({p}) = {s} still carries float dust");
        }
    }
    #[test]
    fn a_nonsense_limit_degrades_to_the_venue_floor_not_a_panic() {
        for p in [0.0_f64, -1.0, f64::NAN, f64::INFINITY] {
            let out = buy_limit(p);
            assert!(out.is_finite() && out >= MIN_VENUE_PRICE, "buy_limit({p}) = {out}");
        }
    }
}
