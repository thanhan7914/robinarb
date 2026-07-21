//! Trigger flow: consume `ChangedBatch`, drop stale batches, map its changed
//! pools through the reverse index to the routes that touch them, tier-1 rank
//! those (rayon), tier-2 exact optimize, and apply the net-profit gate. Emits
//! opportunities to the executor/paper sink.
//!
//! Only the routes traversing a changed pool are re-evaluated (not all ~86k):
//! a full-universe scan measured ~48ms live (dominated detect→sign latency),
//! while a batch typically flags a handful of pools mapping to far fewer
//! routes, keeping the hot path to a few ms.

use super::optimizer::{self, Opportunity};
use super::store::RouteStore;
use crate::config::{AnchorConfig, RoutingConfig};
use crate::engine::{ChangedBatch, Engine, PoolIdx};
use crate::state::ChainState;
use crate::gas::{route_gas_units, GasStation, HopGasKind};
use crate::stats::Stats;
use alloy::primitives::{Address, U256};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// A fully-vetted opportunity with net profit after gas.
#[derive(Debug, Clone)]
pub struct EvaluatedOpportunity {
    pub opp: Opportunity,
    /// Net profit in the OPPORTUNITY'S OWN ANCHOR's smallest unit (wei for
    /// WETH, but 6-decimal units for USDC, 8-decimal for cbBTC — see
    /// `opp.route_id` -> `Route.anchor`). Naming kept for continuity with
    /// `Opportunity.gross_profit`, which was already anchor-native.
    pub net_profit_wei: u128,
    pub gas_units: u64,
    /// Always true ETH-wei (what the tx actually costs to execute) —
    /// NOT anchor-converted, unlike `net_profit_wei`.
    pub gas_cost_wei: u128,
    pub priority_fee_wei: u128,
    pub block: u64,
    /// Seal instant of the ChangedBatch that surfaced this opp — anchor for
    /// the detect→wire latency the sender measures.
    pub detected_at: Instant,
}

pub struct Evaluator {
    pub engine: Arc<Engine>,
    pub state: Arc<ChainState>,
    pub store: Arc<RouteStore>,
    pub gas: Arc<GasStation>,
    pub cfg: RoutingConfig,
    /// Per-anchor thresholds (min/max trade size, min profit) — routes carry
    /// which anchor they belong to (`Route.anchor`), looked up here.
    pub anchors: Vec<AnchorConfig>,
    /// WETH-per-anchor exchange rates from `pricing.rs`, used to convert the
    /// always-ETH-denominated gas cost into a non-WETH anchor's own units.
    pub prices: Arc<FxHashMap<Address, f64>>,
    pub stats: Arc<Stats>,
}

