#[derive(Debug, Clone, PartialEq)]
pub enum SweepAction {
    Skip { why: &'static str },
    Observe { why: &'static str, target: f64, excess: f64 },
    Sell { qty: f64, why: &'static str, dust: bool },
}
#[derive(Debug, Clone, Copy)]
pub struct SweepInputs {
    pub owned: f64,
    pub physical: f64,
    pub leader: f64,
    pub mark: f64,
    pub redeemable: bool,
    pub just_sold: bool,
    pub proportional_target: Option<f64>,
    pub prior: Option<(u64, f64, f64)>,
    pub confirm_secs: u64,
    pub min_usd: f64,
}
pub const DUST_SHARES: f64 = crate::venue::DUST_SHARES;
pub const SAME_OBSERVATION: f64 = 0.05;
pub fn excess_shares(owned: f64, physical: f64, target: f64) -> f64 {
    (owned.max(0.0).min(physical.max(0.0)) - target.max(0.0)).max(0.0)
}
pub fn same_observation(a: f64, b: f64) -> bool {
    (a - b).abs() <= SAME_OBSERVATION
}
pub fn observation_to_store(
    prior: Option<&(u64, f64, f64)>,
    now: u64,
    target: f64,
    excess: f64,
) -> (u64, f64, f64) {
    match prior {
        Some(
            (seen, t, e),
        ) if same_observation(*t, target) && same_observation(*e, excess) => {
            (*seen, target, excess)
        }
        _ => (now, target, excess),
    }
}
pub fn decide(i: &SweepInputs) -> SweepAction {
    if i.redeemable {
        return SweepAction::Skip {
            why: "resolved_redeem_at_par",
        };
    }
    let sellable = i.owned.min(i.physical).max(0.0);
    if sellable <= 1e-9 {
        return SweepAction::Skip {
            why: "nothing_sellable",
        };
    }
    if i.just_sold && sellable < DUST_SHARES && i.leader <= 1e-9 {
        return SweepAction::Sell {
            qty: sellable,
            why: "post_sell_dust",
            dust: true,
        };
    }
    if i.just_sold && sellable < DUST_SHARES {
        return SweepAction::Skip {
            why: "dust_but_leader_mid_exit",
        };
    }
    let leader_flat = i.leader <= 1e-9;
    let target = if leader_flat {
        0.0
    } else {
        match i.proportional_target {
            Some(t) => t,
            None => {
                return SweepAction::Skip {
                    why: "no_sizing_basis",
                };
            }
        }
    };
    let sellable_now = i.owned.max(0.0).min(i.physical.max(0.0));
    if !leader_flat && sellable_now <= target * crate::firerate::OVERCOPY_TOLERANCE {
        return SweepAction::Skip {
            why: "within_overcopy_tolerance",
        };
    }
    let excess = excess_shares(i.owned, i.physical, target);
    if excess <= 0.01 || excess * i.mark < i.min_usd {
        return SweepAction::Skip {
            why: "excess_not_worth_selling",
        };
    }
    let why = if leader_flat { "leader_flat" } else { "proportional_excess" };
    let confirmed = i
        .prior
        .map(|(age, t0, e0)| {
            age >= i.confirm_secs && same_observation(target, t0)
                && same_observation(excess, e0)
        })
        .unwrap_or(false);
    if confirmed {
        SweepAction::Sell {
            qty: excess,
            why,
            dust: false,
        }
    } else {
        SweepAction::Observe {
            why,
            target,
            excess,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn base() -> SweepInputs {
        SweepInputs {
            owned: 100.0,
            physical: 100.0,
            leader: 1_000.0,
            mark: 0.50,
            redeemable: false,
            just_sold: false,
            proportional_target: Some(100.0),
            prior: None,
            confirm_secs: 60,
            min_usd: 0.02,
        }
    }
    #[test]
    fn excess_is_bounded_by_the_wallet_and_never_negative() {
        assert!((excess_shares(20.816325, 20.8328, 0.0) - 20.816325).abs() < 1e-9);
        assert_eq!(
            excess_shares(10.0, 7.0, 0.0), 7.0,
            "the physical wallet is an availability ceiling"
        );
        assert_eq!(
            excess_shares(5.0, 5.0, 8.0), 0.0,
            "normalization must never create a buy requirement"
        );
        assert_eq!(excess_shares(13.0, 13.0, 13.0), 0.0);
        assert_eq!(excess_shares(- 5.0, 10.0, 0.0), 0.0);
        assert_eq!(excess_shares(10.0, - 5.0, 0.0), 0.0);
    }
    #[test]
    fn an_observation_keeps_its_clock_while_the_numbers_hold() {
        let kept = observation_to_store(Some(&(100, 0.0, 50.0)), 160, 0.0, 50.0);
        assert_eq!(kept, (100, 0.0, 50.0));
        let moved = observation_to_store(Some(&(100, 0.0, 50.0)), 160, 8.0, 42.0);
        assert_eq!(moved, (160, 8.0, 42.0));
        assert_eq!(observation_to_store(None, 160, 0.0, 50.0), (160, 0.0, 50.0));
        assert!(same_observation(50.0, 50.0 + SAME_OBSERVATION));
        assert!(! same_observation(50.0, 50.0 + SAME_OBSERVATION * 2.0));
    }
    #[test]
    fn a_resolved_market_is_never_swept() {
        let i = SweepInputs {
            redeemable: true,
            leader: 0.0,
            ..base()
        };
        assert_eq!(decide(& i), SweepAction::Skip { why : "resolved_redeem_at_par" });
    }
    #[test]
    fn a_flat_leader_needs_the_SAME_confirmation_on_either_wake_path() {
        let flat = SweepInputs {
            leader: 0.0,
            ..base()
        };
        for just_sold in [false, true] {
            let i = SweepInputs { just_sold, ..flat };
            match decide(&i) {
                SweepAction::Observe { why, target, excess } => {
                    assert_eq!(why, "leader_flat");
                    assert_eq!(target, 0.0);
                    assert!((excess - 100.0).abs() < 1e-9);
                }
                other => {
                    panic!("just_sold={just_sold} must observe first, got {other:?}")
                }
            }
        }
        let i = SweepInputs {
            prior: Some((60, 0.0, 100.0)),
            ..flat
        };
        assert_eq!(
            decide(& i), SweepAction::Sell { qty : 100.0, why : "leader_flat", dust :
            false }
        );
    }
    #[test]
    fn a_leader_who_LEAVES_RETURNS_and_LEAVES_cannot_confirm_on_the_stale_stamp() {
        let flat = SweepInputs {
            leader: 0.0,
            ..base()
        };
        let back = SweepInputs {
            leader: 800.0,
            proportional_target: Some(40.0),
            prior: Some((90, 0.0, 100.0)),
            ..base()
        };
        match decide(&back) {
            SweepAction::Observe { target, excess, .. } => {
                assert!((target - 40.0).abs() < 1e-9);
                assert!((excess - 60.0).abs() < 1e-9, "excess must be the NEW one");
            }
            other => panic!("a changed book must re-observe, got {other:?}"),
        }
        let again = SweepInputs {
            prior: Some((90, 40.0, 60.0)),
            ..flat
        };
        assert!(
            matches!(decide(& again), SweepAction::Observe { .. }),
            "a fresh exit must earn its own confirmation window"
        );
    }
    #[test]
    fn dust_is_swept_only_after_the_leader_is_FLAT() {
        let mid_exit = SweepInputs {
            owned: 0.4,
            physical: 0.4,
            leader: 5_000.0,
            just_sold: true,
            ..base()
        };
        assert_eq!(
            decide(& mid_exit), SweepAction::Skip { why : "dust_but_leader_mid_exit" },
            "while he holds, the mirror owns the remainder"
        );
        let flat = SweepInputs {
            leader: 0.0,
            ..mid_exit
        };
        assert_eq!(
            decide(& flat), SweepAction::Sell { qty : 0.4, why : "post_sell_dust", dust :
            true }
        );
        let fresh = SweepInputs {
            just_sold: false,
            ..flat
        };
        assert!(
            ! matches!(decide(& fresh), SweepAction::Sell { dust : true, .. }),
            "must not dump a position the buy path just opened"
        );
    }
    #[test]
    fn the_chain_is_a_CEILING_never_an_entitlement() {
        let i = SweepInputs {
            owned: 100.0,
            physical: 2.8,
            leader: 0.0,
            prior: Some((60, 0.0, 2.8)),
            ..base()
        };
        assert_eq!(
            decide(& i), SweepAction::Sell { qty : 2.8, why : "leader_flat", dust : false
            }
        );
        let shared = SweepInputs {
            owned: 50.0,
            physical: 250.0,
            leader: 0.0,
            prior: Some((60, 0.0, 50.0)),
            ..base()
        };
        assert_eq!(shared_qty(decide(& shared)), 50.0);
    }
    fn shared_qty(a: SweepAction) -> f64 {
        match a {
            SweepAction::Sell { qty, .. } => qty,
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn a_lane_with_no_sizing_basis_does_nothing_unless_he_is_flat() {
        let i = SweepInputs {
            proportional_target: None,
            ..base()
        };
        assert_eq!(decide(& i), SweepAction::Skip { why : "no_sizing_basis" });
        let flat = SweepInputs {
            leader: 0.0,
            proportional_target: None,
            prior: Some((60, 0.0, 100.0)),
            ..base()
        };
        assert_eq!(shared_qty(decide(& flat)), 100.0);
    }
    #[test]
    fn economically_pointless_sells_are_refused() {
        let tiny = SweepInputs {
            owned: 100.0,
            physical: 100.0,
            proportional_target: Some(99.995),
            ..base()
        };
        assert_eq!(
            decide(& tiny), SweepAction::Skip { why : "within_overcopy_tolerance" }
        );
        let cheap = SweepInputs {
            mark: 0.0001,
            leader: 0.0,
            owned: 100.0,
            physical: 100.0,
            ..base()
        };
        assert_eq!(
            decide(& cheap), SweepAction::Skip { why : "excess_not_worth_selling" }
        );
    }
    #[test]
    fn the_2026_08_17_DRIBBLE_is_dead_a_wobbling_target_is_not_a_malfunction() {
        for target in [1188.195_f64, 1186.893, 1186.328] {
            let i = SweepInputs {
                owned: 1188.4832,
                physical: 1188.4832,
                leader: 6420.23,
                mark: 0.9255,
                proportional_target: Some(target),
                prior: Some((3600, target, 1188.4832 - target)),
                ..base()
            };
            assert_eq!(
                decide(& i), SweepAction::Skip { why : "within_overcopy_tolerance" },
                "a {:.3}-share 'excess' on 1,188 shares is input wobble, not a \
malfunction — selling it was the ratchet",
                1188.4832 - target
            );
        }
    }
    #[test]
    fn a_DOUBLED_position_is_still_trimmed_and_only_back_to_target() {
        let i = SweepInputs {
            owned: 200.0,
            physical: 200.0,
            prior: Some((60, 100.0, 100.0)),
            ..base()
        };
        assert_eq!(
            decide(& i), SweepAction::Sell { qty : 100.0, why : "proportional_excess",
            dust : false }
        );
    }
    #[test]
    fn exactly_AT_tolerance_does_not_trim_matching_the_firerate_boundary() {
        let i = SweepInputs {
            owned: 150.0,
            physical: 150.0,
            prior: Some((60, 100.0, 50.0)),
            ..base()
        };
        assert_eq!(decide(& i), SweepAction::Skip { why : "within_overcopy_tolerance" });
    }
    #[test]
    fn the_tolerance_never_dulls_the_FLAT_leader_backstop() {
        let i = SweepInputs {
            leader: 0.0,
            owned: 1.2,
            physical: 1.2,
            prior: Some((60, 0.0, 1.2)),
            ..base()
        };
        assert_eq!(
            decide(& i), SweepAction::Sell { qty : 1.2, why : "leader_flat", dust : false
            }
        );
    }
    #[test]
    fn an_observation_must_AGE_not_merely_exist() {
        let flat = SweepInputs {
            leader: 0.0,
            prior: Some((59, 0.0, 100.0)),
            ..base()
        };
        assert!(matches!(decide(& flat), SweepAction::Observe { .. }), "59s is not 60s");
    }
}
