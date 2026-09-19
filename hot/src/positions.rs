use std::collections::HashMap;
#[derive(Debug, Clone, PartialEq)]
pub enum Completeness {
    Complete,
    Truncated { pages: usize, cap: usize },
    Failed { after_pages: usize, why: String },
}
impl Completeness {
    pub fn is_complete(&self) -> bool {
        matches!(self, Completeness::Complete)
    }
    pub fn reason(&self) -> String {
        match self {
            Completeness::Complete => "complete".into(),
            Completeness::Truncated { pages, cap } => {
                format!("truncated after {pages} page(s) at the {cap}-page budget")
            }
            Completeness::Failed { after_pages, why } => {
                format!("page {} failed: {why}", after_pages + 1)
            }
        }
    }
}
#[derive(Debug, Clone)]
pub struct Positions {
    pub rows: Vec<serde_json::Value>,
    pub completeness: Completeness,
    pub as_of: i64,
}
pub const PAGE: usize = 500;
pub const MAX_PAGES: usize = 50;
impl Positions {
    /// A malformed balance row cannot establish that any token is absent.
    pub fn confirms_zero(&self, token: &str, not_before: i64) -> bool {
        if !self.may_act_destructively() || self.as_of < not_before {
            return false;
        }
        for row in &self.rows {
            let Some(asset) = row["asset"].as_str().filter(|s| !s.is_empty()) else {
                return false;
            };
            let Some(size) = row["size"].as_f64()
                .or_else(|| row["size"].as_str().and_then(|s| s.parse().ok())) else {
                return false;
            };
            if !size.is_finite() || size < 0.0 || (asset == token && size > 1e-9) {
                return false;
            }
        }
        true
    }
    pub fn empty(as_of: i64) -> Self {
        Self {
            rows: Vec::new(),
            completeness: Completeness::Complete,
            as_of,
        }
    }
    pub fn may_act_destructively(&self) -> bool {
        self.completeness.is_complete()
    }
    pub fn by_asset(&self) -> HashMap<String, f64> {
        let mut out = HashMap::with_capacity(self.rows.len());
        for r in &self.rows {
            let Some(a) = r["asset"].as_str() else { continue };
            let sz = r["size"]
                .as_f64()
                .or_else(|| r["size"].as_str().and_then(|s| s.parse().ok()))
                .unwrap_or(0.0);
            if sz.is_finite() {
                *out.entry(a.to_string()).or_insert(0.0) += sz;
            }
        }
        out
    }
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}
pub fn page_url(
    base: &str,
    user: &str,
    size_threshold: &str,
    offset: usize,
    extra: &str,
) -> String {
    format!(
        "{base}/positions?user={user}&sizeThreshold={size_threshold}\
             &limit={PAGE}&offset={offset}{extra}"
    )
}
pub fn judge(
    page_lens: &[usize],
    failed_at: Option<(usize, String)>,
    cap: usize,
) -> Completeness {
    if let Some((idx, why)) = failed_at {
        return Completeness::Failed {
            after_pages: idx,
            why,
        };
    }
    match page_lens.last() {
        Some(&n) if n < PAGE => Completeness::Complete,
        None => Completeness::Complete,
        Some(_) if page_lens.len() >= cap => {
            Completeness::Truncated {
                pages: page_lens.len(),
                cap,
            }
        }
        Some(_) => {
            Completeness::Truncated {
                pages: page_lens.len(),
                cap,
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn row(asset: &str, size: &str) -> serde_json::Value {
        serde_json::json!({ "asset" : asset, "size" : size })
    }
    #[test]
    fn a_SHORT_final_page_proves_completeness() {
        assert_eq!(judge(& [500, 500, 12], None, MAX_PAGES), Completeness::Complete);
        assert_eq!(judge(& [3], None, MAX_PAGES), Completeness::Complete);
        assert_eq!(judge(& [0], None, MAX_PAGES), Completeness::Complete);
        assert_eq!(judge(& [], None, MAX_PAGES), Completeness::Complete);
    }
    #[test]
    fn a_FULL_final_page_at_the_budget_is_TRUNCATED() {
        let lens = vec![500usize; MAX_PAGES];
        assert!(
            matches!(judge(& lens, None, MAX_PAGES), Completeness::Truncated { .. })
        );
    }
    #[test]
    fn a_FAILED_page_is_never_COMPLETE_however_much_we_got() {
        let c = judge(&[500, 500], Some((2, "503".into())), MAX_PAGES);
        assert!(! c.is_complete());
        assert!(c.reason().contains("page 3 failed"));
        assert!(! judge(& [7], Some((1, "timeout".into())), MAX_PAGES).is_complete());
    }
    #[test]
    fn DESTRUCTIVE_action_is_refused_on_anything_but_a_complete_read() {
        let mk = |c: Completeness| Positions {
            rows: vec![],
            completeness: c,
            as_of: 0,
        };
        assert!(mk(Completeness::Complete).may_act_destructively());
        assert!(
            ! mk(Completeness::Truncated { pages : 50, cap : 50 })
            .may_act_destructively()
        );
        assert!(
            ! mk(Completeness::Failed { after_pages : 1, why : "x".into() })
            .may_act_destructively()
        );
    }
    #[test]
    fn by_asset_parses_BOTH_numeric_and_string_sizes_and_sums_duplicates() {
        let p = Positions {
            rows: vec![
                row("T", "10.5"), serde_json::json!({ "asset" : "U", "size" : 3.25 }),
                row("T", "1.5"), serde_json::json!({ "nope" : 1 })
            ],
            completeness: Completeness::Complete,
            as_of: 0,
        };
        let m = p.by_asset();
        assert!((m["T"] - 12.0).abs() < 1e-9, "duplicate rows for one asset must SUM");
        assert!((m["U"] - 3.25).abs() < 1e-9);
        assert_eq!(m.len(), 2, "a row with no asset is skipped, not fatal");
    }
    #[test]
    fn page_url_advances_the_offset_and_keeps_extras() {
        let u = page_url("https://x", "0xabc", "0.0001", 1000, "&redeemable=true");
        assert!(u.contains("offset=1000"));
        assert!(u.contains("limit=500"));
        assert!(u.contains("redeemable=true"));
    }
}
pub async fn fetch(
    http: &reqwest::Client,
    base: &str,
    user: &str,
    size_threshold: &str,
    extra: &str,
    now: i64,
) -> Positions {
    let mut rows = Vec::new();
    let mut lens = Vec::new();
    let mut failed = None;
    for page in 0..MAX_PAGES {
        let url = page_url(base, user, size_threshold, page * PAGE, extra);
        let got = match http.get(&url).send().await {
            Ok(r) if r.status().is_success() => {
                r.json::<Vec<serde_json::Value>>()
                    .await
                    .map_err(|e| format!("decode: {e}"))
            }
            Ok(r) => Err(format!("http {}", r.status().as_u16())),
            Err(e) => Err(format!("transport: {e}")),
        };
        match got {
            Ok(batch) => {
                let n = batch.len();
                rows.extend(batch);
                lens.push(n);
                if n < PAGE {
                    break;
                }
            }
            Err(why) => {
                failed = Some((page, why));
                break;
            }
        }
    }
    let completeness = judge(&lens, failed, MAX_PAGES);
    Positions {
        rows,
        completeness,
        as_of: now,
    }
}
