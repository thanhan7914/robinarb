//! Constant-product (Uniswap V2 + Aerodrome volatile) swap math.
//!
//! Exact integer form matches the on-chain `getAmountOut`:
//!   amountInWithFee = amountIn * (10000 - feeBps)
//!   out = (amountInWithFee * reserveOut) / (reserveIn * 10000 + amountInWithFee)

use alloy::primitives::U256;

/// Exact V2 output. `fee_bps` is the LP fee in basis points (30 = 0.30%).
pub fn get_amount_out(amount_in: U256, reserve_in: U256, reserve_out: U256, fee_bps: u32) -> U256 {
    if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
        return U256::ZERO;
    }
    let fee_factor = U256::from(10_000u32 - fee_bps);
    let amount_in_with_fee = amount_in * fee_factor;
    let numerator = amount_in_with_fee * reserve_out;
    let denominator = reserve_in * U256::from(10_000u32) + amount_in_with_fee;
    numerator / denominator
}

/// f64 fast path for the optimizer inner loop (ranking / ternary probes).
#[inline]
pub fn get_amount_out_f64(amount_in: f64, reserve_in: f64, reserve_out: f64, fee_bps: u32) -> f64 {
    if amount_in <= 0.0 || reserve_in <= 0.0 || reserve_out <= 0.0 {
        return 0.0;
    }
    let gamma = (10_000 - fee_bps as i64) as f64 / 10_000.0;
    let ain = amount_in * gamma;
    ain * reserve_out / (reserve_in + ain)
}

/// Coefficients of one V2 hop viewed as `f(x) = a*x / (b + c*x)`:
///   a = gamma * reserve_out, b = reserve_in, c = gamma   (gamma in 1e-scale as f64)
/// Used to compose a pure-V2 chain into a single rational function so the optimal
/// input has a closed form (see routing::optimizer).
#[derive(Clone, Copy, Debug)]
pub struct HopCoef {
    pub a: f64,
    pub b: f64,
    pub c: f64,
}

pub fn hop_coef(reserve_in: f64, reserve_out: f64, fee_bps: u32) -> HopCoef {
    let gamma = (10_000 - fee_bps as i64) as f64 / 10_000.0;
    HopCoef { a: gamma * reserve_out, b: reserve_in, c: gamma }
}

/// Compose g∘f for two hops of the rational family. If f=(a1,b1,c1) then g∘f is:
///   (a2*a1) / (b2*b1 + (b2*c1 + c2*a1) * x)
pub fn compose(f: HopCoef, g: HopCoef) -> HopCoef {
    HopCoef {
        a: g.a * f.a,
        b: g.b * f.b,
        c: g.b * f.c + g.c * f.a,
    }
}

#[cfg(test)]
impl HopCoef {
    fn eval(&self, x: f64) -> f64 {
        self.a * x / (self.b + self.c * x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_matches_reference() {
        // reserves 1000/2000, fee 0.30%, in=10
        let out = get_amount_out(
            U256::from(10u64),
            U256::from(1000u64),
            U256::from(2000u64),
            30,
        );
        // amountInWithFee = 10*9970 = 99700
        // num = 99700*2000 = 199_400_000
        // den = 1000*10000 + 99700 = 10_099_700
        // out = 19
        assert_eq!(out, U256::from(19u64));
    }

    #[test]
    fn compose_equals_sequential() {
        let f = hop_coef(1000.0, 2000.0, 30);
        let g = hop_coef(5000.0, 3000.0, 30);
        let x = 12.5;
        let seq = g.eval(f.eval(x));
        let comp = compose(f, g).eval(x);
        assert!((seq - comp).abs() < 1e-9, "seq={seq} comp={comp}");
    }

    #[test]
    fn closed_form_optimum_beats_grid() {
        // Profitable 2-hop cycle: rate1 * rate2 * gamma^2 > 1.
        // Hop1: 100 WETH / 200000 X (2000 X per WETH).
        // Hop2: 200000 X / 110 WETH (0.00055 WETH per X). Product 1.1 > 1.
        let h1 = hop_coef(100.0, 200_000.0, 30);
        let h2 = hop_coef(200_000.0, 110.0, 30);
        let coef = compose(h1, h2);
        let HopCoef { a, b, c } = coef;
        assert!(a > b, "must be profitable: a={a} b={b}");
        let x_star = ((a * b).sqrt() - b) / c;
        assert!(x_star > 0.0);
        let profit = |x: f64| coef.eval(x) - x;
        let p_star = profit(x_star);
        assert!(p_star > 0.0, "optimum must be profitable");
        // Fine grid bracketing x_star must not beat the closed form.
        let mut best = f64::MIN;
        let steps = 20_000;
        for i in 1..=steps {
            let x = x_star * 3.0 * (i as f64) / (steps as f64);
            best = best.max(profit(x));
        }
        assert!(p_star >= best - p_star.abs() * 1e-6, "closed form {p_star} < grid best {best}");
    }
}
