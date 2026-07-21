//! Gas pricing + net-profit gate. Caches the L2 base fee and the Arbitrum
//! `ArbGasInfo` precompile's L1 pricing params. The profit gate subtracts
//! BOTH L2 execution cost and the L1 calldata fee.
//!
//! Base fee source: the REAL `base_fee_per_gas` from block headers — pushed
//! via `on_new_head` on every ingest NewHead, plus a header poll as
//! fallback/initial fill. NEVER `eth_gasPrice`: that RPC suggestion is
//! commonly padded well above the real base fee and overprices every
//! opportunity — treat it as untrustworthy on any chain until proven
//! otherwise.
//!
//! Arbitrum's L1 fee model: `ArbGasInfo::getPricesInWei()` returns a
//! `perL1CalldataUnit` price already fully resolved (no separate base/blob
//! scalar multiplication step). `eth_estimateGas` on Arbitrum already folds
//! the L1 component into its answer, but that's only usable for a FINAL
//! pre-submission estimate, not the hot path's per-candidate-route profit
//! gate — that still needs this struct's locally-cached formula so scoring
//! many routes per block doesn't cost an RPC round trip each.

use crate::abi::IArbGasInfo;
use crate::config::GasConfig;
use crate::constants::ARB_GAS_INFO;
use alloy::eips::BlockNumberOrTag;
use alloy::providers::{DynProvider, Provider};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct GasStation {
    /// Latest L2 base fee (wei), from block headers.
    base_fee: AtomicU64,
    /// ArbGasInfo::getPricesInWei().perL1CalldataUnit — wei per L1 calldata
    /// unit (a byte, zero or non-zero, is 16 units per the precompile's own
    /// doc comment; see abi.rs).
    per_l1_calldata_unit: AtomicU64,
    /// ArbGasInfo::getL1BaseFeeEstimate() — kept for observability/sanity
    /// logging (check-config), not used directly in the fee formula (the
    /// pre-resolved `per_l1_calldata_unit` price already prices it in).
    l1_base_fee_estimate: AtomicU64,
    cfg: GasConfig,
}

impl GasStation {
    pub fn new(cfg: GasConfig) -> Self {
        Self {
            base_fee: AtomicU64::new(0),
            per_l1_calldata_unit: AtomicU64::new(0),
            l1_base_fee_estimate: AtomicU64::new(0),
            cfg,
        }
    }

    /// Push the base fee from a block header (called on every ingest NewHead).
    /// Zero-cost and always freshest; the `spawn` poll below is only a fallback.
    pub fn on_new_head(&self, base_fee_per_gas: u128) {
        if base_fee_per_gas > 0 {
            self.base_fee.store(base_fee_per_gas as u64, Ordering::Relaxed);
        }
    }

    /// Spawn a background task: latest-header base fee each `interval`
    /// (fallback for when no NewHead is flowing, e.g. tools / ingest gap),
    /// ArbGasInfo params every ~15 intervals (L1 pricing moves on L1 cadence,
    /// not per L2 block).
    pub fn spawn(self: &Arc<Self>, provider: DynProvider, interval: Duration) {
        let this = self.clone();
        tokio::spawn(async move {
            let oracle = IArbGasInfo::new(ARB_GAS_INFO, &provider);
            let mut tick: u64 = 0;
            loop {
                if let Ok(Some(block)) =
                    provider.get_block_by_number(BlockNumberOrTag::Latest).await
                {
                    if let Some(fee) = block.header.base_fee_per_gas {
                        this.on_new_head(fee as u128);
                    }
                }
                if tick % 15 == 0 {
                    if let Ok(v) = oracle.getL1BaseFeeEstimate().call().await {
                        this.l1_base_fee_estimate.store(v.to::<u64>(), Ordering::Relaxed);
                    }
                    if let Ok(p) = oracle.getPricesInWei().call().await {
                        this.per_l1_calldata_unit
                            .store(p.perL1CalldataUnit.to::<u64>(), Ordering::Relaxed);
                    }
                }
                tick += 1;
                tokio::time::sleep(interval).await;
            }
        });
    }

    pub fn base_fee(&self) -> u128 {
        self.base_fee.load(Ordering::Relaxed) as u128
    }

    pub fn l1_base_fee_estimate(&self) -> u128 {
        self.l1_base_fee_estimate.load(Ordering::Relaxed) as u128
    }

    /// Priority fee (wei/gas). Robinhood Chain's sequencer is FCFS — a tip
    /// buys nothing, so this stays at the minimum floor by default
    /// (prio_fraction = 0). The profit-scaled term only kicks in if
    /// prio_fraction is raised in config, and is clamped to max_prio_gwei
    /// either way.
    pub fn priority_fee(&self, net_profit_wei: u128, gas_units: u64) -> u128 {
        let budget = (net_profit_wei as f64 * self.cfg.prio_fraction) / gas_units.max(1) as f64;
        let min = (self.cfg.min_prio_gwei * 1e9) as u128;
        let max = (self.cfg.max_prio_gwei * 1e9) as u128;
        (budget as u128).clamp(min, max)
    }

    /// Estimate the L1 calldata fee (wei) for a tx with `calldata_len` bytes.
    /// Arbitrum model: calldata units = bytes * 16 (uniform, no zero/non-zero
    /// distinction on Nitro — see abi.rs's `IArbGasInfo::getPricesInWei`
    /// doc), fee = units * perL1CalldataUnit. `l1_fee_margin` is a safety
    /// multiplier on top of that estimate.
    pub fn estimate_l1_fee(&self, calldata_len: usize) -> u128 {
        let per_unit = self.per_l1_calldata_unit.load(Ordering::Relaxed) as u128;
        if per_unit == 0 {
            return 0;
        }
        let units = (calldata_len as u128) * 16;
        let fee = units * per_unit;
        (fee as f64 * self.cfg.l1_fee_margin) as u128
    }

    /// Total gas cost (wei) for a route: L2 execution + L1 calldata fee.
    pub fn total_gas_cost(&self, gas_units: u64, priority_fee: u128, calldata_len: usize) -> u128 {
        // Buffer the base fee one block's max increase (~12.5%) — an
        // EIP-1559-style buffer; Arbitrum's L2 base fee follows the same
        // per-block adjustment shape.
        let l2_fee_per_gas = (self.base_fee() * 9 / 8) + priority_fee;
        let l2 = l2_fee_per_gas * gas_units as u128;
        let l1 = self.estimate_l1_fee(calldata_len);
        l2 + l1
    }
}

/// Per-hop + overhead gas estimate for a route (refined from receipts
/// later). These numeric placeholders need recalibration against real
/// `eth_estimateGas` on Robinhood Chain — do not trust them as final.
pub fn route_gas_units(hop_kinds: &[HopGasKind], flashloan: bool) -> u64 {
    let mut g = 90_000; // base overhead
    for k in hop_kinds {
        g += match k {
            HopGasKind::V2 => 90_000,
            HopGasKind::V3 => 150_000,
            HopGasKind::AeroStable => 120_000,
            // unlock + swap + settle/take + possible WETH wrap at the segment
            // boundary. Deliberately conservative: adjacent V4 hops share one
            // unlock and cost less than this per hop.
            HopGasKind::V4 => 250_000,
        };
    }
    if flashloan {
        g += 90_000;
    }
    g
}

#[derive(Clone, Copy)]
pub enum HopGasKind {
    V2,
    V3,
    AeroStable,
    V4,
}
