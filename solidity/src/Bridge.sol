// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.24;

/// One bridged Sui coin type on L2, deployed per coin type by `BridgeFactory`.
///
/// The sequencer relayer mints here when it sees a matching `bridge::Deposit`
/// on Sui, and users call `initiateWithdrawal` to burn, which the relayer
/// forwards to Sui via `bridge::withdraw`. There is no external relayer: the
/// sequencer holds both `BridgeCap` (Sui) and the `relayer` role (L2).
contract Bridge {
    /* ---------- ERC20 metadata & balances ---------- */

    string public name;
    string public symbol;
    uint8 public constant decimals = 9;
    string public coinType;

    uint256 public totalSupply;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    /* ---------- Bridge state ---------- */

    /// Only this address may mint. Set at construction and rotable by the owner.
    address public relayer;
    /// Highest Sui deposit nonce already minted. Enforces strict ordering and
    /// makes relay retries idempotent. Meaningful only once `mintedAny` is true
    /// (0 is a valid first nonce, so the counter alone can't distinguish "nothing
    /// minted" from "nonce 0 minted").
    uint64 public lastMintedDepositNonce;
    /// True once the first mint has happened. Disambiguates the nonce-0 case.
    bool public mintedAny;
    /// Counter for L2 withdrawal nonces.
    uint64 public lastWithdrawalNonce;
    /// True once the contract has been configured (by constructor or
    /// `initialize`). The genesis predeploy runs no constructor, so it boots
    /// `initialized == false` and is configured via `initialize` once.
    bool public initialized;

    /* ---------- Events ---------- */

    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(
        address indexed owner,
        address indexed spender,
        uint256 value
    );

    event DepositMinted(
        uint64 indexed nonce,
        address indexed recipient,
        uint256 amount,
        string coinType
    );

    /// `suiRecipient` is a 32-byte Sui address.
    event WithdrawalInitiated(
        uint64 indexed nonce,
        address indexed from,
        bytes32 indexed suiRecipient,
        uint256 amount
    );

    event RelayerChanged(address indexed previous, address indexed next);

    modifier onlyRelayer() {
        require(msg.sender == relayer, "Bridge: not relayer");
        _;
    }

    /// `coinType_` is the Sui `TypeName` (e.g. "0x2::sui::SUI"); it is stored and
    /// echoed on mints so the relayer can sanity-check routing. `relayer_` is
    /// normally the factory's stored relayer (the sequencer's EVM key).
    constructor(
        string memory name_,
        string memory symbol_,
        string memory coinType_,
        address relayer_
    ) {
        _setup(name_, symbol_, coinType_, relayer_);
    }

    /// One-time configuration for the genesis predeploy, where the constructor
    /// never ran. Callable by anyone, but exactly once: the first caller sets the
    /// metadata and the initial `relayer`. On a fresh single-sequencer chain the
    /// operator calls this before adopting the token into the factory.
    function initialize(
        string memory name_,
        string memory symbol_,
        string memory coinType_,
        address relayer_
    ) external {
        require(!initialized, "Bridge: already initialized");
        _setup(name_, symbol_, coinType_, relayer_);
    }

    function _setup(
        string memory name_,
        string memory symbol_,
        string memory coinType_,
        address relayer_
    ) internal {
        require(relayer_ != address(0), "Bridge: zero relayer");
        initialized = true;
        name = name_;
        symbol = symbol_;
        coinType = coinType_;
        relayer = relayer_;
        emit RelayerChanged(address(0), relayer_);
    }

    /* ---------- Relayer ops ---------- */

    /// Mints bridged tokens for a Sui deposit. The first mint must be nonce 0;
    /// every subsequent mint must be strictly greater than the last. Resubmitting
    /// a stale nonce reverts so retries are safe. The relayed `coinType` is this
    /// contract's own (`coinType`), emitted on success.
    function mint(
        address recipient,
        uint256 amount,
        uint64 nonce
    ) external onlyRelayer {
        require(
            mintedAny ? nonce > lastMintedDepositNonce : nonce == 0,
            "Bridge: stale nonce"
        );
        require(recipient != address(0), "Bridge: mint to zero");
        lastMintedDepositNonce = nonce;
        mintedAny = true;
        totalSupply += amount;
        balanceOf[recipient] += amount;
        emit DepositMinted(nonce, recipient, amount, coinType);
        emit Transfer(address(0), recipient, amount);
    }

    function setRelayer(address next) external onlyRelayer {
        require(next != address(0), "Bridge: zero relayer");
        emit RelayerChanged(relayer, next);
        relayer = next;
    }

    /* ---------- User ops ---------- */

    /// Burns `amount` of the caller's tokens, requesting a release to
    /// `suiRecipient` on Sui. The relayer watches `WithdrawalInitiated` and calls
    /// `bridge::withdraw`.
    function initiateWithdrawal(bytes32 suiRecipient, uint256 amount) external {
        require(amount > 0, "Bridge: zero amount");
        require(suiRecipient != bytes32(0), "Bridge: zero recipient");
        uint256 bal = balanceOf[msg.sender];
        require(bal >= amount, "Bridge: insufficient balance");
        unchecked {
            balanceOf[msg.sender] = bal - amount;
            totalSupply -= amount;
        }
        uint64 nonce = lastWithdrawalNonce + 1;
        lastWithdrawalNonce = nonce;
        emit WithdrawalInitiated(nonce, msg.sender, suiRecipient, amount);
        emit Transfer(msg.sender, address(0), amount);
    }

    /* ---------- ERC20 ---------- */

    function transfer(address to, uint256 amount) external returns (bool) {
        _transfer(msg.sender, to, amount);
        return true;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        emit Approval(msg.sender, spender, amount);
        return true;
    }

    function transferFrom(
        address from,
        address to,
        uint256 amount
    ) external returns (bool) {
        uint256 allowed = allowance[from][msg.sender];
        if (allowed != type(uint256).max) {
            require(allowed >= amount, "Bridge: insufficient allowance");
            allowance[from][msg.sender] = allowed - amount;
        }
        _transfer(from, to, amount);
        return true;
    }

    function _transfer(address from, address to, uint256 amount) internal {
        require(to != address(0), "Bridge: transfer to zero");
        uint256 bal = balanceOf[from];
        require(bal >= amount, "Bridge: insufficient balance");
        unchecked {
            balanceOf[from] = bal - amount;
            balanceOf[to] += amount;
        }
        emit Transfer(from, to, amount);
    }
}
