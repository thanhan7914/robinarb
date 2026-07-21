//! Uniswap V3 / Slipstream / Pancake V3 exact-input single-pool swap simulation.
//!
//! Reimplements the on-chain `UniswapV3Pool.swap()` exact-input loop over an
//! immutable `TickTable` snapshot, using the battle-tested `uniswap_v3_math`
//! primitives (`compute_swap_step`, tick/bitmap math). Fee is in pips (1e-6):
//! e.g. the 0.3% tier is 3000.

use crate::constants::{max_sqrt_ratio, min_sqrt_ratio, MAX_TICK, MIN_TICK};
use alloy::primitives::{I256, U256};
use uniswap_v3_math::{bit_math, liquidity_math, swap_math, tick_math};

/// Lazy tick-data source for the direct-read swap walk: the walk asks for
/// exactly the bitmap words and tick nets it crosses, and the impl reads them
/// from wherever truth lives (ChainState/MDBX in production, an in-memory map
/// in tests).
pub trait TickSource {
    fn bitmap_word(&self, word: i16) -> Option<U256>;
    fn liquidity_net(&self, tick: i32) -> i128;
}

/// `TickBitmap.position`: word index + bit index of a compressed tick.
#[inline]
fn position(compressed: i32) -> (i16, u8) {
    ((compressed >> 8) as i16, (compressed & 0xff) as u8)
}

/// `TickBitmap.nextInitializedTickWithinOneWord` over a lazy `TickSource` —
/// same math as `uniswap_v3_math::tick_bitmap::next_initialized_tick_within_
/// one_word` (verified against it property-style in the tests below), minus
/// the `&HashMap` coupling that forced a materialized table.
/// `Ok(None)` = the word this step needs was never hydrated — caller must
/// stop the walk, not assume it's empty (see `TickSource::bitmap_word` doc).
fn next_initialized_tick_src<T: TickSource>(
    src: &T,
    tick: i32,
    tick_spacing: i32,
    lte: bool,
) -> Result<Option<(i32, bool)>, uniswap_v3_math::error::UniswapV3MathError> {
    let compressed = if tick < 0 && tick % tick_spacing != 0 {
        (tick / tick_spacing) - 1
    } else {
        tick / tick_spacing
    };
    let one = U256::from(1u8);
    if lte {
        let (word_pos, bit_pos) = position(compressed);
        let Some(word) = src.bitmap_word(word_pos) else { return Ok(None) };
        let mask = (one << bit_pos) - one + (one << bit_pos);
        let masked = word & mask;
        let initialized = !masked.is_zero();
        let next = if initialized {
            (compressed - (bit_pos.overflowing_sub(bit_math::most_significant_bit(masked)?).0) as i32)
                * tick_spacing
        } else {
            (compressed - bit_pos as i32) * tick_spacing
        };
        Ok(Some((next, initialized)))
    } else {
        let (word_pos, bit_pos) = position(compressed + 1);
        let Some(word) = src.bitmap_word(word_pos) else { return Ok(None) };
        let mask = !((one << bit_pos) - one);
        let masked = word & mask;
        let initialized = !masked.is_zero();
        let next = if initialized {
            (compressed + 1 + (bit_math::least_significant_bit(masked)?.overflowing_sub(bit_pos).0) as i32)
                * tick_spacing
        } else {
            (compressed + 1 + (0xff - bit_pos) as i32) * tick_spacing
        };
        Ok(Some((next, initialized)))
    }
}

/// Exact-input quote over a lazy `TickSource` — the direct-read counterpart
/// of `get_amount_out`. No window guard: the source can serve any word, so
/// the only bounds are the price limits and the crossing guard.
pub fn get_amount_out_src<T: TickSource>(
    amount_in: U256,
    zero_for_one: bool,
    sqrt_price_x96: U256,
    tick: i32,
    liquidity: u128,
    fee_pips: u32,
    tick_spacing: i32,
    src: &T,
) -> U256 {
    match try_swap_src(amount_in, zero_for_one, sqrt_price_x96, tick, liquidity, fee_pips, tick_spacing, src) {
        Ok(out) => out,
        Err(_) => U256::ZERO,
    }
}

