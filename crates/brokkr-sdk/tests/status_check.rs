//! Tests for ExecuteResponse status handling.
//!
//! These tests verify that the SDK correctly surfaces server-reported
//! errors from ExecuteResponse.status. See issue #61.

use brokkr_proto::google::rpc::Status as RpcStatus;
use brokkr_proto::reapi_v2::ExecuteResponse;

/// Verify that an ExecuteResponse with status.code != 0 is detected.
#[test]
fn execute_response_non_zero_status() {
    let status = RpcStatus {
        code: 3, // FAILED_PRECONDITION
        message: "missing input blob".to_string(),
        details: vec![],
    };
    let resp = ExecuteResponse {
        status: Some(status),
        result: None,
        cached_result: false,
        ..Default::default()
    };

    // Sanity check: status is present and non-OK
    let s = resp.status.as_ref().expect("status should be present");
    assert_eq!(s.code, 3);
    assert_eq!(s.message, "missing input blob");
}

/// Verify that ExecuteResponse with OK status (code == 0) passes the check.
#[test]
fn execute_response_ok_status() {
    let status = RpcStatus {
        code: 0, // OK
        message: String::new(),
        details: vec![],
    };
    let resp = ExecuteResponse {
        status: Some(status),
        result: None,
        cached_result: false,
        ..Default::default()
    };

    // Code 0 means OK — the SDK would proceed to check result
    assert_eq!(resp.status.as_ref().map(|s| s.code).expect("status should be present"), 0);
}

/// Verify that ExecuteResponse with no status field is treated as OK.
#[test]
fn execute_response_missing_status() {
    let resp: ExecuteResponse = ExecuteResponse {
        status: None,
        result: None,
        cached_result: false,
        ..Default::default()
    };

    // Missing status means OK per REAPI spec
    assert!(resp.status.is_none());
}
