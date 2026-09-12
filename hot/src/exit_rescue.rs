use std::time::Duration;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    NoMatch,
    NotEnoughBalance,
    Transient,
    Stable,
}
pub fn classify(body: &str) -> Failure {
    let b = body.to_ascii_lowercase();
    if b.contains("not enough balance") || b.contains("allowance") {
        return Failure::NotEnoughBalance;
    }
    if b.contains("no orders found to match") || b.contains("fak") {
        return Failure::NoMatch;
    }
    if b.contains("transport_error") || b.contains("timeout")
        || b.contains("502 bad gateway") || b.contains("503 service")
        || b.contains("504 gateway")
    {
        return Failure::Transient;
    }
    Failure::Stable
}
pub fn reported_balance(body: &str) -> Option<f64> {
    let b = body.to_ascii_lowercase();
    let i = b.find("balance:")? + "balance:".len();
    let digits: String = b[i..]
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<f64>().ok().map(|raw| raw / 1e6)
}
pub const FAST_ATTEMPTS: usize = 20;
pub const FAST_DELAY: Duration = Duration::from_millis(250);
pub const MED_ATTEMPTS: usize = 20;
pub const MED_DELAY: Duration = Duration::from_millis(500);
pub const SLOW_DELAY: Duration = Duration::from_millis(1000);
pub const MAX_ATTEMPTS: usize = 60;
pub const MAX_BALANCE_ATTEMPTS: usize = 3;
pub const FALLBACK_AFTER: Duration = Duration::from_secs(15);
pub const GIVE_UP_AFTER: Duration = Duration::from_secs(60);
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    pub delay: Duration,
    pub sell_all: bool,
}
pub fn next_step(f: Failure, attempt: usize, elapsed: Duration) -> Option<Step> {
    match f {
        Failure::Stable => None,
        Failure::Transient => {
            if elapsed >= GIVE_UP_AFTER || attempt >= MAX_ATTEMPTS {
                return None;
            }
            let delay = if attempt < FAST_ATTEMPTS {
                FAST_DELAY
            } else if attempt < FAST_ATTEMPTS + MED_ATTEMPTS {
                MED_DELAY
            } else {
                SLOW_DELAY
            };
            Some(Step { delay, sell_all: false })
        }
        Failure::NotEnoughBalance => {
            if elapsed >= GIVE_UP_AFTER || attempt >= MAX_BALANCE_ATTEMPTS {
                return None;
            }
            Some(Step {
                delay: FAST_DELAY,
                sell_all: true,
            })
        }
        Failure::NoMatch => {
            if elapsed >= GIVE_UP_AFTER || attempt >= MAX_ATTEMPTS {
                return None;
            }
            let delay = if attempt < FAST_ATTEMPTS {
                FAST_DELAY
            } else if attempt < FAST_ATTEMPTS + MED_ATTEMPTS {
                MED_DELAY
            } else {
                SLOW_DELAY
            };
            Some(Step {
                delay,
                sell_all: elapsed >= FALLBACK_AFTER,
            })
        }
    }
}
pub fn retry_salt_seed(now_ms: u128, attempt: usize) -> u128 {
    now_ms ^ ((attempt as u128 + 1) << 40)
}
pub fn retry_size(
    sell_all: bool,
    chain_reported: Option<f64>,
    intended: f64,
    full_holding: f64,
) -> f64 {
    if sell_all {
        let ceiling = intended.min(full_holding).max(0.0);
        let chain = chain_reported.unwrap_or(ceiling);
        chain.min(ceiling).max(0.0)
    } else {
        intended
    }
}
const _: () = assert!(MAX_ATTEMPTS <= 60);
const _: () = assert!(MAX_BALANCE_ATTEMPTS <= 3);
#[cfg(test)]
mod tests {
    #[test]
    fn an_AMBIGUOUS_outcome_must_never_reach_the_rescue_ladder() {
        use crate::race_send::{resolve, Outcome, PathResult};
        let all_timeout = vec![
            PathResult { path : "h1".into(), http : 0, ms : 10_000.0, body :
            "{\"transport_error\":\"timeout\"}".into() }, PathResult { path : "h2"
            .into(), http : 0, ms : 10_000.0, body : "{\"transport_error\":\"timeout\"}"
            .into() }, PathResult { path : "h1b".into(), http : 0, ms : 10_000.0, body :
            "{\"transport_error\":\"timeout\"}".into() },
        ];
        let out = resolve(&all_timeout);
        assert!(
            matches!(out, Outcome::Ambiguous { .. }),
            "a silent race must not look like a refusal: {out:?}"
        );
        assert!(
            ! matches!(out, Outcome::Rejected { .. }),
            "Rejected is the ONLY outcome that may re-sign a fresh order"
        );
    }
    #[test]
    fn a_real_refusal_still_reaches_the_ladder() {
        use crate::race_send::{resolve, Outcome, PathResult};
        let refused = vec![
            PathResult { path : "h1".into(), http : 400, ms : 12.0, body :
            "{\"error\":\"not enough balance / allowance\"}".into() }
        ];
        assert!(matches!(resolve(& refused), Outcome::Rejected { .. }));
    }
    use super::*;
    #[test]
    fn a_timeout_is_RETRIED_not_abandoned() {
        for body in [
            "{\"transport_error\":\"timeout\"}",
            "{\"transport_error\":\"connection closed\"}",
            "<html>502 Bad Gateway</html>",
        ] {
            assert_eq!(classify(body), Failure::Transient, "body: {body}");
        }
        let step = next_step(Failure::Transient, 0, Duration::from_secs(0))
            .expect("a timeout must be retried");
        assert!(! step.sell_all, "silence is NOT evidence the size was wrong");
        assert!(
            next_step(Failure::Transient, MAX_ATTEMPTS, Duration::from_secs(0)).is_none()
        );
        assert!(next_step(Failure::Transient, 0, GIVE_UP_AFTER).is_none());
    }
    #[test]
    fn sell_all_can_only_LOWER_the_ask_never_reach_another_lanes_shares() {
        assert_eq!(
            retry_size(true, Some(250.0), 50.0, 50.0), 50.0,
            "must never sell more than this lane holds"
        );
        assert_eq!(
            retry_size(true, Some(2.8), 50.0, 50.0), 2.8,
            "chain truth may reduce the ask"
        );
        assert_eq!(retry_size(true, None, 50.0, 50.0), 50.0);
        assert_eq!(retry_size(true, Some(- 5.0), 50.0, 50.0), 0.0);
        assert_eq!(retry_size(false, Some(250.0), 10.0, 50.0), 10.0);
    }
    const Z: Duration = Duration::ZERO;
    #[test]
    fn every_retry_gets_a_DISTINCT_salt_seed_even_within_one_millisecond() {
        let now = 1_785_600_000_000u128;
        let seeds: std::collections::HashSet<u128> = (0..200)
            .map(|a| retry_salt_seed(now, a))
            .collect();
        assert_eq!(seeds.len(), 200, "salt seeds collided within the same millisecond");
    }
    #[test]
    fn salt_seeds_also_differ_across_milliseconds_for_the_same_attempt() {
        assert_ne!(retry_salt_seed(1_000, 3), retry_salt_seed(1_001, 3));
    }
    #[test]
    fn before_the_fallback_we_retry_HIS_SLICE_not_the_whole_position() {
        assert_eq!(retry_size(false, None, 75.0, 100.0), 75.0);
        assert_eq!(
            retry_size(false, Some(2.8), 75.0, 100.0), 75.0,
            "a reported balance must not leak into the pre-fallback size"
        );
    }
    #[test]
    fn the_fallback_prefers_CHAIN_TRUTH_over_our_own_holding() {
        assert_eq!(retry_size(true, Some(2.816664), 75.0, 100.0), 2.816664);
        assert_eq!(
            retry_size(true, None, 75.0, 100.0), 75.0,
            "with no chain evidence the escalation must not exceed the trim"
        );
    }
    #[test]
    fn the_fallback_NEVER_sells_more_than_the_exit_intended() {
        assert_eq!(
            retry_size(true, None, 10.0, 100.0), 10.0,
            "a 10% trim must never escalate into a 100% liquidation"
        );
        assert_eq!(retry_size(true, Some(4.0), 10.0, 100.0), 4.0);
        assert_eq!(
            retry_size(true, Some(9_000.0), 10.0, 100.0), 10.0,
            "another lane's inventory is not ours to sell"
        );
    }
    #[test]
    fn a_FULL_exit_still_reaches_the_whole_position() {
        assert_eq!(retry_size(true, None, 100.0, 100.0), 100.0);
        assert_eq!(retry_size(true, Some(2.8), 100.0, 100.0), 2.8);
        assert_eq!(retry_size(true, None, 140.0, 100.0), 100.0);
    }
    #[test]
    fn the_REAL_rejection_we_saw_is_classified_as_a_balance_problem() {
        let body = r#"{"error":"not enough balance / allowance: the balance is not enough -> balance: 2816664, sum of matched orders"}"#;
        assert_eq!(classify(body), Failure::NotEnoughBalance);
    }
    #[test]
    fn the_TRUE_balance_is_recovered_from_the_refusal_itself() {
        let body = r#"{"error":"not enough balance / allowance: the balance is not enough -> balance: 2816664, sum of matched orders"}"#;
        let b = reported_balance(body).expect("balance must parse");
        assert!((b - 2.816664).abs() < 1e-9, "got {b}");
    }
    #[test]
    fn a_refusal_without_a_parseable_balance_yields_NONE_not_zero() {
        assert_eq!(reported_balance(r#"{"error":"no orders found to match"}"#), None);
        assert_eq!(reported_balance("balance:"), None);
        assert_eq!(reported_balance("balance: abc"), None);
    }
    #[test]
    fn the_FAK_kill_is_the_one_case_worth_retrying() {
        let body = r#"{"error":"no orders found to match with FAK order. FAK orders are partially filled or killed if no match is found."}"#;
        assert_eq!(classify(body), Failure::NoMatch);
    }
    #[test]
    fn an_UNKNOWN_error_fails_CLOSED_and_is_never_retried() {
        assert_eq!(classify(r#"{"error":"market is closed"}"#), Failure::Stable);
        assert_eq!(classify("garbage"), Failure::Stable);
        assert_eq!(classify(""), Failure::Stable);
    }
    #[test]
    fn a_STABLE_failure_is_never_retried_even_at_attempt_zero() {
        assert_eq!(next_step(Failure::Stable, 0, Z), None);
    }
    #[test]
    fn the_LADDER_matches_the_operator_spec_20x250_then_20x500_then_1s() {
        let f = Failure::NoMatch;
        assert_eq!(next_step(f, 0, Z).unwrap().delay, FAST_DELAY);
        assert_eq!(next_step(f, 19, Z).unwrap().delay, FAST_DELAY);
        assert_eq!(
            next_step(f, 20, Z).unwrap().delay, MED_DELAY, "21st attempt steps to 500ms"
        );
        assert_eq!(next_step(f, 39, Z).unwrap().delay, MED_DELAY);
        assert_eq!(
            next_step(f, 40, Z).unwrap().delay, SLOW_DELAY, "41st attempt steps to 1s"
        );
        assert_eq!(next_step(f, MAX_ATTEMPTS, Z), None);
    }
    #[test]
    fn the_fast_and_medium_phases_sum_to_the_FALLBACK_deadline() {
        let ladder = FAST_DELAY * FAST_ATTEMPTS as u32 + MED_DELAY * MED_ATTEMPTS as u32;
        assert_eq!(
            ladder, FALLBACK_AFTER, "20x250ms + 20x500ms must equal the 15s fallback"
        );
    }
    #[test]
    fn SELL_ALL_engages_only_after_the_fallback_deadline() {
        let f = Failure::NoMatch;
        assert!(
            ! next_step(f, 5, Duration::from_secs(14)).unwrap().sell_all,
            "before 15s we still sell the mirrored slice"
        );
        assert!(
            next_step(f, 41, FALLBACK_AFTER).unwrap().sell_all,
            "at 15s we switch to chain-truth sell-all"
        );
        assert!(next_step(f, 50, Duration::from_secs(30)).unwrap().sell_all);
    }
    #[test]
    fn a_BALANCE_failure_goes_STRAIGHT_to_sell_all_without_waiting_15s() {
        let s = next_step(Failure::NotEnoughBalance, 0, Z).unwrap();
        assert!(s.sell_all, "an oversell must re-derive size from chain immediately");
    }
    #[test]
    fn the_ladder_is_BOUNDED_in_wall_clock_even_if_attempts_are_few() {
        assert_eq!(next_step(Failure::NoMatch, 0, GIVE_UP_AFTER), None);
        assert_eq!(
            next_step(Failure::NoMatch, 0, GIVE_UP_AFTER + Duration::from_secs(1)), None
        );
        assert_eq!(
            next_step(Failure::NotEnoughBalance, 0, GIVE_UP_AFTER), None,
            "even the balance path stops at the deadline"
        );
    }
}
