// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

// Deployless batch hydration (PoolSync constructor-return trick): deploy with
// eth_call, read pool state in one round-trip, ABI-encode the result as the
// "creation output". Never actually deployed on-chain.
//
// The Rust bootstrap currently hydrates via Multicall3 view calls; these
// contracts are the faster path to wire in later (one call per ~50 pools with an
// arbitrary struct shape). Kept in sync with abi.rs PoolSnap* structs.

interface IV2 {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function getReserves() external view returns (uint112, uint112, uint32);
}

interface IERC20D {
    function decimals() external view returns (uint8);
}

interface IV3 {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function fee() external view returns (uint24);
    function tickSpacing() external view returns (int24);
    function liquidity() external view returns (uint128);
    function slot0() external view returns (uint160 sqrtPriceX96, int24 tick);
}

/// Batch V2/fork pool snapshots.
contract DataSyncV2 {
    struct Snap {
        address pool;
        address token0;
        address token1;
        uint8 dec0;
        uint8 dec1;
        uint112 reserve0;
        uint112 reserve1;
    }

    constructor(address[] memory pools) {
        Snap[] memory out = new Snap[](pools.length);
        for (uint256 i = 0; i < pools.length; i++) {
            IV2 p = IV2(pools[i]);
            (uint112 r0, uint112 r1,) = p.getReserves();
            address t0 = p.token0();
            address t1 = p.token1();
            out[i] = Snap(pools[i], t0, t1, _dec(t0), _dec(t1), r0, r1);
        }
        bytes memory data = abi.encode(out);
        assembly {
            return(add(data, 0x20), mload(data))
        }
    }

    function _dec(address t) internal view returns (uint8) {
        try IERC20D(t).decimals() returns (uint8 d) {
            return d;
        } catch {
            return 18;
        }
    }
}

/// Batch V3-family pool snapshots.
contract DataSyncV3 {
    struct Snap {
        address pool;
        address token0;
        address token1;
        uint8 dec0;
        uint8 dec1;
        uint24 fee;
        int24 tickSpacing;
        uint160 sqrtPriceX96;
        int24 tick;
        uint128 liquidity;
    }

    constructor(address[] memory pools) {
        Snap[] memory out = new Snap[](pools.length);
        for (uint256 i = 0; i < pools.length; i++) {
            IV3 p = IV3(pools[i]);
            (uint160 sp, int24 tk) = p.slot0();
            address t0 = p.token0();
            address t1 = p.token1();
            out[i] = Snap(
                pools[i], t0, t1, _dec(t0), _dec(t1), p.fee(), p.tickSpacing(), sp, tk, p.liquidity()
            );
        }
        bytes memory data = abi.encode(out);
        assembly {
            return(add(data, 0x20), mload(data))
        }
    }

    function _dec(address t) internal view returns (uint8) {
        try IERC20D(t).decimals() returns (uint8 d) {
            return d;
        } catch {
            return 18;
        }
    }
}
