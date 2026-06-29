//! gRPC-level tests for client authentication (ADR 0011): the auth interceptor
//! gates real client calls. Uses the lightweight `Capabilities` service (it
//! returns immediately and needs no scheduler/worker) wrapped with the same
//! `auth_interceptor` the binary applies to all client-facing services.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::disallowed_methods,
    dead_code
)]

use std::sync::Arc;
use std::time::Duration;

use brokkr_control::{auth_interceptor, Authenticator, CapabilitiesService, JwtAuth};
use brokkr_proto::reapi_v2::capabilities_client::CapabilitiesClient;
use brokkr_proto::reapi_v2::capabilities_server::CapabilitiesServer;
use brokkr_proto::reapi_v2::GetCapabilitiesRequest;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::json;
use tonic::transport::Server;
use tonic::Request;

/// Far-future expiry (~2100) so minted tokens are valid without the wall clock.
const FAR_FUTURE: u64 = 4_102_444_800;

/// Boot a control-plane `Capabilities` service guarded by HMAC JWT auth; return
/// its endpoint URL.
async fn boot_authed_server(secret: &[u8]) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let auth = Arc::new(Authenticator::Jwt(Box::new(JwtAuth::hmac(
        secret, "tenant",
    ))));
    tokio::spawn(async move {
        Server::builder()
            .add_service(CapabilitiesServer::with_interceptor(
                CapabilitiesService,
                auth_interceptor(auth),
            ))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    format!("http://{addr}")
}

fn bearer(secret: &[u8], tenant: &str) -> String {
    let jwt = encode(
        &Header::new(Algorithm::HS256),
        &json!({ "tenant": tenant, "exp": FAR_FUTURE }),
        &EncodingKey::from_secret(secret),
    )
    .unwrap();
    format!("Bearer {jwt}")
}

#[tokio::test]
async fn rejects_call_without_a_token() {
    let secret = b"shared-secret";
    let endpoint = boot_authed_server(secret).await;
    let mut client = CapabilitiesClient::connect(endpoint).await.unwrap();
    let status = client
        .get_capabilities(GetCapabilitiesRequest::default())
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn rejects_call_with_an_invalid_token() {
    let secret = b"shared-secret";
    let endpoint = boot_authed_server(secret).await;
    let mut client = CapabilitiesClient::connect(endpoint).await.unwrap();
    let mut req = Request::new(GetCapabilitiesRequest::default());
    req.metadata_mut()
        .insert("authorization", "Bearer not-a-valid-jwt".parse().unwrap());
    let status = client.get_capabilities(req).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn accepts_call_with_a_valid_token() {
    let secret = b"shared-secret";
    let endpoint = boot_authed_server(secret).await;
    let mut client = CapabilitiesClient::connect(endpoint).await.unwrap();
    let mut req = Request::new(GetCapabilitiesRequest::default());
    req.metadata_mut()
        .insert("authorization", bearer(secret, "team-a").parse().unwrap());
    let resp = client.get_capabilities(req).await;
    assert!(resp.is_ok(), "valid token should pass auth, got {resp:?}");
}
