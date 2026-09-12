use tiny_keccak::{Hasher, Keccak};
pub const CTF: [u8; 20] = hex_addr("4D97DCd97eC945f40cF65F87097ACe5EA0476045");
pub const USDC: [u8; 20] = hex_addr("2791Bca1f2de4661ED88A30C99A7a9449Aa84174");
pub const BINARY_PARTITION: [u64; 2] = [1, 2];
pub const MERGE_ADAPTER_BINARY: [u8; 20] = hex_addr(
    "ada100db00ca00073811820692005400218fce1f",
);
pub const MERGE_ADAPTER_NEGRISK: [u8; 20] = hex_addr(
    "ada2005600dec949baf300f4c6120000bdb6eaab",
);
pub const COLLATERAL_PUSD: [u8; 20] = hex_addr(
    "c011a7e12a19f7b1f670d46f03b03f3342e82dfb",
);
pub fn merge_adapter_for(neg_risk: bool) -> [u8; 20] {
    if neg_risk { MERGE_ADAPTER_NEGRISK } else { MERGE_ADAPTER_BINARY }
}
const fn hex_addr(s: &str) -> [u8; 20] {
    let b = s.as_bytes();
    let mut out = [0u8; 20];
    let mut i = 0;
    while i < 20 {
        out[i] = (hex_nib(b[i * 2]) << 4) | hex_nib(b[i * 2 + 1]);
        i += 1;
    }
    out
}
const fn hex_nib(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}
fn keccak(data: &[u8]) -> [u8; 32] {
    let mut k = Keccak::v256();
    let mut o = [0u8; 32];
    k.update(data);
    k.finalize(&mut o);
    o
}
#[derive(Debug, Clone, PartialEq)]
pub struct MergePlan {
    pub lane: String,
    pub condition_id: [u8; 32],
    pub yes_token: String,
    pub no_token: String,
    pub amount_units: u128,
}
pub fn merge_calldata(
    collateral: &[u8; 20],
    condition_id: &[u8; 32],
    partition: &[u64],
    amount_units: u128,
) -> Vec<u8> {
    let selector = &keccak(
        b"mergePositions(address,bytes32,bytes32,uint256[],uint256)",
    )[..4];
    let mut out = Vec::with_capacity(4 + 32 * (5 + 1 + partition.len()));
    out.extend_from_slice(selector);
    out.extend_from_slice(&word_addr(collateral));
    out.extend_from_slice(&[0u8; 32]);
    out.extend_from_slice(condition_id);
    out.extend_from_slice(&word_u128(160));
    out.extend_from_slice(&word_u128(amount_units));
    out.extend_from_slice(&word_u128(partition.len() as u128));
    for &p in partition {
        out.extend_from_slice(&word_u128(p as u128));
    }
    out
}
pub(crate) fn word_addr(a: &[u8; 20]) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a);
    w
}
pub(crate) fn word_u128(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}
pub fn condition_id_bytes(hex: &str) -> Option<[u8; 32]> {
    let raw = hex.strip_prefix("0x").unwrap_or(hex);
    let bytes = hex::decode(raw).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut o = [0u8; 32];
    o.copy_from_slice(&bytes);
    Some(o)
}
#[derive(Debug, Clone, PartialEq)]
pub struct Orphan {
    pub condition_id: String,
    pub token_a: String,
    pub token_b: String,
    pub shares_a: f64,
    pub shares_b: f64,
}
impl Orphan {
    pub fn mergeable(&self) -> f64 {
        self.shares_a.min(self.shares_b)
    }
    pub fn residual(&self) -> Option<(&str, f64)> {
        let d = (self.shares_a - self.shares_b).abs();
        if d <= 1e-6 {
            None
        } else if self.shares_a > self.shares_b {
            Some((&self.token_a, d))
        } else {
            Some((&self.token_b, d))
        }
    }
}
pub fn find_orphans<F: Fn(&str) -> bool, G: Fn(&str) -> Option<usize>>(
    holdings: &[(String, f64)],
    token_condition: &std::collections::HashMap<String, String>,
    leader_flat: F,
    outcomes_of: G,
) -> Vec<Orphan> {
    use std::collections::HashMap;
    let mut by_cond: HashMap<&str, Vec<(&str, f64)>> = HashMap::new();
    for (tok, sh) in holdings {
        if *sh <= 1e-6 {
            continue;
        }
        if let Some(cid) = token_condition.get(tok) {
            by_cond.entry(cid).or_default().push((tok, *sh));
        }
    }
    let mut out = Vec::new();
    for (cid, legs) in by_cond {
        if legs.len() != 2 || !leader_flat(cid) {
            continue;
        }
        if outcomes_of(cid) != Some(2) {
            continue;
        }
        out.push(Orphan {
            condition_id: cid.to_string(),
            token_a: legs[0].0.to_string(),
            token_b: legs[1].0.to_string(),
            shares_a: legs[0].1,
            shares_b: legs[1].1,
        });
    }
    out
}
#[derive(Debug, Clone, PartialEq)]
pub enum ReconAction {
    MergeThenSellResidual {
        condition_id: String,
        yes_token: String,
        no_token: String,
        amount: f64,
        residual: Option<(String, f64)>,
    },
    SellBoth { yes_token: String, no_token: String, shares_a: f64, shares_b: f64 },
}
#[cfg(test)]
pub fn plan_reconciliation(orphans: &[Orphan], merge_enabled: bool) -> Vec<ReconAction> {
    orphans
        .iter()
        .map(|o| {
            if merge_enabled {
                ReconAction::MergeThenSellResidual {
                    condition_id: o.condition_id.clone(),
                    yes_token: o.token_a.clone(),
                    no_token: o.token_b.clone(),
                    amount: o.mergeable(),
                    residual: o.residual().map(|(t, s)| (t.to_string(), s)),
                }
            } else {
                ReconAction::SellBoth {
                    yes_token: o.token_a.clone(),
                    no_token: o.token_b.clone(),
                    shares_a: o.shares_a,
                    shares_b: o.shares_b,
                }
            }
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    fn orphan(a: f64, b: f64) -> Orphan {
        Orphan {
            condition_id: "C".into(),
            token_a: "Y".into(),
            token_b: "N".into(),
            shares_a: a,
            shares_b: b,
        }
    }
    #[test]
    fn a_MULTI_OUTCOME_market_is_never_merged_as_binary() {
        let held = vec![("A".to_string(), 100.0), ("B".to_string(), 100.0)];
        let mut map = std::collections::HashMap::new();
        map.insert("A".to_string(), "C1".to_string());
        map.insert("B".to_string(), "C1".to_string());
        assert!(
            find_orphans(& held, & map, | _ | true, | _ | Some(5)).is_empty(),
            "a 5-outcome market must never be treated as a binary pair"
        );
        assert_eq!(
            find_orphans(& held, & map, | _ | true, | _ | Some(2)).len(), 1,
            "a genuinely binary market still merges"
        );
    }
    #[test]
    fn an_UNKNOWN_outcome_count_fails_CLOSED() {
        let held = vec![("A".to_string(), 100.0), ("B".to_string(), 100.0)];
        let mut map = std::collections::HashMap::new();
        map.insert("A".to_string(), "C1".to_string());
        map.insert("B".to_string(), "C1".to_string());
        assert!(find_orphans(& held, & map, | _ | true, | _ | None).is_empty());
    }
    #[test]
    fn merge_disabled_gets_flat_by_selling_both_legs() {
        let acts = plan_reconciliation(&[orphan(100.0, 60.0)], false);
        assert_eq!(
            acts, vec![ReconAction::SellBoth { yes_token : "Y".into(), no_token : "N"
            .into(), shares_a : 100.0, shares_b : 60.0 }]
        );
    }
    #[test]
    fn merge_enabled_merges_the_min_and_sells_the_residual() {
        let acts = plan_reconciliation(&[orphan(100.0, 60.0)], true);
        assert_eq!(
            acts, vec![ReconAction::MergeThenSellResidual { condition_id : "C".into(),
            yes_token : "Y".into(), no_token : "N".into(), amount : 60.0, residual :
            Some(("Y".into(), 40.0)) }]
        );
    }
    #[test]
    fn merge_enabled_equal_legs_has_no_residual_sell() {
        let acts = plan_reconciliation(&[orphan(50.0, 50.0)], true);
        match &acts[0] {
            ReconAction::MergeThenSellResidual { amount, residual, .. } => {
                assert!((amount - 50.0).abs() < 1e-9);
                assert!(residual.is_none(), "equal legs merge clean, nothing to sell");
            }
            _ => panic!("expected a merge action"),
        }
    }
    fn condmap(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(t, c)| (t.to_string(), c.to_string())).collect()
    }
    #[test]
    fn find_orphans_flags_both_sides_only_when_the_leader_is_flat() {
        let map = condmap(&[("YES", "C1"), ("NO", "C1"), ("Z", "C2")]);
        let held = vec![
            ("YES".to_string(), 100.0), ("NO".to_string(), 60.0), ("Z".to_string(), 40.0)
        ];
        let o = find_orphans(&held, &map, |c| c == "C1", |_| Some(2));
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].condition_id, "C1");
        assert!((o[0].mergeable() - 60.0).abs() < 1e-9);
        let (tok, resid) = o[0].residual().unwrap();
        assert_eq!(tok, "YES");
        assert!((resid - 40.0).abs() < 1e-9, "40 YES to sell after merging 60");
    }
    #[test]
    fn a_both_sides_position_the_leader_STILL_holds_is_left_alone() {
        let map = condmap(&[("YES", "C1"), ("NO", "C1")]);
        let held = vec![("YES".to_string(), 100.0), ("NO".to_string(), 100.0)];
        assert!(
            find_orphans(& held, & map, | _ | false, | _ | Some(2)).is_empty(),
            "leader not flat → do not touch the pair"
        );
    }
    #[test]
    fn a_three_outcome_negrisk_condition_is_never_binary_merged() {
        let map = condmap(&[("A", "C"), ("B", "C"), ("D", "C")]);
        let held = vec![("A".into(), 10.0), ("B".into(), 10.0), ("D".into(), 10.0)];
        assert!(
            find_orphans(& held, & map, | _ | true, | _ | Some(2)).is_empty(),
            "3 held outcomes is negRisk — a binary merge would be wrong"
        );
    }
    #[test]
    fn equal_legs_have_no_residual_to_sell() {
        let o = Orphan {
            condition_id: "C".into(),
            token_a: "Y".into(),
            token_b: "N".into(),
            shares_a: 50.0,
            shares_b: 50.0,
        };
        assert!((o.mergeable() - 50.0).abs() < 1e-9);
        assert!(o.residual().is_none());
    }
    #[test]
    fn the_selector_is_the_real_mergePositions_selector() {
        let sel = &keccak(
            b"mergePositions(address,bytes32,bytes32,uint256[],uint256)",
        )[..4];
        assert_eq!(hex::encode(sel), "9e7212ad");
    }
    #[test]
    fn calldata_has_the_right_shape_and_values() {
        let cid = [0x11u8; 32];
        let data = merge_calldata(&USDC, &cid, &BINARY_PARTITION, 5_000_000);
        assert_eq!(data.len(), 4 + 32 * 8);
        assert_eq!(& data[4 + 12..4 + 32], & USDC[..]);
        assert_eq!(& data[4 + 64..4 + 96], & cid[..]);
        assert_eq!(data[4 + 96 + 31], 160);
        let amt = &data[4 + 128..4 + 160];
        assert_eq!(u128::from_be_bytes(amt[16..].try_into().unwrap()), 5_000_000);
        assert_eq!(data[4 + 160 + 31], 2);
        assert_eq!(data[4 + 192 + 31], 1);
        assert_eq!(data[4 + 224 + 31], 2);
    }
    #[test]
    fn condition_id_parsing_rejects_the_wrong_length() {
        assert!(condition_id_bytes("0x1234").is_none());
        assert_eq!(
            condition_id_bytes(& format!("0x{}", "aa".repeat(32))).unwrap(), [0xaa; 32]
        );
    }
    #[test]
    fn known_addresses_decode() {
        assert_eq!(hex::encode(CTF), "4d97dcd97ec945f40cf65f87097ace5ea0476045");
        assert_eq!(hex::encode(USDC), "2791bca1f2de4661ed88a30c99a7a9449aa84174");
    }
}
