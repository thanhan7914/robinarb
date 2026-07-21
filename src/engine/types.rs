//! Interned pool identity + protocol tag.

use serde::{Deserialize, Serialize};

/// Interned pool index — used everywhere on the hot path instead of the 20-byte address.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct PoolIdx(pub u32);

impl PoolIdx {
    #[inline]
    pub fn get(self) -> usize {
        self.0 as usize
    }
}

/// Protocol family. Determines decoding, hydration, and quote math.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum DexTag {
    /// Uniswap V2 and forks with a static per-factory fee (Sushi, BaseSwap, ...).
    UniV2Fork,
    /// Aerodrome volatile pool (x*y=k, dynamic fee from factory).
    AeroVolatile,
    /// Aerodrome stable pool (x^3 y + x y^3 = k).
    AeroStable,
    /// Uniswap V3 and forks sharing the standard Swap topic.
    UniV3Fork,
    /// Pancake V3 (distinct Swap topic + callback name).
    PancakeV3,
    /// Aerodrome Slipstream (concentrated liquidity, tickSpacing-keyed).
    Slipstream,
    /// Uniswap V4 vanilla pools (no hooks, static fee). Live inside the
    /// singleton PoolManager: no per-pool contract, identity is a bytes32
    /// poolId; our 20-byte `address` is synthetic (first 20 bytes of the id).
    UniV4,
    /// Algebra Integral (Hydrex, QuickSwap v4). Concentrated-liquidity with
    /// UniV3-identical tick-walk math (reuses `PoolData::V3`), but a different
    /// state reader (`globalState()` not `slot0()`), Swap/Burn event layout, and
    /// swap callback (`algebraSwapCallback`). Fee is dynamic, carried inline by
    /// the Swap event.
    Algebra,
}

impl DexTag {
    /// V3-family pools use tick math and the tick table. Deliberately FALSE
    /// for UniV4: this predicate gates per-pool-address eth_calls
    /// (slot0/liquidity/tickBitmap) that a V4 synthetic address cannot serve —
    /// V4 state is read through StateView by poolId.
    #[inline]
    pub fn is_v3_family(self) -> bool {
        matches!(self, DexTag::UniV3Fork | DexTag::PancakeV3 | DexTag::Slipstream)
    }

    /// Uniswap V4 (singleton PoolManager, poolId-keyed).
    #[inline]
    pub fn is_v4(self) -> bool {
        matches!(self, DexTag::UniV4)
    }

    /// Algebra Integral (Hydrex, QuickSwap v4).
    #[inline]
    pub fn is_algebra(self) -> bool {
        matches!(self, DexTag::Algebra)
    }

    /// Pools that quote with the constant-product formula.
    #[inline]
    pub fn is_v2_family(self) -> bool {
        matches!(self, DexTag::UniV2Fork | DexTag::AeroVolatile)
    }

}
