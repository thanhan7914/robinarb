//! Verification tooling (M1 checks): gas-oracle reachability, local-state vs
//! on-chain-state comparison, and model-quote vs on-chain-quote per hop.

use crate::abi::{IAeroPool, IArbGasInfo, IPoolQuoter, IStateView, IUniV2Pair, IUniV3Pool, IV4Quoter};
use crate::app::App;
use crate::config::{AnchorConfig, Config};
use crate::constants::{ARB_GAS_INFO, POOL_QUOTER_ADDR, POOL_QUOTER_CODE, UNIV4_QUOTER, UNIV4_STATE_VIEW};
use crate::engine::{DexTag, PoolIdx};
use crate::state::{ClFee, SlotPlan};
use crate::math;
use crate::routing::optimizer;
use crate::routing::types::Hop;
use alloy::primitives::{Address, U256};
use alloy::providers::{DynProvider, ProviderBuilder};
use anyhow::Result;
use rustc_hash::FxHashMap;

/// Retry an eth_call through transient rate limits (429) on weak RPCs; any
/// other error (e.g. a real pool revert) fails fast. Verify tooling fires
/// thousands of sequential eth_calls, which bursts past free-tier CU/s caps.
async fn with_429_backoff<T, F, Fut>(mut f: F) -> Result<T, alloy::contract::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, alloy::contract::Error>>,
{
    let mut backoff = 400u64;
    for _ in 0..4 {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if e.to_string().contains("429") => {
                tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                backoff *= 2;
            }
            Err(e) => return Err(e),
        }
    }
    f().await
}

pub async fn check_gas_oracle(cfg: &Config) -> Result<()> {
    let http = ProviderBuilder::new().connect_http(cfg.rpc.http[0].parse()?);
    let oracle = IArbGasInfo::new(ARB_GAS_INFO, &http);
    let l1 = oracle.getL1BaseFeeEstimate().call().await?;
    let prices = oracle.getPricesInWei().call().await?;
    tracing::info!(
        l1_base_fee_estimate = %l1,
        per_l2_tx = %prices.perL2Tx,
        per_l1_calldata_unit = %prices.perL1CalldataUnit,
        "ArbGasInfo reachable"
    );
    Ok(())
}

