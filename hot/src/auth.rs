use crate::order::keccak;
use hmac::{Hmac, Mac};
use k256::ecdsa::{RecoveryId, Signature, SigningKey};
use sha2::Sha256;
const CLOB_DOMAIN_TYPE: &str = "EIP712Domain(string name,string version,uint256 chainId)";
const CLOB_DOMAIN_NAME: &str = "ClobAuthDomain";
const CLOB_DOMAIN_VERSION: &str = "1";
const CLOB_AUTH_TYPE: &str = "ClobAuth(address address,string timestamp,uint256 nonce,string message)";
const MSG_TO_SIGN: &str = "This message attests that I control the given wallet";
pub const POLY_ADDRESS: &str = "POLY_ADDRESS";
pub const POLY_SIGNATURE: &str = "POLY_SIGNATURE";
pub const POLY_TIMESTAMP: &str = "POLY_TIMESTAMP";
pub const POLY_NONCE: &str = "POLY_NONCE";
pub const POLY_API_KEY: &str = "POLY_API_KEY";
pub const POLY_PASSPHRASE: &str = "POLY_PASSPHRASE";
#[derive(Debug, Clone, PartialEq)]
pub struct ApiCreds {
    pub key: String,
    pub secret: String,
    pub passphrase: String,
}
impl ApiCreds {
    pub fn redacted(&self) -> String {
        let tail = self.key.rsplit('-').next().unwrap_or("");
        format!("apiKey=…{tail} secret=<redacted> passphrase=<redacted>")
    }
}
fn word_u128(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}
fn word_addr(addr: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(addr.trim_start_matches("0x"))
        .map_err(|e| format!("address not hex: {e}"))?;
    if bytes.len() != 20 {
        return Err(format!("address must be 20 bytes, got {}", bytes.len()));
    }
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(&bytes);
    Ok(w)
}
fn b64url_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}
fn b64url_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for ch in s.bytes() {
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return Err(format!("bad base64 char {:?}", ch as char)),
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}
fn clob_auth_domain_separator() -> [u8; 32] {
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(&keccak(CLOB_DOMAIN_TYPE.as_bytes()));
    buf.extend_from_slice(&keccak(CLOB_DOMAIN_NAME.as_bytes()));
    buf.extend_from_slice(&keccak(CLOB_DOMAIN_VERSION.as_bytes()));
    buf.extend_from_slice(&word_u128(crate::order::CHAIN_ID as u128));
    keccak(&buf)
}
fn clob_auth_struct_hash(
    address: &str,
    timestamp: u64,
    nonce: u128,
) -> Result<[u8; 32], String> {
    let mut buf = Vec::with_capacity(160);
    buf.extend_from_slice(&keccak(CLOB_AUTH_TYPE.as_bytes()));
    buf.extend_from_slice(&word_addr(address)?);
    buf.extend_from_slice(&keccak(timestamp.to_string().as_bytes()));
    buf.extend_from_slice(&word_u128(nonce));
    buf.extend_from_slice(&keccak(MSG_TO_SIGN.as_bytes()));
    Ok(keccak(&buf))
}
pub fn clob_auth_digest(
    address: &str,
    timestamp: u64,
    nonce: u128,
) -> Result<[u8; 32], String> {
    let mut buf = Vec::with_capacity(66);
    buf.extend_from_slice(&[0x19, 0x01]);
    buf.extend_from_slice(&clob_auth_domain_separator());
    buf.extend_from_slice(&clob_auth_struct_hash(address, timestamp, nonce)?);
    Ok(keccak(&buf))
}
pub fn address_from_key(private_key: &[u8; 32]) -> Result<String, String> {
    let sk = SigningKey::from_bytes(private_key.into())
        .map_err(|e| format!("bad key: {e}"))?;
    let pt = sk.verifying_key().to_encoded_point(false);
    let bytes = pt.as_bytes();
    if bytes.len() != 65 || bytes[0] != 0x04 {
        return Err("unexpected public key encoding".into());
    }
    let h = keccak(&bytes[1..]);
    Ok(format!("0x{}", hex::encode(& h[12..])))
}
pub fn sign_clob_auth(
    private_key: &[u8; 32],
    address: &str,
    timestamp: u64,
    nonce: u128,
) -> Result<String, String> {
    let sk = SigningKey::from_bytes(private_key.into())
        .map_err(|e| format!("bad key: {e}"))?;
    let digest = clob_auth_digest(address, timestamp, nonce)?;
    let (sig, recid): (Signature, RecoveryId) = sk
        .sign_prehash_recoverable(&digest)
        .map_err(|e| format!("sign failed: {e}"))?;
    let mut out = Vec::with_capacity(65);
    out.extend_from_slice(&sig.r().to_bytes());
    out.extend_from_slice(&sig.s().to_bytes());
    out.push(recid.to_byte() + 27);
    Ok(format!("0x{}", hex::encode(out)))
}
pub fn l1_headers(
    private_key: &[u8; 32],
    address: &str,
    timestamp: u64,
    nonce: u128,
) -> Result<Vec<(&'static str, String)>, String> {
    Ok(
        vec![
            (POLY_ADDRESS, address.to_string()), (POLY_SIGNATURE,
            sign_clob_auth(private_key, address, timestamp, nonce) ?), (POLY_TIMESTAMP,
            timestamp.to_string()), (POLY_NONCE, nonce.to_string()),
        ],
    )
}
pub fn hmac_signature(
    secret: &str,
    timestamp: u64,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<String, String> {
    let key = b64url_decode(secret)?;
    let path = path.split('?').next().unwrap_or(path);
    let mut msg = format!("{timestamp}{method}{path}");
    if let Some(b) = body {
        if !b.is_empty() {
            msg.push_str(&b.replace('\'', "\""));
        }
    }
    let mut mac = <Hmac<Sha256>>::new_from_slice(&key)
        .map_err(|e| format!("hmac key: {e}"))?;
    mac.update(msg.as_bytes());
    Ok(b64url_encode(&mac.finalize().into_bytes()))
}
pub fn l2_headers(
    address: &str,
    creds: &ApiCreds,
    timestamp: u64,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<Vec<(&'static str, String)>, String> {
    Ok(
        vec![
            (POLY_ADDRESS, address.to_string()), (POLY_SIGNATURE, hmac_signature(& creds
            .secret, timestamp, method, path, body) ?), (POLY_TIMESTAMP, timestamp
            .to_string()), (POLY_API_KEY, creds.key.clone()), (POLY_PASSPHRASE, creds
            .passphrase.clone()),
        ],
    )
}
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
fn parse_creds(body: &str) -> Result<ApiCreds, String> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("creds not json: {e}"))?;
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    match (get("apiKey"), get("secret"), get("passphrase")) {
        (Some(key), Some(secret), Some(passphrase)) => {
            Ok(ApiCreds {
                key,
                secret,
                passphrase,
            })
        }
        _ => Err(format!("creds missing fields: {}", & body[..body.len().min(200)])),
    }
}
pub async fn derive_or_create_creds(
    http: &reqwest::Client,
    host: &str,
    private_key: &[u8; 32],
    address: &str,
    nonce: u128,
) -> Result<ApiCreds, String> {
    let ts = now_secs();
    let hdrs = l1_headers(private_key, address, ts, nonce)?;
    let apply = |mut rb: reqwest::RequestBuilder| {
        for (k, v) in &hdrs {
            rb = rb.header(*k, v);
        }
        rb
    };
    let r = apply(http.get(format!("{host}/auth/derive-api-key")))
        .send()
        .await
        .map_err(|e| format!("derive transport: {e}"))?;
    let status = r.status();
    let body = r.text().await.unwrap_or_default();
    if status.is_success() {
        if let Ok(c) = parse_creds(&body) {
            return Ok(c);
        }
    }
    let r2 = apply(http.post(format!("{host}/auth/api-key")))
        .send()
        .await
        .map_err(|e| format!("create transport: {e}"))?;
    let s2 = r2.status();
    let b2 = r2.text().await.unwrap_or_default();
    if s2.is_success() {
        return parse_creds(&b2);
    }
    Err(
        format!(
            "could not obtain L2 creds — derive {} ({}), create {} ({})", status, body
            .chars().take(120).collect::< String > (), s2, b2.chars().take(120)
            .collect::< String > ()
        ),
    )
}
