use k256::ecdsa::{RecoveryId, Signature, SigningKey};
use tiny_keccak::{Hasher, Keccak};
pub fn keccak(bytes: &[u8]) -> [u8; 32] {
    let mut k = Keccak::v256();
    let mut out = [0u8; 32];
    k.update(bytes);
    k.finalize(&mut out);
    out
}
pub fn rlp_bytes(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len() + 9);
    if b.len() == 1 && b[0] < 0x80 {
        out.push(b[0]);
    } else if b.len() <= 55 {
        out.push(0x80 + b.len() as u8);
        out.extend_from_slice(b);
    } else {
        let len = be_minimal(b.len() as u128);
        out.push(0xb7 + len.len() as u8);
        out.extend_from_slice(&len);
        out.extend_from_slice(b);
    }
    out
}
pub fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
    let payload: Vec<u8> = items.concat();
    let mut out = Vec::with_capacity(payload.len() + 9);
    if payload.len() <= 55 {
        out.push(0xc0 + payload.len() as u8);
    } else {
        let len = be_minimal(payload.len() as u128);
        out.push(0xf7 + len.len() as u8);
        out.extend_from_slice(&len);
    }
    out.extend_from_slice(&payload);
    out
}
fn be_minimal(v: u128) -> Vec<u8> {
    if v == 0 {
        return Vec::new();
    }
    let b = v.to_be_bytes();
    let first = b.iter().position(|x| *x != 0).unwrap_or(15);
    b[first..].to_vec()
}
pub fn rlp_uint(v: u128) -> Vec<u8> {
    rlp_bytes(&be_minimal(v))
}
fn word_u128(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}
fn word_addr(a: &[u8; 20]) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a);
    w
}
fn tail_bytes(b: &[u8]) -> Vec<u8> {
    let mut out = word_u128(b.len() as u128).to_vec();
    out.extend_from_slice(b);
    let pad = (32 - b.len() % 32) % 32;
    out.extend(std::iter::repeat(0u8).take(pad));
    out
}
pub fn exec_transaction_selector() -> [u8; 4] {
    let sig = "execTransaction(address,uint256,bytes,uint8,uint256,uint256,uint256,\
address,address,bytes)";
    let h = keccak(sig.as_bytes());
    [h[0], h[1], h[2], h[3]]
}
pub fn prevalidated_signature(owner: &[u8; 20]) -> Vec<u8> {
    let mut sig = Vec::with_capacity(65);
    sig.extend_from_slice(&word_addr(owner));
    sig.extend_from_slice(&[0u8; 32]);
    sig.push(1);
    sig
}
pub fn exec_transaction_calldata(
    to: &[u8; 20],
    inner: &[u8],
    owner: &[u8; 20],
) -> Vec<u8> {
    let sigs = prevalidated_signature(owner);
    const HEAD: u128 = 32 * 10;
    let off_data = HEAD;
    let off_sigs = HEAD + 32 + ((inner.len() as u128 + 31) / 32) * 32;
    let mut out = Vec::with_capacity(512 + inner.len());
    out.extend_from_slice(&exec_transaction_selector());
    out.extend_from_slice(&word_addr(to));
    out.extend_from_slice(&word_u128(0));
    out.extend_from_slice(&word_u128(off_data));
    out.extend_from_slice(&word_u128(0));
    out.extend_from_slice(&word_u128(0));
    out.extend_from_slice(&word_u128(0));
    out.extend_from_slice(&word_u128(0));
    out.extend_from_slice(&word_addr(&[0u8; 20]));
    out.extend_from_slice(&word_addr(&[0u8; 20]));
    out.extend_from_slice(&word_u128(off_sigs));
    out.extend_from_slice(&tail_bytes(inner));
    out.extend_from_slice(&tail_bytes(&sigs));
    out
}
#[derive(Debug, Clone)]
pub struct Tx1559 {
    pub chain_id: u64,
    pub nonce: u64,
    pub max_priority_fee: u128,
    pub max_fee: u128,
    pub gas_limit: u64,
    pub to: [u8; 20],
    pub value: u128,
    pub data: Vec<u8>,
}
impl Tx1559 {
    fn body(&self) -> Vec<Vec<u8>> {
        vec![
            rlp_uint(self.chain_id as u128), rlp_uint(self.nonce as u128), rlp_uint(self
            .max_priority_fee), rlp_uint(self.max_fee), rlp_uint(self.gas_limit as u128),
            rlp_bytes(& self.to), rlp_uint(self.value), rlp_bytes(& self.data),
            rlp_list(& []),
        ]
    }
    pub fn signing_hash(&self) -> [u8; 32] {
        let mut payload = vec![0x02u8];
        payload.extend_from_slice(&rlp_list(&self.body()));
        keccak(&payload)
    }
    pub fn sign(&self, key: &[u8; 32]) -> Result<Vec<u8>, String> {
        let sk = SigningKey::from_bytes(key.into())
            .map_err(|e| format!("bad key: {e}"))?;
        let (sig, rec): (Signature, RecoveryId) = sk
            .sign_prehash_recoverable(&self.signing_hash())
            .map_err(|e| format!("sign failed: {e}"))?;
        Ok(self.encode_signed(rec.to_byte(), &sig.r().to_bytes(), &sig.s().to_bytes()))
    }
    pub fn encode_signed(&self, y_parity: u8, r: &[u8], s: &[u8]) -> Vec<u8> {
        let mut items = self.body();
        items.push(rlp_uint(y_parity as u128));
        items.push(rlp_bytes(strip_zeros(r)));
        items.push(rlp_bytes(strip_zeros(s)));
        let mut out = vec![0x02u8];
        out.extend_from_slice(&rlp_list(&items));
        out
    }
    pub fn tx_hash(&self, signed: &[u8]) -> String {
        format!("0x{}", hex::encode(keccak(signed)))
    }
}
fn strip_zeros(b: &[u8]) -> &[u8] {
    let first = b.iter().position(|x| *x != 0).unwrap_or(b.len());
    &b[first..]
}
pub fn address_of(key: &[u8; 32]) -> Result<[u8; 20], String> {
    let sk = SigningKey::from_bytes(key.into()).map_err(|e| format!("bad key: {e}"))?;
    let pk = sk.verifying_key().to_encoded_point(false);
    let bytes = pk.as_bytes();
    if bytes.len() != 65 {
        return Err("unexpected public key encoding".into());
    }
    let h = keccak(&bytes[1..]);
    let mut a = [0u8; 20];
    a.copy_from_slice(&h[12..]);
    Ok(a)
}
pub fn preflight_owner(
    threshold: u64,
    owners: &[[u8; 20]],
    signer: &[u8; 20],
) -> Result<(), String> {
    if threshold != 1 {
        return Err(
            format!(
                "Safe threshold is {threshold}, not 1 — a pre-validated signature is no longer \
sufficient and every merge would revert"
            ),
        );
    }
    if !owners.iter().any(|o| o == signer) {
        return Err(
            format!(
                "our signer 0x{} is not an owner of the Safe — it cannot pre-validate",
                hex::encode(signer)
            ),
        );
    }
    Ok(())
}
pub const DEFAULT_RPC: &str = "https://polygon.drpc.org";
fn hex_u128(v: &serde_json::Value) -> Option<u128> {
    u128::from_str_radix(v.as_str()?.trim_start_matches("0x"), 16).ok()
}
pub async fn rpc(
    http: &reqwest::Client,
    url: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!(
        { "jsonrpc" : "2.0", "id" : 1, "method" : method, "params" : params }
    );
    let r = http
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("{method}: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("{method}: HTTP {}", r.status()));
    }
    let v: serde_json::Value = r.json().await.map_err(|e| format!("{method}: {e}"))?;
    if let Some(e) = v.get("error") {
        return Err(format!("{method}: {e}"));
    }
    v.get("result").cloned().ok_or_else(|| format!("{method}: no result"))
}
pub async fn send_safe_call(
    http: &reqwest::Client,
    url: &str,
    key: &[u8; 32],
    safe: &[u8; 20],
    inner_to: &[u8; 20],
    inner: &[u8],
    chain_id: u64,
) -> Result<String, String> {
    let owner = address_of(key)?;
    let data = exec_transaction_calldata(inner_to, inner, &owner);
    let from = format!("0x{}", hex::encode(owner));
    let to = format!("0x{}", hex::encode(safe));
    let dhex = format!("0x{}", hex::encode(& data));
    let est = rpc(
            http,
            url,
            "eth_estimateGas",
            serde_json::json!([{ "from" : from, "to" : to, "data" : dhex }]),
        )
        .await
        .map_err(|e| format!("simulation refused the merge (nothing sent): {e}"))?;
    let gas = hex_u128(&est).ok_or("unreadable gas estimate")? as u64;
    let nonce = hex_u128(
            &rpc(
                    http,
                    url,
                    "eth_getTransactionCount",
                    serde_json::json!([from, "pending"]),
                )
                .await?,
        )
        .ok_or("unreadable nonce")? as u64;
    let base = hex_u128(&rpc(http, url, "eth_gasPrice", serde_json::json!([])).await?)
        .ok_or("unreadable gas price")?;
    let tip = base.max(30_000_000_000u128);
    let tx = Tx1559 {
        chain_id,
        nonce,
        max_priority_fee: tip,
        max_fee: tip + base * 2,
        gas_limit: gas.saturating_mul(3) / 2 + 50_000,
        to: *safe,
        value: 0,
        data,
    };
    let signed = tx.sign(key)?;
    let expect = tx.tx_hash(&signed);
    let sent = rpc(
            http,
            url,
            "eth_sendRawTransaction",
            serde_json::json!([format!("0x{}", hex::encode(& signed))]),
        )
        .await
        .map_err(|e| format!("broadcast failed, outcome UNKNOWN: {e}"))?;
    let got = sent.as_str().unwrap_or(&expect).to_string();
    if got != expect {
        eprintln!("[txsend] ⚠️ node returned {got}, we computed {expect}");
    }
    Ok(got)
}
pub async fn wait_receipt(
    http: &reqwest::Client,
    url: &str,
    tx_hash: &str,
    timeout_secs: u64,
) -> Result<bool, String> {
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(timeout_secs);
    loop {
        if let Ok(v) = rpc(
                http,
                url,
                "eth_getTransactionReceipt",
                serde_json::json!([tx_hash]),
            )
            .await
        {
            if !v.is_null() {
                return Ok(v.get("status").and_then(|s| s.as_str()) == Some("0x1"));
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(
                format!("no receipt for {tx_hash} within {timeout_secs}s — UNKNOWN"),
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}
#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    #[test]
    fn rlp_matches_the_SPEC_vectors() {
        assert_eq!(rlp_bytes(b"dog"), vec![0x83, b'd', b'o', b'g']);
        assert_eq!(rlp_bytes(b""), vec![0x80]);
        assert_eq!(rlp_bytes(& [0x0f]), vec![0x0f], "a single byte < 0x80 is itself");
        assert_eq!(rlp_bytes(& [0x04, 0x00]), vec![0x82, 0x04, 0x00]);
        assert_eq!(rlp_list(& []), vec![0xc0]);
        assert_eq!(
            rlp_list(& [rlp_bytes(b"cat"), rlp_bytes(b"dog")]), vec![0xc8, 0x83, b'c',
            b'a', b't', 0x83, b'd', b'o', b'g']
        );
        let long = vec![b'a'; 56];
        assert_eq!(rlp_bytes(& long) [0], 0xb8);
        assert_eq!(rlp_bytes(& vec![b'a'; 55]) [0], 0x80 + 55);
    }
    #[test]
    fn an_integer_is_MINIMAL_big_endian_and_zero_is_empty() {
        assert_eq!(rlp_uint(0), vec![0x80]);
        assert_eq!(rlp_uint(1), vec![0x01]);
        assert_eq!(rlp_uint(1024), vec![0x82, 0x04, 0x00]);
        assert_eq!(rlp_uint(0x7f), vec![0x7f]);
        assert_eq!(rlp_uint(0x80), vec![0x81, 0x80]);
        assert_eq!(be_minimal(0x0000_00ff), vec![0xff], "leading zeros must go");
    }
    #[test]
    fn the_execTransaction_selector_is_the_real_one() {
        assert_eq!(exec_transaction_selector(), [0x6a, 0x76, 0x12, 0x02]);
    }
    #[test]
    fn a_prevalidated_signature_is_owner_then_zero_then_ONE() {
        let owner = [0x11u8; 20];
        let s = prevalidated_signature(&owner);
        assert_eq!(s.len(), 65);
        assert_eq!(
            & s[0..12], & [0u8; 12], "the address is right-aligned in the r word"
        );
        assert_eq!(& s[12..32], & owner);
        assert_eq!(& s[32..64], & [0u8; 32], "s must be zero");
        assert_eq!(s[64], 1, "v=1 is what marks it pre-validated");
    }
    #[test]
    fn execTransaction_calldata_has_correct_dynamic_OFFSETS() {
        let to = [0xaau8; 20];
        let owner = [0xbbu8; 20];
        let inner = vec![0xcc; 100];
        let cd = exec_transaction_calldata(&to, &inner, &owner);
        let word = |i: usize| -> u128 {
            let o = 4 + i * 32;
            u128::from_be_bytes(cd[o + 16..o + 32].try_into().unwrap())
        };
        assert_eq!(word(1), 0, "value must be zero");
        assert_eq!(word(3), 0, "operation MUST be CALL, never DELEGATECALL");
        assert_eq!(word(2), 320, "data offset");
        assert_eq!(word(9), 480, "signatures offset must clear the padded data tail");
        let d = 4 + 320;
        assert_eq!(u128::from_be_bytes(cd[d + 16..d + 32].try_into().unwrap()), 100);
        assert_eq!(& cd[d + 32..d + 132], & inner[..]);
        let s = 4 + 480;
        assert_eq!(u128::from_be_bytes(cd[s + 16..s + 32].try_into().unwrap()), 65);
    }
    fn sample_tx() -> Tx1559 {
        Tx1559 {
            chain_id: 137,
            nonce: 7,
            max_priority_fee: 30_000_000_000,
            max_fee: 100_000_000_000,
            gas_limit: 300_000,
            to: [0x42u8; 20],
            value: 0,
            data: vec![0xde, 0xad, 0xbe, 0xef],
        }
    }
    #[test]
    fn a_signed_transaction_is_TYPE_2_and_recovers_to_our_own_address() {
        let key = [0x4cu8; 32];
        let tx = sample_tx();
        let signed = tx.sign(&key).expect("signs");
        assert_eq!(signed[0], 0x02, "must be an EIP-1559 typed transaction");
        let sk = SigningKey::from_bytes(&key.into()).unwrap();
        let (sig, rec) = sk.sign_prehash_recoverable(&tx.signing_hash()).unwrap();
        let vk = k256::ecdsa::VerifyingKey::recover_from_prehash(
                &tx.signing_hash(),
                &sig,
                rec,
            )
            .expect("recovers");
        let pt = vk.to_encoded_point(false);
        let h = keccak(&pt.as_bytes()[1..]);
        assert_eq!(
            & h[12..], & address_of(& key).unwrap() [..],
            "the signature does not recover to our own address"
        );
        assert!(rec.to_byte() <= 1, "EIP-1559 parity is 0/1, never 27/28");
    }
    #[test]
    fn changing_ANY_field_changes_the_signing_hash() {
        let base = sample_tx().signing_hash();
        let mut t = sample_tx();
        t.nonce = 8;
        assert_ne!(t.signing_hash(), base, "nonce");
        let mut t = sample_tx();
        t.chain_id = 1;
        assert_ne!(t.signing_hash(), base, "⛔ chain id — replay protection");
        let mut t = sample_tx();
        t.max_fee += 1;
        assert_ne!(t.signing_hash(), base, "max fee");
        let mut t = sample_tx();
        t.data.push(0);
        assert_ne!(t.signing_hash(), base, "calldata");
        let mut t = sample_tx();
        t.to[0] ^= 1;
        assert_ne!(t.signing_hash(), base, "destination");
    }
    #[test]
    fn preflight_REFUSES_a_safe_we_cannot_prevalidate_for() {
        let me = [0x11u8; 20];
        let other = [0x22u8; 20];
        assert!(preflight_owner(1, & [me], & me).is_ok());
        assert!(
            preflight_owner(2, & [me, other], & me).is_err(), "threshold 2 must refuse"
        );
        assert!(
            preflight_owner(1, & [other], & me).is_err(), "not an owner must refuse"
        );
        assert!(preflight_owner(1, & [], & me).is_err(), "no owners at all must refuse");
        assert!(preflight_owner(1, & [other, me], & me).is_ok());
    }
    #[test]
    fn address_of_is_stable_and_20_bytes() {
        let a = address_of(&[0x4cu8; 32]).expect("derives");
        assert_eq!(a.len(), 20);
        assert_eq!(a, address_of(& [0x4cu8; 32]).unwrap(), "must be deterministic");
        assert_ne!(
            a, address_of(& [0x4du8; 32]).unwrap(), "different key, different address"
        );
    }
}
