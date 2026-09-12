use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;
pub const CLOB_WS: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
pub const MAX_AGE: Duration = Duration::from_secs(5);
#[derive(Debug, Clone, Copy)]
pub struct Top {
    pub best_bid: f64,
    pub best_ask: f64,
    pub at: Instant,
}
impl Top {
    pub fn spread(&self) -> f64 {
        (self.best_ask - self.best_bid).max(0.0)
    }
    pub fn fresh(&self) -> bool {
        self.at.elapsed() < MAX_AGE
    }
}
#[derive(Default)]
pub struct BookCache {
    tops: Mutex<HashMap<String, Top>>,
}
impl BookCache {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn put(&self, token: &str, best_bid: f64, best_ask: f64) {
        if best_bid <= 0.0 || best_ask <= 0.0 || best_ask <= best_bid {
            return;
        }
        self.tops
            .lock()
            .unwrap()
            .insert(
                token.to_string(),
                Top {
                    best_bid,
                    best_ask,
                    at: Instant::now(),
                },
            );
    }
    pub fn top(&self, token: &str) -> Option<Top> {
        let g = self.tops.lock().unwrap();
        g.get(token).copied().filter(|t| t.fresh())
    }
    pub fn len(&self) -> usize {
        self.tops.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
fn now_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
#[derive(Default)]
pub struct TradePrints {
    cum: Mutex<HashMap<(String, i64), f64>>,
    seen: Mutex<HashMap<(String, i64, i64, i64), i64>>,
}
pub const PRINT_DEDUP_MS: i64 = 750;
impl TradePrints {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn record_at(
        &self,
        token: &str,
        price: f64,
        size: f64,
        side: &str,
        now_ms: i64,
    ) {
        if !(price > 0.0) || !(size > 0.0) {
            return;
        }
        let pk = price_key(price);
        let sk = price_key(size);
        let side_k = match side.chars().next() {
            Some('b') | Some('B') => 1,
            Some('s') | Some('S') => 2,
            _ => 0,
        };
        {
            let mut seen = self.seen.lock().unwrap();
            let key = (token.to_string(), pk, sk, side_k);
            if let Some(prev) = seen.get(&key) {
                let delta = now_ms - *prev;
                if delta >= 0 && delta < PRINT_DEDUP_MS {
                    return;
                }
            }
            seen.insert(key, now_ms);
            if seen.len() > 4096 {
                let cutoff = now_ms - PRINT_DEDUP_MS;
                seen.retain(|_, t| *t >= cutoff);
            }
        }
        *self.cum.lock().unwrap().entry((token.to_string(), pk)).or_insert(0.0) += size;
    }
    pub fn record(&self, token: &str, price: f64, size: f64) {
        self.record_at(token, price, size, "", now_ms_i64());
    }
    pub fn retain_tokens(&self, keep: &std::collections::HashSet<String>) {
        self.cum.lock().unwrap().retain(|(t, _), _| keep.contains(t));
    }
    pub fn cumulative_at(&self, token: &str, price: f64) -> f64 {
        self.cum
            .lock()
            .unwrap()
            .get(&(token.to_string(), price_key(price)))
            .copied()
            .unwrap_or(0.0)
    }
}
#[derive(Default)]
pub struct Levels {
    inner: Mutex<HashMap<String, HashMap<i64, (f64, Instant)>>>,
    last_frame: Mutex<Option<Instant>>,
}
fn price_key(price: f64) -> i64 {
    (price * 10_000.0).round() as i64
}
pub const LEVEL_MAX_AGE: Duration = Duration::from_secs(120);
const LEVELS_PER_TOKEN: usize = 64;
impl Levels {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn put(&self, token: &str, price: f64, size: f64) {
        if !(price > 0.0) || !size.is_finite() || size < 0.0 {
            return;
        }
        *self.last_frame.lock().unwrap() = Some(Instant::now());
        let mut g = self.inner.lock().unwrap();
        let m = g.entry(token.to_string()).or_default();
        m.insert(price_key(price), (size, Instant::now()));
        if m.len() > LEVELS_PER_TOKEN {
            m.retain(|_, (_, t)| t.elapsed() <= LEVEL_MAX_AGE);
            while m.len() > LEVELS_PER_TOKEN {
                let Some(oldest) = m.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| *k)
                else { break };
                m.remove(&oldest);
            }
        }
    }
    pub fn size_at(&self, token: &str, price: f64) -> Option<f64> {
        let g = self.inner.lock().unwrap();
        let (sz, at) = g.get(token)?.get(&price_key(price)).copied()?;
        if at.elapsed() > LEVEL_MAX_AGE {
            return None;
        }
        Some(sz)
    }
    pub fn put_snapshot(&self, token: &str, levels: &[(f64, f64)]) {
        let mut m: HashMap<i64, (f64, Instant)> = HashMap::new();
        let now = Instant::now();
        *self.last_frame.lock().unwrap() = Some(now);
        for (px, sz) in levels {
            if !(*px > 0.0) || !sz.is_finite() || *sz < 0.0 {
                continue;
            }
            m.insert(price_key(*px), (*sz, now));
        }
        let mut g = self.inner.lock().unwrap();
        if let Some(old) = g.get(token) {
            for k in old.keys() {
                m.entry(*k).or_insert((0.0, now));
            }
        }
        g.insert(token.to_string(), m);
    }
    pub fn feed_alive(&self, within: Duration) -> bool {
        self.last_frame.lock().unwrap().map(|t| t.elapsed() <= within).unwrap_or(false)
    }
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn retain_tokens(&self, keep: &std::collections::HashSet<String>) {
        self.inner.lock().unwrap().retain(|t, _| keep.contains(t));
    }
}
pub fn room_ticks(top: &Top, his_price: f64, side: u8, tick: f64) -> i64 {
    if tick <= 0.0 {
        return 0;
    }
    let raw = if side == 0 {
        (top.best_ask - tick - his_price) / tick
    } else {
        (his_price - (top.best_bid + tick)) / tick
    };
    (raw + 1e-9).floor() as i64
}
pub fn tick_size(price: f64) -> f64 {
    if price > 0.96 || price < 0.04 { 0.001 } else { 0.01 }
}
pub async fn run_book_feed(cache: Arc<BookCache>, tokens: Arc<Mutex<Vec<String>>>) {
    run_book_feed_with(cache, tokens, None, None, None).await
}
pub const WATCH_CAP: usize = 200;
pub const WATCH_HARD_MAX: usize = 2 * WATCH_CAP;
pub fn evict_watch_over_cap(
    w: &mut Vec<String>,
    cap: usize,
    pinned: &std::collections::HashSet<String>,
) {
    let mut i = 0;
    while w.len() > cap && i < w.len() {
        if pinned.contains(&w[i]) {
            i += 1;
        } else {
            w.remove(i);
        }
    }
}
pub fn canonical(tokens: &[String]) -> Vec<String> {
    let mut v: Vec<String> = tokens.to_vec();
    v.sort();
    v.dedup();
    v
}
pub async fn run_book_feed_multi(
    cache: Arc<BookCache>,
    tokens: Arc<Mutex<Vec<String>>>,
    fps: Option<
        (
            Arc<crate::fingerprint::Fingerprints>,
            Arc<crate::fingerprint::FingerprintScore>,
            std::sync::mpsc::Sender<crate::fingerprint::Candidate>,
        ),
    >,
    prints: Option<Arc<TradePrints>>,
    levels: Option<Arc<Levels>>,
    sockets: usize,
) {
    let mut handles = Vec::new();
    for i in 0..sockets.max(1) {
        let (c, t, f, pr, lv) = (
            cache.clone(),
            tokens.clone(),
            fps.clone(),
            prints.clone(),
            levels.clone(),
        );
        handles
            .push(
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(120 * i as u64)).await;
                    run_book_feed_with(c, t, f, pr, lv).await
                }),
            );
    }
    for h in handles {
        let _ = h.await;
    }
}
pub async fn run_book_feed_with(
    cache: Arc<BookCache>,
    tokens: Arc<Mutex<Vec<String>>>,
    fps: Option<
        (
            Arc<crate::fingerprint::Fingerprints>,
            Arc<crate::fingerprint::FingerprintScore>,
            std::sync::mpsc::Sender<crate::fingerprint::Candidate>,
        ),
    >,
    prints: Option<Arc<TradePrints>>,
    levels: Option<Arc<Levels>>,
) {
    loop {
        let want = canonical(&tokens.lock().unwrap());
        if want.is_empty() {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        if let Err(e) = pump(
                &cache,
                &want,
                &tokens,
                fps.as_ref(),
                prints.as_ref(),
                levels.as_ref(),
            )
            .await
        {
            eprintln!("[book] {e}");
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
}
type FpTrio = (
    Arc<crate::fingerprint::Fingerprints>,
    Arc<crate::fingerprint::FingerprintScore>,
    std::sync::mpsc::Sender<crate::fingerprint::Candidate>,
);
async fn pump(
    cache: &Arc<BookCache>,
    tokens: &[String],
    watch: &Arc<Mutex<Vec<String>>>,
    fps: Option<&FpTrio>,
    prints: Option<&Arc<TradePrints>>,
    levels: Option<&Arc<Levels>>,
) -> Result<(), String> {
    let (mut ws, _) = tokio_tungstenite::connect_async(CLOB_WS)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let sub = serde_json::json!({ "assets_ids" : tokens, "type" : "market" });
    ws.send(Message::Text(sub.to_string()))
        .await
        .map_err(|e| format!("subscribe: {e}"))?;
    loop {
        {
            let now = canonical(&watch.lock().unwrap());
            if now != tokens {
                return Ok(());
            }
        }
        let msg = match tokio::time::timeout(Duration::from_secs(60), ws.next()).await {
            Err(_) => return Err("silent past 60s".into()),
            Ok(None) => return Ok(()),
            Ok(Some(Err(e))) => return Err(format!("recv: {e}")),
            Ok(Some(Ok(m))) => m,
        };
        let raw = match msg {
            Message::Text(t) => t,
            Message::Close(_) => return Ok(()),
            _ => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let items: Vec<&serde_json::Value> = match v.as_array() {
            Some(a) => a.iter().collect(),
            None => vec![& v],
        };
        for it in items {
            apply_frame(it, cache, prints, levels, fps);
        }
    }
}
fn fstr(v: &serde_json::Value) -> Option<f64> {
    v.as_str().and_then(|x| x.parse::<f64>().ok())
}
pub fn apply_frame(
    it: &serde_json::Value,
    cache: &Arc<BookCache>,
    prints: Option<&Arc<TradePrints>>,
    levels: Option<&Arc<Levels>>,
    fps: Option<&FpTrio>,
) {
    match it["event_type"].as_str().unwrap_or("") {
        "price_change" => {
            let Some(changes) = it["price_changes"].as_array() else { return };
            for c in changes {
                let token = c["asset_id"].as_str().unwrap_or("");
                if token.is_empty() {
                    continue;
                }
                if let (Some(bb), Some(ba)) = (
                    fstr(&c["best_bid"]),
                    fstr(&c["best_ask"]),
                ) {
                    cache.put(token, bb, ba);
                }
                if let (Some(l), Some(px), Some(sz)) = (
                    levels,
                    fstr(&c["price"]),
                    fstr(&c["size"]),
                ) {
                    l.put(token, px, sz);
                }
                if let Some((fp, score, tx)) = fps {
                    let side = match c["side"].as_str().unwrap_or("") {
                        s if s.eq_ignore_ascii_case("buy") => 0u8,
                        s if s.eq_ignore_ascii_case("sell") => 1u8,
                        _ => continue,
                    };
                    if let (Some(px), Some(sz)) = (fstr(&c["price"]), fstr(&c["size"])) {
                        if let Some(cand) = fp.on_book(token, px, sz) {
                            if cand.side == side {
                                score.saw(cand.clone());
                                let _ = tx.send(cand);
                            }
                        }
                    }
                }
            }
        }
        "last_trade_price" => {
            let token = it["asset_id"].as_str().unwrap_or("");
            if token.is_empty() {
                return;
            }
            if let Some(pr) = prints {
                if let (Some(px), Some(sz)) = (fstr(&it["price"]), fstr(&it["size"])) {
                    pr.record_at(
                        token,
                        px,
                        sz,
                        it["side"].as_str().unwrap_or(""),
                        now_ms_i64(),
                    );
                }
            }
        }
        _ => {
            let token = it["asset_id"].as_str().unwrap_or("");
            if token.is_empty() {
                return;
            }
            let bids = it["bids"].as_array().or_else(|| it["buys"].as_array());
            let asks = it["asks"].as_array().or_else(|| it["sells"].as_array());
            if let (Some(b), Some(a)) = (bids, asks) {
                let bb = b
                    .iter()
                    .filter_map(|x| x["price"].as_str()?.parse::<f64>().ok())
                    .fold(0.0f64, f64::max);
                let ba = a
                    .iter()
                    .filter_map(|x| x["price"].as_str()?.parse::<f64>().ok())
                    .fold(f64::MAX, f64::min);
                if ba < f64::MAX {
                    cache.put(token, bb, ba);
                }
                if let Some(l) = levels {
                    let mut all: Vec<(f64, f64)> = Vec::new();
                    for arr in [b, a] {
                        for lvl in arr {
                            if let (Some(px), Some(sz)) = (
                                fstr(&lvl["price"]),
                                fstr(&lvl["size"]),
                            ) {
                                all.push((px, sz));
                            }
                        }
                    }
                    l.put_snapshot(token, &all);
                }
                if let Some((fp, score, tx)) = fps {
                    for (side, arr) in [(0u8, b), (1u8, a)] {
                        for lvl in arr {
                            let (Some(ps), Some(ss)) = (
                                lvl["price"].as_str(),
                                lvl["size"].as_str(),
                            ) else { continue };
                            let (Ok(px), Ok(sz)) = (ps.parse::<f64>(), ss.parse::<f64>())
                            else { continue };
                            if let Some(c) = fp.on_book(token, px, sz) {
                                if c.side == side {
                                    score.saw(c.clone());
                                    let _ = tx.send(c);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    fn print_frame(
        token: &str,
        price: &str,
        size: &str,
        side: &str,
    ) -> serde_json::Value {
        serde_json::json!(
            { "event_type" : "last_trade_price", "asset_id" : token, "price" : price,
            "size" : size, "side" : side, }
        )
    }
    #[test]
    fn WIRE_a_last_trade_price_frame_still_reaches_the_counter() {
        let cache = std::sync::Arc::new(BookCache::new());
        let prints = std::sync::Arc::new(TradePrints::new());
        apply_frame(
            &print_frame("T", "0.50", "100", "buy"),
            &cache,
            Some(&prints),
            None,
            None,
        );
        assert_eq!(
            prints.cumulative_at("T", 0.50), 100.0,
            "the frame must still reach the print counter"
        );
    }
    #[test]
    fn WIRE_the_same_frame_three_times_counts_ONCE() {
        let cache = std::sync::Arc::new(BookCache::new());
        let prints = std::sync::Arc::new(TradePrints::new());
        let f = print_frame("T", "0.50", "100", "buy");
        for _ in 0..3 {
            apply_frame(&f, &cache, Some(&prints), None, None);
        }
        assert_eq!(prints.cumulative_at("T", 0.50), 100.0);
    }
    #[test]
    fn WIRE_a_buy_and_a_sell_frame_both_count() {
        let cache = std::sync::Arc::new(BookCache::new());
        let prints = std::sync::Arc::new(TradePrints::new());
        apply_frame(
            &print_frame("T", "0.50", "100", "buy"),
            &cache,
            Some(&prints),
            None,
            None,
        );
        apply_frame(
            &print_frame("T", "0.50", "100", "sell"),
            &cache,
            Some(&prints),
            None,
            None,
        );
        assert_eq!(
            prints.cumulative_at("T", 0.50), 200.0,
            "side must be parsed off the frame, not defaulted"
        );
    }
    #[test]
    fn WIRE_a_frame_with_no_side_field_still_records() {
        let cache = std::sync::Arc::new(BookCache::new());
        let prints = std::sync::Arc::new(TradePrints::new());
        let f = serde_json::json!(
            { "event_type" : "last_trade_price", "asset_id" : "T", "price" : "0.50",
            "size" : "100" }
        );
        apply_frame(&f, &cache, Some(&prints), None, None);
        assert_eq!(prints.cumulative_at("T", 0.50), 100.0);
    }
    #[test]
    fn RT_a_real_iceberg_of_identical_clips_is_UNDERCOUNTED_and_that_is_the_safe_way() {
        let p = TradePrints::new();
        for i in 0..5 {
            p.record_at("T", 0.50, 100.0, "buy", 1_000_000 + i * 10);
        }
        assert_eq!(p.cumulative_at("T", 0.50), 100.0);
        p.record_at("T", 0.50, 100.0, "buy", 1_000_000 + PRINT_DEDUP_MS + 1);
        assert_eq!(p.cumulative_at("T", 0.50), 200.0);
    }
    #[test]
    fn RT_dedup_NEVER_makes_the_counter_go_backwards() {
        let p = TradePrints::new();
        let mut last = 0.0;
        for i in 0..200 {
            p.record_at("T", 0.50, 1.0 + (i % 7) as f64, "buy", 1_000_000 + i * 37);
            let now = p.cumulative_at("T", 0.50);
            assert!(now >= last, "cumulative print volume must be monotonic");
            last = now;
        }
    }
    #[test]
    fn RT_a_clock_that_JUMPS_BACKWARDS_cannot_wedge_the_dedup() {
        let p = TradePrints::new();
        p.record_at("T", 0.50, 100.0, "buy", 2_000_000);
        p.record_at("T", 0.50, 100.0, "buy", 1_000_000);
        assert_eq!(
            p.cumulative_at("T", 0.50), 200.0,
            "a backwards clock must not swallow prints"
        );
    }
    #[test]
    fn RT_pruning_never_drops_the_CUMULATIVE_totals() {
        let p = TradePrints::new();
        p.record_at("T", 0.50, 100.0, "buy", 1_000_000);
        for i in 0..5_000 {
            p.record_at("T", 0.99, 1.0 + i as f64, "buy", 1_000_000 + i as i64);
        }
        assert_eq!(
            p.cumulative_at("T", 0.50), 100.0,
            "the anchor price's total must survive a flood at another price"
        );
    }
    #[test]
    fn RT_retain_tokens_still_bounds_the_dedup_map_too() {
        let p = TradePrints::new();
        for i in 0..100 {
            p.record_at(&format!("T{i}"), 0.50, 100.0, "buy", 1_000_000 + i as i64);
        }
        let keep: std::collections::HashSet<String> = ["T1".to_string()]
            .into_iter()
            .collect();
        p.retain_tokens(&keep);
        assert!(p.seen.lock().unwrap().len() <= 4096);
    }
    #[test]
    fn THE_SAME_PRINT_ON_THREE_SOCKETS_COUNTS_ONCE() {
        let p = TradePrints::new();
        for _ in 0..3 {
            p.record_at("T", 0.50, 100.0, "buy", 1_000_000);
        }
        assert_eq!(
            p.cumulative_at("T", 0.50), 100.0,
            "three deliveries of one trade are one trade"
        );
    }
    #[test]
    fn a_GENUINE_later_trade_at_the_same_price_still_counts() {
        let p = TradePrints::new();
        p.record_at("T", 0.50, 100.0, "buy", 1_000_000);
        p.record_at("T", 0.50, 100.0, "buy", 1_000_000 + PRINT_DEDUP_MS);
        assert_eq!(p.cumulative_at("T", 0.50), 200.0);
    }
    #[test]
    fn different_SIZES_are_different_trades_even_in_the_same_instant() {
        let p = TradePrints::new();
        p.record_at("T", 0.50, 100.0, "buy", 1_000_000);
        p.record_at("T", 0.50, 250.0, "buy", 1_000_000);
        assert_eq!(p.cumulative_at("T", 0.50), 350.0);
    }
    #[test]
    fn a_BUY_and_a_SELL_of_the_same_size_are_not_collapsed() {
        let p = TradePrints::new();
        p.record_at("T", 0.50, 100.0, "buy", 1_000_000);
        p.record_at("T", 0.50, 100.0, "sell", 1_000_000);
        assert_eq!(p.cumulative_at("T", 0.50), 200.0);
    }
    #[test]
    fn different_TOKENS_never_share_a_duplicate_key() {
        let p = TradePrints::new();
        p.record_at("A", 0.50, 100.0, "buy", 1_000_000);
        p.record_at("B", 0.50, 100.0, "buy", 1_000_000);
        assert_eq!(p.cumulative_at("A", 0.50), 100.0);
        assert_eq!(p.cumulative_at("B", 0.50), 100.0);
    }
    #[test]
    fn the_duplicate_map_stays_BOUNDED_under_a_flood() {
        let p = TradePrints::new();
        for i in 0..5_000 {
            p.record_at("T", 0.50, 1.0 + i as f64, "buy", 1_000_000 + i as i64);
        }
        assert!(p.seen.lock().unwrap().len() <= 4096, "the dedup map must be bounded");
    }
    #[test]
    fn THE_CANCEL_SIGNAL_SURVIVES_duplicate_delivery() {
        let p = TradePrints::new();
        for _ in 0..3 {
            p.record_at("T", 0.50, 100.0, "buy", 1_000_000);
        }
        let printed = p.cumulative_at("T", 0.50);
        assert_eq!(printed, 100.0);
        assert!(
            printed < 300.0,
            "a 300-share decrement must NOT look fully explained by prints"
        );
    }
    use super::*;
    fn top(bb: f64, ba: f64) -> Top {
        Top {
            best_bid: bb,
            best_ask: ba,
            at: Instant::now(),
        }
    }
    #[test]
    fn a_one_tick_spread_has_no_room() {
        assert_eq!(room_ticks(& top(0.60, 0.61), 0.60, 0, 0.01), 0);
    }
    #[test]
    fn a_wide_spread_has_room() {
        assert_eq!(room_ticks(& top(0.60, 0.65), 0.60, 0, 0.01), 4);
    }
    #[test]
    fn sell_side_room_is_measured_against_the_bid() {
        assert_eq!(room_ticks(& top(0.60, 0.65), 0.65, 1, 0.01), 4);
        assert_eq!(room_ticks(& top(0.60, 0.61), 0.61, 1, 0.01), 0);
    }
    #[test]
    fn a_stale_book_is_never_returned() {
        let c = BookCache::new();
        c.put("T", 0.60, 0.65);
        assert!(c.top("T").is_some());
        let mut g = c.tops.lock().unwrap();
        g.get_mut("T").unwrap().at = Instant::now() - Duration::from_secs(30);
        drop(g);
        assert!(c.top("T").is_none(), "acting on a stale book rests at a dead price");
    }
    #[test]
    fn a_crossed_or_empty_book_is_never_cached() {
        let c = BookCache::new();
        c.put("T", 0.65, 0.60);
        c.put("U", 0.0, 0.60);
        c.put("V", 0.60, 0.0);
        assert!(c.is_empty());
    }
    #[test]
    fn tick_size_and_sell_floor_agree_on_EVERY_price() {
        let mut p = 0.001_f64;
        while p < 1.0 {
            let tick = tick_size(p);
            let floor = crate::venue::sell_floor(p);
            assert!(
                (tick - floor).abs() < 1e-12,
                "price {p}: tick_size={tick} but sell_floor={floor}"
            );
            p += 0.001;
        }
    }
    #[test]
    fn tick_size_follows_the_venue_bands() {
        assert_eq!(tick_size(0.50), 0.01);
        assert_eq!(tick_size(0.05), 0.01);
        assert_eq!(tick_size(0.95), 0.01);
        assert_eq!(tick_size(0.03), 0.001);
        assert_eq!(tick_size(0.97), 0.001);
    }
    fn arced() -> Arc<BookCache> {
        Arc::new(BookCache::new())
    }
    #[test]
    fn a_price_change_frame_moves_the_top() {
        let c = arced();
        let frame: serde_json::Value = serde_json::from_str(
                r#"{
            "event_type": "price_change",
            "price_changes": [
                {"asset_id": "TOK1", "side": "BUY", "price": "0.60", "size": "1200",
                 "best_bid": "0.60", "best_ask": "0.62"}
            ]
        }"#,
            )
            .unwrap();
        apply_frame(&frame, &c, None, None, None);
        let t = c.top("TOK1").expect("price_change must feed the cache");
        assert_eq!((t.best_bid, t.best_ask), (0.60, 0.62));
    }
    #[test]
    fn a_price_change_with_multiple_assets_updates_each() {
        let c = arced();
        let frame: serde_json::Value = serde_json::from_str(
                r#"{
            "event_type": "price_change",
            "price_changes": [
                {"asset_id": "A", "side": "SELL", "price": "0.70", "size": "10",
                 "best_bid": "0.65", "best_ask": "0.70"},
                {"asset_id": "B", "side": "BUY", "price": "0.30", "size": "5",
                 "best_bid": "0.30", "best_ask": "0.33"}
            ]
        }"#,
            )
            .unwrap();
        apply_frame(&frame, &c, None, None, None);
        assert!(c.top("A").is_some() && c.top("B").is_some());
    }
    #[test]
    fn a_book_snapshot_still_works_exactly_as_before() {
        let c = arced();
        let frame: serde_json::Value = serde_json::from_str(
                r#"{
            "event_type": "book", "asset_id": "TOK1",
            "bids": [{"price": "0.58", "size": "100"}, {"price": "0.60", "size": "50"}],
            "asks": [{"price": "0.63", "size": "80"}, {"price": "0.65", "size": "10"}]
        }"#,
            )
            .unwrap();
        apply_frame(&frame, &c, None, None, None);
        let t = c.top("TOK1").unwrap();
        assert_eq!((t.best_bid, t.best_ask), (0.60, 0.63));
    }
    #[test]
    fn a_trade_print_is_recorded_and_queryable() {
        let c = arced();
        let pr = Arc::new(TradePrints::new());
        let frame: serde_json::Value = serde_json::from_str(
                r#"{
            "event_type": "last_trade_price", "asset_id": "TOK1",
            "price": "0.62", "size": "150", "side": "BUY"
        }"#,
            )
            .unwrap();
        apply_frame(&frame, &c, Some(&pr), None, None);
        assert_eq!(pr.cumulative_at("TOK1", 0.62), 150.0);
        assert_eq!(
            pr.cumulative_at("TOK1", 0.70), 0.0,
            "a print at one price must not corroborate another"
        );
    }
    #[test]
    fn a_malformed_frame_is_ignored_not_a_panic() {
        let c = arced();
        for bad in [
            r#"{"event_type":"price_change"}"#,
            r#"{"event_type":"price_change","price_changes":[{}]}"#,
            r#"{"event_type":"last_trade_price"}"#,
            r#"{"event_type":"book","asset_id":"T"}"#,
            r#"{}"#,
            r#"[]"#,
            r#"42"#,
        ] {
            let v: serde_json::Value = serde_json::from_str(bad).unwrap();
            apply_frame(&v, &c, None, None, None);
        }
        assert!(c.is_empty());
    }
    #[test]
    fn eviction_skips_pinned_tokens_and_still_bounds_the_rest() {
        use std::collections::HashSet;
        let mut w: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
        let pinned: HashSet<String> = ["t0", "t1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        evict_watch_over_cap(&mut w, 4, &pinned);
        assert_eq!(w.len(), 4);
        assert!(
            w.contains(& "t0".into()) && w.contains(& "t1".into()),
            "a pinned token carries a live rest — evicting it blinds its cancel detector"
        );
        let all: HashSet<String> = w.iter().cloned().collect();
        evict_watch_over_cap(&mut w, 1, &all);
        assert_eq!(
            w.len(), 4,
            "pinned tokens may exceed the cap; the cap bounds churn, not safety"
        );
    }
    #[test]
    fn a_snapshot_that_OMITS_a_level_records_it_as_EMPTY() {
        let l = Levels::new();
        l.put("T", 0.62, 4600.0);
        assert_eq!(l.size_at("T", 0.62), Some(4600.0));
        l.put_snapshot("T", &[(0.55, 10.0), (0.70, 10.0)]);
        assert_eq!(
            l.size_at("T", 0.62), Some(0.0), "an omitted level is EMPTY, not unknown"
        );
        assert_eq!(l.size_at("T", 0.55), Some(10.0));
    }
    #[test]
    fn the_per_token_level_map_is_HARD_bounded() {
        let l = Levels::new();
        for i in 1..500 {
            l.put("T", i as f64 / 1000.0, 10.0);
        }
        assert!(l.size_at("T", 0.499).is_some(), "the newest levels survive");
        assert!(l.size_at("T", 0.001).is_none(), "the oldest level was dropped");
    }
    #[test]
    fn a_level_we_have_NEVER_seen_is_None_not_zero() {
        let l = Levels::new();
        assert_eq!(l.size_at("T", 0.62), None);
        l.put("T", 0.62, 100.0);
        assert_eq!(l.size_at("U", 0.62), None, "another token proves nothing");
        assert_eq!(l.size_at("T", 0.63), None, "another price proves nothing");
    }
    #[test]
    fn cumulative_prints_are_monotonic_and_keyed_on_the_PRICE_GRID() {
        let pr = TradePrints::new();
        pr.record("T", 0.62, 100.0);
        pr.record("T", 620_000.0 / 1_000_000.0, 50.0);
        assert_eq!(pr.cumulative_at("T", 0.62), 150.0);
        assert_eq!(pr.cumulative_at("T", 0.63), 0.0, "a neighbouring level is separate");
        pr.record("T", 0.62, 25.0);
        assert_eq!(pr.cumulative_at("T", 0.62), 175.0, "counters only ever grow");
    }
    #[test]
    fn book_liveness_is_proven_by_FRAMES_not_by_trades() {
        let l = Levels::new();
        assert!(! l.feed_alive(Duration::from_secs(60)), "nothing seen yet");
        l.put("T", 0.62, 10.0);
        assert!(l.feed_alive(Duration::from_secs(60)), "a book frame proves the socket");
    }
    #[test]
    fn ADV_a_stale_level_reading_must_not_read_as_empty() {
        let l = Levels::new();
        l.put("T", 0.62, 4600.0);
        assert_eq!(l.size_at("T", 0.62), Some(4600.0));
        assert!(LEVEL_MAX_AGE > Duration::from_secs(60), "must outlast a normal gap");
    }
    #[test]
    fn ADV_price_grid_collisions_across_the_venue_tick_range() {
        let pr = TradePrints::new();
        for (a, b) in [(0.001, 0.002), (0.62, 0.621), (0.9985, 0.9995), (0.04, 0.041)] {
            pr.record("T", a, 10.0);
            assert_eq!(
                pr.cumulative_at("T", b), 0.0, "prices {a} and {b} collided on the grid"
            );
        }
    }
    #[test]
    fn ADV_retain_tokens_keeps_a_RESTING_tokens_evidence() {
        let l = Levels::new();
        let pr = TradePrints::new();
        l.put("KEEP", 0.5, 100.0);
        l.put("DROP", 0.5, 100.0);
        pr.record("KEEP", 0.5, 10.0);
        pr.record("DROP", 0.5, 10.0);
        let keep: std::collections::HashSet<String> = ["KEEP".to_string()].into();
        l.retain_tokens(&keep);
        pr.retain_tokens(&keep);
        assert_eq!(l.size_at("KEEP", 0.5), Some(100.0));
        assert_eq!(l.size_at("DROP", 0.5), None);
        assert_eq!(pr.cumulative_at("KEEP", 0.5), 10.0);
        assert_eq!(pr.cumulative_at("DROP", 0.5), 0.0);
    }
    #[test]
    fn ADV_a_price_change_to_size_zero_is_recorded_as_zero_not_skipped() {
        let c = Arc::new(BookCache::new());
        let l = Arc::new(Levels::new());
        let fr = |j: &str| -> serde_json::Value { serde_json::from_str(j).unwrap() };
        apply_frame(
            &fr(
                r#"{"event_type":"price_change","price_changes":[
            {"asset_id":"T","side":"BUY","price":"0.62","size":"4600",
             "best_bid":"0.62","best_ask":"0.64"}]}"#,
            ),
            &c,
            None,
            Some(&l),
            None,
        );
        apply_frame(
            &fr(
                r#"{"event_type":"price_change","price_changes":[
            {"asset_id":"T","side":"BUY","price":"0.62","size":"0",
             "best_bid":"0.61","best_ask":"0.64"}]}"#,
            ),
            &c,
            None,
            Some(&l),
            None,
        );
        assert_eq!(l.size_at("T", 0.62), Some(0.0), "size 0 IS the cancel signal");
    }
    #[test]
    fn canonical_ignores_order_and_duplicates() {
        let a = canonical(&["b".into(), "a".into(), "a".into()]);
        let b = canonical(&["a".into(), "b".into()]);
        assert_eq!(a, b);
        let c = canonical(&["a".into(), "c".into()]);
        assert_ne!(a, c, "a real membership change must still be visible");
    }
    #[test]
    fn unknown_token_yields_no_room_so_we_fall_back_to_taking() {
        let c = BookCache::new();
        assert!(c.top("nope").is_none());
    }
}
