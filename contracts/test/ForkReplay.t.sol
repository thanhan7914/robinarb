// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test, console2} from "forge-std/Test.sol";
import {ArbExecutor} from "../src/ArbExecutor.sol";

interface IPoolTokens {
    function token0() external view returns (address);
}

interface IERC20Bal {
    function balanceOf(address) external view returns (uint256);
}

/// Replays an opportunity captured by paper mode (paper_opps.jsonl,
/// route_id 5174 at block 48669985) against a Base fork at that exact block:
/// WETH -> USDC (Pancake V3) -> VELVET (Slipstream) -> WETH (Slipstream).
///
/// FINDING: the model predicted +277877328022289 wei but actual execution
/// returns 42917813793594067 for 43156392469620768 in — a REAL LOSS of
/// ~0.55%. Cause: the final hop is a $3k-liquidity Slipstream pool with
/// tickSpacing 200; an $83 swap walks far past the hydrated ±16-word tick
/// window, so the model overestimates output. These recurring paper "opps"
/// are phantoms. Until the tick window is fixed
/// (full Mint/Burn history) or micro CL pools are filtered, expect
/// ProfitTooLow here — this test locks in that knowledge and will FAIL the
/// day the route genuinely turns profitable (flip the assertion then).
///
///   BASE_RPC_URL=<archive rpc> forge test --match-contract ForkReplay -vv
contract ForkReplayTest is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant VELVET = 0xbF927b841994731C573BDF09ceB0c6B0Aa887cDd;

    address constant POOL_WETH_USDC = 0x72AB388E2E2F6FaceF59E3C3FA2C4E29011c2D38; // pancake V3, 100 pips (factory-verified)
    // NOTE: pools.toml labels these "aerodrome" (DexScreener hint) but their
    // on-chain factory() is Slipstream — CL pools, V3 swap interface (kind 2).
    address constant POOL_VELVET_USDC = 0x6b0F53cbD9272D8117e9535FE25371dedF39a1bE; // slipstream, tickSpacing 1
    address constant POOL_VELVET_WETH = 0xF579B16f9b1A4aCc872D34a8141fbbD36C0ce10C; // slipstream, tickSpacing 200

    // From the paper_opps.jsonl record.
    uint256 constant FORK_BLOCK = 48669985;
    uint256 constant AMOUNT_IN = 43156392469620768; // 0.04316 WETH
    uint256 constant PREDICTED_GROSS_OUT = 43434269797643057; // model gross_out

    function testForkReplayVelvetRoute() public {
        vm.createSelectFork(vm.envString("BASE_RPC_URL"), FORK_BLOCK);

        ArbExecutor exec = new ArbExecutor();
        deal(WETH, address(exec), AMOUNT_IN);

        ArbExecutor.Hop[] memory hops = new ArbExecutor.Hop[](3);
        hops[0] = ArbExecutor.Hop({
            kind: 2,
            pool: POOL_WETH_USDC,
            zeroForOne: tokenInIsToken0(POOL_WETH_USDC, WETH),
            feeBps: 0
        });
        hops[1] = ArbExecutor.Hop({
            kind: 2,
            pool: POOL_VELVET_USDC,
            zeroForOne: tokenInIsToken0(POOL_VELVET_USDC, USDC),
            feeBps: 0
        });
        hops[2] = ArbExecutor.Hop({
            kind: 2,
            pool: POOL_VELVET_WETH,
            zeroForOne: tokenInIsToken0(POOL_VELVET_WETH, VELVET),
            feeBps: 0
        });

        // Measured on fork: 43156392469620768 in -> 42917813793594067 out
        // (-238578676026701 wei), so the contract's own guard must trip.
        vm.expectRevert(ArbExecutor.ProfitTooLow.selector);
        exec.executeArb(hops, new ArbExecutor.V4Key[](0), WETH, AMOUNT_IN, 0, false);
    }

    function tokenInIsToken0(address pool, address tokenIn) internal view returns (bool) {
        return IPoolTokens(pool).token0() == tokenIn;
    }
}
