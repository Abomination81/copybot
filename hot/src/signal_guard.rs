use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
pub const ORDER_TTL_SECS: i64 = 24 * 60 * 60;
pub const MARKET_TTL_SECS: i64 = 24 * 60 * 60;
const _: () = assert!(ORDER_TTL_SECS >= MARKET_TTL_SECS);
const MAX_FUTURE_SKEW_SECS: i64 = 300;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    DuplicateOrder,
    MarketCooldown { remaining_secs: i64 },
    InvalidIdentity,
    Persistence(String),
}
#[derive(Debug, Serialize, Deserialize, Clone)]
struct Entry {
    t: i64,
    salt: String,
    condition: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    lane: String,
    #[serde(default)]
    his_filled: f64,
    #[serde(default)]
    our_copied: f64,
    #[serde(default)]
    our_released: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    token: String,
}
#[derive(Debug, Clone)]
struct Order {
    t: i64,
    condition: [u8; 32],
    lane: String,
    his_filled: f64,
    our_copied: f64,
    our_released: f64,
    token: String,
}
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Progress {
    pub his_filled: f64,
    pub our_copied: f64,
}
struct State {
    orders: HashMap<[u8; 32], Order>,
    markets: HashMap<(String, [u8; 32]), (i64, [u8; 32], String)>,
    file: File,
}
pub struct SignalGuard {
    path: PathBuf,
    inner: Mutex<State>,
}
fn parse_key(value: &str, field: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(value).map_err(|e| format!("invalid {field} hex: {e}"))?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| format!("invalid {field} length (expected 32 bytes)"))?;
    if key.iter().all(|&b| b == 0) {
        return Err(format!("zero {field}"));
    }
    Ok(key)
}
fn sync_parent(path: &Path) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| format!("sync signal-guard directory {}: {e}", parent.display()))
}
fn write_compacted(path: &Path, entries: &[Entry]) -> Result<(), String> {
    let tmp = PathBuf::from(format!("{}.compact.tmp", path.display()));
    let mut out = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)
        .map_err(|e| format!("open {}: {e}", tmp.display()))?;
    for entry in entries {
        serde_json::to_writer(&mut out, entry)
            .map_err(|e| format!("serialize {}: {e}", tmp.display()))?;
        out.write_all(b"\n").map_err(|e| format!("write {}: {e}", tmp.display()))?;
    }
    out.flush()
        .and_then(|_| out.sync_data())
        .map_err(|e| format!("sync {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("replace {}: {e}", path.display()))?;
    sync_parent(path)
}
fn expire(state: &mut State, now: i64) {
    state.orders.retain(|_, o| now.saturating_sub(o.t) < ORDER_TTL_SECS);
    state.markets.retain(|_, (t, _, _)| now.saturating_sub(*t) < MARKET_TTL_SECS);
}
fn market_gate(
    state: &State,
    salt: &[u8; 32],
    condition: &[u8; 32],
    lane: &str,
    token: &str,
    now: i64,
) -> Result<(), Refusal> {
    for key in [(String::new(), *condition), (lane.to_string(), *condition)] {
        if let Some((t, owner, owner_token)) = state.markets.get(&key) {
            if owner == salt {
                continue;
            }
            if !owner_token.is_empty() && owner_token == token {
                continue;
            }
            return Err(Refusal::MarketCooldown {
                remaining_secs: (MARKET_TTL_SECS - now.saturating_sub(*t)).max(0),
            });
        }
    }
    Ok(())
}
impl SignalGuard {
    pub fn open(path: impl AsRef<Path>, now: i64) -> Result<Self, String> {
        Self::open_with_wal(path, None::<&Path>, now)
    }
    pub fn open_with_wal(
        path: impl AsRef<Path>,
        wal: Option<impl AsRef<Path>>,
        now: i64,
    ) -> Result<Self, String> {
        Self::open_absorbing(path, wal, now).map(|(g, _)| g)
    }
    pub fn open_absorbing(
        path: impl AsRef<Path>,
        wal: Option<impl AsRef<Path>>,
        now: i64,
    ) -> Result<(Self, usize), String> {
        let path = path.as_ref().to_path_buf();
        let wal_path = wal.map(|w| w.as_ref().to_path_buf());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| {
                    format!("create signal-guard directory {}: {e}", parent.display())
                })?;
        }
        let mut orders: HashMap<[u8; 32], Order> = HashMap::new();
        let mut markets: HashMap<(String, [u8; 32]), (i64, [u8; 32], String)> = HashMap::new();
        let mut compacted = Vec::new();
        let mut saw_any = path.exists();
        let mut wal_rows = 0usize;
        for (src, is_wal) in [(Some(path.clone()), false), (wal_path.clone(), true)] {
            let Some(src) = src else { continue };
            if !src.exists() {
                continue;
            }
            saw_any = true;
            let input = File::open(&src)
                .map_err(|e| format!("open signal guard {}: {e}", src.display()))?;
            for (line_no, line) in BufReader::new(input).lines().enumerate() {
                let line = match line {
                    Ok(l) => l,
                    Err(e) if is_wal => {
                        eprintln!(
                            "[signal-guard] wal {} line {}: {e} — stopping replay here",
                            src.display(), line_no + 1
                        );
                        break;
                    }
                    Err(e) => {
                        return Err(
                            format!("read {}:{}: {e}", src.display(), line_no + 1),
                        );
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                if is_wal {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                        if v.get("ev").is_some() {
                            continue;
                        }
                    }
                }
                let entry: Entry = match serde_json::from_str(&line) {
                    Ok(e) => e,
                    Err(e) if is_wal => {
                        eprintln!(
                            "[signal-guard] wal {} line {}: {e} — stopping replay here",
                            src.display(), line_no + 1
                        );
                        break;
                    }
                    Err(e) => {
                        return Err(
                            format!(
                                "corrupt signal guard {}:{}: {e}", src.display(), line_no +
                                1
                            ),
                        );
                    }
                };
                if entry.t > now + MAX_FUTURE_SKEW_SECS {
                    return Err(
                        format!(
                            "signal guard {}:{} is {}s in the future", src.display(),
                            line_no + 1, entry.t - now
                        ),
                    );
                }
                let salt = parse_key(&entry.salt, "salt")?;
                let condition = parse_key(&entry.condition, "condition")?;
                if now.saturating_sub(entry.t) < MARKET_TTL_SECS {
                    let slot = orders
                        .entry(salt)
                        .or_insert(Order {
                            t: entry.t,
                            condition,
                            lane: entry.lane.clone(),
                            his_filled: 0.0,
                            our_copied: 0.0,
                            our_released: 0.0,
                            token: entry.token.clone(),
                        });
                    if slot.token.is_empty() {
                        slot.token = entry.token.clone();
                    }
                    slot.t = slot.t.max(entry.t);
                    slot.his_filled = slot.his_filled.max(entry.his_filled);
                    slot.our_copied = slot.our_copied.max(entry.our_copied);
                    slot.our_released = slot.our_released.max(entry.our_released);
                    if !entry.lane.is_empty() {
                        slot.lane = entry.lane.clone();
                    }
                    if entry.our_copied > 0.0 {
                        let tok = entry.token.clone();
                        markets
                            .entry((entry.lane.clone(), condition))
                            .and_modify(|v| {
                                if entry.t > v.0 {
                                    *v = (entry.t, salt, tok.clone());
                                }
                            })
                            .or_insert((entry.t, salt, tok));
                    }
                    if is_wal {
                        wal_rows += 1;
                    }
                    compacted.push(entry);
                }
            }
        }
        if saw_any {
            write_compacted(&path, &compacted)?;
        }
        let existed = path.exists();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .map_err(|e| {
                format!("open signal guard {} for append: {e}", path.display())
            })?;
        if !existed {
            file.sync_data()
                .map_err(|e| format!("sync new signal guard {}: {e}", path.display()))?;
            sync_parent(&path)?;
        }
        Ok((
            Self {
                path,
                inner: Mutex::new(State { orders, markets, file }),
            },
            wal_rows,
        ))
    }
    pub fn observe(
        &self,
        salt: &[u8; 32],
        condition: &[u8; 32],
        lane: &str,
        fill_size: f64,
        his_order_size: f64,
        now: i64,
    ) -> Result<Progress, Refusal> {
        if salt.iter().all(|&b| b == 0) || condition.iter().all(|&b| b == 0) {
            return Err(Refusal::InvalidIdentity);
        }
        let mut state = self
            .inner
            .lock()
            .map_err(|_| Refusal::Persistence("signal-guard mutex poisoned".into()))?;
        expire(&mut state, now);
        if let Some(existing) = state.orders.get(salt) {
            if existing.condition != *condition {
                return Err(Refusal::InvalidIdentity);
            }
        }
        let ceiling = if his_order_size > 0.0 { his_order_size } else { f64::INFINITY };
        let slot = state
            .orders
            .entry(*salt)
            .or_insert(Order {
                t: now,
                condition: *condition,
                lane: lane.to_string(),
                his_filled: 0.0,
                our_copied: 0.0,
                our_released: 0.0,
                token: String::new(),
            });
        slot.t = now;
        slot.his_filled = (slot.his_filled + fill_size.max(0.0)).min(ceiling);
        Ok(Progress {
            his_filled: slot.his_filled,
            our_copied: (slot.our_copied - slot.our_released).max(0.0),
        })
    }
    pub fn check(
        &self,
        salt: &[u8; 32],
        condition: &[u8; 32],
        lane: &str,
        token: &str,
        now: i64,
    ) -> Result<(), Refusal> {
        if salt.iter().all(|&b| b == 0) || condition.iter().all(|&b| b == 0) {
            return Err(Refusal::InvalidIdentity);
        }
        let mut state = self
            .inner
            .lock()
            .map_err(|_| Refusal::Persistence("signal-guard mutex poisoned".into()))?;
        expire(&mut state, now);
        market_gate(&state, salt, condition, lane, token, now)
    }
    pub fn commit(
        &self,
        salt: &[u8; 32],
        condition: &[u8; 32],
        lane: &str,
        token: &str,
        shares: f64,
        now: i64,
    ) -> Result<(), Refusal> {
        self.commit_inner(salt, condition, lane, token, shares, now, None, None)
    }
    pub fn commit_with(
        &self,
        salt: &[u8; 32],
        condition: &[u8; 32],
        lane: &str,
        token: &str,
        shares: f64,
        now: i64,
        wal: &crate::wal::Wal,
        extra: Option<&[u8]>,
    ) -> Result<(), Refusal> {
        self.commit_inner(salt, condition, lane, token, shares, now, Some(wal), extra)
    }
    fn commit_inner(
        &self,
        salt: &[u8; 32],
        condition: &[u8; 32],
        lane: &str,
        token: &str,
        shares: f64,
        now: i64,
        wal: Option<&crate::wal::Wal>,
        extra: Option<&[u8]>,
    ) -> Result<(), Refusal> {
        if salt.iter().all(|&b| b == 0) || condition.iter().all(|&b| b == 0) {
            return Err(Refusal::InvalidIdentity);
        }
        if !(shares > 0.0) {
            return Ok(());
        }
        let mut state = self
            .inner
            .lock()
            .map_err(|_| Refusal::Persistence("signal-guard mutex poisoned".into()))?;
        expire(&mut state, now);
        market_gate(&state, salt, condition, lane, token, now)?;
        let (his_filled, already, released) = match state.orders.get(salt) {
            Some(o) if o.condition != *condition => return Err(Refusal::InvalidIdentity),
            Some(o) => (o.his_filled, o.our_copied, o.our_released),
            None => (0.0, 0.0, 0.0),
        };
        let entry = Entry {
            t: now,
            salt: hex::encode(salt),
            condition: hex::encode(condition),
            lane: lane.to_string(),
            his_filled,
            our_copied: already + shares,
            our_released: released,
            token: token.to_string(),
        };
        let line = serde_json::to_vec(&entry)
            .map_err(|e| Refusal::Persistence(format!("serialize signal guard: {e}")))?;
        match wal {
            Some(w) => {
                let mut rows: Vec<&[u8]> = Vec::with_capacity(2);
                rows.push(&line);
                if let Some(x) = extra {
                    rows.push(x);
                }
                w.append_atomic(&rows).map_err(Refusal::Persistence)?;
            }
            None => {
                state
                    .file
                    .write_all(&line)
                    .and_then(|_| state.file.write_all(b"\n"))
                    .and_then(|_| state.file.flush())
                    .and_then(|_| state.file.sync_data())
                    .map_err(|e| Refusal::Persistence(
                        format!("write {}: {e}", self.path.display()),
                    ))?;
            }
        }
        let slot = state
            .orders
            .entry(*salt)
            .or_insert(Order {
                t: now,
                condition: *condition,
                lane: lane.to_string(),
                his_filled,
                our_copied: 0.0,
                our_released: 0.0,
                token: token.to_string(),
            });
        if slot.token.is_empty() {
            slot.token = token.to_string();
        }
        slot.t = now;
        slot.our_copied += shares;
        slot.lane = lane.to_string();
        state
            .markets
            .insert((lane.to_string(), *condition), (now, *salt, token.to_string()));
        Ok(())
    }
    pub fn release(
        &self,
        salt: &[u8; 32],
        condition: &[u8; 32],
        lane: &str,
        shares: f64,
        now: i64,
    ) -> Result<(), Refusal> {
        if salt.iter().all(|&b| b == 0) || condition.iter().all(|&b| b == 0) {
            return Err(Refusal::InvalidIdentity);
        }
        if !(shares > 0.0) {
            return Ok(());
        }
        let mut state = self
            .inner
            .lock()
            .map_err(|_| Refusal::Persistence("signal-guard mutex poisoned".into()))?;
        let Some(o) = state.orders.get(salt) else { return Ok(()) };
        if o.condition != *condition {
            return Err(Refusal::InvalidIdentity);
        }
        let outstanding = (o.our_copied - o.our_released).max(0.0);
        let give_back = shares.min(outstanding);
        if give_back <= 0.0 {
            return Ok(());
        }
        let entry = Entry {
            t: now,
            salt: hex::encode(salt),
            condition: hex::encode(condition),
            lane: lane.to_string(),
            his_filled: o.his_filled,
            our_copied: o.our_copied,
            our_released: o.our_released + give_back,
            token: o.token.clone(),
        };
        let line = serde_json::to_vec(&entry)
            .map_err(|e| Refusal::Persistence(format!("serialize signal guard: {e}")))?;
        state
            .file
            .write_all(&line)
            .and_then(|_| state.file.write_all(b"\n"))
            .and_then(|_| state.file.flush())
            .and_then(|_| state.file.sync_data())
            .map_err(|e| Refusal::Persistence(
                format!("write {}: {e}", self.path.display()),
            ))?;
        if let Some(slot) = state.orders.get_mut(salt) {
            slot.our_released += give_back;
            slot.t = now;
        }
        Ok(())
    }
    pub fn copied(&self, salt: &[u8; 32]) -> f64 {
        self.inner
            .lock()
            .ok()
            .and_then(|s| {
                s.orders.get(salt).map(|o| (o.our_copied - o.our_released).max(0.0))
            })
            .unwrap_or(0.0)
    }
    pub fn active_markets(&self) -> Result<usize, Refusal> {
        self.inner
            .lock()
            .map(|s| s.markets.len())
            .map_err(|_| Refusal::Persistence("signal-guard mutex poisoned".into()))
    }
}
#[cfg(test)]
mod tests {
    const TOK: &str = "tok-A";
    const TOK_OTHER: &str = "tok-B";
    use super::*;
    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(
                format!(
                    "copybot-signal-{name}-{}-{}.jsonl", std::process::id(), crate
                    ::ledger::now_secs()
                ),
            )
    }
    fn key(n: u8) -> [u8; 32] {
        let mut out = [0u8; 32];
        out[31] = n;
        out
    }
    const L: &str = "example_lane_26";
    fn first_buy(g: &SignalGuard, salt: u8, cond: u8, shares: f64, now: i64) {
        g.observe(&key(salt), &key(cond), L, shares, shares, now).unwrap();
        g.commit(&key(salt), &key(cond), L, TOK, shares, now).unwrap();
    }
    #[test]
    fn a_CANCELLED_rest_gives_its_commitment_back_so_the_next_tranche_sizes_whole() {
        let path = temp("cancel-release");
        let g = SignalGuard::open(&path, 1_000).unwrap();
        let p = g.observe(&key(1), &key(9), L, 100.0, 1_000.0, 1_000).unwrap();
        assert_eq!(p.our_copied, 0.0, "nothing copied yet");
        g.commit(&key(1), &key(9), L, TOK, 40.0, 1_000).unwrap();
        assert_eq!(
            g.observe(& key(1), & key(9), L, 0.0, 1_000.0, 1_001).unwrap().our_copied,
            40.0, "the rest is committed"
        );
        g.release(&key(1), &key(9), L, 40.0, 1_002).unwrap();
        assert_eq!(
            g.observe(& key(1), & key(9), L, 0.0, 1_000.0, 1_003).unwrap().our_copied,
            0.0, "a cancelled rest must not suppress the next clip"
        );
        drop(g);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn a_FAK_that_partially_fills_must_give_back_what_the_VENUE_KILLED() {
        let path = temp("fak-partial");
        let g = SignalGuard::open(&path, 1_000).unwrap();
        g.observe(&key(2), &key(8), L, 1_320.0, 5_000.0, 1_000).unwrap();
        g.commit(&key(2), &key(8), L, TOK, 264.0, 1_000).unwrap();
        assert_eq!(
            g.observe(& key(2), & key(8), L, 0.0, 5_000.0, 1_001).unwrap().our_copied,
            264.0, "the whole order is committed up front"
        );
        g.release(&key(2), &key(8), L, 264.0 - 8.0, 1_002).unwrap();
        assert_eq!(
            g.observe(& key(2), & key(8), L, 0.0, 5_000.0, 1_003).unwrap().our_copied,
            8.0, "a killed remainder must not suppress the rest of his order"
        );
        drop(g);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn a_RESTING_order_must_keep_its_commitment_while_it_is_still_working() {
        let path = temp("resting-keeps");
        let g = SignalGuard::open(&path, 1_000).unwrap();
        g.observe(&key(4), &key(6), L, 1_000.0, 5_000.0, 1_000).unwrap();
        g.commit(&key(4), &key(6), L, TOK, 200.0, 1_000).unwrap();
        g.release(&key(4), &key(6), L, 150.0, 1_001).unwrap();
        assert_eq!(
            g.observe(& key(4), & key(6), L, 0.0, 5_000.0, 1_002).unwrap().our_copied,
            50.0, "the guard would now believe only 50 is spoken for"
        );
        g.commit(&key(4), &key(6), L, TOK, 150.0, 1_003).unwrap();
        assert_eq!(
            g.observe(& key(4), & key(6), L, 0.0, 5_000.0, 1_004).unwrap().our_copied,
            200.0, "committed twice for one clip — this is what the GTC gate prevents"
        );
        drop(g);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn releasing_MORE_than_we_hold_can_never_manufacture_headroom() {
        let path = temp("fak-overrelease");
        let g = SignalGuard::open(&path, 1_000).unwrap();
        g.observe(&key(3), &key(7), L, 500.0, 5_000.0, 1_000).unwrap();
        g.commit(&key(3), &key(7), L, TOK, 100.0, 1_000).unwrap();
        g.release(&key(3), &key(7), L, 10_000.0, 1_001).unwrap();
        assert_eq!(
            g.observe(& key(3), & key(7), L, 0.0, 5_000.0, 1_002).unwrap().our_copied,
            0.0, "clamped at the outstanding commitment, never below zero"
        );
        g.release(&key(3), &key(7), L, 10_000.0, 1_003).unwrap();
        assert_eq!(
            g.observe(& key(3), & key(7), L, 0.0, 5_000.0, 1_004).unwrap().our_copied,
            0.0, "still zero, not negative headroom"
        );
        drop(g);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn a_PARTIALLY_filled_rest_releases_only_the_unfilled_remainder() {
        let path = temp("cancel-partial");
        let g = SignalGuard::open(&path, 1_000).unwrap();
        g.observe(&key(1), &key(9), L, 100.0, 1_000.0, 1_000).unwrap();
        g.commit(&key(1), &key(9), L, TOK, 40.0, 1_000).unwrap();
        g.release(&key(1), &key(9), L, 25.0, 1_002).unwrap();
        assert_eq!(
            g.observe(& key(1), & key(9), L, 0.0, 1_000.0, 1_003).unwrap().our_copied,
            15.0, "only the FILLED part still counts as copied"
        );
        drop(g);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn a_release_can_never_give_back_more_than_was_committed() {
        let path = temp("cancel-overrelease");
        let g = SignalGuard::open(&path, 1_000).unwrap();
        g.observe(&key(1), &key(9), L, 100.0, 1_000.0, 1_000).unwrap();
        g.commit(&key(1), &key(9), L, TOK, 40.0, 1_000).unwrap();
        g.release(&key(1), &key(9), L, 40.0, 1_002).unwrap();
        g.release(&key(1), &key(9), L, 40.0, 1_003).unwrap();
        assert_eq!(
            g.observe(& key(1), & key(9), L, 0.0, 1_000.0, 1_004).unwrap().our_copied,
            0.0, "clamped at zero, never negative"
        );
        drop(g);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn a_NEW_ORDER_on_the_SAME_TOKEN_is_him_adding_and_must_be_COPIED() {
        let path = temp("scale_in");
        let guard = SignalGuard::open(&path, 1_000).unwrap();
        first_buy(&guard, 1, 9, 10.0, 1_000);
        guard.observe(&key(2), &key(9), L, 10.0, 10.0, 1_001).unwrap();
        guard
            .commit(&key(2), &key(9), L, TOK, 10.0, 1_001)
            .expect("⛔ THE BUG: this refused MarketCooldown and cost us the add");
        guard.observe(&key(3), &key(9), L, 10.0, 10.0, 40_000).unwrap();
        guard.commit(&key(3), &key(9), L, TOK, 10.0, 40_000).unwrap();
        drop(guard);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn check_agrees_with_commit_about_who_is_blocked() {
        let path = temp("check_agrees");
        let guard = SignalGuard::open(&path, 1_000).unwrap();
        first_buy(&guard, 1, 9, 10.0, 1_000);
        assert!(
            guard.check(& key(2), & key(9), L, TOK, 1_001).is_ok(),
            "same token: check must let the add through"
        );
        assert!(
            matches!(guard.check(& key(2), & key(9), L, TOK_OTHER, 1_001),
            Err(Refusal::MarketCooldown { .. })),
            "other outcome: check must refuse it, like commit does"
        );
        drop(guard);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn an_OLD_ROW_with_no_token_still_blocks_the_whole_market() {
        let path = temp("legacy_row");
        std::fs::write(
                &path,
                format!(
                    "{{\"t\":1000,\"salt\":\"{}\",\"condition\":\"{}\",\"lane\":\"{L}\",\
              \"his_filled\":10.0,\"our_copied\":10.0}}\n",
                    hex::encode(key(1)), hex::encode(key(9))
                ),
            )
            .unwrap();
        let guard = SignalGuard::open(&path, 1_001).unwrap();
        guard.observe(&key(2), &key(9), L, 10.0, 10.0, 1_001).unwrap();
        assert!(
            matches!(guard.commit(& key(2), & key(9), L, TOK, 10.0, 1_001),
            Err(Refusal::MarketCooldown { .. })),
            "a tokenless row must block every token, not just an empty one"
        );
        drop(guard);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn a_RELEASE_does_not_erase_the_token_and_reinstate_the_cooldown() {
        let path = temp("release_token");
        {
            let g = SignalGuard::open(&path, 1_000).unwrap();
            first_buy(&g, 1, 9, 10.0, 1_000);
            g.release(&key(1), &key(9), L, 4.0, 1_001).unwrap();
        }
        let restarted = SignalGuard::open(&path, 1_002).unwrap();
        restarted.observe(&key(2), &key(9), L, 10.0, 10.0, 1_002).unwrap();
        restarted
            .commit(&key(2), &key(9), L, TOK, 10.0, 1_002)
            .expect("the token must survive a release + restart");
        drop(restarted);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn a_second_ORDER_on_the_OTHER_OUTCOME_of_the_same_market_is_blocked_for_24h() {
        let path = temp("market");
        let guard = SignalGuard::open(&path, 1_000).unwrap();
        first_buy(&guard, 1, 9, 10.0, 1_000);
        guard.observe(&key(2), &key(9), L, 10.0, 10.0, 1_001).unwrap();
        assert!(
            matches!(guard.commit(& key(2), & key(9), L, TOK_OTHER, 10.0, 1_001),
            Err(Refusal::MarketCooldown { .. }))
        );
        drop(guard);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn the_SAME_order_may_keep_TOPPING_UP_the_market_it_owns() {
        let path = temp("topup");
        let guard = SignalGuard::open(&path, 1_000).unwrap();
        first_buy(&guard, 1, 9, 10.0, 1_000);
        guard
            .commit(&key(1), &key(9), L, TOK, 5.0, 1_005)
            .expect("his own order must be able to finish");
        let p = guard.observe(&key(1), &key(9), L, 0.0, 0.0, 1_006).unwrap();
        assert_eq!(p.our_copied, 15.0, "both commits must accumulate");
        drop(guard);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn the_120_TRANCHE_ORDER_converges_on_ONE_correctly_sized_clip() {
        let path = temp("tranches");
        let guard = SignalGuard::open(&path, 1_000).unwrap();
        let pct = 0.015;
        let mut sent = 0.0;
        for i in 0..120 {
            let p = guard
                .observe(&key(1), &key(9), L, 5000.0 / 120.0, 5000.0, 1_000 + i)
                .unwrap();
            let delta = (p.his_filled * pct - p.our_copied).floor();
            if delta > 0.0 {
                guard.commit(&key(1), &key(9), L, TOK, delta, 1_000 + i).unwrap();
                sent += delta;
            }
        }
        assert!((sent - 75.0).abs() <= 1.0, "copied {sent} shares, expected ~75");
        drop(guard);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn his_ORDER_SIZE_caps_the_running_total_against_a_double_counted_tranche() {
        let path = temp("clamp");
        let guard = SignalGuard::open(&path, 1_000).unwrap();
        for _ in 0..10 {
            guard.observe(&key(1), &key(9), L, 400.0, 1000.0, 1_000).unwrap();
        }
        let p = guard.observe(&key(1), &key(9), L, 0.0, 1000.0, 1_000).unwrap();
        assert_eq!(p.his_filled, 1000.0, "cannot exceed the order he signed");
        drop(guard);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn what_we_COPIED_survives_a_process_restart() {
        let path = temp("restart");
        {
            let g = SignalGuard::open(&path, 2_000).unwrap();
            first_buy(&g, 1, 8, 30.0, 2_000);
        }
        let restarted = SignalGuard::open(&path, 2_001).unwrap();
        let p = restarted.observe(&key(1), &key(8), L, 0.0, 0.0, 2_001).unwrap();
        assert_eq!(p.our_copied, 30.0, "a restart must not forget what we bought");
        restarted.observe(&key(2), &key(8), L, 10.0, 10.0, 2_001).unwrap();
        assert!(
            matches!(restarted.commit(& key(2), & key(8), L, TOK_OTHER, 10.0, 2_001),
            Err(Refusal::MarketCooldown { .. }))
        );
        restarted.observe(&key(3), &key(8), L, 10.0, 10.0, 2_002).unwrap();
        restarted
            .commit(&key(3), &key(8), L, TOK, 10.0, 2_002)
            .expect("him adding to the SAME position must survive a restart");
        drop(restarted);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn the_quarantine_is_PER_LANE() {
        let path = temp("lanes");
        let guard = SignalGuard::open(&path, 1_000).unwrap();
        first_buy(&guard, 1, 9, 10.0, 1_000);
        guard.observe(&key(2), &key(9), "example_lane_25", 10.0, 10.0, 1_001).unwrap();
        guard
            .commit(&key(2), &key(9), "example_lane_25", TOK, 10.0, 1_001)
            .expect("a different lane's book is not quarantined by example_lane_26's position");
        guard.observe(&key(3), &key(9), "example_lane_25", 10.0, 10.0, 1_002).unwrap();
        assert!(
            matches!(guard.commit(& key(3), & key(9), "example_lane_25", TOK_OTHER, 10.0, 1_002),
            Err(Refusal::MarketCooldown { .. }))
        );
        drop(guard);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn a_LEGACY_row_with_no_lane_still_blocks_EVERY_lane() {
        let path = temp("legacy");
        std::fs::write(
                &path,
                format!(
                    "{{\"t\":1000,\"salt\":\"{}\",\"condition\":\"{}\",\"our_copied\":5.0}}\n",
                    hex::encode(key(1)), hex::encode(key(9))
                ),
            )
            .unwrap();
        let guard = SignalGuard::open(&path, 1_001).unwrap();
        guard.observe(&key(2), &key(9), "any-lane", 10.0, 10.0, 1_001).unwrap();
        assert!(
            matches!(guard.commit(& key(2), & key(9), "any-lane", TOK, 10.0, 1_001),
            Err(Refusal::MarketCooldown { .. }))
        );
        drop(guard);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn an_OBSERVED_but_never_committed_order_does_NOT_claim_the_market() {
        let path = temp("noclaim");
        let guard = SignalGuard::open(&path, 1_000).unwrap();
        guard.observe(&key(1), &key(9), L, 0.4, 0.4, 1_000).unwrap();
        guard.commit(&key(1), &key(9), L, TOK, 0.0, 1_000).unwrap();
        guard.observe(&key(2), &key(9), L, 50.0, 50.0, 1_001).unwrap();
        guard
            .commit(&key(2), &key(9), L, TOK, 50.0, 1_001)
            .expect("a market we never actually bought is not quarantined");
        drop(guard);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn an_expired_market_is_allowed_again() {
        let path = temp("expiry");
        let guard = SignalGuard::open(&path, 3_000).unwrap();
        first_buy(&guard, 1, 7, 10.0, 3_000);
        first_buy(&guard, 2, 7, 10.0, 3_000 + MARKET_TTL_SECS);
        drop(guard);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn zero_or_corrupt_identity_fails_closed() {
        let path = temp("invalid");
        let guard = SignalGuard::open(&path, 4_000).unwrap();
        assert_eq!(
            guard.commit(& [0u8; 32], & key(1), L, TOK, 1.0, 4_000),
            Err(Refusal::InvalidIdentity)
        );
        assert_eq!(
            guard.observe(& key(1), & [0u8; 32], L, 1.0, 1.0, 4_000),
            Err(Refusal::InvalidIdentity)
        );
        drop(guard);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn an_order_CANNOT_be_re_copied_while_it_still_owns_the_quarantine() {
        let path = temp("ttlgap");
        let guard = SignalGuard::open(&path, 1_000).unwrap();
        first_buy(&guard, 1, 9, 100.0, 1_000);
        let later = 1_000 + MARKET_TTL_SECS - 60;
        let p = guard.observe(&key(1), &key(9), L, 0.0, 0.0, later).unwrap();
        assert_eq!(
            p.our_copied, 100.0,
            "what we bought must not expire before the market it claimed"
        );
        drop(guard);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn one_SALT_may_never_span_two_MARKETS() {
        let path = temp("collide");
        let guard = SignalGuard::open(&path, 6_000).unwrap();
        first_buy(&guard, 1, 9, 10.0, 6_000);
        assert_eq!(
            guard.observe(& key(1), & key(8), L, 10.0, 10.0, 6_001),
            Err(Refusal::InvalidIdentity)
        );
        assert_eq!(
            guard.commit(& key(1), & key(8), L, TOK, 10.0, 6_001),
            Err(Refusal::InvalidIdentity)
        );
        drop(guard);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn a_corrupt_wal_refuses_to_boot() {
        let path = temp("corrupt");
        std::fs::write(&path, b"not json\n").unwrap();
        assert!(SignalGuard::open(& path, 5_000).is_err());
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn a_REJECTED_buy_releases_its_commitment_so_the_next_tranche_retries_it() {
        let path = temp("release");
        let g = SignalGuard::open(&path, 1_000).unwrap();
        g.observe(&key(1), &key(9), L, 100.0, 1_000.0, 1_000).unwrap();
        g.commit(&key(1), &key(9), L, TOK, 40.0, 1_000).unwrap();
        assert_eq!(g.copied(& key(1)), 40.0);
        g.release(&key(1), &key(9), L, 40.0, 1_001).unwrap();
        assert_eq!(g.copied(& key(1)), 0.0, "the refused shares must come back");
        let p = g.observe(&key(1), &key(9), L, 0.0, 1_000.0, 1_002).unwrap();
        assert_eq!(p.our_copied, 0.0);
        drop(g);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn a_release_SURVIVES_a_restart() {
        let path = temp("release_restart");
        {
            let g = SignalGuard::open(&path, 1_000).unwrap();
            g.observe(&key(1), &key(9), L, 100.0, 1_000.0, 1_000).unwrap();
            g.commit(&key(1), &key(9), L, TOK, 40.0, 1_000).unwrap();
            g.release(&key(1), &key(9), L, 40.0, 1_001).unwrap();
        }
        let g = SignalGuard::open(&path, 1_002).unwrap();
        assert_eq!(g.copied(& key(1)), 0.0, "the release must replay too");
        drop(g);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn a_release_can_NEVER_exceed_what_was_committed() {
        let path = temp("release_clamp");
        let g = SignalGuard::open(&path, 1_000).unwrap();
        g.observe(&key(1), &key(9), L, 100.0, 1_000.0, 1_000).unwrap();
        g.commit(&key(1), &key(9), L, TOK, 10.0, 1_000).unwrap();
        g.release(&key(1), &key(9), L, 999.0, 1_001).unwrap();
        assert_eq!(g.copied(& key(1)), 0.0, "clamped, never negative");
        g.release(&key(1), &key(9), L, 5.0, 1_002).unwrap();
        assert_eq!(g.copied(& key(1)), 0.0, "releasing again is a no-op");
        drop(g);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn a_PARTIAL_release_leaves_the_rest_committed() {
        let path = temp("release_partial");
        let g = SignalGuard::open(&path, 1_000).unwrap();
        g.observe(&key(1), &key(9), L, 100.0, 1_000.0, 1_000).unwrap();
        g.commit(&key(1), &key(9), L, TOK, 40.0, 1_000).unwrap();
        g.release(&key(1), &key(9), L, 15.0, 1_001).unwrap();
        assert!((g.copied(& key(1)) - 25.0).abs() < 1e-9);
        drop(g);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn releasing_does_NOT_lift_the_24h_market_quarantine() {
        let path = temp("release_quarantine");
        let g = SignalGuard::open(&path, 1_000).unwrap();
        first_buy(&g, 1, 9, 10.0, 1_000);
        g.release(&key(1), &key(9), L, 10.0, 1_001).unwrap();
        g.observe(&key(2), &key(9), L, 10.0, 10.0, 1_002).unwrap();
        assert!(
            matches!(g.commit(& key(2), & key(9), L, TOK_OTHER, 10.0, 1_002),
            Err(Refusal::MarketCooldown { .. })),
            "the OTHER OUTCOME stays blocked even after a release"
        );
        drop(g);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn a_release_for_the_WRONG_condition_is_refused() {
        let path = temp("release_wrong");
        let g = SignalGuard::open(&path, 1_000).unwrap();
        g.observe(&key(1), &key(9), L, 100.0, 1_000.0, 1_000).unwrap();
        g.commit(&key(1), &key(9), L, TOK, 40.0, 1_000).unwrap();
        assert!(
            matches!(g.release(& key(1), & key(8), L, 40.0, 1_001),
            Err(Refusal::InvalidIdentity))
        );
        assert_eq!(g.copied(& key(1)), 40.0, "nothing released on a mismatch");
        drop(g);
        let _ = std::fs::remove_file(path);
    }
}
