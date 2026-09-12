pub const MAX_AGE_SECS: i64 = 60;
pub const BOOT_GRACE_SECS: i64 = 120;
pub const MAX_FUTURE_SECS: i64 = 300;
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Warmup { since_start: i64 },
    NeverSeen { since_start: i64 },
    Fresh { age_secs: i64 },
    Stale { age_secs: i64 },
}
impl Verdict {
    pub fn buys_gated(&self) -> bool {
        matches!(self, Verdict::Stale { .. } | Verdict::NeverSeen { .. })
    }
    pub fn describe(&self) -> String {
        match self {
            Verdict::Warmup { since_start } => {
                format!(
                    "guardian not seen yet ({since_start}s since start, grace \
                         {BOOT_GRACE_SECS}s) — not gated while warming up"
                )
            }
            Verdict::NeverSeen { since_start } => {
                format!(
                    "⛔ NO GUARDIAN PASS HAS EVER BEEN SEEN in {since_start}s since \
                         start (grace {BOOT_GRACE_SECS}s) — nothing is supervising this \
                         bot, so BUYS ARE GATED. Exits keep mirroring. Check that the \
                         guardian timer is running and writing run/guardian_state.json; \
                         this clears itself as soon as it does."
                )
            }
            Verdict::Fresh { age_secs } => {
                format!("guardian completed a pass {age_secs}s ago")
            }
            Verdict::Stale { age_secs } => {
                format!(
                    "⛔ NO GUARDIAN PASS IN {age_secs}s (limit {MAX_AGE_SECS}s) — the \
                         bot is trading unsupervised, so BUYS ARE GATED until it returns. \
                         Exits keep mirroring. This clears itself the moment the guardian \
                         writes again; no operator action is needed if it comes back."
                )
            }
        }
    }
}
pub fn assess(last_seen: Option<i64>, started_at: i64, now: i64) -> Verdict {
    match last_seen {
        None => {
            let since = (now - started_at).max(0);
            if since > BOOT_GRACE_SECS {
                Verdict::NeverSeen {
                    since_start: since,
                }
            } else {
                Verdict::Warmup {
                    since_start: since,
                }
            }
        }
        Some(t) => {
            let age = (now - t).max(0);
            if age > MAX_AGE_SECS {
                Verdict::Stale { age_secs: age }
            } else {
                Verdict::Fresh { age_secs: age }
            }
        }
    }
}
#[derive(Debug)]
pub struct Watch {
    last_seen: Option<i64>,
    started_at: i64,
}
impl Watch {
    pub fn new(started_at: i64) -> Self {
        Self {
            last_seen: None,
            started_at,
        }
    }
    pub fn observe(&mut self, mtime: Option<i64>, now: i64) -> Verdict {
        let mtime = mtime.filter(|t| *t <= now + MAX_FUTURE_SECS);
        if let Some(t) = mtime {
            self.last_seen = Some(
                match self.last_seen {
                    Some(prev) if prev > t => prev,
                    _ => t,
                },
            );
        }
        assess(self.last_seen, self.started_at, now)
    }
    pub fn last_seen(&self) -> Option<i64> {
        self.last_seen
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    const NOW: i64 = 1_700_000_000;
    #[test]
    fn a_guardian_not_seen_YET_does_not_gate_during_the_boot_grace() {
        let v = assess(None, NOW, NOW + 5);
        assert!(matches!(v, Verdict::Warmup { .. }), "{v:?}");
        assert!(! v.buys_gated());
        assert!(
            ! assess(None, NOW, NOW + BOOT_GRACE_SECS).buys_gated(),
            "the boundary is still warming up"
        );
    }
    #[test]
    fn A_GUARDIAN_THAT_NEVER_APPEARS_GATES_BUYS_once_the_grace_expires() {
        let v = assess(None, NOW, NOW + BOOT_GRACE_SECS + 1);
        assert!(matches!(v, Verdict::NeverSeen { .. }), "{v:?}");
        assert!(v.buys_gated(), "an absent guardian must stop buying");
        assert!(v.describe().contains("BUYS ARE GATED"));
        assert!(v.describe().contains("Exits keep mirroring"));
    }
    #[test]
    fn a_guardian_that_appears_LATE_still_clears_the_gate() {
        let mut w = Watch::new(NOW);
        assert!(w.observe(None, NOW + BOOT_GRACE_SECS + 1).buys_gated());
        assert!(
            ! w.observe(Some(NOW + BOOT_GRACE_SECS + 2), NOW + BOOT_GRACE_SECS + 2)
            .buys_gated(), "it starts working the moment the guardian appears"
        );
    }
    #[test]
    fn a_recent_pass_is_fresh() {
        assert!(! assess(Some(NOW - 11), NOW, NOW).buys_gated());
        assert!(
            ! assess(Some(NOW - MAX_AGE_SECS), NOW, NOW).buys_gated(),
            "exactly at the limit is still fresh"
        );
    }
    #[test]
    fn a_guardian_that_STOPPED_gates_buys() {
        let v = assess(Some(NOW - MAX_AGE_SECS - 1), NOW, NOW);
        assert!(v.buys_gated());
        assert!(v.describe().contains("BUYS ARE GATED"));
        assert!(v.describe().contains("Exits keep mirroring"));
    }
    #[test]
    fn THE_DEADLOCK_TEST_the_gate_CLEARS_ITSELF_when_the_guardian_returns() {
        let mut w = Watch::new(NOW);
        w.observe(Some(NOW - 1000), NOW);
        assert!(w.observe(None, NOW).buys_gated(), "stale while it is away");
        assert!(
            ! w.observe(Some(NOW), NOW).buys_gated(),
            "and buys resume the moment it writes again — no operator, no unlatch"
        );
    }
    #[test]
    fn a_FAILED_read_never_moves_the_clock_forward() {
        let mut w = Watch::new(NOW);
        w.observe(Some(NOW - 1000), NOW);
        for _ in 0..5 {
            w.observe(None, NOW);
        }
        assert_eq!(w.last_seen(), Some(NOW - 1000));
        assert!(w.observe(None, NOW).buys_gated());
    }
    #[test]
    fn a_vanished_file_TRIPS_the_gate_rather_than_disarming_it() {
        let mut w = Watch::new(NOW);
        assert!(
            ! w.observe(Some(NOW - 3600), NOW - 3600).buys_gated(), "fresh when written"
        );
        assert!(w.observe(None, NOW).buys_gated());
    }
    #[test]
    fn an_OLDER_mtime_never_ages_a_healthy_guardian() {
        let mut w = Watch::new(NOW);
        w.observe(Some(NOW), NOW);
        assert!(! w.observe(Some(NOW - 10_000), NOW).buys_gated());
        assert_eq!(w.last_seen(), Some(NOW));
    }
    #[test]
    fn ORDINARY_clock_skew_is_fresh_not_negatively_aged() {
        match assess(Some(NOW + 60), NOW, NOW) {
            Verdict::Fresh { age_secs } => {
                assert_eq!(age_secs, 0, "clamped, never negative")
            }
            other => panic!("clock skew must not gate buys, got {other:?}"),
        }
    }
    #[test]
    fn AN_IMPLAUSIBLE_FUTURE_MTIME_CANNOT_DISABLE_THE_GATE_FOREVER() {
        let mut w = Watch::new(NOW);
        let year = 31_536_000;
        assert!(
            matches!(w.observe(Some(NOW + year), NOW), Verdict::Warmup { .. }),
            "a year-ahead stamp is evidence of nothing, so nothing is armed"
        );
        assert!(
            ! w.observe(Some(NOW + year), NOW).buys_gated(),
            "and inside the boot grace it still must not stop trading"
        );
        assert!(
            w.observe(Some(NOW + year), NOW + BOOT_GRACE_SECS + 1).buys_gated(),
            "a bogus stamp cannot buy indefinite immunity"
        );
        w.observe(Some(NOW), NOW);
        assert!(
            w.observe(Some(NOW + year), NOW + 1000).buys_gated(),
            "the bogus stamp must NOT rescue a guardian that has actually stopped"
        );
    }
    #[test]
    fn a_future_stamp_never_overwrites_a_good_one() {
        let mut w = Watch::new(NOW);
        w.observe(Some(NOW), NOW);
        w.observe(Some(NOW + 31_536_000), NOW);
        assert_eq!(w.last_seen(), Some(NOW), "the sane observation is what is kept");
    }
    #[test]
    fn the_gate_is_re_evaluated_every_pass_and_never_sticks() {
        let mut w = Watch::new(NOW);
        w.observe(Some(NOW - 1000), NOW);
        for i in 0..10 {
            let fresh = w.observe(Some(NOW + i), NOW + i);
            assert!(! fresh.buys_gated(), "a live condition, not a memory of one");
        }
    }
}
