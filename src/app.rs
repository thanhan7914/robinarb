use crate::config::Config;
use crate::engine::{ChangedBatch, Engine};
use crate::gas::GasStation;
use crate::ingest::slot_layout;
use crate::ingest::{bootstrap, discovery, loader};
use crate::routing::{build_routes, EvaluatedOpportunity, Evaluator, GraphConfig, RouteStore};
use crate::state::{build_slot_plans, ChainState};
use crate::stats::{spawn_stats_loop, Stats};
use alloy::primitives::Address;
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::sol_types::SolEvent;
use anyhow::Result;
use rustc_hash::FxHashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

pub struct App {
    pub cfg: Config,
    pub http: DynProvider,
    pub engine: Arc<Engine>,
    pub state: Arc<ChainState>,
    pub store: Arc<RouteStore>,
    pub gas: Arc<GasStation>,
    pub stats: Arc<Stats>,
    /// WETH-per-anchor exchange rates, fetched once at bootstrap — see `pricing.rs`.
    pub prices: Arc<FxHashMap<Address, f64>>,
}

impl App {
    /// Discover + hydrate + build all state. Does not start ingest.
    pub async fn bootstrap(cfg: Config) -> Result<Self> {
        let http: DynProvider =
            ProviderBuilder::new().connect_http(cfg.rpc.http[0].parse()?).erased();

        let stats = Arc::new(Stats::default());

        // 1. Get the pool set: factory discovery, or a fixed list from pools.toml.
        let disc_provider: DynProvider = if let Some(a) = &cfg.rpc.archive {
            ProviderBuilder::new().connect_http(a.parse()?).erased()
        } else {
            http.clone()
        };
        let pools_file =
            crate::config::read_pools_file(std::path::Path::new(&cfg.discovery.pools_file))?;

        // V4 log demux/backfill key off the canonical PoolManager constant; a
        // kind="v4" [[dex]] pointing anywhere else would discover pools whose
        // events never reach the engine (silently stale state).
        for d in &cfg.dexes {
            if d.kind == "v4" {
                anyhow::ensure!(
                    d.factory == crate::constants::UNIV4_POOL_MANAGER,
                    "[[dex]] {} kind=v4 must use the Robinhood Chain PoolManager {}",
                    d.name,
                    crate::constants::UNIV4_POOL_MANAGER,
                );
            }
        }

        let (discovered, enforce_liquidity) = if cfg.discovery.enabled {
            (discovery::discover_all(&disc_provider, &cfg).await?, true)
        } else {
            anyhow::ensure!(
                !pools_file.pools.is_empty(),
                "discovery.enabled = false but {} has no [[pool]] entries",
                cfg.discovery.pools_file
            );
            tracing::info!(
                pins = pools_file.pools.len(),
                "discovery disabled; loading pools from {}",
                cfg.discovery.pools_file
            );
            (loader::load_pinned_pools(&disc_provider, &cfg, &pools_file.pools).await?, true)
        };

        // 2. Hydrate state + tick tables.
        let registrations =
            bootstrap::hydrate(&disc_provider, &cfg, discovered, enforce_liquidity).await?;
        anyhow::ensure!(!registrations.is_empty(), "no pools survived hydration/filter");

        // 3. Engine (registry only — state lives in ChainState).
        let engine = Arc::new(Engine::new(registrations));

        let dyn_factories: Vec<alloy::primitives::Address> =
            cfg.dexes.iter().filter(|d| d.dynamic_fee).map(|d| d.factory).collect();
        if !dyn_factories.is_empty() {
            for m in &engine.metas {
                if dyn_factories.contains(&m.factory) && !m.dex.is_v4() {
                    engine.fee_dynamic.insert(m.idx);
                }
            }
        }

        // 3b. ChainState: RPC-backed, no local datadir to open. Discover slot
        // layouts, precompute every pool's read recipe, and start with an
        // empty memo — the ingest pipeline's Resync (fired on first connect,
        // see `ingest/rpc_backend.rs`) does the initial prefetch once
        // `run()` wires the ingest channels up. A pool without a plan cannot
        // be quoted at all — fail loudly rather than silently routing a
        // pool with no read recipe.
        let layouts = slot_layout::discover_layouts(&engine, &http).await;
        anyhow::ensure!(!layouts.is_empty(), "no slot layouts discovered; cannot quote");
        let tip = http.get_block_number().await?;
        let mut chain = ChainState::new(http.clone(), tip);
        let plans = build_slot_plans(&engine, &layouts, &http).await;
        let planless: Vec<_> = engine
            .metas
            .iter()
            .filter(|m| plans.get(m.idx.0 as usize).map_or(true, |p| p.is_none()))
            .map(|m| m.address)
            .collect();
        if !planless.is_empty() {
            tracing::warn!(
                count = planless.len(),
                pools = ?planless,
                "pools with no slot plan — unquotable, will never appear in opportunities"
            );
        }
        chain.set_plans(plans);
        let state = Arc::new(chain);

        // 3c. One-time full prefetch: `build_routes` below reads pool
        // liquidity SYNCHRONOUSLY via `ChainState::read`, which is
        // memo-only — see state.rs module doc. Populate the memo for every
        // planned pool
        // BEFORE calling it, or every read comes back zero and route-building
        // silently drops every pool as illiquid. This is bootstrap-only
        // (fine to be slow/async here); the ingest pipeline's own Resync
        // handles the live, ongoing memo population once `run()` starts.
        state.prefetch_all(&engine).await;

        // 3d. One-time WIDE hydration of the persistent tick cache for every
        // CL pool (see state.rs's `hydrate_all_ticks` doc). Runs once here;
        // the live ingest pipeline (`ingest/pipeline.rs`) keeps it correct
        // afterward via targeted `patch_cl_ticks` calls and the tick-resync
        // loop, not by repeating this.
        state.hydrate_all_ticks(&engine).await;

        // 4. Routes — one anchor-rooted cycle enumeration per configured anchor.
        let anchor_tokens: Vec<alloy::primitives::Address> =
            cfg.anchors.iter().map(|a| a.token).collect();
        let gcfg = GraphConfig {
            max_hops: cfg.routing.max_hops,
            max_routes: cfg.routing.max_routes,
            pair_fanout: cfg.routing.pair_fanout,
        };
        let routes = build_routes(&engine, &state, &gcfg, &anchor_tokens);
        let store = Arc::new(RouteStore::build(routes, engine.len()));

        // 4b. Anchor exchange rates (WETH-per-anchor), fetched once from
        // Binance — see pricing.rs.
        let prices = Arc::new(crate::pricing::fetch_anchor_rates(&cfg.anchors).await?);

        // 5. Gas station.
        let gas = Arc::new(GasStation::new(cfg.gas.clone()));
        gas.spawn(http.clone(), Duration::from_secs(2));

        Ok(Self { cfg, http, engine, state, store, gas, stats, prices })
    }

