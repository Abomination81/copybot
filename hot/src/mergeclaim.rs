use std::collections::HashMap;
pub const CLAIM_BACKSTOP_SECS: u64 = 900;
pub const EXECUTOR_TIMEOUT_SECS: u64 = 180;
#[derive(Debug, Default)]
pub struct MergeClaims {
    held: HashMap<(String, String), u64>,
}
impl MergeClaims {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn claim(&mut self, lane: &str, tokens: &[&str], now: u64) -> bool {
        if tokens.iter().any(|t| self.is_claimed(lane, t, now)) {
            return false;
        }
        for t in tokens {
            self.held.insert((lane.to_string(), (*t).to_string()), now);
        }
        true
    }
    pub fn release(&mut self, lane: &str, tokens: &[&str]) {
        for t in tokens {
            self.held.remove(&(lane.to_string(), (*t).to_string()));
        }
    }
    pub fn is_claimed(&self, lane: &str, token: &str, now: u64) -> bool {
        match self.held.get(&(lane.to_string(), token.to_string())) {
            Some(at) => now.saturating_sub(*at) < CLAIM_BACKSTOP_SECS,
            None => false,
        }
    }
    pub fn len(&self) -> usize {
        self.held.len()
    }
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
    pub fn evict_expired(&mut self, now: u64) {
        self.held.retain(|_, at| now.saturating_sub(*at) < CLAIM_BACKSTOP_SECS);
    }
}
#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    const A: &str = "token_yes";
    const B: &str = "token_no";
    #[test]
    fn a_claimed_leg_is_not_swept() {
        let mut c = MergeClaims::new();
        assert!(c.claim("example_lane_25", & [A, B], 1_000));
        assert!(c.is_claimed("example_lane_25", A, 1_000));
        assert!(c.is_claimed("example_lane_25", B, 1_000), "BOTH legs must be held, not just one");
    }
    #[test]
    fn a_REFUSED_claim_leaves_NOTHING_claimed() {
        let mut c = MergeClaims::new();
        assert!(c.claim("example_lane_25", & [A], 1_000));
        assert!(
            ! c.claim("example_lane_25", & [A, B], 1_000), "must not claim over an existing claim"
        );
        assert!(
            ! c.is_claimed("example_lane_25", B, 1_000),
            "the refused claim took leg B anyway — this is the bug"
        );
    }
    #[test]
    fn releasing_frees_both_legs_and_is_idempotent() {
        let mut c = MergeClaims::new();
        c.claim("example_lane_25", &[A, B], 1_000);
        c.release("example_lane_25", &[A, B]);
        assert!(! c.is_claimed("example_lane_25", A, 1_000) && ! c.is_claimed("example_lane_25", B, 1_000));
        c.release("example_lane_25", &[A, B]);
        c.release("example_lane_25", &[A, B]);
        assert!(c.is_empty());
    }
    #[test]
    fn one_lanes_merge_does_not_freeze_ANOTHER_lanes_sweep() {
        let mut c = MergeClaims::new();
        assert!(c.claim("example_lane_25", & [A, B], 1_000));
        assert!(! c.is_claimed("example_lane_26", A, 1_000), "lanes own separate slices");
        assert!(
            c.claim("example_lane_26", & [A, B], 1_000),
            "and may claim the same token independently"
        );
    }
    #[test]
    fn the_backstop_can_NEVER_expire_while_a_merge_is_still_working() {
        assert!(
            CLAIM_BACKSTOP_SECS > EXECUTOR_TIMEOUT_SECS * 2,
            "backstop {CLAIM_BACKSTOP_SECS}s gives no margin over a {EXECUTOR_TIMEOUT_SECS}s \
executor — a claim could expire mid-merge"
        );
    }
    #[test]
    fn a_claim_from_a_DEAD_process_does_not_wedge_the_token_forever() {
        let mut c = MergeClaims::new();
        c.claim("example_lane_25", &[A, B], 1_000);
        assert!(c.is_claimed("example_lane_25", A, 1_000 + CLAIM_BACKSTOP_SECS - 1));
        assert!(
            ! c.is_claimed("example_lane_25", A, 1_000 + CLAIM_BACKSTOP_SECS),
            "the backstop must eventually free a leg nobody released"
        );
        c.evict_expired(1_000 + CLAIM_BACKSTOP_SECS);
        assert!(c.is_empty());
    }
    #[test]
    fn a_BACKWARD_clock_step_keeps_the_claim_held_and_does_not_panic() {
        let mut c = MergeClaims::new();
        c.claim("example_lane_25", &[A, B], 10_000);
        assert!(c.is_claimed("example_lane_25", A, 9_000), "held is the safe direction");
        c.evict_expired(9_000);
        assert!(! c.is_empty(), "a backward step must not evict a live claim");
    }
    #[test]
    fn an_empty_token_list_claims_nothing_and_succeeds_trivially() {
        let mut c = MergeClaims::new();
        assert!(c.claim("example_lane_25", & [], 1_000));
        assert!(c.is_empty(), "claiming no legs must not invent an entry");
    }
}
