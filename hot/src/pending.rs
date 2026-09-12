use std::collections::HashMap;
use std::io::Write;
#[derive(Debug, Clone, Default)]
pub struct InFlight {
    pub by_lane: HashMap<String, f64>,
    pub by_token: HashMap<(String, String), f64>,
    pub tokens: Vec<(String, String)>,
    pub sell_shares_by_token: HashMap<(String, String), f64>,
    pub buy_shares_by_token: HashMap<(String, String), f64>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub lane: String,
    pub token: String,
    pub side: u8,
    pub order_hash: String,
    pub shares: f64,
    pub limit: f64,
    pub ts: i64,
    pub why: String,
    pub resting: bool,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Filled { shares: f64, price: f64 },
    NotFilled,
    Unknown,
}
pub const STALE_SECS: i64 = 45;
pub const STALE_RESTING_SECS: i64 = 1_800 + 180;
pub fn stale_after(p: &Pending) -> i64 {
    stale_after_kind(p, false)
}
pub fn stale_after_kind(p: &Pending, corroborated: bool) -> i64 {
    match (p.resting, corroborated) {
        (true, true) => crate::restwatch::MAX_REST_AGE_SECS,
        (true, false) => STALE_RESTING_SECS,
        (false, _) => STALE_SECS,
    }
}
pub const SETTLE_SECS: i64 = 5;
fn num(v: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|k| {
            v[*k].as_f64().or_else(|| v[*k].as_str().and_then(|s| s.parse::<f64>().ok()))
        })
}
pub fn parse_order_status(body: &str, side: u8) -> Verdict {
    parse_order_status_kind(body, side, false)
}
pub fn parse_order_status_kind(body: &str, side: u8, resting: bool) -> Verdict {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Verdict::Unknown
    };
    if !v.is_object() {
        return Verdict::Unknown;
    }
    if v["error"].is_string()
        || v["errorMsg"].as_str().filter(|s| !s.is_empty()).is_some()
    {
        return Verdict::Unknown;
    }
    let status = v["status"].as_str().unwrap_or("").to_ascii_lowercase();
    let share_key = if side == 0 { "takingAmount" } else { "makingAmount" };
    let other_key = if side == 0 { "makingAmount" } else { "takingAmount" };
    let matched = num(&v, &["size_matched", "sizeMatched", "filled_size", "filledSize"])
        .or_else(|| {
            if status == "matched" || status == "filled" {
                num(&v, &[share_key])
            } else {
                None
            }
        })
        .unwrap_or(0.0);
    if resting && matched > 0.0 && (status == "live" || status == "delayed") {
        return Verdict::Unknown;
    }
    if matched > 0.0 {
        let full_fill = (status == "matched" || status == "filled")
            && num(&v, &[share_key])
                .map(|s| (s - matched).abs() <= s.abs() * 1e-6 + 1e-9)
                .unwrap_or(false);
        let price = if full_fill {
            num(&v, &[other_key])
                .filter(|usd| *usd > 0.0)
                .map(|usd| usd / matched)
                .unwrap_or(0.0)
        } else {
            0.0
        };
        return Verdict::Filled {
            shares: matched,
            price,
        };
    }
    match status.as_str() {
        "cancelled" | "canceled" | "unmatched" | "expired" | "rejected" => {
            Verdict::NotFilled
        }
        "live" | "delayed" if !resting => Verdict::NotFilled,
        "live" | "delayed" => Verdict::Unknown,
        _ => Verdict::Unknown,
    }
}
pub fn parse_trades(body: &str, order_hash: &str, side: u8, resting: bool) -> Verdict {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Verdict::Unknown
    };
    let (rows, complete): (&Vec<serde_json::Value>, bool) = match &v {
        serde_json::Value::Array(a) => (a, true),
        serde_json::Value::Object(o) => {
            let Some(a) = o
                .get("data")
                .or_else(|| o.get("history"))
                .or_else(|| o.get("trades"))
                .and_then(|x| x.as_array()) else { return Verdict::Unknown };
            let more = match o.get("next_cursor") {
                None => false,
                Some(serde_json::Value::Null) => false,
                Some(serde_json::Value::String(c)) => {
                    !(c.is_empty() || c == "LTE=" || c == "END")
                }
                Some(_) => true,
            };
            (a, !more)
        }
        _ => return Verdict::Unknown,
    };
    let want = order_hash.trim_start_matches("0x").to_ascii_lowercase();
    if want.is_empty() {
        return Verdict::Unknown;
    }
    let mut shares = 0.0;
    let mut usd = 0.0;
    let mut hit = false;
    for row in rows {
        let ident = |v: &serde_json::Value, keys: &[&str]| -> bool {
            keys.iter()
                .any(|k| {
                    v[*k]
                        .as_str()
                        .map(|x| x.trim_start_matches("0x").eq_ignore_ascii_case(&want))
                        .unwrap_or(false)
                })
        };
        let we_are_taker = ident(
            row,
            &[
                "taker_order_id",
                "takerOrderId",
                "order_id",
                "orderId",
                "id",
                "order_hash",
            ],
        );
        let maker_entry = row["maker_orders"]
            .as_array()
            .or_else(|| row["makerOrders"].as_array())
            .and_then(|ms| {
                ms
                    .iter()
                    .find(|m| ident(m, &["order_id", "orderId", "id", "order_hash"]))
            });
        if !we_are_taker && maker_entry.is_none() {
            continue;
        }
        let status = row["status"].as_str().unwrap_or("").to_ascii_uppercase();
        if status == "FAILED" || status == "ORDER_STATUS_FAILED" {
            continue;
        }
        hit = true;
        let src = maker_entry.filter(|_| !we_are_taker);
        let sz = src
            .and_then(|m| num(m, &["size", "matched_amount", "matchedAmount"]))
            .or_else(|| num(row, &["size", "matched_amount", "matchedAmount"]))
            .unwrap_or(0.0);
        let px = src
            .and_then(|m| num(m, &["price"]))
            .or_else(|| num(row, &["price"]))
            .unwrap_or(0.0);
        if sz > 0.0 {
            shares += sz;
            usd += sz * px;
        }
    }
    if !hit {
        return if complete && !resting { Verdict::NotFilled } else { Verdict::Unknown };
    }
    if shares <= 0.0 {
        return Verdict::Unknown;
    }
    let _ = side;
    Verdict::Filled {
        shares,
        price: if usd > 0.0 { usd / shares } else { 0.0 },
    }
}
pub fn knows_shares_but_not_price(v: &Verdict) -> bool {
    matches!(v, Verdict::Filled { shares, price } if * shares > 0.0 && * price <= 0.0)
}
pub fn merge_price_from_trades(status: Verdict, trades: Verdict) -> Verdict {
    match (status, trades) {
        (
            Verdict::Filled { shares, price },
            Verdict::Filled { price: tp, .. },
        ) if price <= 0.0 && tp > 0.0 => {
            Verdict::Filled {
                shares,
                price: tp,
            }
        }
        (s, _) => s,
    }
}
pub fn recovered_price(resting: bool, venue_price: f64, limit: f64) -> (f64, bool) {
    if venue_price > 0.0 {
        (venue_price, false)
    } else if resting {
        (limit, false)
    } else {
        (limit, true)
    }
}
pub struct PendingLog {
    pub last_write_error: Option<String>,
    pub write_failures: u64,
    pub unreadable_sources: usize,
    path: String,
    open: HashMap<String, Pending>,
}
impl PendingLog {
    pub fn open(path: &str) -> Self {
        Self::open_with_wal(path, None).0
    }
    pub fn open_with_wal(path: &str, wal: Option<&str>) -> (Self, usize) {
        let mut open: HashMap<String, Pending> = HashMap::new();
        let mut from_wal = 0usize;
        let mut unreadable = 0usize;
        let mut resolved: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (src, is_wal) in [(Some(path), false), (wal, true)] {
            let Some(src) = src else { continue };
            if src.is_empty() {
                continue;
            }
            let raw = match std::fs::read_to_string(src) {
                Ok(r) => r,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    eprintln!(
                        "[pending] ⛔ CANNOT READ {src}: {e} — this is NOT an empty \
                               journal. Outstanding orders are UNKNOWN."
                    );
                    unreadable += 1;
                    continue;
                }
            };
            for line in raw.lines() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue
                };
                let Some(hash) = v["order_hash"].as_str() else { continue };
                match v["ev"].as_str().unwrap_or("") {
                    "pending" => {
                        if is_wal {
                            from_wal += 1;
                        }
                        if resolved.contains(hash) {
                            continue;
                        }
                        open.insert(
                            hash.to_string(),
                            Pending {
                                lane: v["lane"].as_str().unwrap_or("").to_string(),
                                token: v["token"].as_str().unwrap_or("").to_string(),
                                side: v["side"].as_u64().unwrap_or(0) as u8,
                                order_hash: hash.to_string(),
                                shares: v["shares"].as_f64().unwrap_or(0.0),
                                limit: v["limit"].as_f64().unwrap_or(0.0),
                                ts: v["t"].as_i64().unwrap_or(0),
                                why: v["why"].as_str().unwrap_or("").to_string(),
                                resting: v["resting"].as_bool().unwrap_or(false),
                            },
                        );
                    }
                    "resolved" => {
                        resolved.insert(hash.to_string());
                        open.remove(hash);
                    }
                    _ => {}
                }
            }
        }
        let me = Self {
            path: path.to_string(),
            open,
            last_write_error: None,
            write_failures: 0,
            unreadable_sources: unreadable,
        };
        (me, from_wal)
    }
    pub fn absorb_wal(&self, rows: usize) -> Result<usize, String> {
        if rows == 0 {
            return Ok(0);
        }
        self.compact().map(|_| rows)
    }
    pub fn compact(&self) -> Result<(), String> {
        if self.path.is_empty() {
            return Ok(());
        }
        let tmp = format!("{}.compact.tmp", self.path);
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| format!("open {tmp}: {e}"))?;
        for p in self.open.values() {
            let row = serde_json::json!(
                { "ev" : "pending", "lane" : p.lane, "token" : p.token, "side" : p.side,
                "order_hash" : p.order_hash, "shares" : p.shares, "limit" : p.limit,
                "why" : p.why, "t" : p.ts, "resting" : p.resting }
            );
            writeln!(out, "{row}").map_err(|e| format!("write {tmp}: {e}"))?;
        }
        out.flush()
            .and_then(|_| out.sync_data())
            .map_err(|e| format!("sync {tmp}: {e}"))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| format!("replace {}: {e}", self.path))?;
        if let Some(dir) = std::path::Path::new(&self.path).parent() {
            let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
        }
        Ok(())
    }
    pub fn row_bytes(p: &Pending) -> Vec<u8> {
        let row = serde_json::json!(
            { "ev" : "pending", "lane" : p.lane, "token" : p.token, "side" : p.side,
            "order_hash" : p.order_hash, "shares" : p.shares, "limit" : p.limit, "why" :
            p.why, "t" : p.ts, "resting" : p.resting }
        );
        let mut b = row.to_string().into_bytes();
        b.push(b'\n');
        b
    }
    pub fn apply_record(&mut self, p: Pending) {
        self.open.insert(p.order_hash.clone(), p);
    }
    fn append(&self, row: &serde_json::Value) -> bool {
        if self.path.is_empty() {
            return true;
        }
        let done = (|| -> std::io::Result<()> {
            if let Some(dir) = std::path::Path::new(&self.path).parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            writeln!(f, "{row}")?;
            f.flush()?;
            f.sync_data()
        })();
        if let Err(e) = done {
            eprintln!("[pending] CRITICAL: cannot persist {}: {e}", self.path);
            return false;
        }
        true
    }
    pub fn record(&mut self, p: Pending) -> Result<(), String> {
        let row = serde_json::json!(
            { "ev" : "pending", "lane" : p.lane, "token" : p.token, "side" : p.side,
            "order_hash" : p.order_hash, "shares" : p.shares, "limit" : p.limit, "why" :
            p.why, "t" : p.ts, "resting" : p.resting }
        );
        if !self.append(&row) {
            let e = format!("cannot persist pending order {}", p.order_hash);
            self.write_failures += 1;
            self.last_write_error = Some(e.clone());
            return Err(e);
        }
        self.open.insert(p.order_hash.clone(), p);
        Ok(())
    }
    pub fn resolve(
        &mut self,
        order_hash: &str,
        verdict: &str,
        booked: Option<(f64, f64)>,
    ) {
        let row = serde_json::json!(
            { "ev" : "resolved", "order_hash" : order_hash, "verdict" : verdict,
            "booked_shares" : booked.map(| b | b.0), "booked_price" : booked.map(| b | b
            .1), "t" : crate ::ledger::now_secs() }
        );
        if self.append(&row) {
            self.open.remove(order_hash);
        }
    }
    pub fn resolve_booked(
        &mut self,
        order_hash: &str,
        verdict: &str,
        receipt: crate::ledger::Booked,
    ) -> Result<(), String> {
        let row = serde_json::json!(
            { "ev" : "resolved", "order_hash" : order_hash, "verdict" : verdict,
            "booked_shares" : receipt.shares, "booked_price" : receipt.price, "t" : crate
            ::ledger::now_secs() }
        );
        if !self.append(&row) {
            let e = format!("cannot persist resolution of {order_hash}");
            self.write_failures += 1;
            self.last_write_error = Some(e.clone());
            return Err(e);
        }
        self.open.remove(order_hash);
        Ok(())
    }
    pub fn due(&self, now: i64) -> Vec<Pending> {
        let mut out: Vec<Pending> = self
            .open
            .values()
            .filter(|p| now - p.ts >= SETTLE_SECS)
            .cloned()
            .collect();
        out.sort_by_key(|p| p.ts);
        out
    }
    pub fn stale(&self, now: i64) -> Vec<Pending> {
        self.stale_excluding(now, &std::collections::HashSet::new())
    }
    pub fn stale_excluding(
        &self,
        now: i64,
        live_rests: &std::collections::HashSet<String>,
    ) -> Vec<Pending> {
        self.open
            .values()
            .filter(|p| {
                let bare = p.order_hash.trim_start_matches("0x");
                let corroborated = live_rests.contains(bare)
                    || live_rests.contains(&p.order_hash);
                now - p.ts > stale_after_kind(p, corroborated)
            })
            .cloned()
            .collect()
    }
    pub fn persistence_ok(&self) -> bool {
        self.last_write_error.is_none()
    }
    pub fn len(&self) -> usize {
        self.open.len()
    }
    pub fn is_open(&self, order_hash: &str) -> bool {
        self.open.contains_key(order_hash)
    }
    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }
    pub fn in_flight(&self) -> InFlight {
        let mut by_lane: HashMap<String, f64> = HashMap::new();
        let mut by_token: HashMap<(String, String), f64> = HashMap::new();
        let mut sell_shares_by_token: HashMap<(String, String), f64> = HashMap::new();
        let mut buy_shares_by_token: HashMap<(String, String), f64> = HashMap::new();
        let mut tokens = Vec::new();
        for p in self.open.values() {
            tokens.push((p.lane.clone(), p.token.clone()));
            if p.side != 0 {
                if p.shares.is_finite() && p.shares > 0.0 {
                    *sell_shares_by_token
                        .entry((p.lane.clone(), p.token.clone()))
                        .or_default() += p.shares;
                }
                continue;
            }
            if p.shares.is_finite() && p.shares > 0.0 {
                *buy_shares_by_token
                    .entry((p.lane.clone(), p.token.clone()))
                    .or_default() += p.shares;
            }
            let usd = p.shares * p.limit;
            if !usd.is_finite() || usd <= 0.0 {
                continue;
            }
            *by_lane.entry(p.lane.clone()).or_default() += usd;
            *by_token.entry((p.lane.clone(), p.token.clone())).or_default() += usd;
        }
        InFlight {
            by_lane,
            by_token,
            tokens,
            sell_shares_by_token,
            buy_shares_by_token,
        }
    }
    pub fn open_tokens(&self) -> Vec<(String, String)> {
        self.open.values().map(|p| (p.lane.clone(), p.token.clone())).collect()
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn a_RESTING_order_absent_from_the_trades_list_has_not_filled_YET() {
        assert_eq!(
            parse_trades("[]", "0xdeadbeef", 0, true), Verdict::Unknown,
            "a live GTC absent from TRADES has not filled YET — keep the row open"
        );
    }
    #[test]
    fn a_NON_resting_order_absent_from_a_complete_list_is_still_NotFilled() {
        assert_eq!(parse_trades("[]", "0xdeadbeef", 0, false), Verdict::NotFilled);
    }
    #[test]
    fn an_INCOMPLETE_list_is_Unknown_for_BOTH_kinds() {
        let paged = r#"{"data":[],"next_cursor":"MTAw"}"#;
        for resting in [true, false] {
            assert_eq!(
                parse_trades(paged, "0xdeadbeef", 0, resting), Verdict::Unknown,
                "an unread list can never say NotFilled (resting={resting})"
            );
        }
    }
    #[test]
    fn a_MAKER_fill_at_its_own_limit_is_EXACT_not_provisional() {
        assert_eq!(
            recovered_price(true, 0.0, 0.50), (0.50, false),
            "a maker fills AT its resting price — the limit IS the fill price"
        );
    }
    #[test]
    fn a_TAKER_fill_at_the_limit_stays_provisional() {
        assert_eq!(recovered_price(false, 0.0, 0.64), (0.64, true));
    }
    #[test]
    fn a_real_venue_price_wins_for_BOTH_kinds() {
        assert_eq!(recovered_price(true, 0.48, 0.50), (0.48, false));
        assert_eq!(recovered_price(false, 0.48, 0.64), (0.48, false));
    }
    #[test]
    fn a_priceless_verdict_is_the_COMMON_case_not_an_edge_one() {
        for body in [
            r#"{"status":"matched","size_matched":"610","price":"0.25","side":"BUY"}"#,
            r#"{"status":"matched","size_matched":"7.5","price":"0.6","side":"BUY"}"#,
            r#"{"status":"matched","sizeMatched":"200","price":"0.4","side":"SELL"}"#,
        ] {
            let v = parse_order_status(body, 0);
            assert!(
                knows_shares_but_not_price(& v),
                "order-status bodies carry shares and no money leg: {body}"
            );
        }
    }
    #[test]
    fn a_REAL_order_status_body_knows_the_SHARES_and_NOT_the_price() {
        let body = r#"{"id":"0xabc","status":"matched","market":"0xcond",
                       "original_size":"610","size_matched":"610","price":"0.25",
                       "side":"BUY","asset_id":"tok"}"#;
        let v = parse_order_status(body, 0);
        match v {
            Verdict::Filled { shares, price } => {
                assert!((shares - 610.0).abs() < 1e-9, "the share count IS readable");
                assert_eq!(price, 0.0, "and the price is NOT — no money leg exists");
            }
            other => panic!("expected Filled, got {other:?}"),
        }
        assert!(
            knows_shares_but_not_price(& v),
            "this is the state that MUST send us to the trades record"
        );
    }
    #[test]
    fn the_resolver_takes_SHARES_from_status_and_PRICE_from_trades() {
        let status = Verdict::Filled {
            shares: 610.0,
            price: 0.0,
        };
        let trades = Verdict::Filled {
            shares: 500.0,
            price: 0.1283,
        };
        match merge_price_from_trades(status, trades) {
            Verdict::Filled { shares, price } => {
                assert!(
                    (shares - 610.0).abs() < 1e-9,
                    "⛔ the SHARE count must stay from the status — a partial trades page would silently shrink a fill we already sized correctly"
                );
                assert!(
                    (price - 0.1283).abs() < 1e-9, "the PRICE must come from trades"
                );
            }
            other => panic!("expected Filled, got {other:?}"),
        }
    }
    #[test]
    fn a_status_that_ALREADY_knows_its_price_is_left_alone() {
        let status = Verdict::Filled {
            shares: 10.0,
            price: 0.42,
        };
        let trades = Verdict::Filled {
            shares: 10.0,
            price: 0.99,
        };
        match merge_price_from_trades(status.clone(), trades) {
            Verdict::Filled { price, .. } => {
                assert!(
                    (price - 0.42).abs() < 1e-9,
                    "a readable venue price is authoritative; trades must not override it"
                )
            }
            other => panic!("expected Filled, got {other:?}"),
        }
        assert!(! knows_shares_but_not_price(& status));
    }
    #[test]
    fn a_PRICELESS_trades_answer_never_makes_things_worse() {
        for t in [
            Verdict::Filled {
                shares: 10.0,
                price: 0.0,
            },
            Verdict::Unknown,
            Verdict::NotFilled,
        ] {
            let status = Verdict::Filled {
                shares: 10.0,
                price: 0.0,
            };
            match merge_price_from_trades(status, t) {
                Verdict::Filled { shares, price } => {
                    assert!((shares - 10.0).abs() < 1e-9);
                    assert_eq!(price, 0.0, "still priceless — the caller marks it");
                }
                other => panic!("must stay Filled, got {other:?}"),
            }
        }
    }
    #[test]
    fn a_NON_fill_verdict_is_never_promoted_by_a_trades_answer() {
        for s in [Verdict::Unknown, Verdict::NotFilled] {
            let out = merge_price_from_trades(
                s.clone(),
                Verdict::Filled {
                    shares: 5.0,
                    price: 0.4,
                },
            );
            assert_eq!(out, s, "a non-fill status must survive untouched");
        }
    }
    #[test]
    fn an_ABSENT_journal_is_legitimately_empty() {
        let d = std::env::temp_dir().join(format!("cb_p0r_a_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let p = d.join("nope.jsonl");
        let _ = std::fs::remove_file(&p);
        let (log, _) = PendingLog::open_with_wal(p.to_str().unwrap(), None);
        assert_eq!(log.unreadable_sources, 0, "absent is not a fault");
    }
    #[test]
    fn an_UNREADABLE_journal_is_reported_rather_than_read_as_empty() {
        let d = std::env::temp_dir().join(format!("cb_p0r_b_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let blocker = d.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let p = blocker.join("pending.jsonl");
        let (log, _) = PendingLog::open_with_wal(p.to_str().unwrap(), None);
        assert!(
            log.unreadable_sources > 0,
            "an unreadable journal must be reported, not treated as empty"
        );
        assert!(log.open_tokens().is_empty(), "and it genuinely knows nothing");
    }
    #[test]
    fn a_FAILED_trade_NEVER_becomes_inventory() {
        let body = r#"[{"status":"FAILED","size":"100","price":"0.5",
                        "taker_order_id":"0xdeadbeef"}]"#;
        assert_eq!(
            parse_trades(body, "0xdeadbeef", 0, false), Verdict::NotFilled,
            "a failed trade is not a fill"
        );
    }
    #[test]
    fn OUR_MAKER_QUANTITY_is_taken_from_OUR_entry_not_the_takers() {
        let body = r#"[{"status":"CONFIRMED","size":"100","price":"0.5",
                        "taker_order_id":"0xsomeoneelse",
                        "maker_orders":[{"order_id":"0xdeadbeef","size":"7","price":"0.49"}]}]"#;
        match parse_trades(body, "0xdeadbeef", 0, false) {
            Verdict::Filled { shares, price } => {
                assert!(
                    (shares - 7.0).abs() < 1e-9,
                    "our quantity, not the taker's: {shares}"
                );
                assert!((price - 0.49).abs() < 1e-9, "and our price: {price}");
            }
            other => panic!("we ARE in this trade as a maker — got {other:?}"),
        }
    }
    #[test]
    fn a_hash_that_is_NOT_ours_is_not_our_fill() {
        let body = r#"[{"status":"CONFIRMED","size":"100","price":"0.5",
                        "taker_order_id":"0xsomeoneelse"}]"#;
        assert_eq!(parse_trades(body, "0xdeadbeef", 0, false), Verdict::NotFilled);
    }
    #[test]
    fn an_ordinary_TAKER_fill_still_books_the_top_level_size() {
        let body = r#"[{"status":"CONFIRMED","size":"100","price":"0.5",
                        "taker_order_id":"0xdeadbeef"}]"#;
        match parse_trades(body, "0xdeadbeef", 0, false) {
            Verdict::Filled { shares, price } => {
                assert!((shares - 100.0).abs() < 1e-9);
                assert!((price - 0.5).abs() < 1e-9);
            }
            other => panic!("an ordinary taker fill must still book — got {other:?}"),
        }
    }
    #[test]
    fn a_NON_TERMINAL_status_is_still_booked_and_that_is_DELIBERATE() {
        let body = r#"[{"status":"MATCHED","size":"10","price":"0.5",
                        "taker_order_id":"0xdeadbeef"}]"#;
        assert!(
            matches!(parse_trades(body, "0xdeadbeef", 0, false), Verdict::Filled { .. }),
            "deliberate: unproven statuses still book until live capture says otherwise"
        );
    }
    use super::*;
    #[test]
    fn a_LIVE_gtc_with_a_partial_fill_stays_UNKNOWN() {
        let body = r#"{"status":"live","size_matched":"30","takingAmount":"30","makingAmount":"15"}"#;
        assert_eq!(
            parse_order_status_kind(body, 0, true), Verdict::Unknown,
            "resting + live + partial must keep the row open"
        );
        assert!(
            matches!(parse_order_status_kind(body, 0, false), Verdict::Filled { .. })
        );
        let done = r#"{"status":"matched","size_matched":"100","takingAmount":"100","makingAmount":"50"}"#;
        assert!(
            matches!(parse_order_status_kind(done, 0, true), Verdict::Filled { shares, ..
            } if shares == 100.0)
        );
        let part = r#"{"status":"canceled","size_matched":"30","makingAmount":"15"}"#;
        assert!(
            matches!(parse_order_status_kind(part, 0, true), Verdict::Filled { shares, ..
            } if shares == 30.0)
        );
    }
    #[test]
    fn in_flight_counts_BUY_shares_for_the_custody_arrival_allowance() {
        let path = tmp("inflight_buys");
        std::fs::remove_file(&path).ok();
        let mut log = PendingLog::open(&path);
        let mut b = p("h1", 100);
        b.side = 0;
        b.shares = 40.0;
        b.token = "T9".into();
        log.apply_record(b);
        let f = log.in_flight();
        assert_eq!(
            f.buy_shares_by_token.get(& ("example_lane_26".into(), "T9".into())), Some(& 40.0)
        );
        assert!(
            f.sell_shares_by_token.is_empty(),
            "a buy must never widen the SELL allowance — that direction hides theft"
        );
        std::fs::remove_file(&path).ok();
    }
    fn tmp2(tag: &str) -> String {
        let p = std::env::temp_dir()
            .join(
                format!(
                    "cb2_pend_{tag}_{}_{}.jsonl", std::process::id(), crate
                    ::ledger::now_secs()
                ),
            );
        let _ = std::fs::remove_file(&p);
        p.to_string_lossy().into_owned()
    }
    fn row_pending(hash: &str, ts: i64) -> String {
        serde_json::json!(
            { "ev" : "pending", "lane" : "example_lane_20", "token" : "T1", "side" : 0,
            "order_hash" : hash, "shares" : 10.0, "limit" : 0.28, "t" : ts, "why" :
            "submitting", "resting" : false }
        )
            .to_string()
    }
    fn row_resolved(hash: &str, ts: i64) -> String {
        serde_json::json!(
            { "ev" : "resolved", "order_hash" : hash, "verdict" : "matched",
            "booked_shares" : 20.0, "booked_price" : 0.14, "t" : ts }
        )
            .to_string()
    }
    #[test]
    fn THE_INCIDENT_a_wal_write_ahead_row_cannot_reopen_an_order_the_file_RESOLVED() {
        let (f, w) = (tmp2("resurrect_f"), tmp2("resurrect_w"));
        std::fs::write(
                &f,
                format!(
                    "{}\n{}\n", row_pending("0xaaa", 1_000), row_resolved("0xaaa", 1_003)
                ),
            )
            .unwrap();
        std::fs::write(&w, format!("{}\n", row_pending("0xaaa", 1_000))).unwrap();
        let (log, _) = PendingLog::open_with_wal(&f, Some(&w));
        assert!(
            ! log.is_open("0xaaa"),
            "a resolved order must STAY resolved — reading the WAL second used to \
                 raise it from the dead and reserve its shares all over again"
        );
        assert_eq!(log.len(), 0);
        std::fs::remove_file(&f).ok();
        std::fs::remove_file(&w).ok();
    }
    #[test]
    fn a_resolution_in_the_WAL_also_settles_a_pending_row_in_the_FILE() {
        let (f, w) = (tmp2("either_f"), tmp2("either_w"));
        std::fs::write(&f, format!("{}\n", row_pending("0xbbb", 1_000))).unwrap();
        std::fs::write(&w, format!("{}\n", row_resolved("0xbbb", 1_004))).unwrap();
        let (log, _) = PendingLog::open_with_wal(&f, Some(&w));
        assert!(! log.is_open("0xbbb"));
        std::fs::remove_file(&f).ok();
        std::fs::remove_file(&w).ok();
    }
    #[test]
    fn a_GENUINELY_unresolved_order_still_survives_the_replay() {
        let (f, w) = (tmp2("live_f"), tmp2("live_w"));
        std::fs::write(
                &f,
                format!(
                    "{}\n{}\n", row_pending("0xccc", 1_000), row_resolved("0xddd", 1_003)
                ),
            )
            .unwrap();
        std::fs::write(&w, format!("{}\n", row_pending("0xeee", 1_005))).unwrap();
        let (log, from_wal) = PendingLog::open_with_wal(&f, Some(&w));
        assert!(log.is_open("0xccc"), "an unresolved file row must survive");
        assert!(log.is_open("0xeee"), "an unresolved WAL row must survive");
        assert_eq!(log.len(), 2);
        assert_eq!(from_wal, 1);
        std::fs::remove_file(&f).ok();
        std::fs::remove_file(&w).ok();
    }
    #[test]
    fn a_REISSUED_hash_is_not_wrongly_suppressed_within_one_replay() {
        let f = tmp2("reissue");
        std::fs::write(
                &f,
                format!(
                    "{}\n{}\n{}\n", row_pending("0xfff", 1_000), row_resolved("0xfff",
                    1_003), row_pending("0xfff", 1_010)
                ),
            )
            .unwrap();
        let (log, _) = PendingLog::open_with_wal(&f, None);
        assert!(
            ! log.is_open("0xfff"),
            "documented: a hash once resolved stays resolved for this replay. Order \
                 digests are unique per salt, so a genuine reissue has a DIFFERENT hash."
        );
        std::fs::remove_file(&f).ok();
    }
    #[test]
    fn a_TAKER_order_reported_live_did_not_fill() {
        let body = r#"{"status":"live","size_matched":"0"}"#;
        assert_eq!(parse_order_status_kind(body, 1, false), Verdict::NotFilled);
        assert_eq!(
            parse_order_status(body, 1), Verdict::NotFilled, "default stays taker"
        );
    }
    #[test]
    fn a_RESTING_order_reported_live_is_STILL_WORKING_not_a_verdict() {
        let body = r#"{"status":"live","size_matched":"0"}"#;
        assert_eq!(parse_order_status_kind(body, 1, true), Verdict::Unknown);
        assert_eq!(
            parse_order_status_kind(r#"{"status":"delayed"}"#, 1, true), Verdict::Unknown
        );
    }
    #[test]
    fn a_resting_order_that_LEAVES_the_book_still_resolves() {
        for st in ["cancelled", "canceled", "unmatched", "expired", "rejected"] {
            let body = format!(r#"{{"status":"{st}","size_matched":"0"}}"#);
            assert_eq!(
                parse_order_status_kind(& body, 1, true), Verdict::NotFilled,
                "{st} is terminal for a resting order too"
            );
        }
    }
    #[test]
    fn a_resting_order_that_FILLED_is_still_booked() {
        let body = r#"{"status":"matched","size_matched":"40","takingAmount":"25.6"}"#;
        match parse_order_status_kind(body, 1, true) {
            Verdict::Filled { shares, .. } => assert!((shares - 40.0).abs() < 1e-9),
            v => panic!("a filled resting order must book, got {v:?}"),
        }
    }
    #[test]
    fn the_resting_flag_is_PERSISTED_and_old_rows_read_as_taker() {
        let p = Pending {
            lane: "l".into(),
            token: "t".into(),
            side: 1,
            order_hash: "0xabc".into(),
            shares: 5.0,
            limit: 0.5,
            ts: 1,
            why: "submitting".into(),
            resting: true,
        };
        let raw = String::from_utf8(PendingLog::row_bytes(&p)).unwrap();
        assert!(raw.contains("\"resting\":true"), "must be persisted: {raw}");
    }
    #[test]
    fn a_PAGINATED_object_reply_is_readable_at_all() {
        let body = r#"{"data":[{"id":"1","size":"25","price":"0.42",
                       "taker_order_id":"0xABC"}],"next_cursor":"LTE="}"#;
        assert_eq!(
            parse_trades(body, "0xabc", 0, false), Verdict::Filled { shares : 25.0, price
            : 0.42 }
        );
    }
    #[test]
    fn ABSENCE_from_a_PARTIAL_page_is_UNKNOWN_not_NOT_FILLED() {
        let body = r#"{"data":[{"id":"1","size":"5","price":"0.5",
                       "taker_order_id":"0xZZZ"}],"next_cursor":"MTAwOjEwMA=="}"#;
        assert_eq!(parse_trades(body, "0xabc", 0, false), Verdict::Unknown);
    }
    #[test]
    fn ABSENCE_from_a_COMPLETE_page_is_a_real_NOT_FILLED() {
        let body = r#"{"data":[{"id":"1","size":"5","price":"0.5",
                       "taker_order_id":"0xZZZ"}],"next_cursor":"LTE="}"#;
        assert_eq!(parse_trades(body, "0xabc", 0, false), Verdict::NotFilled);
        assert_eq!(
            parse_trades(r#"[{"id":"1","taker_order_id":"0xZZZ"}]"#, "0xabc", 0, false),
            Verdict::NotFilled
        );
    }
    #[test]
    fn an_UNINTERPRETABLE_cursor_counts_as_INCOMPLETE() {
        let body = r#"{"data":[],"next_cursor":{"weird":1}}"#;
        assert_eq!(parse_trades(body, "0xabc", 0, false), Verdict::Unknown);
    }
    #[test]
    fn an_UNKNOWN_SHAPE_is_never_a_verdict() {
        assert_eq!(
            parse_trades(r#"{"error":"Unauthorized"}"#, "0xabc", 0, false),
            Verdict::Unknown
        );
        assert_eq!(parse_trades("not json", "0xabc", 0, false), Verdict::Unknown);
        assert_eq!(parse_trades("7", "0xabc", 0, false), Verdict::Unknown);
    }
    #[test]
    fn PARTIAL_FILLS_across_rows_are_summed_at_a_weighted_price() {
        let body = r#"[{"size":"10","price":"0.40","taker_order_id":"0xABC"},
                       {"size":"30","price":"0.50","taker_order_id":"0xabc"}]"#;
        match parse_trades(body, "0xabc", 0, false) {
            Verdict::Filled { shares, price } => {
                assert!((shares - 40.0).abs() < 1e-9);
                assert!((price - 0.475).abs() < 1e-9, "got {price}");
            }
            v => panic!("expected a fill, got {v:?}"),
        }
    }
    #[test]
    fn a_failed_write_LATCHES_and_is_visible_not_merely_refused() {
        let dir = std::env::temp_dir()
            .join(format!("cb2_pendfail_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut l = PendingLog::open(dir.to_str().unwrap());
        assert!(l.persistence_ok(), "healthy before any write");
        let p = Pending {
            lane: "a".into(),
            token: "T".into(),
            side: 1,
            order_hash: "deadbeef".into(),
            shares: 1.0,
            limit: 0.5,
            ts: 1,
            why: "submitting".into(),
            resting: false,
        };
        let err = l.record(p).expect_err("a write to a directory must fail");
        assert!(err.contains("deadbeef"));
        assert!(! l.persistence_ok(), "the fault must LATCH, not vanish");
        assert_eq!(l.write_failures, 1);
        assert!(l.last_write_error.is_some());
        assert!(l.is_empty(), "memory must never claim more than the disk can prove");
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn a_successful_write_keeps_the_log_healthy_and_indexed() {
        let dir = std::env::temp_dir()
            .join(format!("cb2_pendok_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pending.jsonl");
        let mut l = PendingLog::open(path.to_str().unwrap());
        l.record(Pending {
                lane: "a".into(),
                token: "T".into(),
                side: 1,
                order_hash: "abc".into(),
                shares: 2.0,
                limit: 0.4,
                ts: 1,
                why: "submitting".into(),
                resting: false,
            })
            .expect("write ok");
        assert!(l.persistence_ok());
        assert_eq!(l.len(), 1);
        l.resolve("abc", "rejected", None);
        assert_eq!(l.len(), 0);
        let reloaded = PendingLog::open(path.to_str().unwrap());
        assert_eq!(reloaded.len(), 0, "a resolved order must not reappear on restart");
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn a_MATCHED_row_CANNOT_be_closed_without_the_ledger_receipt() {
        let dir = std::env::temp_dir()
            .join(format!("cb2_pendrcpt_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pending.jsonl");
        let lpath = dir.join("ledger.jsonl");
        let mut l = PendingLog::open(path.to_str().unwrap());
        l.record(Pending {
                lane: "a".into(),
                token: "T".into(),
                side: 0,
                order_hash: "deadbeef".into(),
                shares: 5.0,
                limit: 0.4,
                ts: 1,
                why: "submitting".into(),
                resting: false,
            })
            .expect("write ok");
        let mut led = crate::ledger::Ledger::new(
            lpath.to_str().unwrap(),
            &[("a".to_string(), crate::risk::RiskConfig::default())],
        );
        assert_eq!(l.len(), 1, "a match alone must not close the row");
        let receipt = led
            .book_fill("a", "T", 0, 5.0, 0.4, 0.0, None, "deadbeef")
            .expect("the ledger took the fill");
        l.resolve_booked("deadbeef", "matched", receipt).expect("row closes");
        assert_eq!(l.len(), 0);
        let reloaded = PendingLog::open(path.to_str().unwrap());
        assert_eq!(reloaded.len(), 0, "the resolution must survive a restart");
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn a_row_left_open_after_its_fill_landed_cannot_DOUBLE_BOOK_on_recovery() {
        let dir = std::env::temp_dir()
            .join(format!("cb2_pendidem_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lpath = dir.join("ledger.jsonl");
        let mut led = crate::ledger::Ledger::new(
            lpath.to_str().unwrap(),
            &[("a".to_string(), crate::risk::RiskConfig::default())],
        );
        let _first = led
            .book_fill("a", "T", 0, 5.0, 0.4, 0.0, None, "samehash")
            .expect("booked once");
        assert!((led.lanes["a"].positions["T"].shares - 5.0).abs() < 1e-9);
        let again = led.book_fill("a", "T", 0, 5.0, 0.4, 0.0, None, "samehash");
        assert!(
            again.is_some(), "an already-booked fill still yields a receipt to close on"
        );
        assert!(
            (led.lanes["a"].positions["T"].shares - 5.0).abs() < 1e-9,
            "re-booking must move NOTHING, held {}", led.lanes["a"].positions["T"].shares
        );
        let reloaded = crate::ledger::Ledger::new(
            lpath.to_str().unwrap(),
            &[("a".to_string(), crate::risk::RiskConfig::default())],
        );
        assert!(
            reloaded.booked_orders.contains("samehash"),
            "the idempotency key must be rebuilt by replay or a restart re-opens the hole"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn a_FAILED_resolution_LATCHES_instead_of_closing_quietly() {
        let mut l = PendingLog::open("/nonexistent-dir-cb2/pending.jsonl");
        let receipt = crate::ledger::Booked {
            shares: 5.0,
            price: 0.4,
        };
        assert!(
            l.resolve_booked("h", "matched", receipt).is_err(),
            "an unwritable resolution must be reported, not swallowed"
        );
        assert!(! l.persistence_ok(), "and it must LATCH so the buy gate can see it");
    }
    #[test]
    fn a_re_record_of_the_same_digest_does_not_duplicate_on_replay() {
        let dir = std::env::temp_dir()
            .join(format!("cb2_pendre_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pending.jsonl");
        let mut l = PendingLog::open(path.to_str().unwrap());
        let mk = |why: &str| Pending {
            lane: "a".into(),
            token: "T".into(),
            side: 1,
            order_hash: "same".into(),
            shares: 2.0,
            limit: 0.4,
            ts: 1,
            why: why.into(),
            resting: false,
        };
        l.record(mk("submitting")).unwrap();
        l.record(mk("ambiguous:h1:0")).unwrap();
        assert_eq!(l.len(), 1, "one order, one row");
        let reloaded = PendingLog::open(path.to_str().unwrap());
        assert_eq!(reloaded.len(), 1);
        assert_eq!(
            reloaded.open.get("same").map(| p | p.why.as_str()), Some("ambiguous:h1:0"),
            "the LAST word about an order wins on replay"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn a_matched_buy_reads_shares_from_the_TAKER_leg_and_price_from_the_maker_leg() {
        let body = r#"{"status":"matched","takingAmount":"50","makingAmount":"30"}"#;
        assert_eq!(
            parse_order_status(body, 0), Verdict::Filled { shares : 50.0, price : 0.6 }
        );
    }
    #[test]
    fn a_matched_sell_reads_the_legs_the_OTHER_way_round() {
        let body = r#"{"status":"matched","makingAmount":"50","takingAmount":"35"}"#;
        assert_eq!(
            parse_order_status(body, 1), Verdict::Filled { shares : 50.0, price : 0.7 }
        );
    }
    #[test]
    fn size_matched_wins_over_the_raw_legs() {
        let body = r#"{"status":"matched","size_matched":"20","takingAmount":"50","makingAmount":"30"}"#;
        match parse_order_status(body, 0) {
            Verdict::Filled { shares, price } => {
                assert!((shares - 20.0).abs() < 1e-9);
                assert!(
                    price.abs() < 1e-9,
                    "a partial must not derive price from order totals: got {price}"
                );
            }
            v => panic!("want partial fill, got {v:?}"),
        }
    }
    #[test]
    fn a_partial_then_cancelled_gtc_never_books_the_order_total_as_its_price() {
        let body = r#"{"status":"canceled","size_matched":"30",
                       "takingAmount":"100","makingAmount":"50"}"#;
        match parse_order_status_kind(body, 0, true) {
            Verdict::Filled { shares, price } => {
                assert!((shares - 30.0).abs() < 1e-9, "the 30 that matched books");
                assert!(
                    price.abs() < 1e-9,
                    "price must be 0 (-> tape), not 50/30=1.67: got {price}"
                );
            }
            v => panic!("a cancelled partial books its matched shares, got {v:?}"),
        }
        let full = r#"{"status":"matched","size_matched":"100",
                       "takingAmount":"100","makingAmount":"50"}"#;
        match parse_order_status_kind(full, 0, true) {
            Verdict::Filled { shares, price } => {
                assert!((shares - 100.0).abs() < 1e-9);
                assert!((price - 0.50).abs() < 1e-9, "full fill: 50/100 = 0.50");
            }
            v => panic!("full fill must book with its price, got {v:?}"),
        }
    }
    #[test]
    fn a_dead_order_is_NOT_FILLED_and_an_unrecognised_one_stays_UNKNOWN() {
        for s in ["cancelled", "canceled", "unmatched", "expired", "rejected"] {
            assert_eq!(
                parse_order_status(& format!(r#"{{"status":"{s}"}}"#), 0),
                Verdict::NotFilled, "{s} is a real answer"
            );
        }
        assert_eq!(parse_order_status(r#"{"status":"live"}"#, 0), Verdict::NotFilled);
        assert_eq!(
            parse_order_status(r#"{"status":"weird-new-state"}"#, 0), Verdict::Unknown
        );
        assert_eq!(parse_order_status("not json", 0), Verdict::Unknown);
        assert_eq!(parse_order_status(r#"{"error":"not found"}"#, 0), Verdict::Unknown);
        assert_eq!(parse_order_status(r#"{"errorMsg":"boom"}"#, 0), Verdict::Unknown);
    }
    #[test]
    fn an_unmatched_status_with_no_size_never_books() {
        assert_eq!(parse_order_status(r#"{"success":true}"#, 0), Verdict::Unknown);
        assert_eq!(
            parse_order_status(r#"{"status":"matched","size_matched":"0"}"#, 0),
            Verdict::Unknown
        );
    }
    #[test]
    fn our_order_is_found_in_a_trades_list_and_tranches_SUM() {
        let body = r#"[
          {"taker_order_id":"0xAABB","size":"10","price":"0.50"},
          {"maker_orders":[{"order_id":"0xaabb"}],"size":"30","price":"0.60"},
          {"taker_order_id":"0xffff","size":"99","price":"0.10"}
        ]"#;
        match parse_trades(body, "0xAABB", 0, false) {
            Verdict::Filled { shares, price } => {
                assert!((shares - 40.0).abs() < 1e-9, "both tranches, got {shares}");
                assert!((price - 0.575).abs() < 1e-9, "size-weighted, got {price}");
            }
            v => panic!("want summed fill, got {v:?}"),
        }
    }
    #[test]
    fn an_order_absent_from_a_readable_list_is_not_filled_but_junk_is_unknown() {
        let body = r#"[{"taker_order_id":"0xdead","size":"5","price":"0.5"}]"#;
        assert_eq!(parse_trades(body, "0xbeef", 0, false), Verdict::NotFilled);
        assert_eq!(parse_trades("not json", "0xbeef", 0, false), Verdict::Unknown);
        assert_eq!(
            parse_trades(r#"{"error":"nope"}"#, "0xbeef", 0, false), Verdict::Unknown
        );
        assert_eq!(
            parse_trades("[]", "", 0, false), Verdict::Unknown, "no hash = no answer"
        );
    }
    fn tmp(tag: &str) -> String {
        std::env::temp_dir()
            .join(format!("cb2_pending_{tag}_{}", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }
    fn p(hash: &str, ts: i64) -> Pending {
        Pending {
            lane: "example_lane_26".into(),
            token: "T1".into(),
            side: 0,
            order_hash: hash.into(),
            shares: 10.0,
            limit: 0.5,
            ts,
            why: "duplicate_only".into(),
            resting: false,
        }
    }
    #[test]
    fn in_flight_sell_shares_count_only_SELLS_and_sum_per_lane_and_token() {
        let path = tmp("inflight");
        std::fs::remove_file(&path).ok();
        let mut log = PendingLog::open(&path);
        let mut sell = |h: &str, lane: &str, tok: &str, sh: f64| {
            let mut r = p(h, 100);
            r.side = 1;
            r.lane = lane.into();
            r.token = tok.into();
            r.shares = sh;
            log.apply_record(r);
        };
        sell("a", "example_lane_26", "T1", 30.0);
        sell("b", "example_lane_26", "T1", 12.5);
        sell("c", "example_lane_26", "T2", 7.0);
        sell("d", "example_lane_25", "T1", 99.0);
        let mut buy = p("e", 100);
        buy.side = 0;
        buy.shares = 500.0;
        log.apply_record(buy);
        let f = log.in_flight().sell_shares_by_token;
        assert_eq!(
            f.get(& ("example_lane_26".into(), "T1".into())), Some(& 42.5), "tranches must sum"
        );
        assert_eq!(f.get(& ("example_lane_26".into(), "T2".into())), Some(& 7.0));
        assert_eq!(
            f.get(& ("example_lane_25".into(), "T1".into())), Some(& 99.0),
            "lanes are siloed — one lane's exit cannot excuse another's shortfall"
        );
        assert_eq!(f.len(), 3, "a BUY leaked into the in-flight sell allowance");
        std::fs::remove_file(&path).ok();
    }
    #[test]
    fn a_resolved_sell_stops_excusing_a_venue_shortfall() {
        let path = tmp("inflight_resolve");
        std::fs::remove_file(&path).ok();
        let mut log = PendingLog::open(&path);
        let mut r = p("h1", 100);
        r.side = 1;
        r.shares = 30.0;
        log.apply_record(r);
        assert_eq!(log.in_flight().sell_shares_by_token.len(), 1);
        log.resolve("h1", "matched", Some((30.0, 0.5)));
        assert!(log.in_flight().sell_shares_by_token.is_empty());
        std::fs::remove_file(&path).ok();
    }
    #[test]
    fn a_pending_row_SURVIVES_a_restart_and_resolving_closes_it() {
        let path = tmp("restart");
        std::fs::remove_file(&path).ok();
        {
            let mut log = PendingLog::open(&path);
            assert!(log.is_empty());
            log.record(p("0xaaa", 1000))
                .expect("the write-ahead must succeed in a test");
            log.record(p("0xbbb", 1000))
                .expect("the write-ahead must succeed in a test");
            assert_eq!(log.len(), 2);
        }
        {
            let mut log = PendingLog::open(&path);
            assert_eq!(log.len(), 2, "both rows must survive the restart");
            log.resolve("0xaaa", "filled", Some((10.0, 0.5)));
            assert_eq!(log.len(), 1);
        }
        {
            let log = PendingLog::open(&path);
            assert_eq!(log.len(), 1, "a resolved row must stay closed");
            assert_eq!(log.due(2000) [0].order_hash, "0xbbb");
        }
        std::fs::remove_file(&path).ok();
    }
    #[test]
    fn a_fresh_row_is_not_probed_until_the_venue_could_answer() {
        let path = tmp("settle");
        std::fs::remove_file(&path).ok();
        let mut log = PendingLog::open(&path);
        log.record(p("0xaaa", 1000)).expect("the write-ahead must succeed in a test");
        assert!(log.due(1000).is_empty(), "asking instantly only ever answers unknown");
        assert_eq!(log.due(1000 + SETTLE_SECS).len(), 1);
        std::fs::remove_file(&path).ok();
    }
    #[test]
    fn a_row_that_never_resolves_becomes_STALE_and_is_never_silently_dropped() {
        let path = tmp("stale");
        std::fs::remove_file(&path).ok();
        let mut log = PendingLog::open(&path);
        log.record(p("0xaaa", 1000)).expect("the write-ahead must succeed in a test");
        assert!(log.stale(1000 + STALE_SECS).is_empty());
        assert_eq!(log.stale(1001 + STALE_SECS).len(), 1);
        assert_eq!(log.len(), 1, "stale is a REPORT, not a deletion");
        std::fs::remove_file(&path).ok();
    }
    #[test]
    fn open_tokens_name_what_boot_reconciliation_must_leave_alone() {
        let path = tmp("tokens");
        std::fs::remove_file(&path).ok();
        let mut log = PendingLog::open(&path);
        log.record(p("0xaaa", 1000)).expect("the write-ahead must succeed in a test");
        assert_eq!(log.open_tokens(), vec![("example_lane_26".to_string(), "T1".to_string())]);
        log.resolve("0xaaa", "filled", Some((10.0, 0.5)));
        assert!(log.open_tokens().is_empty());
        std::fs::remove_file(&path).ok();
    }
}
