// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.24;

import {Bridge} from "./Bridge.sol";

/// Deploys (or adopts) the canonical L2 `Bridge` ERC20 for each Sui coin type.
///
/// The Sui vault accepts deposits of any coin type. This factory is what makes
/// that safe instead of stranding: for every coin type there is exactly one
/// canonical token (enforced by `tokenFor`), anyone can create it, and the
/// relayer only honors `WithdrawalInitiated` logs from tokens registered here.
/// That last property is security-critical: a fake bridge contract emitting
/// forged withdrawal logs must never cause a Sui release, and the relayer can
/// always verify an address against this registry.
///
/// Besides runtime creation, one `Bridge` is predeployed in genesis at a fixed
/// address (so the flagship coin has a stable, integration-friendly token
/// address) and adopted here after `Bridge.initialize` configures it.
contract BridgeFactory {
    /// Only this address may be set as `relayer` on new tokens. Normally the
    /// sequencer's EVM key. Rotated via `setRelayer`; existing tokens rotate
    /// themselves through their own `setRelayer`.
    address public relayer;
    bool public initialized;

    /// Sui `TypeName` string -> canonical token address.
    mapping(string => address) public tokenFor;

    event BridgeCreated(
        address indexed token,
        string coinType,
        string name,
        string symbol
    );
    event RelayerChanged(address indexed previous, address indexed next);

    modifier onlyRelayer() {
        require(msg.sender == relayer, "BridgeFactory: not relayer");
        _;
    }

    /// One-time configuration. The factory is a genesis predeploy: genesis runs
    /// no constructor, so it boots `initialized == false` and the operator calls
    /// this once after chain start.
    function initialize(address relayer_) external {
        require(!initialized, "BridgeFactory: already initialized");
        require(relayer_ != address(0), "BridgeFactory: zero relayer");
        initialized = true;
        relayer = relayer_;
        emit RelayerChanged(address(0), relayer_);
    }

    /// Deploys the canonical token for `coinType`. Permissionless: anyone may
    /// create the token for a coin type that has none, which is what lets the
    /// vault accept any coin without an operator gate. Reverts if one already
    /// exists, so there is exactly one canonical representation per coin type.
    function createBridge(
        string calldata name,
        string calldata symbol,
        string calldata coinType
    ) external returns (address token) {
        require(initialized, "BridgeFactory: not initialized");
        require(bytes(coinType).length > 0, "BridgeFactory: empty coin type");
        require(
            tokenFor[coinType] == address(0),
            "BridgeFactory: coin type already has a token"
        );

        Bridge created = new Bridge(name, symbol, coinType, relayer);
        tokenFor[coinType] = address(created);
        emit BridgeCreated(address(created), coinType, name, symbol);
        return address(created);
    }

    /// Registers an already-deployed `Bridge` (the genesis predeploy) as the
    /// canonical token for its coin type. Only the factory relayer may adopt, so
    /// the canonical address for a predeployed coin stays operator-controlled;
    /// the token itself is verified on-chain: its `coinType()` must match and its
    /// `relayer()` must be the factory's.
    function adoptBridge(string calldata coinType, address token) external onlyRelayer {
        require(initialized, "BridgeFactory: not initialized");
        require(token != address(0), "BridgeFactory: zero token");
        require(bytes(coinType).length > 0, "BridgeFactory: empty coin type");
        require(
            tokenFor[coinType] == address(0),
            "BridgeFactory: coin type already has a token"
        );
        require(
            keccak256(bytes(Bridge(token).coinType())) == keccak256(bytes(coinType)),
            "BridgeFactory: token coin type mismatch"
        );
        require(
            Bridge(token).relayer() == relayer,
            "BridgeFactory: token relayer mismatch"
        );

        tokenFor[coinType] = token;
        emit BridgeCreated(token, coinType, Bridge(token).name(), Bridge(token).symbol());
    }

    function setRelayer(address next) external onlyRelayer {
        require(next != address(0), "BridgeFactory: zero relayer");
        emit RelayerChanged(relayer, next);
        relayer = next;
    }
}
