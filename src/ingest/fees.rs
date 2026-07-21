//! Dynamic-fee tracker. Fees are the ONE quote input that MDBX storage reads
//! cannot always supply:
//!   - Slipstream/AeroCL re-price `fee()` on swaps with no event, and the fee
//!     lives behind a module call, not in a pool slot we've discovered;
//!   - plugin-fee Algebra pools charge the plugin's `getCurrentFee()`, which
//!     diverges from the `lastFee` packed in globalState (measured: Hydrex
//!     fee()=25 wei-exact vs lastFee=500 stale) — pools where the two agree
//!     read fee straight from the word instead and never appear here;
//!   - Aerodrome v2 (volatile/stable) fee comes from `factory.getFee`.
//!
//! On change the fee is written into `PoolMeta` (the quote path's fee source)
//! and a ChangedBatch is emitted so affected routes re-evaluate.
//!
//! Two rhythms: a ~400ms fast path over `engine.fee_recheck` (pools the
//! trigger loop just saw change — fee re-prices exactly in swap blocks), and
//! a full sweep every `sweep_interval` as the safety net.

use crate::abi::{IAeroFactory, IUniV3Pool};
use crate::engine::{ChangedBatch, DexTag, Engine, PoolIdx};
use crate::ingest::bootstrap;
use crate::ingest::multicall::{self, Call};
use crate::state::{ChainState, ClFee, SlotPlan};
use alloy::primitives::U256;
use alloy::providers::DynProvider;
use alloy::sol_types::{SolCall, SolValue};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const MC_BATCH: usize = 400;

/// Runs forever; exits when `changed_tx` closes.
pub async fn run_fee_poll_loop(
    engine: Arc<Engine>,
    state: Arc<ChainState>,
    store: Arc<crate::routing::RouteStore>,
    provider: DynProvider,
    changed_tx: mpsc::Sender<ChangedBatch>,
    sweep_interval: Duration,
) {
    // CL pools that need fee() polled: flagged dynamic_fee, minus pools whose
    // slot plan reads the fee from the globalState word (free at quote time).
    let cl_pools: Vec<PoolIdx> = engine
        .fee_dynamic
        .iter()
        .map(|e| *e)
        .filter(|idx| !matches!(state.plan(*idx), Some(SlotPlan::Cl { fee: ClFee::Word { .. }, .. })))
        .collect();
    // Aerodrome v2 pools: factory.getFee on the sweep only (fee changes are
    // rare governance/keeper actions, not per-swap).
    let aero_pools: Vec<PoolIdx> = engine
        .metas
        .iter()
        .filter(|m| matches!(m.dex, DexTag::AeroVolatile | DexTag::AeroStable))
        .map(|m| m.idx)
        .collect();

    if cl_pools.is_empty() && aero_pools.is_empty() {
        return;
    }
    tracing::info!(
        cl = cl_pools.len(),
        aero = aero_pools.len(),
        "dynamic-fee tracker active"
    );

    let fast_tick = Duration::from_millis(400);
    let mut last_sweep = Instant::now() - sweep_interval; // sweep immediately

    loop {
        tokio::time::sleep(fast_tick).await;

        let sweep = last_sweep.elapsed() >= sweep_interval;
        let cl_targets: Vec<PoolIdx> = if sweep {
            last_sweep = Instant::now();
            engine.fee_recheck.clear();
            cl_pools.clone()
        } else {
            let flagged: Vec<PoolIdx> = engine.fee_recheck.iter().map(|e| *e).collect();
            for idx in &flagged {
                engine.fee_recheck.remove(idx);
            }
            // fee_recheck may contain fee-from-word pools (the trigger loop
            // flags every fee_dynamic pool) — those need no poll.
            flagged
                .into_iter()
                .filter(|idx| {
                    !matches!(
                        state.plan(*idx),
                        Some(SlotPlan::Cl { fee: ClFee::Word { .. }, .. })
                    )
                })
                .collect()
        };

        let mut changed: Vec<PoolIdx> = Vec::new();

        if !cl_targets.is_empty() {
            let calls: Vec<Call> = cl_targets
                .iter()
                .map(|idx| Call {
                    target: engine.meta(*idx).address,
                    calldata: IUniV3Pool::feeCall {}.abi_encode().into(),
                })
                .collect();
            match multicall::aggregate3(&provider, &calls, MC_BATCH).await {
                Ok(results) => {
                    for (&idx, r) in cl_targets.iter().zip(results.iter()) {
                        if !r.success {
                            continue;
                        }
                        let Some(new_fee) =
                            bootstrap::decode_u128(&r.returnData).map(|v| v as u32)
                        else {
                            continue;
                        };
                        let meta = engine.meta(idx);
                        if meta.fee_pips() != new_fee {
                            meta.set_fee_pips(new_fee);
                            meta.set_fee_bps(new_fee / 100);
                            changed.push(idx);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "dynamic fee poll failed");
                    for idx in &cl_targets {
                        engine.fee_recheck.insert(*idx);
                    }
                }
            }
        }

        if sweep && !aero_pools.is_empty() {
            let calls: Vec<Call> = aero_pools
                .iter()
                .map(|idx| {
                    let meta = engine.meta(*idx);
                    Call {
                        target: meta.factory,
                        calldata: IAeroFactory::getFeeCall {
                            pool: meta.address,
                            stable: meta.dex == DexTag::AeroStable,
                        }
                        .abi_encode()
                        .into(),
                    }
                })
                .collect();
            match multicall::aggregate3(&provider, &calls, MC_BATCH).await {
                Ok(results) => {
                    for (&idx, r) in aero_pools.iter().zip(results.iter()) {
                        if !r.success {
                            continue;
                        }
                        let Ok(fee) = U256::abi_decode(&r.returnData) else { continue };
                        let fee_bps = fee.to::<u32>();
                        let meta = engine.meta(idx);
                        if meta.fee_bps() != fee_bps {
                            meta.set_fee_bps(fee_bps);
                            meta.set_fee_pips(fee_bps * 100);
                            changed.push(idx);
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "aero getFee sweep failed"),
            }
        }

        if !changed.is_empty() {
            tracing::info!(pools = changed.len(), "dynamic fee changed");
            // Prefetch (no memo clear -- see ChainState::prefetch_neighborhood
            // doc) BEFORE announcing, so the evaluator's re-quote of these
            // pools' routes doesn't read zero for anything not already cached.
            state.prefetch_neighborhood(&engine, &store, &changed).await;
            let batch = ChangedBatch { block: engine.head(), pools: changed, ts: Instant::now() };
            if changed_tx.send(batch).await.is_err() {
                return;
            }
        }
    }
}
