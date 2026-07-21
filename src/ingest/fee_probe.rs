//! Fee-on-transfer / rebase token detection. Some tokens skim a percentage
//! on `transfer`, which the constant-product math never accounts for —
//! every route through them reverts on-chain at the NEXT hop with "transfer
//! amount exceeds balance". This probes every newly-seen non-anchor token
//! once, caches the verdict, and feeds positives into the same blacklist
//! `bootstrap::apply_filter` already enforces — no route is ever built
//! through a token confirmed to short its transfers.
//!
//! Detection method: inject `contracts/src/FeeProbe.sol` via eth_call state
//! override at a REAL address already holding a real balance of the token —
//! one of that token's own discovered pools (or, for V4, the PoolManager
//! singleton that holds all V4 liquidity). Only the account's CODE is
//! overridden; its storage — hence its real token balance — is untouched, so
//! `transfer` runs with genuine funds against the token's real, unmodified
//! logic. This is the same override-injection technique `verify.rs` already
//! uses for `PoolQuoter`, just addressed at a real holder instead of a fixed
//! synthetic one.
//!
//! Two subtleties the probe has to account for:
//! 1. Tax tokens commonly exempt their OWN liquidity pool from the fee (so
//!    AMM math isn't corrupted) — probing FROM that exact pool alone gives a
//!    false negative even though the SAME token taxes every ordinary
//!    address, including our arb contract. Handled by chaining a second hop
//!    through `relay` (an ordinary, never-exempted address) before measuring.
//! 2. A token can have MULTIPLE pools, and exemption lists are per-address,
//!    not per-token — one pool can be exempted while another isn't. Probing
//!    only the first-seen pool can miss the tax entirely. Handled by
//!    probing up to `MAX_HOLDERS_PER_TOKEN` distinct pools and flagging the
//!    token bad if ANY of them reveals a shortfall — a route can land on any
//!    of them, so the worst case is the only one that matters.

use crate::abi::IFeeProbe;
use crate::config::AnchorConfig;
use crate::constants::{FEE_PROBE_CODE, UNIV4_POOL_MANAGER};
use crate::ingest::discovery::DiscoveredPool;
use alloy::primitives::{address, Address, U256};
use alloy::providers::DynProvider;
use alloy::rpc::types::state::StateOverridesBuilder;
use anyhow::Context;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Ordinary, never-exempted second hop — see module doc point 1.
const RELAY: Address = address!("00000000000000000000000000000000EE1A4001");
/// Synthetic transfer target. Never read as an absolute balance — every
/// probe measures the delta around its own `transfer`, so any pre-existing
/// balance here (e.g. dust from an unrelated real token) is harmless.
const DUMMY_RECIPIENT: Address = address!("00000000000000000000000000000000C0FFEE01");

/// Distinct pools tried per token before giving up — see module doc point 2.
const MAX_HOLDERS_PER_TOKEN: usize = 3;

/// Bound on concurrent probe eth_calls — each is a real state-overridden
/// call, heavier than a plain storage read, so kept well under
/// `ChainState::MAX_CONCURRENT_RPC`.
const MAX_CONCURRENT_PROBES: usize = 50;

/// Fraction-of-amount tolerance before a shortfall counts as a real fee
/// (1 bp) — wide enough to absorb any incidental integer-division dust from
/// share-based accounting, far tighter than the ~1% fees actually observed.
const TOLERANCE_BPS: u64 = 1;

type Cache = HashMap<String, bool>;

fn cache_path(dir: &str) -> PathBuf {
    PathBuf::from(dir).join("fee_probe.json")
}

fn load_cache(dir: &str) -> Cache {
    std::fs::read_to_string(cache_path(dir))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_cache(dir: &str, cache: &Cache) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).ok();
    let p = cache_path(dir);
    std::fs::write(&p, serde_json::to_string(cache)?)
        .with_context(|| format!("writing cache {}", p.display()))?;
    Ok(())
}

