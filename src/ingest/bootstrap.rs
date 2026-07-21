//! Hydrate discovered pools into `PoolRegistration`s via Multicall3, and build V3
//! tick tables around the current price (window scan).
//!
//! Tick hydration note: this uses a bounded bitmap window (`tick_window_words`
//! either side of the current word) — correct for near-price quoting and fast to
//! bring up. `hydrate_ticks_full` (scanning Mint/Burn history) gives the complete
//! map for large-size routing and can be enabled once an archive node is wired;
//! the tick table type and downstream math are identical either way.

use crate::abi::{IAeroFactory, IAeroPool, IAlgebraPool, IERC20, IStateView, IUniV2Pair, IUniV3Pool};
use crate::config::{AnchorConfig, Config, FilterConfig};
use crate::constants::UNIV4_STATE_VIEW;
use crate::engine::pool_meta::{PoolMeta, V4Meta};
use crate::engine::{DexTag, PoolIdx, PoolRegistration};
use crate::ingest::discovery::DiscoveredPool;
use crate::ingest::multicall::{self, Call};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::DynProvider;
use alloy::sol_types::{SolCall, SolValue};
use anyhow::Result;
use rustc_hash::FxHashMap;
use std::collections::HashMap;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;

const MC_BATCH: usize = 400;

