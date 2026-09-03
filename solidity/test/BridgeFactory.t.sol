// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {Bridge} from "../src/Bridge.sol";
import {BridgeFactory} from "../src/BridgeFactory.sol";

/// Factory tests pin the two properties the bridge's safety rests on:
///
/// - one canonical token per coin type (duplicates revert), so bridged liquidity
///   is never split across representations, and
/// - creation is permissionless once the factory is initialized, so any Sui coin
///   can gain an L2 representation without an operator gate.
contract BridgeFactoryTest is Test {
    BridgeFactory internal factory;
    address internal relayer = address(0xA11CE);
    address internal alice = address(0xA1);

    function setUp() public {
        factory = new BridgeFactory();
        factory.initialize(relayer);
    }

    /* ---------- initialize (genesis predeploy path) ---------- */

    function test_InitializeSetsRelayer() public {
        BridgeFactory fresh = new BridgeFactory();
        assertFalse(fresh.initialized());
        fresh.initialize(relayer);
        assertTrue(fresh.initialized());
        assertEq(fresh.relayer(), relayer);
    }

    function test_RevertIf_InitializeTwice() public {
        vm.expectRevert("BridgeFactory: already initialized");
        factory.initialize(address(1));
    }

    function test_RevertIf_InitializeZeroRelayer() public {
        BridgeFactory fresh = new BridgeFactory();
        vm.expectRevert("BridgeFactory: zero relayer");
        fresh.initialize(address(0));
    }

    /* ---------- createBridge ---------- */

    function test_CreateBridgeIsPermissionless() public {
        // Any account, not just the relayer, can create the canonical token.
        vm.prank(alice);
        address token = factory.createBridge("Bridged SUI", "SUI", "0x2::sui::SUI");

        assertEq(factory.tokenFor("0x2::sui::SUI"), token);
        Bridge bridge = Bridge(token);
        assertEq(bridge.coinType(), "0x2::sui::SUI");
        assertEq(bridge.name(), "Bridged SUI");
        assertEq(bridge.symbol(), "SUI");
        // The factory's relayer (not the creator) received the relayer role.
        assertEq(bridge.relayer(), relayer);
    }

    function test_CreateBridgeEmitsEvent() public {
        address expected = vm.computeCreateAddress(
            address(factory),
            vm.getNonce(address(factory))
        );
        vm.expectEmit(true, false, false, true);
        emit BridgeCreated(expected, "0x2::sui::SUI", "Bridged SUI", "SUI");
        factory.createBridge("Bridged SUI", "SUI", "0x2::sui::SUI");
    }

    function test_RevertIf_DuplicateCoinType() public {
        factory.createBridge("Bridged SUI", "SUI", "0x2::sui::SUI");
        vm.expectRevert("BridgeFactory: coin type already has a token");
        factory.createBridge("impersonator", "FAKE", "0x2::sui::SUI");
    }

    function test_RevertIf_EmptyCoinType() public {
        vm.expectRevert("BridgeFactory: empty coin type");
        factory.createBridge("x", "x", "");
    }

    function test_RevertIf_NotInitialized() public {
        BridgeFactory fresh = new BridgeFactory();
        vm.expectRevert("BridgeFactory: not initialized");
        fresh.createBridge("x", "x", "0x2::sui::SUI");
    }

    function test_DistinctCoinTypesGetDistinctTokens() public {
        address a = factory.createBridge("Bridged SUI", "SUI", "0x2::sui::SUI");
        address b = factory.createBridge("Bridged USDS", "USDS", "0xdba34672e30cb065b1f29e3e9aac1d9e9b7f24dd::usds::USDS");
        assertFalse(a == b);
        assertEq(factory.tokenFor("0x2::sui::SUI"), a);
        assertEq(
            factory.tokenFor("0xdba34672e30cb065b1f29e3e9aac1d9e9b7f24dd::usds::USDS"),
            b
        );
    }

    /* ---------- adoptBridge (genesis predeploy path) ---------- */

    /// Emulates a genesis predeploy: runtime bytecode planted with no
    /// constructor execution, then configured via `initialize`.
    function deployRawGenesisBridge(
        string memory name,
        string memory symbol,
        string memory coinType
    ) internal returns (Bridge) {
        address addr = address(uint160(uint256(keccak256("genesisBridge"))));
        vm.etch(addr, type(Bridge).runtimeCode);
        Bridge genesis = Bridge(addr);
        genesis.initialize(name, symbol, coinType, relayer);
        return genesis;
    }

    function test_AdoptBridgeRegistersPredeployedToken() public {
        Bridge genesis = deployRawGenesisBridge("Bridged SUI", "SUI", "0x2::sui::SUI");

        vm.prank(relayer);
        factory.adoptBridge("0x2::sui::SUI", address(genesis));

        assertEq(factory.tokenFor("0x2::sui::SUI"), address(genesis));
        // The relayer can mint through the adopted token like any created one.
        vm.prank(relayer);
        genesis.mint(alice, 1e9, 0);
        assertEq(genesis.balanceOf(alice), 1e9);
    }

    function test_AdoptBridgeEmitsSameEventAsCreation() public {
        Bridge genesis = deployRawGenesisBridge("Bridged SUI", "SUI", "0x2::sui::SUI");
        vm.prank(relayer);
        vm.expectEmit(true, false, false, true);
        emit BridgeCreated(address(genesis), "0x2::sui::SUI", "Bridged SUI", "SUI");
        factory.adoptBridge("0x2::sui::SUI", address(genesis));
    }

    function test_RevertIf_AdoptNotRelayer() public {
        Bridge genesis = deployRawGenesisBridge("Bridged SUI", "SUI", "0x2::sui::SUI");
        vm.prank(alice);
        vm.expectRevert("BridgeFactory: not relayer");
        factory.adoptBridge("0x2::sui::SUI", address(genesis));
    }

    function test_RevertIf_AdoptDuplicateCoinType() public {
        Bridge genesis = deployRawGenesisBridge("Bridged SUI", "SUI", "0x2::sui::SUI");
        vm.startPrank(relayer);
        factory.adoptBridge("0x2::sui::SUI", address(genesis));
        vm.expectRevert("BridgeFactory: coin type already has a token");
        factory.adoptBridge("0x2::sui::SUI", address(genesis));
        vm.stopPrank();
    }

    function test_RevertIf_AdoptCoinTypeMismatch() public {
        Bridge genesis = deployRawGenesisBridge("Bridged SUI", "SUI", "0x2::sui::SUI");
        // Adopting under a different coin type than the token reports.
        vm.prank(relayer);
        vm.expectRevert("BridgeFactory: token coin type mismatch");
        factory.adoptBridge("0x2::sui::SUI2", address(genesis));
    }

    function test_RevertIf_AdoptTokenRelayerMismatch() public {
        // A predeploy initialized with a different relayer must not be adoptable.
        address addr = address(uint160(uint256(keccak256("genesisBridge"))));
        vm.etch(addr, type(Bridge).runtimeCode);
        Bridge(addr).initialize("x", "x", "0x2::sui::SUI", address(0xBAD));

        vm.prank(relayer);
        vm.expectRevert("BridgeFactory: token relayer mismatch");
        factory.adoptBridge("0x2::sui::SUI", addr);
    }

    function test_RevertIf_AdoptZeroToken() public {
        vm.prank(relayer);
        vm.expectRevert("BridgeFactory: zero token");
        factory.adoptBridge("0x2::sui::SUI", address(0));
    }

    function test_RevertIf_AdoptOnUninitializedFactory() public {
        // An uninitialized factory has relayer == address(0), so any caller
        // hits the onlyRelayer guard first; adoption is doubly gated.
        BridgeFactory fresh = new BridgeFactory();
        vm.prank(relayer);
        vm.expectRevert("BridgeFactory: not relayer");
        fresh.adoptBridge("0x2::sui::SUI", address(1));
    }

    /* ---------- role rotation ---------- */

    function test_SetRelayerAppliesToNewTokensOnly() public {
        address first = factory.createBridge("a", "A", "0x1::a::A");

        address next = address(0xB0B);
        vm.prank(relayer);
        factory.setRelayer(next);
        assertEq(factory.relayer(), next);

        // Existing token keeps the old relayer...
        assertEq(Bridge(first).relayer(), relayer);
        // ...new tokens get the new one.
        address second = factory.createBridge("b", "B", "0x1::b::B");
        assertEq(Bridge(second).relayer(), next);
    }

    function test_RevertIf_SetRelayerNotRelayer() public {
        vm.prank(alice);
        vm.expectRevert("BridgeFactory: not relayer");
        factory.setRelayer(address(1));
    }

    function test_RevertIf_SetRelayerZero() public {
        vm.prank(relayer);
        vm.expectRevert("BridgeFactory: zero relayer");
        factory.setRelayer(address(0));
    }

    event BridgeCreated(address indexed token, string coinType, string name, string symbol);
}
