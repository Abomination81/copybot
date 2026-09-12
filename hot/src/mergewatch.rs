pub const SAFE_EXEC_SELECTOR: [u8; 4] = [0x6a, 0x76, 0x12, 0x02];
pub const MULTISEND_SELECTOR: [u8; 4] = [0x8d, 0x80, 0xff, 0x0a];
pub const MERGE_SELECTOR: [u8; 4] = [0x9e, 0x72, 0x12, 0xad];
pub const SPLIT_SELECTOR: [u8; 4] = [0x72, 0xce, 0x42, 0x75];
pub const NEG_RISK_SPLIT_SELECTOR: [u8; 4] = [0xa3, 0xd7, 0xda, 0x1d];
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairKind {
    Merge,
    Split,
}
pub const NEG_RISK_MERGE_SELECTOR: [u8; 4] = [0xb1, 0x0c, 0x5c, 0x17];
pub const COLLATERAL_DECIMALS: f64 = 1e6;
pub const OBSERVED_COLLATERAL: [u8; 20] = [
    0xc0, 0x11, 0xa7, 0xe1, 0x2a, 0x19, 0xf7, 0xb1, 0xf6, 0x70, 0xd4, 0x6f, 0x03, 0xb0,
    0x3f, 0x33, 0x42, 0xe8, 0x2d, 0xfb,
];
pub const MAX_MERGE_SHARES: f64 = 1e9;
pub const MAX_BATCH_CALLS: usize = 64;
pub const MAX_DEPTH: usize = 4;
#[derive(Debug, Clone, PartialEq)]
pub struct Merge {
    pub kind: PairKind,
    pub condition_id: String,
    pub shares: f64,
    pub collateral: [u8; 20],
    pub sized: bool,
}
#[inline]
fn word(b: &[u8], i: usize) -> Option<&[u8]> {
    b.get(i * 32..(i + 1) * 32)
}
fn word_usize(b: &[u8], i: usize) -> Option<usize> {
    let w = word(b, i)?;
    if w[..24].iter().any(|&x| x != 0) {
        return None;
    }
    let mut v: u64 = 0;
    for &byte in &w[24..] {
        v = v.checked_mul(256)?.checked_add(byte as u64)?;
    }
    usize::try_from(v).ok()
}
fn word_u128(b: &[u8], i: usize) -> Option<u128> {
    let w = word(b, i)?;
    if w[..16].iter().any(|&x| x != 0) {
        return None;
    }
    let mut v: u128 = 0;
    for &byte in &w[16..] {
        v = v.checked_mul(256)?.checked_add(byte as u128)?;
    }
    Some(v)
}
fn decode_merge_body(body: &[u8], kind: PairKind) -> Option<Merge> {
    let cond = word(body, 2)?;
    if cond.iter().all(|&x| x == 0) {
        return None;
    }
    let amount = word_u128(body, 4)?;
    if amount == 0 {
        return None;
    }
    let shares = amount as f64 / COLLATERAL_DECIMALS;
    if !shares.is_finite() || shares <= 0.0 || shares > MAX_MERGE_SHARES {
        return None;
    }
    let mut collateral = [0u8; 20];
    collateral.copy_from_slice(&word(body, 0)?[12..]);
    Some(Merge {
        kind,
        condition_id: hex_lower(cond),
        shares,
        collateral,
        sized: true,
    })
}
fn hex_lower(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(char::from_digit((x >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((x & 15) as u32, 16).unwrap_or('0'));
    }
    s
}
const PARTITION_OFFSET: usize = 0xa0;
fn pair_at(b: &[u8], selector: &[u8; 4], kind: PairKind) -> Option<Merge> {
    if b.len() < 4 + 32 * 5 || b[..4] != *selector {
        return None;
    }
    let body = &b[4..];
    if word(body, 1)?.iter().any(|&x| x != 0) {
        return None;
    }
    if word_usize(body, 3)? != PARTITION_OFFSET {
        return None;
    }
    decode_merge_body(body, kind)
}
pub fn decode_pair_actions(calldata: &[u8]) -> Vec<Merge> {
    let mut out = Vec::new();
    const SHORTEST: usize = 4 + 32 * 2;
    if calldata.len() < SHORTEST {
        return out;
    }
    let last = calldata.len() - SHORTEST;
    let mut i = 0usize;
    while i <= last && out.len() < MAX_BATCH_CALLS {
        if calldata[i..i + 4] == NEG_RISK_SPLIT_SELECTOR {
            if let Some(cond) = word(&calldata[i + 4..], 0) {
                if cond.iter().any(|&x| x != 0) {
                    out.push(Merge {
                        kind: PairKind::Split,
                        condition_id: hex_lower(cond),
                        shares: 0.0,
                        collateral: [0u8; 20],
                        sized: false,
                    });
                    i += 4 + 32 * 2;
                    continue;
                }
            }
        }
        if calldata[i..i + 4] == NEG_RISK_MERGE_SELECTOR {
            if let Some(cond) = word(&calldata[i + 4..], 0) {
                if cond.iter().any(|&x| x != 0) {
                    out.push(Merge {
                        kind: PairKind::Merge,
                        condition_id: hex_lower(cond),
                        shares: 0.0,
                        collateral: [0u8; 20],
                        sized: false,
                    });
                    i += 4 + 32 * 2;
                    continue;
                }
            }
        }
        if calldata[i..i + 4] == MERGE_SELECTOR {
            if let Some(m) = pair_at(&calldata[i..], &MERGE_SELECTOR, PairKind::Merge) {
                out.push(m);
                i += 4 + 32 * 5;
                continue;
            }
        }
        if calldata[i..i + 4] == SPLIT_SELECTOR {
            if let Some(m) = pair_at(&calldata[i..], &SPLIT_SELECTOR, PairKind::Split) {
                out.push(m);
                i += 4 + 32 * 5;
                continue;
            }
        }
        i += 1;
    }
    out
}
pub fn decode_merges(calldata: &[u8]) -> Vec<Merge> {
    decode_pair_actions(calldata)
        .into_iter()
        .filter(|m| m.kind == PairKind::Merge)
        .collect()
}
pub fn route<'a, I>(tx_to: &str, calldata: &[u8], lanes: I) -> Vec<(usize, Merge)>
where
    I: IntoIterator<Item = (usize, &'a [u8; 20])>,
{
    let mut owner: Option<usize> = None;
    for (ix, w20) in lanes {
        if is_leader_safe(tx_to, w20) {
            owner = Some(ix);
            break;
        }
    }
    let Some(ix) = owner else { return Vec::new() };
    decode_pair_actions(calldata).into_iter().map(|m| (ix, m)).collect()
}
pub fn is_leader_safe(tx_to: &str, leader: &[u8; 20]) -> bool {
    let want = hex_lower(leader);
    tx_to.trim_start_matches("0x").eq_ignore_ascii_case(&want)
}
#[derive(Debug, Clone, PartialEq)]
pub struct MergeResponse {
    pub merge_pairs: f64,
    pub flatten: Option<(String, f64)>,
    pub note: &'static str,
}
pub fn plan_response(
    ours_a: f64,
    ours_b: f64,
    token_a: &str,
    token_b: &str,
    his_pairs: f64,
    our_fraction: f64,
) -> Option<MergeResponse> {
    if !(his_pairs > 0.0) || !our_fraction.is_finite() || our_fraction <= 0.0 {
        return None;
    }
    let a = ours_a.max(0.0);
    let b = ours_b.max(0.0);
    let intended = his_pairs * our_fraction;
    let pairs = intended.min(a).min(b);
    if pairs <= 1e-9 {
        let held = if a > 1e-9 {
            Some((token_a.to_string(), a.min(intended)))
        } else if b > 1e-9 {
            Some((token_b.to_string(), b.min(intended)))
        } else {
            None
        };
        return held
            .map(|h| MergeResponse {
                merge_pairs: 0.0,
                flatten: Some(h),
                note: "we hold ONE leg only — nothing pairs off, so his exit is ours to sell",
            });
    }
    let excess_a = (a - pairs).max(0.0);
    let excess_b = (b - pairs).max(0.0);
    let cap = (intended - pairs).max(0.0);
    let flatten = if excess_a > 1e-9 && excess_a >= excess_b {
        Some((token_a.to_string(), excess_a.min(cap.max(excess_a.min(intended)))))
    } else if excess_b > 1e-9 {
        Some((token_b.to_string(), excess_b.min(cap.max(excess_b.min(intended)))))
    } else {
        None
    };
    Some(MergeResponse {
        merge_pairs: pairs,
        flatten,
        note: "merge the matched pairs at $1, sell the leg that could not pair",
    })
}