/// Bitmap words hydrated either side of the current price word (bootstrap and
/// refresh both use this window).
/// Hydrate all discovered pools; apply the working-set filter; return registrations.
pub async fn hydrate(
    provider: &DynProvider,
    cfg: &Config,
    discovered: Vec<DiscoveredPool>,
    enforce_liquidity: bool,
) -> Result<Vec<PoolRegistration>> {
    // 0. Dedupe by address (duplicate pins / cache entries): Engine treats a
    // duplicate address as fatal since V4 synthetic addresses made collisions
    // a real (if astronomically unlikely) cross-wiring hazard.
    let discovered = {
        let mut seen: std::collections::HashSet<Address> = std::collections::HashSet::new();
        let mut uniq = Vec::with_capacity(discovered.len());
        for p in discovered {
            if seen.insert(p.address) {
                uniq.push(p);
            } else {
                tracing::warn!(pool = %p.address, "duplicate pool entry; keeping first");
            }
        }
        uniq
    };

    // 1. Token decimals (dedupe).
    let mut token_set: Vec<Address> = Vec::new();
    for p in &discovered {
        for t in [p.token0, p.token1] {
            if !token_set.contains(&t) {
                token_set.push(t);
            }
        }
    }
    let decimals = fetch_decimals(provider, &token_set).await?;

    // 2. Per-pool state.
    let mut regs: Vec<PoolRegistration> = Vec::new();
    let mut v3_pending: Vec<(usize, DiscoveredPool, U256, i32, u128, u32, i32, u8, u8)> = Vec::new();

    // 2a. V2 / Aero state via getReserves; Aero fee via factory.getFee.
    let v2_calls: Vec<(usize, Call)> = discovered
        .iter()
        .enumerate()
        .filter(|(_, p)| p.dex.is_v2_family() || p.dex == DexTag::AeroStable)
        .map(|(i, p)| {
            (i, Call { target: p.address, calldata: reserves_calldata(p.dex).into() })
        })
        .collect();

    let v2_results = if v2_calls.is_empty() {
        vec![]
    } else {
        let calls: Vec<Call> = v2_calls.iter().map(|(_, c)| c.clone()).collect();
        multicall::aggregate3(provider, &calls, MC_BATCH).await?
    };

    // Aero fee calls (getFee) in a second batch keyed by pool index.
    let aero_fee_idx: Vec<usize> = discovered
        .iter()
        .enumerate()
        .filter(|(_, p)| matches!(p.dex, DexTag::AeroVolatile | DexTag::AeroStable))
        .map(|(i, _)| i)
        .collect();
    let aero_fee_calls: Vec<Call> = aero_fee_idx
        .iter()
        .map(|&i| {
            let p = &discovered[i];
            let stable = p.dex == DexTag::AeroStable;
            Call {
                target: p.factory,
                calldata: IAeroFactory::getFeeCall { pool: p.address, stable }
                    .abi_encode()
                    .into(),
            }
        })
        .collect();
    let aero_fee_results = if aero_fee_calls.is_empty() {
        vec![]
    } else {
        multicall::aggregate3(provider, &aero_fee_calls, MC_BATCH).await?
    };
    let mut aero_fee_map: FxHashMap<usize, u32> = FxHashMap::default();
    for (slot, &i) in aero_fee_idx.iter().enumerate() {
        if let Some(r) = aero_fee_results.get(slot) {
            if r.success {
                if let Ok(fee) = U256::abi_decode(&r.returnData) {
                    aero_fee_map.insert(i, fee.to::<u32>());
                }
            }
        }
    }

    for (slot, (i, _)) in v2_calls.iter().enumerate() {
        let p = &discovered[*i];
        let res = match v2_results.get(slot) {
            Some(r) if r.success => r,
            _ => continue,
        };
        let dec0 = *decimals.get(&p.token0).unwrap_or(&18);
        let dec1 = *decimals.get(&p.token1).unwrap_or(&18);
        let idx = PoolIdx(regs.len() as u32);

        match p.dex {
            DexTag::UniV2Fork | DexTag::AeroVolatile => {
                let Some((r0, r1)) = decode_reserves_v2ish(p.dex, &res.returnData) else { continue };
                let fee_bps = if p.dex == DexTag::AeroVolatile {
                    *aero_fee_map.get(i).unwrap_or(&30)
                } else {
                    p.fee_bps.unwrap_or(30)
                };
                let meta = Arc::new(PoolMeta {
                    idx,
                    address: p.address,
                    dex: p.dex,
                    factory: p.factory,
                    token0: p.token0,
                    token1: p.token1,
                    dec0,
                    dec1,
                    fee_bps: AtomicU32::new(fee_bps),
                    fee_pips: AtomicU32::new(fee_bps * 100),
                    tick_spacing: 0,
                    v4: None,
                });
                let _ = fee_bps;
                let gate = Some(anchor_sides(&meta, r0 as f64, r1 as f64, &cfg.anchors));
                regs.push(PoolRegistration { meta, gate });
            }
            DexTag::AeroStable => {
                let Some((r0, r1)) = decode_reserves_u256(&res.returnData) else { continue };
                let fee_bps = *aero_fee_map.get(i).unwrap_or(&5);
                let meta = Arc::new(PoolMeta {
                    idx,
                    address: p.address,
                    dex: p.dex,
                    factory: p.factory,
                    token0: p.token0,
                    token1: p.token1,
                    dec0,
                    dec1,
                    fee_bps: AtomicU32::new(fee_bps),
                    fee_pips: AtomicU32::new(fee_bps * 100),
                    tick_spacing: 0,
                    v4: None,
                });
                let _ = fee_bps;
                let gate = Some(anchor_sides(
                    &meta,
                    crate::math::u256_to_f64(r0),
                    crate::math::u256_to_f64(r1),
                    &cfg.anchors,
                ));
                regs.push(PoolRegistration { meta, gate });
            }
            _ => {}
        }
    }

    // 2b. V3-family: slot0 + liquidity (+ fee for slipstream). Collect for tick pass.
    let v3_pools: Vec<(usize, &DiscoveredPool)> = discovered
        .iter()
        .enumerate()
        .filter(|(_, p)| p.dex.is_v3_family())
        .collect();

    if !v3_pools.is_empty() {
        // slot0 + liquidity per pool (two calls each, interleaved).
        let mut calls: Vec<Call> = Vec::with_capacity(v3_pools.len() * 3);
        for (_, p) in &v3_pools {
            calls.push(Call { target: p.address, calldata: IUniV3Pool::slot0Call {}.abi_encode().into() });
            calls.push(Call { target: p.address, calldata: IUniV3Pool::liquidityCall {}.abi_encode().into() });
            calls.push(Call { target: p.address, calldata: IUniV3Pool::feeCall {}.abi_encode().into() });
        }
        let results = multicall::aggregate3(provider, &calls, MC_BATCH).await?;

        for (n, (i, p)) in v3_pools.iter().enumerate() {
            let base = n * 3;
            let slot0 = results.get(base);
            let liq = results.get(base + 1);
            let fee = results.get(base + 2);
            let (Some(s), Some(l)) = (slot0, liq) else { continue };
            if !s.success || !l.success {
                continue;
            }
            let Some((sqrt_price, tick)) = decode_slot0(&s.returnData) else { continue };
            let liquidity = decode_u128(&l.returnData).unwrap_or(0);
            let fee_pips = p
                .fee_pips
                .or_else(|| fee.and_then(|f| if f.success { decode_u128(&f.returnData).map(|v| v as u32) } else { None }))
                .unwrap_or(3000);
            let tick_spacing = p.tick_spacing.unwrap_or(60);
            let dec0 = *decimals.get(&p.token0).unwrap_or(&18);
            let dec1 = *decimals.get(&p.token1).unwrap_or(&18);
            v3_pending.push((*i, (*p).clone(), sqrt_price, tick, liquidity, fee_pips, tick_spacing, dec0, dec1));
        }
    }

    for (_, p, _sqrt_price, _tick, _liquidity, fee_pips, tick_spacing, dec0, dec1) in v3_pending {
        let idx = PoolIdx(regs.len() as u32);
        let meta = Arc::new(PoolMeta {
            idx,
            address: p.address,
            dex: p.dex,
            factory: p.factory,
            token0: p.token0,
            token1: p.token1,
            dec0,
            dec1,
            fee_bps: AtomicU32::new(fee_pips / 100),
            fee_pips: AtomicU32::new(fee_pips),
            tick_spacing,
            v4: None,
        });
        // CL pool: gated on its real anchor-token balanceOf (step 2d).
        regs.push(PoolRegistration { meta, gate: None });
    }

    // 2d. Algebra Integral: globalState (not slot0) + liquidity + fee, then the
    // Algebra tick sibling (tickTable + Algebra ticks()). Same PoolData::V3 shape.
    let algebra_pools: Vec<(usize, &DiscoveredPool)> = discovered
        .iter()
        .enumerate()
        .filter(|(_, p)| p.dex.is_algebra())
        .collect();
    if !algebra_pools.is_empty() {
        let mut calls: Vec<Call> = Vec::with_capacity(algebra_pools.len() * 3);
        for (_, p) in &algebra_pools {
            calls.push(Call { target: p.address, calldata: IAlgebraPool::globalStateCall {}.abi_encode().into() });
            calls.push(Call { target: p.address, calldata: IAlgebraPool::liquidityCall {}.abi_encode().into() });
            calls.push(Call { target: p.address, calldata: IAlgebraPool::feeCall {}.abi_encode().into() });
        }
        let results = multicall::aggregate3(provider, &calls, MC_BATCH).await?;

        // (pool, sqrt_price, tick, liquidity, fee_pips, tick_spacing, dec0, dec1)
        let mut pending: Vec<(&DiscoveredPool, U256, i32, u128, u32, i32, u8, u8)> = Vec::new();
        for (n, (_, p)) in algebra_pools.iter().enumerate() {
            let base = n * 3;
            let (Some(gs), Some(l), fee) = (results.get(base), results.get(base + 1), results.get(base + 2))
            else { continue };
            if !gs.success || !l.success {
                continue;
            }
            let Ok(state) = IAlgebraPool::globalStateCall::abi_decode_returns(&gs.returnData) else {
                continue;
            };
            if state.price.is_zero() {
                continue; // uninitialized
            }
            let liquidity = decode_u128(&l.returnData).unwrap_or(0);
            // Effective (plugin-computed) fee now — the Swap decoder keeps it
            // fresh thereafter from overrideFee.
            let fee_pips = fee
                .and_then(|f| if f.success { decode_u128(&f.returnData).map(|v| v as u32) } else { None })
                .unwrap_or(3000);
            let tick_spacing = p.tick_spacing.unwrap_or(60);
            let dec0 = *decimals.get(&p.token0).unwrap_or(&18);
            let dec1 = *decimals.get(&p.token1).unwrap_or(&18);
            pending.push((
                *p,
                U256::from(state.price),
                state.tick.as_i32(),
                liquidity,
                fee_pips,
                tick_spacing,
                dec0,
                dec1,
            ));
        }

        for (p, _sqrt_price, _tick, _liquidity, fee_pips, tick_spacing, dec0, dec1) in pending {
            let idx = PoolIdx(regs.len() as u32);
            let meta = Arc::new(PoolMeta {
                idx,
                address: p.address,
                dex: DexTag::Algebra,
                factory: p.factory,
                token0: p.token0,
                token1: p.token1,
                dec0,
                dec1,
                fee_bps: AtomicU32::new(fee_pips / 100),
                fee_pips: AtomicU32::new(fee_pips),
                tick_spacing,
                v4: None,
            });
            // CL pool: gated on its real anchor-token balanceOf (step 2d).
            regs.push(PoolRegistration { meta, gate: None });
        }
    }

    // 2e. Uniswap V4: pools live in the singleton PoolManager — state comes
    // from StateView by poolId, never from the (synthetic) pool address. Same
    // V3 tick machinery downstream.
    let v4_pools: Vec<&DiscoveredPool> = discovered.iter().filter(|p| p.dex.is_v4()).collect();
    if !v4_pools.is_empty() {
        let mut calls: Vec<Call> = Vec::with_capacity(v4_pools.len() * 2);
        for p in &v4_pools {
            let id = p.pool_id.expect("V4 DiscoveredPool always carries pool_id");
            calls.push(Call {
                target: UNIV4_STATE_VIEW,
                calldata: IStateView::getSlot0Call { poolId: id }.abi_encode().into(),
            });
            calls.push(Call {
                target: UNIV4_STATE_VIEW,
                calldata: IStateView::getLiquidityCall { poolId: id }.abi_encode().into(),
            });
        }
        let results = multicall::aggregate3(provider, &calls, MC_BATCH).await?;

        // (pool, pool_id, sqrt_price, tick, liquidity, fee_pips, tick_spacing)
        let mut v4_pending: Vec<(&DiscoveredPool, B256, U256, i32, u128, u32, i32)> = Vec::new();
        for (n, p) in v4_pools.iter().enumerate() {
            let id = p.pool_id.expect("checked above");
            let (Some(s), Some(l)) = (results.get(n * 2), results.get(n * 2 + 1)) else { continue };
            if !s.success || !l.success {
                continue;
            }
            let Ok(slot0) = IStateView::getSlot0Call::abi_decode_returns(&s.returnData) else {
                continue;
            };
            if slot0.sqrtPriceX96.is_zero() {
                continue; // uninitialized id
            }
            // Governance protocol fee makes the effective fee direction-
            // dependent and != PoolKey.fee — the model can't price that.
            // It is 0 on Base today; drop loudly if that ever changes.
            if slot0.protocolFee != alloy::primitives::aliases::U24::ZERO {
                tracing::warn!(pool_id = %id, protocol_fee = %slot0.protocolFee,
                    "V4 protocol fee enabled; dropping pool (model can't price it)");
                continue;
            }
            let fee_pips = p.fee_pips.expect("V4 discovery always sets fee_pips");
            // Vanilla pools: lpFee always equals the static PoolKey fee.
            if slot0.lpFee.to::<u32>() != fee_pips {
                tracing::warn!(pool_id = %id, key_fee = fee_pips, lp_fee = %slot0.lpFee,
                    "V4 lpFee != PoolKey fee on a supposedly static pool; dropping");
                continue;
            }
            let liquidity = decode_u128(&l.returnData).unwrap_or(0);
            let tick_spacing = p.tick_spacing.expect("V4 discovery always sets tick_spacing");
            v4_pending.push((
                p,
                id,
                U256::from(slot0.sqrtPriceX96),
                slot0.tick.as_i32(),
                liquidity,
                fee_pips,
                tick_spacing,
            ));
        }

        for (p, id, sqrt_price, tick, liquidity, fee_pips, tick_spacing) in v4_pending {
            let _ = tick;
            let dec0 = *decimals.get(&p.token0).unwrap_or(&18);
            let dec1 = *decimals.get(&p.token1).unwrap_or(&18);
            let idx = PoolIdx(regs.len() as u32);
            let meta = Arc::new(PoolMeta {
                idx,
                address: p.address,
                dex: DexTag::UniV4,
                factory: p.factory,
                token0: p.token0,
                token1: p.token1,
                dec0,
                dec1,
                fee_bps: AtomicU32::new(fee_pips / 100),
                fee_pips: AtomicU32::new(fee_pips),
                tick_spacing,
                v4: Some(Box::new(V4Meta {
                    pool_id: id,
                    currency0: p.currency0_raw.expect("V4 discovery always sets currency0_raw"),
                    currency1: p.token1,
                    fee_pips,
                    tick_spacing,
                })),
            });
            // Singleton custody: balanceOf is meaningless. Estimate the
            // virtual reserve on each side at the current price (token0:
            // x = L·2^96/√P, token1: y = L·√P/2^96). Virtual depth is
            // exactly the metric the fork replays taught us not to trust
            // near range edges, so demand 2× the configured gate.
            let sp = crate::math::u256_to_f64(sqrt_price) / 2f64.powi(96);
            let (r0v, r1v) = if sp > 0.0 {
                let l = liquidity as f64;
                (l / sp, l * sp)
            } else {
                (0.0, 0.0)
            };
            let mut gate = anchor_sides(&meta, r0v, r1v, &cfg.anchors);
            for s in &mut gate {
                s.1 /= 2.0;
            }
            regs.push(PoolRegistration { meta, gate: Some(gate) });
        }
    }

    // 2d. Real anchor-token balance for V3-family/Algebra pools that touch a
    // configured anchor. Their virtual `liquidity` says nothing about depth
    // (a $3k pool passes any gate on it), and the fork replay traced 143/209
    // phantom opps to exactly those micro CL pools — so gate on the anchor
    // token the pool actually holds. `(pool address, anchor token)` keyed,
    // since a CL pool can touch two anchors at once (e.g. WETH/USDC).
    let mut cl_anchor_bal: FxHashMap<(Address, Address), f64> = FxHashMap::default();
    if enforce_liquidity {
        let targets: Vec<(Address, &AnchorConfig)> = regs
            .iter()
            .filter(|r| r.gate.is_none()) // CL pools (V4 carries its virtual estimate)
            .flat_map(|r| {
                cfg.anchors
                    .iter()
                    .filter(|a| r.meta.token0 == a.token || r.meta.token1 == a.token)
                    .map(|a| (r.meta.address, a))
            })
            .collect();
        if !targets.is_empty() {
            let calls: Vec<Call> = targets
                .iter()
                .map(|(pool, a)| Call { target: a.token, calldata: balance_of_calldata(*pool) })
                .collect();
            let results = multicall::aggregate3(provider, &calls, MC_BATCH).await?;
            for ((pool, a), r) in targets.iter().zip(results.iter()) {
                if r.success {
                    if let Some(bal) = decode_u128(&r.returnData) {
                        cl_anchor_bal.insert((*pool, a.token), bal as f64 / 10f64.powi(a.decimals as i32));
                    }
                }
            }
        }
    }

    // 2e. Fee-on-transfer / rebase detection. Runs once per newly-seen
    // non-anchor token (cached after); positives are unioned into the
    // blacklist so no route is ever built through a token that shorts its
    // own transfers — see `ingest/fee_probe.rs` for why.
    let fee_tokens =
        crate::ingest::fee_probe::detect(provider, &discovered, &decimals, &cfg.anchors, &cfg.discovery.cache_dir)
            .await;
    let mut filter = cfg.filter.clone();
    filter.token_blacklist.extend(fee_tokens);

    // 3. Working-set filter.
    let kept = apply_filter(regs, &filter, enforce_liquidity, &cl_anchor_bal, &cfg.anchors);
    // Re-index sequentially after filtering.
    let reindexed = reindex(kept);
    tracing::info!(pools = reindexed.len(), "hydration complete");
    Ok(reindexed)
}

