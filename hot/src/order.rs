use k256::ecdsa::{RecoveryId, Signature, SigningKey};
use tiny_keccak::{Hasher, Keccak};
pub const CHAIN_ID: u64 = 137;
pub const CTF_EXCHANGE_V2: &str = "0xe111180000d2663c0091e4f400237545b87b996b";
pub const NEG_RISK_CTF_EXCHANGE_V2: &str = "0xe2222d279d744050d28e00520010520000310f59";
const ORDER_TYPE: &str = "Order(uint256 salt,address maker,address signer,uint256 tokenId,\
uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,uint256 timestamp,\
bytes32 metadata,bytes32 builder)";
const SOLADY_TYPE: &str = "TypedDataSign(Order contents,string name,string version,\
uint256 chainId,address verifyingContract,bytes32 salt)Order(uint256 salt,address maker,\
address signer,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint8 side,\
uint8 signatureType,uint256 timestamp,bytes32 metadata,bytes32 builder)";
const DOMAIN_TYPE: &str = "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
const DOMAIN_NAME: &str = "Polymarket CTF Exchange";
const DOMAIN_VERSION: &str = "2";
pub fn keccak(data: &[u8]) -> [u8; 32] {
    let mut k = Keccak::v256();
    let mut out = [0u8; 32];
    k.update(data);
    k.finalize(&mut out);
    out
}
fn word_u128(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}
fn word_addr(addr: &str) -> [u8; 32] {
    let clean = addr.trim_start_matches("0x");
    let bytes = hex::decode(clean).expect("address must be hex");
    assert_eq!(bytes.len(), 20, "address must be 20 bytes");
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(&bytes);
    w
}
#[derive(Debug, Clone)]
pub struct Order {
    pub salt: u128,
    pub maker: String,
    pub signer: String,
    pub token_id: String,
    pub maker_amount: u128,
    pub taker_amount: u128,
    pub side: u8,
    pub signature_type: u8,
    pub timestamp: u128,
    pub neg_risk: bool,
}
fn word_decimal_u256(dec: &str) -> [u8; 32] {
    let mut acc = [0u8; 32];
    for ch in dec.bytes() {
        let d = (ch - b'0') as u16;
        debug_assert!(d < 10, "tokenId must be decimal");
        let mut carry = d as u32;
        for i in (0..32).rev() {
            let cur = acc[i] as u32 * 10 + carry;
            acc[i] = (cur & 0xff) as u8;
            carry = cur >> 8;
        }
    }
    acc
}
impl Order {
    pub fn exchange(&self) -> &'static str {
        if self.neg_risk { NEG_RISK_CTF_EXCHANGE_V2 } else { CTF_EXCHANGE_V2 }
    }
    fn domain_separator(&self) -> [u8; 32] {
        let mut buf = Vec::with_capacity(160);
        buf.extend_from_slice(&keccak(DOMAIN_TYPE.as_bytes()));
        buf.extend_from_slice(&keccak(DOMAIN_NAME.as_bytes()));
        buf.extend_from_slice(&keccak(DOMAIN_VERSION.as_bytes()));
        buf.extend_from_slice(&word_u128(CHAIN_ID as u128));
        buf.extend_from_slice(&word_addr(self.exchange()));
        keccak(&buf)
    }
    fn struct_hash(&self) -> [u8; 32] {
        let mut buf = Vec::with_capacity(32 * 12);
        buf.extend_from_slice(&keccak(ORDER_TYPE.as_bytes()));
        buf.extend_from_slice(&word_u128(self.salt));
        buf.extend_from_slice(&word_addr(&self.maker));
        buf.extend_from_slice(&word_addr(&self.signer));
        buf.extend_from_slice(&word_decimal_u256(&self.token_id));
        buf.extend_from_slice(&word_u128(self.maker_amount));
        buf.extend_from_slice(&word_u128(self.taker_amount));
        buf.extend_from_slice(&word_u128(self.side as u128));
        buf.extend_from_slice(&word_u128(self.signature_type as u128));
        buf.extend_from_slice(&word_u128(self.timestamp));
        buf.extend_from_slice(&[0u8; 32]);
        buf.extend_from_slice(&[0u8; 32]);
        keccak(&buf)
    }
    pub fn digest(&self) -> [u8; 32] {
        let mut buf = Vec::with_capacity(66);
        buf.extend_from_slice(&[0x19, 0x01]);
        buf.extend_from_slice(&self.domain_separator());
        buf.extend_from_slice(&self.struct_hash());
        keccak(&buf)
    }
    pub fn sign_1271(&self, private_key: &[u8; 32]) -> String {
        let contents_hash = self.struct_hash();
        let app_sep = self.domain_separator();
        let mut buf = Vec::with_capacity(32 * 7);
        buf.extend_from_slice(&keccak(SOLADY_TYPE.as_bytes()));
        buf.extend_from_slice(&contents_hash);
        buf.extend_from_slice(&keccak(b"DepositWallet"));
        buf.extend_from_slice(&keccak(b"1"));
        buf.extend_from_slice(&word_u128(CHAIN_ID as u128));
        buf.extend_from_slice(&word_addr(&self.signer));
        buf.extend_from_slice(&[0u8; 32]);
        let tds_struct_hash = keccak(&buf);
        let mut d = Vec::with_capacity(66);
        d.extend_from_slice(&[0x19, 0x01]);
        d.extend_from_slice(&app_sep);
        d.extend_from_slice(&tds_struct_hash);
        let digest = keccak(&d);
        let sk = SigningKey::from_bytes(private_key.into()).expect("bad private key");
        let (sig, recid): (Signature, RecoveryId) = sk
            .sign_prehash_recoverable(&digest)
            .expect("sign failed");
        let mut inner = Vec::with_capacity(65);
        inner.extend_from_slice(&sig.r().to_bytes());
        inner.extend_from_slice(&sig.s().to_bytes());
        inner.push(recid.to_byte() + 27);
        format!(
            "0x{}{}{}{}{:04x}", hex::encode(inner), hex::encode(app_sep),
            hex::encode(contents_hash), hex::encode(ORDER_TYPE.as_bytes()), ORDER_TYPE
            .len() as u16,
        )
    }
    pub fn sign_for_type(&self, private_key: &[u8; 32]) -> String {
        if self.signature_type == 3 {
            self.sign_1271(private_key)
        } else {
            self.sign(private_key)
        }
    }
    pub fn sign(&self, private_key: &[u8; 32]) -> String {
        let sk = SigningKey::from_bytes(private_key.into()).expect("bad private key");
        let digest = self.digest();
        let (sig, recid): (Signature, RecoveryId) = sk
            .sign_prehash_recoverable(&digest)
            .expect("sign failed");
        let mut out = Vec::with_capacity(65);
        out.extend_from_slice(&sig.r().to_bytes());
        out.extend_from_slice(&sig.s().to_bytes());
        out.push(recid.to_byte() + 27);
        format!("0x{}", hex::encode(out))
    }
}
pub fn amounts(price: f64, size: f64, side: u8) -> (u128, u128) {
    let shares_micro = (size * 1e6).round() as u128;
    if side == 0 {
        let usd_cents = (price * size * 100.0 + 1e-7).floor();
        ((usd_cents * 10_000.0) as u128, shares_micro)
    } else {
        let shares_2dp = (size * 100.0 + 1e-9).floor() / 100.0;
        let usd_5dp = (shares_2dp * price * 1e5 + 1e-9).floor() / 1e5;
        (((shares_2dp * 1e6).round()) as u128, ((usd_5dp * 1e6).round()) as u128)
    }
}
pub fn json_body(
    o: &Order,
    signature: &str,
    owner: &str,
    order_type: &str,
) -> serde_json::Value {
    json_body_exp(o, signature, owner, order_type, 0)
}
pub fn json_body_exp(
    o: &Order,
    signature: &str,
    owner: &str,
    order_type: &str,
    expiration: u64,
) -> serde_json::Value {
    serde_json::json!(
        { "order" : { "salt" : o.salt, "maker" : o.maker, "signer" : o.signer, "tokenId"
        : o.token_id, "makerAmount" : o.maker_amount.to_string(), "takerAmount" : o
        .taker_amount.to_string(), "side" : if o.side == 0 { "BUY" } else { "SELL" },
        "signatureType" : o.signature_type, "timestamp" : o.timestamp.to_string(),
        "metadata" : format!("0x{}", "00".repeat(32)), "builder" : format!("0x{}", "00"
        .repeat(32)), "expiration" : expiration.to_string(), "signature" : signature, },
        "owner" : owner, "orderType" : order_type, "deferExec" : false, "postOnly" :
        false, }
    )
}
pub const MAX_SALT: u128 = 1u128 << 62;
#[inline]
pub fn safe_salt(seed: u128) -> u128 {
    seed % MAX_SALT
}
