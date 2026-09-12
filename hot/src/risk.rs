use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
pub struct RiskConfig {
    #[serde(default)]
    pub max_drawdown_usd: f64,
    #[serde(default)]
    pub max_drawdown_pct: f64,
    #[serde(default)]
    pub max_consecutive_losses: u32,
    #[serde(default)]
    pub pnl_skew_window: usize,
    #[serde(default)]
    pub pnl_skew_threshold_usd: f64,
    #[serde(default)]
    pub max_open_positions: usize,
}
#[derive(Debug, Default)]
pub struct LaneRisk {
    pub name: String,
    pub cfg: RiskConfig,
    pub realised_pnl: f64,
    pub high_water: f64,
    pub consecutive_losses: u32,
    pub open_positions: usize,
    pub closed: Vec<f64>,
    pub tripped: Option<String>,
}
impl LaneRisk {
    pub fn new(name: &str, cfg: RiskConfig) -> Self {
        Self {
            name: name.into(),
            cfg,
            ..Default::default()
        }
    }
    pub fn on_close(&mut self, pnl: f64) {
        self.realised_pnl += pnl;
        if self.realised_pnl > self.high_water {
            self.high_water = self.realised_pnl;
        }
        self.closed.push(pnl);
        if self.closed.len() > 5000 {
            self.closed.drain(..2500);
        }
        if pnl < 0.0 {
            self.consecutive_losses += 1;
        } else {
            self.consecutive_losses = 0;
        }
    }
    pub fn on_adjust(&mut self, pnl: f64) {
        self.realised_pnl += pnl;
        if self.realised_pnl > self.high_water {
            self.high_water = self.realised_pnl;
        }
    }
    pub fn on_open(&mut self) {
        self.open_positions += 1;
    }
    pub fn on_flat(&mut self) {
        self.open_positions = self.open_positions.saturating_sub(1);
    }
    pub fn drawdown(&self) -> f64 {
        (self.high_water - self.realised_pnl).max(0.0)
    }
    pub fn evaluate(&mut self) -> Option<String> {
        if let Some(r) = &self.tripped {
            return Some(r.clone());
        }
        let c = self.cfg;
        let dd = self.drawdown();
        if c.max_drawdown_usd > 0.0 && dd >= c.max_drawdown_usd {
            return self.trip(format!("drawdown ${dd:.2} >= ${:.2}", c.max_drawdown_usd));
        }
        if c.max_drawdown_pct > 0.0 && self.high_water > 0.0 {
            let frac = dd / self.high_water;
            if frac >= c.max_drawdown_pct {
                return self
                    .trip(
                        format!(
                            "drawdown {:.1}% of high-water ${:.2} >= {:.1}%", frac *
                            100.0, self.high_water, c.max_drawdown_pct * 100.0
                        ),
                    );
            }
        }
        if c.max_consecutive_losses > 0
            && self.consecutive_losses >= c.max_consecutive_losses
        {
            return self
                .trip(
                    format!(
                        "{} consecutive losses >= {}", self.consecutive_losses, c
                        .max_consecutive_losses
                    ),
                );
        }
        if c.pnl_skew_window > 0 && c.pnl_skew_threshold_usd < 0.0
            && self.closed.len() >= c.pnl_skew_window
        {
            let w: f64 = self
                .closed[self.closed.len() - c.pnl_skew_window..]
                .iter()
                .sum();
            if w <= c.pnl_skew_threshold_usd {
                return self
                    .trip(
                        format!(
                            "rolling {}-trade PnL ${w:.2} <= ${:.2}", c.pnl_skew_window,
                            c.pnl_skew_threshold_usd
                        ),
                    );
            }
        }
        if c.max_open_positions > 0 && self.open_positions > c.max_open_positions {
            return self
                .trip(
                    format!(
                        "{} open positions > {}", self.open_positions, c
                        .max_open_positions
                    ),
                );
        }
        None
    }
    fn trip(&mut self, reason: String) -> Option<String> {
        self.tripped = Some(reason.clone());
        Some(reason)
    }
    pub fn clear(&mut self) {
        self.tripped = None;
        self.high_water = self.realised_pnl;
        self.consecutive_losses = 0;
    }
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!(
            { "lane" : self.name, "realised_pnl" : (self.realised_pnl * 1e4).round() /
            1e4, "high_water" : (self.high_water * 1e4).round() / 1e4, "drawdown" : (self
            .drawdown() * 1e4).round() / 1e4, "consecutive_losses" : self
            .consecutive_losses, "open_positions" : self.open_positions, "closed_trades"
            : self.closed.len(), "tripped" : self.tripped, }
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn lane(cfg: RiskConfig) -> LaneRisk {
        LaneRisk::new("t", cfg)
    }
    #[test]
    fn dollar_drawdown_trips() {
        let mut l = lane(RiskConfig {
            max_drawdown_usd: 100.0,
            ..Default::default()
        });
        l.on_close(200.0);
        assert!(l.evaluate().is_none());
        l.on_close(-99.0);
        assert!(l.evaluate().is_none(), "99 < 100 must not trip");
        l.on_close(-2.0);
        assert!(l.evaluate().unwrap().contains("drawdown"));
    }
    #[test]
    fn pct_drawdown_measures_from_the_peak_not_from_zero() {
        let mut l = lane(RiskConfig {
            max_drawdown_pct: 0.20,
            ..Default::default()
        });
        l.on_close(100.0);
        l.on_close(-15.0);
        assert!(l.evaluate().is_none());
        l.on_close(-6.0);
        assert!(l.evaluate().unwrap().contains("high-water"));
    }
    #[test]
    fn a_profitable_lane_can_still_be_in_drawdown() {
        let mut l = lane(RiskConfig {
            max_drawdown_usd: 50.0,
            ..Default::default()
        });
        for _ in 0..10 {
            l.on_close(20.0);
        }
        assert!(l.evaluate().is_none());
        l.on_close(-60.0);
        assert!(l.evaluate().is_some());
    }
    #[test]
    fn a_win_resets_the_loss_streak() {
        let mut l = lane(RiskConfig {
            max_consecutive_losses: 3,
            ..Default::default()
        });
        l.on_close(-1.0);
        l.on_close(-1.0);
        assert!(l.evaluate().is_none());
        l.on_close(1.0);
        l.on_close(-1.0);
        l.on_close(-1.0);
        assert!(l.evaluate().is_none());
        l.on_close(-1.0);
        assert!(l.evaluate().unwrap().contains("consecutive losses"));
    }
    #[test]
    fn pnl_skew_needs_a_full_window_first() {
        let mut l = lane(RiskConfig {
            pnl_skew_window: 10,
            pnl_skew_threshold_usd: -1.0,
            ..Default::default()
        });
        for _ in 0..9 {
            l.on_close(-5.0);
        }
        assert!(l.evaluate().is_none(), "must not trip on a partial window");
        l.on_close(-5.0);
        assert!(l.evaluate().is_some());
    }
    #[test]
    fn a_tripped_breaker_stays_tripped_even_if_pnl_recovers() {
        let mut l = lane(RiskConfig {
            max_drawdown_usd: 10.0,
            ..Default::default()
        });
        l.on_close(100.0);
        l.on_close(-20.0);
        let r = l.evaluate().unwrap();
        for _ in 0..20 {
            l.on_close(50.0);
        }
        assert_eq!(
            l.evaluate().unwrap(), r, "auto-resume turns a bad day into a disaster"
        );
    }
    #[test]
    fn operator_clear_rebases_so_it_does_not_instantly_retrip() {
        let mut l = lane(RiskConfig {
            max_drawdown_usd: 10.0,
            ..Default::default()
        });
        l.on_close(100.0);
        l.on_close(-20.0);
        assert!(l.evaluate().is_some());
        l.clear();
        assert!(l.evaluate().is_none());
        assert_eq!(l.high_water, l.realised_pnl);
    }
    #[test]
    fn disabled_limits_never_trip() {
        let mut l = lane(RiskConfig::default());
        for _ in 0..50 {
            l.on_close(-1000.0);
        }
        assert!(l.evaluate().is_none());
    }
    #[test]
    fn runaway_open_positions_trip() {
        let mut l = lane(RiskConfig {
            max_open_positions: 3,
            ..Default::default()
        });
        for _ in 0..4 {
            l.on_open();
        }
        assert!(l.evaluate().unwrap().contains("open positions"));
    }
}