fn reindex(mut regs: Vec<PoolRegistration>) -> Vec<PoolRegistration> {
    for (i, reg) in regs.iter_mut().enumerate() {
        // PoolMeta.idx is inside an Arc; rebuild the Arc with corrected idx.
        let m = &reg.meta;
        let new_meta = PoolMeta {
            idx: PoolIdx(i as u32),
            address: m.address,
            dex: m.dex,
            factory: m.factory,
            token0: m.token0,
            token1: m.token1,
            dec0: m.dec0,
            dec1: m.dec1,
            fee_bps: AtomicU32::new(m.fee_bps()),
            fee_pips: AtomicU32::new(m.fee_pips()),
            tick_spacing: m.tick_spacing,
            v4: m.v4.clone(),
        };
        reg.meta = Arc::new(new_meta);
    }
    regs
}

/// Keep pools whose tokens pass whitelist/blacklist and that carry enough
/// anchor-side liquidity (on at least ONE matching anchor) to be worth
/// watching. Applies to pinned pools too: pools.toml is machine-generated
/// from DEX Screener, not hand-picked, and the fork replay showed micro
/// pools slipping through it are pure noise.
fn apply_filter(
    regs: Vec<PoolRegistration>,
    filter: &FilterConfig,
    enforce_liquidity: bool,
    cl_anchor_bal: &FxHashMap<(Address, Address), f64>,
    anchors: &[AnchorConfig],
) -> Vec<PoolRegistration> {
    regs.into_iter()
        .filter(|reg| {
            let m = &reg.meta;
            if filter.token_blacklist.contains(&m.token0) || filter.token_blacklist.contains(&m.token1) {
                return false;
            }
            if !filter.token_whitelist.is_empty() {
                let ok = |t: &Address| {
                    anchors.iter().any(|a| a.token == *t) || filter.token_whitelist.contains(t)
                };
                if !ok(&m.token0) || !ok(&m.token1) {
                    return false;
                }
            }
            if enforce_liquidity && !anchor_liquidity(reg, cl_anchor_bal, anchors) {
                // tracing::debug!(pool = %m.address, "below anchor liquidity gate; dropping");
                return false;
            }
            true
        })
        .collect()
}

