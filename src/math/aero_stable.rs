//! Aerodrome (Solidly) stable-pool swap math: invariant k = x^3·y + x·y^3 on
//! decimal-normalized balances (each reserve scaled to 1e18). Mirrors the
//! on-chain `Pool.getAmountOut` / `_get_y` Newton iteration exactly.
//!
//! On-chain reference (Aerodrome Pool.sol):
//!   xy = _k(x0, y)              // invariant at 1e18 scale
//!   _get_y solves for new y via Newton's method
//!   amountOut = (reserveOut_scaled - y) rescaled back to token decimals
//!
//! All intermediate math uses U256 at 1e18 fixed point.

use alloy::primitives::U256;

fn e18() -> U256 {
    U256::from(1_000_000_000_000_000_000u64)
}

/// k = x^3·y + x·y^3, all at 1e18 scale (matches Pool._k for stable pools).
fn k(x: U256, y: U256) -> U256 {
    let unit = e18();
    // _a = (x * y) / 1e18
    let a = x * y / unit;
    // _b = (x*x/1e18 + y*y/1e18)
    let b = (x * x / unit) + (y * y / unit);
    // k = _a * _b / 1e18
    a * b / unit
}

/// f(x0, y) = x0^3·y + x0·y^3 at 1e18 scale (the value the Newton step targets).
fn f(x0: U256, y: U256) -> U256 {
    let unit = e18();
    // x0*y^3/1e18^3 + x0^3*y/1e18^3, computed stepwise like the contract:
    // _a = x0 * ((y*y/1e18)*y/1e18) / 1e18
    let y2 = y * y / unit;
    let y3 = y2 * y / unit;
    let a = x0 * y3 / unit;
    // _b = ((x0*x0/1e18)*x0/1e18) * y / 1e18
    let x2 = x0 * x0 / unit;
    let x3 = x2 * x0 / unit;
    let b = x3 * y / unit;
    a + b
}

/// d/dy of f = 3·x0·y^2 + x0^3 (at 1e18 scale), the Newton derivative.
fn d(x0: U256, y: U256) -> U256 {
    let unit = e18();
    let three = U256::from(3u64);
    let y2 = y * y / unit;
    let a = three * x0 * y2 / unit;
    let x2 = x0 * x0 / unit;
    let x3 = x2 * x0 / unit;
    a + x3
}

/// Solve for new y given x0 and target invariant `xy`, Newton's method (255 iters
/// max like the contract).
fn get_y(x0: U256, xy: U256, mut y: U256) -> U256 {
    let unit = e18();
    for _ in 0..255 {
        let y_prev = y;
        let k_val = f(x0, y);
        if k_val < xy {
            let dy = (xy - k_val) * unit / d(x0, y);
            y = y + dy;
        } else {
            let dy = (k_val - xy) * unit / d(x0, y);
            y = y - dy;
        }
        if y > y_prev {
            if y - y_prev <= U256::from(1u64) {
                return y;
            }
        } else if y_prev - y <= U256::from(1u64) {
            return y;
        }
    }
    y
}

/// Exact stable-pool output.
/// `reserve_in`/`reserve_out` are raw token amounts; `dec_in`/`dec_out` are 10^decimals.
pub fn get_amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    dec_in: U256,
    dec_out: U256,
    fee_bps: u32,
) -> U256 {
    if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
        return U256::ZERO;
    }
    let unit = e18();

    // Take fee off the input (Aerodrome: amountIn -= amountIn * fee / 10000).
    let fee = amount_in * U256::from(fee_bps) / U256::from(10_000u32);
    let amount_in_after = amount_in - fee;

    // Normalize to 1e18.
    let x0 = reserve_in * unit / dec_in;
    let y0 = reserve_out * unit / dec_out;
    let amt = amount_in_after * unit / dec_in;

    let xy = k(x0, y0);
    let x_new = x0 + amt;
    let y_new = get_y(x_new, xy, y0);
    let out_scaled = y0 - y_new;

    // Back to token-out decimals.
    out_scaled * dec_out / unit
}

/// f64 fast path for the optimizer inner loop (approximate; exact form gates the trade).
pub fn get_amount_out_f64(
    amount_in: f64,
    reserve_in: f64,
    reserve_out: f64,
    dec_in: f64,
    dec_out: f64,
    fee_bps: u32,
) -> f64 {
    if amount_in <= 0.0 || reserve_in <= 0.0 || reserve_out <= 0.0 {
        return 0.0;
    }
    let gamma = (10_000 - fee_bps as i64) as f64 / 10_000.0;
    let x0 = reserve_in / dec_in;
    let y0 = reserve_out / dec_out;
    let amt = amount_in * gamma / dec_in;

    let kk = x0 * x0 * x0 * y0 + x0 * y0 * y0 * y0;
    let x_new = x0 + amt;
    // Newton solve in f64.
    let mut y = y0;
    for _ in 0..64 {
        let fy = x_new * x_new * x_new * y + x_new * y * y * y;
        let dy = 3.0 * x_new * y * y + x_new * x_new * x_new;
        let step = (fy - kk) / dy;
        let ny = y - step;
        if (ny - y).abs() < 1e-18 {
            y = ny;
            break;
        }
        y = ny;
    }
    let out = (y0 - y) * dec_out;
    out.max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_swap_near_balance_is_low_slippage() {
        // Balanced 1e24 / 1e24 stable pool, 18 decimals, 5 bps fee, swap 1e21.
        let dec = U256::from(10u64).pow(U256::from(18));
        let r = U256::from(10u64).pow(U256::from(24));
        let amt = U256::from(10u64).pow(U256::from(21));
        let out = get_amount_out(amt, r, r, dec, dec, 5);
        // Near balance the stable curve gives ~1:1 minus fee; output within 1% of input.
        let out_f = crate::math::u256_to_f64(out);
        let in_f = crate::math::u256_to_f64(amt);
        assert!(out_f > in_f * 0.98 && out_f < in_f, "out={out_f} in={in_f}");
    }

    #[test]
    fn stable_output_below_input_after_fee() {
        let dec = U256::from(10u64).pow(U256::from(18));
        let r = U256::from(10u64).pow(U256::from(24));
        let amt = U256::from(10u64).pow(U256::from(20));
        let out = get_amount_out(amt, r, r, dec, dec, 5);
        assert!(out < amt, "stable swap must lose fee");
        assert!(!out.is_zero());
    }
}
