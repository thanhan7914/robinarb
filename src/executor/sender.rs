//! Build + sign the EIP-1559 tx and fan it out to lanes. Maintains a per-route
//! revert blacklist.

use super::encoder;
use super::lanes::Lane;
use super::nonce::NonceManager;
use crate::abi::IERC20;
use crate::config::{AnchorConfig, Config, ExecutionConfig};
use crate::constants::CHAIN_ID;
use crate::engine::Engine;
use crate::gas::GasStation;
use crate::routing::{EvaluatedOpportunity, RouteStore};
use crate::stats::Stats;
use alloy::consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy::eips::eip2718::Encodable2718;
use alloy::network::TxSignerSync;
use alloy::primitives::{Address, TxKind, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::signers::local::PrivateKeySigner;
use dashmap::DashSet;
use rustc_hash::FxHashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

pub struct Sender {
    pub engine: Arc<Engine>,
    pub store: Arc<RouteStore>,
    pub gas: Arc<GasStation>,
    pub provider: DynProvider,
    pub signer: PrivateKeySigner,
    pub executor: Address,
    pub nonce: Arc<NonceManager>,
    pub lanes: Vec<Arc<dyn Lane>>,
    pub cfg: ExecutionConfig,
    pub anchors: Vec<AnchorConfig>,
    /// WETH-per-anchor exchange rates from `pricing.rs` — only used to make
    /// `log_result`'s realized-profit-vs-gas-cost comparison apples-to-apples
    /// for a non-WETH anchor (gas is always paid in ETH).
    pub prices: Arc<FxHashMap<Address, f64>>,
    pub stats: Arc<Stats>,
    /// route id -> block until which it's suppressed.
    blacklist: Mutex<FxHashMap<u32, u64>>,
    /// Routes with a tx currently in flight (submitted, receipt not yet
    /// seen). The pending fast-path re-surfaces the SAME opp every ~50ms
    /// while a revert receipt takes ~2s to arrive, so without this the same
    /// dead route gets sent several times, burning gas on each. Cleared when
    /// `submit` returns (receipt seen, timed out, or all lanes rejected).
    in_flight: DashSet<u32>,
    /// Executor balance per anchor token, kept fresh by `spawn_balance_loop`
    /// so the submit hot path reads it lock-free instead of doing an eth_call
    /// per opp.
    own_balances: arc_swap::ArcSwap<FxHashMap<Address, U256>>,
}

impl Sender {
    pub async fn build(
        engine: Arc<Engine>,
        store: Arc<RouteStore>,
        gas: Arc<GasStation>,
        provider: DynProvider,
        cfg: &Config,
        lanes: Vec<Arc<dyn Lane>>,
        stats: Arc<Stats>,
        prices: Arc<FxHashMap<Address, f64>>,
    ) -> anyhow::Result<Self> {
        let key = std::env::var(&cfg.wallet.key_env)
            .map_err(|_| anyhow::anyhow!("missing env {}", cfg.wallet.key_env))?;
        let signer: PrivateKeySigner = key.parse()?;
        let executor = cfg
            .wallet
            .executor_contract
            .ok_or_else(|| anyhow::anyhow!("wallet.executor_contract not set"))?;
        let nonce = Arc::new(NonceManager::new(provider.clone(), signer.address()).await?);
        let mut own0: FxHashMap<Address, U256> = FxHashMap::default();
        for a in &cfg.anchors {
            let bal = IERC20::new(a.token, &provider)
                .balanceOf(executor)
                .call()
                .await
                .unwrap_or(U256::ZERO);
            own0.insert(a.token, bal);
        }
        Ok(Self {
            engine,
            store,
            gas,
            provider,
            signer,
            executor,
            nonce,
            lanes,
            cfg: cfg.execution.clone(),
            anchors: cfg.anchors.clone(),
            prices,
            stats,
            blacklist: Mutex::new(FxHashMap::default()),
            in_flight: DashSet::new(),
            own_balances: arc_swap::ArcSwap::from_pointee(own0),
        })
    }

    pub async fn run(self: Arc<Self>, mut rx: mpsc::Receiver<EvaluatedOpportunity>) {
        while let Some(eval) = rx.recv().await {
            // Blacklist check.
            {
                let bl = self.blacklist.lock().await;
                if let Some(until) = bl.get(&eval.opp.route_id) {
                    if eval.block < *until {
                        continue;
                    }
                }
            }
            // In-flight dedup: don't fire the same route again while a tx for
            // it is still pending (insert returns false if already present).
            if !self.in_flight.insert(eval.opp.route_id) {
                continue;
            }
            let this = self.clone();
            tokio::spawn(async move {
                let route_id = eval.opp.route_id;
                if let Err(e) = this.submit(eval).await {
                    tracing::warn!(error = %e, "submit failed");
                }
                this.in_flight.remove(&route_id);
            });
        }
    }

    /// Background refresher for the executor's per-anchor balances, so the
    /// submit hot path never blocks on RPC. One tick per Base block; also
    /// re-fetched after every receipt (the balance just changed).
    pub fn spawn_balance_loop(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
                this.refresh_own_balances().await;
            }
        });
    }

    async fn refresh_own_balances(&self) {
        let mut updated: FxHashMap<Address, U256> = (**self.own_balances.load()).clone();
        for a in &self.anchors {
            if let Ok(bal) = IERC20::new(a.token, &self.provider).balanceOf(self.executor).call().await {
                updated.insert(a.token, bal);
            }
        }
        self.own_balances.store(std::sync::Arc::new(updated));
    }

    async fn submit(&self, eval: EvaluatedOpportunity) -> anyhow::Result<()> {
        // Pre-sign staleness gate: if the opp out-aged the pipeline, its
        // pricing state is dead and sending only burns gas on a guaranteed
        // revert — opportunities on this chain die within ~1 block.
        if self.cfg.stale_presign_ms > 0
            && eval.detected_at.elapsed().as_millis() as u64 > self.cfg.stale_presign_ms
        {
            Stats::bump(&self.stats.opps_dropped_presign);
            return Ok(());
        }

        // Everything from here to the lane fan-out must stay RPC-free and
        // log-free: the opp dies within ~1 block, so any pre-send latency
        // is paid in missed wins.
        let anchor = self.store.route(eval.opp.route_id).anchor;
        let own = self.own_balances.load().get(&anchor).copied().unwrap_or(U256::ZERO);
        let (calldata, use_flashloan) =
            encoder::encode(&self.engine, &self.store, &eval, own, self.cfg.use_flashloan);

        let base_fee = self.gas.base_fee();
        let max_fee = base_fee * 2 + eval.priority_fee_wei;

        let mut tx = TxEip1559 {
            chain_id: CHAIN_ID,
            nonce: self.nonce.reserve(),
            gas_limit: self.cfg.gas_limit,
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: eval.priority_fee_wei,
            to: TxKind::Call(self.executor),
            value: U256::ZERO,
            access_list: Default::default(),
            input: calldata,
        };

        let sig = self.signer.sign_transaction_sync(&mut tx)?;
        let signed = tx.into_signed(sig);
        let envelope = TxEnvelope::Eip1559(signed);
        let raw = envelope.encoded_2718();
        let raw = alloy::primitives::Bytes::from(raw);

        let signed_us = eval.detected_at.elapsed().as_micros() as u64;

        // Fan out to all lanes concurrently — bytes go on the wire FIRST;
        // logging and counters wait until the sends are in flight. Each lane
        // task stamps detect→lane-ack with the shared anchor.
        let t0 = eval.detected_at;
        let mut handles = Vec::new();
        for lane in &self.lanes {
            let lane = lane.clone();
            let raw = raw.clone();
            handles.push(tokio::spawn(async move {
                let res = lane.submit(&raw).await;
                (lane.name(), res, t0.elapsed().as_micros() as u64)
            }));
        }
        let wire_us = eval.detected_at.elapsed().as_micros() as u64;

        tracing::info!(
            route = eval.opp.route_id,
            net = eval.net_profit_wei,
            amount_in = %eval.opp.amount_in,
            flashloan = use_flashloan,
            signed_us,
            wire_us,
            "submitting arb"
        );
        Stats::bump(&self.stats.txs_sent);
        let mut any_ok = None;
        let mut accept_us: Option<u64> = None;
        for h in handles {
            if let Ok((name, res, done_us)) = h.await {
                match res {
                    Ok(hash) => {
                        tracing::info!(lane = name, %hash, detect_to_ack_us = done_us, "accepted");
                        any_ok = Some(hash);
                        accept_us = Some(accept_us.map_or(done_us, |b| b.min(done_us)));
                    }
                    Err(e) => tracing::debug!(lane = name, error = %e, "lane rejected"),
                }
            }
        }

        match any_ok {
            Some(hash) => {
                let timing = serde_json::json!({
                    "signed_us": signed_us,
                    "wire_us": wire_us,
                    "lane_ack_us": accept_us,
                });
                self.monitor(hash, eval, use_flashloan, timing).await;
            }
            None => {
                // All lanes rejected — resync nonce in case of gap.
                let _ = self.nonce.resync().await;
            }
        }
        Ok(())
    }

    async fn monitor(
        &self,
        hash: alloy::primitives::TxHash,
        eval: EvaluatedOpportunity,
        use_flashloan: bool,
        timing: serde_json::Value,
    ) {
        for _ in 0..10 {
            tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
            // Raw JSON receipt (not the typed Ethereum receipt) so any
            // chain-specific extra fields survive. `r["l1Fee"]` below reads
            // as 0 on Robinhood Chain (no such field on Arbitrum receipts,
            // and this chain's perL1CalldataUnit is currently 0 anyway) —
            // harmless, kept only so `gas_cost`'s shape stays consistent for
            // the log-parsing tooling that reads `live_results.jsonl`.
            let receipt: Option<serde_json::Value> = self
                .provider
                .raw_request("eth_getTransactionReceipt".into(), (hash,))
                .await
                .ok()
                .filter(|v: &serde_json::Value| !v.is_null());
            let Some(r) = receipt else { continue };

            let status = r["status"].as_str() == Some("0x1");
            if status {
                Stats::bump(&self.stats.txs_landed);
            } else {
                Stats::bump(&self.stats.txs_reverted);
                tracing::warn!(%hash, "reverted; blacklisting route");
                let mut bl = self.blacklist.lock().await;
                bl.insert(eval.opp.route_id, eval.block + self.cfg.blacklist_blocks);
            }
            // Balance just changed (profit in, or gas-only on revert of a
            // self-funded leg) — refresh the cache out-of-band.
            self.refresh_own_balances().await;
            self.log_result(hash, &eval, use_flashloan, Some(&r), status, &timing);
            return;
        }
        tracing::warn!(%hash, "receipt not seen in monitor window");
        self.log_result(hash, &eval, use_flashloan, None, false, &timing);
    }

    /// Append the realized-vs-predicted record for one submitted tx to
    /// `live_results.jsonl` (the live counterpart of `paper_opps.jsonl`).
    fn log_result(
        &self,
        hash: alloy::primitives::TxHash,
        eval: &EvaluatedOpportunity,
        use_flashloan: bool,
        receipt: Option<&serde_json::Value>,
        status: bool,
        timing: &serde_json::Value,
    ) {
        let anchor = self.store.route(eval.opp.route_id).anchor;
        let realized = receipt.map(|r| {
            let gas_used = hex_u128(&r["gasUsed"]);
            let gas_price = hex_u128(&r["effectiveGasPrice"]);
            let l1_fee = hex_u128(&r["l1Fee"]);
            let gas_cost = gas_used * gas_price + l1_fee;
            // Realized anchor-token profit = sum of Transfers of `anchor` into
            // the executor minus those out of it. Flashloan borrow/repay legs
            // cancel out. Gas itself is always paid in ETH, never `anchor` —
            // convert to ETH-wei (via the bootstrap-fetched rate) before
            // netting against gas_cost so the two are comparable; identity
            // for the WETH anchor.
            let anchor_delta = anchor_delta_from_logs(&r["logs"], anchor, self.executor);
            let anchor_delta_eth_wei = self.anchor_delta_to_weth_wei(anchor, anchor_delta);
            let net = anchor_delta_eth_wei - gas_cost as i128;
            serde_json::json!({
                "block": hex_u128(&r["blockNumber"]),
                "gas_used": gas_used,
                "effective_gas_price": gas_price,
                "l1_fee_wei": l1_fee.to_string(),
                "gas_cost_wei": gas_cost.to_string(),
                "anchor_delta": anchor_delta.to_string(),
                "net_profit_eth_wei": net.to_string(),
            })
        });
        let hops: Vec<String> = self
            .store
            .route(eval.opp.route_id)
            .hops
            .iter()
            .map(|h| format!("{:?}", self.engine.meta(h.pool).address))
            .collect();
        let record = serde_json::json!({
            "ts": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            "block": eval.block,
            "route_id": eval.opp.route_id,
            "hops": hops,
            "tx_hash": format!("{hash:?}"),
            "status": receipt.map(|_| if status { "landed" } else { "reverted" }).unwrap_or("unknown"),
            "flashloan": use_flashloan,
            "timing": timing,
            "predicted": {
                "amount_in": eval.opp.amount_in.to_string(),
                "gross_profit": eval.opp.gross_profit.to_string(),
                "gas_units": eval.gas_units,
                "gas_cost_wei": eval.gas_cost_wei.to_string(),
                "net_profit_wei": eval.net_profit_wei.to_string(),
                "priority_fee_wei": eval.priority_fee_wei.to_string(),
            },
            "realized": realized,
        });
        tracing::info!(target: "live", "{}", record);
        let write = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("live_results.jsonl")
            .and_then(|mut f| {
                use std::io::Write;
                writeln!(f, "{record}")
            });
        if let Err(e) = write {
            tracing::warn!(error = %e, "live_results.jsonl write failed");
        }
    }

    /// Signed `amount` (in `anchor`'s smallest unit) -> ETH-wei, via the
    /// bootstrap-fetched WETH-per-anchor rate. Identity for the WETH anchor.
    fn anchor_delta_to_weth_wei(&self, anchor: Address, amount: i128) -> i128 {
        let Some(a) = self.anchors.iter().find(|a| a.token == anchor) else { return amount };
        let Some(&rate) = self.prices.get(&anchor) else { return amount };
        let sign = if amount < 0 { -1i128 } else { 1i128 };
        let scaled = (amount.unsigned_abs() as f64 / 10f64.powi(a.decimals as i32) * rate * 1e18) as i128;
        sign * scaled
    }
}

