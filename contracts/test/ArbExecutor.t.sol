// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {ArbExecutor} from "../src/ArbExecutor.sol";

contract ArbExecutorTest is Test {
    ArbExecutor exec;
    address owner = address(this);
    address attacker = address(0xBEEF);

    function setUp() public {
        exec = new ArbExecutor();
    }

    function testOwnerAndExecutorSet() public view {
        assertEq(exec.owner(), owner);
        assertTrue(exec.executors(owner));
    }

    function testCallbackAuthReverts() public {
        // Direct callback call with no in-flight swap => expectedCaller is zero,
        // msg.sender != expectedCaller => BadCallback.
        vm.expectRevert(ArbExecutor.BadCallback.selector);
        exec.uniswapV3SwapCallback(1, 0, "");
    }

    function testPancakeCallbackAuthReverts() public {
        vm.expectRevert(ArbExecutor.BadCallback.selector);
        exec.pancakeV3SwapCallback(0, 1, "");
    }

    function testOnlyExecutor() public {
        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](0);
        vm.prank(attacker);
        vm.expectRevert(ArbExecutor.NotExecutor.selector);
        exec.executeArb(hops, new ArbExecutor.V4Key[](0), exec.WETH(), 1 ether, 0, false);
    }

    function testWithdrawOnlyOwner() public {
        // Cache first: prank/expectRevert bind to the NEXT external call, and
        // `exec.WETH()` would otherwise consume both.
        address weth = exec.WETH();
        vm.prank(attacker);
        vm.expectRevert(ArbExecutor.NotOwner.selector);
        exec.withdraw(weth, 0);
    }

    function testSetExecutor() public {
        exec.setExecutor(attacker, true);
        assertTrue(exec.executors(attacker));
        exec.setExecutor(attacker, false);
        assertFalse(exec.executors(attacker));
    }

    // Fork-gated end-to-end tests (2-hop, 3-hop, flashloan round-trip) run with:
    //   forge test --fork-url $BASE_RPC_URL --match-test testFork
    // They require live pool addresses; add them once wiring against Base mainnet.
    // Placeholder to document intent:
    function testForkPlaceholder() public {
        if (block.chainid != 8453) {
            vm.skip(true);
        }
        // TODO: fund executor with WETH via deal(), pick two real pools that form
        // a profitable-at-test-block cycle, executeArb self-funded, assert the
        // WETH balance increased.
    }
}
