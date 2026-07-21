// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Script} from "forge-std/Script.sol";
import {ArbExecutor} from "../src/ArbExecutor.sol";

contract Deploy is Script {
    function run() external {
        uint256 pk = vm.envUint("PRIVATE_KEY");
        vm.startBroadcast(pk);
        ArbExecutor exec = new ArbExecutor();
        vm.stopBroadcast();
        // Log so the Rust side can pick up wallet.executor_contract.
        // forge script prints returns; also emit for clarity.
        // solhint-disable-next-line
        assembly {
            // no-op
        }
        require(address(exec) != address(0), "deploy failed");
    }
}
