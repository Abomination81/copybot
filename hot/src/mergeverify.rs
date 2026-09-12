#[derive(Debug, Clone, PartialEq)]
pub enum Preflight {
    Send { pairs: f64 },
    DoNotMerge { why: &'static str, holdable: bool },
}
#[derive(Debug, Clone, PartialEq)]
pub enum Confirmation {
    Confirmed { pairs: f64 },
    NotApplied,
    Inconsistent { why: &'static str },
}
pub const DUST: f64 = 1e-4;
pub const CONFIRM_TOLERANCE: f64 = 1e-3;
#[allow(clippy::too_many_arguments)]
pub fn preflight(
    want_pairs: f64,
    chain_a: f64,
    chain_b: f64,
    pool_claim_a: f64,
    pool_claim_b: f64,
    outcomes: Option<usize>,
    collateral_known: bool,
    callee_known: bool,
) -> Preflight {
    if !want_pairs.is_finite() || want_pairs <= DUST {
        return Preflight::DoNotMerge {
            why: "nothing to merge",
            holdable: false,
        };
    }
    match outcomes {
        Some(2) => {}
        Some(_) => {
            return Preflight::DoNotMerge {
                why: "not a binary market — a [1,2] partition would be wrong here",
                holdable: false,
            };
        }
        None => {
            return Preflight::DoNotMerge {
                why: "outcome count UNKNOWN — an unknown market shape is not a binary one",
                holdable: false,
            };
        }
    }
    if !collateral_known {
        return Preflight::DoNotMerge {
            why: "collateral not observed — our constants are stale, so we refuse to guess",
            holdable: true,
        };
    }
    if !callee_known {
        return Preflight::DoNotMerge {
            why: "merge callee not observed — the adapter set has migrated before",
            holdable: true,
        };
    }
    let ours_a = (chain_a - pool_claim_a).min(chain_a).max(0.0);
    let ours_b = (chain_b - pool_claim_b).min(chain_b).max(0.0);
    let can = want_pairs.min(ours_a).min(ours_b);
    if can <= DUST {
        return Preflight::DoNotMerge {
            why: "the wallet does not hold both legs — nothing pairs off",
            holdable: false,
        };
    }
    Preflight::Send { pairs: can }
}
pub fn confirm(
    before_a: f64,
    before_b: f64,
    after_a: f64,
    after_b: f64,
    merged: f64,
) -> Confirmation {
    let fell_a = before_a - after_a;
    let fell_b = before_b - after_b;
    if fell_a.abs() <= DUST && fell_b.abs() <= DUST {
        return Confirmation::NotApplied;
    }
    if fell_a < -CONFIRM_TOLERANCE || fell_b < -CONFIRM_TOLERANCE {
        return Confirmation::Inconsistent {
            why: "a leg GREW — this was not our merge",
        };
    }
    if (fell_a - merged).abs() > CONFIRM_TOLERANCE
        || (fell_b - merged).abs() > CONFIRM_TOLERANCE
    {
        return Confirmation::Inconsistent {
            why: "the legs did not fall by the merged amount — something else moved them",
        };
    }
    if (fell_a - fell_b).abs() > CONFIRM_TOLERANCE {
        return Confirmation::Inconsistent {
            why: "the legs fell by DIFFERENT amounts — a merge burns one of each",
        };
    }
    Confirmation::Confirmed {
        pairs: merged,
    }
}
pub fn confirm_split(
    before_a: f64,
    before_b: f64,
    after_a: f64,
    after_b: f64,
    minted: f64,
) -> Confirmation {
    let rose_a = after_a - before_a;
    let rose_b = after_b - before_b;
    if rose_a.abs() <= DUST && rose_b.abs() <= DUST {
        return Confirmation::NotApplied;
    }
    if rose_a < -CONFIRM_TOLERANCE || rose_b < -CONFIRM_TOLERANCE {
        return Confirmation::Inconsistent {
            why: "a leg FELL — this was not our split",
        };
    }
    if (rose_a - minted).abs() > CONFIRM_TOLERANCE
        || (rose_b - minted).abs() > CONFIRM_TOLERANCE
    {
        return Confirmation::Inconsistent {
            why: "the legs did not rise by the minted amount — something else moved them",
        };
    }
    if (rose_a - rose_b).abs() > CONFIRM_TOLERANCE {
        return Confirmation::Inconsistent {
            why: "the legs rose by DIFFERENT amounts — a split mints one of each",
        };
    }
    Confirmation::Confirmed {
        pairs: minted,
    }
}
pub fn may_sell_minted_leg(c: &Confirmation) -> bool {
    matches!(c, Confirmation::Confirmed { .. })
}
pub fn may_retry(c: &Confirmation) -> bool {
    matches!(c, Confirmation::NotApplied)
}
pub const MAX_MERGE_ATTEMPTS: usize = 3;
pub fn give_up(attempts: usize, last: &Confirmation) -> bool {
    attempts >= MAX_MERGE_ATTEMPTS || !may_retry(last)
}
pub fn credible(merged: f64, his_holding: Option<f64>, tolerance: f64) -> bool {
    if !(merged > 0.0) || !merged.is_finite() {
        return false;
    }
    match his_holding {
        None => false,
        Some(h) if !h.is_finite() || h <= 0.0 => false,
        Some(h) => merged <= h * (1.0 + tolerance.max(0.0)),
    }
}
pub const CREDIBILITY_TOLERANCE: f64 = 0.10;
#[derive(Debug, Clone, PartialEq)]
pub enum ExecOutcome {
    Confirmed,
    NotSent,
    Unknown,
}
pub fn classify_exec(code: Option<i32>) -> ExecOutcome {
    match code {
        Some(0) => ExecOutcome::Confirmed,
        Some(3) => ExecOutcome::NotSent,
        Some(2) => ExecOutcome::NotSent,
        _ => ExecOutcome::Unknown,
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextAction {
    BookThenFlattenResidual,
    HoldPairAndAlarm,
}
pub fn after_exec(o: &ExecOutcome) -> NextAction {
    match o {
        ExecOutcome::Confirmed => NextAction::BookThenFlattenResidual,
        ExecOutcome::NotSent | ExecOutcome::Unknown => NextAction::HoldPairAndAlarm,
    }
}
pub fn alarm_text(o: &ExecOutcome, pairs: f64, condition: &str) -> String {
    let retry = match o {
        ExecOutcome::NotSent => {
            "The merge provably did not happen, so it is safe to try \
again later."
        }
        _ => {
            "The merge outcome is UNKNOWN — it may yet land, so it must NOT be retried \
until the chain is re-read."
        }
    };
    format!(
        "MERGE DID NOT COMPLETE on …{}: {pairs:.4} pair(s) are still held. {retry} \
NOTHING IS AT DIRECTIONAL RISK — a YES+NO pair settles at $1 whatever the market does, so \
the position is dormant capital, not exposure. ⛔ DO NOT SELL ONE LEG: selling is two \
orders that can half-succeed, and a single leg IS directional risk. Merge it or leave it.",
        & condition[condition.len().saturating_sub(10)..]
    )
}
pub const EXEC_KILL_SECS: u64 = 210;
