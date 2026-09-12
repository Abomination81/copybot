use std::collections::HashMap;
use std::sync::Mutex;
#[derive(Debug, Clone, PartialEq)]
pub struct Resting {
    pub order_id: String,
    pub lane: String,
    pub token: String,
    pub side: u8,
    pub limit: f64,
    pub shares: f64,
    pub placed: i64,
    pub salt: [u8; 32],
    pub condition: [u8; 32],
    pub his_price: f64,
    pub his_remaining: f64,
    pub printed_at_place: f64,
    pub contested_at_place: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    HeExited,
    Expired,
    Disarmed,
    OverCap,
    HeCancelled,
    RestingDisabled,
    LaneHalted,
}
pub fn halted_lane_must_pull(
    side: u8,
    halted: bool,
    armed: bool,
    retired: bool,
) -> bool {
    if side != 0 {
        return false;
    }
    halted || retired || !armed
}
impl CancelReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            CancelReason::HeExited => "he_exited",
            CancelReason::HeCancelled => "he_cancelled",
            CancelReason::RestingDisabled => "resting_disabled",
            CancelReason::LaneHalted => "lane_halted",
            CancelReason::Expired => "expired",
            CancelReason::Disarmed => "disarmed",
            CancelReason::OverCap => "over_cap",
        }
    }
}
#[derive(Default)]
pub struct RestingBook {
    settling: Mutex<std::collections::HashSet<String>>,
    inner: Mutex<HashMap<String, Resting>>,
}
impl RestingBook {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            settling: Mutex::new(std::collections::HashSet::new()),
        }
    }
    pub fn add(&self, r: Resting) {
        self.inner.lock().unwrap().insert(r.order_id.clone(), r);
    }
    pub fn contains(&self, order_id: &str) -> bool {
        let g = self.inner.lock().unwrap();
        let bare = order_id.trim_start_matches("0x");
        g.contains_key(order_id) || g.contains_key(&format!("0x{bare}"))
            || g.contains_key(bare)
    }
    pub fn try_claim_settle(&self, order_id: &str) -> bool {
        self.settling.lock().unwrap().insert(Self::key(order_id))
    }
    pub fn finish_settle(&self, order_id: &str) {
        self.settling.lock().unwrap().remove(&Self::key(order_id));
    }
    fn key(order_id: &str) -> String {
        order_id.trim_start_matches("0x").to_string()
    }
    pub fn remove(&self, order_id: &str) -> Option<Resting> {
        let mut g = self.inner.lock().unwrap();
        let bare = order_id.trim_start_matches("0x");
        g.remove(order_id)
            .or_else(|| g.remove(&format!("0x{bare}")))
            .or_else(|| g.remove(bare))
    }
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn by_token(&self, token: &str) -> Vec<Resting> {
        self.inner
            .lock()
            .unwrap()
            .values()
            .filter(|r| r.token == token)
            .cloned()
            .collect()
    }
    pub fn expired(&self, now: i64, ttl_secs: i64) -> Vec<Resting> {
        self.inner
            .lock()
            .unwrap()
            .values()
            .filter(|r| now - r.placed >= ttl_secs)
            .cloned()
            .collect()
    }
    pub fn all(&self) -> Vec<Resting> {
        self.inner.lock().unwrap().values().cloned().collect()
    }
    pub fn rehydrate(&self, rows: &serde_json::Value, fallback_lane: &str) -> usize {
        let Some(rows) = rows.as_array() else { return 0 };
        let num = |v: &serde_json::Value, k: &str| -> f64 {
            v[k]
                .as_f64()
                .or_else(|| v[k].as_str().and_then(|x| x.parse().ok()))
                .unwrap_or(0.0)
        };
        let mut n = 0;
        for r in rows {
            let Some(id) = r["id"].as_str().or_else(|| r["orderID"].as_str()) else {
                continue
            };
            if self.contains(id) {
                continue;
            }
            let Some(token) = r["asset_id"].as_str().or_else(|| r["market"].as_str())
            else { continue };
            let original = num(r, "original_size").max(num(r, "size"));
            let matched = num(r, "size_matched");
            let remaining = (original - matched).max(0.0);
            if remaining <= 1e-9 {
                continue;
            }
            let side = match r["side"]
                .as_str()
                .unwrap_or("")
                .to_ascii_uppercase()
                .as_str()
            {
                "SELL" => 1,
                _ => 0,
            };
            self.add(Resting {
                order_id: id.to_string(),
                lane: fallback_lane.to_string(),
                token: token.to_string(),
                side,
                limit: num(r, "price"),
                shares: remaining,
                placed: r["created_at"].as_i64().unwrap_or_else(crate::ledger::now_secs),
                salt: [0u8; 32],
                condition: [0u8; 32],
                his_price: 0.0,
                his_remaining: 0.0,
                printed_at_place: 0.0,
                contested_at_place: true,
            });
            n += 1;
        }
        n
    }
    pub fn over_cap(&self, cap: usize) -> Vec<Resting> {
        let mut v = self.all();
        if v.len() <= cap {
            return Vec::new();
        }
        v.sort_by_key(|r| r.placed);
        v.truncate(v.len() - cap);
        v
    }
    pub const MAX_PRUNE_FRAC: f64 = 0.34;
    pub const PRUNE_FRAC_APPLIES_ABOVE: usize = 4;
    pub fn prune_is_plausible(&self, prune_count: usize) -> bool {
        let live = self.len();
        if live <= Self::PRUNE_FRAC_APPLIES_ABOVE {
            return true;
        }
        (prune_count as f64) < (live as f64) * Self::MAX_PRUNE_FRAC
    }
    pub fn over_cap_per_lane(&self, cap: usize) -> Vec<Resting> {
        let mut by_lane: std::collections::HashMap<String, Vec<Resting>> = Default::default();
        for r in self.all() {
            by_lane.entry(r.lane.clone()).or_default().push(r);
        }
        let mut doomed = Vec::new();
        for (_, mut v) in by_lane {
            if v.len() <= cap {
                continue;
            }
            v.sort_by_key(|r| r.placed);
            v.truncate(v.len() - cap);
            doomed.extend(v);
        }
        doomed
    }
    pub fn snapshot(&self) -> serde_json::Value {
        let mut v = self.all();
        v.sort_by_key(|r| r.placed);
        serde_json::json!(
            { "n" : v.len(), "orders" : v.iter().map(| r | serde_json::json!({ "order_id"
            : r.order_id, "lane" : r.lane, "token" : r.token, "side" : if r.side == 0 {
            "BUY" } else { "SELL" }, "limit" : r.limit, "shares" : r.shares, "placed" : r
            .placed, })).collect::< Vec < _ >> (), }
        )
    }
}
pub fn cancel_body(ids: &[String]) -> String {
    serde_json::to_string(ids).unwrap_or_else(|_| "[]".into())
}
pub fn cancel_succeeded(order_id: &str, body: &str) -> bool {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return false,
    };
    if let Some(c) = v.get("canceled").and_then(|x| x.as_array()) {
        if c.iter().any(|x| x.as_str() == Some(order_id)) {
            return true;
        }
    }
    if let Some(nc) = v.get("not_canceled").and_then(|x| x.as_object()) {
        if let Some(reason) = nc.get(order_id).and_then(|x| x.as_str()) {
            let r = reason.to_ascii_lowercase();
            return r.contains("can't be found") || r.contains("cannot be found")
                || r.contains("already canceled") || r.contains("matched");
        }
        return false;
    }
    false
}
#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::halted_lane_must_pull as pull;
    #[test]
    fn A_HALTED_LANES_RESTING_BUY_IS_PULLED() {
        assert!(pull(0, true, true, false));
    }
    #[test]
    fn A_HALTED_LANES_RESTING_SELL_IS_NEVER_PULLED() {
        assert!(! pull(1, true, true, false));
        assert!(! pull(1, true, false, true), "not even a retired, disarmed lane");
    }
    #[test]
    fn a_DISARMED_lane_pulls_its_buys_even_while_other_lanes_trade() {
        assert!(pull(0, false, false, false));
    }
    #[test]
    fn a_RETIRED_lane_pulls_its_buys_but_still_drains_its_sells() {
        assert!(pull(0, false, true, true));
        assert!(! pull(1, false, true, true), "a retired lane still follows him out");
    }
    #[test]
    fn a_HEALTHY_armed_lane_keeps_both_sides_working() {
        assert!(! pull(0, false, true, false));
        assert!(! pull(1, false, true, false));
    }
    #[test]
    fn RT_the_boot_window_does_not_mass_cancel_before_lanes_arm() {
        assert!(
            pull(0, false, false, false),
            "an unarmed lane's buy IS pullable — but only once some lane is armed"
        );
    }
    #[test]
    fn RT_a_SELL_survives_every_combination_of_lane_state() {
        for halted in [false, true] {
            for armed in [false, true] {
                for retired in [false, true] {
                    assert!(
                        ! pull(1, halted, armed, retired),
                        "a resting SELL must never be pulled by lane state \
                             (halted={halted} armed={armed} retired={retired})"
                    );
                }
            }
        }
    }
    #[test]
    fn RT_an_unknown_side_is_treated_as_a_SELL_and_left_alone() {
        for bad in [2u8, 7, 255] {
            assert!(! pull(bad, true, false, true), "only side==0 is risk-increasing");
        }
    }
    #[test]
    fn RT_the_predicate_is_pure_and_stable_under_repetition() {
        for _ in 0..100 {
            assert!(pull(0, true, true, false));
            assert!(! pull(0, false, true, false));
        }
    }
    #[test]
    fn the_reason_is_named_in_the_audit_trail() {
        assert_eq!(super::CancelReason::LaneHalted.as_str(), "lane_halted");
    }
    use super::*;
    fn r(id: &str, token: &str, side: u8, placed: i64) -> Resting {
        Resting {
            order_id: id.into(),
            lane: "example_lane_26".into(),
            token: token.into(),
            side,
            limit: 0.62,
            shares: 5.0,
            placed,
            salt: [7u8; 32],
            condition: [9u8; 32],
            his_price: 0.62,
            his_remaining: 4600.0,
            printed_at_place: 0.0,
            contested_at_place: false,
        }
    }
    fn r_lane(id: &str, lane: &str, placed: i64) -> Resting {
        let mut x = r(id, "T", 0, placed);
        x.lane = lane.into();
        x
    }
    #[test]
    fn the_per_lane_cap_protects_a_QUIET_lane_from_a_busy_one() {
        let b = RestingBook::new();
        for i in 0..10 {
            b.add(r_lane(&format!("busy{i}"), "example_lane_26", 100 + i));
        }
        b.add(r_lane("quiet0", "example_lane_25", 100));
        let doomed = b.over_cap_per_lane(6);
        assert_eq!(doomed.len(), 4, "only example_lane_26's oldest four exceed the per-lane cap");
        assert!(
            doomed.iter().all(| o | o.lane == "example_lane_26"),
            "a quiet lane's single rest must never be evicted for a busy lane"
        );
    }
    #[test]
    fn rehydrate_NEVER_overwrites_a_row_we_already_track() {
        let b = RestingBook::new();
        b.add(r("0xabc", "T", 0, 100));
        let venue = serde_json::json!(
            [{ "id" : "0xabc", "asset_id" : "T", "side" : "BUY", "price" : "0.5",
            "original_size" : "10", "size_matched" : "0" }, { "id" : "0xnew", "asset_id"
            : "T2", "side" : "SELL", "price" : "0.7", "original_size" : "5",
            "size_matched" : "0" }]
        );
        assert_eq!(b.rehydrate(& venue, "example_lane_26"), 1, "only the UNKNOWN order is adopted");
        let ours = b.all().into_iter().find(|x| x.order_id == "0xabc").unwrap();
        assert_eq!(ours.salt, [7u8; 32], "our live row kept its real identity");
        assert!(b.contains("0xnew"));
    }
    #[test]
    fn a_MAJORITY_prune_is_refused_because_it_indicts_the_READ() {
        let b = RestingBook::new();
        for i in 0..10 {
            b.add(r(&format!("o{i}"), "T", 0, 100 + i));
        }
        assert!(b.prune_is_plausible(3), "a few real cancels are ordinary");
        assert!(! b.prune_is_plausible(4), "a third or more indicts the read");
        assert!(! b.prune_is_plausible(10));
        let tiny = RestingBook::new();
        for i in 0..3 {
            tiny.add(r(&format!("t{i}"), "T", 0, 100 + i));
        }
        assert!(tiny.prune_is_plausible(3), "1-of-3 is not evidence of a bad read");
    }
    #[test]
    fn only_ONE_settler_may_claim_an_order() {
        let b = RestingBook::new();
        b.add(r("0xabc", "T", 0, 100));
        assert!(b.try_claim_settle("0xabc"), "first caller wins");
        assert!(! b.try_claim_settle("0xabc"), "second must be refused");
        assert!(! b.try_claim_settle("abc"), "and the bare form is the SAME order");
        b.finish_settle("abc");
        assert!(b.try_claim_settle("0xabc"), "released after the first finishes");
    }
    #[test]
    fn contains_and_remove_normalise_the_0x_prefix() {
        let b = RestingBook::new();
        b.add(r("0xdead", "T", 0, 100));
        assert!(b.contains("0xdead") && b.contains("dead"));
        assert!(b.remove("dead").is_some(), "a bare id must remove the 0x row");
        assert!(b.is_empty());
    }
    #[test]
    fn rehydrate_adopts_the_venues_OWN_open_orders() {
        let b = RestingBook::new();
        let rows = serde_json::json!(
            [{ "id" : "0xA", "asset_id" : "TOK1", "side" : "BUY", "price" : "0.42",
            "original_size" : "100", "size_matched" : "25", "created_at" : 1700 }, { "id"
            : "0xB", "asset_id" : "TOK2", "side" : "SELL", "price" : "0.90",
            "original_size" : "50", "size_matched" : "0", "created_at" : 1710 },]
        );
        assert_eq!(b.rehydrate(& rows, "example_lane_26"), 2);
        let all = b.all();
        let a = all.iter().find(|r| r.order_id == "0xA").expect("0xA");
        assert!(
            (a.shares - 75.0).abs() < 1e-9, "only the WORKING remainder is cancellable"
        );
        assert_eq!(a.side, 0);
        assert!((a.limit - 0.42).abs() < 1e-9);
        assert_eq!(a.placed, 1700);
        assert_eq!(all.iter().find(| r | r.order_id == "0xB").unwrap().side, 1);
    }
    #[test]
    fn a_FULLY_MATCHED_order_is_not_resting() {
        let b = RestingBook::new();
        let rows = serde_json::json!(
            [{ "id" : "0xA", "asset_id" : "T", "side" : "BUY", "price" : "0.5",
            "original_size" : "10", "size_matched" : "10" }]
        );
        assert_eq!(b.rehydrate(& rows, "example_lane_26"), 0);
        assert!(b.is_empty());
    }
    #[test]
    fn a_MALFORMED_row_is_skipped_without_losing_the_others() {
        let b = RestingBook::new();
        let rows = serde_json::json!(
            [{ "no_id" : true }, { "id" : "0xB", "asset_id" : "T", "side" : "BUY",
            "price" : "0.5", "original_size" : "5" },]
        );
        assert_eq!(b.rehydrate(& rows, "example_lane_26"), 1);
    }
    #[test]
    fn a_NON_ARRAY_payload_adopts_nothing() {
        let b = RestingBook::new();
        assert_eq!(b.rehydrate(& serde_json::json!({ "error" : "nope" }), "example_lane_26"), 0);
    }
    #[test]
    fn his_sell_finds_our_BUY_on_that_token() {
        let b = RestingBook::new();
        b.add(r("a", "T1", 0, 100));
        b.add(r("b", "T2", 0, 100));
        let hit = b.by_token("T1");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].order_id, "a");
    }
    #[test]
    fn by_token_does_not_filter_by_side() {
        let b = RestingBook::new();
        b.add(r("buy", "T1", 0, 100));
        b.add(r("sell", "T1", 1, 100));
        assert_eq!(b.by_token("T1").len(), 2);
    }
    #[test]
    fn expiry_is_inclusive_so_a_ttl_of_zero_kills_everything() {
        let b = RestingBook::new();
        b.add(r("a", "T1", 0, 1_000));
        assert_eq!(b.expired(1_030, 30).len(), 1, "exactly at the ttl must expire");
        assert_eq!(b.expired(1_029, 30).len(), 0);
    }
    #[test]
    fn over_cap_sheds_the_OLDEST_first() {
        let b = RestingBook::new();
        b.add(r("old", "T1", 0, 100));
        b.add(r("mid", "T2", 0, 200));
        b.add(r("new", "T3", 0, 300));
        let shed = b.over_cap(2);
        assert_eq!(shed.len(), 1);
        assert_eq!(shed[0].order_id, "old", "newest orders track his current intent");
        assert!(b.over_cap(3).is_empty());
        assert!(b.over_cap(9).is_empty());
    }
    #[test]
    fn removing_an_order_stops_it_being_cancelled_again() {
        let b = RestingBook::new();
        b.add(r("a", "T1", 0, 100));
        assert!(b.remove("a").is_some());
        assert!(b.remove("a").is_none());
        assert!(b.by_token("T1").is_empty());
    }
    #[test]
    fn an_order_the_venue_cannot_find_counts_as_CANCELLED() {
        let body = r#"{"not_canceled":{"0xabc":"order can't be found - already canceled or matched"}}"#;
        assert!(cancel_succeeded("0xabc", body));
    }
    #[test]
    fn an_explicitly_cancelled_id_counts() {
        assert!(
            cancel_succeeded("0xabc", r#"{"canceled":["0xabc"],"not_canceled":{}}"#)
        );
    }
    #[test]
    fn an_id_the_venue_REFUSED_for_a_real_reason_does_NOT_count() {
        let body = r#"{"not_canceled":{"0xabc":"not owned by this api key"}}"#;
        assert!(! cancel_succeeded("0xabc", body));
    }
    #[test]
    fn an_unparseable_or_error_body_is_never_success() {
        assert!(! cancel_succeeded("0xabc", "<html>502</html>"));
        assert!(! cancel_succeeded("0xabc", r#"{"error":"unauthorized"}"#));
    }
    #[test]
    fn an_id_absent_from_both_lists_remains_tracked() {
        assert!(
            ! cancel_succeeded("0xabc", r#"{"canceled":["0xother"],"not_canceled":{}}"#)
        );
        assert!(! cancel_succeeded("0xabc", r#"{}"#));
    }
    #[test]
    fn cancel_body_is_a_json_array_of_ids() {
        assert_eq!(cancel_body(& ["a".into(), "b".into()]), r#"["a","b"]"#);
        assert_eq!(cancel_body(& []), "[]");
    }
}
pub async fn cancel_batch(
    http: &reqwest::Client,
    host: &str,
    address: &str,
    creds: &crate::auth::ApiCreds,
    ids: &[String],
) -> Vec<(String, bool)> {
    if ids.is_empty() {
        return Vec::new();
    }
    let body = cancel_body(ids);
    let ts = crate::auth::now_secs();
    let hdrs = match crate::auth::l2_headers(
        address,
        creds,
        ts,
        "DELETE",
        "/orders",
        Some(&body),
    ) {
        Ok(h) => h,
        Err(_) => return ids.iter().map(|i| (i.clone(), false)).collect(),
    };
    let mut rb = http
        .delete(format!("{host}/orders"))
        .header("Content-Type", "application/json")
        .body(body);
    for (k, v) in &hdrs {
        rb = rb.header(*k, v);
    }
    let text = match rb.send().await {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return ids.iter().map(|i| (i.clone(), false)).collect(),
    };
    ids.iter().map(|i| (i.clone(), cancel_succeeded(i, &text))).collect()
}
