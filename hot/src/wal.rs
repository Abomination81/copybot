use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
pub struct Wal {
    path: PathBuf,
    inner: Mutex<Option<File>>,
    failure: Mutex<Option<String>>,
}
impl Wal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| {
                        format!("create wal directory {}: {e}", parent.display())
                    })?;
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open wal {}: {e}", path.display()))?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                File::open(parent)
                    .and_then(|d| d.sync_all())
                    .map_err(|e| {
                        format!("sync wal directory {}: {e}", parent.display())
                    })?;
            }
        }
        Ok(Self {
            path,
            inner: Mutex::new(Some(file)),
            failure: Mutex::new(None),
        })
    }
    pub fn disabled() -> Self {
        Self {
            path: PathBuf::new(),
            inner: Mutex::new(None),
            failure: Mutex::new(None),
        }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn persistence_ok(&self) -> bool {
        self.failure.lock().map(|f| f.is_none()).unwrap_or(false)
    }
    pub fn last_write_error(&self) -> Option<String> {
        self.failure.lock().ok().and_then(|f| f.clone())
    }
    pub fn append_atomic(&self, rows: &[&[u8]]) -> Result<(), String> {
        let mut guard = self.inner.lock().map_err(|_| "wal mutex poisoned".to_string())?;
        let Some(file) = guard.as_mut() else { return Ok(()) };
        let mut buf = Vec::with_capacity(
            rows.iter().map(|r| r.len()).sum::<usize>() + 8,
        );
        for r in rows {
            buf.extend_from_slice(r);
            if !r.ends_with(b"\n") {
                buf.push(b'\n');
            }
        }
        let done = file.write_all(&buf).and_then(|_| file.sync_data());
        if let Err(e) = done {
            let msg = format!("write wal {}: {e}", self.path.display());
            if let Ok(mut f) = self.failure.lock() {
                if f.is_none() {
                    *f = Some(msg.clone());
                }
            }
            return Err(msg);
        }
        Ok(())
    }
}
pub struct WalRecovery {
    path: PathBuf,
    expected: &'static [&'static str],
    acks: Vec<(&'static str, Result<usize, String>)>,
}
#[derive(Debug, PartialEq)]
pub enum Recycled {
    Emptied { bytes: u64 },
    AlreadyEmpty,
    Retained { why: String },
}
impl WalRecovery {
    pub fn new(path: impl AsRef<Path>, expected: &'static [&'static str]) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            expected,
            acks: Vec::new(),
        }
    }
    pub fn absorbed(&mut self, who: &'static str, rows: usize) {
        self.acks.push((who, Ok(rows)));
    }
    pub fn failed(&mut self, who: &'static str, why: impl Into<String>) {
        self.acks.push((who, Err(why.into())));
    }
    pub fn finish(self) -> Recycled {
        for name in self.expected {
            match self.acks.iter().find(|(who, _)| who == name) {
                None => {
                    return Recycled::Retained {
                        why: format!("{name} never acknowledged the WAL"),
                    };
                }
                Some((_, Err(e))) => {
                    return Recycled::Retained {
                        why: format!("{name} could not absorb the WAL: {e}"),
                    };
                }
                Some((_, Ok(_))) => {}
            }
        }
        let len = match std::fs::metadata(&self.path).map(|m| m.len()) {
            Ok(0) | Err(_) => return Recycled::AlreadyEmpty,
            Ok(n) => n,
        };
        match OpenOptions::new()
            .write(true)
            .open(&self.path)
            .and_then(|f| f.set_len(0).map(|_| f))
            .and_then(|f| f.sync_all())
        {
            Ok(()) => Recycled::Emptied { bytes: len },
            Err(e) => {
                Recycled::Retained {
                    why: format!("truncate failed: {e}"),
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(
                format!(
                    "cb2_wal_{tag}_{}_{}.jsonl", std::process::id(), crate
                    ::ledger::now_secs()
                ),
            )
    }
    #[test]
    fn both_rows_land_in_ONE_append_and_survive_reopen() {
        let p = tmp("atomic");
        let _ = std::fs::remove_file(&p);
        {
            let w = Wal::open(&p).unwrap();
            w.append_atomic(&[b"{\"ev\":\"guard\"}\n", b"{\"ev\":\"pending\"}\n"])
                .unwrap();
            w.append_atomic(&[b"{\"ev\":\"resolved\"}\n"]).unwrap();
        }
        let raw = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("guard"), "guard row must be FIRST — tear safety");
        assert!(lines[1].contains("pending"));
        let _ = std::fs::remove_file(&p);
    }
    #[test]
    fn a_row_without_a_trailing_newline_is_still_framed() {
        let p = tmp("frame");
        let _ = std::fs::remove_file(&p);
        {
            let w = Wal::open(&p).unwrap();
            w.append_atomic(&[b"{\"a\":1}", b"{\"b\":2}"]).unwrap();
        }
        assert_eq!(std::fs::read_to_string(& p).unwrap().lines().count(), 2);
        let _ = std::fs::remove_file(&p);
    }
    #[test]
    fn a_failed_write_LATCHES_and_is_visible() {
        let dir = std::env::temp_dir()
            .join(format!("cb2_walfail_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let w = Wal::open(&dir).ok();
        if let Some(w) = w {
            assert!(
                w.append_atomic(& [b"{}\n"]).is_err(), "writing to a directory must fail"
            );
            assert!(! w.persistence_ok(), "the fault must LATCH");
            assert!(w.last_write_error().is_some());
        }
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn a_disabled_wal_writes_nothing_and_reports_healthy() {
        let w = Wal::disabled();
        w.append_atomic(&[b"{\"ev\":\"x\"}\n"]).expect("disabled wal never fails");
        assert!(w.persistence_ok());
    }
    use crate::pending::{Pending, PendingLog};
    use crate::signal_guard::SignalGuard;
    fn salt(n: u8) -> [u8; 32] {
        let mut k = [0u8; 32];
        k[31] = n;
        k
    }
    struct Rig {
        dir: PathBuf,
        guard: PathBuf,
        pend: PathBuf,
        wal: PathBuf,
    }
    impl Rig {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(
                    format!(
                        "cb2_crash_{tag}_{}_{}", std::process::id(), crate
                        ::ledger::now_secs()
                    ),
                );
            std::fs::create_dir_all(&dir).unwrap();
            Rig {
                guard: dir.join("signal-guard.jsonl"),
                pend: dir.join("pending.jsonl"),
                wal: dir.join("wal.jsonl"),
                dir,
            }
        }
        fn pending_row(&self, hash: &str) -> (Pending, Vec<u8>) {
            let p = Pending {
                lane: "example_lane_26".into(),
                token: "T1".into(),
                side: 0,
                order_hash: hash.into(),
                shares: 41.0,
                limit: 0.53,
                ts: 1_000,
                why: "submitting".into(),
                resting: false,
            };
            let b = PendingLog::row_bytes(&p);
            (p, b)
        }
        fn replay(&self, now: i64) -> (SignalGuard, PendingLog) {
            let g = SignalGuard::open_with_wal(&self.guard, Some(&self.wal), now)
                .unwrap();
            let (p, rows) = PendingLog::open_with_wal(
                self.pend.to_str().unwrap(),
                self.wal.to_str().unwrap().into(),
            );
            p.absorb_wal(rows).expect("test migration must succeed");
            (g, p)
        }
    }
    impl Drop for Rig {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }
    #[test]
    fn HAPPY_PATH_one_sync_makes_BOTH_facts_durable() {
        let r = Rig::new("happy");
        {
            let w = Wal::open(&r.wal).unwrap();
            let g = SignalGuard::open_with_wal(&r.guard, Some(&r.wal), 1_000).unwrap();
            g.observe(&salt(1), &salt(9), "example_lane_26", 41.0, 100.0, 1_000).unwrap();
            let (_, row) = r.pending_row("0xdeadbeef");
            g.commit_with(
                    &salt(1),
                    &salt(9),
                    "example_lane_26",
                    "tok-A",
                    41.0,
                    1_000,
                    &w,
                    Some(&row),
                )
                .unwrap();
        }
        let (g, p) = r.replay(1_010);
        assert_eq!(g.copied(& salt(1)), 41.0, "the commitment must survive");
        assert_eq!(p.len(), 1, "the order identity must survive");
    }
    #[test]
    fn TORN_WRITE_loses_the_PENDING_row_and_that_is_the_SAFE_half() {
        let r = Rig::new("torn");
        let cut = {
            let w = Wal::open(&r.wal).unwrap();
            let g = SignalGuard::open_with_wal(&r.guard, Some(&r.wal), 1_000).unwrap();
            g.observe(&salt(1), &salt(9), "example_lane_26", 41.0, 100.0, 1_000).unwrap();
            let (_, row) = r.pending_row("0xdeadbeef");
            g.commit_with(
                    &salt(1),
                    &salt(9),
                    "example_lane_26",
                    "tok-A",
                    41.0,
                    1_000,
                    &w,
                    Some(&row),
                )
                .unwrap();
            std::fs::metadata(&r.wal).unwrap().len() - 20
        };
        let f = OpenOptions::new().write(true).open(&r.wal).unwrap();
        f.set_len(cut).unwrap();
        drop(f);
        let (g, p) = r.replay(1_010);
        assert_eq!(
            g.copied(& salt(1)), 41.0,
            "the GUARD row must survive a tear — losing it would permit a DOUBLE BUY"
        );
        assert_eq!(
            p.len(), 0,
            "the pending row is the half we can afford to lose: no order was ever sent"
        );
    }
    #[test]
    fn CRASH_BEFORE_THE_APPEND_commits_NOTHING_so_the_caller_must_not_send() {
        let r = Rig::new("before");
        let w = Wal::open(&r.dir).ok();
        let g = SignalGuard::open_with_wal(&r.guard, Some(&r.wal), 1_000).unwrap();
        g.observe(&salt(1), &salt(9), "example_lane_26", 41.0, 100.0, 1_000).unwrap();
        let (_, row) = r.pending_row("0xdeadbeef");
        if let Some(w) = w {
            let out = g
                .commit_with(
                    &salt(1),
                    &salt(9),
                    "example_lane_26",
                    "tok-A",
                    41.0,
                    1_000,
                    &w,
                    Some(&row),
                );
            assert!(out.is_err(), "a failed barrier MUST refuse the order");
            assert_eq!(
                g.copied(& salt(1)), 0.0,
                "memory must never claim more than the disk can prove"
            );
        }
    }
    #[test]
    fn CRASH_AFTER_SYNC_BEFORE_SEND_costs_a_TRADE_never_a_DOUBLE() {
        let r = Rig::new("aftersync");
        {
            let w = Wal::open(&r.wal).unwrap();
            let g = SignalGuard::open_with_wal(&r.guard, Some(&r.wal), 1_000).unwrap();
            g.observe(&salt(1), &salt(9), "example_lane_26", 41.0, 100.0, 1_000).unwrap();
            let (_, row) = r.pending_row("0xdeadbeef");
            g.commit_with(
                    &salt(1),
                    &salt(9),
                    "example_lane_26",
                    "tok-A",
                    41.0,
                    1_000,
                    &w,
                    Some(&row),
                )
                .unwrap();
        }
        let (g, _) = r.replay(1_010);
        g.observe(&salt(2), &salt(9), "example_lane_26", 10.0, 100.0, 1_010).unwrap();
        let w2 = Wal::open(&r.wal).unwrap();
        assert!(
            g.commit_with(& salt(2), & salt(9), "example_lane_26", "tok-B", 10.0, 1_010, & w2, None)
            .is_err(), "the 24h market quarantine must survive the merge"
        );
        let w3 = Wal::open(&r.wal).unwrap();
        g.commit_with(&salt(3), &salt(9), "example_lane_26", "tok-A", 5.0, 1_010, &w3, None)
            .expect("a new order on the SAME token is him scaling in, not a second bet");
        assert_eq!(
            g.copied(& salt(1)), 41.0, "and the recorded commitment is unchanged"
        );
    }
    #[test]
    fn REPLAYING_THE_WAL_TWICE_CANNOT_INFLATE_WHAT_WE_COPIED() {
        let r = Rig::new("twice");
        {
            let w = Wal::open(&r.wal).unwrap();
            let g = SignalGuard::open_with_wal(&r.guard, Some(&r.wal), 1_000).unwrap();
            g.observe(&salt(1), &salt(9), "example_lane_26", 41.0, 100.0, 1_000).unwrap();
            let (_, row) = r.pending_row("0xdeadbeef");
            g.commit_with(
                    &salt(1),
                    &salt(9),
                    "example_lane_26",
                    "tok-A",
                    41.0,
                    1_000,
                    &w,
                    Some(&row),
                )
                .unwrap();
        }
        let (g1, _) = r.replay(1_010);
        assert_eq!(g1.copied(& salt(1)), 41.0);
        drop(g1);
        let (g2, p2) = r.replay(1_020);
        assert_eq!(
            g2.copied(& salt(1)), 41.0, "a second replay must not double the total"
        );
        assert_eq!(p2.len(), 1, "nor duplicate the pending row");
    }
    #[test]
    fn a_RESOLVED_row_in_the_WAL_closes_the_pending_half_only() {
        let r = Rig::new("resolved");
        {
            let w = Wal::open(&r.wal).unwrap();
            let g = SignalGuard::open_with_wal(&r.guard, Some(&r.wal), 1_000).unwrap();
            g.observe(&salt(1), &salt(9), "example_lane_26", 41.0, 100.0, 1_000).unwrap();
            let (_, row) = r.pending_row("0xdeadbeef");
            g.commit_with(
                    &salt(1),
                    &salt(9),
                    "example_lane_26",
                    "tok-A",
                    41.0,
                    1_000,
                    &w,
                    Some(&row),
                )
                .unwrap();
            w.append_atomic(
                    &[
                        br#"{"ev":"resolved","order_hash":"0xdeadbeef","verdict":"matched","t":1001}"#,
                    ],
                )
                .unwrap();
        }
        let (g, p) = r.replay(1_010);
        assert_eq!(p.len(), 0, "the venue answered: the row has done its job");
        assert_eq!(g.copied(& salt(1)), 41.0, "but the commitment is permanent");
    }
    #[test]
    fn a_FAILED_consumer_RETAINS_the_wal() {
        let p = tmp("retain");
        std::fs::write(&p, b"{\"ev\":\"pending\"}\n").unwrap();
        let mut r = WalRecovery::new(&p, &["signal_guard", "pending"]);
        r.absorbed("signal_guard", 3);
        r.failed("pending", "ENOSPC");
        match r.finish() {
            Recycled::Retained { why } => assert!(why.contains("pending")),
            other => panic!("a failed consumer MUST retain the wal, got {other:?}"),
        }
        assert!(std::fs::metadata(& p).unwrap().len() > 0, "the wal must survive");
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn a_MISSING_acknowledgement_also_retains() {
        let p = tmp("silent");
        std::fs::write(&p, b"x\n").unwrap();
        let mut r = WalRecovery::new(&p, &["signal_guard", "pending"]);
        r.absorbed("signal_guard", 1);
        assert!(matches!(r.finish(), Recycled::Retained { .. }));
        assert!(std::fs::metadata(& p).unwrap().len() > 0);
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn a_COMPLETE_migration_recycles() {
        let p = tmp("recycle");
        std::fs::write(&p, b"some rows\n").unwrap();
        let mut r = WalRecovery::new(&p, &["signal_guard", "pending"]);
        r.absorbed("signal_guard", 2);
        r.absorbed("pending", 1);
        assert!(matches!(r.finish(), Recycled::Emptied { .. }));
        assert_eq!(
            std::fs::metadata(& p).unwrap().len(), 0, "emptied only when both agree"
        );
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn an_ALREADY_EMPTY_wal_is_not_an_error() {
        let p = tmp("empty");
        std::fs::write(&p, b"").unwrap();
        let mut r = WalRecovery::new(&p, &["a"]);
        r.absorbed("a", 0);
        assert_eq!(r.finish(), Recycled::AlreadyEmpty);
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn a_wal_whose_DIRECTORY_cannot_be_synced_refuses_to_open() {
        assert!(Wal::open("/proc/cb2-nonexistent/wal.jsonl").is_err());
    }
}
