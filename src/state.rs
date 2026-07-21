//! ChainState — the single state source for all quote math, RPC-backed.
//!
//! There is no local datastore to read from directly; all state comes from
//! RPC reads, which are async I/O and can't be called from the sync math hot
//! path. So `read()` is split from fetching:
//!   - `read()` stays SYNC and reads ONLY the per-block memo (a plain
//!     HashMap lookup, no I/O) — this is what the math/routing hot path
//!     calls.
//!   - `prefetch()` is an async step that batch-fetches every (addr, slot)
//!     a `ChangedBatch` of pools will need, via concurrent `eth_getStorageAt`
//!     calls, and populates the memo BEFORE the evaluator touches those
//!     pools. The ingest pipeline is responsible for calling `prefetch()` on
//!     every pool it flags, ahead of emitting the `ChangedBatch` — a pool
//!     whose slots were never prefetched reads as zero from `read()`
//!     (fail-safe: quotes zero, never looks profitable, never causes a bad
//!     trade).
//!
//! `prefetch()` currently fires N concurrent `eth_getStorageAt` calls rather
//! than a single JSON-RPC batch request — functionally fine (all requests in
//! flight simultaneously), but a real JSON-RPC batch transport would be a
//! worthwhile follow-up once the concrete provider type is settled.

use std::sync::atomic::{AtomicU64, Ordering};

use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::providers::{DynProvider, Provider};
use dashmap::DashMap;
use rustc_hash::{FxBuildHasher, FxHashMap};

use crate::constants::UNIV4_POOL_MANAGER;
use crate::engine::{DexTag, Engine, PoolIdx};
use crate::ingest::slot_layout::{self, SlotLayout};

/// Uniswap V4 `StateLibrary.sol`'s `POOLS_SLOT` — base slot of
/// `mapping(PoolId => Pool.State) pools` in `PoolManager`. Protocol-level
/// constant — verify bit-for-bit against `IStateView.getSlot0`/`getLiquidity`
/// on Robinhood Chain's own PoolManager before trusting in production.
pub const V4_POOLS_SLOT: u64 = 6;
/// `Pool.State` field offsets from the pool's state slot (slot0 is +0).
pub const V4_LIQUIDITY_OFFSET: u64 = 3;
pub const V4_TICKS_OFFSET: u64 = 4;
pub const V4_TICK_BITMAP_OFFSET: u64 = 5;

/// `_getPoolStateSlot`: `keccak256(abi.encodePacked(poolId, POOLS_SLOT))`.
pub fn v4_state_slot(pool_id: B256) -> U256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(pool_id.as_slice());
    buf[32..].copy_from_slice(&U256::from(V4_POOLS_SLOT).to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(buf).0)
}

/// How a CL pool's per-tick info word is laid out (see `SlotLayout` docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickCodec {
    /// V3/V4: `liquidityNet` = high 128 (signed) bits of the SAME word as
    /// `liquidityGross` (mapping_slot + 0).
    Packed,
    /// Algebra: `liquidityDelta` = low 128 (signed) bits of mapping_slot+1.
    TwoWord,
}

/// Where a CL pool's quote fee comes from.
#[derive(Debug, Clone, Copy)]
pub enum ClFee {
    /// `meta.fee_pips` — static forks (set once at hydration) and dynamic
    /// pools kept fresh by the fee poller.
    Meta,
    /// Decoded from the globalState word every quote already reads
    /// (Algebra `lastFee`, only for pools where a probe proved
    /// `fee() == lastFee` — a dynamic-fee plugin breaks that).
    Word { shift: u32 },
}

/// Per-pool read recipe, precomputed once at bootstrap: absolute slots +
/// decode dispatch, so the hot path does no layout lookups and no repeated
/// keccak for V4 state slots.
#[derive(Debug, Clone)]
pub enum SlotPlan {
    /// UniV2-style packed reserves (`reserve0 u112 | reserve1 u112 | ts`).
    V2Packed { addr: Address, slot: U256 },
    /// Solidly-style: two consecutive full-word reserve slots.
    V2TwoSlot { addr: Address, r0: U256 },
    /// V3 / Pancake / Algebra / V4.
    Cl {
        /// Pool address, or the PoolManager singleton for V4.
        addr: Address,
        slot0: U256,
        liquidity: U256,
        /// Bit offset of liquidity inside its word (0 except Algebra).
        liquidity_shift: u32,
        ticks_base: U256,
        bitmap_base: U256,
        tick_codec: TickCodec,
        fee: ClFee,
    },
}

