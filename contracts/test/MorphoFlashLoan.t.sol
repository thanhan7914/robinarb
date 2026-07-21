// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

// Morpho Blue flashloan integration test — fork against the LOCAL Robinhood
// Chain node (ROBIN_RPC_URL env var, default http://127.0.0.1:8547), which
// has the real Morpho Blue singleton already deployed and synced
// (0x9D53d5E3bd5E8d4Cbfa6DB1ca238AEA02E651010).
//
// Deliberately isolates Morpho borrow/repay/approve MECHANICS from real
// swap-execution correctness: `_runHops` is overridden to simulate a route
// producing a configurable profit via `deal`, instead of routing through
// real pools. Arb opportunities are transient and not a reliable, repeatable
// test fixture — the swap math itself is already covered separately by the
// Rust `verify-quote`/`verify-math` CLI tooling against live on-chain state,
// which is a better fit for "is this route's output correct" than a
// Solidity fork test would be anyway.
//
// USDG, not WETH, for the happy-path tests below: Morpho's own on-chain
// WETH balance is 0 (nothing supplied to a WETH market yet), so a WETH
// flashLoan() reverts on Morpho's OWN first `safeTransfer`, before our
// contract's logic ever runs. USDG has real, large liquidity ("Robinhood
// Earn" deposits). See `testWethFlashLoanCurrentlyHasNoLiquidity` below,
// which documents this as a known CURRENT-STATE fact (may change as more
// gets supplied to Morpho), not a bug in this contract.

import {Test, console2} from "forge-std/Test.sol";
import {StdCheats} from "forge-std/StdCheats.sol";
import {ArbExecutor, IERC20} from "../src/ArbExecutor.sol";

contract MorphoFlashLoanHarness is ArbExecutor, StdCheats {
    address public simulatedAnchor;
    uint256 public simulatedProfit;

    function setSimulation(address anchor_, uint256 profit_) external {
        simulatedAnchor = anchor_;
        simulatedProfit = profit_;
    }

    /// Test-only stand-in for real swap execution — see file header. Bumps
    /// this contract's `simulatedAnchor` balance up by `simulatedProfit` on
    /// top of whatever it already holds (which, by the time this runs
    /// inside `onMorphoFlashLoan`, already includes the just-borrowed
    /// `assets` — Morpho pushes BEFORE calling the callback).
    function _runHops(Hop[] memory, V4Key[] memory, uint256) internal override {
        uint256 current = IERC20(simulatedAnchor).balanceOf(address(this));
        deal(simulatedAnchor, address(this), current + simulatedProfit);
    }
}

contract MorphoFlashLoanTest is Test {
    address constant WETH = 0x0Bd7D308f8E1639FAb988df18A8011f41EAcAD73;
    address constant USDG = 0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168; // 6 decimals -- confirmed on-chain, not the 18 first guessed (see config.toml)
    address constant MORPHO = 0x9D53d5E3bd5E8d4Cbfa6DB1ca238AEA02E651010;

    MorphoFlashLoanHarness h;

    function setUp() public {
        string memory url = vm.envOr("ROBIN_RPC_URL", string("http://127.0.0.1:8547"));
        vm.createSelectFork(url);
        h = new MorphoFlashLoanHarness();
        // executeArb is onlyExecutor; the deployer (this test contract, via
        // the harness's constructor msg.sender) is already an executor —
        // see ArbExecutor's constructor. No extra setup needed.

        // Standing max approval replaces the per-call approve() inside
        // onMorphoFlashLoan — required deploy-time setup now, or Morpho's
        // own safeTransferFrom repay pull reverts.
        h.approveMorphoMax(WETH);
        h.approveMorphoMax(USDG);
    }

    /// Sanity: Morpho is really there at the address we hardcoded, on the
    /// fork we're actually testing against — not assumed.
    function testMorphoIsDeployed() public {
        assertGt(MORPHO.code.length, 0, "Morpho has no code on this fork");
    }

    /// Documents current chain state, not a contract bug: Morpho's own WETH
    /// balance is 0, so any WETH flashLoan reverts on Morpho's own initial
    /// `safeTransfer`, before onMorphoFlashLoan ever runs. If this test
    /// starts failing (i.e. the flashLoan stops reverting), that's GOOD
    /// news — it means WETH liquidity showed up on Morpho and the
    /// flashloan choice for WETH-anchored routes should be revisited
    /// (Morpho over Uniswap v3 flash(), since Morpho is free).
    function testWethFlashLoanCurrentlyHasNoLiquidity() public {
        assertEq(IERC20(WETH).balanceOf(MORPHO), 0, "Morpho now has WETH liquidity -- revisit the flashloan choice for WETH routes");
        h.setSimulation(WETH, 0.01 ether);
        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](0);
        ArbExecutor.V4Key[] memory keys = new ArbExecutor.V4Key[](0);
        vm.expectRevert();
        h.executeArb(hops, keys, WETH, 1 ether, 0, true);
    }

    /// Happy path, USDG (real Morpho liquidity — see file header): borrow
    /// 1000 USDG, "route" returns borrowed + profit (simulated), repay via
    /// approve, profit stays in the contract.
    function testFlashLoanBorrowRepayProfit() public {
        uint256 amountIn = 1_000e6; // 1000 USDG
        uint256 profit = 10e6; // 10 USDG
        h.setSimulation(USDG, profit);

        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](0);
        ArbExecutor.V4Key[] memory keys = new ArbExecutor.V4Key[](0);

        uint256 morphoBalBefore = IERC20(USDG).balanceOf(MORPHO);

        h.executeArb(hops, keys, USDG, amountIn, profit, true);

        // Morpho got its `amountIn` back exactly (fee-free — see
        // ArbExecutor.sol's onMorphoFlashLoan doc) and the harness kept the
        // profit.
        assertEq(IERC20(USDG).balanceOf(MORPHO), morphoBalBefore, "Morpho balance should be unchanged (fee-free, fully repaid)");
        assertEq(IERC20(USDG).balanceOf(address(h)), profit, "harness should be left holding exactly the profit");
    }

    /// Guard: a route that returns LESS than minProfit must revert the
    /// whole flashloan (Morpho's flashLoan has no partial-success mode —
    /// a revert inside onMorphoFlashLoan reverts the outer flashLoan call,
    /// which reverts executeArb, so no funds move at all).
    function testFlashLoanRevertsBelowMinProfit() public {
        uint256 amountIn = 1_000e6;
        uint256 actualProfit = 1e6; // 1 USDG
        uint256 claimedMinProfit = 10e6; // higher than what the "route" actually returns
        h.setSimulation(USDG, actualProfit);

        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](0);
        ArbExecutor.V4Key[] memory keys = new ArbExecutor.V4Key[](0);

        vm.expectRevert(ArbExecutor.ProfitTooLow.selector);
        h.executeArb(hops, keys, USDG, amountIn, claimedMinProfit, true);
    }

    /// Guard: only Morpho itself may call the callback (anyone else trying
    /// to call onMorphoFlashLoan directly, e.g. to trick the contract into
    /// running arbitrary hops without an actual loan backing them, must
    /// revert).
    function testCallbackRejectsNonMorphoCaller() public {
        vm.expectRevert(ArbExecutor.BadCallback.selector);
        h.onMorphoFlashLoan(1e6, abi.encode(new ArbExecutor.Hop[](0), new ArbExecutor.V4Key[](0), USDG, uint256(0)));
    }
}
