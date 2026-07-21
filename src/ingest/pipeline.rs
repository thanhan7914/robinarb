//! The block-batched ingest pipeline: consume `IngestEvent`s, batch per-block
//! dirty pools, prefetch the FULL route-neighborhood those pools touch (not
//! just the dirty pools themselves — see below), and emit `ChangedBatch`.
//!
//! There is no local RAM mirror of pool state — state lives on chain and is
//! read on demand through `ChainState`, so this pipeline's only job is
//! telling `ChainState` WHAT to prefetch and WHEN, then telling the
//! evaluator WHICH pools changed.
//!
//! IMPORTANT correctness note (why "full route-neighborhood", not just
//! "dirty pools"): `ChainState::read()` is memo-only (see `state.rs` module
//! doc) — a slot that was never prefetched this generation reads as zero.
//! `Evaluator::run` re-evaluates every ROUTE touching a dirty pool, and a
//! route's `quote()` reads EVERY pool in its hop list, not just the dirty
//! one. If we only prefetched the directly-dirty pools, any OTHER pool in
//! that same route would read as zero and the route would quote garbage.
//! So `seal()` below expands dirty pools -> affected route ids -> the full
//! set of pools those routes touch, and prefetches THAT set.

use super::backend::IngestEvent;
use crate::engine::{ChangedBatch, Engine, PoolIdx};
use crate::gas::GasStation;
use crate::routing::RouteStore;
use crate::state::ChainState;
use rustc_hash::FxHashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub struct PipelineConfig {
    pub quiet_seal_ms: u64,
}

/// Runs until the ingest channel closes.
pub async fn run_pipeline(
    engine: Arc<Engine>,
    state: Arc<ChainState>,
    store: Arc<RouteStore>,
    mut rx: mpsc::Receiver<IngestEvent>,
    changed_tx: mpsc::Sender<ChangedBatch>,
    tick_resync_tx: mpsc::Sender<PoolIdx>,
    cfg: PipelineConfig,
    gas: Arc<GasStation>,
) {
    let quiet = Duration::from_millis(cfg.quiet_seal_ms);
    let mut open_block: u64 = 0;
    let mut dirty: FxHashSet<PoolIdx> = FxHashSet::default();
    let mut have_open = false;

    loop {
        let next = if have_open {
            match tokio::time::timeout(quiet, rx.recv()).await {
                Ok(Some(ev)) => Some(ev),
                Ok(None) => break,
                Err(_) => {
                    seal(&engine, &state, &store, open_block, &mut dirty, &changed_tx).await;
                    have_open = false;
                    continue;
                }
            }
        } else {
            match rx.recv().await {
                Some(ev) => Some(ev),
                None => break,
            }
        };

        let Some(ev) = next else { break };
        match ev {
            IngestEvent::NewHead { number, base_fee_per_gas, .. } => {
                if have_open && number > open_block {
                    seal(&engine, &state, &store, open_block, &mut dirty, &changed_tx).await;
                }
                gas.on_new_head(base_fee_per_gas);
                engine.set_head(number);
                open_block = number;
                have_open = true;
            }
            IngestEvent::PoolLog { block, idx, touched_ticks } => {
                if block > open_block {
                    if have_open {
                        seal(&engine, &state, &store, open_block, &mut dirty, &changed_tx).await;
                    }
                    open_block = block;
                    have_open = true;
                }
                dirty.insert(idx);
                // Mint/Burn/ModifyLiquidity: patch the persistent tick_cache
                // for exactly the ticks this event touched (state.rs's
                // hydrate_all_ticks/patch_cl_ticks doc) BEFORE seal() below
                // prefetches this pool's base slots — a Swap-only dirty
                // pool has `touched_ticks = None` and costs nothing extra
                // here (its tick data didn't change, only slot0/liquidity,
                // which seal()'s regular prefetch already covers).
                let is_cl = matches!(state.plan(idx), Some(crate::state::SlotPlan::Cl { .. }));
                if let Some([lo, hi]) = touched_ticks {
                    if let Some(crate::state::SlotPlan::Cl { addr, ticks_base, bitmap_base, .. }) =
                        state.plan(idx)
                    {
                        let (addr, ticks_base, bitmap_base) = (*addr, *ticks_base, *bitmap_base);
                        let tick_spacing = engine.meta(idx).tick_spacing;
                        state.patch_cl_ticks(addr, ticks_base, bitmap_base, tick_spacing, &[lo, hi]).await;
                    }
                }
                // Queue this pool for a wider tick-window RE-CENTERING too —
                // not just for Mint/Burn/ModifyLiquidity but for a plain
                // Swap as well: a swap alone never changes liquidityNet at
                // any tick (so it never needs `patch_cl_ticks` above), but
                // it's the PRIMARY way a pool's current price actually
                // drifts away from wherever its hydrated window was last
                // centered (see state.rs's `run_tick_resync_loop` doc).
                // `try_send` — this must never block/slow the ingest hot
                // path; a full queue just means the resync loop is already
                // busy, drop and let the NEXT dirty event for this pool try
                // again.
                if is_cl {
                    let _ = tick_resync_tx.try_send(idx);
                }
            }
            IngestEvent::Resync => {
                tracing::warn!("ingest resync: prefetching + re-announcing every pool");
                resync_all(&engine, &state, &changed_tx, open_block.max(engine.head())).await;
            }
        }
    }

    if have_open {
        seal(&engine, &state, &store, open_block, &mut dirty, &changed_tx).await;
    }
}

