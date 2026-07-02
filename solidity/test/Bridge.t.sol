// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.24;

import {Test, console2} from "forge-std/Test.sol";
import {Bridge} from "../src/Bridge.sol";

/// Enshrined L2 bridge contract tests.
///
/// These pin the invariants the relayer relies on:
/// - the first mint must be nonce 0 (regression for the original off-by-one that
///   blocked the first deposit forever),
/// - mints are strictly increasing thereafter, so a replayed nonce reverts,
/// - only the relayer can mint / rotate the role,
/// - `initiateWithdrawal` burns exactly the caller's balance and is the only way
///   to create the logs the relayer releases on Sui.
contract BridgeTest is Test {
    Bridge internal bridge;
    address internal relayer = address(0xA11CE);
    address internal alice = address(0xA1);
    bytes32 internal suiRecipient = bytes32(uint256(uint160(0xB0B)));

    function setUp() public {
        vm.prank(relayer);
        bridge = new Bridge("Bridged USDSui", "USDS", "0x2::sui::SUI");
        // Fund alice with bridged tokens by minting from a known deposit.
        vm.startPrank(relayer);
        bridge.mint(alice, 1_000e9, 0); // first deposit nonce is 0
        vm.stopPrank();
    }

    /* ---------- mint: nonce-0 + strict ordering (regressions) ---------- */

    function test_MintNonceZeroSucceeds() public {
        // The first deposit ever must mint at nonce 0. Deploy fresh so the
        // setUp() mint doesn't mask this.
        vm.prank(relayer);
        Bridge fresh = new Bridge("x", "x", "0x2::sui::SUI");
        vm.prank(relayer);
        fresh.mint(alice, 5e9, 0);
        assertEq(fresh.lastMintedDepositNonce(), 0);
        assertTrue(fresh.mintedAny());
        assertEq(fresh.balanceOf(alice), 5e9);
    }

    function test_MintStrictlyIncreasing() public {
        vm.startPrank(relayer);
        bridge.mint(alice, 1e9, 1); // 0 already minted in setUp
        bridge.mint(alice, 1e9, 2);
        assertEq(bridge.lastMintedDepositNonce(), 2);
        vm.stopPrank();
    }

    function test_RevertIf_MintReplaysCurrentNonce() public {
        vm.prank(relayer);
        vm.expectRevert("Bridge: stale nonce");
        bridge.mint(alice, 1e9, 0); // 0 already minted in setUp
    }

    function test_MintAllowsSkippingNonce() public {
        // Skipping is legal: in a multi-coin vault, coin X's relayer mints the
        // global nonces that belong to X (e.g. 0, 2, 5) and skips the others.
        // Only going backwards / replaying is forbidden.
        vm.startPrank(relayer);
        bridge.mint(alice, 1e9, 2); // 1 belongs to another coin; 2 > 0 is fine
        bridge.mint(alice, 1e9, 5);
        assertEq(bridge.lastMintedDepositNonce(), 5);
        vm.stopPrank();
    }

    function test_RevertIf_MintGoesBackwards() public {
        vm.startPrank(relayer);
        bridge.mint(alice, 1e9, 5);
        vm.expectRevert("Bridge: stale nonce");
        bridge.mint(alice, 1e9, 4); // below the high-water mark
        vm.stopPrank();
    }

    function test_RevertIf_MintZeroRecipient() public {
        vm.prank(relayer);
        vm.expectRevert("Bridge: mint to zero");
        bridge.mint(address(0), 1e9, 1);
    }

    function test_MintEmitsDepositMintedAndTransfer() public {
        vm.prank(relayer);
        vm.expectEmit(true, true, false, true);
        emit DepositMinted(1, alice, 2e9, "0x2::sui::SUI");
        vm.expectEmit(true, true, false, true);
        emit Transfer(address(0), alice, 2e9);
        bridge.mint(alice, 2e9, 1);
    }

    /* ---------- access control ---------- */

    function test_RevertIf_MintNotRelayer() public {
        vm.prank(alice);
        vm.expectRevert("Bridge: not relayer");
        bridge.mint(alice, 1e9, 1);
    }

    function test_RevertIf_MarkWithdrawalProcessedNotRelayer() public {
        vm.prank(alice);
        vm.expectRevert("Bridge: not relayer");
        bridge.markWithdrawalProcessed(1);
    }

    function test_RevertIf_MarkWithdrawalProcessedStale() public {
        vm.startPrank(relayer);
        bridge.markWithdrawalProcessed(1);
        vm.expectRevert("Bridge: stale nonce");
        bridge.markWithdrawalProcessed(1);
        vm.stopPrank();
    }

    /* ---------- role rotation ---------- */

    function test_SetRelayer() public {
        address next = address(0xB0B);
        vm.prank(relayer);
        vm.expectEmit(true, true, false, false);
        emit RelayerChanged(relayer, next);
        bridge.setRelayer(next);

        assertEq(bridge.relayer(), next);
        // Old relayer is now locked out.
        vm.prank(relayer);
        vm.expectRevert("Bridge: not relayer");
        bridge.mint(alice, 1e9, 1);
        // New relayer can mint.
        vm.prank(next);
        bridge.mint(alice, 1e9, 1);
    }

    function test_RevertIf_SetRelayerZero() public {
        vm.prank(relayer);
        vm.expectRevert("Bridge: zero relayer");
        bridge.setRelayer(address(0));
    }

    function test_RevertIf_SetRelayerNotRelayer() public {
        vm.prank(alice);
        vm.expectRevert("Bridge: not relayer");
        bridge.setRelayer(address(1));
    }

    /* ---------- initialize (genesis predeploy path) ---------- */

    function test_InitializeSetsState() public {
        // Genesis predeploy runs no constructor, so it starts unconfigured and
        // the first initialize() sets metadata + relayer.
        Bridge genesis = deployRawGenesis();
        assertFalse(genesis.initialized());
        assertEq(genesis.relayer(), address(0));

        vm.prank(relayer);
        genesis.initialize("Bridged USDSui", "USDS", "0x2::sui::SUI", relayer);

        assertTrue(genesis.initialized());
        assertEq(genesis.name(), "Bridged USDSui");
        assertEq(genesis.symbol(), "USDS");
        assertEq(genesis.coinType(), "0x2::sui::SUI");
        assertEq(genesis.relayer(), relayer);
        // Mint works now that the relayer is set.
        vm.prank(relayer);
        genesis.mint(alice, 1e9, 0);
        assertEq(genesis.balanceOf(alice), 1e9);
    }

    function test_RevertIf_InitializeTwice() public {
        Bridge genesis = deployRawGenesis();
        vm.startPrank(relayer);
        genesis.initialize("x", "x", "0x2::sui::SUI", relayer);
        vm.expectRevert("Bridge: already initialized");
        genesis.initialize("y", "y", "z", address(1));
        vm.stopPrank();
    }

    function test_RevertIf_InitializeZeroRelayer() public {
        Bridge genesis = deployRawGenesis();
        vm.prank(relayer);
        vm.expectRevert("Bridge: zero relayer");
        genesis.initialize("x", "x", "0x2::sui::SUI", address(0));
    }

    function test_RevertIf_ConstructorThenInitialize() public {
        // A normally-deployed contract (constructor ran) is already initialized.
        vm.expectRevert("Bridge: already initialized");
        bridge.initialize("x", "x", "z", relayer);
    }

    /// Emulates a genesis predeploy: plants the runtime bytecode at a fresh
    /// address with NO constructor execution and empty storage, exactly as a
    /// genesis `alloc.code` entry would.
    function deployRawGenesis() internal returns (Bridge) {
        address addr = address(uint160(uint256(keccak256("genesisBridge"))));
        vm.etch(addr, type(Bridge).runtimeCode);
        return Bridge(addr);
    }

    /* ---------- initiateWithdrawal ---------- */

    function test_InitiateWithdrawalBurnsAndEmits() public {
        uint256 before = bridge.balanceOf(alice);
        uint256 supplyBefore = bridge.totalSupply();

        vm.prank(alice);
        vm.expectEmit(true, true, true, true);
        emit WithdrawalInitiated(1, alice, suiRecipient, 100e9);
        vm.expectEmit(true, true, false, true);
        emit Transfer(alice, address(0), 100e9);
        bridge.initiateWithdrawal(suiRecipient, 100e9);

        assertEq(bridge.balanceOf(alice), before - 100e9);
        assertEq(bridge.totalSupply(), supplyBefore - 100e9);
        assertEq(bridge.lastWithdrawalNonce(), 1);
    }

    function test_WithdrawalNonceIncrements() public {
        vm.startPrank(alice);
        bridge.initiateWithdrawal(suiRecipient, 1e9);
        bridge.initiateWithdrawal(suiRecipient, 1e9);
        vm.stopPrank();
        assertEq(bridge.lastWithdrawalNonce(), 2);
    }

    function test_RevertIf_WithdrawZeroAmount() public {
        vm.prank(alice);
        vm.expectRevert("Bridge: zero amount");
        bridge.initiateWithdrawal(suiRecipient, 0);
    }

    function test_RevertIf_WithdrawZeroRecipient() public {
        vm.prank(alice);
        vm.expectRevert("Bridge: zero recipient");
        bridge.initiateWithdrawal(bytes32(0), 1e9);
    }

    function test_RevertIf_WithdrawInsufficientBalance() public {
        uint256 tooMuch = bridge.balanceOf(alice) + 1;
        vm.prank(alice);
        vm.expectRevert("Bridge: insufficient balance");
        bridge.initiateWithdrawal(suiRecipient, tooMuch);
    }

    function test_InitiateWithdrawalOnlyBurnsCaller() public {
        // A second account cannot burn alice's balance even after an approval,
        // because initiateWithdrawal always debits msg.sender.
        address attacker = address(0xBAD);
        assertEq(bridge.balanceOf(attacker), 0);
        vm.prank(attacker);
        vm.expectRevert("Bridge: insufficient balance");
        bridge.initiateWithdrawal(suiRecipient, 1);
    }

    /* ---------- metadata ---------- */

    function test_Metadata() public view {
        assertEq(bridge.name(), "Bridged USDSui");
        assertEq(bridge.symbol(), "USDS");
        assertEq(uint8(bridge.decimals()), 9);
        assertEq(bridge.coinType(), "0x2::sui::SUI");
        assertEq(bridge.relayer(), relayer);
    }

    /* ---------- ERC20 ---------- */

    function test_Transfer() public {
        address bob = address(0xB0B);
        vm.prank(alice);
        assertTrue(bridge.transfer(bob, 50e9));
        assertEq(bridge.balanceOf(alice), 1_000e9 - 50e9);
        assertEq(bridge.balanceOf(bob), 50e9);
    }

    function test_RevertIf_TransferInsufficientBalance() public {
        uint256 tooMuch = bridge.balanceOf(alice) + 1;
        vm.prank(alice);
        vm.expectRevert("Bridge: insufficient balance");
        bridge.transfer(address(0xB0B), tooMuch);
    }

    function test_ApproveAndTransferFrom() public {
        address bob = address(0xB0B);
        vm.prank(alice);
        bridge.approve(bob, 30e9);
        assertEq(bridge.allowance(alice, bob), 30e9);

        vm.prank(bob);
        assertTrue(bridge.transferFrom(alice, bob, 10e9));
        assertEq(bridge.allowance(alice, bob), 20e9); // decremented
        assertEq(bridge.balanceOf(bob), 10e9);
    }

    function test_InfiniteAllowanceIsNotDecremented() public {
        address bob = address(0xB0B);
        vm.prank(alice);
        bridge.approve(bob, type(uint256).max);
        vm.prank(bob);
        bridge.transferFrom(alice, bob, 10e9);
        assertEq(bridge.allowance(alice, bob), type(uint256).max);
    }

    function test_TransferFromZeroAllowanceReverts() public {
        address bob = address(0xB0B);
        vm.prank(bob);
        vm.expectRevert("Bridge: insufficient allowance");
        bridge.transferFrom(alice, bob, 1);
    }

    /* ---------- events mirrors ---------- */

    event DepositMinted(uint64 indexed nonce, address indexed recipient, uint256 amount, string coinType);
    event WithdrawalInitiated(uint64 indexed nonce, address indexed from, bytes32 indexed suiRecipient, uint256 amount);
    event RelayerChanged(address indexed previous, address indexed next);
    event Transfer(address indexed from, address indexed to, uint256 value);
}
