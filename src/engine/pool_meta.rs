//! Immutable-per-pool metadata (set at hydration; fee may update for Aerodrome).

use super::types::{DexTag, PoolIdx};
use crate::abi::V4PoolKey;
use alloy::primitives::{keccak256, Address, B256};
use alloy::sol_types::SolValue;
use std::sync::atomic::{AtomicU32, Ordering};

/// Uniswap V4 identity. The engine-wide `PoolMeta.address` for a V4 pool is
/// SYNTHETIC (first 20 bytes of the poolId — no code lives there); everything
/// that talks to the chain about a V4 pool must go through this struct. The
/// currencies here are RAW PoolKey currencies: `currency0` may be
/// `Address::ZERO` = native ETH, while `meta.token0` is normalized to WETH so
/// routing only ever sees WETH. Never rebuild a PoolKey from `meta.token0/1`.
#[derive(Debug, Clone)]
pub struct V4Meta {
    pub pool_id: B256,
    pub currency0: Address,
    pub currency1: Address,
    /// Static PoolKey fee in pips (dynamic-fee pools are dropped at discovery).
    pub fee_pips: u32,
    pub tick_spacing: i32,
}

impl V4Meta {
    /// poolId = keccak256(abi.encode(PoolKey)) with hooks = 0 (vanilla-only).
    pub fn pool_id_of(currency0: Address, currency1: Address, fee_pips: u32, tick_spacing: i32) -> B256 {
        let key = V4PoolKey {
            currency0,
            currency1,
            fee: alloy::primitives::aliases::U24::from(fee_pips),
            tickSpacing: alloy::primitives::aliases::I24::try_from(tick_spacing).expect("tick spacing fits i24"),
            hooks: Address::ZERO,
        };
        keccak256(key.abi_encode())
    }

    /// Engine-wide 20-byte identity for a V4 pool (collision odds vs the pool
    /// count are birthday-negligible; Engine asserts uniqueness at register).
    pub fn synthetic_address(pool_id: B256) -> Address {
        Address::from_slice(&pool_id[0..20])
    }
}

#[derive(Debug)]
pub struct PoolMeta {
    pub idx: PoolIdx,
    pub address: Address,
    pub dex: DexTag,
    pub factory: Address,
    pub token0: Address,
    pub token1: Address,
    pub dec0: u8,
    pub dec1: u8,
    /// V2 forks: static LP fee. Aerodrome: current factory fee. V3: fee pips / 100 (unused for math).
    pub fee_bps: AtomicU32,
    /// CL families (V3/Slipstream/Pancake/Algebra/V4): the EXACT swap fee in
    /// pips, the value quote math divides by 1e6. Static forks set it once at
    /// hydration; dynamic-fee pools (Slipstream, plugin-fee Algebra) have the
    /// fee poller keep it fresh. This is the quote-path fee source now that
    /// per-pool state snapshots are gone — `fee_bps` is V2-family only.
    pub fee_pips: AtomicU32,
    /// V3 / Slipstream / V4 only.
    pub tick_spacing: i32,
    /// Uniswap V4 only: poolId + raw PoolKey currencies.
    pub v4: Option<Box<V4Meta>>,
}

impl PoolMeta {
    #[inline]
    pub fn fee_bps(&self) -> u32 {
        self.fee_bps.load(Ordering::Relaxed)
    }
    #[inline]
    pub fn set_fee_bps(&self, v: u32) {
        self.fee_bps.store(v, Ordering::Relaxed);
    }
    #[inline]
    pub fn fee_pips(&self) -> u32 {
        self.fee_pips.load(Ordering::Relaxed)
    }
    #[inline]
    pub fn set_fee_pips(&self, v: u32) {
        self.fee_pips.store(v, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256};

    /// Anchor: real vanilla pool initialized on Base at block 48778435 —
    /// PoolKey (native ETH, 0xa66e…, fee 100, tickSpacing 1, hooks 0). The id
    /// comes from the on-chain Initialize event itself, so this pins our
    /// abi.encode(PoolKey) layout to the chain's (V4 poolId derivation is
    /// protocol-level and chain-agnostic).
    #[test]
    fn pool_id_matches_onchain_initialize() {
        let id = V4Meta::pool_id_of(
            Address::ZERO,
            address!("a66e4d253fa6b5e8b919c7d15a93598454a76dfb"),
            100,
            1,
        );
        assert_eq!(
            id,
            b256!("8509d16ca1776b6d82a6ecaf1f4cc342736abaadfcb6146c48968940a1e8bb76")
        );
    }

    #[test]
    fn synthetic_address_is_id_prefix() {
        let id = b256!("8509d16ca1776b6d82a6ecaf1f4cc342736abaadfcb6146c48968940a1e8bb76");
        assert_eq!(
            V4Meta::synthetic_address(id),
            address!("8509d16ca1776b6d82a6ecaf1f4cc342736abaad")
        );
    }
}
