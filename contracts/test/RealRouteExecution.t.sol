// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

// End-to-end test of `executeArb` with a REAL route through REAL pools —
// MorphoFlashLoan.t.sol deliberately bypasses real swap execution via a
// harness override; this covers the ACTUAL swaps too, not just the
// flashloan glue.
//
// Route: route_id=330 from a real paper_opps.jsonl entry (block 14559038)
// — a 2-hop Uniswap v4 cycle, USDG -> tokenX -> USDG, across two different
// fee tiers (0.7% and 1.0%) of the same currency pair. Both hops are v4 and
// share a connecting currency, so ArbExecutor's `_runHops` merges them into
// ONE PoolManager.unlock() segment — this also exercises
// `_runV4Segment`/`unlockCallback` on this chain's actual PoolManager
// (0x8366a39C...).
//
// Forked at the EXACT block the opportunity was logged at, not `latest` —
// an apples-to-apples replay of what the Rust model saw, not a fresh
// (and likely already-closed) opportunity.

import {Test, console2} from "forge-std/Test.sol";
import {ArbExecutor, IERC20} from "../src/ArbExecutor.sol";

contract RealRouteExecutionTest is Test {
    address constant USDG = 0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168;
    address constant TOKEN_X = 0x39dBED3a2bd333467115dE45665cC57F813C4571;

    ArbExecutor h;

    function setUp() public {
        // Our own local node runs WITHOUT --execution.caching.archive, so
        // historical state beyond a short in-memory window is already
        // pruned by the time this runs. The PUBLIC RPC keeps archive depth
        // — use it here for a true apples-to-apples replay at the exact
        // block the opportunity was logged at.
        string memory url = vm.envOr("ROBIN_RPC_URL", string("https://rpc.mainnet.chain.robinhood.com"));
        vm.createSelectFork(url, 14_559_038);
        h = new ArbExecutor();
        // Standing max approval replaces the per-call approve() inside
        // onMorphoFlashLoan — required deploy-time setup now, or Morpho's
        // own safeTransferFrom repay pull reverts.
        h.approveMorphoMax(USDG);
    }

    function _route330() internal pure returns (
        ArbExecutor.Hop[] memory hops,
        ArbExecutor.V4Key[] memory keys,
        uint256 amountIn,
        uint256 modelNetProfit
    ) {
        keys = new ArbExecutor.V4Key[](2);
        // hop0 pool_id 0x13f9ab4e...: fee 0.7% (7000 pips), tickSpacing 140
        keys[0] = ArbExecutor.V4Key({
            currency0: TOKEN_X,
            currency1: USDG,
            fee: 7000,
            tickSpacing: 140
        });
        // hop1 pool_id 0xf7f9cd06...: fee 1.0% (10000 pips), tickSpacing 1
        keys[1] = ArbExecutor.V4Key({
            currency0: TOKEN_X,
            currency1: USDG,
            fee: 10000,
            tickSpacing: 1
        });

        hops = new ArbExecutor.Hop[](2);
        // zfo=[false, true] from the paper log: hop0 USDG->tokenX (currency1->currency0),
        // hop1 tokenX->USDG (currency0->currency1).
        hops[0] = ArbExecutor.Hop({kind: 3, pool: address(0), zeroForOne: false, feeBps: 0});
        hops[1] = ArbExecutor.Hop({kind: 3, pool: address(0), zeroForOne: true, feeBps: 1});

        amountIn = 417_935_992; // raw USDG units (6 decimals) -- from the paper log
        modelNetProfit = 8_347_878; // what the Rust model predicted, same units
    }

    /// Self-funded path first (simpler failure surface than flashloan+repay
    /// -- isolates "does the real swap encoding/execution work" from "does
    /// Morpho borrow/repay work", which MorphoFlashLoan.t.sol already
    /// covers separately).
    function testRealRouteSelfFunded() public {
        (ArbExecutor.Hop[] memory hops, ArbExecutor.V4Key[] memory keys, uint256 amountIn,) = _route330();

        deal(USDG, address(h), amountIn);
        uint256 before = IERC20(USDG).balanceOf(address(h));

        // minProfit=0: we want to see the REAL realized profit, not gate on
        // the model's prediction yet (that's the assertion below).
        h.executeArb(hops, keys, USDG, amountIn, 0, false);

        uint256 afterBal = IERC20(USDG).balanceOf(address(h));
        console2.log("USDG before:", before);
        console2.log("USDG after: ", afterBal);
        console2.log("realized profit (raw USDG units):", afterBal - before);

        assertGt(afterBal, before, "route should be profitable, as the Rust model predicted");
    }

    /// Full path: Morpho flashloan + real swaps + real repay, end to end --
    /// the actual shape `[execution].enabled = true` would use live.
    function testRealRouteViaMorphoFlashloan() public {
        (ArbExecutor.Hop[] memory hops, ArbExecutor.V4Key[] memory keys, uint256 amountIn,) = _route330();

        uint256 before = IERC20(USDG).balanceOf(address(h));
        // minProfit=0 here too, for the same reason as above -- assert the
        // realized amount separately rather than gating the call on it.
        h.executeArb(hops, keys, USDG, amountIn, 0, true);
        uint256 afterBal = IERC20(USDG).balanceOf(address(h));

        console2.log("realized profit via Morpho flashloan (raw USDG units):", afterBal - before);
        assertGt(afterBal, before, "flashloan route should be profitable");
    }

    /// Sanity: the realized profit should be in the same ballpark as what
    /// the Rust model predicted at the same block -- not exact (the model
    /// doesn't simulate every last wei of AMM rounding), but not wildly off
    /// either. This is the actual "is the Rust<->Solidity encoding/math
    /// consistent" check.
    function testRealizedProfitMatchesModelRoughly() public {
        (ArbExecutor.Hop[] memory hops, ArbExecutor.V4Key[] memory keys, uint256 amountIn, uint256 modelNetProfit) = _route330();

        deal(USDG, address(h), amountIn);
        uint256 before = IERC20(USDG).balanceOf(address(h));
        h.executeArb(hops, keys, USDG, amountIn, 0, false);
        uint256 realized = IERC20(USDG).balanceOf(address(h)) - before;

        console2.log("model predicted at block 14559038 (net, post-gas):", modelNetProfit);
        console2.log("realized (no gas deducted, this is a raw swap):", realized);
        // Same block as the model prediction now (see setUp) -- this is the
        // real "is Rust's quote math consistent with Solidity's real swap
        // execution" check. Not exact (model is gross_profit-ish pre-gas,
        // realized here has no gas deducted either, and AMM rounding
        // differs from the model's f64 math) -- 20% is generous but should
        // catch a genuinely wrong encoding (e.g. swapped hop order, wrong
        // zeroForOne) rather than just rounding noise.
        assertApproxEqRel(realized, modelNetProfit, 0.2e18, "realized profit too far from model prediction -- possible encoding mismatch");
    }
}
