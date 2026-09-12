use std::collections::HashMap;
#[derive(Debug, Clone, PartialEq)]
pub enum MergeSignal {
    Exit { shares: f64 },
    NonSignal,
    Unknown,
}
#[derive(Debug, Clone, Copy, Default)]
struct Balance {
    sized: f64,
    poisoned: bool,
}
#[derive(Debug, Default)]
pub struct PairLedger {
    balances: HashMap<(String, String), Balance>,
    seeded: std::collections::HashSet<String>,
}
pub type SeedRow = (bool, String, f64, bool);
impl PairLedger {
    pub fn new() -> Self {
        Self::default()
    }
    fn entry(&mut self, leader: &str, condition: &str) -> &mut Balance {
        self.balances.entry((leader.to_string(), condition.to_string())).or_default()
    }
    pub fn credit_split(
        &mut self,
        leader: &str,
        condition: &str,
        shares: f64,
        sized: bool,
    ) {
        let b = self.entry(leader, condition);
        if !sized || !shares.is_finite() || shares < 0.0 {
            b.poisoned = true;
            return;
        }
        b.sized += shares;
    }
    pub fn merge_signal(
        &mut self,
        leader: &str,
        condition: &str,
        shares: f64,
        sized: bool,
    ) -> MergeSignal {
        let b = self.entry(leader, condition);
        if b.poisoned || !sized || !shares.is_finite() || shares < 0.0 {
            b.poisoned = true;
            return MergeSignal::Unknown;
        }
        let covered = shares.min(b.sized);
        b.sized -= covered;
        let excess = shares - covered;
        if excess <= 1e-6 {
            MergeSignal::NonSignal
        } else {
            MergeSignal::Exit {
                shares: excess,
            }
        }
    }
    pub fn seed(&mut self, leader: &str, rows: &[SeedRow]) {
        if !self.seeded.insert(leader.to_string()) {
            return;
        }
        for (is_split, cond, shares, sized) in rows {
            if *is_split {
                self.credit_split(leader, cond, *shares, *sized);
            } else {
                let _ = self.merge_signal(leader, cond, *shares, *sized);
            }
        }
    }
    pub fn is_seeded(&self, leader: &str) -> bool {
        self.seeded.contains(leader)
    }
    pub fn balance(&self, leader: &str, condition: &str) -> Option<f64> {
        match self.balances.get(&(leader.to_string(), condition.to_string())) {
            Some(b) if b.poisoned => None,
            Some(b) => Some(b.sized),
            None => Some(0.0),
        }
    }
}
pub fn seed_row_from_activity(row: &serde_json::Value) -> Option<SeedRow> {
    let is_split = match row["type"].as_str()? {
        "SPLIT" => true,
        "MERGE" => false,
        _ => return None,
    };
    let cond = row["conditionId"].as_str().filter(|c| !c.is_empty())?;
    let shares = row["size"]
        .as_f64()
        .or_else(|| row["size"].as_str().and_then(|x| x.parse().ok()));
    match shares {
        Some(v) if v.is_finite() && v >= 0.0 => {
            Some((is_split, cond.trim_start_matches("0x").to_ascii_lowercase(), v, true))
        }
        _ => {
            Some((
                is_split,
                cond.trim_start_matches("0x").to_ascii_lowercase(),
                0.0,
                false,
            ))
        }
    }
}
pub fn seed_rows_from_activity(rows: &[serde_json::Value]) -> Vec<SeedRow> {
    let mut with_ts: Vec<(i64, SeedRow)> = rows
        .iter()
        .filter_map(|r| {
            let sr = seed_row_from_activity(r)?;
            let ts = r["timestamp"].as_i64().unwrap_or(i64::MIN);
            Some((ts, sr))
        })
        .collect();
    with_ts.sort_by_key(|(ts, _)| *ts);
    with_ts.into_iter().map(|(_, sr)| sr).collect()
}
