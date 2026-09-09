// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {KurabaseSchema} from "../src/KurabaseSchema.sol";

interface DeployVm {
    function envUint(string calldata name) external returns (uint256);
    function envAddress(string calldata name) external returns (address);
    function envOr(string calldata name, address fallbackValue) external returns (address);
    function addr(uint256 privateKey) external returns (address);
    function startBroadcast(uint256 privateKey) external;
    function stopBroadcast() external;
}

/// @notice Deploys an instance and explicitly grants its execution identities.
/// @dev Secrets are read from the process environment, never embedded in source.
contract Deploy {
    DeployVm private constant vm = DeployVm(address(uint160(uint256(keccak256("hevm cheat code")))));

    event InstanceDeployed(address indexed instance, address indexed owner, address gateway, address identityAuthority);

    function run() external returns (KurabaseSchema instance) {
        uint256 ownerKey = vm.envUint("OWNER_PRIVATE_KEY");
        address instanceOwner = vm.addr(ownerKey);
        address gateway = vm.envAddress("GATEWAY_ADDRESS");
        address issuer = vm.envOr("IDENTITY_AUTHORITY_ADDRESS", address(0));
        address serviceGateway = vm.envOr("SERVICE_GATEWAY_ADDRESS", address(0));
        require(gateway != instanceOwner, "Use a separate application gateway key");
        require(serviceGateway == address(0) || serviceGateway != gateway, "Separate user and service execution keys");

        vm.startBroadcast(ownerKey);
        instance = new KurabaseSchema(instanceOwner, issuer);
        instance.setGateway(gateway, true, false);
        if (serviceGateway != address(0)) instance.setGateway(serviceGateway, true, true);
        vm.stopBroadcast();

        emit InstanceDeployed(address(instance), instanceOwner, gateway, issuer);
    }
}