/// Decode audit: ChainState raw reads + decoders vs a fresh eth_call for
/// `sample` pools — the cheapest smoke test that every SlotPlan (slot
/// numbers, packing, fee source) is right. Logs divergences; returns Ok
/// regardless (diagnostic).
pub async fn verify_state(app: &App, sample: usize) -> Result<()> {
    use crate::ingest::slot_layout::{decode_shifted128, decode_v2_packed, decode_v3_slot0};

    let http = &app.http;
    let n = app.engine.len().min(sample.max(1));
    let mut mismatches = 0u64;
    let mut skipped = 0u64;

    for i in 0..n {
        let idx = PoolIdx(i as u32);
        let meta = app.engine.meta(idx);
        let Some(plan) = app.state.plan(idx) else {
            skipped += 1;
            continue;
        };
        // Pin generations: sync ChainState to the node tip and re-check after
        // the eth_calls; a tip race retries.
        let mut ok: Option<bool> = None;
        for _ in 0..3 {
            let t0 = app.state.refresh_tip().await.unwrap_or(0);
            app.state.advance_to(t0);
            // advance_to just cleared the memo -- read() is memo-only (see
            // state.rs module doc), so this pool's slots must be re-fetched
            // before any read() call below, or every decode below reads zero.
            app.state.prefetch(&crate::state::ChainState::plan_slots(plan)).await;
            app.state.hydrate_cl_ticks_for_pool(plan, app.engine.meta(idx).tick_spacing).await;
            let res = match plan {
                SlotPlan::V2Packed { addr, slot } => {
                    let (r0, r1) = decode_v2_packed(app.state.read(*addr, *slot));
                    verify_v2(http, meta.address, r0, r1).await
                }
                SlotPlan::V2TwoSlot { addr, r0 } => {
                    let w0 = app.state.read(*addr, *r0);
                    let w1 = app.state.read(*addr, *r0 + U256::from(1u8));
                    verify_aero(http, meta.address, w0, w1).await
                }
                SlotPlan::Cl { addr, slot0, liquidity, liquidity_shift, fee, .. } => {
                    let word0 = app.state.read(*addr, *slot0);
                    let (sqrt, tick) = decode_v3_slot0(word0);
                    let liq = decode_shifted128(app.state.read(*addr, *liquidity), *liquidity_shift);
                    if let Some(v4) = meta.v4.as_ref() {
                        verify_v4(http, v4.pool_id, sqrt, tick, liq).await
                    } else if meta.dex.is_algebra() {
                        let fee_word = match fee {
                            ClFee::Word { shift } => {
                                Some(((word0 >> *shift) & U256::from(0xffffu32)).to::<u32>())
                            }
                            ClFee::Meta => None,
                        };
                        verify_algebra(http, meta.address, sqrt, tick, liq, fee_word).await
                    } else {
                        verify_v3(http, meta.address, sqrt, tick, liq).await
                    }
                }
            };
            if app.state.refresh_tip().await.unwrap_or(0) != t0 {
                continue; // tip moved mid-check; retry
            }
            ok = res.ok();
            break;
        }
        match ok {
            Some(true) => {}
            Some(false) => {
                mismatches += 1;
                tracing::warn!(pool = %meta.address, dex = ?meta.dex, "decode audit MISMATCH");
            }
            None => tracing::debug!(pool = %meta.address, "decode audit call failed"),
        }
    }

    tracing::info!(checked = n, mismatches, skipped, "verify-state (decode audit) complete");
    Ok(())
}

async fn verify_v2(p: &DynProvider, pool: alloy::primitives::Address, r0: u128, r1: u128) -> Result<bool> {
    let c = IUniV2Pair::new(pool, p);
    let res = c.getReserves().call().await?;
    Ok(res.reserve0.to::<u128>() == r0 && res.reserve1.to::<u128>() == r1)
}

async fn verify_aero(p: &DynProvider, pool: alloy::primitives::Address, r0: U256, r1: U256) -> Result<bool> {
    let c = IAeroPool::new(pool, p);
    let res = c.getReserves().call().await?;
    Ok(res.reserve0 == r0 && res.reserve1 == r1)
}

async fn verify_v3(
    p: &DynProvider,
    pool: alloy::primitives::Address,
    sqrt: U256,
    tick: i32,
    liq: u128,
) -> Result<bool> {
    let c = IUniV3Pool::new(pool, p);
    let s = c.slot0().call().await?;
    let onchain_liq = c.liquidity().call().await?;
    Ok(U256::from(s.sqrtPriceX96) == sqrt && s.tick.as_i32() == tick && onchain_liq == liq)
}

async fn verify_algebra(
    p: &DynProvider,
    pool: alloy::primitives::Address,
    sqrt: U256,
    tick: i32,
    liq: u128,
    fee_word: Option<u32>,
) -> Result<bool> {
    let c = crate::abi::IAlgebraPool::new(pool, p);
    let s = c.globalState().call().await?;
    let onchain_liq = c.liquidity().call().await?;
    let mut ok = U256::from(s.price) == sqrt && s.tick.as_i32() == tick && onchain_liq == liq;
    if let Some(f) = fee_word {
        // fee-from-word pools: the word's fee must equal what a swap pays.
        let fee = c.fee().call().await?;
        ok &= f == fee as u32;
    }
    Ok(ok)
}

async fn verify_v4(
    p: &DynProvider,
    pool_id: alloy::primitives::B256,
    sqrt: U256,
    tick: i32,
    liq: u128,
) -> Result<bool> {
    let c = IStateView::new(UNIV4_STATE_VIEW, p);
    let s = c.getSlot0(pool_id).call().await?;
    let onchain_liq = c.getLiquidity(pool_id).call().await?;
    Ok(U256::from(s.sqrtPriceX96) == sqrt && s.tick.as_i32() == tick && onchain_liq == liq)
}

