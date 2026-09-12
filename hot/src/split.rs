use crate::merge::{word_addr, word_u128};
use tiny_keccak::{Hasher, Keccak};
pub const SPLIT_ADAPTER: [u8; 20] = hex_addr("ada100874d00e3331d00f2007a9c336a65009718");
pub const MULTISEND: [u8; 20] = hex_addr("a238cbeb142c10ef7ad8442c6d1f9e89e07e7761");
pub const UNITS_PER_SHARE: f64 = 1e6;
pub const HIS_PAIRS: f64 = 200.0;
pub fn split_calldata(
    collateral: &[u8; 20],
    condition_id: &[u8; 32],
    partition: &[u64],
    amount_units: u128,
) -> Vec<u8> {
    let selector = &keccak(
        b"splitPosition(address,bytes32,bytes32,uint256[],uint256)",
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
pub fn amount_units(pairs: f64) -> Result<u128, String> {
    if !(pairs.is_finite() && pairs > 0.0) {
        return Err(format!("refusing to split {pairs} pairs"));
    }
    let units = (pairs * UNITS_PER_SHARE).floor();
    if units < 1.0 {
        return Err("split amount rounds to zero".into());
    }
    Ok(units as u128)
}
#[derive(Debug, Clone, PartialEq)]
pub struct BatchCall {
    pub to: [u8; 20],
    pub data: Vec<u8>,
}
pub fn multi_send_calldata(calls: &[BatchCall]) -> Vec<u8> {
    let mut packed: Vec<u8> = Vec::new();
    for c in calls {
        packed.push(0u8);
        packed.extend_from_slice(&c.to);
        packed.extend_from_slice(&[0u8; 32]);
        packed.extend_from_slice(&word_u128(c.data.len() as u128));
        packed.extend_from_slice(&c.data);
    }
    let selector = &keccak(b"multiSend(bytes)")[..4];
    let mut out = Vec::with_capacity(4 + 64 + packed.len() + 32);
    out.extend_from_slice(selector);
    out.extend_from_slice(&word_u128(32));
    out.extend_from_slice(&word_u128(packed.len() as u128));
    out.extend_from_slice(&packed);
    let rem = packed.len() % 32;
    if rem != 0 {
        out.extend(std::iter::repeat(0u8).take(32 - rem));
    }
    out
}
pub fn plan_calldata(
    condition_ids: &[[u8; 32]],
    collateral: &[u8; 20],
    partition: &[u64],
    amount_units: u128,
) -> Vec<u8> {
    let calls: Vec<BatchCall> = condition_ids
        .iter()
        .map(|cid| BatchCall {
            to: SPLIT_ADAPTER,
            data: split_calldata(collateral, cid, partition, amount_units),
        })
        .collect();
    if calls.len() == 1 {
        calls.into_iter().next().unwrap().data
    } else {
        multi_send_calldata(&calls)
    }
}
pub fn inner_callee(n: usize) -> [u8; 20] {
    if n == 1 { SPLIT_ADAPTER } else { MULTISEND }
}
fn keccak(bytes: &[u8]) -> [u8; 32] {
    let mut k = Keccak::v256();
    let mut out = [0u8; 32];
    k.update(bytes);
    k.finalize(&mut out);
    out
}
const fn hex_addr(s: &str) -> [u8; 20] {
    let b = s.as_bytes();
    let mut out = [0u8; 20];
    let mut i = 0;
    while i < 20 {
        let hi = hex_nib(b[i * 2]);
        let lo = hex_nib(b[i * 2 + 1]);
        out[i] = (hi << 4) | lo;
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
