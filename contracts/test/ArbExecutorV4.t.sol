// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

// Uniswap V4 execution fork tests (BASE_RPC_URL required). Pools used are the
// pinned vanilla V4 pools from pools.toml, all verified live via StateView:
//   ETH/USDC   500/10 (native)  0x96d4b53a…
//   WETH/USDC  500/10 (real W)  0x90333bb0…
//   ETH/cbBTC  500/10 (native)  0x2fbe93bf…
// Every output assertion is wei-exact against the official V4Quoter at the
// same fork block.

import {Test, console2} from "forge-std/Test.sol";
import {ArbExecutor, IERC20, IPoolManager} from "../src/ArbExecutor.sol";

interface IV4Quoter {
    struct QuoteExactSingleParams {
        IPoolManager.PoolKey poolKey;
        bool zeroForOne;
        uint128 exactAmount;
        bytes hookData;
    }

    function quoteExactInputSingle(QuoteExactSingleParams memory params)
        external
        returns (uint256 amountOut, uint256 gasEstimate);
}

/// Guard-free harness: measure realized amounts of arbitrary hop lists.
contract V4Harness is ArbExecutor {
    function run(ArbExecutor.Hop[] calldata hops, ArbExecutor.V4Key[] calldata v4Keys, uint256 amountIn)
        external
        returns (uint256)
    {
        _runHops(hops, v4Keys, amountIn);
        return 0;
    }
}

