pub const PREFIX: &str = "L-";
pub const HEX_LEN: usize = 12;
pub fn mint(leader: &str) -> String {
    let lower = leader.trim().to_ascii_lowercase();
    let norm: String = lower
        .trim_start_matches("0x")
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in norm.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{PREFIX}{:0width$x}", h & 0x0000_ffff_ffff_ffff, width = HEX_LEN)
}
pub fn is_valid(id: &str) -> bool {
    id.len() == PREFIX.len() + HEX_LEN && id.starts_with(PREFIX)
        && id[PREFIX.len()..].chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
}
#[derive(Debug, Clone, PartialEq)]
pub enum Check {
    Mint(String),
    Agrees,
    Conflict { stored: String, expected: String },
    Malformed(String),
}
pub fn check(stored: Option<&str>, leader: &str) -> Check {
    let expected = mint(leader);
    match stored {
        None => Check::Mint(expected),
        Some(s) if s.is_empty() => Check::Mint(expected),
        Some(s) if !is_valid(s) => Check::Malformed(s.to_string()),
        Some(s) if s == expected => Check::Agrees,
        Some(s) => {
            Check::Conflict {
                stored: s.to_string(),
                expected,
            }
        }
    }
}