// ---------------------------------------------------------------------------
// Model-quote vs on-chain-quote per hop (the key M1 check for phantom opps)
// ---------------------------------------------------------------------------

/// Find the currently most-profitable routes (per the model) and verify each hop
/// against an authoritative on-chain quote, then report the REAL chained output.
pub async fn verify_quotes(app: &App, top: usize) -> Result<()> {
    let anchor_by_token: FxHashMap<Address, &AnchorConfig> =
        app.cfg.anchors.iter().map(|a| (a.token, a)).collect();

    // Collect model-profitable routes now.
    let mut opps = Vec::new();
    for route in &app.store.routes {
        let anchor = anchor_by_token
            .get(&route.anchor)
            .expect("route.anchor must match a configured [[anchor]]");
        let min_in = anchor.min_trade * 10f64.powi(anchor.decimals as i32);
        let max_in = anchor.max_trade * 10f64.powi(anchor.decimals as i32);
        if let Some(o) = optimizer::optimize(&app.engine, &app.state, route, min_in, max_in) {
            opps.push(o);
        }
    }
    opps.sort_by(|a, b| b.gross_profit.cmp(&a.gross_profit));
    opps.truncate(top.max(1));

    // Positive math check: sample pools and compare model(fresh) vs on-chain,
    // regardless of profitability. Confirms the quote math itself is correct.
    verify_pool_sample(app, 12).await?;

    if opps.is_empty() {
        tracing::info!("no model-profitable routes right now (efficient market on this pool set)");
        return Ok(());
    }

    println!("\n=== verifying {} model-profitable routes (model vs on-chain) ===\n", opps.len());
    for o in &opps {
        let route = app.store.route(o.route_id);
        println!("route_id={} model amount_in={} model gross_out={}", o.route_id, o.amount_in, o.gross_out);
        for (i, hop) in route.hops.iter().enumerate() {
            let meta = app.engine.meta(hop.pool);
            let sym = format!("{} {:?} in={:?}", dex_name(meta.dex), meta.address, hop.token_in);
            println!("  hop {i}: {sym}");
        }

        // Two parallel chains at the SAME sized input:
        //   model = direct-read quote off ChainState (what the bot trades on)
        //   chain = authoritative on-chain quoter/view (ground truth)
        let mut amt_model = o.amount_in;
        let mut amt_chain = o.amount_in;
        let mut all_verified = true;
        for (i, hop) in route.hops.iter().enumerate() {
            let meta = app.engine.meta(hop.pool);
            let t0 = app.state.refresh_tip().await.unwrap_or(0);
            app.state.advance_to(t0);
            // Targeted, not prefetch_all: only this hop's pool is read below
            // (unlike bench_route_scan, which needs the whole universe).
            if let Some(plan) = app.state.plan(hop.pool) {
                app.state.prefetch(&crate::state::ChainState::plan_slots(plan)).await;
                app.state.hydrate_cl_ticks_for_pool(plan, app.engine.meta(hop.pool).tick_spacing).await;
            }
            let model_out = math::quote(&app.state, meta, amt_model, hop.zero_for_one);
            let chain_out = onchain_quote_hop(&app.http, app, hop, amt_chain).await?;
            match chain_out {
                Some(c) => {
                    println!(
                        "    hop {i} [{}]: model={amt_model}->{model_out}  chain={c}  diff={:+.4}%",
                        dex_name(meta.dex),
                        pct_diff(model_out, c),
                    );
                    amt_model = model_out;
                    amt_chain = c;
                }
                None => {
                    println!(
                        "    hop {i} [{}]: model_out={model_out}  chain=UNVERIFIED (no quoter)",
                        dex_name(meta.dex)
                    );
                    all_verified = false;
                    amt_model = model_out;
                    amt_chain = model_out; // carry to keep chaining
                }
            }
        }

        println!(
            "  => model gross={} ({})  |  on-chain gross={} ({})",
            amt_model,
            signed(amt_model, o.amount_in),
            amt_chain,
            signed(amt_chain, o.amount_in),
        );
        let verdict = if !all_verified {
            "UNKNOWN (some hops had no on-chain quoter)"
        } else if amt_chain > o.amount_in {
            "REAL (on-chain output exceeds input)"
        } else {
            "PHANTOM (on-chain output <= input — no real profit)"
        };
        println!("  VERDICT: {verdict}\n");
    }
    Ok(())
}