/// Parse a 0x-hex JSON quantity; absent/malformed fields become 0.
fn hex_u128(v: &serde_json::Value) -> u128 {
    v.as_str()
        .and_then(|s| u128::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0)
}

/// Net `token` moved into `account` across a receipt's logs (smallest unit, signed).
fn anchor_delta_from_logs(logs: &serde_json::Value, token: Address, account: Address) -> i128 {
    // keccak256("Transfer(address,address,uint256)")
    const TRANSFER_TOPIC: &str =
        "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
    let token = format!("{token:?}").to_lowercase();
    let acc_topic = format!("0x{:0>64}", format!("{account:?}")[2..].to_lowercase());
    let mut delta = 0i128;
    let Some(logs) = logs.as_array() else { return 0 };
    for log in logs {
        if log["address"].as_str().map(str::to_lowercase).as_deref() != Some(token.as_str()) {
            continue;
        }
        let topics = log["topics"].as_array();
        let Some([t0, from, to]) = topics.and_then(|t| t.get(0..3)).and_then(|t| <&[_; 3]>::try_from(t).ok()) else {
            continue;
        };
        if t0.as_str() != Some(TRANSFER_TOPIC) {
            continue;
        }
        let amount = log["data"]
            .as_str()
            .and_then(|s| u128::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            .unwrap_or(0) as i128;
        if to.as_str().map(str::to_lowercase).as_deref() == Some(acc_topic.as_str()) {
            delta += amount;
        }
        if from.as_str().map(str::to_lowercase).as_deref() == Some(acc_topic.as_str()) {
            delta -= amount;
        }
    }
    delta
}
