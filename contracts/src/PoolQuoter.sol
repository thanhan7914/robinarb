// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

// Generic V3-family quoter, injected via eth_call STATE OVERRIDE (never
// deployed). Addressed by POOL — not CREATE2-derived from a factory like
// QuoterV2, so one contract quotes every UniV3-style fork (Uniswap, Sushi,
// Pancake, Slipstream, Aerodrome CL a/b) with the pool's CURRENT fee,
// including dynamic fees. Quoting works the same way QuoterV2 does
// internally: execute pool.swap and revert with the amounts inside the
// callback, before any payment is due.
//
// After editing, regenerate the embedded runtime bytecode in
// src/constants.rs: forge build, then copy
// out/PoolQuoter.sol/PoolQuoter.json .deployedBytecode.object

interface IV3PoolSwap {
    function swap(
        address recipient,
        bool zeroForOne,
        int256 amountSpecified,
        uint160 sqrtPriceLimitX96,
        bytes calldata data
    ) external returns (int256 amount0, int256 amount1);
}

contract PoolQuoter {
    uint160 internal constant MIN_SQRT_RATIO = 4295128739;
    uint160 internal constant MAX_SQRT_RATIO =
        1461446703485210103287273052203988822378723970342;

    /// Exact-input quote against one pool. Reverts only if the pool itself
    /// reverts (the original revert data is bubbled up).
    function quotePool(address pool, bool zeroForOne, uint256 amountIn)
        external
        returns (uint256 amountOut)
    {
        try IV3PoolSwap(pool).swap(
            address(this),
            zeroForOne,
            int256(amountIn),
            zeroForOne ? MIN_SQRT_RATIO + 1 : MAX_SQRT_RATIO - 1,
            ""
        ) {
            revert("swap did not revert");
        } catch (bytes memory reason) {
            if (reason.length != 64) {
                // Real pool revert (not our callback) — bubble it up.
                assembly {
                    revert(add(reason, 0x20), mload(reason))
                }
            }
            (int256 a0, int256 a1) = abi.decode(reason, (int256, int256));
            int256 outDelta = zeroForOne ? a1 : a0;
            return outDelta < 0 ? uint256(-outDelta) : 0;
        }
    }

    // Uniswap/Sushi/Slipstream/Aerodrome CL callback.
    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata)
        external
        pure
    {
        _revertWithDeltas(amount0Delta, amount1Delta);
    }

    // Pancake V3 uses a renamed callback; same semantics.
    function pancakeV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata)
        external
        pure
    {
        _revertWithDeltas(amount0Delta, amount1Delta);
    }

    // Algebra Integral (Hydrex, QuickSwap v4): swap() shares the UniV3 selector
    // (address,bool,int256,uint160,bytes) but calls back algebraSwapCallback.
    function algebraSwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata)
        external
        pure
    {
        _revertWithDeltas(amount0Delta, amount1Delta);
    }

    function _revertWithDeltas(int256 a0, int256 a1) private pure {
        bytes memory data = abi.encode(a0, a1);
        assembly {
            revert(add(data, 0x20), mload(data))
        }
    }
}