pub struct ChainState {
    provider: DynProvider,
    /// Sealed tip, advanced by the ingest pipeline.
    tip: AtomicU64,
    /// Per-block memo of RPC reads (slot0/liquidity/reserves); cleared on
    /// every tip advance — these change on every swap, so "fresh every
    /// block" is the only correct policy for them.
    memo: DashMap<(Address, U256), U256, FxBuildHasher>,
    /// Persistent tick-bitmap/tick-net cache — NEVER cleared by `advance_to`.
    /// Unlike slot0/liquidity, a tick's net-liquidity only changes on a
    /// Mint/Burn/ModifyLiquidity touching that specific tick, not on every
    /// swap — so this is hydrated wide ONCE (`hydrate_all_ticks`, bootstrap)
    /// and kept correct via targeted `patch_cl_ticks` calls when the ingest
    /// pipeline decodes a liquidity-changing event (`ingest/rpc_backend.rs`),
    /// plus `run_tick_resync_loop` re-centering a pool's window when its
    /// price drifts (see that function's doc).
    tick_cache: DashMap<(Address, U256), U256, FxBuildHasher>,
    /// Per-pool read recipes, indexed by `PoolIdx`.
    plans: Vec<Option<SlotPlan>>,
    // Soak observability: n_rpc = a prefetch miss that had to be fetched
    // inline (went to RPC), ideally near-zero once the ingest pipeline's
    // prefetch discipline is right.
    n_rpc: AtomicU64,
    n_memo: AtomicU64,
}

impl ChainState {
    /// No datadir to open — RPC-backed, so this just wraps the provider.
    /// `plans` filled in later via `set_plans` (layout discovery needs RPC,
    /// which needs the app wired).
    pub fn new(provider: DynProvider, tip: u64) -> Self {
        Self {
            provider,
            tip: AtomicU64::new(tip),
            memo: DashMap::with_hasher(FxBuildHasher::default()),
            tick_cache: DashMap::with_hasher(FxBuildHasher::default()),
            plans: Vec::new(),
            n_rpc: AtomicU64::new(0),
            n_memo: AtomicU64::new(0),
        }
    }

    pub fn set_plans(&mut self, plans: Vec<Option<SlotPlan>>) {
        self.plans = plans;
    }

    #[inline]
    pub fn plan(&self, idx: PoolIdx) -> Option<&SlotPlan> {
        self.plans.get(idx.0 as usize).and_then(|p| p.as_ref())
    }

    #[inline]
    pub fn tip(&self) -> u64 {
        self.tip.load(Ordering::Relaxed)
    }

    /// Poll the RPC for the current head block number. Used by verify.rs's
    /// tooling to detect "did the chain move while I was reading" — a plain
    /// `eth_blockNumber` call.
    pub async fn refresh_tip(&self) -> anyhow::Result<u64> {
        Ok(self.provider.get_block_number().await?)
    }

    /// THE quote-path read: memo (block-scoped) then tick_cache (persistent),
    /// sync, no I/O — see module doc. A slot that was never prefetched into
    /// either reads as zero (fail-safe: an unquotable pool never looks
    /// profitable, never causes a bad trade). A given (addr, slot) is only ever written
    /// to ONE of the two maps in practice (base slots vs. keccak-derived
    /// tick/bitmap slots are disjoint by construction), so checking both
    /// costs nothing extra in the common case.
    #[inline]
    pub fn read(&self, addr: Address, slot: U256) -> U256 {
        if let Some(v) = self.memo.get(&(addr, slot)) {
            self.n_memo.fetch_add(1, Ordering::Relaxed);
            return *v;
        }
        if let Some(v) = self.tick_cache.get(&(addr, slot)) {
            self.n_memo.fetch_add(1, Ordering::Relaxed);
            return *v;
        }
        U256::ZERO
    }

    /// Same lookup as `read`, but restricted to `tick_cache` and reporting
    /// whether the slot was ever actually hydrated — `None` means "never
    /// fetched", distinct from `Some(U256::ZERO)` ("fetched, genuinely
    /// zero"). Only bitmap-word reads need this distinction (see `read`'s
    /// doc); tick nets are always hydrated in the same pass as the bitmap
    /// word that marks them initialized, so they don't need the check.
    #[inline]
    pub fn read_tick_cache_checked(&self, addr: Address, slot: U256) -> Option<U256> {
        self.tick_cache.get(&(addr, slot)).map(|v| *v)
    }

    pub fn read_counters(&self) -> (u64, u64) {
        (self.n_rpc.load(Ordering::Relaxed), self.n_memo.load(Ordering::Relaxed))
    }

    /// Batch-fetch every (addr, slot) pair via concurrent `eth_getStorageAt`
    /// at `latest` and populate the memo. Call this for every pool a
    /// `ChangedBatch` is about to flag, BEFORE sending the batch downstream —
    /// the evaluator's `read()` calls are sync and only ever see what's
    /// already here.
    pub async fn prefetch(&self, pairs: &[(Address, U256)]) {
        self.prefetch_into(pairs, false).await;
    }