    /// Run the full pipeline until Ctrl-C.
    pub async fn run(self) -> Result<()> {
        let (changed_tx, changed_rx) = mpsc::channel::<ChangedBatch>(1024);
        let (opp_tx, opp_rx) = mpsc::channel::<EvaluatedOpportunity>(256);

        // Ingest: WS-subscribe + eth_getLogs-gap-replay (ingest/rpc_backend.rs)
        // feeding the block-batching pipeline (ingest/pipeline.rs) that owns
        // ChainState's prefetch discipline.
        {
            use crate::ingest::backend::{FilterSet, IngestBackend};
            use crate::ingest::pipeline::{run_pipeline, PipelineConfig};
            use crate::ingest::rpc_backend::RpcBackend;

            let topics = event_topics();
            let filters = Arc::new(FilterSet::new(self.engine.clone(), topics));
            let (ingest_tx, ingest_rx) = mpsc::channel(4096);

            let backend = Box::new(RpcBackend { url: self.cfg.rpc.ws.clone() });
            tokio::spawn(async move {
                let _ = backend.run(filters, ingest_tx).await;
            });

            // Tick-window resync queue: every dirty CL pool (Swap included,
            // not just Mint/Burn/ModifyLiquidity — a swap alone is how a
            // pool's price actually drifts, see state.rs's
            // `run_tick_resync_loop` doc) gets queued here; a separate loop
            // re-centers + re-hydrates ONLY those pools' tick windows,
            // batched, off the ingest hot path.
            let (tick_resync_tx, tick_resync_rx) = mpsc::channel::<crate::engine::PoolIdx>(4096);
            {
                let engine = self.engine.clone();
                let state = self.state.clone();
                tokio::spawn(async move {
                    state
                        .run_tick_resync_loop(&engine, tick_resync_rx, Duration::from_millis(400))
                        .await;
                });
            }

            let engine = self.engine.clone();
            let state = self.state.clone();
            let store = self.store.clone();
            let gas = self.gas.clone();
            let changed_tx = changed_tx.clone();
            tokio::spawn(async move {
                run_pipeline(
                    engine,
                    state,
                    store,
                    ingest_rx,
                    changed_tx,
                    tick_resync_tx,
                    PipelineConfig { quiet_seal_ms: 300 },
                    gas,
                )
                .await;
            });
            tracing::info!("rpc ingest pipeline active");
        }

        // Dynamic-fee tracker: trigger-flagged fast re-reads + 30s sweep.
        {
            let engine = self.engine.clone();
            let state = self.state.clone();
            let store = self.store.clone();
            let provider = self.http.clone();
            let changed_tx = changed_tx.clone();
            tokio::spawn(async move {
                crate::ingest::fees::run_fee_poll_loop(
                    engine,
                    state,
                    store,
                    provider,
                    changed_tx,
                    Duration::from_secs(30),
                )
                .await;
            });
        }

        // Evaluator.
        {
            let evaluator = Evaluator {
                engine: self.engine.clone(),
                state: self.state.clone(),
                store: self.store.clone(),
                gas: self.gas.clone(),
                cfg: self.cfg.routing.clone(),
                anchors: self.cfg.anchors.clone(),
                prices: self.prices.clone(),
                stats: self.stats.clone(),
            };
            tokio::spawn(async move {
                evaluator.run(changed_rx, opp_tx).await;
            });
        }

        // Sink: live sender or paper logger.
        if self.cfg.execution.enabled {
            self.spawn_sender(opp_rx).await?;
        } else {
            spawn_paper_logger(opp_rx, self.engine.clone(), self.store.clone());
        }

        spawn_stats_loop(self.stats.clone(), self.state.clone(), self.engine.len());

        tracing::info!(
            "robinarb running ({} mode)",
            if self.cfg.execution.enabled { "LIVE" } else { "paper" }
        );
        tokio::signal::ctrl_c().await?;
        tracing::info!("shutting down");
        Ok(())
    }

