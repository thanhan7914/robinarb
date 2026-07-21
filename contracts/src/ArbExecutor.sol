// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

// ============================================================================
// Robinhood Chain (4663) arbitrage executor.
//
// Flashloan source is Morpho Blue -- Robinhood's own first-party lending
// backend (powers the "Robinhood Earn" product), fee-free by protocol
// design. Address 0x9D53d5E3bd5E8d4Cbfa6DB1ca238AEA02E651010. Interface
// signatures (flashLoan / onMorphoFlashLoan) verified against
// github.com/morpho-org/morpho-blue's actual Morpho.sol source, not guessed
// -- in particular that repayment is pulled via `safeTransferFrom` AFTER the
// callback returns (not a plain transfer during it), which is why
// `onMorphoFlashLoan` below ends with an `approve`, not a `transfer`.
//
// `forge build` passing means the Solidity is well-formed, not that the
// flashloan flow is correct -- run an end-to-end fork test of `executeArb`
// before any real capital goes near this.
// ============================================================================

interface IERC20 {
    function transfer(address, uint256) external returns (bool);
    function approve(address, uint256) external returns (bool);
    function balanceOf(address) external view returns (uint256);
}

interface IUniV2Pair {
    function getReserves() external view returns (uint112, uint112, uint32);
    function token0() external view returns (address);
    function token1() external view returns (address);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

interface IAeroPool {
    function getAmountOut(uint256 amountIn, address tokenIn) external view returns (uint256);
    function token0() external view returns (address);
    function token1() external view returns (address);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

interface IUniV3Pool {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function swap(
        address recipient,
        bool zeroForOne,
        int256 amountSpecified,
        uint160 sqrtPriceLimitX96,
        bytes calldata data
    ) external returns (int256 amount0, int256 amount1);
}

/// Morpho Blue core singleton -- flashLoan signature verified verbatim
/// against github.com/morpho-org/morpho-blue's Morpho.sol.
interface IMorpho {
    function flashLoan(address token, uint256 assets, bytes calldata data) external;
}

/// Callback Morpho invokes mid-flashLoan, before pulling repayment.
interface IMorphoFlashLoanCallback {
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external;
}

interface IWETH {
    function deposit() external payable;
    function withdraw(uint256) external;
}

/// Minimal Uniswap V4 PoolManager surface (singleton, flash accounting).
/// BalanceDelta is an int256 packing two int128s: amount0 = high 128 bits,
/// amount1 = low 128 bits; positive = credit owed to us, negative = we owe.
interface IPoolManager {
    struct PoolKey {
        address currency0; // address(0) = native ETH
        address currency1;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
    }

    struct SwapParams {
        bool zeroForOne;
        int256 amountSpecified; // negative = exact input
        uint160 sqrtPriceLimitX96;
    }

