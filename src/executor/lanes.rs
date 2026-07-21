//! Submission lane. `Lane` abstracts "submit a raw signed tx, get back its
//! hash" so the sender can hold multiple lanes and race them — Robinhood
//! Chain has plain FCFS sequencing (no priority-fee ordering), so racing
//! lanes is purely a hedge against per-endpoint latency variance, not a
//! priority mechanism.
//!
//! `RpcLane` submits via a configured provider. Point it at
//! `cfg.rpc.sequencer` or `cfg.rpc.send_rpc`, NOT the local self-hosted
//! node — the local node is started with
//! `--execution.forwarding-target=null` and rejects raw-tx submission.
//!
//! `IsolatedSenderLane` runs an `RpcLane` (warm, keepalive, and every real
//! send) on a dedicated OS thread with its own single-thread tokio runtime —
//! a completely separate scheduler queue and reactor from the ingest
//! pipeline's, so a burst of concurrent prefetch tasks on the main runtime
//! (`ChainState::MAX_CONCURRENT_RPC`) can never delay polling the send
//! future. `submit()` on the main runtime just posts to a channel and
//! awaits a oneshot reply; the actual `eth_sendRawTransaction` always runs
//! on the dedicated thread. `app.rs::spawn_sender` constructs one
//! `IsolatedSenderLane` per configured endpoint and races them.

use alloy::primitives::{Bytes, TxHash};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

#[async_trait]
pub trait Lane: Send + Sync {
    fn name(&self) -> &'static str;
    async fn submit(&self, raw_tx: &Bytes) -> anyhow::Result<TxHash>;
}

/// Submit via a configured provider — see module doc for which endpoints
/// are valid. Normally used only from inside `IsolatedSenderLane`'s
/// dedicated thread, not constructed directly by `app.rs`.
pub struct RpcLane {
    provider: DynProvider,
}

impl RpcLane {
    pub fn new(provider: DynProvider) -> Self {
        Self { provider }
    }

    /// Pre-warm the connection so the FIRST real submission isn't the one
    /// paying the cold-handshake cost.
    pub async fn warm(&self) {
        let _ = self.provider.get_chain_id().await;
    }

    /// Keep the connection hot with a periodic ping. Submissions are sparse
    /// (opportunities are seconds apart, not every ~100ms block), so
    /// without this the pooled connection goes idle between them and each
    /// real send pays a fresh handshake.
    pub fn spawn_keepalive(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                this.warm().await;
            }
        });
    }
}

#[async_trait]
impl Lane for RpcLane {
    fn name(&self) -> &'static str {
        "rpc"
    }

    async fn submit(&self, raw_tx: &Bytes) -> anyhow::Result<TxHash> {
        // Timer is INSIDE the actual HTTP round trip, on the dedicated
        // thread — isolates network/server-side latency from any
        // channel/runtime overhead upstream of this call.
        let t0 = std::time::Instant::now();
        let pending = self.provider.send_raw_transaction(raw_tx).await?;
        tracing::info!(inner_send_us = t0.elapsed().as_micros() as u64, "raw send round trip (dedicated thread)");
        Ok(*pending.tx_hash())
    }
}

type SendJob = (Bytes, oneshot::Sender<anyhow::Result<TxHash>>);

/// Runs an `RpcLane` on a dedicated OS thread with its own single-thread
/// tokio runtime, isolated from the ingest pipeline's ~200-concurrent-
/// connection workload on the main runtime — see module doc. This is the
/// `Lane` `app.rs` should actually construct.
pub struct IsolatedSenderLane {
    name: &'static str,
    tx: mpsc::UnboundedSender<SendJob>,
    // Kept only so the thread isn't detached-and-forgotten from Rust's point
    // of view; never joined (the thread runs for the process's lifetime).
    _thread: std::thread::JoinHandle<()>,
}

impl IsolatedSenderLane {
    /// Spawns the dedicated thread, builds the provider and warms the
    /// connection ON that thread, then returns once ready to accept sends.
    /// `name` identifies this lane in logs — matters once more than one is
    /// racing (`app.rs::spawn_sender`), so `sender.rs`'s "accepted" log can
    /// say which endpoint actually won.
    pub fn spawn(url: String, name: &'static str) -> anyhow::Result<Arc<Self>> {
        let sequencer_url = url;
        let (tx, mut rx) = mpsc::unbounded_channel::<SendJob>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();

        let thread = std::thread::Builder::new().name("seq-sender".into()).spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow::anyhow!("building sender runtime: {e}")));
                    return;
                }
            };
            rt.block_on(async move {
                let url = match sequencer_url.parse() {
                    Ok(u) => u,
                    Err(e) => {
                        let _ = ready_tx.send(Err(anyhow::anyhow!("parsing sequencer url: {e}")));
                        return;
                    }
                };
                let provider: DynProvider = ProviderBuilder::new().connect_http(url).erased();
                let lane = Arc::new(RpcLane::new(provider));
                lane.warm().await;
                lane.spawn_keepalive();
                let _ = ready_tx.send(Ok(()));

                while let Some((raw_tx, resp)) = rx.recv().await {
                    let lane = lane.clone();
                    tokio::spawn(async move {
                        let res = lane.submit(&raw_tx).await;
                        let _ = resp.send(res);
                    });
                }
            });
        })?;

        ready_rx.recv().map_err(|_| anyhow::anyhow!("sender thread died before warming up"))??;
        Ok(Arc::new(Self { name, tx, _thread: thread }))
    }
}