    /// Same fetch mechanics as `prefetch`, but writes into `tick_cache`
    /// (persistent) instead of `memo` (block-scoped).
    pub async fn prefetch_ticks(&self, pairs: &[(Address, U256)]) {
        self.prefetch_into(pairs, true).await;
    }

    /// Concurrency cap for `prefetch_into`. An unbounded `join_all` over a
    /// large batch (the bootstrap-time wide tick hydration fires ~3M
    /// requests across ~6k CL pools) opens far too many simultaneous TCP
    /// connections to the local node and effectively hangs — there's no such
    /// thing as unlimited concurrent connections even against localhost.
    /// This bounds in-flight requests to a number a single local node
    /// handles comfortably, while still getting the "fire many at once, not
    /// serially" latency win the batching is for.
    const MAX_CONCURRENT_RPC: usize = 200;

    async fn prefetch_into(&self, pairs: &[(Address, U256)], into_tick_cache: bool) {
        use futures::stream::{self, StreamExt};

        let mut results = stream::iter(pairs.iter().copied().map(|(addr, slot)| {
            let provider = self.provider.clone();
            async move {
                let v = provider.get_storage_at(addr, slot).await;
                (addr, slot, v)
            }
        }))
        .buffer_unordered(Self::MAX_CONCURRENT_RPC);

        while let Some((addr, slot, res)) = results.next().await {
            match res {
                Ok(v) => {
                    self.n_rpc.fetch_add(1, Ordering::Relaxed);
                    if into_tick_cache {
                        self.tick_cache.insert((addr, slot), v);
                    } else {
                        self.memo.insert((addr, slot), v);
                    }
                }
                Err(e) => {
                    tracing::warn!(%addr, %slot, error = %e, "prefetch eth_getStorageAt failed; slot stays unset (reads as zero)");
                }
            }
        }
    }

    /// Bitmap-word window (each side of a pool's current tick) hydrated ONCE
    /// at bootstrap into the persistent `tick_cache`. 64 is a deliberate
    /// cost/coverage compromise, not a correctness guarantee: wider coverage
    /// costs proportionally more `eth_getStorageAt` calls across every CL
    /// pool at bootstrap (thousands of pools), so full-range hydration isn't
    /// worth the one-time cost. A quote for a tick outside the hydrated
    /// range reads zero and under-quotes rather than erroring — retune
    /// wider (accepting a slower bootstrap) if real trade sizes need it.
    const BOOTSTRAP_TICK_WORD_RANGE: i16 = 64;
    /// Narrower window used by `patch_cl_ticks`'s bitmap re-check around a
    /// SPECIFIC tick a Mint/Burn/ModifyLiquidity event touched — the touched
    /// tick's own word plus immediate neighbors, not a full re-scan.
    const PATCH_WORD_RANGE: i16 = 1;

    /// One-time, bootstrap-only wide hydration of the persistent tick cache
    /// for every CL pool. Batches ALL pools' bitmap words into one
    /// `prefetch_ticks` call, then ALL pools' initialized-tick-net slots
    /// into a second one. Call this once, after `set_plans`, before `run()`
    /// starts ingest.
    pub async fn hydrate_all_ticks(&self, engine: &Engine) {
        let plans: Vec<(&SlotPlan, i32)> = engine
            .metas
            .iter()
            .filter_map(|m| {
                let plan = self.plan(m.idx)?;
                matches!(plan, SlotPlan::Cl { .. }).then_some((plan, m.tick_spacing))
            })
            .collect();
        self.prefetch_cl_tick_windows(&plans, Self::BOOTSTRAP_TICK_WORD_RANGE).await;
    }

    /// Drains `rx` and, for each distinct pool that had a real tick-changing
    /// event since the last drain, re-centers + re-hydrates ONLY that pool's
    /// tick window around its CURRENT price. Runs forever, batching arrivals
    /// within `batch_window` so repeated triggers for the same pool collapse
    /// into one re-hydration instead of one per event.
    pub async fn run_tick_resync_loop(
        &self,
        engine: &Engine,
        mut rx: tokio::sync::mpsc::Receiver<PoolIdx>,
        batch_window: std::time::Duration,
    ) {
        let mut pending: rustc_hash::FxHashSet<PoolIdx> = rustc_hash::FxHashSet::default();
        loop {
            let got_one = if pending.is_empty() {
                rx.recv().await
            } else {
                match tokio::time::timeout(batch_window, rx.recv()).await {
                    Ok(v) => v,
                    Err(_) => None, // timed out with a non-empty batch -- flush it below
                }
            };
            match got_one {
                Some(idx) => {
                    pending.insert(idx);
                    continue;
                }
                None if pending.is_empty() => return, // channel closed, nothing left to flush
                None => {}                             // batch_window elapsed -- flush
            }

            let batch: Vec<PoolIdx> = pending.drain().collect();
            tracing::debug!(pools = batch.len(), "tick resync: re-centering window for changed pools");
            use futures::stream::{self, StreamExt};
            stream::iter(batch.into_iter().filter_map(|idx| {
                let plan = self.plan(idx)?;
                let spacing = engine.meta(idx).tick_spacing;
                Some(async move { self.hydrate_cl_ticks_for_pool(plan, spacing).await })
            }))
            .buffer_unordered(16)
            .collect::<Vec<()>>()
            .await;
        }
    }

