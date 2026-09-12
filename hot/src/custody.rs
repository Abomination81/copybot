use serde::Serialize;
#[derive(Debug, Clone, PartialEq)]
pub struct Holding {
    pub lane: String,
    pub token: String,
    pub ledger: f64,
    pub venue: f64,
    pub in_flight: f64,
    pub in_flight_buy: f64,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Verdict {
    Agreed,
    Explained { delta: f64, in_flight: f64 },
    Unattributed { delta: f64 },
    Unknown,
}
impl Verdict {
    pub fn is_alarm(&self) -> bool {
        matches!(self, Verdict::Unattributed { .. })
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Verdict::Agreed => "agreed",
            Verdict::Explained { .. } => "explained",
            Verdict::Unattributed { .. } => "unattributed",
            Verdict::Unknown => "unknown",
        }
    }
}
pub const SHARE_TOLERANCE: f64 = 0.01;
pub const REL_TOLERANCE: f64 = 0.001;
pub const MIN_REPORTABLE_SHARES: f64 = 1.0;
fn tolerance_for(ledger: f64) -> f64 {
    SHARE_TOLERANCE.max(ledger.abs() * REL_TOLERANCE)
}
pub fn lane_share_of(total: f64, pool_claim: f64, ours: f64) -> f64 {
    (total - (pool_claim - ours)).max(0.0)
}
pub fn classify(h: &Holding, complete: bool) -> Verdict {
    if !complete || !h.venue.is_finite() || !h.ledger.is_finite() {
        return Verdict::Unknown;
    }
    let delta = h.venue - h.ledger;
    if delta.abs() <= tolerance_for(h.ledger) {
        return Verdict::Agreed;
    }
    let inflight = h.in_flight.abs();
    if delta < 0.0 && delta.abs() <= inflight + tolerance_for(h.ledger) {
        return Verdict::Explained {
            delta,
            in_flight: inflight,
        };
    }
    let inflight_buy = h.in_flight_buy.abs();
    if delta > 0.0 && delta <= inflight_buy + tolerance_for(h.ledger) {
        return Verdict::Explained {
            delta,
            in_flight: inflight_buy,
        };
    }
    if delta.abs() < MIN_REPORTABLE_SHARES {
        return Verdict::Agreed;
    }
    Verdict::Unattributed { delta }
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    pub lane: String,
    pub token: String,
    pub ledger: f64,
    pub venue: f64,
    pub delta: f64,
    pub verdict: String,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub checked: usize,
    pub agreed: usize,
    pub explained: usize,
    pub unattributed: usize,
    pub unknown: usize,
    pub unattributed_shares: f64,
    pub rows: Vec<Row>,
    pub complete: bool,
}
impl Report {
    pub fn is_alarm(&self) -> bool {
        self.unattributed > 0
    }
}
pub const MAX_ROWS: usize = 12;
pub fn audit(holdings: &[Holding], complete: bool) -> Report {
    let mut r = Report {
        checked: holdings.len(),
        agreed: 0,
        explained: 0,
        unattributed: 0,
        unknown: 0,
        unattributed_shares: 0.0,
        rows: Vec::new(),
        complete,
    };
    for h in holdings {
        let v = classify(h, complete);
        match &v {
            Verdict::Agreed => r.agreed += 1,
            Verdict::Explained { .. } => r.explained += 1,
            Verdict::Unknown => r.unknown += 1,
            Verdict::Unattributed { delta } => {
                r.unattributed += 1;
                r.unattributed_shares += delta.abs();
                r.rows
                    .push(Row {
                        lane: h.lane.clone(),
                        token: h.token.clone(),
                        ledger: h.ledger,
                        venue: h.venue,
                        delta: *delta,
                        verdict: v.kind().into(),
                    });
            }
        }
    }
    r.rows
        .sort_by(|a, b| {
            b
                .delta
                .abs()
                .partial_cmp(&a.delta.abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    r.rows.truncate(MAX_ROWS);
    r
}
pub fn describe(r: &Report) -> String {
    if !r.complete {
        return "custody: venue position read was incomplete — no comparison made"
            .into();
    }
    if r.unattributed == 0 {
        return format!(
            "custody: {} holdings agree with the venue ({} explained by orders \
                        in flight)",
            r.agreed, r.explained
        );
    }
    let worst = r.rows.first();
    format!(
        "custody: {} holding(s) moved with NO order of ours behind them — {:.4} shares \
             total{}",
        r.unattributed, r.unattributed_shares, worst.map(| w |
        format!("; worst {} …{} ledger {:.4} vs venue {:.4} ({:+.4})", w.lane, & w
        .token[w.token.len().saturating_sub(8)..], w.ledger, w.venue, w.delta))
        .unwrap_or_default()
    )
}
