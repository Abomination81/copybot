use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
#[derive(Debug)]
pub struct RawTx {
    pub source: String,
    pub hash: String,
    pub input: Vec<u8>,
    pub to_neg_risk: bool,
    pub to: String,
    pub seen_ns: u128,
}
#[derive(Debug, Clone)]
pub struct FeedConfig {
    pub name: String,
    pub url: String,
    pub sockets: usize,
}
pub const CTF_EXCHANGE_V2: &str = "e111180000d2663c0091e4f400237545b87b996b";
pub const NEG_RISK_CTF_EXCHANGE_V2: &str = "e2222d279d744050d28e00520010520000310f59";
#[derive(Debug, Default)]
pub struct WatchedAddresses {
    inner: std::sync::RwLock<Vec<String>>,
}
impl WatchedAddresses {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(&self, addrs: impl IntoIterator<Item = String>) {
        let v: Vec<String> = addrs
            .into_iter()
            .map(|a| a.trim().trim_start_matches("0x").to_ascii_lowercase())
            .filter(|a| a.len() == 40 && a.chars().all(|c| c.is_ascii_hexdigit()))
            .collect();
        if let Ok(mut g) = self.inner.write() {
            *g = v;
        }
    }
    pub fn snapshot(&self) -> Vec<String> {
        self.inner.read().map(|g| g.clone()).unwrap_or_default()
    }
    pub fn is_empty(&self) -> bool {
        self.inner.read().map(|g| g.is_empty()).unwrap_or(true)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxKind {
    Exchange,
    NegRiskExchange,
    Watched,
}
pub fn classify_to(to_lower: &str, watch: &[String]) -> Option<TxKind> {
    if to_lower == CTF_EXCHANGE_V2 {
        return Some(TxKind::Exchange);
    }
    if to_lower == NEG_RISK_CTF_EXCHANGE_V2 {
        return Some(TxKind::NegRiskExchange);
    }
    if !to_lower.is_empty() && watch.iter().any(|w| w == to_lower) {
        return Some(TxKind::Watched);
    }
    None
}
pub const WATCH_REFRESH_FRAMES: u32 = 128;
const PROVE_TIMEOUT: Duration = Duration::from_secs(15);
#[derive(Default, Debug)]
pub struct FeedStats {
    pub frames: AtomicU64,
    pub exchange_txs: AtomicU64,
    pub reconnects: AtomicU64,
    pub errors: AtomicU64,
    pub wins: AtomicU64,
    pub recycle: std::sync::atomic::AtomicBool,
}
pub struct FeedRegistry {
    pub per: std::collections::HashMap<String, Arc<FeedStats>>,
    pub total: Arc<FeedStats>,
}
impl FeedRegistry {
    pub fn snapshot(&self) -> Vec<(String, u64, u64, u64, u64)> {
        let mut v: Vec<_> = self
            .per
            .iter()
            .map(|(k, s)| {
                (
                    k.clone(),
                    s.frames.load(Ordering::Relaxed),
                    s.exchange_txs.load(Ordering::Relaxed),
                    s.reconnects.load(Ordering::Relaxed),
                    s.errors.load(Ordering::Relaxed),
                )
            })
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
    pub fn record_win(&self, socket: &str) {
        if let Some(s) = self.per.get(socket) {
            s.wins.fetch_add(1, Ordering::Relaxed);
            self.total.wins.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn wins_snapshot(&self) -> Vec<(String, u64)> {
        let mut v: Vec<_> = self
            .per
            .iter()
            .map(|(k, s)| (k.clone(), s.wins.load(Ordering::Relaxed)))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }
    pub fn score_and_recycle(&self, keep_top: usize, min_sample: u64) -> Vec<String> {
        let ranked = self.wins_snapshot();
        let losers = pick_losers(&ranked, keep_top, min_sample);
        for name in &losers {
            if let Some(s) = self.per.get(name) {
                s.recycle.store(true, Ordering::Relaxed);
            }
        }
        for (_, s) in self.per.iter() {
            s.wins.store(0, Ordering::Relaxed);
        }
        losers
    }
}
pub fn pick_losers(
    ranked: &[(String, u64)],
    keep_top: usize,
    min_sample: u64,
) -> Vec<String> {
    let total: u64 = ranked.iter().map(|(_, w)| *w).sum();
    if total < min_sample || ranked.len() <= keep_top {
        return Vec::new();
    }
    let even = total as f64 / ranked.len() as f64;
    ranked
        .iter()
        .enumerate()
        .filter(|(i, (_, w))| *i >= keep_top && (*w as f64) < even * 0.5)
        .map(|(_, (n, _))| n.clone())
        .collect()
}
#[derive(Clone)]
pub struct StatsSink {
    own: Arc<FeedStats>,
    total: Arc<FeedStats>,
}
impl StatsSink {
    fn bump2(a: &AtomicU64, b: &AtomicU64) {
        a.fetch_add(1, Ordering::Relaxed);
        b.fetch_add(1, Ordering::Relaxed);
    }
    pub fn frame(&self) {
        Self::bump2(&self.own.frames, &self.total.frames)
    }
    pub fn exchange_tx(&self) {
        Self::bump2(&self.own.exchange_txs, &self.total.exchange_txs)
    }
    pub fn reconnect(&self) {
        Self::bump2(&self.own.reconnects, &self.total.reconnects)
    }
    pub fn error(&self) {
        Self::bump2(&self.own.errors, &self.total.errors)
    }
    pub fn win(&self) {
        Self::bump2(&self.own.wins, &self.total.wins)
    }
    fn take_recycle(&self) -> bool {
        self.own.recycle.swap(false, Ordering::Relaxed)
    }
}
pub fn spawn_all(
    feeds: &[FeedConfig],
    tx: mpsc::UnboundedSender<RawTx>,
    total: Arc<FeedStats>,
    watched: Arc<WatchedAddresses>,
) -> (Vec<tokio::task::JoinHandle<()>>, FeedRegistry) {
    let mut handles = Vec::new();
    let mut per = std::collections::HashMap::new();
    for f in feeds {
        for i in 0..f.sockets {
            let name = format!("{}{}", f.name, i);
            let own = Arc::new(FeedStats::default());
            per.insert(name.clone(), own.clone());
            let sink = StatsSink {
                own,
                total: total.clone(),
            };
            let url = f.url.clone();
            let tx = tx.clone();
            let watched = watched.clone();
            handles
                .push(
                    tokio::spawn(async move {
                        run_socket(name, url, tx, sink, watched).await;
                    }),
                );
        }
    }
    (handles, FeedRegistry { per, total })
}
pub fn backoff_ms(consecutive_failures: u32) -> u64 {
    match consecutive_failures {
        0 => 1_500,
        1..=3 => 2_000,
        4..=8 => 10_000,
        _ => 60_000,
    }
}
pub fn escalates(ran: Duration, ended_ok: bool, healthy: Duration) -> bool {
    !ended_ok && ran < healthy
}
async fn run_socket(
    name: String,
    url: String,
    tx: mpsc::UnboundedSender<RawTx>,
    stats: StatsSink,
    watched: Arc<WatchedAddresses>,
) {
    const HEALTHY_SESSION: Duration = Duration::from_secs(60);
    let mut consecutive_failures: u32 = 0;
    loop {
        let started = std::time::Instant::now();
        let outcome = pump(&name, &url, &tx, &stats, watched.clone()).await;
        let ran = started.elapsed();
        let ended_ok = outcome.is_ok();
        if escalates(ran, ended_ok, HEALTHY_SESSION) {
            consecutive_failures = consecutive_failures.saturating_add(1);
            stats.error();
            if let Err(e) = &outcome {
                if consecutive_failures <= 3 {
                    eprintln!("[{name}] {e}");
                } else if consecutive_failures == 4 {
                    eprintln!("[{name}] repeated failures — backing off, silencing");
                }
            }
        } else {
            consecutive_failures = 0;
            if ended_ok {
                eprintln!("[{name}] stream ended cleanly");
            }
        }
        stats.reconnect();
        tokio::time::sleep(Duration::from_millis(backoff_ms(consecutive_failures)))
            .await;
    }
}
async fn pump(
    name: &str,
    url: &str,
    out: &mpsc::UnboundedSender<RawTx>,
    stats: &StatsSink,
    watched: Arc<WatchedAddresses>,
) -> Result<(), String> {
    let mut watch_list: Vec<String> = watched.snapshot();
    let mut frames_since_watch_refresh: u32 = 0;
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let sub = if url.contains("alchemy") {
        let mut to: Vec<String> = vec![
            format!("0x{CTF_EXCHANGE_V2}"), format!("0x{NEG_RISK_CTF_EXCHANGE_V2}")
        ];
        to.extend(watched.snapshot().into_iter().map(|a| format!("0x{a}")));
        serde_json::json!(
            { "jsonrpc" : "2.0", "id" : 1, "method" : "eth_subscribe", "params" :
            ["alchemy_pendingTransactions", { "toAddress" : to }] }
        )
    } else {
        serde_json::json!(
            { "jsonrpc" : "2.0", "id" : 1, "method" : "eth_subscribe", "params" :
            ["newPendingTransactions", true] }
        )
    };
    ws.send(Message::Text(sub.to_string()))
        .await
        .map_err(|e| format!("subscribe send: {e}"))?;
    match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => {
            if t.contains("\"error\"") {
                return Err(
                    format!("subscription rejected: {}", & t[..t.len().min(120)]),
                );
            }
        }
        Ok(Some(Ok(_))) => {}
        Ok(Some(Err(e))) => return Err(format!("ack: {e}")),
        Ok(None) => return Err("closed before ack".into()),
        Err(_) => return Err("ack timeout".into()),
    }
    let mut proved = false;
    loop {
        let next = tokio::time::timeout(
                if proved { Duration::from_secs(90) } else { PROVE_TIMEOUT },
                ws.next(),
            )
            .await;
        if stats.take_recycle() {
            return Ok(());
        }
        let msg = match next {
            Err(_) if !proved => {
                return Err("subscribed but delivered NOTHING — black hole".into());
            }
            Err(_) => return Err("silent past 90s".into()),
            Ok(None) => return Ok(()),
            Ok(Some(Err(e))) => return Err(format!("recv: {e}")),
            Ok(Some(Ok(m))) => m,
        };
        let raw = match msg {
            Message::Text(t) => t,
            Message::Ping(p) => {
                if ws.send(Message::Pong(p)).await.is_err() {
                    return Err("pong send failed".into());
                }
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => return Ok(()),
            _ => continue,
        };
        proved = true;
        stats.frame();
        frames_since_watch_refresh += 1;
        if frames_since_watch_refresh >= WATCH_REFRESH_FRAMES {
            frames_since_watch_refresh = 0;
            watch_list = watched.snapshot();
        }
        let lower_has = |needle: &str| {
            raw
                .as_bytes()
                .windows(needle.len())
                .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
        };
        let is_ctf = lower_has(CTF_EXCHANGE_V2);
        let is_neg = if is_ctf { false } else { lower_has(NEG_RISK_CTF_EXCHANGE_V2) };
        let is_watched = if is_ctf || is_neg {
            false
        } else {
            watch_list.iter().any(|a| lower_has(a))
        };
        if !is_ctf && !is_neg && !is_watched {
            continue;
        }
        let seen_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let v: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let r = &v["params"]["result"];
        let to = r["to"]
            .as_str()
            .unwrap_or("")
            .trim_start_matches("0x")
            .to_ascii_lowercase();
        let Some(kind) = classify_to(&to, &watch_list) else {
            continue;
        };
        let neg = kind == TxKind::NegRiskExchange;
        if kind != TxKind::Watched {
            stats.exchange_tx();
        }
        let (hash, input) = (
            r["hash"].as_str().unwrap_or(""),
            r["input"].as_str().unwrap_or(""),
        );
        if hash.is_empty() || input.len() < 10 {
            continue;
        }
        let bytes = match hex::decode(input.trim_start_matches("0x")) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if out
            .send(RawTx {
                source: name.to_string(),
                hash: hash.to_string(),
                input: bytes,
                to_neg_risk: neg,
                to: to.clone(),
                seen_ns,
            })
            .is_err()
        {
            return Ok(());
        }
    }
}
