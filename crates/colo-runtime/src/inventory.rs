//! Expected venue inventory, checked against balance snapshots.
//!
//! The ledger starts from an approved baseline and moves with every fill:
//! base and quote by the traded amounts, the fee asset by the fee. A venue
//! balance below the expected amount by more than the tolerance is a
//! shortfall; a surplus is ignored, so deposits and outside activity never
//! alarm. Only an explicit rebaseline moves the starting point.

use std::collections::BTreeMap;

use gateway::runtime::AssetBalance;
use serde::{Deserialize, Serialize};

/// Shortfall tolerance as a share of the asset's amount, in basis points.
pub const DRIFT_TOLERANCE_BPS: f64 = 10.0;
/// Consecutive settled snapshots that must show a shortfall before it counts.
pub const DRIFT_CHECKS: u32 = 3;
/// Absolute floor for assets with no step size, against float rounding.
const MIN_TOLERANCE: f64 = 1e-8;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Inventory {
    expected: BTreeMap<String, f64>,
    /// Fixed at the baseline: the larger of the bps share and the step floor.
    tolerance: BTreeMap<String, f64>,
    #[serde(skip)]
    streak: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Shortfall {
    pub asset: String,
    pub expected: f64,
    pub actual: f64,
}

impl std::fmt::Display for Shortfall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is {} short: expected {}, venue has {}",
            self.asset,
            self.expected - self.actual,
            self.expected,
            self.actual
        )
    }
}

impl Inventory {
    /// Starts the ledger from these amounts. `floors` gives each asset's
    /// minimum tolerance (two venue steps for a base asset).
    pub fn rebaseline(&mut self, amounts: &BTreeMap<String, f64>, floors: &BTreeMap<String, f64>) {
        self.expected = amounts.clone();
        self.tolerance = amounts
            .iter()
            .map(|(asset, amount)| {
                let floor = floors.get(asset).copied().unwrap_or(0.0);
                let share = amount.abs() * DRIFT_TOLERANCE_BPS / 10_000.0;
                (asset.clone(), share.max(floor).max(MIN_TOLERANCE))
            })
            .collect();
        for (asset, floor) in floors {
            self.tolerance.entry(asset.clone()).or_insert(floor.max(MIN_TOLERANCE));
        }
        self.streak = 0;
    }

    /// No baseline yet: nothing is checked.
    pub fn is_tracking(&self) -> bool {
        !self.expected.is_empty()
    }

    pub fn expected(&self, asset: &str) -> f64 {
        self.expected.get(asset).copied().unwrap_or(0.0)
    }

    /// Moves the expected amount of one asset; ignored before a baseline.
    pub fn record(&mut self, asset: &str, delta: f64) {
        if self.is_tracking() && delta.is_finite() {
            *self.expected.entry(asset.to_string()).or_insert(0.0) += delta;
        }
    }

    /// Assets the venue holds less of than expected, beyond the tolerance.
    /// A full snapshot is assumed: an asset it leaves out holds zero.
    pub fn shortfalls(&self, balances: &[AssetBalance]) -> Vec<Shortfall> {
        self.expected
            .iter()
            .filter_map(|(asset, expected)| {
                let actual: f64 = balances
                    .iter()
                    .filter(|b| b.asset == *asset)
                    .map(AssetBalance::total)
                    .sum();
                let tolerance = self
                    .tolerance
                    .get(asset)
                    .copied()
                    .unwrap_or(MIN_TOLERANCE)
                    .max(expected.abs() * DRIFT_TOLERANCE_BPS / 10_000.0);
                (expected - actual > tolerance).then(|| Shortfall {
                    asset: asset.clone(),
                    expected: *expected,
                    actual,
                })
            })
            .collect()
    }

    /// One snapshot check. Returns the shortfalls once they have shown on
    /// `DRIFT_CHECKS` settled snapshots in a row. An unsettled snapshot (an
    /// order whose venue state is unknown) neither counts nor resets.
    pub fn check(&mut self, balances: &[AssetBalance], settled: bool) -> Option<Vec<Shortfall>> {
        if !self.is_tracking() {
            return None;
        }
        let shortfalls = self.shortfalls(balances);
        if shortfalls.is_empty() {
            self.streak = 0;
            return None;
        }
        if !settled {
            return None;
        }
        self.streak += 1;
        (self.streak >= DRIFT_CHECKS).then_some(shortfalls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn balance(asset: &str, total: f64) -> AssetBalance {
        AssetBalance { asset: asset.into(), free: total, locked: 0.0 }
    }

    fn ledger() -> Inventory {
        let mut inventory = Inventory::default();
        inventory.rebaseline(
            &BTreeMap::from([("BTC".into(), 1.0), ("USDT".into(), 10_000.0)]),
            &BTreeMap::from([("BTC".into(), 0.00002)]),
        );
        inventory
    }

    #[test]
    fn untracked_ledger_never_alarms() {
        let mut inventory = Inventory::default();
        inventory.record("BTC", 5.0);
        for _ in 0..DRIFT_CHECKS {
            assert!(inventory.check(&[], true).is_none());
        }
    }

    #[test]
    fn surplus_is_ignored_and_small_drift_is_tolerated() {
        let mut inventory = ledger();
        let balances = [balance("BTC", 3.0), balance("USDT", 9_995.0)];
        for _ in 0..DRIFT_CHECKS {
            assert!(inventory.check(&balances, true).is_none());
        }
    }

    #[test]
    fn shortfall_counts_only_after_consecutive_settled_checks() {
        let mut inventory = ledger();
        let short = [balance("BTC", 1.0), balance("USDT", 9_000.0)];
        assert!(inventory.check(&short, true).is_none());
        assert!(inventory.check(&short, false).is_none(), "unsettled does not count");
        assert!(inventory.check(&short, true).is_none());
        let found = inventory.check(&short, true).expect("third settled check alarms");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].asset, "USDT");
    }

    #[test]
    fn a_clean_snapshot_resets_the_streak() {
        let mut inventory = ledger();
        let short = [balance("BTC", 1.0), balance("USDT", 9_000.0)];
        let clean = [balance("BTC", 1.0), balance("USDT", 10_000.0)];
        inventory.check(&short, true);
        inventory.check(&short, true);
        inventory.check(&clean, true);
        assert!(inventory.check(&short, true).is_none());
    }

    #[test]
    fn missing_asset_counts_as_zero_and_fees_move_their_own_asset() {
        let mut inventory = ledger();
        inventory.record("BNB", -0.01);
        assert_eq!(inventory.expected("BNB"), -0.01);
        let found = inventory.shortfalls(&[balance("USDT", 10_000.0)]);
        assert_eq!(found.iter().map(|s| s.asset.as_str()).collect::<Vec<_>>(), ["BTC"]);
    }
}