#[allow(clippy::too_many_arguments)]
fn try_swap_src<T: TickSource>(
    amount_in: U256,
    zero_for_one: bool,
    sqrt_price_x96: U256,
    start_tick: i32,
    start_liquidity: u128,
    fee_pips: u32,
    tick_spacing: i32,
    src: &T,
) -> Result<U256, uniswap_v3_math::error::UniswapV3MathError> {
    if amount_in.is_zero() {
        return Ok(U256::ZERO);
    }
    let sqrt_price_limit = if zero_for_one {
        min_sqrt_ratio() + U256::from(1)
    } else {
        max_sqrt_ratio() - U256::from(1)
    };

    let mut amount_remaining = I256::from_raw(amount_in);
    let mut amount_calculated = I256::ZERO;
    let mut sqrt_price = sqrt_price_x96;
    let mut tick = start_tick;
    let mut liquidity = start_liquidity;
    let mut guard = 0u32;

    while amount_remaining != I256::ZERO && sqrt_price != sqrt_price_limit {
        guard += 1;
        if guard > 1024 {
            break;
        }

        let sqrt_price_start = sqrt_price;
        let Some((mut next_tick, initialized)) =
            next_initialized_tick_src(src, tick, tick_spacing, zero_for_one)?
        else {
            // Edge of hydrated territory — stop here instead of
            // extrapolating current liquidity through the unknown region
            // (see `TickSource::bitmap_word` doc). Whatever's accumulated
            // so far is a safe lower-bound quote.
            break;
        };
        next_tick = next_tick.clamp(MIN_TICK, MAX_TICK);

        let sqrt_price_next = tick_math::get_sqrt_ratio_at_tick(next_tick)?;
        let sqrt_target = if (zero_for_one && sqrt_price_next < sqrt_price_limit)
            || (!zero_for_one && sqrt_price_next > sqrt_price_limit)
        {
            sqrt_price_limit
        } else {
            sqrt_price_next
        };

        let (sqrt_price_new, amount_in_step, amount_out_step, fee_amount) =
            swap_math::compute_swap_step(sqrt_price, sqrt_target, liquidity, amount_remaining, fee_pips)?;

        sqrt_price = sqrt_price_new;
        let consumed = I256::from_raw(amount_in_step + fee_amount);
        amount_remaining = amount_remaining - consumed;
        amount_calculated = amount_calculated - I256::from_raw(amount_out_step);

        if sqrt_price == sqrt_price_next {
            if initialized {
                let mut liquidity_net = src.liquidity_net(next_tick);
                if zero_for_one {
                    liquidity_net = -liquidity_net;
                }
                liquidity = liquidity_math::add_delta(liquidity, liquidity_net)?;
            }
            tick = if zero_for_one { next_tick - 1 } else { next_tick };
        } else if sqrt_price != sqrt_price_start {
            tick = tick_math::get_tick_at_sqrt_ratio(sqrt_price)?;
        }
    }

    Ok((-amount_calculated).into_raw())
}

/// Test-only relic of the mirror era: the windowed in-memory tick map the
/// old snapshot walk consumed. Kept (with that walk) solely as a reference
/// implementation for the differential tests below.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct TickTable {
    pub bitmap: std::collections::HashMap<i16, U256>,
    pub ticks: std::collections::BTreeMap<i32, i128>,
    pub min_word: i16,
    pub max_word: i16,
}

#[cfg(test)]
impl Default for TickTable {
    fn default() -> Self {
        Self {
            bitmap: Default::default(),
            ticks: Default::default(),
            min_word: i16::MIN,
            max_word: i16::MAX,
        }
    }
}

#[cfg(test)]
impl TickTable {
    pub fn liquidity_net(&self, tick: i32) -> i128 {
        self.ticks.get(&tick).copied().unwrap_or(0)
    }
    #[inline]
    pub fn word_in_window(&self, word: i16) -> bool {
        word >= self.min_word && word <= self.max_word
    }
}

