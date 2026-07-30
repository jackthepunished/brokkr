//! The client half of the leader redirect (Phase 5 I9b W3, plan §17 task 7:
//! "clients can talk to any; followers redirect to leader").
//!
//! I8c taught a follower to refuse a metadata write and name the leader; I9b
//! W1/W2 made that hint dialable (`x-brokkr-leader-addr`). These tests prove
//! the last link: a `BrokkrClient` pointed at a **follower** transparently
//! reaches the leader.
//!
//! Two real `ActionCacheService` servers are stood up on loopback — one backed
//! by a store that always answers `NotLeader` with a hint at the other, one
//! backed by a working store. No Raft is involved on purpose: what is under
//! test is the *client's* redirect behaviour, and driving it from a real
//! election would make the test slower and less precise about which branch it
//! exercised.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use brokkr_cas::{ActionCache, CasError};
use brokkr_common::Digest;
use brokkr_control::ActionCacheService;
use brokkr_proto::reapi_v2 as rapi;
use brokkr_proto::reapi_v2::action_cache_server::ActionCacheServer;
use brokkr_sdk::BrokkrClient;
use tokio::sync::Mutex;
use tonic::transport::Server;

/// A store that refuses everything with `NotLeader`, pointing at `leader_addr`.
#[derive(Debug)]
struct FollowerStore {
    leader: String,
    leader_addr: Option<String>,
}

#[async_trait]
impl ActionCache for FollowerStore {
    async fn get_action_result(
        &self,
        _digest: &Digest,
    ) -> Result<Option<rapi::ActionResult>, CasError> {
        Err(CasError::NotLeader {
            leader: Some(self.leader.clone()),
            leader_addr: self.leader_addr.clone(),
        })
    }

    async fn update_action_result(
        &self,
        _digest: &Digest,
        _result: rapi::ActionResult,
    ) -> Result<(), CasError> {
        Err(CasError::NotLeader {
            leader: Some(self.leader.clone()),
            leader_addr: self.leader_addr.clone(),
        })
    }
}

/// A store that works, and records what it was asked to write.
#[derive(Debug, Default)]
struct LeaderStore {
    stored: Mutex<Option<rapi::ActionResult>>,
}

#[async_trait]
impl ActionCache for LeaderStore {
    async fn get_action_result(
        &self,
        _digest: &Digest,
    ) -> Result<Option<rapi::ActionResult>, CasError> {
        Ok(self.stored.lock().await.clone())
    }

    async fn update_action_result(
        &self,
        _digest: &Digest,
        result: rapi::ActionResult,
    ) -> Result<(), CasError> {
        *self.stored.lock().await = Some(result);
        Ok(())
    }
}

/// Serve `backend` on an ephemeral loopback port; returns its `http://` URL.
async fn serve(backend: Arc<dyn ActionCache>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        Server::builder()
            .add_service(ActionCacheServer::new(ActionCacheService::new(backend)))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    // Let the listener start accepting before anyone dials it.
    tokio::time::sleep(Duration::from_millis(80)).await;
    format!("http://{addr}")
}

fn digest() -> rapi::Digest {
    rapi::Digest {
        hash: "a".repeat(64),
        size_bytes: 3,
    }
}

/// A client pointed at a follower reaches the leader, for both directions of
/// the ActionCache — the redirect is not a read-only convenience.
#[tokio::test]
async fn a_client_pointed_at_a_follower_reaches_the_leader() {
    let leader_store = Arc::new(LeaderStore::default());
    let leader_url = serve(leader_store.clone()).await;
    // The server advertises `host:port`; the client supplies the scheme.
    let leader_hostport = leader_url.trim_start_matches("http://").to_string();
    let follower_url = serve(Arc::new(FollowerStore {
        leader: "control-leader".to_string(),
        leader_addr: Some(leader_hostport),
    }))
    .await;

    let mut client = BrokkrClient::connect(follower_url).await.unwrap();

    // A write against the follower lands on the leader's store.
    let result = rapi::ActionResult {
        exit_code: 0,
        stdout_raw: b"hello".to_vec(),
        ..Default::default()
    };
    client
        .update_action_result(&digest(), result.clone())
        .await
        .expect("the write must follow the redirect, not fail");
    assert_eq!(
        leader_store
            .stored
            .lock()
            .await
            .as_ref()
            .unwrap()
            .stdout_raw,
        b"hello",
        "the leader actually stored it"
    );

    // And the read follows too.
    let fetched = client.get_action_result(&digest()).await.unwrap();
    assert_eq!(fetched.unwrap().stdout_raw, b"hello");
}

/// A follower that names a leader it cannot address (the window between an
/// election and that leader publishing `cfg/nodes/<id>`) must surface the
/// refusal *with its hint intact*, not a stripped or invented error — the
/// caller's own endpoint list is the better next move.
#[tokio::test]
async fn an_unroutable_hint_surfaces_the_original_refusal() {
    let follower_url = serve(Arc::new(FollowerStore {
        leader: "control-2".to_string(),
        leader_addr: None,
    }))
    .await;
    let mut client = BrokkrClient::connect(follower_url).await.unwrap();

    let status = client.get_action_result(&digest()).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        status
            .metadata()
            .get("x-brokkr-leader")
            .map(|v| v.to_str().unwrap()),
        Some("control-2"),
        "the leader hint must survive being reported back to the caller"
    );
}

/// A hint cycle — a follower that names *itself* — must terminate on the hop
/// budget rather than loop forever.
#[tokio::test]
async fn a_self_referential_hint_terminates() {
    // Bind first so the store can name its own address.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let backend: Arc<dyn ActionCache> = Arc::new(FollowerStore {
        leader: "itself".to_string(),
        leader_addr: Some(addr.to_string()),
    });
    tokio::spawn(async move {
        Server::builder()
            .add_service(ActionCacheServer::new(ActionCacheService::new(backend)))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(80)).await;

    let mut client = BrokkrClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(10), client.get_action_result(&digest()))
        .await
        .expect("the redirect loop must terminate, not hang")
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(
        status.message().contains("hops"),
        "the error should say the hop budget was spent, got: {}",
        status.message()
    );
}
