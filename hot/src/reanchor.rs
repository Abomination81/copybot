use std::collections::HashMap;
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Move {
    Raise { from: f64, to: f64 },
    Lower { from: f64, to: f64 },
    Agrees,
    Refused(Refusal),
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Refusal {
    IncompleteRead,
    WouldZeroAHeldPosition,
    WouldShrinkAHeldPosition,
    NotANumber,
}
impl Refusal {
    pub fn why(&self) -> &'static str {
        match self {
            Refusal::IncompleteRead => "venue read incomplete — absence proves nothing",
            Refusal::WouldZeroAHeldPosition => {
                "venue shows none of a position we hold; zeroing it would arm frac=1.0 \
                 on the next trim — that decision belongs to the orphan sweep"
            }
            Refusal::WouldShrinkAHeldPosition => {
                "venue shows less than we modelled on a position we hold; lowering it \
                 makes the next exit larger, which is the direction that oversells"
            }
            Refusal::NotANumber => "non-finite share count",
        }
    }
}
pub const TOLERANCE: f64 = 0.005;
pub const MIN_SHARES: f64 = 1.0;
pub fn decide(modelled: f64, venue: Option<f64>, we_hold: f64, complete: bool) -> Move {
    if !complete {
        return Move::Refused(Refusal::IncompleteRead);
    }
    if !modelled.is_finite() || !we_hold.is_finite()
        || venue.map(|v| !v.is_finite()).unwrap_or(false)
    {
        return Move::Refused(Refusal::NotANumber);
    }
    let v = venue.unwrap_or(0.0);
    if v <= MIN_SHARES && we_hold > 1e-9 {
        return Move::Refused(Refusal::WouldZeroAHeldPosition);
    }
    let tol = TOLERANCE * modelled.abs().max(v.abs()).max(1.0);
    if (v - modelled).abs() <= tol {
        return Move::Agrees;
    }
    if v > modelled {
        return Move::Raise {
            from: modelled,
            to: v,
        };
    }
    if we_hold > 1e-9 {
        return Move::Refused(Refusal::WouldShrinkAHeldPosition);
    }
    Move::Lower {
        from: modelled,
        to: v,
    }
}
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub raised: usize,
    pub lowered: usize,
    pub agreed: usize,
    pub refused: usize,
    pub disarmed_zeros: usize,
    pub shares_raised: f64,
}
pub fn plan(
    modelled: &HashMap<String, f64>,
    venue: &HashMap<String, f64>,
    held: &HashMap<String, f64>,
    complete: bool,
) -> (Vec<(String, f64)>, Report) {
    let mut writes = Vec::new();
    let mut r = Report::default();
    let mut tokens: Vec<&String> = modelled.keys().chain(venue.keys()).collect();
    tokens.sort();
    tokens.dedup();
    for t in tokens {
        let m = modelled.get(t).copied().unwrap_or(0.0);
        let v = venue.get(t).copied();
        let h = held.get(t).copied().unwrap_or(0.0);
        match decide(m, v, h, complete) {
            Move::Raise { from, to } => {
                r.raised += 1;
                r.shares_raised += to - from;
                if from <= 1e-9 && h > 1e-9 {
                    r.disarmed_zeros += 1;
                }
                writes.push((t.clone(), to));
            }
            Move::Lower { to, .. } => {
                r.lowered += 1;
                writes.push((t.clone(), to));
            }
            Move::Agrees => r.agreed += 1,
            Move::Refused(_) => r.refused += 1,
        }
    }
    (writes, r)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn m(p: &[(&str, f64)]) -> HashMap<String, f64> {
        p.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }
    fn map(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }
    #[test]
    fn RT_a_TRUNCATED_page_cannot_liquidate_anything() {
        let modelled = m(&[("A", 5000.0), ("B", 9000.0), ("C", 100.0)]);
        let venue = m(&[("A", 5000.0)]);
        let held = m(&[("B", 300.0), ("C", 40.0)]);
        let (writes, r) = plan(&modelled, &venue, &held, false);
        assert!(writes.is_empty(), "an incomplete read must write NOTHING");
        assert_eq!(r.raised + r.lowered, 0);
    }
    #[test]
    fn RT_even_a_COMPLETE_read_cannot_zero_a_held_position() {
        let (writes, r) = plan(&m(&[("A", 5000.0)]), &m(&[]), &m(&[("A", 300.0)]), true);
        assert!(writes.is_empty());
        assert_eq!(r.refused, 1);
    }
    #[test]
    fn RT_the_repair_only_ever_makes_exits_SMALLER_on_held_tokens() {
        let modelled = m(&[("A", 0.0), ("B", 100.0), ("C", 9000.0)]);
        let venue = m(&[("A", 76437.0), ("B", 5000.0), ("C", 20.0)]);
        let held = m(&[("A", 320.0), ("B", 50.0), ("C", 10.0)]);
        let (writes, _) = plan(&modelled, &venue, &held, true);
        for (tok, v) in &writes {
            let before = modelled[tok];
            assert!(
                * v >= before,
                "{tok}: wrote {v} below modelled {before} — that INCREASES frac"
            );
        }
    }
    #[test]
    fn RT_lowering_is_confined_to_tokens_we_do_not_hold() {
        let (writes, _) = plan(&m(&[("X", 9000.0)]), &m(&[("X", 20.0)]), &m(&[]), true);
        assert_eq!(
            writes, vec![("X".to_string(), 20.0)],
            "no position means no exit to mis-size"
        );
    }
    #[test]
    fn RT_an_empty_venue_book_while_we_hold_everything_writes_nothing() {
        let modelled = m(&[("A", 100.0), ("B", 200.0), ("C", 300.0)]);
        let held = m(&[("A", 10.0), ("B", 20.0), ("C", 30.0)]);
        let (writes, r) = plan(&modelled, &m(&[]), &held, true);
        assert!(writes.is_empty(), "an empty book must not rewrite the whole model");
        assert_eq!(r.refused, 3);
    }
    #[test]
    fn RT_repeated_runs_converge_and_stop_writing() {
        let mut modelled = m(&[("A", 0.0)]);
        let venue = m(&[("A", 5000.0)]);
        let held = m(&[("A", 100.0)]);
        let (w1, _) = plan(&modelled, &venue, &held, true);
        for (t, v) in &w1 {
            modelled.insert(t.clone(), *v);
        }
        let (w2, r2) = plan(&modelled, &venue, &held, true);
        assert!(w2.is_empty(), "second pass must be a no-op");
        assert_eq!(r2.agreed, 1);
    }
    #[test]
    fn FRT_reanchor_keeps_working_correctly_DURING_a_split_brain() {
        let (writes, r) = plan(
            &m(&[("A", 0.0)]),
            &m(&[("A", 5000.0)]),
            &m(&[("A", 100.0)]),
            true,
        );
        assert_eq!(writes.len(), 1, "re-anchor still repairs during a split brain");
        assert_eq!(r.disarmed_zeros, 1);
    }
    #[test]
    fn FRT_every_reanchor_write_is_safe_under_ADVERSARIAL_venue_data() {
        let modelled = m(&[("A", 5000.0), ("B", 0.0), ("C", 100.0), ("D", 9000.0)]);
        let held = m(&[("A", 300.0), ("B", 50.0), ("C", 0.0), ("D", 10.0)]);
        for venue in [
            m(&[]),
            m(&[("A", 0.0), ("B", 0.0), ("C", 0.0), ("D", 0.0)]),
            m(&[("A", 1.0), ("D", 0.5)]),
            m(&[("A", f64::NAN)]),
            m(&[("A", 1e12)]),
        ] {
            let (writes, _) = plan(&modelled, &venue, &held, true);
            for (tok, v) in &writes {
                let before = modelled.get(tok).copied().unwrap_or(0.0);
                let hold = held.get(tok).copied().unwrap_or(0.0);
                if hold > 1e-9 {
                    assert!(
                        * v >= before,
                        "{tok}: wrote {v} < modelled {before} while holding {hold} \
                             — that ENLARGES the next exit"
                    );
                }
            }
        }
    }
    #[test]
    fn THE_MEASURED_CASE_a_modelled_zero_on_a_held_position_is_RAISED() {
        let m = decide(0.0, Some(76_437.0), 320.83, true);
        assert_eq!(m, Move::Raise { from : 0.0, to : 76_437.0 });
    }
    #[test]
    fn raising_is_UNCONDITIONAL_because_it_can_only_shrink_an_exit() {
        assert!(matches!(decide(100.0, Some(5_000.0), 50.0, true), Move::Raise { .. }));
        assert!(matches!(decide(0.0, Some(10.0), 0.0, true), Move::Raise { .. }));
    }
    #[test]
    fn IT_NEVER_ZEROES_A_POSITION_WE_HOLD() {
        assert_eq!(
            decide(5_000.0, Some(0.0), 320.0, true),
            Move::Refused(Refusal::WouldZeroAHeldPosition)
        );
        assert_eq!(
            decide(5_000.0, None, 320.0, true),
            Move::Refused(Refusal::WouldZeroAHeldPosition),
            "ABSENT and ZERO are the same claim and both are refused"
        );
        assert_eq!(
            decide(5_000.0, Some(0.5), 320.0, true),
            Move::Refused(Refusal::WouldZeroAHeldPosition)
        );
    }
    #[test]
    fn an_INCOMPLETE_read_changes_NOTHING_in_either_direction() {
        assert_eq!(
            decide(0.0, Some(9_000.0), 10.0, false),
            Move::Refused(Refusal::IncompleteRead)
        );
        assert_eq!(
            decide(9_000.0, Some(10.0), 10.0, false),
            Move::Refused(Refusal::IncompleteRead)
        );
    }
    #[test]
    fn IT_NEVER_SHRINKS_A_POSITION_WE_HOLD_not_just_never_zeroes_one() {
        assert_eq!(
            decide(9_000.0, Some(20.0), 10.0, true),
            Move::Refused(Refusal::WouldShrinkAHeldPosition)
        );
        assert_eq!(
            decide(9_000.0, Some(1_000.0), 50.0, true),
            Move::Refused(Refusal::WouldShrinkAHeldPosition),
            "even a modest shrink is the oversell direction while we hold it"
        );
    }
    #[test]
    fn lowering_is_allowed_only_when_we_hold_NOTHING_to_mis_size() {
        assert_eq!(
            decide(9_000.0, Some(1_000.0), 0.0, true), Move::Lower { from : 9_000.0, to :
            1_000.0 }
        );
    }
    #[test]
    fn a_token_we_do_NOT_hold_may_go_to_zero_because_no_exit_can_be_mis_sized() {
        assert!(matches!(decide(5_000.0, Some(0.0), 0.0, true), Move::Lower { .. }));
    }
    #[test]
    fn small_disagreements_are_left_alone() {
        assert_eq!(decide(1_000.0, Some(1_002.0), 5.0, true), Move::Agrees);
        assert_eq!(decide(0.0, Some(0.0), 0.0, true), Move::Agrees);
    }
    #[test]
    fn a_NaN_from_either_side_is_refused_not_written() {
        assert_eq!(
            decide(f64::NAN, Some(10.0), 0.0, true), Move::Refused(Refusal::NotANumber)
        );
        assert_eq!(
            decide(10.0, Some(f64::NAN), 0.0, true), Move::Refused(Refusal::NotANumber)
        );
    }
    #[test]
    fn plan_finds_tokens_the_MODEL_has_never_heard_of() {
        let (writes, r) = plan(
            &map(&[("A", 100.0)]),
            &map(&[("A", 100.0), ("B", 9_000.0)]),
            &map(&[("B", 42.0)]),
            true,
        );
        assert_eq!(r.agreed, 1);
        assert_eq!(r.raised, 1);
        assert_eq!(r.disarmed_zeros, 1, "B was an armed frac=1.0 and is now disarmed");
        assert_eq!(writes, vec![("B".to_string(), 9_000.0)]);
    }
    #[test]
    fn plan_on_an_INCOMPLETE_read_writes_NOTHING() {
        let (writes, r) = plan(
            &map(&[("A", 5_000.0)]),
            &map(&[]),
            &map(&[("A", 10.0)]),
            false,
        );
        assert!(writes.is_empty());
        assert_eq!(r.refused, 1);
        assert_eq!(r.raised + r.lowered, 0);
    }
}
