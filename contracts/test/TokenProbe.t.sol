// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";

interface IERC20 {
    function transfer(address, uint256) external returns (bool);
    function balanceOf(address) external view returns (uint256);
}

/// Does a token skim on transfer? deal() a balance, transfer, compare received.
/// Quoters can't see transfer taxes — this is the ground truth check.
contract TokenProbeTest is Test {
    address constant AIXBT = 0x4F9Fd6Be4a90f2620860d680c0d4d5Fb53d1A825;
    address constant VIRTUAL = 0x0b3e328455c4059EEb9e3f84b5543F74E24e7E1b;
    address constant V2_POOL = 0x7464850CC1cFb54A2223229b77B1BCA2f888D946; // VIRTUAL/AIXBT

    function setUp() public {
        string memory rpc = vm.envOr("BASE_RPC_URL", string(""));
        vm.skip(bytes(rpc).length == 0); // offline runs skip instead of failing
        vm.createSelectFork(rpc);
    }

    function probe(address token, string memory name, address to) internal {
        address alice = makeAddr("alice");
        deal(token, alice, 1e18);
        vm.prank(alice);
        IERC20(token).transfer(to, 1e18);
        uint256 got = IERC20(token).balanceOf(to);
        emit log_named_string("token", name);
        emit log_named_uint("sent", 1e18);
        emit log_named_uint("received", got);
    }

    function testTransferAIXBTToWallet() public {
        probe(AIXBT, "AIXBT->wallet", makeAddr("bob"));
    }

    function testTransferAIXBTToPool() public {
        // Some tokens tax only transfers to/from AMM pairs.
        uint256 before = IERC20(AIXBT).balanceOf(V2_POOL);
        address alice = makeAddr("alice");
        deal(AIXBT, alice, 1e18);
        vm.prank(alice);
        IERC20(AIXBT).transfer(V2_POOL, 1e18);
        emit log_named_uint("pool received", IERC20(AIXBT).balanceOf(V2_POOL) - before);
    }

    function testTransferVIRTUALToWallet() public {
        probe(VIRTUAL, "VIRTUAL->wallet", makeAddr("bob"));
    }
}