/// Comprehensive per-DEX quote verification + timing. For every pool, quote
/// token0->token1 at several sizes (model on fresh state vs on-chain quoter),
/// aggregate accuracy per DEX tag, and benchmark local vs on-chain quote latency.
pub async fn verify_math(app: &App) -> Result<()> {
    use std::time::Instant;

    #[derive(Default)]
    struct Agg {
        pools: std::collections::HashSet<usize>,
        quotes: u64,
        max_diff: f64,
        sum_diff: f64,
        unverified: u64,
    }
    let mut per_dex: FxHashMap<DexTag, Agg> = FxHashMap::default();

    println!("\n=== per-DEX quote verification (direct-read model vs on-chain) ===");
    let n = app.engine.len();
    for i in 0..n {
        let idx = PoolIdx(i as u32);
        let meta = app.engine.meta(idx);
        let unit = U256::from(10u64).pow(U256::from(meta.dec0));
        // small (0.001), medium (1), large (50) units of token0 — large exercises tick crossing.
        let sizes = [unit / U256::from(1000u64), unit, unit * U256::from(50u64)];

        let hop = Hop { pool: idx, zero_for_one: true, token_in: meta.token0 };
        // Dynamic-fee pools: do what the live fee poller does — re-read
        // fee() so the quote uses the fee a swap would actually be charged
        // (verify has no poller running; without this, drift since
        // bootstrap shows up as a constant-per-size pseudo-diff).
        if app.engine.fee_dynamic.contains(&idx) {
            if let Ok(f) =
                crate::abi::IUniV3Pool::new(meta.address, &app.http).fee().call().await
            {
                let pips = f.to::<u32>();
                meta.set_fee_pips(pips);
                meta.set_fee_bps(pips / 100);
            }
        }

        for size in sizes {
            if size.is_zero() {
                continue;
            }
            // Model + chain must describe the same block: sync ChainState to
            // the node tip, quote both, retry if the tip moved underneath.
            let mut model = U256::ZERO;
            let mut chain: Option<U256> = None;
            for _ in 0..3 {
                let t0 = app.state.refresh_tip().await.unwrap_or(0);
                app.state.advance_to(t0);
                if let Some(plan) = app.state.plan(idx) {
                    app.state.prefetch(&crate::state::ChainState::plan_slots(plan)).await;
                    app.state.hydrate_cl_ticks_for_pool(plan, app.engine.meta(idx).tick_spacing).await;
                }
                model = math::quote(&app.state, meta, size, true);
                chain = onchain_quote_hop(&app.http, app, &hop, size).await.unwrap_or(None);
                if app.state.refresh_tip().await.unwrap_or(0) == t0 {
                    break;
                }
            }
            let agg = per_dex.entry(meta.dex).or_default();
            agg.pools.insert(i);
            match chain {
                Some(c) => {
                    let d = pct_diff(model, c).abs();
                    // Any divergence is gate-relevant — log it. Sign matters:
                    // model > chain = inflation (danger).
                    if d > 0.0 {
                        tracing::debug!(
                            pool = %meta.address,
                            size = %size,
                            model = %model,
                            chain = %c,
                            diff = format!("{:+.5}%", pct_diff(model, c)),
                            "model vs on-chain quote"
                        );
                    }
                    agg.quotes += 1;
                    agg.sum_diff += d;
                    agg.max_diff = agg.max_diff.max(d);
                }
                _ => agg.unverified += 1,
            }
        }
    }

    println!("  {:<12} {:>6} {:>7} {:>12} {:>12}", "dex", "pools", "quotes", "max_diff", "avg_diff");
    for (tag, a) in &per_dex {
        let avg = if a.quotes > 0 { a.sum_diff / a.quotes as f64 } else { 0.0 };
        println!(
            "  {:<12} {:>6} {:>7} {:>11.6}% {:>11.6}%{}",
            dex_name(*tag),
            a.pools.len(),
            a.quotes,
            a.max_diff,
            avg,
            if a.unverified > 0 { format!("  ({} unverified)", a.unverified) } else { String::new() },
        );
    }

    // --- Direct-read quote timing (ChainState) ---
    println!("\n=== direct-read quote latency (memo only, RPC-backed ChainState; memo-hot = ternary profile) ===");
    let mut tag_sample: FxHashMap<DexTag, PoolIdx> = FxHashMap::default();
    for m in &app.engine.metas {
        tag_sample.entry(m.dex).or_insert(m.idx);
    }
    for (tag, idx) in &tag_sample {
        let meta = app.engine.meta(*idx);
        let size = U256::from(10u64).pow(U256::from(meta.dec0)) / U256::from(1000u64);
        // Cold: first read of each slot goes to MDBX.
        let t = Instant::now();
        let first = math::quote(&app.state, meta, size, true);
        let cold_ns = t.elapsed().as_nanos();
        std::hint::black_box(first);
        // Hot: repeats resolve in the block memo — the profile the 60-iter
        // ternary search actually sees.
        let iters = 50_000u32;
        let t = Instant::now();
        let mut acc = U256::ZERO;
        for _ in 0..iters {
            acc = acc.wrapping_add(math::quote(
                &app.state,
                meta,
                std::hint::black_box(size),
                true,
            ));
        }
        let per = t.elapsed().as_nanos() as f64 / iters as f64;
        std::hint::black_box(acc);
        println!(
            "  {:<12} cold {:>8} ns   hot {:>8.1} ns/quote ({:.2} M/s)",
            dex_name(*tag),
            cold_ns,
            per,
            1e3 / per
        );
    }

    // --- On-chain quote latency (network round-trip) ---
    println!("\n=== on-chain quote latency (eth_call round-trip) ===");
    let mut samples = Vec::new();
    for i in 0..n.min(5) {
        let idx = PoolIdx(i as u32);
        let meta = app.engine.meta(idx);
        let hop = Hop { pool: idx, zero_for_one: true, token_in: meta.token0 };
        let size = U256::from(10u64).pow(U256::from(meta.dec0)) / U256::from(1000u64);
        let t = Instant::now();
        let _ = onchain_quote_hop(&app.http, app, &hop, size).await;
        samples.push(t.elapsed().as_millis());
    }
    if !samples.is_empty() {
        let avg = samples.iter().sum::<u128>() as f64 / samples.len() as f64;
        println!("  avg {avg:.0} ms/quote over {} calls (network-bound)", samples.len());
    }
    println!();
    Ok(())
}

