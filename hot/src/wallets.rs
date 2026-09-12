use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalletSpec {
    pub name: String,
    pub leader: String,
    pub seed_usd: f64,
    pub pct: f64,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "stop_none")]
    pub stop_mode: String,
    #[serde(default = "band_min")]
    pub min_buy_price: f64,
    #[serde(default = "band_max")]
    pub max_buy_price: f64,
    #[serde(default = "eff_cap")]
    pub max_effective_pct: f64,
    #[serde(default = "no")]
    pub compound: bool,
    #[serde(default = "yes")]
    pub copy_makers: bool,
    #[serde(default)]
    pub lane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buy_slippage_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sell_slippage_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sell_floor_frac: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_order_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_fill_floor: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sell_all_frac: Option<f64>,
    #[serde(default)]
    pub copy_maker_sells: Option<bool>,
    #[serde(default)]
    pub exclude_political: bool,
    #[serde(default)]
    pub leader_max_order_usd: Option<f64>,
    #[serde(default)]
    pub leader_peak_exposure_usd: Option<f64>,
    #[serde(default)]
    pub risk: Option<crate::risk::RiskConfig>,
}
impl WalletSpec {
    pub fn lane_id(&self) -> String {
        match crate::laneid::check(self.lane_id.as_deref(), &self.leader) {
            crate::laneid::Check::Agrees => self.lane_id.clone().unwrap_or_default(),
            crate::laneid::Check::Mint(id) => id,
            crate::laneid::Check::Conflict { stored, expected } => {
                eprintln!(
                    "[laneid] ⛔ {}: stored id {stored} does not match its leader \
                           (expects {expected}). A leader swapped under an existing lane \
                           is a DIFFERENT lane wearing its name, history and ledger book. \
                           Not repaired — an operator has to say which was edited.",
                    self.name
                );
                stored
            }
            crate::laneid::Check::Malformed(bad) => {
                eprintln!(
                    "[laneid] ⛔ {}: stored id {bad:?} is malformed — not \
                           overwritten, because a rewrite would orphan every row already \
                           carrying it.",
                    self.name
                );
                bad
            }
        }
    }
    pub fn copy_maker_sells(&self) -> bool {
        self.copy_maker_sells.unwrap_or(self.copy_makers)
    }
    pub fn leader_stats(&self) -> Result<crate::budget::LeaderStats, String> {
        let (Some(max_order_usd), Some(peak_exposure_usd)) = (
            self.leader_max_order_usd,
            self.leader_peak_exposure_usd,
        ) else {
            return Err(
                format!(
                    "wallet {}: leader_max_order_usd and leader_peak_exposure_usd are REQUIRED \
                 — they describe {}'s own order flow and decide whether this lane's caps \
                 truncate his biggest orders. Measure them with \
                 `tools/leader_stats.py {}`; do NOT copy another lane's numbers.",
                    self.name, self.name, self.leader
                ),
            );
        };
        let s = crate::budget::LeaderStats {
            max_order_usd,
            peak_exposure_usd,
        };
        s.validate(&self.name)?;
        Ok(s)
    }
}
fn yes() -> bool {
    true
}
fn no() -> bool {
    false
}
fn stop_none() -> String {
    "none".into()
}
fn band_min() -> f64 {
    0.02
}
fn band_max() -> f64 {
    0.95
}
fn eff_cap() -> f64 {
    0.05
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WalletFile {
    #[serde(default)]
    pub wallets: Vec<WalletSpec>,
}
#[derive(Clone)]
pub struct WalletRegistry {
    path: String,
    inner: Arc<RwLock<Vec<WalletSpec>>>,
    load_fault: Option<String>,
}
impl WalletRegistry {
    pub fn load_or_seed(path: &str, boot: Vec<WalletSpec>) -> Self {
        let (specs, load_fault) = match std::fs::read_to_string(path) {
            Ok(raw) => {
                match serde_json::from_str::<WalletFile>(&raw)
                    .map_err(|e| e.to_string())
                    .and_then(|f| validate_pool(&f.wallets).map(|_| f.wallets))
                {
                    Ok(w) => (w, None),
                    Err(e) => {
                        eprintln!(
                            "[wallets] ⛔ {path} is PRESENT but unusable ({e}) — refusing \
                               to fall back to the boot set, which is a DIFFERENT pool"
                        );
                        (
                            boot.clone(),
                            Some(
                                format!(
                                    "the wallet registry {path} exists but cannot be used ({e}). It is \
not the same pool as the TOML, so booting from the TOML would silently drop or re-parameter \
live lanes. Restore it from a backup, or move it aside to start from the TOML deliberately."
                                ),
                            ),
                        )
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let reg = Self {
                    path: path.to_string(),
                    inner: Arc::new(RwLock::new(boot.clone())),
                    load_fault: None,
                };
                let _ = reg.persist();
                (boot, None)
            }
            Err(e) => {
                eprintln!(
                    "[wallets] ⛔ cannot READ {path} ({e}) — NOT overwriting it"
                );
                (
                    boot.clone(),
                    Some(
                        format!(
                            "cannot read the wallet registry {path}: {e}. It has deliberately NOT \
been overwritten. Fix the permissions or the disk, then restart."
                        ),
                    ),
                )
            }
        };
        Self {
            path: path.to_string(),
            inner: Arc::new(RwLock::new(specs)),
            load_fault,
        }
    }
    pub fn load_fault(&self) -> Option<&str> {
        self.load_fault.as_deref()
    }
    pub fn specs(&self) -> Vec<WalletSpec> {
        self.inner.read().unwrap().clone()
    }
    pub fn seed_of(&self, name: &str) -> Option<f64> {
        self.inner.read().unwrap().iter().find(|w| w.name == name).map(|w| w.seed_usd)
    }
    pub fn reload(&self) -> bool {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(r) => r,
            Err(_) => return false,
        };
        let next = match serde_json::from_str::<WalletFile>(&raw)
            .map_err(|e| e.to_string())
            .and_then(|f| validate_pool(&f.wallets).map(|_| f.wallets))
        {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[wallets] {} rejected ({e}) — pool unchanged", self.path);
                return false;
            }
        };
        let mut g = self.inner.write().unwrap();
        if *g == next {
            return false;
        }
        *g = next;
        true
    }
    fn write_file(&self, specs: &[WalletSpec]) -> std::io::Result<()> {
        let file = WalletFile {
            wallets: specs.to_vec(),
        };
        let body = serde_json::to_string_pretty(&file).unwrap_or_default();
        let tmp = format!("{}.tmp", self.path);
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(body.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        if let Some(dir) = std::path::Path::new(&self.path).parent() {
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    }
    pub fn mint_missing_ids(&self) -> std::io::Result<usize> {
        let mut g = self.inner.write().unwrap();
        let mut candidate = g.clone();
        let mut minted = 0usize;
        for w in candidate.iter_mut() {
            if w.lane_id.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
                w.lane_id = Some(crate::laneid::mint(&w.leader));
                minted += 1;
            }
        }
        if minted == 0 {
            return Ok(0);
        }
        self.write_file(&candidate)?;
        *g = candidate;
        Ok(minted)
    }
    pub fn duplicate_ids(&self) -> Vec<(String, Vec<String>)> {
        let g = self.inner.read().unwrap();
        let mut by_id: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        for w in g.iter() {
            if !w.enabled {
                continue;
            }
            by_id.entry(w.lane_id()).or_default().push(w.name.clone());
        }
        let mut dupes: Vec<(String, Vec<String>)> = by_id
            .into_iter()
            .filter(|(_, names)| names.len() > 1)
            .collect();
        dupes.sort();
        dupes
    }
    pub fn id_map(&self) -> std::collections::HashMap<String, String> {
        self.inner
            .read()
            .unwrap()
            .iter()
            .map(|w| (w.name.clone(), w.lane_id()))
            .collect()
    }
    pub fn persist(&self) -> std::io::Result<()> {
        let specs = self.inner.read().unwrap().clone();
        self.write_file(&specs)
    }
    pub fn mutate(
        &self,
        edit: impl FnOnce(&mut Vec<WalletSpec>),
    ) -> std::io::Result<()> {
        let mut g = self.inner.write().unwrap();
        edit(&mut g);
        self.write_file(&g)
    }
    pub fn transact(
        &self,
        edit: impl FnOnce(&[WalletSpec]) -> Result<Vec<WalletSpec>, String>,
    ) -> Result<(), String> {
        let mut g = self.inner.write().unwrap();
        let mut next = edit(&g)?;
        for w in next.iter_mut() {
            if w.lane_id.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
                w.lane_id = Some(crate::laneid::mint(&w.leader));
            }
        }
        validate_pool(&next)?;
        self.write_file(&next).map_err(|e| format!("cannot persist wallets: {e}"))?;
        *g = next;
        Ok(())
    }
}
pub fn validate_pool(specs: &[WalletSpec]) -> Result<(), String> {
    let mut seen_names: Vec<String> = Vec::new();
    let mut seen_leaders: Vec<(String, String)> = Vec::new();
    let mut seen_ids: Vec<(String, String)> = Vec::new();
    for w in specs {
        let id = match w.lane_id.as_deref() {
            Some(i) if !i.is_empty() => i,
            _ => continue,
        };
        if !crate::laneid::is_valid(id) {
            return Err(format!("wallet {:?}: malformed lane_id {id:?}", w.name));
        }
        if let Some((other, _)) = seen_ids.iter().find(|(_, i)| i == id) {
            return Err(
                format!(
                    "lane_id {id} is claimed by BOTH {other:?} and {:?} — two lanes sharing \
                 an identity would join their money-bearing rows into one book",
                    w.name
                ),
            );
        }
        seen_ids.push((w.name.clone(), id.to_string()));
    }
    for w in specs {
        let name = w.name.trim();
        if name.is_empty() {
            return Err("a wallet with no name cannot be a ledger key".into());
        }
        if seen_names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            return Err(format!("duplicate wallet name {name:?}"));
        }
        seen_names.push(name.to_string());
        if !w.enabled {
            continue;
        }
        let leader = w.leader.to_ascii_lowercase();
        if let Some((other, _)) = seen_leaders.iter().find(|(_, l)| l == &leader) {
            return Err(
                format!(
                    "wallets {other:?} and {name:?} both copy leader {leader} — one leader may back only one wallet, or every fill is copied twice"
                ),
            );
        }
        seen_leaders.push((name.to_string(), leader));
    }
    Ok(())
}
#[derive(Debug, Clone, PartialEq)]
pub struct Reprice {
    pub name: String,
    pub spec: WalletSpec,
}
pub struct Reconcile {
    pub to_add: Vec<WalletSpec>,
    pub to_retire: Vec<String>,
    pub to_resume: Vec<WalletSpec>,
}
pub fn is_stopped(stop_mode: &str) -> bool {
    !matches!(stop_mode.trim().to_ascii_lowercase().as_str(), "" | "none")
}
pub fn reconcile(pool: &[WalletSpec], live: &[String], retired: &[String]) -> Reconcile {
    let is_live = |n: &str| live.iter().any(|l| l == n);
    let is_retired = |n: &str| retired.iter().any(|r| r == n);
    let to_add = pool
        .iter()
        .filter(|w| w.enabled && !is_live(&w.name))
        .cloned()
        .collect();
    let to_resume = pool
        .iter()
        .filter(|w| {
            w.enabled && is_live(&w.name) && is_retired(&w.name)
                && !is_stopped(&w.stop_mode)
        })
        .cloned()
        .collect();
    let to_retire = live
        .iter()
        .filter(|n| !pool.iter().any(|w| &w.name == *n && w.enabled))
        .cloned()
        .collect();
    Reconcile {
        to_add,
        to_retire,
        to_resume,
    }
}
pub fn repricing(
    pool: &[WalletSpec],
    applied: &[(String, f64, f64, f64, bool, bool, bool, f64, f64)],
) -> Vec<Reprice> {
    let mut out = Vec::new();
    for w in pool.iter().filter(|w| w.enabled) {
        let Some((_, pct, seed, ceil, comp, rest_buys, rest_sells, lo, hi)) = applied
            .iter()
            .find(|(n, ..)| n == &w.name) else { continue };
        let differs = (w.pct - pct).abs() > 1e-12 || w.copy_makers != *rest_buys
            || w.copy_maker_sells() != *rest_sells || (w.seed_usd - seed).abs() > 1e-9
            || (w.max_effective_pct - ceil).abs() > 1e-12
            || (w.min_buy_price - lo).abs() > 1e-12
            || (w.max_buy_price - hi).abs() > 1e-12 || w.compound != *comp;
        if differs {
            out.push(Reprice {
                name: w.name.clone(),
                spec: w.clone(),
            });
        }
    }
    out
}
