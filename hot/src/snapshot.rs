#[derive(Debug, Clone, PartialEq)]
pub enum Snapshot {
    Failed(String),
    Data(usize),
}
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Believable,
    Failed(String),
    Unconfirmed(String),
}
pub fn judge_empty(snaps: &[Snapshot], required: usize) -> Verdict {
    if let Some(Snapshot::Failed(e)) = snaps
        .iter()
        .find(|s| matches!(s, Snapshot::Failed(_)))
    {
        return Verdict::Failed(e.clone());
    }
    let sizes: Vec<usize> = snaps
        .iter()
        .filter_map(|s| match s {
            Snapshot::Data(n) => Some(*n),
            _ => None,
        })
        .collect();
    if sizes.len() < required.max(1) {
        return Verdict::Unconfirmed(
            format!("only {} successful read(s), need {required}", sizes.len()),
        );
    }
    match sizes.iter().find(|n| **n > 0) {
        Some(n) => {
            Verdict::Unconfirmed(format!("a read returned {n} position(s), not empty"))
        }
        None => Verdict::Believable,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_FAILED_read_is_never_evidence_of_emptiness() {
        assert!(
            matches!(judge_empty(& [Snapshot::Data(0), Snapshot::Failed("timeout"
            .into())], 2), Verdict::Failed(_))
        );
    }
    #[test]
    fn ONE_empty_read_is_not_enough() {
        assert!(
            matches!(judge_empty(& [Snapshot::Data(0)], 2), Verdict::Unconfirmed(_))
        );
    }
    #[test]
    fn TWO_independent_empty_reads_are_believable() {
        assert_eq!(
            judge_empty(& [Snapshot::Data(0), Snapshot::Data(0)], 2), Verdict::Believable
        );
    }
    #[test]
    fn a_DISAGREEMENT_is_unconfirmed_not_empty() {
        assert!(
            matches!(judge_empty(& [Snapshot::Data(0), Snapshot::Data(3)], 2),
            Verdict::Unconfirmed(_))
        );
    }
    #[test]
    fn a_NON_EMPTY_wallet_is_never_believable_however_many_reads() {
        assert!(
            matches!(judge_empty(& [Snapshot::Data(2), Snapshot::Data(2)], 2),
            Verdict::Unconfirmed(_))
        );
    }
    #[test]
    fn no_reads_at_all_is_unconfirmed() {
        assert!(matches!(judge_empty(& [], 2), Verdict::Unconfirmed(_)));
    }
}
