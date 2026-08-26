//! Deciding when a price is "very low".
//!
//! "Low" is only meaningful relative to history, so this is deliberately
//! percentile-based rather than a fixed number: an item that normally costs
//! 900g and now costs 400g is interesting, and no absolute threshold could
//! know that without being re-tuned every patch.
//!
//! Two consequences worth knowing:
//!
//! * **Alerting is dead until there is history.** With fewer than
//!   [`AlertRule::min_samples`] observations there is no baseline, and the rule
//!   falls back to the optional per-item hard floor. Expect roughly a week of
//!   collection before percentile alerts start firing usefully.
//! * **Thin markets are ignored.** A cheap price on 4 units is not a buying
//!   opportunity, it is noise, so [`AlertRule::min_quantity`] filters it out
//!   before anything else is considered.

use cluster_core::Millis;
use serde::{Deserialize, Serialize};

use super::{Copper, ItemId, PriceSample, Region};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertSeverity {
    /// Cheap: below the configured percentile of recent history.
    Low,
    /// Unusually cheap: at or below half that percentile, or under a hard floor.
    VeryLow,
}

impl AlertSeverity {
    pub const fn as_str(self) -> &'static str {
        match self {
            AlertSeverity::Low => "low",
            AlertSeverity::VeryLow => "very_low",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertRule {
    /// How far back the baseline looks.
    pub lookback_ms: u64,
    /// Alert when the current price is at or below this percentile of the
    /// baseline. 10 means "cheaper than 90% of the last fortnight".
    pub percentile: u8,
    /// Below this many observations, the percentile is not trustworthy and
    /// only the hard floor applies.
    pub min_samples: usize,
    /// Markets thinner than this are ignored entirely.
    pub min_quantity: u64,
    /// Minimum gap between two alerts for the same item.
    pub cooldown_ms: u64,
}

impl Default for AlertRule {
    fn default() -> Self {
        Self {
            lookback_ms: 14 * 24 * 60 * 60 * 1000,
            percentile: 10,
            min_samples: 48,
            min_quantity: 20,
            cooldown_ms: 6 * 60 * 60 * 1000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alert {
    pub item: ItemId,
    pub region: Region,
    pub severity: AlertSeverity,
    pub observed_at: Millis,
    /// The price that triggered this.
    pub current: Copper,
    /// What it usually costs -- the median of the baseline window.
    pub baseline: Copper,
    /// The threshold that was crossed.
    pub threshold: Copper,
    /// How far below baseline, as a percentage. Saturates at 0.
    pub discount_percent: u8,
    pub quantity: u64,
}

impl Alert {
    pub fn headline(&self, name: &str) -> String {
        format!(
            "{name} is {}% below normal on {}: {} (usually {})",
            self.discount_percent, self.region, self.current, self.baseline
        )
    }
}

/// Evaluate one item against its own history.
///
/// `history` is the baseline window, oldest or newest first -- order does not
/// matter, it is sorted internally. `current` is excluded from its own
/// baseline by the caller passing only prior samples.
pub fn evaluate(
    rule: &AlertRule,
    current: &PriceSample,
    history: &[PriceSample],
    floor: Option<Copper>,
) -> Option<Alert> {
    if current.quantity < rule.min_quantity {
        return None;
    }

    // A hard floor works from the first observation, which is what makes the
    // tracker useful before a baseline exists.
    if let Some(floor) = floor
        && current.p05_unit_price <= floor
    {
        return Some(build(rule, current, floor, floor, AlertSeverity::VeryLow));
    }

    let window: Vec<u64> = history
        .iter()
        .filter(|s| current.observed_at.since(s.observed_at) <= rule.lookback_ms)
        .map(|s| s.p05_unit_price.get())
        .collect();

    if window.len() < rule.min_samples {
        return None;
    }

    let threshold = Copper(percentile(&window, rule.percentile));
    if current.p05_unit_price > threshold {
        return None;
    }

    let baseline = Copper(percentile(&window, 50));
    let severe = Copper(percentile(
        &window,
        rule.percentile.saturating_div(2).max(1),
    ));
    let severity = if current.p05_unit_price <= severe {
        AlertSeverity::VeryLow
    } else {
        AlertSeverity::Low
    };

    Some(build(rule, current, threshold, baseline, severity))
}

fn build(
    _rule: &AlertRule,
    current: &PriceSample,
    threshold: Copper,
    baseline: Copper,
    severity: AlertSeverity,
) -> Alert {
    let discount = if baseline.get() == 0 {
        0
    } else {
        let saved = baseline.get().saturating_sub(current.p05_unit_price.get());
        ((saved * 100) / baseline.get()).min(100) as u8
    };
    Alert {
        item: current.item,
        region: current.region,
        severity,
        observed_at: current.observed_at,
        current: current.p05_unit_price,
        baseline,
        threshold,
        discount_percent: discount,
        quantity: current.quantity,
    }
}

/// Nearest-rank percentile over an unsorted slice.
fn percentile(values: &[u64], percent: u8) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = ((sorted.len() as u64 * percent as u64).div_ceil(100)).max(1) as usize;
    sorted[rank.min(sorted.len()) - 1]
}
