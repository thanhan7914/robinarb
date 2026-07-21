//! WS ingest backend: `eth_subscribe` newHeads + logs (filtered by topic0
//! only, with local address/poolId membership check via `FilterSet::resolve`).
//! Resolves each log to a `PoolIdx` inline via `FilterSet::resolve` instead
//! of carrying the raw log downstream. Reconnect tuning below is sized for
//! Robinhood Chain's fast block time (~0.1s) — the head watchdog and
//! backfill gap budget are provisional, re-tune once real uptime data
//! accumulates.
//!
//! Gap handling: the backend tracks the position of the last delivered log
//! (block, log_index). On reconnect it replays the missed range with
//! `eth_getLogs` (address + topic filtered), so a dropped connection loses no
//! events. Only when the gap is too large to replay does it fall back to
//! `Resync` (full pool refresh via the pipeline's prefetch-all path). A
//! newHeads watchdog kills half-open connections that would otherwise hang
//! the stream silently forever.

use super::backend::{FilterSet, IngestBackend, IngestEvent};
use super::multicall;
use alloy::providers::{DynProvider, Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::Filter;
use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Reconnect gaps larger than this are healed by a full Resync instead of
/// log replay — kept conservative until real reconnect-gap behavior is
/// observed, since an oversized backfill range risks running into the
/// free-tier eth_getLogs cap other RPCs impose.
const MAX_BACKFILL_BLOCKS: u64 = 3_000;
/// A stream silent this long is presumed a dead connection. This is a
/// wall-clock budget (RPC jitter/hiccups are a wall-clock phenomenon, not a
/// block-count one), independent of this chain's block time.
const HEAD_WATCHDOG: Duration = Duration::from_secs(15);
/// getLogs range per request during backfill.
const BACKFILL_CHUNK: u64 = 200;

/// (block, log_index) of the last log handed to the pipeline. `log_index ==
/// u64::MAX` marks the whole block as complete (a later head confirmed it).
type Cursor = (u64, u64);

pub struct RpcBackend {
    pub url: String,
}

#[async_trait]
impl IngestBackend for RpcBackend {
    async fn run(
        self: Box<Self>,
        filters: Arc<FilterSet>,
        tx: mpsc::Sender<IngestEvent>,
    ) -> anyhow::Result<()> {
        let mut cursor: Option<Cursor> = None;
        loop {
            if let Err(e) = self.run_once(&filters, &tx, &mut cursor).await {
                tracing::warn!(error = %e, "rpc ingest backend disconnected; reconnecting");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

impl RpcBackend {
    async fn run_once(
        &self,
        filters: &Arc<FilterSet>,
        tx: &mpsc::Sender<IngestEvent>,
        cursor: &mut Option<Cursor>,
    ) -> anyhow::Result<()> {
        let ws = ProviderBuilder::new().connect_ws(WsConnect::new(self.url.clone())).await?;

        // Subscribe FIRST, then backfill up to the current head: logs arriving
        // while we replay sit buffered in the subscription and are deduped by
        // the cursor check below.
        let block_sub = ws.subscribe_blocks().await?;
        let mut block_stream = block_sub.into_stream();
        let log_filter = Filter::new().event_signature(filters.topics.clone());
        let log_sub = ws.subscribe_logs(&log_filter).await?;
        let mut log_stream = log_sub.into_stream();

        if let Some(c) = *cursor {
            let head = ws.get_block_number().await?;
            let erased: DynProvider = ws.clone().erased();
            *cursor = Some(backfill(&erased, filters, tx, c, head).await?);
        } else {
            // FIRST connect: state was hydrated BEFORE this stream existed, so
            // events between the hydration reads and this point were never
            // seen. Request a full prefetch-all now that the stream is live
            // to close that startup gap.
            tracing::info!("first connect: requesting full resync to close the startup gap");
            let _ = tx.send(IngestEvent::Resync).await;
        }
        tracing::info!("rpc ingest subscriptions live");

        let mut head_deadline = tokio::time::Instant::now() + HEAD_WATCHDOG;
        loop {
            tokio::select! {
                maybe_header = block_stream.next() => {
                    let Some(header) = maybe_header else {
                        anyhow::bail!("block stream ended");
                    };
                    head_deadline = tokio::time::Instant::now() + HEAD_WATCHDOG;
                    let complete = (header.number.saturating_sub(1), u64::MAX);
                    if cursor.is_none_or(|c| c < complete) {
                        *cursor = Some(complete);
                    }
                    let ev = IngestEvent::NewHead {
                        number: header.number,
                        timestamp: header.timestamp,
                        base_fee_per_gas: header.base_fee_per_gas.unwrap_or(0) as u128,
                    };
                    if tx.send(ev).await.is_err() {
                        return Ok(());
                    }
                }
                maybe_log = log_stream.next() => {
                    let Some(log) = maybe_log else {
                        anyhow::bail!("log stream ended");
                    };
                    let Some(idx) = filters.resolve(&log) else { continue };
                    let block = log.block_number.unwrap_or(0);
                    let pos = (block, log.log_index.unwrap_or(u64::MAX));
                    if cursor.is_some_and(|c| pos <= c) {
                        continue;
                    }
                    *cursor = Some(pos);
                    let touched_ticks = super::backend::decode_touched_ticks(&log);
                    if tx.send(IngestEvent::PoolLog { block, idx, touched_ticks }).await.is_err() {
                        return Ok(());
                    }
                }
                _ = tokio::time::sleep_until(head_deadline) => {
                    anyhow::bail!("no newHeads for {HEAD_WATCHDOG:?}; connection presumed dead");
                }
            }
        }
    }
}

/// Replay (cursor..=head] through `eth_getLogs`; returns the advanced cursor.
/// Falls back to `Resync` when the gap is too large to replay log-by-log.
async fn backfill(
    provider: &DynProvider,
    filters: &Arc<FilterSet>,
    tx: &mpsc::Sender<IngestEvent>,
    cursor: Cursor,
    head: u64,
) -> anyhow::Result<Cursor> {
    let (cblock, cidx) = cursor;
    let from = if cidx == u64::MAX { cblock + 1 } else { cblock };
    if from > head {
        return Ok(cursor);
    }
    if head - from > MAX_BACKFILL_BLOCKS {
        tracing::warn!(from, head, "gap too large to backfill; requesting full resync");
        let _ = tx.send(IngestEvent::Resync).await;
        return Ok((head, u64::MAX));
    }

    let addresses: Vec<alloy::primitives::Address> = filters.engine.by_address.keys().copied().collect();
    let filter = Filter::new().address(addresses).event_signature(filters.topics.clone());

    let mut replayed = 0usize;
    let mut from_chunk = from;
    while from_chunk <= head {
        let to_chunk = (from_chunk + BACKFILL_CHUNK - 1).min(head);
        let logs = multicall::get_logs_bisect(provider, &filter, from_chunk, to_chunk).await?;
        for log in logs {
            let Some(idx) = filters.resolve(&log) else { continue };
            let block = log.block_number.unwrap_or(0);
            if (block, log.log_index.unwrap_or(u64::MAX)) <= cursor {
                continue;
            }
            let touched_ticks = super::backend::decode_touched_ticks(&log);
            if tx.send(IngestEvent::PoolLog { block, idx, touched_ticks }).await.is_err() {
                return Ok((head, u64::MAX));
            }
            replayed += 1;
        }
        from_chunk = to_chunk + 1;
    }
    if replayed > 0 {
        tracing::info!(from, to = head, logs = replayed, "backfilled missed logs after reconnect");
    }
    Ok((head, u64::MAX))
}
