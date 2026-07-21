// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {ArbExecutor} from "../src/ArbExecutor.sol";

/// Regression for the flashloan-guard subsidy bug: tx 0x98d4b0fa… landed at
/// −0.000321 WETH because receiveFlashLoan compared the ABSOLUTE balance
/// against repay + minProfit —
/// the 0.01 WETH pre-funded for self-funded mode silently covered the
/// shortfall of a phantom opp (VIRTUAL cluster, pool 0x3f0296bf fee-jump).
///
/// Replays that exact calldata at the exact pre-tx block against the FIXED
/// contract, WITH the same 0.01 WETH pre-fund. The delta check must revert
/// ProfitTooLow no matter how much idle WETH the contract holds.
///
///   BASE_RPC_URL=<archive rpc> forge test --match-contract GuardRegression -vv
contract GuardRegressionTest is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    // Route 3309 of the live session: WETH -> VIRTUAL (CL) -> WETH (slipstream 0x3f0296bf).
    address constant POOL_A = 0xC200F21EfE67c7F41B81A854c26F9cdA80593065;
    address constant POOL_B = 0x3f0296BF652e19bca772EC3dF08b32732F93014A;

    // From tx 0x98d4b0fa670af5c63dc926a8c08124759ef37af1dc8760d3d992c7851f43c545
    // (included in block 48707906 → fork its parent state).
    uint256 constant FORK_BLOCK = 48707905;
    uint256 constant AMOUNT_IN = 189169582748200960;
    uint256 constant MIN_PROFIT = 40566958080553;
    uint256 constant PREFUND = 10000000000000000; // the 0.01 WETH that masked the loss

    function testFlashloanGuardNotSubsidizedByIdleFunds() public {
        vm.createSelectFork(vm.envString("BASE_RPC_URL"), FORK_BLOCK);

        ArbExecutor exec = new ArbExecutor();
        deal(WETH, address(exec), PREFUND);

        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](2);
        hops[0] = ArbExecutor.Hop({kind: 2, pool: POOL_A, zeroForOne: false, feeBps: 6});
        hops[1] = ArbExecutor.Hop({kind: 2, pool: POOL_B, zeroForOne: true, feeBps: 1});

        // On mainnet this landed and drained 321074908859864 wei from the
        // pre-fund. With the delta check it must revert instead.
        vm.expectRevert(ArbExecutor.ProfitTooLow.selector);
        exec.executeArb(hops, new ArbExecutor.V4Key[](0), WETH, AMOUNT_IN, MIN_PROFIT, true);

        // And the idle funds must be untouched afterwards.
        assertEq(IERC20Bal(WETH).balanceOf(address(exec)), PREFUND);
    }
}

interface IERC20Bal {
    function balanceOf(address) external view returns (uint256);
}
