//! config.toml + pools.toml parsing.

use alloy::primitives::Address;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub rpc: RpcConfig,
    pub wallet: WalletConfig,
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    #[serde(default)]
    pub filter: FilterConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub gas: GasConfig,
    #[serde(default)]
    pub execution: ExecutionConfig,
    #[serde(rename = "dex")]
    pub dexes: Vec<DexConfig>,
    #[serde(rename = "anchor")]
    pub anchors: Vec<AnchorConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcConfig {
    pub http: Vec<String>,
    pub ws: String,
    pub archive: Option<String>,
    #[serde(default = "default_sequencer")]
    pub sequencer: String,
    #[serde(default = "default_send_rpc")]
    pub send_rpc: String,
}

fn default_send_rpc() -> String {
    "https://rpc.mainnet.chain.robinhood.com".to_string()
}

fn default_sequencer() -> String {
    "https://sequencer.mainnet.chain.robinhood.com".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct WalletConfig {
    /// Deployed ArbExecutor contract. Zero address = paper-only.
    #[serde(default)]
    pub executor_contract: Option<Address>,
    #[serde(default = "default_key_env")]
    pub key_env: String,
}

fn default_key_env() -> String {
    "PRIVATE_KEY".to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DiscoveryConfig {
    /// false = skip factory scan and load ONLY the pools listed in `pools_file`
    /// (fast path for testing).
    pub enabled: bool,
    /// Pin/override list; also the sole source when `enabled = false`.
    pub pools_file: String,
    pub cache_dir: String,
    /// eth_getLogs chunk for factory PoolCreated scans.
    pub getlogs_chunk: u64,
    /// eth_getLogs chunk for the Mint/Burn tick backfill (no address filter — keep small).
    pub tick_sync_chunk: u64,
    /// Max concurrent getLogs requests.
    pub concurrency: usize,
    /// Block where scanning starts if no cache exists (0 = Base genesis).
    pub start_block: u64,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            pools_file: "pools.toml".to_string(),
            cache_dir: "cache".to_string(),
            getlogs_chunk: 10_000,
            tick_sync_chunk: 1_500,
            concurrency: 8,
            start_block: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FilterConfig {
    /// If non-empty, only pools whose BOTH tokens are listed here (plus any
    /// anchor token) are kept.
    pub token_whitelist: Vec<Address>,
    pub token_blacklist: Vec<Address>,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self { token_whitelist: vec![], token_blacklist: vec![] }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    pub max_hops: usize,
    pub max_routes: usize,
    /// Per token pair keep at most this many pools (by liquidity) when building the graph.
    pub pair_fanout: usize,
    /// Drop ChangedBatch older than this.
    pub drop_ms: u64,
    /// Tier-1 gate: product of spot rates must exceed 1 + this many bps.
    pub spot_gate_bps: f64,
    /// Max routes surviving tier 1 that go to exact evaluation, per batch.
    pub tier2_top_n: usize,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            max_hops: 3,
            max_routes: 100_000,
            pair_fanout: 6,
            drop_ms: 800,
            spot_gate_bps: 5.0,
            tier2_top_n: 200,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GasConfig {
    /// Fraction of net profit to spend on priority fee. Robinhood Chain's
    /// sequencer is FCFS — tips do not buy ordering — so this defaults to 0
    /// (tip stays at the min_prio_gwei floor). Only raise it if that
    /// assumption changes.
    pub prio_fraction: f64,
    pub min_prio_gwei: f64,
    pub max_prio_gwei: f64,
    /// Safety multiplier on the L1 data fee estimate.
    pub l1_fee_margin: f64,
}

impl Default for GasConfig {
    fn default() -> Self {
        Self { prio_fraction: 0.0, min_prio_gwei: 0.001, max_prio_gwei: 0.005, l1_fee_margin: 1.2 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ExecutionConfig {
    /// false = paper mode: log opportunities as JSON lines, never send.
    pub enabled: bool,
    pub use_flashloan: bool,
    /// Suppress a route for this many blocks after a revert.
    pub blacklist_blocks: u64,
    pub gas_limit: u64,
    /// Drop an opportunity if it's older than this by the time the sender is
    /// about to sign it (measured from batch seal). Robinhood Chain's block
    /// time is ~0.1s, so opportunities die within ~1 block — retune this
    /// against real opportunity lifetimes as they're observed. 0 disables
    /// the gate.
    pub stale_presign_ms: u64,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            use_flashloan: true,
            blacklist_blocks: 50,
            gas_limit: 2_000_000,
            stale_presign_ms: 750,
        }
    }
}

/// A base token routes can cycle out from and back to. Every numeric
/// threshold here is in THIS token's own units — 1 WETH, 1 USDC (6 decimals),
/// and 1 cbBTC (8 decimals) are not interchangeable, so unlike the old
/// single-anchor config these can't share one global set of thresholds.
#[derive(Debug, Clone, Deserialize)]
pub struct AnchorConfig {
    pub symbol: String,
    pub token: Address,
    pub decimals: u8,
    /// Working-set gate: pool must hold at least this much of the anchor
    /// token (whole units, not wei) on the anchor's side to be watched.
    pub min_liquidity: f64,
    /// Minimum net profit, in the anchor's smallest unit, to act.
    pub min_profit: String,
    /// Max trade size in whole anchor units (flashloan cap).
    pub max_trade: f64,
    /// Min trade size in whole anchor units for the optimizer search range.
    pub min_trade: f64,
}

impl AnchorConfig {
    pub fn min_profit(&self) -> alloy::primitives::U256 {
        alloy::primitives::U256::from_str_radix(&self.min_profit, 10)
            .unwrap_or_else(|_| panic!("anchor {}: min_profit must be a decimal integer", self.symbol))
    }
}

/// One DEX (factory) entry. `kind` selects protocol semantics.
#[derive(Debug, Clone, Deserialize)]
pub struct DexConfig {
    pub name: String,
    /// "v2" | "v3" | "v4" | "pancake_v3" — plus "aero"/"slipstream"/"algebra"
    /// kept parseable but unused on Robinhood Chain (no confirmed venue of
    /// those kinds yet).
    pub kind: String,
    pub factory: Address,
    /// V2 forks only: LP fee in basis points (30 = 0.30%). Aero reads factory.getFee.
    #[serde(default)]
    pub fee_bps: Option<u32>,
    /// Block the factory was deployed at (discovery scan start).
    #[serde(default)]
    pub deploy_block: Option<u64>,
    /// Pool fee re-prices on-chain over time (Aerodrome's newer CL factories
    /// adjust it with volatility — measured 255→165 pips in minutes). Enables
    /// the periodic fee() poller for this factory's pools.
    #[serde(default)]
    pub dynamic_fee: bool,
}

/// Optional pin/override list. Everything else is hydrated on-chain.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PoolsFile {
    #[serde(rename = "pool", default)]
    pub pools: Vec<PoolPin>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PoolPin {
    /// Pool contract address — every DEX except Uniswap V4 (whose pools have
    /// no address). Exactly one of `address` / `pool_id` / the full-key fields
    /// must identify the pool; the loader rejects ambiguous pins.
    #[serde(default)]
    pub address: Option<Address>,
    /// Must match a [[dex]] name.
    pub dex: String,
    /// V4 pin by poolId (resolved against the V4 discovery cache, which holds
    /// the PoolKey preimage).
    #[serde(default)]
    pub pool_id: Option<alloy::primitives::B256>,
    /// V4 pin by full PoolKey (hand-pinning; hooks is implicitly 0). The
    /// loader derives the poolId locally — the key IS the identity — and
    /// truth-probes liveness via StateView.getSlot0.
    #[serde(default)]
    pub currency0: Option<Address>,
    #[serde(default)]
    pub currency1: Option<Address>,
    /// Fee in pips (e.g. 500 = 0.05%).
    #[serde(default)]
    pub fee: Option<u32>,
    #[serde(default)]
    pub tick_spacing: Option<i32>,
}

pub fn read_config(path: &Path) -> Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let cfg: Config = toml::from_str(&raw).context("parsing config.toml")?;
    for required in [(crate::constants::WETH, "WETH"), (crate::constants::USDG, "USDG")] {
        anyhow::ensure!(
            cfg.anchors.iter().any(|a| a.token == required.0),
            "config.toml: missing required [[anchor]] entry for {} ({})",
            required.1,
            required.0
        );
    }
    Ok(cfg)
}

pub fn read_pools_file(path: &Path) -> Result<PoolsFile> {
    if !path.exists() {
        return Ok(PoolsFile::default());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading pools file {}", path.display()))?;
    let pf: PoolsFile = toml::from_str(&raw).context("parsing pools.toml")?;
    Ok(pf)
}

