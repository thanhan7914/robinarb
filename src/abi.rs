//! All on-chain interfaces (alloy `sol!` bindings): events, view calls, executor calldata.
//!
//! Event topic gotchas handled here (see plan):
//! - Aerodrome v2 `Sync(uint256,uint256)` hashes differently from UniV2 `Sync(uint112,uint112)`.
//! - Pancake V3 `Swap` has two extra protocol-fee fields => different topic0 and decoder.

use alloy::sol;

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

sol! {
    /// Uniswap V2 family (Sushi/Pancake/BaseSwap... share this layout).
    #[derive(Debug)]
    contract UniV2Events {
        event Sync(uint112 reserve0, uint112 reserve1);
        event PairCreated(address indexed token0, address indexed token1, address pair, uint256 index);
    }
}

sol! {
    /// Aerodrome v2 (volatile + stable). NOTE: uint256 reserves => distinct topic0.
    #[derive(Debug)]
    contract AeroEvents {
        event Sync(uint256 reserve0, uint256 reserve1);
        event PoolCreated(address indexed token0, address indexed token1, bool indexed stable, address pool, uint256 index);
        event SetCustomFee(address indexed pool, uint256 fee);
    }
}

sol! {
    /// Uniswap V3 family (also Slipstream, and V3 forks except Pancake's Swap).
    #[derive(Debug)]
    contract UniV3Events {
        event Swap(
            address indexed sender,
            address indexed recipient,
            int256 amount0,
            int256 amount1,
            uint160 sqrtPriceX96,
            uint128 liquidity,
            int24 tick
        );
        event Mint(
            address sender,
            address indexed owner,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );
        event Burn(
            address indexed owner,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );
        event PoolCreated(
            address indexed token0,
            address indexed token1,
            uint24 indexed fee,
            int24 tickSpacing,
            address pool
        );
    }
}

sol! {
    /// Pancake V3: Swap carries protocol fee fields => different topic0.
    #[derive(Debug)]
    contract PancakeV3Events {
        event Swap(
            address indexed sender,
            address indexed recipient,
            int256 amount0,
            int256 amount1,
            uint160 sqrtPriceX96,
            uint128 liquidity,
            int24 tick,
            uint128 protocolFeesToken0,
            uint128 protocolFeesToken1
        );
    }
}

sol! {
    /// Algebra Integral (Hydrex, QuickSwap v4). Swap carries the post-swap
    /// sqrtPrice/liquidity/tick PLUS the effective fee charged (overrideFee) and
    /// the plugin's cut (pluginFee) — so the model re-prices the dynamic fee with
    /// zero extra RPC. Distinct topic0 (0x121cb44e…) vs UniV3's 0xc42079f9. Mint
    /// reuses UniV3Events::Mint (identical topic0); Burn has a distinct topic0 and
    /// is matched raw (see dexes/algebra.rs ALGEBRA_BURN_TOPIC) so its exact ABI
    /// need not be pinned.
    #[derive(Debug)]
    contract AlgebraEvents {
        event Swap(
            address indexed sender,
            address indexed recipient,
            int256 amount0,
            int256 amount1,
            uint160 price,
            uint128 liquidity,
            int24 tick,
            uint24 overrideFee,
            uint24 pluginFee
        );
    }
}

sol! {
    /// Algebra Integral pool reads. `globalState()` replaces UniV3's `slot0()`
    /// (slot0 reverts on these pools); `tickTable(int16)` is the UniV3-style word
    /// bitmap (reused by the tick-window hydration); `ticks()` returns the Algebra
    /// tick struct where `liquidityDelta` is the UniV3 `liquidityNet`.
    #[sol(rpc)]
    #[derive(Debug)]
    contract IAlgebraPool {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function tickSpacing() external view returns (int24);
        function liquidity() external view returns (uint128);
        function fee() external view returns (uint16);
        function globalState() external view returns (
            uint160 price,
            int24 tick,
            uint16 lastFee,
            uint8 pluginConfig,
            uint16 communityFee,
            bool unlocked
        );
        function tickTable(int16 wordPosition) external view returns (uint256);
        function ticks(int24 tick) external view returns (
            uint256 liquidityTotal,
            int128 liquidityDelta,
            int24 prevTick,
            int24 nextTick,
            uint256 outerFeeGrowth0Token,
            uint256 outerFeeGrowth1Token
        );
    }
}

sol! {
    /// Aerodrome Slipstream CLFactory: pools keyed by tickSpacing, not fee.
    #[derive(Debug)]
    contract SlipstreamEvents {
        event PoolCreated(
            address indexed token0,
            address indexed token1,
            int24 indexed tickSpacing,
            address pool
        );
    }
}

