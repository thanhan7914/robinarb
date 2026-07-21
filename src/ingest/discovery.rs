//! Factory pool discovery with an incremental per-DEX cache (ports PoolSync's
//! approach). Scans `PoolCreated`/`PairCreated` logs, decoding per protocol, and
//! persists `{last_synced_block, pools}` so restarts only sync the delta.

use crate::abi::{AeroEvents, SlipstreamEvents, UniV2Events, UniV3Events, UniV4Events};
use crate::config::{Config, DexConfig};
use crate::constants::{UNIV4_DYNAMIC_FEE_FLAG, UNIV4_MAX_LP_FEE, WETH};
use crate::engine::pool_meta::V4Meta;
use crate::engine::DexTag;
use crate::ingest::multicall;
use alloy::primitives::{Address, B256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A pool found by scanning a factory. Metadata (decimals, reserves, ticks) is
/// hydrated later by bootstrap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredPool {
    pub address: Address,
    pub dex: DexTag,
    pub factory: Address,
    pub token0: Address,
    pub token1: Address,
    /// UniV3-family fee in pips (from the PoolCreated event).
    pub fee_pips: Option<u32>,
    /// V3 / Slipstream tick spacing.
    pub tick_spacing: Option<i32>,
    /// V2 forks LP fee in bps (from config).
    pub fee_bps: Option<u32>,
    /// Uniswap V4 only: the pool's bytes32 id (`address` above is synthetic —
    /// the id's first 20 bytes). serde default keeps older caches loadable.
    #[serde(default)]
    pub pool_id: Option<B256>,
    /// Uniswap V4 only: RAW PoolKey currency0 (Address::ZERO = native ETH;
    /// `token0` above is then normalized to WETH for routing).
    #[serde(default)]
    pub currency0_raw: Option<Address>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DexCache {
    last_synced_block: u64,
    pools: Vec<DiscoveredPool>,
}

fn cache_path(dir: &str, dex_name: &str) -> PathBuf {
    PathBuf::from(dir).join(format!("discovery_{dex_name}.json"))
}

fn load_cache(dir: &str, dex_name: &str) -> DexCache {
    let p = cache_path(dir, dex_name);
    if let Ok(raw) = std::fs::read_to_string(&p) {
        if let Ok(c) = serde_json::from_str::<DexCache>(&raw) {
            return c;
        }
    }
    DexCache { last_synced_block: 0, pools: vec![] }
}

fn save_cache(dir: &str, dex_name: &str, cache: &DexCache) -> Result<()> {
    std::fs::create_dir_all(dir).ok();
    let p = cache_path(dir, dex_name);
    std::fs::write(&p, serde_json::to_string(cache)?)
        .with_context(|| format!("writing cache {}", p.display()))?;
    Ok(())
}

/// Discover pools for every configured DEX, using and updating the cache.
pub async fn discover_all(
    provider: &DynProvider,
    cfg: &Config,
) -> Result<Vec<DiscoveredPool>> {
    let head = provider.get_block_number().await?;
    let mut all = Vec::new();

    for dex in &cfg.dexes {
        let mut cache = load_cache(&cfg.discovery.cache_dir, &dex.name);
        let start = if cache.last_synced_block == 0 {
            dex.deploy_block.unwrap_or(cfg.discovery.start_block)
        } else {
            cache.last_synced_block + 1
        };

        if start <= head {
            tracing::info!(dex = %dex.name, start, head, cached = cache.pools.len(), "scanning factory");
            scan_factory_incremental(
                provider,
                dex,
                start,
                head,
                cfg.discovery.getlogs_chunk,
                &cfg.discovery.cache_dir,
                &mut cache,
            )
            .await?;
            tracing::info!(dex = %dex.name, found = cache.pools.len(), "discovered");
        } else {
            tracing::info!(dex = %dex.name, cached = cache.pools.len(), "up to date (from cache)");
        }

        all.extend(cache.pools);
    }

    tracing::info!(total = all.len(), "discovery complete");
    Ok(all)
}

/// Scan [from, to] chunk by chunk, decoding pools and **checkpointing the cache**
/// periodically (every ~10s) and its `last_synced_block`. An interrupted run
/// resumes from the last checkpoint instead of restarting from `deploy_block`.
async fn scan_factory_incremental(
    provider: &DynProvider,
    dex: &DexConfig,
    from: u64,
    to: u64,
    chunk: u64,
    cache_dir: &str,
    cache: &mut DexCache,
) -> Result<()> {
    let topic = match dex.kind.as_str() {
        "v2" => UniV2Events::PairCreated::SIGNATURE_HASH,
        "aero" => AeroEvents::PoolCreated::SIGNATURE_HASH,
        "v3" | "pancake_v3" => UniV3Events::PoolCreated::SIGNATURE_HASH,
        "slipstream" => SlipstreamEvents::PoolCreated::SIGNATURE_HASH,
        // V4: `factory` is the singleton PoolManager; pools announce via Initialize.
        "v4" => UniV4Events::Initialize::SIGNATURE_HASH,
        other => anyhow::bail!("unsupported dex kind {other}"),
    };
    let filter = Filter::new().address(dex.factory).event_signature(topic);

    let total = to.saturating_sub(from).saturating_add(1);
    let mut start = from;
    let mut last_log = std::time::Instant::now();
    let mut last_save = std::time::Instant::now();

    while start <= to {
        let end = (start + chunk - 1).min(to);
        let ranged = filter.clone().from_block(start).to_block(end);
        let logs = multicall::get_logs_bisect(provider, &ranged, start, end).await?;
        for log in &logs {
            if let Some(dp) = decode_pool_log(dex, log)? {
                cache.pools.push(dp);
            }
        }
        cache.last_synced_block = end;

        // Progress (throttled to ~3s).
        if last_log.elapsed() >= std::time::Duration::from_secs(3) || end == to {
            let done = end.saturating_sub(from).saturating_add(1);
            let pct = done.saturating_mul(100) / total.max(1);
            tracing::info!(
                dex = %dex.name, block = end, head = to, pct, found = cache.pools.len(),
                "scan progress"
            );
            last_log = std::time::Instant::now();
        }

        // Checkpoint (every ~10s and at completion) so interrupts can resume.
        if last_save.elapsed() >= std::time::Duration::from_secs(10) || end == to {
            save_cache(cache_dir, &dex.name, cache)?;
            last_save = std::time::Instant::now();
        }

        start = end + 1;
    }
    Ok(())
}

fn decode_pool_log(dex: &DexConfig, log: &alloy::rpc::types::Log) -> Result<Option<DiscoveredPool>> {
    let dp = match dex.kind.as_str() {
        "v2" => {
            let ev = UniV2Events::PairCreated::decode_log(&log.inner)?;
            DiscoveredPool {
                address: ev.pair,
                dex: DexTag::UniV2Fork,
                factory: dex.factory,
                token0: ev.token0,
                token1: ev.token1,
                fee_pips: None,
                tick_spacing: None,
                fee_bps: dex.fee_bps,
                pool_id: None,
                currency0_raw: None,
            }
        }
        "aero" => {
            let ev = AeroEvents::PoolCreated::decode_log(&log.inner)?;
            DiscoveredPool {
                address: ev.pool,
                dex: if ev.stable { DexTag::AeroStable } else { DexTag::AeroVolatile },
                factory: dex.factory,
                token0: ev.token0,
                token1: ev.token1,
                fee_pips: None,
                tick_spacing: None,
                fee_bps: None, // read from factory.getFee at hydration
                pool_id: None,
                currency0_raw: None,
            }
        }
        "v3" | "pancake_v3" => {
            let ev = UniV3Events::PoolCreated::decode_log(&log.inner)?;
            let dt = if dex.kind == "pancake_v3" { DexTag::PancakeV3 } else { DexTag::UniV3Fork };
            DiscoveredPool {
                address: ev.pool,
                dex: dt,
                factory: dex.factory,
                token0: ev.token0,
                token1: ev.token1,
                fee_pips: Some(ev.fee.to::<u32>()),
                tick_spacing: Some(ev.tickSpacing.as_i32()),
                fee_bps: None,
                pool_id: None,
                currency0_raw: None,
            }
        }
        "slipstream" => {
            let ev = SlipstreamEvents::PoolCreated::decode_log(&log.inner)?;
            DiscoveredPool {
                address: ev.pool,
                dex: DexTag::Slipstream,
                factory: dex.factory,
                token0: ev.token0,
                token1: ev.token1,
                fee_pips: None, // read from pool.fee() at hydration
                tick_spacing: Some(ev.tickSpacing.as_i32()),
                fee_bps: None,
                pool_id: None,
                currency0_raw: None,
            }
        }
        "v4" => {
            let ev = UniV4Events::Initialize::decode_log(&log.inner)?;
            return Ok(v4_pool_from_key(
                dex.factory,
                ev.id,
                ev.currency0,
                ev.currency1,
                ev.fee.to::<u32>(),
                ev.tickSpacing.as_i32(),
                ev.hooks,
            ));
        }
        _ => return Ok(None),
    };
    Ok(Some(dp))
}

/// Build a V4 `DiscoveredPool` from PoolKey parts, applying the vanilla-only
/// policy and native-ETH normalization. Returns None for pools we refuse to
/// model: hooked (hooks can rewrite amounts/fees at will), dynamic-fee, and
/// the degenerate native-ETH/WETH pair (normalizes to WETH/WETH).
pub(crate) fn v4_pool_from_key(
    pool_manager: Address,
    pool_id: B256,
    currency0: Address,
    currency1: Address,
    fee_pips: u32,
    tick_spacing: i32,
    hooks: Address,
) -> Option<DiscoveredPool> {
    if hooks != Address::ZERO || fee_pips == UNIV4_DYNAMIC_FEE_FLAG || fee_pips > UNIV4_MAX_LP_FEE {
        return None;
    }
    if currency0 == Address::ZERO && currency1 == WETH {
        return None;
    }
    let token0 = if currency0 == Address::ZERO { WETH } else { currency0 };
    Some(DiscoveredPool {
        address: V4Meta::synthetic_address(pool_id),
        dex: DexTag::UniV4,
        factory: pool_manager,
        token0,
        token1: currency1,
        fee_pips: Some(fee_pips),
        tick_spacing: Some(tick_spacing),
        fee_bps: None,
        pool_id: Some(pool_id),
        currency0_raw: Some(currency0),
    })
}

/// Read a DEX's discovery cache (loader uses this to resolve V4 poolId pins
/// against the Initialize scan, which holds the PoolKey preimage).
pub fn cached_pools(cache_dir: &str, dex_name: &str) -> Vec<DiscoveredPool> {
    load_cache(cache_dir, dex_name).pools
}
