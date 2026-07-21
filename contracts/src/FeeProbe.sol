// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

// Fee-on-transfer / rebase detector, injected via eth_call STATE OVERRIDE
// (never deployed) at TWO real addresses: `holder` (a real address already
// holding a real balance of the token — typically one of that token's own
// discovered pools) and `relay` (any other synthetic address, code-overridden
// with this same contract). Only CODE is overridden at either address; real
// storage (hence real balances) is untouched.
//
// Two hops, not one: `holder` -> `relay` -> `dummy`. Some tokens exempt their
// OWN pool/pair from transfer fees — a common pattern so a tax token's AMM
// math isn't corrupted by the fee — so a single-hop probe FROM the pool
// gives a false negative even though the SAME token taxes every ordinary
// (non-exempt) address, including our own arb contract. `relay` is an
// ordinary address by construction (never listed on any exemption list), so
// the second hop reproduces exactly what our arb contract experiences
// mid-route.
//
// After editing, regenerate the embedded runtime bytecode in
// src/constants.rs: forge build, then copy
// out/FeeProbe.sol/FeeProbe.json .deployedBytecode.object

interface IERC20Probe {
    function transfer(address, uint256) external returns (bool);
    function balanceOf(address) external view returns (uint256);
}

interface IFeeProbeRelay {
    function relay(address token, address dummy, uint256 amount) external returns (uint256 received);
}

contract FeeProbe is IFeeProbeRelay {
    /// Entry point, called with `holder`'s code overridden to this contract.
    /// `relay` must ALSO be code-overridden to this same contract (its
    /// `relay` entry point below is what actually measures the fee).
    function probe(address token, address relay_, address dummy, uint256 amount)
        external
        returns (uint256 received)
    {
        IERC20Probe(token).transfer(relay_, amount);
        uint256 gotByRelay = IERC20Probe(token).balanceOf(relay_);
        received = IFeeProbeRelay(relay_).relay(token, dummy, gotByRelay);
    }

    /// The hop that actually matters: an ordinary (non-exempt) address
    /// forwarding to another ordinary address — exactly what our arb
    /// contract does mid-route.
    function relay(address token, address dummy, uint256 amount) external returns (uint256 received) {
        uint256 before = IERC20Probe(token).balanceOf(dummy);
        IERC20Probe(token).transfer(dummy, amount);
        received = IERC20Probe(token).balanceOf(dummy) - before;
    }
}