/// Expand `dirty` to the full route-neighborhood, prefetch it, advance the
/// state generation, and emit a `ChangedBatch` for the directly-dirty pools
/// (routing only needs to know which pools to look up via `routes_for_pool` —
/// the wider prefetch is an implementation detail routing never sees).
async fn seal(
    engine: &Arc<Engine>,
    state: &Arc<ChainState>,
    store: &Arc<RouteStore>,
    block: u64,
    dirty: &mut FxHashSet<PoolIdx>,
    changed_tx: &mpsc::Sender<ChangedBatch>,
) {
    if dirty.is_empty() {
        return;
    }
    let pools: Vec<PoolIdx> = dirty.drain().collect();
    state.advance_and_prefetch(engine, store, block, &pools).await;

    let batch = ChangedBatch { block, pools, ts: Instant::now() };
    if changed_tx.send(batch).await.is_err() {
        tracing::debug!("changed-batch receiver dropped");
    }
}

async fn prefetch_pools(state: &Arc<ChainState>, pools: impl Iterator<Item = PoolIdx>) {
    let mut slots = Vec::new();
    for idx in pools {
        if let Some(plan) = state.plan(idx) {
            slots.extend(ChainState::plan_slots(plan));
        }
    }
    if !slots.is_empty() {
        state.prefetch(&slots).await;
    }
}

/// Full resync: prefetch every planned pool's slots and announce all of them
/// as one `ChangedBatch` so the evaluator re-checks the entire route universe.
/// NOTE: base slots only (via `prefetch_pools`, not the CL tick-window
/// prefetch `seal()` does) — doing the full tick-window pass for all ~17k
/// pools on every resync would be expensive, and resync is a rare event
/// (first connect / large reconnect gap). A CL pool quoted immediately after
/// a resync may under-quote until it next appears in a `seal()`-driven
/// dirty batch. Acceptable known gap, not silent — revisit if resync
/// frequency turns out higher than expected once live.
async fn resync_all(
    engine: &Arc<Engine>,
    state: &Arc<ChainState>,
    changed_tx: &mpsc::Sender<ChangedBatch>,
    block: u64,
) {
    let all: Vec<PoolIdx> = (0..engine.len() as u32).map(PoolIdx).collect();
    state.advance_to(block);
    prefetch_pools(state, all.iter().copied()).await;
    let batch = ChangedBatch { block, pools: all, ts: Instant::now() };
    if changed_tx.send(batch).await.is_err() {
        tracing::debug!("changed-batch receiver dropped (resync)");
    }
}
