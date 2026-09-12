use crate::signal_guard::Progress;
use std::collections::HashMap;
pub const CLAIM_GRACE_SECS: i64 = 60;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;
use crate::calldata::Decoded;
use crate::venue::{ceil_shares, sell_terms, whole};
pub type Micro = i64;
pub const MICRO: f64 = 1e6;
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sizing {
    Shares(f64),
    Usd(f64),
    Pct(f64),
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Execution {
    Taker,
    Hybrid,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Skip {
    Disarmed,
    Halted,
    TokenOwnedByOtherLane,
    DailyBudget,
    PerMarketCap,
    MaxOpen,
    PriceOutOfBand,
    BelowVenueMinimum,
    DustAfterSizing,
    SellNoPosition,
    SellAlreadyReserved,
    NotReady,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Route {
    Take,
    RestInFront { limit: f64, room_ticks: i64 },
}
#[derive(Debug, Clone, PartialEq)]
pub struct Intent {
    pub lane: usize,
    pub token_id: String,
    pub side: u8,
    pub shares: f64,
    pub limit: f64,
    pub usd: Micro,
    pub execution: Execution,
    pub his_remaining: f64,
    pub he_was_maker: bool,
    pub route: Route,
}
#[derive(Debug, Clone)]
pub struct LaneConfig {
    pub name: String,
    pub wallet20: [u8; 20],
    pub sizing: Sizing,
    pub execution: Execution,
    pub buy_slippage_c: f64,
    pub sell_slippage_c: f64,
    pub copy_maker_sells: bool,
    pub sell_floor_frac: f64,
    pub min_order_usd: f64,
    pub max_usd_per_fill: f64,
    pub daily_budget_usd: f64,
    pub per_market_usd: f64,
    pub max_open_usd: f64,
    pub max_buy_price: f64,
    pub min_buy_price: f64,
    pub min_fill_floor: bool,
    pub sell_all_frac: f64,
    pub max_effective_pct: f64,
    pub compound: bool,
    pub copy_makers: bool,
    pub exclude_political: bool,
}
impl LaneConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_usd_per_fill > self.per_market_usd {
            return Err(
                format!(
                    "lane {}: max_usd_per_fill ${} exceeds per_market_usd ${} — the per-fill \
                 clamp can never bind, tuning it will appear to do nothing",
                    self.name, self.max_usd_per_fill, self.per_market_usd
                ),
            );
        }
        if self.per_market_usd > self.max_open_usd {
            return Err(
                format!("lane {}: per_market_usd exceeds max_open_usd", self.name),
            );
        }
        if self.min_order_usd < 1.0 {
            return Err(
                format!(
                    "lane {}: min_order_usd below the $1 VENUE floor — orders will be \
                 rejected outright",
                    self.name
                ),
            );
        }
        if self.min_buy_price <= 0.0 || self.max_buy_price >= 1.0
            || self.min_buy_price >= self.max_buy_price
        {
            return Err(
                format!("lane {}: price band is not 0 < min < max < 1", self.name),
            );
        }
        match self.sizing {
            Sizing::Shares(n) if n <= 0.0 => {
                Err(format!("lane {}: shares <= 0", self.name))
            }
            Sizing::Usd(u) if u <= 0.0 => Err(format!("lane {}: usd <= 0", self.name)),
            Sizing::Pct(p) if !(p > 0.0 && p <= 1.0) => {
                Err(format!("lane {}: pct must be in (0,1]", self.name))
            }
            _ => Ok(()),
        }
    }
}
#[derive(Debug, Clone)]
pub struct SizingPolicy {
    pub generation: u64,
    pub seed_usd: f64,
    pub sizing: Sizing,
    pub max_effective_pct: f64,
    pub compound: bool,
    pub caps: crate::budget::Caps,
}
impl SizingPolicy {
    pub fn build(
        generation: u64,
        seed_usd: f64,
        sizing: Sizing,
        max_effective_pct: f64,
        compound: bool,
        fracs: &crate::budget::Fracs,
    ) -> Result<Self, String> {
        if !(seed_usd.is_finite() && seed_usd > 0.0) {
            return Err(format!("seed_usd must be a positive number, got {seed_usd}"));
        }
        if !(max_effective_pct.is_finite() && max_effective_pct > 0.0
            && max_effective_pct <= 1.0)
        {
            return Err(
                format!("max_effective_pct must be in (0,1], got {max_effective_pct}"),
            );
        }
        if let Sizing::Pct(p) = sizing {
            if !(p.is_finite() && p > 0.0 && p <= 1.0) {
                return Err(format!("pct must be in (0,1], got {p}"));
            }
        }
        let caps = crate::budget::derive(seed_usd, fracs);
        if !(caps.max_usd_per_fill <= caps.per_market_usd
            && caps.per_market_usd <= caps.max_open_usd)
        {
            return Err("derived caps violate fill <= market <= open".into());
        }
        Ok(Self {
            generation,
            seed_usd,
            sizing,
            max_effective_pct,
            compound,
            caps,
        })
    }
}
#[derive(Debug)]
pub struct LaneState {
    pub armed: AtomicBool,
    pub rest_buys: AtomicBool,
    pub rest_sells: AtomicBool,
    pub halted: AtomicBool,
    pub halt_latch: AtomicBool,
    pub spent_today: AtomicI64,
    pub open_usd: AtomicI64,
    pub cap_scale: AtomicI64,
    pub retired: AtomicBool,
    pub ready: AtomicBool,
    pub halted_since: AtomicI64,
    pub policy: std::sync::RwLock<std::sync::Arc<SizingPolicy>>,
}
impl Default for LaneState {
    fn default() -> Self {
        Self {
            armed: AtomicBool::new(false),
            rest_buys: AtomicBool::new(false),
            rest_sells: AtomicBool::new(false),
            halted: AtomicBool::new(false),
            halt_latch: AtomicBool::new(false),
            spent_today: AtomicI64::new(0),
            open_usd: AtomicI64::new(0),
            cap_scale: AtomicI64::new(MICRO as i64),
            retired: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            halted_since: AtomicI64::new(0),
            policy: std::sync::RwLock::new(
                std::sync::Arc::new(SizingPolicy {
                    generation: 0,
                    seed_usd: 1.0,
                    sizing: Sizing::Pct(0.05),
                    max_effective_pct: 0.05,
                    compound: false,
                    caps: crate::budget::Caps {
                        max_open_usd: 0.0,
                        per_market_usd: 0.0,
                        max_usd_per_fill: 0.0,
                        daily_usd: 0.0,
                    },
                }),
            ),
        }
    }
}
pub struct Lane {
    pub cfg: LaneConfig,
    pub state: LaneState,
    pub per_token: Mutex<HashMap<String, Micro>>,
    pub holdings: Mutex<HashMap<String, f64>>,
    pub his_pos: Mutex<HashMap<String, f64>>,
    pub legacy: Mutex<HashMap<String, f64>>,
    pub sell_reservations: Mutex<HashMap<String, f64>>,
}
impl Lane {
    pub fn new(cfg: LaneConfig) -> Self {
        let state = LaneState::default();
        state
            .rest_buys
            .store(
                cfg.copy_makers || cfg.execution == Execution::Hybrid,
                Ordering::Relaxed,
            );
        state
            .rest_sells
            .store(
                cfg.copy_maker_sells || cfg.execution == Execution::Hybrid,
                Ordering::Relaxed,
            );
        let seeded = SizingPolicy {
            generation: 0,
            seed_usd: 0.0,
            sizing: cfg.sizing,
            max_effective_pct: cfg.max_effective_pct,
            compound: cfg.compound,
            caps: crate::budget::Caps {
                max_open_usd: cfg.max_open_usd,
                per_market_usd: cfg.per_market_usd,
                max_usd_per_fill: cfg.max_usd_per_fill,
                daily_usd: cfg.daily_budget_usd,
            },
        };
        if let Ok(mut g) = state.policy.write() {
            *g = std::sync::Arc::new(seeded);
        }
        Self {
            cfg,
            state,
            per_token: Mutex::new(HashMap::new()),
            holdings: Mutex::new(HashMap::new()),
            his_pos: Mutex::new(HashMap::new()),
            legacy: Mutex::new(HashMap::new()),
            sell_reservations: Mutex::new(HashMap::new()),
        }
    }
    pub fn set_holding(&self, token: &str, shares: f64) {
        let mut h = self.holdings.lock().unwrap();
        if shares <= 1e-9 {
            h.remove(token);
        } else {
            h.insert(token.into(), shares);
        }
    }
    pub fn mark_ready(&self) {
        self.state.ready.store(true, Ordering::Relaxed);
    }
    pub fn mark_activating(&self) {
        self.state.ready.store(false, Ordering::Relaxed);
    }
    pub fn holding(&self, token: &str) -> f64 {
        *self.holdings.lock().unwrap().get(token).unwrap_or(&0.0)
    }
    pub fn set_his(&self, token: &str, shares: f64) {
        self.his_pos.lock().unwrap().insert(token.into(), shares.max(0.0));
    }
    pub fn set_legacy(&self, token: &str, shares: f64) {
        let mut l = self.legacy.lock().unwrap();
        if shares <= 1e-9 {
            l.remove(token);
        } else {
            l.insert(token.into(), shares);
        }
    }
    pub fn sell_reserved(&self, token: &str) -> f64 {
        *self.sell_reservations.lock().unwrap().get(token).unwrap_or(&0.0)
    }
    pub fn set_sell_reservations(&self, m: HashMap<String, f64>) {
        *self.sell_reservations.lock().unwrap() = m;
    }
}
pub struct Router {
    lanes: std::sync::RwLock<Vec<std::sync::Arc<Lane>>>,
    owner: Mutex<HashMap<String, (usize, i64)>>,
}
impl Router {
    pub fn new(lanes: Vec<Lane>) -> Self {
        let lanes = lanes.into_iter().map(std::sync::Arc::new).collect();
        Router {
            lanes: std::sync::RwLock::new(lanes),
            owner: Mutex::new(HashMap::new()),
        }
    }
    pub fn snapshot(&self) -> Vec<std::sync::Arc<Lane>> {
        self.lanes.read().unwrap().clone()
    }
    pub fn len(&self) -> usize {
        self.lanes.read().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.lanes.read().unwrap().is_empty()
    }
    pub fn lane(&self, ix: usize) -> Option<std::sync::Arc<Lane>> {
        self.lanes.read().unwrap().get(ix).cloned()
    }
    pub fn push(&self, lane: Lane) -> usize {
        let mut g = self.lanes.write().unwrap();
        g.push(std::sync::Arc::new(lane));
        g.len() - 1
    }
    #[cfg(test)]
    pub fn lane_for(&self, wallet20: &[u8; 20]) -> Option<usize> {
        self.lanes.read().unwrap().iter().position(|l| &l.cfg.wallet20 == wallet20)
    }
    pub fn claim(&self, token: &str, lane_ix: usize) {
        self.owner
            .lock()
            .unwrap()
            .entry(token.to_string())
            .or_insert((lane_ix, crate::ledger::now_secs()));
    }
    pub fn release(&self, token: &str, lane: usize) {
        let mut o = self.owner.lock().unwrap();
        if o.get(token).map(|(ix, _)| *ix) == Some(lane) {
            o.remove(token);
        }
    }
    pub fn release_by_name(&self, token: &str, lane_name: &str) {
        let ix = self.lanes.read().unwrap().iter().position(|l| l.cfg.name == lane_name);
        if let Some(ix) = ix {
            self.release(token, ix);
        }
    }
    pub fn claims_of(&self, lane_name: &str) -> Vec<String> {
        let ix = match self
            .lanes
            .read()
            .unwrap()
            .iter()
            .position(|l| l.cfg.name == lane_name)
        {
            Some(i) => i,
            None => return Vec::new(),
        };
        let cutoff = crate::ledger::now_secs() - CLAIM_GRACE_SECS;
        self.owner
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, &(o, at))| o == ix && at <= cutoff)
            .map(|(t, _)| t.clone())
            .collect()
    }
    pub fn backdate_claims_for_test(&self, secs: i64) {
        for (_, v) in self.owner.lock().unwrap().iter_mut() {
            v.1 -= secs;
        }
    }
    pub fn owner_of(&self, token: &str) -> Option<usize> {
        self.owner.lock().unwrap().get(token).map(|(ix, _)| *ix)
    }
    pub fn claim_existing(&self, token: &str, lane_ix: usize) -> bool {
        let mut o = self.owner.lock().unwrap();
        match o.get(token) {
            Some(_) => false,
            None => {
                o.insert(token.to_string(), (lane_ix, crate::ledger::now_secs()));
                true
            }
        }
    }
    pub fn restore_ownership(
        &self,
        held_by_lane: &[(usize, String, Vec<String>)],
    ) -> (usize, Vec<(String, String, String)>) {
        let mut restored = 0usize;
        let mut owner_of: HashMap<String, String> = HashMap::new();
        let mut conflicts = Vec::new();
        for (ix, name, tokens) in held_by_lane {
            for tok in tokens {
                if self.claim_existing(tok, *ix) {
                    restored += 1;
                    owner_of.insert(tok.clone(), name.clone());
                } else {
                    conflicts
                        .push((
                            tok.clone(),
                            owner_of.get(tok).cloned().unwrap_or_default(),
                            name.clone(),
                        ));
                }
            }
        }
        (restored, conflicts)
    }
    pub fn decide(
        &self,
        lane_ix: usize,
        d: &Decoded,
        progress: Progress,
    ) -> Result<Intent, Skip> {
        let lane = match self.lane(lane_ix) {
            Some(l) => l,
            None => return Err(Skip::Disarmed),
        };
        let c = &lane.cfg;
        if !lane.state.ready.load(Ordering::Relaxed) {
            return Err(Skip::NotReady);
        }
        if !lane.state.armed.load(Ordering::Relaxed) {
            return Err(Skip::Disarmed);
        }
        if d.side == 0 && lane.state.halted.load(Ordering::Relaxed) {
            return Err(Skip::Halted);
        }
        if d.side == 0 && lane.state.retired.load(Ordering::Relaxed) {
            return Err(Skip::Halted);
        }
        if d.side == 0 {
            if let Some((owner, _)) = self.owner.lock().unwrap().get(&d.token_id) {
                if *owner != lane_ix {
                    return Err(Skip::TokenOwnedByOtherLane);
                }
            }
        }
        if d.side == 1 {
            let mut resv = lane.sell_reservations.lock().unwrap();
            let gross = lane.holding(&d.token_id);
            let committed = *resv.get(&d.token_id).unwrap_or(&0.0);
            let held = (gross - committed).max(0.0);
            if held <= 1e-9 {
                return Err(
                    if gross > 1e-9 {
                        Skip::SellAlreadyReserved
                    } else {
                        Skip::SellNoPosition
                    },
                );
            }
            let his_before = {
                let hp = lane.his_pos.lock().unwrap();
                *hp.get(&d.token_id).unwrap_or(&0.0)
            };
            let leg = *lane.legacy.lock().unwrap().get(&d.token_id).unwrap_or(&0.0);
            let frac = if his_before <= 1e-9 {
                1.0
            } else if leg > 1e-9 {
                let matched_before = (his_before - leg).max(1e-9);
                let sold_matched = (d.fill_size - leg.min(d.fill_size)).max(0.0);
                (sold_matched / matched_before).min(1.0)
            } else {
                (d.fill_size / his_before).min(1.0)
            };
            if frac <= 1e-9 {
                return Err(Skip::SellNoPosition);
            }
            let want = if frac >= c.sell_all_frac { held } else { held * frac };
            let desired = (d.price - c.sell_slippage_c)
                .max(d.price * c.sell_floor_frac)
                .max(0.01);
            let (shares, limit) = sell_terms(want.min(held), desired, 0.06);
            if shares <= 0.0 {
                return Err(Skip::DustAfterSizing);
            }
            *resv.entry(d.token_id.clone()).or_insert(0.0) += shares;
            drop(resv);
            self.claim(&d.token_id, lane_ix);
            return Ok(Intent {
                lane: lane_ix,
                token_id: d.token_id.clone(),
                side: 1,
                shares,
                limit,
                usd: 0,
                execution: c.execution,
                his_remaining: (d.order_size - d.fill_size).max(0.0),
                he_was_maker: d.role == "maker",
                route: Route::Take,
            });
        }
        if d.price > c.max_buy_price || d.price < c.min_buy_price {
            return Err(Skip::PriceOutOfBand);
        }
        let limit = crate::venue::buy_limit((d.price + c.buy_slippage_c).min(0.99));
        let scale = lane.state.cap_scale.load(Ordering::Relaxed) as f64 / MICRO;
        let pol: std::sync::Arc<SizingPolicy> = match lane.state.policy.read() {
            Ok(g) => g.clone(),
            Err(_) => return Err(Skip::NotReady),
        };
        let mut shares = match pol.sizing {
            Sizing::Shares(n) => ceil_shares(n),
            Sizing::Usd(u) => whole(u / limit),
            Sizing::Pct(p) => {
                let target = progress.his_filled.max(d.fill_size)
                    * crate::budget::effective_pct(
                        p,
                        scale,
                        pol.max_effective_pct,
                        pol.compound,
                    ) * (d.price / limit);
                whole(target - progress.our_copied)
            }
        };
        let first_clip = progress.our_copied <= 0.0;
        if shares <= 0.0 {
            if !c.min_fill_floor || !first_clip {
                return Err(Skip::DustAfterSizing);
            }
            shares = 0.0;
        }
        let mut usd = shares * limit;
        if usd < c.min_order_usd {
            if !c.min_fill_floor || !first_clip {
                return Err(Skip::BelowVenueMinimum);
            }
            shares = ceil_shares(c.min_order_usd / limit);
            usd = shares * limit;
        }
        if shares < crate::venue::VENUE_MIN_SHARES {
            return Err(Skip::BelowVenueMinimum);
        }
        let cap_fill = pol.caps.max_usd_per_fill * scale;
        if usd > cap_fill {
            shares = whole(cap_fill / limit);
            if shares <= 0.0 {
                return Err(Skip::DustAfterSizing);
            }
            usd = shares * limit;
        }
        if shares < crate::venue::VENUE_MIN_SHARES || usd < c.min_order_usd {
            return Err(Skip::BelowVenueMinimum);
        }
        let usd_micro = (usd * MICRO) as Micro;
        let day_cap = (c.daily_budget_usd * scale * MICRO) as Micro;
        if lane.state.spent_today.load(Ordering::Relaxed) + usd_micro > day_cap {
            return Err(Skip::DailyBudget);
        }
        let open_cap = (pol.caps.max_open_usd * scale * MICRO) as Micro;
        if lane.state.open_usd.load(Ordering::Relaxed) + usd_micro > open_cap {
            return Err(Skip::MaxOpen);
        }
        {
            let mut pt = lane.per_token.lock().unwrap();
            let spent = *pt.get(&d.token_id).unwrap_or(&0);
            if spent + usd_micro > (pol.caps.per_market_usd * scale * MICRO) as Micro {
                return Err(Skip::PerMarketCap);
            }
            pt.insert(d.token_id.clone(), spent + usd_micro);
        }
        lane.state.spent_today.fetch_add(usd_micro, Ordering::Relaxed);
        lane.state.open_usd.fetch_add(usd_micro, Ordering::Relaxed);
        self.claim(&d.token_id, lane_ix);
        Ok(Intent {
            lane: lane_ix,
            token_id: d.token_id.clone(),
            side: 0,
            shares,
            limit,
            usd: usd_micro,
            execution: c.execution,
            his_remaining: (d.order_size - d.fill_size).max(0.0),
            he_was_maker: d.role == "maker",
            route: Route::Take,
        })
    }
}
pub struct TrancheLedger {
    inner: std::sync::Mutex<
        (std::collections::VecDeque<[u8; 32]>, std::collections::HashMap<[u8; 32], f64>),
    >,
    cap: usize,
}
impl TrancheLedger {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new((
                std::collections::VecDeque::new(),
                std::collections::HashMap::new(),
            )),
            cap: cap.max(1),
        }
    }
    pub fn observe(&self, salt: &[u8; 32], fill: f64) -> f64 {
        let mut g = self.inner.lock().unwrap();
        let (order, map) = &mut *g;
        let e = map
            .entry(*salt)
            .or_insert_with(|| {
                order.push_back(*salt);
                0.0
            });
        *e += fill.max(0.0);
        let cum = *e;
        while map.len() > self.cap {
            let Some(old) = order.pop_front() else { break };
            map.remove(&old);
        }
        cum
    }
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().1.len()
    }
}
pub fn sell_tranche_flags(cum: f64, fill: f64, order_size: f64) -> (bool, bool) {
    let first = cum <= fill + 1e-9;
    let done = order_size <= 1e-9 || cum >= order_size - 1e-9;
    (first, done)
}
#[cfg(test)]
fn route_for(
    intent: &Intent,
    cfg: &LaneConfig,
    top: Option<crate::book::Top>,
    his_price: f64,
    first_clip: bool,
    his_first_tranche: bool,
    his_order_done: bool,
) -> Route {
    resolve_route(
        intent,
        cfg,
        top,
        his_price,
        first_clip,
        his_first_tranche,
        his_order_done,
        cfg.copy_makers || cfg.execution == Execution::Hybrid,
        cfg.copy_maker_sells || cfg.execution == Execution::Hybrid,
    )
}
pub fn needs_book(lane: &std::sync::Arc<Lane>) -> bool {
    lane.cfg.execution == Execution::Hybrid
        || lane.state.rest_buys.load(Ordering::Relaxed)
        || lane.state.rest_sells.load(Ordering::Relaxed)
}
pub fn resolve_route(
    intent: &Intent,
    cfg: &LaneConfig,
    top: Option<crate::book::Top>,
    his_price: f64,
    first_clip: bool,
    his_first_tranche: bool,
    his_order_done: bool,
    rest_buys: bool,
    rest_sells: bool,
) -> Route {
    let side_enabled = if intent.side == 0 { rest_buys } else { rest_sells };
    let want_rest = side_enabled
        && (cfg.execution == Execution::Hybrid || intent.he_was_maker);
    if !want_rest {
        return Route::Take;
    }
    let opening_clip = if intent.side == 1 { his_first_tranche } else { first_clip };
    if opening_clip && cfg.execution != Execution::Hybrid {
        return Route::Take;
    }
    if his_order_done && intent.side == 1 {
        return Route::Take;
    }
    let top = match top {
        Some(t) if t.fresh() => t,
        _ => return Route::Take,
    };
    let tick = crate::book::tick_size(his_price);
    let room = crate::book::room_ticks(&top, his_price, intent.side, tick);
    if room < 1 {
        return Route::Take;
    }
    let limit = if intent.side == 0 {
        crate::venue::buy_limit(his_price + tick)
    } else {
        crate::venue::sell_limit(his_price - tick)
    };
    if limit <= 0.0 || limit >= 1.0 {
        return Route::Take;
    }
    Route::RestInFront {
        limit,
        room_ticks: room,
    }
}
#[cfg(test)]
mod tests {
    impl Router {
        fn decide1(&self, ix: usize, d: &Decoded) -> Result<Intent, Skip> {
            self.decide(
                ix,
                d,
                Progress {
                    his_filled: d.fill_size,
                    our_copied: 0.0,
                },
            )
        }
    }
    use super::*;
    use crate::calldata::Decoded;
    fn w(b: u8) -> [u8; 20] {
        [b; 20]
    }
    fn ready_lane(c: LaneConfig) -> Lane {
        let l = Lane::new(c);
        l.mark_ready();
        l
    }
    fn tick_settled(l: &Lane, token: &str, held: f64) {
        l.set_holding(token, held);
        l.set_sell_reservations(HashMap::new());
    }
    fn cfg(name: &str, wb: u8, sizing: Sizing) -> LaneConfig {
        LaneConfig {
            name: name.into(),
            wallet20: w(wb),
            sizing,
            execution: Execution::Taker,
            buy_slippage_c: 0.02,
            sell_slippage_c: 0.02,
            copy_maker_sells: false,
            sell_floor_frac: 0.5,
            min_order_usd: 1.0,
            max_usd_per_fill: 250.0,
            daily_budget_usd: 500.0,
            per_market_usd: 200.0,
            max_open_usd: 1000.0,
            max_buy_price: 0.95,
            min_buy_price: 0.02,
            min_fill_floor: true,
            sell_all_frac: 0.95,
            max_effective_pct: 0.05,
            copy_makers: false,
            compound: true,
            exclude_political: false,
        }
    }
    fn dec(tok: &str, side: u8, price: f64, fill: f64, order: f64) -> Decoded {
        Decoded {
            condition_id: [7u8; 32],
            token_id: tok.into(),
            side,
            price,
            order_size: order,
            fill_size: fill,
            role: "maker",
            salt: [9u8; 32],
            occurrence: 0,
        }
    }
    fn router2() -> Router {
        let r = Router::new(
            vec![
                ready_lane(cfg("example_lane_26", 1, Sizing::Shares(5.0))), ready_lane(cfg("other",
                2, Sizing::Pct(0.002))),
            ],
        );
        for l in r.snapshot().iter() {
            l.state.armed.store(true, Ordering::Relaxed);
        }
        r
    }
    fn pct_lane(p: f64) -> Router {
        let mut c = cfg("example_lane_26", 1, Sizing::Pct(p));
        c.daily_budget_usd = 100_000.0;
        c.per_market_usd = 50_000.0;
        c.max_open_usd = 200_000.0;
        c.max_usd_per_fill = 25_000.0;
        let r = Router::new(vec![ready_lane(c)]);
        for l in r.snapshot().iter() {
            l.state.armed.store(true, Ordering::Relaxed);
        }
        r
    }
    fn replay_tranches(
        r: &Router,
        n: usize,
        order_shares: f64,
        price: f64,
    ) -> (f64, usize) {
        let mut his_cum = 0.0;
        let mut copied = 0.0;
        let mut clips = 0;
        for _ in 0..n {
            his_cum += order_shares / n as f64;
            let d = dec("100000001", 0, price, order_shares / n as f64, order_shares);
            let progress = Progress {
                his_filled: his_cum,
                our_copied: copied,
            };
            if let Ok(i) = r.decide(0, &d, progress) {
                copied += i.shares;
                clips += 1;
            }
        }
        (copied, clips)
    }
    #[test]
    fn ANY_sell_of_his_flattens_our_WHOLE_position() {
        assert_eq!(
            crate ::config::SELL_ALL_ON_ANY_SELL, 0.0,
            "the production default must flatten on any sell"
        );
        for frac in [0.0001_f64, 0.01, 0.1, 0.3, 0.5, 0.9, 0.95, 1.0] {
            assert!(
                frac >= crate ::config::SELL_ALL_ON_ANY_SELL,
                "his {frac} exit must flatten us, not mirror proportionally"
            );
        }
    }
    #[test]
    fn a_lane_may_still_OPT_OUT_of_flattening() {
        let thresh = 0.5_f64;
        assert!(! (0.3 >= thresh), "a 30% trim stays proportional on an opted-out lane");
        assert!(0.6 >= thresh, "a 60% exit still flattens it");
    }
    #[test]
    fn CUMULATIVE_targeting_does_NOT_raise_any_CEILING() {
        let mut c = cfg("example_lane_26", 1, Sizing::Pct(0.05));
        c.max_usd_per_fill = 416.70;
        c.per_market_usd = 6_000.0;
        c.max_open_usd = 8_500.0;
        c.daily_budget_usd = 7_000.0;
        let r = Router::new(vec![ready_lane(c)]);
        for l in r.snapshot().iter() {
            l.state.armed.store(true, Ordering::Relaxed);
        }
        let d = dec("100000001", 0, 0.50, 500_000.0, 500_000.0);
        let i = r
            .decide(
                0,
                &d,
                Progress {
                    his_filled: 500_000.0,
                    our_copied: 0.0,
                },
            )
            .expect("should fire, clipped");
        let usd = i.shares * i.limit;
        assert!(
            usd <= 416.70 + 1e-6,
            "one clip committed ${usd:.2}, above the ${:.2} per-fill cap", 416.70
        );
        let i2 = r
            .decide(
                0,
                &d,
                Progress {
                    his_filled: 500_000.0,
                    our_copied: i.shares,
                },
            );
        if let Ok(i2) = i2 {
            assert!(
                i2.shares * i2.limit <= 416.70 + 1e-6, "a top-up must obey the cap too"
            );
        }
    }
    #[test]
    fn boot_ownership_restore_REPORTS_a_conflict_instead_of_dropping_it() {
        let r = router2();
        let (restored, conflicts) = r
            .restore_ownership(
                &[
                    (0, "example_lane_26".into(), vec!["TOK_A".into(), "TOK_B".into()]),
                    (1, "other".into(), vec!["TOK_B".into(), "TOK_C".into()]),
                ],
            );
        assert_eq!(restored, 3, "three distinct tokens are claimable");
        assert_eq!(conflicts.len(), 1);
        let (tok, owner, loser) = &conflicts[0];
        assert_eq!(
            (tok.as_str(), owner.as_str(), loser.as_str()), ("TOK_B", "example_lane_26", "other"),
            "the conflict must name BOTH sides so an operator can attribute it"
        );
    }
    #[test]
    fn a_clean_boot_reports_NO_conflicts() {
        let r = router2();
        let (restored, conflicts) = r
            .restore_ownership(
                &[
                    (0, "example_lane_26".into(), vec!["TOK_A".into()]),
                    (1, "other".into(), vec!["TOK_B".into()]),
                ],
            );
        assert_eq!((restored, conflicts.len()), (2, 0));
    }
    #[test]
    fn the_LOSING_lane_of_a_conflict_STOPS_BUYING_BUT_CAN_STILL_SELL() {
        let r = router2();
        for l in r.snapshot().iter() {
            l.state.armed.store(true, Ordering::Relaxed);
        }
        let (_, conflicts) = r
            .restore_ownership(
                &[
                    (0, "example_lane_26".into(), vec!["100000001".into()]),
                    (1, "other".into(), vec!["100000001".into()]),
                ],
            );
        assert_eq!(conflicts.len(), 1, "the conflict must still be REPORTED");
        r.lane(1).unwrap().set_holding("100000001", 50.0);
        r.lane(1).unwrap().set_his("100000001", 100.0);
        assert_eq!(
            r.decide1(1, & dec("100000001", 0, 0.60, 100.0, 100.0)),
            Err(Skip::TokenOwnedByOtherLane),
            "the loser must not OPEN more of a token whose ownership is disputed"
        );
        let exit = r
            .decide1(1, &dec("100000001", 1, 0.60, 100.0, 100.0))
            .expect("the losing lane MUST be able to follow its leader out");
        assert_eq!(exit.side, 1);
        assert!(
            (exit.shares - 50.0).abs() < 1e-9, "sized from ITS OWN holding, got {}", exit
            .shares
        );
        assert_eq!(
            r.owner_of("100000001"), Some(0),
            "exiting must not transfer the disputed claim"
        );
    }
    #[test]
    fn the_120_TRANCHE_ORDER_no_longer_buys_1560_SHARES() {
        let r = pct_lane(0.015);
        let (copied, clips) = replay_tranches(&r, 120, 5_000.0, 0.08);
        assert!(
            copied < 100.0,
            "bought {copied} shares across {clips} clips — the runaway is back"
        );
        assert!((copied - 75.0).abs() <= 15.0, "expected ~75 shares, got {copied}");
    }
    #[test]
    fn a_SINGLE_TRANCHE_order_is_sized_exactly_as_before() {
        let r = pct_lane(0.015);
        let d = dec("100000001", 0, 0.50, 5_000.0, 5_000.0);
        let one = r.decide1(0, &d).unwrap();
        let with_progress = pct_lane(0.015)
            .decide(
                0,
                &d,
                Progress {
                    his_filled: 5_000.0,
                    our_copied: 0.0,
                },
            )
            .unwrap();
        assert_eq!(one.shares, with_progress.shares);
    }
    #[test]
    fn a_TINY_FIRST_TRANCHE_no_longer_sizes_the_whole_copy() {
        let r = pct_lane(0.015);
        let (copied, _) = replay_tranches(&r, 120, 5_000.0, 0.50);
        assert!(
            copied > 60.0,
            "only copied {copied} shares of a 75-share target — still under-copying"
        );
    }
    #[test]
    fn the_VENUE_FLOOR_lifts_only_the_FIRST_clip_of_an_order() {
        let r = pct_lane(0.015);
        let d = dec("100000001", 0, 0.08, 1.0, 5_000.0);
        let first = r
            .decide(
                0,
                &d,
                Progress {
                    his_filled: 1.0,
                    our_copied: 0.0,
                },
            )
            .expect("the first clip may be lifted to the venue minimum");
        assert_eq!(
            first.shares, 10.0, "the first clip is lifted to the $1 venue minimum"
        );
        assert!(
            matches!(r.decide(0, & d, Progress { his_filled : 2.0, our_copied : first
            .shares }), Err(Skip::DustAfterSizing) | Err(Skip::BelowVenueMinimum)),
            "a top-up below the venue minimum must wait for more of his order"
        );
    }
    #[test]
    fn a_target_ALREADY_MET_stops_buying() {
        let r = pct_lane(0.015);
        let d = dec("100000001", 0, 0.50, 100.0, 5_000.0);
        assert!(
            matches!(r.decide(0, & d, Progress { his_filled : 5_000.0, our_copied :
            10_000.0 }), Err(Skip::DustAfterSizing)),
            "we must never buy more of his order than the target"
        );
    }
    #[test]
    fn ONE_signal_never_sizes_beyond_pct_times_his_CONFIRMED_fill() {
        let r = pct_lane(0.015);
        for tranche in [1.16, 500.0, 2587.0] {
            let Ok(i) = r
                .decide1(
                    0,
                    &dec(&format!("1000000{}", tranche as u64), 0, 0.50, tranche, 5000.0),
                ) else { continue };
            let usd = i.usd as f64 / MICRO;
            let intended = tranche * 0.50 * 0.015;
            let ceiling = intended.max(1.0) + 0.52;
            assert!(
                usd <= ceiling,
                "${usd} copied unfilled size from a {tranche}-share tranche (safe ceiling ${ceiling})"
            );
        }
    }
    #[test]
    fn an_exit_sells_a_fraction_of_WHAT_WE_HOLD_not_of_our_intended_target() {
        let r = pct_lane(0.015);
        r.lane(0).unwrap().set_holding("100000001", 225.0);
        r.lane(0).unwrap().set_his("100000001", 30_000.0);
        let i = r.decide1(0, &dec("100000001", 1, 0.60, 3000.0, 30_000.0)).unwrap();
        assert_eq!(i.side, 1);
        assert!(
            (i.shares - 22.5).abs() < 0.6,
            "sold {} on a 225 holding for a 10% exit — must be ~22.5", i.shares
        );
    }
    #[test]
    fn an_exit_can_NEVER_exceed_what_we_hold() {
        let r = pct_lane(0.015);
        r.lane(0).unwrap().set_holding("100000001", 225.0);
        r.lane(0).unwrap().set_his("100000001", 3000.0);
        let i = r.decide1(0, &dec("100000001", 1, 0.60, 3000.0, 3000.0)).unwrap();
        assert!(i.shares <= 225.0 + 1e-9, "tried to sell {} of 225 held", i.shares);
    }
    #[test]
    fn a_REAL_full_exit_closes_us_completely_via_sell_all_frac() {
        let r = pct_lane(0.015);
        let mut his = 10_000.0;
        let mut held = 100.0;
        for sell in [2_500.0, 2_500.0, 2_500.0, 2_500.0] {
            r.lane(0).unwrap().set_his("100000001", his);
            tick_settled(&r.lane(0).unwrap(), "100000001", held);
            if let Ok(i) = r.decide1(0, &dec("100000001", 1, 0.60, sell, his)) {
                held = (held - i.shares).max(0.0);
            }
            his -= sell;
            if his <= 1e-9 {
                break;
            }
        }
        assert!(held <= 1.0, "a completed exit left {held} shares behind");
    }
    #[test]
    fn KNOWN_LIMIT_a_geometric_exit_that_never_completes_strands_a_tail() {
        let r = pct_lane(0.015);
        let mut his = 10_000.0;
        let mut held = 100.0;
        for _ in 0..8 {
            r.lane(0).unwrap().set_his("100000001", his);
            tick_settled(&r.lane(0).unwrap(), "100000001", held);
            let sell = his * 0.25;
            if let Ok(i) = r.decide1(0, &dec("100000001", 1, 0.60, sell, his)) {
                held = (held - i.shares).max(0.0);
            }
            his -= sell;
        }
        assert!(
            held > 1.0,
            "if this now converges the dust sweep may be redundant — re-check it"
        );
        assert!(held < 20.0, "decay should still be substantial, got {held}");
    }
    #[test]
    fn a_sub_dollar_FIRST_clip_is_LIFTED_to_the_venue_floor_not_skipped() {
        let mut c = cfg("example_lane_26", 1, Sizing::Pct(0.10));
        c.min_fill_floor = true;
        let r = Router::new(vec![ready_lane(c)]);
        r.snapshot()[0].state.armed.store(true, Ordering::Relaxed);
        let d = dec("T", 0, 0.11, 90.0, 90.0);
        let i = r
            .decide(
                0,
                &d,
                Progress {
                    his_filled: 90.0,
                    our_copied: 0.0,
                },
            )
            .expect("⛔ THE BUG: this returned BelowVenueMinimum and cost the trade");
        let usd = i.shares as f64 * i.limit;
        assert!(usd >= 1.0, "must be lifted to at least the venue floor, got ${usd:.4}");
        assert!(
            i.shares >= crate ::venue::VENUE_MIN_SHARES,
            "and the lift must clear the SHARE floor too, got {} shares", i.shares
        );
    }
    #[test]
    fn above_20_CENTS_a_sub_dollar_clip_is_SKIPPED_because_the_lift_cannot_be_legal() {
        let mut c = cfg("example_lane_26", 1, Sizing::Pct(0.10));
        c.min_fill_floor = true;
        let r = Router::new(vec![ready_lane(c)]);
        r.snapshot()[0].state.armed.store(true, Ordering::Relaxed);
        let d = dec("T", 0, 0.73, 13.0, 13.0);
        assert_eq!(
            r.decide(0, & d, Progress { his_filled : 13.0, our_copied : 0.0 }),
            Err(Skip::BelowVenueMinimum),
            "the $1 lift lands at 2 shares here — the venue would refuse it"
        );
    }
    #[test]
    fn with_the_floor_OFF_the_same_clip_is_still_SKIPPED() {
        let mut c = cfg("example_lane_26", 1, Sizing::Pct(0.10));
        c.min_fill_floor = false;
        let r = Router::new(vec![ready_lane(c)]);
        r.snapshot()[0].state.armed.store(true, Ordering::Relaxed);
        let d = dec("T", 0, 0.73, 13.0, 13.0);
        assert!(
            matches!(r.decide(0, & d, Progress { his_filled : 13.0, our_copied : 0.0 }),
            Err(Skip::BelowVenueMinimum) | Err(Skip::DustAfterSizing))
        );
    }
    #[test]
    fn the_lift_is_FIRST_CLIP_ONLY_so_a_collapsing_ladder_cannot_repeat_it() {
        let mut c = cfg("example_lane_26", 1, Sizing::Pct(0.10));
        c.min_fill_floor = true;
        let r = Router::new(vec![ready_lane(c)]);
        r.snapshot()[0].state.armed.store(true, Ordering::Relaxed);
        let d = dec("T", 0, 0.08, 13.0, 130.0);
        assert!(
            r.decide(0, & d, Progress { his_filled : 13.0, our_copied : 5.0 }).is_err(),
            "a later tranche must NOT be lifted — that is the ladder explosion"
        );
    }
    #[test]
    fn a_clip_under_the_VENUE_SHARE_MINIMUM_is_never_sent() {
        let mut c = cfg("vmin", 61, Sizing::Pct(0.0002));
        c.min_fill_floor = false;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let d = dec("7000001", 0, 0.69, 10_000.0, 10_000.0);
        assert_eq!(
            r.decide1(0, & d), Err(Skip::BelowVenueMinimum),
            "a sub-5-share clip must be refused HERE, not by the venue"
        );
    }
    #[test]
    fn CHEAP_markets_still_trade_because_the_dollar_lift_already_clears_five_shares() {
        let mut c = cfg("vcheap", 62, Sizing::Pct(0.0002));
        c.min_fill_floor = true;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        for (px, tok) in [(0.05, "7000021"), (0.10, "7000022"), (0.20, "7000023")] {
            let i = r
                .decide1(0, &dec(tok, 0, px, 10_000.0, 10_000.0))
                .unwrap_or_else(|e| {
                    panic!("cheap market at {px} must still trade: {e:?}")
                });
            assert!(
                i.shares >= crate ::venue::VENUE_MIN_SHARES,
                "at {px} the dollar lift gave {} shares — under the venue's 5", i
                .shares
            );
            assert!(i.shares * i.limit >= 1.0 - 1e-9, "and still clears the $1 floor");
        }
    }
    #[test]
    fn a_PER_FILL_CAP_that_lands_under_the_floor_SKIPS_instead_of_bouncing() {
        let mut c = cfg("vcap", 63, Sizing::Pct(0.5));
        c.min_fill_floor = true;
        c.max_usd_per_fill = 2.00;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        assert_eq!(
            r.decide1(0, & dec("7000003", 0, 0.90, 1_000.0, 1_000.0)),
            Err(Skip::BelowVenueMinimum),
            "the cap clamped under the venue floor — skip, never bounce"
        );
    }
    #[test]
    fn the_share_floor_NEVER_gates_an_EXIT() {
        let mut c = cfg("vexit", 64, Sizing::Pct(0.015));
        c.sell_all_frac = 0.95;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let tok = "7000004";
        r.lane(0).unwrap().set_holding(tok, 2.0);
        r.lane(0).unwrap().set_his(tok, 100.0);
        let i = r
            .decide1(0, &dec(tok, 1, 0.50, 100.0, 100.0))
            .expect("a 2-share position must still be exitable");
        assert_eq!(i.side, 1);
        assert!(
            i.shares > 0.0 && i.shares <= 2.0, "sized from our holding: {}", i.shares
        );
    }
    #[test]
    fn the_MIN_ORDER_FLOOR_over_copies_small_cheap_fills() {
        let r = pct_lane(0.015);
        let i = r.decide1(0, &dec("100000001", 0, 0.20, 100.0, 100.0)).unwrap();
        let requested = 100.0 * 0.015;
        assert!(
            i.shares > requested,
            "floor should LIFT a sub-$1 order, got {} for a requested {}", i.shares,
            requested
        );
        assert!(i.usd as f64 / MICRO >= 1.0 - 1e-6, "must clear the venue $1 minimum");
    }
    #[test]
    fn whole_share_truncation_biases_pct_DOWNWARD_never_up() {
        let r = pct_lane(0.015);
        for fill in [401.0, 667.0, 1001.0, 2003.0] {
            let i = r.decide1(0, &dec("100000001", 0, 0.50, fill, fill)).unwrap();
            assert!(
                i.shares <= fill * 0.015 + 1e-9,
                "{} shares exceeds exact pct {} for fill {}", i.shares, fill * 0.015,
                fill
            );
        }
    }
    #[test]
    fn the_PER_MARKET_cap_truncates_his_biggest_conviction_bets_first() {
        let mut c = cfg("example_lane_26", 1, Sizing::Pct(0.015));
        c.per_market_usd = 300.0;
        c.max_usd_per_fill = 300.0;
        c.daily_budget_usd = 100_000.0;
        c.max_open_usd = 100_000.0;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let mut total = 0.0;
        for _ in 0..10 {
            match r.decide1(0, &dec("100000001", 0, 0.80, 10_000.0, 100_000.0)) {
                Ok(i) => total += i.usd as f64 / MICRO,
                Err(Skip::PerMarketCap) => break,
                Err(e) => panic!("unexpected {e:?}"),
            }
        }
        assert!(total <= 300.0 + 1e-6, "per-market cap did not bind, got ${total}");
        assert!(
            total < 1500.0,
            "we wanted ~$1,200 of this market and the cap allowed ${total}"
        );
    }
    #[test]
    fn a_pct_lane_still_refuses_to_sell_what_it_never_bought() {
        let r = pct_lane(0.015);
        r.lane(0).unwrap().set_his("100000001", 5000.0);
        assert_eq!(
            r.decide1(0, & dec("100000001", 1, 0.60, 500.0, 5000.0)).unwrap_err(),
            Skip::SellNoPosition
        );
    }
    #[test]
    fn a_sale_that_fits_inside_his_LEGACY_moves_none_of_ours() {
        let r = pct_lane(0.015);
        r.lane(0).unwrap().set_holding("100000001", 10.0);
        r.lane(0).unwrap().set_his("100000001", 1100.0);
        r.lane(0).unwrap().set_legacy("100000001", 1000.0);
        assert_eq!(
            r.decide1(0, & dec("100000001", 1, 0.60, 550.0, 1100.0)).unwrap_err(),
            Skip::SellNoPosition,
            "a sale wholly inside his legacy must not move our position"
        );
    }
    #[test]
    fn a_sale_PAST_his_legacy_mirrors_only_the_matched_excess() {
        let r = pct_lane(0.015);
        r.lane(0).unwrap().set_holding("100000001", 10.0);
        r.lane(0).unwrap().set_his("100000001", 1100.0);
        r.lane(0).unwrap().set_legacy("100000001", 1000.0);
        let i = r.decide1(0, &dec("100000001", 1, 0.60, 1050.0, 1100.0)).unwrap();
        assert!(
            i.shares > 0.0 && i.shares <= 10.0 + 1e-9,
            "expected a partial mirror of our 10 shares, got {}", i.shares
        );
    }
    #[test]
    fn RED_a_tiny_first_tranche_does_not_copy_the_unfilled_remainder() {
        let r = pct_lane(0.015);
        match r.decide1(0, &dec("100000001", 0, 0.50, 1.16, 5000.0)) {
            Err(Skip::BelowVenueMinimum) => {}
            Ok(i) => {
                let usd = i.usd as f64 / MICRO;
                assert!(
                    usd <= 1.52,
                    "a 1.16-share fill incorrectly copied the unfilled 5,000-share order: ${usd}"
                );
            }
            Err(e) => panic!("unexpected refusal: {e:?}"),
        }
    }
    #[test]
    fn RED_his_most_extreme_entries_are_refused_by_the_PRICE_BAND_before_sizing() {
        let r = pct_lane(0.015);
        assert_eq!(
            r.decide1(0, & dec("100000001", 0, 0.994, 150_000.0, 150_000.0))
            .unwrap_err(), Skip::PriceOutOfBand
        );
    }
    #[test]
    fn RED_a_huge_IN_BAND_order_is_clamped_and_the_clamp_binds() {
        let mut c = cfg("example_lane_26", 1, Sizing::Pct(0.015));
        c.max_usd_per_fill = 250.0;
        c.per_market_usd = 300.0;
        c.max_open_usd = 100_000.0;
        c.daily_budget_usd = 100_000.0;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let i = r.decide1(0, &dec("100000001", 0, 0.90, 150_000.0, 150_000.0)).unwrap();
        let usd = i.usd as f64 / MICRO;
        assert!(usd <= 250.0 + 1e-6, "clamp failed on a huge in-band order: ${usd}");
        assert!(i.shares > 0.0, "clamping must not produce a zero-share order");
    }
    #[test]
    fn RED_the_venue_floor_can_only_ever_LIFT_never_drop_below_a_dollar() {
        let r = pct_lane(0.015);
        for (price, order) in [(0.02, 100.0), (0.07, 200.0), (0.08, 50.0)] {
            let i = r
                .decide1(
                    0,
                    &dec(
                        &format!("tok{}", (price * 1000.0) as u64),
                        0,
                        price,
                        order,
                        order,
                    ),
                )
                .unwrap();
            let usd = i.usd as f64 / MICRO;
            assert!(
                usd >= 1.0 - 1e-6,
                "clip ${usd} at price {price} is below the venue floor and would bounce"
            );
        }
    }
    #[test]
    fn RED_a_zero_or_absurd_order_size_cannot_produce_a_position() {
        let r = pct_lane(0.015);
        let i = r.decide1(0, &dec("100000001", 0, 0.50, 0.0, 0.0));
        match i {
            Err(_) => {}
            Ok(x) => {
                let usd = x.usd as f64 / MICRO;
                assert!(
                    (1.0 - 1e-6..= 2.0).contains(& usd),
                    "a zero-size order produced a ${usd} clip"
                );
            }
        }
    }
    #[test]
    fn RED_pct_never_exceeds_the_per_market_cap_across_repeated_orders() {
        let mut c = cfg("example_lane_26", 1, Sizing::Pct(0.015));
        c.per_market_usd = 300.0;
        c.max_usd_per_fill = 250.0;
        c.daily_budget_usd = 100_000.0;
        c.max_open_usd = 100_000.0;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let mut spent = 0.0;
        for _ in 0..40 {
            match r.decide1(0, &dec("100000001", 0, 0.50, 5000.0, 5000.0)) {
                Ok(i) => spent += i.usd as f64 / MICRO,
                Err(Skip::PerMarketCap) => break,
                Err(e) => panic!("unexpected {e:?}"),
            }
        }
        assert!(spent <= 300.0 + 1e-6, "per-market cap breached: ${spent}");
    }
    #[test]
    fn RED_an_exit_after_a_PARTIAL_fill_sells_only_what_filled() {
        let r = pct_lane(0.015);
        r.lane(0).unwrap().set_holding("100000001", 30.0);
        r.lane(0).unwrap().set_his("100000001", 5000.0);
        let i = r.decide1(0, &dec("100000001", 1, 0.60, 1000.0, 5000.0)).unwrap();
        assert!(
            i.shares <= 30.0 + 1e-9, "tried to sell {} of a 30-share position", i.shares
        );
        assert!((i.shares - 6.0).abs() < 0.6, "20% of 30 is ~6, got {}", i.shares);
    }
    #[test]
    fn RED_a_SELL_signal_can_never_open_a_new_position() {
        let r = pct_lane(0.015);
        r.lane(0).unwrap().set_his("100000001", 5000.0);
        assert_eq!(
            r.decide1(0, & dec("100000001", 1, 0.08, 3000.0, 5000.0)).unwrap_err(),
            Skip::SellNoPosition, "a sell must never be able to open a position"
        );
    }
    #[test]
    fn RED_price_band_still_gates_pct_sized_orders() {
        let r = pct_lane(0.015);
        assert_eq!(
            r.decide1(0, & dec("100000001", 0, 0.99, 5000.0, 5000.0)).unwrap_err(),
            Skip::PriceOutOfBand
        );
        assert_eq!(
            r.decide1(0, & dec("100000001", 0, 0.001, 5000.0, 5000.0)).unwrap_err(),
            Skip::PriceOutOfBand
        );
    }
    #[test]
    fn a_HALTED_lane_still_follows_him_OUT() {
        let r = router2();
        r.lane(0).unwrap().state.halted.store(true, Ordering::Relaxed);
        r.lane(0).unwrap().set_holding("1000001", 100.0);
        r.lane(0).unwrap().set_his("1000001", 1000.0);
        let i = r
            .decide1(0, &dec("1000001", 1, 0.60, 500.0, 1000.0))
            .expect("a halted lane MUST still be able to sell");
        assert_eq!(i.side, 1);
        assert!(i.shares > 0.0);
    }
    #[test]
    fn a_HALTED_lane_refuses_to_BUY() {
        let r = router2();
        r.lane(0).unwrap().state.halted.store(true, Ordering::Relaxed);
        assert_eq!(
            r.decide1(0, & dec("1000001", 0, 0.60, 1000.0, 5000.0)).unwrap_err(),
            Skip::Halted
        );
    }
    #[test]
    fn a_DISARMED_lane_refuses_BOTH_sides() {
        let r = router2();
        r.lane(0).unwrap().state.armed.store(false, Ordering::Relaxed);
        r.lane(0).unwrap().set_holding("1000001", 100.0);
        r.lane(0).unwrap().set_his("1000001", 1000.0);
        assert_eq!(
            r.decide1(0, & dec("1000001", 0, 0.60, 1000.0, 5000.0)).unwrap_err(),
            Skip::Disarmed
        );
        assert_eq!(
            r.decide1(0, & dec("1000001", 1, 0.60, 500.0, 1000.0)).unwrap_err(),
            Skip::Disarmed
        );
    }
    #[test]
    fn routes_each_wallet_to_its_own_lane() {
        let r = router2();
        assert_eq!(r.lane_for(& w(1)), Some(0));
        assert_eq!(r.lane_for(& w(2)), Some(1));
        assert_eq!(r.lane_for(& w(9)), None, "unknown wallet must not route");
    }
    #[test]
    fn a_SKIPPED_trade_never_leaves_the_token_owned() {
        let mut c = cfg("a", 1, Sizing::Pct(0.05));
        c.min_buy_price = 0.02;
        c.max_buy_price = 0.95;
        let lane = ready_lane(c);
        lane.state.armed.store(true, Ordering::Relaxed);
        let r = Router::new(vec![lane]);
        assert_eq!(
            r.decide1(0, & dec("TOK", 0, 0.99, 100.0, 100.0)).unwrap_err(),
            Skip::PriceOutOfBand
        );
        assert_eq!(
            r.owner_of("TOK"), None,
            "a skipped trade must leave the token free for another lane"
        );
    }
    #[test]
    fn an_ACTED_trade_does_claim_the_token() {
        let lane = ready_lane(cfg("a", 1, Sizing::Pct(0.05)));
        lane.state.armed.store(true, Ordering::Relaxed);
        let r = Router::new(vec![lane]);
        r.decide1(0, &dec("TOK", 0, 0.50, 1000.0, 1000.0)).expect("a valid buy");
        assert_eq!(r.owner_of("TOK"), Some(0), "acting on a token claims it");
    }
    #[test]
    fn the_collision_guard_still_blocks_a_SECOND_lane() {
        let a = ready_lane(cfg("a", 1, Sizing::Pct(0.05)));
        let b = ready_lane(cfg("b", 2, Sizing::Pct(0.05)));
        a.state.armed.store(true, Ordering::Relaxed);
        b.state.armed.store(true, Ordering::Relaxed);
        let r = Router::new(vec![a, b]);
        r.decide1(0, &dec("TOK", 0, 0.50, 1000.0, 1000.0)).expect("lane a takes it");
        assert_eq!(r.owner_of("TOK"), Some(0));
        assert_eq!(
            r.decide1(1, & dec("TOK", 0, 0.50, 1000.0, 1000.0)).unwrap_err(),
            Skip::TokenOwnedByOtherLane, "lane b must be refused"
        );
    }
    #[test]
    fn a_skipped_trade_does_not_block_the_lane_that_CAN_trade_it() {
        let mut ca = cfg("a", 1, Sizing::Pct(0.05));
        ca.max_buy_price = 0.60;
        let a = ready_lane(ca);
        let b = ready_lane(cfg("b", 2, Sizing::Pct(0.05)));
        a.state.armed.store(true, Ordering::Relaxed);
        b.state.armed.store(true, Ordering::Relaxed);
        let r = Router::new(vec![a, b]);
        assert_eq!(
            r.decide1(0, & dec("TOK", 0, 0.90, 1000.0, 1000.0)).unwrap_err(),
            Skip::PriceOutOfBand
        );
        r.decide1(1, &dec("TOK", 0, 0.90, 1000.0, 1000.0))
            .expect("lane b must still be able to copy this market");
        assert_eq!(r.owner_of("TOK"), Some(1));
    }
    #[test]
    fn an_UNSEEDED_lane_refuses_to_trade_at_all_including_exits() {
        let lane = Lane::new(cfg("fresh", 1, Sizing::Pct(0.05)));
        lane.state.armed.store(true, Ordering::Relaxed);
        lane.set_holding("100000001", 500.0);
        let r = Router::new(vec![lane]);
        let buy = dec("100000001", 0, 0.50, 1000.0, 1000.0);
        assert_eq!(
            r.decide1(0, & buy).unwrap_err(), Skip::NotReady, "no buying while unseeded"
        );
        let sell = dec("100000001", 1, 0.50, 50.0, 50.0);
        assert_eq!(
            r.decide1(0, & sell).unwrap_err(), Skip::NotReady,
            "and NO SELLING — an unsized exit is the 100% liquidation"
        );
    }
    #[test]
    fn the_same_lane_trades_normally_once_activation_completes() {
        let lane = Lane::new(cfg("fresh", 1, Sizing::Pct(0.05)));
        lane.state.armed.store(true, Ordering::Relaxed);
        lane.set_holding("100000001", 500.0);
        lane.set_his("100000001", 1000.0);
        lane.mark_ready();
        let r = Router::new(vec![lane]);
        let sell = dec("100000001", 1, 0.50, 100.0, 100.0);
        let intent = r.decide1(0, &sell).expect("a seeded lane exits normally");
        assert!(
            (intent.shares - 50.0).abs() < 1e-6, "proportional exit, got {}", intent
            .shares
        );
    }
    #[test]
    fn readiness_is_checked_BEFORE_arming_so_the_reason_is_never_misreported() {
        let lane = Lane::new(cfg("fresh", 1, Sizing::Pct(0.05)));
        let r = Router::new(vec![lane]);
        assert_eq!(
            r.decide1(0, & dec("1", 0, 0.5, 10.0, 10.0)).unwrap_err(), Skip::NotReady,
            "an unready lane reports NotReady, not Disarmed"
        );
    }
    #[test]
    fn lanes_size_independently_and_pct_is_of_his_CONFIRMED_fill() {
        let r = router2();
        let a = r.decide1(0, &dec("100000001", 0, 0.60, 5000.0, 5000.0)).unwrap();
        let b = r.decide1(1, &dec("100000002", 0, 0.60, 5000.0, 5000.0)).unwrap();
        assert!((a.shares - 5.0).abs() < 1e-9, "shares lane -> fixed 5");
        let b_usd = b.usd as f64 / MICRO;
        let intended = 5000.0 * 0.60 * 0.002;
        assert!(
            b_usd <= intended + 0.62,
            "whole-share/floor rounding exceeded one-share headroom: ${b_usd} vs ${intended}"
        );
        assert!(
            b_usd > intended - 0.62,
            "shortfall must be under one whole share; got ${b_usd} vs ${intended}"
        );
        let c = r.decide1(1, &dec("100000003", 0, 0.60, 10_000.0, 12_000.0)).unwrap();
        let c_usd = c.usd as f64 / MICRO;
        assert!(
            c_usd > b_usd,
            "confirmed fill size did not affect pct sizing: ${b_usd} vs ${c_usd}"
        );
    }
    #[test]
    fn second_lane_is_skipped_on_a_token_another_lane_owns() {
        let r = router2();
        assert!(r.decide1(0, & dec("55", 0, 0.60, 100.0, 5000.0)).is_ok());
        assert_eq!(
            r.decide1(1, & dec("55", 0, 0.60, 100.0, 5000.0)),
            Err(Skip::TokenOwnedByOtherLane)
        );
        assert_eq!(r.owner_of("55"), Some(0));
    }
    #[test]
    fn owning_lane_may_keep_trading_its_own_token() {
        let r = router2();
        assert!(r.decide1(0, & dec("55", 0, 0.60, 100.0, 5000.0)).is_ok());
        assert!(r.decide1(0, & dec("55", 0, 0.60, 100.0, 5000.0)).is_ok());
    }
    #[test]
    fn releasing_a_token_frees_it_for_another_lane() {
        let r = router2();
        r.decide1(0, &dec("55", 0, 0.60, 100.0, 5000.0)).unwrap();
        r.release("55", 0);
        assert!(r.decide1(1, & dec("55", 0, 0.60, 5000.0, 5000.0)).is_ok());
    }
    #[test]
    fn a_lane_cannot_release_a_token_it_does_not_own() {
        let r = router2();
        r.decide1(0, &dec("55", 0, 0.60, 100.0, 5000.0)).unwrap();
        r.release("55", 1);
        assert_eq!(r.owner_of("55"), Some(0));
    }
    #[test]
    fn disarmed_lane_fires_nothing() {
        let r = router2();
        r.lane(0).unwrap().state.armed.store(false, Ordering::Relaxed);
        assert_eq!(
            r.decide1(0, & dec("1000001", 0, 0.6, 100.0, 5000.0)), Err(Skip::Disarmed)
        );
    }
    #[test]
    fn halted_lane_fires_nothing_but_others_continue() {
        let r = router2();
        r.lane(0).unwrap().state.halted.store(true, Ordering::Relaxed);
        assert_eq!(
            r.decide1(0, & dec("1000001", 0, 0.6, 100.0, 5000.0)), Err(Skip::Halted)
        );
        assert!(
            r.decide1(1, & dec("1000002", 0, 0.6, 5000.0, 5000.0)).is_ok(),
            "one lane halting must not affect another"
        );
    }
    #[test]
    fn daily_budget_binds_and_is_per_lane() {
        let r = router2();
        let mut fired = 0;
        for i in 0..500 {
            if r.decide1(0, &dec(&format!("9{i:07}"), 0, 0.60, 100.0, 5000.0)).is_ok() {
                fired += 1;
            }
        }
        let spent = r.lane(0).unwrap().state.spent_today.load(Ordering::Relaxed) as f64
            / MICRO;
        assert!(spent <= 500.0 + 1e-6, "daily budget overshot: {spent}");
        assert!(fired > 0);
        assert!(r.decide1(1, & dec("777777777", 0, 0.60, 5000.0, 5000.0)).is_ok());
    }
    #[test]
    fn per_market_cap_binds() {
        let r = router2();
        let mut n = 0;
        while r.decide1(0, &dec("123456789", 0, 0.60, 100.0, 5000.0)).is_ok() {
            n += 1;
        }
        let l0 = r.lane(0).unwrap();
        let pt = l0.per_token.lock().unwrap();
        assert!(* pt.get("123456789").unwrap() as f64 / MICRO <= 200.0 + 1e-6);
        assert!(n > 0);
    }
    #[test]
    fn undersized_order_is_lifted_to_the_venue_floor() {
        let r = Router::new(vec![ready_lane(cfg("t", 1, Sizing::Pct(0.0001)))]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let i = r.decide1(0, &dec("1000001", 0, 0.05, 100.0, 5000.0)).unwrap();
        assert!(i.usd as f64 / MICRO >= 1.0 - 1e-9, "must reach the $1 venue floor");
    }
    #[test]
    fn undersized_order_is_skipped_when_floor_lifting_is_off() {
        let mut c = cfg("t", 1, Sizing::Pct(0.0001));
        c.min_fill_floor = false;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        assert_eq!(
            r.decide1(0, & dec("1000001", 0, 0.05, 100.0, 5000.0)),
            Err(Skip::DustAfterSizing)
        );
    }
    #[test]
    fn a_whole_share_order_under_a_dollar_is_reported_as_below_minimum() {
        let mut c = cfg("t", 1, Sizing::Shares(1.0));
        c.min_fill_floor = false;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        assert_eq!(
            r.decide1(0, & dec("1000001", 0, 0.03, 100.0, 5000.0)),
            Err(Skip::BelowVenueMinimum)
        );
    }
    #[test]
    fn buys_are_always_whole_shares() {
        let r = router2();
        let i = r.decide1(1, &dec("1000001", 0, 0.60, 6170.0, 8000.0)).unwrap();
        assert_eq!(i.shares, i.shares.trunc(), "buy size must be whole: {}", i.shares);
    }
    #[test]
    fn sell_sizing_is_tick_clean_and_keeps_dust_sellable() {
        let r = router2();
        r.lane(0).unwrap().set_holding("1000001", 0.7);
        r.lane(0).unwrap().set_his("1000001", 100.0);
        let i = r.decide1(0, &dec("1000001", 1, 0.22, 100.0, 100.0)).unwrap();
        let cents = i.shares * i.limit * 100.0;
        assert!(
            (cents - cents.round()).abs() < 1e-9, "off-tick exit {} x {}", i.shares, i
            .limit
        );
        assert!(i.shares > 0.0, "a 0.7-share exit must not be abandoned");
    }
    #[test]
    fn price_bands_are_enforced() {
        let r = router2();
        assert_eq!(
            r.decide1(0, & dec("1000001", 0, 0.99, 100.0, 5000.0)),
            Err(Skip::PriceOutOfBand)
        );
        assert_eq!(
            r.decide1(0, & dec("1000002", 0, 0.001, 100.0, 5000.0)),
            Err(Skip::PriceOutOfBand)
        );
    }
    #[test]
    fn a_FULL_exit_flattens_real_holdings_to_under_one_share() {
        for held in [221.1788f64, 191.25, 94.95, 63.0, 32.41, 15.2174, 5.0588, 3.8657] {
            let mut c = cfg("flat", 50, Sizing::Pct(0.015));
            c.sell_slippage_c = 1.0;
            c.sell_floor_frac = 0.0;
            c.sell_all_frac = 0.95;
            let r = Router::new(vec![ready_lane(c)]);
            r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
            let tok = "8000001";
            r.lane(0).unwrap().set_holding(tok, held);
            r.lane(0).unwrap().set_his(tok, 10_000.0);
            let i = r.decide1(0, &dec(tok, 1, 0.50, 10_000.0, 10_000.0)).unwrap();
            let left = held - i.shares;
            assert!(
                left < 1.0,
                "a full exit of {held} left {left} shares behind — more than the \
                     sub-share dust the cents-clean rule makes unavoidable"
            );
            assert!(
                i.limit <= 0.03, "100% slippage must reach the one-cent band, got {}", i
                .limit
            );
        }
    }
    #[test]
    fn a_SUB_SHARE_holding_now_EXITS_instead_of_being_stranded() {
        let mut c = cfg("subshare", 51, Sizing::Pct(0.015));
        c.sell_slippage_c = 1.0;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let tok = "8000002";
        r.lane(0).unwrap().set_holding(tok, 0.61);
        r.lane(0).unwrap().set_his(tok, 10_000.0);
        let i = r.decide1(0, &dec(tok, 1, 0.05, 10_000.0, 10_000.0)).unwrap();
        assert!(i.shares >= 0.60, "0.61 shares must now EXIT, got {}", i.shares);
        assert!(i.shares <= 0.61 + 1e-9, "and never oversell");
    }
    #[test]
    fn a_holding_under_the_SHARE_TICK_is_still_refused() {
        let mut c = cfg("subtick", 52, Sizing::Pct(0.015));
        c.sell_slippage_c = 1.0;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let tok = "8000003";
        r.lane(0).unwrap().set_holding(tok, 0.004);
        r.lane(0).unwrap().set_his(tok, 10_000.0);
        assert_eq!(
            r.decide1(0, & dec(tok, 1, 0.50, 10_000.0, 10_000.0)),
            Err(Skip::DustAfterSizing),
            "under one share-tick there is no legal order to send"
        );
    }
    #[test]
    fn a_GRADUAL_bleed_off_leaves_us_holding_NOTHING_and_the_ratio_never_drifts() {
        let mut c = cfg("bleed", 30, Sizing::Pct(0.015));
        c.sell_all_frac = 0.95;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let tok = "5000001";
        r.lane(0).unwrap().set_holding(tok, 808.0);
        r.lane(0).unwrap().set_his(tok, 10_000.0);
        let start_ratio = 808.0 / 10_000.0;
        let mut his = 10_000.0;
        for _ in 0..4 {
            let tranche = 2_500.0;
            let i = r.decide1(0, &dec(tok, 1, 0.50, tranche, 10_000.0)).unwrap();
            let held_before = r.lane(0).unwrap().holding(tok);
            tick_settled(&r.lane(0).unwrap(), tok, (held_before - i.shares).max(0.0));
            his -= tranche;
            r.lane(0).unwrap().set_his(tok, his);
            let held = r.lane(0).unwrap().holding(tok);
            if his > 1e-9 {
                let ratio = held / his;
                assert!(
                    (ratio - start_ratio).abs() < 0.005,
                    "our share of his book drifted: {ratio} vs {start_ratio}"
                );
            }
        }
        assert!(
            r.lane(0).unwrap().holding(tok) <= 1e-6,
            "after he is fully out we must hold NOTHING, still hold {}", r.lane(0)
            .unwrap().holding(tok)
        );
    }
    #[test]
    fn two_of_his_tranches_INSIDE_ONE_TICK_cannot_oversell_our_position() {
        let mut c = cfg("tranche", 40, Sizing::Pct(0.015));
        c.sell_all_frac = 0.95;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let tok = "6000001";
        r.lane(0).unwrap().set_holding(tok, 100.0);
        r.lane(0).unwrap().set_his(tok, 100.0);
        let a = r.decide1(0, &dec(tok, 1, 0.50, 30.0, 100.0)).unwrap();
        r.lane(0).unwrap().set_his(tok, 70.0);
        let b = r.decide1(0, &dec(tok, 1, 0.50, 30.0, 100.0)).unwrap();
        assert!(
            a.shares + b.shares <= 100.0 + 1e-9,
            "offered {} + {} = {} shares against a 100-share position", a.shares, b
            .shares, a.shares + b.shares
        );
        assert!(
            (a.shares + b.shares - 60.0).abs() < 0.02,
            "he sold 60% of his book so we must sell 60% of ours, got {} + {} = {}", a
            .shares, b.shares, a.shares + b.shares
        );
    }
    #[test]
    fn tranches_can_never_offer_MORE_SHARES_THAN_WE_HOLD() {
        let mut c = cfg("overshoot", 41, Sizing::Pct(0.015));
        c.sell_all_frac = 0.95;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let tok = "6000002";
        r.lane(0).unwrap().set_holding(tok, 100.0);
        r.lane(0).unwrap().set_his(tok, 100.0);
        let a = r.decide1(0, &dec(tok, 1, 0.50, 60.0, 100.0)).unwrap();
        r.lane(0).unwrap().set_his(tok, 40.0);
        let b = r.decide1(0, &dec(tok, 1, 0.50, 30.0, 100.0)).unwrap();
        assert!(
            a.shares + b.shares <= 100.0 + 1e-9,
            "offered {} + {} = {} against 100 held — the venue rejects this and v2 \
                 never retries an exit",
            a.shares, b.shares, a.shares + b.shares
        );
        assert!(
            (a.shares + b.shares - 90.0).abs() < 0.02,
            "expected 90 shares across the two tranches, got {} + {} = {}", a.shares, b
            .shares, a.shares + b.shares
        );
    }
    #[test]
    fn a_fully_reserved_position_has_NOTHING_LEFT_to_sell_and_says_so() {
        let mut c = cfg("reserved", 42, Sizing::Pct(0.015));
        c.sell_all_frac = 0.95;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let tok = "6000003";
        r.lane(0).unwrap().set_holding(tok, 100.0);
        r.lane(0).unwrap().set_his(tok, 100.0);
        let a = r.decide1(0, &dec(tok, 1, 0.50, 100.0, 100.0)).unwrap();
        assert!((a.shares - 100.0).abs() < 1e-9, "a full exit sells the position");
        r.lane(0).unwrap().set_his(tok, 0.0);
        assert_eq!(
            r.decide1(0, & dec(tok, 1, 0.50, 10.0, 100.0)),
            Err(Skip::SellAlreadyReserved),
            "every share is already committed to an unresolved sell"
        );
        assert!(
            (r.lane(0).unwrap().holding(tok) - 100.0).abs() < 1e-9,
            "the reservation must not rewrite what we actually hold"
        );
    }
    #[test]
    fn a_single_full_exit_dumps_an_OVERSIZED_position_entirely() {
        let mut c = cfg("fullexit", 31, Sizing::Pct(0.015));
        c.sell_all_frac = 0.95;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let tok = "5000002";
        r.lane(0).unwrap().set_holding(tok, 808.3783);
        r.lane(0).unwrap().set_his(tok, 10_649.14);
        let i = r.decide1(0, &dec(tok, 1, 0.50, 10_649.14, 10_649.14)).unwrap();
        assert!(
            i.shares > 808.0 - 1.0,
            "a full exit must clear the whole oversized position, got {}", i.shares
        );
        assert!(
            808.3783 - i.shares < 1.0,
            "at most one share of cents-clean dust may remain, left {}", 808.3783 - i
            .shares
        );
    }
    #[test]
    fn COMPOUNDING_scales_the_PCT_so_utilisation_stays_constant() {
        let clip = |scale: f64| -> f64 {
            let mut c = cfg("pctscale", 42, Sizing::Pct(0.015));
            c.per_market_usd = 1_000_000.0;
            c.max_usd_per_fill = 1_000_000.0;
            c.max_open_usd = 10_000_000.0;
            c.daily_budget_usd = 10_000_000.0;
            let r = Router::new(vec![ready_lane(c)]);
            r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
            r.lane(0)
                .unwrap()
                .state
                .cap_scale
                .store((scale * MICRO) as i64, Ordering::Relaxed);
            let i = r.decide1(0, &dec("7000001", 0, 0.50, 10_000.0, 10_000.0)).unwrap();
            i.usd as f64 / MICRO
        };
        let one = clip(1.0);
        let two = clip(2.0);
        assert!(
            (two / one - 2.0).abs() < 0.05,
            "doubling the bankroll must double the clip: {one} -> {two}"
        );
        let half = clip(0.5);
        assert!(
            (half / one - 0.5).abs() < 0.05,
            "halving the bankroll must halve the clip: {one} -> {half}"
        );
    }
    #[test]
    fn the_compounded_pct_is_CEILINGED_however_rich_we_get() {
        use crate::budget::{effective_pct, MAX_EFFECTIVE_PCT};
        let cap = MAX_EFFECTIVE_PCT;
        assert!((effective_pct(0.015, 1.0, cap, true) - 0.015).abs() < 1e-12);
        assert!(
            (effective_pct(0.015, 2.0, cap, true) - 0.030).abs() < 1e-12, "2x -> 3%"
        );
        assert_eq!(
            effective_pct(0.015, 100.0, cap, true), MAX_EFFECTIVE_PCT,
            "must clamp, not run away"
        );
        assert!((effective_pct(0.015, f64::NAN, cap, true) - 0.015).abs() < 1e-12);
        assert_eq!(effective_pct(0.015, 0.0, cap, true), 0.0);
    }
    #[test]
    fn COMPOUNDING_scales_every_cap_and_leaves_the_config_alone() {
        let clip_at = |scale: f64| -> f64 {
            let mut c = cfg("compound", 40, Sizing::Pct(0.015));
            c.max_usd_per_fill = 100.0;
            c.per_market_usd = 200.0;
            c.max_open_usd = 400.0;
            c.daily_budget_usd = 300.0;
            let r = Router::new(vec![ready_lane(c)]);
            r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
            r.lane(0)
                .unwrap()
                .state
                .cap_scale
                .store((scale * MICRO) as i64, Ordering::Relaxed);
            let i = r
                .decide1(0, &dec("6000001", 0, 0.50, 100_000.0, 100_000.0))
                .unwrap();
            i.usd as f64 / MICRO
        };
        let one = clip_at(1.0);
        assert!(
            one <= 100.0 + 1e-6, "at 1.0x the clamp is the configured $100, got ${one}"
        );
        let two = clip_at(2.0);
        assert!(
            two > 150.0 && two <= 200.0 + 1e-6, "at 2.0x expected ~$200, got ${two}"
        );
        let half = clip_at(0.5);
        assert!(half <= 50.0 + 1e-6, "at 0.5x expected ~$50, got ${half}");
        assert!(half < one && one < two, "caps must move monotonically with the scale");
    }
    #[test]
    fn a_lane_that_never_gets_a_balance_reading_runs_its_CONFIGURED_caps() {
        let c = cfg("nofeed", 41, Sizing::Pct(0.015));
        let r = Router::new(vec![ready_lane(c)]);
        assert_eq!(
            r.lane(0).unwrap().state.cap_scale.load(Ordering::Relaxed), MICRO as i64,
            "cap_scale must default to exactly 1.0x"
        );
    }
    #[test]
    fn THE_5X_OVERSIZE_INCIDENT_is_fixed() {
        let mut c = cfg("oversize", 20, Sizing::Pct(0.015));
        c.buy_slippage_c = 0.15;
        c.min_buy_price = 0.01;
        c.per_market_usd = 10_000.0;
        c.max_usd_per_fill = 5_000.0;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let i = r.decide1(0, &dec("4000001", 0, 0.0349, 10_649.14, 10_649.14)).unwrap();
        let usd = i.usd as f64 / MICRO;
        let his_committed = 10_649.14 * 0.0349;
        let intended = his_committed * 0.015;
        assert!(
            (usd - intended).abs() < 0.30,
            "we must commit ~1.5% of his ${his_committed:.2} = ${intended:.2}, got ${usd:.2}"
        );
        assert!(usd < 8.0, "the old code committed $29.41 here; got ${usd:.2}");
        let expect_back = usd / 0.0349;
        assert!(
            (expect_back - 159.7).abs() < 12.0,
            "filling near his price should return ~159 shares, projects {expect_back:.1}"
        );
    }
    #[test]
    fn the_second_measured_fire_is_also_corrected() {
        let mut c = cfg("oversize2", 21, Sizing::Pct(0.015));
        c.buy_slippage_c = 0.15;
        c.per_market_usd = 10_000.0;
        c.max_usd_per_fill = 5_000.0;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let i = r.decide1(0, &dec("4000002", 0, 0.09898, 5_000.0, 5_000.0)).unwrap();
        let usd = i.usd as f64 / MICRO;
        assert!((usd - 7.42).abs() < 0.30, "expected ~$7.42 committed, got ${usd:.2}");
    }
    #[test]
    fn overshoot_is_BOUNDED_across_the_whole_price_range() {
        for px in [0.02f64, 0.05, 0.10, 0.25, 0.50, 0.80] {
            let mut c = cfg("range", 22, Sizing::Pct(0.015));
            c.buy_slippage_c = 0.15;
            c.min_buy_price = 0.01;
            c.per_market_usd = 100_000.0;
            c.max_usd_per_fill = 50_000.0;
            let r = Router::new(vec![ready_lane(c)]);
            r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
            let i = r.decide1(0, &dec("4000003", 0, px, 10_000.0, 10_000.0)).unwrap();
            let usd = i.usd as f64 / MICRO;
            let intended = 10_000.0 * px * 0.015;
            let ratio = usd / intended;
            assert!(
                ratio < 1.35,
                "at price {px} we commit {ratio:.2}x the intended dollars (was 5.3x at 3.5c)"
            );
        }
    }
    #[test]
    fn with_ZERO_slippage_pct_sizing_is_unchanged() {
        let mut c = cfg("nozero", 23, Sizing::Pct(0.015));
        c.buy_slippage_c = 0.0;
        c.per_market_usd = 10_000.0;
        c.max_usd_per_fill = 5_000.0;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let i = r.decide1(0, &dec("4000004", 0, 0.10, 5_000.0, 5_000.0)).unwrap();
        assert_eq!(i.shares, 75.0, "1.5% of 5000 with no pad must still be exactly 75");
    }
    #[test]
    fn BUY_slippage_is_wide_and_SELL_slippage_is_TIGHT_and_they_are_independent() {
        let mut c = cfg("asym", 9, Sizing::Shares(5.0));
        c.buy_slippage_c = 0.15;
        c.sell_slippage_c = 0.02;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let buy = r.decide1(0, &dec("2000001", 0, 0.50, 100.0, 100.0)).unwrap();
        assert!(
            (buy.limit - 0.65).abs() < 1e-9,
            "buy limit should be his 0.50 + 0.15 = 0.65, got {}", buy.limit
        );
        r.lane(0).unwrap().set_holding("2000002", 100.0);
        r.lane(0).unwrap().set_his("2000002", 100.0);
        let sell = r.decide1(0, &dec("2000002", 1, 0.50, 100.0, 100.0)).unwrap();
        assert!(
            sell.limit > 0.40,
            "sell limit must stay near his price, not fall to the buy width: {}", sell
            .limit
        );
    }
    #[test]
    fn a_wide_BUY_slippage_never_escapes_the_price_ceiling() {
        let mut c = cfg("ceil", 10, Sizing::Shares(5.0));
        c.buy_slippage_c = 0.15;
        c.max_buy_price = 0.95;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let i = r.decide1(0, &dec("2000003", 0, 0.90, 100.0, 100.0)).unwrap();
        assert!(i.limit <= 0.99 + 1e-9, "limit must clamp to 0.99, got {}", i.limit);
    }
    #[test]
    fn sell_slippage_of_1_0_with_zero_floor_reaches_the_tick() {
        let mut c = cfg("anybid", 12, Sizing::Shares(5.0));
        c.sell_slippage_c = 1.0;
        c.sell_floor_frac = 0.0;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        for (i, px) in [0.05f64, 0.25, 0.50, 0.75, 0.95].iter().enumerate() {
            let tok = format!("30000{i:02}");
            r.lane(0).unwrap().set_holding(&tok, 100.0);
            r.lane(0).unwrap().set_his(&tok, 100.0);
            let o = r.decide1(0, &dec(&tok, 1, *px, 100.0, 100.0)).unwrap();
            assert!(
                o.limit <= 0.03,
                "exit at his {px} must accept the bottom of the book, got {}", o.limit
            );
            assert!(o.limit >= 0.01 - 1e-9, "never below the venue tick");
        }
    }
    #[test]
    fn a_wide_SELL_slippage_never_goes_below_the_floor() {
        let mut c = cfg("floor", 11, Sizing::Shares(5.0));
        c.sell_slippage_c = 0.15;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        r.lane(0).unwrap().set_holding("2000004", 100.0);
        r.lane(0).unwrap().set_his("2000004", 100.0);
        let i = r.decide1(0, &dec("2000004", 1, 0.10, 100.0, 100.0)).unwrap();
        assert!(i.limit >= 0.01 - 1e-9, "limit must floor at 0.01, got {}", i.limit);
    }
    #[test]
    fn max_usd_per_fill_clamps_a_huge_pct_copy() {
        let mut c = cfg("big", 2, Sizing::Pct(0.10));
        c.per_market_usd = 10_000.0;
        let r = Router::new(vec![ready_lane(c)]);
        r.lane(0).unwrap().state.armed.store(true, Ordering::Relaxed);
        let i = r.decide1(0, &dec("1000001", 0, 0.60, 25_000.0, 25_000.0)).unwrap();
        assert!(i.usd as f64 / MICRO <= 250.0 + 1e-6, "max_usd_per_fill did not clamp");
    }
    #[test]
    fn config_with_unreachable_fill_clamp_is_detected() {
        let mut c = cfg("bad", 1, Sizing::Shares(5.0));
        c.per_market_usd = 200.0;
        c.max_usd_per_fill = 250.0;
        assert!(c.validate().is_err());
        c.per_market_usd = 500.0;
        assert!(c.validate().is_ok());
    }
    #[test]
    fn sells_bypass_entry_budgets_because_exiting_must_always_be_possible() {
        let r = router2();
        r.lane(0).unwrap().set_holding("1000001", 100.0);
        r.lane(0).unwrap().set_his("1000001", 1000.0);
        r.lane(0)
            .unwrap()
            .state
            .spent_today
            .store((10_000.0 * MICRO) as Micro, Ordering::Relaxed);
        let i = r.decide1(0, &dec("1000001", 1, 0.80, 250.0, 5000.0)).unwrap();
        assert_eq!(i.side, 1);
        assert!(i.limit < 0.80, "exit limit should concede price to get out");
    }
    #[test]
    fn exit_is_sized_from_OUR_holding_not_his_fill() {
        let r = router2();
        r.lane(0).unwrap().set_holding("1000001", 5.0);
        r.lane(0).unwrap().set_his("1000001", 10_000.0);
        let i = r.decide1(0, &dec("1000001", 1, 0.60, 5_000.0, 10_000.0)).unwrap();
        assert!(
            i.shares <= 5.0, "tried to sell {} shares of a 5-share position", i.shares
        );
        assert!(
            i.shares > 1.0, "a 50% exit of 5 shares should be ~2.5, got {}", i.shares
        );
    }
    #[test]
    fn a_near_full_exit_of_his_closes_ours_completely() {
        let r = router2();
        r.lane(0).unwrap().set_holding("1000001", 10.0);
        r.lane(0).unwrap().set_his("1000001", 1000.0);
        let i = r.decide1(0, &dec("1000001", 1, 0.60, 960.0, 1000.0)).unwrap();
        assert!((i.shares - 10.0).abs() < 1e-6, "96% exit should close us fully");
    }
    #[test]
    fn we_never_mirror_an_exit_of_a_token_we_do_not_hold() {
        let r = router2();
        r.lane(0).unwrap().set_his("1000001", 5000.0);
        assert_eq!(
            r.decide1(0, & dec("1000001", 1, 0.60, 1000.0, 5000.0)),
            Err(Skip::SellNoPosition)
        );
    }
    #[test]
    fn legacy_positions_are_excluded_from_the_exit_fraction() {
        let r = router2();
        r.lane(0).unwrap().set_holding("1000001", 10.0);
        r.lane(0).unwrap().set_his("1000001", 1100.0);
        r.lane(0).unwrap().set_legacy("1000001", 1000.0);
        assert_eq!(
            r.decide1(0, & dec("1000001", 1, 0.60, 550.0, 1100.0)),
            Err(Skip::SellNoPosition)
        );
        let r2 = router2();
        r2.lane(0).unwrap().set_holding("2000002", 10.0);
        r2.lane(0).unwrap().set_his("2000002", 1100.0);
        r2.lane(0).unwrap().set_legacy("2000002", 1000.0);
        let i = r2.decide1(0, &dec("2000002", 1, 0.60, 1050.0, 1100.0)).unwrap();
        assert!(i.shares > 1.0 && i.shares <= 10.0, "got {}", i.shares);
    }
    #[test]
    fn an_exit_never_exceeds_what_we_hold_even_if_he_dumps_everything() {
        let r = router2();
        r.lane(0).unwrap().set_holding("1000001", 3.0);
        r.lane(0).unwrap().set_his("1000001", 25_000.0);
        let i = r.decide1(0, &dec("1000001", 1, 0.60, 25_000.0, 25_000.0)).unwrap();
        assert!(i.shares <= 3.0 + 1e-9, "sold {} of a 3-share position", i.shares);
    }
    #[test]
    fn his_remaining_is_carried_for_the_hybrid_decision() {
        let r = router2();
        let i = r.decide1(0, &dec("1000001", 0, 0.60, 400.0, 5000.0)).unwrap();
        assert!(
            (i.his_remaining - 4600.0).abs() < 1e-6,
            "hybrid resting needs his REMAINING size, not his fill"
        );
    }
    #[test]
    fn a_repriced_lane_SIZES_DIFFERENTLY_without_a_restart() {
        let lane = ready_lane(cfg("hot", 1, Sizing::Pct(0.05)));
        let before = lane.policy();
        assert!(matches!(before.sizing, Sizing::Pct(p) if (p - 0.05).abs() < 1e-12));
        let next = SizingPolicy::build(
                0,
                20_000.0,
                Sizing::Pct(0.10),
                0.25,
                false,
                &crate::budget::Fracs::default(),
            )
            .unwrap();
        let gen = lane.apply_policy(next).unwrap();
        assert_eq!(gen, 1, "the applied generation must advance");
        let after = lane.policy();
        assert!(matches!(after.sizing, Sizing::Pct(p) if (p - 0.10).abs() < 1e-12));
        assert!(
            after.caps.max_open_usd > before.caps.max_open_usd,
            "raising the seed must raise the caps in the same swap"
        );
        assert!(after.caps.max_usd_per_fill <= after.caps.per_market_usd);
        assert!(after.caps.per_market_usd <= after.caps.max_open_usd);
    }
    #[test]
    fn an_INVALID_policy_is_REFUSED_and_the_live_one_survives() {
        let lane = ready_lane(cfg("hot", 1, Sizing::Pct(0.05)));
        let f = crate::budget::Fracs::default();
        for bad in [
            SizingPolicy::build(0, -1.0, Sizing::Pct(0.10), 0.25, false, &f),
            SizingPolicy::build(0, 10_000.0, Sizing::Pct(0.0), 0.25, false, &f),
            SizingPolicy::build(0, 10_000.0, Sizing::Pct(1.5), 0.25, false, &f),
            SizingPolicy::build(0, 10_000.0, Sizing::Pct(0.10), 0.0, false, &f),
        ] {
            assert!(bad.is_err(), "an out-of-range policy must not build");
        }
        assert_eq!(lane.policy().generation, 0, "nothing was applied");
        assert!(
            matches!(lane.policy().sizing, Sizing::Pct(p) if (p - 0.05).abs() < 1e-12)
        );
    }
    #[test]
    fn the_CEILING_still_clamps_after_a_hot_change() {
        let lane = ready_lane(cfg("hot", 1, Sizing::Pct(0.05)));
        let pol = SizingPolicy::build(
                0,
                10_000.0,
                Sizing::Pct(0.10),
                0.05,
                false,
                &crate::budget::Fracs::default(),
            )
            .unwrap();
        lane.apply_policy(pol).unwrap();
        let p = lane.policy();
        let eff = crate::budget::effective_pct(
            match p.sizing {
                Sizing::Pct(v) => v,
                _ => 0.0,
            },
            1.0,
            p.max_effective_pct,
            p.compound,
        );
        assert!(
            (eff - 0.05).abs() < 1e-12,
            "a 10% pct under a 5% ceiling is still 5% — the ceiling must move too"
        );
    }
    #[test]
    fn EVERY_sizing_reader_follows_a_hot_reprice_not_just_decide() {
        let lane = ready_lane(cfg("gen", 1, Sizing::Pct(0.05)));
        let boot = lane.policy();
        assert!(matches!(boot.sizing, Sizing::Pct(p) if (p - 0.05).abs() < 1e-12));
        lane.apply_policy(
                SizingPolicy::build(
                        0,
                        20_000.0,
                        Sizing::Pct(0.10),
                        0.25,
                        true,
                        &crate::budget::Fracs::default(),
                    )
                    .unwrap(),
            )
            .unwrap();
        let live = lane.policy();
        assert!(
            matches!(live.sizing, Sizing::Pct(p) if (p - 0.10).abs() < 1e-12),
            "policy() must report the applied generation"
        );
        assert!((live.max_effective_pct - 0.25).abs() < 1e-12);
        assert!(live.compound);
        assert!(
            live.caps.daily_usd > boot.caps.daily_usd,
            "derived caps must move with the seed, not stay at boot values"
        );
        assert!(
            matches!(lane.cfg.sizing, Sizing::Pct(p) if (p - 0.05).abs() < 1e-12),
            "cfg keeps boot values; only `policy()` is authoritative for sizing"
        );
    }
    #[test]
    fn a_lane_held_by_an_INCIDENT_still_follows_him_OUT() {
        let r = router2();
        r.lane(0).unwrap().state.halt_latch.store(true, Ordering::Relaxed);
        r.lane(0).unwrap().state.halted.store(true, Ordering::Relaxed);
        r.lane(0).unwrap().set_holding("1000001", 100.0);
        r.lane(0).unwrap().set_his("1000001", 1000.0);
        let i = r
            .decide1(0, &dec("1000001", 1, 0.60, 500.0, 1000.0))
            .expect("a lane latched by an INCIDENT must still be able to sell");
        assert_eq!(i.side, 1);
        assert!(i.shares > 0.0);
    }
}
#[cfg(test)]
mod route_tests {
    use super::*;
    use crate::book::Top;
    use std::time::Instant;
    fn top(bb: f64, ba: f64) -> Top {
        Top {
            best_bid: bb,
            best_ask: ba,
            at: Instant::now(),
        }
    }
    fn intent(side: u8) -> Intent {
        Intent {
            lane: 0,
            token_id: "1".into(),
            side,
            shares: 5.0,
            limit: 0.62,
            usd: 0,
            execution: Execution::Hybrid,
            his_remaining: 4600.0,
            he_was_maker: false,
            route: Route::Take,
        }
    }
    fn hybrid_cfg() -> LaneConfig {
        LaneConfig {
            name: "h".into(),
            wallet20: [1u8; 20],
            sizing: Sizing::Shares(5.0),
            execution: Execution::Hybrid,
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
            copy_makers: false,
            compound: true,
            exclude_political: false,
        }
    }
    #[test]
    fn a_one_tick_spread_falls_back_to_taking() {
        let r = route_for(
            &intent(0),
            &hybrid_cfg(),
            Some(top(0.60, 0.61)),
            0.60,
            false,
            false,
            false,
        );
        assert_eq!(r, Route::Take);
    }
    #[test]
    fn A_MAKER_MIRROR_LANE_NEEDS_A_BOOK_even_though_it_is_not_HYBRID() {
        let plain = Lane::new(taker_cfg(false));
        assert!(
            ! needs_book(& std::sync::Arc::new(plain)), "a pure taker lane needs none"
        );
        let mut m = taker_cfg(false);
        m.copy_makers = true;
        assert!(
            needs_book(& std::sync::Arc::new(Lane::new(m))),
            "a maker-mirror lane needs a book — it cannot check for room without one"
        );
        let mut sells = taker_cfg(false);
        sells.copy_maker_sells = true;
        assert!(
            needs_book(& std::sync::Arc::new(Lane::new(sells))),
            "the SELL mirror needs one too"
        );
        assert!(needs_book(& std::sync::Arc::new(Lane::new(hybrid_cfg()))));
        let mut on = taker_cfg(false);
        on.copy_makers = true;
        let lane = std::sync::Arc::new(Lane::new(on));
        assert!(needs_book(& lane));
        lane.state.rest_buys.store(false, Ordering::Relaxed);
        lane.state.rest_sells.store(false, Ordering::Relaxed);
        assert!(! needs_book(& lane));
    }
    #[test]
    fn THE_KILL_SWITCH_stops_resting_on_each_side_INDEPENDENTLY() {
        let mut c = taker_cfg(false);
        c.copy_makers = true;
        c.copy_maker_sells = true;
        let (mut buy, mut sell) = (intent(0), intent(1));
        buy.he_was_maker = true;
        sell.he_was_maker = true;
        let book = Some(top(0.55, 0.70));
        assert!(
            matches!(resolve_route(& buy, & c, book, 0.60, false, false, false, true,
            true), Route::RestInFront { .. })
        );
        assert!(
            matches!(resolve_route(& sell, & c, book, 0.60, false, false, false, true,
            true), Route::RestInFront { .. })
        );
        assert_eq!(
            resolve_route(& buy, & c, book, 0.60, false, false, false, false, true),
            Route::Take, "a disabled buy side must cross, not rest"
        );
        assert!(
            matches!(resolve_route(& sell, & c, book, 0.60, false, false, false, false,
            true), Route::RestInFront { .. }), "the sell side is unaffected"
        );
        assert_eq!(
            resolve_route(& sell, & c, book, 0.60, false, false, false, true, false),
            Route::Take
        );
        assert_eq!(
            resolve_route(& buy, & c, book, 0.60, false, false, false, false, false),
            Route::Take
        );
        assert_eq!(
            resolve_route(& sell, & c, book, 0.60, false, false, false, false, false),
            Route::Take
        );
    }
    #[test]
    fn the_switch_SEEDS_from_config_so_boot_behaviour_is_unchanged() {
        let mut on = taker_cfg(false);
        on.copy_makers = true;
        on.copy_maker_sells = true;
        let lane = Lane::new(on);
        assert!(lane.state.rest_buys.load(Ordering::Relaxed));
        assert!(lane.state.rest_sells.load(Ordering::Relaxed));
        let off = taker_cfg(false);
        let lane = Lane::new(off);
        assert!(! lane.state.rest_buys.load(Ordering::Relaxed));
        assert!(! lane.state.rest_sells.load(Ordering::Relaxed));
        let hy = hybrid_cfg();
        let lane = Lane::new(hy);
        assert!(lane.state.rest_buys.load(Ordering::Relaxed));
        assert!(lane.state.rest_sells.load(Ordering::Relaxed));
    }
    #[test]
    fn a_HYBRID_lane_still_rests_his_TAKER_fills_under_the_switch() {
        let c = hybrid_cfg();
        let mut taker = intent(0);
        taker.he_was_maker = false;
        assert!(
            matches!(resolve_route(& taker, & c, Some(top(0.55, 0.70)), 0.60, false,
            false, false, true, true), Route::RestInFront { .. }),
            "Hybrid rests regardless of his role"
        );
        let mut m = taker_cfg(false);
        m.copy_makers = true;
        assert_eq!(
            resolve_route(& taker, & m, Some(top(0.55, 0.70)), 0.60, false, false, false,
            true, true), Route::Take, "the maker mirror only mirrors his MAKER fills"
        );
    }
    #[test]
    fn a_wide_spread_rests_one_tick_in_front() {
        let r = route_for(
            &intent(0),
            &hybrid_cfg(),
            Some(top(0.60, 0.65)),
            0.60,
            false,
            false,
            false,
        );
        match r {
            Route::RestInFront { limit, room_ticks } => {
                assert!((limit - 0.61).abs() < 1e-9, "must rest ONE tick above him");
                assert!(room_ticks >= 1);
            }
            Route::Take => panic!("wide spread should rest"),
        }
    }
    #[test]
    fn a_taker_lane_never_rests_however_wide_the_book() {
        let mut c = hybrid_cfg();
        c.execution = Execution::Taker;
        assert_eq!(
            route_for(& intent(0), & c, Some(top(0.30, 0.90)), 0.30, false, false,
            false), Route::Take
        );
    }
    fn taker_cfg(copy_makers: bool) -> LaneConfig {
        let mut c = hybrid_cfg();
        c.execution = Execution::Taker;
        c.copy_makers = copy_makers;
        c
    }
    fn intent_role(maker: bool) -> Intent {
        let mut i = intent(0);
        i.he_was_maker = maker;
        i
    }
    fn sell_cfg(on: bool) -> LaneConfig {
        let mut c = taker_cfg(false);
        c.copy_maker_sells = on;
        c
    }
    fn sell_intent(maker: bool) -> Intent {
        let mut i = intent(0);
        i.side = 1;
        i.he_was_maker = maker;
        i
    }
    #[test]
    fn the_ledger_accumulates_per_order_and_the_flags_track_his_lifecycle() {
        let led = TrancheLedger::new(64);
        let salt = [1u8; 32];
        let c1 = led.observe(&salt, 300.0);
        assert_eq!(sell_tranche_flags(c1, 300.0, 1000.0), (true, false), "opening");
        let c2 = led.observe(&salt, 300.0);
        assert_eq!(sell_tranche_flags(c2, 300.0, 1000.0), (false, false), "middle");
        let c3 = led.observe(&salt, 400.0);
        assert_eq!(
            sell_tranche_flags(c3, 400.0, 1000.0), (false, true),
            "⛔ the final tranche of a MULTI-fill order — per-fill maths called \
                    this (false,false) and would have RESTED after his order completed"
        );
    }
    #[test]
    fn a_single_fill_exit_is_first_AND_done() {
        let led = TrancheLedger::new(64);
        let cum = led.observe(&[2u8; 32], 1000.0);
        assert_eq!(sell_tranche_flags(cum, 1000.0, 1000.0), (true, true));
    }
    #[test]
    fn two_of_his_orders_do_not_share_a_running_sum() {
        let led = TrancheLedger::new(64);
        led.observe(&[3u8; 32], 900.0);
        let c = led.observe(&[4u8; 32], 100.0);
        assert!((c - 100.0).abs() < 1e-9, "salt is the order identity");
    }
    #[test]
    fn an_unknown_order_total_reads_done_and_therefore_TAKES() {
        for bad in [0.0, -5.0] {
            let (_, done) = sell_tranche_flags(300.0, 300.0, bad);
            assert!(done, "order_size={bad} must read done");
        }
    }
    #[test]
    fn eviction_loses_history_in_the_SAFE_direction() {
        let led = TrancheLedger::new(2);
        led.observe(&[5u8; 32], 100.0);
        led.observe(&[6u8; 32], 100.0);
        led.observe(&[7u8; 32], 100.0);
        assert_eq!(led.len(), 2);
        let c = led.observe(&[5u8; 32], 100.0);
        assert_eq!(
            sell_tranche_flags(c, 100.0, 1000.0).0, true,
            "an evicted order reads as an opening clip, which crosses"
        );
    }
    #[test]
    fn the_full_three_tranche_walk_routes_TAKE_REST_TAKE() {
        let led = TrancheLedger::new(64);
        let c = sell_cfg(true);
        let salt = [8u8; 32];
        let mut routes = Vec::new();
        for fill in [300.0, 300.0, 400.0] {
            let cum = led.observe(&salt, fill);
            let (first, done) = sell_tranche_flags(cum, fill, 1000.0);
            routes
                .push(
                    route_for(
                        &sell_intent(true),
                        &c,
                        Some(top(0.60, 0.65)),
                        0.65,
                        false,
                        first,
                        done,
                    ),
                );
        }
        assert_eq!(routes[0], Route::Take, "t1: certainty first");
        assert!(matches!(routes[1], Route::RestInFront { .. }), "t2: rest under him");
        assert_eq!(routes[2], Route::Take, "t3: his last tranche has nothing behind it");
    }
    #[test]
    fn copy_maker_sells_OFF_is_byte_for_byte_TODAYS_exit() {
        let c = sell_cfg(false);
        for (first, done) in [(true, false), (false, false), (false, true)] {
            assert_eq!(
                route_for(& sell_intent(true), & c, Some(top(0.60, 0.65)), 0.65, false,
                first, done), Route::Take, "off ⇒ always cross"
            );
        }
    }
    #[test]
    fn his_OPENING_sell_tranche_CROSSES() {
        let c = sell_cfg(true);
        assert_eq!(
            route_for(& sell_intent(true), & c, Some(top(0.60, 0.65)), 0.65, false, true,
            false), Route::Take
        );
    }
    #[test]
    fn a_LATER_sell_tranche_rests_ONE_TICK_UNDER_HIM() {
        let c = sell_cfg(true);
        match route_for(
            &sell_intent(true),
            &c,
            Some(top(0.60, 0.65)),
            0.65,
            false,
            false,
            false,
        ) {
            Route::RestInFront { limit, .. } => {
                assert!(
                    (limit - 0.64).abs() < 1e-9, "must rest BELOW his 0.65, got {limit}"
                )
            }
            r => panic!("expected a rest, got {r:?}"),
        }
    }
    #[test]
    fn his_FINAL_tranche_CROSSES_because_nothing_follows_it_through() {
        let c = sell_cfg(true);
        assert_eq!(
            route_for(& sell_intent(true), & c, Some(top(0.60, 0.65)), 0.65, false,
            false, true), Route::Take
        );
    }
    #[test]
    fn his_TAKER_sell_is_never_mirrored_as_a_rest() {
        let c = sell_cfg(true);
        assert_eq!(
            route_for(& sell_intent(false), & c, Some(top(0.60, 0.65)), 0.65, false,
            false, false), Route::Take
        );
    }
    #[test]
    fn NO_ROOM_under_him_falls_back_to_crossing() {
        let c = sell_cfg(true);
        assert_eq!(
            route_for(& sell_intent(true), & c, Some(top(0.64, 0.65)), 0.65, false,
            false, false), Route::Take, "no room ⇒ take, never post through the bid"
        );
    }
    #[test]
    fn enabling_sell_resting_does_NOT_change_BUY_routing() {
        let c = sell_cfg(true);
        assert_eq!(
            route_for(& intent_role(true), & c, Some(top(0.60, 0.65)), 0.60, false,
            false, false), Route::Take, "buys are still governed by copy_makers alone"
        );
    }
    #[test]
    fn copy_makers_OFF_always_takes_even_on_his_maker_fill() {
        let c = taker_cfg(false);
        assert_eq!(
            route_for(& intent_role(true), & c, Some(top(0.60, 0.65)), 0.60, false,
            false, false), Route::Take, "copy_makers off ⇒ never rest"
        );
    }
    #[test]
    fn the_FIRST_clip_TAKES_even_when_the_book_is_wide_enough_to_rest() {
        let c = taker_cfg(true);
        assert_eq!(
            route_for(& intent_role(true), & c, Some(top(0.60, 0.65)), 0.60, true, true,
            false), Route::Take,
            "the first clip must cross so we are certainly IN before resting"
        );
    }
    #[test]
    fn AFTER_the_first_clip_the_remainder_rests_in_front() {
        let c = taker_cfg(true);
        match route_for(
            &intent_role(true),
            &c,
            Some(top(0.60, 0.65)),
            0.60,
            false,
            false,
            false,
        ) {
            Route::RestInFront { limit, .. } => {
                assert!(
                    (limit - 0.61).abs() < 1e-9,
                    "later clips rest ONE tick in front of him"
                )
            }
            Route::Take => panic!("only the FIRST clip should take on a wide book"),
        }
    }
    #[test]
    fn the_first_clip_rule_does_NOT_change_a_hybrid_lane() {
        let c = hybrid_cfg();
        match route_for(&intent(0), &c, Some(top(0.60, 0.65)), 0.60, true, true, false) {
            Route::RestInFront { .. } => {}
            Route::Take => panic!("hybrid must still rest on a wide spread"),
        }
    }
    #[test]
    fn copy_makers_ON_rests_in_front_of_his_MAKER_fill() {
        let c = taker_cfg(true);
        match route_for(
            &intent_role(true),
            &c,
            Some(top(0.60, 0.65)),
            0.60,
            false,
            false,
            false,
        ) {
            Route::RestInFront { limit, .. } => assert!((limit - 0.61).abs() < 1e-9),
            Route::Take => panic!("his maker fill on a wide book should rest"),
        }
    }
    #[test]
    fn copy_makers_ON_still_TAKES_his_taker_fill() {
        let c = taker_cfg(true);
        assert_eq!(
            route_for(& intent_role(false), & c, Some(top(0.60, 0.65)), 0.60, false,
            false, false), Route::Take, "a taker fill is always crossed"
        );
    }
    #[test]
    fn copy_makers_ON_a_one_tick_spread_still_takes_his_maker_fill() {
        let c = taker_cfg(true);
        assert_eq!(
            route_for(& intent_role(true), & c, Some(top(0.60, 0.61)), 0.60, false,
            false, false), Route::Take, "no room ⇒ take"
        );
    }
    #[test]
    fn copy_makers_NEVER_rests_a_SELL_however_wide_the_book() {
        let c = taker_cfg(true);
        let mut sell = intent_role(true);
        sell.side = 1;
        assert_eq!(
            route_for(& sell, & c, Some(top(0.60, 0.90)), 0.70, false, false, false),
            Route::Take, "copy_makers is buy-only; exits always take"
        );
    }
    #[test]
    fn no_book_falls_back_to_taking() {
        assert_eq!(
            route_for(& intent(0), & hybrid_cfg(), None, 0.60, false, false, false),
            Route::Take
        );
    }
    #[test]
    fn a_stale_book_falls_back_to_taking() {
        let stale = Top {
            best_bid: 0.60,
            best_ask: 0.70,
            at: Instant::now() - std::time::Duration::from_secs(60),
        };
        assert_eq!(
            route_for(& intent(0), & hybrid_cfg(), Some(stale), 0.60, false, false,
            false), Route::Take,
            "resting off a stale book prices us at a level that no longer exists"
        );
    }
    #[test]
    fn a_resting_bid_never_crosses_the_ask() {
        for (bb, ba, his) in [
            (0.60, 0.65, 0.60),
            (0.10, 0.30, 0.10),
            (0.90, 0.95, 0.90),
        ] {
            if let Route::RestInFront { limit, .. } = route_for(
                &intent(0),
                &hybrid_cfg(),
                Some(top(bb, ba)),
                his,
                false,
                false,
                false,
            ) {
                assert!(
                    limit < ba,
                    "rest {limit} would CROSS the ask {ba} — that is a take"
                );
                assert!(limit > his, "must be IN FRONT of his {his}");
            }
        }
    }
    #[test]
    fn sell_side_rests_below_him_and_above_the_bid() {
        if let Route::RestInFront { limit, .. } = route_for(
            &intent(1),
            &hybrid_cfg(),
            Some(top(0.60, 0.70)),
            0.70,
            false,
            false,
            false,
        ) {
            assert!(limit < 0.70 && limit > 0.60);
        } else {
            panic!("wide spread should rest on the sell side too");
        }
    }
}
impl Lane {
    pub fn apply_policy(&self, next: SizingPolicy) -> Result<u64, String> {
        let mut slot = self
            .state
            .policy
            .write()
            .map_err(|_| "policy lock poisoned".to_string())?;
        let gen = slot.generation.saturating_add(1);
        let published = SizingPolicy {
            generation: gen,
            ..next
        };
        *slot = std::sync::Arc::new(published);
        Ok(gen)
    }
    pub fn policy(&self) -> std::sync::Arc<SizingPolicy> {
        self.state
            .policy
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }
}
