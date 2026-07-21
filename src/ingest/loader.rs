//! Load a fixed set of pools from `pools.toml` (solarb-v2 style) instead of
//! scanning factories. Used for fast testing and for pinning specific pools.
//!
//! Classification is AUTHORITATIVE via the pool's on-chain `factory()`, matched
//! against the configured [[dex]] factories — the `dex` field in pools.toml is
//! ignored. Pools from unknown factories (e.g. Slipstream forks, or a version
//! DEX Screener mislabels) are dropped, because we can't guarantee their event
//! format / quote math. This feeds the same `bootstrap::hydrate` path.
//!
//! Uniswap V4 pools have no address (they live in the singleton PoolManager),
//! so their pins carry a `pool_id` (resolved against the V4 discovery cache)
//! or a full PoolKey. There the id itself is the identity — keccak256 of the
//! key — so nothing needs trusting; the on-chain truth probe is liveness:
//! `StateView.getSlot0(poolId).sqrtPriceX96 != 0`.

use crate::abi::{IAeroPool, IStateView, IUniV2Pair, IUniV3Pool};
use crate::config::{Config, DexConfig, PoolPin};
use crate::constants::UNIV4_STATE_VIEW;
use crate::engine::pool_meta::V4Meta;
use crate::engine::DexTag;
use crate::ingest::discovery::{self, DiscoveredPool};
use crate::ingest::multicall::{self, Call};
use alloy::primitives::{Address, B256};
use alloy::providers::DynProvider;
use alloy::sol_types::SolCall;
use anyhow::Result;
use rustc_hash::FxHashMap;

const MC_BATCH: usize = 200;
const CALLS_PER_PIN: usize = 6;

/// Resolve `pins` into `DiscoveredPool`s: address pins classified by their
/// on-chain factory, V4 pins (no address) by poolId/PoolKey + liveness probe.
pub async fn load_pinned_pools(
    provider: &DynProvider,
    cfg: &Config,
    pins: &[PoolPin],
) -> Result<Vec<DiscoveredPool>> {
    // Reject ambiguous pins up front: an address plus any V4 identity field
    // means the operator's intent is unclear (a V4 synthetic address is not a
    // valid `address` pin — it has no code to probe).
    for p in pins {
        anyhow::ensure!(
            !(p.address.is_some()
                && (p.pool_id.is_some() || p.currency0.is_some() || p.currency1.is_some())),
            "pools.toml pin for dex '{}' mixes `address` with V4 identity fields",
            p.dex
        );
    }
    let (addr_pins, v4_pins): (Vec<&PoolPin>, Vec<&PoolPin>) =
        pins.iter().partition(|p| p.address.is_some());
    let mut out = load_address_pins(provider, cfg, &addr_pins).await?;
    out.extend(load_v4_pins(provider, cfg, &v4_pins).await?);
    Ok(out)
}

