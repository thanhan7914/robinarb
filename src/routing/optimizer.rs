//! Optimal input sizing. Pure constant-product cycles get a closed-form optimum;
//! mixed cycles (any V3/Slipstream/AeroStable hop) use ternary search over a
//! log-spaced range. Everything is verified with exact integer quotes.

use super::types::Route;
use crate::engine::Engine;
use crate::math;
use crate::state::{ChainState, SlotPlan};
use alloy::primitives::U256;

/// A sized, profitable-looking opportunity (gross; gas not yet subtracted).
#[derive(Debug, Clone)]
pub struct Opportunity {
    pub route_id: u32,
    pub amount_in: U256,
    pub gross_out: U256,
    /// gross_out - amount_in (WETH), saturating at 0.
    pub gross_profit: U256,
}

/// Exact chained quote of a route at `amount_in`. Returns final WETH out.
pub fn quote_route(engine: &Engine, state: &ChainState, route: &Route, amount_in: U256) -> Option<U256> {
    let mut amt = amount_in;
    for hop in &route.hops {
        amt = math::quote(state, engine.meta(hop.pool), amt, hop.zero_for_one);
        if amt.is_zero() {
            return None;
        }
    }
    Some(amt)
}

/// f64 chained quote (optimizer inner loop).
pub fn quote_route_f64(engine: &Engine, state: &ChainState, route: &Route, amount_in: f64) -> f64 {
    let mut amt = amount_in;
    for hop in &route.hops {
        amt = math::quote_f64(state, engine.meta(hop.pool), amt, hop.zero_for_one);
        if amt <= 0.0 {
            return 0.0;
        }
    }
    amt
}

/// Product of per-hop spot rates: the tier-1 gate value (>1 means potentially
/// profitable before slippage).
pub fn spot_product(engine: &Engine, state: &ChainState, route: &Route) -> f64 {
    let mut prod = 1.0;
    for hop in &route.hops {
        prod *= math::spot_rate(state, engine.meta(hop.pool), hop.zero_for_one);
        if prod == 0.0 {
            return 0.0;
        }
    }
    prod
}

/// Constant-product hops only (UniV2 forks + AeroVolatile — NOT AeroStable,
/// whose curve breaks the closed form). Reads reserves through ChainState.
fn is_pure_v2(engine: &Engine, state: &ChainState, route: &Route) -> Option<Vec<(f64, f64, u32)>> {
    use crate::engine::DexTag;
    let mut hops = Vec::with_capacity(route.hops.len());
    for hop in &route.hops {
        let meta = engine.meta(hop.pool);
        let (r0, r1) = match state.plan(hop.pool)? {
            SlotPlan::V2Packed { addr, slot } => {
                let (r0, r1) =
                    crate::ingest::slot_layout::decode_v2_packed(state.read(*addr, *slot));
                (r0 as f64, r1 as f64)
            }
            SlotPlan::V2TwoSlot { addr, r0 } if meta.dex == DexTag::AeroVolatile => {
                let w0 = state.read(*addr, *r0);
                let w1 = state.read(*addr, *r0 + U256::from(1u8));
                (
                    crate::math::u256_to_f64(w0),
                    crate::math::u256_to_f64(w1),
                )
            }
            _ => return None,
        };
        let (rin, rout) = if hop.zero_for_one { (r0, r1) } else { (r1, r0) };
        hops.push((rin, rout, meta.fee_bps()));
    }
    Some(hops)
}

/// Closed-form optimum for a pure-V2 chain. Composes the hops into one rational
/// function `A*x / (B + C*x)`; optimal input `x* = (sqrt(A*B) - B) / C` when A>B.
fn closed_form_optimal(hops: &[(f64, f64, u32)]) -> Option<f64> {
    let mut coef = math::v2::hop_coef(hops[0].0, hops[0].1, hops[0].2);
    for h in &hops[1..] {
        let next = math::v2::hop_coef(h.0, h.1, h.2);
        coef = math::v2::compose(coef, next);
    }
    let math::v2::HopCoef { a, b, c } = coef;
    if a <= b || c <= 0.0 {
        return None; // not profitable at any size
    }
    let x = ((a * b).sqrt() - b) / c;
    if x > 0.0 {
        Some(x)
    } else {
        None
    }
}

/// Ternary search maximizing profit(amount_in) over [lo, hi] (f64, unimodal).
fn ternary_search(engine: &Engine, state: &ChainState, route: &Route, lo: f64, hi: f64, iters: u32) -> f64 {
    let (mut lo, mut hi) = (lo, hi);
    let profit = |x: f64| quote_route_f64(engine, state, route, x) - x;
    for _ in 0..iters {
        let m1 = lo + (hi - lo) / 3.0;
        let m2 = hi - (hi - lo) / 3.0;
        if profit(m1) < profit(m2) {
            lo = m1;
        } else {
            hi = m2;
        }
    }
    (lo + hi) / 2.0
}

/// Size a route. `min_in`/`max_in` bound the search (wei). Returns an opportunity
/// if the exact quote shows positive gross profit.
pub fn optimize(engine: &Engine, state: &ChainState, route: &Route, min_in: f64, max_in: f64) -> Option<Opportunity> {
    // Pick candidate input size.
    let x_star = if let Some(hops) = is_pure_v2(engine, state, route) {
        closed_form_optimal(&hops)?.clamp(min_in, max_in)
    } else {
        // Single-probe prune before the ~60-iter ternary (each iter = a full
        // V3 tick-walk chain). Arb-cycle profit is unimodal from 0; if it's
        // already <= 0 at the smallest allowed size, slippage has eaten the
        // edge and every larger size is worse — skip the expensive search.
        if quote_route_f64(engine, state, route, min_in) - min_in <= 0.0 {
            return None;
        }
        ternary_search(engine, state, route, min_in, max_in, 60).clamp(min_in, max_in)
    };

    // Verify with exact integer quote.
    let amount_in = U256::from(x_star.max(0.0) as u128);
    if amount_in.is_zero() {
        return None;
    }
    let gross_out = quote_route(engine, state, route, amount_in)?;
    if gross_out <= amount_in {
        return None;
    }
    let gross_profit = gross_out - amount_in;
    Some(Opportunity { route_id: route.id, amount_in, gross_out, gross_profit })
}