    /// Single-pool wide hydration — for `verify.rs`'s CLI tooling, which
    /// checks pools one at a time with no ingest pipeline running to keep
    /// `tick_cache` warm via `hydrate_all_ticks`/`patch_cl_ticks`. Not for
    /// the hot path (see `prefetch_neighborhood`'s doc for why).
    pub async fn hydrate_cl_ticks_for_pool(&self, plan: &SlotPlan, tick_spacing: i32) {
        self.prefetch_cl_tick_windows(&[(plan, tick_spacing)], Self::BOOTSTRAP_TICK_WORD_RANGE).await;
    }

    /// Only handles `TickCodec::Packed` (V3-family: the only CL kind
    /// actually configured today — univ3/univ4/pancakev3). `TickCodec::TwoWord`
    /// (Algebra) uses a different raw-tick word index with no `tick_spacing`
    /// division and is intentionally a no-op here — Algebra has no confirmed
    /// venue on this chain yet, so this is dead code same as the `kind`
    /// itself, not a silent gap in something actually in use.
    async fn prefetch_cl_tick_windows(&self, plans: &[(&SlotPlan, i32)], word_range: i16) {
        struct PoolWindow {
            addr: Address,
            ticks_base: U256,
            bitmap_base: U256,
            spacing: i32,
            lo: i16,
            hi: i16,
        }

        let mut windows = Vec::new();
        let mut bitmap_slots = Vec::new();
        for &(plan, tick_spacing) in plans {
            let SlotPlan::Cl { addr, slot0, ticks_base, bitmap_base, tick_codec: TickCodec::Packed, .. } =
                plan
            else {
                continue;
            };
            let word0 = self.read(*addr, *slot0);
            let (_, tick) = crate::ingest::slot_layout::decode_v3_slot0(word0);
            let spacing = tick_spacing.max(1);
            let center_word = (crate::ingest::slot_layout::compress_tick(tick, spacing) >> 8) as i16;
            let lo = center_word.saturating_sub(word_range);
            let hi = center_word.saturating_add(word_range);
            for w in lo..=hi {
                bitmap_slots
                    .push((*addr, crate::ingest::slot_layout::mapping_slot_signed(w as i64, *bitmap_base)));
            }
            windows.push(PoolWindow { addr: *addr, ticks_base: *ticks_base, bitmap_base: *bitmap_base, spacing, lo, hi });
        }
        if bitmap_slots.is_empty() {
            return;
        }
        self.prefetch_ticks(&bitmap_slots).await;

        let mut tick_slots = Vec::new();
        for pw in &windows {
            for w in pw.lo..=pw.hi {
                let bits =
                    self.read(pw.addr, crate::ingest::slot_layout::mapping_slot_signed(w as i64, pw.bitmap_base));
                if bits.is_zero() {
                    continue;
                }
                for bit in 0..256u32 {
                    if bits.bit(bit as usize) {
                        let raw_tick = (w as i32 * 256 + bit as i32) * pw.spacing;
                        tick_slots.push((
                            pw.addr,
                            crate::ingest::slot_layout::mapping_slot_signed(raw_tick as i64, pw.ticks_base),
                        ));
                    }
                }
            }
        }
        if !tick_slots.is_empty() {
            self.prefetch_ticks(&tick_slots).await;
        }
    }

