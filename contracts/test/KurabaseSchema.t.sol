// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {KurabaseSchema} from "../src/KurabaseSchema.sol";

interface Vm {
    struct Log {
        bytes32[] topics;
        bytes data;
        address emitter;
    }
    function addr(uint256 privateKey) external returns (address);
    function sign(uint256 privateKey, bytes32 digest) external returns (uint8 v, bytes32 r, bytes32 s);
    function prank(address sender) external;
    function expectRevert(bytes4 selector) external;
    function expectRevert(bytes calldata reason) external;
    function warp(uint256 timestamp) external;
    function chainId(uint256 chainId_) external;
    function etch(address target, bytes calldata code) external;
    function recordLogs() external;
    function getRecordedLogs() external returns (Log[] memory);
}

contract KurabaseSchemaTest {
    Vm private constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));
    uint256 private constant USER_KEY = 0xa11ce;
    uint256 private constant GATEWAY_KEY = 0xb0b;
    uint256 private constant ISSUER_KEY = 0xcafe;
    uint256 private constant DEVELOPER_KEY = 0xd00d;
    uint256 private constant DELEGATED_ISSUER_KEY = 0x1d3a;
    bytes32 private constant TABLE = bytes32(uint256(1));
    bytes32 private constant ROW = bytes32(uint256(2));
    bytes32 private constant COLUMN = bytes32(uint256(3));
    bytes32 private constant OTHER_ROW = bytes32(uint256(4));
    address private user;
    address private gateway;
    address private issuer;
    KurabaseSchema private db;

    function setUp() public {
        vm.warp(1000);
        user = vm.addr(USER_KEY);
        gateway = vm.addr(GATEWAY_KEY);
        issuer = vm.addr(ISSUER_KEY);
        db = new KurabaseSchema(address(this), issuer);
        db.setGateway(gateway, true, false);
    }

    function _session() private view returns (KurabaseSchema.Session memory) {
        (,, uint256 epoch) = db.gateways(gateway);
        return KurabaseSchema.Session(gateway, user, bytes32(uint256(uint160(user))), bytes32(0), 2000, 0, epoch);
    }

    function _sign(KurabaseSchema.Session memory session, uint256 key) private returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, db.hashSession(session));
        return abi.encodePacked(r, s, v);
    }

    function _operation(uint8 kind, bytes32 row, bytes32 column, bytes memory data)
        private
        pure
        returns (KurabaseSchema.Operation memory)
    {
        return KurabaseSchema.Operation(kind, TABLE, row, column, data);
    }

    function _insert(bytes32 row) private pure returns (KurabaseSchema.Operation[] memory ops) {
        ops = new KurabaseSchema.Operation[](2);
        ops[0] = _operation(2, row, bytes32(0), "");
        ops[1] = _operation(3, row, COLUMN, hex"0568656c6c6f");
    }

    function _execute(KurabaseSchema.Operation[] memory ops) private {
        KurabaseSchema.Session memory session = _session();
        bytes memory signature = _sign(session, USER_KEY);
        uint256 expected = db.revision();
        vm.prank(gateway);
        db.execute(expected, ops, session, signature);
    }

    function testOwnerAndDeveloperPrivilegedWrites() public {
        address developer = address(0xdede);
        db.setDeveloper(developer, true);
        vm.prank(developer);
        db.executePrivileged(0, _insert(ROW));
        require(db.rowExists(TABLE, ROW));
        db.executePrivileged(1, _insert(OTHER_ROW));
        require(db.revision() == 2);
    }

    function testIndependentUserSessionWritesAndReads() public {
        KurabaseSchema.Session memory session = _session();
        bytes memory signature = _sign(session, USER_KEY);
        require(db.verifySession(session, signature) == db.sessionDigest(session));
        vm.prank(gateway);
        db.execute(0, _insert(ROW), session, signature);
        (bool exists, bytes memory data) = db.getCell(TABLE, ROW, COLUMN);
        require(exists && keccak256(data) == keccak256(hex"0568656c6c6f"));
        require(db.rowVersion(TABLE, ROW) == 1);
        require(db.rowGeneration(TABLE, ROW) == 1);
    }

    function testUnauthorizedGatewayCannotUseLegitimateSession() public {
        address rogue = address(0xbad);
        KurabaseSchema.Session memory session = _session();
        session.gateway = rogue;
        bytes memory signature = _sign(session, USER_KEY);
        vm.expectRevert(KurabaseSchema.Unauthorized.selector);
        vm.prank(rogue);
        db.execute(0, _insert(ROW), session, signature);
        require(db.revision() == 0);
    }

    function testGatewayCannotForgeUserSession() public {
        KurabaseSchema.Session memory session = _session();
        bytes memory signature = _sign(session, GATEWAY_KEY);
        vm.expectRevert(KurabaseSchema.InvalidSignature.selector);
        vm.prank(gateway);
        db.execute(0, _insert(ROW), session, signature);
    }

    function testGatewayCannotUseUnsignedSession() public {
        KurabaseSchema.Session memory session = _session();
        vm.expectRevert(KurabaseSchema.InvalidSignature.selector);
        vm.prank(gateway);
        db.execute(0, _insert(ROW), session, "");
    }

    function testSessionCannotBeRelayedByAnotherGateway() public {
        address second = address(0x2222);
        db.setGateway(second, true, false);
        KurabaseSchema.Session memory session = _session();
        bytes memory signature = _sign(session, USER_KEY);
        vm.expectRevert(KurabaseSchema.InvalidSession.selector);
        vm.prank(second);
        db.execute(0, _insert(ROW), session, signature);
        session.gateway = second;
        vm.expectRevert(KurabaseSchema.InvalidSignature.selector);
        vm.prank(second);
        db.execute(0, _insert(ROW), session, signature);
    }

    function testMultipleGatewaysRequireSeparateUserGrants() public {
        address second = address(0x2222);
        db.setGateway(second, true, false);
        _execute(_insert(ROW));
        KurabaseSchema.Session memory session = _session();
        session.gateway = second;
        bytes memory signature = _sign(session, USER_KEY);
        vm.prank(second);
        db.execute(1, _insert(OTHER_ROW), session, signature);
        require(db.rowExists(TABLE, ROW) && db.rowExists(TABLE, OTHER_ROW));
    }

    function testUserCannotInventApplicationIdentityOrClaims() public {
        KurabaseSchema.Session memory session = _session();
        session.uid = keccak256("someone-else");
        bytes memory signature = _sign(session, USER_KEY);
        vm.expectRevert(KurabaseSchema.InvalidSignature.selector);
        db.verifySession(session, signature);
        session = _session();
        session.claimsHash = keccak256("admin");
        signature = _sign(session, USER_KEY);
        vm.expectRevert(KurabaseSchema.InvalidSignature.selector);
        db.verifySession(session, signature);
    }

    function testIndependentIssuerAttestsApplicationIdentityAndClaims() public {
        KurabaseSchema.Session memory session = _session();
        session.uid = keccak256("application-user-123");
        session.claimsHash = keccak256("verified-claims");
        bytes memory signature = _sign(session, ISSUER_KEY);
        require(db.verifySession(session, signature) == db.hashSession(session));
        vm.prank(gateway);
        db.execute(0, _insert(ROW), session, signature);
        require(db.revision() == 1);
    }

    function testRevokedIdentityAuthorityCannotIssueSessions() public {
        KurabaseSchema.Session memory session = _session();
        bytes memory signature = _sign(session, ISSUER_KEY);
        db.setIdentityAuthority(issuer, false);
        vm.expectRevert(KurabaseSchema.InvalidSignature.selector);
        db.verifySession(session, signature);
    }

    function testRevokedDeveloperInvalidatesItsIdentityAuthority() public {
        address developer = vm.addr(DEVELOPER_KEY);
        address delegatedIssuer = vm.addr(DELEGATED_ISSUER_KEY);
        db.setDeveloper(developer, true);
        vm.prank(developer);
        db.setIdentityAuthority(delegatedIssuer, true);
        KurabaseSchema.Session memory session = _session();
        session.uid = keccak256("application-user-delegated-by-developer");
        bytes memory signature = _sign(session, DELEGATED_ISSUER_KEY);
        require(db.verifySession(session, signature) == db.hashSession(session));

        db.setDeveloper(developer, false);
        require(!db.isIdentityAuthorityAuthorized(delegatedIssuer));
        vm.expectRevert(KurabaseSchema.InvalidSignature.selector);
        db.verifySession(session, signature);
        vm.expectRevert(KurabaseSchema.Unauthorized.selector);
        vm.prank(delegatedIssuer);
        db.revokeUserSessions(user, 1);
    }

    function testGatewayAndIdentityIssuerKeysMustBeSeparate() public {
        vm.expectRevert(KurabaseSchema.Unauthorized.selector);
        db.setIdentityAuthority(gateway, true);
        vm.expectRevert(KurabaseSchema.Unauthorized.selector);
        db.setGateway(issuer, true, false);
    }

    function testExpiryBoundaryIsExclusive() public {
        KurabaseSchema.Session memory session = _session();
        bytes memory signature = _sign(session, USER_KEY);
        vm.warp(session.expiresAt);
        vm.expectRevert(KurabaseSchema.SessionExpired.selector);
        db.verifySession(session, signature);
    }

    function testUserRevokesIndividualSession() public {
        KurabaseSchema.Session memory session = _session();
        bytes memory signature = _sign(session, USER_KEY);
        vm.prank(user);
        db.revokeSession(session);
        vm.expectRevert(KurabaseSchema.SessionRevoked.selector);
        db.verifySession(session, signature);
    }

    function testNonceRevocationIsMonotonicAndAllowsNewSessions() public {
        KurabaseSchema.Session memory session = _session();
        bytes memory signature = _sign(session, USER_KEY);
        vm.prank(user);
        db.revokeUserSessions(user, 1);
        vm.expectRevert(KurabaseSchema.SessionRevoked.selector);
        db.verifySession(session, signature);
        session.nonce = 1;
        signature = _sign(session, USER_KEY);
        require(db.verifySession(session, signature) == db.hashSession(session));
        vm.expectRevert(KurabaseSchema.InvalidSession.selector);
        vm.prank(user);
        db.revokeUserSessions(user, 0);
    }

    function testStrangerCannotRevokeSessions() public {
        KurabaseSchema.Session memory session = _session();
        vm.expectRevert(KurabaseSchema.Unauthorized.selector);
        vm.prank(address(0xbad));
        db.revokeSession(session);
        vm.expectRevert(KurabaseSchema.Unauthorized.selector);
        vm.prank(address(0xbad));
        db.revokeUserSessions(user, 1);
    }

    function testGatewayRevocationAndRegrantDoNotReviveSession() public {
        KurabaseSchema.Session memory session = _session();
        bytes memory signature = _sign(session, USER_KEY);
        db.setGateway(gateway, false, false);
        vm.expectRevert(KurabaseSchema.Unauthorized.selector);
        db.verifySession(session, signature);
        db.setGateway(gateway, true, false);
        vm.expectRevert(KurabaseSchema.InvalidSession.selector);
        db.verifySession(session, signature);
    }

    function testRevokedDeveloperCannotLeaveUsableChildGateway() public {
        address developer = address(0xdede);
        db.setDeveloper(developer, true);
        vm.prank(developer);
        db.setGateway(gateway, true, false);
        KurabaseSchema.Session memory session = _session();
        bytes memory signature = _sign(session, USER_KEY);
        db.setDeveloper(developer, false);
        require(!db.isGatewayAuthorized(gateway));
        vm.expectRevert(KurabaseSchema.Unauthorized.selector);
        db.verifySession(session, signature);
        db.setDeveloper(developer, true);
        require(!db.isGatewayAuthorized(gateway));
    }

    function testOwnerTransferInvalidatesOldOwnerGatewayGrants() public {
        db.transferOwnership(address(0x1234));
        require(!db.isGatewayAuthorized(gateway));
        vm.expectRevert(KurabaseSchema.Unauthorized.selector);
        db.setDeveloper(address(0x5555), true);
    }

    function testGatewayCannotExpandAuthority() public {
        vm.expectRevert(KurabaseSchema.Unauthorized.selector);
        vm.prank(gateway);
        db.setGateway(gateway, true, true);
        vm.expectRevert(KurabaseSchema.Unauthorized.selector);
        vm.prank(gateway);
        db.setDeveloper(gateway, true);
        vm.expectRevert(KurabaseSchema.Unauthorized.selector);
        vm.prank(gateway);
        db.executePrivileged(0, _insert(ROW));
    }

    function testPrivilegedGatewayRequiresExplicitScope() public {
        db.setGateway(gateway, true, true);
        vm.prank(gateway);
        db.executePrivileged(0, _insert(ROW));
        require(db.rowExists(TABLE, ROW));
        db.setGateway(gateway, false, false);
        vm.expectRevert(KurabaseSchema.Unauthorized.selector);
        vm.prank(gateway);
        db.executePrivileged(1, _insert(OTHER_ROW));
    }

    function testOrdinarySessionCannotModifyCatalogOrMigrations() public {
        KurabaseSchema.Operation[] memory ops = new KurabaseSchema.Operation[](1);
        ops[0] = KurabaseSchema.Operation(0, bytes32(0), bytes32(0), COLUMN, "policy");
        KurabaseSchema.Session memory session = _session();
        bytes memory signature = _sign(session, USER_KEY);
        vm.expectRevert(KurabaseSchema.Unauthorized.selector);
        vm.prank(gateway);
        db.execute(0, ops, session, signature);
        ops[0].kind = 6;
        vm.expectRevert(KurabaseSchema.Unauthorized.selector);
        vm.prank(gateway);
        db.execute(0, ops, session, signature);
    }

    function testSessionReplayAcrossInstancesFails() public {
        KurabaseSchema second = new KurabaseSchema(address(this), issuer);
        second.setGateway(gateway, true, false);
        KurabaseSchema.Session memory session = _session();
        bytes memory signature = _sign(session, USER_KEY);
        vm.expectRevert(KurabaseSchema.InvalidSignature.selector);
        second.verifySession(session, signature);
    }

    function testSessionReplayAcrossChainsFails() public {
        KurabaseSchema.Session memory session = _session();
        bytes memory signature = _sign(session, USER_KEY);
        vm.chainId(block.chainid + 1);
        vm.expectRevert(KurabaseSchema.InvalidSignature.selector);
        db.verifySession(session, signature);
    }

    function testMalformedAndMalleableSignaturesRejected() public {
        KurabaseSchema.Session memory session = _session();
        vm.expectRevert(KurabaseSchema.InvalidSignature.selector);
        db.verifySession(session, hex"00");
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(USER_KEY, db.hashSession(session));
        uint256 curveOrder = 0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141;
        bytes memory signature = abi.encodePacked(r, bytes32(curveOrder - uint256(s)), v == 27 ? uint8(28) : uint8(27));
        vm.expectRevert(KurabaseSchema.InvalidSignature.selector);
        db.verifySession(session, signature);
    }

    function testSchemaRevisionRejectsConcurrentDisjointWrites() public {
        _execute(_insert(ROW));
        KurabaseSchema.Session memory session = _session();
        bytes memory signature = _sign(session, USER_KEY);
        vm.expectRevert(abi.encodeWithSelector(KurabaseSchema.TransactionConflict.selector, 0, 1));
        vm.prank(gateway);
        db.execute(0, _insert(OTHER_ROW), session, signature);
        require(!db.rowExists(TABLE, OTHER_ROW));
        _execute(_insert(OTHER_ROW));
        require(db.revision() == 2);
    }

    function testInvalidLaterOperationRollsBackEarlierState() public {
        KurabaseSchema.Operation[] memory ops = _insert(ROW);
        ops[1] = _operation(3, OTHER_ROW, COLUMN, "bad");
        vm.expectRevert(abi.encodeWithSelector(KurabaseSchema.RowNotFound.selector, TABLE, OTHER_ROW));
        db.executePrivileged(0, ops);
        require(!db.rowExists(TABLE, ROW));
        require(db.rowGeneration(TABLE, ROW) == 0);
        require(db.rowVersion(TABLE, ROW) == 0);
        require(db.revision() == 0);
    }

    function testDeleteAndReinsertDoesNotResurrectCells() public {
        _execute(_insert(ROW));
        KurabaseSchema.Operation[] memory ops = new KurabaseSchema.Operation[](1);
        ops[0] = _operation(5, ROW, bytes32(0), "");
        _execute(ops);
        (bool exists,) = db.getCell(TABLE, ROW, COLUMN);
        require(!exists);
        ops[0].kind = 2;
        _execute(ops);
        (exists,) = db.getCell(TABLE, ROW, COLUMN);
        require(!exists && db.rowGeneration(TABLE, ROW) == 2);
    }

    function testDeleteCellAndExplicitNullAreDistinct() public {
        _execute(_insert(ROW));
        KurabaseSchema.Operation[] memory ops = new KurabaseSchema.Operation[](1);
        ops[0] = _operation(3, ROW, COLUMN, hex"00");
        _execute(ops);
        (bool exists, bytes memory value) = db.getCell(TABLE, ROW, COLUMN);
        require(exists && value.length == 1 && value[0] == 0);
        ops[0] = _operation(4, ROW, COLUMN, "");
        _execute(ops);
        (exists, value) = db.getCell(TABLE, ROW, COLUMN);
        require(!exists && value.length == 0);
    }

    function testDuplicateInsertAndMissingDeleteRejected() public {
        _execute(_insert(ROW));
        vm.expectRevert(abi.encodeWithSelector(KurabaseSchema.RowAlreadyExists.selector, TABLE, ROW));
        db.executePrivileged(1, _insert(ROW));
        KurabaseSchema.Operation[] memory ops = new KurabaseSchema.Operation[](1);
        ops[0] = _operation(5, OTHER_ROW, bytes32(0), "");
        vm.expectRevert(abi.encodeWithSelector(KurabaseSchema.RowNotFound.selector, TABLE, OTHER_ROW));
        db.executePrivileged(1, ops);
        require(db.revision() == 1);
    }

    function testCatalogAndMigrationAreAtomicAndReplayable() public {
        bytes32 key = keccak256("table:posts");
        bytes32 version = keccak256("202609090001");
        KurabaseSchema.Operation[] memory ops = new KurabaseSchema.Operation[](4);
        ops[0] = KurabaseSchema.Operation(0, bytes32(0), bytes32(0), key, "{\"name\":\"posts\"}");
        ops[1] = _operation(2, ROW, bytes32(0), "");
        ops[2] = _operation(3, ROW, COLUMN, hex"010203");
        ops[3] = KurabaseSchema.Operation(6, bytes32(0), bytes32(0), version, "202609090001");
        vm.recordLogs();
        db.executePrivileged(0, ops);
        Vm.Log[] memory logs = vm.getRecordedLogs();
        require(logs.length == 5);
        bytes32 operationTopic = keccak256("OperationApplied(uint256,uint256,uint8,bytes32,bytes32,bytes32,bytes)");
        for (uint256 i; i < ops.length; ++i) {
            require(logs[i].emitter == address(db));
            require(logs[i].topics[0] == operationTopic && logs[i].topics[1] == bytes32(uint256(1)));
            (uint256 index, uint8 kind, bytes32 tableId, bytes32 rowId, bytes32 columnId, bytes memory data) =
                abi.decode(logs[i].data, (uint256, uint8, bytes32, bytes32, bytes32, bytes));
            require(index == i && kind == ops[i].kind && tableId == ops[i].tableId);
            require(rowId == ops[i].rowId && columnId == ops[i].columnId);
            require(keccak256(data) == keccak256(ops[i].data));
        }
        require(db.appliedMigrations(version));
        require(db.catalogExists(key) && keccak256(db.catalog(key)) == keccak256(ops[0].data));
        ops = new KurabaseSchema.Operation[](2);
        ops[0] = _operation(2, OTHER_ROW, bytes32(0), "");
        ops[1] = KurabaseSchema.Operation(6, bytes32(0), bytes32(0), version, "202609090001");
        vm.expectRevert(abi.encodeWithSelector(KurabaseSchema.MigrationAlreadyApplied.selector, version));
        db.executePrivileged(1, ops);
        require(!db.rowExists(TABLE, OTHER_ROW) && db.revision() == 1);
    }

    function testCatalogDeletionPreservesExplicitExistence() public {
        KurabaseSchema.Operation[] memory ops = new KurabaseSchema.Operation[](1);
        ops[0] = KurabaseSchema.Operation(0, bytes32(0), bytes32(0), COLUMN, "");
        db.executePrivileged(0, ops);
        require(db.catalogExists(COLUMN) && db.catalog(COLUMN).length == 0);
        ops[0].kind = 1;
        db.executePrivileged(1, ops);
        require(!db.catalogExists(COLUMN));
        vm.expectRevert(abi.encodeWithSelector(KurabaseSchema.CatalogNotFound.selector, COLUMN));
        db.executePrivileged(2, ops);
    }

    function testPlanHashUsesPortableAbiAndBindsEveryContextField() public view {
        KurabaseSchema.Operation[] memory ops = _insert(ROW);
        bytes32 context = keccak256("actor");
        bytes32 expected = keccak256(abi.encode(block.chainid, address(db), uint256(0), context, ops));
        require(db.hashPlan(0, ops, context) == expected);
        require(db.hashPlan(1, ops, context) != expected);
        require(db.hashPlan(0, ops, bytes32(0)) != expected);
        ops[1].data = "changed";
        require(db.hashPlan(0, ops, context) != expected);
    }

    function testOffChainAbiAndEip712KnownVectors() public {
        vm.chainId(31337);
        address target = 0x1111111111111111111111111111111111111111;
        vm.etch(target, address(db).code);
        KurabaseSchema vectorDb = KurabaseSchema(target);
        bytes32 context = 0x2222222222222222222222222222222222222222222222222222222222222222;
        require(
            vectorDb.hashPlan(7, _insert(ROW), context)
                == 0xc61f3b202cf1cad0b4fe7ab1f8bc0611aeb2f634def00ffd80498bfcc497d020
        );
        KurabaseSchema.Session memory session = KurabaseSchema.Session(
            0x3333333333333333333333333333333333333333,
            0x4444444444444444444444444444444444444444,
            0x0000000000000000000000004444444444444444444444444444444444444444,
            bytes32(0),
            2000000000,
            5,
            2
        );
        require(vectorDb.domainSeparator() == 0x81a0712f291ad758daa858b7aaae0b3b7aeab08fa37d64fe30d4d7292db01444);
        require(vectorDb.hashSession(session) == 0x79e77f2053e7c565a151d3194d494cad7e5af6b623aec78a292a6f52b47715a4);
    }

    function testEmptyBatchAndOversizedBatchRejected() public {
        KurabaseSchema.Operation[] memory ops = new KurabaseSchema.Operation[](0);
        vm.expectRevert(KurabaseSchema.ResourceLimit.selector);
        db.executePrivileged(0, ops);
        ops = new KurabaseSchema.Operation[](1025);
        vm.expectRevert(KurabaseSchema.ResourceLimit.selector);
        db.executePrivileged(0, ops);
    }

    function testInvalidCanonicalOperationFieldsRejected() public {
        KurabaseSchema.Operation[] memory ops = _insert(ROW);
        ops[0].columnId = COLUMN;
        vm.expectRevert(abi.encodeWithSelector(KurabaseSchema.InvalidOperation.selector, 0));
        db.executePrivileged(0, ops);
        ops[0].columnId = bytes32(0);
        ops[1].kind = 255;
        vm.expectRevert(abi.encodeWithSelector(KurabaseSchema.InvalidOperation.selector, 1));
        db.executePrivileged(0, ops);
        require(!db.rowExists(TABLE, ROW));
    }

    function testZeroAdministrativeAddressesRejected() public {
        vm.expectRevert(KurabaseSchema.InvalidAddress.selector);
        new KurabaseSchema(address(0), address(0));
        vm.expectRevert(KurabaseSchema.InvalidAddress.selector);
        db.setDeveloper(address(0), true);
        vm.expectRevert(KurabaseSchema.InvalidAddress.selector);
        db.setGateway(address(0), true, false);
        vm.expectRevert(KurabaseSchema.InvalidAddress.selector);
        db.transferOwnership(address(0));
    }

    function testFuzzCellRoundTripAndRollback(bytes calldata value, uint64 rawRow) public {
        bytes32 row = bytes32(uint256(rawRow) + 1);
        KurabaseSchema.Operation[] memory ops = _insert(row);
        ops[1].data = value;
        db.executePrivileged(0, ops);
        (bool exists, bytes memory stored) = db.getCell(TABLE, row, COLUMN);
        require(exists && keccak256(stored) == keccak256(value));
        ops = new KurabaseSchema.Operation[](2);
        ops[0] = _operation(3, row, COLUMN, "changed");
        ops[1] = _operation(255, row, COLUMN, "");
        vm.expectRevert(abi.encodeWithSelector(KurabaseSchema.InvalidOperation.selector, 1));
        db.executePrivileged(1, ops);
        (, stored) = db.getCell(TABLE, row, COLUMN);
        require(keccak256(stored) == keccak256(value) && db.revision() == 1);
    }

    function testFuzzSessionFieldTampering(uint256 nonce, bytes32 arbitraryUid) public {
        KurabaseSchema.Session memory session = _session();
        bytes memory signature = _sign(session, USER_KEY);
        session.nonce = nonce | 1;
        session.uid = arbitraryUid == bytes32(0) ? bytes32(uint256(1)) : arbitraryUid;
        vm.expectRevert(KurabaseSchema.InvalidSignature.selector);
        db.verifySession(session, signature);
    }
}
