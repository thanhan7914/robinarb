//! Offline anchor-rooted cycle enumeration. Builds a token graph (nodes =
//! tokens, parallel edges = pools) and DFS-enumerates 2- and 3-hop cycles
//! anchor->..->anchor (one enumeration per configured anchor token) with no
//! repeated pool or intermediate token.

use super::types::{Hop, Route};
use crate::engine::{Engine, PoolIdx};
use crate::state::{ChainState, SlotPlan};
use alloy::primitives::Address;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

/// An edge = a pool connecting two tokens, with a liquidity proxy for fan-out ranking.
struct Edge {
    pool: PoolIdx,
    token0: Address,
    token1: Address,
    liq: f64,
}

pub struct GraphConfig {
    pub max_hops: usize,
    pub max_routes: usize,
    pub pair_fanout: usize,
}

/// Build all routes from the engine's current pool set, once per anchor
/// token. `max_routes` in `cfg` bounds EACH anchor's enumeration separately
/// (not the combined total) — a hot anchor shouldn't starve the others.
pub fn build_routes(engine: &Engine, state: &ChainState, cfg: &GraphConfig, anchors: &[Address]) -> Vec<Route> {
    // Adjacency: token -> list of edges.
    let mut adj: FxHashMap<Address, Vec<Edge>> = FxHashMap::default();
    for (_, idx) in engine.pool_addresses() {
        let meta = engine.meta(idx);
        let liq = pool_liq_proxy(engine, state, idx);
        let e0 = Edge { pool: idx, token0: meta.token0, token1: meta.token1, liq };
        let e1 = Edge { pool: idx, token0: meta.token0, token1: meta.token1, liq };
        adj.entry(meta.token0).or_default().push(e0);
        adj.entry(meta.token1).or_default().push(e1);
    }

    // Cap fan-out per (token, neighbor) pair by liquidity.
    for edges in adj.values_mut() {
        // Group by neighbor token isn't trivial without the "from" side; instead
        // sort all edges by liquidity desc and keep a generous cap per node.
        edges.sort_by(|a, b| b.liq.partial_cmp(&a.liq).unwrap_or(std::cmp::Ordering::Equal));
        let cap = cfg.pair_fanout * 8;
        if edges.len() > cap {
            edges.truncate(cap);
        }
    }

    let mut routes: Vec<Route> = Vec::new();
    for &anchor in anchors {
        let mut next_id: u32 = 0;
        let mut anchor_routes: Vec<Route> = Vec::new();

        // DFS state.
        let mut path: Vec<Hop> = Vec::new();
        let mut used_pools: SmallVec<[PoolIdx; 4]> = SmallVec::new();
        let mut visited_tokens: SmallVec<[Address; 4]> = SmallVec::new();
        visited_tokens.push(anchor);

        dfs(
            &adj,
            anchor,
            anchor,
            cfg.max_hops,
            &mut path,
            &mut used_pools,
            &mut visited_tokens,
            &mut anchor_routes,
            &mut next_id,
            cfg.max_routes,
        );

        tracing::info!(anchor = %anchor, routes = anchor_routes.len(), "cycle enumeration complete for anchor");
        routes.extend(anchor_routes);
    }

    // Renumber to a single flat, unique id space across all anchors (each
    // anchor's DFS above restarts `next_id` from 0).
    for (i, r) in routes.iter_mut().enumerate() {
        r.id = i as u32;
    }

    tracing::info!(routes = routes.len(), anchors = anchors.len(), "cycle enumeration complete (all anchors)");
    routes
}

#[allow(clippy::too_many_arguments)]
fn dfs(
    adj: &FxHashMap<Address, Vec<Edge>>,
    start: Address,
    current: Address,
    max_hops: usize,
    path: &mut Vec<Hop>,
    used_pools: &mut SmallVec<[PoolIdx; 4]>,
    visited_tokens: &mut SmallVec<[Address; 4]>,
    routes: &mut Vec<Route>,
    next_id: &mut u32,
    max_routes: usize,
) {
    if routes.len() >= max_routes {
        return;
    }
    let depth = path.len();
    if depth >= max_hops {
        return;
    }
    let Some(edges) = adj.get(&current) else { return };

    for edge in edges {
        if used_pools.contains(&edge.pool) {
            continue;
        }
        let next_token = if edge.token0 == current {
            edge.token1
        } else if edge.token1 == current {
            edge.token0
        } else {
            continue;
        };

        let closes = next_token == start;
        let hop = Hop {
            pool: edge.pool,
            zero_for_one: edge.token0 == current,
            token_in: current,
        };

        if closes {
            // Need at least 2 hops for a real cycle.
            if depth + 1 >= 2 {
                path.push(hop);
                routes.push(Route { id: *next_id, anchor: start, hops: SmallVec::from_slice(path) });
                *next_id += 1;
                path.pop();
                if routes.len() >= max_routes {
                    return;
                }
            }
            continue;
        }

        // Don't revisit an intermediate token.
        if visited_tokens.contains(&next_token) {
            continue;
        }

        path.push(hop);
        used_pools.push(edge.pool);
        visited_tokens.push(next_token);

        dfs(adj, start, next_token, max_hops, path, used_pools, visited_tokens, routes, next_id, max_routes);

        visited_tokens.pop();
        used_pools.pop();
        path.pop();
    }
}

/// A cheap liquidity proxy for fan-out ranking (one-shot bootstrap reads).
fn pool_liq_proxy(engine: &Engine, state: &ChainState, idx: PoolIdx) -> f64 {
    use crate::math::u256_to_f64;
    use crate::ingest::slot_layout::{decode_shifted128, decode_v2_packed};
    let _ = engine;
    match state.plan(idx) {
        Some(SlotPlan::V2Packed { addr, slot }) => {
            let (r0, r1) = decode_v2_packed(state.read(*addr, *slot));
            (r0 as f64) * (r1 as f64)
        }
        Some(SlotPlan::V2TwoSlot { addr, r0 }) => {
            let w0 = state.read(*addr, *r0);
            let w1 = state.read(*addr, *r0 + alloy::primitives::U256::from(1u8));
            u256_to_f64(w0) * u256_to_f64(w1)
        }
        Some(SlotPlan::Cl { addr, liquidity, liquidity_shift, .. }) => {
            decode_shifted128(state.read(*addr, *liquidity), *liquidity_shift) as f64
        }
        None => 0.0,
    }
}
