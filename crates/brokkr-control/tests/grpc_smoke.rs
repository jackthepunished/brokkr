//! Boots the control plane gRPC server in-process, then exercises CAS,
//! Capabilities, and ActionCache via real Tonic clients.

#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use brokkr_cas::{RedbActionCache, RedbCas};
use brokkr_common::Digest;
use brokkr_control::{
    ActionCacheService, CapabilitiesService, CasService, ExecutionService, Scheduler,
};
use brokkr_proto::reapi_v2::{
    self as rapi, action_cache_client::ActionCacheClient, action_cache_server::ActionCacheServer,
    capabilities_client::CapabilitiesClient, capabilities_server::CapabilitiesServer,
    content_addressable_storage_client::ContentAddressableStorageClient,
    content_addressable_storage_server::ContentAddressableStorageServer,
    execution_server::ExecutionServer,
};
use tokio::net::TcpListener;
use tonic::transport::{Channel, Server};

async fn boot_server() -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cas = Arc::new(RedbCas::open(dir.path().join("cas.redb")).unwrap());
    let ac = Arc::new(RedbActionCache::open(dir.path().join("ac.redb")).unwrap());
    let scheduler = Scheduler::new(cas.clone(), ac.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    tokio::spawn(async move {
        Server::builder()
            .add_service(ContentAddressableStorageServer::new(CasService::new(cas)))
            .add_service(ActionCacheServer::new(ActionCacheService::new(ac)))
            .add_service(CapabilitiesServer::new(CapabilitiesService))
            .add_service(ExecutionServer::new(ExecutionService::new(scheduler)))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    // Give the spawned server a tick to start accepting before clients connect.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (format!("http://{addr}"), dir)
}

async fn client(addr: &str) -> Channel {
    Channel::from_shared(addr.to_string())
        .unwrap()
        .connect()
        .await
        .unwrap()
}

#[tokio::test]
async fn capabilities_returns_sha256_and_v2() {
    let (addr, _dir) = boot_server().await;
    let mut c = CapabilitiesClient::new(client(&addr).await);
    let resp = c
        .get_capabilities(rapi::GetCapabilitiesRequest::default())
        .await
        .unwrap()
        .into_inner();
    let cache = resp.cache_capabilities.unwrap();
    assert!(cache
        .digest_functions
        .contains(&(rapi::digest_function::Value::Sha256 as i32)));
    assert_eq!(resp.high_api_version.unwrap().major, 2);
}

#[tokio::test]
async fn cas_roundtrip_and_find_missing() {
    let (addr, _dir) = boot_server().await;
    let mut cas = ContentAddressableStorageClient::new(client(&addr).await);

    let payload = b"grpc hello";
    let d = Digest::of(payload);
    let proto_d = rapi::Digest {
        hash: d.hash().to_string(),
        size_bytes: d.size_bytes(),
    };

    let upd = cas
        .batch_update_blobs(rapi::BatchUpdateBlobsRequest {
            instance_name: String::new(),
            requests: vec![rapi::batch_update_blobs_request::Request {
                digest: Some(proto_d.clone()),
                data: payload.to_vec(),
                compressor: 0,
            }],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(upd.responses.len(), 1);
    assert_eq!(upd.responses[0].status.as_ref().unwrap().code, 0);

    let read = cas
        .batch_read_blobs(rapi::BatchReadBlobsRequest {
            instance_name: String::new(),
            digests: vec![proto_d.clone()],
            acceptable_compressors: vec![],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(read.responses[0].data, payload);

    let other = rapi::Digest {
        hash: Digest::of(b"nope").hash().to_string(),
        size_bytes: 4,
    };
    let missing = cas
        .find_missing_blobs(rapi::FindMissingBlobsRequest {
            instance_name: String::new(),
            blob_digests: vec![proto_d, other.clone()],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(missing.missing_blob_digests, vec![other]);
}

#[tokio::test]
async fn action_cache_miss_then_hit() {
    let (addr, _dir) = boot_server().await;
    let mut ac = ActionCacheClient::new(client(&addr).await);

    let action_d = rapi::Digest {
        hash: Digest::of(b"action xyz").hash().to_string(),
        size_bytes: Digest::of(b"action xyz").size_bytes(),
    };

    let miss = ac
        .get_action_result(rapi::GetActionResultRequest {
            instance_name: String::new(),
            action_digest: Some(action_d.clone()),
            inline_stdout: false,
            inline_stderr: false,
            inline_output_files: vec![],
            digest_function: 0,
        })
        .await;
    assert_eq!(
        miss.unwrap_err().code(),
        tonic::Code::NotFound,
        "first lookup should miss"
    );

    let result = rapi::ActionResult {
        stdout_raw: b"cached stdout".to_vec(),
        exit_code: 0,
        ..Default::default()
    };
    ac.update_action_result(rapi::UpdateActionResultRequest {
        instance_name: String::new(),
        action_digest: Some(action_d.clone()),
        action_result: Some(result.clone()),
        results_cache_policy: None,
        digest_function: 0,
    })
    .await
    .unwrap();

    let hit = ac
        .get_action_result(rapi::GetActionResultRequest {
            instance_name: String::new(),
            action_digest: Some(action_d),
            inline_stdout: false,
            inline_stderr: false,
            inline_output_files: vec![],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(hit.stdout_raw, b"cached stdout");
}
