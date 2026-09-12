#![cfg_attr(test, allow(non_snake_case))]
use copybot_hot::book::BookCache;
use copybot_hot::control::Control;
use copybot_hot::fingerprint::{FingerprintScore, Fingerprints};
use copybot_hot::ledger::Ledger;
use copybot_hot::txpool::{self, TxpoolStats};
use copybot_hot::calldata::{decode_all, participants};
use copybot_hot::config::{addr20, Root};
use copybot_hot::feeds::{FeedConfig, FeedStats, RawTx};
use copybot_hot::lanes::{resolve_route, Route, Router, MICRO};
use copybot_hot::order::{amounts, json_body, safe_salt, Order};
use copybot_hot::race::{FillKey, RaceBook};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
type DashboardLane = (
    String,
    String,
    Option<f64>,
    f64,
    Arc<copybot_hot::matchup::Matchup>,
);
#[derive(Clone, Copy, Default)]
struct PhysicalWallet {
    cash: Option<f64>,
    equity: Option<f64>,
    fetched_at: Option<i64>,
    stale_reason: Option<&'static str>,
}
fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}
#[allow(clippy::too_many_arguments)]
async fn resolve_pending(
    log: &Arc<std::sync::Mutex<copybot_hot::pending::PendingLog>>,
    control: &Arc<std::sync::Mutex<Control>>,
    router: &Arc<Router>,
    emitter: &Arc<Emitter>,
    clob: &str,
    creds: &Arc<Option<copybot_hot::auth::ApiCreds>>,
    signer_addr: &str,
    _sig_type: u8,
    resting: Option<&Arc<copybot_hot::resting::RestingBook>>,
) {
    use copybot_hot::pending::Verdict;
    let Some(cr) = creds.as_ref().as_ref() else { return };
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default();
    let now = copybot_hot::ledger::now_secs();
    let due = log.lock().unwrap().due(now);
    if due.is_empty() {
        return;
    }
    let get = |path: String| {
        let (http, clob, cr, addr) = (
            http.clone(),
            clob.to_string(),
            cr.clone(),
            signer_addr.to_string(),
        );
        async move {
            let hdrs = copybot_hot::auth::l2_headers(
                    &addr,
                    &cr,
                    copybot_hot::auth::now_secs(),
                    "GET",
                    &path,
                    None,
                )
                .ok()?;
            let mut rq = http.get(format!("{clob}{path}"));
            for (k, v) in hdrs {
                rq = rq.header(k, v);
            }
            let resp = rq.send().await.ok()?;
            if !resp.status().is_success() {
                return None;
            }
            resp.text().await.ok()
        }
    };
    for p in due {
        let hash = p.order_hash.trim_start_matches("0x").to_string();
        let mut verdict = match get(format!("/data/order/0x{hash}")).await {
            Some(body) => {
                copybot_hot::pending::parse_order_status_kind(&body, p.side, p.resting)
            }
            None => Verdict::Unknown,
        };
        if copybot_hot::pending::knows_shares_but_not_price(&verdict) {
            if let Some(body) = get(format!("/data/trades?asset_id={}", p.token)).await {
                verdict = copybot_hot::pending::merge_price_from_trades(
                    verdict,
                    copybot_hot::pending::parse_trades(&body, &hash, p.side, p.resting),
                );
            }
        }
        if verdict == Verdict::Unknown {
            if let Some(body) = get(format!("/data/trades?asset_id={}", p.token)).await {
                verdict = copybot_hot::pending::parse_trades(
                    &body,
                    &hash,
                    p.side,
                    p.resting,
                );
            }
        }
        match verdict {
            Verdict::Filled { shares, price } => {
                let (px, px_provisional) = copybot_hot::pending::recovered_price(
                    p.resting,
                    price,
                    p.limit,
                );
                let mut sz = shares;
                if p.side == 1 {
                    let held = control
                        .lock()
                        .unwrap()
                        .ledger
                        .holdings(&p.lane)
                        .get(&p.token)
                        .copied()
                        .unwrap_or(0.0);
                    sz = sz.min(held);
                }
                let receipt = if sz > 0.0 {
                    let r = control
                        .lock()
                        .unwrap()
                        .ledger
                        .book_fill_ex_px(
                            &p.lane,
                            &p.token,
                            p.side,
                            sz,
                            px,
                            0.0,
                            None,
                            &p.order_hash,
                            p.resting,
                            px_provisional,
                        );
                    if r.is_some() && p.side == 0 {
                        if let Some(ix) = router
                            .snapshot()
                            .iter()
                            .position(|l| l.cfg.name == p.lane)
                        {
                            router.claim_existing(&p.token, ix);
                        }
                    }
                    r
                } else {
                    log.lock()
                        .unwrap()
                        .resolve(&p.order_hash, "filled", Some((0.0, px)));
                    continue;
                };
                let Some(r) = receipt else {
                    eprintln!(
                        "[pending] {} could NOT book a recovered fill — row kept open",
                        p.lane
                    );
                    emitter
                        .emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "pending_recover_failed", "lane" :
                                p.lane, "tok" : & p.token[..p.token.len().min(14)], "shares"
                                : sz, "price" : px }
                            ),
                        );
                    continue;
                };
                if let Err(e) = log
                    .lock()
                    .unwrap()
                    .resolve_booked(&p.order_hash, "filled", r)
                {
                    eprintln!(
                        "[pending] {} fill IS booked but its row stayed open: {e}", p
                        .lane
                    );
                }
                eprintln!(
                    "[pending] {} RECOVERED a lost fill: {sz:.4} sh @ {px:.4} on …{}",
                    p.lane, & p.token[p.token.len().saturating_sub(8)..]
                );
                emitter
                    .emit(
                        serde_json::json!(
                            { "t" : now_ms(), "ev" : "pending_recovered", "lane" : p
                            .lane, "tok" : & p.token[..p.token.len().min(14)], "side" : p
                            .side, "shares" : sz, "price" : px, "why" : p.why }
                        ),
                    );
            }
            Verdict::NotFilled => {
                log.lock().unwrap().resolve(&p.order_hash, "not_filled", None);
                emitter
                    .emit(
                        serde_json::json!(
                            { "t" : now_ms(), "ev" : "pending_not_filled", "lane" : p
                            .lane, "tok" : & p.token[..p.token.len().min(14)] }
                        ),
                    );
            }
            Verdict::Unknown => {}
        }
    }
    let live_rests: std::collections::HashSet<String> = resting
        .map(|rb| {
            rb
                .all()
                .into_iter()
                .map(|r| r.order_id.trim_start_matches("0x").to_string())
                .collect()
        })
        .unwrap_or_default();
    for p in log.lock().unwrap().stale_excluding(now, &live_rests) {
        eprintln!(
            "[pending] ⚠️  UNRESOLVED for {}s: {} {} …{} ({:.4} sh) — verify by hand",
            now - p.ts, p.lane, if p.side == 0 { "BUY" } else { "SELL" }, & p.token[p
            .token.len().saturating_sub(8)..], p.shares
        );
        emitter
            .emit(
                serde_json::json!(
                    { "t" : now_ms(), "ev" : "pending_stale", "lane" : p.lane, "tok" : &
                    p.token[..p.token.len().min(14)], "age_secs" : now - p.ts, "shares" :
                    p.shares, "why" : p.why }
                ),
            );
    }
}
fn is_political_title(t: &str) -> bool {
    let t = t.to_ascii_lowercase();
    const KW: &[&str] = &[
        "president",
        "election",
        "senate",
        "primary",
        "democratic",
        "republican",
        "congress",
        "governor",
        "mayor",
        "parliament",
        "prime minister",
        "putin",
        "xi jinping",
        "kamala",
        "newsom",
        "trump",
        "biden",
        "nominee",
        "electoral",
        "referendum",
        "impeach",
        "cabinet",
        "out as ",
        "win the 20",
    ];
    KW.iter().any(|k| t.contains(k))
}
fn wallet_equity_pnl(portfolio: Option<f64>, funding_basis: f64) -> Option<f64> {
    portfolio.map(|p| ((p - funding_basis) * 100.0).round() / 100.0)
}
fn release_cancelled(
    guard: &Option<Arc<copybot_hot::signal_guard::SignalGuard>>,
    emitter: &Arc<Emitter>,
    order: &copybot_hot::resting::Resting,
    reason: &'static str,
) {
    if order.side != 0 {
        return;
    }
    if order.salt.iter().all(|&b| b == 0) {
        return;
    }
    let Some(g) = guard.as_ref() else { return };
    if let Err(e) = g
        .release(
            &order.salt,
            &order.condition,
            &order.lane,
            order.shares,
            copybot_hot::ledger::now_secs(),
        )
    {
        eprintln!(
            "[{}] cancel-release FAILED ({reason}): {e:?} — {} shares stay \
                   committed against his order",
            order.lane, order.shares
        );
        emitter
            .emit(
                serde_json::json!(
                    { "t" : now_ms(), "ev" : "cancel_release_failed", "lane" : order
                    .lane, "tok" : order.token, "shares" : order.shares, "reason" :
                    reason, "why" : format!("{e:?}"), }
                ),
            );
    }
}
#[allow(clippy::too_many_arguments)]
async fn settle_cancelled_rest(
    http: &reqwest::Client,
    clob: &str,
    addr: &str,
    cr: &copybot_hot::auth::ApiCreds,
    rb: &Arc<copybot_hot::resting::RestingBook>,
    guard: &Option<Arc<copybot_hot::signal_guard::SignalGuard>>,
    emitter: &Arc<Emitter>,
    order_id: &str,
    reason: &'static str,
) {
    use copybot_hot::pending::Verdict;
    if !rb.try_claim_settle(order_id) {
        return;
    }
    struct Claim<'a>(&'a Arc<copybot_hot::resting::RestingBook>, String);
    impl Drop for Claim<'_> {
        fn drop(&mut self) {
            self.0.finish_settle(&self.1);
        }
    }
    let _claim = Claim(rb, order_id.to_string());
    let Some(o) = rb
        .all()
        .into_iter()
        .find(|r| {
            let bare = r.order_id.trim_start_matches("0x");
            bare == order_id.trim_start_matches("0x")
        }) else { return };
    let bare = order_id.trim_start_matches("0x");
    let path = format!("/data/order/0x{bare}");
    let body = match copybot_hot::auth::l2_headers(
        addr,
        cr,
        copybot_hot::auth::now_secs(),
        "GET",
        &path,
        None,
    ) {
        Ok(h) => {
            let mut rq = http.get(format!("{clob}{path}"));
            for (k, v) in &h {
                rq = rq.header(*k, v);
            }
            match rq.send().await {
                Ok(r) if r.status().is_success() => r.text().await.ok(),
                _ => None,
            }
        }
        Err(_) => None,
    };
    let verdict = match body {
        Some(b) => copybot_hot::pending::parse_order_status_kind(&b, o.side, true),
        None => {
            let tpath = format!("/data/trades?asset_id={}", o.token);
            let tbody = match copybot_hot::auth::l2_headers(
                addr,
                cr,
                copybot_hot::auth::now_secs(),
                "GET",
                &tpath,
                None,
            ) {
                Ok(h) => {
                    let mut rq = http.get(format!("{clob}{tpath}"));
                    for (k, v) in &h {
                        rq = rq.header(*k, v);
                    }
                    match rq.send().await {
                        Ok(r) if r.status().is_success() => r.text().await.ok(),
                        _ => None,
                    }
                }
                Err(_) => None,
            };
            match tbody {
                Some(t) => copybot_hot::pending::parse_trades(&t, bare, o.side, true),
                None => copybot_hot::pending::Verdict::Unknown,
            }
        }
    };
    match verdict {
        Verdict::Filled { shares: filled, .. } => {
            rb.remove(&o.order_id);
            let unfilled = (o.shares - filled).max(0.0);
            if unfilled > 1e-9 {
                let mut give = o.clone();
                give.shares = unfilled;
                release_cancelled(guard, emitter, &give, reason);
            }
            emitter
                .emit(
                    serde_json::json!(
                        { "t" : now_ms(), "ev" : "rest_cancelled", "lane" : o.lane, "tok"
                        : & o.token[..o.token.len().min(14)], "reason" : reason, "filled"
                        : filled, "released" : unfilled }
                    ),
                );
        }
        Verdict::NotFilled => {
            rb.remove(&o.order_id);
            release_cancelled(guard, emitter, &o, reason);
            emitter
                .emit(
                    serde_json::json!(
                        { "t" : now_ms(), "ev" : "rest_cancelled", "lane" : o.lane, "tok"
                        : & o.token[..o.token.len().min(14)], "reason" : reason, "filled"
                        : 0.0, "released" : o.shares }
                    ),
                );
        }
        Verdict::Unknown => {}
    }
}
fn rest_verdict(
    levels: &Arc<copybot_hot::book::Levels>,
    prints: &Arc<copybot_hot::book::TradePrints>,
    o: &copybot_hot::resting::Resting,
) -> copybot_hot::restwatch::RestVerdict {
    use copybot_hot::restwatch::{classify, Evidence, FEED_LIVENESS_WINDOW};
    classify(
        &Evidence {
            anchor_remaining: o.his_remaining,
            anchor_printed: o.printed_at_place,
            anchor_contested: o.contested_at_place,
            level_now: levels.size_at(&o.token, o.his_price),
            printed_now: prints.cumulative_at(&o.token, o.his_price),
            feed_alive: levels.feed_alive(FEED_LIVENESS_WINDOW),
        },
    )
}
fn latch_buys(
    lane: &copybot_hot::lanes::Lane,
    incidents: &Arc<std::sync::Mutex<copybot_hot::incidents::Incidents>>,
    kind: &str,
    why: &str,
) {
    let mut g = match incidents.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let persisted = g.raise(&lane.cfg.name, kind, why, copybot_hot::ledger::now_secs());
    lane.state.halt_latch.store(true, Ordering::Relaxed);
    match persisted {
        Ok(true) => {
            eprintln!(
                "[{}] INCIDENT {kind}: {why} — buys halted until cleared \
                               (exits keep mirroring)",
                lane.cfg.name
            )
        }
        Ok(false) => {}
        Err(e) => {
            eprintln!(
                "[{}] CRITICAL: buys latched but the incident could not be \
                             persisted ({e}) — a restart may forget this",
                lane.cfg.name
            )
        }
    }
}
const ROUTES: &[(&str, &str)] = &[
    ("GET", "/"),
    ("GET", "/index"),
    ("GET", "/index.html"),
    ("GET", "/pool"),
    ("GET", "/pool2"),
    ("GET", "/api/trades"),
    ("GET", "/api/pool"),
    ("GET", "/api/equity"),
    ("GET", "/api/pnl"),
    ("GET", "/api/positions"),
    ("GET", "/api/response_times"),
    ("GET", "/api/errors"),
    ("GET", "/api/matchup"),
    ("GET", "/api/status"),
    ("POST", "/api/wallets"),
    ("POST", "/api/arm"),
    ("POST", "/api/flatten"),
    ("POST", "/api/incident/clear"),
];
fn header_of<'a>(req: &'a str, name: &str) -> Option<&'a str> {
    let want = format!("{}:", name.to_ascii_lowercase());
    req.lines()
        .take_while(|l| !l.is_empty())
        .find(|l| l.to_ascii_lowercase().starts_with(&want))
        .map(|l| l[want.len()..].trim())
}
fn mutation_refusal(req: &str) -> Option<(&'static str, String)> {
    let ct = header_of(req, "content-type").unwrap_or("");
    let ct_main = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if ct_main != "application/json" {
        return Some((
            "415 Unsupported Media Type",
            format!(
                "mutations require Content-Type: application/json (got {ct:?}). This is what \
stops a hostile page in your browser from aiming a request at this origin: that content \
type forces a preflight, and we answer none."
            ),
        ));
    }
    if let Some(origin) = header_of(req, "origin") {
        let host = header_of(req, "host").unwrap_or("");
        let authority = origin.split("://").nth(1).unwrap_or("");
        let origin_host = authority.split(':').next().unwrap_or("");
        let ok = origin.is_empty() || (!authority.is_empty() && authority == host)
            || origin_host == "127.0.0.1" || origin_host == "localhost";
        if !ok {
            return Some((
                "403 Forbidden",
                format!(
                    "cross-origin mutation refused (Origin {origin:?}, Host {host:?})"
                ),
            ));
        }
    }
    None
}
fn route_refusal(method: &str, path: &str) -> Option<(&'static str, &'static str)> {
    if method == "GET" && path == "/api/status" {
        return None;
    }
    if ROUTES.iter().any(|(_, p)| *p == path) {
        return Some(("405 Method Not Allowed", "method not allowed for this path"));
    }
    Some(("404 Not Found", "no such endpoint"))
}
#[derive(Debug, Clone, Copy, PartialEq)]
enum MergeOutcome {
    Merged,
    DidNotHappen,
    Unresolved,
}
async fn run_merge_native(
    http: &reqwest::Client,
    rpc_url: &str,
    key: &[u8; 32],
    safe: &[u8; 20],
    adapter: &[u8; 20],
    condition: &str,
    pairs: f64,
    chain_id: u64,
) -> Result<String, String> {
    if !(pairs.is_finite() && pairs > 0.0) {
        return Err(format!("refusing to merge {pairs} pairs"));
    }
    let cid = copybot_hot::merge::condition_id_bytes(condition)
        .ok_or_else(|| format!("unreadable conditionId {condition}"))?;
    let amount = (pairs * 1e6).floor() as u128;
    if amount == 0 {
        return Err("merge amount rounds to zero".into());
    }
    let inner = copybot_hot::merge::merge_calldata(
        &copybot_hot::merge::COLLATERAL_PUSD,
        &cid,
        &copybot_hot::merge::BINARY_PARTITION,
        amount,
    );
    let tx = copybot_hot::txsend::send_safe_call(
            http,
            rpc_url,
            key,
            safe,
            adapter,
            &inner,
            chain_id,
        )
        .await?;
    match copybot_hot::txsend::wait_receipt(
            http,
            rpc_url,
            &tx,
            copybot_hot::mergeclaim::EXECUTOR_TIMEOUT_SECS,
        )
        .await
    {
        Ok(true) => Ok(tx),
        Ok(false) => Err(format!("merge reverted on chain ({tx})")),
        Err(e) => Err(format!("{e} — outcome UNKNOWN, hold the pair")),
    }
}
async fn seed_pair_ledger(
    http: &reqwest::Client,
    leader: &str,
    ledger: &std::sync::Mutex<copybot_hot::pairledger::PairLedger>,
) -> Result<(usize, usize), String> {
    let mut all: Vec<serde_json::Value> = Vec::new();
    for offset in (0..4000).step_by(500) {
        let url = format!(
            "https://data-api.polymarket.com/activity?\
user={leader}&limit=500&offset={offset}"
        );
        let rows: Vec<serde_json::Value> = match http.get(&url).send().await {
            Ok(r) if r.status().is_success() => {
                r.json().await.map_err(|e| e.to_string())?
            }
            Ok(r) => return Err(format!("activity HTTP {}", r.status())),
            Err(e) => return Err(e.to_string()),
        };
        let n = rows.len();
        all.extend(rows);
        if n < 500 {
            break;
        }
    }
    let seeds = copybot_hot::pairledger::seed_rows_from_activity(&all);
    let n = seeds.len();
    let mut g = ledger.lock().map_err(|_| "pair ledger lock poisoned".to_string())?;
    g.seed(leader, &seeds);
    let with_balance = seeds
        .iter()
        .filter(|(_, c, _, _)| g.balance(leader, c).map(|b| b > 1e-6).unwrap_or(true))
        .map(|(_, c, _, _)| c.clone())
        .collect::<std::collections::HashSet<_>>()
        .len();
    Ok((n, with_balance))
}
fn recovery_target_shares(
    leader_size: f64,
    leader_avg_price: f64,
    our_avg_price: f64,
    pct: f64,
    scale: f64,
    min_order_usd: f64,
    max_effective_pct: f64,
    compound: bool,
) -> Option<f64> {
    copybot_hot::budget::target_shares(
        leader_size,
        leader_avg_price,
        our_avg_price,
        pct,
        scale,
        min_order_usd,
        max_effective_pct,
        compound,
    )
}
fn marked_position_value(rows: &serde_json::Value) -> Option<f64> {
    let rows = rows.as_array()?;
    let mut total = 0.0;
    for p in rows {
        let v = p["currentValue"]
            .as_f64()
            .or_else(|| p["currentValue"].as_str().and_then(|x| x.parse().ok()))?;
        if !v.is_finite() {
            return None;
        }
        total += v;
    }
    Some(total)
}
fn handle_wallet_patch(
    reg: &copybot_hot::wallets::WalletRegistry,
    physical: Option<f64>,
    live_lanes: &[String],
    body: &str,
) -> (&'static str, String) {
    use copybot_hot::wallets::WalletSpec;
    let bad = |m: String| (
        "400 Bad Request",
        serde_json::json!({ "error" : m }).to_string(),
    );
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return bad(format!("bad json: {e}")),
    };
    let Some(obj) = v.as_object() else {
        return bad("body must be a JSON object".into())
    };
    const KNOWN: &[&str] = &[
        "name",
        "leader",
        "seed_usd",
        "pct",
        "enabled",
        "min_buy_price",
        "max_buy_price",
        "max_effective_pct",
        "compound",
        "copy_makers",
        "exclude_political",
        "leader_max_order_usd",
        "leader_peak_exposure_usd",
    ];
    let unknown: Vec<&str> = obj
        .keys()
        .map(|k| k.as_str())
        .filter(|k| !KNOWN.contains(k))
        .collect();
    if !unknown.is_empty() {
        return bad(
            format!("unknown field(s): {}. Nothing was changed.", unknown.join(", ")),
        );
    }
    let name = match v["name"].as_str() {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => return bad("name required".into()),
    };
    let finite_pos = |x: f64| x.is_finite() && x > 0.0;
    let result = reg
        .transact(|cur| {
            let mut next = cur.to_vec();
            let existing = next.iter().position(|w| w.name == name);
            const BUILD_ONLY: &[&str] = &[
                "pct",
                "min_buy_price",
                "max_buy_price",
                "max_effective_pct",
                "compound",
                "copy_makers",
                "exclude_political",
            ];
            if existing.is_some() && live_lanes.iter().any(|l| l == &name) {
                let attempted: Vec<&str> = BUILD_ONLY
                    .iter()
                    .copied()
                    .filter(|k| obj.contains_key(*k))
                    .collect();
                if !attempted.is_empty() {
                    return Err(
                        format!(
                            "{} cannot change while {name} is loaded — the running lane keeps its \
built configuration, so accepting this would show one risk level and trade another. \
Disable {name}, then re-enable it, to rebuild the lane with the new settings.",
                            attempted.join(", ")
                        ),
                    );
                }
            }
            match existing {
                Some(i) => {
                    if obj.contains_key("leader") {
                        return Err(
                            format!(
                                "a lane's leader is its identity and cannot change — {name}'s \
positions were copied from its current leader and their exit signals come from that \
wallet. Retire {name} and create a new wallet for the new leader."
                            ),
                        );
                    }
                    if let Some(sd) = obj.get("seed_usd") {
                        let sd = sd.as_f64().unwrap_or(f64::NAN);
                        if !finite_pos(sd) {
                            return Err("seed_usd must be a finite number > 0".into());
                        }
                        next[i].seed_usd = sd;
                    }
                    if let Some(en) = obj.get("enabled") {
                        match en.as_bool() {
                            Some(b) => next[i].enabled = b,
                            None => return Err("enabled must be true or false".into()),
                        }
                    }
                }
                None => {
                    let leader = match v["leader"].as_str() {
                        Some(
                            l,
                        ) if l.starts_with("0x") && l.len() == 42
                            && l[2..].chars().all(|c| c.is_ascii_hexdigit()) => {
                            l.to_lowercase()
                        }
                        _ => {
                            return Err(
                                "a new wallet needs a 0x… leader address (42 hex chars)"
                                    .into(),
                            );
                        }
                    };
                    if let Some(clash) = next
                        .iter()
                        .find(|w| w.leader.eq_ignore_ascii_case(&leader))
                    {
                        return Err(
                            format!(
                                "leader {leader} is already copied by wallet {:?}. One leader may \
back only one wallet — two would copy every fill twice.",
                                clash.name
                            ),
                        );
                    }
                    let seed = v["seed_usd"].as_f64().unwrap_or(f64::NAN);
                    if !finite_pos(seed) {
                        return Err("seed_usd must be a finite number > 0".into());
                    }
                    let pct = v["pct"].as_f64().unwrap_or(f64::NAN);
                    if !(pct.is_finite() && pct > 0.0 && pct <= 1.0) {
                        return Err("pct must be in (0, 1]".into());
                    }
                    let bmin = v["min_buy_price"].as_f64().unwrap_or(0.02);
                    let bmax = v["max_buy_price"].as_f64().unwrap_or(0.95);
                    if !(bmin.is_finite() && bmax.is_finite() && bmin > 0.0 && bmax < 1.0
                        && bmin < bmax)
                    {
                        return Err(
                            "buy band must satisfy 0 < min_buy_price < max_buy_price < 1"
                                .into(),
                        );
                    }
                    let eff = v["max_effective_pct"].as_f64().unwrap_or(0.05);
                    if !(eff.is_finite() && eff > 0.0 && eff <= 1.0) {
                        return Err("max_effective_pct must be in (0, 1]".into());
                    }
                    let leader_max_order_usd = v["leader_max_order_usd"].as_f64();
                    let leader_peak_exposure_usd = v["leader_peak_exposure_usd"]
                        .as_f64();
                    if !leader_max_order_usd.is_some_and(finite_pos)
                        || !leader_peak_exposure_usd.is_some_and(finite_pos)
                    {
                        return Err(
                            format!(
                                "wallet {name}: leader_max_order_usd and leader_peak_exposure_usd are \
                         REQUIRED and must be > 0 — they decide whether this lane's caps \
                         truncate the leader's biggest orders. Measure them with \
                         `tools/leader_stats.py <address>`; do NOT copy another wallet's \
                         numbers."
                            ),
                        );
                    }
                    next.push(WalletSpec {
                        name: name.clone(),
                        leader,
                        seed_usd: seed,
                        pct,
                        leader_max_order_usd,
                        leader_peak_exposure_usd,
                        enabled: v["enabled"].as_bool().unwrap_or(true),
                        stop_mode: "none".into(),
                        min_buy_price: bmin,
                        max_buy_price: bmax,
                        max_effective_pct: eff,
                        compound: v["compound"].as_bool().unwrap_or(true),
                        lane_id: None,
                        copy_makers: v["copy_makers"].as_bool().unwrap_or(true),
                        copy_maker_sells: v["copy_maker_sells"].as_bool(),
                        exclude_political: v["exclude_political"]
                            .as_bool()
                            .unwrap_or(false),
                        risk: serde_json::from_value(v["risk"].clone()).ok().flatten(),
                        buy_slippage_c: None,
                        sell_slippage_c: None,
                        sell_floor_frac: None,
                        min_order_usd: None,
                        min_fill_floor: None,
                        sell_all_frac: None,
                    });
                }
            }
            let sum: f64 = next.iter().filter(|w| w.enabled).map(|w| w.seed_usd).sum();
            let before: f64 = cur.iter().filter(|w| w.enabled).map(|w| w.seed_usd).sum();
            let overcommit = if sum > before + 1e-6 {
                match physical {
                    Some(p) if sum > p + 1e-6 => {
                        Some(
                            format!(
                                "enabled budgets ${sum:.2} exceed the shared wallet ${p:.2} by \
${:.2} — ACCEPTED: a virtual budget is a spending cap, not a reservation. The venue \
refuses an order the wallet cannot fund; it does not lose money. Add funds if lanes \
start missing fills.",
                                sum - p
                            ),
                        )
                    }
                    Some(_) => None,
                    None => {
                        Some(
                            "wallet equity is not currently known, so this INCREASE could not be \
checked against a real balance — ACCEPTED unchecked."
                                .to_string(),
                        )
                    }
                }
            } else {
                None
            };
            if let Some(w) = &overcommit {
                eprintln!("[wallets] ⚠️  {w}");
            }
            Ok(next)
        });
    match result {
        Ok(()) => {
            let sum: f64 = reg
                .specs()
                .iter()
                .filter(|w| w.enabled)
                .map(|w| w.seed_usd)
                .sum();
            let warning = physical
                .filter(|p| sum > p + 1e-6)
                .map(|p| {
                    format!(
                        "enabled budgets ${sum:.2} exceed the wallet ${p:.2} \
by ${:.2} — accepted; the venue refuses what the wallet cannot fund",
                        sum - p
                    )
                });
            (
                "200 OK",
                serde_json::json!(
                    { "ok" : true, "name" : name, "sum_seed" : (sum * 100.0).round() /
                    100.0, "warning" : warning }
                )
                    .to_string(),
            )
        }
        Err(e) if e.starts_with("cannot persist") => {
            ("500 Internal Server Error", serde_json::json!({ "error" : e }).to_string())
        }
        Err(e) => bad(e),
    }
}
fn actor_of(req: &str) -> String {
    match header_of(req, "cf-access-authenticated-user-email") {
        Some(e) if !e.trim().is_empty() => e.trim().to_string(),
        _ => "unidentified (no Cf-Access identity on the request)".to_string(),
    }
}
fn handle_arm_post(path: &str, body: &str, actor: &str) -> (&'static str, String) {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            return (
                "400 Bad Request",
                serde_json::json!({ "error" : format!("bad json: {e}") }).to_string(),
            );
        }
    };
    let Some(lane) = v["lane"].as_str().filter(|s| !s.is_empty()) else {
        return (
            "400 Bad Request",
            serde_json::json!({ "error" : "lane required" }).to_string(),
        );
    };
    let state = v["state"].as_str().unwrap_or("");
    let (armed, halt) = match state {
        "armed" => {
            let want = format!("arm {lane}");
            if v["phrase"].as_str() != Some(want.as_str()) {
                return (
                    "400 Bad Request",
                    serde_json::json!(
                        { "error" : format!("arming requires phrase {want:?}") }
                    )
                        .to_string(),
                );
            }
            (true, false)
        }
        "halt_buys" => (true, true),
        "off" => (false, false),
        _ => {
            return (
                "400 Bad Request",
                serde_json::json!({ "error" : "state must be armed | halt_buys | off" })
                    .to_string(),
            );
        }
    };
    let _oplock = copybot_hot::errors::LockFile::acquire(path);
    let mut doc: serde_json::Value = match std::fs::read_to_string(path) {
        Ok(raw) if raw.trim().is_empty() => serde_json::json!({ "lanes" : {} }),
        Ok(raw) => {
            match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(e) => {
                    return (
                        "409 Conflict",
                        serde_json::json!(
                            { "error" :
                            format!("the operator file is present but unreadable ({e}); refusing to replace \
it, because doing so would drop every other lane's arm state. Fix or move {path} by \
hand, then retry.")
                            }
                        )
                            .to_string(),
                    );
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            serde_json::json!({ "lanes" : {} })
        }
        Err(e) => {
            return (
                "500 Internal Server Error",
                serde_json::json!(
                    { "error" : format!("cannot read the operator file: {e}") }
                )
                    .to_string(),
            );
        }
    };
    if !doc["lanes"].is_object() {
        return (
            "409 Conflict",
            serde_json::json!(
                { "error" :
                "the operator file has no `lanes` object; refusing to overwrite it" }
            )
                .to_string(),
        );
    }
    doc["lanes"][lane] = serde_json::json!(
        { "armed" : armed, "halt_buys" : halt, "by" : actor, "at" :
        copybot_hot::ledger::now_secs(), "state" : state, }
    );
    doc["_meta"] = serde_json::json!(
        { "by" : "api:/api/arm", "at" : copybot_hot::ledger::now_secs(), "why" :
        format!("{actor} set {lane} to {state}"), "actor" : actor, "pid" :
        std::process::id(), }
    );
    let tmp = format!(
        "{path}.api.{}.{}.tmp", std::process::id(), std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(| d | d.as_nanos()).unwrap_or(0)
    );
    let done = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(serde_json::to_string(&doc).unwrap_or_default().as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        if let Some(dir) = std::path::Path::new(path).parent() {
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    })();
    match done {
        Ok(()) => {
            (
                "200 OK",
                serde_json::json!(
                    { "ok" : true, "lane" : lane, "state" : state, "armed" : armed,
                    "halt_buys" : halt }
                )
                    .to_string(),
            )
        }
        Err(e) => {
            (
                "500 Internal Server Error",
                serde_json::json!({ "error" : format!("write: {e}") }).to_string(),
            )
        }
    }
}
fn handle_flatten_post(
    path: &str,
    body: &str,
    armed_of: impl Fn(&str) -> Option<bool>,
) -> (&'static str, String) {
    let intent: copybot_hot::flatten::Intent = match serde_json::from_str(body) {
        Ok(i) => i,
        Err(e) => {
            return (
                "400 Bad Request",
                serde_json::json!({ "error" : format!("bad intent: {e}") }).to_string(),
            );
        }
    };
    if copybot_hot::flatten::Mode::parse(&intent.mode).is_none() {
        return (
            "400 Bad Request",
            serde_json::json!({ "error" : format!("unknown mode {:?}", intent.mode) })
                .to_string(),
        );
    }
    if intent.lane.is_empty() || intent.lane.len() > 32
        || !intent
            .lane
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return (
            "400 Bad Request",
            serde_json::json!({ "error" : "lane must be 1-32 chars of [A-Za-z0-9_-]" })
                .to_string(),
        );
    }
    match armed_of(&intent.lane) {
        None => {
            return (
                "404 Not Found",
                serde_json::json!(
                    { "error" : format!("unknown lane {:?} — nothing has been sold",
                    intent.lane) }
                )
                    .to_string(),
            );
        }
        Some(armed) => {
            if let Err(reason) = copybot_hot::flatten::gate(
                &intent,
                &intent.lane,
                armed,
                copybot_hot::ledger::now_secs(),
            ) {
                return (
                    "409 Conflict",
                    serde_json::json!(
                        { "error" : reason, "note" :
                        "NOT queued — nothing has been sold" }
                    )
                        .to_string(),
                );
            }
        }
    }
    let path = format!("{path}.{}", intent.lane);
    let tmp = format!(
        "{path}.tmp.{}.{}", std::process::id(), std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(| d | d.as_nanos()).unwrap_or(0)
    );
    let write = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })();
    match write {
        Ok(()) => {
            (
                "200 OK",
                serde_json::json!(
                    { "ok" : true, "lane" : intent.lane, "mode" : intent.mode, "note" :
                    "queued — the runtime gates and executes it" }
                )
                    .to_string(),
            )
        }
        Err(e) => {
            (
                "500 Internal Server Error",
                serde_json::json!({ "error" : format!("write: {e}") }).to_string(),
            )
        }
    }
}
fn seed_operator_file(path: &std::path::Path, body: &[u8]) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| {
                format!("create operator directory {}: {e}", parent.display())
            })?;
    }
    let tmp = std::path::PathBuf::from(
        format!("{}.seed-{}.tmp", path.display(), std::process::id()),
    );
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(body)?;
        file.flush()?;
        file.sync_data()?;
        std::fs::rename(&tmp, path)?;
        let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if let Err(e) = result {
        let _cleanup = std::fs::remove_file(&tmp);
        return Err(format!("seed operator file {}: {e}", path.display()));
    }
    Ok(())
}
fn halt_buys_on_persistence_fault_with(
    control: &Arc<std::sync::Mutex<Control>>,
    router: &Arc<Router>,
    pend: Option<&Arc<std::sync::Mutex<copybot_hot::pending::PendingLog>>>,
    incidents: &Arc<std::sync::Mutex<copybot_hot::incidents::Incidents>>,
) {
    let ledger_fault = control
        .lock()
        .map(|c| !c.ledger.persistence_ok())
        .unwrap_or(true);
    let pend_fault = pend
        .map(|p| p.lock().map(|l| !l.persistence_ok()).unwrap_or(true))
        .unwrap_or(false);
    let fault = ledger_fault || pend_fault;
    if fault {
        for lane in router.snapshot().iter() {
            latch_buys(
                lane,
                incidents,
                "persistence",
                "ledger or pending log cannot persist",
            );
        }
        eprintln!(
            "[CRITICAL] durable-memory fault (ledger_ok={}, pending_ok={}) — all \
BUY lanes halted; exits remain enabled",
            ! ledger_fault, ! pend_fault
        );
    }
}
fn book_and_resolve(
    control: &Arc<std::sync::Mutex<Control>>,
    pend: &Arc<std::sync::Mutex<copybot_hot::pending::PendingLog>>,
    router: &Arc<Router>,
    incidents: &Arc<std::sync::Mutex<copybot_hot::incidents::Incidents>>,
    emitter: &Arc<Emitter>,
    lane: &str,
    token: &str,
    side: u8,
    limit: f64,
    body: &str,
    his_price: Option<f64>,
    order_hash: &str,
    resting: bool,
) -> (bool, f64) {
    let receipt = control
        .lock()
        .unwrap()
        .book_response(lane, token, side, limit, body, his_price, order_hash, resting);
    let mut resolve_err: Option<String> = None;
    let had_receipt = receipt.is_some();
    let still_open_after: usize;
    let filled_shares = receipt.as_ref().map(|r| r.shares).unwrap_or(0.0);
    let booked = match receipt {
        Some(r) => {
            if let Err(e) = pend.lock().unwrap().resolve_booked(order_hash, "matched", r)
            {
                resolve_err = Some(e.to_string());
                eprintln!(
                    "[{lane}] fill IS booked but its pending row stayed open: {e}"
                );
            }
            true
        }
        None => {
            eprintln!(
                "[{lane}] NOT BOOKED: {} {} @ {limit} — refusal inside a 200, an \
                       AMBIGUOUS reply, or a failed ledger append. The pending row stays \
                       OPEN for the resolver; if it is still unbooked later, this is \
                       unattributed inventory.",
                if side == 0 { "BUY" } else { "SELL" }, & token[..token.len().min(14)]
            );
            false
        }
    };
    still_open_after = pend.lock().unwrap().len();
    let orphaned = had_receipt && resolve_err.is_none()
        && pend.lock().unwrap().is_open(order_hash);
    if orphaned {
        eprintln!(
            "[{lane}] ⛔ ORPHANED PENDING ROW: fill booked, resolve reported success, \
                   and the row {order_hash} is STILL OPEN. Its shares remain reserved."
        );
    }
    emitter
        .emit(
            serde_json::json!(
                { "t" : now_ms(), "ev" : "book_resolve", "lane" : lane, "tok" : &
                token[..token.len().min(14)], "side" : if side == 0 { "BUY" } else {
                "SELL" }, "booked" : booked, "had_receipt" : had_receipt, "resolve_err" :
                resolve_err, "orphaned" : orphaned, "pending_open_after" :
                still_open_after, "order_hash" : order_hash, }
            ),
        );
    halt_buys_on_persistence_fault_with(control, router, Some(pend), incidents);
    (booked, filled_shares)
}
struct Emitter {
    log: std::sync::Mutex<copybot_hot::capped::CappedLog>,
    stdout: Option<mpsc::Sender<String>>,
    throttle: std::sync::Mutex<std::collections::HashMap<&'static str, (i64, u64)>>,
}
const RATE_LIMITED: &[(&str, i64)] = &[
    ("warm", 60),
    ("feed_stats", 60),
    ("recon_gap", 60),
    ("latency", 30),
];
impl Emitter {
    fn new(path: String, mirror_stdout: bool) -> Self {
        let stdout = if mirror_stdout {
            let (tx, mut rx) = mpsc::channel::<String>(4096);
            tokio::spawn(async move {
                while let Some(line) = rx.recv().await {
                    println!("{line}");
                }
            });
            Some(tx)
        } else {
            None
        };
        let p = std::path::Path::new(&path);
        let dir = p
            .parent()
            .map(|d| d.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let name = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("events")
            .to_string();
        let log = copybot_hot::capped::CappedLog::open(
                &dir,
                &name,
                copybot_hot::capped::DEFAULT_CAP_BYTES,
            )
            .unwrap_or_else(|e| panic!("cannot open ledger {}: {e}", dir.display()));
        Self {
            log: std::sync::Mutex::new(log),
            stdout,
            throttle: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
    fn ledger_status(&self) -> serde_json::Value {
        self.log.lock().unwrap().status()
    }
    fn emit(&self, mut v: serde_json::Value) {
        let mut suppressed = 0u64;
        if let Some(kind) = v["ev"].as_str() {
            if let Some((k, gap)) = RATE_LIMITED.iter().find(|(k, _)| *k == kind) {
                let now = copybot_hot::ledger::now_secs();
                let mut t = self.throttle.lock().unwrap();
                let e = t.entry(k).or_insert((0, 0));
                if now - e.0 < *gap {
                    e.1 += 1;
                    return;
                }
                suppressed = std::mem::take(&mut e.1);
                e.0 = now;
            }
        }
        if suppressed > 0 {
            v["suppressed"] = serde_json::json!(suppressed);
        }
        let line = v.to_string();
        let mut log = self.log.lock().unwrap();
        if let Err(e) = log.write_line(&line) {
            let n = log.write_errors;
            if n == 1 || n.is_power_of_two() {
                eprintln!("[CRITICAL] event ledger write failed ({n} errors): {e}");
            }
        }
        drop(log);
        if let Some(stdout) = &self.stdout {
            let _ = stdout.try_send(line);
        }
    }
}
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
async fn submit_tracked(
    pend: &Arc<std::sync::Mutex<copybot_hot::pending::PendingLog>>,
    em: &Emitter,
    prov: &copybot_hot::provenance::Provenance,
    order_hash: String,
    resting: bool,
    paths: &[(String, reqwest::Client)],
    url: &str,
    body: &str,
    headers: &[(&'static str, String)],
    timeout: Duration,
    already_durable: bool,
) -> Result<
    (copybot_hot::race_send::Outcome, Vec<copybot_hot::race_send::PathResult>),
    String,
> {
    use copybot_hot::race_send::Outcome;
    let hash = order_hash.clone();
    let p = copybot_hot::pending::Pending {
        lane: prov.lane.clone(),
        token: prov.token.clone(),
        side: prov.side,
        order_hash,
        shares: prov.shares,
        limit: prov.limit,
        ts: copybot_hot::ledger::now_secs(),
        why: prov.why(),
        resting,
    };
    {
        let mut row = prov.to_json();
        if let Some(m) = row.as_object_mut() {
            m.insert("t".into(), (now_ms() as i64).into());
            m.insert("ev".into(), "order_submit".into());
            m.insert("order_hash".into(), hash.clone().into());
            m.insert("resting".into(), resting.into());
        }
        em.emit(row);
    }
    if prov.is_oversell() {
        eprintln!(
            "[{}] ⚠️ {} would sell {:.4} sh of …{} while the ledger holds {:.4}",
            prov.lane, prov.origin.label(), prov.shares, & prov.token[prov.token.len()
            .saturating_sub(8)..], prov.held_before
        );
    }
    if !already_durable {
        pend.lock().unwrap().record(p)?;
    }
    let (outcome, results) = copybot_hot::race_send::race_submit(
            paths,
            url,
            body,
            headers,
            timeout,
        )
        .await;
    match &outcome {
        Outcome::Rejected { .. } => {
            pend.lock().unwrap().resolve(&hash, "rejected", None);
        }
        _ => {}
    }
    em.emit(
        serde_json::json!(
            { "t" : now_ms(), "ev" : "order_result", "origin" : prov.origin.kind(),
            "lane" : prov.lane, "tok" : prov.token, "side" : if prov.side == 1 { "SELL" }
            else { "BUY" }, "order_hash" : hash, "outcome" : format!("{outcome:?}"),
            "paths" : results.len(), }
        ),
    );
    Ok((outcome, results))
}
async fn reactivate_lane(
    lane: &std::sync::Arc<copybot_hot::lanes::Lane>,
    control: &Arc<std::sync::Mutex<Control>>,
    http: &reqwest::Client,
) -> Result<usize, String> {
    let name = lane.cfg.name.clone();
    let leader = format!("0x{}", hex::encode(lane.cfg.wallet20));
    let snap = copybot_hot::positions::fetch(
            http,
            "https://data-api.polymarket.com",
            &leader,
            "0.0001",
            "",
            copybot_hot::ledger::now_secs(),
        )
        .await;
    if !snap.may_act_destructively() {
        return Err(
            format!(
                "leader book is not a complete list ({}) — refusing to seed \
from a partial book, because an omitted token reads as fully exited",
                snap.completeness.reason()
            ),
        );
    }
    let rows: Vec<serde_json::Value> = snap.rows;
    let mut book: std::collections::HashMap<String, f64> = Default::default();
    for p in &rows {
        let t = p["asset"].as_str().unwrap_or("").to_string();
        let sz = p["size"]
            .as_f64()
            .or_else(|| p["size"].as_str().and_then(|x| x.parse().ok()))
            .unwrap_or(0.0);
        if !t.is_empty() && sz > 0.0 {
            book.insert(t, sz);
        }
    }
    let held = control.lock().unwrap().ledger.holdings(&name);
    if book.is_empty() && !held.is_empty() {
        return Err("leader snapshot is empty while our ledger has holdings".into());
    }
    control.lock().unwrap().seed_his_book(&name, &book);
    Ok(book.len())
}
async fn rescue_exit(
    first_body: &str,
    ord: copybot_hot::order::Order,
    pk: Option<[u8; 32]>,
    owner: &str,
    order_type: &str,
    intended_shares: f64,
    full_holding: f64,
    limit: f64,
    paths: &[(String, reqwest::Client)],
    clob: &str,
    creds: Option<&copybot_hot::auth::ApiCreds>,
    addr: &str,
    lane: &str,
    token: &str,
    control: &Arc<std::sync::Mutex<Control>>,
    router: &Arc<Router>,
    emitter: &Arc<Emitter>,
    wake: &tokio::sync::mpsc::UnboundedSender<String>,
    pend: &Arc<std::sync::Mutex<copybot_hot::pending::PendingLog>>,
    incidents: &Arc<std::sync::Mutex<copybot_hot::incidents::Incidents>>,
) -> bool {
    use copybot_hot::exit_rescue as er;
    use copybot_hot::order::{amounts, json_body};
    use copybot_hot::race_send::Outcome;
    let started = std::time::Instant::now();
    let mut fail = er::classify(first_body);
    let mut chain_size = er::reported_balance(first_body);
    let mut attempt = 0usize;
    emitter
        .emit(
            serde_json::json!(
                { "ev" : "exit_rescue_start", "lane" : lane, "token" : token, "why" :
                format!("{fail:?}"), "chain_size" : chain_size, "t" : now_ms() }
            ),
        );
    while let Some(step) = er::next_step(fail, attempt, started.elapsed()) {
        tokio::time::sleep(step.delay).await;
        let shares = er::retry_size(
            step.sell_all,
            chain_size,
            intended_shares,
            full_holding,
        );
        if shares <= 0.0 {
            break;
        }
        let mut o = ord.clone();
        o.salt = copybot_hot::order::safe_salt(er::retry_salt_seed(now_ms(), attempt));
        o.timestamp = now_ms();
        let (ma, ta) = amounts(limit, shares, 1);
        o.maker_amount = ma;
        o.taker_amount = ta;
        let sig = match pk.as_ref().map(|k| o.sign_for_type(k)) {
            Some(s) => s,
            None => break,
        };
        let body = match serde_json::to_string(&json_body(&o, &sig, owner, order_type)) {
            Ok(b) => b,
            Err(_) => break,
        };
        let hdrs = match creds {
            Some(cr) => {
                match copybot_hot::auth::l2_headers(
                    addr,
                    cr,
                    copybot_hot::auth::now_secs(),
                    "POST",
                    "/order",
                    Some(&body),
                ) {
                    Ok(headers) => headers,
                    Err(e) => {
                        emitter
                            .emit(
                                serde_json::json!(
                                    { "ev" : "exit_rescue_FAILED", "lane" : lane, "token" :
                                    token, "why" : format!("L2 header build failed: {e}"), "msg"
                                    : "[CRITICAL] exit rescue could not authenticate", "t" :
                                    now_ms(), }
                                ),
                            );
                        break;
                    }
                }
            }
            None => break,
        };
        let retry_hash = hex::encode(o.digest());
        let Ok((outcome, _)) = submit_tracked(
                pend,
                emitter,
                &copybot_hot::provenance::Provenance::own(
                    copybot_hot::provenance::Origin::ExitRescue {
                        attempt: attempt as u32,
                    },
                    lane,
                    token,
                    1,
                    shares,
                    limit,
                    full_holding,
                ),
                retry_hash.clone(),
                false,
                paths,
                &format!("{clob}/order"),
                &body,
                &hdrs,
                std::time::Duration::from_secs(10),
                false,
            )
            .await else {
            emitter
                .emit(
                    serde_json::json!(
                        { "ev" : "exit_rescue_FAILED", "lane" : lane, "token" : token,
                        "why" : "cannot persist pending order", "t" : now_ms() }
                    ),
                );
            break;
        };
        attempt += 1;
        match &outcome {
            Outcome::Matched { body, .. } => {
                book_and_resolve(
                    control,
                    pend,
                    router,
                    incidents,
                    emitter,
                    lane,
                    token,
                    1,
                    limit,
                    body,
                    None,
                    &retry_hash,
                    false,
                );
                let _ = wake.send(token.to_string());
                emitter
                    .emit(
                        serde_json::json!(
                            { "ev" : "exit_rescue_ok", "lane" : lane, "token" : token,
                            "attempts" : attempt, "shares" : shares, "sell_all" : step
                            .sell_all, "secs" : started.elapsed().as_secs_f64(), "t" :
                            now_ms() }
                        ),
                    );
                return true;
            }
            Outcome::Rejected { body } => {
                fail = er::classify(body);
                if let Some(b) = er::reported_balance(body) {
                    chain_size = Some(b);
                }
            }
            _ => break,
        }
    }
    emitter
        .emit(
            serde_json::json!(
                { "ev" : "exit_rescue_FAILED", "lane" : lane, "token" : token, "attempts"
                : attempt, "why" : format!("{fail:?}"), "secs" : started.elapsed()
                .as_secs_f64(), "msg" :
                "[CRITICAL] exit could not be completed — position still held", "t" :
                now_ms() }
            ),
        );
    eprintln!(
        "[{lane}] [CRITICAL] EXIT RESCUE FAILED on {token} after {attempt} attempts \
({fail:?}) — position is STILL HELD"
    );
    false
}
async fn control_task(
    ctl: Arc<std::sync::Mutex<Control>>,
    router: Arc<Router>,
    pend_ctl: Arc<std::sync::Mutex<copybot_hot::pending::PendingLog>>,
    emitter: Arc<Emitter>,
    watch: Arc<std::sync::Mutex<Vec<String>>>,
    fps: Arc<Fingerprints>,
    resting: Arc<copybot_hot::resting::RestingBook>,
    levels_ctl: Arc<copybot_hot::book::Levels>,
    prints_ctl: Arc<copybot_hot::book::TradePrints>,
) {
    let mut last = String::new();
    loop {
        {
            let mut c = ctl.lock().unwrap();
            let in_flight = pend_ctl.lock().unwrap().in_flight();
            c.tick_with(&router, &in_flight);
            for lane in router.snapshot().iter() {
                if !copybot_hot::lanes::needs_book(lane) {
                    continue;
                }
                let held = c.ledger.holdings(&lane.cfg.name);
                let mut w = watch.lock().unwrap();
                for (t, sh) in held.iter() {
                    if *sh <= 1e-9 {
                        continue;
                    }
                    if !w.iter().any(|x| x == t) {
                        w.push(t.clone());
                    }
                }
                let pinned: std::collections::HashSet<String> = resting
                    .all()
                    .into_iter()
                    .map(|r| r.token)
                    .chain(fps.known_tokens())
                    .chain(
                        held.iter().filter(|(_, s)| **s > 1e-9).map(|(t, _)| t.clone()),
                    )
                    .collect();
                copybot_hot::book::evict_watch_over_cap(
                    &mut w,
                    copybot_hot::book::WATCH_CAP,
                    &pinned,
                );
                let keep: std::collections::HashSet<String> = w
                    .iter()
                    .cloned()
                    .collect();
                drop(w);
                levels_ctl.retain_tokens(&keep);
                prints_ctl.retain_tokens(&keep);
            }
            let snap = c.status(&router).to_string();
            if snap != last {
                emitter
                    .emit(
                        serde_json::json!(
                            { "t" : now_ms(), "ev" : "control", "lanes" : c.status(&
                            router) ["lanes"] }
                        ),
                    );
                last = snap;
            }
            let _ = &fps;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
const RECON_SECS: u64 = 5;
const RECON_QUIET_SECS: i64 = 120;
const RECON_RELEASE_QUIET_SECS: i64 = 1_800;
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let cfg_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "deploy/copybot2.toml".into());
    let root = match Root::load(&cfg_path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("CONFIG ERROR: {e}");
            std::process::exit(2);
        }
    };
    let lanes = match root.build_lanes() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("CONFIG ERROR: {e}");
            std::process::exit(2);
        }
    };
    let shadow = root.bot.mode == "shadow";
    let live = root.bot.mode == "live" || shadow;
    let funder = root.bot.funder.clone();
    let signer = root.bot.signer.clone();
    let sig_type = root.bot.signature_type;
    if addr20(&funder).is_err() || addr20(&signer).is_err() {
        eprintln!("CONFIG ERROR: funder/signer must be 20-byte hex addresses");
        std::process::exit(2);
    }
    let _execution_lease = if live && !shadow {
        match copybot_hot::lease::ExecutionLease::acquire_for_funder(&funder) {
            Ok(lease) => {
                eprintln!("[lease] acquired {}", lease.path.display());
                Some(lease)
            }
            Err(e) => {
                eprintln!("REFUSING TO START: {e}");
                std::process::exit(2);
            }
        }
    } else {
        None
    };
    let pk_hex = std::env::var("PRIVATE_KEY").unwrap_or_default();
    let pk = match hex::decode(pk_hex.trim_start_matches("0x")) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            Some(k)
        }
        _ => None,
    };
    let pk = match (shadow, pk) {
        (true, None) => {
            eprintln!(
                "[shadow] no PRIVATE_KEY — signing with a THROWAWAY key so orders \
actually reach the venue and are refused. This key controls no funds."
            );
            let mut k = [0u8; 32];
            for (i, b) in k.iter_mut().enumerate() {
                *b = (i as u8).wrapping_add(1);
            }
            Some(k)
        }
        (_, other) => other,
    };
    let signer_addr: String = match pk.as_ref().map(copybot_hot::auth::address_from_key)
    {
        Some(Ok(a)) => {
            if !a.eq_ignore_ascii_case(&signer) {
                eprintln!(
                    "[auth] ⚠️  config signer {signer} does NOT own PRIVATE_KEY \
({a}) — using the DERIVED address everywhere. Fix the config."
                );
            }
            a
        }
        Some(Err(e)) => {
            eprintln!("[auth] cannot derive signer address: {e}");
            signer.clone()
        }
        None => signer.clone(),
    };
    if live && !shadow && pk.is_none() {
        eprintln!("REFUSING TO START: mode=live but PRIVATE_KEY is unset or malformed");
        std::process::exit(2);
    }
    let emitter = Arc::new(Emitter::new(root.bot.events_path.clone(), !live || shadow));
    let prints = Arc::new(copybot_hot::book::TradePrints::new());
    let levels = Arc::new(copybot_hot::book::Levels::new());
    let leader_pnl: Arc<
        std::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>,
    > = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let custody_report: Arc<std::sync::Mutex<Option<copybot_hot::custody::Report>>> = Arc::new(
        std::sync::Mutex::new(None),
    );
    let reanchor_pulse: Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                String,
                (i64, i64, usize, usize, usize, usize, usize),
            >,
        >,
    > = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let rt_path = format!("{}.response_times", root.bot.control_path);
    let response_times = Arc::new(
        copybot_hot::response_times::Ring::with_file(&rt_path),
    );
    if let Err(e) = response_times.compact() {
        eprintln!(
            "[rt] could not compact {rt_path}: {e} — chart still works, file grows"
        );
    }
    let router = Arc::new(Router::new(lanes));
    let book = Arc::new(RaceBook::new());
    let stats = Arc::new(FeedStats::default());
    let feeds: Vec<FeedConfig> = root
        .feed
        .iter()
        .map(|f| FeedConfig {
            name: f.name.clone(),
            url: f.url.clone(),
            sockets: f.sockets,
        })
        .collect();
    let total_sockets: usize = feeds.iter().map(|f| f.sockets).sum();
    emitter
        .emit(
            serde_json::json!(
                { "t" : now_ms(), "ev" : "boot", "mode" : root.bot.mode, "lanes" : router
                .snapshot().iter().map(| l | serde_json::json!({ "name" : l.cfg.name,
                "wallet" : format!("0x{}", hex::encode(l.cfg.wallet20)), "sizing" :
                format!("{:?}", l.policy().sizing), "execution" : format!("{:?}", l.cfg
                .execution), "daily_usd" : l.policy().caps.daily_usd, })).collect::< Vec
                < _ >> (), "feeds" : feeds.iter().map(| f | format!("{}x{}", f.name, f
                .sockets)).collect::< Vec < _ >> (), "sockets" : total_sockets, "note" :
                if shadow {
                "SHADOW — full live path, credentials forced OFF, every \
                             order will be rejected 401"
                } else if live { "LIVE — lanes still need arming in the control file" }
                else { "DRY — nothing will be submitted" }, }
            ),
        );
    let fps = Arc::new(Fingerprints::new());
    let fpscore = Arc::new(FingerprintScore::new());
    let resting_book = Arc::new(copybot_hot::resting::RestingBook::new());
    let sell_tranches = Arc::new(copybot_hot::lanes::TrancheLedger::new(4096));
    let fire_rate = Arc::new(
        copybot_hot::firerate::FireRate::new(
            router.len(),
            copybot_hot::firerate::DEFAULT_LIMIT,
        ),
    );
    let incidents_path = format!("{}.incidents", root.bot.control_path);
    let incidents = Arc::new(
        std::sync::Mutex::new(copybot_hot::incidents::Incidents::open(&incidents_path)),
    );
    {
        let g = incidents.lock().unwrap();
        if let Some(e) = g.unreadable() {
            eprintln!(
                "[incidents] CANNOT READ {incidents_path}: {e} — EVERY lane is \
                       latched for buys until this is fixed (exits still mirror)"
            );
        } else if !g.is_empty() {
            for i in g.open_incidents() {
                eprintln!(
                    "[incidents] OPEN: {} — {} ({}) raised at {}; buys stay halted \
                           until cleared",
                    i.lane, i.why, i.kind, i.t
                );
            }
        }
    }
    let funding_path = format!("{}.funding", root.bot.control_path);
    let funding = Arc::new(
        std::sync::Mutex::new(copybot_hot::funding::Funding::open(&funding_path)),
    );
    match funding.lock().unwrap().basis() {
        Some(b) => eprintln!("[funding] declared external funding: ${b:.2}"),
        None => {
            eprintln!(
                "[funding] NO external funding declared — physical P&L will \
                           read UNKNOWN until you record a deposit (see run/*.funding)"
            )
        }
    }
    let wal_path = format!("{}.wal", root.bot.control_path);
    let wal = match copybot_hot::wal::Wal::open(&wal_path) {
        Ok(w) => Arc::new(w),
        Err(e) => {
            if live && !shadow {
                eprintln!("REFUSING TO START: write-ahead log unavailable: {e}");
                std::process::exit(2);
            }
            eprintln!("[wal] {e} — continuing without it (not live)");
            Arc::new(copybot_hot::wal::Wal::disabled())
        }
    };
    let mut wal_recovery = copybot_hot::wal::WalRecovery::new(
        &wal_path,
        &["signal_guard", "pending"],
    );
    let signal_guard = if live && !shadow {
        match copybot_hot::signal_guard::SignalGuard::open_absorbing(
            &root.bot.signal_guard_path,
            Some(&wal_path),
            copybot_hot::ledger::now_secs(),
        ) {
            Ok((guard, rows)) => {
                wal_recovery.absorbed("signal_guard", rows);
                Some(Arc::new(guard))
            }
            Err(e) => {
                eprintln!("REFUSING TO START: signal guard unavailable: {e}");
                std::process::exit(2);
            }
        }
    } else {
        wal_recovery.absorbed("signal_guard", 0);
        None
    };
    let books = Arc::new(BookCache::new());
    let watch_tokens: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(
        std::sync::Mutex::new(Vec::new()),
    );
    {
        let (fptx, _fprx) = std::sync::mpsc::channel();
        tokio::spawn(
            copybot_hot::book::run_book_feed_multi(
                books.clone(),
                watch_tokens.clone(),
                Some((fps.clone(), fpscore.clone(), fptx)),
                Some(prints.clone()),
                Some(levels.clone()),
                3,
            ),
        );
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<RawTx>();
    let watched_addrs = Arc::new(copybot_hot::feeds::WatchedAddresses::new());
    let (_handles, feed_registry) = copybot_hot::feeds::spawn_all(
        &feeds,
        tx.clone(),
        stats.clone(),
        watched_addrs.clone(),
    );
    let feed_registry = Arc::new(feed_registry);
    if root.bot.recycle_secs > 0 {
        let (reg, ev) = (feed_registry.clone(), emitter.clone());
        let every = root.bot.recycle_secs;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(every)).await;
                let ranked = reg.wins_snapshot();
                let losers = reg.score_and_recycle(2, 20);
                if !losers.is_empty() {
                    ev.emit(
                        serde_json::json!(
                            { "t" : now_ms(), "ev" : "socket_recycle", "recycled" :
                            losers, "ranking" : ranked.iter().take(6).map(| (n, w) |
                            serde_json::json!({ "s" : n, "wins" : w })).collect::< Vec <
                            _ >> (), }
                        ),
                    );
                }
            }
        });
        eprintln!(
            "[feeds] socket scoring on: ranking every {every}s, protecting the top 2"
        );
    }
    let risk_cfgs: Vec<(String, copybot_hot::risk::RiskConfig)> = root
        .lane
        .iter()
        .map(|l| (l.name.clone(), l.risk.unwrap_or_default()))
        .collect();
    let ledger = Ledger::new(&root.bot.ledger_path, &risk_cfgs);
    let operator_path = format!("{}.operator", root.bot.control_path);
    if !std::path::Path::new(&operator_path).exists() {
        let seed = serde_json::json!(
            { "_howto" :
            "set armed=true to allow a lane to fire; the control plane can \
                       only ever take this away",
            "lanes" : risk_cfgs.iter().map(| (n, _) | (n.clone(), serde_json::json!({
            "armed" : false }))).collect::< serde_json::Map < _, _ >> () }
        );
        let body = serde_json::to_vec_pretty(&seed)
            .expect("operator seed is composed only of serializable JSON values");
        if let Err(e) = seed_operator_file(std::path::Path::new(&operator_path), &body) {
            if live && !shadow {
                eprintln!("REFUSING TO START: {e}");
                std::process::exit(2);
            }
            eprintln!("[control] WARN: {e}; all lanes remain fail-closed");
        }
    }
    let control = Arc::new(std::sync::Mutex::new(Control::new(ledger, &operator_path)));
    let merge_claims = Arc::new(
        std::sync::Mutex::new(copybot_hot::mergeclaim::MergeClaims::new()),
    );
    let pair_ledger = Arc::new(
        std::sync::Mutex::new(copybot_hot::pairledger::PairLedger::new()),
    );
    let wallets_path = format!("{}.wallets", root.bot.control_path);
    let registry = copybot_hot::wallets::WalletRegistry::load_or_seed(
        &wallets_path,
        root.wallet_specs(),
    );
    match registry.mint_missing_ids() {
        Ok(0) => {}
        Ok(n) => {
            eprintln!(
                "[laneid] minted and persisted {n} lane id(s) — from now on \
                            the stored value is authoritative, not the leader"
            )
        }
        Err(e) => {
            eprintln!(
                "[laneid] ⚠️ could not persist minted lane ids: {e} — they \
                             remain derived, so a leader edit would move them"
            )
        }
    }
    let dupes = registry.duplicate_ids();
    if !dupes.is_empty() {
        for (id, names) in &dupes {
            eprintln!(
                "[laneid] ⛔ id {id} is claimed by {} lanes: {}", names.len(), names
                .join(", ")
            );
        }
        if live && !shadow {
            eprintln!("[boot] ⛔ REFUSING TO START: duplicate lane identities");
            std::process::exit(2);
        }
    }
    control.lock().unwrap().ledger.lane_ids = registry.id_map();
    watched_addrs.set(registry.specs().iter().map(|w| w.leader.clone()));
    eprintln!(
        "[feeds] watching {} leader address(es) for merges alongside the exchanges",
        watched_addrs.snapshot().len()
    );
    {
        let mut c = control.lock().unwrap();
        let mut restored = 0usize;
        for spec in registry.specs() {
            if c.ledger.lanes.contains_key(&spec.name) {
                continue;
            }
            c.ledger.ensure_lane(&spec.name, spec.risk.unwrap_or_default());
            restored += 1;
        }
        if restored > 0 {
            eprintln!(
                "[ledger] restored {restored} runtime wallet book(s) before \
reconciliation — the pool is now fully known"
            );
        }
    }
    if let Some(why) = registry.load_fault() {
        eprintln!("[boot] ⛔ REFUSING TO START: {why}");
        let errpath = format!(
            "{}/errors.jsonl", std::path::Path::new(& root.bot.control_path).parent()
            .map(| p | p.to_string_lossy().into_owned()).unwrap_or_else(|| "run".into())
        );
        copybot_hot::errors::record(
            &errpath,
            &copybot_hot::errors::ErrorRow {
                t: copybot_hot::ledger::now_secs(),
                lane: String::new(),
                kind: "wallets".into(),
                detail: format!("wallet registry unusable: {why}"),
                human: format!(
                    "The bot REFUSED TO START because it could not read its wallet list. \
Nothing has been sold and no position has changed — but it is also not following anyone \
out right now, so treat this as urgent. {why}"
                ),
                severity: "stop".into(),
            },
        );
        if live && !shadow {
            std::process::exit(2);
        }
        eprintln!(
            "[boot] (continuing anyway: not live — this would be fatal in live)"
        );
    }
    if let Ok(reason) = std::env::var("RESET_REALISED") {
        let r = reason.trim();
        if r.is_empty() || r == "0" || r == "1" || r.eq_ignore_ascii_case("false")
            || r.eq_ignore_ascii_case("true")
        {
            eprintln!(
                "[ledger] RESET_REALISED refused: the value must BE the reason \
(written to the ledger). Got {r:?}. Example: RESET_REALISED=\"rebaselined after X\""
            );
        } else {
            let mut g = control.lock().unwrap();
            let newest_reset = g.ledger.newest_realised_reset_t();
            let now = copybot_hot::ledger::now_secs();
            if newest_reset.is_some_and(|t| now - t < 86_400) {
                eprintln!(
                    "[ledger] RESET_REALISED refused: a reset already happened in \
the last 24h. If this is deliberate, remove the env var, wait, or clear it manually — \
a reset that fires on every restart is how a P&L baseline silently rots."
                );
            } else {
                let names: Vec<String> = g.ledger.lanes.keys().cloned().collect();
                for n in &names {
                    let before = g.ledger.lanes[n].risk.realised_pnl;
                    g.ledger.realised_reset(n, r);
                    eprintln!(
                        "[ledger] {n}: realised RESET {before:+.2} -> 0.00 \
(positions untouched) — {r}"
                    );
                }
            }
        }
    }
    {
        let sidecar = format!("{}.continuity.json", root.bot.ledger_path);
        let verdict = control.lock().unwrap().ledger.verify_continuity(&sidecar);
        match &verdict {
            copybot_hot::ledger::ContinuityVerdict::Intact => {}
            copybot_hot::ledger::ContinuityVerdict::FirstRun => {
                eprintln!("[ledger] continuity: first run — recording the baseline");
            }
            copybot_hot::ledger::ContinuityVerdict::Broken(why) => {
                eprintln!("[ledger] ⛔ CONTINUITY BROKEN: {why}");
                for lane in router.snapshot().iter() {
                    latch_buys(
                        lane,
                        &incidents,
                        "continuity",
                        "ledger continuity broken",
                    );
                }
                emitter
                    .emit(
                        serde_json::json!(
                            { "t" : now_ms(), "ev" : "ledger_continuity_broken", "why" :
                            why, "buys_halted" : true }
                        ),
                    );
                copybot_hot::errors::record(
                    &format!(
                        "{}/errors.jsonl", std::path::Path::new(& root.bot.control_path)
                        .parent().map(| p | p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "run".into())
                    ),
                    &copybot_hot::errors::ErrorRow {
                        t: copybot_hot::ledger::now_secs(),
                        lane: String::new(),
                        kind: "ledger".into(),
                        detail: format!("continuity check failed: {why}"),
                        human: format!(
                            "The accounting file no longer contains the history it had at \
the last start ({why}). Profit, loss and position sizes are all derived from it, so \
buying is stopped until you confirm the change was intended. Selling still works. If you \
ran ledger_adjust or edited the file on purpose, this is expected — restart to accept \
the new baseline."
                        ),
                        severity: "stop".into(),
                    },
                );
            }
        }
        if let Err(e) = control.lock().unwrap().ledger.record_continuity(&sidecar) {
            eprintln!("[ledger] ⚠️  cannot record continuity baseline: {e}");
        }
        if let Some(c) = control.lock().unwrap().ledger.continuity() {
            if c.lines > copybot_hot::ledger::LEDGER_GROWTH_ALARM_LINES {
                eprintln!(
                    "[ledger] ⚠️  {} lines ({} bytes) — past the {} line mark where \
snapshot/checkpointing is worth its risk",
                    c.lines, c.bytes, copybot_hot::ledger::LEDGER_GROWTH_ALARM_LINES
                );
            }
        }
    }
    {
        let lanes = router.snapshot();
        let held_by_lane: Vec<(usize, String, Vec<String>)> = lanes
            .iter()
            .enumerate()
            .map(|(ix, lane)| {
                let held = control.lock().unwrap().ledger.holdings(&lane.cfg.name);
                (ix, lane.cfg.name.clone(), held.keys().cloned().collect())
            })
            .collect();
        let (restored, conflicts) = router.restore_ownership(&held_by_lane);
        if restored > 0 {
            eprintln!("[owner] restored {restored} token claim(s) from the ledger");
        }
        for (tok, owner, loser) in &conflicts {
            for name in [owner, loser] {
                if let Some(l) = lanes.iter().find(|l| &l.cfg.name == name) {
                    latch_buys(
                        &l,
                        &incidents,
                        "ownership_conflict",
                        "two lanes claim one token",
                    );
                }
            }
            let short = &tok[tok.len().saturating_sub(8)..];
            eprintln!(
                "[owner] ⛔ CONFLICT on …{short}: {owner:?} and {loser:?} both hold it in their ledgers — buys halted on both, attribution required"
            );
            emitter
                .emit(
                    serde_json::json!(
                        { "t" : now_ms(), "ev" : "ownership_conflict", "tok" : &
                        tok[..tok.len().min(14)], "owner" : owner, "conflicting" : loser,
                        "buys_halted" : true, }
                    ),
                );
            let errpath = format!(
                "{}/errors.jsonl", std::path::Path::new(& root.bot.control_path).parent()
                .map(| p | p.to_string_lossy().into_owned()).unwrap_or_else(|| "run"
                .into())
            );
            copybot_hot::errors::record(
                &errpath,
                &copybot_hot::errors::ErrorRow {
                    t: copybot_hot::ledger::now_secs(),
                    lane: loser.clone(),
                    kind: "ownership".into(),
                    detail: format!(
                        "{owner} and {loser} both hold …{short} in their ledgers"
                    ),
                    human: format!(
                        "Two wallets ({owner} and {loser}) both believe they own the same \
market position. The shares exist only once, so one set of books is wrong and the profit \
split between them cannot be trusted. Buying is stopped on both until you say which one \
owns it. BOTH can still sell: each will follow its own leader out, sized from its own \
books — so if both exit, the second may be rejected for balance and will retry through \
the rescue ladder. Attribute it, or flatten by hand, rather than leaving it."
                    ),
                    severity: "stop".into(),
                },
            );
        }
    }
    let pending_path = format!("{}.pending", root.bot.control_path);
    let drift_by_lane: Arc<std::sync::Mutex<std::collections::HashMap<String, f64>>> = Arc::new(
        std::sync::Mutex::new(Default::default()),
    );
    let unattributed: Arc<
        std::sync::Mutex<std::collections::HashMap<String, (String, f64, f64)>>,
    > = Arc::new(std::sync::Mutex::new(Default::default()));
    let errors_path = format!(
        "{}/errors.jsonl", std::path::Path::new(& root.bot.control_path).parent().map(| p
        | p.to_string_lossy().into_owned()).unwrap_or_else(|| "run".into())
    );
    let pending_log = {
        let (log, wal_rows) = copybot_hot::pending::PendingLog::open_with_wal(
            &pending_path,
            Some(&wal_path),
        );
        match log.absorb_wal(wal_rows) {
            Ok(n) => wal_recovery.absorbed("pending", n),
            Err(e) => wal_recovery.failed("pending", e),
        }
        Arc::new(std::sync::Mutex::new(log))
    };
    let pending_unreadable = pending_log.lock().unwrap().unreadable_sources;
    if pending_unreadable > 0 {
        eprintln!(
            "[pending] ⛔ {pending_unreadable} journal source(s) UNREADABLE — the \
                   set of outstanding orders is UNKNOWN, not empty. Halting buys on every \
                   lane; exits keep mirroring."
        );
        for lane in router.snapshot().iter() {
            latch_buys(
                lane,
                &incidents,
                "pending_unreadable",
                "the pending journal could not be read at boot, so outstanding orders are unknown — buying more would compound an exposure we cannot enumerate",
            );
        }
    }
    let pending_tokens = pending_log.lock().unwrap().open_tokens();
    if !pending_tokens.is_empty() {
        eprintln!(
            "[pending] {} unresolved fill(s) carried over — reconciliation will \
                   leave them to the resolver",
            pending_tokens.len()
        );
    }
    match wal_recovery.finish() {
        copybot_hot::wal::Recycled::Emptied { bytes } => {
            eprintln!("[wal] replayed and recycled {bytes} byte(s)")
        }
        copybot_hot::wal::Recycled::AlreadyEmpty => {}
        copybot_hot::wal::Recycled::Retained { why } => {
            eprintln!(
                "[wal] RETAINED — {why}. It will be replayed again next boot; \
                       investigate before this repeats."
            )
        }
    }
    const UNATTRIBUTED_MIN_USD: f64 = 1.0;
    {
        let rec_http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        let fetch = |w: String| {
            let c = rec_http.clone();
            async move {
                let snap = copybot_hot::positions::fetch(
                        &c,
                        "https://data-api.polymarket.com",
                        &w,
                        "0.01",
                        "",
                        copybot_hot::ledger::now_secs(),
                    )
                    .await;
                if !snap.may_act_destructively() {
                    return Err(
                        format!(
                            "positions for {w} are not a complete list ({}) — \
refusing to reconcile against a partial portfolio",
                            snap.completeness.reason()
                        ),
                    );
                }
                Ok(snap.rows)
            }
        };
        let ours_result = fetch(funder.clone()).await;
        let ours_error = ours_result.as_ref().err().cloned();
        let ours = ours_result.unwrap_or_default();
        if let Some(e) = &ours_error {
            eprintln!("[recon] ⛔ OUR chain positions unverified: {e}");
        }
        let safe_confirmed_empty = if ours_error.is_none() && ours.is_empty() {
            let second = fetch(funder.clone()).await;
            let snaps = [
                copybot_hot::snapshot::Snapshot::Data(0),
                match &second {
                    Ok(v) => copybot_hot::snapshot::Snapshot::Data(v.len()),
                    Err(e) => copybot_hot::snapshot::Snapshot::Failed(e.clone()),
                },
            ];
            match copybot_hot::snapshot::judge_empty(&snaps, 2) {
                copybot_hot::snapshot::Verdict::Believable => {
                    eprintln!(
                        "[recon] wallet is FLAT (confirmed by two independent \
reads) — phantom ledger holdings will be released"
                    );
                    true
                }
                v => {
                    eprintln!(
                        "[recon] wallet looked flat but that is not confirmed \
({v:?}) — treating as a bad read"
                    );
                    false
                }
            }
        } else {
            false
        };
        for (lane_ix, lane) in router.snapshot().iter().enumerate() {
            let name = lane.cfg.name.clone();
            let w = format!("0x{}", hex::encode(lane.cfg.wallet20));
            let his = match fetch(w).await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[seed] ⛔ {name}: {e}");
                    control
                        .lock()
                        .unwrap()
                        .set_boot_fault(
                            &name,
                            format!("leader position snapshot unavailable: {e}"),
                        );
                    continue;
                }
            };
            if let Some(e) = &ours_error {
                control
                    .lock()
                    .unwrap()
                    .set_boot_fault(
                        &name,
                        format!("our position snapshot unavailable: {e}"),
                    );
            }
            let mut book: std::collections::HashMap<String, f64> = Default::default();
            for p in &his {
                let t = p["asset"].as_str().unwrap_or("").to_string();
                let sz = p["size"]
                    .as_f64()
                    .or_else(|| p["size"].as_str().and_then(|x| x.parse().ok()))
                    .unwrap_or(0.0);
                if !t.is_empty() && sz > 0.0 {
                    book.insert(t, sz);
                }
            }
            let held = control.lock().unwrap().ledger.holdings(&name);
            let chain_sz: std::collections::HashMap<String, f64> = ours
                .iter()
                .filter_map(|p| {
                    let t = p["asset"].as_str()?.to_string();
                    let sz = p["size"]
                        .as_f64()
                        .or_else(|| p["size"].as_str().and_then(|x| x.parse().ok()))?;
                    Some((t, sz))
                })
                .collect();
            let redeemable: std::collections::HashSet<String> = ours
                .iter()
                .filter(|p| p["redeemable"].as_bool().unwrap_or(false))
                .filter_map(|p| p["asset"].as_str().map(str::to_string))
                .collect();
            let ours_known = held.values().filter(|v| **v > 0.01).count();
            let still_there = held
                .iter()
                .filter(|(t, v)| **v > 0.01 && chain_sz.contains_key(*t))
                .count();
            let trustworthy = ours_error.is_none()
                && (safe_confirmed_empty || ours_known == 0
                    || still_there * 2 >= ours_known);
            if !trustworthy {
                eprintln!(
                    "[recon] {name}: REFUSING snapshot — only {still_there}/{ours_known} \
of our positions present. Treating as a bad read, not a liquidation."
                );
                control
                    .lock()
                    .unwrap()
                    .set_boot_fault(
                        &name,
                        format!(
                            "untrusted wallet snapshot: {still_there}/{ours_known} ledger positions present"
                        ),
                    );
            }
            let mut adopted = 0usize;
            let mut released = 0usize;
            for (tok, sz) in &chain_sz {
                if !book.contains_key(tok) {
                    continue;
                }
                if redeemable.contains(tok) {
                    continue;
                }
                if pending_tokens.iter().any(|(_, t)| t == tok) {
                    continue;
                }
                let claimed_by_pool = control.lock().unwrap().ledger.pool_claim(tok);
                let unexplained = sz - claimed_by_pool;
                let diff = (sz - held.get(tok).copied().unwrap_or(0.0)).min(unexplained);
                if diff <= 0.01 {
                    continue;
                }
                if let Some(other) = router.owner_of(tok) {
                    if other != lane_ix {
                        eprintln!(
                            "[recon] {name}: NOT adopting …{} — lane {other} owns it",
                            & tok[tok.len().saturating_sub(8)..]
                        );
                        emitter
                            .emit(
                                serde_json::json!(
                                    { "t" : now_ms(), "ev" : "recon_contested", "lane" : name,
                                    "tok" : & tok[..tok.len().min(14)], "owner_lane" : other,
                                    "shares" : diff }
                                ),
                            );
                        continue;
                    }
                }
                let px = ours
                    .iter()
                    .find(|p| p["asset"].as_str() == Some(tok.as_str()))
                    .and_then(|p| {
                        p["avgPrice"]
                            .as_f64()
                            .or_else(|| {
                                p["avgPrice"].as_str().and_then(|x| x.parse().ok())
                            })
                    })
                    .unwrap_or(0.0);
                if px <= 0.0 {
                    continue;
                }
                let had_history = control
                    .lock()
                    .unwrap()
                    .ledger
                    .lanes
                    .get(&name)
                    .map(|b| {
                        b.positions.get(tok).map(|p| p.shares > 1e-9).unwrap_or(false)
                    })
                    .unwrap_or(false);
                let proven = had_history
                    || pending_log
                        .lock()
                        .unwrap()
                        .open_tokens()
                        .iter()
                        .any(|(l, t)| l == &name && t == tok);
                let value_usd = diff * px;
                if !proven && value_usd >= UNATTRIBUTED_MIN_USD {
                    let short = &tok[tok.len().saturating_sub(8)..];
                    eprintln!(
                        "[recon] ⛔ {name}: …{short} ({diff:.4} sh @ {px:.4}, \
${value_usd:.2}) has NO ledger history and NO pending order — left UNATTRIBUTED"
                    );
                    emitter
                        .emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "recon_unattributed", "lane" :
                                name, "tok" : & tok[..tok.len().min(14)], "shares" : diff,
                                "price" : px, "usd" : value_usd }
                            ),
                        );
                    unattributed
                        .lock()
                        .unwrap()
                        .insert(tok.clone(), (name.clone(), diff, value_usd));
                    copybot_hot::errors::record(
                        &errors_path,
                        &copybot_hot::errors::ErrorRow {
                            t: copybot_hot::ledger::now_secs(),
                            lane: name.clone(),
                            kind: "unattributed".into(),
                            detail: format!(
                                "…{short}: {diff:.4} sh @ {px:.4} (${value_usd:.2}), \
no ledger history and no pending order"
                            ),
                            human: format!(
                                "The wallet holds ${value_usd:.2} of a market that {name}'s \
leader also trades, but nothing in our records shows we bought it. It has NOT been added \
to {name}'s books, so we will not sell it by mistake — it may be a manual trade or another \
strategy's. Tell us whose it is, or flatten it by hand."
                            ),
                            severity: "warn".into(),
                        },
                    );
                    continue;
                }
                control
                    .lock()
                    .unwrap()
                    .ledger
                    .record_recon_fill(&name, tok, 0, diff, px, 0.0);
                router.claim_existing(tok, lane_ix);
                adopted += 1;
                if !proven {
                    eprintln!(
                        "[recon] ⚠️  {name}: adopted …{} ({diff:.4} sh @ {px:.4}, \
${value_usd:.2}) with NO provenance — below the ${UNATTRIBUTED_MIN_USD:.2} materiality line",
                        & tok[tok.len().saturating_sub(8)..]
                    );
                    emitter
                        .emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "recon_adopt_no_history", "lane" :
                                name, "tok" : & tok[..tok.len().min(14)], "shares" : diff,
                                "price" : px, "usd" : value_usd }
                            ),
                        );
                }
            }
            if trustworthy {
                for (tok, have) in &held {
                    if *have <= 1e-9 {
                        continue;
                    }
                    let on_chain = chain_sz.get(tok).copied().unwrap_or(0.0);
                    let gone = have - on_chain;
                    if gone <= 1e-9 {
                        continue;
                    }
                    if gone <= 0.01 && on_chain > 0.0 {
                        continue;
                    }
                    let px = control.lock().unwrap().ledger.avg_cost(&name, tok);
                    control
                        .lock()
                        .unwrap()
                        .ledger
                        .record_recon_fill(&name, tok, 1, gone, px, 0.0);
                    released += 1;
                }
            }
            if adopted > 0 || released > 0 {
                eprintln!(
                    "[recon] {name}: adopted {adopted}, RELEASED {released} phantom \
position(s) the chain no longer shows"
                );
                if safe_confirmed_empty && released > 0 {
                    copybot_hot::errors::record(
                        &errors_path,
                        &copybot_hot::errors::ErrorRow {
                            t: copybot_hot::ledger::now_secs(),
                            lane: name.clone(),
                            kind: "recon".into(),
                            detail: format!(
                                "wallet confirmed empty; released all {released} \
ledger position(s) for {name}"
                            ),
                            human: format!(
                                "The wallet is reported as holding nothing, confirmed twice, so \
all {released} of {name}'s recorded positions were cleared. Profit and loss are unchanged \
and nothing was sold. This is normal once every market has settled — but if {name} should \
still be holding something, check that the funder address is right."
                            ),
                            severity: "warn".into(),
                        },
                    );
                }
            } else if !ours.is_empty() {
                eprintln!("[recon] {name}: ledger matches the chain");
            }
            if book.is_empty() && !held.is_empty() {
                let reason = "leader position snapshot is empty while our ledger has holdings";
                eprintln!("[seed] ⛔ {name}: {reason}; refusing to enable execution");
                control.lock().unwrap().set_boot_fault(&name, reason);
                continue;
            }
            let legacy = control.lock().unwrap().seed_his_book(&name, &book);
            let now_held = control.lock().unwrap().ledger.holdings(&name).len();
            lane.mark_ready();
            if incidents.lock().map(|g| g.latched(&name)).unwrap_or(true) {
                lane.state.halt_latch.store(true, Ordering::Relaxed);
                eprintln!(
                    "[{name}] buys remain HALTED: an incident from before the \
                           restart is still open — clear it deliberately to resume"
                );
            }
            eprintln!(
                "[seed] {name}: his book {} positions, we hold {}, {} marked legacy",
                book.len(), now_held, legacy
            );
            emitter
                .emit(
                    serde_json::json!(
                        { "t" : now_ms(), "ev" : "recon", "lane" : name, "adopted" :
                        adopted, "his_positions" : book.len(), "we_hold" : now_held,
                        "legacy" : legacy }
                    ),
                );
        }
    }
    if live && !shadow {
        let faults = control.lock().unwrap().boot_faults.clone();
        if !faults.is_empty() {
            for (lane, reason) in faults {
                eprintln!("[boot] REFUSING LIVE EXECUTION for {lane}: {reason}");
            }
            std::process::exit(2);
        }
    }
    if shadow {
        let (c2, r2, e2, f2) = (
            control.clone(),
            router.clone(),
            emitter.clone(),
            funder.clone(),
        );
        tokio::spawn(async move {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default();
            loop {
                tokio::time::sleep(Duration::from_secs(300)).await;
                let snap = copybot_hot::positions::fetch(
                        &http,
                        "https://data-api.polymarket.com",
                        &f2,
                        "0.01",
                        "",
                        copybot_hot::ledger::now_secs(),
                    )
                    .await;
                if !snap.may_act_destructively() {
                    continue;
                }
                let chain = &snap.rows;
                for lane in r2.snapshot().iter() {
                    let name = lane.cfg.name.clone();
                    let held = c2.lock().unwrap().ledger.holdings(&name);
                    let his = c2
                        .lock()
                        .unwrap()
                        .his_pos
                        .get(&name)
                        .cloned()
                        .unwrap_or_default();
                    let mut adopted = 0usize;
                    for p in chain {
                        let tok = p["asset"].as_str().unwrap_or("").to_string();
                        if tok.is_empty() || !his.contains_key(&tok) {
                            continue;
                        }
                        let sz = p["size"]
                            .as_f64()
                            .or_else(|| p["size"].as_str().and_then(|x| x.parse().ok()))
                            .unwrap_or(0.0);
                        let diff = sz - held.get(&tok).copied().unwrap_or(0.0);
                        if diff <= 0.01 {
                            continue;
                        }
                        let px = p["avgPrice"]
                            .as_f64()
                            .or_else(|| {
                                p["avgPrice"].as_str().and_then(|x| x.parse().ok())
                            })
                            .unwrap_or(0.0);
                        if px <= 0.0 {
                            continue;
                        }
                        c2.lock()
                            .unwrap()
                            .ledger
                            .record_fill(&name, &tok, 0, diff, px, 0.0);
                        adopted += 1;
                    }
                    if adopted > 0 {
                        e2.emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "recon", "lane" : name, "adopted" :
                                adopted, "periodic" : true }
                            ),
                        );
                    }
                }
            }
        });
    }
    {
        let supervised = tokio::spawn(
            control_task(
                control.clone(),
                router.clone(),
                pending_log.clone(),
                emitter.clone(),
                watch_tokens.clone(),
                fps.clone(),
                resting_book.clone(),
                levels.clone(),
                prints.clone(),
            ),
        );
        let em_sup = emitter.clone();
        tokio::spawn(async move {
            let outcome = supervised.await;
            let why = match &outcome {
                Err(e) if e.is_panic() => "control task PANICKED".to_string(),
                Err(e) => format!("control task ended abnormally: {e}"),
                Ok(()) => "control task returned, which it must never do".to_string(),
            };
            eprintln!(
                "[CRITICAL] {why} — the only writer of `armed` is gone, so the \
kill switch is inert. Aborting so systemd restarts a coherent process."
            );
            em_sup
                .emit(
                    serde_json::json!(
                        { "t" : now_ms(), "ev" : "control_task_died", "why" : why,
                        "action" : "abort" }
                    ),
                );
            tokio::time::sleep(Duration::from_millis(250)).await;
            std::process::abort();
        });
    }
    let creds_slot: Arc<std::sync::Mutex<Option<copybot_hot::auth::ApiCreds>>> = Arc::new(
        std::sync::Mutex::new(None),
    );
    let physical_wallet: Arc<std::sync::Mutex<PhysicalWallet>> = Arc::new(
        std::sync::Mutex::new(PhysicalWallet::default()),
    );
    {
        let port: u16 = std::env::var("DASHBOARD_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8806);
        let (c, r, fp2, sc2, bk2) = (
            control.clone(),
            router.clone(),
            fps.clone(),
            fpscore.clone(),
            books.clone(),
        );
        let pend_dash = pending_log.clone();
        let custody_dash = custody_report.clone();
        let leader_pnl_dash = leader_pnl.clone();
        let reanchor_dash = reanchor_pulse.clone();
        let (rb_dash, levels_dash, prints_dash) = (
            resting_book.clone(),
            levels.clone(),
            prints.clone(),
        );
        let fund_pool = funding.clone();
        let inc_dash = incidents.clone();
        let (registry_d, phys_d, ctrl_path_d) = (
            registry.clone(),
            physical_wallet.clone(),
            root.bot.control_path.clone(),
        );
        let pool2_path_d = std::env::var("POOL_PAGE")
            .or_else(|_| std::env::var("POOL2_PATH"))
            .unwrap_or_else(|_| {
                let dir = std::path::Path::new(&root.bot.control_path)
                    .parent()
                    .map(|d| d.to_string_lossy().into_owned())
                    .unwrap_or_else(|| ".".into());
                format!("{dir}/pool.html")
            });
        eprintln!(
            "[dash] /pool served from {pool2_path_d} (edit + refresh, no restart)"
        );
        let errors_path_d = errors_path.clone();
        let drift_dash = drift_by_lane.clone();
        let unattributed_dash = unattributed.clone();
        let response_times_dash = response_times.clone();
        let dash_lanes: Vec<DashboardLane> = router
            .snapshot()
            .iter()
            .map(|lane| {
                let seed = root
                    .lane
                    .iter()
                    .find(|x| x.name == lane.cfg.name)
                    .and_then(|x| x.budget.bankroll_usd);
                (
                    lane.cfg.name.clone(),
                    format!("0x{}", hex::encode(lane.cfg.wallet20)),
                    seed,
                    lane.policy().caps.daily_usd,
                    Arc::new(copybot_hot::matchup::Matchup::new()),
                )
            })
            .collect();
        let http_mu = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        let funder_mu = funder.clone();
        let pos_cache: Arc<
            tokio::sync::Mutex<Option<copybot_hot::positions::Positions>>,
        > = Arc::new(tokio::sync::Mutex::new(None));
        let title_cache: Arc<
            std::sync::RwLock<std::collections::HashMap<String, String>>,
        > = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        {
            let (http_t, funder_t, cache_t) = (
                http_mu.clone(),
                funder_mu.clone(),
                title_cache.clone(),
            );
            tokio::spawn(async move {
                const PAGE: usize = 500;
                const MAX_PAGES: usize = 20;
                let mut tick = tokio::time::interval(Duration::from_secs(300));
                loop {
                    tick.tick().await;
                    let mut scanned = 0usize;
                    for page in 0..MAX_PAGES {
                        let url = format!(
                            "https://data-api.polymarket.com/trades?user={funder_t}\
                             &limit={PAGE}&offset={}",
                            page * PAGE
                        );
                        let rows: Vec<serde_json::Value> = match http_t
                            .get(&url)
                            .send()
                            .await
                        {
                            Ok(r) if r.status().is_success() => {
                                r.json().await.unwrap_or_default()
                            }
                            _ => break,
                        };
                        let n = rows.len();
                        if n > 0 {
                            let mut w = cache_t.write().unwrap();
                            for row in &rows {
                                if let (Some(tok), Some(title)) = (
                                    row["asset"].as_str(),
                                    row["title"].as_str(),
                                ) {
                                    w.entry(tok.to_string())
                                        .or_insert_with(|| title.to_string());
                                }
                            }
                        }
                        scanned += n;
                        if n < PAGE {
                            break;
                        }
                    }
                    eprintln!(
                        "[titles] sweep: {scanned} trades scanned, {} known", cache_t
                        .read().unwrap().len()
                    );
                }
            });
        }
        let (creds_dash, clob_dash, addr_dash, sig_dash) = (
            creds_slot.clone(),
            root.bot.clob_host.clone(),
            signer_addr.clone(),
            sig_type,
        );
        let signal_guard_dash = signal_guard.clone();
        tokio::spawn(async move {
            let lst = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[dash] bind: {e}");
                    return;
                }
            };
            eprintln!("[dash] http://127.0.0.1:{port}  (loopback; no app token)");
            const MAX_DASH_CONNS: usize = 32;
            const REQUEST_DEADLINE: Duration = Duration::from_secs(15);
            let conn_limit = std::sync::Arc::new(
                tokio::sync::Semaphore::new(MAX_DASH_CONNS),
            );
            loop {
                let Ok((mut sock, _)) = lst.accept().await else { continue };
                let Ok(permit) = conn_limit.clone().try_acquire_owned() else {
                    use tokio::io::AsyncWriteExt;
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
                        )
                        .await;
                    continue;
                };
                let (c, r, fp2, sc2, bk2) = (
                    c.clone(),
                    r.clone(),
                    fp2.clone(),
                    sc2.clone(),
                    bk2.clone(),
                );
                let (http2, funder2, lane_specs) = (
                    http_mu.clone(),
                    funder_mu.clone(),
                    dash_lanes.clone(),
                );
                let pos_cache2 = pos_cache.clone();
                let title_cache2 = title_cache.clone();
                let (creds_d, clob_d, addr_d, sigtype_d) = (
                    creds_dash.clone(),
                    clob_dash.clone(),
                    addr_dash.clone(),
                    sig_dash,
                );
                let signal_guard_d = signal_guard_dash.clone();
                let pend_d = pend_dash.clone();
                let custody_d = custody_dash.clone();
                let leader_pnl_d = leader_pnl_dash.clone();
                let reanchor_d = reanchor_dash.clone();
                let (rb_d, levels_d, prints_d) = (
                    rb_dash.clone(),
                    levels_dash.clone(),
                    prints_dash.clone(),
                );
                let inc_srv = inc_dash.clone();
                let (registry_c, phys_c, ctrl_path_c) = (
                    registry_d.clone(),
                    phys_d.clone(),
                    ctrl_path_d.clone(),
                );
                let ctl_http = c.clone();
                let pool2_path_c = pool2_path_d.clone();
                let errors_path_c = errors_path_d.clone();
                let drift_d = drift_dash.clone();
                let unattributed_d = unattributed_dash.clone();
                let response_times_c = response_times_dash.clone();
                let fund_pool2 = fund_pool.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let _permit = permit;
                    let served = tokio::time::timeout(
                            REQUEST_DEADLINE,
                            async move {
                                let mut raw: Vec<u8> = Vec::with_capacity(4096);
                                let mut tmp = [0u8; 4096];
                                loop {
                                    match sock.read(&mut tmp).await {
                                        Ok(0) => break,
                                        Ok(n) => raw.extend_from_slice(&tmp[..n]),
                                        Err(_) => break,
                                    }
                                    let s = String::from_utf8_lossy(&raw);
                                    if let Some(idx) = s.find("\r\n\r\n") {
                                        let want = s[..idx]
                                            .lines()
                                            .find_map(|l| {
                                                l.to_ascii_lowercase()
                                                    .strip_prefix("content-length:")
                                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                            })
                                            .unwrap_or(0);
                                        if raw.len() - (idx + 4) >= want {
                                            break;
                                        }
                                    }
                                    if raw.len() > 65_536 {
                                        break;
                                    }
                                }
                                let req = String::from_utf8_lossy(&raw).into_owned();
                                let body: &str = req
                                    .find("\r\n\r\n")
                                    .map(|i| &req[i + 4..])
                                    .unwrap_or("");
                                let mut parts = req.split_whitespace();
                                let method = parts.next().unwrap_or("");
                                let target = parts.next().unwrap_or("");
                                let path = target.split('?').next().unwrap_or("");
                                let query = target
                                    .split_once('?')
                                    .map(|(_, q)| q)
                                    .unwrap_or("");
                                let _ = query;
                                if method == "GET"
                                    && (path == "/" || path == "/index"
                                        || path == "/index.html")
                                {
                                    let resp = "HTTP/1.1 302 Found\r\nLocation: /pool\r\n\
                            Content-Length: 0\r\n\r\n";
                                    let _ = sock.write_all(resp.as_bytes()).await;
                                    return;
                                }
                                if method == "GET" && (path == "/pool" || path == "/pool2")
                                {
                                    let (code, body) = match std::fs::read_to_string(
                                        &pool2_path_c,
                                    ) {
                                        Ok(html) => ("200 OK", html),
                                        Err(e) => {
                                            (
                                                "404 Not Found",
                                                format!(
                                                    "<pre style=\"font:14px ui-monospace;padding:24px;\
                                 background:#0b0b0e;color:#f2f2f5\">\
                                 /pool2 is served from a file that is not there yet.\n\n\
                                 expected: {pool2_path_c}\n error:    {e}\n\n\
                                 Drop the page there and refresh — no restart needed.</pre>"
                                                ),
                                            )
                                        }
                                    };
                                    let resp = format!(
                                        "HTTP/1.1 {code}\r\nContent-Type: text/html; \
                            charset=utf-8\r\nCache-Control: no-store\r\n\
                            Content-Length: {}\r\n\r\n{}",
                                        body.len(), body
                                    );
                                    let _ = sock.write_all(resp.as_bytes()).await;
                                    return;
                                }
                                if method == "GET" && path == "/api/trades" {
                                    let path = req.split_whitespace().nth(1).unwrap_or("");
                                    let q = path.split_once('?').map(|(_, q)| q).unwrap_or("");
                                    let want_lane = q
                                        .split('&')
                                        .find_map(|p| p.strip_prefix("lane="))
                                        .filter(|v| !v.is_empty() && *v != "all");
                                    let limit: usize = q
                                        .split('&')
                                        .find_map(|p| p.strip_prefix("limit="))
                                        .and_then(|v| v.parse().ok())
                                        .unwrap_or(50)
                                        .min(500);
                                    let ledger_path = c.lock().unwrap().ledger.path.clone();
                                    let mut rows: Vec<serde_json::Value> = Vec::new();
                                    match std::fs::metadata(&ledger_path) {
                                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                                            let out = serde_json::json!(
                                                { "status" : "stale", "stale_reason" :
                                                format!("ledger unreadable: {e}"), "trades" :
                                                serde_json::Value::Null }
                                            )
                                                .to_string();
                                            let resp = format!(
                                                "HTTP/1.1 503 Service Unavailable\r\n\
                                    Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                                                out.len(), out
                                            );
                                            let _ = sock.write_all(resp.as_bytes()).await;
                                            return;
                                        }
                                        _ => {}
                                    }
                                    if let Ok(raw) = std::fs::read_to_string(&ledger_path) {
                                        for line in raw.lines().rev() {
                                            if rows.len() >= limit {
                                                break;
                                            }
                                            let Ok(v) = serde_json::from_str::<
                                                serde_json::Value,
                                            >(line.trim()) else { continue };
                                            if v["ev"].as_str() != Some("fill") {
                                                continue;
                                            }
                                            if v["recon"].as_bool() == Some(true) {
                                                continue;
                                            }
                                            let lane = v["lane"].as_str().unwrap_or("");
                                            if let Some(w) = want_lane {
                                                if lane != w {
                                                    continue;
                                                }
                                            }
                                            let drag_c = match (
                                                v["price"].as_f64(),
                                                v["his_price"].as_f64(),
                                            ) {
                                                (Some(px), Some(his)) if his > 0.0 => {
                                                    serde_json::json!(((px - his) * 10000.0).round() / 100.0)
                                                }
                                                _ => serde_json::Value::Null,
                                            };
                                            let title = v["token"]
                                                .as_str()
                                                .and_then(|t| title_cache2.read().unwrap().get(t).cloned());
                                            rows.push(
                                                serde_json::json!(
                                                    { "lane" : lane, "token" : v["token"], "title" : title,
                                                    "side" : v["side"], "shares" : v["shares"], "price" :
                                                    v["price"], "his_price" : v["his_price"], "drag_c" : drag_c,
                                                    "fee" : v["fee"], "t" : v["t"], "exec" : v["exec"].as_str()
                                                    .unwrap_or("taker"), }
                                                ),
                                            );
                                        }
                                    }
                                    let out = serde_json::to_string(
                                            &serde_json::json!({ "trades" : rows }),
                                        )
                                        .unwrap_or_default();
                                    let resp = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: \
                            application/json\r\nContent-Length: {}\r\n\r\n{}",
                                        out.len(), out
                                    );
                                    let _ = sock.write_all(resp.as_bytes()).await;
                                    return;
                                }
                                if method == "GET" && path == "/api/pool" {
                                    let specs = registry_c.specs();
                                    let phys = *phys_c.lock().unwrap();
                                    let snap = r.snapshot();
                                    let mut lanes_out = Vec::new();
                                    {
                                        let g = c.lock().unwrap();
                                        let st = g.status(&r);
                                        for lane in snap.iter() {
                                            let name = lane.cfg.name.clone();
                                            let armed = lane.state.armed.load(Ordering::Relaxed);
                                            let retired = lane.state.retired.load(Ordering::Relaxed);
                                            let spec = specs.iter().find(|w| w.name == name);
                                            let seed = spec.map(|s| s.seed_usd);
                                            let pct = spec.map(|s| s.pct);
                                            let enabled = spec.map(|s| s.enabled).unwrap_or(true);
                                            let stop_mode = spec
                                                .map(|s| s.stop_mode.clone())
                                                .unwrap_or_else(|| "none".into());
                                            let realised = st["lanes"][&name]["risk"]["realised_pnl"]
                                                .as_f64()
                                                .unwrap_or(0.0);
                                            let cap_scale = st["lanes"][&name]["cap_scale"]
                                                .as_f64()
                                                .unwrap_or(1.0);
                                            let spent_today = st["lanes"][&name]["spent_today"]
                                                .as_f64()
                                                .unwrap_or(0.0);
                                            let open_usd = g.ledger.open_usd(&name);
                                            let vb = seed
                                                .map(|s| copybot_hot::budget::virtual_bankroll(
                                                    s,
                                                    realised,
                                                ));
                                            let band_lo = spec.map(|s| s.min_buy_price).unwrap_or(0.02);
                                            let band_hi = spec.map(|s| s.max_buy_price).unwrap_or(0.95);
                                            let lpol = lane.policy();
                                            let eff_cap = lpol.max_effective_pct;
                                            let compound = lpol.compound;
                                            let eff_pct = copybot_hot::budget::effective_pct(
                                                pct.unwrap_or(0.0),
                                                cap_scale,
                                                eff_cap,
                                                compound,
                                            );
                                            let reanchor_json = {
                                                let g = reanchor_d.lock().unwrap();
                                                match g.get(&name) {
                                                    Some(
                                                        (
                                                            last,
                                                            last_complete,
                                                            raised,
                                                            lowered,
                                                            agreed,
                                                            refused,
                                                            disarmed,
                                                        ),
                                                    ) => {
                                                        let n = copybot_hot::ledger::now_secs();
                                                        serde_json::json!(
                                                            { "last_secs_ago" : n - last, "last_complete_secs_ago" : if
                                                            * last_complete == 0 { serde_json::Value::Null } else {
                                                            serde_json::json!(n - last_complete) }, "blind" : *
                                                            last_complete == 0 || (n - last_complete) > 1_800, "raised"
                                                            : raised, "lowered" : lowered, "agreed" : agreed, "refused"
                                                            : refused, "disarmed_zeros" : disarmed, }
                                                        )
                                                    }
                                                    None => serde_json::Value::Null,
                                                }
                                            };
                                            lanes_out
                                                .push(
                                                    serde_json::json!(
                                                        { "name" : name, "leader" : format!("0x{}", hex::encode(lane
                                                        .cfg.wallet20)), "armed" : armed, "retired" : retired,
                                                        "enabled" : enabled, "stop_mode" : stop_mode, "seed_usd" :
                                                        seed, "pct" : pct, "lane_id" : specs.iter().find(| w | w
                                                        .name == name).map(| w | w.lane_id()).unwrap_or_default(),
                                                        "reanchor" : reanchor_json, "realised_pnl" : (realised *
                                                        100.0).round() / 100.0, "leader_pnl" : leader_pnl_d.lock()
                                                        .unwrap().get(& name).cloned()
                                                        .unwrap_or(serde_json::Value::Null), "virtual_bankroll" :
                                                        vb, "cap_scale" : cap_scale, "open_usd" : (open_usd * 100.0)
                                                        .round() / 100.0, "spent_today" : (spent_today * 100.0)
                                                        .round() / 100.0, "daily_budget" : lpol.caps.daily_usd,
                                                        "min_buy_price" : band_lo, "max_buy_price" : band_hi,
                                                        "max_effective_pct" : eff_cap, "compound" : compound,
                                                        "effective_pct" : eff_pct, "configured_ceiling" : spec.map(|
                                                        s | s.max_effective_pct), "sizing_fit" : spec.and_then(| s |
                                                        s.leader_stats().ok()).map(| ls | { let cap_fill = lpol.caps
                                                        .max_usd_per_fill * cap_scale; let need = eff_pct * ls
                                                        .max_order_usd; let frac = if need > 0.0 { (cap_fill / need)
                                                        .min(1.0) } else { 1.0 }; serde_json::json!({
                                                        "his_max_order_usd" : ls.max_order_usd, "need_usd" : (need *
                                                        100.0).round() / 100.0, "per_fill_cap_usd" : (cap_fill *
                                                        100.0).round() / 100.0, "delivered_frac_on_his_largest" :
                                                        (frac * 10_000.0).round() / 10_000.0, "clipped" : frac <
                                                        0.999, }) }), "copy_makers" : spec.map(| s | s.copy_makers)
                                                        .unwrap_or(false), "exclude_political" : spec.map(| s | s
                                                        .exclude_political).unwrap_or(false), "risk" : st["lanes"]
                                                        [& name] ["risk"].clone(), "halted" : st["lanes"] [& name]
                                                        ["halted"].clone(), "halt_latch" : lane.state.halt_latch
                                                        .load(Ordering::Relaxed), "ready" : lane.state.ready
                                                        .load(Ordering::Relaxed), "share_drift" : drift_d.lock()
                                                        .unwrap().get(& name).copied(), "boot_fault" : st["lanes"]
                                                        [& name] ["boot_fault"].clone(), }
                                                    ),
                                                );
                                        }
                                    }
                                    let sum_seed: f64 = specs
                                        .iter()
                                        .filter(|w| w.enabled)
                                        .map(|w| w.seed_usd)
                                        .sum();
                                    let funding_basis = fund_pool2.lock().unwrap().basis();
                                    let out = serde_json::to_string(
                                            &serde_json::json!(
                                                { "physical_cash" : phys.cash, "physical_equity" : phys
                                                .equity, "physical_as_of" : phys.fetched_at,
                                                "physical_age_secs" : phys.fetched_at.map(| t |
                                                copybot_hot::ledger::now_secs() - t),
                                                "physical_stale_reason" : phys.stale_reason, "sum_seed" :
                                                (sum_seed * 100.0).round() / 100.0, "headroom" : phys.equity
                                                .map(| p | ((p - sum_seed) * 100.0).round() / 100.0),
                                                "wallet" : { "portfolio" : phys.equity, "funding_basis" :
                                                funding_basis, "equity_pnl" :
                                                copybot_hot::funding::physical_pnl(phys.equity,
                                                funding_basis), "equity_pnl_source" :
                                                "wallet mark-to-market equity minus \
declared net external funding (deposits - withdrawals); independent of lane allocation",
                                                "sum_seed_allocation" : (sum_seed * 100.0).round() / 100.0,
                                                "as_of" : phys.fetched_at, }, "dedupe" : true,
                                                "unattributed" : unattributed_d.lock().unwrap().iter().map(|
                                                (tok, (lane, sh, usd)) | serde_json::json!({ "token" : tok,
                                                "lane" : lane, "shares" : sh, "usd" : usd, })).collect::<
                                                Vec < _ >> (), "lanes" : lanes_out, }
                                            ),
                                        )
                                        .unwrap_or_default();
                                    let resp = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: \
                            application/json\r\nContent-Length: {}\r\n\r\n{}",
                                        out.len(), out
                                    );
                                    let _ = sock.write_all(resp.as_bytes()).await;
                                    return;
                                }
                                if method == "GET" && path == "/api/equity" {
                                    let path = req.split_whitespace().nth(1).unwrap_or("");
                                    let sel = path
                                        .split_once('?')
                                        .and_then(|(_, q)| {
                                            q.split('&').find_map(|p| p.strip_prefix("lane="))
                                        })
                                        .unwrap_or("all")
                                        .to_string();
                                    let lane_opt = if sel == "all" {
                                        None
                                    } else {
                                        Some(sel.as_str())
                                    };
                                    let lpath = c.lock().unwrap().ledger.path.clone();
                                    if let Err(e) = std::fs::metadata(&lpath) {
                                        if e.kind() != std::io::ErrorKind::NotFound {
                                            let out = serde_json::json!(
                                                { "status" : "stale", "stale_reason" :
                                                format!("ledger unreadable: {e}"), "lane" : sel, "points" :
                                                serde_json::Value::Null }
                                            )
                                                .to_string();
                                            let resp = format!(
                                                "HTTP/1.1 503 Service Unavailable\r\n\
                                    Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                                                out.len(), out
                                            );
                                            let _ = sock.write_all(resp.as_bytes()).await;
                                            return;
                                        }
                                    }
                                    let series = copybot_hot::ledger::equity_series_at(
                                        &lpath,
                                        lane_opt,
                                    );
                                    let pts: Vec<serde_json::Value> = series
                                        .iter()
                                        .map(|(t, v)| {
                                            serde_json::json!({ "time" : t, "value" : v })
                                        })
                                        .collect();
                                    let out = serde_json::to_string(
                                            &serde_json::json!({ "lane" : sel, "points" : pts }),
                                        )
                                        .unwrap_or_default();
                                    let resp = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: \
                            application/json\r\nContent-Length: {}\r\n\r\n{}",
                                        out.len(), out
                                    );
                                    let _ = sock.write_all(resp.as_bytes()).await;
                                    return;
                                }
                                if method == "GET" && path == "/api/pnl" {
                                    let now = copybot_hot::ledger::now_secs();
                                    let snap = r.snapshot();
                                    let win = |w: copybot_hot::ledger::RealisedWindows| {
                                        serde_json::json!(
                                            { "total" : w.total, "d1" : w.d1, "d7" : w.d7, "d30" : w
                                            .d30, "ytd" : w.ytd, "y1" : w.y1 }
                                        )
                                    };
                                    let lpath = c.lock().unwrap().ledger.path.clone();
                                    let all = win(
                                        copybot_hot::ledger::Ledger::realised_windows_at(
                                            &lpath,
                                            None,
                                            now,
                                        ),
                                    );
                                    let lanes_out: Vec<serde_json::Value> = snap
                                        .iter()
                                        .map(|lane| {
                                            let mut o = win(
                                                copybot_hot::ledger::Ledger::realised_windows_at(
                                                    &lpath,
                                                    Some(&lane.cfg.name),
                                                    now,
                                                ),
                                            );
                                            o["name"] = serde_json::json!(lane.cfg.name);
                                            o
                                        })
                                        .collect();
                                    let out = serde_json::to_string(
                                            &serde_json::json!(
                                                { "now" : now, "all" : all, "lanes" : lanes_out }
                                            ),
                                        )
                                        .unwrap_or_default();
                                    let resp = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: \
                            application/json\r\nContent-Length: {}\r\n\r\n{}",
                                        out.len(), out
                                    );
                                    let _ = sock.write_all(resp.as_bytes()).await;
                                    return;
                                }
                                if method == "GET" && path == "/api/positions" {
                                    const POS_FRESH_SECS: i64 = 15;
                                    const POS_FALLBACK_SECS: i64 = 300;
                                    let now_s = copybot_hot::ledger::now_secs();
                                    let cached: Option<copybot_hot::positions::Positions> = pos_cache2
                                        .lock()
                                        .await
                                        .clone();
                                    let snapshot = if let Some(c) = cached
                                        .as_ref()
                                        .filter(|c| now_s - c.as_of <= POS_FRESH_SECS)
                                    {
                                        c.clone()
                                    } else {
                                        let fetched = tokio::time::timeout(
                                                Duration::from_secs(15),
                                                copybot_hot::positions::fetch(
                                                    &http2,
                                                    "https://data-api.polymarket.com",
                                                    &funder2,
                                                    "0.0001",
                                                    "",
                                                    now_s,
                                                ),
                                            )
                                            .await;
                                        let fetched = match fetched {
                                            Ok(s) => s,
                                            Err(_) => {
                                                copybot_hot::positions::Positions {
                                                    rows: Vec::new(),
                                                    completeness: copybot_hot::positions::Completeness::Failed {
                                                        after_pages: 0,
                                                        why: "positions request timed out".into(),
                                                    },
                                                    as_of: now_s,
                                                }
                                            }
                                        };
                                        match &fetched.completeness {
                                            copybot_hot::positions::Completeness::Complete => {
                                                *pos_cache2.lock().await = Some(fetched.clone());
                                                fetched
                                            }
                                            copybot_hot::positions::Completeness::Failed { .. } => {
                                                match cached
                                                    .filter(|c| now_s - c.as_of <= POS_FALLBACK_SECS)
                                                {
                                                    Some(c) => c,
                                                    None => fetched,
                                                }
                                            }
                                            _ => fetched,
                                        }
                                    };
                                    let stale_reason: Option<String> = match &snapshot
                                        .completeness
                                    {
                                        copybot_hot::positions::Completeness::Failed { .. } => {
                                            Some(snapshot.completeness.reason())
                                        }
                                        _ => None,
                                    };
                                    let ours: Vec<serde_json::Value> = snapshot.rows.clone();
                                    {
                                        let mut w = title_cache2.write().unwrap();
                                        for row in &ours {
                                            if let (Some(tok), Some(title)) = (
                                                row["asset"].as_str(),
                                                row["title"].as_str(),
                                            ) {
                                                w.entry(tok.to_string())
                                                    .or_insert_with(|| title.to_string());
                                            }
                                        }
                                    }
                                    if let Some(why) = &stale_reason {
                                        let out = serde_json::json!(
                                            { "status" : "stale", "stale_reason" : why, "as_of" :
                                            copybot_hot::ledger::now_secs(), "positions" :
                                            serde_json::Value::Null, }
                                        )
                                            .to_string();
                                        let resp = format!(
                                            "HTTP/1.1 503 Service Unavailable\r\n\
                                Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                                            out.len(), out
                                        );
                                        let _ = sock.write_all(resp.as_bytes()).await;
                                        return;
                                    }
                                    let pos_of = |tok: &str| -> Option<&serde_json::Value> {
                                        ours.iter().find(|p| p["asset"].as_str() == Some(tok))
                                    };
                                    let snap = r.snapshot();
                                    let mut out_rows = Vec::new();
                                    {
                                        let g = c.lock().unwrap();
                                        for lane in snap.iter() {
                                            let leader = format!(
                                                "0x{}", hex::encode(lane.cfg.wallet20)
                                            );
                                            for (tok, shares, avg, opened_t, his, his_known) in g
                                                .ledger
                                                .open_positions(&lane.cfg.name)
                                            {
                                                let p = pos_of(&tok);
                                                let mark = p.and_then(|p| p["curPrice"].as_f64());
                                                let title = p.and_then(|p| p["title"].as_str());
                                                let outcome = p.and_then(|p| p["outcome"].as_str());
                                                let end_date = p.and_then(|p| p["endDate"].as_str());
                                                let redeemable = p
                                                    .and_then(|p| p["redeemable"].as_bool())
                                                    .unwrap_or(false);
                                                let unreal = mark
                                                    .map(|m| ((m - avg) * shares * 100.0).round() / 100.0);
                                                out_rows
                                                    .push(
                                                        serde_json::json!(
                                                            { "lane" : lane.cfg.name, "leader" : leader, "token" : tok,
                                                            "title" : title, "outcome" : outcome, "shares" : (shares *
                                                            100.0).round() / 100.0, "our_price" : (avg * 10000.0)
                                                            .round() / 10000.0, "leader_price" : his, "his_known_shares"
                                                            : (his_known * 100.0).round() / 100.0, "drag_known" :
                                                            his_known > 1e-9 && (shares - his_known) <= 1e-4 * shares
                                                            .max(1.0), "mark" : mark, "entry_ts" : opened_t,
                                                            "unrealised" : unreal, "end_date" : end_date, "redeemable" :
                                                            redeemable, }
                                                        ),
                                                    );
                                            }
                                        }
                                    }
                                    let complete = snapshot.completeness.is_complete();
                                    let out = serde_json::to_string(
                                            &serde_json::json!(
                                                { "status" : if complete { "ok" } else { "partial" },
                                                "complete" : complete, "completeness" : snapshot
                                                .completeness.reason(), "as_of" :
                                                copybot_hot::ledger::now_secs(), "positions" : out_rows }
                                            ),
                                        )
                                        .unwrap_or_default();
                                    let resp = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: \
                            application/json\r\nContent-Length: {}\r\n\r\n{}",
                                        out.len(), out
                                    );
                                    let _ = sock.write_all(resp.as_bytes()).await;
                                    return;
                                }
                                if method == "GET" && path == "/api/response_times" {
                                    let samples = response_times_c.tail(50);
                                    let out = serde_json::to_string(
                                            &serde_json::json!({ "samples" : samples }),
                                        )
                                        .unwrap_or_default();
                                    let resp = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: \
                            application/json\r\nContent-Length: {}\r\n\r\n{}",
                                        out.len(), out
                                    );
                                    let _ = sock.write_all(resp.as_bytes()).await;
                                    return;
                                }
                                if method == "GET" && path == "/api/errors" {
                                    let rows = copybot_hot::errors::tail(&errors_path_c, 200);
                                    let now_s = copybot_hot::ledger::now_secs();
                                    let cutoff = now_s
                                        - copybot_hot::errors::ACTIVE_WINDOW_SECS;
                                    let counts = copybot_hot::errors::counts_by_lane(&rows);
                                    let active = copybot_hot::errors::counts_by_lane_since(
                                        &rows,
                                        cutoff,
                                    );
                                    let out = serde_json::to_string(
                                            &serde_json::json!(
                                                { "errors" : rows, "active_window_secs" :
                                                copybot_hot::errors::ACTIVE_WINDOW_SECS, "active_by_lane" :
                                                active.iter().map(| (l, s, w, n) | serde_json::json!({
                                                "lane" : l, "stops" : s, "warns" : w, "notices" : n }))
                                                .collect::< Vec < _ >> (), "by_lane" : counts.iter().map(|
                                                (l, s, w, n) | serde_json::json!({ "lane" : l, "stops" : s,
                                                "warns" : w, "notices" : n })).collect::< Vec < _ >> (), }
                                            ),
                                        )
                                        .unwrap_or_default();
                                    let resp = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: \
                            application/json\r\nContent-Length: {}\r\n\r\n{}",
                                        out.len(), out
                                    );
                                    let _ = sock.write_all(resp.as_bytes()).await;
                                    return;
                                }
                                if method == "POST" && path == "/api/wallets" {
                                    if let Some((code, msg)) = mutation_refusal(&req) {
                                        let out = serde_json::json!({ "error" : msg }).to_string();
                                        let resp = format!(
                                            "HTTP/1.1 {code}\r\nContent-Type: \
                                application/json\r\nContent-Length: {}\r\n\r\n{}",
                                            out.len(), out
                                        );
                                        let _ = sock.write_all(resp.as_bytes()).await;
                                        return;
                                    }
                                    let live_names: Vec<String> = r
                                        .snapshot()
                                        .iter()
                                        .map(|l| l.cfg.name.clone())
                                        .collect();
                                    let (code, out) = handle_wallet_patch(
                                        &registry_c,
                                        phys_c.lock().unwrap().equity,
                                        &live_names,
                                        body,
                                    );
                                    if code.starts_with("200") {
                                        ctl_http.lock().unwrap().ledger.lane_ids = registry_c
                                            .id_map();
                                    }
                                    let resp = format!(
                                        "HTTP/1.1 {code}\r\nContent-Type: \
                            application/json\r\nContent-Length: {}\r\n\r\n{}",
                                        out.len(), out
                                    );
                                    let _ = sock.write_all(resp.as_bytes()).await;
                                    return;
                                }
                                if method == "POST" && path == "/api/incident/clear" {
                                    if let Some((code, msg)) = mutation_refusal(&req) {
                                        let out = serde_json::json!({ "error" : msg }).to_string();
                                        let resp = format!(
                                            "HTTP/1.1 {code}\r\nContent-Type: \
                                application/json\r\nContent-Length: {}\r\n\r\n{}",
                                            out.len(), out
                                        );
                                        let _ = sock.write_all(resp.as_bytes()).await;
                                        return;
                                    }
                                    let v: serde_json::Value = serde_json::from_str(body)
                                        .unwrap_or(serde_json::Value::Null);
                                    let lane = v["lane"].as_str().unwrap_or("");
                                    let want = format!("clear {lane}");
                                    let (code, out) = if lane.is_empty() {
                                        (
                                            "400 Bad Request",
                                            serde_json::json!({ "error" : "lane required" }).to_string(),
                                        )
                                    } else if v["phrase"].as_str() != Some(want.as_str()) {
                                        (
                                            "400 Bad Request",
                                            serde_json::json!(
                                                { "error" : format!("clearing requires phrase {want:?}") }
                                            )
                                                .to_string(),
                                        )
                                    } else {
                                        let by = v["by"]
                                            .as_str()
                                            .unwrap_or("api:/api/incident/clear");
                                        let res: Result<bool, String> = inc_srv
                                            .lock()
                                            .map_err(|_| "journal lock poisoned".to_string())
                                            .and_then(|mut g| {
                                                g.clear(lane, by, copybot_hot::ledger::now_secs())
                                            });
                                        match res {
                                            Ok(was_open) => {
                                                let risk_cleared = c
                                                    .lock()
                                                    .ok()
                                                    .map(|mut g| {
                                                        g
                                                            .ledger
                                                            .clear_risk(
                                                                lane,
                                                                "operator cleared via /api/incident/clear",
                                                            )
                                                    })
                                                    .unwrap_or(false);
                                                if let Some(l) = r
                                                    .snapshot()
                                                    .iter()
                                                    .find(|l| l.cfg.name == lane)
                                                {
                                                    l.state.halt_latch.store(false, Ordering::Relaxed);
                                                }
                                                (
                                                    "200 OK",
                                                    serde_json::json!(
                                                        { "ok" : true, "lane" : lane, "was_open" : was_open,
                                                        "risk_breaker_cleared" : risk_cleared, "note" :
                                                        "buys resume at the next control tick" }
                                                    )
                                                        .to_string(),
                                                )
                                            }
                                            Err(e) => {
                                                (
                                                    "500 Internal Server Error",
                                                    serde_json::json!({ "error" : e }).to_string(),
                                                )
                                            }
                                        }
                                    };
                                    let resp = format!(
                                        "HTTP/1.1 {code}\r\nContent-Type: \
                            application/json\r\nContent-Length: {}\r\n\r\n{}",
                                        out.len(), out
                                    );
                                    let _ = sock.write_all(resp.as_bytes()).await;
                                    return;
                                }
                                if method == "POST" && path == "/api/arm" {
                                    if let Some((code, msg)) = mutation_refusal(&req) {
                                        let out = serde_json::json!({ "error" : msg }).to_string();
                                        let resp = format!(
                                            "HTTP/1.1 {code}\r\nContent-Type: \
                                application/json\r\nContent-Length: {}\r\n\r\n{}",
                                            out.len(), out
                                        );
                                        let _ = sock.write_all(resp.as_bytes()).await;
                                        return;
                                    }
                                    let (code, out) = handle_arm_post(
                                        &format!("{ctrl_path_c}.operator"),
                                        body,
                                        &actor_of(&req),
                                    );
                                    let resp = format!(
                                        "HTTP/1.1 {code}\r\nContent-Type: \
                            application/json\r\nContent-Length: {}\r\n\r\n{}",
                                        out.len(), out
                                    );
                                    let _ = sock.write_all(resp.as_bytes()).await;
                                    return;
                                }
                                if method == "POST" && path == "/api/flatten" {
                                    if let Some((code, msg)) = mutation_refusal(&req) {
                                        let out = serde_json::json!({ "error" : msg }).to_string();
                                        let resp = format!(
                                            "HTTP/1.1 {code}\r\nContent-Type: \
                                application/json\r\nContent-Length: {}\r\n\r\n{}",
                                            out.len(), out
                                        );
                                        let _ = sock.write_all(resp.as_bytes()).await;
                                        return;
                                    }
                                    let fpath = format!("{ctrl_path_c}.flatten");
                                    let (code, out) = handle_flatten_post(
                                        &fpath,
                                        body,
                                        |name| {
                                            r.snapshot()
                                                .iter()
                                                .find(|l| l.cfg.name == name)
                                                .map(|l| l.state.armed.load(Ordering::Relaxed))
                                        },
                                    );
                                    let resp = format!(
                                        "HTTP/1.1 {code}\r\nContent-Type: \
                            application/json\r\nContent-Length: {}\r\n\r\n{}",
                                        out.len(), out
                                    );
                                    let _ = sock.write_all(resp.as_bytes()).await;
                                    return;
                                }
                                if method == "GET" && path == "/api/matchup" {
                                    let path = req.split_whitespace().nth(1).unwrap_or("");
                                    let requested = path
                                        .split_once('?')
                                        .and_then(|(_, q)| {
                                            q.split('&').find_map(|part| part.strip_prefix("lane="))
                                        });
                                    let resolved = lane_specs
                                        .iter()
                                        .find(|x| requested.map(|v| v == x.0).unwrap_or(false))
                                        .cloned()
                                        .or_else(|| {
                                            let want = requested?;
                                            let lane = r
                                                .snapshot()
                                                .into_iter()
                                                .find(|l| l.cfg.name == want)?;
                                            Some((
                                                lane.cfg.name.clone(),
                                                format!("0x{}", hex::encode(lane.cfg.wallet20)),
                                                registry_c.seed_of(&lane.cfg.name),
                                                lane.policy().caps.daily_usd,
                                                std::sync::Arc::new(copybot_hot::matchup::Matchup::new()),
                                            ))
                                        })
                                        .or_else(|| {
                                            if requested.is_none() {
                                                lane_specs.first().cloned()
                                            } else {
                                                None
                                            }
                                        });
                                    let Some(
                                        (lane_name, lane_wallet, seed, lane_budget, mu2),
                                    ) = resolved else {
                                        let out = serde_json::json!(
                                            { "error" : "no such lane", "lane" : requested, }
                                        )
                                            .to_string();
                                        let resp = format!(
                                            "HTTP/1.1 404 Not Found\r\nContent-Type: \
                                    application/json\r\nContent-Length: {}\r\n\r\n{}",
                                            out.len(), out
                                        );
                                        let _ = sock.write_all(resp.as_bytes()).await;
                                        return;
                                    };
                                    let ledger_snapshot = c
                                        .lock()
                                        .ok()
                                        .and_then(|g| {
                                            g.ledger
                                                .lanes
                                                .get(&lane_name)
                                                .map(|b| (
                                                    b.risk.realised_pnl,
                                                    g.ledger.open_usd(&lane_name),
                                                ))
                                        });
                                    let led = ledger_snapshot.map(|x| x.0);
                                    let ledger_open = ledger_snapshot.map(|x| x.1);
                                    let lane_epoch = c
                                        .lock()
                                        .ok()
                                        .and_then(|g| g.ledger.lane_epoch(&lane_name));
                                    let mut body = mu2
                                        .get(&http2, &funder2, &lane_wallet, led, lane_epoch)
                                        .await;
                                    if let Some(open) = ledger_open {
                                        let chain_deployed = body["deployed"].as_f64();
                                        body["chain_deployed"] = serde_json::json!(chain_deployed);
                                        body["deployed"] = serde_json::json!(
                                            (open * 100.0).round() / 100.0
                                        );
                                        body["cost_basis_delta"] = serde_json::json!(
                                            chain_deployed.map(| x | ((open - x) * 100.0).round() /
                                            100.0)
                                        );
                                        body["deployed_source"] = serde_json::json!("lane ledger");
                                    }
                                    let have = creds_d.lock().unwrap().clone();
                                    let cash = match have {
                                        Some(cr) => {
                                            copybot_hot::matchup::free_cash(
                                                    &http2,
                                                    &clob_d,
                                                    &addr_d,
                                                    &cr,
                                                    sigtype_d,
                                                )
                                                .await
                                        }
                                        None => None,
                                    };
                                    let st = c.lock().unwrap().status(&r);
                                    let spent = st["lanes"][&lane_name]["spent_today"]
                                        .as_f64()
                                        .unwrap_or(0.0);
                                    let armed = st["lanes"][&lane_name]["armed"]
                                        .as_bool()
                                        .unwrap_or(false);
                                    let cap_scale = st["lanes"][&lane_name]["cap_scale"]
                                        .as_f64()
                                        .unwrap_or(1.0);
                                    let realised = led.unwrap_or(0.0);
                                    let virtual_bankroll = seed
                                        .map(|s| copybot_hot::budget::virtual_bankroll(
                                            s,
                                            realised,
                                        ));
                                    let portfolio = cash
                                        .map(|c| {
                                            let v = body["wallet_value"].as_f64().unwrap_or(0.0);
                                            ((c + v) * 100.0).round() / 100.0
                                        });
                                    let funding_basis: f64 = registry_c
                                        .specs()
                                        .iter()
                                        .filter(|w| w.enabled)
                                        .map(|w| w.seed_usd)
                                        .sum();
                                    let equity_pnl = wallet_equity_pnl(
                                        portfolio,
                                        funding_basis,
                                    );
                                    body["wallet"] = serde_json::json!(
                                        { "portfolio" : portfolio, "funding_basis" : (funding_basis
                                        * 100.0).round() / 100.0, "equity_pnl" : equity_pnl,
                                        "equity_pnl_source" :
                                        "wallet mark-to-market equity minus enabled virtual seeds; not adjusted for later deposits or withdrawals",
                                        "cash" : cash, "deployed" : body["deployed"],
                                        "chain_deployed" : body["chain_deployed"],
                                        "cost_basis_delta" : body["cost_basis_delta"],
                                        "deployed_source" : body["deployed_source"], "value" :
                                        body["value"], "shared_position_value" :
                                        body["wallet_value"], "shared_position_count" :
                                        body["wallet_positions"], "spent_today" : (spent * 100.0)
                                        .round() / 100.0, "daily_budget" : lane_budget,
                                        "seed_bankroll" : seed, "virtual_bankroll" :
                                        virtual_bankroll, "sizing_bankroll" : seed.map(| s | s *
                                        cap_scale), "cap_scale" : cap_scale, "realised_pnl" :
                                        realised, "physical_shared" : true, }
                                    );
                                    body["selected_lane"] = serde_json::json!(lane_name);
                                    body["strategies"] = serde_json::json!(
                                        lane_specs.iter().map(| x | & x.0).collect::< Vec < _ >> ()
                                    );
                                    body["runtime"] = serde_json::json!(
                                        { "port" : port, "pid" : std::process::id(), "authoritative"
                                        : armed, "warning" : if armed { serde_json::Value::Null }
                                        else {
                                        serde_json::json!("DISARMED SLOT: figures are not the live authority")
                                        } }
                                    );
                                    let out = serde_json::to_string(&body).unwrap_or_default();
                                    let resp = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: \
                            application/json\r\nContent-Length: {}\r\n\r\n{}",
                                        out.len(), out
                                    );
                                    let _ = sock.write_all(resp.as_bytes()).await;
                                    return;
                                }
                                if let Some((code, msg)) = route_refusal(method, path) {
                                    let out = serde_json::json!(
                                        { "error" : msg, "path" : path, "method" : method }
                                    )
                                        .to_string();
                                    let resp = format!(
                                        "HTTP/1.1 {code}\r\nContent-Type: \
                            application/json\r\nContent-Length: {}\r\n\r\n{}",
                                        out.len(), out
                                    );
                                    let _ = sock.write_all(resp.as_bytes()).await;
                                    return;
                                }
                                let mut body = c.lock().unwrap().status(&r);
                                for (name, _, seed, _, _) in &lane_specs {
                                    let realised = body["lanes"][name]["risk"]["realised_pnl"]
                                        .as_f64()
                                        .unwrap_or(0.0);
                                    body["lanes"][name]["seed_bankroll"] = serde_json::json!(
                                        seed
                                    );
                                    body["lanes"][name]["virtual_bankroll"] = serde_json::json!(
                                        seed.map(| s | copybot_hot::budget::virtual_bankroll(s,
                                        realised))
                                    );
                                }
                                body["fingerprint"] = serde_json::json!(
                                    { "watching" : fp2.known_len(), "emitted" : * fp2.emitted
                                    .lock().unwrap(), "abstained_contested" : * fp2
                                    .abstained_contested.lock().unwrap(), "abstained_stale" : *
                                    fp2.abstained_stale.lock().unwrap(), "shadow" : sc2
                                    .verdict(30), }
                                );
                                body["books_cached"] = serde_json::json!(bk2.len());
                                body["resting"] = {
                                    let now = copybot_hot::ledger::now_secs();
                                    let rows: Vec<serde_json::Value> = rb_d
                                        .all()
                                        .into_iter()
                                        .map(|o| {
                                            let v = rest_verdict(&levels_d, &prints_d, &o);
                                            serde_json::json!(
                                                { "lane" : o.lane, "side" : if o.side == 0 { "BUY" } else {
                                                "SELL" }, "tok" : & o.token[..o.token.len().min(14)],
                                                "limit" : o.limit, "shares" : o.shares, "age_secs" : now - o
                                                .placed, "verdict" : format!("{v:?}"), "max_age_secs" :
                                                copybot_hot::restwatch::max_age_for(o.side), "anchored" : o
                                                .his_remaining > 0.0, }
                                            )
                                        })
                                        .collect();
                                    serde_json::json!({ "n" : rows.len(), "orders" : rows })
                                };
                                body["custody"] = match custody_d.lock().unwrap().as_ref() {
                                    Some(r) => {
                                        serde_json::json!(
                                            { "checked" : r.checked, "agreed" : r.agreed, "explained" :
                                            r.explained, "unattributed" : r.unattributed, "unknown" : r
                                            .unknown, "complete" : r.complete, "unattributed_shares" :
                                            (r.unattributed_shares * 10_000.0).round() / 10_000.0,
                                            "alarm" : r.is_alarm(), "rows" : r.rows, "msg" :
                                            copybot_hot::custody::describe(r), }
                                        )
                                    }
                                    None => serde_json::Value::Null,
                                };
                                body["pending_persistence"] = {
                                    let l = pend_d.lock().unwrap();
                                    serde_json::json!(
                                        { "ok" : l.persistence_ok(), "write_failures" : l
                                        .write_failures, "open_orders" : l.len(), "last_error" : l
                                        .last_write_error }
                                    )
                                };
                                body["signal_guard"] = match signal_guard_d {
                                    Some(guard) => {
                                        match guard.active_markets() {
                                            Ok(active) => {
                                                serde_json::json!(
                                                    { "enabled" : true, "healthy" : true, "active_markets" :
                                                    active, "market_ttl_seconds" :
                                                    copybot_hot::signal_guard::MARKET_TTL_SECS, }
                                                )
                                            }
                                            Err(e) => {
                                                serde_json::json!(
                                                    { "enabled" : true, "healthy" : false, "error" :
                                                    format!("{e:?}"), }
                                                )
                                            }
                                        }
                                    }
                                    None => {
                                        serde_json::json!({ "enabled" : false, "healthy" : true })
                                    }
                                };
                                let out = serde_json::to_string_pretty(&body)
                                    .unwrap_or_default();
                                let resp = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                        Content-Length: {}\r\n\r\n{}",
                                    out.len(), out
                                );
                                let _ = sock.write_all(resp.as_bytes()).await;
                            },
                        )
                        .await;
                    if served.is_err() {
                        eprintln!(
                            "[dash] dropped a connection that stalled past the \
{REQUEST_DEADLINE:?} request deadline"
                        );
                    }
                });
            }
        });
    }
    let txstats = Arc::new(TxpoolStats::default());
    let txstats_fill = txstats.clone();
    if let Some(rpc) = root.bot.txpool_rpc.clone() {
        tokio::spawn(
            txpool::run(rpc, tx.clone(), txstats.clone(), Duration::from_secs(20)),
        );
    }
    {
        let (s, e, bk, wt) = (
            stats.clone(),
            emitter.clone(),
            books.clone(),
            watch_tokens.clone(),
        );
        let (reg, rb) = (feed_registry.clone(), book.clone());
        let reg_rb = resting_book.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let per: Vec<serde_json::Value> = reg
                    .snapshot()
                    .into_iter()
                    .map(|(name, frames, xtx, recon, errs)| {
                        serde_json::json!(
                            { "feed" : name, "frames" : frames, "exchange_txs" : xtx,
                            "reconnects" : recon, "errors" : errs, }
                        )
                    })
                    .collect();
                let (wins, confirms, redeliveries) = rb.stats();
                e.emit(
                    serde_json::json!(
                        { "t" : now_ms(), "ev" : "feed_stats", "books_cached" : bk.len(),
                        "books_watched" : wt.lock().unwrap().len(), "frames" : s.frames
                        .load(Ordering::Relaxed), "exchange_txs" : s.exchange_txs
                        .load(Ordering::Relaxed), "reconnects" : s.reconnects
                        .load(Ordering::Relaxed), "errors" : s.errors
                        .load(Ordering::Relaxed), "per_feed" : per, "resting" : reg_rb
                        .snapshot(), "race" : { "wins" : wins, "confirms" : confirms,
                        "redeliveries" : redeliveries, "behind_ms" : rb
                        .median_behind_ms(), }, }
                    ),
                );
            }
        });
    }
    let mk_client = |h2: bool| {
        let b = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .tcp_nodelay(true)
            .pool_max_idle_per_host(4)
            .timeout(Duration::from_secs(10));
        if h2 { b.build() } else { b.http1_only().build() }
    };
    let http = mk_client(false).expect("h1 client");
    let mut order_paths: Vec<(String, reqwest::Client)> = vec![
        ("h1".into(), http.clone())
    ];
    if root.bot.race_h2 {
        match mk_client(true) {
            Ok(c) => order_paths.push(("h2".into(), c)),
            Err(e) => eprintln!("[transport] h2 unavailable: {e}"),
        }
        match mk_client(false) {
            Ok(c) => order_paths.push(("h1b".into(), c)),
            Err(e) => eprintln!("[transport] third path unavailable: {e}"),
        }
    }
    eprintln!(
        "[transport] racing {} paths: {}", order_paths.len(), order_paths.iter().map(|
        (n, _) | n.as_str()).collect::< Vec < _ >> ().join(", ")
    );
    let order_paths = Arc::new(order_paths);
    let clob = root.bot.clob_host.clone();
    let creds: Arc<Option<copybot_hot::auth::ApiCreds>> = if shadow {
        eprintln!(
            "[auth] SHADOW — credentials deliberately NOT derived. Every order \
will be rejected 401 by the venue. This process cannot place an order."
        );
        Arc::new(None)
    } else {
        use copybot_hot::auth::derive_or_create_creds;
        match pk {
            Some(pk) => {
                match derive_or_create_creds(&http, &clob, &pk, &signer_addr, 0).await {
                    Ok(c) => {
                        eprintln!("[auth] L2 credentials derived — {}", c.redacted());
                        *creds_slot.lock().unwrap() = Some(c.clone());
                        Arc::new(Some(c))
                    }
                    Err(e) => {
                        eprintln!("[auth] FAILED to obtain L2 credentials: {e}");
                        Arc::new(None)
                    }
                }
            }
            None => {
                eprintln!(
                    "[auth] no PRIVATE_KEY in env — L2 auth UNAVAILABLE (paper mode only)"
                );
                Arc::new(None)
            }
        }
    };
    if let Some(cr) = creds.as_ref() {
        let path = "/data/orders";
        let ts = copybot_hot::auth::now_secs();
        match copybot_hot::auth::l2_headers(&signer_addr, cr, ts, "GET", path, None) {
            Ok(h) => {
                let mut rb = http.get(format!("{clob}{path}"));
                for (k, v) in &h {
                    rb = rb.header(*k, v);
                }
                match rb.send().await {
                    Ok(r) if r.status().is_success() => {
                        eprintln!(
                            "[auth] self-check OK — {path} accepted our L2 headers"
                        );
                        match r.json::<serde_json::Value>().await {
                            Ok(v) => {
                                let n = (v.as_array().map(|a| a.len()))
                                    .or_else(|| v["data"].as_array().map(|a| a.len()))
                                    .unwrap_or(0);
                                if n > 0 {
                                    eprintln!(
                                        "[resting] venue holds {n} live order(s); attribution deferred to the open-orders reconcile (~60s)"
                                    );
                                    emitter
                                        .emit(
                                            serde_json::json!(
                                                { "t" : now_ms(), "ev" : "resting_seen_at_boot", "orders" :
                                                n }
                                            ),
                                        );
                                }
                            }
                            Err(e) => {
                                eprintln!(
                                    "[resting] could not read the open-order list ({e}); a GTC order the venue still holds would be unmanaged"
                                )
                            }
                        }
                    }
                    Ok(r) => {
                        eprintln!(
                            "[auth] ⛔ SELF-CHECK FAILED: {path} -> {}. Credentials \
derived but are NOT accepted; every order would be refused. Refusing to continue.",
                            r.status()
                        );
                        if live && !shadow {
                            std::process::exit(2);
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "[auth] ⛔ self-check could not run ({e}); auth is UNVERIFIED"
                        );
                        if live && !shadow {
                            std::process::exit(2);
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("[auth] ⛔ self-check header build failed: {e}");
                if live && !shadow {
                    std::process::exit(2);
                }
            }
        }
    }
    if live && !shadow && creds.is_none() {
        eprintln!(
            "[auth] ⛔ REFUSING TO ARM: mode=live but there are no L2 credentials.\n\
                   [auth]    Every /order POST would be rejected 401 by the venue.\n\
                   [auth]    Set PRIVATE_KEY, or run mode=paper."
        );
        std::process::exit(2);
    }
    let auth_addr = signer_addr.clone();
    {
        const TTL_SECS: i64 = 1_800;
        const MAX_LIVE_PER_LANE: usize = 6;
        const MAX_LIVE: usize = 36;
        let (rb, http_c, clob_c, addr_c, creds_c, r_c, e_c) = (
            resting_book.clone(),
            http.clone(),
            clob.clone(),
            auth_addr.clone(),
            creds.clone(),
            router.clone(),
            emitter.clone(),
        );
        let sg_sweep = signal_guard.clone();
        let (levels_sweep, prints_sweep) = (levels.clone(), prints.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if rb.is_empty() {
                    continue;
                }
                let Some(cr) = creds_c.as_ref().as_ref() else { continue };
                let now = copybot_hot::ledger::now_secs();
                let armed_anywhere = r_c
                    .snapshot()
                    .iter()
                    .any(|l| l.state.armed.load(Ordering::Relaxed));
                use copybot_hot::resting::CancelReason;
                let mut doomed: Vec<(String, CancelReason)> = Vec::new();
                if !armed_anywhere {
                    for o in rb.all() {
                        doomed.push((o.order_id, CancelReason::Disarmed));
                    }
                } else {
                    let by_name: std::collections::HashMap<String, std::sync::Arc<_>> = r_c
                        .snapshot()
                        .into_iter()
                        .map(|l| (l.cfg.name.clone(), l))
                        .collect();
                    for o in rb.all() {
                        let pull = match by_name.get(&o.lane) {
                            Some(l) => {
                                copybot_hot::resting::halted_lane_must_pull(
                                    o.side,
                                    l.state.halted.load(Ordering::Relaxed),
                                    l.state.armed.load(Ordering::Relaxed),
                                    l.state.retired.load(Ordering::Relaxed),
                                )
                            }
                            None => o.side == 0,
                        };
                        if pull {
                            doomed.push((o.order_id, CancelReason::LaneHalted));
                        }
                    }
                    for o in rb.expired(now, TTL_SECS) {
                        if doomed.iter().any(|(i, _)| i == &o.order_id) {
                            continue;
                        }
                        let v = rest_verdict(&levels_sweep, &prints_sweep, &o);
                        if copybot_hot::restwatch::renewable(v, now - o.placed, o.side) {
                            continue;
                        }
                        doomed.push((o.order_id, CancelReason::Expired));
                    }
                    for o in rb.all() {
                        if doomed.iter().any(|(i, _)| i == &o.order_id) {
                            continue;
                        }
                        if rest_verdict(&levels_sweep, &prints_sweep, &o)
                            == copybot_hot::restwatch::RestVerdict::HeCancelled
                        {
                            doomed.push((o.order_id, CancelReason::HeCancelled));
                        }
                    }
                    for o in rb.all() {
                        let permitted = r_c
                            .snapshot()
                            .iter()
                            .find(|l| l.cfg.name == o.lane)
                            .map(|l| {
                                if o.side == 0 {
                                    l.state.rest_buys.load(Ordering::Relaxed)
                                } else {
                                    l.state.rest_sells.load(Ordering::Relaxed)
                                }
                            })
                            .unwrap_or(false);
                        if !permitted && !doomed.iter().any(|(i, _)| i == &o.order_id) {
                            doomed.push((o.order_id, CancelReason::RestingDisabled));
                        }
                    }
                    for o in rb
                        .over_cap_per_lane(MAX_LIVE_PER_LANE)
                        .into_iter()
                        .chain(rb.over_cap(MAX_LIVE))
                    {
                        if !doomed.iter().any(|(i, _)| i == &o.order_id) {
                            doomed.push((o.order_id, CancelReason::OverCap));
                        }
                    }
                }
                if doomed.is_empty() {
                    continue;
                }
                let mut ids: Vec<String> = doomed
                    .iter()
                    .map(|(i, _)| i.clone())
                    .collect();
                ids.sort();
                ids.dedup();
                let res = copybot_hot::resting::cancel_batch(
                        &http_c,
                        &clob_c,
                        &addr_c,
                        cr,
                        &ids,
                    )
                    .await;
                let settles = res
                    .iter()
                    .filter(|(_, ok)| *ok)
                    .map(|(id, _)| {
                        let why = doomed
                            .iter()
                            .find(|(i, _)| i == id)
                            .map(|(_, r)| r.as_str())
                            .unwrap_or("cancelled");
                        settle_cancelled_rest(
                            &http_c,
                            &clob_c,
                            &addr_c,
                            cr,
                            &rb,
                            &sg_sweep,
                            &e_c,
                            id,
                            why,
                        )
                    });
                futures_util::future::join_all(settles).await;
                let why = doomed.first().map(|(_, r)| r.as_str()).unwrap_or("?");
                e_c.emit(
                    serde_json::json!(
                        { "t" : now_ms(), "ev" : "cancel_sweep", "reason" : why,
                        "attempted" : ids.len(), "cancelled" : res.iter().filter(| (_,
                        ok) | * ok).count(), "still_live" : rb.len(), }
                    ),
                );
            }
        });
    }
    let (sweep_tx, mut sweep_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let sweep_tx = std::sync::Arc::new(sweep_tx);
    if live {
        let pend3 = pending_log.clone();
        let inc3 = incidents.clone();
        let mc3 = merge_claims.clone();
        let (c3, r3, e3, f3, pk3, sa3, st3, clob3, creds3, paths3) = (
            control.clone(),
            router.clone(),
            emitter.clone(),
            funder.clone(),
            pk,
            signer_addr.clone(),
            sig_type,
            clob.clone(),
            creds.clone(),
            order_paths.clone(),
        );
        tokio::spawn(async move {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default();
            let mut swept: std::collections::HashMap<String, u64> = Default::default();
            let mut pending: std::collections::HashMap<
                (String, String),
                (u64, f64, f64),
            > = Default::default();
            let mut first_backstop = true;
            loop {
                let delay = if first_backstop { 10 } else { 60 };
                let just_sold: Option<String> = tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(delay)) => {
                    first_backstop = false; None }, tok = sweep_rx.recv() => {
                    tokio::time::sleep(Duration::from_secs(3)). await; match tok {
                    Some(t) => Some(t), None => continue } }
                };
                let Some(cr) = creds3.as_ref() else { continue };
                let Some(key) = pk3.as_ref() else { continue };
                let url = format!(
                    "https://data-api.polymarket.com/positions?user={f3}\
&sizeThreshold=0.0001&limit=500"
                );
                let Ok(resp) = http.get(&url).send().await else { continue };
                if !resp.status().is_success() {
                    continue;
                }
                let Ok(v) = resp.json::<serde_json::Value>().await else { continue };
                let Some(ours) = v.as_array() else { continue };
                for lane in r3.snapshot().iter() {
                    let name = lane.cfg.name.clone();
                    if !lane.state.armed.load(Ordering::Relaxed) {
                        pending.retain(|(lane_name, _), _| lane_name != &name);
                        e3.emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "sweep_skip", "lane" : name,
                                "reason" : "lane_disarmed" }
                            ),
                        );
                        continue;
                    }
                    let leader = format!("0x{}", hex::encode(lane.cfg.wallet20));
                    let his_url = format!(
                        "https://data-api.polymarket.com/positions?user={leader}\
&sizeThreshold=0.0001&limit=500"
                    );
                    let his = match http.get(&his_url).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            match resp.json::<serde_json::Value>().await {
                                Ok(rows) => {
                                    rows.as_array()
                                        .map(|a| {
                                            a
                                                .iter()
                                                .filter_map(|p| {
                                                    let token = p["asset"].as_str()?.to_string();
                                                    let size = p["size"]
                                                        .as_f64()
                                                        .or_else(|| p["size"].as_str().and_then(|x| x.parse().ok()))
                                                        .unwrap_or(0.0);
                                                    let avg = p["avgPrice"]
                                                        .as_f64()
                                                        .or_else(|| {
                                                            p["avgPrice"].as_str().and_then(|x| x.parse().ok())
                                                        })
                                                        .unwrap_or(0.0);
                                                    Some((token, (size, avg)))
                                                })
                                                .collect::<std::collections::HashMap<_, _>>()
                                        })
                                }
                                Err(_) => None,
                            }
                        }
                        _ => None,
                    };
                    let Some(his) = his else {
                        e3.emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "sweep_skip", "lane" : name,
                                "reason" : "leader_positions_unavailable" }
                            ),
                        );
                        continue;
                    };
                    let held = c3.lock().unwrap().ledger.holdings(&name);
                    for p in ours {
                        let tok = p["asset"].as_str().unwrap_or("").to_string();
                        if tok.is_empty() {
                            continue;
                        }
                        if !held.contains_key(&tok) {
                            continue;
                        }
                        if p["redeemable"].as_bool().unwrap_or(false) {
                            continue;
                        }
                        if mc3
                            .lock()
                            .map(|g| g.is_claimed(&name, &tok, now_ms() as u64 / 1000))
                            .unwrap_or(true)
                        {
                            e3.emit(
                                serde_json::json!(
                                    { "t" : now_ms(), "ev" : "sweep_skip", "lane" : name,
                                    "token" : tok, "reason" : "merge_in_flight" }
                                ),
                            );
                            continue;
                        }
                        let (his_sz, his_avg) = his
                            .get(&tok)
                            .copied()
                            .unwrap_or((0.0, 0.0));
                        let physical_sz = p["size"]
                            .as_f64()
                            .or_else(|| p["size"].as_str().and_then(|x| x.parse().ok()))
                            .unwrap_or(0.0);
                        let ours_claim = held.get(&tok).copied().unwrap_or(0.0);
                        let pool = c3.lock().unwrap().ledger.pool_claim(&tok);
                        let sz = ours_claim
                            .min(
                                copybot_hot::custody::lane_share_of(
                                    physical_sz,
                                    pool,
                                    ours_claim,
                                ),
                            );
                        let px = p["curPrice"]
                            .as_f64()
                            .or_else(|| {
                                p["curPrice"].as_str().and_then(|x| x.parse().ok())
                            })
                            .unwrap_or(0.0);
                        let our_avg = p["avgPrice"]
                            .as_f64()
                            .or_else(|| {
                                p["avgPrice"].as_str().and_then(|x| x.parse().ok())
                            })
                            .unwrap_or(0.0);
                        if sz <= 1e-6 || px <= 0.0 {
                            continue;
                        }
                        let obs_key = (name.clone(), tok.clone());
                        let now_s = now_ms() as u64 / 1000;
                        let prior = pending
                            .get(&obs_key)
                            .map(|(seen, t, e)| (now_s.saturating_sub(*seen), *t, *e));
                        let pol = lane.policy();
                        let proportional_target = match pol.sizing {
                            copybot_hot::lanes::Sizing::Pct(pct) => {
                                let scale = lane.state.cap_scale.load(Ordering::Relaxed)
                                    as f64 / MICRO;
                                recovery_target_shares(
                                    his_sz,
                                    his_avg,
                                    our_avg,
                                    pct,
                                    scale,
                                    lane.cfg.min_order_usd,
                                    pol.max_effective_pct,
                                    pol.compound,
                                )
                            }
                            _ => None,
                        };
                        let action = copybot_hot::sweep::decide(
                            &copybot_hot::sweep::SweepInputs {
                                owned: held.get(&tok).copied().unwrap_or(0.0),
                                physical: physical_sz,
                                leader: his_sz,
                                mark: px,
                                redeemable: p["redeemable"].as_bool().unwrap_or(false),
                                just_sold: just_sold.as_deref() == Some(tok.as_str()),
                                proportional_target,
                                prior,
                                confirm_secs: 60,
                                min_usd: 0.02,
                            },
                        );
                        let (sell_qty, reason, is_dust) = match action {
                            copybot_hot::sweep::SweepAction::Skip { .. } => {
                                pending.remove(&obs_key);
                                continue;
                            }
                            copybot_hot::sweep::SweepAction::Observe {
                                why,
                                target,
                                excess,
                            } => {
                                let stored = copybot_hot::sweep::observation_to_store(
                                    pending.get(&obs_key),
                                    now_s,
                                    target,
                                    excess,
                                );
                                pending.insert(obs_key.clone(), stored);
                                e3.emit(
                                    serde_json::json!(
                                        { "t" : now_ms(), "ev" : "recovery_pending", "lane" : name,
                                        "tok" : tok, "owned" : sz, "physical" : physical_sz,
                                        "leader" : his_sz, "target" : target, "excess" : excess,
                                        "reason" : why, "his_avg" : his_avg, "our_avg" : our_avg,
                                        "cap_scale" : lane.state.cap_scale.load(Ordering::Relaxed)
                                        as f64 / MICRO, "confirm_after_secs" : 60 }
                                    ),
                                );
                                continue;
                            }
                            copybot_hot::sweep::SweepAction::Sell { qty, why, dust } => {
                                pending.remove(&obs_key);
                                (qty, why, dust)
                            }
                        };
                        let now = now_ms() as u64 / 1000;
                        let sweep_key = format!("{name}:{tok}");
                        if swept
                            .get(&sweep_key)
                            .map(|t| now.saturating_sub(*t) < 60)
                            .unwrap_or(false)
                        {
                            continue;
                        }
                        swept.insert(sweep_key, now);
                        let (shares, limit) = if is_dust {
                            copybot_hot::venue::dust_fak_terms(sell_qty, px)
                        } else {
                            copybot_hot::venue::cleanup_fak_terms(sell_qty, px)
                        };
                        if shares <= 0.0 {
                            e3.emit(
                                serde_json::json!(
                                    { "t" : now_ms(), "ev" : "sweep_skip", "lane" : name, "tok"
                                    : tok, "shares" : sz, "reason" : "below_venue_share_tick" }
                                ),
                            );
                            continue;
                        }
                        let (ma, ta) = copybot_hot::order::amounts(limit, shares, 1);
                        let neg = p["negativeRisk"].as_bool().unwrap_or(false);
                        let ord = copybot_hot::order::Order {
                            salt: copybot_hot::order::safe_salt(now_ms()),
                            maker: f3.clone(),
                            signer: sa3.clone(),
                            token_id: tok.clone(),
                            maker_amount: ma,
                            taker_amount: ta,
                            side: 1,
                            signature_type: st3,
                            timestamp: now_ms(),
                            neg_risk: neg,
                        };
                        let sig = ord.sign_for_type(key);
                        let body = copybot_hot::order::json_body(
                            &ord,
                            &sig,
                            &cr.key,
                            "FAK",
                        );
                        let Ok(body_s) = serde_json::to_string(&body) else { continue };
                        let hdrs = match copybot_hot::auth::l2_headers(
                            &sa3,
                            cr,
                            copybot_hot::auth::now_secs(),
                            "POST",
                            "/order",
                            Some(&body_s),
                        ) {
                            Ok(headers) => headers,
                            Err(err) => {
                                e3.emit(
                                    serde_json::json!(
                                        { "t" : now_ms(), "ev" : "sweep_skip", "lane" : name, "tok"
                                        : tok, "reason" : "auth_header_failed", "error" : err, }
                                    ),
                                );
                                continue;
                            }
                        };
                        let sweep_hash = hex::encode(ord.digest());
                        let (outcome, _res) = match submit_tracked(
                                &pend3,
                                &e3,
                                &copybot_hot::provenance::Provenance::own(
                                    copybot_hot::provenance::Origin::DustSweep {
                                        reason: reason.to_string(),
                                    },
                                    &name,
                                    &tok,
                                    1,
                                    shares,
                                    limit,
                                    sz,
                                ),
                                sweep_hash.clone(),
                                false,
                                &paths3,
                                &format!("{clob3}/order"),
                                &body_s,
                                &hdrs,
                                Duration::from_secs(10),
                                false,
                            )
                            .await
                        {
                            Ok(v) => v,
                            Err(e) => {
                                eprintln!("[{name}] sweep REFUSING TO SUBMIT: {e}");
                                e3.emit(
                                    serde_json::json!(
                                        { "t" : now_ms(), "ev" : "submit_refused", "lane" : name,
                                        "tok" : tok, "why" : e }
                                    ),
                                );
                                continue;
                            }
                        };
                        e3.emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "sweep", "lane" : name, "tok" :
                                tok, "shares" : shares, "limit" : limit, "reason" : reason,
                                "order_type" : "FAK", "max_slippage" : "100%", "usd" :
                                shares * limit, "outcome" : format!("{:?}", outcome)
                                .chars().take(90).collect::< String > () }
                            ),
                        );
                        if let copybot_hot::race_send::Outcome::Matched { body, .. } = &outcome {
                            book_and_resolve(
                                &c3,
                                &pend3,
                                &r3,
                                &inc3,
                                &e3,
                                &name,
                                &tok,
                                1,
                                limit,
                                body,
                                None,
                                &sweep_hash,
                                false,
                            );
                        }
                    }
                }
            }
        });
    }
    if live {
        let flatten_path = format!("{}.flatten", root.bot.control_path);
        let pend4 = pending_log.clone();
        let inc4 = incidents.clone();
        let (c4, r4, e4, f4, pk4, sa4, st4, clob4, creds4, paths4, wake4, reg4) = (
            control.clone(),
            router.clone(),
            emitter.clone(),
            funder.clone(),
            pk,
            signer_addr.clone(),
            sig_type,
            clob.clone(),
            creds.clone(),
            order_paths.clone(),
            sweep_tx.clone(),
            registry.clone(),
        );
        let err4 = errors_path.clone();
        tokio::spawn(async move {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default();
            let mut done: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let mut boxes: Vec<String> = r4
                    .snapshot()
                    .iter()
                    .map(|l| format!("{flatten_path}.{}", l.cfg.name))
                    .collect();
                boxes.push(flatten_path.clone());
                let found = boxes
                    .iter()
                    .find_map(|m| {
                        let raw = std::fs::read_to_string(m).ok()?;
                        let intent: copybot_hot::flatten::Intent = serde_json::from_str(
                                &raw,
                            )
                            .ok()?;
                        if done.get(&intent.lane).copied().unwrap_or(0) >= intent.ts {
                            return None;
                        }
                        Some(intent)
                    });
                let Some(intent) = found else { continue };
                let Some(lane) = r4
                    .snapshot()
                    .iter()
                    .find(|l| l.cfg.name == intent.lane)
                    .cloned() else {
                    let why = "unknown lane";
                    e4.emit(
                        serde_json::json!(
                            { "t" : now_ms(), "ev" : "flatten_gate_refused", "reason" :
                            why, "lane" : intent.lane }
                        ),
                    );
                    copybot_hot::errors::record(
                        &err4,
                        &copybot_hot::errors::ErrorRow {
                            t: copybot_hot::ledger::now_secs(),
                            lane: intent.lane.clone(),
                            kind: "flatten".into(),
                            detail: format!("flatten intent refused: {why}"),
                            human: format!(
                                "A flatten was requested for {:?}, which is not a wallet this \
bot runs. NOTHING HAS BEEN SOLD. Check the wallet name and try again.",
                                intent.lane
                            ),
                            severity: "warn".into(),
                        },
                    );
                    done.insert(intent.lane.clone(), intent.ts);
                    continue;
                };
                let armed = lane.state.armed.load(Ordering::Relaxed);
                let mode = match copybot_hot::flatten::gate(
                    &intent,
                    &lane.cfg.name,
                    armed,
                    copybot_hot::ledger::now_secs(),
                ) {
                    Ok(m) => m,
                    Err(reason) => {
                        e4.emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "flatten_gate_refused", "lane" :
                                intent.lane, "mode" : intent.mode, "reason" : reason }
                            ),
                        );
                        copybot_hot::errors::record(
                            &err4,
                            &copybot_hot::errors::ErrorRow {
                                t: copybot_hot::ledger::now_secs(),
                                lane: intent.lane.clone(),
                                kind: "flatten".into(),
                                detail: format!("flatten intent refused: {reason}"),
                                human: format!(
                                    "A {} flatten of {} was refused: {reason}. NOTHING HAS BEEN \
SOLD — the wallet still holds everything it held before.",
                                    intent.mode, intent.lane
                                ),
                                severity: "warn".into(),
                            },
                        );
                        done.insert(intent.lane.clone(), intent.ts);
                        continue;
                    }
                };
                done.insert(intent.lane.clone(), intent.ts);
                e4.emit(
                    serde_json::json!(
                        { "t" : now_ms(), "ev" : "flatten_start", "lane" : lane.cfg.name,
                        "mode" : mode.as_str() }
                    ),
                );
                lane.state.retired.store(true, Ordering::Relaxed);
                let modestr = mode.as_str().to_string();
                let laname = lane.cfg.name.clone();
                let _ = reg4
                    .mutate(|specs| {
                        if let Some(w) = specs.iter_mut().find(|w| w.name == laname) {
                            w.stop_mode = modestr.clone();
                        }
                    });
                if mode == copybot_hot::flatten::Mode::Graceful {
                    e4.emit(
                        serde_json::json!(
                            { "t" : now_ms(), "ev" : "flatten_done", "lane" : lane.cfg
                            .name, "mode" : mode.as_str(), "positions_sold" : 0 }
                        ),
                    );
                    continue;
                }
                let (Some(cr), Some(key)) = (creds4.as_ref(), pk4.as_ref()) else {
                    continue
                };
                let url = format!(
                    "https://data-api.polymarket.com/positions?user={f4}\
&sizeThreshold=0.0001&limit=500"
                );
                let ours: Vec<serde_json::Value> = match http.get(&url).send().await {
                    Ok(r) if r.status().is_success() => {
                        r.json().await.unwrap_or_default()
                    }
                    _ => Vec::new(),
                };
                let mark_of = |tok: &str| -> f64 {
                    ours.iter()
                        .find(|p| p["asset"].as_str() == Some(tok))
                        .and_then(|p| {
                            p["curPrice"]
                                .as_f64()
                                .or_else(|| {
                                    p["curPrice"].as_str().and_then(|x| x.parse().ok())
                                })
                        })
                        .unwrap_or(0.0)
                };
                let held = c4.lock().unwrap().ledger.holdings(&lane.cfg.name);
                let mut sold = 0usize;
                for (tok, sz) in held.iter() {
                    if *sz <= 1e-9 {
                        continue;
                    }
                    let mark = mark_of(tok);
                    let avg = c4.lock().unwrap().ledger.avg_cost(&lane.cfg.name, tok);
                    if !copybot_hot::flatten::should_sell(mode, mark, avg) {
                        continue;
                    }
                    let px = if mark > 0.0 { mark } else { 0.02 };
                    let (shares, limit) = if mode == copybot_hot::flatten::Mode::Panic {
                        copybot_hot::venue::dust_fak_terms(*sz, px)
                    } else {
                        copybot_hot::venue::cleanup_fak_terms(*sz, px)
                    };
                    if shares <= 0.0 {
                        continue;
                    }
                    let neg = ours
                        .iter()
                        .find(|p| p["asset"].as_str() == Some(tok.as_str()))
                        .and_then(|p| p["negativeRisk"].as_bool())
                        .unwrap_or(false);
                    let (ma, ta) = copybot_hot::order::amounts(limit, shares, 1);
                    let ord = copybot_hot::order::Order {
                        salt: copybot_hot::order::safe_salt(now_ms()),
                        maker: f4.clone(),
                        signer: sa4.clone(),
                        token_id: tok.clone(),
                        maker_amount: ma,
                        taker_amount: ta,
                        side: 1,
                        signature_type: st4,
                        timestamp: now_ms(),
                        neg_risk: neg,
                    };
                    let sig = ord.sign_for_type(key);
                    let body = copybot_hot::order::json_body(&ord, &sig, &cr.key, "FAK");
                    let Ok(body_s) = serde_json::to_string(&body) else { continue };
                    let hdrs = match copybot_hot::auth::l2_headers(
                        &sa4,
                        cr,
                        copybot_hot::auth::now_secs(),
                        "POST",
                        "/order",
                        Some(&body_s),
                    ) {
                        Ok(h) => h,
                        Err(_) => continue,
                    };
                    let flat_hash = hex::encode(ord.digest());
                    let (outcome, _) = match submit_tracked(
                            &pend4,
                            &e4,
                            &copybot_hot::provenance::Provenance::own(
                                copybot_hot::provenance::Origin::Flatten {
                                    mode: mode.as_str().to_string(),
                                },
                                &lane.cfg.name,
                                &tok,
                                1,
                                shares,
                                limit,
                                *sz,
                            ),
                            flat_hash.clone(),
                            false,
                            &paths4,
                            &format!("{clob4}/order"),
                            &body_s,
                            &hdrs,
                            Duration::from_secs(10),
                            false,
                        )
                        .await
                    {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!(
                                "[{}] flatten REFUSING TO SUBMIT: {e}", lane.cfg.name
                            );
                            continue;
                        }
                    };
                    match &outcome {
                        copybot_hot::race_send::Outcome::Matched { body, .. } => {
                            book_and_resolve(
                                &c4,
                                &pend4,
                                &r4,
                                &inc4,
                                &e4,
                                &lane.cfg.name,
                                tok,
                                1,
                                limit,
                                body,
                                None,
                                &flat_hash,
                                false,
                            );
                            let _ = wake4.send(tok.clone());
                            sold += 1;
                        }
                        copybot_hot::race_send::Outcome::Rejected { body } => {
                            rescue_exit(
                                    body,
                                    ord,
                                    pk4,
                                    &cr.key,
                                    "FAK",
                                    shares,
                                    *sz,
                                    limit,
                                    &paths4,
                                    &clob4,
                                    creds4.as_ref().as_ref(),
                                    &sa4,
                                    &lane.cfg.name,
                                    tok,
                                    &c4,
                                    &r4,
                                    &e4,
                                    &wake4,
                                    &pend4,
                                    &inc4,
                                )
                                .await;
                            sold += 1;
                        }
                        _ => {}
                    }
                }
                e4.emit(
                    serde_json::json!(
                        { "t" : now_ms(), "ev" : "flatten_done", "lane" : lane.cfg.name,
                        "mode" : mode.as_str(), "positions_sold" : sold }
                    ),
                );
            }
        });
    }
    const MERGE_MATERIAL_USD: f64 = 1.0;
    if live {
        let execute = std::env::var("MERGE_RECONCILE").ok().as_deref() == Some("1");
        eprintln!(
            "[merge] reconciliation task up — execute={execute} (set MERGE_RECONCILE=1 to act)"
        );
        let pend5 = pending_log.clone();
        let inc5 = incidents.clone();
        let mc5 = merge_claims.clone();
        let rpc_url5 = std::env::var("FILLWATCH_RPC")
            .unwrap_or_else(|_| copybot_hot::txsend::DEFAULT_RPC.to_string());
        let chain_id5: u64 = std::env::var("CHAIN_ID")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(137);
        let safe20_5 = {
            let v = hex::decode(funder.trim_start_matches("0x")).unwrap_or_default();
            let mut a = [0u8; 20];
            if v.len() == 20 {
                a.copy_from_slice(&v);
            }
            a
        };
        let mut merge_enabled5 = std::env::var("COPYBOT_MERGE_ENABLE").ok().as_deref()
            == Some("1");
        if merge_enabled5 && safe20_5 == [0u8; 20] {
            eprintln!(
                "[merge] ⛔ CRITICAL: COPYBOT_MERGE_ENABLE=1 but FUNDER_ADDRESS is not a \
readable 20-byte address — merging is DISABLED and pairs will be flattened by selling BOTH \
legs, paying the book twice."
            );
            merge_enabled5 = false;
        }
        let merge_enabled5 = merge_enabled5;
        eprintln!(
            "[merge] on-chain merging enabled={merge_enabled5} \
(set COPYBOT_MERGE_ENABLE=1 to merge instead of selling both legs)"
        );
        let (c5, r5, e5, f5, pk5, sa5, st5, clob5, creds5, paths5, wake5) = (
            control.clone(),
            router.clone(),
            emitter.clone(),
            funder.clone(),
            pk,
            signer_addr.clone(),
            sig_type,
            clob.clone(),
            creds.clone(),
            order_paths.clone(),
            sweep_tx.clone(),
        );
        tokio::spawn(async move {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default();
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            let mut cond_outcomes: std::collections::HashMap<String, usize> = Default::default();
            loop {
                tick.tick().await;
                let ours_snap = copybot_hot::positions::fetch(
                        &http,
                        "https://data-api.polymarket.com",
                        &f5,
                        "0.0001",
                        "",
                        copybot_hot::ledger::now_secs(),
                    )
                    .await;
                if !ours_snap.may_act_destructively() {
                    continue;
                }
                let ours: Vec<serde_json::Value> = ours_snap.rows;
                let mut tok_cond = std::collections::HashMap::new();
                for p in &ours {
                    if let (Some(a), Some(c)) = (
                        p["asset"].as_str(),
                        p["conditionId"].as_str(),
                    ) {
                        tok_cond.insert(a.to_string(), c.to_string());
                    }
                }
                let phys_of = |rows: &[serde_json::Value], tok: &str| {
                    rows
                        .iter()
                        .find(|p| p["asset"].as_str() == Some(tok))
                        .and_then(|p| {
                            p["size"]
                                .as_f64()
                                .or_else(|| p["size"].as_str().and_then(|x| x.parse().ok()))
                        })
                        .unwrap_or(0.0)
                };
                let mark_of = |tok: &str| {
                    ours
                        .iter()
                        .find(|p| p["asset"].as_str() == Some(tok))
                        .and_then(|p| p["curPrice"].as_f64())
                        .unwrap_or(0.0)
                };
                let neg_of = |tok: &str| {
                    ours
                        .iter()
                        .find(|p| p["asset"].as_str() == Some(tok))
                        .and_then(|p| p["negativeRisk"].as_bool())
                        .unwrap_or(false)
                };
                for lane in r5.snapshot().iter() {
                    if !lane.state.armed.load(Ordering::Relaxed)
                        || lane.state.retired.load(Ordering::Relaxed)
                    {
                        continue;
                    }
                    let leader = format!("0x{}", hex::encode(lane.cfg.wallet20));
                    let his_snap = copybot_hot::positions::fetch(
                            &http,
                            "https://data-api.polymarket.com",
                            &leader,
                            "0.0001",
                            "",
                            copybot_hot::ledger::now_secs(),
                        )
                        .await;
                    if !his_snap.may_act_destructively() {
                        continue;
                    }
                    let his: Vec<serde_json::Value> = his_snap.rows;
                    if his.is_empty() {
                        let s2 = copybot_hot::positions::fetch(
                                &http,
                                "https://data-api.polymarket.com",
                                &leader,
                                "0.0001",
                                "",
                                copybot_hot::ledger::now_secs(),
                            )
                            .await;
                        let second = if s2.completeness.is_complete() {
                            copybot_hot::snapshot::Snapshot::Data(s2.len())
                        } else {
                            copybot_hot::snapshot::Snapshot::Failed(
                                s2.completeness.reason(),
                            )
                        };
                        let snaps = [copybot_hot::snapshot::Snapshot::Data(0), second];
                        if copybot_hot::snapshot::judge_empty(&snaps, 2)
                            != copybot_hot::snapshot::Verdict::Believable
                        {
                            eprintln!(
                                "[merge] {}: leader looked flat but a second read \
disagreed — skipping rather than liquidating",
                                lane.cfg.name
                            );
                            continue;
                        }
                    }
                    let his_conds: std::collections::HashSet<String> = his
                        .iter()
                        .filter_map(|p| p["conditionId"].as_str().map(|s| s.to_string()))
                        .collect();
                    let held: Vec<(String, f64)> = c5
                        .lock()
                        .unwrap()
                        .ledger
                        .holdings(&lane.cfg.name)
                        .into_iter()
                        .collect();
                    {
                        let mut candidates: std::collections::HashMap<&str, usize> = Default::default();
                        for (tok, sh) in &held {
                            if *sh <= 1e-6 {
                                continue;
                            }
                            if let Some(cid) = tok_cond.get(tok) {
                                *candidates.entry(cid.as_str()).or_default() += 1;
                            }
                        }
                        for (cid, n) in candidates {
                            if n != 2 || his_conds.contains(cid) {
                                continue;
                            }
                            if cond_outcomes.contains_key(cid) {
                                continue;
                            }
                            let murl = format!(
                                "https://clob.polymarket.com/markets/{cid}"
                            );
                            let count = match http.get(&murl).send().await {
                                Ok(r) if r.status().is_success() => {
                                    r.json::<serde_json::Value>()
                                        .await
                                        .ok()
                                        .and_then(|m| m["tokens"].as_array().map(|a| a.len()))
                                }
                                _ => None,
                            };
                            if let Some(c) = count {
                                cond_outcomes.insert(cid.to_string(), c);
                            }
                        }
                    }
                    let orphans = copybot_hot::merge::find_orphans(
                        &held,
                        &tok_cond,
                        |c| !his_conds.contains(c),
                        |c| cond_outcomes.get(c).copied(),
                    );
                    for o in &orphans {
                        let cid_ok = copybot_hot::merge::condition_id_bytes(
                                &o.condition_id,
                            )
                            .map(|cid| {
                                copybot_hot::merge::merge_calldata(
                                        &copybot_hot::merge::USDC,
                                        &cid,
                                        &copybot_hot::merge::BINARY_PARTITION,
                                        (o.mergeable() * 1e6) as u128,
                                    )
                                    .len()
                            })
                            .unwrap_or(0);
                        e5.emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "merge_reconcile", "lane" : lane
                                .cfg.name, "condition" : o.condition_id, "mergeable" : o
                                .mergeable(), "residual" : o.residual().map(| (t, s) |
                                serde_json::json!({ "token" : t, "shares" : s })),
                                "merge_calldata_bytes" : cid_ok, "execute" : execute }
                            ),
                        );
                        if !execute {
                            continue;
                        }
                        let (Some(cr), Some(key)) = (creds5.as_ref(), pk5.as_ref()) else {
                            continue
                        };
                        let legs = [o.token_a.as_str(), o.token_b.as_str()];
                        let now_s = now_ms() as u64 / 1000;
                        let claimed = mc5
                            .lock()
                            .map(|mut g| g.claim(&lane.cfg.name, &legs, now_s))
                            .unwrap_or(false);
                        let pairs = o.mergeable();
                        if !claimed {
                            e5.emit(
                                serde_json::json!(
                                    { "t" : now_ms(), "ev" : "merge_skip", "lane" : lane.cfg
                                    .name, "condition" : o.condition_id, "reason" :
                                    "legs_already_claimed" }
                                ),
                            );
                            continue;
                        }
                        if !merge_enabled5 {
                            if let Ok(mut g) = mc5.lock() {
                                g.release(&lane.cfg.name, &legs);
                            }
                            e5.emit(
                                serde_json::json!(
                                    { "t" : now_ms(), "ev" : "merge_skip", "lane" : lane.cfg
                                    .name, "condition" : o.condition_id, "reason" :
                                    "COPYBOT_MERGE_ENABLE not set — selling both legs" }
                                ),
                            );
                        }
                        let before_a = phys_of(&ours, &o.token_a);
                        let before_b = phys_of(&ours, &o.token_b);
                        let outcome = if !merge_enabled5 {
                            MergeOutcome::DidNotHappen
                        } else {
                            let neg = neg_of(&o.token_a);
                            let r = run_merge_native(
                                    &http,
                                    &rpc_url5,
                                    key,
                                    &safe20_5,
                                    &copybot_hot::merge::merge_adapter_for(neg),
                                    &o.condition_id,
                                    pairs,
                                    chain_id5,
                                )
                                .await;
                            match r {
                                Ok(tx) => {
                                    let (ma, mb) = (mark_of(&o.token_a), mark_of(&o.token_b));
                                    let booked = c5
                                        .lock()
                                        .ok()
                                        .and_then(|mut g| {
                                            g
                                                .ledger
                                                .book_merge(
                                                    &lane.cfg.name,
                                                    &o.token_a,
                                                    &o.token_b,
                                                    pairs,
                                                    ma,
                                                    mb,
                                                    &tx,
                                                )
                                        })
                                        .is_some();
                                    e5.emit(
                                        serde_json::json!(
                                            { "t" : now_ms(), "ev" : "merge_executed", "lane" : lane.cfg
                                            .name, "condition" : o.condition_id, "pairs" : pairs, "tx" :
                                            tx, "booked" : booked, "neg_risk" : neg }
                                        ),
                                    );
                                    if booked {
                                        MergeOutcome::Merged
                                    } else {
                                        e5.emit(
                                            serde_json::json!(
                                                { "t" : now_ms(), "ev" : "merge_unbooked", "lane" : lane.cfg
                                                .name, "condition" : o.condition_id, "tx" : tx, "pairs" :
                                                pairs, "severity" : "critical", "why" :
                                                "merge confirmed on chain but the ledger \
refused the booking — holding the pair; do NOT sell, the shares are already gone"
                                                }
                                            ),
                                        );
                                        MergeOutcome::Unresolved
                                    }
                                }
                                Err(why) => {
                                    let unknown = why.contains("UNKNOWN");
                                    e5.emit(
                                        serde_json::json!(
                                            { "t" : now_ms(), "ev" : "merge_failed", "lane" : lane.cfg
                                            .name, "condition" : o.condition_id, "pairs" : pairs, "why"
                                            : why, "outcome_unknown" : unknown, "severity" : "warn" }
                                        ),
                                    );
                                    if !unknown {
                                        MergeOutcome::DidNotHappen
                                    } else {
                                        let after = copybot_hot::positions::fetch(
                                                &http,
                                                "https://data-api.polymarket.com",
                                                &f5,
                                                "0.0001",
                                                "",
                                                copybot_hot::ledger::now_secs(),
                                            )
                                            .await;
                                        if !after.may_act_destructively() {
                                            e5.emit(
                                                serde_json::json!(
                                                    { "t" : now_ms(), "ev" : "merge_unresolved", "lane" : lane
                                                    .cfg.name, "condition" : o.condition_id, "pairs" : pairs,
                                                    "severity" : "critical", "why" :
                                                    "merge outcome UNKNOWN and the position \
read was incomplete — cannot tell whether it landed; holding the matched pair"
                                                    }
                                                ),
                                            );
                                            MergeOutcome::Unresolved
                                        } else {
                                            let aa = phys_of(&after.rows, &o.token_a);
                                            let ab = phys_of(&after.rows, &o.token_b);
                                            let v = copybot_hot::mergeverify::confirm(
                                                before_a,
                                                before_b,
                                                aa,
                                                ab,
                                                pairs,
                                            );
                                            e5.emit(
                                                serde_json::json!(
                                                    { "t" : now_ms(), "ev" : "merge_resolved", "lane" : lane.cfg
                                                    .name, "condition" : o.condition_id, "pairs" : pairs,
                                                    "verdict" : format!("{v:?}"), "before" : [before_a,
                                                    before_b], "after" : [aa, ab] }
                                                ),
                                            );
                                            match v {
                                                copybot_hot::mergeverify::Confirmation::Confirmed {
                                                    ..
                                                } => {
                                                    let (ma, mb) = (mark_of(&o.token_a), mark_of(&o.token_b));
                                                    let key = format!(
                                                        "mergeresolve:{}:{}", o.condition_id, pairs
                                                    );
                                                    let booked = c5
                                                        .lock()
                                                        .ok()
                                                        .and_then(|mut g| {
                                                            g
                                                                .ledger
                                                                .book_merge(
                                                                    &lane.cfg.name,
                                                                    &o.token_a,
                                                                    &o.token_b,
                                                                    pairs,
                                                                    ma,
                                                                    mb,
                                                                    &key,
                                                                )
                                                        })
                                                        .is_some();
                                                    if booked {
                                                        MergeOutcome::Merged
                                                    } else {
                                                        MergeOutcome::Unresolved
                                                    }
                                                }
                                                copybot_hot::mergeverify::Confirmation::NotApplied => {
                                                    MergeOutcome::DidNotHappen
                                                }
                                                copybot_hot::mergeverify::Confirmation::Inconsistent {
                                                    why,
                                                } => {
                                                    e5.emit(
                                                        serde_json::json!(
                                                            { "t" : now_ms(), "ev" : "merge_inconsistent", "lane" : lane
                                                            .cfg.name, "condition" : o.condition_id, "severity" :
                                                            "critical", "why" : why }
                                                        ),
                                                    );
                                                    MergeOutcome::Unresolved
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        };
                        let mut to_sell: Vec<String> = o
                            .residual()
                            .map(|(t, _)| t.to_string())
                            .into_iter()
                            .collect();
                        if outcome == MergeOutcome::DidNotHappen {
                            to_sell = legs.iter().map(|t| t.to_string()).collect();
                        }
                        if outcome == MergeOutcome::Unresolved && !to_sell.is_empty() {
                            e5.emit(
                                serde_json::json!(
                                    { "t" : now_ms(), "ev" : "merge_hold_pair", "lane" : lane
                                    .cfg.name, "condition" : o.condition_id, "holding_pairs" :
                                    pairs, "flattening_residual" : to_sell, "why" :
                                    "the matched pair is inert and is held; the residual is \
naked exposure and is flattened regardless of the merge outcome"
                                    }
                                ),
                            );
                        }
                        for tok in to_sell.iter().map(|t| t.as_str()) {
                            let sz = c5
                                .lock()
                                .unwrap()
                                .ledger
                                .holdings(&lane.cfg.name)
                                .get(tok)
                                .copied()
                                .unwrap_or(0.0);
                            if sz <= 1e-9 {
                                continue;
                            }
                            let m = mark_of(tok);
                            let px = if m > 0.0 { m } else { 0.02 };
                            let (shares, limit) = if sz * px >= MERGE_MATERIAL_USD {
                                copybot_hot::venue::cleanup_fak_terms(sz, px)
                            } else {
                                copybot_hot::venue::dust_fak_terms(sz, px)
                            };
                            if shares <= 0.0 {
                                continue;
                            }
                            let (ma, ta) = copybot_hot::order::amounts(limit, shares, 1);
                            let ord = copybot_hot::order::Order {
                                salt: copybot_hot::order::safe_salt(now_ms()),
                                maker: f5.clone(),
                                signer: sa5.clone(),
                                token_id: tok.to_string(),
                                maker_amount: ma,
                                taker_amount: ta,
                                side: 1,
                                signature_type: st5,
                                timestamp: now_ms(),
                                neg_risk: neg_of(tok),
                            };
                            let sig = ord.sign_for_type(key);
                            let body = copybot_hot::order::json_body(
                                &ord,
                                &sig,
                                &cr.key,
                                "FAK",
                            );
                            let Ok(bs) = serde_json::to_string(&body) else { continue };
                            let hdrs = match copybot_hot::auth::l2_headers(
                                &sa5,
                                cr,
                                copybot_hot::auth::now_secs(),
                                "POST",
                                "/order",
                                Some(&bs),
                            ) {
                                Ok(h) => h,
                                Err(_) => continue,
                            };
                            let m_hash = hex::encode(ord.digest());
                            let (outcome, _) = match submit_tracked(
                                    &pend5,
                                    &e5,
                                    &copybot_hot::provenance::Provenance::own(
                                        copybot_hot::provenance::Origin::MergeReconcile,
                                        &lane.cfg.name,
                                        tok,
                                        1,
                                        shares,
                                        limit,
                                        sz,
                                    ),
                                    m_hash.clone(),
                                    false,
                                    &paths5,
                                    &format!("{clob5}/order"),
                                    &bs,
                                    &hdrs,
                                    Duration::from_secs(10),
                                    false,
                                )
                                .await
                            {
                                Ok(v) => v,
                                Err(e) => {
                                    eprintln!(
                                        "[{}] merge REFUSING TO SUBMIT: {e}", lane.cfg.name
                                    );
                                    continue;
                                }
                            };
                            match &outcome {
                                copybot_hot::race_send::Outcome::Matched { body, .. } => {
                                    book_and_resolve(
                                        &c5,
                                        &pend5,
                                        &r5,
                                        &inc5,
                                        &e5,
                                        &lane.cfg.name,
                                        tok,
                                        1,
                                        limit,
                                        body,
                                        None,
                                        &m_hash,
                                        false,
                                    );
                                    let _ = wake5.send(tok.to_string());
                                }
                                copybot_hot::race_send::Outcome::Rejected { body } => {
                                    rescue_exit(
                                            body,
                                            ord,
                                            pk5,
                                            &cr.key,
                                            "FAK",
                                            shares,
                                            sz,
                                            limit,
                                            &paths5,
                                            &clob5,
                                            creds5.as_ref().as_ref(),
                                            &sa5,
                                            &lane.cfg.name,
                                            tok,
                                            &c5,
                                            &r5,
                                            &e5,
                                            &wake5,
                                            &pend5,
                                            &inc5,
                                        )
                                        .await;
                                }
                                _ => {}
                            }
                        }
                        if let Ok(mut g) = mc5.lock() {
                            g.release(&lane.cfg.name, &legs);
                        }
                        e5.emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "merge_reconcile_done", "lane" :
                                lane.cfg.name, "condition" : o.condition_id }
                            ),
                        );
                    }
                }
            }
        });
    }
    let latency = Arc::new(copybot_hot::latency::Latency::new());
    tokio::spawn(
        copybot_hot::latency::spawn_baseline_probe(
            latency.clone(),
            http.clone(),
            clob.clone(),
            Duration::from_secs(30),
        ),
    );
    {
        let (l2, e3) = (latency.clone(), emitter.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let snap = l2.snapshot();
                if snap["series"].as_object().map(|m| m.is_empty()).unwrap_or(true) {
                    continue;
                }
                e3.emit(
                    serde_json::json!(
                        { "t" : now_ms(), "ev" : "latency", "lat" : snap, "ledger" : e3
                        .ledger_status() }
                    ),
                );
            }
        });
    }
    if live {
        let fire = std::env::var("ORPHAN_SWEEP").ok().as_deref() == Some("1");
        eprintln!(
            "[orphan] stranded-position sweep up — fire={fire} \
                   (set ORPHAN_SWEEP=1 to act)"
        );
        let (c6, r6, e6, f6, pk6, sa6, st6, clob6, creds6) = (
            control.clone(),
            router.clone(),
            emitter.clone(),
            funder.clone(),
            pk,
            signer_addr.clone(),
            sig_type,
            clob.clone(),
            creds.clone(),
        );
        let user6 = funder.clone();
        let pend6 = pending_log.clone();
        let paths6 = order_paths.clone();
        tokio::spawn(async move {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default();
            let mut orphan_said: std::collections::HashMap<
                (String, String),
                (u64, u64),
            > = std::collections::HashMap::new();
            const ORPHAN_RESAY_SECS: u64 = 3600;
            let mut tick = tokio::time::interval(Duration::from_secs(90));
            loop {
                tick.tick().await;
                let mut rows: Vec<copybot_hot::orphan::Holding> = Vec::new();
                for lane in r6.snapshot().iter() {
                    if !lane.state.armed.load(Ordering::Relaxed) {
                        continue;
                    }
                    let holds = c6.lock().unwrap().ledger.holdings(&lane.cfg.name);
                    for (tok, ours) in holds.iter() {
                        if *ours <= 1e-9 {
                            continue;
                        }
                        rows.push(copybot_hot::orphan::Holding {
                            lane: lane.cfg.name.clone(),
                            token: tok.clone(),
                            ours: *ours,
                            his: *lane.his_pos.lock().unwrap().get(tok).unwrap_or(&0.0),
                            reserved: lane.sell_reserved(tok),
                            legacy: *lane.legacy.lock().unwrap().get(tok).unwrap_or(&0.0),
                        });
                    }
                }
                let plan = copybot_hot::orphan::plan(
                    &rows,
                    copybot_hot::orphan::MIN_SHARES,
                );
                let list = match plan {
                    copybot_hot::orphan::Sweep::Idle => continue,
                    copybot_hot::orphan::Sweep::Refused {
                        why,
                        would_have_fired,
                        held,
                    } => {
                        eprintln!("[orphan] REFUSED: {why}");
                        e6.emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "orphan_refused", "why" : why,
                                "would_have_fired" : would_have_fired, "held" : held }
                            ),
                        );
                        continue;
                    }
                    copybot_hot::orphan::Sweep::Fire(v) => v,
                };
                let snap = copybot_hot::positions::fetch(
                        &http,
                        "https://data-api.polymarket.com",
                        &user6,
                        "0.0001",
                        "",
                        copybot_hot::ledger::now_secs(),
                    )
                    .await;
                if !snap.may_act_destructively() {
                    eprintln!(
                        "[orphan] positions read incomplete ({}) — skipping this pass",
                        snap.completeness.reason()
                    );
                    continue;
                }
                let mark_of = |tok: &str| {
                    snap
                        .rows
                        .iter()
                        .find(|p| p["asset"].as_str() == Some(tok))
                        .and_then(|p| p["curPrice"].as_f64())
                        .unwrap_or(0.0)
                };
                let neg_of = |tok: &str| {
                    snap
                        .rows
                        .iter()
                        .find(|p| p["asset"].as_str() == Some(tok))
                        .and_then(|p| p["negativeRisk"].as_bool())
                        .unwrap_or(false)
                };
                for o in list {
                    let sz = o.strandable();
                    let m = mark_of(&o.token);
                    let px = if m > 0.0 { m } else { 0.02 };
                    let (shares, limit) = copybot_hot::venue::cleanup_fak_terms(sz, px);
                    if shares <= 0.0 {
                        continue;
                    }
                    let said_key = (o.lane.clone(), o.token.clone());
                    let now_s = now_ms() as u64 / 1000;
                    let say = match orphan_said.get_mut(&said_key) {
                        None => {
                            orphan_said.insert(said_key.clone(), (now_s, 0));
                            Some(0)
                        }
                        Some(
                            (last, held_back),
                        ) if now_s.saturating_sub(*last) >= ORPHAN_RESAY_SECS => {
                            let n = *held_back;
                            *last = now_s;
                            *held_back = 0;
                            Some(n)
                        }
                        Some((_, held_back)) => {
                            *held_back += 1;
                            None
                        }
                    };
                    if let Some(suppressed) = say {
                        eprintln!(
                            "[orphan] {} holds {shares:.4} sh of …{} that he has fully \
                                   left — {} at {limit:.4} (mark {m:.4})",
                            o.lane, & o.token[o.token.len().saturating_sub(8)..], if fire
                            { "EXITING" } else { "would exit" }
                        );
                        e6.emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "orphan_exit", "lane" : o.lane,
                                "tok" : & o.token[..o.token.len().min(14)], "shares" :
                                shares, "limit" : limit, "mark" : m, "ours" : o.ours,
                                "reserved" : o.reserved, "fired" : fire, "suppressed_passes"
                                : suppressed, "unsellable" : o.ours * m < 1.0 }
                            ),
                        );
                    }
                    if !fire {
                        continue;
                    }
                    let (Some(cr), Some(key)) = (creds6.as_ref().as_ref(), pk6.as_ref())
                    else { continue };
                    let (ma, ta) = copybot_hot::order::amounts(limit, shares, 1);
                    let ord = copybot_hot::order::Order {
                        salt: copybot_hot::order::safe_salt(now_ms()),
                        maker: f6.clone(),
                        signer: sa6.clone(),
                        token_id: o.token.clone(),
                        maker_amount: ma,
                        taker_amount: ta,
                        side: 1,
                        signature_type: st6,
                        timestamp: now_ms(),
                        neg_risk: neg_of(&o.token),
                    };
                    let sig = ord.sign_for_type(key);
                    let body = copybot_hot::order::json_body(&ord, &sig, &cr.key, "FAK");
                    let Ok(bs) = serde_json::to_string(&body) else { continue };
                    let Ok(hdrs) = copybot_hot::auth::l2_headers(
                        &sa6,
                        cr,
                        copybot_hot::ledger::now_secs() as u64,
                        "POST",
                        "/order",
                        Some(&bs),
                    ) else { continue };
                    let sweep_hash = hex::encode(ord.digest());
                    let hdr_vec: Vec<(&'static str, String)> = hdrs
                        .into_iter()
                        .collect();
                    let outcome = match submit_tracked(
                            &pend6,
                            &e6,
                            &copybot_hot::provenance::Provenance::mirrored(
                                copybot_hot::provenance::Origin::OrphanSweep,
                                &o.lane,
                                &o.token,
                                1,
                                shares,
                                limit,
                                o.ours,
                                Some(o.his),
                                None,
                                None,
                                None,
                            ),
                            sweep_hash,
                            false,
                            &paths6,
                            &format!("{clob6}/order"),
                            &bs,
                            &hdr_vec,
                            Duration::from_secs(10),
                            false,
                        )
                        .await
                    {
                        Ok((outcome, _)) => outcome,
                        Err(e) => {
                            eprintln!("[orphan] REFUSING TO SUBMIT: {e}");
                            continue;
                        }
                    };
                    e6.emit(
                        serde_json::json!(
                            { "t" : now_ms(), "ev" : "orphan_exit_resp", "lane" : o.lane,
                            "outcome" : format!("{outcome:?}") }
                        ),
                    );
                }
            }
        });
    }
    if live {
        let (reg, rt, ctl) = (registry.clone(), router.clone(), control.clone());
        let phys = physical_wallet.clone();
        let ev_rt = emitter.clone();
        let watched_rt = watched_addrs.clone();
        let pl_rt = pair_ledger.clone();
        tokio::spawn(async move {
            let http_rt = reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_default();
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            let mut seed_retry: std::collections::HashMap<
                String,
                (u32, std::time::Instant),
            > = std::collections::HashMap::new();
            loop {
                tick.tick().await;
                if reg.reload() {
                    if let Err(e) = reg.mint_missing_ids() {
                        eprintln!(
                            "[laneid] ⚠️ could not persist a new lane's id: {e}"
                        );
                    }
                    ctl.lock().unwrap().ledger.lane_ids = reg.id_map();
                }
                let pool = reg.specs();
                watched_rt.set(pool.iter().map(|w| w.leader.clone()));
                let snap = rt.snapshot();
                let live_names: Vec<String> = snap
                    .iter()
                    .map(|l| l.cfg.name.clone())
                    .collect();
                let retired_names: Vec<String> = snap
                    .iter()
                    .filter(|l| l.state.retired.load(Ordering::Relaxed))
                    .map(|l| l.cfg.name.clone())
                    .collect();
                let plan = copybot_hot::wallets::reconcile(
                    &pool,
                    &live_names,
                    &retired_names,
                );
                let applied: Vec<(String, f64, f64, f64, bool, bool, bool, f64, f64)> = rt
                    .snapshot()
                    .iter()
                    .map(|l| {
                        let p = l.policy();
                        let pct = match p.sizing {
                            copybot_hot::lanes::Sizing::Pct(v) => v,
                            _ => f64::NAN,
                        };
                        (
                            l.cfg.name.clone(),
                            pct,
                            p.seed_usd,
                            p.max_effective_pct,
                            p.compound,
                            l.state.rest_buys.load(Ordering::Relaxed),
                            l.state.rest_sells.load(Ordering::Relaxed),
                            l.cfg.min_buy_price,
                            l.cfg.max_buy_price,
                        )
                    })
                    .collect();
                for rp in copybot_hot::wallets::repricing(&pool, &applied) {
                    let pool_seed: f64 = pool
                        .iter()
                        .filter(|w| w.enabled)
                        .map(|w| w.seed_usd)
                        .sum();
                    let physical = phys.lock().unwrap().equity;
                    if let Err(e) = copybot_hot::budget::seeds_fit_wallet(
                        pool_seed,
                        physical,
                    ) {
                        eprintln!(
                            "[wallets] ⚠️  OVER-COMMITTED, repricing {} anyway: {e}",
                            rp.name
                        );
                    }
                    let Some(lane) = rt
                        .snapshot()
                        .iter()
                        .find(|l| l.cfg.name == rp.name)
                        .cloned() else { continue };
                    let built = copybot_hot::lanes::SizingPolicy::build(
                        0,
                        rp.spec.seed_usd,
                        copybot_hot::lanes::Sizing::Pct(rp.spec.pct),
                        rp.spec.max_effective_pct,
                        rp.spec.compound,
                        &copybot_hot::budget::Fracs::default(),
                    );
                    let hybrid = lane.cfg.execution
                        == copybot_hot::lanes::Execution::Hybrid;
                    let (want_b, want_s) = (
                        rp.spec.copy_makers || hybrid,
                        rp.spec.copy_maker_sells() || hybrid,
                    );
                    let was_b = lane.state.rest_buys.swap(want_b, Ordering::Relaxed);
                    let was_s = lane.state.rest_sells.swap(want_s, Ordering::Relaxed);
                    if was_b != want_b || was_s != want_s {
                        eprintln!(
                            "[wallets] {} resting: buys {was_b}->{want_b} \
                                   sells {was_s}->{want_s}",
                            rp.name
                        );
                        ev_rt
                            .emit(
                                serde_json::json!(
                                    { "t" : now_ms(), "ev" : "rest_switch", "lane" : rp.name,
                                    "rest_buys" : want_b, "rest_sells" : want_s }
                                ),
                            );
                    }
                    match built.and_then(|pol| lane.apply_policy(pol)) {
                        Ok(gen) => {
                            let p = lane.policy();
                            eprintln!(
                                "[wallets] {} REPRICED (gen {gen}): pct {:.3}% ceiling {:.1}% \
                                       seed ${:.0} -> fill ${:.2} / market ${:.0} / open ${:.0} / daily ${:.0}",
                                rp.name, rp.spec.pct * 100.0, rp.spec.max_effective_pct *
                                100.0, p.seed_usd, p.caps.max_usd_per_fill, p.caps
                                .per_market_usd, p.caps.max_open_usd, p.caps.daily_usd
                            );
                            ev_rt
                                .emit(
                                    serde_json::json!(
                                        { "t" : now_ms(), "ev" : "lane_repriced", "lane" : rp.name,
                                        "generation" : gen, "pct" : rp.spec.pct, "max_effective_pct"
                                        : rp.spec.max_effective_pct, "seed_usd" : p.seed_usd, "caps"
                                        : { "fill" : p.caps.max_usd_per_fill, "market" : p.caps
                                        .per_market_usd, "open" : p.caps.max_open_usd, "daily" : p
                                        .caps.daily_usd }, }
                                    ),
                                );
                        }
                        Err(e) => eprintln!("[wallets] {} reprice REFUSED: {e}", rp.name),
                    }
                }
                for spec in &plan.to_add {
                    let pool_seed: f64 = pool
                        .iter()
                        .filter(|w| w.enabled)
                        .map(|w| w.seed_usd)
                        .sum();
                    let physical = phys.lock().unwrap().equity;
                    if let Err(e) = copybot_hot::budget::seeds_fit_wallet(
                        pool_seed,
                        physical,
                    ) {
                        eprintln!(
                            "[wallets] ⚠️  OVER-COMMITTED, loading {} anyway: {e}",
                            spec.name
                        );
                    }
                    match copybot_hot::config::build_runtime_lane(spec) {
                        Ok((lane, risk)) => {
                            ctl.lock().unwrap().ledger.ensure_lane(&spec.name, risk);
                            let ix = rt.push(lane);
                            let Some(placed) = rt.lane(ix) else { continue };
                            match reactivate_lane(&placed, &ctl, &http_rt).await {
                                Ok(seeded) => {
                                    placed.mark_ready();
                                    let brk = if spec.risk.is_none() {
                                        format!(
                                            "risk: DEFAULT drawdown stop ${:.0} \
                                                 (15% of seed; set a `risk` block to override)",
                                            risk.max_drawdown_usd
                                        )
                                    } else {
                                        format!(
                                            "risk: dd ${:.0}/{:.1}% losses {} open {}", risk
                                            .max_drawdown_usd, risk.max_drawdown_pct * 100.0, risk
                                            .max_consecutive_losses, risk.max_open_positions
                                        )
                                    };
                                    eprintln!(
                                        "[wallets] LOADED {} (lane {ix}) seed ${:.0} \
pct {:.3}% {brk} — leader book seeded ({seeded} positions); disarmed, arm it in the operator file",
                                        spec.name, spec.seed_usd, spec.pct * 100.0
                                    );
                                }
                                Err(e) => {
                                    eprintln!(
                                        "[wallets] {} loaded but NOT READY: {e} \
(will retry; it cannot trade until its leader book is seeded)",
                                        spec.name
                                    )
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("[wallets] refused to load {}: {e}", spec.name)
                        }
                    }
                }
                for l in rt.snapshot().iter() {
                    if l.state.retired.load(Ordering::Relaxed) {
                        continue;
                    }
                    let leader_hex = format!("0x{}", hex::encode(l.cfg.wallet20));
                    if pl_rt.lock().map(|g| g.is_seeded(&leader_hex)).unwrap_or(true) {
                        continue;
                    }
                    if let Some((_, next)) = seed_retry.get(&leader_hex) {
                        if std::time::Instant::now() < *next {
                            continue;
                        }
                    }
                    match seed_pair_ledger(&http_rt, &leader_hex, &pl_rt).await {
                        Ok((rows, conds)) => {
                            seed_retry.remove(&leader_hex);
                            eprintln!(
                                "[pairledger] {} seeded {rows} split/merge row(s), {conds} \
condition(s) carrying a balance",
                                l.cfg.name
                            );
                        }
                        Err(e) => {
                            let n = seed_retry
                                .get(&leader_hex)
                                .map(|(n, _)| *n)
                                .unwrap_or(0) + 1;
                            let wait = Duration::from_secs(
                                30u64.saturating_mul(1u64 << n.min(4)).min(600),
                            );
                            seed_retry
                                .insert(
                                    leader_hex.clone(),
                                    (n, std::time::Instant::now() + wait),
                                );
                            eprintln!(
                                "[pairledger] ⚠️  {} could NOT seed ({e}) — split balances start \
EMPTY, so an income withdrawal would read as an EXIT. Merge handling is observe-only, so \
nothing acts on it; retry #{n} in {}s.",
                                l.cfg.name, wait.as_secs()
                            );
                        }
                    }
                }
                for l in rt.snapshot().iter() {
                    if l.state.ready.load(Ordering::Relaxed)
                        || l.state.retired.load(Ordering::Relaxed)
                    {
                        continue;
                    }
                    if !pool.iter().any(|w| w.enabled && w.name == l.cfg.name) {
                        continue;
                    }
                    if let Ok(seeded) = reactivate_lane(l, &ctl, &http_rt).await {
                        l.mark_ready();
                        eprintln!(
                            "[wallets] {} became READY on retry ({seeded} leader \
positions)",
                            l.cfg.name
                        );
                    }
                }
                for spec in &plan.to_resume {
                    let Some(lane) = rt
                        .snapshot()
                        .iter()
                        .find(|l| l.cfg.name == spec.name)
                        .cloned() else { continue };
                    lane.mark_activating();
                    match reactivate_lane(&lane, &ctl, &http_rt).await {
                        Ok(seeded) => {
                            lane.state.retired.store(false, Ordering::Relaxed);
                            lane.mark_ready();
                            eprintln!(
                                "[wallets] RESUMED {} — leader book re-seeded ({seeded} \
positions); still disarmed, arm it in the operator file",
                                spec.name
                            );
                        }
                        Err(e) => {
                            eprintln!(
                                "[wallets] refused to resume {}: {e} \
(stays retired; will retry)",
                                spec.name
                            )
                        }
                    }
                }
                for name in &plan.to_retire {
                    if let Some(lane) = rt
                        .snapshot()
                        .iter()
                        .find(|l| &l.cfg.name == name)
                    {
                        if !lane.state.retired.swap(true, Ordering::Relaxed) {
                            eprintln!(
                                "[wallets] RETIRED {name} — stops buying, drains its \
                                       book, then sits inert"
                            );
                        }
                    }
                }
            }
        });
    }
    if live {
        let (rb_r, http_r, clob_r, addr_r, creds_r, e_r, sg_r, rt_r, ctl_r) = (
            resting_book.clone(),
            http.clone(),
            clob.clone(),
            auth_addr.clone(),
            creds.clone(),
            emitter.clone(),
            signal_guard.clone(),
            router.clone(),
            control.clone(),
        );
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            let mut last_unattributable = 0usize;
            loop {
                tick.tick().await;
                let Some(cr) = creds_r.as_ref().as_ref() else { continue };
                let path = "/data/orders";
                let Ok(h) = copybot_hot::auth::l2_headers(
                    &addr_r,
                    cr,
                    copybot_hot::auth::now_secs(),
                    "GET",
                    path,
                    None,
                ) else { continue };
                let mut rq = http_r.get(format!("{clob_r}{path}"));
                for (k, v) in &h {
                    rq = rq.header(*k, v);
                }
                let Ok(resp) = rq.send().await else { continue };
                if !resp.status().is_success() {
                    continue;
                }
                let Ok(v) = resp.json::<serde_json::Value>().await else { continue };
                let Some(rows) = v.as_array() else { continue };
                if rows.is_empty() && !rb_r.is_empty() {
                    e_r.emit(
                        serde_json::json!(
                            { "t" : now_ms() as i64, "ev" : "open_orders_reconcile",
                            "skipped" :
                            "empty venue list \
while we hold rests — refusing to prune on an ambiguous read",
                            "ours" : rb_r.len() }
                        ),
                    );
                    continue;
                }
                let venue_ids: std::collections::HashSet<String> = rows
                    .iter()
                    .filter_map(|r| r["id"].as_str().or_else(|| r["orderID"].as_str()))
                    .map(|i| i.trim_start_matches("0x").to_string())
                    .collect();
                let missing: Vec<String> = rb_r
                    .all()
                    .into_iter()
                    .filter(|o| !venue_ids.contains(o.order_id.trim_start_matches("0x")))
                    .map(|o| o.order_id)
                    .collect();
                let mut pruned = 0usize;
                if !missing.is_empty() && !rb_r.prune_is_plausible(missing.len()) {
                    e_r.emit(
                        serde_json::json!(
                            { "t" : now_ms() as i64, "ev" : "open_orders_reconcile",
                            "skipped" :
                            "too many of our rests are absent at once — treating \
this as a bad read, not a mass cancellation",
                            "missing" : missing.len(), "live" : rb_r.len() }
                        ),
                    );
                } else {
                    for id in &missing {
                        settle_cancelled_rest(
                                &http_r,
                                &clob_r,
                                &addr_r,
                                cr,
                                &rb_r,
                                &sg_r,
                                &e_r,
                                id,
                                "venue_gone",
                            )
                            .await;
                        pruned += 1;
                    }
                }
                let mut adopted = 0usize;
                let mut unattributable = 0usize;
                for r in rows {
                    let Some(id) = r["id"].as_str().or_else(|| r["orderID"].as_str())
                    else { continue };
                    if rb_r.contains(id) {
                        continue;
                    }
                    let Some(tok) = r["asset_id"]
                        .as_str()
                        .or_else(|| r["market"].as_str()) else { continue };
                    let snap = rt_r.snapshot();
                    let owner = rt_r
                        .owner_of(tok)
                        .and_then(|ix| snap.get(ix).map(|l| l.cfg.name.clone()))
                        .or_else(|| {
                            let g = ctl_r.lock().unwrap();
                            let by_held: Vec<String> = snap
                                .iter()
                                .filter(|l| {
                                    g
                                        .ledger
                                        .holdings(&l.cfg.name)
                                        .get(tok)
                                        .copied()
                                        .unwrap_or(0.0) > 1e-9
                                })
                                .map(|l| l.cfg.name.clone())
                                .collect();
                            if by_held.len() == 1 {
                                return Some(by_held[0].clone());
                            }
                            let by_his: Vec<String> = snap
                                .iter()
                                .filter(|l| {
                                    g.his_pos
                                        .get(&l.cfg.name)
                                        .map(|h| h.contains_key(tok))
                                        .unwrap_or(false)
                                })
                                .map(|l| l.cfg.name.clone())
                                .collect();
                            if by_his.len() == 1 {
                                Some(by_his[0].clone())
                            } else {
                                None
                            }
                        });
                    match owner {
                        Some(lane) => {
                            adopted += rb_r.rehydrate(&serde_json::json!([r]), &lane);
                        }
                        None => unattributable += 1,
                    }
                }
                if pruned > 0 || adopted > 0 || unattributable != last_unattributable {
                    e_r.emit(
                        serde_json::json!(
                            { "t" : now_ms() as i64, "ev" : "open_orders_reconcile",
                            "pruned" : pruned, "adopted" : adopted, "unattributable" :
                            unattributable, "live" : rb_r.len() }
                        ),
                    );
                }
                last_unattributable = unattributable;
            }
        });
    }
    if live {
        let (rt_lp, reg_lp, lp) = (router.clone(), registry.clone(), leader_pnl.clone());
        tokio::spawn(async move {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_default();
            let mut tick = tokio::time::interval(Duration::from_secs(900));
            loop {
                tick.tick().await;
                for lane in rt_lp.snapshot().iter() {
                    let name = lane.cfg.name.clone();
                    let Some(spec) = reg_lp.specs().into_iter().find(|w| w.name == name)
                    else { continue };
                    let mut out = serde_json::Map::new();
                    for (win, key) in [("1d", "d1"), ("7d", "d7"), ("30d", "d30")] {
                        let url = format!(
                            "https://lb-api.polymarket.com/profit?window={win}&limit=1&address={}",
                            spec.leader
                        );
                        if let Ok(r) = http.get(&url).send().await {
                            if let Ok(v) = r.json::<serde_json::Value>().await {
                                if let Some(a) = v.as_array().and_then(|a| a.first()) {
                                    if let Some(x) = a["amount"].as_f64() {
                                        out.insert(
                                            key.into(),
                                            serde_json::json!((x * 100.0).round() / 100.0),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    if !out.is_empty() {
                        lp.lock().unwrap().insert(name, serde_json::Value::Object(out));
                    }
                }
            }
        });
    }
    if live {
        let gate = control.lock().unwrap().guardian_stale.clone();
        let gpath = std::path::Path::new(&root.bot.control_path)
            .parent()
            .map(|d| d.join("guardian_state.json"))
            .unwrap_or_else(|| std::path::PathBuf::from("run/guardian_state.json"));
        eprintln!(
            "[guardwatch] watching {} (buys gate after {}s without a completed                    guardian pass; exits are never gated)",
            gpath.display(), copybot_hot::guardwatch::MAX_AGE_SECS
        );
        tokio::spawn(async move {
            let mut w = copybot_hot::guardwatch::Watch::new(
                copybot_hot::ledger::now_secs(),
            );
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            let mut was_gated = false;
            let mut warned_unarmed = false;
            loop {
                tick.tick().await;
                let mtime = std::fs::metadata(&gpath)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64);
                let v = w.observe(mtime, copybot_hot::ledger::now_secs());
                let gated = v.buys_gated();
                gate.store(gated, Ordering::Relaxed);
                if gated != was_gated {
                    eprintln!("[guardwatch] {}", v.describe());
                    was_gated = gated;
                }
                if matches!(v, copybot_hot::guardwatch::Verdict::Warmup { .. })
                    && !warned_unarmed
                {
                    eprintln!("[guardwatch] {}", v.describe());
                    warned_unarmed = true;
                }
            }
        });
    }
    if live {
        let (ctl_ra, rt_ra, em_ra, reg_ra) = (
            control.clone(),
            router.clone(),
            emitter.clone(),
            registry.clone(),
        );
        let pulse_ra = reanchor_pulse.clone();
        tokio::spawn(async move {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default();
            let mut tick = tokio::time::interval(Duration::from_secs(300));
            loop {
                tick.tick().await;
                for lane in rt_ra.snapshot().iter() {
                    if lane.state.retired.load(Ordering::Relaxed) {
                        continue;
                    }
                    let name = lane.cfg.name.clone();
                    let Some(spec) = reg_ra.specs().into_iter().find(|w| w.name == name)
                    else { continue };
                    let snap = copybot_hot::positions::fetch(
                            &http,
                            "https://data-api.polymarket.com",
                            &spec.leader,
                            "0.0001",
                            "",
                            copybot_hot::ledger::now_secs(),
                        )
                        .await;
                    let complete = snap.may_act_destructively();
                    let venue: std::collections::HashMap<String, f64> = snap
                        .rows
                        .iter()
                        .filter_map(|p| {
                            let t = p["asset"].as_str()?.to_string();
                            let sz = p["size"]
                                .as_f64()
                                .or_else(|| {
                                    p["size"].as_str().and_then(|x| x.parse().ok())
                                })?;
                            Some((t, sz))
                        })
                        .collect();
                    let (writes, report) = {
                        let g = ctl_ra.lock().unwrap();
                        let modelled = g.his_pos.get(&name).cloned().unwrap_or_default();
                        let held = g.ledger.holdings(&name);
                        copybot_hot::reanchor::plan(&modelled, &venue, &held, complete)
                    };
                    {
                        let mut g = pulse_ra.lock().unwrap();
                        let e = g.entry(name.clone()).or_insert((0, 0, 0, 0, 0, 0, 0));
                        let last_complete = if complete {
                            now_ms() as i64 / 1000
                        } else {
                            e.1
                        };
                        *e = (
                            now_ms() as i64 / 1000,
                            last_complete,
                            report.raised,
                            report.lowered,
                            report.agreed,
                            report.refused,
                            report.disarmed_zeros,
                        );
                    }
                    if writes.is_empty() {
                        continue;
                    }
                    {
                        let mut g = ctl_ra.lock().unwrap();
                        let hp = g.his_pos.entry(name.clone()).or_default();
                        for (tok, v) in &writes {
                            hp.insert(tok.clone(), *v);
                        }
                    }
                    for (tok, v) in &writes {
                        lane.set_his(tok, *v);
                    }
                    if report.disarmed_zeros > 0 || report.raised > 0
                        || report.lowered > 0
                    {
                        eprintln!(
                            "[reanchor] {name}: raised {} lowered {} agreed {} \
                                   refused {} (disarmed {} modelled-zero position(s))",
                            report.raised, report.lowered, report.agreed, report.refused,
                            report.disarmed_zeros
                        );
                        em_ra
                            .emit(
                                serde_json::json!(
                                    { "t" : now_ms() as i64, "ev" : "his_book_reanchored",
                                    "lane" : name, "raised" : report.raised, "lowered" : report
                                    .lowered, "agreed" : report.agreed, "refused" : report
                                    .refused, "disarmed_zeros" : report.disarmed_zeros,
                                    "shares_raised" : (report.shares_raised * 100.0).round() /
                                    100.0, "complete" : complete }
                                ),
                            );
                    }
                }
            }
        });
    }
    if live {
        let (ctl_ca, rt_ca, em_ca, fund_ca, pend_ca, last_ca) = (
            control.clone(),
            router.clone(),
            emitter.clone(),
            funder.clone(),
            pending_log.clone(),
            custody_report.clone(),
        );
        tokio::spawn(async move {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default();
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            let mut alarm_since: Option<i64> = None;
            loop {
                tick.tick().await;
                let snap = copybot_hot::positions::fetch(
                        &http,
                        "https://data-api.polymarket.com",
                        &fund_ca,
                        "0.0001",
                        "",
                        copybot_hot::ledger::now_secs(),
                    )
                    .await;
                let complete = snap.may_act_destructively();
                let venue: std::collections::HashMap<String, f64> = snap
                    .rows
                    .iter()
                    .filter_map(|p| {
                        let t = p["asset"].as_str()?.to_string();
                        let sz = p["size"]
                            .as_f64()
                            .or_else(|| {
                                p["size"].as_str().and_then(|x| x.parse().ok())
                            })?;
                        Some((t, sz))
                    })
                    .collect();
                let flight = pend_ca.lock().unwrap().in_flight();
                let (in_flight, in_flight_buys) = (
                    flight.sell_shares_by_token,
                    flight.buy_shares_by_token,
                );
                let mut rows: Vec<copybot_hot::custody::Holding> = Vec::new();
                for lane in rt_ca.snapshot().iter() {
                    let name = lane.cfg.name.clone();
                    let held = ctl_ca.lock().unwrap().ledger.holdings(&name);
                    for (tok, ours) in held.iter() {
                        if *ours <= 1e-9 {
                            continue;
                        }
                        let pool = ctl_ca.lock().unwrap().ledger.pool_claim(tok);
                        let total = venue.get(tok).copied().unwrap_or(0.0);
                        let mine = copybot_hot::custody::lane_share_of(
                            total,
                            pool,
                            *ours,
                        );
                        rows.push(copybot_hot::custody::Holding {
                            lane: name.clone(),
                            token: tok.clone(),
                            ledger: *ours,
                            venue: mine,
                            in_flight: in_flight
                                .get(&(name.clone(), tok.clone()))
                                .copied()
                                .unwrap_or(0.0),
                            in_flight_buy: in_flight_buys
                                .get(&(name.clone(), tok.clone()))
                                .copied()
                                .unwrap_or(0.0),
                        });
                    }
                }
                let report = copybot_hot::custody::audit(&rows, complete);
                *last_ca.lock().unwrap() = Some(report.clone());
                if report.is_alarm() {
                    let now = copybot_hot::ledger::now_secs();
                    let first = alarm_since.is_none();
                    if alarm_since.is_none() {
                        alarm_since = Some(now);
                    }
                    if first {
                        eprintln!(
                            "[custody] {}", copybot_hot::custody::describe(& report)
                        );
                        em_ca
                            .emit(
                                serde_json::json!(
                                    { "t" : now_ms() as i64, "ev" : "custody_delta", "msg" :
                                    copybot_hot::custody::describe(& report), "unattributed" :
                                    report.unattributed, "unattributed_shares" : (report
                                    .unattributed_shares * 10_000.0).round() / 10_000.0,
                                    "checked" : report.checked, "rows" : report.rows, }
                                ),
                            );
                    }
                } else if alarm_since.take().is_some() {
                    em_ca
                        .emit(
                            serde_json::json!(
                                { "t" : now_ms() as i64, "ev" : "custody_clear", "msg" :
                                copybot_hot::custody::describe(& report) }
                            ),
                        );
                }
            }
        });
    }
    const SETTLE_CLOCK_SLOP_SECS: i64 = 120;
    if live {
        let (ctl, rt, em, fund) = (
            control.clone(),
            router.clone(),
            emitter.clone(),
            funder.clone(),
        );
        tokio::spawn(async move {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default();
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let url = format!(
                    "https://data-api.polymarket.com/positions?user={fund}\
&sizeThreshold=0.0001&limit=500&redeemable=true"
                );
                let rows: Vec<serde_json::Value> = match http.get(&url).send().await {
                    Ok(r) if r.status().is_success() => {
                        r.json().await.unwrap_or_default()
                    }
                    _ => continue,
                };
                let mut payout: std::collections::HashMap<String, f64> = Default::default();
                for p in &rows {
                    let view = copybot_hot::settlement::PositionView {
                        redeemable: p["redeemable"].as_bool().unwrap_or(false),
                        cur_price: p["curPrice"]
                            .as_f64()
                            .or_else(|| {
                                p["curPrice"].as_str().and_then(|x| x.parse().ok())
                            })
                            .unwrap_or(0.0),
                    };
                    if let (Some(tok), Some(pay)) = (
                        p["asset"].as_str(),
                        copybot_hot::settlement::resolved_payout(view),
                    ) {
                        if !tok.is_empty() {
                            payout.insert(tok.to_string(), pay);
                        }
                    }
                }
                if payout.is_empty() {
                    continue;
                }
                for lane in rt.snapshot().iter() {
                    let name = lane.cfg.name.clone();
                    let held = ctl.lock().unwrap().ledger.holdings(&name);
                    for tok in held.keys() {
                        let Some(&pay) = payout.get(tok) else { continue };
                        let mut g = ctl.lock().unwrap();
                        let before = g
                            .ledger
                            .lanes
                            .get(&name)
                            .map(|b| b.risk.realised_pnl)
                            .unwrap_or(0.0);
                        g.ledger.record_settlement(&name, tok, pay);
                        let after = g
                            .ledger
                            .lanes
                            .get(&name)
                            .map(|b| b.risk.realised_pnl)
                            .unwrap_or(0.0);
                        drop(g);
                        eprintln!(
                            "[settle] {name}: token …{} resolved at {pay:.3} \
                                   -> realised {:+.2}",
                            & tok[tok.len().saturating_sub(8)..], after - before
                        );
                        em.emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "settle", "lane" : name, "tok" : &
                                tok[..tok.len().min(14)], "payout" : pay, "realised_delta" :
                                ((after - before) * 100.0).round() / 100.0 }
                            ),
                        );
                    }
                }
            }
        });
    }
    if live {
        let (ctl, rt, em, fund) = (
            control.clone(),
            router.clone(),
            emitter.clone(),
            funder.clone(),
        );
        tokio::spawn(async move {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default();
            let mut market_tokens: std::collections::HashMap<String, Vec<String>> = Default::default();
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let ledger_path = ctl.lock().unwrap().ledger.path.clone();
                let (corrected, releases) = copybot_hot::ledger::scan_for_settlement(
                    &ledger_path,
                );
                const PAGE: usize = 200;
                const MAX_PAGES: usize = 25;
                let mut redemptions = Vec::new();
                for page in 0..MAX_PAGES {
                    let url = format!(
                        "https://data-api.polymarket.com/activity?user={fund}\
&limit={PAGE}&offset={}&type=REDEEM",
                        page * PAGE
                    );
                    let body: serde_json::Value = match http.get(&url).send().await {
                        Ok(r) if r.status().is_success() => {
                            r.json().await.unwrap_or_default()
                        }
                        _ => break,
                    };
                    let rows = copybot_hot::settlement::parse_redemptions(&body);
                    let short = rows.len() < PAGE;
                    let fresh = rows.iter().any(|r| !corrected.contains(&r.key()));
                    redemptions.extend(rows);
                    if short || !fresh {
                        break;
                    }
                    if page + 1 == MAX_PAGES {
                        eprintln!(
                            "[settle] stopped at {MAX_PAGES} pages with unbooked \
redemptions still older — the next tick continues from the top"
                        );
                    }
                }
                if redemptions.is_empty() {
                    continue;
                }
                let todo = copybot_hot::settlement::unbooked(&redemptions, &corrected);
                if todo.is_empty() {
                    continue;
                }
                let lane_names: Vec<String> = rt
                    .snapshot()
                    .iter()
                    .map(|l| l.cfg.name.clone())
                    .collect();
                for r in todo {
                    let tokens = if let Some(t) = market_tokens.get(&r.condition_id) {
                        t.clone()
                    } else {
                        let murl = format!(
                            "https://clob.polymarket.com/markets/{}", r.condition_id
                        );
                        let toks: Vec<String> = match http.get(&murl).send().await {
                            Ok(resp) if resp.status().is_success() => {
                                resp.json::<serde_json::Value>()
                                    .await
                                    .ok()
                                    .and_then(|m| {
                                        m["tokens"]
                                            .as_array()
                                            .map(|a| {
                                                a
                                                    .iter()
                                                    .map(|t| t["token_id"].as_str().unwrap_or("").to_string())
                                                    .collect()
                                            })
                                    })
                                    .unwrap_or_default()
                            }
                            _ => Vec::new(),
                        };
                        if !toks.is_empty() {
                            market_tokens.insert(r.condition_id.clone(), toks.clone());
                        }
                        toks
                    };
                    let Some(tok) = tokens
                        .get(r.outcome_index as usize)
                        .filter(|t| !t.is_empty()) else { continue };
                    let payout_per_share = r.usdc / r.size;
                    let mut booked: Option<(String, f64)> = None;
                    for name in &lane_names {
                        let key = r.lane_key(name);
                        if corrected.contains(&key) {
                            continue;
                        }
                        let xkey = copybot_hot::settlement::cross_writer_key(name, tok);
                        if corrected.contains(&xkey) {
                            eprintln!(
                                "[settle] {name}: skipping token …{} — already \
                                       booked by settlewatch ({xkey})",
                                & tok[tok.len().saturating_sub(8)..]
                            );
                            em.emit(
                                serde_json::json!(
                                    { "t" : now_ms(), "ev" : "settle_dedupe_crosswriter", "lane"
                                    : name, "tok" : & tok[..tok.len().min(14)], "key" : key,
                                    "alt_key" : xkey, "tx" : & r.tx, "note" :
                                    "settlewatch already credited this settlement; \
                                         if this token really redeemed twice, this is the \
                                         payout that went unbooked"
                                    }
                                ),
                            );
                            continue;
                        }
                        let Some(&(released, avg_cost, rel_t)) = releases
                            .get(&(name.clone(), tok.to_string())) else { continue };
                        if rel_t < r.ts - SETTLE_CLOCK_SLOP_SECS {
                            continue;
                        }
                        let shares = released.min(r.size);
                        if shares <= 1e-9 {
                            continue;
                        }
                        booked = Some((name.clone(), shares));
                        let proceeds = shares * payout_per_share;
                        let mut g = ctl.lock().unwrap();
                        g.ledger
                            .record_realised_adjustment_tx(
                                name,
                                tok,
                                shares,
                                proceeds,
                                avg_cost,
                                "redemption observed after an earlier P&L-neutral \
                             reconciliation release; corrected to the real on-chain payout",
                                &key,
                            );
                        drop(g);
                        eprintln!(
                            "[settle] {name}: CORRECTED a prior neutral release for \
                                   token …{} — real payout ${payout_per_share:.4}/sh \
                                   (${proceeds:.2} total)",
                            & tok[tok.len().saturating_sub(8)..]
                        );
                        em.emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "settle_correction", "lane" : name,
                                "tok" : & tok[..tok.len().min(14)], "payout" :
                                payout_per_share, "shares" : shares, "key" : key, "proceeds"
                                : (proceeds * 100.0).round() / 100.0 }
                            ),
                        );
                        break;
                    }
                    if let Some((lane, shares)) = booked {
                        if shares + 1e-9 < r.size {
                            eprintln!(
                                "[settle] {lane} claimed {shares:.4} of a \
{:.4}-share redemption on …{} — the remainder belongs to no lane",
                                r.size, & tok[tok.len().saturating_sub(8)..]
                            );
                            em.emit(
                                serde_json::json!(
                                    { "t" : now_ms(), "ev" : "settle_unclaimed_remainder",
                                    "lane" : lane, "tok" : & tok[..tok.len().min(14)], "claimed"
                                    : shares, "redeemed" : r.size }
                                ),
                            );
                        }
                    }
                }
            }
        });
    }
    if live {
        let (lg, ctl, rt, em, clob_r, creds_r, addr_r) = (
            pending_log.clone(),
            control.clone(),
            router.clone(),
            emitter.clone(),
            clob.clone(),
            creds.clone(),
            signer_addr.clone(),
        );
        let rb_res = resting_book.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            loop {
                tick.tick().await;
                if lg.lock().unwrap().is_empty() {
                    continue;
                }
                resolve_pending(
                        &lg,
                        &ctl,
                        &rt,
                        &em,
                        &clob_r,
                        &creds_r,
                        &addr_r,
                        sig_type,
                        Some(&rb_res),
                    )
                    .await;
            }
        });
    }
    if live {
        let (ctl, rt, em, fund, pend) = (
            control.clone(),
            router.clone(),
            emitter.clone(),
            funder.clone(),
            pending_log.clone(),
        );
        let errpath = errors_path.clone();
        let drift_w = drift_by_lane.clone();
        tokio::spawn(async move {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default();
            let mut tick = tokio::time::interval(Duration::from_secs(RECON_SECS));
            loop {
                tick.tick().await;
                let snap = copybot_hot::positions::fetch(
                        &http,
                        "https://data-api.polymarket.com",
                        &fund,
                        "0.0001",
                        "",
                        (now_ms() / 1000) as i64,
                    )
                    .await;
                if !snap.may_act_destructively() {
                    em.emit(
                        serde_json::json!(
                            { "t" : now_ms(), "ev" : "recon_skipped_incomplete", "why" :
                            snap.completeness.reason(), "rows" : snap.len(), "note" :
                            "a partial portfolio cannot prove a token is gone", }
                        ),
                    );
                    continue;
                }
                if snap.is_empty() {
                    continue;
                }
                let chain = snap.by_asset();
                let inflight: std::collections::HashSet<String> = pend
                    .lock()
                    .unwrap()
                    .open_tokens()
                    .into_iter()
                    .map(|(_, t)| t)
                    .collect();
                let now = (now_ms() / 1000) as i64;
                for lane in rt.snapshot().iter() {
                    let name = lane.cfg.name.clone();
                    let held = ctl.lock().unwrap().ledger.holdings(&name);
                    let mut total_drift = 0.0f64;
                    for (tok, sh) in held.iter() {
                        let total = chain.get(tok).copied().unwrap_or(0.0);
                        let pool = ctl.lock().unwrap().ledger.pool_claim(tok);
                        let on_chain = copybot_hot::custody::lane_share_of(
                            total,
                            pool,
                            *sh,
                        );
                        let d = sh - on_chain;
                        if d.abs() <= 0.001 {
                            continue;
                        }
                        total_drift += d.abs();
                        em.emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "recon_gap", "lane" : name, "tok" :
                                & tok[..tok.len().min(14)], "ledger" : sh, "chain" :
                                on_chain, "pooled" : total, "pool_claim" : pool, "delta" :
                                d, "note" : if d > 0.0 {
                                "ledger over chain — an exit could oversell" } else {
                                "chain over ledger — a position we may not be tracking" }
                                }
                            ),
                        );
                    }
                    drift_w.lock().unwrap().insert(name.clone(), total_drift);
                    let phantoms: Vec<(String, f64, f64)> = ctl
                        .lock()
                        .unwrap()
                        .ledger
                        .releasable(&name, &chain, RECON_RELEASE_QUIET_SECS, now)
                        .into_iter()
                        .filter(|(t, _, _)| !inflight.contains(t))
                        .collect();
                    for (tok, gone, px) in phantoms {
                        ctl.lock()
                            .unwrap()
                            .ledger
                            .record_recon_fill(&name, &tok, 1, gone, px, 0.0);
                        eprintln!(
                            "[recon] {name}: released phantom …{} ({gone:.4} sh) — \
the chain has not shown it for {RECON_RELEASE_QUIET_SECS}s",
                            & tok[tok.len().saturating_sub(8)..]
                        );
                        em.emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "recon_released", "lane" : name,
                                "tok" : & tok[..tok.len().min(14)], "shares" : gone, "price"
                                : px, "note" :
                                "phantom share count corrected; P&L unchanged" }
                            ),
                        );
                        copybot_hot::errors::record(
                            &errpath,
                            &copybot_hot::errors::ErrorRow {
                                t: now,
                                lane: name.clone(),
                                kind: "recon".into(),
                                detail: format!(
                                    "released {gone:.4} sh of …{} at cost {px:.4}", & tok[tok
                                    .len().saturating_sub(10)..]
                                ),
                                human: format!(
                                    "Corrected our share count on one market by {gone:.4} of a \
share — the wallet no longer holds it. Profit and loss are unchanged, and nothing \
needed selling."
                                ),
                                severity: "notice".into(),
                            },
                        );
                    }
                    let surplus: Vec<(String, f64, f64)> = ctl
                        .lock()
                        .unwrap()
                        .ledger
                        .adoptable(&name, &chain, RECON_QUIET_SECS, now, 0.05)
                        .into_iter()
                        .filter(|(t, _, _)| !inflight.contains(t))
                        .collect();
                    for (tok, extra, px) in surplus {
                        ctl.lock()
                            .unwrap()
                            .ledger
                            .record_recon_fill(&name, &tok, 0, extra, px, 0.0);
                        eprintln!(
                            "[recon] {name}: adopted …{} ({extra:.4} sh) — the chain \
has shown it for {RECON_QUIET_SECS}s and we hold the market",
                            & tok[tok.len().saturating_sub(8)..]
                        );
                        em.emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "recon_adopted", "lane" : name,
                                "tok" : & tok[..tok.len().min(14)], "shares" : extra,
                                "price" : px, "note" :
                                "untracked remainder booked at our own cost; P&L unchanged"
                                }
                            ),
                        );
                        copybot_hot::errors::record(
                            &errpath,
                            &copybot_hot::errors::ErrorRow {
                                t: now,
                                lane: name.clone(),
                                kind: "recon".into(),
                                detail: format!(
                                    "adopted {extra:.4} sh of …{} at cost {px:.4}", & tok[tok
                                    .len().saturating_sub(10)..]
                                ),
                                human: format!(
                                    "Started tracking {extra:.4} of a share we already owned on \
one market — left over from an earlier sale. It can now be sold with the rest; profit and \
loss are unchanged."
                                ),
                                severity: "notice".into(),
                            },
                        );
                    }
                }
            }
        });
    }
    let seed_bankrolls = registry.clone();
    let seed_registry = registry.clone();
    if live && !shadow {
        let (cs, cl, ad, wallet, cell) = (
            creds_slot.clone(),
            clob.clone(),
            auth_addr.clone(),
            funder.clone(),
            physical_wallet.clone(),
        );
        let hc = mk_client(false).expect("seed-check client");
        tokio::spawn(async move {
            for _ in 0..20 {
                if cs.lock().unwrap().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let mut first = true;
            loop {
                let creds = cs.lock().unwrap().clone();
                let cash = match creds {
                    Some(cr) => {
                        copybot_hot::matchup::free_cash(&hc, &cl, &ad, &cr, sig_type)
                            .await
                    }
                    None => None,
                };
                let pos_snap = copybot_hot::positions::fetch(
                        &hc,
                        "https://data-api.polymarket.com",
                        &wallet,
                        "0",
                        "",
                        copybot_hot::ledger::now_secs(),
                    )
                    .await;
                let marked = if pos_snap.completeness.is_complete() {
                    marked_position_value(&serde_json::Value::Array(pos_snap.rows))
                } else {
                    None
                };
                let equity = copybot_hot::budget::wallet_equity(cash, marked);
                {
                    let mut snapshot = cell.lock().unwrap();
                    if cash.is_some() {
                        snapshot.cash = cash;
                    }
                    if equity.is_some() {
                        snapshot.equity = equity;
                    }
                    if equity.is_some() {
                        snapshot.fetched_at = Some(copybot_hot::ledger::now_secs());
                        snapshot.stale_reason = None;
                    } else {
                        snapshot.stale_reason = Some(
                            if cash.is_some() {
                                "positions could not be valued — equity is the LAST GOOD one \
and its age is real"
                            } else {
                                "balance refresh returned nothing usable"
                            },
                        );
                    }
                }
                let total_seed: f64 = seed_registry
                    .specs()
                    .iter()
                    .filter(|w| w.enabled)
                    .map(|w| w.seed_usd)
                    .sum();
                if first && total_seed > 0.0 {
                    let fc = cash
                        .map(|c| format!("{c:.2}"))
                        .unwrap_or_else(|| "?".into());
                    let dep = marked
                        .map(|m| format!("{:.2}", m.max(0.0)))
                        .unwrap_or_else(|| "?".into());
                    match copybot_hot::budget::seeds_fit_wallet(total_seed, equity) {
                        Ok(()) => {
                            eprintln!(
                                "[seed] free cash ${fc} + deployed ${dep} = equity ${} ; seeds ${total_seed:.2} ; headroom ${}",
                                equity.map(| e | format!("{e:.2}")).unwrap_or_else(||
                                "unread".into()), equity.map(| e | format!("{:.2}", e -
                                total_seed)).unwrap_or_else(|| "?".into())
                            )
                        }
                        Err(e) => eprintln!("[seed] ⚠️  {e}"),
                    }
                    first = false;
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }
    {
        let (rt, em, reg) = (router.clone(), emitter.clone(), seed_bankrolls.clone());
        let ctl_b = control.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(120));
            loop {
                tick.tick().await;
                for lane in rt.snapshot().iter() {
                    let Some(seed) = reg.seed_of(&lane.cfg.name) else { continue };
                    let realised = match ctl_b.lock() {
                        Ok(g) => {
                            g.ledger
                                .lanes
                                .get(&lane.cfg.name)
                                .map(|b| b.risk.realised_pnl)
                        }
                        Err(_) => None,
                    };
                    let Some(realised) = realised else {
                        eprintln!(
                            "[bankroll] missing lane ledger for {} — scale unchanged",
                            lane.cfg.name
                        );
                        continue;
                    };
                    let live = copybot_hot::budget::virtual_bankroll(seed, realised);
                    let want = copybot_hot::budget::bankroll_scale(live, seed);
                    let old = lane.state.cap_scale.load(Ordering::Relaxed) as f64
                        / copybot_hot::lanes::MICRO;
                    let next = copybot_hot::budget::slew(old, want);
                    if (next - old).abs() > 1e-4 {
                        lane.state
                            .cap_scale
                            .store(
                                (next * copybot_hot::lanes::MICRO) as i64,
                                Ordering::Relaxed,
                            );
                        em.emit(
                            serde_json::json!(
                                { "ev" : "bankroll", "lane" : & lane.cfg.name,
                                "realised_pnl" : (realised * 100.0).round() / 100.0,
                                "virtual_bankroll" : (live * 100.0).round() / 100.0, "seed"
                                : seed, "scale_from" : (old * 1000.0).round() / 1000.0,
                                "scale_to" : (next * 1000.0).round() / 1000.0, "slewed" :
                                (next - want).abs() > 1e-6, "t" : now_ms() }
                            ),
                        );
                    }
                }
            }
        });
    }
    {
        let (h, c, e) = (order_paths.clone(), clob.clone(), emitter.clone());
        tokio::spawn(async move {
            loop {
                let t = std::time::Instant::now();
                let mut ok = true;
                let mut rtts: Vec<f64> = Vec::new();
                for (_name, cl) in h.iter() {
                    for _ in 0..2 {
                        let t1 = std::time::Instant::now();
                        ok &= cl.get(format!("{c}/time")).send().await.is_ok();
                        rtts.push(t1.elapsed().as_micros() as f64 / 1000.0);
                    }
                }
                rtts.sort_by(|a, b| a.partial_cmp(b).unwrap());
                e.emit(
                    serde_json::json!(
                        { "t" : now_ms(), "ev" : "warm", "ok" : ok, "rtt_ms" : t
                        .elapsed().as_micros() as f64 / 1000.0, "best_ms" : rtts.first(),
                        "worst_ms" : rtts.last(), }
                    ),
                );
                tokio::time::sleep(Duration::from_secs(25)).await;
            }
        });
    }
    let politics: Arc<std::sync::RwLock<std::collections::HashSet<[u8; 32]>>> = Arc::new(
        std::sync::RwLock::new(std::collections::HashSet::new()),
    );
    if live {
        let (pol, reg) = (politics.clone(), registry.clone());
        let hc = mk_client(false).unwrap_or_default();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(600));
            loop {
                tick.tick().await;
                let leaders: Vec<String> = reg
                    .specs()
                    .into_iter()
                    .filter(|w| w.exclude_political)
                    .map(|w| w.leader)
                    .collect();
                if leaders.is_empty() {
                    continue;
                }
                let mut set = std::collections::HashSet::new();
                for leader in leaders {
                    let url = format!(
                        "https://data-api.polymarket.com/activity?\
user={leader}&limit=500"
                    );
                    let rows: Vec<serde_json::Value> = match hc.get(&url).send().await {
                        Ok(r) if r.status().is_success() => {
                            r.json().await.unwrap_or_default()
                        }
                        _ => continue,
                    };
                    for r in &rows {
                        let ty = r["type"].as_str().unwrap_or("");
                        let political = ty == "SPLIT" || ty == "MERGE"
                            || is_political_title(r["title"].as_str().unwrap_or(""));
                        if political {
                            if let Some(cid) = r["conditionId"]
                                .as_str()
                                .and_then(copybot_hot::merge::condition_id_bytes)
                            {
                                set.insert(cid);
                            }
                        }
                    }
                }
                let n = set.len();
                if let Ok(mut g) = pol.write() {
                    *g = set;
                }
                eprintln!("[politics] exclusion set refreshed: {n} conditions");
            }
        });
    }
    let mut tx_seen = copybot_hot::race::SeenTx::new(8192);
    let mut behind = copybot_hot::race::BehindTally::default();
    let mut since_race_report: u64 = 0;
    const FEED_RACE_EVERY: u64 = 250_000;
    while let Some(raw) = rx.recv().await {
        let t0 = std::time::Instant::now();
        let queue_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| (d.as_nanos().saturating_sub(raw.seen_ns)) as f64 / 1000.0)
            .unwrap_or(0.0);
        if let Some(behind_ns) = tx_seen.first_delivery_at(&raw.hash, raw.seen_ns) {
            behind.record(&raw.source, behind_ns);
            since_race_report += 1;
            if since_race_report >= FEED_RACE_EVERY {
                since_race_report = 0;
                if !behind.is_empty() {
                    emitter
                        .emit(
                            serde_json::json!(
                                { "t" : now_ms(), "ev" : "feed_race", "note" :
                                "ms each feed arrived BEHIND the first feed to deliver the same transaction. If every feed sits inside a few ms, a faster source wins nothing measurable; a wide spread is real headroom.",
                                "behind" : behind.drain_json(), }
                            ),
                        );
                }
            }
            continue;
        }
        feed_registry.record_win(&raw.source);
        if !raw.to.is_empty() {
            let owned: Vec<(usize, [u8; 20])> = (0..router.len())
                .filter_map(|ix| router.lane(ix).map(|l| (ix, l.cfg.wallet20)))
                .collect();
            let acts = copybot_hot::mergewatch::route(
                &raw.to,
                &raw.input,
                owned.iter().map(|(ix, w)| (*ix, w)),
            );
            for (lane_ix, act) in acts {
                let Some(lane) = router.lane(lane_ix) else { continue };
                let name = lane.cfg.name.clone();
                let leader = format!("0x{}", hex::encode(lane.cfg.wallet20));
                match act.kind {
                    copybot_hot::mergewatch::PairKind::Split => {
                        pair_ledger
                            .lock()
                            .unwrap()
                            .credit_split(
                                &leader,
                                &act.condition_id,
                                act.shares,
                                act.sized,
                            );
                        emitter
                            .emit(
                                serde_json::json!(
                                    { "t" : now_ms(), "ev" : "leader_split", "lane" : name,
                                    "cond" : & act.condition_id[..act.condition_id.len()
                                    .min(14)], "shares" : act.shares, "sized" : act.sized,
                                    "note" : "credited to his split balance; NOT copied" }
                                ),
                            );
                    }
                    copybot_hot::mergewatch::PairKind::Merge => {
                        let signal = pair_ledger
                            .lock()
                            .unwrap()
                            .merge_signal(
                                &leader,
                                &act.condition_id,
                                act.shares,
                                act.sized,
                            );
                        let (kind, exit_shares) = match &signal {
                            copybot_hot::pairledger::MergeSignal::Exit { shares } => {
                                ("exit", *shares)
                            }
                            copybot_hot::pairledger::MergeSignal::NonSignal => {
                                ("income_withdrawal", 0.0)
                            }
                            copybot_hot::pairledger::MergeSignal::Unknown => {
                                ("UNKNOWN_needs_human", 0.0)
                            }
                        };
                        emitter
                            .emit(
                                serde_json::json!(
                                    { "t" : now_ms(), "ev" : "leader_merge", "lane" : name,
                                    "cond" : & act.condition_id[..act.condition_id.len()
                                    .min(14)], "merged" : act.shares, "sized" : act.sized,
                                    "signal" : kind, "exit_shares" : exit_shares, "acted" :
                                    false, "note" :
                                    "OBSERVE-ONLY: decoded and netted, no order placed" }
                                ),
                            );
                        if matches!(
                            signal, copybot_hot::pairledger::MergeSignal::Unknown
                        ) {
                            copybot_hot::errors::record(
                                &errors_path,
                                &copybot_hot::errors::ErrorRow {
                                    t: copybot_hot::ledger::now_secs(),
                                    lane: name.clone(),
                                    kind: "merge_unknown".into(),
                                    detail: format!(
                                        "…{} merged {:.4} — split balance unknowable", & act
                                        .condition_id[act.condition_id.len().saturating_sub(10)..],
                                        act.shares
                                    ),
                                    human: format!(
                                        "{name}'s leader merged a position we cannot classify \
because an earlier split on that market had no readable size. It may be a real exit or a \
savings withdrawal — nothing was done automatically. Check the market by hand."
                                    ),
                                    severity: "warn".into(),
                                },
                            );
                        }
                    }
                }
            }
        }
        let present = participants(&raw.input);
        if present.is_empty() {
            continue;
        }
        for lane_ix in 0..router.len() {
            let lane = match router.lane(lane_ix) {
                Some(l) => l,
                None => continue,
            };
            let w20 = lane.cfg.wallet20;
            if !present.iter().any(|p| p == &w20) {
                continue;
            }
            for d in decode_all(&raw.input, &w20) {
                if lane.cfg.exclude_political
                    && politics
                        .read()
                        .map(|s| s.contains(&d.condition_id))
                        .unwrap_or(false)
                {
                    continue;
                }
                if copybot_hot::lanes::needs_book(&lane) {
                    let mut w = watch_tokens.lock().unwrap();
                    if !w.iter().any(|t| t == &d.token_id) {
                        w.push(d.token_id.clone());
                        while w.len() > copybot_hot::book::WATCH_HARD_MAX {
                            w.remove(0);
                        }
                    }
                }
                if d.side == 1 && !resting_book.is_empty() {
                    let his_taker_exit = d.role != "maker";
                    let live: Vec<_> = resting_book
                        .by_token(&d.token_id)
                        .into_iter()
                        .filter(|o| o.side == 0 || his_taker_exit)
                        .collect();
                    if !live.is_empty() {
                        if let Some(cr) = creds.as_ref().as_ref() {
                            let ids: Vec<String> = live
                                .iter()
                                .map(|o| o.order_id.clone())
                                .collect();
                            let (rb2, h2, cl2, ad2, cr2, e3) = (
                                resting_book.clone(),
                                http.clone(),
                                clob.clone(),
                                auth_addr.clone(),
                                cr.clone(),
                                emitter.clone(),
                            );
                            let tok2 = d.token_id.clone();
                            let sg2 = signal_guard.clone();
                            tokio::spawn(async move {
                                let res = copybot_hot::resting::cancel_batch(
                                        &h2,
                                        &cl2,
                                        &ad2,
                                        &cr2,
                                        &ids,
                                    )
                                    .await;
                                futures_util::future::join_all(
                                        res
                                            .iter()
                                            .filter(|(_, ok)| *ok)
                                            .map(|(id, _)| {
                                                settle_cancelled_rest(
                                                    &h2,
                                                    &cl2,
                                                    &ad2,
                                                    &cr2,
                                                    &rb2,
                                                    &sg2,
                                                    &e3,
                                                    id,
                                                    "he_exited",
                                                )
                                            }),
                                    )
                                    .await;
                                e3.emit(
                                    serde_json::json!(
                                        { "t" : now_ms(), "ev" : "cancel_sweep", "reason" :
                                        copybot_hot::resting::CancelReason::HeExited.as_str(), "tok"
                                        : tok2, "attempted" : ids.len(), "cancelled" : res.iter()
                                        .filter(| (_, ok) | * ok).count(), "still_live" : rb2.len(),
                                        }
                                    ),
                                );
                            });
                        }
                    }
                }
                let key = FillKey {
                    tx: raw.hash.to_ascii_lowercase(),
                    token: d.token_id.clone(),
                    side: d.side,
                    size_micro: (d.fill_size * 1e6) as u64,
                };
                if !book.first_seen(&raw.source, key, None) {
                    continue;
                }
                if raw.source == "txpool" {
                    txstats_fill.rescued.fetch_add(1, Ordering::Relaxed);
                }
                if d.side == 0 {
                    let his_now = control
                        .lock()
                        .unwrap()
                        .observe_his_fill(
                            &lane.cfg.name,
                            &d.token_id,
                            d.side,
                            d.fill_size,
                        );
                    lane.set_his(&d.token_id, his_now);
                }
                let mut progress = copybot_hot::signal_guard::Progress {
                    his_filled: d.fill_size,
                    our_copied: 0.0,
                };
                if d.side == 0 {
                    if let Some(guard) = &signal_guard {
                        match guard
                            .observe(
                                &d.salt,
                                &d.condition_id,
                                &lane.cfg.name,
                                d.fill_size,
                                d.order_size,
                                copybot_hot::ledger::now_secs(),
                            )
                        {
                            Ok(p) => progress = p,
                            Err(reason) => {
                                emitter
                                    .emit(
                                        serde_json::json!(
                                            { "t" : now_ms(), "ev" : "signal_guard_skip", "lane" : lane
                                            .cfg.name, "tok" : d.token_id, "condition" : hex::encode(d
                                            .condition_id), "his_order_id" : hex::encode(d.salt), "why"
                                            : format!("{reason:?}"), }
                                        ),
                                    );
                                continue;
                            }
                        }
                        if let Err(reason) = guard
                            .check(
                                &d.salt,
                                &d.condition_id,
                                &lane.cfg.name,
                                &d.token_id,
                                copybot_hot::ledger::now_secs(),
                            )
                        {
                            emitter
                                .emit(
                                    serde_json::json!(
                                        { "t" : now_ms(), "ev" : "signal_guard_skip", "lane" : lane
                                        .cfg.name, "tok" : d.token_id, "condition" : hex::encode(d
                                        .condition_id), "why" : format!("{reason:?}"), }
                                    ),
                                );
                            if matches!(
                                reason, copybot_hot::signal_guard::Refusal::Persistence(_)
                            ) {
                                latch_buys(
                                    &lane,
                                    &incidents,
                                    "persistence",
                                    "signal guard cannot persist",
                                );
                            }
                            continue;
                        }
                    }
                }
                {
                    let remaining = (d.order_size - d.fill_size).max(0.0);
                    let book_size = books
                        .top(&d.token_id)
                        .map(|t| if d.side == 0 { t.best_bid } else { t.best_ask })
                        .unwrap_or(0.0);
                    let _ = book_size;
                    fpscore.confirm(&d.token_id, d.side, d.price, d.fill_size);
                    if remaining > 0.0 {
                        fps.learn(&d.token_id, d.side, d.price, remaining, remaining);
                    } else {
                        fps.forget(&d.token_id);
                    }
                }
                let decided = router.decide(lane_ix, &d, progress);
                let lane_name = lane.cfg.name.clone();
                if d.side == 1 {
                    let his_now = control
                        .lock()
                        .unwrap()
                        .observe_his_fill(&lane_name, &d.token_id, d.side, d.fill_size);
                    lane.set_his(&d.token_id, his_now);
                }
                match decided {
                    Err(skip) => {
                        *control
                            .lock()
                            .unwrap()
                            .skips
                            .entry(format!("{skip:?}"))
                            .or_insert(0) += 1;
                        emitter
                            .emit(
                                serde_json::json!(
                                    { "t" : now_ms(), "ev" : "skip", "lane" : lane_name, "why" :
                                    format!("{skip:?}"), "src" : raw.source, "tok" : & d
                                    .token_id[..d.token_id.len().min(14)], "side" : if d.side ==
                                    0 { "BUY" } else { "SELL" }, "px" : d.price, "his_fill" : d
                                    .fill_size, "his_order" : d.order_size, "his_order_id" :
                                    hex::encode(d.salt), "progress_committed" : d.side == 0, }
                                ),
                            );
                    }
                    Ok(mut intent) => {
                        if intent.side == 0 {
                            let pol = lane.policy();
                            let deployed = lane.state.open_usd.load(Ordering::Relaxed)
                                as f64 / MICRO;
                            let available = copybot_hot::budget::lane_available(
                                pol.seed_usd,
                                deployed,
                            );
                            let (free_cash, cash_age) = physical_wallet
                                .lock()
                                .ok()
                                .map(|w| (
                                    w.cash,
                                    w.fetched_at.map(|t| copybot_hot::ledger::now_secs() - t),
                                ))
                                .unwrap_or((None, None));
                            let cost = intent.usd as f64 / MICRO;
                            if !copybot_hot::budget::buy_fits_aged(
                                available,
                                free_cash,
                                cost,
                                cash_age,
                            ) {
                                *control
                                    .lock()
                                    .unwrap()
                                    .skips
                                    .entry("NoCash".to_string())
                                    .or_insert(0) += 1;
                                emitter
                                    .emit(
                                        serde_json::json!(
                                            { "t" : now_ms(), "ev" : "skip", "lane" : lane_name, "why" :
                                            "NoCash", "src" : raw.source, "tok" : & intent
                                            .token_id[..intent.token_id.len().min(14)], "cost_usd" :
                                            cost, "lane_available_usd" : available, "lane_seed_usd" :
                                            pol.seed_usd, "lane_deployed_usd" : deployed,
                                            "physical_cash" : free_cash, }
                                        ),
                                    );
                                continue;
                            }
                        }
                        let intended_frac = match lane.cfg.sizing {
                            copybot_hot::lanes::Sizing::Pct(p) => p,
                            _ => 0.0,
                        };
                        let overcopied = copybot_hot::firerate::is_overcopy(
                            progress.his_filled,
                            progress.our_copied,
                            intended_frac,
                            copybot_hot::firerate::OVERCOPY_TOLERANCE,
                        );
                        if intent.side == 0
                            && fire_rate.record(lane_ix, copybot_hot::ledger::now_secs())
                            && !overcopied
                        {
                            emitter
                                .emit(
                                    serde_json::json!(
                                        { "t" : now_ms(), "ev" : "fire_rate_corroborated", "lane" :
                                        lane_name, "tok" : intent.token_id, "in_window" : fire_rate
                                        .count(lane_ix, copybot_hot::ledger::now_secs()),
                                        "his_filled" : progress.his_filled, "our_copied" : progress
                                        .our_copied, "intended_frac" : intended_frac, "note" :
                                        "burst rate breached but our size tracks his fills — NOT halted",
                                        }
                                    ),
                                );
                        }
                        if intent.side == 0 && overcopied
                            && fire_rate.count(lane_ix, copybot_hot::ledger::now_secs())
                                >= fire_rate.limit()
                        {
                            latch_buys(
                                &lane,
                                &incidents,
                                "fire_rate",
                                "buy rate breached AND we hold more than his fills justify",
                            );
                            eprintln!(
                                "[{lane_name}] FIRE-RATE HALT: >{} buys in {}s — buys stopped, exits still allowed",
                                fire_rate.limit(), copybot_hot::firerate::WINDOW_SECS
                            );
                            emitter
                                .emit(
                                    serde_json::json!(
                                        { "t" : now_ms(), "ev" : "fire_rate_halt", "lane" :
                                        lane_name, "limit" : fire_rate.limit(), "window_s" :
                                        copybot_hot::firerate::WINDOW_SECS, "in_window" : fire_rate
                                        .count(lane_ix, copybot_hot::ledger::now_secs()), "tok" :
                                        intent.token_id, "his_filled" : progress.his_filled,
                                        "our_copied" : progress.our_copied, "intended_frac" :
                                        intended_frac, "note" :
                                        "BUYS halted; SELLS continue so we can still follow him out",
                                        }
                                    ),
                                );
                            continue;
                        }
                        let (his_first_tranche, his_order_done, his_cum_filled) = if intent
                            .side == 1
                        {
                            let cum = sell_tranches.observe(&d.salt, d.fill_size);
                            let (first, done) = copybot_hot::lanes::sell_tranche_flags(
                                cum,
                                d.fill_size,
                                d.order_size,
                            );
                            (first, done, cum)
                        } else {
                            (false, false, progress.his_filled)
                        };
                        intent.route = resolve_route(
                            &intent,
                            &lane.cfg,
                            books.top(&intent.token_id),
                            d.price,
                            progress.our_copied <= 0.0,
                            his_first_tranche,
                            his_order_done,
                            lane.state.rest_buys.load(Ordering::Relaxed),
                            lane.state.rest_sells.load(Ordering::Relaxed),
                        );
                        let (limit, order_type) = match intent.route {
                            Route::Take => (intent.limit, "FAK"),
                            Route::RestInFront { limit, .. } => (limit, "GTC"),
                        };
                        let (ma, ta) = amounts(limit, intent.shares, intent.side);
                        let salt = safe_salt(now_ms() ^ (t0.elapsed().as_nanos() << 20));
                        let ord = Order {
                            salt,
                            maker: funder.clone(),
                            signer: signer_addr.clone(),
                            token_id: intent.token_id.clone(),
                            maker_amount: ma,
                            taker_amount: ta,
                            side: intent.side,
                            signature_type: sig_type,
                            timestamp: now_ms(),
                            neg_risk: raw.to_neg_risk,
                        };
                        let sig = pk.as_ref().map(|k| ord.sign_for_type(k));
                        let elapsed_us = t0.elapsed().as_nanos() as f64 / 1000.0;
                        response_times
                            .push(copybot_hot::response_times::Sample {
                                t: copybot_hot::ledger::now_secs(),
                                lane: lane_name.clone(),
                                token: intent.token_id.clone(),
                                side: intent.side,
                                us: elapsed_us,
                            });
                        let ingest_lag_us = (std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos())
                            .unwrap_or(0))
                            .saturating_sub(raw.seen_ns) / 1_000;
                        let fire_ev = serde_json::json!(
                            { "t" : now_ms(), "ev" : if live { "fire" } else {
                            "would_fire" }, "ingest_lag_us" : ingest_lag_us, "lane" :
                            lane_name, "src" : raw.source, "tx" : raw.hash, "tok" :
                            intent.token_id, "condition" : hex::encode(d.condition_id),
                            "his_order_id" : hex::encode(d.salt), "side" : if intent.side
                            == 0 { "BUY" } else { "SELL" }, "shares" : intent.shares,
                            "limit" : limit, "route" : match intent.route { Route::Take
                            => serde_json::json!("take"), Route::RestInFront { limit,
                            room_ticks } => serde_json::json!({ "rest" : limit,
                            "room_ticks" : room_ticks }), }, "usd" : intent.usd as f64 /
                            MICRO, "his_px" : d.price, "pad_c" : ((limit - d.price) *
                            10000.0).round() / 100.0, "his_fill" : d.fill_size,
                            "his_order" : d.order_size, "his_remaining" : intent
                            .his_remaining, "exec" : format!("{:?}", intent.execution),
                            "signal_to_ready_us" : elapsed_us, "queue_us" : queue_us, }
                        );
                        if !live {
                            emitter.emit(fire_ev.clone());
                        }
                        if live {
                            let Some(sig) = sig else {
                                latch_buys(
                                    &lane,
                                    &incidents,
                                    "signature",
                                    "missing order signature",
                                );
                                emitter
                                    .emit(
                                        serde_json::json!(
                                            { "t" : now_ms(), "ev" : "submit_precondition_failed",
                                            "lane" : lane_name, "tok" : intent.token_id, "why" :
                                            "missing order signature", "buys_halted" : true, }
                                        ),
                                    );
                                continue;
                            };
                            let Some(credential) = creds.as_ref().as_ref() else {
                                latch_buys(
                                    &lane,
                                    &incidents,
                                    "credentials",
                                    "API credentials unavailable",
                                );
                                emitter
                                    .emit(
                                        serde_json::json!(
                                            { "t" : now_ms(), "ev" : "submit_precondition_failed",
                                            "lane" : lane_name, "tok" : intent.token_id, "why" :
                                            "missing L2 credentials", "buys_halted" : true, }
                                        ),
                                    );
                                continue;
                            };
                            let owner = credential.key.clone();
                            let body = json_body(&ord, &sig, &owner, order_type);
                            let body = match serde_json::to_string(&body) {
                                Ok(s) => s,
                                Err(e) => {
                                    latch_buys(
                                        &lane,
                                        &incidents,
                                        "precondition",
                                        "submit precondition failed",
                                    );
                                    emitter
                                        .emit(
                                            serde_json::json!(
                                                { "t" : now_ms(), "ev" : "submit_precondition_failed",
                                                "lane" : lane_name, "tok" : intent.token_id, "why" :
                                                format!("order body serialize failed: {e}"), "buys_halted" :
                                                true, }
                                            ),
                                        );
                                    continue;
                                }
                            };
                            let hdrs = match copybot_hot::auth::l2_headers(
                                &auth_addr,
                                credential,
                                copybot_hot::auth::now_secs(),
                                "POST",
                                "/order",
                                Some(&body),
                            ) {
                                Ok(h) => h,
                                Err(e) => {
                                    latch_buys(
                                        &lane,
                                        &incidents,
                                        "precondition",
                                        "submit precondition failed",
                                    );
                                    emitter
                                        .emit(
                                            serde_json::json!(
                                                { "t" : now_ms(), "ev" : "submit_precondition_failed",
                                                "lane" : lane_name, "tok" : intent.token_id, "why" :
                                                format!("L2 header build failed: {e}"), "buys_halted" :
                                                true, }
                                            ),
                                        );
                                    continue;
                                }
                            };
                            let mut buy_wal_row: Option<copybot_hot::pending::Pending> = None;
                            if intent.side == 0 {
                                let Some(guard) = &signal_guard else {
                                    latch_buys(
                                        &lane,
                                        &incidents,
                                        "precondition",
                                        "submit precondition failed",
                                    );
                                    emitter
                                        .emit(
                                            serde_json::json!(
                                                { "t" : now_ms(), "ev" : "submit_precondition_failed",
                                                "lane" : lane_name, "tok" : intent.token_id, "why" :
                                                "live signal guard missing", "buys_halted" : true, }
                                            ),
                                        );
                                    continue;
                                };
                                let p = copybot_hot::pending::Pending {
                                    lane: lane_name.clone(),
                                    token: intent.token_id.clone(),
                                    side: intent.side,
                                    order_hash: hex::encode(ord.digest()),
                                    shares: intent.shares,
                                    limit,
                                    ts: copybot_hot::ledger::now_secs(),
                                    why: "submitting".into(),
                                    resting: order_type == "GTC",
                                };
                                let row = copybot_hot::pending::PendingLog::row_bytes(&p);
                                if let Err(reason) = guard
                                    .commit_with(
                                        &d.salt,
                                        &d.condition_id,
                                        &lane_name,
                                        &intent.token_id,
                                        intent.shares,
                                        copybot_hot::ledger::now_secs(),
                                        &wal,
                                        Some(&row),
                                    )
                                {
                                    if matches!(
                                        reason, copybot_hot::signal_guard::Refusal::Persistence(_)
                                    ) {
                                        latch_buys(
                                            &lane,
                                            &incidents,
                                            "persistence",
                                            "signal guard commit cannot persist",
                                        );
                                    }
                                    emitter
                                        .emit(
                                            serde_json::json!(
                                                { "t" : now_ms(), "ev" : "signal_guard_skip", "lane" :
                                                lane_name, "tok" : intent.token_id, "condition" :
                                                hex::encode(d.condition_id), "why" : format!("{reason:?}"),
                                                }
                                            ),
                                        );
                                    continue;
                                }
                                pending_log.lock().unwrap().apply_record(p.clone());
                                buy_wal_row = Some(p);
                            }
                            emitter.emit(fire_ev);
                            control.lock().unwrap().record_fire(&lane_name);
                            let (paths, c, e2) = (
                                order_paths.clone(),
                                clob.clone(),
                                emitter.clone(),
                            );
                            let creds_c = creds.clone();
                            let addr_c = auth_addr.clone();
                            let lat_c = latency.clone();
                            let c2 = control.clone();
                            let ln = lane_name.clone();
                            let tok_for_resp = intent.token_id.clone();
                            let side_for_resp = intent.side;
                            let limit_for_resp = limit;
                            let shares_for_resp = intent.shares;
                            let his_price_for_resp = d.price;
                            let rb_c = resting_book.clone();
                            let wake_c = sweep_tx.clone();
                            let router_c = router.clone();
                            let ord_c = ord.clone();
                            let pk_c = pk;
                            let owner_c = owner.clone();
                            let otype_c = order_type;
                            let full_holding = lane.holding(&intent.token_id);
                            let prov_his_pos = lane
                                .his_pos
                                .lock()
                                .unwrap()
                                .get(&intent.token_id)
                                .copied();
                            let anchor_level = levels.size_at(&intent.token_id, d.price);
                            let anchor_his = if anchor_level.is_some() {
                                (d.order_size - his_cum_filled).max(0.0)
                            } else {
                                0.0
                            };
                            let anchor_printed = prints
                                .cumulative_at(&intent.token_id, d.price);
                            let anchor_contested = match (anchor_level, anchor_his) {
                                (Some(lvl), his) if his > 0.0 => {
                                    lvl
                                        > his * (1.0 + copybot_hot::restwatch::DEFICIT_TOLERANCE)
                                }
                                _ => true,
                            };
                            let anchor_price = d.price;
                            let prov_his_order = d.order_size;
                            let prov_his_filled = progress.his_filled;
                            let prov_our_copied = progress.our_copied;
                            let pend_c = pending_log.clone();
                            let sg_c = signal_guard.clone();
                            let inc_c = incidents.clone();
                            let salt_c = d.salt;
                            let cond_c = d.condition_id;
                            let hash_c = hex::encode(ord.digest());
                            tokio::spawn(async move {
                                use copybot_hot::race_send::Outcome;
                                let (outcome, results) = match submit_tracked(
                                        &pend_c,
                                        &e2,
                                        &copybot_hot::provenance::Provenance::mirrored(
                                            if side_for_resp == 1 {
                                                copybot_hot::provenance::Origin::MirrorSell
                                            } else {
                                                copybot_hot::provenance::Origin::MirrorBuy
                                            },
                                            &ln,
                                            &tok_for_resp,
                                            side_for_resp,
                                            shares_for_resp,
                                            limit_for_resp,
                                            full_holding,
                                            prov_his_pos,
                                            Some(prov_his_order),
                                            Some(prov_his_filled),
                                            Some(prov_our_copied),
                                        ),
                                        hash_c.clone(),
                                        otype_c == "GTC",
                                        &paths,
                                        &format!("{c}/order"),
                                        &body,
                                        &hdrs,
                                        Duration::from_secs(10),
                                        buy_wal_row.is_some(),
                                    )
                                    .await
                                {
                                    Ok(v) => v,
                                    Err(e) => {
                                        eprintln!("[{ln}] REFUSING TO SUBMIT: {e}");
                                        e2.emit(
                                            serde_json::json!(
                                                { "t" : now_ms(), "ev" : "submit_refused", "lane" : ln,
                                                "tok" : & tok_for_resp[..tok_for_resp.len().min(14)], "why"
                                                : e }
                                            ),
                                        );
                                        halt_buys_on_persistence_fault_with(
                                            &c2,
                                            &router_c,
                                            Some(&pend_c),
                                            &inc_c,
                                        );
                                        return;
                                    }
                                };
                                let booked = match &outcome {
                                    Outcome::Matched { body, winner, ms } => {
                                        let (durable, filled) = book_and_resolve(
                                            &c2,
                                            &pend_c,
                                            &router_c,
                                            &inc_c,
                                            &e2,
                                            &ln,
                                            &tok_for_resp,
                                            side_for_resp,
                                            limit_for_resp,
                                            body,
                                            Some(his_price_for_resp),
                                            &hash_c,
                                            otype_c == "GTC",
                                        );
                                        if side_for_resp == 0 && durable && otype_c != "GTC" {
                                            let unfilled = shares_for_resp - filled;
                                            if unfilled > 1e-9 {
                                                if let Some(g) = sg_c.as_ref() {
                                                    match g
                                                        .release(
                                                            &salt_c,
                                                            &cond_c,
                                                            &ln,
                                                            unfilled,
                                                            copybot_hot::ledger::now_secs(),
                                                        )
                                                    {
                                                        Ok(_) => {
                                                            e2.emit(
                                                                serde_json::json!(
                                                                    { "t" : now_ms(), "ev" : "commit_released", "lane" : ln,
                                                                    "tok" : tok_for_resp, "ordered" : shares_for_resp, "filled"
                                                                    : filled, "released" : unfilled, "why" :
                                                                    "partial fill — the \
venue killed the remainder" }
                                                                ),
                                                            )
                                                        }
                                                        Err(e) => {
                                                            eprintln!(
                                                                "[{ln}] could \
not release {unfilled} unfilled shares: {e:?} — this order stays under-copied"
                                                            )
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        if side_for_resp == 1 {
                                            let _ = wake_c.send(tok_for_resp.clone());
                                        }
                                        serde_json::json!(
                                            { "outcome" : "matched", "winner" : winner, "ms" : ms }
                                        )
                                    }
                                    Outcome::Resting { order_id, winner, ms, .. } => {
                                        rb_c.add(copybot_hot::resting::Resting {
                                            order_id: order_id.clone(),
                                            lane: ln.clone(),
                                            token: tok_for_resp.clone(),
                                            side: side_for_resp,
                                            limit: limit_for_resp,
                                            shares: shares_for_resp,
                                            placed: copybot_hot::ledger::now_secs(),
                                            salt: salt_c,
                                            condition: cond_c,
                                            his_price: anchor_price,
                                            his_remaining: anchor_his,
                                            printed_at_place: anchor_printed,
                                            contested_at_place: anchor_contested,
                                        });
                                        serde_json::json!(
                                            { "outcome" : "resting", "order_id" : order_id, "winner" :
                                            winner, "ms" : ms }
                                        )
                                    }
                                    Outcome::DuplicateOnly { winner } => {
                                        c2.lock()
                                            .unwrap()
                                            .ambiguous
                                            .push(
                                                serde_json::json!(
                                                    { "lane":& ln, "token":& tok_for_resp, "why" :
                                                    "all paths returned duplicate" }
                                                ),
                                            );
                                        let _ = pend_c
                                            .lock()
                                            .unwrap()
                                            .record(copybot_hot::pending::Pending {
                                                lane: ln.clone(),
                                                token: tok_for_resp.clone(),
                                                side: side_for_resp,
                                                order_hash: hash_c.clone(),
                                                shares: shares_for_resp,
                                                limit: limit_for_resp,
                                                ts: copybot_hot::ledger::now_secs(),
                                                why: "duplicate_only".into(),
                                                resting: otype_c == "GTC",
                                            });
                                        serde_json::json!(
                                            { "outcome" : "duplicate_only", "winner" : winner }
                                        )
                                    }
                                    Outcome::Rejected { body } => {
                                        let _ = c2
                                            .lock()
                                            .unwrap()
                                            .book_response(
                                                &ln,
                                                &tok_for_resp,
                                                side_for_resp,
                                                limit_for_resp,
                                                body,
                                                Some(his_price_for_resp),
                                                &hash_c,
                                                otype_c == "GTC",
                                            );
                                        if side_for_resp == 0 {
                                            if let Some(g) = sg_c.as_ref() {
                                                if let Err(e) = g
                                                    .release(
                                                        &salt_c,
                                                        &cond_c,
                                                        &ln,
                                                        shares_for_resp,
                                                        copybot_hot::ledger::now_secs(),
                                                    )
                                                {
                                                    eprintln!(
                                                        "[{ln}] could not release a \
                                                            rejected commitment: {e:?}"
                                                    );
                                                }
                                            }
                                            let held = router_c
                                                .snapshot()
                                                .iter()
                                                .find(|l| l.cfg.name == ln)
                                                .map(|l| l.holding(&tok_for_resp))
                                                .unwrap_or(0.0);
                                            if held <= 1e-9 {
                                                router_c.release_by_name(&tok_for_resp, &ln);
                                            }
                                        }
                                        if side_for_resp == 1 {
                                            let rescued = rescue_exit(
                                                    body,
                                                    ord_c,
                                                    pk_c,
                                                    &owner_c,
                                                    otype_c,
                                                    shares_for_resp,
                                                    full_holding,
                                                    limit_for_resp,
                                                    &paths,
                                                    &c,
                                                    creds_c.as_ref().as_ref(),
                                                    &addr_c,
                                                    &ln,
                                                    &tok_for_resp,
                                                    &c2,
                                                    &router_c,
                                                    &e2,
                                                    &wake_c,
                                                    &pend_c,
                                                    &inc_c,
                                                )
                                                .await;
                                            serde_json::json!(
                                                { "outcome" : "rejected", "rescued" : rescued }
                                            )
                                        } else {
                                            serde_json::json!({ "outcome" : "rejected" })
                                        }
                                    }
                                    Outcome::Ambiguous { detail } => {
                                        c2.lock()
                                            .unwrap()
                                            .ambiguous
                                            .push(
                                                serde_json::json!(
                                                    { "lane":& ln, "token":& tok_for_resp, "why" :
                                                    format!("no venue verdict: {detail}") }
                                                ),
                                            );
                                        let _ = pend_c
                                            .lock()
                                            .unwrap()
                                            .record(copybot_hot::pending::Pending {
                                                lane: ln.clone(),
                                                token: tok_for_resp.clone(),
                                                side: side_for_resp,
                                                order_hash: hash_c.clone(),
                                                shares: shares_for_resp,
                                                limit: limit_for_resp,
                                                ts: copybot_hot::ledger::now_secs(),
                                                why: format!("ambiguous:{detail}"),
                                                resting: otype_c == "GTC",
                                            });
                                        serde_json::json!(
                                            { "outcome" : "ambiguous", "detail" : detail, "rescued" :
                                            false }
                                        )
                                    }
                                    Outcome::NoResponse => {
                                        c2.lock()
                                            .unwrap()
                                            .ambiguous
                                            .push(
                                                serde_json::json!(
                                                    { "lane":& ln, "token":& tok_for_resp, "why" :
                                                    "no response from any path" }
                                                ),
                                            );
                                        let _ = pend_c
                                            .lock()
                                            .unwrap()
                                            .record(copybot_hot::pending::Pending {
                                                lane: ln.clone(),
                                                token: tok_for_resp.clone(),
                                                side: side_for_resp,
                                                order_hash: hash_c.clone(),
                                                shares: shares_for_resp,
                                                limit: limit_for_resp,
                                                ts: copybot_hot::ledger::now_secs(),
                                                why: "no_response".into(),
                                                resting: otype_c == "GTC",
                                            });
                                        serde_json::json!({ "outcome" : "no_response" })
                                    }
                                };
                                {
                                    let per: Vec<(String, f64)> = results
                                        .iter()
                                        .map(|r| (r.path.clone(), r.ms))
                                        .collect();
                                    let win = results
                                        .iter()
                                        .map(|r| r.ms)
                                        .fold(f64::INFINITY, f64::min);
                                    if win.is_finite() {
                                        lat_c.record_order(win, &per);
                                    }
                                }
                                let winner_path = match &outcome {
                                    Outcome::Matched { winner, .. }
                                    | Outcome::Resting { winner, .. }
                                    | Outcome::DuplicateOnly { winner } => Some(winner.clone()),
                                    _ => None,
                                };
                                let landed = matches!(
                                    & outcome, Outcome::Matched { .. } | Outcome::Resting { .. }
                                    | Outcome::DuplicateOnly { .. }
                                );
                                e2.emit(
                                    serde_json::json!(
                                        { "t" : now_ms(), "ev" : "clob_resp", "lane" : ln, "tok" :
                                        tok_for_resp, "side" : if side_for_resp == 0 { "BUY" } else
                                        { "SELL" }, "limit" : limit_for_resp, "ok" : landed, "race"
                                        : booked, "paths" : results.iter().map(| r | { let role = if
                                        Some(& r.path) == winner_path.as_ref() { "winner" } else if
                                        landed && copybot_hot::race_send::looks_duplicate(& r.body)
                                        { "dup_loser" } else { "error" }; serde_json::json!({ "path"
                                        : r.path, "ms" : r.ms, "http" : r.http, "role" : role,
                                        "body" : r.body.chars().take(120).collect::< String > () })
                                        }).collect::< Vec < _ >> (), }
                                    ),
                                );
                            });
                        }
                    }
                }
            }
        }
    }
}
#[cfg(test)]
mod http_handler_tests {
    fn req_with(ct: &str, origin: Option<&str>) -> String {
        let mut r = String::from(
            "POST /api/arm HTTP/1.1\r\nHost: box.tailnet.ts.net\r\n",
        );
        if !ct.is_empty() {
            r.push_str(&format!("Content-Type: {ct}\r\n"));
        }
        if let Some(o) = origin {
            r.push_str(&format!("Origin: {o}\r\n"));
        }
        r.push_str("\r\n{}");
        r
    }
    #[test]
    fn a_SIMPLE_cross_origin_content_type_cannot_mutate() {
        for ct in [
            "text/plain",
            "application/x-www-form-urlencoded",
            "multipart/form-data",
            "",
        ] {
            let v = super::mutation_refusal(&req_with(ct, Some("https://evil.example")));
            assert!(v.is_some(), "content-type {ct:?} must be refused");
        }
    }
    #[test]
    fn a_CROSS_ORIGIN_json_request_is_still_refused() {
        let v = super::mutation_refusal(
            &req_with("application/json", Some("https://evil.example")),
        );
        assert_eq!(v.map(| (c, _) | c), Some("403 Forbidden"));
    }
    #[test]
    fn the_DASHBOARDS_OWN_request_passes() {
        assert!(super::mutation_refusal(& req_with("application/json", None)).is_none());
        assert!(
            super::mutation_refusal(& req_with("application/json; charset=utf-8", None))
            .is_none()
        );
        assert!(
            super::mutation_refusal(& req_with("application/json",
            Some("https://box.tailnet.ts.net"))).is_none()
        );
        assert!(
            super::mutation_refusal(& req_with("application/json",
            Some("http://127.0.0.1:8807"))).is_none()
        );
    }
    #[test]
    fn header_lookup_is_case_insensitive_and_stops_at_the_body() {
        let r = "POST /x HTTP/1.1\r\nCoNtEnT-TyPe: application/json\r\n\r\nOrigin: evil";
        assert_eq!(super::header_of(r, "content-type"), Some("application/json"));
        assert_eq!(
            super::header_of(r, "origin"), None,
            "a header-shaped line in the BODY must never be read as a header"
        );
    }
    fn arm_file(tag: &str, body: &str) -> String {
        let dir = std::env::temp_dir()
            .join(format!("cb041-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("control.json.operator").to_string_lossy().into_owned();
        if !body.is_empty() {
            std::fs::write(&p, body).unwrap();
        }
        p
    }
    #[test]
    fn an_arm_write_stamps_provenance_and_REPLACES_a_stale_stamp() {
        let p = arm_file(
            "meta",
            r#"{"lanes":{"example_lane_26":{"armed":true,"halt_buys":true}},
                                     "_meta":{"by":"switch.sh","at":1,"why":"an older edit"}}"#,
        );
        let (code, _) = super::handle_arm_post(
            &p,
            r#"{"lane":"example_lane_26","state":"off"}"#,
            "test@example.com",
        );
        assert_eq!(code, "200 OK");
        let doc: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(&p).unwrap(),
            )
            .unwrap();
        assert_eq!(
            doc["_meta"] ["by"], serde_json::json!("api:/api/arm"),
            "the stale switch.sh stamp must not survive"
        );
        assert!(
            doc["_meta"] ["at"].as_i64().unwrap() > 1_700_000_000,
            "the timestamp must be current, not the stale 1"
        );
        assert!(doc["_meta"] ["why"].as_str().unwrap().contains("example_lane_26"));
        assert_eq!(doc["lanes"] ["example_lane_26"] ["armed"], serde_json::json!(false));
    }
    #[test]
    fn each_lane_KEEPS_ITS_OWN_author_when_another_lane_is_changed_after_it() {
        let p = arm_file("perlane", r#"{"lanes":{}}"#);
        super::handle_arm_post(
            &p,
            r#"{"lane":"example_lane_26","state":"halt_buys"}"#,
            "alice@example.com",
        );
        super::handle_arm_post(
            &p,
            r#"{"lane":"example_lane_25","state":"halt_buys"}"#,
            "bob@example.com",
        );
        let doc: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(&p).unwrap(),
            )
            .unwrap();
        assert_eq!(
            doc["lanes"] ["example_lane_26"] ["by"], serde_json::json!("alice@example.com"),
            "example_lane_26 must keep ITS author after example_lane_25 was changed later"
        );
        assert_eq!(doc["lanes"] ["example_lane_25"] ["by"], serde_json::json!("bob@example.com"));
        assert_eq!(doc["_meta"] ["actor"], serde_json::json!("bob@example.com"));
        for l in ["example_lane_26", "example_lane_25"] {
            assert_eq!(doc["lanes"] [l] ["armed"], serde_json::json!(true));
            assert_eq!(doc["lanes"] [l] ["halt_buys"], serde_json::json!(true));
            assert!(doc["lanes"] [l] ["at"].as_i64().unwrap() > 1_700_000_000);
        }
    }
    #[test]
    fn a_request_with_no_Cf_Access_identity_is_recorded_as_UNIDENTIFIED() {
        let a = super::actor_of("POST /api/arm HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(a.starts_with("unidentified"), "got {a:?}");
        let b = super::actor_of(
            "POST /api/arm HTTP/1.1\r\nCf-Access-Authenticated-User-Email: sam@example.com\r\n\r\n",
        );
        assert_eq!(b, "sam@example.com");
        let c = super::actor_of(
            "POST /api/arm HTTP/1.1\r\nCf-Access-Authenticated-User-Email:    \r\n\r\n",
        );
        assert!(c.starts_with("unidentified"), "got {c:?}");
    }
    #[test]
    fn provenance_does_not_disturb_the_lanes_object_the_runtime_reads() {
        let p = arm_file("metasafe", r#"{"lanes":{"example_lane_25":{"armed":true}}}"#);
        super::handle_arm_post(
            &p,
            r#"{"lane":"example_lane_26","state":"halt_buys"}"#,
            "test@example.com",
        );
        let doc: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(&p).unwrap(),
            )
            .unwrap();
        assert!(doc["lanes"].is_object());
        assert_eq!(doc["lanes"] ["example_lane_25"] ["armed"], serde_json::json!(true));
        assert_eq!(doc["lanes"] ["example_lane_26"] ["halt_buys"], serde_json::json!(true));
    }
    #[test]
    fn arming_ONE_lane_leaves_the_others_untouched() {
        let p = arm_file(
            "merge",
            r#"{"lanes":{"example_lane_26":{"armed":true,"halt_buys":true},
                                               "example_lane_25":{"armed":true,"halt_buys":false}}}"#,
        );
        let (code, _) = super::handle_arm_post(
            &p,
            r#"{"lane":"example_lane_25","state":"off"}"#,
            "test@example.com",
        );
        assert_eq!(code, "200 OK");
        let doc: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(&p).unwrap(),
            )
            .unwrap();
        assert_eq!(doc["lanes"] ["example_lane_25"] ["armed"], serde_json::json!(false));
        assert_eq!(
            doc["lanes"] ["example_lane_26"] ["armed"], serde_json::json!(true),
            "another lane's arm state must survive"
        );
        assert_eq!(
            doc["lanes"] ["example_lane_26"] ["halt_buys"], serde_json::json!(true),
            "and so must its halt_buys"
        );
    }
    #[test]
    fn an_UNREADABLE_operator_file_is_REFUSED_not_replaced() {
        let p = arm_file("corrupt", "{ this is not json");
        let (code, out) = super::handle_arm_post(
            &p,
            r#"{"lane":"example_lane_25","state":"off"}"#,
            "test@example.com",
        );
        assert_eq!(code, "409 Conflict", "{out}");
        assert_eq!(
            std::fs::read_to_string(& p).unwrap(), "{ this is not json",
            "the file must be left exactly as it was"
        );
    }
    #[test]
    fn a_MISSING_operator_file_is_the_one_case_that_starts_empty() {
        let p = arm_file("absent", "");
        let (code, _) = super::handle_arm_post(
            &p,
            r#"{"lane":"example_lane_25","state":"off"}"#,
            "test@example.com",
        );
        assert_eq!(code, "200 OK");
        let doc: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(&p).unwrap(),
            )
            .unwrap();
        assert_eq!(doc["lanes"] ["example_lane_25"] ["armed"], serde_json::json!(false));
    }
    #[test]
    fn a_document_with_NO_lanes_object_is_refused() {
        let p = arm_file("nolanes", r#"{"something":"else"}"#);
        let (code, _) = super::handle_arm_post(
            &p,
            r#"{"lane":"example_lane_25","state":"off"}"#,
            "test@example.com",
        );
        assert_eq!(code, "409 Conflict");
    }
    #[test]
    fn ARMING_still_requires_the_confirmation_phrase() {
        let p = arm_file("phrase", r#"{"lanes":{}}"#);
        let (code, _) = super::handle_arm_post(
            &p,
            r#"{"lane":"example_lane_25","state":"armed"}"#,
            "test@example.com",
        );
        assert_eq!(code, "400 Bad Request");
        let (code, _) = super::handle_arm_post(
            &p,
            r#"{"lane":"example_lane_25","state":"armed","phrase":"arm example_lane_25"}"#,
            "test@example.com",
        );
        assert_eq!(code, "200 OK");
    }
    #[test]
    fn no_DECISION_event_is_ever_rate_limited() {
        for kind in [
            "fire",
            "would_fire",
            "skip",
            "signal_guard_skip",
            "clob_resp",
            "submit_precondition_failed",
            "settle_correction",
            "pending_recovered",
            "pending_not_filled",
            "ownership_conflict",
            "recon_unattributed",
            "fire_rate_halt",
        ] {
            assert!(
                ! super::RATE_LIMITED.iter().any(| (k, _) | * k == kind),
                "{kind} carries a decision and must never be throttled"
            );
        }
    }
    #[test]
    fn the_throttled_kinds_are_the_MEASURED_noise() {
        let kinds: Vec<&str> = super::RATE_LIMITED.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec!["warm", "feed_stats", "recon_gap", "latency"]);
        for (_, gap) in super::RATE_LIMITED {
            assert!(
                * gap > 0 && * gap <= 300, "a health gap of {gap}s is not a heartbeat"
            );
        }
    }
    use super::{route_refusal, ROUTES};
    #[test]
    fn the_STATUS_route_is_the_only_one_that_falls_through() {
        assert_eq!(route_refusal("GET", "/api/status"), None);
    }
    #[test]
    fn an_UNKNOWN_path_is_404_not_the_status_document() {
        for p in ["/garbage", "/api/", "/api/statuses", "/api/status/extra", "/.env"] {
            let (code, _) = route_refusal("GET", p).expect("must refuse");
            assert!(code.starts_with("404"), "{p} -> {code}");
        }
    }
    #[test]
    fn a_PREFIX_of_a_control_route_is_NOT_that_route() {
        for p in [
            "/api/armAnything",
            "/api/arm2",
            "/api/flattenEverything",
            "/api/walletsX",
        ] {
            let (code, _) = route_refusal("POST", p).expect("must refuse");
            assert!(code.starts_with("404"), "{p} reached a control handler: {code}");
        }
    }
    #[test]
    fn a_KNOWN_path_with_the_WRONG_method_is_405() {
        assert_eq!(
            route_refusal("GET", "/api/arm").map(| (c, _) | c),
            Some("405 Method Not Allowed")
        );
        assert_eq!(
            route_refusal("POST", "/api/status").map(| (c, _) | c),
            Some("405 Method Not Allowed")
        );
    }
    #[test]
    fn every_path_the_LIVE_dashboards_and_guardian_call_is_routable() {
        for (m, p) in [
            ("GET", "/api/status"),
            ("GET", "/api/pool"),
            ("GET", "/api/matchup"),
            ("GET", "/api/equity"),
            ("GET", "/api/errors"),
            ("GET", "/api/positions"),
            ("GET", "/api/response_times"),
            ("GET", "/api/trades"),
            ("POST", "/api/wallets"),
            ("POST", "/api/arm"),
            ("POST", "/api/flatten"),
            ("GET", "/pool"),
        ] {
            assert!(ROUTES.contains(& (m, p)), "{m} {p} is not routable");
        }
    }
    use super::{
        handle_arm_post, handle_flatten_post, handle_wallet_patch, marked_position_value,
        mutation_refusal, recovery_target_shares, wallet_equity_pnl,
    };
    use copybot_hot::wallets::{WalletRegistry, WalletSpec};
    fn spec(name: &str, seed: f64, enabled: bool) -> WalletSpec {
        WalletSpec {
            name: name.into(),
            leader: format!(
                "0x{:040x}", name.bytes().fold(1u128, | a, b | a.wrapping_mul(31)
                .wrapping_add(b as u128))
            ),
            seed_usd: seed,
            leader_max_order_usd: Some(9_600.0),
            leader_peak_exposure_usd: Some(390_000.0),
            pct: 0.01,
            enabled,
            stop_mode: "none".into(),
            min_buy_price: 0.02,
            max_buy_price: 0.95,
            max_effective_pct: 0.05,
            compound: true,
            lane_id: None,
            copy_makers: false,
            copy_maker_sells: Some(false),
            exclude_political: false,
            risk: None,
            buy_slippage_c: None,
            sell_slippage_c: None,
            sell_floor_frac: None,
            min_order_usd: None,
            min_fill_floor: None,
            sell_all_frac: None,
        }
    }
    fn tmp(tag: &str) -> String {
        std::env::temp_dir()
            .join(format!("cb2_http_{tag}_{}", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }
    #[test]
    fn a_budget_edit_within_the_wallet_is_applied() {
        let p = tmp("edit");
        let reg = WalletRegistry::load_or_seed(&p, vec![spec("a", 1000.0, true)]);
        let (code, _) = handle_wallet_patch(
            &reg,
            Some(5000.0),
            &[],
            r#"{"name":"a","seed_usd":2500}"#,
        );
        assert_eq!(code, "200 OK");
        assert_eq!(reg.seed_of("a"), Some(2500.0));
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn patching_leader_on_an_EXISTING_wallet_is_refused_not_ignored() {
        let p = tmp("leader");
        let reg = WalletRegistry::load_or_seed(&p, vec![spec("a", 1000.0, true)]);
        let before = reg.specs()[0].leader.clone();
        let (code, body) = handle_wallet_patch(
            &reg,
            Some(5000.0),
            &[],
            r#"{"name":"a","leader":"0x9999999999999999999999999999999999999999"}"#,
        );
        assert_eq!(
            code, "400 Bad Request",
            "a leader change must refuse loudly, not 200-and-ignore: {body}"
        );
        assert!(body.contains("identity"), "the refusal must say WHY: {body}");
        assert_eq!(reg.specs() [0].leader, before, "and nothing may change");
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn origin_check_is_equality_not_suffix() {
        let req = |origin: &str| {
            format!(
                "POST /api/arm HTTP/1.1\r\nHost: bot.mysite.com\r\n\
             Content-Type: application/json\r\nOrigin: {origin}\r\n\r\n"
            )
        };
        assert!(mutation_refusal(& req("https://bot.mysite.com")).is_none());
        assert!(
            mutation_refusal(& req("https://evilbot.mysite.com")).is_some(),
            "a suffix-matching origin must be refused"
        );
        assert!(
            mutation_refusal(& req("null")).is_some(),
            "Origin: null has no business mutating a trading bot"
        );
        assert!(mutation_refusal(& req("http://127.0.0.1:8807")).is_none());
        assert!(mutation_refusal(& req("http://localhost:3000")).is_none());
        assert!(
            mutation_refusal(& req("https://127.0.0.1.evil.com")).is_some(),
            "a loopback-PREFIXED host is not loopback"
        );
        assert!(
            mutation_refusal("POST /api/arm HTTP/1.1\r\nHost: b\r\nContent-Type: application/json\r\n\r\n")
            .is_none()
        );
    }
    #[test]
    fn over_allocation_is_ACCEPTED_and_REPORTED_never_refused() {
        let p = tmp("over");
        let reg = WalletRegistry::load_or_seed(
            &p,
            vec![spec("a", 1000.0, true), spec("b", 1000.0, true)],
        );
        let (code, body) = handle_wallet_patch(
            &reg,
            Some(1500.0),
            &[],
            r#"{"name":"a","seed_usd":2000}"#,
        );
        assert_eq!(code, "200 OK", "over-allocation is the operator's call: {body}");
        assert_eq!(reg.seed_of("a"), Some(2000.0), "and it must actually persist");
        assert!(
            body.contains("exceed the wallet"),
            "the breach must be REPORTED in the response, not swallowed: {body}"
        );
        let (code, body) = handle_wallet_patch(
            &reg,
            Some(1500.0),
            &[],
            r#"{"name":"a","seed_usd":400}"#,
        );
        assert_eq!(code, "200 OK", "reducing an over-budget pool must work: {body}");
        assert_eq!(reg.seed_of("a"), Some(400.0));
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn an_unknown_wallet_with_no_leader_is_refused_but_with_one_is_added() {
        let p = tmp("add");
        let reg = WalletRegistry::load_or_seed(&p, vec![spec("a", 1000.0, true)]);
        let (code, _) = handle_wallet_patch(
            &reg,
            None,
            &[],
            r#"{"name":"z","seed_usd":500}"#,
        );
        assert_eq!(code, "400 Bad Request", "cannot patch a wallet that does not exist");
        let (code, body) = handle_wallet_patch(
            &reg,
            Some(9999.0),
            &[],
            &format!(
                r#"{{"name":"z","leader":"0x{:040x}","seed_usd":500,"pct":0.01}}"#, 2
            ),
        );
        assert_eq!(
            code, "400 Bad Request", "an UNMEASURED leader must be refused: {body}"
        );
        assert_eq!(reg.seed_of("z"), None, "and nothing enters the pool");
        let (code, _) = handle_wallet_patch(
            &reg,
            Some(9999.0),
            &[],
            &format!(
                r#"{{"name":"z","leader":"0x{:040x}","seed_usd":500,"pct":0.01,
                          "leader_max_order_usd":9600}}"#,
                2
            ),
        );
        assert_eq!(code, "400 Bad Request", "one of the two measurements is not enough");
        let (code, body) = handle_wallet_patch(
            &reg,
            Some(9999.0),
            &[],
            &format!(
                r#"{{"name":"z","leader":"0x{:040x}","seed_usd":500,"pct":0.01,
                          "leader_max_order_usd":9600,"leader_peak_exposure_usd":390000}}"#,
                2
            ),
        );
        assert_eq!(code, "200 OK", "a measured wallet is added: {body}");
        assert_eq!(reg.seed_of("z"), Some(500.0), "the new wallet is in the pool");
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn an_unknown_balance_no_longer_BLOCKS_an_increase() {
        let p = tmp("nobal");
        let reg = WalletRegistry::load_or_seed(&p, vec![spec("a", 1000.0, true)]);
        let (code, body) = handle_wallet_patch(
            &reg,
            None,
            &[],
            r#"{"name":"a","seed_usd":1000000}"#,
        );
        assert_eq!(
            code, "200 OK", "an unknown balance must not block an increase: {body}"
        );
        assert_eq!(reg.seed_of("a"), Some(1000000.0));
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn a_pool_INSIDE_its_wallet_gets_no_warning_at_all() {
        let p = tmp("under");
        let reg = WalletRegistry::load_or_seed(&p, vec![spec("a", 1000.0, true)]);
        let (code, body) = handle_wallet_patch(
            &reg,
            Some(50000.0),
            &[],
            r#"{"name":"a","seed_usd":2000}"#,
        );
        assert_eq!(code, "200 OK");
        assert!(
            body.contains("\"warning\":null"),
            "a pool within its wallet must carry no warning: {body}"
        );
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn one_leader_may_back_only_ONE_wallet() {
        let p = tmp("dupleader");
        let reg = WalletRegistry::load_or_seed(&p, vec![spec("a", 1000.0, true)]);
        let same = reg.specs()[0].leader.clone();
        let (code, body) = handle_wallet_patch(
            &reg,
            Some(50_000.0),
            &[],
            &format!(r#"{{"name":"clone","leader":"{same}","seed_usd":500,"pct":0.01}}"#),
        );
        assert_eq!(code, "400 Bad Request", "{body}");
        assert!(body.contains("already copied"));
        assert_eq!(reg.specs().len(), 1, "nothing was added");
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn a_BUILD_ONLY_field_is_refused_on_a_LIVE_lane_instead_of_lying() {
        let p = tmp("buildonly");
        let reg = WalletRegistry::load_or_seed(&p, vec![spec("a", 1000.0, true)]);
        let live = vec!["a".to_string()];
        let (code, body) = handle_wallet_patch(
            &reg,
            Some(50_000.0),
            &live,
            r#"{"name":"a","pct":0.25}"#,
        );
        assert_eq!(code, "400 Bad Request", "{body}");
        assert!(body.contains("Disable"), "must say how to apply it: {body}");
        assert_eq!(
            handle_wallet_patch(& reg, Some(50_000.0), & live,
            r#"{"name":"a","seed_usd":2500}"#).0, "200 OK"
        );
        assert_eq!(reg.seed_of("a"), Some(2500.0));
        let (code, body) = handle_wallet_patch(
            &reg,
            Some(50_000.0),
            &[],
            r#"{"name":"a","pct":0.02}"#,
        );
        assert_eq!(code, "200 OK", "{body}");
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn an_unknown_field_is_a_rejection_not_a_silent_success() {
        let p = tmp("unknown");
        let reg = WalletRegistry::load_or_seed(&p, vec![spec("a", 1000.0, true)]);
        let (code, body) = handle_wallet_patch(
            &reg,
            Some(50_000.0),
            &[],
            r#"{"name":"a","seed_usdd":500}"#,
        );
        assert_eq!(code, "400 Bad Request");
        assert!(body.contains("unknown field"), "{body}");
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn a_nonpositive_or_nonfinite_seed_is_refused_on_an_EXISTING_wallet_too() {
        let p = tmp("badseed");
        let reg = WalletRegistry::load_or_seed(&p, vec![spec("a", 1000.0, true)]);
        for bad in [r#"{"name":"a","seed_usd":0}"#, r#"{"name":"a","seed_usd":-5}"#] {
            assert_eq!(
                handle_wallet_patch(& reg, Some(50_000.0), & [], bad).0,
                "400 Bad Request", "{bad}"
            );
        }
        assert_eq!(reg.seed_of("a"), Some(1000.0), "unchanged throughout");
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn DISABLING_a_wallet_always_works_even_when_equity_is_unknown() {
        let p = tmp("disable_nobal");
        let reg = WalletRegistry::load_or_seed(&p, vec![spec("a", 1000.0, true)]);
        let (code, body) = handle_wallet_patch(
            &reg,
            None,
            &["a".to_string()],
            r#"{"name":"a","enabled":false}"#,
        );
        assert_eq!(
            code, "200 OK", "must be able to disable during a balance outage: {body}"
        );
        assert!(! reg.specs() [0].enabled);
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn a_malformed_wallet_body_is_refused() {
        let p = tmp("bad");
        let reg = WalletRegistry::load_or_seed(&p, vec![spec("a", 1000.0, true)]);
        assert_eq!(
            handle_wallet_patch(& reg, None, & [], "not json").0, "400 Bad Request"
        );
        assert_eq!(
            handle_wallet_patch(& reg, None, & [], r#"{"seed_usd":1}"#).0,
            "400 Bad Request"
        );
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn arming_needs_a_typed_phrase_but_stopping_never_does() {
        let p = tmp("arm");
        std::fs::remove_file(&p).ok();
        assert_eq!(
            handle_arm_post(& p, r#"{"lane":"example_lane_26","state":"off"}"#, "test@example.com")
            .0, "200 OK"
        );
        assert_eq!(
            handle_arm_post(& p, r#"{"lane":"example_lane_26","state":"halt_buys"}"#,
            "test@example.com").0, "200 OK"
        );
        assert_eq!(
            handle_arm_post(& p, r#"{"lane":"example_lane_26","state":"armed"}"#,
            "test@example.com").0, "400 Bad Request"
        );
        assert_eq!(
            handle_arm_post(& p,
            r#"{"lane":"example_lane_26","state":"armed","phrase":"arm example_lane_25"}"#,
            "test@example.com").0, "400 Bad Request"
        );
        assert_eq!(
            handle_arm_post(& p,
            r#"{"lane":"example_lane_26","state":"armed","phrase":"arm example_lane_26"}"#, "test@example.com")
            .0, "200 OK"
        );
        assert_eq!(
            handle_arm_post(& p, r#"{"lane":"example_lane_26","state":"sideways"}"#,
            "test@example.com").0, "400 Bad Request"
        );
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn the_three_states_map_onto_armed_and_halt_buys() {
        let p = tmp("armstates");
        std::fs::remove_file(&p).ok();
        let read = || -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()
        };
        handle_arm_post(
            &p,
            r#"{"lane":"example_lane_26","state":"halt_buys"}"#,
            "test@example.com",
        );
        let d = read();
        assert_eq!(
            d["lanes"] ["example_lane_26"] ["armed"], serde_json::json!(true),
            "halt keeps the lane ARMED so it still follows him OUT"
        );
        assert_eq!(d["lanes"] ["example_lane_26"] ["halt_buys"], serde_json::json!(true));
        handle_arm_post(&p, r#"{"lane":"example_lane_26","state":"off"}"#, "test@example.com");
        let d = read();
        assert_eq!(
            d["lanes"] ["example_lane_26"] ["armed"], serde_json::json!(false),
            "off disarms entirely — exits stop too, positions are HELD"
        );
        assert_eq!(d["lanes"] ["example_lane_26"] ["halt_buys"], serde_json::json!(false));
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn setting_one_lane_never_touches_another() {
        let p = tmp("armmerge");
        std::fs::remove_file(&p).ok();
        std::fs::write(&p, r#"{"lanes":{"example_lane_26":{"armed":true},"example_lane_25":{"armed":true}}}"#)
            .unwrap();
        handle_arm_post(&p, r#"{"lane":"example_lane_26","state":"off"}"#, "test@example.com");
        let d: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(&p).unwrap(),
            )
            .unwrap();
        assert_eq!(d["lanes"] ["example_lane_26"] ["armed"], serde_json::json!(false));
        assert_eq!(
            d["lanes"] ["example_lane_25"] ["armed"], serde_json::json!(true),
            "example_lane_25 must be untouched"
        );
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn a_valid_flatten_intent_is_written_and_a_bad_one_is_not() {
        let p = tmp("flat");
        let mbox = format!("{p}.a");
        std::fs::remove_file(&mbox).ok();
        let now = copybot_hot::ledger::now_secs();
        let good = format!(
            r#"{{"lane":"a","mode":"panic","phrase":"panic a","ts":{now}}}"#
        );
        let (code, _) = handle_flatten_post(&p, &good, |_| Some(false));
        assert_eq!(code, "200 OK");
        assert_eq!(
            std::fs::read_to_string(& mbox).unwrap(), good, "the exact body is persisted"
        );
        let (code, _) = handle_flatten_post(&p, "{not json}", |_| Some(false));
        assert_eq!(code, "400 Bad Request");
        let bad_mode = format!(
            r#"{{"lane":"a","mode":"nope","phrase":"x","ts":{now}}}"#
        );
        let (code, _) = handle_flatten_post(&p, &bad_mode, |_| Some(false));
        assert_eq!(code, "400 Bad Request", "an unknown mode is refused");
        assert_eq!(
            std::fs::read_to_string(& mbox).unwrap(), good,
            "the bad writes did not overwrite"
        );
        std::fs::remove_file(&mbox).ok();
    }
    #[test]
    fn an_ARMED_PANIC_is_REFUSED_AT_THE_ROUTE_not_silently_a_second_later() {
        let p = tmp("flatarmed");
        let mbox = format!("{p}.a");
        std::fs::remove_file(&mbox).ok();
        let now = copybot_hot::ledger::now_secs();
        let body = format!(
            r#"{{"lane":"a","mode":"panic","phrase":"panic a","ts":{now}}}"#
        );
        let (code, out) = handle_flatten_post(&p, &body, |_| Some(true));
        assert_eq!(
            code, "409 Conflict",
            "a 200 here IS the bug — the UI reports success and nothing sells"
        );
        assert!(
            out.contains("DISARMED"),
            "the refusal must carry the runtime's own reason, got {out}"
        );
        assert!(
            std::fs::metadata(& mbox).is_err(),
            "a request that cannot execute must not be queued at all"
        );
        let (code, _) = handle_flatten_post(&p, &body, |_| Some(false));
        assert_eq!(code, "200 OK");
        std::fs::remove_file(&mbox).ok();
    }
    #[test]
    fn an_unknown_lane_is_told_so_INSTEAD_of_being_queued_into_silence() {
        let p = tmp("flatunk");
        let now = copybot_hot::ledger::now_secs();
        let body = format!(
            r#"{{"lane":"ghost","mode":"panic","phrase":"panic ghost","ts":{now}}}"#
        );
        let (code, out) = handle_flatten_post(&p, &body, |_| None);
        assert_eq!(code, "404 Not Found");
        assert!(out.contains("nothing has been sold"), "got {out}");
        assert!(std::fs::metadata(format!("{p}.ghost")).is_err());
    }
    #[test]
    fn two_lanes_flattened_in_the_SAME_SECOND_both_survive() {
        let p = tmp("flatbox");
        let now = copybot_hot::ledger::now_secs();
        let a = format!(
            r#"{{"lane":"example_lane_26","mode":"panic","phrase":"panic example_lane_26","ts":{now}}}"#
        );
        let b = format!(
            r#"{{"lane":"example_lane_25","mode":"panic","phrase":"panic example_lane_25","ts":{now}}}"#
        );
        std::fs::remove_file(format!("{p}.example_lane_26")).ok();
        std::fs::remove_file(format!("{p}.example_lane_25")).ok();
        assert_eq!(handle_flatten_post(& p, & a, | _ | Some(false)).0, "200 OK");
        assert_eq!(handle_flatten_post(& p, & b, | _ | Some(false)).0, "200 OK");
        assert_eq!(
            std::fs::read_to_string(format!("{p}.example_lane_26")).unwrap(), a,
            "example_lane_26's intent was erased by example_lane_25's — one wallet would not flatten"
        );
        assert_eq!(std::fs::read_to_string(format!("{p}.example_lane_25")).unwrap(), b);
        std::fs::remove_file(format!("{p}.example_lane_26")).ok();
        std::fs::remove_file(format!("{p}.example_lane_25")).ok();
    }
    #[test]
    fn a_lane_name_can_never_CHOOSE_THE_FILE_the_route_writes() {
        let p = tmp("flattrav");
        let now = copybot_hot::ledger::now_secs();
        for bad in ["../../etc/passwd", "a/b", "", "with space", &"x".repeat(33)] {
            let body = format!(
                r#"{{"lane":"{bad}","mode":"panic","phrase":"panic {bad}","ts":{now}}}"#
            );
            let (code, _) = handle_flatten_post(&p, &body, |_| Some(false));
            assert_eq!(code, "400 Bad Request", "lane {bad:?} must be refused");
        }
    }
    #[test]
    fn wallet_equity_change_uses_total_equity_not_free_cash() {
        assert_eq!(wallet_equity_pnl(Some(10_254.43), 10_000.0), Some(254.43));
        assert_eq!(wallet_equity_pnl(None, 10_000.0), None);
        let marked = marked_position_value(
            &serde_json::json!(
                [{ "currentValue" : 600.25 }, { "currentValue" : "291.47" }]
            ),
        );
        assert_eq!(marked, Some(891.72));
        assert_eq!(
            wallet_equity_pnl(Some(9_362.71 + marked.unwrap()), 10_000.0), Some(254.43)
        );
    }
    #[test]
    fn recovery_honours_a_FIXED_RATE_lane_instead_of_assuming_compounding() {
        let (leader, l_px, our_px, pct, min_usd, cap) = (
            1_000.0,
            0.40,
            0.50,
            0.015,
            1.0,
            0.05,
        );
        let scale = 1.50;
        let compounding = recovery_target_shares(
                leader,
                l_px,
                our_px,
                pct,
                scale,
                min_usd,
                cap,
                true,
            )
            .expect("compounding target");
        let fixed = recovery_target_shares(
                leader,
                l_px,
                our_px,
                pct,
                scale,
                min_usd,
                cap,
                false,
            )
            .expect("fixed target");
        assert!(
            compounding > fixed,
            "a compounding lane should target MORE after gains: {compounding} vs {fixed}"
        );
        let flat = recovery_target_shares(
                leader,
                l_px,
                our_px,
                pct,
                1.0,
                min_usd,
                cap,
                false,
            )
            .expect("unscaled target");
        assert!(
            (fixed - flat).abs() < 1e-9,
            "compound=false must ignore cap_scale entirely: {fixed} vs {flat}"
        );
    }
    #[test]
    fn recovery_and_the_hot_path_agree_on_the_same_lane() {
        for compound in [true, false] {
            for scale in [0.75_f64, 1.0, 1.4] {
                let hot = copybot_hot::budget::effective_pct(
                    0.015,
                    scale,
                    0.05,
                    compound,
                );
                let rec = recovery_target_shares(
                        1_000.0,
                        0.50,
                        0.50,
                        0.015,
                        scale,
                        1.0,
                        0.05,
                        compound,
                    )
                    .expect("target");
                let implied = (rec - 1.0) * 0.50 / (1_000.0 * 0.50);
                assert!(
                    (implied - hot).abs() < 1e-9,
                    "compound={compound} scale={scale}: recovery implies {implied}, hot path {hot}"
                );
            }
        }
    }
    #[test]
    fn restart_recovery_is_sell_only_and_leader_flat_is_unambiguous() {
        let target = recovery_target_shares(
                0.0,
                0.0,
                0.59,
                0.015,
                1.001084,
                1.0,
                0.05,
                true,
            )
            .expect("leader-flat target");
        assert_eq!(target, 0.0);
        assert!(
            (copybot_hot::sweep::excess_shares(20.816325, 20.8328, target) - 20.816325)
            .abs() < 1e-9
        );
        assert_eq!(
            copybot_hot::sweep::excess_shares(10.0, 7.0, 0.0), 7.0,
            "the physical wallet is an availability ceiling"
        );
        assert_eq!(
            copybot_hot::sweep::excess_shares(5.0, 5.0, 8.0), 0.0,
            "normalization must never create a buy requirement"
        );
    }
    #[test]
    fn restart_recovery_preserves_dollar_ratio_and_entry_rounding() {
        let target = recovery_target_shares(
                1_000.0,
                0.40,
                0.50,
                0.015,
                1.0,
                1.0,
                0.05,
                true,
            )
            .expect("priced target");
        assert!((target - 13.0).abs() < 1e-9);
        assert_eq!(copybot_hot::sweep::excess_shares(13.0, 13.0, target), 0.0);
        assert!(
            recovery_target_shares(100.0, 0.0, 0.5, 0.015, 1.0, 1.0, 0.05, true)
            .is_none(),
            "unknown cost basis must fail closed while the leader is still in"
        );
    }
    #[test]
    fn the_incident_clear_route_is_REGISTERED_and_phrase_gated() {
        assert_eq!(
            route_refusal("GET", "/api/incident/clear").map(| (c, _) | c),
            Some("405 Method Not Allowed"),
            "a registered path must 405 on the wrong method, never 404"
        );
        assert!(
            ROUTES.contains(& ("POST", "/api/incident/clear")),
            "the clear route must stay registered, or a safety latch becomes \
                 permanent and the only recovery is editing files by hand"
        );
    }
}