async fn load_address_pins(
    provider: &DynProvider,
    cfg: &Config,
    pins: &[&PoolPin],
) -> Result<Vec<DiscoveredPool>> {
    if pins.is_empty() {
        return Ok(vec![]);
    }
    // Map lowercased factory address -> configured dex.
    let mut by_factory: FxHashMap<Address, &DexConfig> = FxHashMap::default();
    for d in &cfg.dexes {
        by_factory.insert(d.factory, d);
    }

    // Six probe calls per pin: token0, token1, factory, tickSpacing, fee, stable.
    let mut calls: Vec<Call> = Vec::with_capacity(pins.len() * CALLS_PER_PIN);
    for pin in pins {
        let a = pin.address.expect("partitioned on address.is_some()");
        calls.push(Call { target: a, calldata: IUniV2Pair::token0Call {}.abi_encode().into() });
        calls.push(Call { target: a, calldata: IUniV2Pair::token1Call {}.abi_encode().into() });
        calls.push(Call { target: a, calldata: factory_calldata() });
        calls.push(Call { target: a, calldata: IUniV3Pool::tickSpacingCall {}.abi_encode().into() });
        calls.push(Call { target: a, calldata: IUniV3Pool::feeCall {}.abi_encode().into() });
        calls.push(Call { target: a, calldata: IAeroPool::stableCall {}.abi_encode().into() });
    }
    let res = multicall::aggregate3(provider, &calls, MC_BATCH).await?;

    let mut out = Vec::with_capacity(pins.len());
    let mut dropped = 0usize;
    for (i, pin) in pins.iter().enumerate() {
        let address = pin.address.expect("partitioned on address.is_some()");
        let base = i * CALLS_PER_PIN;
        let token0 = res.get(base).filter(|r| r.success).and_then(|r| decode_addr(&r.returnData));
        let token1 = res.get(base + 1).filter(|r| r.success).and_then(|r| decode_addr(&r.returnData));
        let factory = res.get(base + 2).filter(|r| r.success).and_then(|r| decode_addr(&r.returnData));
        let tick_spacing = res
            .get(base + 3)
            .filter(|r| r.success)
            .and_then(|r| IUniV3Pool::tickSpacingCall::abi_decode_returns(&r.returnData).ok())
            .map(|v| v.as_i32());
        let fee_pips = res
            .get(base + 4)
            .filter(|r| r.success)
            .and_then(|r| IUniV3Pool::feeCall::abi_decode_returns(&r.returnData).ok())
            .map(|v| v.to::<u32>());
        let stable = res
            .get(base + 5)
            .filter(|r| r.success)
            .and_then(|r| IAeroPool::stableCall::abi_decode_returns(&r.returnData).ok());

        let (Some(token0), Some(token1)) = (token0, token1) else {
            tracing::warn!(pool = %address, "no token0/token1; dropping");
            dropped += 1;
            continue;
        };

        // Authoritative: match the pool's factory to a configured dex.
        let Some(factory) = factory else {
            tracing::warn!(pool = %address, "no factory(); dropping");
            dropped += 1;
            continue;
        };
        let Some(dex_cfg) = by_factory.get(&factory).copied() else {
            tracing::warn!(pool = %address, %factory, "unknown factory; dropping");
            dropped += 1;
            continue;
        };

        let dp = match dex_cfg.kind.as_str() {
            "v2" => DiscoveredPool {
                address,
                dex: DexTag::UniV2Fork,
                factory,
                token0,
                token1,
                fee_pips: None,
                tick_spacing: None,
                fee_bps: dex_cfg.fee_bps,
                pool_id: None,
                currency0_raw: None,
            },
            "aero" => {
                let is_stable = stable.unwrap_or(false);
                DiscoveredPool {
                    address,
                    dex: if is_stable { DexTag::AeroStable } else { DexTag::AeroVolatile },
                    factory,
                    token0,
                    token1,
                    fee_pips: None,
                    tick_spacing: None,
                    fee_bps: None, // read from factory.getFee at hydration
                    pool_id: None,
                    currency0_raw: None,
                }
            }
            "v3" | "pancake_v3" | "slipstream" => {
                let tag = match dex_cfg.kind.as_str() {
                    "pancake_v3" => DexTag::PancakeV3,
                    "slipstream" => DexTag::Slipstream,
                    _ => DexTag::UniV3Fork,
                };
                DiscoveredPool {
                    address,
                    dex: tag,
                    factory,
                    token0,
                    token1,
                    fee_pips,
                    tick_spacing,
                    fee_bps: None,
                    pool_id: None,
                    currency0_raw: None,
                }
            }
            "algebra" => DiscoveredPool {
                address,
                dex: DexTag::Algebra,
                factory,
                token0,
                token1,
                // Fee is dynamic — hydration reads fee() fresh, then the Swap
                // decoder keeps it current from overrideFee.
                fee_pips: None,
                tick_spacing,
                fee_bps: None,
                pool_id: None,
                currency0_raw: None,
            },
            other => {
                tracing::warn!(pool = %address, kind = other, "unsupported kind; dropping");
                dropped += 1;
                continue;
            }
        };
        out.push(dp);
    }

    tracing::info!(loaded = out.len(), dropped, "loaded pools from pools.toml (classified by factory)");
    Ok(out)
}