    function unlock(bytes calldata data) external returns (bytes memory);
    function swap(PoolKey memory key, SwapParams memory params, bytes calldata hookData)
        external
        returns (int256 swapDelta);
    function sync(address currency) external;
    function settle() external payable returns (uint256 paid);
    function take(address currency, address to, uint256 amount) external;
}

/// Atomic arbitrage executor for Robinhood Chain. Supports self-funded and
/// Morpho Blue flashloan capital in any of the caller-supplied anchor tokens
/// (WETH, USDG — see `executeArb`'s `anchor` param). One `Hop[]` route is
/// executed in sequence; each hop's output token feeds the next. Callback
/// auth uses transient storage.
contract ArbExecutor is IMorphoFlashLoanCallback {
    struct Hop {
        uint8 kind; // 0 = UniV2 fork, 1 = Aerodrome (vol/stable), 2 = UniV3 family, 3 = UniV4, 4 = Algebra
        address pool; // kind 3: unused (zero) — the pool has no address
        bool zeroForOne;
        uint16 feeBps; // kind 0: LP fee in bps; kind 3: index into the v4Keys array
    }

    /// Uniswap V4 PoolKey material, one entry per V4 hop in the route (the
    /// encoder does not dedupe repeat pools; hooks is always 0 under the
    /// vanilla-only policy, inserted by the contract). Kept out of Hop so
    /// routes without V4 pay no extra calldata.
    struct V4Key {
        address currency0; // address(0) = native ETH
        address currency1;
        uint24 fee;
        int24 tickSpacing;
    }

    address public owner;
    mapping(address => bool) public executors;

    address public constant WETH = 0x0Bd7D308f8E1639FAb988df18A8011f41EAcAD73;
    address public constant MORPHO = 0x9D53d5E3bd5E8d4Cbfa6DB1ca238AEA02E651010;
    address public constant POOL_MANAGER = 0x8366a39CC670B4001A1121B8F6A443A643e40951;
    uint160 internal constant MIN_SQRT_RATIO = 4295128740; // 4295128739 + 1
    uint160 internal constant MAX_SQRT_RATIO =
        1461446703485210103287273052203988822378723970341; // MAX - 1

    // Transient callback guard.
    address private transient expectedCaller;
    bool private transient inFlight;
    bool private transient inV4;

    error NotOwner();
    error NotExecutor();
    error ProfitTooLow();
    error BadCallback();

    constructor() {
        owner = msg.sender;
        executors[msg.sender] = true;
    }

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    modifier onlyExecutor() {
        if (!executors[msg.sender]) revert NotExecutor();
        _;
    }

    function setExecutor(address who, bool ok) external onlyOwner {
        executors[who] = ok;
    }

    function transferOwnership(address next) external onlyOwner {
        owner = next;
    }

    function withdraw(address token, uint256 amount) external onlyOwner {
        if (token == address(0)) {
            // Rescue native ETH (receive() is open; dust could strand here).
            (bool ok,) = owner.call{value: amount}("");
            if (!ok) revert BadCallback();
            return;
        }
        IERC20(token).transfer(owner, amount);
    }

    /// One-time setup per anchor token: MORPHO is a fixed constant (never
    /// route-supplied), so a max approval is safe and lets
    /// `onMorphoFlashLoan` skip re-approving on every single flashloan call.
    /// Call once per anchor right after deployment — a per-tx `approve()`
    /// would cost an SSTORE (~2,900-20,000 gas, always cold since it's a
    /// fresh tx every time) on EVERY flashloan send, landed or reverted.
    function approveMorphoMax(address token) external onlyOwner {
        IERC20(token).approve(MORPHO, type(uint256).max);
    }

    /// Execute a route. `anchor` is the token the route starts/ends at (WETH
    /// or USDG) — `amountIn` is denominated in it. Reverts (losing only gas)
    /// if the realized `anchor` gain is below `minProfit`.
    function executeArb(
        Hop[] calldata hops,
        V4Key[] calldata v4Keys,
        address anchor,
        uint256 amountIn,
        uint256 minProfit,
        bool useFlashloan
    ) external onlyExecutor {
        if (useFlashloan) {
            inFlight = true;
            IMorpho(MORPHO).flashLoan(anchor, amountIn, abi.encode(hops, v4Keys, anchor, minProfit));
            inFlight = false;
        } else {
            uint256 start = IERC20(anchor).balanceOf(address(this));
            _runHops(hops, v4Keys, amountIn);
            uint256 end = IERC20(anchor).balanceOf(address(this));
            if (end < start + minProfit) revert ProfitTooLow();
        }
    }

    /// Morpho Blue flashloan callback. Morpho already transferred `assets`
    /// of the flash-loaned token to us BEFORE calling this (see
    /// Morpho.sol's flashLoan: `safeTransfer` then call then
    /// `safeTransferFrom`) — so unlike Balancer's push-style repay, we do
    /// NOT transfer anything back here. Morpho pulls `assets` itself via
    /// `safeTransferFrom` immediately after this function returns. Fee-free
    /// by protocol design (confirmed against Morpho.sol source, not
    /// assumed) — no fee amount to add to the repay bar, unlike Balancer's
    /// version.
    ///
    /// Repayment relies on a standing max approval to MORPHO set once via
    /// `approveMorphoMax` — NOT a per-call `approve()` here (MORPHO is a
    /// fixed constant, never route-supplied, so a standing max approval is
    /// safe and avoids an SSTORE on every flashloan send). Deploy-time setup
    /// must call `approveMorphoMax(anchor)` for every anchor token before
    /// this contract can actually flashloan it — Morpho's `safeTransferFrom`
    /// will revert on insufficient allowance otherwise, same as any
    /// missing-approval mistake.
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external {
        if (msg.sender != MORPHO) revert BadCallback();
        if (!inFlight) revert BadCallback();

        (Hop[] memory hops, V4Key[] memory v4Keys, address anchor, uint256 minProfit) =
            abi.decode(data, (Hop[], V4Key[], address, uint256));
        uint256 start = IERC20(anchor).balanceOf(address(this));
        _runHops(hops, v4Keys, assets);

        uint256 end = IERC20(anchor).balanceOf(address(this));
        // Delta check: `start` already includes the borrowed `assets`, so
        // this requires the hops to have returned assets + minProfit (no
        // fee term — see doc above). Absolute-balance check, not a
        // profit-only check — a relative check alone lets pre-existing
        // contract balance quietly subsidize a losing arb.
        if (end < start + minProfit) revert ProfitTooLow();
    }

    function _runHops(Hop[] memory hops, V4Key[] memory v4Keys, uint256 amountIn) internal virtual {
        uint256 amount = amountIn;
        uint256 i = 0;
        while (i < hops.length) {
            uint8 k = hops[i].kind;
            if (k == 0) {
                amount = _swapV2(hops[i], amount);
                i++;
            } else if (k == 1) {
                amount = _swapAero(hops[i], amount);
                i++;
            } else if (k == 2 || k == 4) {
                // Algebra (kind 4) shares UniV3's swap selector and exact-input
                // (positive amountSpecified) convention — only its callback name
                // differs (see algebraSwapCallback). Same swap path.
                amount = _swapV3(hops[i], amount);
                i++;
            } else {
                // Maximal run of V4 hops whose connecting currencies actually
                // match, executed inside ONE unlock (intermediate deltas net to
                // zero — no transfers, no wrap). The currency check matters:
                // the router normalizes native ETH to WETH, so two adjacent V4
                // hops can both say "WETH" at the route level while one uses
                // native ETH and the other real WETH — those must settle
                // separately (the boundary wrap/unwrap reconciles them).
                uint256 j = i + 1;
                while (
                    j < hops.length && hops[j].kind == 3
                        && _v4OutCurrency(hops[j - 1], v4Keys) == _v4InCurrency(hops[j], v4Keys)
                ) {
                    j++;
                }
                amount = _runV4Segment(hops, v4Keys, i, j, amount);
                i = j;
            }
        }
    }

    function _swapV2(Hop memory h, uint256 amountIn) internal returns (uint256 amountOut) {
        IUniV2Pair pair = IUniV2Pair(h.pool);
        (uint112 r0, uint112 r1,) = pair.getReserves();
        (uint256 rIn, uint256 rOut) =
            h.zeroForOne ? (uint256(r0), uint256(r1)) : (uint256(r1), uint256(r0));
        address tokenIn = h.zeroForOne ? pair.token0() : pair.token1();

        uint256 amountInWithFee = amountIn * (10000 - h.feeBps);
        amountOut = (amountInWithFee * rOut) / (rIn * 10000 + amountInWithFee);

        IERC20(tokenIn).transfer(h.pool, amountIn);
        (uint256 a0, uint256 a1) = h.zeroForOne ? (uint256(0), amountOut) : (amountOut, uint256(0));
        pair.swap(a0, a1, address(this), "");
    }

    function _swapAero(Hop memory h, uint256 amountIn) internal returns (uint256 amountOut) {
        IAeroPool pool = IAeroPool(h.pool);
        address tokenIn = h.zeroForOne ? pool.token0() : pool.token1();
        amountOut = pool.getAmountOut(amountIn, tokenIn);

        IERC20(tokenIn).transfer(h.pool, amountIn);
        (uint256 a0, uint256 a1) = h.zeroForOne ? (uint256(0), amountOut) : (amountOut, uint256(0));
        pool.swap(a0, a1, address(this), "");
    }

    function _swapV3(Hop memory h, uint256 amountIn) internal returns (uint256 amountOut) {
        expectedCaller = h.pool;
        (int256 amount0, int256 amount1) = IUniV3Pool(h.pool).swap(
            address(this),
            h.zeroForOne,
            int256(amountIn),
            h.zeroForOne ? MIN_SQRT_RATIO : MAX_SQRT_RATIO,
            abi.encode(h.zeroForOne)
        );
        expectedCaller = address(0);
        // Output is the negative delta.
        amountOut = uint256(-(h.zeroForOne ? amount1 : amount0));
    }

    function _v4InCurrency(Hop memory h, V4Key[] memory keys) internal pure returns (address) {
        V4Key memory k = keys[h.feeBps];
        return h.zeroForOne ? k.currency0 : k.currency1;
    }

    function _v4OutCurrency(Hop memory h, V4Key[] memory keys) internal pure returns (address) {
        V4Key memory k = keys[h.feeBps];
        return h.zeroForOne ? k.currency1 : k.currency0;
    }

    /// Execute hops [i, j) — all V4 with matching connecting currencies —
    /// inside one PoolManager.unlock. The route-level token at both edges is
    /// WETH (or a mid-route ERC20); if the pool edge is native ETH the wrap/
    /// unwrap happens here at the segment boundary.
    function _runV4Segment(
        Hop[] memory hops,
        V4Key[] memory keys,
        uint256 i,
        uint256 j,
        uint256 amountIn
    ) internal returns (uint256 amountOut) {
        uint256 n = j - i;
        V4Key[] memory segKeys = new V4Key[](n);
        bool[] memory dirs = new bool[](n);
        for (uint256 k = 0; k < n; k++) {
            segKeys[k] = keys[hops[i + k].feeBps];
            dirs[k] = hops[i + k].zeroForOne;
        }
        inV4 = true;
        bytes memory res = IPoolManager(POOL_MANAGER).unlock(abi.encode(segKeys, dirs, amountIn));
        inV4 = false;
        amountOut = abi.decode(res, (uint256));
    }

    /// PoolManager flash-accounting callback: run the segment's swaps, then
    /// settle the net input and take the net output. Intermediate currencies
    /// cancel out inside the accounting and never touch this contract.
    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        if (msg.sender != POOL_MANAGER) revert BadCallback();
        if (!inV4) revert BadCallback();
        (V4Key[] memory keys, bool[] memory dirs, uint256 amountIn) =
            abi.decode(data, (V4Key[], bool[], uint256));

        uint256 amount = amountIn;
        for (uint256 k = 0; k < keys.length; k++) {
            int256 delta = IPoolManager(POOL_MANAGER).swap(
                IPoolManager.PoolKey(
                    keys[k].currency0, keys[k].currency1, keys[k].fee, keys[k].tickSpacing, address(0)
                ),
                IPoolManager.SwapParams(
                    dirs[k],
                    -int256(amount), // negative = exact input
                    dirs[k] ? MIN_SQRT_RATIO : MAX_SQRT_RATIO
                ),
                ""
            );
            // BalanceDelta: amount0 = high int128, amount1 = low int128;
            // the output side is a positive credit. A non-positive value means
            // the hop encoding is inconsistent with the key's currency order —
            // fail loudly instead of wrapping to a huge uint.
            int128 outSigned = dirs[k] ? int128(int256(delta)) : int128(delta >> 128);
            if (outSigned <= 0) revert BadCallback();
            amount = uint256(uint128(outSigned));
        }

        // Settle what we owe (the segment input).
        address inCur = dirs[0] ? keys[0].currency0 : keys[0].currency1;
        if (inCur == address(0)) {
            IWETH(WETH).withdraw(amountIn);
            IPoolManager(POOL_MANAGER).settle{value: amountIn}();
        } else {
            IPoolManager(POOL_MANAGER).sync(inCur);
            IERC20(inCur).transfer(POOL_MANAGER, amountIn);
            IPoolManager(POOL_MANAGER).settle();
        }
        // Take what we're owed (the segment output).
        uint256 last = keys.length - 1;
        address outCur = dirs[last] ? keys[last].currency1 : keys[last].currency0;
        if (outCur == address(0)) {
            IPoolManager(POOL_MANAGER).take(address(0), address(this), amount);
            IWETH(WETH).deposit{value: amount}();
        } else {
            IPoolManager(POOL_MANAGER).take(outCur, address(this), amount);
        }
        return abi.encode(amount);
    }

