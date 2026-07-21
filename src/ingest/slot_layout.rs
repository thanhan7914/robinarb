//! Per-DEX storage-slot layout discovery, used by `ChainState` to build each
//! pool's read recipe (`SlotPlan`) at bootstrap. Layouts differ across forks
//! and hardcoding them is how quoters end up reading the wrong pool (§6
//! lesson, storage edition) — every offset here is matched against a real
//! on-chain view call, never assumed from source.

use crate::engine::{DexTag, Engine, PoolIdx};
use alloy::eips::BlockId;
use alloy::primitives::U256;
use alloy::providers::{DynProvider, Provider};
use rustc_hash::FxHashMap;
use std::sync::Arc;

/// Storage slots scanned per pool during layout discovery.
const SCAN_SLOTS: u64 = 24;
/// Sample pools tried per DexTag during layout discovery.
const DISCOVERY_SAMPLES: usize = 3;

// ---------------------------------------------------------------------------
// Slot-layout discovery (per DexTag, at bootstrap)
// ---------------------------------------------------------------------------

/// Where a pool's hot fields live in storage. Discovered per DexTag by
/// matching eth_call values against a storage scan of a sample pool — layouts
/// differ across forks and hardcoding them is how quoters end up reading the
/// wrong pool (§6 lesson, storage edition).
#[derive(Debug, Clone, Copy)]
pub enum SlotLayout {
    /// UniV2-style packed reserves: `reserve0 u112 | reserve1 u112 | ts u32`.
    V2Packed { slot: u64 },
    /// Solidly/Aerodrome v2: two consecutive full-word reserve slots.
    V2TwoSlots { slot_r0: u64 },
    /// V3 family: packed slot0 (`sqrtPriceX96 u160 | tick i24 | ...`) + the
    /// standalone `liquidity u128` slot, plus the two mapping BASE slots
    /// (`mapping(int24 => Tick.Info) ticks` / `mapping(int16 => uint256)
    /// tickBitmap`) needed to read tick data directly from storage — a real
    /// tick/word's mapping slot is `keccak256(abi.encodePacked(int256(key),
    /// base))`. Discovered empirically per fork like slot0/liquidity: forks
    /// diverge here too (confirmed — PancakeV3's deployed `liquidity` sits at
    /// slot 5, not the slot 4 its public source's declared storage order
    /// would suggest, so guessing offsets from source is not safe; only a
    /// real match against `ticks()`/`tickBitmap()` view calls is trusted).
    V3 { slot0: u64, liquidity: u64, ticks: u64, tick_bitmap: u64 },
    /// Algebra Integral (Hydrex, QuickSwap v4): `globalState` packs
    /// `price|tick|...` the same way as V3's slot0 (`decode_v3_slot0` is
    /// reused as-is), but `liquidity` is packed alongside `nextTickGlobal`/
    /// `prevTickGlobal` rather than living alone at the low 128 bits — so
    /// its bit offset (`liquidity_shift`) is part of the discovered layout,
    /// not assumed to be 0. Tick data uses `tickTable`/`ticks` (Algebra's
    /// own names) with a materially different per-tick word layout than V3
    /// (`liquidityDelta` is the SECOND word of the mapping slot, not packed
    /// into the first alongside a gross counter).
    ///
    /// `fee_shift`: bit offset of the current swap fee (`lastFee`, u16)
    /// INSIDE the globalState word — Algebra re-prices fee on swaps with no
    /// event, but unlike Slipstream the live fee sits in the very word every
    /// quote already reads, so once this offset is discovered (matched
    /// against both `fee()` and `globalState().lastFee`, never assumed) the
    /// fee needs no separate tracking at all. `None` = no trustworthy match
    /// (e.g. plugin overrides `fee()` away from `lastFee`) — fee must then
    /// come from meta + the fee poller instead.
    Algebra {
        global_state: u64,
        liquidity_slot: u64,
        liquidity_shift: u32,
        ticks: u64,
        tick_table: u64,
        fee_shift: Option<u32>,
    },
}

/// Extract a 128-bit unsigned window starting at an arbitrary bit offset —
/// the general form of `decode_low128` (`shift=0`), needed because Algebra
/// packs `liquidity` alongside two `int24` ticks instead of alone.
pub(crate) fn decode_shifted128(w: U256, shift: u32) -> u128 {
    let mask = (U256::from(1u8) << 128usize) - U256::from(1u8);
    ((w >> shift) & mask).to::<u128>()
}

