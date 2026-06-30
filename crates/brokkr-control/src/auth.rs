//! Client authentication (ADR 0011).
//!
//! Clients present a JWT bearer token; the control plane validates its
//! signature, expiry, and (when configured) issuer / audience, and reads the
//! tenant from a configured claim. That authenticated tenant is *authoritative*
//! — it overrides the client-asserted `x-brokkr-tenant` header (ADR 0010).
//!
//! [`Authenticator::Disabled`] is "open mode": no token is required and the
//! tenant falls back to the header (with a loud startup warning, like the
//! TLS-disabled posture). This keeps local dev and the in-process fixtures
//! working until an operator configures verification.
//!
//! First increment: validation against a *configured* key (HMAC secret or RSA
//! public key). Live OIDC/JWKS-URL discovery is a deferred follow-up — see ADR
//! 0011.

use std::sync::Arc;

use brokkr_common::{IdError, TenantId};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use thiserror::Error;
use tonic::{Request, Status};

/// Why a request failed authentication.
#[derive(Debug, Error)]
pub enum AuthError {
    /// Auth is enabled but the request carried no bearer token.
    #[error("missing Authorization bearer token")]
    MissingToken,
    /// The token failed validation (bad signature, expired, wrong iss/aud, …).
    #[error("invalid token: {0}")]
    InvalidToken(String),
    /// The token validated but has no tenant claim.
    #[error("token is missing the '{0}' tenant claim")]
    MissingTenantClaim(String),
    /// The tenant claim is present but not a valid [`TenantId`].
    #[error("invalid tenant in token: {0}")]
    InvalidTenant(#[source] IdError),
}

/// A configured JWT validator: a decoding key, validation rules, and which
/// claim carries the tenant id.
pub struct JwtAuth {
    decoding_key: DecodingKey,
    validation: Validation,
    tenant_claim: String,
}

impl JwtAuth {
    fn new(decoding_key: DecodingKey, alg: Algorithm, tenant_claim: impl Into<String>) -> Self {
        let mut validation = Validation::new(alg);
        // `exp` is required by default (good). Audience is only checked when the
        // operator opts in via `with_audience`; otherwise tokens without an
        // `aud` claim are accepted.
        validation.validate_aud = false;
        Self {
            decoding_key,
            validation,
            tenant_claim: tenant_claim.into(),
        }
    }

    /// HMAC (HS256) validator from a shared `secret`.
    pub fn hmac(secret: &[u8], tenant_claim: impl Into<String>) -> Self {
        Self::new(
            DecodingKey::from_secret(secret),
            Algorithm::HS256,
            tenant_claim,
        )
    }

    /// RSA (RS256) validator from a PEM-encoded public key (the common OIDC
    /// case).
    pub fn rsa_pem(pem: &[u8], tenant_claim: impl Into<String>) -> Result<Self, AuthError> {
        let key = DecodingKey::from_rsa_pem(pem)
            .map_err(|e| AuthError::InvalidToken(format!("bad RSA public key: {e}")))?;
        Ok(Self::new(key, Algorithm::RS256, tenant_claim))
    }

    /// Require this issuer (`iss`).
    pub fn with_issuer(mut self, issuer: &str) -> Self {
        self.validation.set_issuer(&[issuer]);
        self
    }

    /// Require this audience (`aud`).
    pub fn with_audience(mut self, audience: &str) -> Self {
        self.validation.set_audience(&[audience]);
        self
    }