    /// Incremental update: a Mint/Burn/ModifyLiquidity event told us exactly
    /// which ticks changed (decoded in `ingest/rpc_backend.rs`, carried by
    /// `IngestEvent::PoolLog::touched_ticks`) — re-fetch just THOSE ticks'
    /// net-liquidity slots plus their bitmap word (liquidityGross crossing
    /// zero flips the bitmap bit), instead of a blind window re-scan. This
    /// is what keeps `tick_cache` correct after the one-time bootstrap
    /// hydration, at a cost of a handful of slots per event rather than a
    /// whole window.
    pub async fn patch_cl_ticks(
        &self,
        addr: Address,
        ticks_base: U256,
        bitmap_base: U256,
        tick_spacing: i32,
        ticks: &[i32],
    ) {
        if ticks.is_empty() {
            return;
        }
        let spacing = tick_spacing.max(1);
        let mut bitmap_slots = Vec::new();
        let mut words = Vec::new();
        for &t in ticks {
            let word = (crate::ingest::slot_layout::compress_tick(t, spacing) >> 8) as i16;
            for w in (word.saturating_sub(Self::PATCH_WORD_RANGE))..=(word.saturating_add(Self::PATCH_WORD_RANGE)) {
                bitmap_slots.push((addr, crate::ingest::slot_layout::mapping_slot_signed(w as i64, bitmap_base)));
                words.push(w);
            }
        }
        self.prefetch_ticks(&bitmap_slots).await;

        // The touched ticks themselves always get a fresh tick-net read
        // (a Mint/Burn on tickLower/tickUpper always rewrites that tick's
        // info word, whether or not its bitmap bit flipped).
        let mut tick_slots: Vec<(Address, U256)> = ticks
            .iter()
            .map(|&t| (addr, crate::ingest::slot_layout::mapping_slot_signed(t as i64, ticks_base)))
            .collect();
        // Also re-scan the immediate bitmap neighborhood for any OTHER tick
        // whose initialized bit flipped as a side effect (rare, but a Mint's
        // tickLower/tickUpper aren't the only bits in their word).
        for &w in &words {
            let bits = self.read(addr, crate::ingest::slot_layout::mapping_slot_signed(w as i64, bitmap_base));
            if bits.is_zero() {
                continue;
            }
            for bit in 0..256u32 {
                if bits.bit(bit as usize) {
                    let raw_tick = (w as i32 * 256 + bit as i32) * spacing;
                    tick_slots.push((addr, crate::ingest::slot_layout::mapping_slot_signed(raw_tick as i64, ticks_base)));
                }
            }
        }
        self.prefetch_ticks(&tick_slots).await;
    }

    /// Prefetch every planned pool's slots. Used at bootstrap (before
    /// `build_routes` reads liquidity) and by `verify.rs`'s CLI tooling
    /// (which has no ingest pipeline running to keep the memo populated) —
    /// NOT for the hot path, this is an unconditional full scan.
    pub async fn prefetch_all(&self, engine: &Engine) {
        let mut slots = Vec::new();
        for m in &engine.metas {
            if let Some(plan) = self.plan(m.idx) {
                slots.extend(Self::plan_slots(plan));
            }
        }
        self.prefetch(&slots).await;
    }

    /// Every (addr, slot) pair `plan` needs for a bare quote (not full tick
    /// walk — bitmap/tick words are fetched on demand by `ClTickReader`
    /// through the SAME prefetch discipline, see `ingest/pipeline.rs`).
    pub fn plan_slots(plan: &SlotPlan) -> Vec<(Address, U256)> {
        match plan {
            SlotPlan::V2Packed { addr, slot } => vec![(*addr, *slot)],
            SlotPlan::V2TwoSlot { addr, r0 } => vec![(*addr, *r0), (*addr, *r0 + U256::from(1u8))],
            SlotPlan::Cl { addr, slot0, liquidity, .. } => vec![(*addr, *slot0), (*addr, *liquidity)],
        }
    }

    /// Expand `dirty` to the full set of pools any route touching them also
    /// spans (a route's `quote()` reads every hop, not just the changed one
    /// -- see module doc), then prefetch that expanded set. Does NOT clear
    /// the memo -- safe to call from a concurrent emitter that doesn't own
    /// the block-generation boundary (e.g. `ingest/fees.rs`'s fee poller,
    /// which runs alongside the main pipeline and must not wipe out ITS
    /// fresh prefetches for unrelated pools). Callers must call this BEFORE
    /// sending their `ChangedBatch`.
    pub async fn prefetch_neighborhood(
        &self,
        _engine: &Engine,
        store: &crate::routing::RouteStore,
        dirty: &[PoolIdx],
    ) {
        let mut neighborhood: rustc_hash::FxHashSet<PoolIdx> = dirty.iter().copied().collect();
        for &p in dirty {
            for &route_id in store.routes_for_pool(p) {
                for hop in &store.route(route_id).hops {
                    neighborhood.insert(hop.pool);
                }
            }
        }
        let mut slots = Vec::new();
        for idx in &neighborhood {
            if let Some(plan) = self.plan(*idx) {
                slots.extend(Self::plan_slots(plan));
            }
        }
        if !slots.is_empty() {
            self.prefetch(&slots).await;
        }
        // NOTE: no per-pool tick-window re-fetch here. Tick data lives in
        // the persistent `tick_cache` (hydrated once at bootstrap via
        // `hydrate_all_ticks`, kept correct incrementally via
        // `patch_cl_ticks` when the ingest pipeline decodes a
        // Mint/Burn/ModifyLiquidity event, and re-centered by
        // `run_tick_resync_loop` — see `ingest/pipeline.rs`).
    }

