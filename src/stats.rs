//! Global counters + the 30s summary loop (pattern: solarb-v2 start_stats_monitoring).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
pub struct Stats {
    pub batches_evaluated: AtomicU64,
    pub batches_dropped_stale: AtomicU64,
    pub routes_tier1: AtomicU64,
    pub routes_tier1_passed: AtomicU64,
    pub routes_tier2: AtomicU64,
    /// Opportunities dropped for being too old right before signing (the opp
    /// out-aged the pipeline — pricing state is dead, sending would just burn gas).
    pub opps_dropped_presign: AtomicU64,
    pub opportunities_found: AtomicU64,
    pub txs_sent: AtomicU64,
    pub txs_landed: AtomicU64,
    pub txs_reverted: AtomicU64,
}

impl Stats {
    pub fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }
}

pub fn spawn_stats_loop(
    stats: Arc<Stats>,
    state: Arc<crate::state::ChainState>,
    pool_count: usize,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.tick().await; // skip immediate tick
        loop {
            interval.tick().await;
            let s = &stats;
            // (reads_rpc, reads_memo) — see ChainState::read_counters. No
            // "overlay" counter here: no pending-state fast path exists on
            // this chain (no preconfirmation mechanism to layer on top of).
            let (reads_rpc, reads_memo) = state.read_counters();
            tracing::info!(
                pools = pool_count,
                reads_rpc,
                reads_memo,
                batches = s.batches_evaluated.load(Ordering::Relaxed),
                stale = s.batches_dropped_stale.load(Ordering::Relaxed),
                t1 = s.routes_tier1.load(Ordering::Relaxed),
                t1_pass = s.routes_tier1_passed.load(Ordering::Relaxed),
                t2 = s.routes_tier2.load(Ordering::Relaxed),
                stale_presign = s.opps_dropped_presign.load(Ordering::Relaxed),
                opps = s.opportunities_found.load(Ordering::Relaxed),
                sent = s.txs_sent.load(Ordering::Relaxed),
                landed = s.txs_landed.load(Ordering::Relaxed),
                reverted = s.txs_reverted.load(Ordering::Relaxed),
                "stats"
            );
        }
    });
}