/// Benchmark: re-quote EVERY route in the store, ignoring the ChangedBatch
/// trigger entirely (as if every block had to re-evaluate the whole route
/// universe instead of only the pools a changeset flagged). Mirrors the real
/// evaluator's two-tier shape (`routing/evaluator.rs`: rayon spot-gate, then
/// exact optimize on survivors) so the numbers are directly comparable to
/// what a single trigger costs today.
pub fn bench_route_scan(app: &App) -> Result<()> {
    use rayon::prelude::*;
    use std::time::Instant;

    let routes = &app.store.routes;
    let n = routes.len();
    let anchor_by_token: FxHashMap<Address, &AnchorConfig> =
        app.cfg.anchors.iter().map(|a| (a.token, a)).collect();
    let trade_range = |route: &crate::routing::types::Route| -> (f64, f64) {
        let anchor = anchor_by_token
            .get(&route.anchor)
            .expect("route.anchor must match a configured [[anchor]]");
        (
            anchor.min_trade * 10f64.powi(anchor.decimals as i32),
            anchor.max_trade * 10f64.powi(anchor.decimals as i32),
        )
    };
    let gate = 1.0 + app.cfg.routing.spot_gate_bps / 10_000.0;

    println!("\n=== full route-scan benchmark ({n} routes, NO batch/trigger filter) ===");

    // Tier 1 (spot-product gate), serial baseline vs the real rayon path.
    let t0 = Instant::now();
    let passed_serial =
        routes.iter().filter(|r| optimizer::spot_product(&app.engine, &app.state, r) > gate).count();
    let tier1_serial = t0.elapsed();

    let t0 = Instant::now();
    let passed: Vec<u32> = routes
        .par_iter()
        .filter_map(|r| {
            (optimizer::spot_product(&app.engine, &app.state, r) > gate).then_some(r.id)
        })
        .collect();
    let tier1_parallel = t0.elapsed();

    println!(
        "  tier-1 spot-gate   serial:   {:>8.2} ms  ({:>10.0} routes/s)  -> {passed_serial} passed",
        tier1_serial.as_secs_f64() * 1000.0,
        n as f64 / tier1_serial.as_secs_f64()
    );
    println!(
        "  tier-1 spot-gate   parallel: {:>8.2} ms  ({:>10.0} routes/s)  -> {} passed ({:.2}%)",
        tier1_parallel.as_secs_f64() * 1000.0,
        n as f64 / tier1_parallel.as_secs_f64(),
        passed.len(),
        100.0 * passed.len() as f64 / n as f64
    );

    // Tier 2 (exact ternary-search optimize) on EVERY route — worst case,
    // as if tier-1 never filtered anything.
    let t0 = Instant::now();
    let opps_all_serial = routes
        .iter()
        .filter(|r| {
            let (min_in, max_in) = trade_range(r);
            optimizer::optimize(&app.engine, &app.state, r, min_in, max_in).is_some()
        })
        .count();
    let tier2_all_serial = t0.elapsed();

    let t0 = Instant::now();
    let opps_all_parallel = routes
        .par_iter()
        .filter(|r| {
            let (min_in, max_in) = trade_range(r);
            optimizer::optimize(&app.engine, &app.state, r, min_in, max_in).is_some()
        })
        .count();
    let tier2_all_parallel = t0.elapsed();

    println!(
        "  tier-2 exact-opt   serial,   ALL {n} routes: {:>8.2} ms  ({:>10.0} routes/s)  -> {opps_all_serial} opps",
        tier2_all_serial.as_secs_f64() * 1000.0,
        n as f64 / tier2_all_serial.as_secs_f64()
    );
    println!(
        "  tier-2 exact-opt   parallel, ALL {n} routes: {:>8.2} ms  ({:>10.0} routes/s)  -> {opps_all_parallel} opps",
        tier2_all_parallel.as_secs_f64() * 1000.0,
        n as f64 / tier2_all_parallel.as_secs_f64()
    );

    // Realistic 2-tier pipeline (tier-1 gate -> tier-2 only on survivors),
    // but scanning the WHOLE route universe instead of a trigger-filtered
    // subset — this is the real "what would a full scan cost" number.
    let t0 = Instant::now();
    let opps_realistic = passed
        .par_iter()
        .filter(|&&id| {
            let r = app.store.route(id);
            let (min_in, max_in) = trade_range(r);
            optimizer::optimize(&app.engine, &app.state, r, min_in, max_in).is_some()
        })
        .count();
    let tier2_realistic = t0.elapsed();
    let realistic_total = tier1_parallel + tier2_realistic;

    println!(
        "  realistic (tier1->tier2 on survivors), full scan: {:>8.2} ms total  -> {opps_realistic} opps",
        realistic_total.as_secs_f64() * 1000.0
    );
    println!(
        "\n  for comparison, the trigger-filtered path only re-quotes the handful of\n  routes touching pools a ChangedBatch actually flagged (see `routes_tier1`/\n  `routes_tier2` in the 30s stats log) — not all {n} routes above."
    );

    Ok(())
}

