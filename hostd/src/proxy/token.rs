//! Ephemeral JWTs authenticating proxy requests to a VM's exposed ports.
//!
//! hostd mints short-lived tokens scoped to one VM + one exposed port (via
//! `POST /api/vms/{id}/ports/{port}/token`); the proxy server verifies them
//! on every request to learn which guest to forward to. Tokens are signed
//! with an HMAC secret generated randomly at hostd startup and never
//! persisted, so a daemon restart invalidates every outstanding token — the
//! same ephemeral model as the VMs themselves.
//!
//! The `proto` claim distinguishes the forwarding mode. Only `http` exists
//! today; a `tcp` variant (e.g. Postgres wire protocol) will carry tokens in
//! the protocol's own handshake instead of an HTTP header.

use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::vmm::vm::VmId;

/// Default token lifetime when the mint request does not specify one.
pub(crate) const DEFAULT_TTL_SECS: u64 = 15 * 60;
/// Longest lifetime a mint request may ask for.
pub(crate) const MAX_TTL_SECS: u64 = 24 * 60 * 60;

/// Forwarding mode a token is valid for. Only `http` is implemented; `tcp` is
/// reserved for the planned raw-TCP proxying (see the `proxy` module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Proto {
    Http,
}

/// JWT claims identifying the forwarding target of one proxy request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Claims {
    pub vm_id: String,
    pub port: u16,
    pub proto: Proto,
    pub iat: u64,
    pub exp: u64,
}

/// Mints and verifies proxy JWTs against a per-boot HMAC secret.
pub(crate) struct ProxyTokens {
    secret: [u8; 32],
}

impl ProxyTokens {
    pub(crate) fn new() -> Self {
        Self {
            secret: rand::rng().random(),
        }
    }

    /// Mint a token for `vm_id` + `port`, valid for `ttl_secs` (clamped to
    /// 1..=MAX_TTL_SECS). Returns the token and its expiry time.
    pub(crate) fn mint(
        &self,
        vm_id: &VmId,
        port: u16,
        ttl_secs: u64,
    ) -> Result<(String, DateTime<Utc>)> {
        let ttl = ttl_secs.clamp(1, MAX_TTL_SECS) as i64;
        let now = Utc::now();
        let expires_at = now + Duration::seconds(ttl);
        let claims = Claims {
            vm_id: vm_id.to_string(),
            port,
            proto: Proto::Http,
            iat: now.timestamp() as u64,
            exp: expires_at.timestamp() as u64,
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(&self.secret),
        )
        .map_err(|e| Error::proxy_token(e.to_string()))?;
        Ok((token, expires_at))
    }

    /// Verify a token's signature and expiry, returning its claims.
    pub(crate) fn verify(&self, token: &str) -> Result<Claims> {
        // No expiry leeway: proxy tokens are meant to be short-lived.
        let mut validation = Validation::default();
        validation.leeway = 0;
        decode::<Claims>(
            token,
            &DecodingKey::from_secret(&self.secret),
            &validation,
        )
        .map(|data| data.claims)
        .map_err(|e| {
            let msg = match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => "token expired".to_string(),
                _ => format!("invalid token: {e}"),
            };
            Error::proxy_token(msg)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm_id() -> VmId {
        VmId::from("vm-1-test")
    }

    #[test]
    fn mint_verify_roundtrip() {
        let tokens = ProxyTokens::new();
        let (token, expires_at) = tokens.mint(&vm_id(), 8080, 60).unwrap();
        let claims = tokens.verify(&token).unwrap();
        assert_eq!(claims.vm_id, "vm-1-test");
        assert_eq!(claims.port, 8080);
        assert_eq!(claims.proto, Proto::Http);
        assert!(claims.exp > claims.iat);
        assert!(expires_at > Utc::now());
    }

    #[test]
    fn ttl_is_clamped() {
        let tokens = ProxyTokens::new();
        let (_, expires_at) = tokens.mint(&vm_id(), 8080, MAX_TTL_SECS * 10).unwrap();
        let max_exp = Utc::now() + Duration::seconds(MAX_TTL_SECS as i64 + 5);
        assert!(expires_at <= max_exp);
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let issuer = ProxyTokens::new();
        let other = ProxyTokens::new();
        let (token, _) = issuer.mint(&vm_id(), 8080, 60).unwrap();
        assert!(matches!(
            other.verify(&token),
            Err(Error::ProxyToken(_))
        ));
    }

    #[test]
    fn verify_rejects_expired_token() {
        let tokens = ProxyTokens::new();
        // Encode an already-expired token by hand (mint clamps ttl to >= 1s).
        let now = Utc::now();
        let claims = Claims {
            vm_id: vm_id().to_string(),
            port: 8080,
            proto: Proto::Http,
            iat: (now - Duration::seconds(120)).timestamp() as u64,
            exp: (now - Duration::seconds(60)).timestamp() as u64,
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(&tokens.secret),
        )
        .unwrap();
        let err = tokens.verify(&token).unwrap_err();
        assert!(matches!(err, Error::ProxyToken(ref m) if m == "token expired"));
    }
}