/// `keccak256(abi.encodePacked(int256(key), uint256(base)))` — the Solidity
/// mapping-slot formula for a signed key type (ticks/tickBitmap use this for
/// both plain V3-family pools and, nested under a pool's state slot, V4).
pub(crate) fn mapping_slot_signed(key: i64, base: U256) -> U256 {
    let key_bytes = alloy::primitives::I256::try_from(key).expect("fits i256").to_be_bytes::<32>();
    let base_bytes = base.to_be_bytes::<32>();
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&key_bytes);
    buf[32..].copy_from_slice(&base_bytes);
    U256::from_be_bytes(alloy::primitives::keccak256(buf).0)
}

pub async fn discover_layouts(
    engine: &Arc<Engine>,
    provider: &DynProvider,
) -> FxHashMap<DexTag, SlotLayout> {
    let mut samples: FxHashMap<DexTag, Vec<PoolIdx>> = FxHashMap::default();
    for m in &engine.metas {
        // V4 state lives in the PoolManager keyed by poolId (derived slots),
        // not at the pool's (synthetic) address — no per-pool layout to probe.
        if m.dex.is_v4() {
            continue;
        }
        let v = samples.entry(m.dex).or_default();
        if v.len() < DISCOVERY_SAMPLES {
            v.push(m.idx);
        }
    }

    let mut out = FxHashMap::default();
    for (tag, idxs) in samples {
        let mut found = None;
        for idx in idxs {
            match discover_one(engine, provider, idx).await {
                Ok(Some(layout)) => {
                    found = Some(layout);
                    break;
                }
                Ok(None) => continue,
                Err(e) => {
                    tracing::debug!(error = %e, pool = %engine.meta(idx).address, "layout probe failed");
                }
            }
        }
        match found {
            Some(layout) => {
                tracing::info!(?tag, ?layout, "slot layout discovered");
                out.insert(tag, layout);
            }
            None => tracing::warn!(?tag, "no slot layout found; pool cannot be quoted"),
        }
    }

    // Per-pool fee-from-word trust probe. `fee_shift` says where `lastFee`
    // lives in the word; a pool may only source its quote fee from there if
    // the fee a swap would actually be charged (`fee()`, plugin-aware)
    // equals that field — pools with a dynamic-fee plugin fail this
    // (measured: Hydrex fee()=25 wei-exact vs lastFee=500 stale) and must
    // stay on the fee poller. Logged here at discovery time; the same probe
    // gates the per-pool slot plans.
    if let Some(&SlotLayout::Algebra { global_state, fee_shift: Some(shift), .. }) =
        out.get(&DexTag::Algebra)
    {
        for m in &engine.metas {
            if m.dex != DexTag::Algebra {
                continue;
            }
            match algebra_fee_word_trusted(provider, m.address, global_state, shift).await {
                Ok(true) => tracing::info!(pool = %m.address, "algebra fee-from-word OK (fee()==lastFee)"),
                Ok(false) => tracing::info!(pool = %m.address, "algebra fee-from-word untrusted (plugin fee) — stays on fee poller"),
                Err(e) => tracing::warn!(pool = %m.address, error = %e, "algebra fee-from-word probe failed"),
            }
        }
    }
    out
}

/// Whether one Algebra pool may source its quote fee from the globalState
/// word: `(word >> shift) & 0xFFFF` must equal a same-block `fee()` call.
pub async fn algebra_fee_word_trusted(
    provider: &DynProvider,
    address: alloy::primitives::Address,
    gs_slot: u64,
    shift: u32,
) -> anyhow::Result<bool> {
    use crate::abi::IAlgebraPool;
    let block = BlockId::from(provider.get_block_number().await?);
    let word =
        provider.get_storage_at(address, U256::from(gs_slot)).block_id(block).await?;
    let fee = IAlgebraPool::new(address, provider).fee().block(block).call().await?;
    Ok((word >> shift) & U256::from(0xffffu32) == U256::from(fee))
}

