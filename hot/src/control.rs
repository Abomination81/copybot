use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use crate::ledger::Ledger;
use crate::lanes::{Router, MICRO};
pub struct Control {
    pub ledger: Ledger,
    pub operator_path: String,
    pub his_pos: HashMap<String, HashMap<String, f64>>,
    pub legacy: HashMap<String, HashMap<String, f64>>,
    pub fires: u64,
    pub skips: HashMap<String, u64>,
    pub ambiguous: Vec<serde_json::Value>,
    pub trips: Vec<serde_json::Value>,
    pub reconciliations: Vec<serde_json::Value>,
    pub boot_faults: HashMap<String, String>,
    pub guardian_stale: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
fn halt_causes(
    reason: &Option<String>,
    persistence_fault: bool,
    boot_fault: Option<&String>,
) -> Vec<String> {
    let mut causes = Vec::new();
    if let Some(r) = reason {
        causes.push(format!("breaker: {r}"));
    }
    if persistence_fault {
        causes.push("ledger persistence fault".to_string());
    }
    if let Some(f) = boot_fault {
        causes.push(format!("boot fault: {f}"));
    }
    causes
}
impl Control {
    pub fn new(ledger: Ledger, operator_path: &str) -> Self {
        let his_pos = ledger.lanes.keys().map(|k| (k.clone(), HashMap::new())).collect();
        let legacy = ledger.lanes.keys().map(|k| (k.clone(), HashMap::new())).collect();
        Self {
            ledger,
            operator_path: operator_path.into(),
            his_pos,
            legacy,
            fires: 0,
            skips: HashMap::new(),
            ambiguous: Vec::new(),
            trips: Vec::new(),
            reconciliations: Vec::new(),
            boot_faults: HashMap::new(),
            guardian_stale: std::sync::Arc::new(
                std::sync::atomic::AtomicBool::new(false),
            ),
        }
    }
    pub fn set_boot_fault(&mut self, lane: &str, reason: impl Into<String>) {
        self.boot_faults.entry(lane.to_string()).or_insert_with(|| reason.into());
    }
    pub fn operator_intent(&self) -> HashMap<String, bool> {
        let raw = match std::fs::read_to_string(&self.operator_path) {
            Ok(r) => r,
            Err(_) => return HashMap::new(),
        };
        let v: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return HashMap::new(),
        };
        v["lanes"]
            .as_object()
            .map(|o| {
                o
                    .iter()
                    .map(|(k, x)| (k.clone(), x["armed"].as_bool().unwrap_or(false)))
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn operator_halts(&self) -> HashMap<String, bool> {
        let raw = match std::fs::read_to_string(&self.operator_path) {
            Ok(r) => r,
            Err(_) => return HashMap::new(),
        };
        let v: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return HashMap::new(),
        };
        v["lanes"]
            .as_object()
            .map(|o| {
                o
                    .iter()
                    .map(|(k, x)| (k.clone(), x["halt_buys"].as_bool().unwrap_or(false)))
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn seed_his_book(
        &mut self,
        lane: &str,
        positions: &HashMap<String, f64>,
    ) -> usize {
        let mut marked = 0;
        let held = self.ledger.holdings(lane);
        for (token, shares) in positions {
            self.his_pos.entry(lane.into()).or_default().insert(token.clone(), *shares);
            if held.get(token).copied().unwrap_or(0.0) > 1e-9 {
                continue;
            }
            self.legacy.entry(lane.into()).or_default().insert(token.clone(), *shares);
            marked += 1;
        }
        marked
    }
    pub fn observe_his_fill(
        &mut self,
        lane: &str,
        token: &str,
        side: u8,
        size: f64,
    ) -> f64 {
        let hp = self.his_pos.entry(lane.into()).or_default();
        let cur = hp.get(token).copied().unwrap_or(0.0);
        let next = if side == 0 { cur + size } else { (cur - size).max(0.0) };
        hp.insert(token.into(), next);
        if side == 1 {
            if let Some(l) = self.legacy.get_mut(lane) {
                if let Some(v) = l.get_mut(token) {
                    *v = (*v - size).max(0.0);
                }
            }
        }
        next
    }
    pub fn book_response(
        &mut self,
        lane: &str,
        token: &str,
        side: u8,
        limit: f64,
        raw: &str,
        his_price: Option<f64>,
        order_hash: &str,
        resting: bool,
    ) -> Option<crate::ledger::Booked> {
        let resp: serde_json::Value = serde_json::from_str(raw)
            .unwrap_or(serde_json::Value::Null);
        let Some(book) = self.ledger.lanes.get_mut(lane) else { return None };
        if !resp.is_object() {
            book.rejected += 1;
            return None;
        }
        let err = resp["errorMsg"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| resp["error"].as_str());
        if resp["success"] == serde_json::json!(false) || err.is_some() {
            book.rejected += 1;
            return None;
        }
        let share_key = if side == 0 { "takingAmount" } else { "makingAmount" };
        let read = |k: &str| {
            resp[k]
                .as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .or_else(|| resp[k].as_f64())
        };
        let mut size = [
            "size_matched",
            "sizeMatched",
            share_key,
            "filled_size",
            "filledSize",
        ]
            .iter()
            .find_map(|k| read(k))
            .unwrap_or(0.0);
        let matched = resp["status"]
            .as_str()
            .map(|s| {
                let s = s.to_ascii_lowercase();
                s == "matched" || s == "filled" || s == "success"
            })
            .unwrap_or(false);
        let has_tx = resp["transactionsHashes"].is_array()
            || resp["transactionHashes"].is_array();
        if size <= 0.0 {
            if matched || has_tx || resp["success"] == serde_json::json!(true) {
                self.ambiguous
                    .push(
                        serde_json::json!(
                            { "lane" : lane, "token" : token, "raw" : raw.chars()
                            .take(200).collect::< String > () }
                        ),
                    );
            } else {
                book.rejected += 1;
            }
            return None;
        }
        let other_keys: &[&str] = if side == 0 {
            &[
                "makingAmount",
                "making_amount",
                "makerAmount",
                "maker_amount",
                "matched_amount",
                "matchedAmount",
                "value",
                "usd",
            ]
        } else {
            &[
                "takingAmount",
                "taking_amount",
                "takerAmount",
                "taker_amount",
                "matched_amount",
                "matchedAmount",
                "value",
                "usd",
            ]
        };
        let mut price = limit;
        let mut px_provisional = true;
        if let Some(usd) = other_keys.iter().find_map(|k| read(k)) {
            if usd > 0.0 && size > 0.0 {
                price = usd / size;
                px_provisional = false;
            }
        }
        if side == 1 {
            if let Some(p) = book.positions.get(token) {
                size = size.min(p.shares);
            }
        }
        if size > 0.0 {
            let fee = ["fee", "feeAmount", "fee_amount", "makerFee", "takerFee"]
                .iter()
                .find_map(|k| read(k))
                .or_else(|| {
                    ["fee_rate_bps", "feeRateBps"]
                        .iter()
                        .find_map(|k| read(k))
                        .map(|bps| bps / 10_000.0 * price * size)
                })
                .filter(|f| f.is_finite() && *f >= 0.0)
                .unwrap_or(0.0);
            if fee > 0.0 {
                eprintln!(
                    "[fee] ⚠️ VENUE REPORTED A FEE: {fee:.6} on {lane} {token} \
                           — fee-free market assumption no longer holds"
                );
            }
            if px_provisional {
                self.ambiguous
                    .push(
                        serde_json::json!(
                            { "lane" : lane, "token" : token, "why" :
                            "price PROVISIONAL: the venue reply carried no amount leg, so \
this fill is booked at our LIMIT — dearer than truth on a buy, cheaper on a sell"
                            }
                        ),
                    );
            }
            self.ledger
                .book_fill_ex_px(
                    lane,
                    token,
                    side,
                    size,
                    price,
                    fee,
                    his_price,
                    order_hash,
                    resting,
                    px_provisional,
                )
        } else {
            None
        }
    }
    pub fn tick(&mut self, router: &Arc<Router>) {
        self.tick_with(router, &crate::pending::InFlight::default())
    }
    pub fn tick_with(
        &mut self,
        router: &Arc<Router>,
        in_flight: &crate::pending::InFlight,
    ) {
        let intent = self.operator_intent();
        let halts = self.operator_halts();
        let persistence_fault = !self.ledger.persistence_ok();
        for lane in router.snapshot().iter() {
            let name = &lane.cfg.name;
            let Some(book) = self.ledger.lanes.get_mut(name) else { continue };
            book.roll_day(crate::ledger::now_secs());
            let reason = book.risk.evaluate();
            if let Some(r) = &reason {
                if !self.trips.iter().any(|t| t["lane"] == serde_json::json!(name)) {
                    self.trips.push(serde_json::json!({ "lane" : name, "reason" : r }));
                }
            }
            let wants = intent.get(name).copied().unwrap_or(false);
            let boot_fault = self.boot_faults.get(name);
            let breaker = reason.is_some() || persistence_fault || boot_fault.is_some();
            if breaker {
                if !lane.state.halt_latch.load(Ordering::Relaxed) {
                    eprintln!(
                        "[{name}] HALT_LATCH TRIP: {}", halt_causes(& reason,
                        persistence_fault, boot_fault).join("; ")
                    );
                }
                lane.state.halt_latch.store(true, Ordering::Relaxed);
            }
            let latched = lane.state.halt_latch.load(Ordering::Relaxed);
            let op_halt = halts.get(name).copied().unwrap_or(false);
            let unwatched = self.guardian_stale.load(Ordering::Relaxed);
            let halted = latched || op_halt || unwatched;
            let prev = lane.state.halted_since.load(Ordering::Relaxed);
            if halted && prev == 0 {
                lane.state
                    .halted_since
                    .store(crate::ledger::now_secs(), Ordering::Relaxed);
            } else if !halted && prev != 0 {
                lane.state.halted_since.store(0, Ordering::Relaxed);
            }
            lane.state.armed.store(wants && boot_fault.is_none(), Ordering::Relaxed);
            lane.state.halted.store(halted, Ordering::Relaxed);
            let flight = in_flight.by_lane.get(name).copied().unwrap_or(0.0);
            lane.state
                .spent_today
                .store(((book.spent_today + flight) * MICRO) as i64, Ordering::Relaxed);
            lane.state
                .open_usd
                .store(
                    ((self.ledger.open_usd(name) + flight) * MICRO) as i64,
                    Ordering::Relaxed,
                );
            let per_token = self
                .ledger
                .lanes
                .get(name)
                .map(|b| {
                    b.positions
                        .iter()
                        .filter(|(_, p)| p.shares > 1e-9 && p.cost > 1e-9)
                        .map(|(token, p)| (token.clone(), (p.cost * MICRO) as i64))
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default();
            let mut per_token = per_token;
            for ((l, tok), usd) in in_flight.by_token.iter() {
                if l != name {
                    continue;
                }
                *per_token.entry(tok.clone()).or_insert(0) += (*usd * MICRO) as i64;
            }
            *lane.per_token.lock().unwrap() = per_token;
            let held = self.ledger.holdings(name);
            let stale: Vec<String> = lane
                .holdings
                .lock()
                .unwrap()
                .keys()
                .filter(|k| !held.contains_key(*k))
                .cloned()
                .collect();
            for t in stale {
                lane.set_holding(&t, 0.0);
                router.release_by_name(&t, name);
            }
            let claimed = router.claims_of(name);
            for t in claimed {
                if held.contains_key(&t) {
                    continue;
                }
                if in_flight.tokens.iter().any(|(l, tok)| l == name && tok == &t) {
                    continue;
                }
                router.release_by_name(&t, name);
            }
            for (t, sh) in &held {
                lane.set_holding(t, *sh);
            }
            let mut reserved: HashMap<String, f64> = HashMap::new();
            for ((l, tok), sh) in in_flight.sell_shares_by_token.iter() {
                if l != name {
                    continue;
                }
                reserved.insert(tok.clone(), *sh);
            }
            lane.set_sell_reservations(reserved);
            if let Some(hp) = self.his_pos.get(name) {
                for (t, v) in hp {
                    lane.set_his(t, *v);
                }
            }
            if let Some(lg) = self.legacy.get(name) {
                for (t, v) in lg {
                    lane.set_legacy(t, *v);
                }
            }
        }
    }
    pub fn status(&self, router: &Arc<Router>) -> serde_json::Value {
        let lanes: serde_json::Value = router
            .snapshot()
            .iter()
            .map(|l| {
                let name = &l.cfg.name;
                let b = self.ledger.lanes.get(name);
                (
                    name.clone(),
                    serde_json::json!(
                        { "armed" : l.state.armed.load(Ordering::Relaxed), "halted" : l
                        .state.halted.load(Ordering::Relaxed), "halted_since" : l.state
                        .halted_since.load(Ordering::Relaxed), "ready" : l.state.ready
                        .load(Ordering::Relaxed), "retired" : l.state.retired
                        .load(Ordering::Relaxed), "risk" : b.map(| x | x.risk
                        .snapshot()), "spent_today" : b.map(| x | x.spent_today),
                        "open_usd" : self.ledger.open_usd(name), "cap_scale" : l.state
                        .cap_scale.load(Ordering::Relaxed) as f64 / MICRO, "holdings" :
                        self.ledger.holdings(name), "fired" : b.map(| x | x.fired),
                        "filled" : b.map(| x | x.filled), "rejected" : b.map(| x | x
                        .rejected), "boot_fault" : self.boot_faults.get(name), }
                    ),
                )
            })
            .collect::<serde_json::Map<_, _>>()
            .into();
        serde_json::json!(
            { "fires" : self.fires, "skips" : self.skips, "ambiguous" : self.ambiguous
            .len(), "trips" : self.trips, "reconciliations" : self.reconciliations.len(),
            "corrupt_ledger_lines" : self.ledger.corrupt_lines, "ledger_persistence" : {
            "ok" : self.ledger.persistence_ok(), "write_failures" : self.ledger
            .write_failures, "last_error" : self.ledger.last_write_error, }, "lanes" :
            lanes, }
        )
    }
    pub fn record_fire(&mut self, lane: &str) {
        self.fires += 1;
        if let Some(book) = self.ledger.lanes.get_mut(lane) {
            book.fired += 1;
        }
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn an_IN_FLIGHT_buy_still_counts_against_the_caps_after_the_tick() {
        let d = tmp("inflight_caps");
        let (mut c, router) = setup(&d, RiskConfig::default(), &[("a", true)]);
        let mut f = crate::pending::InFlight::default();
        f.by_lane.insert("a".into(), 300.0);
        f.by_token.insert(("a".into(), "TOK".into()), 300.0);
        c.tick_with(&router, &f);
        let lane = router.lane(0).unwrap();
        let open = lane.state.open_usd.load(Ordering::Relaxed) as f64 / MICRO;
        let spent = lane.state.spent_today.load(Ordering::Relaxed) as f64 / MICRO;
        assert!(
            (open - 300.0).abs() < 1e-6, "unresolved buys must count as open: {open}"
        );
        assert!((spent - 300.0).abs() < 1e-6, "and against today's budget: {spent}");
        let pt = *lane.per_token.lock().unwrap().get("TOK").unwrap();
        assert!(
            ((pt as f64 / MICRO) - 300.0).abs() < 1e-6, "and against the market cap"
        );
    }
    #[test]
    fn a_RESOLVED_order_stops_counting_so_phantoms_cannot_accumulate() {
        let d = tmp("inflight_gone");
        let (mut c, router) = setup(&d, RiskConfig::default(), &[("a", true)]);
        let mut f = crate::pending::InFlight::default();
        f.by_lane.insert("a".into(), 300.0);
        c.tick_with(&router, &f);
        assert!(router.lane(0).unwrap().state.open_usd.load(Ordering::Relaxed) > 0);
        c.tick_with(&router, &crate::pending::InFlight::default());
        assert_eq!(
            router.lane(0).unwrap().state.open_usd.load(Ordering::Relaxed), 0,
            "a resolved order must leave no phantom reservation"
        );
    }
    #[test]
    fn in_flight_SELLS_commit_no_capital_but_DO_commit_SHARES() {
        let mut log = crate::pending::PendingLog::open("");
        log.record(crate::pending::Pending {
                lane: "a".into(),
                token: "T".into(),
                side: 1,
                order_hash: "h".into(),
                shares: 100.0,
                limit: 0.9,
                ts: 1,
                why: "x".into(),
                resting: false,
            })
            .unwrap();
        let f = log.in_flight();
        assert!(
            f.by_lane.get("a").is_none(), "a sell must not add to committed capital"
        );
        assert_eq!(
            f.tokens.len(), 1, "but the token IS still ours while the order lives"
        );
        assert_eq!(
            f.sell_shares_by_token.get(& ("a".into(), "T".into())), Some(& 100.0),
            "and its shares are committed until the venue answers"
        );
    }
    #[test]
    fn a_RESOLVED_SELL_releases_its_SHARE_reservation_so_we_can_exit_again() {
        let d = tmp("sell_resv_release");
        let (mut c, router) = setup(&d, RiskConfig::default(), &[("a", true)]);
        let mut f = crate::pending::InFlight::default();
        f.sell_shares_by_token.insert(("a".into(), "TOK".into()), 60.0);
        c.tick_with(&router, &f);
        assert!(
            (router.lane(0).unwrap().sell_reserved("TOK") - 60.0).abs() < 1e-9,
            "an unresolved sell must hold its shares"
        );
        c.tick_with(&router, &crate::pending::InFlight::default());
        assert_eq!(
            router.lane(0).unwrap().sell_reserved("TOK"), 0.0,
            "a resolved sell must leave no phantom reservation"
        );
    }
    #[test]
    fn an_orphaned_claim_is_collected_but_an_IN_FLIGHT_one_is_not() {
        let d = tmp("orphan_claim");
        let (mut c, router) = setup(&d, RiskConfig::default(), &[("a", true)]);
        router.claim("ORPHAN", 0);
        router.claim("INFLIGHT", 0);
        router.backdate_claims_for_test(crate::lanes::CLAIM_GRACE_SECS + 5);
        assert_eq!(router.owner_of("ORPHAN"), Some(0));
        let mut f = crate::pending::InFlight::default();
        f.tokens.push(("a".to_string(), "INFLIGHT".to_string()));
        c.tick_with(&router, &f);
        assert_eq!(
            router.owner_of("ORPHAN"), None,
            "a claim with no holding and no live order must be collected"
        );
        assert_eq!(
            router.owner_of("INFLIGHT"), Some(0),
            "a claim with an order still in flight must be KEPT"
        );
    }
    #[test]
    fn a_FRESH_claim_is_never_collected_even_with_no_holding_and_no_inflight_row() {
        let d = tmp("fresh_claim");
        let (mut c, router) = setup(&d, RiskConfig::default(), &[("a", true)]);
        router.claim("JUST_CLAIMED", 0);
        c.tick_with(&router, &crate::pending::InFlight::default());
        assert_eq!(
            router.owner_of("JUST_CLAIMED"), Some(0),
            "a claim younger than the grace must survive collection"
        );
    }
    #[test]
    fn a_claim_we_actually_HOLD_is_never_collected() {
        let d = tmp("held_claim");
        let (mut c, router) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.ledger.record_fill("a", "HELD", 0, 10.0, 0.5, 0.0);
        router.claim("HELD", 0);
        c.tick_with(&router, &crate::pending::InFlight::default());
        assert_eq!(
            router.owner_of("HELD"), Some(0), "we own this token because we hold it"
        );
    }
    use super::*;
    #[test]
    fn observe_his_fill_returns_the_post_fill_book_so_the_lane_can_publish_it() {
        let mut c = Control::new(
            Ledger::new("", &[("a".into(), RiskConfig::default())]),
            "",
        );
        assert_eq!(c.observe_his_fill("a", "T", 0, 5_000.0), 5_000.0);
        assert_eq!(
            c.observe_his_fill("a", "T", 0, 20_000.0), 25_000.0,
            "the buy must be visible immediately, not one tick later"
        );
        let his_before = 25_000.0_f64;
        let frac = (5_000.0_f64 / his_before).min(1.0);
        assert!((frac - 0.2).abs() < 1e-9, "a 20% trim must read as 20%, got {frac}");
        assert!(frac < 0.95, "and must NOT promote to a full exit");
        assert_eq!(c.observe_his_fill("a", "T", 1, 5_000.0), 20_000.0);
    }
    #[test]
    fn a_sell_can_never_drive_his_book_negative() {
        let mut c = Control::new(
            Ledger::new("", &[("a".into(), RiskConfig::default())]),
            "",
        );
        c.observe_his_fill("a", "T", 0, 10.0);
        assert_eq!(c.observe_his_fill("a", "T", 1, 999.0), 0.0);
    }
    use crate::lanes::{Execution, Lane, LaneConfig, Sizing};
    use crate::risk::RiskConfig;
    fn tmp(name: &str) -> String {
        let p = std::env::temp_dir().join(format!("cb2_{name}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&p);
        p.to_string_lossy().into()
    }
    fn cfg(name: &str, w: u8) -> LaneConfig {
        LaneConfig {
            name: name.into(),
            wallet20: [w; 20],
            sizing: Sizing::Shares(5.0),
            execution: Execution::Taker,
            buy_slippage_c: 0.02,
            sell_slippage_c: 0.02,
            copy_maker_sells: false,
            sell_floor_frac: 0.5,
            min_order_usd: 1.0,
            max_usd_per_fill: 250.0,
            daily_budget_usd: 500.0,
            per_market_usd: 300.0,
            max_open_usd: 1000.0,
            max_buy_price: 0.95,
            min_buy_price: 0.02,
            min_fill_floor: true,
            sell_all_frac: 0.95,
            max_effective_pct: 0.05,
            compound: true,
            copy_makers: false,
            exclude_political: false,
        }
    }
    fn setup(
        dir: &str,
        risk: RiskConfig,
        armed: &[(&str, bool)],
    ) -> (Control, Arc<Router>) {
        let op = format!("{dir}/op.json");
        let lanes: serde_json::Map<String, serde_json::Value> = armed
            .iter()
            .map(|(n, a)| (n.to_string(), serde_json::json!({ "armed" : a })))
            .collect();
        std::fs::write(&op, serde_json::json!({ "lanes" : lanes }).to_string()).unwrap();
        let cfgs: Vec<(String, RiskConfig)> = armed
            .iter()
            .map(|(n, _)| (n.to_string(), risk))
            .collect();
        let led = Ledger::new(&format!("{dir}/led.jsonl"), &cfgs);
        let router = Arc::new(
            Router::new(
                armed
                    .iter()
                    .enumerate()
                    .map(|(i, (n, _))| Lane::new(cfg(n, i as u8 + 1)))
                    .collect(),
            ),
        );
        (Control::new(led, &op), router)
    }
    #[test]
    fn nothing_is_armed_without_operator_intent() {
        let d = tmp("noop");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", false)]);
        c.tick(&r);
        assert!(! r.lane(0).unwrap().state.armed.load(Ordering::Relaxed));
    }
    #[test]
    fn an_unreadable_operator_file_fails_CLOSED() {
        let d = tmp("bad");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        std::fs::write(&c.operator_path, "{{{ not json").unwrap();
        c.tick(&r);
        assert!(
            ! r.lane(0).unwrap().state.armed.load(Ordering::Relaxed),
            "unreadable intent must arm NOTHING"
        );
    }
    #[test]
    fn operator_arming_is_honoured() {
        let d = tmp("arm");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.tick(&r);
        assert!(r.lane(0).unwrap().state.armed.load(Ordering::Relaxed));
    }
    #[test]
    fn open_usd_is_TRUED_UP_from_the_ledger_not_left_to_accumulate() {
        let d = tmp("openusd");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.ledger.record_fill("a", "T1", 0, 100.0, 0.50, 0.0);
        r.lane(0)
            .unwrap()
            .state
            .open_usd
            .fetch_add((90.0 * MICRO) as i64, Ordering::Relaxed);
        assert!(
            r.lane(0).unwrap().state.open_usd.load(Ordering::Relaxed) > (50.0 * MICRO) as
            i64, "precondition: the atomic carries more than the ledger truly holds"
        );
        c.tick(&r);
        let after = r.lane(0).unwrap().state.open_usd.load(Ordering::Relaxed) as f64
            / MICRO;
        assert!(
            (after - 50.0).abs() < 1e-6,
            "tick must re-store open_usd from the ledger, got {after} not 50.0"
        );
    }
    #[test]
    fn open_usd_falls_when_a_position_is_SOLD() {
        let d = tmp("openusd2");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.ledger.record_fill("a", "T1", 0, 100.0, 0.50, 0.0);
        c.tick(&r);
        let held = r.lane(0).unwrap().state.open_usd.load(Ordering::Relaxed) as f64
            / MICRO;
        assert!((held - 50.0).abs() < 1e-6);
        c.ledger.record_fill("a", "T1", 1, 100.0, 0.55, 0.0);
        c.tick(&r);
        let flat = r.lane(0).unwrap().state.open_usd.load(Ordering::Relaxed) as f64
            / MICRO;
        assert!(
            flat.abs() < 1e-6, "a closed position must free its reservation, got {flat}"
        );
    }
    #[test]
    fn per_market_reservations_are_rebuilt_from_remaining_ledger_cost() {
        let d = tmp("permarket");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.ledger.record_fill("a", "T1", 0, 100.0, 0.50, 0.0);
        r.lane(0)
            .unwrap()
            .per_token
            .lock()
            .unwrap()
            .insert("T1".into(), (90.0 * MICRO) as i64);
        c.tick(&r);
        let reserved = *r.lane(0).unwrap().per_token.lock().unwrap().get("T1").unwrap();
        assert_eq!(
            reserved, (50.0 * MICRO) as i64,
            "a rejected order must not consume the market cap forever"
        );
        c.ledger.record_fill("a", "T1", 1, 100.0, 0.55, 0.0);
        c.tick(&r);
        assert!(
            ! r.lane(0).unwrap().per_token.lock().unwrap().contains_key("T1"),
            "a full exit must release the entire per-market reservation"
        );
    }
    #[test]
    fn a_breaker_overrides_operator_arming() {
        let d = tmp("brk");
        let (mut c, r) = setup(
            &d,
            RiskConfig {
                max_consecutive_losses: 2,
                ..Default::default()
            },
            &[("a", true)],
        );
        for i in 0..2 {
            let t = format!("T{i}");
            c.ledger.record_fill("a", &t, 0, 100.0, 0.50, 0.0);
            c.ledger.record_fill("a", &t, 1, 100.0, 0.40, 0.0);
        }
        c.tick(&r);
        assert!(
            r.lane(0).unwrap().state.armed.load(Ordering::Relaxed),
            "operator intent remains armed so exits stay available"
        );
        assert!(r.lane(0).unwrap().state.halted.load(Ordering::Relaxed));
    }
    #[test]
    fn the_control_plane_can_never_arm_a_lane_by_itself() {
        let d = tmp("never");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", false)]);
        for _ in 0..50 {
            c.ledger.record_fill("a", "T", 0, 10.0, 0.5, 0.0);
            c.ledger.record_fill("a", "T", 1, 10.0, 0.9, 0.0);
        }
        c.tick(&r);
        assert!(! r.lane(0).unwrap().state.armed.load(Ordering::Relaxed));
    }
    #[test]
    fn one_lane_halting_leaves_the_other_armed() {
        let d = tmp("iso");
        let (mut c, r) = setup(
            &d,
            RiskConfig {
                max_consecutive_losses: 2,
                ..Default::default()
            },
            &[("a", true), ("b", true)],
        );
        for i in 0..2 {
            let t = format!("T{i}");
            c.ledger.record_fill("a", &t, 0, 100.0, 0.50, 0.0);
            c.ledger.record_fill("a", &t, 1, 100.0, 0.40, 0.0);
        }
        c.tick(&r);
        assert!(r.lane(0).unwrap().state.armed.load(Ordering::Relaxed));
        assert!(
            r.lane(1).unwrap().state.armed.load(Ordering::Relaxed),
            "lanes must be isolated"
        );
        assert!(r.lane(0).unwrap().state.halted.load(Ordering::Relaxed));
        assert!(! r.lane(1).unwrap().state.halted.load(Ordering::Relaxed));
    }
    #[test]
    fn persistence_failure_halts_buys_but_preserves_exits() {
        let d = tmp("persist");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.ledger.last_write_error = Some("disk full".into());
        c.tick(&r);
        assert!(r.lane(0).unwrap().state.armed.load(Ordering::Relaxed));
        assert!(r.lane(0).unwrap().state.halted.load(Ordering::Relaxed));
    }
    #[test]
    fn halt_causes_names_each_condition_independently() {
        assert_eq!(
            halt_causes(& None, false, None), Vec::< String >::new(),
            "no condition true -> no causes"
        );
        assert_eq!(
            halt_causes(& Some("3 consecutive losses >= 3".into()), false, None),
            vec!["breaker: 3 consecutive losses >= 3"]
        );
        assert_eq!(halt_causes(& None, true, None), vec!["ledger persistence fault"]);
        let bf = "leader book seed failed".to_string();
        assert_eq!(
            halt_causes(& None, false, Some(& bf)),
            vec!["boot fault: leader book seed failed"]
        );
    }
    #[test]
    fn halt_causes_reports_ALL_conditions_that_are_simultaneously_true() {
        let reason = Some("drawdown $500.00 >= $500.00".to_string());
        let bf = "boot fault".to_string();
        let causes = halt_causes(&reason, true, Some(&bf));
        assert_eq!(
            causes.len(), 3, "all three simultaneous causes must be named: {causes:?}"
        );
        assert!(causes[0].contains("drawdown"));
        assert!(causes[1].contains("persistence"));
        assert!(causes[2].contains("boot fault"));
    }
    #[test]
    fn a_halt_latch_trip_is_logged_ONCE_not_every_tick_while_latched() {
        let d = tmp("latch_once");
        let (mut c, r) = setup(
            &d,
            RiskConfig {
                max_consecutive_losses: 1,
                ..Default::default()
            },
            &[("a", true)],
        );
        c.ledger.record_fill("a", "T", 0, 100.0, 0.50, 0.0);
        c.ledger.record_fill("a", "T", 1, 100.0, 0.40, 0.0);
        assert!(
            ! r.lane(0).unwrap().state.halt_latch.load(Ordering::Relaxed),
            "not latched yet"
        );
        c.tick(&r);
        assert!(
            r.lane(0).unwrap().state.halt_latch.load(Ordering::Relaxed), "now latched"
        );
        for _ in 0..10 {
            c.tick(&r);
            assert!(
                r.lane(0).unwrap().state.halt_latch.load(Ordering::Relaxed),
                "stays latched"
            );
        }
    }
    #[test]
    fn a_HOT_PATH_halt_LATCHES_across_control_ticks() {
        let d = tmp("latch");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.tick(&r);
        assert!(
            ! r.lane(0).unwrap().state.halted.load(Ordering::Relaxed), "clean to start"
        );
        r.lane(0).unwrap().state.halt_latch.store(true, Ordering::Relaxed);
        for _ in 0..5 {
            c.tick(&r);
            assert!(
                r.lane(0).unwrap().state.halted.load(Ordering::Relaxed),
                "a hot-path halt must SURVIVE the control tick"
            );
        }
        assert!(
            r.lane(0).unwrap().state.armed.load(Ordering::Relaxed),
            "halt stops BUYS only; disarming would freeze liquidation"
        );
    }
    #[test]
    fn an_OPERATOR_halt_stops_buys_but_KEEPS_the_lane_armed_and_is_reversible() {
        let d = tmp("ophalt");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        std::fs::write(
                &c.operator_path,
                r#"{"lanes":{"a":{"armed":true,"halt_buys":true}}}"#,
            )
            .unwrap();
        c.tick(&r);
        assert!(
            r.lane(0).unwrap().state.armed.load(Ordering::Relaxed),
            "halt_buys must NOT disarm — exits keep mirroring"
        );
        assert!(
            r.lane(0).unwrap().state.halted.load(Ordering::Relaxed), "buys are stopped"
        );
        assert!(
            ! r.lane(0).unwrap().state.halt_latch.load(Ordering::Relaxed),
            "an operator halt is not a breaker trip"
        );
        std::fs::write(&c.operator_path, r#"{"lanes":{"a":{"armed":true}}}"#).unwrap();
        c.tick(&r);
        assert!(
            ! r.lane(0).unwrap().state.halted.load(Ordering::Relaxed),
            "clearing the operator halt resumes buying"
        );
    }
    #[test]
    fn lifting_an_operator_halt_does_NOT_clear_a_tripped_breaker() {
        let d = tmp("ophalt2");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        r.lane(0).unwrap().state.halt_latch.store(true, Ordering::Relaxed);
        std::fs::write(
                &c.operator_path,
                r#"{"lanes":{"a":{"armed":true,"halt_buys":true}}}"#,
            )
            .unwrap();
        c.tick(&r);
        assert!(r.lane(0).unwrap().state.halted.load(Ordering::Relaxed));
        std::fs::write(&c.operator_path, r#"{"lanes":{"a":{"armed":true}}}"#).unwrap();
        c.tick(&r);
        assert!(
            r.lane(0).unwrap().state.halted.load(Ordering::Relaxed),
            "a latched breaker survives the operator lifting their own halt"
        );
    }
    #[test]
    fn a_BOOT_FAULT_refuses_to_arm_however_the_operator_file_reads() {
        let d = tmp("bootfault");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.tick(&r);
        assert!(
            r.lane(0).unwrap().state.armed.load(Ordering::Relaxed), "arms when clean"
        );
        c.set_boot_fault("a", "leader position snapshot unavailable");
        c.tick(&r);
        assert!(
            ! r.lane(0).unwrap().state.armed.load(Ordering::Relaxed),
            "a lane we cannot trust must never arm"
        );
        assert!(r.lane(0).unwrap().state.halted.load(Ordering::Relaxed));
    }
    #[test]
    fn fire_counters_are_global_and_per_lane() {
        let d = tmp("fires");
        let (mut c, _r) = setup(&d, RiskConfig::default(), &[("a", true), ("b", true)]);
        c.record_fire("b");
        c.record_fire("b");
        assert_eq!(c.fires, 2);
        assert_eq!(c.ledger.lanes["a"].fired, 0);
        assert_eq!(c.ledger.lanes["b"].fired, 2);
    }
    #[test]
    fn a_matched_buy_books_the_position() {
        let d = tmp("buy");
        let (mut c, _r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        let r = c
            .book_response(
                "a",
                "T1",
                0,
                0.62,
                r#"{"status":"matched","success":true,"takingAmount":"5.06","makingAmount":"3.14"}"#,
                None,
                "h_buy",
                false,
            );
        assert!(
            r.is_some(), "a booked fill must hand back the receipt that closes its row"
        );
        let p = &c.ledger.lanes["a"].positions["T1"];
        assert!((p.shares - 5.06).abs() < 1e-6);
        assert!((p.avg_cost() - 0.62).abs() < 0.01, "price derived from the two legs");
    }
    #[test]
    fn AN_UNWATCHED_BOT_HALTS_BUYS_but_stays_ARMED_so_exits_keep_mirroring() {
        let d = tmp("gwatch");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.tick(&r);
        assert!(
            ! r.lane(0).unwrap().state.halted.load(Ordering::Relaxed), "healthy first"
        );
        c.guardian_stale.store(true, Ordering::Relaxed);
        c.tick(&r);
        let lane = r.lane(0).unwrap();
        assert!(
            lane.state.halted.load(Ordering::Relaxed), "no guardian means no buying"
        );
        assert!(
            lane.state.armed.load(Ordering::Relaxed),
            "it must STILL BE ARMED so his exits keep being mirrored"
        );
    }
    #[test]
    fn THE_GATE_CLEARS_ITSELF_no_operator_and_no_unlatch() {
        let d = tmp("gwatch2");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.guardian_stale.store(true, Ordering::Relaxed);
        c.tick(&r);
        assert!(r.lane(0).unwrap().state.halted.load(Ordering::Relaxed));
        c.guardian_stale.store(false, Ordering::Relaxed);
        c.tick(&r);
        assert!(
            ! r.lane(0).unwrap().state.halted.load(Ordering::Relaxed),
            "buys resume on their own the moment the guardian writes again"
        );
        assert!(
            ! r.lane(0).unwrap().state.halt_latch.load(Ordering::Relaxed),
            "and it must NOT have set the sticky latch on the way through"
        );
    }
    #[test]
    fn the_gate_NEVER_clears_a_REAL_latched_halt() {
        let d = tmp("gwatch3");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        r.lane(0).unwrap().state.halt_latch.store(true, Ordering::Relaxed);
        c.guardian_stale.store(true, Ordering::Relaxed);
        c.tick(&r);
        c.guardian_stale.store(false, Ordering::Relaxed);
        c.tick(&r);
        assert!(
            r.lane(0).unwrap().state.halted.load(Ordering::Relaxed),
            "the real fault must still hold the lane"
        );
    }
    #[test]
    fn halted_since_STAMPS_and_CLEARS_across_a_guardian_outage() {
        let d = tmp("gwatch4");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.guardian_stale.store(true, Ordering::Relaxed);
        c.tick(&r);
        assert!(r.lane(0).unwrap().state.halted_since.load(Ordering::Relaxed) > 0);
        c.guardian_stale.store(false, Ordering::Relaxed);
        c.tick(&r);
        assert_eq!(r.lane(0).unwrap().state.halted_since.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn a_price_DERIVED_from_the_venue_is_not_marked_provisional() {
        let d = tmp("pxok");
        let (mut c, _r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.book_response(
            "a",
            "T1",
            0,
            0.62,
            r#"{"status":"matched","success":true,"takingAmount":"100","makingAmount":"50"}"#,
            None,
            "h_ok",
            false,
        );
        let raw = std::fs::read_to_string(format!("{d}/led.jsonl")).unwrap_or_default();
        let row: serde_json::Value = raw
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .find(|v: &serde_json::Value| v["ev"] == "fill")
            .expect("a fill");
        assert!(row.get("px_provisional").is_none(), "the venue gave us this price");
        assert!((row["price"].as_f64().unwrap() - 0.5).abs() < 1e-9, "50/100");
    }
    #[test]
    fn the_money_leg_is_read_under_EVERY_spelling_the_size_leg_accepts() {
        for key in [
            "makingAmount",
            "making_amount",
            "makerAmount",
            "maker_amount",
            "matched_amount",
            "matchedAmount",
            "value",
            "usd",
        ] {
            let d = tmp(&format!("alias_{key}"));
            let (mut c, _r) = setup(&d, RiskConfig::default(), &[("a", true)]);
            let body = format!(
                r#"{{"status":"matched","success":true,"takingAmount":"100","{key}":"50"}}"#
            );
            c.book_response("a", "T1", 0, 0.62, &body, None, &format!("h_{key}"), false);
            let raw = std::fs::read_to_string(format!("{d}/led.jsonl"))
                .unwrap_or_default();
            let row: serde_json::Value = raw
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .find(|v: &serde_json::Value| v["ev"] == "fill")
                .unwrap_or_else(|| panic!("no fill booked for {key}"));
            assert!(
                (row["price"].as_f64().unwrap() - 0.5).abs() < 1e-9,
                "{key}: booked {} not 0.5 (50/100) — the alias was not read",
                row["price"]
            );
            assert!(
                row.get("px_provisional").is_none(),
                "{key}: the venue DID give us the amount"
            );
        }
    }
    #[test]
    fn a_SELL_reads_its_money_leg_under_the_same_spellings() {
        for key in ["takingAmount", "taking_amount", "matched_amount", "value"] {
            let d = tmp(&format!("salias_{key}"));
            let (mut c, _r) = setup(&d, RiskConfig::default(), &[("a", true)]);
            c.ledger.book_fill_ex("a", "T1", 0, 200.0, 0.40, 0.0, None, "seed", false);
            let body = format!(
                r#"{{"status":"matched","success":true,"makingAmount":"100","{key}":"75"}}"#
            );
            c.book_response("a", "T1", 1, 0.10, &body, None, &format!("s_{key}"), false);
            let raw = std::fs::read_to_string(format!("{d}/led.jsonl"))
                .unwrap_or_default();
            let row = raw
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .filter(|v| v["ev"] == "fill" && v["side"] == 1)
                .last()
                .unwrap_or_else(|| panic!("no sell booked for {key}"));
            assert!(
                (row["price"].as_f64().unwrap() - 0.75).abs() < 1e-9,
                "{key}: booked {} not 0.75 (75/100)", row["price"]
            );
        }
    }
    #[test]
    fn a_price_FALLEN_BACK_to_our_limit_IS_marked_provisional() {
        let d = tmp("pxprov");
        let (mut c, _r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.book_response(
            "a",
            "T1",
            0,
            0.62,
            r#"{"status":"matched","success":true,"size_matched":"100"}"#,
            None,
            "h_prov",
            false,
        );
        let raw = std::fs::read_to_string(format!("{d}/led.jsonl")).unwrap_or_default();
        let row: serde_json::Value = raw
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .find(|v: &serde_json::Value| v["ev"] == "fill")
            .expect("a fill");
        assert_eq!(
            row["px_provisional"], serde_json::json!(true),
            "a price we invented must say so"
        );
        assert!((row["price"].as_f64().unwrap() - 0.62).abs() < 1e-9, "it is our limit");
    }
    #[test]
    fn a_provisional_fill_is_STILL_BOOKED_because_the_shares_are_real() {
        let d = tmp("pxbook");
        let (mut c, _r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        let r = c
            .book_response(
                "a",
                "T1",
                0,
                0.62,
                r#"{"status":"matched","success":true,"size_matched":"100"}"#,
                None,
                "h_book",
                false,
            );
        assert!(r.is_some(), "the shares are real and must be booked");
        assert!((c.ledger.lanes["a"].positions["T1"].shares - 100.0).abs() < 1e-6);
    }
    #[test]
    fn a_provisional_price_is_surfaced_as_AMBIGUOUS_for_an_operator() {
        let d = tmp("pxamb");
        let (mut c, _r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.book_response(
            "a",
            "T1",
            0,
            0.62,
            r#"{"status":"matched","success":true,"size_matched":"100"}"#,
            None,
            "h_amb",
            false,
        );
        assert!(
            c.ambiguous.iter().any(| x | x["why"].as_str().map(| s | s
            .contains("PROVISIONAL")).unwrap_or(false)),
            "a systematic accounting bias must reach a human"
        );
    }
    fn fee_of(extra: &str) -> f64 {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(
            0,
        );
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = tmp(&format!("fee{n}"));
        let (mut c, _r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        let raw = format!(
            r#"{{"status":"matched","success":true,"takingAmount":"100","makingAmount":"50"{extra}}}"#
        );
        c.book_response("a", "T1", 0, 0.50, &raw, None, "h_fee", false);
        let raw_log = std::fs::read_to_string(format!("{d}/led.jsonl"))
            .unwrap_or_default();
        raw_log
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["ev"] == "fill")
            .last()
            .and_then(|v| v["fee"].as_f64())
            .unwrap_or(f64::NAN)
    }
    #[test]
    fn an_ABSOLUTE_fee_over_a_dollar_is_not_mistaken_for_a_RATE() {
        assert!(
            (fee_of(r#","fee":"1.50""#) - 1.50).abs() < 1e-9,
            "a $1.50 fee is $1.50, whatever its magnitude happens to be"
        );
    }
    #[test]
    fn a_SUB_BASIS_POINT_rate_is_not_mistaken_for_an_absolute_fee() {
        assert!((fee_of(r#","feeRateBps":"0.5""#) - 0.0025).abs() < 1e-9);
    }
    #[test]
    fn a_rate_in_BASIS_POINTS_is_converted_against_this_fill_s_own_notional() {
        assert!((fee_of(r#","feeRateBps":"50""#) - 0.25).abs() < 1e-9);
        assert!((fee_of(r#","fee_rate_bps":"50""#) - 0.25).abs() < 1e-9);
    }
    #[test]
    fn every_ABSOLUTE_spelling_the_venue_might_use_books_the_same_dollars() {
        for k in ["fee", "feeAmount", "fee_amount", "makerFee", "takerFee"] {
            assert!(
                (fee_of(& format!(r#","{k}":"2.25""#)) - 2.25).abs() < 1e-9,
                "{k} is an absolute amount and must book as one"
            );
        }
    }
    #[test]
    fn an_ABSENT_fee_is_ZERO_and_that_is_still_the_live_case() {
        assert_eq!(fee_of(""), 0.0);
    }
    #[test]
    fn a_NONSENSE_fee_never_corrupts_the_book() {
        assert_eq!(fee_of(r#","fee":"-5.00""#), 0.0, "a negative fee is refused");
        assert_eq!(fee_of(r#","fee":"not-a-number""#), 0.0);
        assert_eq!(fee_of(r#","fee":null"#), 0.0);
    }
    #[test]
    fn a_matched_SELL_reads_the_MAKER_leg_not_the_taker_leg() {
        let d = tmp("sell");
        let (mut c, _r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.ledger.record_fill("a", "T1", 0, 100.0, 0.50, 0.0);
        let _ = c
            .book_response(
                "a",
                "T1",
                1,
                0.60,
                r#"{"status":"matched","success":true,"makingAmount":"100.0","takingAmount":"60.0"}"#,
                None,
                "h_sell",
                false,
            );
        assert!(
            c.ledger.lanes["a"].positions["T1"].shares < 1e-9,
            "the full 100-share exit must book, not a 60-share partial"
        );
    }
    #[test]
    fn matched_but_unreadable_is_AMBIGUOUS_never_no_fill() {
        let d = tmp("amb");
        let (mut c, _r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.ledger.record_fill("a", "T1", 0, 10.0, 0.50, 0.0);
        let r = c
            .book_response(
                "a",
                "T1",
                1,
                0.60,
                r#"{"status":"matched","success":true,"transactionsHashes":["0xabc"]}"#,
                None,
                "h_amb",
                false,
            );
        assert!(r.is_none(), "an ambiguous reply must never yield a receipt");
        assert_eq!(c.ambiguous.len(), 1, "must be flagged, not silently dropped");
        assert_eq!(c.ledger.lanes["a"].rejected, 0, "a match is NOT a reject");
        assert_eq!(c.ledger.lanes["a"].positions["T1"].shares, 10.0, "nothing assumed");
    }
    #[test]
    fn an_error_body_carrying_a_size_is_not_a_fill() {
        let d = tmp("err");
        let (mut c, _r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        let r = c
            .book_response(
                "a",
                "T1",
                0,
                0.62,
                r#"{"errorMsg":"not enough balance","takingAmount":"5.0"}"#,
                None,
                "h_err",
                false,
            );
        assert!(r.is_none(), "a refusal books nothing and must not yield a receipt");
        assert_eq!(c.ledger.lanes["a"].rejected, 1);
        assert!(c.ledger.lanes["a"].positions.is_empty());
    }
    #[test]
    fn a_sell_can_never_book_more_than_we_hold() {
        let d = tmp("over");
        let (mut c, _r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.ledger.record_fill("a", "T1", 0, 5.0, 0.50, 0.0);
        let _ = c
            .book_response(
                "a",
                "T1",
                1,
                0.60,
                r#"{"status":"matched","success":true,"makingAmount":"5000.0"}"#,
                None,
                "h_over",
                false,
            );
        assert!(c.ledger.lanes["a"].positions["T1"].shares >= - 1e-9);
    }
    #[test]
    fn booked_fills_reach_the_hot_path_as_holdings() {
        let d = tmp("hold");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        let _ = c
            .book_response(
                "a",
                "T1",
                0,
                0.62,
                r#"{"status":"matched","success":true,"takingAmount":"5.0","makingAmount":"3.1"}"#,
                None,
                "h_hold",
                false,
            );
        c.tick(&r);
        assert!(
            (r.lane(0).unwrap().holding("T1") - 5.0).abs() < 1e-9,
            "without this the hot path could never size an exit"
        );
    }
    #[test]
    fn a_token_we_HOLD_is_never_re_marked_legacy_on_restart() {
        let d = tmp("leg");
        let (mut c, _r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        c.ledger.record_fill("a", "T1", 0, 10.0, 0.50, 0.0);
        let mut his = HashMap::new();
        his.insert("T1".to_string(), 1000.0);
        his.insert("T2".to_string(), 500.0);
        c.seed_his_book("a", &his);
        assert!(
            ! c.legacy["a"].contains_key("T1"), "re-marking T1 would suppress our exit"
        );
        assert!(c.legacy["a"].contains_key("T2"));
        assert_eq!(c.his_pos["a"] ["T1"], 1000.0, "his book must still be tracked");
    }
    #[test]
    fn his_legacy_tranche_drains_as_he_sells_it() {
        let d = tmp("drain");
        let (mut c, _r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        let mut his = HashMap::new();
        his.insert("T1".to_string(), 1000.0);
        c.seed_his_book("a", &his);
        c.observe_his_fill("a", "T1", 1, 400.0);
        assert_eq!(c.legacy["a"] ["T1"], 600.0);
    }
    #[test]
    fn a_flat_token_releases_its_collision_guard_claim() {
        let d = tmp("rel");
        let (mut c, r) = setup(&d, RiskConfig::default(), &[("a", true)]);
        let _ = c
            .book_response(
                "a",
                "T1",
                0,
                0.62,
                r#"{"status":"matched","success":true,"takingAmount":"5.0","makingAmount":"3.1"}"#,
                None,
                "h_rel_buy",
                false,
            );
        c.tick(&r);
        assert!((r.lane(0).unwrap().holding("T1") - 5.0).abs() < 1e-9);
        let _ = c
            .book_response(
                "a",
                "T1",
                1,
                0.60,
                r#"{"status":"matched","success":true,"makingAmount":"5.0","takingAmount":"3.0"}"#,
                None,
                "h_rel_sell",
                false,
            );
        c.tick(&r);
        assert_eq!(r.lane(0).unwrap().holding("T1"), 0.0);
        assert_eq!(r.owner_of("T1"), None, "another lane must be able to take it now");
    }
}