/// Whether this pool clears the liquidity gate on at least one anchor side
/// it touches. A pool touching no configured anchor is always kept (pure
/// middle-hop — any route through it still enters/exits via an anchor-gated
/// pool elsewhere).
fn anchor_liquidity(
    reg: &PoolRegistration,
    cl_anchor_bal: &FxHashMap<(Address, Address), f64>,
    anchors: &[AnchorConfig],
) -> bool {
    let m = &reg.meta;
    let touches: Vec<&AnchorConfig> =
        anchors.iter().filter(|a| m.token0 == a.token || m.token1 == a.token).collect();
    if touches.is_empty() {
        return true;
    }
    touches.iter().any(|a| {
        let depth = match &reg.gate {
            // Computed at hydration (V2/AeroStable real reserve; V4 virtual/2).
            Some(sides) => sides.iter().find(|(t, _)| *t == a.token).map(|(_, d)| *d).unwrap_or(0.0),
            // CL pool: virtual `liquidity` is not a depth measure — use the
            // anchor balance the pool actually holds (hydrate step 2d).
            None => cl_anchor_bal.get(&(m.address, a.token)).copied().unwrap_or(0.0),
        };
        depth >= a.min_liquidity
    })
}

/// For each side (token0, token1) that matches a configured anchor, the
/// whole-unit depth on that side (normalized by the token's real on-chain
/// decimals, not the anchor's configured `decimals` — defends against a
/// config typo). A pool between two anchors (e.g. WETH/USDC) yields one
/// entry per side.
fn anchor_sides(meta: &PoolMeta, r0: f64, r1: f64, anchors: &[AnchorConfig]) -> Vec<(Address, f64)> {
    let mut out = Vec::new();
    for a in anchors {
        if meta.token0 == a.token {
            out.push((a.token, r0 / 10f64.powi(meta.dec0 as i32)));
        }
        if meta.token1 == a.token {
            out.push((a.token, r1 / 10f64.powi(meta.dec1 as i32)));
        }
    }
    out
}

