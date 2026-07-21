//! The ingest seam: WS-subscribe (Robinhood Chain has no flashblocks/preconf
//! mechanism and no local-node MDBX to read, so this is the sole ingest
//! path). The current `Engine`'s pool set is fixed at bootstrap (no runtime
//! pool-discovery growth), so this uses `Engine::by_address` directly
//! instead of a separately-maintained growable filter.

use alloy::primitives::{Address, B256};
use alloy::rpc::types::Log;
use alloy::sol_types::SolEvent;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::abi::{UniV3Events, UniV4Events};
use crate::constants::UNIV4_POOL_MANAGER;
use crate::engine::{Engine, PoolIdx};

/// Normalized events, delivered in block order. `PoolLog` carries the
/// resolved `PoolIdx` (not the raw log — the log's VALUES are never trusted
/// as a value source, `ChainState::prefetch*` always re-reads truth from
/// chain) plus, for a Mint/Burn/ModifyLiquidity log, the `tickLower`/
/// `tickUpper` it touched — enough for `ChainState::patch_cl_ticks` to
/// refresh exactly those ticks instead of the pipeline needing to guess.
#[derive(Debug, Clone)]
pub enum IngestEvent {
    NewHead { number: u64, timestamp: u64, base_fee_per_gas: u128 },
    PoolLog { block: u64, idx: PoolIdx, touched_ticks: Option<[i32; 2]> },
    /// A reconnect/gap happened; the pipeline must prefetch + re-announce
    /// every pool (see `pipeline.rs`).
    Resync,
}

/// Try to decode `log` as a liquidity-changing event (Mint/Burn — V3-family,
/// covers univ3/pancakev3/sushiswap_v3/swaphood_v3, all near-identical ABI —
/// or ModifyLiquidity — V4) and
/// return the `[tickLower, tickUpper]` it touched. `None` for anything else
/// (Swap, or a decode failure) — a Swap alone never changes tick-net data,
/// only slot0/liquidity, which the per-block memo already re-fetches fresh.
pub fn decode_touched_ticks(log: &Log) -> Option<[i32; 2]> {
    if let Ok(ev) = UniV3Events::Mint::decode_log(&log.inner) {
        return Some([ev.tickLower.as_i32(), ev.tickUpper.as_i32()]);
    }
    if let Ok(ev) = UniV3Events::Burn::decode_log(&log.inner) {
        return Some([ev.tickLower.as_i32(), ev.tickUpper.as_i32()]);
    }
    if let Ok(ev) = UniV4Events::ModifyLiquidity::decode_log(&log.inner) {
        return Some([ev.tickLower.as_i32(), ev.tickUpper.as_i32()]);
    }
    None
}

/// Address + topic membership test over the engine's (fixed-at-bootstrap)
/// pool set. V4 pools have no real address — their logs all come from the
/// PoolManager singleton, demuxed by poolId (topics[1]) via each V4 pool's
/// synthetic address (`PoolMeta::address` = first 20 bytes of poolId,
/// see `engine::pool_meta::V4Meta::synthetic_address`), which is already how
/// `Engine::by_address` indexes them — so no separate poolId map is needed.
pub struct FilterSet {
    pub engine: Arc<Engine>,
    pub topics: Vec<B256>,
}

impl FilterSet {
    pub fn new(engine: Arc<Engine>, topics: Vec<B256>) -> Self {
        Self { engine, topics }
    }

    /// Resolve a log to the `PoolIdx` it belongs to, if any of ours.
    pub fn resolve(&self, log: &Log) -> Option<PoolIdx> {
        let addr = log.address();
        if addr == UNIV4_POOL_MANAGER {
            let pool_id = log.inner.topics().get(1)?;
            let synthetic = Address::from_slice(&pool_id[0..20]);
            return self.engine.by_address.get(&synthetic).copied();
        }
        self.engine.by_address.get(&addr).copied()
    }
}

#[async_trait]
pub trait IngestBackend: Send + 'static {
    /// Run forever, pushing events. Returns on fatal error (caller may restart).
    async fn run(
        self: Box<Self>,
        filters: Arc<FilterSet>,
        tx: mpsc::Sender<IngestEvent>,
    ) -> anyhow::Result<()>;
}