async fn discover_one(
    engine: &Arc<Engine>,
    provider: &DynProvider,
    idx: PoolIdx,
) -> anyhow::Result<Option<SlotLayout>> {
    use crate::abi::{IAeroPool, IUniV2Pair, IUniV3Pool};

    let meta = engine.meta(idx);
    // Pin every read to one block so reference values and storage words agree.
    let block = BlockId::from(provider.get_block_number().await?);
    let words = read_slots(provider, meta.address, (0..SCAN_SLOTS).collect(), block).await?;

    if meta.dex.is_v3_family() {
        let c = IUniV3Pool::new(meta.address, provider);
        let s = c.slot0().block(block).call().await?;
        let liq = c.liquidity().block(block).call().await?;
        let sqrt = U256::from(s.sqrtPriceX96);
        if sqrt.is_zero() || liq == 0 {
            return Ok(None); // dead pool can't disambiguate; try the next sample
        }
        let tick = s.tick.as_i32();
        let slot0 = words.iter().position(|w| decode_v3_slot0(*w) == (sqrt, tick));
        // Match on the LOW 128 bits only: Slipstream/AeroCL pack
        // stakedLiquidity into the high half of the liquidity slot, so the
        // word as a whole is not just `liquidity`. A false match would need
        // another slot's low half to equal a nonzero liquidity exactly.
        let liquidity = words.iter().position(|w| decode_low128(*w) == liq);
        let (Some(s0), Some(lq)) = (slot0, liquidity) else { return Ok(None) };
        if s0 == lq {
            return Ok(None);
        }
        let Some((ticks, tick_bitmap)) =
            discover_v3_tick_mapping(&c, provider, meta.address, block, tick, meta.tick_spacing)
                .await?
        else {
            return Ok(None);
        };
        return Ok(Some(SlotLayout::V3 {
            slot0: s0 as u64,
            liquidity: lq as u64,
            ticks,
            tick_bitmap,
        }));
    }

    if meta.dex == DexTag::Algebra {
        return discover_algebra(provider, meta.tick_spacing, meta.address, &words, block).await;
    }

    match meta.dex {
        DexTag::UniV2Fork => {
            let r = IUniV2Pair::new(meta.address, provider).getReserves().block(block).call().await?;
            let (r0, r1) = (r.reserve0.to::<u128>(), r.reserve1.to::<u128>());
            if r0 == 0 {
                return Ok(None);
            }
            let slot = words.iter().position(|w| decode_v2_packed(*w) == (r0, r1));
            Ok(slot.map(|s| SlotLayout::V2Packed { slot: s as u64 }))
        }
        DexTag::AeroVolatile | DexTag::AeroStable => {
            let r = IAeroPool::new(meta.address, provider).getReserves().block(block).call().await?;
            if r.reserve0.is_zero() {
                return Ok(None);
            }
            let slot = words
                .windows(2)
                .position(|w| w[0] == r.reserve0 && w[1] == r.reserve1);
            Ok(slot.map(|s| SlotLayout::V2TwoSlots { slot_r0: s as u64 }))
        }
        _ => Ok(None),
    }
}

/// Bit-shift candidates tried for Algebra's `liquidity` (packed alongside
/// `nextTickGlobal`/`prevTickGlobal`, two `int24`s = 48 bits, ahead of it per
/// its public source — checked at 0 too in case a deployed variant differs,
/// same "verify, don't assume" discipline as everything else in this file).
const ALGEBRA_LIQUIDITY_SHIFTS: [u32; 2] = [0, 48];