/// balanceOf(owner) calldata, selector 0x70a08231.
fn balance_of_calldata(owner: Address) -> alloy::primitives::Bytes {
    let mut v = Vec::with_capacity(36);
    v.extend_from_slice(&[0x70, 0xa0, 0x82, 0x31]);
    v.extend_from_slice(&[0u8; 12]);
    v.extend_from_slice(owner.as_slice());
    v.into()
}

// ---------------------------------------------------------------------------
// Multicall helpers
// ---------------------------------------------------------------------------

async fn fetch_decimals(
    provider: &DynProvider,
    tokens: &[Address],
) -> Result<HashMap<Address, u8>> {
    let calls: Vec<Call> = tokens
        .iter()
        .map(|t| Call { target: *t, calldata: IERC20::decimalsCall {}.abi_encode().into() })
        .collect();
    let results = multicall::aggregate3(provider, &calls, MC_BATCH).await?;
    let mut map = HashMap::new();
    for (t, r) in tokens.iter().zip(results.iter()) {
        let d = if r.success { decode_u128(&r.returnData).map(|v| v as u8).unwrap_or(18) } else { 18 };
        map.insert(*t, d);
    }
    Ok(map)
}

pub(crate) fn reserves_calldata(dex: DexTag) -> Vec<u8> {
    match dex {
        DexTag::UniV2Fork => IUniV2Pair::getReservesCall {}.abi_encode(),
        DexTag::AeroVolatile | DexTag::AeroStable => IAeroPool::getReservesCall {}.abi_encode(),
        _ => IUniV2Pair::getReservesCall {}.abi_encode(),
    }
}

