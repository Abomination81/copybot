use std::time::Duration;
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RestVerdict {
    Corroborated,
    Uncertain,
    HeCancelled,
}
pub const DEFICIT_TOLERANCE: f64 = 0.10;
pub const FEED_LIVENESS_WINDOW: Duration = Duration::from_secs(60);
pub const MAX_REST_AGE_SECS: i64 = 24 * 60 * 60;
pub const MAX_REST_AGE_BUY_SECS: i64 = 4 * 60 * 60;
#[derive(Debug, Clone, Copy)]
pub struct Evidence {
    pub anchor_remaining: f64,
    pub anchor_printed: f64,
    pub anchor_contested: bool,
    pub level_now: Option<f64>,
    pub printed_now: f64,
    pub feed_alive: bool,
}
pub fn classify(e: &Evidence) -> RestVerdict {
    if !(e.anchor_remaining > 0.0) || !e.anchor_remaining.is_finite() {
        return RestVerdict::Uncertain;
    }
    if e.anchor_contested {
        return RestVerdict::Uncertain;
    }
    let Some(level) = e.level_now else {
        return RestVerdict::Uncertain;
    };
    if !level.is_finite() || !e.printed_now.is_finite() || !e.anchor_printed.is_finite()
    {
        return RestVerdict::Uncertain;
    }
    if e.printed_now < e.anchor_printed {
        return RestVerdict::Uncertain;
    }
    let printed_since = e.printed_now - e.anchor_printed;
    let expected = (e.anchor_remaining - printed_since).max(0.0);
    let tol = e.anchor_remaining * DEFICIT_TOLERANCE;
    if level + tol >= expected {
        return RestVerdict::Corroborated;
    }
    if !e.feed_alive {
        return RestVerdict::Uncertain;
    }
    RestVerdict::HeCancelled
}
pub fn renewable(verdict: RestVerdict, age_secs: i64, side: u8) -> bool {
    verdict == RestVerdict::Corroborated && age_secs < max_age_for(side)
}
pub fn max_age_for(side: u8) -> i64 {
    if side == 0 { MAX_REST_AGE_BUY_SECS } else { MAX_REST_AGE_SECS }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn anchored(level_now: Option<f64>, printed_now: f64) -> Evidence {
        Evidence {
            anchor_remaining: 4600.0,
            anchor_printed: 0.0,
            anchor_contested: false,
            level_now,
            printed_now,
            feed_alive: true,
        }
    }
    use crate::book::{Levels, TradePrints};
    use crate::resting::{Resting, RestingBook};
    fn rest(side: u8, his_rem: f64, printed: f64, contested: bool) -> Resting {
        Resting {
            order_id: "0x1".into(),
            lane: "example_lane_26".into(),
            token: "T".into(),
            side,
            limit: 0.63,
            shares: 100.0,
            placed: 0,
            salt: [1u8; 32],
            condition: [2u8; 32],
            his_price: 0.62,
            his_remaining: his_rem,
            printed_at_place: printed,
            contested_at_place: contested,
        }
    }
    fn ev(o: &Resting, l: &Levels, p: &TradePrints) -> RestVerdict {
        classify(
            &Evidence {
                anchor_remaining: o.his_remaining,
                anchor_printed: o.printed_at_place,
                anchor_contested: o.contested_at_place,
                level_now: l.size_at(&o.token, o.his_price),
                printed_now: p.cumulative_at(&o.token, o.his_price),
                feed_alive: l.feed_alive(FEED_LIVENESS_WINDOW),
            },
        )
    }
    #[test]
    fn FINAL_a_pruned_token_cannot_manufacture_a_cancel() {
        let (l, p) = (Levels::new(), TradePrints::new());
        l.put("T", 0.62, 4600.0);
        p.record("T", 0.62, 3000.0);
        let o = rest(0, 4600.0, 3000.0, false);
        let keep = std::collections::HashSet::new();
        l.retain_tokens(&keep);
        p.retain_tokens(&keep);
        assert_eq!(
            ev(& o, & l, & p), RestVerdict::Uncertain,
            "wiped evidence must never pull a healthy rest"
        );
    }
    #[test]
    fn FINAL_a_SELL_rest_is_judged_on_HIS_level_not_ours() {
        let (l, p) = (Levels::new(), TradePrints::new());
        l.put("T", 0.61, 0.0);
        l.put("T", 0.62, 4600.0);
        let o = rest(1, 4600.0, 0.0, false);
        assert_eq!(ev(& o, & l, & p), RestVerdict::Corroborated);
    }
    #[test]
    fn FINAL_the_per_side_caps_bracket_the_blind_TTL() {
        const BLIND_TTL: i64 = 1_800;
        assert!(max_age_for(0) > BLIND_TTL, "a corroborated BUY must outlive the timer");
        assert!(max_age_for(1) > BLIND_TTL, "and a SELL more so");
        assert!(max_age_for(1) > max_age_for(0), "buys hold budget, sells do not");
    }
    #[test]
    fn FINAL_a_rest_with_no_anchor_can_never_be_renewed() {
        let (l, p) = (Levels::new(), TradePrints::new());
        l.put("T", 0.62, 4600.0);
        let o = rest(0, 0.0, 0.0, true);
        assert_eq!(ev(& o, & l, & p), RestVerdict::Uncertain);
        assert!(! renewable(ev(& o, & l, & p), 60, 0));
    }
    #[test]
    fn FINAL_settlement_claim_survives_the_0x_forms_used_by_each_caller() {
        let b = RestingBook::new();
        assert!(b.try_claim_settle("abc"));
        assert!(! b.try_claim_settle("0xabc"), "venue form is the SAME order");
        b.finish_settle("0xabc");
        assert!(b.try_claim_settle("abc"), "and either form releases it");
    }
    #[test]
    fn THE_AUDIT_CASE_a_FULL_cancel_is_now_detected() {
        assert_eq!(classify(& anchored(Some(0.0), 0.0)), RestVerdict::HeCancelled);
    }
    #[test]
    fn THE_AUDIT_CASE_a_PARTIAL_cancel_is_now_detected() {
        assert_eq!(classify(& anchored(Some(2000.0), 0.0)), RestVerdict::HeCancelled);
    }
    #[test]
    fn a_deficit_EXPLAINED_BY_PRINTS_is_a_fill_not_a_cancel() {
        assert_eq!(
            classify(& anchored(Some(2600.0), 2000.0)), RestVerdict::Corroborated
        );
        assert_eq!(classify(& anchored(Some(0.0), 4600.0)), RestVerdict::Corroborated);
    }
    #[test]
    fn prints_that_only_PARTLY_cover_the_deficit_still_read_as_cancel() {
        assert_eq!(classify(& anchored(Some(500.0), 500.0)), RestVerdict::HeCancelled);
    }
    #[test]
    fn his_size_still_at_the_level_corroborates() {
        assert_eq!(classify(& anchored(Some(4600.0), 0.0)), RestVerdict::Corroborated);
        assert_eq!(
            classify(& anchored(Some(9000.0), 0.0)), RestVerdict::Corroborated,
            "more size than he had is not a deficit"
        );
        assert_eq!(
            classify(& anchored(Some(4200.0), 0.0)), RestVerdict::Corroborated,
            "an 8.7% dip is inside tolerance, not a cancel"
        );
    }
    #[test]
    fn a_DEAD_market_data_feed_never_produces_a_cancel_verdict() {
        let mut e = anchored(Some(0.0), 0.0);
        e.feed_alive = false;
        assert_eq!(classify(& e), RestVerdict::Uncertain);
    }
    #[test]
    fn a_CONTESTED_level_decides_nothing_in_either_direction() {
        let mut cancel_shaped = anchored(Some(0.0), 0.0);
        cancel_shaped.anchor_contested = true;
        assert_eq!(classify(& cancel_shaped), RestVerdict::Uncertain);
        let mut ok_shaped = anchored(Some(4600.0), 0.0);
        ok_shaped.anchor_contested = true;
        assert_eq!(classify(& ok_shaped), RestVerdict::Uncertain);
    }
    #[test]
    fn a_missing_anchor_or_a_missing_level_is_Uncertain() {
        let mut none = anchored(Some(0.0), 0.0);
        none.anchor_remaining = 0.0;
        assert_eq!(classify(& none), RestVerdict::Uncertain);
        assert_eq!(classify(& anchored(None, 0.0)), RestVerdict::Uncertain);
        let mut nan = anchored(Some(0.0), 0.0);
        nan.anchor_remaining = f64::NAN;
        assert_eq!(classify(& nan), RestVerdict::Uncertain);
        assert_eq!(classify(& anchored(Some(f64::NAN), 0.0)), RestVerdict::Uncertain);
    }
    #[test]
    fn a_print_counter_that_went_BACKWARDS_is_Uncertain_not_a_cancel() {
        let mut e = anchored(Some(0.0), 0.0);
        e.anchor_printed = 5000.0;
        assert_eq!(classify(& e), RestVerdict::Uncertain);
    }
    #[test]
    fn ADV_a_contested_level_where_a_STRANGER_leaves_must_not_pull_our_rest() {
        let e = Evidence {
            anchor_remaining: 4600.0,
            anchor_printed: 0.0,
            anchor_contested: true,
            level_now: Some(4600.0),
            printed_now: 0.0,
            feed_alive: true,
        };
        assert_eq!(classify(& e), RestVerdict::Uncertain);
    }
    #[test]
    fn ADV_his_order_growing_does_not_produce_a_deficit() {
        let e = Evidence {
            anchor_remaining: 4600.0,
            anchor_printed: 0.0,
            anchor_contested: false,
            level_now: Some(12000.0),
            printed_now: 0.0,
            feed_alive: true,
        };
        assert_eq!(classify(& e), RestVerdict::Corroborated);
    }
    #[test]
    fn ADV_prints_EXCEEDING_the_anchor_never_flip_the_verdict() {
        let e = Evidence {
            anchor_remaining: 4600.0,
            anchor_printed: 0.0,
            anchor_contested: false,
            level_now: Some(0.0),
            printed_now: 99999.0,
            feed_alive: true,
        };
        assert_eq!(classify(& e), RestVerdict::Corroborated);
    }
    #[test]
    fn renewal_respects_corroboration_AND_the_per_side_hygiene_cap() {
        assert!(
            renewable(RestVerdict::Corroborated, 3 * 3600, 1),
            "a sell may match his patience"
        );
        assert!(! renewable(RestVerdict::Uncertain, 60, 1), "uncertainty never renews");
        assert!(! renewable(RestVerdict::HeCancelled, 60, 1));
        assert!(! renewable(RestVerdict::Corroborated, MAX_REST_AGE_SECS, 1));
        assert!(renewable(RestVerdict::Corroborated, 3 * 3600, 0));
        assert!(
            ! renewable(RestVerdict::Corroborated, 5 * 3600, 0),
            "an unfilled bid must not hold daily budget for a whole day"
        );
        assert!(
            renewable(RestVerdict::Corroborated, 5 * 3600, 1),
            "but a sell at the same age still may"
        );
        assert!(max_age_for(0) < max_age_for(1));
    }
}
