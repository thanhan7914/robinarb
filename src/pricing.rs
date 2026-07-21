//! Anchor exchange rates (WETH-per-anchor), fetched once at bootstrap from
//! Binance's public spot ticker API — no on-chain oracle/quoting, no API key
//! (unauthenticated `ticker/price` endpoint). The only consumer is the
//! evaluator's profit gate (`routing/evaluator.rs`), which needs to convert
//! the always-ETH-denominated gas cost into a non-WETH anchor's own units.
//! Anchor prices vs WETH barely move within one bot run, so a single startup
//! fetch (not a periodic refresh loop) is enough — see the plan's design
//! notes if that assumption ever needs revisiting.
//!
//! A DexScreener-based version of this was tried first and dropped: its
//! `priceUsd` field is the PAIR's `baseToken` price, not necessarily the
//! token we asked for (whichever side of the deepest pair our token landed
//! on), and it silently returned a wildly wrong rate (~2x on ETH, picked a
//! pair where WETH was the quote side). A real exchange's own ticker doesn't
//! have that ambiguity.

use crate::config::AnchorConfig;
use crate::constants::{USDG, WETH};
use alloy::primitives::Address;
use anyhow::{Context, Result};
use rustc_hash::FxHashMap;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct BinanceTicker {
    price: String,
}

/// Binance.US does NOT list any USDG pair (`exchangeInfo` has zero symbols
/// containing "USDG"). USDG (Paxos' Global Dollar) isn't on Binance, but IS
/// on CoinGecko's free public API — use that instead for tokens Binance
/// doesn't carry, rather than assuming a peg.
fn binance_symbol(token: Address) -> Option<&'static str> {
    if token == WETH {
        Some("ETHUSDT")
    } else {
        None
    }
}

/// CoinGecko coin id, for tokens with no Binance ticker.
fn coingecko_id(token: Address) -> Option<&'static str> {
    if token == USDG {
        Some("global-dollar")
    } else {
        None
    }
}

/// USD price via Binance.US's public (no API key) spot ticker. `api.binance
/// .com` itself geo-blocks US-based requests ("restricted location" per its
/// ToS) — this VPS hits that block, so use the Binance.US endpoint instead
/// (same ticker schema, same underlying market data).
async fn fetch_binance_usd_price(client: &reqwest::Client, symbol: &str) -> Result<f64> {
    let url = format!("https://api.binance.us/api/v3/ticker/price?symbol={symbol}");
    let ticker: BinanceTicker = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("binance request failed for {symbol}"))?
        .json()
        .await
        .with_context(|| format!("binance response parse failed for {symbol}"))?;
    ticker
        .price
        .parse()
        .with_context(|| format!("binance price not numeric for {symbol}: {}", ticker.price))
}

/// USD price via CoinGecko's free public `simple/price` endpoint (no API
/// key). Real market price, not an assumed peg — a depegged stablecoin
/// shows up here as a real deviation from 1.0, which the caller should feed
/// straight into the profit-gate math rather than silently rounding to $1.
async fn fetch_coingecko_usd_price(client: &reqwest::Client, coin_id: &str) -> Result<f64> {
    let url = format!("https://api.coingecko.com/api/v3/simple/price?ids={coin_id}&vs_currencies=usd");
    let resp: serde_json::Value = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("coingecko request failed for {coin_id}"))?
        .json()
        .await
        .with_context(|| format!("coingecko response parse failed for {coin_id}"))?;
    resp[coin_id]["usd"]
        .as_f64()
        .with_context(|| format!("coingecko response missing usd price for {coin_id}: {resp}"))
}

/// Dispatch to whichever source has this token, in preference order
/// Binance -> CoinGecko. Fails loudly (no fallback to an assumed price) if
/// neither has it.
async fn fetch_usd_price(client: &reqwest::Client, token: Address) -> Result<f64> {
    if let Some(symbol) = binance_symbol(token) {
        return fetch_binance_usd_price(client, symbol).await;
    }
    if let Some(coin_id) = coingecko_id(token) {
        return fetch_coingecko_usd_price(client, coin_id).await;
    }
    anyhow::bail!("no price source (Binance or CoinGecko) known for token {token}");
}

/// WETH amount equal to 1 whole unit of each anchor token (WETH itself maps
/// to 1.0). Fetched once, eagerly, at bootstrap — a failure here fails
/// bootstrap loudly rather than starting the bot with a wrong/default rate
/// silently baked into every non-WETH profit-gate decision.
pub async fn fetch_anchor_rates(anchors: &[AnchorConfig]) -> Result<FxHashMap<Address, f64>> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; robinarb/1.0)")
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("building reqwest client")?;

    let mut weth_usd: Option<f64> = None;
    let mut rates = FxHashMap::default();
    for anchor in anchors {
        if anchor.token == WETH {
            rates.insert(anchor.token, 1.0);
            continue;
        }
        if weth_usd.is_none() {
            weth_usd = Some(fetch_usd_price(&client, WETH).await?);
        }
        let anchor_usd = fetch_usd_price(&client, anchor.token)
            .await
            .with_context(|| format!("anchor {} has no known price source", anchor.symbol))?;
        anyhow::ensure!(anchor_usd > 0.0, "anchor {} price <= 0", anchor.symbol);
        // WETH equal to 1 whole anchor token, e.g. anchor_usd=$1 (USDG),
        // weth_usd=$3000 -> rate ~= 0.000333 WETH per USDG.
        let rate = anchor_usd / weth_usd.expect("set above");
        tracing::info!(anchor = %anchor.symbol, weth_per_unit = rate, "anchor price fetched");
        rates.insert(anchor.token, rate);
    }
    Ok(rates)
}
