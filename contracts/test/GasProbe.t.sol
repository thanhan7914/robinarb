// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

// One-off gas probe: same WETH->USDC 0.05% swap through UniV3 vs V4 (real
// WETH) vs V4 (native ETH pool, incl. WETH unwrap), plus a 2-hop V4 segment.

import {Test, console2} from "forge-std/Test.sol";
import {ArbExecutor, IERC20} from "../src/ArbExecutor.sol";

contract GasHarness is ArbExecutor {
    function run(ArbExecutor.Hop[] calldata hops, ArbExecutor.V4Key[] calldata v4Keys, uint256 amountIn)
        external
        returns (uint256 gasUsed)
    {
        uint256 g0 = gasleft();
        _runHops(hops, v4Keys, amountIn);
        gasUsed = g0 - gasleft();
    }
}

contract GasProbeTest is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant CBBTC = 0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf;
    address constant UNIV3_WETH_USDC = 0xd0b53D9277642d899DF5C87A3966A349A798F224;

    GasHarness h;

    function setUp() public {
        vm.createSelectFork(vm.envString("BASE_RPC_URL"));
        h = new GasHarness();
        deal(WETH, address(h), 10 ether);
        deal(USDC, address(h), 10000e6);
    }

    function _key(address c0, address c1) internal pure returns (ArbExecutor.V4Key memory) {
        return ArbExecutor.V4Key(c0, c1, 500, 10);
    }

    function testGasV3SingleHop() public {
        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](1);
        hops[0] = ArbExecutor.Hop(2, UNIV3_WETH_USDC, true, 0);
        uint256 g = h.run(hops, new ArbExecutor.V4Key[](0), 1 ether);
        console2.log("V3 WETH->USDC 1 hop:", g);
    }

    function testGasV4Erc20SingleHop() public {
        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](1);
        hops[0] = ArbExecutor.Hop(3, address(0), true, 0);
        ArbExecutor.V4Key[] memory keys = new ArbExecutor.V4Key[](1);
        keys[0] = _key(WETH, USDC);
        uint256 g = h.run(hops, keys, 1 ether);
        console2.log("V4 WETH->USDC (real WETH) 1 hop:", g);
    }

    function testGasV4NativeSingleHop() public {
        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](1);
        hops[0] = ArbExecutor.Hop(3, address(0), true, 0);
        ArbExecutor.V4Key[] memory keys = new ArbExecutor.V4Key[](1);
        keys[0] = _key(address(0), USDC);
        uint256 g = h.run(hops, keys, 1 ether);
        console2.log("V4 ETH->USDC (native, unwrap WETH) 1 hop:", g);
    }

    function testGasV4TwoHopOneUnlock() public {
        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](2);
        hops[0] = ArbExecutor.Hop(3, address(0), false, 0); // USDC -> ETH
        hops[1] = ArbExecutor.Hop(3, address(0), true, 1); // ETH -> cbBTC
        ArbExecutor.V4Key[] memory keys = new ArbExecutor.V4Key[](2);
        keys[0] = _key(address(0), USDC);
        keys[1] = _key(address(0), CBBTC);
        uint256 g = h.run(hops, keys, 2000e6);
        console2.log("V4 USDC->ETH->cbBTC 2 hop 1 unlock:", g);
    }
}
