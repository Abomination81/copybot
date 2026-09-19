use serde::Deserialize;
#[derive(Debug, Clone, Copy)]
pub struct PositionView {
    pub redeemable: bool,
    pub cur_price: f64,
}
pub fn resolved_payout(pos: PositionView) -> Option<f64> {
    if pos.redeemable && pos.cur_price.is_finite() && (0.0..=1.0).contains(&pos.cur_price) {
        Some(pos.cur_price)
    } else { None }
}
#[derive(Debug, Clone, Deserialize)]
pub struct Redemption {
    #[serde(rename = "transactionHash", default)]
    pub tx: String,
    #[serde(rename = "conditionId", default)]
    pub condition_id: String,
    #[serde(rename = "outcomeIndex")]
    pub outcome_index: u32,
    #[serde(default)]
    pub asset: String,
    #[serde(default)]
    pub size: f64,
    #[serde(rename = "usdcSize")]
    pub usdc: f64,
    #[serde(rename = "timestamp", default)]
    pub ts: i64,
}
impl Redemption {
    pub fn key(&self) -> String {
        format!("{}:{}:{}", self.tx, self.condition_id, self.outcome_index)
    }
    pub fn lane_key(&self, lane: &str) -> String {
        format!("{lane}|{}", self.key())
    }
    pub fn is_bookable(&self) -> bool {
        !self.tx.is_empty() && !self.condition_id.is_empty() && self.ts > 0
            && self.size.is_finite() && self.size > 1e-9 && self.usdc.is_finite()
            && (0.0..=1.0).contains(&(self.usdc / self.size))
    }
    pub fn token<'a>(&self, market_tokens: &'a [String]) -> Option<&'a str> {
        let token = market_tokens.get(self.outcome_index as usize)?;
        if token.is_empty() || (!self.asset.is_empty() && self.asset != *token) {
            return None;
        }
        Some(token)
    }
}
pub fn cross_writer_key(lane: &str, token: &str) -> String {
    format!("settle:{lane}:{token}")
}
pub fn parse_redemptions(body: &serde_json::Value) -> Vec<Redemption> {
    let Some(rows) = body.as_array() else { return Vec::new() };
    rows.iter()
        .filter(|r| r["type"].as_str() == Some("REDEEM"))
        .filter_map(|r| serde_json::from_value::<Redemption>(r.clone()).ok())
        .filter(|r| r.is_bookable())
        .collect()
}
pub fn unbooked<'a>(
    redemptions: &'a [Redemption],
    already_booked: &std::collections::HashSet<String>,
) -> Vec<&'a Redemption> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    redemptions
        .iter()
        .filter(|r| !already_booked.contains(&r.key()))
        .filter(|r| seen.insert(r.key()))
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    fn red(tx: &str, cond: &str, idx: u32, size: f64, usdc: f64, ts: i64) -> Redemption {
        Redemption {
            tx: tx.into(),
            asset: String::new(),
            condition_id: cond.into(),
            outcome_index: idx,
            size,
            usdc,
            ts,
        }
    }
    #[test]
    fn a_DUPLICATE_row_in_ONE_batch_is_booked_only_once() {
        let rows = vec![
            red("0xaa", "0xc1", 0, 100.0, 100.0, 1_000), red("0xaa", "0xc1", 0, 100.0,
            100.0, 1_000), red("0xbb", "0xc1", 0, 50.0, 50.0, 1_010)
        ];
        let got = unbooked(&rows, &std::collections::HashSet::new());
        assert_eq!(
            got.len(), 2, "the duplicate must be collapsed, the distinct one kept"
        );
        assert_eq!(got[0].tx, "0xaa");
        assert_eq!(got[1].tx, "0xbb");
    }
    #[test]
    fn an_ALREADY_BOOKED_key_is_still_skipped() {
        let rows = vec![red("0xaa", "0xc1", 0, 100.0, 100.0, 1_000)];
        let booked: std::collections::HashSet<String> = [rows[0].key()]
            .into_iter()
            .collect();
        assert!(unbooked(& rows, & booked).is_empty());
    }
    #[test]
    fn the_idempotency_key_is_PER_LANE() {
        let r = red("0xaa", "0xc1", 0, 100.0, 100.0, 1_000);
        assert_ne!(r.lane_key("example_lane_26"), r.lane_key("example_lane_25"));
        assert!(r.lane_key("example_lane_26").contains(& r.key()));
    }
    #[test]
    fn two_outcomes_of_ONE_tx_stay_distinct() {
        let a = red("0xaa", "0xc1", 0, 10.0, 10.0, 1);
        let b = red("0xaa", "0xc1", 1, 10.0, 0.0, 1);
        assert_ne!(a.key(), b.key());
        assert_eq!(unbooked(& [a, b], & std::collections::HashSet::new()).len(), 2);
    }
    #[test]
    fn a_redeemable_winner_books_at_par_a_loser_at_zero() {
        assert_eq!(
            resolved_payout(PositionView { redeemable : true, cur_price : 1.0 }),
            Some(1.0)
        );
        assert_eq!(
            resolved_payout(PositionView { redeemable : true, cur_price : 0.0 }),
            Some(0.0)
        );
        assert_eq!(
            resolved_payout(PositionView { redeemable : true, cur_price : 1.4 }),
            None
        );
        assert_eq!(
            resolved_payout(PositionView { redeemable : true, cur_price : - 0.2 }),
            None
        );
    }
    #[test]
    fn a_still_trading_position_is_left_alone() {
        assert_eq!(
            resolved_payout(PositionView { redeemable : false, cur_price : 0.93 }), None
        );
    }
    #[test]
    fn parse_keeps_only_bookable_redeems() {
        let body = serde_json::json!(
            [{ "type" : "REDEEM", "transactionHash" : "0xaa", "conditionId" : "0xc1",
            "outcomeIndex" : 0, "size" : 5.11, "usdcSize" : 5.11, "timestamp" : 1000 }, {
            "type" : "TRADE", "transactionHash" : "0xbb", "size" : 9.0 }, { "type" :
            "REDEEM", "transactionHash" : "0xcc", "conditionId" : "0xc2", "outcomeIndex"
            : 1, "size" : 0.0, "usdcSize" : 0.0, "timestamp" : 1001 },]
        );
        let reds = parse_redemptions(&body);
        assert_eq!(reds.len(), 1);
        assert_eq!(reds[0].tx, "0xaa");
        assert!((reds[0].usdc - 5.11).abs() < 1e-9);
    }
    #[test]
    fn unbooked_filters_out_already_recorded_keys() {
        let body = serde_json::json!(
            [{ "type" : "REDEEM", "transactionHash" : "0xaa", "conditionId" : "0xc1",
            "outcomeIndex" : 0, "size" : 5.0, "usdcSize" : 5.0, "timestamp" : 1000 }, {
            "type" : "REDEEM", "transactionHash" : "0xbb", "conditionId" : "0xc2",
            "outcomeIndex" : 1, "size" : 7.0, "usdcSize" : 3.5, "timestamp" : 1001 },]
        );
        let reds = parse_redemptions(&body);
        let mut seen = std::collections::HashSet::new();
        seen.insert(reds[0].key());
        let todo = unbooked(&reds, &seen);
        assert_eq!(todo.len(), 1);
        assert_eq!(todo[0].tx, "0xbb");
    }
    #[test]
    fn the_idempotency_key_separates_two_conditions_in_one_tx() {
        let a = Redemption {
            tx: "0xtx".into(),
            asset: String::new(),
            condition_id: "0xc1".into(),
            outcome_index: 0,
            size: 1.0,
            usdc: 1.0,
            ts: 1,
        };
        let b = Redemption {
            tx: "0xtx".into(),
            asset: String::new(),
            condition_id: "0xc2".into(),
            outcome_index: 0,
            size: 1.0,
            usdc: 1.0,
            ts: 1,
        };
        assert_ne!(a.key(), b.key(), "one tx redeeming two markets must be two keys");
    }
}