/// Exact-input quote. `zero_for_one = true` swaps token0 -> token1 (price falls).
/// Returns the token-out amount. Errors (bad ticks etc.) collapse to `U256::ZERO`.
#[cfg(test)]
pub fn get_amount_out(
    amount_in: U256,
    zero_for_one: bool,
    sqrt_price_x96: U256,
    tick: i32,
    liquidity: u128,
    fee_pips: u32,
    tick_spacing: i32,
    table: &TickTable,
) -> U256 {
    match try_swap(amount_in, zero_for_one, sqrt_price_x96, tick, liquidity, fee_pips, tick_spacing, table) {
        Ok(out) => out,
        Err(_) => U256::ZERO,
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn try_swap(
    amount_in: U256,
    zero_for_one: bool,
    sqrt_price_x96: U256,
    start_tick: i32,
    start_liquidity: u128,
    fee_pips: u32,
    tick_spacing: i32,
    table: &TickTable,
) -> Result<U256, uniswap_v3_math::error::UniswapV3MathError> {
    if amount_in.is_zero() {
        return Ok(U256::ZERO);
    }
    let sqrt_price_limit = if zero_for_one {
        min_sqrt_ratio() + U256::from(1)
    } else {
        max_sqrt_ratio() - U256::from(1)
    };

    let mut amount_remaining = I256::from_raw(amount_in); // positive => exact input
    let mut amount_calculated = I256::ZERO;
    let mut sqrt_price = sqrt_price_x96;
    let mut tick = start_tick;
    let mut liquidity = start_liquidity;

    // Bound the number of tick crossings so a corrupt table can't spin forever.
    let mut guard = 0u32;

    while amount_remaining != I256::ZERO && sqrt_price != sqrt_price_limit {
        guard += 1;
        if guard > 1024 {
            break;
        }

        // Window guard: `next_initialized_tick_within_one_word` treats a word
        // MISSING from the map as EMPTY, i.e. "liquidity continues unchanged
        // across the word". True only for a full tick map; our table is a
        // hydrated window, and beyond it is UNKNOWN. Stop the walk there —
        // the quote becomes a safe lower bound instead of hallucinating depth.
        let compressed = if tick < 0 && tick % tick_spacing != 0 {
            tick / tick_spacing - 1
        } else {
            tick / tick_spacing
        };
        let probe_word = if zero_for_one {
            (compressed >> 8) as i16
        } else {
            ((compressed + 1) >> 8) as i16
        };
        if !table.word_in_window(probe_word) {
            break;
        }

        let sqrt_price_start = sqrt_price;
        let (mut next_tick, initialized) = uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word(
            &table.bitmap,
            tick,
            tick_spacing,
            zero_for_one,
        )?;
        next_tick = next_tick.clamp(MIN_TICK, MAX_TICK);

        let sqrt_price_next = tick_math::get_sqrt_ratio_at_tick(next_tick)?;

        // Clamp the step target to the swap's price limit.
        let sqrt_target = if (zero_for_one && sqrt_price_next < sqrt_price_limit)
            || (!zero_for_one && sqrt_price_next > sqrt_price_limit)
        {
            sqrt_price_limit
        } else {
            sqrt_price_next
        };

        let (sqrt_price_new, amount_in_step, amount_out_step, fee_amount) =
            swap_math::compute_swap_step(sqrt_price, sqrt_target, liquidity, amount_remaining, fee_pips)?;

        sqrt_price = sqrt_price_new;

        // Exact input: consume in + fee, accrue out (negative in the pool's frame).
        let consumed = I256::from_raw(amount_in_step + fee_amount);
        amount_remaining = amount_remaining - consumed;
        amount_calculated = amount_calculated - I256::from_raw(amount_out_step);

        if sqrt_price == sqrt_price_next {
            // Crossed to the next initialized tick.
            if initialized {
                let mut liquidity_net = table.liquidity_net(next_tick);
                if zero_for_one {
                    liquidity_net = -liquidity_net;
                }
                liquidity = liquidity_math::add_delta(liquidity, liquidity_net)?;
            }
            tick = if zero_for_one { next_tick - 1 } else { next_tick };
        } else if sqrt_price != sqrt_price_start {
            tick = tick_math::get_tick_at_sqrt_ratio(sqrt_price)?;
        }
    }

    // amount_calculated is negative (tokens leaving the pool); flip to positive out.
    let out = (-amount_calculated).into_raw();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniswap_v3_math::{tick_bitmap, tick_math};

    #[test]
    fn small_swap_in_deep_liquidity_is_near_spot() {
        // Pool at tick 0 (price 1.0), huge liquidity, no initialized ticks nearby
        // within the swap range => stays in-band. Small swap ~ 1:1 minus fee.
        let sqrt_price = tick_math::get_sqrt_ratio_at_tick(0).unwrap();
        let table = TickTable::default(); // no ticks => walk clamps at bounds
        let liquidity: u128 = 1_000_000_000_000_000_000_000u128; // deep
        let amount_in = U256::from(1_000_000u64);
        let out = get_amount_out(amount_in, true, sqrt_price, 0, liquidity, 3000, 60, &table);
        // With 0.3% fee, output slightly below input at price 1.0.
        assert!(out > U256::ZERO, "must produce output");
        assert!(out < amount_in, "fee must reduce output");
        // Within ~1% of input for such a small trade in deep liquidity.
        let lo = amount_in * U256::from(985u64) / U256::from(1000u64);
        assert!(out > lo, "out {out} too far below input");
    }

    #[test]
    fn zero_amount_gives_zero() {
        let sqrt_price = tick_math::get_sqrt_ratio_at_tick(0).unwrap();
        let table = TickTable::default();
        let out = get_amount_out(U256::ZERO, true, sqrt_price, 0, 1_000_000, 3000, 60, &table);
        assert_eq!(out, U256::ZERO);
    }

    /// In-memory TickSource for tests: same data a full-map TickTable holds
    /// -- every word is "known" (a missing entry means genuinely empty, not
    /// unhydrated), matching the old TickTable's unbounded-window mode.
    struct MapSource {
        bitmap: std::collections::HashMap<i16, U256>,
        ticks: std::collections::BTreeMap<i32, i128>,
    }
    impl TickSource for MapSource {
        fn bitmap_word(&self, word: i16) -> Option<U256> {
            Some(self.bitmap.get(&word).copied().unwrap_or_default())
        }
        fn liquidity_net(&self, tick: i32) -> i128 {
            self.ticks.get(&tick).copied().unwrap_or(0)
        }
    }

    /// Same data, but only words in `[min_word, max_word]` are "known" --
    /// anything outside is unhydrated, not empty. Models the real
    /// `ClTickReader`'s bounded-window behavior.
    struct WindowedMapSource {
        bitmap: std::collections::HashMap<i16, U256>,
        ticks: std::collections::BTreeMap<i32, i128>,
        min_word: i16,
        max_word: i16,
    }
    impl TickSource for WindowedMapSource {
        fn bitmap_word(&self, word: i16) -> Option<U256> {
            if word < self.min_word || word > self.max_word {
                return None;
            }
            Some(self.bitmap.get(&word).copied().unwrap_or_default())
        }
        fn liquidity_net(&self, tick: i32) -> i128 {
            self.ticks.get(&tick).copied().unwrap_or(0)
        }
    }

    fn lcg(state: &mut u64) -> u64 {
        *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *state >> 11
    }

    /// The lazy next-tick walk must agree with the crate's map-based
    /// implementation bit-for-bit: random bitmaps spanning negative/positive
    /// words, all production tick spacings, both directions.
    #[test]
    fn next_tick_src_matches_crate() {
        let mut seed = 0xba5a_2b_5eedu64;
        for &spacing in &[1i32, 10, 60, 200] {
            let mut bitmap = std::collections::HashMap::new();
            for word in -6i16..=6 {
                if lcg(&mut seed) % 3 == 0 {
                    continue; // leave some words empty
                }
                let hi = U256::from(lcg(&mut seed)) << 192;
                let mid = U256::from(lcg(&mut seed)) << 96;
                let lo = U256::from(lcg(&mut seed));
                bitmap.insert(word, hi | mid | lo);
            }
            let src = MapSource { bitmap: bitmap.clone(), ticks: Default::default() };
            for i in 0..2000 {
                // ticks spread across word boundaries, including negatives
                let raw = (lcg(&mut seed) % 3000) as i32 - 1500;
                let tick = raw * spacing + (i % spacing.max(1)); // off-grid too
                if !(MIN_TICK..=MAX_TICK).contains(&tick) {
                    continue;
                }
                for lte in [true, false] {
                    let ours = next_initialized_tick_src(&src, tick, spacing, lte)
                        .unwrap()
                        .expect("MapSource always reports words as known");
                    let theirs = tick_bitmap::next_initialized_tick_within_one_word(
                        &bitmap, tick, spacing, lte,
                    )
                    .unwrap();
                    assert_eq!(ours, theirs, "tick={tick} spacing={spacing} lte={lte}");
                }
            }
        }
    }

    /// Full swap walk: on a FULL map (unbounded table), the lazy-source walk
    /// must produce exactly the table walk's output for every size/direction.
    #[test]
    fn src_walk_matches_table_walk_on_full_map() {
        let mut seed = 42u64;
        let spacing = 60;
        let mut table = TickTable::default(); // unbounded = full-map semantics
        let mut ticks = std::collections::BTreeMap::new();
        let mut bitmap = std::collections::HashMap::new();
        for k in -40i32..=40 {
            if lcg(&mut seed) % 2 == 0 {
                continue;
            }
            let tick = k * spacing;
            let compressed = tick / spacing;
            let (word, bit) = ((compressed >> 8) as i16, (compressed & 0xff) as u8);
            *bitmap.entry(word).or_insert(U256::ZERO) |= U256::from(1u8) << bit;
            let net = (lcg(&mut seed) % 1_000_000_000_000) as i128 - 400_000_000_000;
            ticks.insert(tick, net);
        }
        table.bitmap = bitmap.clone();
        table.ticks = ticks.clone();
        let src = MapSource { bitmap, ticks };

        let sqrt_price = tick_math::get_sqrt_ratio_at_tick(7).unwrap();
        let liquidity: u128 = 5_000_000_000_000_000u128;
        for pow in [12u32, 15, 18, 21] {
            let amount_in = U256::from(10u128).pow(U256::from(pow));
            for zfo in [true, false] {
                let via_table = get_amount_out(
                    amount_in, zfo, sqrt_price, 7, liquidity, 3000, spacing, &table,
                );
                let via_src = get_amount_out_src(
                    amount_in, zfo, sqrt_price, 7, liquidity, 3000, spacing, &src,
                );
                assert_eq!(via_table, via_src, "pow={pow} zfo={zfo}");
            }
        }
    }

    #[test]
    fn walk_stops_at_window_edge() {
        // Same pool, same huge swap; the only difference is the hydrated window.
        // Unbounded (full-map semantics) keeps extrapolating liquidity forever;
        // a bounded window must yield strictly less output — never more.
        let sqrt_price = tick_math::get_sqrt_ratio_at_tick(0).unwrap();
        let liquidity: u128 = 1_000_000_000_000_000_000u128;
        // Big enough to walk far past one bitmap word (word = 256*spacing ticks).
        let amount_in = U256::from(10u128).pow(U256::from(24u64));

        let unbounded = TickTable::default();
        let out_unbounded =
            get_amount_out(amount_in, false, sqrt_price, 0, liquidity, 3000, 60, &unbounded);

        let mut windowed = TickTable::default();
        windowed.min_word = 0;
        windowed.max_word = 0; // knows only the current word
        let out_windowed =
            get_amount_out(amount_in, false, sqrt_price, 0, liquidity, 3000, 60, &windowed);

        assert!(out_windowed > U256::ZERO, "in-window portion must still fill");
        assert!(
            out_windowed < out_unbounded,
            "window cap must reduce output ({out_windowed} vs {out_unbounded})"
        );

        // A pool whose current price is OUTSIDE its window quotes zero.
        let elsewhere = TickTable { min_word: 100, max_word: 116, ..TickTable::default() };
        let out_outside =
            get_amount_out(amount_in, false, sqrt_price, 0, liquidity, 3000, 60, &elsewhere);
        assert_eq!(out_outside, U256::ZERO, "unknown territory must quote zero");
    }

    /// The regression this whole change fixes: `get_amount_out_src` (the
    /// LIVE production path, `ClTickReader` over `ChainState`) must ALSO
    /// stop at the edge of hydrated territory, not silently extrapolate
    /// through it. Before this fix, `bitmap_word` returning a plain `U256`
    /// (defaulting to zero on any cache miss) made an unhydrated word look
    /// identical to a genuinely empty one — the walk never knew to stop,
    /// so a large enough trade quoted MORE output than the real pool would
    /// give (confirmed live: 4.8-12% more, on real reverted routes, at the
    /// exact block the quote was computed from).
    #[test]
    fn src_walk_stops_at_window_edge() {
        let sqrt_price = tick_math::get_sqrt_ratio_at_tick(0).unwrap();
        let liquidity: u128 = 1_000_000_000_000_000_000u128;
        let amount_in = U256::from(10u128).pow(U256::from(24u64));

        let unbounded = MapSource { bitmap: Default::default(), ticks: Default::default() };
        let out_unbounded =
            get_amount_out_src(amount_in, false, sqrt_price, 0, liquidity, 3000, 60, &unbounded);

        let windowed = WindowedMapSource {
            bitmap: Default::default(),
            ticks: Default::default(),
            min_word: 0,
            max_word: 0,
        };
        let out_windowed =
            get_amount_out_src(amount_in, false, sqrt_price, 0, liquidity, 3000, 60, &windowed);

        assert!(out_windowed > U256::ZERO, "in-window portion must still fill");
        assert!(
            out_windowed < out_unbounded,
            "unhydrated territory must reduce output, not inflate it ({out_windowed} vs {out_unbounded})"
        );

        let elsewhere = WindowedMapSource {
            bitmap: Default::default(),
            ticks: Default::default(),
            min_word: 100,
            max_word: 116,
        };
        let out_outside =
            get_amount_out_src(amount_in, false, sqrt_price, 0, liquidity, 3000, 60, &elsewhere);
        assert_eq!(out_outside, U256::ZERO, "pool whose current word is unhydrated must quote zero, not hallucinate depth");
    }
}