sol! {
    /// Uniswap V4: every event is emitted by the singleton PoolManager with
    /// the pool's bytes32 id as topics[1] (PoolId/Currency/IHooks are bytes32/
    /// address/address on the wire). ModifyLiquidity replaces V3's Mint/Burn;
    /// Swap carries post-swap sqrtPrice/liquidity/tick plus the fee charged.
    #[derive(Debug)]
    contract UniV4Events {
        event Initialize(
            bytes32 indexed id,
            address indexed currency0,
            address indexed currency1,
            uint24 fee,
            int24 tickSpacing,
            address hooks,
            uint160 sqrtPriceX96,
            int24 tick
        );
        event Swap(
            bytes32 indexed id,
            address indexed sender,
            int128 amount0,
            int128 amount1,
            uint160 sqrtPriceX96,
            uint128 liquidity,
            int24 tick,
            uint24 fee
        );
        event ModifyLiquidity(
            bytes32 indexed id,
            address indexed sender,
            int24 tickLower,
            int24 tickUpper,
            int256 liquidityDelta,
            bytes32 salt
        );
        event ProtocolFeeUpdated(bytes32 indexed id, uint24 protocolFee);
    }
}

sol! {
    /// Uniswap V4 PoolKey — poolId = keccak256(abi.encode(key)). Vanilla-only
    /// policy means hooks is always address(0) in every key we build.
    #[derive(Debug)]
    struct V4PoolKey {
        address currency0;
        address currency1;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
    }
}

// ---------------------------------------------------------------------------
// View interfaces (hydration / verification)
// ---------------------------------------------------------------------------

sol! {
    #[sol(rpc)]
    #[derive(Debug)]
    contract IERC20 {
        function decimals() external view returns (uint8);
        function symbol() external view returns (string);
        function balanceOf(address) external view returns (uint256);
    }
}

sol! {
    #[sol(rpc)]
    #[derive(Debug)]
    contract IUniV2Pair {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    }
}

sol! {
    #[sol(rpc)]
    #[derive(Debug)]
    contract IAeroPool {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function stable() external view returns (bool);
        function getReserves() external view returns (uint256 reserve0, uint256 reserve1, uint256 blockTimestampLast);
        function getAmountOut(uint256 amountIn, address tokenIn) external view returns (uint256);
    }
}

sol! {
    #[sol(rpc)]
    #[derive(Debug)]
    contract IAeroFactory {
        function getFee(address pool, bool stable) external view returns (uint256);
    }
}

sol! {
    /// Works for UniV3 forks AND Slipstream/Pancake for the fields we read.
    /// slot0's return layouts differ per fork after the first two words, so raw
    /// hydration (DataSync.sol) only decodes (sqrtPriceX96, tick).
    #[sol(rpc)]
    #[derive(Debug)]
    contract IUniV3Pool {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
        function liquidity() external view returns (uint128);
        /// slot0 layout diverges across forks after the first two words; callers
        /// decode only (sqrtPriceX96, tick) from the raw return.
        function slot0() external view returns (uint160 sqrtPriceX96, int24 tick);
        function tickBitmap(int16 wordPosition) external view returns (uint256);
        function ticks(int24 tick) external view returns (
            uint128 liquidityGross,
            int128 liquidityNet,
            uint256 feeGrowthOutside0X128,
            uint256 feeGrowthOutside1X128,
            int56 tickCumulativeOutside,
            uint160 secondsPerLiquidityOutsideX128,
            uint32 secondsOutside,
            bool initialized
        );
    }
}

sol! {
    /// Uniswap V4 StateView periphery (wraps PoolManager.extsload). All reads
    /// are keyed by poolId and Multicall3-batchable like any view call.
    #[sol(rpc)]
    #[derive(Debug)]
    contract IStateView {
        function getSlot0(bytes32 poolId) external view returns (uint160 sqrtPriceX96, int24 tick, uint24 protocolFee, uint24 lpFee);
        function getLiquidity(bytes32 poolId) external view returns (uint128 liquidity);
        function getTickBitmap(bytes32 poolId, int16 tick) external view returns (uint256 tickBitmap);
        function getTickLiquidity(bytes32 poolId, int24 tick) external view returns (uint128 liquidityGross, int128 liquidityNet);
    }
}

sol! {
    /// Official V4Quoter (revert-trick over PoolManager.unlock) — verification
    /// only. The PoolKey must carry the RAW currencies (0x0 = native ETH):
    /// passing normalized WETH quotes a DIFFERENT pool.
    #[sol(rpc)]
    #[derive(Debug)]
    contract IV4Quoter {
        struct PoolKey {
            address currency0;
            address currency1;
            uint24 fee;
            int24 tickSpacing;
            address hooks;
        }
        struct QuoteExactSingleParams {
            PoolKey poolKey;
            bool zeroForOne;
            uint128 exactAmount;
            bytes hookData;
        }
        function quoteExactInputSingle(QuoteExactSingleParams memory params) external returns (uint256 amountOut, uint256 gasEstimate);
    }
}