/// Algebra Integral (Hydrex/QuickSwap v4) layout discovery: `globalState`
/// decodes like V3's slot0 (same bit positions for price/tick), but
/// `liquidity` and the tick mapping words differ enough from V3 to need
/// their own matching logic — see `SlotLayout::Algebra`'s doc comment.
async fn discover_algebra(
    provider: &DynProvider,
    tick_spacing: i32,
    address: alloy::primitives::Address,
    words: &[U256],
    block: BlockId,
) -> anyhow::Result<Option<SlotLayout>> {
    use crate::abi::IAlgebraPool;

    let c = IAlgebraPool::new(address, provider);
    let gs = c.globalState().block(block).call().await?;
    let liq = c.liquidity().block(block).call().await?;
    let sqrt = U256::from(gs.price);
    if sqrt.is_zero() || liq == 0 {
        return Ok(None);
    }
    let tick = gs.tick.as_i32();

    let global_state = words.iter().position(|w| decode_v3_slot0(*w) == (sqrt, tick));
    let mut liquidity_pick = None;
    for (i, w) in words.iter().enumerate() {
        if Some(i) == global_state {
            continue;
        }
        for &shift in &ALGEBRA_LIQUIDITY_SHIFTS {
            if decode_shifted128(*w, shift) == liq {
                liquidity_pick = Some((i as u64, shift));
                break;
            }
        }
        if liquidity_pick.is_some() {
            break;
        }
    }
    let (Some(gs_slot), Some((liq_slot, liq_shift))) = (global_state, liquidity_pick) else {
        return Ok(None);
    };

    let Some((ticks, tick_table)) =
        discover_algebra_tick_mapping(&c, provider, address, block, tick, tick_spacing).await?
    else {
        return Ok(None);
    };
    let fee_shift = discover_algebra_fee_shift(words[gs_slot], gs.lastFee);
    Ok(Some(SlotLayout::Algebra {
        global_state: gs_slot as u64,
        liquidity_slot: liq_slot,
        liquidity_shift: liq_shift,
        ticks,
        tick_table,
        fee_shift,
    }))
}

/// Locate `lastFee` (u16) inside the already-matched globalState word by
/// matching the same-block `globalState().lastFee` view value. This only
/// finds WHERE the field lives; whether that field is a usable fee source
/// for a given pool is a separate per-pool question — pools with a
/// dynamic-fee plugin charge `fee()` (plugin `getCurrentFee`), which can
/// diverge from `lastFee` (measured on Hydrex WETH/USDC: fee()=25 was
/// wei-exact against a real swap while lastFee=500 was stale history), so
/// the trust decision (`fee() == lastFee`?) is made per pool when slot
/// plans are built, not here. Byte-aligned shifts; 184
/// (`u160 price | i24 tick |` → fee) tried first.
fn discover_algebra_fee_shift(gs_word: U256, last_fee: u16) -> Option<u32> {
    if last_fee == 0 {
        return None;
    }
    let want = U256::from(last_fee);
    let mask = U256::from(0xffffu32);
    std::iter::once(184u32)
        .chain((160..=240).step_by(8).filter(|s| *s != 184))
        .find(|&shift| (gs_word >> shift) & mask == want)
}

/// Same "match a real view call" discipline as `discover_v3_tick_mapping`,
/// but for Algebra's `Tick` struct layout: `liquidityTotal` (gross-analog) is
/// a full standalone word (mapping slot + 0), and `liquidityDelta`
/// (net-analog, signed) is the LOW 128 bits of the NEXT word (mapping slot +
/// 1) — packed with `prevTick`/`nextTick`, not sharing a word with
/// `liquidityTotal` the way V3 packs gross+net together.
async fn discover_algebra_tick_mapping(
    c: &crate::abi::IAlgebraPool::IAlgebraPoolInstance<&DynProvider>,
    provider: &DynProvider,
    address: alloy::primitives::Address,
    block: BlockId,
    current_tick: i32,
    tick_spacing: i32,
) -> anyhow::Result<Option<(u64, u64)>> {
    // Probe pick: scan tickTable() view calls outward from the current
    // price's word until a nonzero bitmap word is found — no dependence on
    // any locally cached tick data; the chain itself nominates the probe.
    //
    // Algebra Integral's tickTable words RAW ticks (`word = tick >> 8`) —
    // tickSpacing only constrains mint placement, unlike V3's compressed
    // bitmap (confirmed on-chain: a spacing-compressed word reads 0x0 while
    // the raw word holds the expected bit). So the scan and the probe-tick
    // derivation here use raw indexing.
    let _ = tick_spacing;
    let center_word = (current_tick >> 8) as i16;
    let mut probe = None;
    for delta in probe_word_offsets() {
        let Some(word) = center_word.checked_add(delta) else { continue };
        let bits = c.tickTable(word).block(block).call().await?;
        if let Some(bit) = lowest_set_bit(bits) {
            probe = Some((word, bit, bits));
            break;
        }
    }
    let Some((probe_word, bit, real_table)) = probe else { return Ok(None) };
    let probe_tick = probe_word as i32 * 256 + bit as i32;

    let tick_i24 = alloy::primitives::aliases::I24::try_from(probe_tick).expect("tick fits i24");
    let real_ticks = c.ticks(tick_i24).block(block).call().await?;
    let real_total: U256 = real_ticks.liquidityTotal;
    let real_delta: i128 = real_ticks.liquidityDelta;
    if real_total.is_zero() {
        return Ok(None);
    }

    let mut ticks_base = None;
    for base in 0..TICK_MAPPING_SCAN {
        let slot0 = mapping_slot_signed(probe_tick as i64, U256::from(base));
        let word0 = provider.get_storage_at(address, slot0).block_id(block).await?;
        if word0 != real_total {
            continue;
        }
        let slot1 = slot0 + U256::from(1u8);
        let word1 = provider.get_storage_at(address, slot1).block_id(block).await?;
        let delta = decode_low128(word1) as i128;
        if delta == real_delta {
            ticks_base = Some(base);
            break;
        }
    }
    let mut table_base = None;
    for base in 0..TICK_MAPPING_SCAN {
        let slot = mapping_slot_signed(probe_word as i64, U256::from(base));
        let word = provider.get_storage_at(address, slot).block_id(block).await?;
        if word == real_table {
            table_base = Some(base);
            break;
        }
    }
    Ok(match (ticks_base, table_base) {
        (Some(t), Some(b)) if t != b => Some((t, b)),
        _ => None,
    })
}

