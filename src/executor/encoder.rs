//! Encode an evaluated opportunity into `ArbExecutor.executeArb` calldata.

use crate::abi::IArbExecutor;
use crate::engine::{DexTag, Engine};
use crate::routing::{EvaluatedOpportunity, RouteStore};
use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolCall;

/// Hop kind byte, matching ArbExecutor.sol. Keyed by protocol tag, not the
/// state snapshot: a V4 pool shares `PoolData::V3` with the V3 family but must
/// never encode as kind 2 (its synthetic address has no code to `swap()` on).
fn hop_kind(dex: DexTag) -> u8 {
    match dex {
        DexTag::UniV2Fork | DexTag::AeroVolatile => 0,
        DexTag::AeroStable => 1,
        DexTag::UniV3Fork | DexTag::PancakeV3 | DexTag::Slipstream => 2,
        DexTag::UniV4 => 3,
        // Algebra: UniV3 swap shape but `algebraSwapCallback` — a distinct
        // callback selector the executor must answer, hence its own kind.
        // Requires the ArbExecutor upgrade (P3) before any Algebra route is live.
        DexTag::Algebra => 4,
    }
}

/// Build calldata + the decision to use flashloan. `own_balance` is the
/// executor's current balance in the route's own anchor token (`route.anchor`
/// — WETH, USDC, or cbBTC); if the sized input exceeds it, flashloan.
pub fn encode(
    engine: &Engine,
    store: &RouteStore,
    eval: &EvaluatedOpportunity,
    own_balance: U256,
    force_flashloan: bool,
) -> (Bytes, bool) {
    let route = store.route(eval.opp.route_id);
    let anchor: Address = route.anchor;
    let mut hops = Vec::with_capacity(route.hops.len());
    let mut v4_keys: Vec<IArbExecutor::V4Key> = Vec::new();
    for h in &route.hops {
        let meta = engine.meta(h.pool);
        let kind = hop_kind(meta.dex);
        let (pool, fee_bps) = if kind == 3 {
            // V4: the pool has no address; feeBps carries the index of this
            // hop's PoolKey in the v4Keys array. Currencies come RAW from
            // V4Meta (0x0 = native ETH) — never from the normalized meta
            // tokens, which would address a different pool.
            let v4 = meta.v4.as_ref().expect("UniV4 meta has v4");
            v4_keys.push(IArbExecutor::V4Key {
                currency0: v4.currency0,
                currency1: v4.currency1,
                fee: alloy::primitives::aliases::U24::from(v4.fee_pips),
                tickSpacing: alloy::primitives::aliases::I24::try_from(v4.tick_spacing)
                    .expect("tick spacing fits i24"),
            });
            (alloy::primitives::Address::ZERO, (v4_keys.len() - 1) as u16)
        } else {
            (meta.address, meta.fee_bps() as u16)
        };
        hops.push(IArbExecutor::Hop {
            kind,
            pool,
            zeroForOne: h.zero_for_one,
            feeBps: fee_bps,
        });
    }

    let amount_in = eval.opp.amount_in;
    let use_flashloan = force_flashloan || amount_in > own_balance;

    // minProfit = 0: the contract's delta check still refuses any fill that
    // loses principal (end >= start [+ loan fee]), and gas is sunk once the
    // tx is included — reverting saves nothing. A decayed fill executed at
    // ~0 also consumes the dislocation itself, so the route stops
    // re-firing without needing the revert→blacklist loop. Revisit against
    // replay data if realized P&L on marginal fills turns out negative.
    let min_profit = U256::ZERO;

    let call = IArbExecutor::executeArbCall {
        hops,
        v4Keys: v4_keys,
        anchor,
        amountIn: amount_in,
        minProfit: min_profit,
        useFlashloan: use_flashloan,
    };
    (call.abi_encode().into(), use_flashloan)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tag must keep its historical kind byte (the contract dispatches
    /// on it); V4 gets the new kind 3.
    #[test]
    fn hop_kind_per_dex_tag() {
        assert_eq!(hop_kind(DexTag::UniV2Fork), 0);
        assert_eq!(hop_kind(DexTag::AeroVolatile), 0);
        assert_eq!(hop_kind(DexTag::AeroStable), 1);
        assert_eq!(hop_kind(DexTag::UniV3Fork), 2);
        assert_eq!(hop_kind(DexTag::PancakeV3), 2);
        assert_eq!(hop_kind(DexTag::Slipstream), 2);
        assert_eq!(hop_kind(DexTag::UniV4), 3);
    }
}
