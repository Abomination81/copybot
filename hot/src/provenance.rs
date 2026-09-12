use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Origin {
    MirrorBuy,
    MirrorSell,
    ExitLadder,
    ExitRescue { attempt: u32 },
    DustSweep { reason: String },
    Flatten { mode: String },
    MergeReconcile,
    OrphanSweep,
}
impl Origin {
    pub fn kind(&self) -> &'static str {
        match self {
            Origin::MirrorBuy => "mirror_buy",
            Origin::MirrorSell => "mirror_sell",
            Origin::ExitLadder => "exit_ladder",
            Origin::ExitRescue { .. } => "exit_rescue",
            Origin::DustSweep { .. } => "dust_sweep",
            Origin::Flatten { .. } => "flatten",
            Origin::MergeReconcile => "merge_reconcile",
            Origin::OrphanSweep => "orphan_sweep",
        }
    }
    pub fn label(&self) -> String {
        match self {
            Origin::ExitRescue { attempt } => format!("exit_rescue:attempt{attempt}"),
            Origin::DustSweep { reason } => format!("dust_sweep:{reason}"),
            Origin::Flatten { mode } => format!("flatten:{mode}"),
            other => other.kind().to_string(),
        }
    }
    pub fn is_leader_driven(&self) -> bool {
        matches!(self, Origin::MirrorBuy | Origin::MirrorSell | Origin::ExitLadder)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub origin: Origin,
    pub lane: String,
    pub token: String,
    pub side: u8,
    pub shares: f64,
    pub limit: f64,
    pub held_before: f64,
    pub his_position: Option<f64>,
    pub his_order: Option<f64>,
    pub his_filled: Option<f64>,
    pub our_copied: Option<f64>,
}
impl Provenance {
    pub fn own(
        origin: Origin,
        lane: &str,
        token: &str,
        side: u8,
        shares: f64,
        limit: f64,
        held_before: f64,
    ) -> Self {
        Self {
            origin,
            lane: lane.into(),
            token: token.into(),
            side,
            shares,
            limit,
            held_before,
            his_position: None,
            his_order: None,
            his_filled: None,
            our_copied: None,
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub fn mirrored(
        origin: Origin,
        lane: &str,
        token: &str,
        side: u8,
        shares: f64,
        limit: f64,
        held_before: f64,
        his_position: Option<f64>,
        his_order: Option<f64>,
        his_filled: Option<f64>,
        our_copied: Option<f64>,
    ) -> Self {
        Self {
            origin,
            lane: lane.into(),
            token: token.into(),
            side,
            shares,
            limit,
            held_before,
            his_position,
            his_order,
            his_filled,
            our_copied,
        }
    }
    pub fn pct_of_his_fill(&self) -> Option<f64> {
        let hf = self.his_filled?;
        if hf <= 0.0 {
            return None;
        }
        Some((self.our_copied.unwrap_or(0.0) + self.shares) / hf)
    }
    pub fn pct_of_our_holding(&self) -> Option<f64> {
        if self.side != 1 || self.held_before <= 0.0 {
            return None;
        }
        Some(self.shares / self.held_before)
    }
    pub fn is_oversell(&self) -> bool {
        self.side == 1 && self.held_before > 0.0
            && self.shares
                > self.held_before + Self::oversell_tolerance(self.held_before)
    }
    fn oversell_tolerance(held: f64) -> f64 {
        0.01_f64.max(held.abs() * 0.001)
    }
    pub fn why(&self) -> String {
        self.origin.label()
    }
    pub fn to_json(&self) -> serde_json::Value {
        let round = |x: f64| (x * 10_000.0).round() / 10_000.0;
        let mut v = serde_json::json!(
            { "origin" : self.origin.kind(), "origin_detail" : self.origin.label(),
            "leader_driven" : self.origin.is_leader_driven(), "lane" : self.lane, "tok" :
            self.token, "side" : if self.side == 1 { "SELL" } else { "BUY" }, "shares" :
            round(self.shares), "limit" : round(self.limit), "usd" : round(self.shares *
            self.limit), "held_before" : round(self.held_before), }
        );
        let m = v.as_object_mut().expect("json object");
        if let Some(x) = self.his_position {
            m.insert("his_position".into(), round(x).into());
        }
        if let Some(x) = self.his_order {
            m.insert("his_order".into(), round(x).into());
        }
        if let Some(x) = self.his_filled {
            m.insert("his_filled".into(), round(x).into());
        }
        if let Some(x) = self.our_copied {
            m.insert("our_copied".into(), round(x).into());
        }
        if let Some(p) = self.pct_of_his_fill() {
            m.insert(
                "pct_of_his_fill".into(),
                ((p * 10_000.0).round() / 10_000.0).into(),
            );
        }
        if let Some(p) = self.pct_of_our_holding() {
            m.insert(
                "pct_of_holding".into(),
                ((p * 10_000.0).round() / 10_000.0).into(),
            );
        }
        if self.is_oversell() {
            m.insert("OVERSELL".into(), true.into());
        }
        v
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn buy(shares: f64, his_filled: f64, our_copied: f64) -> Provenance {
        Provenance::mirrored(
            Origin::MirrorBuy,
            "example_lane_26",
            "T",
            0,
            shares,
            0.08,
            0.0,
            Some(5000.0),
            Some(5000.0),
            Some(his_filled),
            Some(our_copied),
        )
    }
    #[test]
    fn every_origin_has_a_stable_kind_and_they_are_all_distinct() {
        let all = [
            Origin::MirrorBuy,
            Origin::MirrorSell,
            Origin::ExitLadder,
            Origin::ExitRescue { attempt: 1 },
            Origin::DustSweep {
                reason: "r".into(),
            },
            Origin::Flatten {
                mode: "panic".into(),
            },
            Origin::MergeReconcile,
            Origin::OrphanSweep,
        ];
        let kinds: std::collections::HashSet<_> = all.iter().map(|o| o.kind()).collect();
        assert_eq!(
            kinds.len(), all.len(),
            "two origins share a label — forensics cannot \
                                            tell those paths apart"
        );
        assert!(all.iter().all(| o | ! o.kind().is_empty()));
    }
    #[test]
    fn the_detail_that_distinguishes_two_runs_survives_into_the_label() {
        assert_eq!(Origin::Flatten { mode : "panic".into() } .label(), "flatten:panic");
        assert_eq!(
            Origin::Flatten { mode : "hybrid".into() } .label(), "flatten:hybrid"
        );
        assert_eq!(Origin::ExitRescue { attempt : 3 } .label(), "exit_rescue:attempt3");
    }
    #[test]
    fn THE_RUNAWAY_METRIC_is_a_field_not_an_investigation() {
        let tranche = buy(14.86, 2236.0, 1519.0);
        let pct = tranche.pct_of_his_fill().unwrap();
        assert!(
            pct > 0.68 && pct < 0.70, "expected the measured runaway ratio, got {pct}"
        );
        let ok = buy(483.0, 5000.0, 0.0);
        assert!((ok.pct_of_his_fill().unwrap() - 0.0966).abs() < 0.001);
        assert!(
            buy(14.86, 1.16, 0.0).pct_of_his_fill().unwrap() > 1.0,
            "the FIRST clip already exceeded his fill; the floor lift is visible \
                 from row one when the ratio is carried"
        );
    }
    #[test]
    fn an_UNKNOWN_leader_position_is_ABSENT_from_the_row_never_zero() {
        let p = Provenance::own(
            Origin::DustSweep {
                reason: "x".into(),
            },
            "example_lane_26",
            "T",
            1,
            5.0,
            0.5,
            10.0,
        );
        let j = p.to_json();
        assert!(
            j.get("his_position").is_none(),
            "an unknown leader position rendered as a number reads as `he is flat`"
        );
        assert!(j.get("pct_of_his_fill").is_none());
    }
    #[test]
    fn a_sell_reports_what_fraction_of_our_position_it_takes() {
        let p = Provenance::own(Origin::MirrorSell, "example_lane_26", "T", 1, 50.0, 0.5, 200.0);
        assert_eq!(p.pct_of_our_holding(), Some(0.25));
        assert!(! p.is_oversell());
    }
    #[test]
    fn a_FULL_EXIT_OF_THE_CHAIN_BALANCE_is_not_an_oversell() {
        let p = Provenance::own(
            Origin::Flatten {
                mode: "panic".into(),
            },
            "example_lane_26",
            "T",
            1,
            1780.15,
            0.09,
            1780.1426,
        );
        assert!(
            ! p.is_oversell(), "a full exit of the chain balance is the normal case"
        );
        assert!(p.to_json().get("OVERSELL").is_none());
        assert!(
            ! Provenance::own(Origin::MirrorSell, "example_lane_26", "T", 1, 50_010.0, 0.5,
            50_000.0).is_oversell()
        );
    }
    #[test]
    fn an_OVERSELL_is_flagged_on_the_row_itself() {
        let p = Provenance::own(Origin::OrphanSweep, "example_lane_26", "T", 1, 300.0, 0.5, 200.0);
        assert!(p.is_oversell());
        assert_eq!(p.to_json().get("OVERSELL").and_then(| v | v.as_bool()), Some(true));
        assert!(p.pct_of_our_holding().unwrap() > 1.0);
    }
    #[test]
    fn a_BUY_never_reports_a_holding_fraction() {
        let p = buy(100.0, 1000.0, 0.0);
        assert_eq!(p.pct_of_our_holding(), None);
        assert!(! p.is_oversell());
    }
    #[test]
    fn leader_driven_separates_HE_SOLD_from_WE_DECIDED() {
        assert!(Origin::MirrorSell.is_leader_driven());
        assert!(! Origin::Flatten { mode : "panic".into() } .is_leader_driven());
        assert!(! Origin::OrphanSweep.is_leader_driven());
        assert!(! Origin::DustSweep { reason : "r".into() } .is_leader_driven());
    }
    #[test]
    fn the_persisted_why_is_the_origin_label_so_old_rows_still_parse() {
        let p = Provenance::own(
            Origin::Flatten {
                mode: "graceful".into(),
            },
            "example_lane_26",
            "T",
            1,
            5.0,
            0.5,
            5.0,
        );
        assert_eq!(p.why(), "flatten:graceful");
    }
    #[test]
    fn a_zero_or_absent_denominator_is_None_not_a_division_by_zero() {
        assert_eq!(buy(10.0, 0.0, 0.0).pct_of_his_fill(), None);
        let p = Provenance::own(Origin::MirrorSell, "example_lane_26", "T", 1, 5.0, 0.5, 0.0);
        assert_eq!(p.pct_of_our_holding(), None);
        assert!(! p.is_oversell(), "no position means no oversell, not an infinite one");
    }
}