/// Positively verify the quote math: for a sample of pools, quote token0->token1
/// at a small amount with the model (fresh state) and against the on-chain
/// quoter, and report the diff. ~0% diff = math confirmed correct.
async fn verify_pool_sample(app: &App, n: usize) -> Result<()> {
    println!("\n=== positive math check: direct-read model vs on-chain, {} pools ===", n.min(app.engine.len()));
    let mut worst = 0.0f64;
    let mut checked = 0;
    for i in 0..app.engine.len().min(n) {
        let idx = PoolIdx(i as u32);
        let meta = app.engine.meta(idx);
        // Test: swap 0.01 units of token0 -> token1.
        let amount_in = U256::from(10u64).pow(U256::from(meta.dec0)) / U256::from(100u64);
        let hop = Hop {
            pool: idx,
            zero_for_one: true,
            token_in: meta.token0,
        };
        let t0 = app.state.refresh_tip().await.unwrap_or(0);
        app.state.advance_to(t0);
        if let Some(plan) = app.state.plan(idx) {
            app.state.prefetch(&crate::state::ChainState::plan_slots(plan)).await;
            app.state.hydrate_cl_ticks_for_pool(plan, app.engine.meta(idx).tick_spacing).await;
        }
        let model = math::quote(&app.state, app.engine.meta(idx), amount_in, hop.zero_for_one);
        let chain = onchain_quote_hop(&app.http, app, &hop, amount_in).await.unwrap_or(None);
        match (Some(model), chain) {
            (Some(f), Some(c)) => {
                let d = pct_diff(f, c);
                worst = worst.max(d.abs());
                checked += 1;
                println!("  {} [{}]: model={f} chain={c} diff={d:+.5}%", meta.address, dex_name(meta.dex));
            }
            _ => {
                println!("  {} [{}]: unverified (no on-chain quoter)", meta.address, dex_name(meta.dex));
            }
        }
    }
    println!("  worst |diff| over {checked} verified pools: {worst:.5}%  (expect ~0 = rounding)\n");
    Ok(())
}

