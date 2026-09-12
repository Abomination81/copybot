use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Incident {
    pub t: i64,
    pub lane: String,
    pub kind: String,
    pub why: String,
    pub ev: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub by: String,
}
pub struct Incidents {
    path: PathBuf,
    open: HashMap<String, Incident>,
    unreadable: Option<String>,
}
impl Incidents {
    pub fn open(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let mut open: HashMap<String, Incident> = HashMap::new();
        let mut unreadable = None;
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                for line in raw.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Ok(i) = serde_json::from_str::<Incident>(line) else { continue };
                    match i.ev.as_str() {
                        "raise" => {
                            open.entry(i.lane.clone()).or_insert(i);
                        }
                        "clear" => {
                            open.remove(&i.lane);
                        }
                        _ => {}
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => unreadable = Some(format!("{e}")),
        }
        Self { path, open, unreadable }
    }
    pub fn latched(&self, lane: &str) -> bool {
        self.unreadable.is_some() || self.open.contains_key(lane)
    }
    pub fn unreadable(&self) -> Option<&str> {
        self.unreadable.as_deref()
    }
    pub fn open_incidents(&self) -> Vec<&Incident> {
        self.open.values().collect()
    }
    pub fn is_empty(&self) -> bool {
        self.open.is_empty() && self.unreadable.is_none()
    }
    pub fn raise(
        &mut self,
        lane: &str,
        kind: &str,
        why: &str,
        now: i64,
    ) -> Result<bool, String> {
        if let Some(open) = self.open.get(lane) {
            if open.kind != kind {
                let i = Incident {
                    t: now,
                    lane: lane.into(),
                    kind: kind.into(),
                    why: why.chars().take(400).collect(),
                    ev: "raise".into(),
                    by: String::new(),
                };
                self.append(&i)?;
            }
            return Ok(false);
        }
        let i = Incident {
            t: now,
            lane: lane.into(),
            kind: kind.into(),
            why: why.chars().take(400).collect(),
            ev: "raise".into(),
            by: String::new(),
        };
        self.append(&i)?;
        self.open.insert(lane.to_string(), i);
        Ok(true)
    }
    pub fn clear(&mut self, lane: &str, by: &str, now: i64) -> Result<bool, String> {
        if let Some(e) = &self.unreadable {
            return Err(
                format!(
                    "incident journal is unreadable ({e}); \
                                fix it before clearing anything"
                ),
            );
        }
        if !self.open.contains_key(lane) {
            return Ok(false);
        }
        let i = Incident {
            t: now,
            lane: lane.into(),
            kind: "clear".into(),
            why: String::new(),
            ev: "clear".into(),
            by: by.into(),
        };
        self.append(&i)?;
        self.open.remove(lane);
        Ok(true)
    }
    fn append(&self, i: &Incident) -> Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("create incident directory: {e}"))?;
            }
        }
        let line = serde_json::to_string(i)
            .map_err(|e| format!("serialize incident: {e}"))?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("open {}: {e}", self.path.display()))?;
        writeln!(f, "{line}")
            .map_err(|e| format!("write {}: {e}", self.path.display()))?;
        f.flush()
            .and_then(|_| f.sync_data())
            .map_err(|e| format!("sync {}: {e}", self.path.display()))?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(
                format!(
                    "cb2_inc_{tag}_{}_{}.jsonl", std::process::id(), crate
                    ::ledger::now_secs()
                ),
            )
    }
    #[test]
    fn a_latch_SURVIVES_the_restart_it_was_protecting_against() {
        let p = tmp("survive");
        {
            let mut inc = Incidents::open(&p);
            inc.raise("example_lane_26", "fire_rate", ">10 buys in 30s", 1_000).unwrap();
            assert!(inc.latched("example_lane_26"));
        }
        let inc = Incidents::open(&p);
        assert!(inc.latched("example_lane_26"), "a restart must NOT clear a safety stop");
        assert!(! inc.latched("example_lane_25"), "and must not latch a lane that was fine");
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn only_a_DELIBERATE_clear_releases_a_lane() {
        let p = tmp("clear");
        let mut inc = Incidents::open(&p);
        inc.raise("example_lane_26", "persistence", "ENOSPC", 1_000).unwrap();
        assert!(inc.clear("example_lane_26", "operator:api", 1_001).unwrap());
        assert!(! inc.latched("example_lane_26"));
        drop(inc);
        assert!(
            ! Incidents::open(& p).latched("example_lane_26"), "the acknowledgement must persist"
        );
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn raising_the_SAME_incident_twice_writes_ONE_row() {
        let p = tmp("idem");
        let mut inc = Incidents::open(&p);
        assert!(inc.raise("example_lane_26", "fire_rate", "x", 1_000).unwrap(), "first is new");
        assert!(! inc.raise("example_lane_26", "fire_rate", "x", 1_001).unwrap(), "second is not");
        assert!(
            ! inc.raise("example_lane_26", "other", "y", 1_002).unwrap(),
            "the lane stays held by the FIRST incident"
        );
        let rows = std::fs::read_to_string(&p).unwrap().lines().count();
        assert_eq!(rows, 2, "same kind dedupes; a different kind is journalled");
        assert_eq!(inc.open_incidents() [0].kind, "fire_rate");
        let re = Incidents::open(&p);
        assert!(re.latched("example_lane_26"));
        assert_eq!(
            re.open_incidents() [0].kind, "fire_rate",
            "replay must agree with live state about the open cause"
        );
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn an_UNREADABLE_journal_latches_EVERYTHING_and_refuses_clears() {
        let dir = std::env::temp_dir()
            .join(format!("cb2_inc_dir_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut inc = Incidents::open(&dir);
        assert!(inc.unreadable().is_some());
        assert!(inc.latched("anything"), "fail toward the stop");
        assert!(
            inc.clear("anything", "operator", 1).is_err(),
            "a clear must not be accepted while the journal is broken"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn a_MISSING_journal_is_a_clean_first_run_not_a_fault() {
        let p = tmp("missing");
        std::fs::remove_file(&p).ok();
        let inc = Incidents::open(&p);
        assert!(inc.unreadable().is_none());
        assert!(! inc.latched("example_lane_26"));
        assert!(inc.is_empty());
    }
    #[test]
    fn a_CORRUPT_TAIL_is_skipped_without_losing_earlier_incidents() {
        let p = tmp("torn");
        std::fs::remove_file(&p).ok();
        {
            let mut inc = Incidents::open(&p);
            inc.raise("example_lane_26", "fire_rate", "real", 1_000).unwrap();
        }
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, "{{\"t\":1001,\"lane\":\"jok").unwrap();
        drop(f);
        let inc = Incidents::open(&p);
        assert!(inc.latched("example_lane_26"), "the intact incident must survive a torn tail");
        assert!(! inc.latched("example_lane_25"), "and the torn one grants nothing");
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn clearing_a_lane_that_is_not_latched_is_a_harmless_no_op() {
        let p = tmp("noop");
        let mut inc = Incidents::open(&p);
        assert!(! inc.clear("example_lane_26", "operator", 1).unwrap());
        std::fs::remove_file(&p).ok();
    }
}