pub(crate) fn decode_reserves_v2ish(dex: DexTag, data: &[u8]) -> Option<(u128, u128)> {
    match dex {
        DexTag::UniV2Fork => {
            let r = IUniV2Pair::getReservesCall::abi_decode_returns(data).ok()?;
            Some((r.reserve0.to::<u128>(), r.reserve1.to::<u128>()))
        }
        DexTag::AeroVolatile => decode_reserves_u256(data).map(|(a, b)| {
            (a.try_into().unwrap_or(u128::MAX), b.try_into().unwrap_or(u128::MAX))
        }),
        _ => None,
    }
}

pub(crate) fn decode_reserves_u256(data: &[u8]) -> Option<(U256, U256)> {
    let r = IAeroPool::getReservesCall::abi_decode_returns(data).ok()?;
    Some((r.reserve0, r.reserve1))
}

/// Decode (sqrtPriceX96, tick) from the first two words of a slot0 return —
/// fork-agnostic (ignores trailing fields).
pub(crate) fn decode_slot0(data: &[u8]) -> Option<(U256, i32)> {
    if data.len() < 64 {
        return None;
    }
    let sqrt_price = U256::from_be_slice(&data[0..32]);
    // int24 sign-extended to 32 bytes; low 4 bytes hold the int32 form.
    let tick = i32::from_be_bytes([data[60], data[61], data[62], data[63]]);
    Some((sqrt_price, tick))
}

pub(crate) fn decode_u128(data: &[u8]) -> Option<u128> {
    if data.len() < 32 {
        return None;
    }
    Some(U256::from_be_slice(&data[0..32]).to::<u128>())
}