/// Authoritative on-chain output for one hop. Returns None if this DEX has no
/// wired on-chain quoter (e.g. Pancake/Slipstream V3).
async fn onchain_quote_hop(
    p: &DynProvider,
    app: &App,
    hop: &Hop,
    amount_in: U256,
) -> Result<Option<U256>> {
    let meta = app.engine.meta(hop.pool);
    match meta.dex {
        DexTag::AeroVolatile | DexTag::AeroStable => {
            // Exact on-chain view — the gold standard for Aerodrome.
            let c = IAeroPool::new(meta.address, p);
            let out =
                with_429_backoff(|| async { c.getAmountOut(amount_in, hop.token_in).call().await })
                    .await?;
            Ok(Some(out))
        }
        DexTag::UniV2Fork => {
            // Standard constant product: read fresh reserves, apply fee.
            let c = IUniV2Pair::new(meta.address, p);
            let r = with_429_backoff(|| async { c.getReserves().call().await }).await?;
            let (rin, rout) = if hop.zero_for_one {
                (U256::from(r.reserve0), U256::from(r.reserve1))
            } else {
                (U256::from(r.reserve1), U256::from(r.reserve0))
            };
            Ok(Some(math::v2::get_amount_out(amount_in, rin, rout, meta.fee_bps())))
        }
        DexTag::UniV3Fork | DexTag::PancakeV3 | DexTag::Slipstream | DexTag::Algebra => {
            // Deployless PoolQuoter injected via eth_call state override. It
            // executes the POOL's own swap code (revert-with-amounts in the
            // callback, QuoterV2's internal technique), keyed by pool address
            // — no CREATE2 derivation from a factory, so every V3 fork
            // (Uniswap, Sushi, Pancake, Slipstream, Aerodrome CL a/b) plus
            // Algebra (via algebraSwapCallback) is quotable, at the pool's
            // current (possibly dynamic) fee.
            let q = IPoolQuoter::new(POOL_QUOTER_ADDR, p);
            match with_429_backoff(|| async {
                q.quotePool(meta.address, hop.zero_for_one, amount_in)
                    .state(pool_quoter_override())
                    .call()
                    .await
            })
            .await
            {
                Ok(out) => Ok(Some(out)),
                Err(e) => {
                    tracing::debug!(pool = %meta.address, error = %e, "pool quoter call failed");
                    Ok(None)
                }
            }
        }
        DexTag::UniV4 => {
            // Official V4Quoter (revert-trick over PoolManager.unlock). The
            // PoolKey MUST carry the raw currencies from V4Meta — normalized
            // WETH would quote a different (real) pool.
            let Some(v4) = meta.v4.as_ref() else { return Ok(None) };
            if amount_in > U256::from(u128::MAX) {
                return Ok(None);
            }
            let q = IV4Quoter::new(UNIV4_QUOTER, p);
            let params = IV4Quoter::QuoteExactSingleParams {
                poolKey: IV4Quoter::PoolKey {
                    currency0: v4.currency0,
                    currency1: v4.currency1,
                    fee: alloy::primitives::aliases::U24::from(v4.fee_pips),
                    tickSpacing: alloy::primitives::aliases::I24::try_from(v4.tick_spacing)
                        .expect("tick spacing fits i24"),
                    hooks: alloy::primitives::Address::ZERO,
                },
                zeroForOne: hop.zero_for_one,
                exactAmount: amount_in.to::<u128>(),
                hookData: alloy::primitives::Bytes::new(),
            };
            match with_429_backoff(|| async { q.quoteExactInputSingle(params.clone()).call().await })
                .await
            {
                Ok(r) => Ok(Some(r.amountOut)),
                Err(e) => {
                    tracing::debug!(pool = %meta.address, error = %e, "v4 quoter call failed");
                    Ok(None)
                }
            }
        }
    }
}

