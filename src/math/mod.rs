pub mod aero_stable;
pub mod v2;
pub mod v3;

use crate::engine::{DexTag, PoolMeta};
use crate::ingest::slot_layout::{decode_shifted128, decode_v2_packed, decode_v3_slot0};
use crate::state::{ChainState, ClFee, ClTickReader, SlotPlan};
use alloy::primitives::U256;

/// Lossy U256 -> f64 (good to ~52 bits; sufficient for ranking/gates).
pub fn u256_to_f64(v: U256) -> f64 {
    let bits = v.bit_len();
    if bits <= 52 {
        return v.to::<u128>() as f64;
    }
    let shift = bits - 52;
    let top = v >> shift;
    (top.to::<u128>() as f64) * 2f64.powi(shift as i32)
}

/// Exact-input quote straight off chain state (memo → MDBX) — the
/// mirror-free path. A pool without a slot plan (or with degenerate state)
/// quotes zero, which no gate ever finds profitable.
pub fn quote(state: &ChainState, meta: &PoolMeta, amount_in: U256, zero_for_one: bool) -> U256 {
    let Some(plan) = state.plan(meta.idx) else { return U256::ZERO };
    match plan {
        SlotPlan::V2Packed { addr, slot } => {
            let (r0, r1) = decode_v2_packed(state.read(*addr, *slot));
            if r0 == 0 || r1 == 0 {
                return U256::ZERO;
            }
            let (rin, rout) =
                if zero_for_one { (U256::from(r0), U256::from(r1)) } else { (U256::from(r1), U256::from(r0)) };
            v2::get_amount_out(amount_in, rin, rout, meta.fee_bps())
        }
        SlotPlan::V2TwoSlot { addr, r0 } => {
            let w0 = state.read(*addr, *r0);
            let w1 = state.read(*addr, *r0 + U256::from(1u8));
            if w0.is_zero() || w1.is_zero() {
                return U256::ZERO;
            }
            if meta.dex == DexTag::AeroStable {
                let d0 = U256::from(10u64).pow(U256::from(meta.dec0));
                let d1 = U256::from(10u64).pow(U256::from(meta.dec1));
                let (rin, rout, din, dout) =
                    if zero_for_one { (w0, w1, d0, d1) } else { (w1, w0, d1, d0) };
                aero_stable::get_amount_out(amount_in, rin, rout, din, dout, meta.fee_bps())
            } else {
                let (rin, rout) = if zero_for_one { (w0, w1) } else { (w1, w0) };
                v2::get_amount_out(amount_in, rin, rout, meta.fee_bps())
            }
        }
        SlotPlan::Cl {
            addr,
            slot0,
            liquidity,
            liquidity_shift,
            ticks_base,
            bitmap_base,
            tick_codec,
            fee,
        } => {
            let word0 = state.read(*addr, *slot0);
            let (sqrt, tick) = decode_v3_slot0(word0);
            if sqrt.is_zero() {
                return U256::ZERO;
            }
            let liq = decode_shifted128(state.read(*addr, *liquidity), *liquidity_shift);
            let fee_pips = match fee {
                ClFee::Meta => meta.fee_pips(),
                ClFee::Word { shift } => ((word0 >> *shift) & U256::from(0xffffu32)).to::<u32>(),
            };
            let reader = ClTickReader::new(state, *addr, *ticks_base, *bitmap_base, *tick_codec);
            // Bitmap indexing differs by family: V3/V4 compress the tick by
            // tickSpacing before wording (`word = (tick/spacing) >> 8`);
            // Algebra Integral's tickTable words RAW ticks (`word = tick>>8`,
            // spacing only constrains where mints may land) — confirmed
            // on-chain: a spacing-compressed word reads 0x0 while the raw
            // word holds the expected bit. Walking with spacing here would
            // skip most initialized ticks and inflate large quotes.
            let walk_spacing = match tick_codec {
                crate::state::TickCodec::Packed => meta.tick_spacing,
                crate::state::TickCodec::TwoWord => 1,
            };
            v3::get_amount_out_src(
                amount_in,
                zero_for_one,
                sqrt,
                tick,
                liq,
                fee_pips,
                walk_spacing,
                &reader,
            )
        }
    }
}

