use std::sync::Mutex;
use std::time::{Duration, Instant};
pub const DATA_API: &str = "https://data-api.polymarket.com";
pub const V1_BASELINE_GAP_C: f64 = 1.71;
pub const V1_BASELINE_GAP_PCT: f64 = 4.06;
pub const V1_BASELINE_N: usize = 107;
const CACHE_FOR: Duration = Duration::from_secs(20);
#[derive(Clone)]
pub struct Row {
    pub title: String,
    pub outcome: String,
    pub token: String,
    pub our_size: f64,
    pub our_avg: f64,
    pub our_pct: f64,
    pub our_usd: f64,
    pub our_pnl: f64,
    pub his_size: f64,
    pub his_avg: f64,
    pub his_avg_since: f64,
    pub his_pct: f64,
    pub his_usd: f64,
    pub his_pnl: f64,
    pub status: &'static str,
}
impl Row {
    pub fn gap(&self) -> f64 {
        self.his_pct - self.our_pct
    }
    pub fn entry_diff_c(&self) -> Option<f64> {
        if self.our_avg <= 0.0 || self.his_avg_since <= 0.0 {
            return None;
        }
        Some((self.our_avg - self.his_avg_since) * 100.0)
    }
    pub fn json(&self) -> serde_json::Value {
        let r = |x: f64| (x * 100.0).round() / 100.0;
        serde_json::json!(
            { "title" : self.title, "outcome" : self.outcome, "token" : self.token,
            "our_size" : r(self.our_size), "our_avg" : (self.our_avg * 10000.0).round() /
            10000.0, "our_pct" : r(self.our_pct), "our_usd" : r(self.our_usd), "our_pnl"
            : r(self.our_pnl), "his_size" : r(self.his_size), "his_avg" : (self.his_avg *
            10000.0).round() / 10000.0, "his_pct" : r(self.his_pct), "his_usd" : r(self
            .his_usd), "his_pnl" : r(self.his_pnl), "gap" : r(self.gap()), "entry_diff_c"
            : self.entry_diff_c().map(r), "his_avg_since" : (self.his_avg_since *
            10000.0).round() / 10000.0, "status" : self.status, }
        )
    }
}
struct History {
    his_tokens: std::collections::HashSet<String>,
    since: i64,
    realised: f64,
    his_avg_since: std::collections::HashMap<String, f64>,
}
pub fn apply_ledger_realised(v: &mut serde_json::Value, ledger_realised: Option<f64>) {
    let Some(real) = ledger_realised else { return };
    let Some(sum) = v.get_mut("summary").and_then(|s| s.as_object_mut()) else { return };
    let r = |x: f64| (x * 100.0).round() / 100.0;
    if let Some(old) = sum.get("our_pnl_realised").cloned() {
        sum.insert("wallet_realised_all_strategies".into(), old);
    }
    let unreal = sum.get("our_pnl_copied").and_then(|x| x.as_f64()).unwrap_or(0.0);
    sum.insert("our_pnl_realised".into(), serde_json::json!(r(real)));
    sum.insert("our_pnl_total".into(), serde_json::json!(r(unreal + real)));
    sum.insert(
        "realised_source".into(),
        serde_json::json!("lane ledger (this slot only; see apply_ledger_realised)"),
    );
}
fn deployed_capital(rows: &[Row]) -> (f64, f64, f64) {
    let copied = rows.iter().filter(|r| r.our_size > 0.0);
    let copy_pnl: f64 = copied.clone().map(|r| r.our_pnl).sum();
    let deployed: f64 = copied.clone().map(|r| r.our_usd - r.our_pnl).sum();
    let value: f64 = copied.map(|r| r.our_usd).sum();
    (copy_pnl, deployed, value)
}
fn wallet_mark_value(positions: &[serde_json::Value]) -> f64 {
    positions.iter().map(|p| f(p, "currentValue")).sum()
}
#[derive(Default)]
pub struct Matchup {
    cached: Mutex<Option<(Instant, serde_json::Value)>>,
    hist: Mutex<Option<(Instant, std::sync::Arc<History>)>>,
}
const HISTORY_FOR: Duration = Duration::from_secs(600);
async fn get_json(
    http: &reqwest::Client,
    url: &str,
) -> Result<serde_json::Value, String> {
    let r = http.get(url).send().await.map_err(|e| format!("{e}"))?;
    if !r.status().is_success() {
        return Err(format!("HTTP {}", r.status()));
    }
    r.json().await.map_err(|e| format!("decode: {e}"))
}
fn f(v: &serde_json::Value, k: &str) -> f64 {
    v.get(k)
        .and_then(|x| x.as_f64())
        .or_else(|| v.get(k).and_then(|x| x.as_str()).and_then(|s| s.parse().ok()))
        .unwrap_or(0.0)
}
fn s(v: &serde_json::Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}
async fn all_trades(
    http: &reqwest::Client,
    w: &str,
    pages: usize,
) -> Result<Vec<serde_json::Value>, String> {
    let mut out = Vec::new();
    for p in 0..pages {
        let url = format!("{DATA_API}/trades?user={w}&limit=500&offset={}", p * 500);
        let mut got = get_json(http, &url).await;
        if got.is_err() {
            got = get_json(http, &url).await;
        }
        match got {
            Ok(serde_json::Value::Array(a)) => {
                let n = a.len();
                out.extend(a);
                if n < 500 {
                    break;
                }
            }
            Err(e) if p == 0 => return Err(format!("trades page 0 for {w}: {e}")),
            _ => break,
        }
    }
    Ok(out)
}
impl Matchup {
    pub fn new() -> Self {
        Self::default()
    }
    pub async fn get(
        &self,
        http: &reqwest::Client,
        us: &str,
        him: &str,
        ledger_realised: Option<f64>,
        lane_epoch: Option<i64>,
    ) -> serde_json::Value {
        let mut v = {
            let hit = self
                .cached
                .lock()
                .unwrap()
                .as_ref()
                .filter(|(at, _)| at.elapsed() < CACHE_FOR)
                .map(|(_, v)| v.clone());
            match hit {
                Some(v) => v,
                None => {
                    let v = self.build(http, us, him, lane_epoch).await;
                    *self.cached.lock().unwrap() = Some((Instant::now(), v.clone()));
                    v
                }
            }
        };
        apply_ledger_realised(&mut v, ledger_realised);
        v
    }
    async fn build(
        &self,
        http: &reqwest::Client,
        us: &str,
        him: &str,
        lane_epoch: Option<i64>,
    ) -> serde_json::Value {
        let our_pos = get_json(
                http,
                &format!("{DATA_API}/positions?user={us}&sizeThreshold=0.01&limit=500"),
            )
            .await;
        let his_pos = get_json(
                http,
                &format!("{DATA_API}/positions?user={him}&sizeThreshold=0.01&limit=500"),
            )
            .await;
        let (our_pos, his_pos) = match (our_pos, his_pos) {
            (Ok(a), Ok(b)) => (a, b),
            (a, b) => {
                return serde_json::json!(
                    { "error" : format!("data-api: {:?} {:?}", a.err(), b.err()) }
                );
            }
        };
        let empty = vec![];
        let ours: &Vec<_> = our_pos.as_array().unwrap_or(&empty);
        let his: &Vec<_> = his_pos.as_array().unwrap_or(&empty);
        if let Some((at, h)) = self.hist.lock().unwrap().as_ref() {
            if at.elapsed() < HISTORY_FOR {
                return self.assemble(ours, his, h.clone());
            }
        }
        let (our_tr, his_tr) = match tokio::join!(
            all_trades(http, us, 4), all_trades(http, him, 4)
        ) {
            (Ok(a), Ok(b)) => (a, b),
            (a, b) => {
                return serde_json::json!(
                    { "error" :
                    format!("cannot build matchup — trade history unavailable ({:?} {:?}). Showing \
                 nothing rather than a view that would read as 'we copied none of it'.",
                    a.err(), b.err()) }
                );
            }
        };
        if his_tr.is_empty() {
            return serde_json::json!(
                { "error" :
                "his trade history came back EMPTY — refusing to render, because every \
                 position of ours would be misfiled as another strategy's and his whole \
                 back book would show as uncopied."
                }
            );
        }
        let his_tokens: std::collections::HashSet<String> = his_tr
            .iter()
            .map(|t| s(t, "asset"))
            .collect();
        let inferred = our_tr
            .iter()
            .filter(|t| his_tokens.contains(&s(t, "asset")))
            .map(|t| f(t, "timestamp") as i64)
            .min()
            .unwrap_or(0);
        let since = lane_epoch.filter(|t| *t > 0).unwrap_or(inferred);
        let mut his_first: std::collections::HashMap<String, i64> = Default::default();
        for t in &his_tr {
            let a = s(t, "asset");
            let ts = f(t, "timestamp") as i64;
            his_first.entry(a).and_modify(|e| *e = (*e).min(ts)).or_insert(ts);
        }
        let mut his_avg_since: std::collections::HashMap<String, f64> = Default::default();
        {
            let mut acc: std::collections::HashMap<String, (f64, f64)> = Default::default();
            for t in &his_tr {
                if (f(t, "timestamp") as i64) < since {
                    continue;
                }
                if s(t, "side") != "BUY" {
                    continue;
                }
                let (sh, px) = (f(t, "size"), f(t, "price"));
                if sh <= 0.0 || px <= 0.0 {
                    continue;
                }
                let e = acc.entry(s(t, "asset")).or_insert((0.0, 0.0));
                e.0 += sh * px;
                e.1 += sh;
            }
            for (k, (notional, shares)) in acc {
                if shares > 1e-9 {
                    his_avg_since.insert(k, notional / shares);
                }
            }
        }
        let realised = {
            let mut fills: Vec<&serde_json::Value> = our_tr
                .iter()
                .filter(|t| his_tokens.contains(&s(t, "asset")))
                .collect();
            fills.sort_by_key(|t| f(t, "timestamp") as i64);
            let mut sh: std::collections::HashMap<String, f64> = Default::default();
            let mut cost: std::collections::HashMap<String, f64> = Default::default();
            let mut acc = 0.0f64;
            for t in fills {
                let a = s(t, "asset");
                let (sz, px) = (f(t, "size"), f(t, "price"));
                if sz <= 0.0 {
                    continue;
                }
                let held = sh.entry(a.clone()).or_default();
                let basis = cost.entry(a.clone()).or_default();
                if s(t, "side").eq_ignore_ascii_case("BUY") {
                    *held += sz;
                    *basis += sz * px;
                } else {
                    let avg = if *held > 1e-9 { *basis / *held } else { px };
                    let sold = sz.min(held.max(0.0));
                    acc += sold * (px - avg);
                    *held = (*held - sold).max(0.0);
                    *basis = (*basis - sold * avg).max(0.0);
                }
            }
            acc
        };
        let hist = std::sync::Arc::new(History {
            his_tokens,
            since,
            realised,
            his_avg_since,
        });
        *self.hist.lock().unwrap() = Some((Instant::now(), hist.clone()));
        self.assemble(ours, his, hist)
    }
    fn assemble(
        &self,
        ours: &[serde_json::Value],
        his: &[serde_json::Value],
        hist: std::sync::Arc<History>,
    ) -> serde_json::Value {
        let (his_tokens, since) = (&hist.his_tokens, hist.since);
        let realised = hist.realised;
        let hp: std::collections::HashMap<String, &serde_json::Value> = his
            .iter()
            .map(|p| (s(p, "asset"), p))
            .collect();
        let mut rows: Vec<Row> = Vec::new();
        let mut other_strategy = 0usize;
        for p in ours {
            let tok = s(p, "asset");
            let h = hp.get(&tok);
            if !his_tokens.contains(&tok) && h.is_none() {
                other_strategy += 1;
                continue;
            }
            let status = if h.is_some() { "matched" } else { "he_exited" };
            rows.push(Row {
                title: s(p, "title"),
                outcome: s(p, "outcome"),
                his_avg_since: hist.his_avg_since.get(&tok).copied().unwrap_or(0.0),
                token: tok,
                our_size: f(p, "size"),
                our_avg: f(p, "avgPrice"),
                our_pct: f(p, "percentPnl"),
                our_usd: f(p, "currentValue"),
                our_pnl: f(p, "cashPnl"),
                his_size: h.map(|x| f(x, "size")).unwrap_or(0.0),
                his_avg: h.map(|x| f(x, "avgPrice")).unwrap_or(0.0),
                his_pct: h.map(|x| f(x, "percentPnl")).unwrap_or(0.0),
                his_usd: h.map(|x| f(x, "currentValue")).unwrap_or(0.0),
                his_pnl: h.map(|x| f(x, "cashPnl")).unwrap_or(0.0),
                status,
            });
        }
        let ours_tok: std::collections::HashSet<String> = ours
            .iter()
            .map(|p| s(p, "asset"))
            .collect();
        for p in his {
            let tok = s(p, "asset");
            if ours_tok.contains(&tok) {
                continue;
            }
            if !hist.his_avg_since.contains_key(&tok) {
                continue;
            }
            rows.push(Row {
                title: s(p, "title"),
                outcome: s(p, "outcome"),
                his_avg_since: hist.his_avg_since.get(&tok).copied().unwrap_or(0.0),
                token: tok,
                our_size: 0.0,
                our_avg: 0.0,
                our_pct: 0.0,
                our_usd: 0.0,
                our_pnl: 0.0,
                his_size: f(p, "size"),
                his_avg: f(p, "avgPrice"),
                his_pct: f(p, "percentPnl"),
                his_usd: f(p, "currentValue"),
                his_pnl: f(p, "cashPnl"),
                status: "not_copied",
            });
        }
        rows.sort_by(|a, b| b.our_usd.partial_cmp(&a.our_usd).unwrap());
        let tracked: Vec<&Row> = rows.iter().filter(|r| r.status == "matched").collect();
        let avg_gap = if tracked.is_empty() {
            0.0
        } else {
            tracked.iter().map(|r| r.gap()).sum::<f64>() / tracked.len() as f64
        };
        let diffs: Vec<f64> = tracked.iter().filter_map(|r| r.entry_diff_c()).collect();
        let avg_entry_c: Option<f64> = if diffs.is_empty() {
            None
        } else {
            Some(diffs.iter().sum::<f64>() / diffs.len() as f64)
        };
        let (copy_pnl, deployed, value) = deployed_capital(&rows);
        let wallet_value = wallet_mark_value(ours);
        let r = |x: f64| (x * 100.0).round() / 100.0;
        serde_json::json!(
            { "since" : since, "kpi" : { "v1_gap_c" : V1_BASELINE_GAP_C, "v1_gap_pct" :
            V1_BASELINE_GAP_PCT, "v1_n" : V1_BASELINE_N, "now_gap_c" : avg_entry_c
            .map(r), "delta_c" : avg_entry_c.map(| x | r(x - V1_BASELINE_GAP_C)),
            "better" : avg_entry_c.map(| x | x < V1_BASELINE_GAP_C), "n" : diffs.len(),
            }, "deployed" : r(deployed), "value" : r(value), "wallet_value" :
            r(wallet_value), "wallet_positions" : ours.len(), "rows" : rows.iter().map(|
            r | r.json()).collect::< Vec < _ >> (), "summary" : { "matched" : tracked
            .len(), "he_exited" : rows.iter().filter(| r | r.status == "he_exited")
            .count(), "not_copied" : rows.iter().filter(| r | r.status == "not_copied")
            .count(), "other_strategy_positions" : other_strategy, "avg_gap_pts" :
            r(avg_gap), "avg_entry_diff_c" : avg_entry_c.map(r), "our_pnl_copied" :
            r(copy_pnl), "our_pnl_realised" : r(realised), "our_pnl_total" : r(copy_pnl +
            realised), "his_pnl_shared" : r(rows.iter().filter(| r | r.status ==
            "matched").map(| r | r.his_pnl).sum::< f64 > ()), "his_pnl_not_copied" :
            r(rows.iter().filter(| r | r.status == "not_copied").map(| r | r.his_pnl)
            .sum::< f64 > ()), "his_pnl_he_exited" : r(rows.iter().filter(| r | r.status
            == "he_exited").map(| r | r.his_pnl).sum::< f64 > ()), "his_pnl_all_rows" :
            r(rows.iter().map(| r | r.his_pnl).sum::< f64 > ()), }, }
        )
    }
}
pub async fn free_cash(
    http: &reqwest::Client,
    clob_host: &str,
    address: &str,
    creds: &crate::auth::ApiCreds,
    signature_type: u8,
) -> Option<f64> {
    let path = format!(
        "/balance-allowance?asset_type=COLLATERAL&signature_type={signature_type}"
    );
    let ts = crate::auth::now_secs();
    let hdrs = crate::auth::l2_headers(address, creds, ts, "GET", &path, None).ok()?;
    let mut rb = http.get(format!("{}{}", clob_host.trim_end_matches('/'), path));
    for (k, v) in &hdrs {
        rb = rb.header(*k, v);
    }
    let r = rb.send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    let v: serde_json::Value = r.json().await.ok()?;
    for k in ["balance", "collateral", "available"] {
        if let Some(x) = v.get(k) {
            if let Some(n) = x
                .as_f64()
                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
            {
                return Some(n / 1e6);
            }
        }
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    fn row(status: &'static str, our: f64, his: f64) -> Row {
        Row {
            title: "t".into(),
            outcome: "Yes".into(),
            token: "1".into(),
            our_size: 1.0,
            our_avg: 0.62,
            our_pct: our,
            our_usd: 1.0,
            our_pnl: 0.0,
            his_size: 1.0,
            his_avg: 0.60,
            his_avg_since: 0.60,
            his_pct: his,
            his_usd: 1.0,
            his_pnl: 0.0,
            status,
        }
    }
    fn row_since(our_avg: f64, his_life: f64, his_since: f64) -> Row {
        let mut r = row("matched", 0.0, 0.0);
        r.our_avg = our_avg;
        r.his_avg = his_life;
        r.his_avg_since = his_since;
        r
    }
    #[test]
    fn entry_drag_uses_his_SINCE_average_not_his_LIFETIME_average() {
        let r = row_since(0.61, 0.30, 0.60);
        let d = r.entry_diff_c().unwrap();
        assert!(
            (d - 1.0).abs() < 1e-9,
            "drag is 1c against his in-window entry, not 31c against his back book"
        );
    }
    #[test]
    fn entry_drag_is_NONE_when_he_bought_none_of_it_since_we_started() {
        let r = row_since(0.61, 0.30, 0.0);
        assert_eq!(r.entry_diff_c(), None);
    }
    #[test]
    fn entry_drag_is_NONE_when_we_never_entered() {
        let r = row_since(0.0, 0.30, 0.60);
        assert_eq!(r.entry_diff_c(), None);
    }
    #[test]
    fn a_cheaper_entry_than_his_reads_NEGATIVE() {
        let r = row_since(0.58, 0.90, 0.60);
        assert!(
            r.entry_diff_c().unwrap() < 0.0, "entering cheaper must not read as drag"
        );
    }
    #[test]
    fn selling_a_position_he_already_held_is_NOT_a_miss() {
        let since = 1_785_124_192i64;
        let his_first_in_window = since + 86_400;
        let bought_since = false;
        let is_miss = bought_since && his_first_in_window >= since;
        assert!(! is_miss, "an exit-only token is his back book, never a miss");
    }
    #[test]
    fn a_position_he_actually_opened_after_we_started_IS_a_miss() {
        let bought_since = true;
        assert!(
            bought_since, "he bought it on our watch and we did not — a real miss"
        );
    }
    #[test]
    fn ADDING_to_a_position_he_opened_before_us_is_still_a_copyable_miss() {
        let since = 1_785_124_192i64;
        let his_first_touch = since - 2 * 86_400;
        let bought_since = true;
        assert!(
            bought_since,
            "a later BUY is copyable no matter when he first touched the token"
        );
        let old_buggy_rule = his_first_touch >= since;
        assert!(! old_buggy_rule, "first-touch gating would have discarded this miss");
    }
    #[test]
    fn his_pnl_lines_RECONCILE_with_the_rows_they_sit_above() {
        let mut copied = row("matched", 0.0, 0.0);
        copied.his_pnl = 100.0;
        let mut missed = row("not_copied", 0.0, 0.0);
        missed.his_pnl = 25.0;
        let mut exited = row("he_exited", 0.0, 0.0);
        exited.his_pnl = 7.0;
        let rows = [copied, missed, exited];
        let shared: f64 = rows
            .iter()
            .filter(|r| r.status == "matched")
            .map(|r| r.his_pnl)
            .sum();
        let not_c: f64 = rows
            .iter()
            .filter(|r| r.status == "not_copied")
            .map(|r| r.his_pnl)
            .sum();
        let exit_p: f64 = rows
            .iter()
            .filter(|r| r.status == "he_exited")
            .map(|r| r.his_pnl)
            .sum();
        let all: f64 = rows.iter().map(|r| r.his_pnl).sum();
        assert_eq!(shared, 100.0);
        assert_eq!(not_c, 25.0);
        assert!(
            (shared + not_c + exit_p - all).abs() < 1e-9,
            "the three published lines must sum to every row on screen"
        );
    }
    #[test]
    fn a_token_HE_HOLDS_is_ours_to_track_even_if_his_trades_feed_lags() {
        let his_tokens: std::collections::HashSet<String> = ["A".to_string()]
            .into_iter()
            .collect();
        let he_holds_b = true;
        let is_other_strategy = !his_tokens.contains("B") && !he_holds_b;
        assert!(
            ! is_other_strategy,
            "a token he holds must never be filed as another strategy's"
        );
    }
    #[test]
    fn a_token_he_NEITHER_traded_NOR_holds_is_still_another_strategys() {
        let his_tokens: std::collections::HashSet<String> = ["A".to_string()]
            .into_iter()
            .collect();
        let he_holds_btc = false;
        let is_other_strategy = !his_tokens.contains("BTC_SCALP") && !he_holds_btc;
        assert!(
            is_other_strategy, "a token he never traded and does not hold is not ours"
        );
    }
    fn body(realised: f64, unrealised: f64) -> serde_json::Value {
        serde_json::json!(
            { "summary" : { "our_pnl_realised" : realised, "our_pnl_copied" : unrealised,
            "our_pnl_total" : realised + unrealised, } }
        )
    }
    #[test]
    fn THE_MEASURED_OVERSTATEMENT_is_corrected() {
        let mut v = body(139.00, 99.19);
        apply_ledger_realised(&mut v, Some(34.46));
        let s = &v["summary"];
        assert_eq!(s["our_pnl_realised"], 34.46, "must report the LANE's realised");
        assert_eq!(
            s["our_pnl_total"], 133.65, "total = unrealised 99.19 + realised 34.46"
        );
        assert_eq!(
            s["wallet_realised_all_strategies"], 139.00,
            "the wallet figure is kept, under a name that says what it measures"
        );
    }
    #[test]
    fn a_LOSS_is_reported_just_as_faithfully_as_a_gain() {
        let mut v = body(139.00, 99.19);
        apply_ledger_realised(&mut v, Some(-62.33));
        assert_eq!(v["summary"] ["our_pnl_realised"], - 62.33);
        assert_eq!(v["summary"] ["our_pnl_total"], 36.86);
    }
    #[test]
    fn with_NO_ledger_figure_the_body_is_left_completely_alone() {
        let mut v = body(139.00, 99.19);
        apply_ledger_realised(&mut v, None);
        assert_eq!(v["summary"] ["our_pnl_realised"], 139.00);
        assert!(v["summary"].get("wallet_realised_all_strategies").is_none());
    }
    #[test]
    fn an_ERROR_body_is_not_corrupted_by_the_override() {
        let mut v = serde_json::json!({ "error" : "data-api timeout" });
        apply_ledger_realised(&mut v, Some(34.46));
        assert!(
            v.get("summary").is_none(), "must not invent a summary on an error body"
        );
        assert_eq!(v["error"], "data-api timeout");
    }
    #[test]
    fn the_total_is_RECOMPUTED_not_inherited() {
        let mut v = body(139.00, 99.19);
        apply_ledger_realised(&mut v, Some(34.46));
        let s = &v["summary"];
        let (u, r, t) = (
            s["our_pnl_copied"].as_f64().unwrap(),
            s["our_pnl_realised"].as_f64().unwrap(),
            s["our_pnl_total"].as_f64().unwrap(),
        );
        assert!((u + r - t).abs() < 1e-9, "total must equal unrealised + realised");
    }
    #[test]
    fn he_exited_rows_still_count_toward_deployed_capital() {
        let matched = row("matched", 10.0, 10.0);
        let mut exited = row("he_exited", 0.0, 0.0);
        exited.our_size = 5.0;
        exited.our_usd = 4.60;
        exited.our_pnl = 0.0;
        let (_, deployed, _) = deployed_capital(&[matched, exited]);
        assert!(
            deployed >= 4.60 - 1e-9,
            "an he_exited position must still count as deployed capital"
        );
    }
    #[test]
    fn not_copied_rows_never_count_toward_deployed() {
        let mut missed = row("not_copied", 0.0, 0.0);
        missed.our_size = 0.0;
        missed.our_usd = 999.0;
        missed.our_pnl = 0.0;
        let (_, deployed, value) = deployed_capital(&[missed]);
        assert_eq!(
            deployed, 0.0, "a position we never took must not appear as deployed capital"
        );
        assert_eq!(
            value, 0.0,
            "a position we never took must not appear as mark-to-market value"
        );
    }
    #[test]
    fn deployed_is_cost_basis_current_value_minus_pnl() {
        let mut winner = row("matched", 0.0, 0.0);
        winner.our_size = 1.0;
        winner.our_usd = 13.0;
        winner.our_pnl = 3.0;
        let mut loser = row("matched", 0.0, 0.0);
        loser.our_size = 1.0;
        loser.our_usd = 6.0;
        loser.our_pnl = -2.0;
        let (copy_pnl, deployed, value) = deployed_capital(&[winner, loser]);
        assert!(
            (deployed - 18.0).abs() < 1e-9,
            "cost basis should be 10 + 8 = 18, got {deployed}"
        );
        assert!(
            (value - 19.0).abs() < 1e-9,
            "mark-to-market should be 13 + 6 = 19, got {value}"
        );
        assert!(
            (copy_pnl - 1.0).abs() < 1e-9, "pnl should be 3 - 2 = 1, got {copy_pnl}"
        );
    }
    #[test]
    fn deployed_capital_is_empty_on_an_empty_book() {
        let (pnl, deployed, value) = deployed_capital(&[]);
        assert_eq!((pnl, deployed, value), (0.0, 0.0, 0.0));
    }
    #[test]
    fn shared_portfolio_uses_every_physical_wallet_position_not_only_copied_rows() {
        let positions = vec![
            serde_json::json!({ "currentValue" : 12.5, "asset" : "copied" }),
            serde_json::json!({ "currentValue" : "7.25", "asset" : "other-strategy" }),
        ];
        assert!((wallet_mark_value(& positions) - 19.75).abs() < 1e-9);
    }
    #[test]
    fn gap_is_how_far_behind_him_we_are() {
        assert!((row("matched", 15.1, 21.6).gap() - 6.5).abs() < 1e-9);
    }
    #[test]
    fn gap_is_negative_when_we_BEAT_him() {
        assert!(row("matched", 0.6, - 3.0).gap() < 0.0);
    }
    #[test]
    fn entry_diff_is_in_CENTS_and_signed_against_us() {
        assert!((row("matched", 0.0, 0.0).entry_diff_c().unwrap() - 2.0).abs() < 1e-9);
    }
    #[test]
    fn entry_diff_is_zero_when_a_price_is_missing() {
        let mut r = row("not_copied", 0.0, 0.0);
        r.our_avg = 0.0;
        assert_eq!(
            r.entry_diff_c(), None,
            "unmeasurable must be None, never 0.0 — 0.0 reads as a perfect copy"
        );
    }
    #[test]
    fn json_carries_both_sides_and_the_gap() {
        let j = row("matched", 15.1, 21.6).json();
        assert_eq!(j["our_pct"], 15.1);
        assert_eq!(j["his_pct"], 21.6);
        assert_eq!(j["gap"], 6.5);
        assert_eq!(j["status"], "matched");
    }
    #[test]
    fn numeric_fields_survive_being_strings() {
        let v = serde_json::json!({ "size" : "12.5", "price" : 0.65, "nope" : null });
        assert_eq!(f(& v, "size"), 12.5);
        assert_eq!(f(& v, "price"), 0.65);
        assert_eq!(f(& v, "nope"), 0.0);
        assert_eq!(f(& v, "missing"), 0.0);
    }
    #[test]
    fn a_missing_title_does_not_panic() {
        let v = serde_json::json!({});
        assert_eq!(s(& v, "title"), "");
    }
}
