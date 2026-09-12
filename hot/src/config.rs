use crate::lanes::{Execution, Lane, LaneConfig, Sizing};
use serde::Deserialize;
#[derive(Debug, Deserialize)]
pub struct Root {
    pub bot: Bot,
    #[serde(default)]
    pub feed: Vec<Feed>,
    pub lane: Vec<LaneToml>,
}
#[derive(Debug, Deserialize)]
pub struct Bot {
    #[serde(default = "dry")]
    pub mode: String,
    pub clob_host: String,
    pub funder: String,
    pub signer: String,
    #[serde(default = "sig2")]
    pub signature_type: u8,
    pub events_path: String,
    pub control_path: String,
    #[serde(default = "dflt_ledger")]
    pub ledger_path: String,
    #[serde(default = "dflt_signal_guard")]
    pub signal_guard_path: String,
    #[serde(default = "dflt_recycle_secs")]
    pub recycle_secs: u64,
    #[serde(default)]
    pub txpool_rpc: Option<String>,
    #[serde(default = "yes")]
    pub race_h2: bool,
}
fn dry() -> String {
    "dry".into()
}
fn dflt_ledger() -> String {
    "data/ledger.jsonl".into()
}
fn dflt_signal_guard() -> String {
    "data/signal-guard.jsonl".into()
}
fn yes() -> bool {
    true
}
fn sig2() -> u8 {
    2
}
#[derive(Debug, Deserialize)]
pub struct Feed {
    pub name: String,
    pub url: String,
    #[serde(default = "one")]
    pub sockets: usize,
}
pub fn dflt_recycle_secs() -> u64 {
    0
}
fn one() -> usize {
    1
}
#[derive(Debug, Deserialize)]
pub struct LaneToml {
    pub name: String,
    pub wallet: String,
    #[serde(default)]
    pub enabled: bool,
    pub sizing: SizingToml,
    pub budget: BudgetToml,
    pub execution: ExecToml,
    #[serde(default)]
    pub leader_max_order_usd: Option<f64>,
    #[serde(default)]
    pub leader_peak_exposure_usd: Option<f64>,
    #[serde(default)]
    pub risk: Option<crate::risk::RiskConfig>,
}
#[derive(Debug, Deserialize)]
pub struct SizingToml {
    pub mode: String,
    #[serde(default)]
    pub fixed_shares: f64,
    #[serde(default)]
    pub flat_usd: f64,
    #[serde(default)]
    pub pct: f64,
    #[serde(default = "f005")]
    pub max_effective_pct: f64,
    #[serde(default = "f")]
    pub compound: bool,
    #[serde(default)]
    pub max_usd_per_fill: Option<f64>,
    #[serde(default = "f1")]
    pub min_order_usd: f64,
    #[serde(default = "t")]
    pub min_fill_floor: bool,
}
fn f1() -> f64 {
    1.0
}
fn t() -> bool {
    true
}
fn f() -> bool {
    false
}
#[derive(Debug, Deserialize)]
pub struct BudgetToml {
    #[serde(default)]
    pub bankroll_usd: Option<f64>,
    #[serde(default)]
    pub daily_usd: Option<f64>,
    #[serde(default)]
    pub per_market_usd: Option<f64>,
    #[serde(default)]
    pub max_open_usd: Option<f64>,
    #[serde(default = "f085")]
    pub open_frac: f64,
    #[serde(default = "f060")]
    pub per_market_frac: f64,
    #[serde(default = "f04167")]
    pub per_fill_frac: f64,
    #[serde(default = "f070")]
    pub daily_frac: f64,
    #[serde(default = "f095")]
    pub max_buy_price: f64,
    #[serde(default = "f002")]
    pub min_buy_price: f64,
}
fn f095() -> f64 {
    0.95
}
fn f002() -> f64 {
    0.02
}
fn f005() -> f64 {
    0.05
}
fn f085() -> f64 {
    0.85
}
fn f060() -> f64 {
    0.60
}
fn f04167() -> f64 {
    0.4167
}
fn f070() -> f64 {
    0.70
}
#[derive(Debug, Deserialize)]
pub struct ExecToml {
    #[serde(default = "taker")]
    pub mode: String,
    #[serde(default)]
    pub slippage_c: Option<f64>,
    #[serde(default)]
    pub buy_slippage_c: Option<f64>,
    #[serde(default)]
    pub sell_slippage_c: Option<f64>,
    #[serde(default)]
    pub sell_floor_frac: f64,
    #[serde(default = "f095f")]
    pub sell_all_frac: f64,
    #[serde(default)]
    pub copy_makers: bool,
    #[serde(default)]
    pub copy_maker_sells: bool,
    #[serde(default)]
    pub exclude_political: bool,
}
fn f095f() -> f64 {
    SELL_ALL_ON_ANY_SELL
}
fn taker() -> String {
    "taker".into()
}
const DEFAULT_SLIPPAGE: f64 = 0.02;
pub fn resolve_caps(l: &LaneToml) -> Result<(crate::budget::Caps, bool), String> {
    let (b, s) = (&l.budget, &l.sizing);
    match b.bankroll_usd {
        Some(bankroll) => {
            let mut typed: Vec<&str> = Vec::new();
            if b.daily_usd.is_some() {
                typed.push("budget.daily_usd");
            }
            if b.per_market_usd.is_some() {
                typed.push("budget.per_market_usd");
            }
            if b.max_open_usd.is_some() {
                typed.push("budget.max_open_usd");
            }
            if s.max_usd_per_fill.is_some() {
                typed.push("sizing.max_usd_per_fill");
            }
            if !typed.is_empty() {
                return Err(
                    format!(
                        "lane {}: bankroll_usd is set, so caps are DERIVED — but {} \
                     {} also set by hand. Remove {} (tune the *_frac values instead), \
                     or remove bankroll_usd to go back to absolute caps.",
                        l.name, typed.join(" and "), if typed.len() == 1 { "is" } else {
                        "are" }, if typed.len() == 1 { "it" } else { "them" }
                    ),
                );
            }
            if !bankroll.is_finite() || bankroll <= 0.0 {
                return Err(
                    format!(
                        "lane {}: bankroll_usd is {bankroll} — a lane with no capital must be \
                     disabled, not given a zero budget that reads as unlimited.",
                        l.name
                    ),
                );
            }
            let f = crate::budget::Fracs {
                open: b.open_frac,
                per_market: b.per_market_frac,
                per_fill: b.per_fill_frac,
                daily: b.daily_frac,
            };
            crate::budget::validate_fracs(&f)
                .map_err(|e| format!("lane {}: {e}", l.name))?;
            Ok((crate::budget::derive(bankroll, &f), true))
        }
        None => {
            let need = |v: Option<f64>, what: &str| {
                v.ok_or_else(|| {
                    format!(
                        "lane {}: no bankroll_usd, so budget.{what} is required", l.name
                    )
                })
            };
            Ok((
                crate::budget::Caps {
                    max_open_usd: need(b.max_open_usd, "max_open_usd")?,
                    per_market_usd: need(b.per_market_usd, "per_market_usd")?,
                    daily_usd: need(b.daily_usd, "daily_usd")?,
                    max_usd_per_fill: s.max_usd_per_fill.unwrap_or(250.0),
                },
                false,
            ))
        }
    }
}
pub const RUNTIME_BUY_SLIPPAGE: f64 = 0.15;
pub const RUNTIME_SELL_SLIPPAGE: f64 = 1.0;
pub const RUNTIME_SELL_FLOOR_FRAC: f64 = 0.5;
pub const SELL_ALL_ON_ANY_SELL: f64 = 0.0;
pub const RUNTIME_SELL_ALL_FRAC: f64 = SELL_ALL_ON_ANY_SELL;
pub fn build_runtime_lane(
    spec: &crate::wallets::WalletSpec,
) -> Result<(Lane, crate::risk::RiskConfig), String> {
    let wallet20 = addr20(&spec.leader)?;
    if !(spec.seed_usd > 0.0) {
        return Err(format!("wallet {}: seed_usd must be positive", spec.name));
    }
    if !(spec.pct > 0.0 && spec.pct <= 1.0) {
        return Err(format!("wallet {}: pct must be in (0,1]", spec.name));
    }
    if !(spec.min_buy_price > 0.0 && spec.max_buy_price < 1.0
        && spec.min_buy_price < spec.max_buy_price)
    {
        return Err(
            format!(
                "wallet {}: buy band must be 0 < min ({}) < max ({}) < 1", spec.name,
                spec.min_buy_price, spec.max_buy_price
            ),
        );
    }
    let fracs = crate::budget::Fracs::default();
    crate::budget::validate_fracs(&fracs)
        .map_err(|e| format!("wallet {}: {e}", spec.name))?;
    let caps = crate::budget::derive(spec.seed_usd, &fracs);
    let leader = spec.leader_stats()?;
    let aggressive = spec.max_effective_pct > crate::budget::MAX_EFFECTIVE_PCT;
    match crate::budget::check_pct_fits(&spec.name, spec.pct, &caps, &leader) {
        Ok(()) => {}
        Err(e) if aggressive => {
            eprintln!(
                "[budget] ADVISORY (wallet opted into {:.0}% sizing): {e}", spec
                .max_effective_pct * 100.0
            )
        }
        Err(e) => return Err(e),
    }
    if let Some(adv) = crate::budget::open_cap_advisory(
        &spec.name,
        spec.pct,
        &caps,
        &leader,
    ) {
        eprintln!("[budget] ADVISORY: {adv}");
    }
    let cfg = LaneConfig {
        name: spec.name.clone(),
        wallet20,
        sizing: Sizing::Pct(spec.pct),
        execution: Execution::Taker,
        buy_slippage_c: spec.buy_slippage_c.unwrap_or(RUNTIME_BUY_SLIPPAGE),
        sell_slippage_c: spec.sell_slippage_c.unwrap_or(RUNTIME_SELL_SLIPPAGE),
        copy_maker_sells: spec.copy_maker_sells(),
        sell_floor_frac: spec.sell_floor_frac.unwrap_or(RUNTIME_SELL_FLOOR_FRAC),
        min_order_usd: spec.min_order_usd.unwrap_or(1.0),
        max_usd_per_fill: caps.max_usd_per_fill,
        daily_budget_usd: caps.daily_usd,
        per_market_usd: caps.per_market_usd,
        max_open_usd: caps.max_open_usd,
        max_buy_price: spec.max_buy_price,
        min_buy_price: spec.min_buy_price,
        min_fill_floor: spec.min_fill_floor.unwrap_or(true),
        sell_all_frac: spec.sell_all_frac.unwrap_or(RUNTIME_SELL_ALL_FRAC),
        max_effective_pct: crate::budget::effective_ceiling(
            spec.pct,
            spec.max_effective_pct,
        ),
        compound: spec.compound,
        copy_makers: spec.copy_makers,
        exclude_political: spec.exclude_political,
    };
    cfg.validate()?;
    let risk = spec
        .risk
        .clone()
        .unwrap_or_else(|| crate::risk::RiskConfig {
            max_drawdown_usd: (spec.seed_usd * 0.15).max(1.0),
            ..Default::default()
        });
    Ok((Lane::new(cfg), risk))
}
pub fn addr20(s: &str) -> Result<[u8; 20], String> {
    let b = hex::decode(s.trim_start_matches("0x")).map_err(|e| format!("{s}: {e}"))?;
    if b.len() != 20 {
        return Err(format!("{s}: expected 20 bytes, got {}", b.len()));
    }
    let mut o = [0u8; 20];
    o.copy_from_slice(&b);
    Ok(o)
}
impl Root {
    pub fn load(path: &str) -> Result<Root, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        toml::from_str(&raw).map_err(|e| format!("{path}: {e}"))
    }
    pub fn total_seed_usd(&self) -> f64 {
        self.lane
            .iter()
            .filter(|l| l.enabled)
            .filter_map(|l| l.budget.bankroll_usd)
            .sum()
    }
    pub fn wallet_specs(&self) -> Vec<crate::wallets::WalletSpec> {
        self.lane
            .iter()
            .filter(|l| l.enabled)
            .map(|l| crate::wallets::WalletSpec {
                name: l.name.clone(),
                leader: l.wallet.clone(),
                seed_usd: l.budget.bankroll_usd.unwrap_or(0.0),
                pct: l.sizing.pct,
                enabled: true,
                stop_mode: "none".into(),
                min_buy_price: l.budget.min_buy_price,
                max_buy_price: l.budget.max_buy_price,
                max_effective_pct: l.sizing.max_effective_pct,
                compound: l.sizing.compound,
                lane_id: None,
                copy_makers: l.execution.copy_makers,
                copy_maker_sells: Some(l.execution.copy_maker_sells),
                exclude_political: l.execution.exclude_political,
                leader_max_order_usd: l.leader_max_order_usd,
                leader_peak_exposure_usd: l.leader_peak_exposure_usd,
                risk: l.risk,
                buy_slippage_c: l.execution.buy_slippage_c,
                sell_slippage_c: l.execution.sell_slippage_c,
                sell_floor_frac: Some(l.execution.sell_floor_frac),
                min_order_usd: Some(l.sizing.min_order_usd),
                min_fill_floor: Some(l.sizing.min_fill_floor),
                sell_all_frac: Some(l.execution.sell_all_frac),
            })
            .collect()
    }
    pub fn build_lanes(&self) -> Result<Vec<Lane>, String> {
        let mut out = Vec::new();
        let mut seen_wallets = Vec::new();
        let mut seen_names = Vec::new();
        for l in self.lane.iter().filter(|l| l.enabled) {
            if seen_names.contains(&l.name) {
                return Err(format!("duplicate lane name {:?}", l.name));
            }
            seen_names.push(l.name.clone());
            let wallet20 = addr20(&l.wallet)?;
            if seen_wallets.contains(&wallet20) {
                return Err(format!("lane {}: duplicate wallet {}", l.name, l.wallet));
            }
            seen_wallets.push(wallet20);
            let sizing = match l.sizing.mode.as_str() {
                "shares" => Sizing::Shares(l.sizing.fixed_shares),
                "usd" => Sizing::Usd(l.sizing.flat_usd),
                "pct" => Sizing::Pct(l.sizing.pct),
                m => return Err(format!("lane {}: unknown sizing mode {m:?}", l.name)),
            };
            let execution = match l.execution.mode.as_str() {
                "taker" => Execution::Taker,
                "hybrid" => Execution::Hybrid,
                m => return Err(format!("lane {}: unknown execution mode {m:?}", l.name)),
            };
            let (caps, derived) = resolve_caps(l)?;
            let leader = crate::budget::LeaderStats {
                max_order_usd: l
                    .leader_max_order_usd
                    .ok_or_else(|| {
                        format!(
                            "lane {}: leader_max_order_usd is REQUIRED — measure it from this \
                     leader's own order flow, do NOT copy another lane's number",
                            l.name
                        )
                    })?,
                peak_exposure_usd: l
                    .leader_peak_exposure_usd
                    .ok_or_else(|| {
                        format!(
                            "lane {}: leader_peak_exposure_usd is REQUIRED — measure it from this \
                     leader's own order flow, do NOT copy another lane's number",
                            l.name
                        )
                    })?,
            };
            leader.validate(&l.name)?;
            if let Sizing::Pct(p) = sizing {
                if let Err(e) = crate::budget::check_pct_fits(
                    &l.name,
                    p,
                    &caps,
                    &leader,
                ) {
                    if derived {
                        return Err(e);
                    }
                    eprintln!(
                        "[{}] ⚠️  BUDGET WARNING (absolute caps, not enforced): {e}",
                        l.name
                    );
                }
                if let Some(adv) = crate::budget::open_cap_advisory(
                    &l.name,
                    p,
                    &caps,
                    &leader,
                ) {
                    eprintln!("[{}] ADVISORY: {adv}", l.name);
                }
            }
            if derived {
                eprintln!(
                    "[{}] caps DERIVED from bankroll: open ${:.2} / market ${:.2} / \
                     fill ${:.2} / daily ${:.2}  (ceiling {:.3}% copy)",
                    l.name, caps.max_open_usd, caps.per_market_usd, caps
                    .max_usd_per_fill, caps.daily_usd, crate ::budget::max_safe_pct(&
                    caps, & leader) * 100.0
                );
            }
            let cfg = LaneConfig {
                name: l.name.clone(),
                wallet20,
                sizing,
                execution,
                buy_slippage_c: l
                    .execution
                    .buy_slippage_c
                    .or(l.execution.slippage_c)
                    .unwrap_or(DEFAULT_SLIPPAGE),
                sell_slippage_c: l
                    .execution
                    .sell_slippage_c
                    .or(l.execution.slippage_c)
                    .unwrap_or(DEFAULT_SLIPPAGE),
                sell_floor_frac: l.execution.sell_floor_frac,
                min_order_usd: l.sizing.min_order_usd,
                max_usd_per_fill: caps.max_usd_per_fill,
                daily_budget_usd: caps.daily_usd,
                per_market_usd: caps.per_market_usd,
                max_open_usd: caps.max_open_usd,
                max_buy_price: l.budget.max_buy_price,
                min_buy_price: l.budget.min_buy_price,
                min_fill_floor: l.sizing.min_fill_floor,
                sell_all_frac: l.execution.sell_all_frac,
                max_effective_pct: crate::budget::effective_ceiling(
                    if let Sizing::Pct(p) = sizing {
                        p
                    } else {
                        l.sizing.max_effective_pct
                    },
                    l.sizing.max_effective_pct,
                ),
                compound: l.sizing.compound,
                copy_makers: l.execution.copy_makers,
                copy_maker_sells: l.execution.copy_maker_sells,
                exclude_political: l.execution.exclude_political,
            };
            cfg.validate()?;
            out.push(Lane::new(cfg));
        }
        if out.is_empty() {
            return Err("no enabled lanes — nothing to copy".into());
        }
        Ok(out)
    }
}