/// Candidate mapping-base slots tried for `ticks`/`tickBitmap` discovery.
/// Standard Uniswap V3 core has them at 5/6 (right after `liquidity` at 4);
/// forks that insert or reorder fields can land anywhere nearby — PancakeV3's
/// own deployed `liquidity` already proved 1 slot off from what its public
/// source's declared order would suggest, so this is scanned, not assumed.
const TICK_MAPPING_SCAN: u64 = 20;

/// How many bitmap words each side of the current price to scan when hunting
/// a probe tick for mapping discovery. ±32 words = ±8192*spacing ticks —
/// any pool with liquidity close enough to be quotable has an initialized
/// tick well inside that.
const PROBE_WORD_RANGE: i16 = 32;

/// 0, -1, +1, -2, +2, … — nearest-first word offsets for the probe scan.
fn probe_word_offsets() -> impl Iterator<Item = i16> {
    std::iter::once(0).chain((1..=PROBE_WORD_RANGE).flat_map(|d| [-d, d]))
}

/// Tick compression: floor(tick / spacing) (rounds toward -inf) — same rule
/// as everywhere else in the codebase (see `mdbx::tick_word_bounds`).
pub(crate) fn compress_tick(tick: i32, spacing: i32) -> i32 {
    if tick < 0 && tick % spacing != 0 { tick / spacing - 1 } else { tick / spacing }
}

fn lowest_set_bit(bits: U256) -> Option<u32> {
    (0..256u32).find(|b| bits.bit(*b as usize))
}

