use std::collections::HashMap;
use std::sync::Mutex;
use crate::order::Order;
#[derive(Clone)]
pub struct Presigned {
    pub order: Order,
    pub signature: String,
}
#[derive(Default)]
pub struct PresignCache {
    by_token: Mutex<HashMap<String, Presigned>>,
    pub hits: Mutex<u64>,
    pub misses: Mutex<u64>,
    pub rejected: Mutex<u64>,
}
impl PresignCache {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn put(&self, p: Presigned) {
        self.by_token.lock().unwrap().insert(p.order.token_id.clone(), p);
    }
    pub fn forget(&self, token: &str) {
        self.by_token.lock().unwrap().remove(token);
    }
    pub fn len(&self) -> usize {
        self.by_token.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn take_if_exact(&self, want: &Order) -> Option<String> {
        let mut m = self.by_token.lock().unwrap();
        let Some(p) = m.get(&want.token_id) else {
            *self.misses.lock().unwrap() += 1;
            return None;
        };
        let o = &p.order;
        let exact = o.token_id == want.token_id && o.side == want.side
            && o.maker_amount == want.maker_amount && o.taker_amount == want.taker_amount
            && o.salt == want.salt && o.timestamp == want.timestamp
            && o.neg_risk == want.neg_risk && o.signature_type == want.signature_type
            && o.maker == want.maker && o.signer == want.signer;
        if !exact {
            *self.rejected.lock().unwrap() += 1;
            m.remove(&want.token_id);
            return None;
        }
        let sig = p.signature.clone();
        m.remove(&want.token_id);
        *self.hits.lock().unwrap() += 1;
        Some(sig)
    }
}
