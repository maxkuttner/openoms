use std::collections::BTreeMap;

use gateway::runtime::AssetBalance;
use serde::{Deserialize, Serialize};

use crate::RuntimeError;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reservation {
    pub asset: String,
    pub amount: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct AssetAccount {
    /// Venue total (free + locked). Local commitments are not removed from it;
    /// free is always derived, so a locked snapshot is never charged twice.
    total: f64,
    committed: f64,
}

/// Base and quote inventory across every instrument of one venue account.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssetLedger {
    assets: BTreeMap<String, AssetAccount>,
}

impl AssetLedger {
    pub fn free(&self, asset: &str) -> f64 {
        self.assets
            .get(asset)
            .map(|a| (a.total - a.committed).max(0.0))
            .unwrap_or(0.0)
    }

    pub fn total(&self, asset: &str) -> f64 {
        self.assets.get(asset).map(|a| a.total).unwrap_or(0.0)
    }

    pub fn committed(&self, asset: &str) -> f64 {
        self.assets.get(asset).map(|a| a.committed).unwrap_or(0.0)
    }

    /// Replace venue totals. Local commitments stay, so the same working order
    /// is not charged once as venue-locked and again as local-committed.
    pub fn snapshot(&mut self, balances: &[AssetBalance]) {
        for account in self.assets.values_mut() {
            account.total = 0.0;
        }
        for balance in balances {
            let account = self.assets.entry(balance.asset.clone()).or_default();
            account.total = (balance.free + balance.locked).max(0.0);
        }
    }

    pub fn can_reserve(&self, asset: &str, amount: f64) -> bool {
        amount.is_finite() && amount >= 0.0 && amount <= self.free(asset)
    }

    pub fn reserve(&mut self, asset: &str, amount: f64) -> Result<Reservation, RuntimeError> {
        if !amount.is_finite() || amount < 0.0 {
            return Err(RuntimeError::CorruptState(
                "cannot reserve a non-finite or negative amount".into(),
            ));
        }
        if !self.can_reserve(asset, amount) {
            return Err(RuntimeError::InsufficientAssets {
                asset: asset.to_string(),
                required: amount,
                available: self.free(asset),
            });
        }
        self.assets.entry(asset.to_string()).or_default().committed += amount;
        Ok(Reservation { asset: asset.to_string(), amount })
    }

    /// Release at most what this asset has committed and never go negative.
    pub fn release(&mut self, asset: &str, amount: f64) {
        if !amount.is_finite() || amount <= 0.0 {
            return;
        }
        if let Some(account) = self.assets.get_mut(asset) {
            account.committed = (account.committed - amount).max(0.0);
        }
    }
}