/// Finds the `ticks`/`tickBitmap` mapping BASE slots for one V3-family pool
/// by matching `keccak256(abi.encodePacked(key, candidate_base))` reads
/// against real `ticks()`/`tickBitmap()` view-call results — the same
/// "trust the chain, not the source" discipline as the slot0/liquidity scan
/// above. The probe tick/word is nominated by the chain itself: scan
/// `tickBitmap()` view calls outward from the current price's word until a
/// nonzero word turns up (no dependence on locally cached tick data).
async fn discover_v3_tick_mapping(
    c: &crate::abi::IUniV3Pool::IUniV3PoolInstance<&DynProvider>,
    provider: &DynProvider,
    address: alloy::primitives::Address,
    block: BlockId,
    current_tick: i32,
    tick_spacing: i32,
) -> anyhow::Result<Option<(u64, u64)>> {
    let center_word = (compress_tick(current_tick, tick_spacing) >> 8) as i16;
    let mut probe = None;
    for delta in probe_word_offsets() {
        let Some(word) = center_word.checked_add(delta) else { continue };
        let bits = c.tickBitmap(word).block(block).call().await?;
        if let Some(bit) = lowest_set_bit(bits) {
            probe = Some((word, bit, bits));
            break;
        }
    }
    let Some((probe_word, bit, real_bitmap)) = probe else { return Ok(None) };
    let probe_tick = (probe_word as i32 * 256 + bit as i32) * tick_spacing;

    let tick_i24 = alloy::primitives::aliases::I24::try_from(probe_tick).expect("tick fits i24");
    let real_ticks = c.ticks(tick_i24).block(block).call().await?;
    let real_gross: u128 = real_ticks.liquidityGross;
    let real_net: i128 = real_ticks.liquidityNet;
    if real_gross == 0 {
        return Ok(None); // bitmap bit set but tick empty — inconsistent pool, skip
    }

    let mut ticks_base = None;
    let mut bitmap_base = None;
    for base in 0..TICK_MAPPING_SCAN {
        let slot = mapping_slot_signed(probe_tick as i64, U256::from(base));
        let word = provider.get_storage_at(address, slot).block_id(block).await?;
        // TickInfo packs liquidityGross (low 128) then liquidityNet (high
        // 128, signed) in the first word — same layout V4's StateLibrary
        // documents and BaseBuster's independently-written reader assumes.
        let gross = decode_low128(word);
        let net = (word >> 128usize).to::<u128>() as i128;
        if gross == real_gross && net == real_net {
            ticks_base = Some(base);
            break;
        }
    }
    for base in 0..TICK_MAPPING_SCAN {
        let slot = mapping_slot_signed(probe_word as i64, U256::from(base));
        let word = provider.get_storage_at(address, slot).block_id(block).await?;
        if word == real_bitmap {
            bitmap_base = Some(base);
            break;
        }
    }
    Ok(match (ticks_base, bitmap_base) {
        (Some(t), Some(b)) if t != b => Some((t, b)),
        _ => None,
    })
}

async fn read_slots(
    provider: &DynProvider,
    address: alloy::primitives::Address,
    slots: Vec<u64>,
    block: BlockId,
) -> anyhow::Result<Vec<U256>> {
    let futs = slots.iter().map(|s| {
        let slot = U256::from(*s);
        async move { provider.get_storage_at(address, slot).block_id(block).await }
    });
    let results = futures::future::join_all(futs).await;
    results.into_iter().map(|r| r.map_err(Into::into)).collect()
}

// ---------------------------------------------------------------------------
// Packed-word decoders
// ---------------------------------------------------------------------------

pub(crate) fn decode_v2_packed(w: U256) -> (u128, u128) {
    let mask = (U256::from(1u8) << 112usize) - U256::from(1u8);
    ((w & mask).to::<u128>(), ((w >> 112usize) & mask).to::<u128>())
}

pub(crate) fn decode_v3_slot0(w: U256) -> (U256, i32) {
    let mask160 = (U256::from(1u8) << 160usize) - U256::from(1u8);
    let sqrt = w & mask160;
    let raw = ((w >> 160usize) & U256::from(0xFF_FFFFu32)).to::<u32>();
    let tick = if raw & 0x80_0000 != 0 { (raw | 0xFF00_0000) as i32 } else { raw as i32 };
    (sqrt, tick)
}

pub(crate) fn decode_low128(w: U256) -> u128 {
    let mask = (U256::from(1u8) << 128usize) - U256::from(1u8);
    (w & mask).to::<u128>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_packed_roundtrip() {
        let r0: u128 = 123_456_789_000_000_000_000; // fits u112
        let r1: u128 = 987_654_321;
        let ts: u64 = 1_784_221_119;
        let w = U256::from(r0) | (U256::from(r1) << 112usize) | (U256::from(ts) << 224usize);
        assert_eq!(decode_v2_packed(w), (r0, r1));
    }

    #[test]
    fn v3_slot0_negative_tick() {
        let sqrt = U256::from_str_radix("1234567890123456789012345678901234567890", 10).unwrap();
        let tick: i32 = -193_461; // typical WETH/USDC-region tick
        let tick_u24 = (tick as u32) & 0xFF_FFFF;
        // observationIndex etc. live above bit 184; stuff junk there.
        let w = sqrt | (U256::from(tick_u24) << 160usize) | (U256::from(0xDEADu32) << 184usize);
        assert_eq!(decode_v3_slot0(w), (sqrt, tick));
    }

    #[test]
    fn v3_slot0_positive_tick() {
        let sqrt = U256::from(4295128739u64);
        let tick: i32 = 887_271;
        let w = sqrt | (U256::from(tick as u32) << 160usize);
        assert_eq!(decode_v3_slot0(w), (sqrt, tick));
    }
}