impl Evaluator {
    pub async fn run(
        self,
        mut rx: mpsc::Receiver<ChangedBatch>,
        out: mpsc::Sender<EvaluatedOpportunity>,
    ) {
        let drop_dur = Duration::from_millis(self.cfg.drop_ms);
        // Small (3-entry) lookup, built once — cheaper and clearer than a
        // linear `.find()` per route inside the hot rayon loops below.
        let anchor_by_token: FxHashMap<Address, AnchorConfig> =
            self.anchors.iter().map(|a| (a.token, a.clone())).collect();

        while let Some(mut batch) = rx.recv().await {
            // Merge any queued batches into one pass (this coalescing is what
            // clears a backlog). Keep the OLDEST ts as the staleness anchor:
            // if we've fallen behind, the merged batch reads old and gets
            // dropped, shedding dead work instead of pricing on stale state.
            // UNION the changed-pool sets so the reverse-index lookup below
            // covers every pool any coalesced batch flagged.
            while let Ok(next) = rx.try_recv() {
                batch.block = next.block.max(batch.block);
                batch.ts = batch.ts.min(next.ts);
                batch.pools.extend(next.pools);
            }

            if batch.ts.elapsed() > drop_dur {
                tracing::debug!(
                    block = batch.block,
                    elapsed_ms = batch.ts.elapsed().as_millis() as u64,
                    "dropping stale batch"
                );
                Stats::bump(&self.stats.batches_dropped_stale);
                continue;
            }

            // Candidate routes = those traversing a changed pool (reverse
            // index), deduped. Evaluating only these instead of all ~86k
            // routes is what keeps detect→sign under a few ms.
            let mut candidate_ids: Vec<u32> = batch
                .pools
                .iter()
                .flat_map(|p| self.store.routes_for_pool(*p).iter().copied())
                .collect();
            candidate_ids.sort_unstable();
            candidate_ids.dedup();
            if candidate_ids.is_empty() {
                continue;
            }
            Stats::bump(&self.stats.batches_evaluated);
            Stats::add(&self.stats.routes_tier1, candidate_ids.len() as u64);

            // Tier 1: parallel spot-product gate over the candidate routes.
            let gate = 1.0 + self.cfg.spot_gate_bps / 10_000.0;
            let mut passed: Vec<(u32, f64)> = candidate_ids
                .par_iter()
                .filter_map(|&id| {
                    let route = self.store.route(id);
                    let prod = optimizer::spot_product(&self.engine, &self.state, route);
                    if prod > gate {
                        Some((id, prod))
                    } else {
                        None
                    }
                })
                .collect();
            if passed.is_empty() {
                continue;
            }
            Stats::add(&self.stats.routes_tier1_passed, passed.len() as u64);

            // Rank by spot product, take top-N for exact evaluation.
            passed.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            passed.truncate(self.cfg.tier2_top_n);

            // Post-tier1 staleness gate: exact evaluation is the expensive part,
            // so re-check age before paying for it (freshest guard also protects
            // against a slow tier-1 on a huge fan-out).
            if batch.ts.elapsed() > drop_dur {
                Stats::bump(&self.stats.batches_dropped_stale);
                continue;
            }

            // Tier 2: exact optimize + net-profit gate, PARALLEL across routes.
            // Each route's tick-walk optimize is independent; side effects
            // (needs_refresh, stats) are on lock-free structures.
            let mut candidates: Vec<EvaluatedOpportunity> = passed
                .par_iter()
                .filter_map(|(id, _)| {
                    let route = self.store.route(*id);
                    // Route was enumerated from a configured anchor (graph.rs)
                    // — the lookup can only miss on a config change mid-run
                    // (route store rebuild is a full restart), so this is a
                    // hard invariant, not a runtime condition to soften.
                    let anchor = anchor_by_token
                        .get(&route.anchor)
                        .expect("route.anchor must match a configured [[anchor]]");
                    let min_in = anchor.min_trade * 10f64.powi(anchor.decimals as i32);
                    let max_in = anchor.max_trade * 10f64.powi(anchor.decimals as i32);
                    Stats::bump(&self.stats.routes_tier2);
                    let opp = optimizer::optimize(&self.engine, &self.state, route, min_in, max_in)?;
                    self.apply_gas_gate(opp, batch.block, anchor, batch.ts)
                })
                .collect();

            if candidates.is_empty() {
                continue;
            }
            // Best first; emit disjoint-pool opportunities. `net_profit_wei`
            // is anchor-native (18 decimals for WETH, 6 for USDC, 8 for
            // cbBTC) — raw cross-anchor comparison would always rank WETH
            // candidates above economically-smaller-magnitude USDC/cbBTC
            // ones regardless of real profit, so rank by ETH-wei equivalent
            // instead (same conversion the gas gate already uses).
            let rank_key = |c: &EvaluatedOpportunity| -> u128 {
                let anchor = anchor_by_token
                    .get(&self.store.route(c.opp.route_id).anchor)
                    .expect("route.anchor must match a configured [[anchor]]");
                self.anchor_to_weth_wei(anchor, c.net_profit_wei)
            };
            candidates.sort_by(|a, b| rank_key(b).cmp(&rank_key(a)));
            let mut used: FxHashSet<PoolIdx> = FxHashSet::default();
            for eval in candidates {
                let route = self.store.route(eval.opp.route_id);
                if route.hops.iter().any(|h| used.contains(&h.pool)) {
                    continue;
                }
                for h in &route.hops {
                    used.insert(h.pool);
                }
                Stats::bump(&self.stats.opportunities_found);
                if out.send(eval).await.is_err() {
                    return;
                }
            }
        }
    }

