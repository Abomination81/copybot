use serde::{Deserialize, Serialize};
pub const MAX_ROWS: usize = 400;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorRow {
    pub t: i64,
    pub lane: String,
    pub kind: String,
    pub detail: String,
    pub human: String,
    pub severity: String,
}
pub fn rank(severity: &str) -> u8 {
    match severity {
        "stop" => 0,
        "warn" => 1,
        "notice" => 2,
        _ => 3,
    }
}
pub fn record(path: &str, row: &ErrorRow) {
    use std::io::Write;
    let Ok(line) = serde_json::to_string(row) else { return };
    if let Some(dir) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _guard = LockFile::acquire(path);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
    trim_locked(path);
}
pub struct LockFile(Option<std::fs::File>);
impl LockFile {
    pub fn acquire(path: &str) -> Self {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(format!("{path}.lock"))
            .ok();
        if let Some(f) = &f {
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
        }
        LockFile(f)
    }
}
impl Drop for LockFile {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        if let Some(f) = &self.0 {
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}
fn trim_locked(path: &str) {
    let Ok(raw) = std::fs::read_to_string(path) else { return };
    let lines: Vec<&str> = raw.lines().collect();
    if lines.len() <= MAX_ROWS * 2 {
        return;
    }
    let keep = lines[lines.len() - MAX_ROWS..].join("\n");
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, format!("{keep}\n")).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}
pub fn tail(path: &str, n: usize) -> Vec<ErrorRow> {
    let Ok(raw) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut rows: Vec<ErrorRow> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    rows.reverse();
    rows.truncate(n);
    rows
}
pub const ACTIVE_WINDOW_SECS: i64 = 6 * 60 * 60;
pub fn counts_by_lane(rows: &[ErrorRow]) -> Vec<(String, usize, usize, usize)> {
    counts_by_lane_since(rows, i64::MIN)
}
pub fn counts_by_lane_since(
    rows: &[ErrorRow],
    cutoff: i64,
) -> Vec<(String, usize, usize, usize)> {
    let mut m: std::collections::BTreeMap<String, (usize, usize, usize)> = Default::default();
    for r in rows.iter().filter(|r| r.t >= cutoff) {
        let e = m.entry(r.lane.clone()).or_default();
        match r.severity.as_str() {
            "stop" => e.0 += 1,
            "warn" => e.1 += 1,
            _ => e.2 += 1,
        }
    }
    m.into_iter().map(|(k, v)| (k, v.0, v.1, v.2)).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    fn tmp(tag: &str) -> String {
        std::env::temp_dir()
            .join(format!("cb2_err_{tag}_{}.jsonl", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }
    fn row(lane: &str, sev: &str) -> ErrorRow {
        ErrorRow {
            t: 1,
            lane: lane.into(),
            kind: "drift".into(),
            detail: "d".into(),
            human: "h".into(),
            severity: sev.into(),
        }
    }
    #[test]
    fn a_HISTORICAL_stop_does_not_count_as_a_current_incident() {
        let now = 1_000_000i64;
        let rows = vec![
            ErrorRow { t : now - 5 * 24 * 3600, lane : "example_lane_26".into(), kind : "trip"
            .into(), detail : "old".into(), human : "h".into(), severity : "stop".into()
            }, ErrorRow { t : now - 60, lane : "example_lane_26".into(), kind : "drift".into(),
            detail : "new".into(), human : "h".into(), severity : "notice".into() },
        ];
        let cutoff = now - ACTIVE_WINDOW_SECS;
        let active = counts_by_lane_since(&rows, cutoff);
        assert_eq!(
            active, vec![("example_lane_26".to_string(), 0, 0, 1)],
            "the days-old stop must not be current"
        );
        assert_eq!(counts_by_lane(& rows), vec![("example_lane_26".to_string(), 1, 0, 1)]);
    }
    #[test]
    fn a_RECENT_stop_is_still_current() {
        let now = 1_000_000i64;
        let rows = vec![
            ErrorRow { t : now - 60, lane : "example_lane_26".into(), kind : "trip".into(), detail :
            "x".into(), human : "h".into(), severity : "stop".into() }
        ];
        assert_eq!(
            counts_by_lane_since(& rows, now - ACTIVE_WINDOW_SECS), vec![("example_lane_26"
            .to_string(), 1, 0, 0)]
        );
    }
    #[test]
    fn CONCURRENT_writers_never_lose_a_row() {
        let dir = std::env::temp_dir().join(format!("cb051-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("errors.jsonl");
        let ps = path.to_str().unwrap().to_string();
        let writers: Vec<_> = (0..4)
            .map(|w| {
                let ps = ps.clone();
                std::thread::spawn(move || {
                    for i in 0..60 {
                        record(
                            &ps,
                            &ErrorRow {
                                t: 1_000 + i,
                                lane: format!("lane{w}"),
                                kind: "drift".into(),
                                detail: format!("w{w}-{i}"),
                                human: "x".into(),
                                severity: "notice".into(),
                            },
                        );
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        let lines = raw.lines().filter(|l| !l.trim().is_empty()).count();
        assert!(lines > 0);
        for l in raw.lines().filter(|l| !l.trim().is_empty()) {
            serde_json::from_str::<ErrorRow>(l)
                .unwrap_or_else(|e| panic!("torn row {l:?}: {e}"));
        }
        assert!(lines <= MAX_ROWS * 2, "the feed must stay bounded, got {lines}");
        assert!(lines >= MAX_ROWS.min(240), "a racing trim discarded rows: {lines}");
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn rows_come_back_newest_first() {
        let p = tmp("order");
        let _ = std::fs::remove_file(&p);
        for i in 0..3 {
            record(
                &p,
                &ErrorRow {
                    t: i,
                    ..row("a", "notice")
                },
            );
        }
        let got = tail(&p, 10);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].t, 2, "newest first — the operator reads the top");
        let _ = std::fs::remove_file(&p);
    }
    #[test]
    fn a_TORN_line_never_hides_the_rows_around_it() {
        let p = tmp("torn");
        let _ = std::fs::remove_file(&p);
        let good = serde_json::to_string(&row("a", "stop")).unwrap();
        let also = serde_json::to_string(&row("c", "notice")).unwrap();
        std::fs::write(&p, format!("{good}\n{{\"t\":1,\"lane\":\"b\"\n{also}\n"))
            .unwrap();
        let got = tail(&p, 10);
        assert_eq!(got.len(), 2, "the good rows survive the broken one");
        assert_eq!(got[0].lane, "c");
        let _ = std::fs::remove_file(&p);
    }
    #[test]
    fn the_file_stays_bounded() {
        let p = tmp("trim");
        let _ = std::fs::remove_file(&p);
        for i in 0..(MAX_ROWS * 2 + 10) {
            record(
                &p,
                &ErrorRow {
                    t: i as i64,
                    ..row("a", "notice")
                },
            );
        }
        let n = std::fs::read_to_string(&p).unwrap().lines().count();
        assert!(n <= MAX_ROWS * 2, "grew to {n}");
        assert_eq!(
            tail(& p, 1) [0].t, (MAX_ROWS * 2 + 9) as i64,
            "and it is the OLDEST rows that are dropped"
        );
        let _ = std::fs::remove_file(&p);
    }
    #[test]
    fn counts_split_by_lane_and_severity() {
        let rows = vec![row("a", "stop"), row("a", "notice"), row("b", "warn")];
        let c = counts_by_lane(&rows);
        assert_eq!(c, vec![("a".to_string(), 1, 0, 1), ("b".to_string(), 0, 1, 0)]);
    }
    #[test]
    fn an_unknown_severity_sorts_last_instead_of_panicking() {
        assert!(rank("stop") < rank("warn"));
        assert!(rank("notice") < rank("from-a-future-version"));
    }
    #[test]
    fn reading_a_file_that_does_not_exist_is_empty_not_an_error() {
        assert!(tail("/nonexistent/cb2/errors.jsonl", 10).is_empty());
    }
}
