use alloy::{
    network::EthereumWallet,
    primitives::{Address, Bytes, B256, U256},
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::types::Filter,
    signers::local::PrivateKeySigner,
    sol,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

sol! {
    #[sol(rpc)]
    interface KurabaseSchema {
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
        function revision() external view returns (uint256);
        function owner() external view returns (address);
        function developers(address account) external view returns (bool);
        function gateways(address account) external view returns (bool enabled, bool privileged, uint256 epoch);
        function isGatewayAuthorized(address account) external view returns (bool);
        function verifySession(Session calldata session, bytes calldata signature) external view returns (bytes32);
        function executePrivileged(uint256 expectedRevision, Operation[] calldata ops) external;
        function execute(uint256 expectedRevision, Operation[] calldata ops, Session calldata session, bytes calldata signature) external;
        event OperationApplied(uint256 indexed revision, uint256 index, uint8 kind, bytes32 tableId, bytes32 rowId, bytes32 columnId, bytes data);
    }
}

pub use KurabaseSchema::{Operation, Session};

#[derive(Clone)]
pub struct Chain {
    provider: DynProvider,
    pub address: Address,
    pub signer: Address,
    deployment_block: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub revision: u64,
    pub kind: u8,
    pub table: B256,
    pub row: B256,
    pub column: B256,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChainSnapshot {
    pub block_number: u64,
    pub block_hash: B256,
    pub revision: u64,
    pub changes: Vec<Change>,
}

impl Chain {
    pub async fn verify_session(&self, session: Session, signature: Bytes) -> Result<()> {
        if session.gateway != self.signer { bail!("Session belongs to a different gateway"); }
        KurabaseSchema::new(self.address, &self.provider).verifySession(session, signature).call().await?;
        Ok(())
    }

    pub async fn privileged(&self) -> Result<bool> {
        let instance = KurabaseSchema::new(self.address, &self.provider);
        if instance.owner().call().await? == self.signer || instance.developers(self.signer).call().await? { return Ok(true); }
        let grant = instance.gateways(self.signer).call().await?;
        Ok(grant.privileged && instance.isGatewayAuthorized(self.signer).call().await?)
    }

    pub async fn info(&self) -> Result<serde_json::Value> {
        let instance = KurabaseSchema::new(self.address, &self.provider);
        let grant = instance.gateways(self.signer).call().await?;
        Ok(serde_json::json!({"contract":self.address,"gateway":self.signer,"chainId":self.provider.get_chain_id().await?,"owner":instance.owner().call().await?,"gatewayAuthorized":instance.isGatewayAuthorized(self.signer).call().await?,"gatewayEpoch":grant.epoch.to_string(),"privileged":self.privileged().await?}))
    }
    pub async fn connect(rpc: &str, address: &str, private_key: &str, deployment_block: u64) -> Result<Self> {
        let signer: PrivateKeySigner = private_key.parse().context("Invalid gateway private key")?;
        let signer_address = signer.address();
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .connect_http(rpc.parse().context("Invalid RPC URL")?)
            .erased();
        Ok(Self { provider, address: address.parse()?, signer: signer_address, deployment_block })
    }

    /// Pin head once and replay only through that exact block. No stale fallback is allowed.
    pub async fn snapshot(&self) -> Result<ChainSnapshot> {
        let head = self.provider.get_block_number().await?;
        let block = self.provider.get_block_by_number(head.into()).await?.context("Chain head unavailable")?;
        let instance = KurabaseSchema::new(self.address, &self.provider);
        let revision: u64 = instance.revision().block(head.into()).call().await?.try_into()?;
        let logs = self.provider.get_logs(&Filter::new().address(self.address).from_block(self.deployment_block).to_block(head)).await?;
        let mut changes = Vec::new();
        for log in logs {
            if let Ok(decoded) = log.log_decode::<KurabaseSchema::OperationApplied>() {
                let e = decoded.inner.data;
                changes.push(Change { revision: e.revision.try_into()?, kind: e.kind, table: e.tableId, row: e.rowId, column: e.columnId, data: e.data.to_vec() });
            }
        }
        Ok(ChainSnapshot { block_number: head, block_hash: block.header.hash, revision, changes })
    }

    pub async fn commit(&self, revision: u64, ops: Vec<Operation>, session: Option<(Session, Bytes)>) -> Result<B256> {
        if ops.is_empty() { bail!("Empty WritePlan"); }
        let instance = KurabaseSchema::new(self.address, &self.provider);
        let receipt = if let Some((session, signature)) = session {
            instance.execute(U256::from(revision), ops, session, signature).send().await?.get_receipt().await?
        } else {
            instance.executePrivileged(U256::from(revision), ops).send().await?.get_receipt().await?
        };
        if !receipt.status() { bail!("Chain commit reverted"); }
        Ok(receipt.transaction_hash)
    }
}

pub fn id(value: u64) -> B256 { B256::from(U256::from(value).to_be_bytes::<32>()) }
pub fn key(value: &str) -> B256 { alloy::primitives::keccak256(value.as_bytes()) }
pub fn operation(kind: u8, table: B256, row: B256, column: B256, data: Vec<u8>) -> Operation {
    Operation { kind, tableId: table, rowId: row, columnId: column, data: data.into() }
}