/// f64 fast quote for the optimizer inner loop, direct-read edition.
/// CL falls back to the exact path (memoized reads make repeats cheap).
pub fn quote_f64(
    state: &ChainState,
    meta: &PoolMeta,
    amount_in: f64,
    zero_for_one: bool,
) -> f64 {
    let Some(plan) = state.plan(meta.idx) else { return 0.0 };
    match plan {
        SlotPlan::V2Packed { addr, slot } => {
            let (r0, r1) = decode_v2_packed(state.read(*addr, *slot));
            let (rin, rout) =
                if zero_for_one { (r0 as f64, r1 as f64) } else { (r1 as f64, r0 as f64) };
            v2::get_amount_out_f64(amount_in, rin, rout, meta.fee_bps())
        }
        SlotPlan::V2TwoSlot { addr, r0 } => {
            let w0 = state.read(*addr, *r0);
            let w1 = state.read(*addr, *r0 + U256::from(1u8));
            if meta.dex == DexTag::AeroStable {
                let d0 = 10f64.powi(meta.dec0 as i32);
                let d1 = 10f64.powi(meta.dec1 as i32);
                let (rin, rout, din, dout) = if zero_for_one {
                    (u256_to_f64(w0), u256_to_f64(w1), d0, d1)
                } else {
                    (u256_to_f64(w1), u256_to_f64(w0), d1, d0)
                };
                aero_stable::get_amount_out_f64(amount_in, rin, rout, din, dout, meta.fee_bps())
            } else {
                let (rin, rout) = if zero_for_one {
                    (u256_to_f64(w0), u256_to_f64(w1))
                } else {
                    (u256_to_f64(w1), u256_to_f64(w0))
                };
                v2::get_amount_out_f64(amount_in, rin, rout, meta.fee_bps())
            }
        }
        SlotPlan::Cl { .. } => {
            let ain = U256::from(amount_in.max(0.0) as u128);
            u256_to_f64(quote(state, meta, ain, zero_for_one))
        }
    }
}

/// Tier-1 marginal post-fee rate from direct reads: V2 needs its reserve
/// word(s), CL only the slot0 word — same formulas as `PoolData::spot_rate`.
pub fn spot_rate(state: &ChainState, meta: &PoolMeta, zero_for_one: bool) -> f64 {
    let Some(plan) = state.plan(meta.idx) else { return 0.0 };
    match plan {
        SlotPlan::V2Packed { addr, slot } => {
            let (r0, r1) = decode_v2_packed(state.read(*addr, *slot));
            let (rin, rout) =
                if zero_for_one { (r0 as f64, r1 as f64) } else { (r1 as f64, r0 as f64) };
            if rin <= 0.0 || rout <= 0.0 {
                return 0.0;
            }
            let gamma = (10_000 - meta.fee_bps() as i64) as f64 / 10_000.0;
            gamma * rout / rin
        }
        SlotPlan::V2TwoSlot { addr, r0 } => {
            let w0 = state.read(*addr, *r0);
            let w1 = state.read(*addr, *r0 + U256::from(1u8));
            let gamma = (10_000 - meta.fee_bps() as i64) as f64 / 10_000.0;
            if meta.dex == DexTag::AeroStable {
                let x = u256_to_f64(w0) / 10f64.powi(meta.dec0 as i32);
                let y = u256_to_f64(w1) / 10f64.powi(meta.dec1 as i32);
                if x <= 0.0 || y <= 0.0 {
                    return 0.0;
                }
                let (a, b) = if zero_for_one { (x, y) } else { (y, x) };
                let num = 3.0 * a * a * b + b * b * b;
                let den = a * a * a + 3.0 * a * b * b;
                if den <= 0.0 {
                    return 0.0;
                }
                gamma * num / den
            } else {
                let (rin, rout) = if zero_for_one {
                    (u256_to_f64(w0), u256_to_f64(w1))
                } else {
                    (u256_to_f64(w1), u256_to_f64(w0))
                };
                if rin <= 0.0 || rout <= 0.0 {
                    return 0.0;
                }
                gamma * rout / rin
            }
        }
        SlotPlan::Cl { addr, slot0, fee, .. } => {
            let word0 = state.read(*addr, *slot0);
            let (sqrt, _) = decode_v3_slot0(word0);
            let sp = u256_to_f64(sqrt);
            let q96 = 2f64.powi(96);
            let ratio = sp / q96;
            let price1_per_0 = ratio * ratio;
            if price1_per_0 <= 0.0 {
                return 0.0;
            }
            let fee_pips = match fee {
                ClFee::Meta => meta.fee_pips(),
                ClFee::Word { shift } => ((word0 >> *shift) & U256::from(0xffffu32)).to::<u32>(),
            };
            let gamma = (1_000_000 - fee_pips as i64) as f64 / 1_000_000.0;
            if zero_for_one {
                gamma * price1_per_0
            } else {
                gamma / price1_per_0
            }
        }
    }
}
