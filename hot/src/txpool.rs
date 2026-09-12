use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use crate::feeds::RawTx;
use tokio::sync::mpsc;
#[derive(Default, Debug)]
pub struct TxpoolStats {
    pub polls: AtomicU64,
    pub errors: AtomicU64,
    pub seen: AtomicU64,
    pub rescued: AtomicU64,
}
pub async fn run(
    url: String,
    tx: mpsc::UnboundedSender<RawTx>,
    stats: Arc<TxpoolStats>,
    every: Duration,
) {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("http");
    loop {
        tokio::time::sleep(every).await;
        stats.polls.fetch_add(1, Ordering::Relaxed);
        let body = serde_json::json!(
            { "jsonrpc" : "2.0", "id" : 1, "method" : "txpool_content", "params" : [] }
        );
        let resp = match http.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(_) => {
                stats.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let v: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => {
                stats.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let pending = &v["result"]["pending"];
        let Some(accounts) = pending.as_object() else {
            stats.errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        for (_from, nonces) in accounts {
            let Some(by_nonce) = nonces.as_object() else { continue };
            for (_n, t) in by_nonce {
                let to = t["to"].as_str().unwrap_or("").to_ascii_lowercase();
                let is_neg = to == crate::feeds::NEG_RISK_CTF_EXCHANGE_V2
                    || to == format!("0x{}", crate ::feeds::NEG_RISK_CTF_EXCHANGE_V2);
                let is_ctf = to == crate::feeds::CTF_EXCHANGE_V2
                    || to == format!("0x{}", crate ::feeds::CTF_EXCHANGE_V2);
                if !is_ctf && !is_neg {
                    continue;
                }
                let (Some(h), Some(inp)) = (t["hash"].as_str(), t["input"].as_str())
                else { continue };
                let Ok(bytes) = hex::decode(inp.trim_start_matches("0x")) else {
                    continue
                };
                stats.seen.fetch_add(1, Ordering::Relaxed);
                if tx
                    .send(RawTx {
                        source: "txpool".into(),
                        hash: h.to_string(),
                        input: bytes,
                        to_neg_risk: is_neg,
                        to: to.trim_start_matches("0x").to_ascii_lowercase(),
                        seen_ns: now_ns,
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}