    async fn spawn_sender(&self, opp_rx: mpsc::Receiver<EvaluatedOpportunity>) -> Result<()> {
        use crate::executor::{IsolatedSenderLane, Lane, Sender};

        let seq_lane: Arc<dyn Lane> = IsolatedSenderLane::spawn(self.cfg.rpc.sequencer.clone(), "sequencer")?;
        let public_lane: Arc<dyn Lane> = IsolatedSenderLane::spawn(self.cfg.rpc.send_rpc.clone(), "rpc")?;
        let lanes: Vec<Arc<dyn Lane>> = vec![seq_lane, public_lane];

        let sender = Sender::build(
            self.engine.clone(),
            self.store.clone(),
            self.gas.clone(),
            self.http.clone(),
            &self.cfg,
            lanes,
            self.stats.clone(),
            self.prices.clone(),
        )
        .await?;
        let sender = Arc::new(sender);
        sender.spawn_balance_loop();
        tokio::spawn(async move {
            sender.run(opp_rx).await;
        });
        Ok(())
    }
}

fn event_topics() -> Vec<alloy::primitives::B256> {
    use crate::abi::{AeroEvents, AlgebraEvents, SlipstreamEvents, UniV2Events, UniV3Events, UniV4Events};
    vec![
        UniV2Events::Sync::SIGNATURE_HASH,
        AeroEvents::Sync::SIGNATURE_HASH,
        UniV3Events::Swap::SIGNATURE_HASH,
        UniV3Events::Mint::SIGNATURE_HASH,
        UniV3Events::Burn::SIGNATURE_HASH,
        AlgebraEvents::Swap::SIGNATURE_HASH,
        SlipstreamEvents::PoolCreated::SIGNATURE_HASH,
        UniV4Events::Swap::SIGNATURE_HASH,
        UniV4Events::ModifyLiquidity::SIGNATURE_HASH,
    ]
}

