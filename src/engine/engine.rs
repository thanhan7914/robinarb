//! The pool registry + trigger-side shared sets. State itself lives in
//! `ChainState` (memo → MDBX) — the engine no longer holds any pool state;
//! quotes pull truth on demand, so there is nothing here to keep in sync and
//! nothing that can go stale.

use super::pool_meta::PoolMeta;
use super::types::PoolIdx;
use alloy::primitives::Address;
use dashmap::DashSet;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// A "these pools changed, re-check the routes touching them" wake-up for
/// the evaluator. Purely advisory: quotes always read live chain state, so a
/// missed batch (or a pool it forgot to list) only delays a re-evaluation,
/// never makes a quote wrong. `pools` is the changed-pool set the evaluator
/// maps through the reverse index (`RouteStore::routes_for_pool`) to the
/// routes it actually re-evaluates — evaluating only those, not the whole
/// universe, is what keeps detect→sign latency low.
#[derive(Debug)]
pub struct ChangedBatch {
    pub block: u64,
    pub pools: Vec<PoolIdx>,
    pub ts: Instant,
}

/// Registry entry created at bootstrap.
pub struct PoolRegistration {
    pub meta: Arc<PoolMeta>,
    /// Per-anchor depth for the bootstrap liquidity gate: one `(anchor
    /// token, whole-unit depth)` entry per side of the pool that matches a
    /// configured `[[anchor]]` (a pool between two anchors, e.g. WETH/USDC,
    /// yields two entries), when computable at hydration time (V2/AeroStable:
    /// real reserve; V4: virtual reserve / 2). `None` = CL pool gated on its
    /// real anchor-token `balanceOf` instead (computed in a later pass).
    pub gate: Option<Vec<(Address, f64)>>,
}

pub struct Engine {
    /// Interned index -> metadata.
    pub metas: Vec<Arc<PoolMeta>>,
    /// Pool address -> index.
    pub by_address: FxHashMap<Address, PoolIdx>,
    /// Pools whose factory re-prices fee() on-chain (config `dynamic_fee`).
    /// Populated once at bootstrap.
    pub fee_dynamic: DashSet<PoolIdx>,
    /// Dynamic-fee pools the trigger loop just saw change: their fee likely
    /// moved (it re-prices on swaps and emits NO event of its own), so the fee
    /// poller re-reads them on its fast path instead of waiting for the sweep.
    pub fee_recheck: DashSet<PoolIdx>,
    pub head_block: AtomicU64,
}

impl Engine {
    pub fn new(registrations: Vec<PoolRegistration>) -> Self {
        let mut metas = Vec::with_capacity(registrations.len());
        let mut by_address = FxHashMap::default();
        let mut pool_ids_seen = FxHashSet::default();

        for reg in registrations {
            let idx = reg.meta.idx;
            // V4 addresses are synthetic (truncated poolId) — a collision with
            // a real pool address would silently cross-wire two pools' state,
            // so fail bootstrap instead.
            assert!(
                by_address.insert(reg.meta.address, idx).is_none(),
                "duplicate pool address {} (V4 synthetic-address collision?)",
                reg.meta.address
            );
            // The tag and the identity payload must agree — downstream code
            // (encoder, verify) expects `dex.is_v4() ⇔ v4.is_some()` and would
            // otherwise panic at runtime instead of at bootstrap.
            assert!(
                reg.meta.dex.is_v4() == reg.meta.v4.is_some(),
                "pool {}: DexTag/V4Meta mismatch",
                reg.meta.address
            );
            if let Some(v4) = &reg.meta.v4 {
                assert!(pool_ids_seen.insert(v4.pool_id), "duplicate V4 poolId {}", v4.pool_id);
            }
            metas.push(reg.meta);
        }

        Self {
            metas,
            by_address,
            fee_dynamic: DashSet::new(),
            fee_recheck: DashSet::new(),
            head_block: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn meta(&self, idx: PoolIdx) -> &Arc<PoolMeta> {
        &self.metas[idx.get()]
    }

    pub fn pool_addresses(&self) -> impl Iterator<Item = (Address, PoolIdx)> + '_ {
        self.by_address.iter().map(|(a, i)| (*a, *i))
    }

    pub fn len(&self) -> usize {
        self.metas.len()
    }

    pub fn set_head(&self, block: u64) {
        self.head_block.store(block, Ordering::Relaxed);
    }
    pub fn head(&self) -> u64 {
        self.head_block.load(Ordering::Relaxed)
    }
}