#[async_trait]
impl Lane for IsolatedSenderLane {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn submit(&self, raw_tx: &Bytes) -> anyhow::Result<TxHash> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.tx
            .send((raw_tx.clone(), resp_tx))
            .map_err(|_| anyhow::anyhow!("sender thread gone"))?;
        resp_rx.await.map_err(|_| anyhow::anyhow!("sender thread dropped the response"))?
    }
}

#[cfg(test)]
mod diag {
    use super::*;
    use std::time::Instant;

    /// Fires real `eth_sendRawTransaction` calls (garbage bytes, cheap
    /// rejection) on the same warm/keepalive cadence the real sender uses,
    /// over the actual alloy/reqwest stack, to isolate connection-pooling
    /// behavior from everything else. Run manually: cargo test --release --
    /// --ignored --nocapture probe_keepalive_repro
    #[tokio::test]
    #[ignore]
    async fn probe_keepalive_repro() {
        let url = "https://sequencer.mainnet.chain.robinhood.com".parse().unwrap();
        let provider = ProviderBuilder::new().connect_http(url).erased();
        let lane = Arc::new(RpcLane::new(provider));
        lane.warm().await;
        lane.spawn_keepalive();

        let garbage = Bytes::from(vec![0u8; 200]);
        for i in 0..8 {
            tokio::time::sleep(Duration::from_millis(1300)).await;
            let t0 = Instant::now();
            let res = lane.submit(&garbage).await;
            println!("send #{i}: {:?} -> {:?}", t0.elapsed(), res.err());
        }
    }

    /// Proves `IsolatedSenderLane` holds up under contention: hammers the
    /// LOCAL node with 200 concurrent requests continuously on the MAIN
    /// runtime (mirroring `ChainState::MAX_CONCURRENT_RPC`'s real ingest
    /// workload) while sending real `eth_sendRawTransaction` calls through
    /// the isolated lane, which lives on its own thread/runtime. If
    /// isolation works, send latency should stay ~18-20ms throughout. Run
    /// manually: cargo test --release -- --ignored --nocapture
    /// probe_isolation_under_load
    #[tokio::test]
    #[ignore]
    async fn probe_isolation_under_load() {
        let lane = IsolatedSenderLane::spawn(
            "https://sequencer.mainnet.chain.robinhood.com".to_string(),
            "sequencer",
        )
        .unwrap();

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let stop = stop.clone();
            let local = alloy::providers::ProviderBuilder::new()
                .connect_http("http://127.0.0.1:8547".parse().unwrap())
                .erased();
            tokio::spawn(async move {
                use futures::stream::{self, StreamExt};
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let local = local.clone();
                    stream::iter(0..200)
                        .map(|_| {
                            let local = local.clone();
                            async move {
                                let _ = local.get_block_number().await;
                            }
                        })
                        .buffer_unordered(200)
                        .collect::<Vec<_>>()
                        .await;
                }
            });
        }

        tokio::time::sleep(Duration::from_millis(300)).await;
        let garbage = Bytes::from(vec![0u8; 200]);
        for i in 0..8 {
            let t0 = Instant::now();
            let res = lane.submit(&garbage).await;
            println!("send #{i} under 200-concurrent local load: {:?} -> {:?}", t0.elapsed(), res.err());
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Fires 3 real sends nearly simultaneously through ONE
    /// `IsolatedSenderLane` to check whether concurrent sends serialize on
    /// the dedicated thread rather than running in parallel. Run manually:
    /// cargo test --release -- --ignored --nocapture probe_concurrent_sends
    #[tokio::test]
    #[ignore]
    async fn probe_concurrent_sends() {
        let lane = IsolatedSenderLane::spawn(
            "https://sequencer.mainnet.chain.robinhood.com".to_string(),
            "sequencer",
        )
        .unwrap();
        let garbage = Bytes::from(vec![0u8; 200]);

        let mut handles = Vec::new();
        for i in 0..3 {
            let lane = lane.clone();
            let garbage = garbage.clone();
            handles.push(tokio::spawn(async move {
                let t0 = Instant::now();
                let res = lane.submit(&garbage).await;
                println!("concurrent send #{i}: {:?} -> {:?}", t0.elapsed(), res.err());
            }));
        }
        for h in handles {
            let _ = h.await;
        }
    }

    /// Sanity check for the dual-lane race: both lanes (`sequencer` + `rpc`)
    /// work independently and report their own names. Run manually: cargo
    /// test --release -- --ignored --nocapture probe_two_lane_race
    #[tokio::test]
    #[ignore]
    async fn probe_two_lane_race() {
        let seq = IsolatedSenderLane::spawn(
            "https://sequencer.mainnet.chain.robinhood.com".to_string(),
            "sequencer",
        )
        .unwrap();
        let public = IsolatedSenderLane::spawn(
            "https://rpc.mainnet.chain.robinhood.com".to_string(),
            "rpc",
        )
        .unwrap();
        assert_eq!(seq.name(), "sequencer");
        assert_eq!(public.name(), "rpc");
        let garbage = Bytes::from(vec![0u8; 200]);

        for i in 0..3 {
            let (r1, r2) = tokio::join!(seq.submit(&garbage), public.submit(&garbage));
            println!("round {i}: sequencer_err={:?} public_err={:?}", r1.is_err(), r2.is_err());
        }
    }
}
