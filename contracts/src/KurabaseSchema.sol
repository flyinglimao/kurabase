// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @notice Canonical generic storage with explicitly delegated SQL execution.
/// @dev SQL policy/constraint evaluation is entrusted to a delegated gateway, but
/// its user execution authority requires a session independently signed by the
/// user or an instance-configured identity authority. Values are opaque bytes.
contract KurabaseSchema {
    struct Operation {
        uint8 kind;
        bytes32 tableId;
        bytes32 rowId;
        bytes32 columnId;
        bytes data;
    }

    struct Session {
        address gateway;
        address user;
        bytes32 uid;
        bytes32 claimsHash;
        uint256 expiresAt;
        uint256 nonce;
        uint256 gatewayEpoch;
    }

    struct GatewayGrant {
        bool enabled;
        bool privileged;
        uint256 epoch;
    }

    uint8 public constant PUT_CATALOG = 0;
    uint8 public constant DELETE_CATALOG = 1;
    uint8 public constant INSERT_ROW = 2;
    uint8 public constant SET_CELL = 3;
    uint8 public constant DELETE_CELL = 4;
    uint8 public constant DELETE_ROW = 5;
    uint8 public constant MARK_MIGRATION = 6;
    uint256 public constant MAX_OPERATIONS = 1024;

    bytes32 public constant SESSION_TYPEHASH = keccak256(
        "Session(address gateway,address user,bytes32 uid,bytes32 claimsHash,uint256 expiresAt,uint256 nonce,uint256 gatewayEpoch)"
    );
    bytes32 private constant DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
    uint256 private constant SECP256K1_HALF_N = 0x7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0;

    address public owner;
    uint256 public revision;
    mapping(address => bool) public developers;
    mapping(address => GatewayGrant) public gateways;
    mapping(address => uint256) public administratorEpoch;
    mapping(address => address) public gatewayDelegator;
    mapping(address => uint256) public gatewayDelegatorEpoch;
    mapping(address => bool) public identityAuthorities;
    mapping(address => address) public identityAuthorityDelegator;
    mapping(address => uint256) public identityAuthorityDelegatorEpoch;
    mapping(address => uint256) public minimumSessionNonce;
    mapping(bytes32 => bool) public revokedSessions;
    mapping(bytes32 => bytes) public catalog;
    mapping(bytes32 => bool) public catalogExists;
    mapping(bytes32 => bool) public appliedMigrations;
    mapping(bytes32 => mapping(bytes32 => bool)) public rowExists;
    mapping(bytes32 => mapping(bytes32 => uint256)) public rowVersion;
    mapping(bytes32 => mapping(bytes32 => uint256)) public rowGeneration;
    mapping(bytes32 => bytes) private cells;
    mapping(bytes32 => bool) private cellExists;

    error Unauthorized();
    error InvalidAddress();
    error TransactionConflict(uint256 expected, uint256 actual);
    error InvalidSession();
    error SessionExpired();
    error SessionRevoked();
    error InvalidSignature();
    error InvalidOperation(uint256 index);
    error RowAlreadyExists(bytes32 tableId, bytes32 rowId);
    error RowNotFound(bytes32 tableId, bytes32 rowId);
    error CatalogNotFound(bytes32 key);
    error MigrationAlreadyApplied(bytes32 version);
    error ResourceLimit();

    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);
    event DeveloperChanged(address indexed developer, bool enabled);
    event GatewayChanged(address indexed gateway, bool enabled, bool privileged, uint256 epoch);
    event IdentityAuthorityChanged(address indexed authority, bool enabled);
    event SessionRevocation(bytes32 indexed sessionDigest);
    event SessionNonceChanged(address indexed user, uint256 minimumNonce);
    event OperationApplied(
        uint256 indexed revision,
        uint256 index,
        uint8 kind,
        bytes32 tableId,
        bytes32 rowId,
        bytes32 columnId,
        bytes data
    );
    event PlanCommitted(
        uint256 indexed revision,
        bytes32 indexed planHash,
        address indexed executor,
        address user,
        bytes32 uid,
        bytes32 claimsHash,
        bytes32 actorContextHash
    );

    constructor(address initialOwner, address initialIdentityAuthority) {
        if (initialOwner == address(0)) revert InvalidAddress();
        owner = initialOwner;
        emit OwnershipTransferred(address(0), initialOwner);
        if (initialIdentityAuthority != address(0)) {
            identityAuthorities[initialIdentityAuthority] = true;
            identityAuthorityDelegator[initialIdentityAuthority] = initialOwner;
            identityAuthorityDelegatorEpoch[initialIdentityAuthority] = administratorEpoch[initialOwner];
            emit IdentityAuthorityChanged(initialIdentityAuthority, true);
        }
    }

    modifier onlyOwner() {
        if (msg.sender != owner) revert Unauthorized();
        _;
    }

    modifier onlyAdministrator() {
        if (!_isAdministrator(msg.sender)) revert Unauthorized();
        _;
    }

    function transferOwnership(address nextOwner) external onlyOwner {
        if (nextOwner == address(0)) revert InvalidAddress();
        emit OwnershipTransferred(owner, nextOwner);
        ++administratorEpoch[owner];
        owner = nextOwner;
    }

    function setDeveloper(address developer, bool enabled) external onlyOwner {
        if (developer == address(0)) revert InvalidAddress();
        developers[developer] = enabled;
        ++administratorEpoch[developer];
        emit DeveloperChanged(developer, enabled);
    }

    /// @dev Every grant update invalidates sessions from the previous grant.
    function setGateway(address gateway, bool enabled, bool privileged) external onlyAdministrator {
        if (gateway == address(0)) revert InvalidAddress();
        if (enabled && identityAuthorities[gateway]) revert Unauthorized();
        GatewayGrant storage grant = gateways[gateway];
        grant.enabled = enabled;
        grant.privileged = enabled && privileged;
        ++grant.epoch;
        gatewayDelegator[gateway] = msg.sender;
        gatewayDelegatorEpoch[gateway] = administratorEpoch[msg.sender];
        emit GatewayChanged(gateway, enabled, grant.privileged, grant.epoch);
    }

    function setIdentityAuthority(address authority, bool enabled) external onlyAdministrator {
        if (authority == address(0)) revert InvalidAddress();
        if (enabled && gateways[authority].enabled) revert Unauthorized();
        identityAuthorities[authority] = enabled;
        identityAuthorityDelegator[authority] = msg.sender;
        identityAuthorityDelegatorEpoch[authority] = administratorEpoch[msg.sender];
        emit IdentityAuthorityChanged(authority, enabled);
    }

    function revokeSession(Session calldata session) external {
        if (
            msg.sender != session.user && msg.sender != session.gateway && !_isAdministrator(msg.sender)
                && !isIdentityAuthorityAuthorized(msg.sender)
        ) revert Unauthorized();
        bytes32 digest = hashSession(session);
        revokedSessions[digest] = true;
        emit SessionRevocation(digest);
    }

    /// @notice Invalidates all older sessions for a user; the floor only increases.
    function revokeUserSessions(address user, uint256 minimumNonce) external {
        if (msg.sender != user && !_isAdministrator(msg.sender) && !isIdentityAuthorityAuthorized(msg.sender)) {
            revert Unauthorized();
        }
        if (minimumNonce <= minimumSessionNonce[user]) revert InvalidSession();
        minimumSessionNonce[user] = minimumNonce;
        emit SessionNonceChanged(user, minimumNonce);
    }

    function domainSeparator() public view returns (bytes32) {
        return keccak256(
            abi.encode(DOMAIN_TYPEHASH, keccak256("KurabaseSchema"), keccak256("1"), block.chainid, address(this))
        );
    }

    function isGatewayAuthorized(address gateway) public view returns (bool) {
        address delegator = gatewayDelegator[gateway];
        return gateways[gateway].enabled && _isAdministrator(delegator)
            && gatewayDelegatorEpoch[gateway] == administratorEpoch[delegator];
    }

    /// @notice An authority inherits the lifetime of the administrator that delegated it.
    /// Revoking that developer (or transferring ownership) disables its signing and
    /// revocation powers without requiring a separate authority transaction.
    function isIdentityAuthorityAuthorized(address authority) public view returns (bool) {
        address delegator = identityAuthorityDelegator[authority];
        return identityAuthorities[authority] && _isAdministrator(delegator)
            && identityAuthorityDelegatorEpoch[authority] == administratorEpoch[delegator];
    }

    function hashSession(Session calldata session) public view returns (bytes32) {
        bytes32 structHash = keccak256(
            abi.encode(
                SESSION_TYPEHASH,
                session.gateway,
                session.user,
                session.uid,
                session.claimsHash,
                session.expiresAt,
                session.nonce,
                session.gatewayEpoch
            )
        );
        return keccak256(abi.encodePacked(hex"1901", domainSeparator(), structHash));
    }

    function sessionDigest(Session calldata session) external view returns (bytes32) {
        return hashSession(session);
    }

    /// @notice Read authentication helper. The HTTP gateway must also check that
    /// session.gateway is its configured executor address. No msg.sender binding
    /// is required for this read-only check; execute enforces that binding.
    function verifySession(Session calldata session, bytes calldata signature) external view returns (bytes32) {
        return _validateSession(session, signature);
    }

    /// @dev Standard ABI encoding, not packed encoding; operation order is significant.
    function hashPlan(uint256 expectedRevision, Operation[] calldata operations, bytes32 actorContextHash)
        public
        view
        returns (bytes32)
    {
        return keccak256(abi.encode(block.chainid, address(this), expectedRevision, actorContextHash, operations));
    }

    function execute(
        uint256 expectedRevision,
        Operation[] calldata operations,
        Session calldata session,
        bytes calldata signature
    ) external returns (bytes32 planHash) {
        if (session.gateway != msg.sender) revert InvalidSession();
        bytes32 actorContextHash = _validateSession(session, signature);
        planHash = _apply(expectedRevision, operations, actorContextHash, false);
        emit PlanCommitted(
            revision, planHash, msg.sender, session.user, session.uid, session.claimsHash, actorContextHash
        );
    }

    /// @notice Explicit service/admin scope, separate from normal user sessions.
    function executePrivileged(uint256 expectedRevision, Operation[] calldata operations)
        external
        returns (bytes32 planHash)
    {
        GatewayGrant memory grant = gateways[msg.sender];
        if (!_isAdministrator(msg.sender) && !(isGatewayAuthorized(msg.sender) && grant.privileged)) {
            revert Unauthorized();
        }
        bytes32 actorContextHash = keccak256(abi.encode(msg.sender));
        planHash = _apply(expectedRevision, operations, actorContextHash, true);
        emit PlanCommitted(revision, planHash, msg.sender, msg.sender, bytes32(0), bytes32(0), actorContextHash);
    }

    function getCell(bytes32 tableId, bytes32 rowId, bytes32 columnId)
        external
        view
        returns (bool exists, bytes memory data)
    {
        if (!rowExists[tableId][rowId]) return (false, bytes(""));
        bytes32 key = _cellKey(tableId, rowId, columnId);
        return (cellExists[key], cells[key]);
    }

    function _isAdministrator(address account) private view returns (bool) {
        return account == owner || developers[account];
    }

    function _validateSession(Session calldata session, bytes calldata signature)
        private
        view
        returns (bytes32 digest)
    {
        GatewayGrant memory grant = gateways[session.gateway];
        if (!isGatewayAuthorized(session.gateway)) revert Unauthorized();
        if (session.user == address(0) || session.gatewayEpoch != grant.epoch || session.uid == bytes32(0)) {
            revert InvalidSession();
        }
        if (block.timestamp >= session.expiresAt) revert SessionExpired();
        digest = hashSession(session);
        if (revokedSessions[digest] || session.nonce < minimumSessionNonce[session.user]) revert SessionRevoked();
        address signer = _recover(digest, signature);
        if (isIdentityAuthorityAuthorized(signer)) return digest;
        if (
            signer != session.user || session.uid != bytes32(uint256(uint160(session.user)))
                || session.claimsHash != bytes32(0)
        ) revert InvalidSignature();
    }

    function _recover(bytes32 digest, bytes calldata signature) private pure returns (address signer) {
        if (signature.length != 65) revert InvalidSignature();
        bytes32 r;
        bytes32 s;
        uint8 v;
        assembly {
            r := calldataload(signature.offset)
            s := calldataload(add(signature.offset, 32))
            v := byte(0, calldataload(add(signature.offset, 64)))
        }
        if (uint256(s) > SECP256K1_HALF_N || (v != 27 && v != 28)) revert InvalidSignature();
        signer = ecrecover(digest, v, r, s);
        if (signer == address(0)) revert InvalidSignature();
    }

    function _apply(
        uint256 expectedRevision,
        Operation[] calldata operations,
        bytes32 actorContextHash,
        bool privileged
    ) private returns (bytes32 planHash) {
        if (expectedRevision != revision) revert TransactionConflict(expectedRevision, revision);
        if (operations.length == 0 || operations.length > MAX_OPERATIONS) revert ResourceLimit();
        planHash = hashPlan(expectedRevision, operations, actorContextHash);
        uint256 nextRevision = revision + 1;
        for (uint256 i; i < operations.length; ++i) {
            Operation calldata op = operations[i];
            if (op.kind == PUT_CATALOG || op.kind == DELETE_CATALOG || op.kind == MARK_MIGRATION) {
                if (!privileged) revert Unauthorized();
                if (op.tableId != bytes32(0) || op.rowId != bytes32(0) || op.columnId == bytes32(0)) {
                    revert InvalidOperation(i);
                }
                if (op.kind == PUT_CATALOG) {
                    catalog[op.columnId] = op.data;
                    catalogExists[op.columnId] = true;
                } else if (op.kind == DELETE_CATALOG) {
                    if (op.data.length != 0) revert InvalidOperation(i);
                    if (!catalogExists[op.columnId]) revert CatalogNotFound(op.columnId);
                    delete catalog[op.columnId];
                    delete catalogExists[op.columnId];
                } else {
                    if (op.data.length == 0) revert InvalidOperation(i);
                    if (appliedMigrations[op.columnId]) revert MigrationAlreadyApplied(op.columnId);
                    appliedMigrations[op.columnId] = true;
                }
            } else if (op.kind >= INSERT_ROW && op.kind <= DELETE_ROW) {
                if (op.tableId == bytes32(0) || op.rowId == bytes32(0)) revert InvalidOperation(i);
                if (op.kind == INSERT_ROW || op.kind == DELETE_ROW) {
                    if (op.columnId != bytes32(0) || op.data.length != 0) revert InvalidOperation(i);
                } else if (op.columnId == bytes32(0)) {
                    revert InvalidOperation(i);
                }
                if (op.kind == INSERT_ROW) {
                    if (rowExists[op.tableId][op.rowId]) revert RowAlreadyExists(op.tableId, op.rowId);
                    rowExists[op.tableId][op.rowId] = true;
                    ++rowGeneration[op.tableId][op.rowId];
                } else {
                    if (!rowExists[op.tableId][op.rowId]) revert RowNotFound(op.tableId, op.rowId);
                    if (op.kind == DELETE_ROW) {
                        delete rowExists[op.tableId][op.rowId];
                    } else {
                        bytes32 key = _cellKey(op.tableId, op.rowId, op.columnId);
                        if (op.kind == SET_CELL) {
                            cells[key] = op.data;
                            cellExists[key] = true;
                        } else {
                            if (op.data.length != 0) revert InvalidOperation(i);
                            delete cells[key];
                            delete cellExists[key];
                        }
                    }
                }
                rowVersion[op.tableId][op.rowId] = nextRevision;
            } else {
                revert InvalidOperation(i);
            }
            emit OperationApplied(nextRevision, i, op.kind, op.tableId, op.rowId, op.columnId, op.data);
        }
        revision = nextRevision;
    }

    function _cellKey(bytes32 tableId, bytes32 rowId, bytes32 columnId) private view returns (bytes32) {
        return keccak256(abi.encode(tableId, rowId, rowGeneration[tableId][rowId], columnId));
    }
}
