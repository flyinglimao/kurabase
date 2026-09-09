use alloy::primitives::{Address, B256, Bytes, U256};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use kura_chain::{Chain, Session};
use kurasql::AuthContext;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::ApiError;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct SessionPayload {
    pub gateway: Address,
    pub user: Address,
    pub uid: B256,
    pub claims_hash: B256,
    pub expires_at: String,
    pub nonce: String,
    pub gateway_epoch: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub session: SessionPayload,
    pub signature: Bytes,
    #[serde(default)]
    pub claims: Value,
}
pub struct Auth { pub context: AuthContext, pub envelope: Option<Envelope> }

impl Envelope {
    pub fn session(&self) -> Result<Session, ApiError> {
        let uint = |s: &str| s.parse::<U256>().map_err(|_| ApiError::unauthorized("Invalid session integer"));
        Ok(Session { gateway:self.session.gateway, user:self.session.user, uid:self.session.uid, claimsHash:self.session.claims_hash, expiresAt:uint(&self.session.expires_at)?, nonce:uint(&self.session.nonce)?, gatewayEpoch:uint(&self.session.gateway_epoch)? })
    }
    pub fn token(&self) -> Result<String, ApiError> {
        Ok(format!("kura.{}",URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).map_err(ApiError::internal)?)))
    }
    pub fn decode(token: &str) -> Result<Self, ApiError> {
        let bytes = URL_SAFE_NO_PAD.decode(token.strip_prefix("kura.").ok_or_else(|| ApiError::unauthorized("Expected signed Kurabase session"))?).map_err(|_| ApiError::unauthorized("Malformed session token"))?;
        serde_json::from_slice(&bytes).map_err(|_| ApiError::unauthorized("Malformed session envelope"))
    }
    pub async fn verify(&self, chain: &Chain) -> Result<AuthContext, ApiError> {
        chain.verify_session(self.session()?, self.signature.clone()).await.map_err(|_| ApiError::unauthorized("Session invalid, expired, revoked, or gateway not delegated"))?;
        if self.session.claims_hash == B256::ZERO {
            if !self.claims.is_null() { return Err(ApiError::unauthorized("Unattested JWT claims")); }
        } else {
            let encoded = serde_json::to_vec(&self.claims).map_err(ApiError::internal)?;
            if alloy::primitives::keccak256(encoded) != self.session.claims_hash { return Err(ApiError::unauthorized("Claims do not match signed hash")); }
        }
        let uid = if let Some(subject)=self.claims.get("sub").and_then(Value::as_str) {
            if alloy::primitives::keccak256(subject.as_bytes()) != self.session.uid { return Err(ApiError::unauthorized("Application subject does not match signed uid")); }
            subject.to_string()
        } else if self.session.uid == B256::left_padding_from(self.session.user.as_slice()) { format!("{:#x}",self.session.user) } else { format!("{:#x}",self.session.uid) };
        let mut context = AuthContext::authenticated(uid);
        if let Some(role)=self.claims.get("role").and_then(Value::as_str) {
            match role {
                "anon" => { context.role="anon".into(); context.uid=None; },
                "authenticated" => {},
                _ => return Err(ApiError::unauthorized("User sessions cannot claim administrative roles")),
            }
        }
        context.jwt_claims = self.claims.clone();
        Ok(context)
    }
}
