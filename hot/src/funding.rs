use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Movement {
    pub t: i64,
    pub usd: f64,
    #[serde(default)]
    pub note: String,
}
pub struct Funding {
    path: PathBuf,
    movements: Vec<Movement>,
}
impl Funding {
    pub fn open(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let mut movements = Vec::new();
        if let Ok(raw) = std::fs::read_to_string(&path) {
            for line in raw.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(m) = serde_json::from_str::<Movement>(line) {
                    if m.usd.is_finite() {
                        movements.push(m);
                    }
                }
            }
        }
        Self { path, movements }
    }
    pub fn basis(&self) -> Option<f64> {
        if self.movements.is_empty() {
            return None;
        }
        let net: f64 = self.movements.iter().map(|m| m.usd).sum();
        Some((net * 100.0).round() / 100.0)
    }
    pub fn record(&mut self, usd: f64, note: &str, now: i64) -> Result<f64, String> {
        if !usd.is_finite() || usd == 0.0 {
            return Err(
                format!("a movement must be a non-zero finite amount, got {usd}"),
            );
        }
        let m = Movement {
            t: now,
            usd,
            note: note.to_string(),
        };
        let line = serde_json::to_string(&m)
            .map_err(|e| format!("serialize movement: {e}"))?;
        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("create funding directory: {e}"))?;
            }
        }
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
        self.movements.push(m);
        Ok(self.basis().unwrap_or(0.0))
    }
    pub fn movements(&self) -> &[Movement] {
        &self.movements
    }
}
pub fn physical_pnl(equity: Option<f64>, basis: Option<f64>) -> Option<f64> {
    match (equity, basis) {
        (Some(e), Some(b)) => Some(((e - b) * 100.0).round() / 100.0),
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("cb2_funding_{tag}_{}.jsonl", std::process::id()))
    }
    #[test]
    fn ALLOCATION_changes_can_never_move_the_pnl() {
        let mut f = Funding::open(tmp("alloc"));
        f.record(40_000.0, "initial + top-up", 1_000).unwrap();
        let before = physical_pnl(Some(40_305.09), f.basis());
        let after = physical_pnl(Some(40_305.09), f.basis());
        assert_eq!(before, after);
        assert_eq!(after, Some(305.09), "the honest number, not +$10,305");
        std::fs::remove_file(tmp("alloc")).ok();
    }
    #[test]
    fn a_DEPOSIT_is_not_profit() {
        let mut f = Funding::open(tmp("dep"));
        f.record(20_000.0, "seed", 1_000).unwrap();
        assert_eq!(physical_pnl(Some(20_300.47), f.basis()), Some(300.47));
        f.record(20_000.0, "second deposit", 2_000).unwrap();
        assert_eq!(
            physical_pnl(Some(40_300.47), f.basis()), Some(300.47),
            "a deposit moves equity AND basis, so profit is untouched"
        );
        std::fs::remove_file(tmp("dep")).ok();
    }
    #[test]
    fn a_WITHDRAWAL_is_not_a_loss() {
        let mut f = Funding::open(tmp("wd"));
        f.record(40_000.0, "in", 1_000).unwrap();
        f.record(-5_000.0, "out", 2_000).unwrap();
        assert_eq!(f.basis(), Some(35_000.0));
        assert_eq!(physical_pnl(Some(35_305.09), f.basis()), Some(305.09));
        std::fs::remove_file(tmp("wd")).ok();
    }
    #[test]
    fn an_UNDECLARED_basis_yields_UNKNOWN_not_zero() {
        let f = Funding::open(tmp("none"));
        assert_eq!(f.basis(), None);
        assert_eq!(physical_pnl(Some(40_305.09), f.basis()), None);
        assert_eq!(physical_pnl(None, Some(40_000.0)), None);
    }
    #[test]
    fn movements_SURVIVE_a_restart() {
        let p = tmp("restart");
        std::fs::remove_file(&p).ok();
        {
            let mut f = Funding::open(&p);
            f.record(20_000.0, "one", 1_000).unwrap();
            f.record(20_000.0, "two", 2_000).unwrap();
        }
        let f = Funding::open(&p);
        assert_eq!(f.basis(), Some(40_000.0));
        assert_eq!(f.movements().len(), 2);
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn a_zero_or_nonfinite_movement_is_refused() {
        let mut f = Funding::open(tmp("bad"));
        assert!(f.record(0.0, "nothing", 1).is_err());
        assert!(f.record(f64::NAN, "nan", 1).is_err());
        assert_eq!(f.basis(), None, "a refused movement records nothing");
    }
}