/// V4 pins: derive/resolve the poolId, apply the vanilla-only policy, then
/// batch-probe liveness through StateView.
async fn load_v4_pins(
    provider: &DynProvider,
    cfg: &Config,
    pins: &[&PoolPin],
) -> Result<Vec<DiscoveredPool>> {
    if pins.is_empty() {
        return Ok(vec![]);
    }
    let mut caches: FxHashMap<String, FxHashMap<B256, DiscoveredPool>> = FxHashMap::default();
    let mut candidates: Vec<DiscoveredPool> = Vec::new();
    let mut dropped = 0usize;

    for pin in pins {
        let Some(dex_cfg) = cfg.dexes.iter().find(|d| d.name == pin.dex) else {
            tracing::warn!(dex = %pin.dex, "pin references unknown [[dex]]; dropping");
            dropped += 1;
            continue;
        };
        // A non-v4 pin without an address is a malformed config, not a pool to
        // skip — before `address` became optional this was a hard parse error,
        // and silently dropping it would shrink the live arb surface unnoticed.
        anyhow::ensure!(
            dex_cfg.kind == "v4",
            "pools.toml pin for dex '{}' is missing `address`",
            pin.dex
        );

        let dp = if let (Some(c0), Some(c1), Some(fee), Some(ts)) =
            (pin.currency0, pin.currency1, pin.fee, pin.tick_spacing)
        {
            // Config values feed keccak/PoolKey building — validate the i24/
            // u24 ranges here so a typo'd pin drops instead of panicking.
            if !(-8_388_608..=8_388_607).contains(&ts)
                || fee > crate::constants::UNIV4_MAX_LP_FEE
            {
                tracing::warn!(dex = %pin.dex, fee, tick_spacing = ts, "v4 pin out of range; dropping");
                dropped += 1;
                continue;
            }
            // Full-key pin: the id is derived locally — the key IS the identity.
            let id = V4Meta::pool_id_of(c0, c1, fee, ts);
            match discovery::v4_pool_from_key(dex_cfg.factory, id, c0, c1, fee, ts, Address::ZERO) {
                Some(dp) => dp,
                None => {
                    tracing::warn!(pool_id = %id, "v4 pin refused by vanilla-only policy; dropping");
                    dropped += 1;
                    continue;
                }
            }
        } else if let Some(id) = pin.pool_id {
            // poolId pin: the PoolKey preimage lives in the V4 discovery cache.
            let cache = caches.entry(dex_cfg.name.clone()).or_insert_with(|| {
                discovery::cached_pools(&cfg.discovery.cache_dir, &dex_cfg.name)
                    .into_iter()
                    .filter_map(|dp| dp.pool_id.map(|id| (id, dp)))
                    .collect()
            });
            match cache.get(&id) {
                Some(dp) => dp.clone(),
                None => {
                    tracing::warn!(
                        pool_id = %id,
                        "poolId not in the V4 discovery cache (run `discover` first, \
                         or pin with the full PoolKey); dropping"
                    );
                    dropped += 1;
                    continue;
                }
            }
        } else {
            tracing::warn!(dex = %pin.dex, "v4 pin needs pool_id or the full PoolKey; dropping");
            dropped += 1;
            continue;
        };
        candidates.push(dp);
    }

    // Liveness probe: an uninitialized id quotes sqrtPrice = 0.
    let calls: Vec<Call> = candidates
        .iter()
        .map(|dp| Call {
            target: UNIV4_STATE_VIEW,
            calldata: IStateView::getSlot0Call { poolId: dp.pool_id.expect("v4 pin has pool_id") }
                .abi_encode()
                .into(),
        })
        .collect();
    let res = multicall::aggregate3(provider, &calls, MC_BATCH).await?;
    let mut out = Vec::with_capacity(candidates.len());
    for (dp, r) in candidates.into_iter().zip(res.iter()) {
        let live = r.success
            && IStateView::getSlot0Call::abi_decode_returns(&r.returnData)
                .map(|s| !s.sqrtPriceX96.is_zero())
                .unwrap_or(false);
        if live {
            out.push(dp);
        } else {
            tracing::warn!(pool_id = ?dp.pool_id, "V4 pool not initialized on-chain; dropping");
            dropped += 1;
        }
    }

    tracing::info!(loaded = out.len(), dropped, "loaded V4 pins (liveness-probed via StateView)");
    Ok(out)
}

/// factory() selector = 0xc45a0155.
fn factory_calldata() -> alloy::primitives::Bytes {
    alloy::primitives::Bytes::from_static(&[0xc4, 0x5a, 0x01, 0x55])
}

fn decode_addr(data: &[u8]) -> Option<Address> {
    if data.len() < 32 {
        return None;
    }
    Some(Address::from_slice(&data[12..32]))
}