sol! {
    #[sol(rpc)]
    #[derive(Debug)]
    contract IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct Result {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls) external payable returns (Result[] memory returnData);
    }
}

// Arbitrum ArbGasInfo precompile (0x...6C on every Arbitrum chain, including
// Robinhood Chain — see constants::ARB_GAS_INFO). Signatures verbatim from
// OffchainLabs/nitro solgen/src/precompiles/ArbGasInfo.sol — Arbitrum's
// method set does not map 1:1 to OP-stack's GasPriceOracle 4-getter shape
// it replaces.
sol! {
    #[sol(rpc)]
    #[derive(Debug)]
    contract IArbGasInfo {
        /// (perL2Tx, perL1CalldataUnit [a byte, non-zero or otherwise, is 16
        /// units], perStorageAlloc, perArbGasBase, perArbGasCongestion,
        /// perArbGasTotal) — all in wei. Uses the caller's preferred
        /// aggregator, or the chain default.
        function getPricesInWei() external view returns (
            uint256 perL2Tx,
            uint256 perL1CalldataUnit,
            uint256 perStorageAlloc,
            uint256 perArbGasBase,
            uint256 perArbGasCongestion,
            uint256 perArbGasTotal
        );
        /// The system's current estimate of the parent chain (L1) base fee.
        function getL1BaseFeeEstimate() external view returns (uint256);
        /// Minimum gas price needed for a transaction to succeed, in wei.
        function getMinimumGasPrice() external view returns (uint256);
    }
}

sol! {
    /// Deployless generic V3-family quoter (contracts/src/PoolQuoter.sol),
    /// injected via eth_call state override — verification tooling only.
    /// Addressed by pool, so it covers every UniV3-style fork regardless of
    /// factory, with the pool's current (possibly dynamic) fee.
    #[sol(rpc)]
    #[derive(Debug)]
    contract IPoolQuoter {
        function quotePool(address pool, bool zeroForOne, uint256 amountIn)
            external
            returns (uint256 amountOut);
    }
}

sol! {
    /// Fee-on-transfer / rebase detector (contracts/src/FeeProbe.sol),
    /// injected via eth_call state override at a REAL holder of the token
    /// under test (its code is swapped in; its storage — hence its real
    /// balance — is left alone). See `ingest/fee_probe.rs`.
    #[sol(rpc)]
    #[derive(Debug)]
    contract IFeeProbe {
        function probe(address token, address relay, address dummy, uint256 amount) external returns (uint256 received);
    }
}

// ---------------------------------------------------------------------------
// ArbExecutor calldata (matches contracts/src/ArbExecutor.sol)
// ---------------------------------------------------------------------------

sol! {
    #[derive(Debug)]
    contract IArbExecutor {
        struct Hop {
            uint8 kind;       // 0 = V2 fork, 1 = Aerodrome vol/stable, 2 = V3 family, 3 = UniV4
            address pool;     // kind 3: zero (V4 pools have no address)
            bool zeroForOne;
            uint16 feeBps;    // kind 0: LP fee in bps; kind 3: index into v4Keys
        }
        /// V4 PoolKey material, RAW currencies (0x0 = native ETH); hooks is
        /// always 0 and added by the contract.
        struct V4Key {
            address currency0;
            address currency1;
            uint24 fee;
            int24 tickSpacing;
        }
        function executeArb(Hop[] calldata hops, V4Key[] calldata v4Keys, address anchor, uint256 amountIn, uint256 minProfit, bool useFlashloan) external;
        function withdraw(address token, uint256 amount) external;
    }
}

// ---------------------------------------------------------------------------
// Deployless hydration (DataSync.sol / TickSync.sol constructor-return trick).
// The creation bytecode is loaded at runtime from Foundry artifacts if present;
// these types only describe the ABI-encoded payloads returned by the constructors.
// ---------------------------------------------------------------------------

sol! {
    #[derive(Debug)]
    struct PoolSnapV2 {
        address pool;
        address token0;
        address token1;
        uint8 dec0;
        uint8 dec1;
        uint112 reserve0;
        uint112 reserve1;
    }

    #[derive(Debug)]
    struct PoolSnapAero {
        address pool;
        address token0;
        address token1;
        uint8 dec0;
        uint8 dec1;
        bool stable;
        uint256 reserve0;
        uint256 reserve1;
        uint256 feeBps; // from factory.getFee (bps)
    }

    #[derive(Debug)]
    struct PoolSnapV3 {
        address pool;
        address token0;
        address token1;
        uint8 dec0;
        uint8 dec1;
        uint24 fee;
        int24 tickSpacing;
        uint160 sqrtPriceX96;
        int24 tick;
        uint128 liquidity;
    }

    #[derive(Debug)]
    struct TickSnap {
        int24 tick;
        int128 liquidityNet;
    }
}
