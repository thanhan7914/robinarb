// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

// Deployless V3 tick reader: given a pool and a word range, return every
// initialized tick's liquidityNet. Used as a verification tool (compare the
// event-built table against on-chain state) and as an alternative to the
// per-word Multicall3 window scan.

interface IV3Ticks {
    function tickSpacing() external view returns (int24);
    function tickBitmap(int16 wordPosition) external view returns (uint256);
    function ticks(int24 tick)
        external
        view
        returns (
            uint128 liquidityGross,
            int128 liquidityNet,
            uint256,
            uint256,
            int56,
            uint160,
            uint32,
            bool
        );
}

contract TickSync {
    struct TickNet {
        int24 tick;
        int128 liquidityNet;
    }

    constructor(address pool, int16 wordStart, int16 wordEnd) {
        IV3Ticks p = IV3Ticks(pool);
        int24 spacing = p.tickSpacing();

        // First pass: count set bits.
        uint256 count = 0;
        for (int256 w = wordStart; w <= wordEnd; w++) {
            uint256 bm = p.tickBitmap(int16(w));
            count += _popcount(bm);
        }

        TickNet[] memory out = new TickNet[](count);
        uint256 n = 0;
        for (int256 w = wordStart; w <= wordEnd; w++) {
            uint256 bm = p.tickBitmap(int16(w));
            if (bm == 0) continue;
            for (uint256 b = 0; b < 256; b++) {
                if ((bm >> b) & 1 == 1) {
                    int24 tick = int24((int256(w) * 256 + int256(b)) * int256(spacing));
                    (, int128 net,,,,,,) = p.ticks(tick);
                    out[n++] = TickNet(tick, net);
                }
            }
        }

        bytes memory data = abi.encode(out);
        assembly {
            return(add(data, 0x20), mload(data))
        }
    }

    function _popcount(uint256 x) internal pure returns (uint256 c) {
        while (x != 0) {
            x &= (x - 1);
            c++;
        }
    }
}
