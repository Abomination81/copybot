use serde::Deserialize;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Graceful,
    Hybrid,
    Panic,
}
impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "graceful" => Some(Mode::Graceful),
            "hybrid" => Some(Mode::Hybrid),
            "panic" => Some(Mode::Panic),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Graceful => "graceful",
            Mode::Hybrid => "hybrid",
            Mode::Panic => "panic",
        }
    }
}
#[derive(Debug, Clone, Deserialize)]
pub struct Intent {
    pub lane: String,
    pub mode: String,
    pub phrase: String,
    pub ts: i64,
    #[serde(default)]
    pub acknowledge_forfeit: bool,
}
pub const INTENT_TTL_SECS: i64 = 60;
pub fn gate(
    intent: &Intent,
    lane_name: &str,
    armed: bool,
    now: i64,
) -> Result<Mode, String> {
    let mode = Mode::parse(&intent.mode)
        .ok_or_else(|| format!("unknown flatten mode {:?}", intent.mode))?;
    if intent.lane != lane_name {
        return Err(format!("intent lane {:?} != {lane_name}", intent.lane));
    }
    let expected = format!("{} {}", mode.as_str(), lane_name);
    if intent.phrase != expected {
        return Err(format!("phrase must be {expected:?}, got {:?}", intent.phrase));
    }
    if now - intent.ts > INTENT_TTL_SECS {
        return Err(
            format!("intent is {}s old (max {INTENT_TTL_SECS})", now - intent.ts),
        );
    }
    if intent.ts > now + 5 {
        return Err("intent timestamp is in the future".into());
    }
    match mode {
        Mode::Panic if armed => {
            Err("panic requires the lane to be DISARMED first".into())
        }
        Mode::Hybrid if !intent.acknowledge_forfeit => {
            Err(
                "hybrid requires acknowledge_forfeit=true (it sells winners early)"
                    .into(),
            )
        }
        _ => Ok(mode),
    }
}
pub fn should_sell(mode: Mode, mark: f64, avg_cost: f64) -> bool {
    match mode {
        Mode::Panic => true,
        Mode::Hybrid => mark > avg_cost + 1e-9,
        Mode::Graceful => false,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn intent(mode: &str, lane: &str, phrase: &str, ts: i64) -> Intent {
        Intent {
            lane: lane.into(),
            mode: mode.into(),
            phrase: phrase.into(),
            ts,
            acknowledge_forfeit: false,
        }
    }
    #[test]
    fn a_correct_panic_intent_on_a_disarmed_lane_passes() {
        let i = intent("panic", "example_lane_26", "panic example_lane_26", 1000);
        assert_eq!(gate(& i, "example_lane_26", false, 1010).unwrap(), Mode::Panic);
    }
    #[test]
    fn panic_on_an_ARMED_lane_is_refused() {
        let i = intent("panic", "example_lane_26", "panic example_lane_26", 1000);
        assert!(
            gate(& i, "example_lane_26", true, 1010).is_err(), "cannot panic while still buying"
        );
    }
    #[test]
    fn a_wrong_phrase_is_refused() {
        assert!(
            gate(& intent("panic", "example_lane_26", "panic", 1000), "example_lane_26", false, 1010).is_err()
        );
        assert!(
            gate(& intent("panic", "example_lane_26", "panic peyz", 1000), "example_lane_26", false, 1010)
            .is_err()
        );
        assert!(
            gate(& intent("panic", "example_lane_26", "PANIC example_lane_26", 1000), "example_lane_26", false, 1010)
            .is_err()
        );
    }
    #[test]
    fn a_stale_or_future_intent_is_refused() {
        assert!(
            gate(& intent("panic", "example_lane_26", "panic example_lane_26", 1000), "example_lane_26", false, 1000 + 61)
            .is_err()
        );
        assert!(
            gate(& intent("panic", "example_lane_26", "panic example_lane_26", 1000), "example_lane_26", false, 900)
            .is_err()
        );
    }
    #[test]
    fn hybrid_requires_the_forfeit_acknowledgement() {
        let mut i = intent("hybrid", "example_lane_26", "hybrid example_lane_26", 1000);
        assert!(gate(& i, "example_lane_26", false, 1010).is_err(), "no ack -> refused");
        i.acknowledge_forfeit = true;
        assert_eq!(gate(& i, "example_lane_26", false, 1010).unwrap(), Mode::Hybrid);
    }
    #[test]
    fn the_intent_lane_must_match_the_target() {
        let i = intent("panic", "peyz", "panic peyz", 1000);
        assert!(
            gate(& i, "example_lane_26", false, 1010).is_err(),
            "an intent for peyz cannot flatten example_lane_26"
        );
    }
    #[test]
    fn panic_sells_everything_hybrid_sells_only_winners_graceful_nothing() {
        assert!(should_sell(Mode::Panic, 0.10, 0.90));
        assert!(should_sell(Mode::Panic, 0.90, 0.10));
        assert!(should_sell(Mode::Hybrid, 0.90, 0.50), "a winner is sold");
        assert!(! should_sell(Mode::Hybrid, 0.40, 0.50), "a loser is left for graceful");
        assert!(
            ! should_sell(Mode::Hybrid, 0.50, 0.50), "a flat position is not a winner"
        );
        assert!(! should_sell(Mode::Graceful, 0.90, 0.10));
    }
}
