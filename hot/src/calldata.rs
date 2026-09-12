const F_MAKER: usize = 1;
const F_TOKEN: usize = 3;
const F_MAKER_AMT: usize = 4;
const F_TAKER_AMT: usize = 5;
const F_SIDE: usize = 6;
pub const MATCH_SELECTOR: [u8; 4] = [0x3c, 0x2b, 0x43, 0x99];
#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    pub condition_id: [u8; 32],
    pub token_id: String,
    pub side: u8,
    pub price: f64,
    pub order_size: f64,
    pub fill_size: f64,
    pub role: &'static str,
    pub salt: [u8; 32],
    pub occurrence: u16,
}
pub const TAKER_OCCURRENCE: u16 = u16::MAX;
#[inline]
fn word(b: &[u8], off: usize) -> Option<&[u8]> {
    b.get(off..off + 32)
}
#[inline]
fn word_usize(b: &[u8], off: usize) -> Option<usize> {
    let w = word(b, off)?;
    if w[..24].iter().any(|&x| x != 0) {
        return None;
    }
    let mut v = 0usize;
    for &byte in &w[24..] {
        v = v.checked_mul(256)?.checked_add(byte as usize)?;
    }
    Some(v)
}
#[inline]
fn word_u128(b: &[u8], off: usize) -> Option<u128> {
    let w = word(b, off)?;
    if w[..16].iter().any(|&x| x != 0) {
        return None;
    }
    let mut v = [0u8; 16];
    v.copy_from_slice(&w[16..]);
    Some(u128::from_be_bytes(v))
}
fn word_dec_string(b: &[u8], off: usize) -> Option<String> {
    let w = word(b, off)?;
    let mut digits: Vec<u8> = Vec::with_capacity(78);
    let mut acc = w.to_vec();
    if acc.iter().all(|&x| x == 0) {
        return Some("0".into());
    }
    while acc.iter().any(|&x| x != 0) {
        let mut rem = 0u32;
        for byte in acc.iter_mut() {
            let cur = rem * 256 + *byte as u32;
            *byte = (cur / 10) as u8;
            rem = cur % 10;
        }
        digits.push(b'0' + rem as u8);
    }
    digits.reverse();
    String::from_utf8(digits).ok()
}
#[inline]
fn addr_matches(b: &[u8], off: usize, wallet_20: &[u8; 20]) -> bool {
    match word(b, off) {
        Some(w) => w[..12].iter().all(|&x| x == 0) && &w[12..] == wallet_20,
        None => false,
    }
}
fn shares_and_price(
    b: &[u8],
    ord: usize,
    fill_raw: u128,
) -> Option<(f64, f64, f64, u8)> {
    let maker_amt = word_u128(b, ord + F_MAKER_AMT * 32)?;
    let taker_amt = word_u128(b, ord + F_TAKER_AMT * 32)?;
    let side = word_u128(b, ord + F_SIDE * 32)? as u8;
    if maker_amt == 0 || taker_amt == 0 || side > 1 {
        return None;
    }
    let (ma, ta) = (maker_amt as f64, taker_amt as f64);
    let price = if side == 0 { ma / ta } else { ta / ma };
    if !(price > 0.0 && price < 1.0) {
        return None;
    }
    let order_size = (if side == 0 { ta } else { ma }) / 1e6;
    let raw = fill_raw as f64 / 1e6;
    let size = if side == 0 { raw / price } else { raw };
    Some((size, price, order_size, side))
}
pub fn participants(calldata: &[u8]) -> Vec<[u8; 20]> {
    let mut out = Vec::new();
    if calldata.len() < 4 + 32 * 7 || calldata[..4] != MATCH_SELECTOR {
        return out;
    }
    let b = &calldata[4..];
    let mut push = |off: usize| {
        if let Some(w) = word(b, off) {
            if w[..12].iter().all(|&x| x == 0) {
                let mut a = [0u8; 20];
                a.copy_from_slice(&w[12..]);
                out.push(a);
            }
        }
    };
    if let Some(t) = word_usize(b, 32) {
        push(t + F_MAKER * 32);
    }
    if let Some(mo) = word_usize(b, 64) {
        if let Some(n) = word_usize(b, mo) {
            if n <= 512 {
                let body = mo + 32;
                for i in 0..n {
                    if let Some(rel) = word_usize(b, body + i * 32) {
                        push(body + rel + F_MAKER * 32);
                    }
                }
            }
        }
    }
    out
}
pub fn decode_all(calldata: &[u8], wallet_20: &[u8; 20]) -> Vec<Decoded> {
    match decode_all_inner(calldata, wallet_20) {
        Some(v) => v,
        None => Vec::new(),
    }
}
fn decode_all_inner(calldata: &[u8], wallet_20: &[u8; 20]) -> Option<Vec<Decoded>> {
    if calldata.len() < 4 + 32 * 7 || calldata[..4] != MATCH_SELECTOR {
        return None;
    }
    let b = &calldata[4..];
    let mut condition_id = [0u8; 32];
    condition_id.copy_from_slice(word(b, 0)?);
    if condition_id.iter().all(|&x| x == 0) {
        return None;
    }
    let taker_order_off = word_usize(b, 32)?;
    let maker_orders_off = word_usize(b, 64)?;
    let taker_fill = word_u128(b, 96)?;
    let maker_fills_off = word_usize(b, 128)?;
    let n_fills = word_usize(b, maker_fills_off)?;
    let n_makers = word_usize(b, maker_orders_off)?;
    if n_makers > 512 || n_fills > 512 {
        return None;
    }
    let makers_body = maker_orders_off + 32;
    let mut out: Vec<Decoded> = Vec::new();
    let push = |
        ord: usize,
        fill_raw: u128,
        role: &'static str,
        occurrence: u16,
        out: &mut Vec<Decoded>,
    | -> Option<()> {
        if fill_raw == 0 {
            return None;
        }
        let (size, price, order_size, side) = shares_and_price(b, ord, fill_raw)?;
        let mut salt = [0u8; 32];
        salt.copy_from_slice(word(b, ord)?);
        let token_id = word_dec_string(b, ord + F_TOKEN * 32)?;
        if token_id.len() < 6 {
            return None;
        }
        if size <= 0.0 || size > order_size * 1.001 {
            return None;
        }
        if salt.iter().all(|&x| x == 0) {
            return None;
        }
        out.push(Decoded {
            condition_id,
            token_id,
            side,
            price,
            order_size,
            fill_size: size,
            role,
            salt,
            occurrence,
        });
        Some(())
    };
    for i in 0..n_makers {
        let Some(rel) = word_usize(b, makers_body + i * 32) else { continue };
        let ord = makers_body + rel;
        if !addr_matches(b, ord + F_MAKER * 32, wallet_20) {
            continue;
        }
        if i >= n_fills {
            continue;
        }
        let Some(fill) = word_u128(b, maker_fills_off + 32 + i * 32) else { continue };
        let _ = push(ord, fill, "maker", i as u16, &mut out);
    }
    if addr_matches(b, taker_order_off + F_MAKER * 32, wallet_20) {
        let _ = push(taker_order_off, taker_fill, "taker", TAKER_OCCURRENCE, &mut out);
    }
    Some(out)
}
pub fn decode_pending(calldata: &[u8], wallet_20: &[u8; 20]) -> Option<Decoded> {
    let mut all = decode_all(calldata, wallet_20);
    if all.len() == 1 { all.pop() } else { None }
}