    /// Same as `prefetch_neighborhood`, but first clears the memo -- for the
    /// caller that OWNS the block-generation boundary (only
    /// `ingest/pipeline.rs`'s main loop; nothing else may call this, or it
    /// will race-wipe the main pipeline's own prefetches).
    pub async fn advance_and_prefetch(
        &self,
        engine: &Engine,
        store: &crate::routing::RouteStore,
        tip: u64,
        dirty: &[PoolIdx],
    ) {
        self.advance_to(tip);
        self.prefetch_neighborhood(engine, store, dirty).await;
    }

    /// Advance the cache generation to a new sealed tip: clear the memo (its
    /// entries described the previous block) and publish the tip. Must run
    /// BEFORE the caller re-prefetches + emits the next `ChangedBatch`, so
    /// consumers never quote against stale memo entries from the prior block.
    pub fn advance_to(&self, tip: u64) {
        self.memo.clear();
        self.tip.store(tip, Ordering::Relaxed);
    }
}

/// Lazy tick source over ChainState for the swap walk: bitmap words and tick
/// nets read on demand, with a one-word local cache (consecutive walk steps
/// usually probe the same word). Sync, reads only from whatever
/// `ChainState::read`/`read_tick_cache_checked` already has cached — a tick
/// word that was never fetched returns `None` (see `bitmap_word` below)
/// rather than being fetched on demand, so the tick walk's caller
/// (routing/optimizer.rs) must prefetch the walk's likely word range up
/// front via `ChainState::prefetch` for a route to quote correctly.
pub struct ClTickReader<'a> {
    pub state: &'a ChainState,
    pub addr: Address,
    pub ticks_base: U256,
    pub bitmap_base: U256,
    pub codec: TickCodec,
    last_word: std::cell::Cell<Option<(i16, U256)>>,
}

impl<'a> ClTickReader<'a> {
    pub fn new(
        state: &'a ChainState,
        addr: Address,
        ticks_base: U256,
        bitmap_base: U256,
        codec: TickCodec,
    ) -> Self {
        Self { state, addr, ticks_base, bitmap_base, codec, last_word: std::cell::Cell::new(None) }
    }

    #[inline]
    fn rd(&self, slot: U256) -> U256 {
        self.state.read(self.addr, slot)
    }
}

impl crate::math::v3::TickSource for ClTickReader<'_> {
    fn bitmap_word(&self, word: i16) -> Option<U256> {
        if let Some((w, bits)) = self.last_word.get() {
            if w == word {
                return Some(bits);
            }
        }
        let slot = crate::ingest::slot_layout::mapping_slot_signed(word as i64, self.bitmap_base);
        let bits = self.state.read_tick_cache_checked(self.addr, slot)?;
        self.last_word.set(Some((word, bits)));
        Some(bits)
    }

    fn liquidity_net(&self, tick: i32) -> i128 {
        let slot0 = crate::ingest::slot_layout::mapping_slot_signed(tick as i64, self.ticks_base);
        match self.codec {
            // V3/V4: net = signed high 128 bits of the gross|net word.
            TickCodec::Packed => {
                let word = self.rd(slot0);
                (word >> 128usize).to::<u128>() as i128
            }
            // Algebra: net (`liquidityDelta`) = signed low 128 of word +1.
            TickCodec::TwoWord => {
                let word1 = self.rd(slot0 + U256::from(1u8));
                crate::ingest::slot_layout::decode_low128(word1) as i128
            }
        }
    }
}