/// Probe every non-anchor token touched by `discovered`, skipping ones
/// already resolved in the on-disk cache. Returns the set confirmed
/// fee-on-transfer (or otherwise short their transfers) — union this into
/// `FilterConfig::token_blacklist` before `apply_filter` runs.
pub async fn detect(
    provider: &DynProvider,
    discovered: &[DiscoveredPool],
    decimals: &HashMap<Address, u8>,
    anchors: &[AnchorConfig],
    cache_dir: &str,
) -> HashSet<Address> {
    use futures::stream::{self, StreamExt};

    let anchor_set: HashSet<Address> = anchors.iter().map(|a| a.token).collect();

    // Up to MAX_HOLDERS_PER_TOKEN distinct real holders per non-anchor token
    // — different pools for the same token can have different exemption
    // status, so one is not enough (see module doc point 2). V4-only tokens
    // only ever have one real holder: the PoolManager singleton.
    let mut holders: HashMap<Address, Vec<Address>> = HashMap::new();
    for p in discovered {
        for t in [p.token0, p.token1] {
            if anchor_set.contains(&t) {
                continue;
            }
            let list = holders.entry(t).or_default();
            let candidate = if p.dex.is_v4() { UNIV4_POOL_MANAGER } else { p.address };
            if !list.contains(&candidate) && list.len() < MAX_HOLDERS_PER_TOKEN {
                list.push(candidate);
            }
        }
    }

    let mut cache = load_cache(cache_dir);
    let to_probe: Vec<(Address, Vec<Address>)> =
        holders.into_iter().filter(|(t, _)| !cache.contains_key(&t.to_string())).collect();

    if !to_probe.is_empty() {
        tracing::info!(count = to_probe.len(), "probing newly-seen tokens for transfer fees");
    }

    let mut results = stream::iter(to_probe.into_iter().map(|(token, pool_holders)| {
        let provider = provider.clone();
        let dec = decimals.get(&token).copied().unwrap_or(18);
        async move {
            let verdict = probe_token(&provider, token, &pool_holders, dec).await;
            (token, verdict)
        }
    }))
    .buffer_unordered(MAX_CONCURRENT_PROBES);

    let mut newly_bad = 0u32;
    while let Some((token, verdict)) = results.next().await {
        match verdict {
            Some(is_fee) => {
                cache.insert(token.to_string(), is_fee);
                if is_fee {
                    newly_bad += 1;
                    tracing::warn!(%token, "transfer shorts the sent amount; blacklisting");
                }
            }
            None => {
                // Inconclusive on every candidate holder (e.g. none had
                // enough real balance to test) — leave unresolved, retried
                // next run.
            }
        }
    }
    if newly_bad > 0 {
        tracing::warn!(count = newly_bad, "new fee-on-transfer tokens detected this run");
    }
    if let Err(e) = save_cache(cache_dir, &cache) {
        tracing::warn!(error = %e, "failed to persist fee-probe cache");
    }

    cache.into_iter().filter(|(_, bad)| *bad).filter_map(|(t, _)| t.parse().ok()).collect()
}

/// Tries every candidate holder in turn; `Some(true)` as soon as one reveals
/// a shortfall (worst case wins — a route can land on any of them). `None`
/// only if every candidate was inconclusive.
async fn probe_token(
    provider: &DynProvider,
    token: Address,
    pool_holders: &[Address],
    decimals: u8,
) -> Option<bool> {
    let mut any_conclusive = false;
    for &holder in pool_holders {
        match probe_one(provider, token, holder, decimals).await {
            Ok(Some(true)) => return Some(true),
            Ok(Some(false)) => any_conclusive = true,
            Ok(None) | Err(_) => {}
        }
    }
    any_conclusive.then_some(false)
}

/// `None` = inconclusive (holder didn't actually have `amount` to give, or
/// the call reverted for some other reason) — never treated as a verdict.
/// Two hops (`holder` -> `RELAY` -> dummy) so a token that exempts `holder`
/// itself (e.g. a pool on its own fee-exemption list) still reveals the fee
/// it charges everyone else — see module doc point 1.
async fn probe_one(
    provider: &DynProvider,
    token: Address,
    holder: Address,
    decimals: u8,
) -> anyhow::Result<Option<bool>> {
    let scale = U256::from(10u128).pow(U256::from(decimals.min(18) as u32));
    let amount = (scale / U256::from(1000)).max(U256::from(1));

    let code: alloy::primitives::Bytes = FEE_PROBE_CODE.parse().expect("FEE_PROBE_CODE is valid hex");
    let overrides = StateOverridesBuilder::default().with_code(holder, code.clone()).with_code(RELAY, code).build();

    let probe = IFeeProbe::new(holder, provider);
    let received =
        match probe.probe(token, RELAY, DUMMY_RECIPIENT, amount).state(overrides).call().await {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };

    let min_ok = amount - (amount * U256::from(TOLERANCE_BPS) / U256::from(10_000));
    Ok(Some(received < min_ok))
}
