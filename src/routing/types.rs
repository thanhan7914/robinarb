//! Route + hop types. A route is an anchor->...->anchor cycle over 2 or 3
//! pools, where "anchor" is one of the configured base tokens (WETH/USDC/
//! cbBTC) — see `config::AnchorConfig`.

use crate::engine::PoolIdx;
use alloy::primitives::Address;
use smallvec::SmallVec;

/// One swap leg: which pool, and the input-token side.
#[derive(Debug, Clone, Copy)]
pub struct Hop {
    pub pool: PoolIdx,
    /// true = input token is pool.token0.
    pub zero_for_one: bool,
    /// Token flowing INTO this hop (== previous hop's output; first == route.anchor).
    pub token_in: Address,
}

/// A closed arbitrage cycle starting and ending at `anchor`.
#[derive(Debug, Clone)]
pub struct Route {
    pub id: u32,
    /// The base token this cycle starts/ends at (amount_in/gross_profit are
    /// denominated in this token's smallest unit).
    pub anchor: Address,
    pub hops: SmallVec<[Hop; 3]>,
}