fn spawn_paper_logger(
    mut rx: mpsc::Receiver<EvaluatedOpportunity>,
    engine: Arc<Engine>,
    store: Arc<RouteStore>,
) {
    tokio::spawn(async move {
        let mut file = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("paper_opps.jsonl")
        {
            Ok(f) => Some(f),
            Err(e) => {
                tracing::warn!(error = %e, "cannot open paper_opps.jsonl; console-only");
                None
            }
        };
        while let Some(eval) = rx.recv().await {
            let route = store.route(eval.opp.route_id);
            let path: Vec<String> = route
                .hops
                .iter()
                .map(|h| format!("{:?}", engine.meta(h.pool).address))
                .collect();
            let zfo: Vec<bool> = route.hops.iter().map(|h| h.zero_for_one).collect();
            let v4: Vec<serde_json::Value> = route
                .hops
                .iter()
                .map(|h| match engine.meta(h.pool).v4.as_ref() {
                    Some(v4) => serde_json::json!({
                        "pool_id": format!("{:?}", v4.pool_id),
                        "currency0": format!("{:?}", v4.currency0),
                        "currency1": format!("{:?}", v4.currency1),
                        "fee": v4.fee_pips,
                        "tick_spacing": v4.tick_spacing,
                    }),
                    None => serde_json::Value::Null,
                })
                .collect();
            let record = serde_json::json!({
                "ts": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                "block": eval.block,
                "route_id": eval.opp.route_id,
                "hops": path,
                "zfo": zfo,
                "v4": v4,
                "amount_in": eval.opp.amount_in.to_string(),
                "gross_out": eval.opp.gross_out.to_string(),
                "gross_profit": eval.opp.gross_profit.to_string(),
                "gas_units": eval.gas_units,
                "gas_cost_wei": eval.gas_cost_wei.to_string(),
                "net_profit_wei": eval.net_profit_wei.to_string(),
            });
            tracing::info!(target: "paper", "{}", record);
            if let Some(f) = file.as_mut() {
                use std::io::Write;
                if let Err(e) = writeln!(f, "{record}").and_then(|_| f.flush()) {
                    tracing::warn!(error = %e, "paper_opps.jsonl write failed");
                    file = None;
                }
            }
        }
    });
}

/// Quick connectivity + config check.
pub async fn check_config(cfg: &Config) -> Result<()> {
    let http: DynProvider =
        ProviderBuilder::new().connect_http(cfg.rpc.http[0].parse()?).erased();
    let chain_id = http.get_chain_id().await?;
    anyhow::ensure!(
        chain_id == crate::constants::CHAIN_ID,
        "chain id {chain_id} != {}",
        crate::constants::CHAIN_ID
    );
    let head = http.get_block_number().await?;
    tracing::info!(chain_id, head, dexes = cfg.dexes.len(), "config + http OK");

    let gas = Arc::new(GasStation::new(cfg.gas.clone()));
    gas.spawn(http.clone(), Duration::from_secs(2));
    tokio::time::sleep(Duration::from_secs(4)).await;
    let base = gas.base_fee();
    anyhow::ensure!(base > 0, "gas station has no base fee after 4s");
    let suggested = http.get_gas_price().await.unwrap_or(0);
    let gas_units = 630_000u64;
    let prio = gas.priority_fee(0, gas_units);
    let calldata_len = 200 + 3 * 160; // 3-hop route, mirrors evaluator's estimate
    let l1_fee = gas.estimate_l1_fee(calldata_len);
    let cost = gas.total_gas_cost(gas_units, prio, calldata_len);
    tracing::info!(
        base_fee_gwei = base as f64 / 1e9,
        eth_gas_price_gwei = suggested as f64 / 1e9,
        tip_gwei = prio as f64 / 1e9,
        l1_calldata_fee_eth = l1_fee as f64 / 1e18,
        cost_630k_3hop_eth = cost as f64 / 1e18,
        "gas OK (base fee from header; eth_gasPrice shown only to expose its padding, l1 fee is currently ~0 on this chain)"
    );
    Ok(())
}
