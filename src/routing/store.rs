//! Precomputed routes + the reverse index pool -> route ids.
//!
//! `Evaluator::run` uses `routes_for_pool` to evaluate ONLY the routes that
//! traverse a pool the current `ChangedBatch` flagged, not the whole route
//! universe — at 86k routes a full tier1+tier2 scan measured ~48ms live
//! (dominates detect→sign latency), while a batch touches only a handful of
//! pools mapping to far fewer routes.

use super::types::Route;
use crate::engine::PoolIdx;

pub struct RouteStore {
    pub routes: Vec<Route>,
    /// pool index -> route ids traversing it.
    by_pool: Vec<Vec<u32>>,
}

impl RouteStore {
    pub fn build(routes: Vec<Route>, pool_count: usize) -> Self {
        let mut by_pool: Vec<Vec<u32>> = vec![Vec::new(); pool_count];
        for route in &routes {
            for hop in &route.hops {
                let p = hop.pool.get();
                if p < by_pool.len() {
                    by_pool[p].push(route.id);
                }
            }
        }
        Self { routes, by_pool }
    }

    #[inline]
    pub fn routes_for_pool(&self, pool: PoolIdx) -> &[u32] {
        self.by_pool.get(pool.get()).map(|v| v.as_slice()).unwrap_or(&[])
    }

    #[inline]
    pub fn route(&self, id: u32) -> &Route {
        &self.routes[id as usize]
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }
}
