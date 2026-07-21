// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

// Algebra Integral (Hydrex, QuickSwap v4) execution fork tests (BASE_RPC_URL
// required). No public Algebra quoter exists on Base, so the wei-exact reference
// is our own deployless PoolQuoter (it runs the pool's real swap and reverts
// with the amounts) — the same oracle the Rust verify uses. Pools are the pinned
// Algebra pools from pools.toml:
//   Hydrex   WETH/USDC   0x82dbe183…
//   QuickSwap USDT/USDC  0xd30B9fA9…
// The executor's realized output must equal PoolQuoter to the wei.

import {Test} from "forge-std/Test.sol";
import {ArbExecutor, IERC20} from "../src/ArbExecutor.sol";
import {PoolQuoter} from "../src/PoolQuoter.sol";

interface IPoolTokens {
    function token0() external view returns (address);
    function token1() external view returns (address);
}

/// Guard-free harness: measure realized amounts of arbitrary hop lists.
contract AlgebraHarness is ArbExecutor {
    function run(ArbExecutor.Hop[] calldata hops, ArbExecutor.V4Key[] calldata v4Keys, uint256 amountIn)
        external
    {
        _runHops(hops, v4Keys, amountIn);
    }
}

contract ArbExecutorAlgebraTest is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant HYDREX_WETH_USDC = 0x82dbe18346a8656dBB5E76F74bf3AE279cC16B29;
    address constant QUICKSWAP_USDT_USDC = 0xd30B9fA98713425c0302593d7F8F094be31E9710;
    // UniV3 WETH/USDC 0.05% — the V3 leg for the mixed round-trip.
    address constant UNIV3_WETH_USDC = 0xd0b53D9277642d899DF5C87A3966A349A798F224;

    AlgebraHarness h;
    PoolQuoter quoter;
    ArbExecutor.V4Key[] noKeys; // empty; Algebra hops don't use V4 keys

    function setUp() public {
        vm.createSelectFork(vm.envString("BASE_RPC_URL"));
        h = new AlgebraHarness();
        quoter = new PoolQuoter();
    }

    /// zeroForOne to swap `tokenIn` out of `pool` (token0 -> token1 when true).
    function _dir(address pool, address tokenIn) internal view returns (bool) {
        return IPoolTokens(pool).token0() == tokenIn;
    }

    function _hop(uint8 kind, address pool, bool zeroForOne) internal pure returns (ArbExecutor.Hop memory) {
        return ArbExecutor.Hop({kind: kind, pool: pool, zeroForOne: zeroForOne, feeBps: 0});
    }

    /// 1 Algebra hop (Hydrex WETH->USDC): realized output == PoolQuoter to the wei.
    function testSingleHopMatchesQuoter() public {
        uint256 amountIn = 1 ether;
        bool zfo = _dir(HYDREX_WETH_USDC, WETH);
        uint256 expected = quoter.quotePool(HYDREX_WETH_USDC, zfo, amountIn);
        assertGt(expected, 0, "quoter returned zero");

        deal(WETH, address(h), amountIn);
        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](1);
        hops[0] = _hop(4, HYDREX_WETH_USDC, zfo);

        h.run(hops, noKeys, amountIn);
        assertEq(IERC20(USDC).balanceOf(address(h)), expected, "Algebra hop out != PoolQuoter");
        assertEq(IERC20(WETH).balanceOf(address(h)), 0, "input not fully spent");
    }

    /// Second Algebra pool (QuickSwap USDT/USDC), USDC in: exercises a different
    /// factory's pools through the same kind-4 path.
    function testQuickswapHopMatchesQuoter() public {
        uint256 amountIn = 10_000e6; // 10k USDC
        bool zfo = _dir(QUICKSWAP_USDT_USDC, USDC);
        address usdt = zfo
            ? IPoolTokens(QUICKSWAP_USDT_USDC).token1()
            : IPoolTokens(QUICKSWAP_USDT_USDC).token0();
        uint256 expected = quoter.quotePool(QUICKSWAP_USDT_USDC, zfo, amountIn);
        assertGt(expected, 0, "quoter returned zero");

        deal(USDC, address(h), amountIn);
        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](1);
        hops[0] = _hop(4, QUICKSWAP_USDT_USDC, zfo);

        h.run(hops, noKeys, amountIn);
        assertEq(IERC20(usdt).balanceOf(address(h)), expected, "QuickSwap hop out != PoolQuoter");
    }

    /// Mixed route WETH -> USDC (Algebra, kind 4) -> WETH (UniV3, kind 2): the
    /// chained output must equal the product of the two PoolQuoter legs, and the
    /// round trip cannot profit (both are real pools with fees).
    function testMixedAlgebraThenV3RoundTrip() public {
        uint256 amountIn = 1 ether;
        bool zfo0 = _dir(HYDREX_WETH_USDC, WETH);
        uint256 midExpected = quoter.quotePool(HYDREX_WETH_USDC, zfo0, amountIn);
        bool zfo1 = _dir(UNIV3_WETH_USDC, USDC);
        uint256 outExpected = quoter.quotePool(UNIV3_WETH_USDC, zfo1, midExpected);

        deal(WETH, address(h), amountIn);
        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](2);
        hops[0] = _hop(4, HYDREX_WETH_USDC, zfo0);
        hops[1] = _hop(2, UNIV3_WETH_USDC, zfo1);

        h.run(hops, noKeys, amountIn);
        assertEq(IERC20(WETH).balanceOf(address(h)), outExpected, "chained out != quoter chain");
        assertEq(IERC20(USDC).balanceOf(address(h)), 0, "intermediate USDC left behind");
        assertLt(outExpected, amountIn, "round trip cannot profit");
    }

    /// algebraSwapCallback must reject any caller that isn't the pool the executor
    /// is currently swapping against (transient expectedCaller guard).
    function testAlgebraCallbackAuth() public {
        vm.expectRevert(ArbExecutor.BadCallback.selector);
        h.algebraSwapCallback(1, 0, "");
    }

    /// A flashloan-funded round trip with no edge reverts ProfitTooLow (delta
    /// guard), losing only gas — same discipline as the V3/V4 paths.
    function testFlashloanRoundTripRevertsProfitTooLow() public {
        ArbExecutor exec = new ArbExecutor();
        exec.setExecutor(address(this), true);
        bool zfo0 = _dir(HYDREX_WETH_USDC, WETH);
        bool zfo1 = _dir(UNIV3_WETH_USDC, USDC);
        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](2);
        hops[0] = _hop(4, HYDREX_WETH_USDC, zfo0);
        hops[1] = _hop(2, UNIV3_WETH_USDC, zfo1);
        vm.expectRevert(ArbExecutor.ProfitTooLow.selector);
        exec.executeArb(hops, noKeys, WETH, 1 ether, 1, true);
    }
}
