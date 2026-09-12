use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
pub const CAP: usize = 64;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Sample {
    pub t: i64,
    pub lane: String,
    pub token: String,
    pub side: u8,
    pub us: f64,
}
#[derive(Default)]
pub struct Ring {
    inner: Mutex<VecDeque<Sample>>,
    path: Option<PathBuf>,
}
impl Ring {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(CAP)),
            path: None,
        }
    }
    pub fn with_file(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut back = VecDeque::with_capacity(CAP);
        if let Ok(raw) = std::fs::read_to_string(&path) {
            for line in raw.lines().rev().take(CAP) {
                if let Ok(s) = serde_json::from_str::<Sample>(line.trim()) {
                    back.push_front(s);
                }
            }
        }
        Self {
            inner: Mutex::new(back),
            path: Some(path),
        }
    }
    pub fn push(&self, s: Sample) {
        if let Some(p) = &self.path {
            if let Ok(line) = serde_json::to_string(&s) {
                let _ = (|| -> std::io::Result<()> {
                    use std::io::Write;
                    let mut f = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(p)?;
                    writeln!(f, "{line}")
                })();
            }
        }
        let mut g = self.inner.lock().unwrap();
        g.push_back(s);
        while g.len() > CAP {
            g.pop_front();
        }
    }
    pub fn compact(&self) -> std::io::Result<()> {
        let Some(p) = &self.path else { return Ok(()) };
        let keep: Vec<Sample> = { self.inner.lock().unwrap().iter().cloned().collect() };
        let tmp = p.with_extension("tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            for s in &keep {
                if let Ok(l) = serde_json::to_string(s) {
                    writeln!(f, "{l}")?;
                }
            }
        }
        std::fs::rename(&tmp, p)
    }
    pub fn tail(&self, n: usize) -> Vec<Sample> {
        let g = self.inner.lock().unwrap();
        g.iter().rev().take(n).cloned().collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn tmp_rt(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir()
            .join(format!("cb2_rt_{tag}_{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }
    #[test]
    fn samples_SURVIVE_a_restart() {
        let p = tmp_rt("survive");
        {
            let r = Ring::with_file(&p);
            r.push(sample(1, 100.0));
            r.push(sample(2, 200.0));
        }
        let reborn = Ring::with_file(&p);
        let got = reborn.tail(10);
        assert_eq!(got.len(), 2, "a restart must not blank the chart");
        assert_eq!(got[0].t, 2, "newest first, order preserved across the reload");
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn a_reload_keeps_only_the_live_WINDOW_however_long_the_file_is() {
        let p = tmp_rt("window");
        {
            let r = Ring::with_file(&p);
            for i in 0..(CAP as i64 + 40) {
                r.push(sample(i, i as f64));
            }
        }
        let reborn = Ring::with_file(&p);
        assert_eq!(reborn.tail(1000).len(), CAP, "the ring stays bounded on reload");
        assert_eq!(reborn.tail(1) [0].t, CAP as i64 + 39, "and keeps the NEWEST");
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn compact_bounds_the_file_so_it_cannot_grow_forever() {
        let p = tmp_rt("compact");
        let r = Ring::with_file(&p);
        for i in 0..(CAP as i64 + 100) {
            r.push(sample(i, i as f64));
        }
        let before = std::fs::read_to_string(&p).unwrap().lines().count();
        r.compact().unwrap();
        let after = std::fs::read_to_string(&p).unwrap().lines().count();
        assert_eq!(before, CAP + 100, "append-only until compacted");
        assert_eq!(after, CAP, "compaction trims to the live window");
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn a_CORRUPT_or_missing_file_is_an_empty_chart_not_a_refusal_to_boot() {
        let p = tmp_rt("torn");
        std::fs::write(
                &p,
                "{\"t\":1,\"lane\":\"a\",\"token\":\"T\",\"side\":0,\"us\":9.0}\n                            {not json at all\n",
            )
            .unwrap();
        let r = Ring::with_file(&p);
        assert_eq!(
            r.tail(10).len(), 1, "the intact sample survives, the torn one is skipped"
        );
        let gone = Ring::with_file(tmp_rt("absent"));
        assert!(gone.tail(10).is_empty());
    }
    #[test]
    fn a_ring_with_NO_file_still_works_and_writes_nothing() {
        let r = Ring::new();
        r.push(sample(1, 10.0));
        assert_eq!(r.tail(5).len(), 1);
        assert!(r.compact().is_ok(), "compaction on a fileless ring is a no-op");
    }
    fn sample(t: i64, us: f64) -> Sample {
        Sample {
            t,
            lane: "example_lane_26".into(),
            token: "T".into(),
            side: 0,
            us,
        }
    }
    #[test]
    fn newest_comes_first() {
        let r = Ring::new();
        r.push(sample(1, 100.0));
        r.push(sample(2, 200.0));
        r.push(sample(3, 300.0));
        let got = r.tail(10);
        assert_eq!(got.iter().map(| s | s.t).collect::< Vec < _ >> (), vec![3, 2, 1]);
    }
    #[test]
    fn old_samples_are_evicted_once_the_ring_is_full() {
        let r = Ring::new();
        for i in 0..(CAP + 10) {
            r.push(sample(i as i64, i as f64));
        }
        let got = r.tail(CAP + 10);
        assert_eq!(got.len(), CAP, "must never grow past the cap");
        assert_eq!(got[0].t, (CAP + 9) as i64, "newest survives");
        assert_eq!(
            * got.last().unwrap(), sample(10, 10.0), "oldest evicted ones are gone"
        );
    }
    #[test]
    fn an_empty_ring_returns_empty_not_a_panic() {
        assert!(Ring::new().tail(10).is_empty());
    }
    #[test]
    fn tail_never_returns_more_than_asked() {
        let r = Ring::new();
        for i in 0..20 {
            r.push(sample(i, i as f64));
        }
        assert_eq!(r.tail(5).len(), 5);
    }
}
