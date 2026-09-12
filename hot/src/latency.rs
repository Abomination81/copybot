use crate::digest::Digest;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
#[derive(Default)]
pub struct Latency {
    series: Mutex<HashMap<String, Digest>>,
}
impl Latency {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn record(&self, name: &str, ms: f64) {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let mut g = self.series.lock().unwrap();
        g.entry(name.to_string()).or_default().push(ms, at);
    }
    pub fn p50(&self, name: &str) -> Option<f64> {
        let g = self.series.lock().unwrap();
        let d = g.get(name)?;
        if d.n() == 0 {
            return None;
        }
        Some(d.pct(0.50))
    }
    pub fn record_order(&self, winner_ms: f64, per_path: &[(String, f64)]) {
        self.record("order", winner_ms);
        for (path, ms) in per_path {
            self.record(&format!("path.{path}"), *ms);
        }
    }
    pub fn stats(&self, name: &str) -> Option<serde_json::Value> {
        let g = self.series.lock().unwrap();
        let d = g.get(name)?;
        if d.n() == 0 {
            return None;
        }
        Some(d.json())
    }
    pub fn venue_ms(&self) -> Option<f64> {
        Some(self.p50("order")? - self.p50("baseline")?)
    }
    pub fn snapshot(&self) -> serde_json::Value {
        let names: Vec<String> = {
            let g = self.series.lock().unwrap();
            let mut n: Vec<String> = g.keys().cloned().collect();
            n.sort();
            n
        };
        let mut out = serde_json::Map::new();
        for n in names {
            if let Some(s) = self.stats(&n) {
                out.insert(n, s);
            }
        }
        let venue = self.venue_ms();
        serde_json::json!(
            { "series" : out, "venue_ms" : venue.map(| v | (v * 100.0).round() / 100.0),
            "note" :
            "order/baseline/path.* are RAW RTT and are NETWORK-BOUND — only \
                     comparable within one machine+route. venue_ms is the differential.",
            }
        )
    }
}
pub async fn spawn_baseline_probe(
    lat: std::sync::Arc<Latency>,
    http: reqwest::Client,
    clob_host: String,
    every: Duration,
) {
    let url = format!("{}/time", clob_host.trim_end_matches('/'));
    loop {
        let t0 = std::time::Instant::now();
        match tokio::time::timeout(Duration::from_secs(10), http.get(&url).send()).await
        {
            Ok(Ok(resp)) => {
                let ok = resp.status().is_success();
                let _ = resp.bytes().await;
                if ok {
                    lat.record("baseline", t0.elapsed().as_micros() as f64 / 1000.0);
                } else {
                    lat.record("baseline.err", 1.0);
                }
            }
            Ok(Err(_)) | Err(_) => lat.record("baseline.err", 1.0),
        }
        tokio::time::sleep(every).await;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn venue_is_the_difference_of_medians() {
        let l = Latency::new();
        for _ in 0..60 {
            l.record("order", 200.0);
            l.record("baseline", 175.0);
        }
        let v = l.venue_ms().unwrap();
        assert!(
            v > 0.0, "order is slower than baseline; venue_ms must be positive: {v}"
        );
        assert!(v < 100.0, "implausible differential: {v}");
    }
    #[test]
    fn venue_is_none_until_BOTH_series_exist() {
        let l = Latency::new();
        assert!(l.venue_ms().is_none());
        l.record("order", 200.0);
        assert!(l.venue_ms().is_none(), "order alone must not yield a venue estimate");
        l.record("baseline", 175.0);
        assert!(l.venue_ms().is_some());
    }
    #[test]
    fn venue_may_be_NEGATIVE_and_is_reported_not_clamped() {
        let l = Latency::new();
        for _ in 0..60 {
            l.record("order", 44.0);
            l.record("baseline", 179.0);
        }
        assert!(l.venue_ms().unwrap() < 0.0);
    }
    #[test]
    fn per_path_rtt_is_tracked_separately_so_one_sick_route_is_visible() {
        let l = Latency::new();
        for _ in 0..40 {
            l.record_order(20.0, &[("h1".into(), 20.0), ("h2".into(), 95.0)]);
        }
        let h1 = l.p50("path.h1").unwrap();
        let h2 = l.p50("path.h2").unwrap();
        assert!(h2 > h1 * 2.0, "the slow path must stand out: h1={h1} h2={h2}");
        assert!(
            (l.p50("order").unwrap() - h1).abs() < h1 * 0.25,
            "the winner's RTT is the fast path"
        );
    }
    #[test]
    fn snapshot_labels_raw_rtt_as_network_bound() {
        let l = Latency::new();
        l.record("order", 200.0);
        l.record("baseline", 175.0);
        let s = l.snapshot();
        assert!(
            s["note"].as_str().unwrap().contains("NETWORK-BOUND"),
            "the snapshot must warn that raw RTT is not portable"
        );
        assert!(s["series"] ["order"] ["n"].as_u64().unwrap() >= 1);
    }
    #[test]
    fn an_empty_tracker_reports_nothing_rather_than_zero() {
        let l = Latency::new();
        assert!(l.stats("order").is_none());
        assert!(l.snapshot() ["venue_ms"].is_null());
    }
    #[test]
    fn a_bad_sample_cannot_poison_the_series() {
        let l = Latency::new();
        l.record("x", 10.0);
        l.record("x", f64::NAN);
        l.record("x", -5.0);
        assert_eq!(l.stats("x").unwrap() ["n"], 1);
    }
    #[test]
    fn memory_does_not_grow_with_sample_count() {
        let l = Latency::new();
        for i in 0..100_000 {
            l.record("order", 20.0 + (i % 5) as f64);
        }
        assert_eq!(l.stats("order").unwrap() ["n"], 100_000);
        let kept = l
            .stats("order")
            .unwrap()["recent_outliers"]
            .as_array()
            .unwrap()
            .len();
        assert!(kept <= 8, "snapshot carries only the highlights");
    }
    #[test]
    fn a_slow_spike_is_retained_as_a_highlight() {
        let l = Latency::new();
        for _ in 0..200 {
            l.record("order", 20.0);
        }
        l.record("order", 5000.0);
        let s = l.stats("order").unwrap();
        assert!(s["outliers"].as_u64().unwrap() >= 1, "the spike must be flagged");
        let recent = s["recent_outliers"].as_array().unwrap();
        assert!(
            recent.iter().any(| o | o["ms"].as_f64().unwrap() > 4000.0),
            "the spike must be individually recoverable: {recent:?}"
        );
    }
}
