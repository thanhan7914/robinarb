//! Multicall3 batching + chunked/bisecting `eth_getLogs` (fastlane discipline).

use crate::abi::IMulticall3;
use crate::constants::MULTICALL3;
use alloy::primitives::{Address, Bytes};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Filter, Log};
use anyhow::Result;
use std::time::Duration;

/// One aggregated call.
#[derive(Clone)]
pub struct Call {
    pub target: Address,
    pub calldata: Bytes,
}

/// Run `calls` through Multicall3.aggregate3 in batches of `batch`. Failures are
/// allowed (returned as `success=false`).
pub async fn aggregate3(
    provider: &DynProvider,
    calls: &[Call],
    batch: usize,
) -> Result<Vec<IMulticall3::Result>> {
    let mc = IMulticall3::new(MULTICALL3, provider);
    let mut out = Vec::with_capacity(calls.len());
    for chunk in calls.chunks(batch.max(1)) {
        let call3: Vec<IMulticall3::Call3> = chunk
            .iter()
            .map(|c| IMulticall3::Call3 {
                target: c.target,
                allowFailure: true,
                callData: c.calldata.clone(),
            })
            .collect();
        // Retry with backoff to ride out transient rate limits (429) on weak RPCs.
        let mut backoff = 400u64;
        let mut attempt = 0;
        let results = loop {
            match mc.aggregate3(call3.clone()).call().await {
                Ok(r) => break r,
                Err(e) => {
                    attempt += 1;
                    if attempt >= 5 {
                        return Err(e.into());
                    }
                    tokio::time::sleep(Duration::from_millis(backoff)).await;
                    backoff *= 2;
                }
            }
        };
        out.extend(results);
    }
    Ok(out)
}


/// Fetch [from, to], bisecting the range on RPC error (range too large / too many
/// results) down to single blocks.
pub fn get_logs_bisect<'a>(
    provider: &'a DynProvider,
    filter: &'a Filter,
    from: u64,
    to: u64,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Log>>> + Send + 'a>> {
    Box::pin(async move {
        let f = filter.clone().from_block(from).to_block(to);
        match retry_get_logs(provider, &f).await {
            Ok(logs) => Ok(logs),
            Err(e) => {
                if from >= to {
                    return Err(e);
                }
                let mid = from + (to - from) / 2;
                let mut left = get_logs_bisect(provider, filter, from, mid).await?;
                let right = get_logs_bisect(provider, filter, mid + 1, to).await?;
                left.extend(right);
                Ok(left)
            }
        }
    })
}

async fn retry_get_logs(provider: &DynProvider, filter: &Filter) -> Result<Vec<Log>> {
    let mut backoff = 500u64;
    let mut attempt = 0;
    loop {
        match provider.get_logs(filter).await {
            Ok(logs) => return Ok(logs),
            Err(e) => {
                attempt += 1;
                if attempt >= 3 {
                    return Err(e.into());
                }
                tokio::time::sleep(Duration::from_millis(backoff)).await;
                backoff *= 2;
            }
        }
    }
}