    /// Validate `token` and return the authenticated tenant from its claim.
    pub fn authenticate(&self, token: &str) -> Result<TenantId, AuthError> {
        let data = decode::<serde_json::Value>(token, &self.decoding_key, &self.validation)
            .map_err(|e| AuthError::InvalidToken(e.to_string()))?;
        let tenant = data
            .claims
            .get(&self.tenant_claim)
            .and_then(|v| v.as_str())
            .ok_or_else(|| AuthError::MissingTenantClaim(self.tenant_claim.clone()))?;
        TenantId::new(tenant.to_string()).map_err(AuthError::InvalidTenant)
    }
}

/// Client authentication policy for the control plane.
pub enum Authenticator {
    /// Open mode — no auth; the tenant comes from the request header (ADR 0010).
    /// The binary logs a prominent warning at startup in this mode.
    Disabled,
    /// JWT bearer auth; the token's tenant claim is authoritative. Boxed
    /// because `JwtAuth` (decoding key + validation) is much larger than the
    /// unit `Disabled` variant.
    Jwt(Box<JwtAuth>),
}

impl Authenticator {
    /// Whether authentication is enforced.
    pub fn is_enabled(&self) -> bool {
        matches!(self, Authenticator::Jwt(_))
    }

    /// Authenticate a request's bearer token (the value *after* `Bearer `).
    ///
    /// - `Disabled` → `Ok(None)`: the caller falls back to the tenant header.
    /// - `Jwt` → `Ok(Some(tenant))` on a valid token, else an [`AuthError`]
    ///   (missing or invalid). The returned tenant is authoritative.
    pub fn authenticate(&self, bearer: Option<&str>) -> Result<Option<TenantId>, AuthError> {
        match self {
            Authenticator::Disabled => Ok(None),
            Authenticator::Jwt(jwt) => {
                let token = bearer.ok_or(AuthError::MissingToken)?;
                jwt.authenticate(token).map(Some)
            }
        }
    }
}

/// Strip the `Bearer ` / `bearer ` prefix from an `authorization` value.
fn strip_bearer(value: &str) -> Option<&str> {
    value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
}

/// Build a tonic interceptor that authenticates the `authorization: Bearer`
/// token against `auth` and, on success, injects the authoritative
/// [`TenantId`] into the request's extensions (the handler then prefers it over
/// the `x-brokkr-tenant` header).
///
/// - Auth enabled + missing/invalid token → `UNAUTHENTICATED` (the request
///   never reaches the handler).
/// - Auth enabled + valid token → request passes with the tenant in extensions.
/// - Open mode (`Disabled`) → request passes unchanged (tenant from header).
///
/// Apply it only to the client-facing services; the internal `WorkerService`
/// is authenticated by mTLS instead.
pub fn auth_interceptor(
    auth: Arc<Authenticator>,
) -> impl FnMut(Request<()>) -> Result<Request<()>, Status> + Clone {
    move |mut req: Request<()>| {
        let bearer = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(strip_bearer);
        match auth.authenticate(bearer) {
            Ok(Some(tenant)) => {
                req.extensions_mut().insert(tenant);
                Ok(req)
            }
            Ok(None) => Ok(req),
            Err(e) => Err(Status::unauthenticated(e.to_string())),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;

    use super::*;

    // A fixed far-future expiry (year ~2100) so tokens are valid without
    // reading the wall clock in tests.
    const FAR_FUTURE: u64 = 4_102_444_800;

    fn hs256(secret: &[u8], claims: serde_json::Value) -> String {
        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(secret),
        )
        .unwrap()
    }

    #[test]
    fn valid_token_yields_tenant() {
        let secret = b"topsecret";
        let auth = JwtAuth::hmac(secret, "tenant");
        let tok = hs256(secret, json!({ "tenant": "team-a", "exp": FAR_FUTURE }));
        assert_eq!(auth.authenticate(&tok).unwrap().as_str(), "team-a");
    }

    #[test]
    fn wrong_signature_rejected() {
        let auth = JwtAuth::hmac(b"right-key", "tenant");
        let tok = hs256(b"wrong-key", json!({ "tenant": "t", "exp": FAR_FUTURE }));
        assert!(matches!(
            auth.authenticate(&tok),
            Err(AuthError::InvalidToken(_))
        ));
    }

    #[test]
    fn expired_token_rejected() {
        let secret = b"s";
        let auth = JwtAuth::hmac(secret, "tenant");
        let tok = hs256(secret, json!({ "tenant": "t", "exp": 100u64 })); // 1970
        assert!(matches!(
            auth.authenticate(&tok),
            Err(AuthError::InvalidToken(_))
        ));
    }

    #[test]
    fn missing_tenant_claim_rejected() {
        let secret = b"s";
        let auth = JwtAuth::hmac(secret, "tenant");
        let tok = hs256(secret, json!({ "exp": FAR_FUTURE }));
        assert!(matches!(
            auth.authenticate(&tok),
            Err(AuthError::MissingTenantClaim(_))
        ));
    }

    #[test]
    fn issuer_and_audience_enforced_when_configured() {
        let secret = b"s";
        let auth = JwtAuth::hmac(secret, "tenant")
            .with_issuer("brokkr")
            .with_audience("brokkr-api");
        let good = hs256(
            secret,
            json!({ "tenant": "t", "exp": FAR_FUTURE, "iss": "brokkr", "aud": "brokkr-api" }),
        );
        assert!(auth.authenticate(&good).is_ok());
        let bad_iss = hs256(
            secret,
            json!({ "tenant": "t", "exp": FAR_FUTURE, "iss": "evil", "aud": "brokkr-api" }),
        );
        assert!(auth.authenticate(&bad_iss).is_err());
    }

    #[test]
    fn disabled_authenticator_defers_to_header() {
        let auth = Authenticator::Disabled;
        assert!(!auth.is_enabled());
        assert!(auth.authenticate(Some("ignored")).unwrap().is_none());
        assert!(auth.authenticate(None).unwrap().is_none());
    }

    #[test]
    fn jwt_authenticator_requires_a_token() {
        let auth = Authenticator::Jwt(Box::new(JwtAuth::hmac(b"s", "tenant")));
        assert!(auth.is_enabled());
        assert!(matches!(
            auth.authenticate(None),
            Err(AuthError::MissingToken)
        ));
    }

    #[test]
    fn jwt_authenticator_returns_authoritative_tenant() {
        let secret = b"s";
        let auth = Authenticator::Jwt(Box::new(JwtAuth::hmac(secret, "tenant")));
        let tok = hs256(secret, json!({ "tenant": "team-x", "exp": FAR_FUTURE }));
        let tenant = auth.authenticate(Some(&tok)).unwrap().unwrap();
        assert_eq!(tenant.as_str(), "team-x");
    }

    fn request_with_auth(value: Option<&str>) -> Request<()> {
        let mut req = Request::new(());
        if let Some(v) = value {
            req.metadata_mut()
                .insert("authorization", v.parse().unwrap());
        }
        req
    }

    #[test]
    fn interceptor_injects_tenant_for_valid_token() {
        let secret = b"s";
        let auth = Arc::new(Authenticator::Jwt(Box::new(JwtAuth::hmac(
            secret, "tenant",
        ))));
        let tok = hs256(secret, json!({ "tenant": "team-y", "exp": FAR_FUTURE }));
        let mut interceptor = auth_interceptor(auth);
        let req = interceptor(request_with_auth(Some(&format!("Bearer {tok}")))).unwrap();
        let tenant = req.extensions().get::<TenantId>().unwrap();
        assert_eq!(tenant.as_str(), "team-y");
    }

    #[test]
    fn interceptor_rejects_missing_and_invalid_tokens() {
        let auth = Arc::new(Authenticator::Jwt(Box::new(JwtAuth::hmac(b"s", "tenant"))));
        let mut interceptor = auth_interceptor(auth);
        // No authorization header.
        let err = interceptor(request_with_auth(None)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        // Garbage token.
        let err = interceptor(request_with_auth(Some("Bearer not-a-jwt"))).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn interceptor_open_mode_passes_through_without_tenant() {
        let auth = Arc::new(Authenticator::Disabled);
        let mut interceptor = auth_interceptor(auth);
        let req = interceptor(request_with_auth(None)).unwrap();
        assert!(req.extensions().get::<TenantId>().is_none());
    }
}