    fn apply_gas_gate(
        &self,
        opp: Opportunity,
        block: u64,
        anchor: &AnchorConfig,
        detected_at: Instant,
    ) -> Option<EvaluatedOpportunity> {
        let route = self.store.route(opp.route_id);
        let kinds: Vec<HopGasKind> = route
            .hops
            .iter()
            .map(|h| {
                let dex = self.engine.meta(h.pool).dex;
                if dex.is_v4() {
                    HopGasKind::V4
                } else if dex.is_v3_family() || dex.is_algebra() {
                    HopGasKind::V3
                } else if dex == crate::engine::DexTag::AeroStable {
                    HopGasKind::AeroStable
                } else {
                    HopGasKind::V2
                }
            })
            .collect();
        let gas_units = route_gas_units(&kinds, true);

        // Estimate calldata length for the L1 fee (selector + hops array +
        // one V4Key per V4 hop: 4 words plus array offset/length overhead).
        let n_v4 = kinds.iter().filter(|k| matches!(k, HopGasKind::V4)).count();
        let calldata_len = 200 + route.hops.len() * 160 + 64 + n_v4 * 128;

        // gas.priority_fee/total_gas_cost always operate in ETH-wei (base
        // fee, L1 data fee — none of that is anchor-denominated), so `gross`
        // must be converted to its ETH-wei equivalent for the tip-sizing
        // call even though the route's own profit is anchor-native.
        let gross = u128_from(opp.gross_profit);
        let gross_weth_wei = self.anchor_to_weth_wei(anchor, gross);
        let priority_fee = self.gas.priority_fee(gross_weth_wei, gas_units);
        let gas_cost_wei = self.gas.total_gas_cost(gas_units, priority_fee, calldata_len);
        // Convert back to the anchor's own units for the actual profit
        // gate — reduces to the identity for the WETH anchor (rate=1.0,
        // decimals=18), so no special-casing needed here.
        let gas_cost = self.weth_wei_to_anchor(anchor, gas_cost_wei);

        if gross <= gas_cost {
            return None;
        }
        let net = gross - gas_cost;
        if U256::from(net) < anchor.min_profit() {
            return None;
        }
        Some(EvaluatedOpportunity {
            opp,
            net_profit_wei: net,
            gas_units,
            gas_cost_wei,
            priority_fee_wei: priority_fee,
            block,
            detected_at,
        })
    }

    /// `amount` in `anchor`'s smallest unit -> ETH-wei, via the
    /// bootstrap-fetched WETH-per-anchor rate (`pricing.rs`). Identity for
    /// the WETH anchor (rate=1.0, decimals=18).
    fn anchor_to_weth_wei(&self, anchor: &AnchorConfig, amount: u128) -> u128 {
        let rate = *self
            .prices
            .get(&anchor.token)
            .expect("anchor rate must be fetched at bootstrap (pricing::fetch_anchor_rates)");
        (amount as f64 / 10f64.powi(anchor.decimals as i32) * rate * 1e18) as u128
    }

    /// Inverse of `anchor_to_weth_wei`.
    fn weth_wei_to_anchor(&self, anchor: &AnchorConfig, wei: u128) -> u128 {
        let rate = *self
            .prices
            .get(&anchor.token)
            .expect("anchor rate must be fetched at bootstrap (pricing::fetch_anchor_rates)");
        (wei as f64 / 1e18 / rate * 10f64.powi(anchor.decimals as i32)) as u128
    }
}

fn u128_from(v: U256) -> u128 {
    if v > U256::from(u128::MAX) {
        u128::MAX
    } else {
        v.to::<u128>()
    }
}