contract ArbExecutorV4Test is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant CBBTC = 0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf;
    address constant QUOTER = 0x0d5e0F971ED27FBfF6c2837bf31316121532048D;
    address constant POOL_MANAGER = 0x498581fF718922c3f8e6A244956aF099B2652b2b;
    // UniV3 WETH/USDC 0.05% (kind 2 hop for the mixed-route test).
    address constant UNIV3_WETH_USDC = 0xd0b53D9277642d899DF5C87A3966A349A798F224;

    V4Harness h;

    function setUp() public {
        vm.createSelectFork(vm.envString("BASE_RPC_URL"));
        h = new V4Harness();
    }

    function _key(address c0, address c1) internal pure returns (ArbExecutor.V4Key memory) {
        return ArbExecutor.V4Key({currency0: c0, currency1: c1, fee: 500, tickSpacing: 10});
    }

    function _quote(address c0, address c1, bool zeroForOne, uint128 amountIn)
        internal
        returns (uint256 out)
    {
        (out,) = IV4Quoter(QUOTER).quoteExactInputSingle(
            IV4Quoter.QuoteExactSingleParams({
                poolKey: IPoolManager.PoolKey(c0, c1, 500, 10, address(0)),
                zeroForOne: zeroForOne,
                exactAmount: amountIn,
                hookData: ""
            })
        );
    }

    function _hopV4(bool zeroForOne, uint16 keyIdx) internal pure returns (ArbExecutor.Hop memory) {
        return ArbExecutor.Hop({kind: 3, pool: address(0), zeroForOne: zeroForOne, feeBps: keyIdx});
    }

    /// 1 hop, ERC20/ERC20 (real-WETH pool): output must equal V4Quoter to the wei.
    function testSingleHopErc20MatchesQuoter() public {
        uint256 amountIn = 1 ether;
        deal(WETH, address(h), amountIn);
        uint256 expected = _quote(WETH, USDC, true, uint128(amountIn));

        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](1);
        hops[0] = _hopV4(true, 0);
        ArbExecutor.V4Key[] memory keys = new ArbExecutor.V4Key[](1);
        keys[0] = _key(WETH, USDC);

        h.run(hops, keys, amountIn);
        assertEq(IERC20(USDC).balanceOf(address(h)), expected, "V4 ERC20 hop != quoter");
        assertEq(IERC20(WETH).balanceOf(address(h)), 0, "input not fully spent");
    }

    /// Native-ETH pool, WETH in: the contract unwraps and settles native.
    function testNativeHopWethInMatchesQuoter() public {
        uint256 amountIn = 1 ether;
        deal(WETH, address(h), amountIn);
        uint256 expected = _quote(address(0), USDC, true, uint128(amountIn));

        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](1);
        hops[0] = _hopV4(true, 0);
        ArbExecutor.V4Key[] memory keys = new ArbExecutor.V4Key[](1);
        keys[0] = _key(address(0), USDC);

        h.run(hops, keys, amountIn);
        assertEq(IERC20(USDC).balanceOf(address(h)), expected, "V4 native-in hop != quoter");
        assertEq(IERC20(WETH).balanceOf(address(h)), 0, "WETH not unwrapped/spent");
        assertEq(address(h).balance, 0, "stray ETH left behind");
    }

    /// Native-ETH pool, WETH out: the contract takes native and wraps to WETH.
    function testNativeHopWethOutMatchesQuoter() public {
        uint256 amountIn = 2000e6; // 2000 USDC
        deal(USDC, address(h), amountIn);
        uint256 expected = _quote(address(0), USDC, false, uint128(amountIn));

        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](1);
        hops[0] = _hopV4(false, 0);
        ArbExecutor.V4Key[] memory keys = new ArbExecutor.V4Key[](1);
        keys[0] = _key(address(0), USDC);

        h.run(hops, keys, amountIn);
        assertEq(IERC20(WETH).balanceOf(address(h)), expected, "V4 native-out hop != quoter");
        assertEq(address(h).balance, 0, "stray ETH left behind");
    }

    /// Two adjacent V4 hops sharing a native-ETH intermediate run in ONE
    /// unlock: USDC -> ETH -> cbBTC. The intermediate nets inside flash
    /// accounting (never wrapped), and the chained output is quoter-exact.
    function testAdjacentNativeHopsSingleUnlock() public {
        uint256 amountIn = 2000e6;
        deal(USDC, address(h), amountIn);
        uint256 midEth = _quote(address(0), USDC, false, uint128(amountIn));
        uint256 expected = _quote(address(0), CBBTC, true, uint128(midEth));

        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](2);
        hops[0] = _hopV4(false, 0); // USDC -> ETH
        hops[1] = _hopV4(true, 1); // ETH -> cbBTC
        ArbExecutor.V4Key[] memory keys = new ArbExecutor.V4Key[](2);
        keys[0] = _key(address(0), USDC);
        keys[1] = _key(address(0), CBBTC);

        h.run(hops, keys, amountIn);
        assertEq(IERC20(CBBTC).balanceOf(address(h)), expected, "chained V4 segment != quoter chain");
        // Intermediate ETH must never surface as WETH or raw balance.
        assertEq(IERC20(WETH).balanceOf(address(h)), 0, "intermediate leaked as WETH");
        assertEq(address(h).balance, 0, "intermediate leaked as ETH");
    }

    /// Adjacent V4 hops whose connecting currencies DIFFER (real WETH out,
    /// native ETH in) must split into two unlocks with a boundary wrap:
    /// USDC -(WETH/USDC real)-> WETH -(ETH/cbBTC native)-> cbBTC.
    function testCurrencyMismatchSplitsSegments() public {
        uint256 amountIn = 2000e6;
        deal(USDC, address(h), amountIn);
        uint256 midWeth = _quote(WETH, USDC, false, uint128(amountIn));
        uint256 expected = _quote(address(0), CBBTC, true, uint128(midWeth));

        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](2);
        hops[0] = _hopV4(false, 0); // USDC -> WETH (real)
        hops[1] = _hopV4(true, 1); // ETH -> cbBTC (native)
        ArbExecutor.V4Key[] memory keys = new ArbExecutor.V4Key[](2);
        keys[0] = _key(WETH, USDC);
        keys[1] = _key(address(0), CBBTC);

        h.run(hops, keys, amountIn);
        assertEq(IERC20(CBBTC).balanceOf(address(h)), expected, "split V4 segments != quoter chain");
        assertEq(IERC20(WETH).balanceOf(address(h)), 0, "boundary WETH not consumed");
        assertEq(address(h).balance, 0, "stray ETH left behind");
    }

    /// Mixed route V3 -> V4(native): WETH -(UniV3)-> USDC -(V4 ETH/USDC)-> WETH.
    function testMixedV3ThenV4Route() public {
        uint256 amountIn = 1 ether;
        deal(WETH, address(h), amountIn);

        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](2);
        hops[0] = ArbExecutor.Hop({kind: 2, pool: UNIV3_WETH_USDC, zeroForOne: true, feeBps: 0});
        hops[1] = _hopV4(false, 0); // USDC -> ETH -> wrapped to WETH
        ArbExecutor.V4Key[] memory keys = new ArbExecutor.V4Key[](1);
        keys[0] = _key(address(0), USDC);

        h.run(hops, keys, amountIn);
        uint256 endWeth = IERC20(WETH).balanceOf(address(h));
        // A round trip pays two 0.05% fees + price impact: output must be
        // slightly below input but well above 99% of it on these deep pools.
        assertLt(endWeth, amountIn, "round trip cannot profit");
        assertGt(endWeth, (amountIn * 99) / 100, "round trip lost too much");
        assertEq(IERC20(USDC).balanceOf(address(h)), 0, "intermediate USDC left behind");
    }

    /// Flashloan mode drives the same V4 path and the delta-based ProfitTooLow
    /// guard still reverts a fee-losing round trip.
    function testFlashloanV4RoundTripRevertsProfitTooLow() public {
        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](2);
        hops[0] = _hopV4(true, 0); // ETH -> USDC (native pool, WETH in)
        hops[1] = _hopV4(false, 0); // USDC -> ETH (same pool back)
        ArbExecutor.V4Key[] memory keys = new ArbExecutor.V4Key[](1);
        keys[0] = _key(address(0), USDC);

        vm.expectRevert(ArbExecutor.ProfitTooLow.selector);
        h.executeArb(hops, keys, WETH, 1 ether, 0, true);
    }

    /// unlockCallback auth: only the PoolManager mid-unlock may call.
    function testUnlockCallbackAuth() public {
        vm.expectRevert(ArbExecutor.BadCallback.selector);
        h.unlockCallback("");

        // Right sender, but no unlock in flight (transient flag unset).
        vm.prank(POOL_MANAGER);
        vm.expectRevert(ArbExecutor.BadCallback.selector);
        h.unlockCallback("");
    }
}