/// Build every pool's read recipe from the discovered per-tag layouts.
/// Pools without a usable layout get `None` — such a pool cannot be quoted
/// at all, so the caller must drop them loudly.
pub async fn build_slot_plans(
    engine: &Engine,
    layouts: &FxHashMap<DexTag, SlotLayout>,
    provider: &DynProvider,
) -> Vec<Option<SlotPlan>> {
    let mut plans: Vec<Option<SlotPlan>> = Vec::with_capacity(engine.metas.len());
    for m in &engine.metas {
        // V4: the singleton PoolManager's slots derive from the poolId —
        // canonical, no per-pool discovery (verify vs IStateView on
        // Robinhood Chain's own PoolManager before trusting in production).
        if let Some(v4) = &m.v4 {
            let state_slot = v4_state_slot(v4.pool_id);
            plans.push(Some(SlotPlan::Cl {
                addr: UNIV4_POOL_MANAGER,
                slot0: state_slot,
                liquidity: state_slot + U256::from(V4_LIQUIDITY_OFFSET),
                liquidity_shift: 0,
                ticks_base: state_slot + U256::from(V4_TICKS_OFFSET),
                bitmap_base: state_slot + U256::from(V4_TICK_BITMAP_OFFSET),
                tick_codec: TickCodec::Packed,
                fee: ClFee::Meta,
            }));
            continue;
        }
        let plan = match layouts.get(&m.dex) {
            Some(&SlotLayout::V2Packed { slot }) => {
                Some(SlotPlan::V2Packed { addr: m.address, slot: U256::from(slot) })
            }
            Some(&SlotLayout::V2TwoSlots { slot_r0 }) => {
                Some(SlotPlan::V2TwoSlot { addr: m.address, r0: U256::from(slot_r0) })
            }
            Some(&SlotLayout::V3 { slot0, liquidity, ticks, tick_bitmap }) => {
                Some(SlotPlan::Cl {
                    addr: m.address,
                    slot0: U256::from(slot0),
                    liquidity: U256::from(liquidity),
                    liquidity_shift: 0,
                    ticks_base: U256::from(ticks),
                    bitmap_base: U256::from(tick_bitmap),
                    tick_codec: TickCodec::Packed,
                    fee: ClFee::Meta,
                })
            }
            Some(&SlotLayout::Algebra {
                global_state,
                liquidity_slot,
                liquidity_shift,
                ticks,
                tick_table,
                fee_shift,
            }) => {
                let fee = match fee_shift {
                    Some(shift) => match slot_layout::algebra_fee_word_trusted(
                        provider,
                        m.address,
                        global_state,
                        shift,
                    )
                    .await
                    {
                        Ok(true) => ClFee::Word { shift },
                        Ok(false) => ClFee::Meta,
                        Err(e) => {
                            tracing::warn!(pool = %m.address, error = %e, "algebra fee probe failed; using meta fee");
                            ClFee::Meta
                        }
                    },
                    None => ClFee::Meta,
                };
                Some(SlotPlan::Cl {
                    addr: m.address,
                    slot0: U256::from(global_state),
                    liquidity: U256::from(liquidity_slot),
                    liquidity_shift,
                    ticks_base: U256::from(ticks),
                    bitmap_base: U256::from(tick_table),
                    tick_codec: TickCodec::TwoWord,
                    fee,
                })
            }
            None => None,
        };
        plans.push(plan);
    }
    plans
}

#[cfg(test)]
mod diag {
    //! Reference diagnostic: rebuilds a 3-hop V4 route's quote from scratch
    //! using a fully-live tick source (every bitmap word fetched on demand
    //! straight from the archive RPC at an exact historical block, no
    //! `ChainState` cache, no bootstrap window cap) and compares against the
    //! real on-chain result. Used to tell apart "the production tick window
    //! is too narrow for this route" from "something is wrong even with
    //! complete data".
    use super::*;
    use crate::math::v3::{get_amount_out_src, TickSource};
    use alloy::providers::ProviderBuilder;
    use std::collections::HashMap;

    struct LiveWideSource {
        bitmap: HashMap<i16, U256>,
        ticks: HashMap<i32, i128>,
    }
    impl TickSource for LiveWideSource {
        fn bitmap_word(&self, word: i16) -> Option<U256> {
            Some(self.bitmap.get(&word).copied().unwrap_or_default())
        }
        fn liquidity_net(&self, tick: i32) -> i128 {
            self.ticks.get(&tick).copied().unwrap_or(0)
        }
    }

    /// Fetches every bitmap word in `[center-range, center+range]` plus the
    /// tick-net for every initialized tick found in them, all via live RPC
    /// at `block`, bounded concurrency (same discipline as `prefetch_into`).
    async fn fetch_wide(
        provider: &DynProvider,
        addr: Address,
        ticks_base: U256,
        bitmap_base: U256,
        tick_spacing: i32,
        center_word: i16,
        range: i16,
        block: u64,
    ) -> LiveWideSource {
        use futures::stream::{self, StreamExt};
        let words: Vec<i16> =
            (center_word.saturating_sub(range)..=center_word.saturating_add(range)).collect();
        let bitmap_pairs: Vec<(i16, U256)> = stream::iter(words.into_iter().map(|w| {
            let provider = provider.clone();
            let slot = slot_layout::mapping_slot_signed(w as i64, bitmap_base);
            async move {
                let mut backoff = 400u64;
                loop {
                    match provider.get_storage_at(addr, slot).number(block).await {
                        Ok(v) => return (w, v),
                        Err(_) => {
                            tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                            backoff = (backoff * 2).min(8000);
                        }
                    }
                }
            }
        }))
        .buffer_unordered(8)
        .collect()
        .await;

        let mut bitmap = HashMap::new();
        let mut initialized_ticks: Vec<i32> = Vec::new();
        for (w, bits) in bitmap_pairs {
            if bits.is_zero() {
                continue;
            }
            for bit in 0..256u32 {
                if bits.bit(bit as usize) {
                    initialized_ticks.push((w as i32 * 256 + bit as i32) * tick_spacing);
                }
            }
            bitmap.insert(w, bits);
        }

        let tick_pairs: Vec<(i32, i128)> = stream::iter(initialized_ticks.into_iter().map(|t| {
            let provider = provider.clone();
            let slot = slot_layout::mapping_slot_signed(t as i64, ticks_base);
            async move {
                let mut backoff = 400u64;
                let word = loop {
                    match provider.get_storage_at(addr, slot).number(block).await {
                        Ok(v) => break v,
                        Err(_) => {
                            tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                            backoff = (backoff * 2).min(8000);
                        }
                    }
                };
                let net = (word >> 128usize).to::<u128>() as i128;
                (t, net)
            }
        }))
        .buffer_unordered(8)
        .collect()
        .await;

        LiveWideSource { bitmap, ticks: tick_pairs.into_iter().collect() }
    }

