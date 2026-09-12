use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
#[derive(Debug, Clone)]
pub struct KnownOrder {
    pub token: String,
    pub side: u8,
    pub price: f64,
    pub remaining: f64,
    pub learned: Instant,
    pub book_size_at_learn: f64,
}
#[derive(Debug, Clone, Copy)]
pub struct Belief {
    pub price: f64,
    pub remaining: f64,
    pub level_size: Option<f64>,
    pub contested: bool,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub token: String,
    pub side: u8,
    pub price: f64,
    pub size: f64,
    pub confidence: Conf,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Conf {
    Uncontested,
    Contested,
}
pub const MAX_BELIEF_AGE: Duration = Duration::from_secs(300);
pub const CONTEST_TOLERANCE: f64 = 0.10;
#[derive(Default)]
pub struct Fingerprints {
    known: Mutex<HashMap<String, KnownOrder>>,
    last_size: Mutex<HashMap<String, f64>>,
    pub emitted: Mutex<u64>,
    pub abstained_contested: Mutex<u64>,
    pub abstained_stale: Mutex<u64>,
}
impl Fingerprints {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn learn(
        &self,
        token: &str,
        side: u8,
        price: f64,
        remaining: f64,
        book_size_now: f64,
    ) {
        if remaining <= 0.0 {
            self.forget(token);
            return;
        }
        self.known
            .lock()
            .unwrap()
            .insert(
                token.into(),
                KnownOrder {
                    token: token.into(),
                    side,
                    price,
                    remaining,
                    learned: Instant::now(),
                    book_size_at_learn: book_size_now,
                },
            );
    }
    pub fn forget(&self, token: &str) {
        self.known.lock().unwrap().remove(token);
        self.last_size.lock().unwrap().remove(token);
    }
    pub fn known_len(&self) -> usize {
        self.known.lock().unwrap().len()
    }
    pub fn known_tokens(&self) -> Vec<String> {
        self.known.lock().unwrap().keys().cloned().collect()
    }
    pub fn belief(&self, token: &str) -> Option<Belief> {
        let known = self.known.lock().unwrap();
        let k = known.get(token)?;
        let last = self.last_size.lock().unwrap().get(token).copied();
        let contested = match last {
            Some(sz) => (sz - k.remaining) > k.remaining * CONTEST_TOLERANCE,
            None => false,
        };
        Some(Belief {
            price: k.price,
            remaining: k.remaining,
            level_size: last,
            contested,
        })
    }
    pub fn on_book(
        &self,
        token: &str,
        price: f64,
        size_at_level: f64,
    ) -> Option<Candidate> {
        let mut known = self.known.lock().unwrap();
        let k = known.get_mut(token)?;
        if (k.price - price).abs() > 1e-9 {
            return None;
        }
        if k.learned.elapsed() > MAX_BELIEF_AGE {
            known.remove(token);
            *self.abstained_stale.lock().unwrap() += 1;
            return None;
        }
        let mut last = self.last_size.lock().unwrap();
        let prev = *last.get(token).unwrap_or(&k.book_size_at_learn);
        last.insert(token.into(), size_at_level);
        let delta = prev - size_at_level;
        if delta <= 1e-9 {
            return None;
        }
        if delta > k.remaining * 1.001 {
            return None;
        }
        let excess = prev - k.remaining;
        let conf = if excess > k.remaining * CONTEST_TOLERANCE {
            *self.abstained_contested.lock().unwrap() += 1;
            Conf::Contested
        } else {
            Conf::Uncontested
        };
        k.remaining = (k.remaining - delta).max(0.0);
        let exhausted = k.remaining <= 1e-9;
        let (side, kprice) = (k.side, k.price);
        if exhausted {
            known.remove(token);
        }
        let _ = (side, kprice);
        *self.emitted.lock().unwrap() += 1;
        Some(Candidate {
            token: token.into(),
            side,
            price,
            size: delta,
            confidence: conf,
        })
    }
}
#[derive(Default)]
pub struct FingerprintScore {
    pending: Mutex<Vec<(Instant, Candidate)>>,
    pub agree: Mutex<u64>,
    pub disagree: Mutex<u64>,
    pub unconfirmed: Mutex<u64>,
    pub leads_ms: Mutex<Vec<f64>>,
}
impl FingerprintScore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn saw(&self, c: Candidate) {
        self.pending.lock().unwrap().push((Instant::now(), c));
    }
    pub fn confirm(&self, token: &str, side: u8, price: f64, size: f64) {
        let mut p = self.pending.lock().unwrap();
        if let Some(ix) = p
            .iter()
            .position(|(_, c)| {
                c.token == token && c.side == side && (c.price - price).abs() < 0.002
                    && (c.size - size).abs() < size.max(1.0) * 0.05
            })
        {
            let (t, _) = p.remove(ix);
            *self.agree.lock().unwrap() += 1;
            self.leads_ms.lock().unwrap().push(t.elapsed().as_secs_f64() * 1000.0);
        } else if p.iter().any(|(_, c)| c.token == token && c.side == side) {
            *self.disagree.lock().unwrap() += 1;
        }
    }
    pub fn expire(&self, older_than: Duration) -> usize {
        let mut p = self.pending.lock().unwrap();
        let before = p.len();
        p.retain(|(t, _)| t.elapsed() < older_than);
        let n = before - p.len();
        *self.unconfirmed.lock().unwrap() += n as u64;
        n
    }
    pub fn verdict(&self, min_samples: u64) -> serde_json::Value {
        let a = *self.agree.lock().unwrap();
        let d = *self.disagree.lock().unwrap();
        let u = *self.unconfirmed.lock().unwrap();
        let mut l = self.leads_ms.lock().unwrap().clone();
        l.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let med = if l.is_empty() { 0.0 } else { l[l.len() / 2] };
        let ghost_rate = if a + u == 0 { 0.0 } else { u as f64 / (a + u) as f64 };
        let ready = d == 0 && a >= min_samples && ghost_rate < 0.02 && med > 50.0;
        serde_json::json!(
            { "agree" : a, "disagree" : d, "unconfirmed" : u, "ghost_rate" : (ghost_rate
            * 1000.0).round() / 1000.0, "median_lead_ms" : (med * 10.0).round() / 10.0,
            "READY_TO_PROMOTE" : ready, "blocker" : if d > 0 {
            "DISAGREED — do not promote" } else if a < min_samples {
            "needs more confirmed fills" } else if ghost_rate >= 0.02 {
            "too many unconfirmed ghosts" } else if med <= 50.0 {
            "lead too small to be worth the risk" } else { "none" }, }
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fp() -> Fingerprints {
        Fingerprints::new()
    }
    #[test]
    fn an_unlearned_token_never_produces_a_candidate() {
        assert!(fp().on_book("T", 0.62, 100.0).is_none());
    }
    #[test]
    fn a_decrement_at_his_level_is_a_candidate() {
        let f = fp();
        f.learn("T", 0, 0.62, 5000.0, 5000.0);
        let c = f.on_book("T", 0.62, 4600.0).unwrap();
        assert!((c.size - 400.0).abs() < 1e-9);
        assert_eq!(c.confidence, Conf::Uncontested);
    }
    #[test]
    fn an_update_at_a_different_price_is_ignored() {
        let f = fp();
        f.learn("T", 0, 0.62, 5000.0, 5000.0);
        assert!(f.on_book("T", 0.61, 10.0).is_none());
    }
    #[test]
    fn liquidity_being_ADDED_is_not_a_fill() {
        let f = fp();
        f.learn("T", 0, 0.62, 5000.0, 5000.0);
        assert!(f.on_book("T", 0.62, 6000.0).is_none());
    }
    #[test]
    fn a_decrement_larger_than_his_order_is_refused() {
        let f = fp();
        f.learn("T", 0, 0.62, 100.0, 100.0);
        assert!(
            f.on_book("T", 0.62, - 900.0).is_none(), "cannot fill more than he rests"
        );
    }
    #[test]
    fn a_contested_level_is_flagged_not_trusted() {
        let f = fp();
        f.learn("T", 0, 0.62, 1000.0, 5000.0);
        let c = f.on_book("T", 0.62, 4500.0).unwrap();
        assert_eq!(
            c.confidence, Conf::Contested, "a decrement on a shared level may not be his"
        );
    }
    #[test]
    fn a_stale_belief_is_dropped_because_cancels_are_invisible() {
        let f = fp();
        f.learn("T", 0, 0.62, 5000.0, 5000.0);
        f.known.lock().unwrap().get_mut("T").unwrap().learned = Instant::now()
            - Duration::from_secs(600);
        assert!(f.on_book("T", 0.62, 4000.0).is_none());
        assert_eq!(* f.abstained_stale.lock().unwrap(), 1);
    }
    #[test]
    fn his_order_is_forgotten_once_fully_consumed() {
        let f = fp();
        f.learn("T", 0, 0.62, 100.0, 100.0);
        f.on_book("T", 0.62, 0.0).unwrap();
        assert_eq!(f.known_len(), 0);
        assert!(f.on_book("T", 0.62, 0.0).is_none());
    }
    #[test]
    fn a_confirmed_candidate_agrees() {
        let s = FingerprintScore::new();
        s.saw(Candidate {
            token: "T".into(),
            side: 0,
            price: 0.62,
            size: 400.0,
            confidence: Conf::Uncontested,
        });
        s.confirm("T", 0, 0.62, 400.0);
        assert_eq!(* s.agree.lock().unwrap(), 1);
    }
    #[test]
    fn a_mismatched_confirmation_is_a_DISAGREEMENT() {
        let s = FingerprintScore::new();
        s.saw(Candidate {
            token: "T".into(),
            side: 0,
            price: 0.62,
            size: 400.0,
            confidence: Conf::Uncontested,
        });
        s.confirm("T", 0, 0.62, 9999.0);
        assert_eq!(* s.disagree.lock().unwrap(), 1);
    }
    #[test]
    fn one_disagreement_blocks_promotion_permanently() {
        let s = FingerprintScore::new();
        for _ in 0..100 {
            s.saw(Candidate {
                token: "T".into(),
                side: 0,
                price: 0.62,
                size: 400.0,
                confidence: Conf::Uncontested,
            });
            s.confirm("T", 0, 0.62, 400.0);
        }
        s.saw(Candidate {
            token: "T".into(),
            side: 0,
            price: 0.62,
            size: 400.0,
            confidence: Conf::Uncontested,
        });
        s.confirm("T", 0, 0.62, 1.0);
        let v = s.verdict(20);
        assert_eq!(v["READY_TO_PROMOTE"], serde_json::json!(false));
        assert!(v["blocker"].as_str().unwrap().contains("DISAGREED"));
    }
    #[test]
    fn ghosts_block_promotion_even_with_zero_disagreements() {
        let s = FingerprintScore::new();
        for _ in 0..50 {
            s.saw(Candidate {
                token: "T".into(),
                side: 0,
                price: 0.62,
                size: 400.0,
                confidence: Conf::Uncontested,
            });
            s.confirm("T", 0, 0.62, 400.0);
        }
        for _ in 0..10 {
            s.saw(Candidate {
                token: "G".into(),
                side: 0,
                price: 0.5,
                size: 1.0,
                confidence: Conf::Uncontested,
            });
        }
        s.expire(Duration::from_millis(0));
        let v = s.verdict(20);
        assert_eq!(v["READY_TO_PROMOTE"], serde_json::json!(false));
        assert!(v["blocker"].as_str().unwrap().contains("ghost"));
    }
}
