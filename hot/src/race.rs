use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
const TTL: Duration = Duration::from_secs(900);
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FillKey {
    pub tx: String,
    pub token: String,
    pub side: u8,
    pub size_micro: u64,
}
pub struct RaceBook {
    inner: Mutex<Inner>,
}
struct Inner {
    seen: HashMap<(FillKey, u32), Instant>,
    per_source: HashMap<(String, FillKey), u32>,
    delivered: HashMap<(String, String, u64), Instant>,
    pub wins: HashMap<String, u64>,
    pub confirms: HashMap<String, u64>,
    pub redeliveries: HashMap<String, u64>,
    pub margins: Vec<(String, Duration)>,
}
impl Default for RaceBook {
    fn default() -> Self {
        Self::new()
    }
}
impl RaceBook {
    pub fn new() -> Self {
        RaceBook {
            inner: Mutex::new(Inner {
                seen: HashMap::new(),
                per_source: HashMap::new(),
                delivered: HashMap::new(),
                wins: HashMap::new(),
                confirms: HashMap::new(),
                redeliveries: HashMap::new(),
                margins: Vec::new(),
            }),
        }
    }
    pub fn first_seen(
        &self,
        source: &str,
        key: FillKey,
        log_index: Option<u64>,
    ) -> bool {
        let now = Instant::now();
        let mut g = self.inner.lock().unwrap();
        g.evict(now);
        if let Some(li) = log_index {
            let ident = (source.to_string(), key.tx.clone(), li);
            if g.delivered.contains_key(&ident) {
                *g.redeliveries.entry(source.to_string()).or_insert(0) += 1;
                return false;
            }
            g.delivered.insert(ident, now);
        }
        let sk = (source.to_string(), key.clone());
        let occ = *g.per_source.get(&sk).unwrap_or(&0);
        g.per_source.insert(sk, occ + 1);
        let full = (key, occ);
        match g.seen.get(&full).copied() {
            None => {
                g.seen.insert(full, now);
                *g.wins.entry(source.to_string()).or_insert(0) += 1;
                true
            }
            Some(first) => {
                *g.confirms.entry(source.to_string()).or_insert(0) += 1;
                let m = now.duration_since(first);
                g.margins.push((source.to_string(), m));
                if g.margins.len() > 500 {
                    g.margins.drain(..250);
                }
                false
            }
        }
    }
    pub fn stats(
        &self,
    ) -> (HashMap<String, u64>, HashMap<String, u64>, HashMap<String, u64>) {
        let g = self.inner.lock().unwrap();
        (g.wins.clone(), g.confirms.clone(), g.redeliveries.clone())
    }
    pub fn median_behind_ms(&self) -> HashMap<String, f64> {
        let g = self.inner.lock().unwrap();
        let mut by: HashMap<String, Vec<f64>> = HashMap::new();
        for (s, d) in g.margins.iter() {
            by.entry(s.clone()).or_default().push(d.as_secs_f64() * 1000.0);
        }
        by.into_iter()
            .map(|(k, mut v)| {
                v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                (k, v[v.len() / 2])
            })
            .collect()
    }
}
impl Inner {
    fn evict(&mut self, now: Instant) {
        if self.delivered.len() > 512 {
            self.delivered.retain(|_, t| now.duration_since(*t) < TTL);
        }
        if self.seen.len() < 512 {
            return;
        }
        let before = self.seen.len();
        self.seen.retain(|_, t| now.duration_since(*t) < TTL);
        if self.seen.len() != before && self.per_source.len() > 4000 {
            self.per_source.clear();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn key(tx: &str, size: f64) -> FillKey {
        FillKey {
            tx: tx.into(),
            token: "123456789".into(),
            side: 0,
            size_micro: (size * 1e6) as u64,
        }
    }
    #[test]
    fn first_source_wins_and_the_rest_confirm() {
        let b = RaceBook::new();
        assert!(b.first_seen("mempool0", key("0xa", 100.0), None));
        assert!(! b.first_seen("mempool1", key("0xa", 100.0), None));
        assert!(! b.first_seen("wss0", key("0xa", 100.0), Some(3)));
    }
    #[test]
    fn redundant_sockets_on_one_provider_never_double_fire() {
        let b = RaceBook::new();
        let mut fires = 0;
        for src in ["pn0", "pn1", "pn2", "bora0", "bora1", "drpc0"] {
            if b.first_seen(src, key("0xdeadbeef", 250.0), None) {
                fires += 1;
            }
        }
        assert_eq!(fires, 1, "six feeds of one fill must produce exactly ONE order");
    }
    #[test]
    fn a_source_redelivering_its_own_log_is_suppressed() {
        let b = RaceBook::new();
        assert!(b.first_seen("wss0", key("0xa", 100.0), Some(7)));
        assert!(! b.first_seen("wss0", key("0xa", 100.0), Some(7)));
        let (_, _, redel) = b.stats();
        assert_eq!(redel.get("wss0"), Some(& 1));
    }
    #[test]
    fn two_distinct_fills_in_one_tx_both_fire() {
        let b = RaceBook::new();
        assert!(b.first_seen("wss0", key("0xa", 100.0), Some(3)));
        assert!(b.first_seen("wss0", key("0xa", 250.0), Some(4)));
    }
    #[test]
    fn identical_fills_in_one_tx_both_fire_via_occurrence() {
        let b = RaceBook::new();
        assert!(b.first_seen("mempool0", key("0xa", 100.0), None));
        assert!(
            b.first_seen("mempool0", key("0xa", 100.0), None),
            "second identical fill should get occurrence 1 and fire"
        );
    }
    #[test]
    fn occurrence_lines_up_across_sources() {
        let b = RaceBook::new();
        assert!(b.first_seen("mempool0", key("0xa", 100.0), None));
        assert!(b.first_seen("mempool0", key("0xa", 100.0), None));
        assert!(! b.first_seen("wss0", key("0xa", 100.0), Some(1)));
        assert!(! b.first_seen("wss0", key("0xa", 100.0), Some(2)));
    }
    #[test]
    fn mempool_and_chain_agree_only_because_size_is_exact() {
        let b = RaceBook::new();
        assert!(b.first_seen("mempool0", key("0xa", 648.67), None));
        assert!(
            ! b.first_seen("wss0", key("0xa", 648.67), Some(2)),
            "same fill from the chain must be recognised as a confirm"
        );
        assert!(
            b.first_seen("wss0", key("0xa", 639.34), Some(3)),
            "different size is a different key (documents the failure mode)"
        );
    }
    #[test]
    fn different_transactions_are_independent() {
        let b = RaceBook::new();
        assert!(b.first_seen("mempool0", key("0xa", 100.0), None));
        assert!(b.first_seen("mempool0", key("0xb", 100.0), None));
    }
    #[test]
    fn win_and_confirm_counts_are_tracked_per_source() {
        let b = RaceBook::new();
        b.first_seen("fast", key("0xa", 1.0), None);
        b.first_seen("slow", key("0xa", 1.0), None);
        b.first_seen("fast", key("0xb", 1.0), None);
        b.first_seen("slow", key("0xb", 1.0), None);
        let (wins, confirms, _) = b.stats();
        assert_eq!(wins.get("fast"), Some(& 2));
        assert_eq!(confirms.get("slow"), Some(& 2));
        assert_eq!(wins.get("slow"), None);
    }
}
#[derive(Debug, Default)]
pub struct BehindTally {
    per_source: std::collections::HashMap<String, Behind>,
}
#[derive(Debug, Default, Clone, Copy)]
pub struct Behind {
    pub n: u64,
    pub sum_us: u128,
    pub max_us: u64,
    pub le_1ms: u64,
    pub le_5ms: u64,
    pub le_20ms: u64,
    pub le_100ms: u64,
    pub over_100ms: u64,
}
impl BehindTally {
    pub fn record(&mut self, source: &str, behind_ns: u128) {
        let us = (behind_ns / 1_000) as u64;
        let e = self.per_source.entry(source.to_string()).or_default();
        e.n += 1;
        e.sum_us += us as u128;
        if us > e.max_us {
            e.max_us = us;
        }
        match us {
            0..=1_000 => e.le_1ms += 1,
            1_001..=5_000 => e.le_5ms += 1,
            5_001..=20_000 => e.le_20ms += 1,
            20_001..=100_000 => e.le_100ms += 1,
            _ => e.over_100ms += 1,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.per_source.is_empty()
    }
    pub fn drain_json(&mut self) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        for (k, v) in self.per_source.iter() {
            out.insert(
                k.clone(),
                serde_json::json!(
                    { "n" : v.n, "mean_ms" : if v.n > 0 { (v.sum_us as f64 / v.n as f64)
                    / 1000.0 } else { 0.0 }, "max_ms" : v.max_us as f64 / 1000.0,
                    "le_1ms" : v.le_1ms, "le_5ms" : v.le_5ms, "le_20ms" : v.le_20ms,
                    "le_100ms" : v.le_100ms, "over_100ms" : v.over_100ms, }
                ),
            );
        }
        self.per_source.clear();
        serde_json::Value::Object(out)
    }
}
pub struct SeenTx {
    order: std::collections::VecDeque<String>,
    set: std::collections::HashSet<String>,
    cap: usize,
    won_at_ns: std::collections::HashMap<String, u128>,
}
impl SeenTx {
    pub fn new(cap: usize) -> Self {
        Self {
            order: std::collections::VecDeque::with_capacity(cap),
            set: std::collections::HashSet::with_capacity(cap),
            cap,
            won_at_ns: std::collections::HashMap::with_capacity(cap),
        }
    }
    pub fn first_delivery(&mut self, tx: &str) -> bool {
        self.first_delivery_at(tx, 0).is_none()
    }
    pub fn first_delivery_at(&mut self, tx: &str, seen_ns: u128) -> Option<u128> {
        if self.set.contains(tx) {
            return Some(
                self.won_at_ns.get(tx).map_or(0, |w| seen_ns.saturating_sub(*w)),
            );
        }
        if self.order.len() >= self.cap {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
                self.won_at_ns.remove(&old);
            }
        }
        self.order.push_back(tx.to_string());
        self.set.insert(tx.to_string());
        if seen_ns > 0 {
            self.won_at_ns.insert(tx.to_string(), seen_ns);
        }
        None
    }
    pub fn len(&self) -> usize {
        self.set.len()
    }
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
}
#[cfg(test)]
mod seen_tx_tests {
    use super::*;
    fn key(tx: &str, size: f64) -> FillKey {
        FillKey {
            tx: tx.into(),
            token: "T".into(),
            side: 0,
            size_micro: (size * 1e6) as u64,
        }
    }
    #[test]
    fn a_REDELIVERED_transaction_is_seen_once() {
        let mut s = SeenTx::new(64);
        assert!(s.first_delivery("0xabc"), "first delivery acts");
        assert!(! s.first_delivery("0xabc"), "a redelivery must NOT act again");
        assert!(! s.first_delivery("0xabc"));
    }
    #[test]
    fn DISTINCT_transactions_are_unaffected() {
        let mut s = SeenTx::new(64);
        assert!(s.first_delivery("0xa"));
        assert!(s.first_delivery("0xb"));
    }
    #[test]
    fn ONE_delivery_carrying_TWO_identical_fills_still_yields_TWO() {
        let mut s = SeenTx::new(64);
        assert!(s.first_delivery("0xa"));
        let b = RaceBook::new();
        assert!(b.first_seen("lava0", key("0xa", 100.0), None));
        assert!(
            b.first_seen("lava0", key("0xa", 100.0), None),
            "two identical fills inside ONE decode still both fire"
        );
    }
    #[test]
    fn a_LOSING_delivery_is_TIMED_not_merely_dropped() {
        let mut s = SeenTx::new(64);
        assert_eq!(
            s.first_delivery_at("0xaa", 1_000_000), None, "the first delivery WINS"
        );
        assert_eq!(
            s.first_delivery_at("0xaa", 4_000_000), Some(3_000_000),
            "a loser must report how far behind it arrived"
        );
        assert_eq!(
            s.first_delivery_at("0xaa", 51_000_000), Some(50_000_000),
            "always measured against the winner, not the last arrival"
        );
    }
    #[test]
    fn a_BACKWARD_stamp_reads_as_zero_rather_than_a_wild_negative() {
        let mut s = SeenTx::new(8);
        assert_eq!(s.first_delivery_at("0xbb", 5_000_000), None);
        assert_eq!(
            s.first_delivery_at("0xbb", 1_000_000), Some(0), "saturating, never negative"
        );
    }
    #[test]
    fn the_plain_first_delivery_still_behaves_exactly_as_before() {
        let mut s = SeenTx::new(4);
        assert!(s.first_delivery("0x1"), "first time is true");
        assert!(! s.first_delivery("0x1"), "every redelivery is false");
        assert!(s.first_delivery("0x2"));
    }
    #[test]
    fn eviction_forgets_the_winning_stamp_TOO() {
        let mut s = SeenTx::new(2);
        s.first_delivery_at("0xa", 1_000);
        s.first_delivery_at("0xb", 2_000);
        s.first_delivery_at("0xc", 3_000);
        assert_eq!(
            s.won_at_ns.len(), 2, "stamps are bounded by the cap: {:?}", s.won_at_ns
        );
        assert!(
            ! s.won_at_ns.contains_key("0xa"), "the evicted hash left no stamp behind"
        );
    }
    #[test]
    fn the_tally_buckets_by_MAGNITUDE_and_resets_each_window() {
        let mut t = BehindTally::default();
        t.record("fast", 500_000);
        t.record("fast", 900_000);
        t.record("slow", 50_000_000);
        t.record("slow", 250_000_000);
        let j = t.drain_json();
        assert_eq!(
            j["fast"] ["le_1ms"], 2, "sub-ms arrivals land in the tightest bucket"
        );
        assert_eq!(j["slow"] ["le_100ms"], 1);
        assert_eq!(j["slow"] ["over_100ms"], 1);
        assert!((j["slow"] ["max_ms"].as_f64().unwrap() - 250.0).abs() < 0.5);
        assert!(t.is_empty(), "drain must reset the window");
        assert_eq!(t.drain_json().as_object().unwrap().len(), 0);
    }
    #[test]
    fn eviction_is_bounded_and_oldest_first() {
        let mut s = SeenTx::new(3);
        for h in ["a", "b", "c"] {
            assert!(s.first_delivery(h));
        }
        assert!(! s.first_delivery("a"), "still remembered while resident");
        assert!(s.first_delivery("d"));
        assert_eq!(s.len(), 3, "memory stays bounded");
        assert!(s.first_delivery("a"), "evicted long after the mempool dropped it");
    }
}