    async fn get_storage_retry(provider: &DynProvider, addr: Address, slot: U256, block: u64) -> U256 {
        let mut backoff = 400u64;
        loop {
            match provider.get_storage_at(addr, slot).number(block).await {
                Ok(v) => return v,
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                    backoff = (backoff * 2).min(8000);
                }
            }
        }
    }

    /// Run manually: cargo test --release -- --ignored --nocapture
    /// root_cause_route_1210_revert
    #[tokio::test]
    #[ignore]
    async fn root_cause_route_1210_revert() {
        let url = "https://rpc.mainnet.chain.robinhood.com".parse().unwrap();
        let provider: DynProvider = ProviderBuilder::new().connect_http(url).erased();
        let block = 15426397u64; // state right before the tx (which sits in block 15426398)

        let pools: [(B256, u32, i32, U256, bool, U256); 3] = {
            // (poolId, fee_pips, tick_spacing, amount_in placeholder unused, zero_for_one, _)
            let p0: B256 =
                "0xfee96a0e7cf4a544f2b42a163eee51be5cb0920f04099ab41662f6d33419a6ab".parse().unwrap();
            let p1: B256 =
                "0xc4db99050d1f8749fbd20721da135af1753bb0e8d79b098e2de13ce79c945a7f".parse().unwrap();
            let p2: B256 =
                "0x4be9657ec9002e528f4f17a5c43edc525a07f888f7b180c2afbf75e096c4f38a".parse().unwrap();
            [
                (p0, 15000u32, 300, U256::ZERO, false, U256::ZERO),
                (p1, 10000u32, 200, U256::ZERO, false, U256::ZERO),
                (p2, 3000u32, 60, U256::ZERO, true, U256::ZERO),
            ]
        };

        let mut amount = U256::from(66565729u64); // real amount_in, from the live tx
        for (pool_id, fee_pips, tick_spacing, _, zero_for_one, _) in pools {
            let state_slot = v4_state_slot(pool_id);
            let slot0 = state_slot;
            let liquidity_slot = state_slot + U256::from(V4_LIQUIDITY_OFFSET);
            let ticks_base = state_slot + U256::from(V4_TICKS_OFFSET);
            let bitmap_base = state_slot + U256::from(V4_TICK_BITMAP_OFFSET);

            let word0 = get_storage_retry(&provider, UNIV4_POOL_MANAGER, slot0, block).await;
            let (sqrt_price, tick) = slot_layout::decode_v3_slot0(word0);
            let liquidity_raw =
                get_storage_retry(&provider, UNIV4_POOL_MANAGER, liquidity_slot, block).await;
            let liquidity: u128 = (liquidity_raw & U256::from(u128::MAX)).to::<u128>();

            let center_word = (slot_layout::compress_tick(tick, tick_spacing) >> 8) as i16;
            // 500 words either side: ~500*256*tick_spacing ticks -- vastly
            // wider than production's 64-word bootstrap window, deliberately
            // "as complete as practical" rather than another arbitrary cap.
            let src = fetch_wide(
                &provider,
                UNIV4_POOL_MANAGER,
                ticks_base,
                bitmap_base,
                tick_spacing,
                center_word,
                500,
                block,
            )
            .await;

            let out = get_amount_out_src(
                amount,
                zero_for_one,
                sqrt_price,
                tick,
                liquidity,
                fee_pips,
                tick_spacing,
                &src,
            );
            println!(
                "pool {pool_id}: tick={tick} liquidity={liquidity} in={amount} out={out}"
            );
            amount = out;
        }

        println!("FINAL (fully-live, 500-word window) out = {amount}");
        println!("REAL ON-CHAIN result was: 65573836");
        println!("BOT'S ORIGINAL PREDICTION implied ~67125626 (start + predicted net_profit_wei 559897 + 66565729)");
    }
}
