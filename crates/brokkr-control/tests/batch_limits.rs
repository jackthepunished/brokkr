//! Tests for CAS batch request-size enforcement at the service boundary.
//!
//! Verifies that `CasService` rejects oversized `FindMissingBlobs`,
//! `BatchReadBlobs`, and `BatchUpdateBlobs` requests before allocating,
//! per the configured `BatchLimits` (issue #66).

#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use std::sync::Arc;

use brokkr_cas::InMemoryCas;
use brokkr_control::services::{BatchLimits, CasService};
use brokkr_proto::reapi_v2 as rapi;
use rapi::batch_update_blobs_request::Request as UpdateRequest;
use rapi::content_addressable_storage_server::ContentAddressableStorage as _;
use tonic::{Code, Request};

/// Tiny limits so tests can trip them without huge payloads.
fn small_limits() -> BatchLimits {
    BatchLimits {
        max_blobs_per_request: 2,
        max_request_bytes: 8,
    }
}

fn service() -> CasService<InMemoryCas> {
    CasService::with_limits(Arc::new(InMemoryCas::new()), small_limits())
}

/// A shape-valid digest. Content is irrelevant: the count check runs before
/// digest parsing, and `find_missing` does not verify content.
fn dummy_digest() -> rapi::Digest {
    rapi::Digest {
        hash: "a".repeat(64),
        size_bytes: 1,
    }
}

#[tokio::test]
async fn find_missing_blobs_rejects_too_many_digests() {
    let req = rapi::FindMissingBlobsRequest {
        blob_digests: vec![dummy_digest(), dummy_digest(), dummy_digest()],
        ..Default::default()
    };
    let err = service()
        .find_missing_blobs(Request::new(req))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("blobs"));
}

#[tokio::test]
async fn find_missing_blobs_allows_within_limit() {
    let req = rapi::FindMissingBlobsRequest {
        blob_digests: vec![dummy_digest(), dummy_digest()],
        ..Default::default()
    };
    let resp = service()
        .find_missing_blobs(Request::new(req))
        .await
        .unwrap();
    // Both are absent from a fresh in-memory CAS, so both come back missing.
    assert_eq!(resp.into_inner().missing_blob_digests.len(), 2);
}

#[tokio::test]
async fn batch_read_blobs_rejects_too_many_digests() {
    let req = rapi::BatchReadBlobsRequest {
        digests: vec![dummy_digest(), dummy_digest(), dummy_digest()],
        ..Default::default()
    };
    let err = service()
        .batch_read_blobs(Request::new(req))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn batch_update_blobs_rejects_too_many_blobs() {
    let blob = || UpdateRequest {
        digest: Some(dummy_digest()),
        data: vec![1],
        ..Default::default()
    };
    let req = rapi::BatchUpdateBlobsRequest {
        requests: vec![blob(), blob(), blob()],
        ..Default::default()
    };
    let err = service()
        .batch_update_blobs(Request::new(req))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("blobs"));
}

#[tokio::test]
async fn batch_update_blobs_rejects_oversized_payload() {
    // One blob (within the count limit) but 9 bytes > the 8-byte limit.
    let req = rapi::BatchUpdateBlobsRequest {
        requests: vec![UpdateRequest {
            digest: Some(dummy_digest()),
            data: vec![0u8; 9],
            ..Default::default()
        }],
        ..Default::default()
    };
    let err = service()
        .batch_update_blobs(Request::new(req))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("bytes"));
}