/// One-entry state override placing the PoolQuoter runtime code at its
/// synthetic address for the duration of an eth_call.
fn pool_quoter_override() -> alloy::rpc::types::state::StateOverride {
    let code: alloy::primitives::Bytes =
        POOL_QUOTER_CODE.parse().expect("POOL_QUOTER_CODE is valid hex");
    alloy::rpc::types::state::StateOverridesBuilder::default()
        .with_code(POOL_QUOTER_ADDR, code)
        .build()
}

fn dex_name(d: DexTag) -> &'static str {
    match d {
        DexTag::UniV2Fork => "v2",
        DexTag::AeroVolatile => "aero-vol",
        DexTag::AeroStable => "aero-stable",
        DexTag::UniV3Fork => "univ3",
        DexTag::PancakeV3 => "pancakev3",
        DexTag::Slipstream => "slipstream",
        DexTag::UniV4 => "univ4",
        DexTag::Algebra => "algebra",
    }
}

fn pct_diff(model: U256, chain: U256) -> f64 {
    if chain.is_zero() {
        return 0.0;
    }
    let m = crate::math::u256_to_f64(model);
    let c = crate::math::u256_to_f64(chain);
    (m - c) / c * 100.0
}

/// Signed profit as a string ("+123" / "-45").
fn signed(out: U256, input: U256) -> String {
    if out >= input {
        format!("+{}", out - input)
    } else {
        format!("-{}", input - out)
    }
}