    /// Receives ETH from WETH.withdraw and PoolManager.take(native).
    receive() external payable {}

    /// UniV3 + Slipstream callback: pay the owed (positive-delta) token.
    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        if (msg.sender != expectedCaller) revert BadCallback();
        if (amount0Delta > 0) {
            IERC20(IUniV3Pool(msg.sender).token0()).transfer(msg.sender, uint256(amount0Delta));
        } else if (amount1Delta > 0) {
            IERC20(IUniV3Pool(msg.sender).token1()).transfer(msg.sender, uint256(amount1Delta));
        }
    }

    /// Pancake V3 callback (distinct selector, same logic).
    function pancakeV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        if (msg.sender != expectedCaller) revert BadCallback();
        if (amount0Delta > 0) {
            IERC20(IUniV3Pool(msg.sender).token0()).transfer(msg.sender, uint256(amount0Delta));
        } else if (amount1Delta > 0) {
            IERC20(IUniV3Pool(msg.sender).token1()).transfer(msg.sender, uint256(amount1Delta));
        }
    }

    /// Algebra Integral callback (Hydrex, QuickSwap v4): distinct selector, same
    /// logic — pay the owed (positive-delta) token to the pool.
    function algebraSwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        if (msg.sender != expectedCaller) revert BadCallback();
        if (amount0Delta > 0) {
            IERC20(IUniV3Pool(msg.sender).token0()).transfer(msg.sender, uint256(amount0Delta));
        } else if (amount1Delta > 0) {
            IERC20(IUniV3Pool(msg.sender).token1()).transfer(msg.sender, uint256(amount1Delta));
        }
    }
}
